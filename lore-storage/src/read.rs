// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::min;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use bytes::BytesMut;
use lore_error_set::prelude::*;
use lore_transport::StorageSession;

use crate::STORE_RETRY_ATTEMPTS;
use crate::compress;
use crate::concurrency::file_count_limit_acquire;
use crate::defragment::DefragmentSink;
use crate::defragment::defragment_pipeline;
use crate::defragment::read_defragment;
use crate::error::StorageError;
use crate::errors::SlowDown;
use crate::fragment_flags::FragmentFlags;
use crate::fs_util;
use crate::hash;
use crate::immutable_store::ImmutableStore;
use crate::immutable_store::StoreError;
use crate::options::ReadOptions;
use crate::store_types::StoreMatch;
use crate::types::Address;
use crate::types::Fragment;
use crate::types::Partition;

fn store_retry() -> crate::Retry {
    // Retry, start at 50 milliseconds, maximum wait 10 seconds
    crate::retry(
        50,
        10_000,
        *STORE_RETRY_ATTEMPTS.get_or_init(|| {
            60 //default try 60 times
        }),
    )
}

/// Load a single raw fragment from store with retry backoff
pub async fn read_raw(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    match_required: StoreMatch,
) -> Result<(Fragment, Bytes), StorageError> {
    let mut retry = store_retry();
    loop {
        debug_assert!(
            !address.hash.is_zero(),
            "Cannot request zero hash from store"
        );
        match store.clone().get(partition, address, match_required).await {
            Ok((fragment, payload)) => {
                debug_assert!(
                    match hash::hash_fragment(fragment, payload.as_ref()) {
                        Ok(loaded_hash) => loaded_hash == address.hash,
                        Err(_) => true,
                    },
                    "Local store loaded data failed hash validation"
                );
                return Ok((fragment, payload));
            }
            Err(StoreError::SlowDown(_)) => {
                if !retry.wait().await {
                    return Err(StorageError::from(SlowDown));
                }
            }
            Err(StoreError::AddressNotFound(_) | StoreError::PayloadNotFound(_)) => {
                return Err(StorageError::from(crate::errors::AddressNotFound::from(
                    address,
                )));
            }
            Err(err) => {
                return Err(StorageError::internal_with_context(err, "store get failed"));
            }
        }
    }
}

pub async fn decompress_and_verify(
    fragment: Fragment,
    buffer: Bytes,
    address: Address,
    options: ReadOptions,
) -> Result<(Fragment, Bytes), StorageError> {
    if !options.decompress && !options.verify {
        return Ok((fragment, buffer));
    }

    let mut fragment = fragment;
    let mut buffer = buffer;

    let mut content_hash = address.hash;
    // Compressed is a group flag, check if any of the flags are set
    if (fragment.flags & FragmentFlags::PayloadCompressed) != 0 {
        let (decompressed_fragment, decompressed_buffer) =
            compress::decompress_async(fragment, buffer.clone())
                .await
                .forward::<StorageError>("failed to decompress fragment")?;
        if options.verify {
            content_hash = hash::hash_slice(decompressed_buffer.as_ref());
        }
        if options.decompress {
            buffer = decompressed_buffer.freeze();
            fragment = decompressed_fragment;
        }
    } else if options.verify {
        content_hash = hash::hash_slice(buffer.as_ref());
    }

    if options.verify && content_hash != address.hash {
        Err(StorageError::internal(format!(
            "fragment hash mismatch, got {content_hash}"
        )))
    } else {
        Ok((fragment, buffer))
    }
}

/// Process-wide count of remote fetches in flight across every [`remote_get_retry`] path; shared by all concurrent operations, layer per-op attribution on top if needed.
pub static REMOTE_FETCH_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// See [`REMOTE_FETCH_INFLIGHT`].
pub fn remote_fetch_inflight() -> u64 {
    REMOTE_FETCH_INFLIGHT.load(Ordering::Relaxed)
}

/// RAII guard around [`REMOTE_FETCH_INFLIGHT`] so the counter can't leak on panic or early return.
struct RemoteFetchGuard;
impl RemoteFetchGuard {
    fn new() -> Self {
        REMOTE_FETCH_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        Self
    }
}
impl Drop for RemoteFetchGuard {
    fn drop(&mut self) {
        REMOTE_FETCH_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Fetch a fragment from a remote session with retry on `SlowDown` and on
/// transient `NotConnected` responses (e.g. the server's session-id map was
/// reset by a QUIC reconnect; the storage layer mapping turns this into
/// `StorageError::NotConnected`, which we recover from by invalidating the
/// cached session and retrying with a fresh `session_start`).
async fn remote_get_retry(
    session: &StorageSession,
    address: Address,
    priority: bool,
) -> Result<(Fragment, Bytes), StorageError> {
    let _guard = RemoteFetchGuard::new();
    let mut retry = store_retry();
    let mut stale_session_retries: u32 = 0;
    loop {
        debug_assert!(
            !address.hash.is_zero(),
            "Cannot request zero hash from store"
        );
        let result = if priority {
            session.get_priority(&address).await
        } else {
            session.get(&address).await
        };
        match result {
            Ok((fragment, payload)) => return Ok((fragment, payload)),
            Err(ref e) if e.is_slow_down() => {
                if !retry.wait().await {
                    return Err(StorageError::from(SlowDown));
                }
            }
            Err(err) => {
                let storage_err = crate::error::protocol_error_to_storage(err, address);
                if matches!(storage_err, StorageError::NotConnected(_))
                    && stale_session_retries < MAX_STALE_SESSION_RETRIES
                {
                    stale_session_retries += 1;
                    session.invalidate().await;
                    if !retry.wait().await {
                        return Err(storage_err);
                    }
                    continue;
                }
                return Err(storage_err);
            }
        }
    }
}

/// Bound on retries for `StorageError::NotConnected` in `remote_get_retry`.
/// Picked so a genuinely permanent server-side failure surfaces quickly
/// rather than looping through the full `store_retry` backoff schedule (60
/// attempts up to 10 s apart). Recovery from a QUIC reconnect typically
/// succeeds on the first or second retry once the session has been
/// re-established.
const MAX_STALE_SESSION_RETRIES: u32 = 5;

/// Unified fragment load: local -> decompress/verify -> optional remote fallback -> heal -> cache.
///
/// When `remote_session` is `Some`, the session is used for remote fetch if the
/// local load fails (miss or corrupt). If the remote data fails verification,
/// heal is attempted once via `session.verify()` before retrying.
///
/// For local-only loading, pass `None`.
pub async fn load_fragment(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(Fragment, Bytes), StorageError> {
    if address.hash.is_zero() {
        return Ok((Fragment::default(), Bytes::default()));
    }

    // If a background leader task dispatched via the write tracker is currently
    // producing the terminal store entry for this address, wait for it before
    // reading. Without this, a same-operation read-after-write (e.g. commit's
    // weave_history loading the delta block that generate_delta_block just
    // handed to the tracker) can race ahead of the leader and miss both local
    // and remote.
    crate::write::wait_if_in_flight(partition, address).await;

    enum LocalFailure {
        Corrupt,
        Other,
    }

    // Local load: try MatchFull first, fallback to MatchHash if not isolated. Callers that
    // bind a handle to remote-only mode disable the local probe entirely via `options.local`.
    let decompress_result = if options.local {
        let local_result =
            match read_raw(store.clone(), partition, address, StoreMatch::MatchFull).await {
                Ok((fragment, payload)) => Ok((fragment, payload)),
                Err(ref err) if matches!(err, StorageError::AddressNotFound(_)) => {
                    if !options.isolate {
                        read_raw(store.clone(), partition, address, StoreMatch::MatchHash).await
                    } else {
                        Err(StorageError::from(crate::errors::AddressNotFound::from(
                            address,
                        )))
                    }
                }
                Err(err) => Err(err),
            };

        // Decompress + verify local data
        match local_result {
            Ok((fragment, buffer)) => {
                match decompress_and_verify(fragment, buffer, address, options).await {
                    Ok((fragment, buffer)) => Ok((fragment, buffer)),
                    Err(err) if matches!(err, StorageError::NotSupported(_)) => return Err(err),
                    Err(err) => {
                        lore_base::lore_debug!(
                            "Fragment {} failed decompression/verification: {err}",
                            address.hash
                        );
                        debug_assert!(
                            false,
                            "Local store data failed decompression or verification"
                        );
                        Err(LocalFailure::Corrupt)
                    }
                }
            }
            Err(e) => {
                lore_base::lore_trace!(
                    "Fragment {} failed loading from local store: {e:?}",
                    address.hash
                );
                Err(LocalFailure::Other)
            }
        }
    } else {
        Err(LocalFailure::Other)
    };

    let local_corrupt = matches!(decompress_result, Err(LocalFailure::Corrupt));
    if let Ok((fragment, payload)) = decompress_result {
        return Ok((fragment, payload));
    }

    // No remote session -> nothing more to try
    if !options.remote {
        return Err(StorageError::from(crate::errors::AddressNotFound::from(
            address,
        )));
    }
    let Some(session) = remote_session else {
        return Err(StorageError::from(crate::errors::AddressNotFound::from(
            address,
        )));
    };

    lore_base::lore_trace!("Fetch immutable fragment {} from remote", address);

    let mut options = options;
    options.verify |= local_corrupt;

    let mut heal_attempted = false;
    loop {
        let (mut fragment, buffer) =
            remote_get_retry(session.as_ref(), address, options.priority).await?;

        fragment.flags |= FragmentFlags::PayloadStoredDurable;
        let store_fragment = fragment;
        let payload = buffer.clone();

        match decompress_and_verify(fragment, buffer, address, options).await {
            Ok((fragment, buffer)) => {
                // Cache the fragment locally. Skip the put entirely when
                // caching is disabled and data is not corrupt and has no
                // local cache priority flag -- matching the original two-level
                // gate in urc-core's load_raw.
                let should_store = options.cache
                    || local_corrupt
                    || (fragment.flags & FragmentFlags::PayloadLocalCachePriority) != 0;

                if should_store {
                    let local_payload = if options.cache
                        || local_corrupt
                        || (fragment.flags & FragmentFlags::PayloadLocalCachePriority)
                            == FragmentFlags::PayloadLocalCachePriority
                    {
                        Some(payload)
                    } else {
                        None
                    };
                    let force = local_corrupt;
                    let _ = store
                        .clone()
                        .put(partition, address, store_fragment, local_payload, force)
                        .await;
                }

                return Ok((fragment, buffer));
            }
            Err(err) => {
                if matches!(err, StorageError::NotSupported(_)) {
                    return Err(err);
                }
                if heal_attempted {
                    lore_base::lore_error!(
                        "Fragment {} still corrupt after heal: {}",
                        address.hash,
                        err
                    );
                    return Err(err);
                }

                lore_base::lore_warn!("Fragment {}: {}. Attempting heal.", address.hash, err);

                let healed = session
                    .verify(&address, true)
                    .await
                    .is_ok_and(|r| r.healed == lore_base::types::HealResult::Healed);

                if !healed {
                    lore_base::lore_error!("Server did not heal fragment {}", address.hash);
                    return Err(err);
                }

                lore_base::lore_debug!("Server healed fragment {}, retrying fetch", address.hash);
                heal_attempted = true;
            }
        }
    }
}

/// Load a single raw fragment from local store, optionally decompressing and verifying.
/// Does not reassemble fragmented data or fallback to remote.
/// Thin wrapper around [`load_fragment`] with no remote session.
pub async fn load_raw_local(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
) -> Result<(Fragment, Bytes), StorageError> {
    load_fragment(store, partition, address, options, None).await
}

/// Read content (defragmenting if needed) into a `Bytes` buffer.
pub async fn read(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<Bytes, StorageError> {
    let options = options.with_decompress();
    let (fragment, buffer) = load_fragment(
        store.clone(),
        partition,
        address,
        options,
        remote_session.clone(),
    )
    .await?;

    if let Some(max) = options.max_content_size
        && fragment.size_content > max
    {
        return Err(StorageError::from(crate::errors::Oversized {
            context: format!(
                "fragment size_content {} exceeds caller-supplied max {max}",
                fragment.size_content
            ),
        }));
    }

    let range = match range {
        Some(range) => {
            min(range.start, fragment.size_content as usize)
                ..min(range.end, fragment.size_content as usize)
        }
        None => 0..fragment.size_content as usize,
    };
    if range.is_empty() {
        return Ok(Bytes::default());
    }

    if (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented {
        let mut target_buffer = BytesMut::with_capacity(range.len());
        unsafe {
            target_buffer.set_len(range.len());
        }
        let target_size = target_buffer.len();
        let target = target_buffer.split();
        read_defragment(
            store,
            partition,
            address,
            range,
            fragment,
            buffer,
            target,
            options,
            0,
            remote_session,
        )
        .await?;
        if !target_buffer.try_reclaim(target_size) {
            return Err(StorageError::internal(
                "failed to reclaim buffer after defragmenting",
            ));
        }
        unsafe {
            target_buffer.set_len(target_size);
        }
        Ok(target_buffer.freeze())
    } else {
        Ok(buffer.slice(range))
    }
}

/// Read content into a pre-allocated buffer with offset/length.
pub async fn read_into(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    slice: &mut [u8],
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    let load_raw_options = options;
    let (fragment, buffer) = load_fragment(
        store.clone(),
        partition,
        address,
        load_raw_options.no_decompress(),
        remote_session.clone(),
    )
    .await?;

    if let Some(max) = options.max_content_size
        && fragment.size_content > max
    {
        return Err(StorageError::from(crate::errors::Oversized {
            context: format!(
                "fragment size_content {} exceeds caller-supplied max {max}",
                fragment.size_content
            ),
        }));
    }

    let range = match range {
        Some(range) => {
            min(range.start, fragment.size_content as usize)
                ..min(range.end, fragment.size_content as usize)
        }
        None => 0..fragment.size_content as usize,
    };
    if range.is_empty() {
        return Ok(());
    }
    if slice.len() != range.len() {
        return Err(StorageError::internal(format!(
            "unexpected size: slice {} vs range {}",
            slice.len(),
            range.len()
        )));
    }

    if (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented {
        let content_size = range.len();
        let mut content = BytesMut::with_capacity(content_size);
        unsafe {
            content.set_len(content_size);
        }
        let target = content.split();
        read_defragment(
            store,
            partition,
            address,
            range,
            fragment,
            buffer,
            target,
            options,
            0,
            remote_session,
        )
        .await?;
        if !content.try_reclaim(content_size) {
            return Err(StorageError::internal(
                "failed to reclaim buffer after defragmenting",
            ));
        }
        unsafe {
            content.set_len(content_size);
        }
        if slice.len() != content.len() {
            return Err(StorageError::internal(format!(
                "unexpected size: slice {} vs content {}",
                slice.len(),
                content.len()
            )));
        }
        slice.copy_from_slice(content.as_ref());
    } else if fragment.flags & FragmentFlags::PayloadCompressed != 0 {
        let (_, decompressed) = compress::decompress_async(fragment, buffer)
            .await
            .map_err(|e| StorageError::internal_with_context(e, "decompress failed"))?;
        let decompressed = decompressed.freeze().slice(range);
        if slice.len() != decompressed.len() {
            return Err(StorageError::internal(format!(
                "unexpected size: slice {} vs decompressed {}",
                slice.len(),
                decompressed.len()
            )));
        }
        slice.copy_from_slice(decompressed.as_ref());
    } else {
        let buffer = buffer.slice(range);
        if slice.len() != buffer.len() {
            return Err(StorageError::internal(format!(
                "unexpected size: slice {} vs buffer {}",
                slice.len(),
                buffer.len()
            )));
        }
        slice.copy_from_slice(buffer.as_ref());
    }
    Ok(())
}

/// Read content into a streaming channel.
pub async fn read_stream(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
    sender: tokio::sync::mpsc::Sender<Bytes>,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<u64, StorageError> {
    let options = options.with_decompress();
    let (fragment, buffer) = load_fragment(
        store.clone(),
        partition,
        address,
        options,
        remote_session.clone(),
    )
    .await?;

    if (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented {
        let store = store.clone();
        lore_base::lore_spawn!(async move {
            let result = defragment_pipeline(
                store,
                partition,
                address,
                fragment,
                buffer,
                DefragmentSink::Stream { sender },
                options,
                remote_session,
            )
            .await;

            if let Err(err) = result {
                lore_base::lore_warn!("error while defragmenting during read_stream: {0}", err);
            }
        });

        Ok(fragment.size_content)
    } else {
        sender
            .send(buffer)
            .await
            .map_err(|_err| StorageError::internal("stream send failed"))?;
        Ok(fragment.size_content)
    }
}

/// Read content into a file (mmap or direct I/O).
///
/// Returns the fragment header along with the file's metadata when the write
/// path captures it on the open handle (single-fragment direct write). Callers
/// that need a stat regardless of path can fall back to a separate metadata
/// query when `None` is returned (the multi-fragment defragment path doesn't
/// surface metadata yet — the file handle moves through the pipeline).
#[allow(clippy::too_many_arguments)]
pub async fn read_into_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    path: &Path,
    temp_file_extension: &str,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(Fragment, Option<std::fs::Metadata>), StorageError> {
    let _count_permit = file_count_limit_acquire()
        .await
        .forward::<StorageError>("permit failed")?;

    // Read the initial fragment
    let options = options.with_decompress();
    let (fragment, buffer) = load_fragment(
        store.clone(),
        partition,
        address,
        options,
        remote_session.clone(),
    )
    .await?;

    {
        if fragment.flags & FragmentFlags::PayloadFragmented == FragmentFlags::PayloadFragmented {
            // Memory map the file and defragment into it
            let mut retry = crate::retry(10, 10_000, 10);

            let file_path = if options.direct_write {
                path.to_path_buf()
            } else {
                let mut temporary_ext = path.extension().unwrap_or_default().to_os_string();
                temporary_ext.push(temp_file_extension);

                let mut temporary_path = path.to_path_buf();
                temporary_path.set_extension(temporary_ext);

                temporary_path
            };

            // Keep the mmap alive on the stack until defragment_file completes.
            let mut _mmap_guard: Option<memmap2::MmapMut> = None;

            let (file, defrag_target) = if options.direct_file_io {
                let file = loop {
                    match crate::defragment::open_file_write(
                        file_path.as_path(),
                        fragment.size_content as usize,
                    )
                    .await
                    {
                        Ok(file) => break file,
                        Err(err) => {
                            if !retry.wait().await {
                                return Err(StorageError::internal_with_context(
                                    err,
                                    &format!("failed to open file: {}", path.display()),
                                ));
                            }
                        }
                    }
                };

                let file = Arc::new(tokio::sync::Mutex::new(file));
                (file.clone(), DefragmentSink::File { file })
            } else {
                let (file, mut mmap) = loop {
                    match crate::defragment::open_mmap_write(
                        file_path.as_path(),
                        fragment.size_content as usize,
                    )
                    .await
                    {
                        Ok(file) => break file,
                        Err(err) => {
                            if !retry.wait().await {
                                return Err(StorageError::internal_with_context(
                                    err,
                                    &format!("failed to open file: {}", path.display()),
                                ));
                            }
                        }
                    }
                };

                let defrag_target = DefragmentSink::Mmap {
                    ptr: mmap.as_mut_ptr(),
                    len: mmap.len(),
                };
                _mmap_guard = Some(mmap);

                (Arc::new(tokio::sync::Mutex::new(file)), defrag_target)
            };

            lore_base::lore_trace!(
                "Opened file for immutable data write: {} size {}",
                path.display(),
                fragment.size_content
            );

            defragment_pipeline(
                store,
                partition,
                address,
                fragment,
                buffer,
                defrag_target,
                options,
                remote_session,
            )
            .await?;

            if options.sync_data {
                file.lock()
                    .await
                    .sync_data()
                    .await
                    .map_err(|e| StorageError::internal_with_context(e, "flush file"))?;
            }
            // tokio::fs::File wraps std::fs::File without a userspace buffer; flush would dispatch to a blocking thread to call a no-op.
            drop(file);

            if !options.direct_write {
                let path_owned = path.to_path_buf();
                let file_path_clone = file_path.clone();
                let rename_err_msg =
                    format!("rename {} -> {}", file_path.display(), path.display());
                lore_base::lore_spawn_blocking!(move || {
                    fs_util::rename_file(file_path_clone.as_path(), path_owned.as_path())
                })
                .await
                .map_err(|e| StorageError::internal_with_context(e, "rename task join"))?
                .map_err(|e| StorageError::internal_with_context(e, &rename_err_msg))?;
            }
        } else {
            // Write directly into the file
            let mut retry = crate::retry(10, 10_000, 10);
            let metadata = loop {
                match write_all_to_file(path, buffer.clone(), options.sync_data).await {
                    Ok(meta) => break meta,
                    Err(err) => {
                        if !retry.wait().await {
                            return Err(StorageError::internal_with_context(
                                err,
                                &format!("write to file: {}", path.display()),
                            ));
                        }
                    }
                }
            };
            return Ok((fragment, Some(metadata)));
        }
    }

    Ok((fragment, None))
}

pub async fn write_all_to_file(
    path: impl AsRef<Path>,
    buffer: Bytes,
    sync_data: bool,
) -> Result<std::fs::Metadata, std::io::Error> {
    // One spawn_blocking trip for open+write+(sync)+stat+close. Saves the caller a separate stat round-trip and keeps the metadata fetch on the open handle (no path resolve, FS cache warm). std::fs::File has no userspace buffer, so flush would be a no-op for unbuffered writes and is omitted in the non-sync path.
    let path_buf = path.as_ref().to_path_buf();
    let buffer_len = buffer.len();
    let path_display_for_trace = path_buf.clone();
    let join_result =
        lore_base::lore_spawn_blocking!(move || -> std::io::Result<std::fs::Metadata> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path_buf)?;
            file.write_all(buffer.as_ref())?;
            if sync_data {
                file.sync_data()?;
            }
            file.metadata()
        })
        .await;
    let metadata = match join_result {
        Ok(io_result) => io_result?,
        Err(join_err) => {
            return Err(std::io::Error::other(format!(
                "spawn_blocking join error writing {}: {join_err}",
                path_display_for_trace.display()
            )));
        }
    };

    lore_base::lore_trace!(
        "Wrote {} bytes to {}",
        buffer_len,
        path_display_for_trace.display()
    );

    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::fragment_flags::FragmentFlags;
    use crate::local::immutable_store::ImmutableStoreSettings;
    use crate::local::immutable_store::LocalImmutableStore;
    use crate::test_util::TempDir;
    use crate::types::Context;
    use crate::write::try_acquire_in_flight;

    async fn make_test_store() -> (TempDir, Arc<dyn ImmutableStore>) {
        let dir = TempDir::new("lore-storage-read-test-");
        let store = LocalImmutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create test store");
        (dir, store)
    }

    fn make_input(seed: u8) -> (Partition, Address, Fragment, Bytes) {
        let payload = vec![seed; 64];
        let hash_value = hash::hash_slice(&payload);
        let partition = Partition::from([seed; 16]);
        let address = Address {
            hash: hash_value,
            context: Context::from([seed; 16]),
        };
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredLocal.bits(),
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        (partition, address, fragment, Bytes::from(payload))
    }

    /// Regression for the tracker-dispatched read-after-write race: a reader
    /// that arrives while a leader holds the in-flight guard must wait for the
    /// terminal store entry instead of returning `AddressNotFound`. This mirrors
    /// the path that `weave_history` takes when it loads the delta block that
    /// `generate_delta_block` just handed to the tracker.
    #[tokio::test(flavor = "multi_thread")]
    async fn load_fragment_waits_for_in_flight_leader() {
        let (_dir, store) = make_test_store().await;
        let (partition, address, fragment, payload) = make_input(0xDE);

        let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

        let reader_store = store.clone();
        let reader = lore_base::lore_spawn!(async move {
            load_fragment(
                reader_store,
                partition,
                address,
                ReadOptions::default().no_verify(),
                None,
            )
            .await
        });

        // Give the reader a real chance to observe the in-flight entry and
        // park itself on the cancellation token rather than blaze through.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !reader.is_finished(),
            "reader must not finish before the leader writes and drops its guard"
        );

        store
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("leader writes terminal entry");
        drop(guard);

        let (loaded_fragment, loaded_payload) = reader
            .await
            .expect("reader task joined")
            .expect("reader observes terminal entry after leader completes");
        assert_eq!(loaded_fragment.size_payload, fragment.size_payload);
        assert_eq!(loaded_payload.as_ref(), payload.as_ref());
    }

    /// When the leader drops its guard without writing (upload failed, task
    /// aborted), the reader must not hang — it should surface the same
    /// `AddressNotFound` it would have seen without the in-flight wait.
    #[tokio::test(flavor = "multi_thread")]
    async fn load_fragment_returns_not_found_when_leader_drops_without_writing() {
        let (_dir, store) = make_test_store().await;
        let (partition, address, _fragment, _payload) = make_input(0xAD);

        let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

        let reader_store = store.clone();
        let reader = lore_base::lore_spawn!(async move {
            load_fragment(
                reader_store,
                partition,
                address,
                ReadOptions::default().no_verify(),
                None,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(guard);

        let err = reader
            .await
            .expect("reader task joined")
            .expect_err("reader must not invent a fragment when leader wrote nothing");
        assert!(
            matches!(err, StorageError::AddressNotFound(_)),
            "expected AddressNotFound, got {err:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_into_single_fragment_respects_range() {
        let (_dir, store) = make_test_store().await;

        let mut payload = vec![0u8; 100];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = i as u8;
        }

        let hash_value = hash::hash_slice(&payload);
        let partition = Partition::from([0; 16]);
        let address = Address {
            hash: hash_value,
            context: Context::from([0; 16]),
        };
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredLocal.bits(),
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };

        store
            .clone()
            .put(
                partition,
                address,
                fragment,
                Some(Bytes::from(payload.clone())),
                false,
            )
            .await
            .expect("put test data");

        let mut out = [0u8; 40];
        read_into(
            store,
            partition,
            address,
            Some(10..50),
            &mut out,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect("read_into should respect range");

        assert_eq!(&out[..], &payload[10..50]);
    }
}
