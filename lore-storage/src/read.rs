// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::min;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use bytes::BytesMut;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_error_set::prelude::*;
use lore_transport::StorageSession;

use crate::compress;
use crate::concurrency::file_count_limit_acquire;
use crate::defragment::DefragmentSink;
use crate::defragment::defragment_pipeline;
use crate::defragment::read_defragment;
use crate::error::StorageError;
use crate::errors::SlowDown;
use crate::fragment_flags::FragmentFlags;
use crate::hash;
use crate::immutable_store::ImmutableStore;
use crate::immutable_store::StoreError;
use crate::mutable_store::MutableStore;
use crate::options::ReadOptions;
use crate::store_types::StoreGetData;
use crate::types::Address;
use crate::types::Fragment;
use crate::types::Partition;

/// Load a single raw fragment from store with retry backoff. How widely the store searches for it
/// is the store's own business - see [`ImmutableStore::read_scope`].
pub async fn read_raw(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> Result<(Fragment, Bytes), StorageError> {
    let mut retry = crate::store_retry();
    loop {
        debug_assert!(
            !address.hash.is_zero(),
            "Cannot request zero hash from store"
        );
        match store
            .clone()
            .get(partition, address)
            .await
            .and_then(StoreGetData::into_payload)
        {
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
            compress::decompress(fragment, buffer.as_ref())
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
///
/// `Disconnected` is deliberately not retried here: on both transports it means the
/// transport already exhausted its own reconnect-and-reissue and gave up, so the remote
/// is down rather than the session being stale.
async fn remote_get_retry(
    session: &StorageSession,
    address: Address,
    priority: bool,
) -> Result<(Fragment, Bytes), StorageError> {
    let _guard = RemoteFetchGuard::new();
    let mut retry = crate::store_retry();
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

    // Callers that bind a handle to remote-only mode disable the local probe entirely via
    // `options.local`.
    let decompress_result = if options.local {
        let local_result = read_raw(store.clone(), partition, address).await;

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

/// Resolve a caller's content range against the content that actually exists.
///
/// `None` is the whole content. A range reaching past the end is clamped rather than refused:
/// a caller reading the tail of content whose size it holds from an earlier lookup gets the
/// bytes that are there. A start past the end resolves to empty — callers that need to tell
/// that apart from genuinely empty content compare their own start against `size_content`,
/// which every entry point here reports back alongside the bytes.
///
/// The result is never inverted, whatever the caller passed, so it is safe to hand to
/// [`Bytes::slice`], which panics on a range starting past its own end.
pub fn resolve_content_range(range: Option<Range<usize>>, size_content: u64) -> Range<usize> {
    let end = usize::try_from(size_content).unwrap_or(usize::MAX);
    match range {
        Some(range) => {
            let start = min(range.start, end);
            start..min(range.end, end).max(start)
        }
        None => 0..end,
    }
}

/// Read content (defragmenting if needed) into a `Bytes` buffer, returning the fragment
/// describing the whole content alongside the bytes the range asked for.
///
/// The fragment comes back because the bytes alone no longer say how much content there is:
/// with a range, `size_content` is what exists and the buffer length is what was asked for.
pub async fn read(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(Fragment, Bytes), StorageError> {
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

    let range = resolve_content_range(range, fragment.size_content);
    if range.is_empty() {
        return Ok((fragment, Bytes::default()));
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
        Ok((fragment, target_buffer.freeze()))
    } else {
        Ok((fragment, buffer.slice(range)))
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

    let range = resolve_content_range(range, fragment.size_content);
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
        let (_, decompressed) = compress::decompress(fragment, buffer.as_ref())
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

/// Read content into a streaming channel, returning the fragment describing the whole content
/// and the content range that will arrive on the channel.
///
/// The returned range is the caller's, clamped to what exists, so a caller can emit a header
/// and account for what it receives before the first chunk lands. Chunks arrive in content
/// order and the caller positions them at `range.start` and upwards; the range is `0..0` when
/// nothing was asked for, and nothing is sent.
///
/// Ranged reads of a fragmented payload fetch only the leaves the range touches, so the work
/// is proportional to the range rather than to the content.
///
/// The range returns before the leaves flow, so a failure part-way through the tree arrives on
/// the channel as an `Err`: it is the only route by which the caller learns its content is short.
#[allow(clippy::too_many_arguments)]
pub async fn read_stream(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
    sender: tokio::sync::mpsc::Sender<Result<Bytes, StorageError>>,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(Fragment, Range<u64>), StorageError> {
    let options = options.with_decompress();
    let (fragment, buffer) = load_fragment(
        store.clone(),
        partition,
        address,
        options,
        remote_session.clone(),
    )
    .await?;

    let range = resolve_content_range(range, fragment.size_content);
    let streamed = range.start as u64..range.end as u64;
    if range.is_empty() {
        return Ok((fragment, streamed));
    }

    if (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented {
        let store = store.clone();
        let pipeline_range = streamed.clone();
        let report = sender.clone();
        lore_base::lore_spawn!(async move {
            let result = defragment_pipeline(
                store,
                partition,
                address,
                fragment,
                buffer,
                pipeline_range,
                DefragmentSink::Stream { sender },
                options,
                remote_session,
            )
            .await;

            if let Err(err) = result {
                lore_base::lore_warn!("error while defragmenting during read_stream: {0}", err);
                let _ = report.send(Err(err)).await;
            }
        });

        Ok((fragment, streamed))
    } else {
        sender
            .send(Ok(buffer.slice(range)))
            .await
            .map_err(|_err| StorageError::internal("read stream closed"))?;
        Ok((fragment, streamed))
    }
}

/// Removes a temporary file that was never renamed into place.
///
/// An orphan is a *full-size* file holding a prefix — the target is sized before any content
/// arrives — and an invisible one, since the staging filters exclude the extension and nothing
/// else deletes them.
///
/// Armed before the open, because a failure part-way through it can leave the file created.
/// Disarmed after the rename because the path is derived from the destination: a guard outliving
/// its own rename would delete the next reader's file.
struct TemporaryFile {
    path: Option<PathBuf>,
}

impl TemporaryFile {
    fn guard(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn renamed(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(err) = std::fs::remove_file(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            lore_base::lore_warn!("failed to remove temporary file {}: {err}", path.display());
        }
    }
}

/// Read content into a file.
///
/// `range` selects the content to write; the file holds exactly that range and nothing else,
/// starting at its first byte. `None` writes the whole content, which is what sizing the file
/// to `size_content` used to mean.
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
    range: Option<Range<usize>>,
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

    let range = resolve_content_range(range, fragment.size_content);

    {
        if fragment.flags & FragmentFlags::PayloadFragmented == FragmentFlags::PayloadFragmented {
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

            let mut temporary =
                (!options.direct_write).then(|| TemporaryFile::guard(file_path.clone()));

            let file = loop {
                match crate::defragment::open_file_write(file_path.as_path(), range.len()).await {
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
            let defrag_target = DefragmentSink::File {
                file: file.clone(),
                size: range.len(),
            };

            lore_base::lore_trace!(
                "Opened file for immutable data write: {} size {}",
                path.display(),
                range.len()
            );

            defragment_pipeline(
                store,
                partition,
                address,
                fragment,
                buffer,
                range.start as u64..range.end as u64,
                defrag_target,
                options,
                remote_session,
            )
            .await?;

            if options.sync_data {
                file.sync_data()
                    .await
                    .map_err(|e| StorageError::internal_with_context(e, "flush file"))?;
            }
            // The handle holds no userspace buffer, so there is nothing to flush.
            drop(file);

            if !options.direct_write {
                let rename_err_msg =
                    format!("rename {} -> {}", file_path.display(), path.display());
                lore_io::IoDriver::global()
                    .rename(file_path.as_path(), path)
                    .await
                    .map_err(|e| StorageError::internal_with_context(e, &rename_err_msg))?;

                if let Some(temporary) = temporary.as_mut() {
                    temporary.renamed();
                }
            }
        } else {
            // Write directly into the file
            let mut retry = crate::retry(10, 10_000, 10);
            let buffer = buffer.slice(range);
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

/// Writes `buffer` as the whole contents of `path` and returns the resulting metadata.
///
/// One driver dispatch covers open, write, optional sync and stat, so the caller needs no
/// separate stat round-trip and the metadata comes off the open handle rather than from a second
/// path resolve. The whole-file operation refuses anything above `lore_io::WHOLE_FILE_LIMIT`,
/// which the content written here cannot reach: an unfragmented fragment's content is bounded by
/// `FRAGMENT_SIZE_THRESHOLD`.
pub async fn write_all_to_file(
    path: impl AsRef<Path>,
    buffer: Bytes,
    sync_data: bool,
) -> Result<std::fs::Metadata, std::io::Error> {
    let path = path.as_ref().to_path_buf();
    let buffer_len = buffer.len();

    // Reissued while the open fails transiently: a reader of this path grants no write access
    // for as long as it is open, so on Windows a write landing on a file being hashed or
    // fragmented waits for that scan rather than failing the materialization.
    let metadata = crate::fs_util::retry_transient(|| {
        let path = path.clone();
        let buffer = buffer.clone();
        async move {
            lore_io::IoDriver::global()
                .write_file_bytes(path, buffer, sync_data)
                .await
        }
    })
    .await?;

    lore_base::lore_trace!("Wrote {} bytes to {}", buffer_len, path.display());

    Ok(metadata)
}

/// The root a key resolved to, plus the session the tail should use for anything the root refers
/// to. Present whenever the caller supplied one, including on a local hit: the root can be cached
/// locally while a fragment list's leaves are not.
struct ResolvedRoot {
    resolved: Hash,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    session: Option<Arc<StorageSession>>,
}

/// Local half of [`read_resolved`]: resolve `key` in the local mutable store and load the root it
/// names from the local store only.
///
/// `None` means the caller should ask the remote instead — the mapping is absent, it is a
/// tombstone, or its root is not cached locally.
///
/// A mapping that *is* present is trusted as-is and not revalidated. Because the key is mutable,
/// that is a weaker guarantee than the immutable [`load_fragment`] path gives: a cached mapping
/// can name a hash the key has since moved off. Freshness is the caller's choice through the same
/// flags a `get` uses — `remote` resolves authoritatively, the default prefers whatever is local.
///
/// On the fall-through it deliberately does not remote-read the locally cached hash. A remote
/// `get_resolved` answers the mapping and the root in one round trip, so re-resolving costs
/// nothing extra and answers against the authoritative mapping.
async fn load_resolved_local(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    options: ReadOptions,
) -> Option<(Hash, Fragment, Bytes)> {
    let resolved = match mutable.load(partition, key, KeyType::Resolve).await {
        Ok(resolved) if !resolved.is_zero() => resolved,
        Ok(_) => return None,
        Err(err) => {
            lore_base::lore_trace!("Key {key} failed to resolve from local mutable store: {err:?}");
            return None;
        }
    };

    let address = Address {
        hash: resolved,
        context,
    };
    match load_fragment(store, partition, address, options.no_remote(), None).await {
        Ok((fragment, buffer)) => Some((resolved, fragment, buffer)),
        Err(err) => {
            lore_base::lore_trace!(
                "Key {key} resolved locally to {resolved}, whose root is not cached: {err:?}"
            );
            None
        }
    }
}

/// Resolve `key` to the root fragment it names, sharing one round trip with the read of that root
/// whenever the answer is not already local.
///
/// The key is always read as [`KeyType::Resolve`], locally and remotely alike.
///
/// Local-first like [`read`]: [`load_resolved_local`] tries the local mutable store and the local
/// copy of the root it names, and only a miss there reaches the remote. A fragment list's leaves
/// go through [`load_fragment`] either way, so they keep their own local-then-remote fallback and
/// local caching.
///
/// On a remote resolve the key->hash mapping is written back to the local mutable store once the
/// payload write-back succeeds, under the same gate — so a later call can be served entirely
/// locally, and the mapping is never left pointing at a root this store does not hold.
///
/// A local root travels with the caller's session anyway: a fragment list's leaves may still
/// exist only remotely.
///
/// A verification failure gets one heal attempt then a re-resolve, as [`load_fragment`] does. The
/// retry re-resolves rather than re-reads, since the heal targets the resolved address and a
/// fresh resolve costs the same single round trip.
///
/// `flags` is a reserved bitmask forwarded to the server; 0 for default behaviour.
#[allow(clippy::too_many_arguments)]
async fn resolve_root(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> Result<ResolvedRoot, StorageError> {
    let options = options.with_decompress();
    let key_address = Address { hash: key, context };

    if options.local
        && let Some((resolved, fragment, buffer)) = load_resolved_local(
            store.clone(),
            mutable.clone(),
            partition,
            key,
            context,
            options,
        )
        .await
    {
        return Ok(ResolvedRoot {
            resolved,
            address: Address {
                hash: resolved,
                context,
            },
            fragment,
            buffer,
            session,
        });
    }

    if !options.remote {
        return Err(StorageError::from(crate::errors::AddressNotFound::from(
            key_address,
        )));
    }
    let Some(session) = session else {
        return Err(StorageError::from(crate::errors::AddressNotFound::from(
            key_address,
        )));
    };

    lore_base::lore_trace!("Resolve key {} from remote", key_address);

    let mut heal_attempted = false;
    let (resolved, address, fragment, buffer) = loop {
        let (resolved, mut fragment, buffer) =
            remote_get_resolved_retry(session.as_ref(), key, context, flags).await?;

        if resolved.is_zero() {
            return Err(StorageError::from(crate::errors::AddressNotFound::from(
                key_address,
            )));
        }

        let address = Address {
            hash: resolved,
            context,
        };

        fragment.flags |= FragmentFlags::PayloadStoredDurable;
        let store_fragment = fragment;
        let raw_payload = buffer.clone();

        match decompress_and_verify(fragment, buffer, address, options).await {
            Ok((fragment, buffer)) => {
                let should_store = options.cache
                    || (fragment.flags & FragmentFlags::PayloadLocalCachePriority) != 0;
                if should_store
                    && store
                        .clone()
                        .put(partition, address, store_fragment, Some(raw_payload), false)
                        .await
                        .is_ok()
                {
                    let _ = mutable
                        .store(partition, key, resolved, KeyType::Resolve)
                        .await;
                }
                break (resolved, address, fragment, buffer);
            }
            Err(err) => {
                if matches!(err, StorageError::NotSupported(_)) {
                    return Err(err);
                }
                if heal_attempted {
                    lore_base::lore_error!(
                        "Key {key} resolved to {resolved}, still corrupt after heal: {err}"
                    );
                    return Err(err);
                }

                lore_base::lore_warn!("Key {key} resolved to {resolved}: {err}. Attempting heal.");
                let healed = session
                    .verify(&address, true)
                    .await
                    .is_ok_and(|r| r.healed == lore_base::types::HealResult::Healed);
                if !healed {
                    lore_base::lore_error!("Server did not heal fragment {resolved}");
                    return Err(err);
                }

                lore_base::lore_debug!("Server healed fragment {resolved}, resolving again");
                heal_attempted = true;
            }
        }
    };

    Ok(ResolvedRoot {
        resolved,
        address,
        fragment,
        buffer,
        session: Some(session),
    })
}

/// `mutable_load(key)` + [`read`] of the resulting address, resolved in one round trip when the
/// remote answers. Returns the resolved hash alongside the content.
///
/// See [`resolve_root`] for how the key is resolved; this reassembles the whole content into one
/// buffer. [`read_resolved_stream`] delivers it fragment by fragment instead.
#[allow(clippy::too_many_arguments)]
pub async fn read_resolved(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    range: Option<Range<usize>>,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> Result<(Hash, Bytes), StorageError> {
    let root = resolve_root(
        store.clone(),
        mutable,
        partition,
        key,
        context,
        flags,
        options,
        session,
    )
    .await?;

    let bytes = read_resolved_content(
        store,
        partition,
        root.address,
        root.fragment,
        root.buffer,
        range,
        options.with_decompress(),
        root.session,
    )
    .await?;
    Ok((root.resolved, bytes))
}

/// [`read_resolved`] delivering the content through `sender` one fragment at a time instead of
/// reassembling it, mirroring what [`read_stream`] does for an address.
///
/// Returns the resolved hash and the content's total size; the bytes follow on the channel. Peak
/// memory is bounded by the channel depth rather than by the content, which is what makes this
/// usable for a key naming something large.
#[allow(clippy::too_many_arguments)]
pub async fn read_resolved_stream(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    options: ReadOptions,
    sender: tokio::sync::mpsc::Sender<Result<Bytes, StorageError>>,
    session: Option<Arc<StorageSession>>,
) -> Result<(Hash, u64), StorageError> {
    let options = options.with_decompress();
    let root = resolve_root(
        store.clone(),
        mutable,
        partition,
        key,
        context,
        flags,
        options,
        session,
    )
    .await?;

    if let Some(max) = options.max_content_size
        && root.fragment.size_content > max
    {
        return Err(StorageError::from(crate::errors::Oversized {
            context: format!(
                "fragment size_content {} exceeds caller-supplied max {max}",
                root.fragment.size_content
            ),
        }));
    }

    if (root.fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented
    {
        let address = root.address;
        let fragment = root.fragment;
        let buffer = root.buffer;
        let remote_session = root.session;
        let report = sender.clone();
        let pipeline_range = 0..fragment.size_content;
        lore_base::lore_spawn!(async move {
            let result = defragment_pipeline(
                store,
                partition,
                address,
                fragment,
                buffer,
                pipeline_range,
                DefragmentSink::Stream { sender },
                options,
                remote_session,
            )
            .await;

            if let Err(err) = result {
                lore_base::lore_warn!(
                    "error while defragmenting during read_resolved_stream: {0}",
                    err
                );
                let _ = report.send(Err(err)).await;
            }
        });
    } else {
        sender
            .send(Ok(root.buffer))
            .await
            .map_err(|_err| StorageError::internal("read stream closed"))?;
    }

    Ok((root.resolved, root.fragment.size_content))
}

/// [`remote_get_retry`] for `get_resolved`: back off on `SlowDown`, recover from a stale
/// session id by invalidating and retrying. `key` supplies error context only.
async fn remote_get_resolved_retry(
    session: &StorageSession,
    key: Hash,
    context: Context,
    flags: u32,
) -> Result<(Hash, Fragment, Bytes), StorageError> {
    let _guard = RemoteFetchGuard::new();
    let mut retry = crate::store_retry();
    let mut stale_session_retries: u32 = 0;
    let key_address = Address { hash: key, context };
    loop {
        debug_assert!(!key.is_zero(), "Cannot resolve zero key from store");
        match session.get_resolved(&key, &context, flags).await {
            Ok(resolved) => return Ok(resolved),
            Err(ref e) if e.is_slow_down() => {
                if !retry.wait().await {
                    return Err(StorageError::from(SlowDown));
                }
            }
            Err(err) => {
                let storage_err = crate::error::protocol_error_to_storage(err, key_address);
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

/// Shared tail of [`read_resolved`]: enforce `max_content_size`, clamp `range` to the content, and
/// reassemble a fragment list's leaves through [`load_fragment`], which may fetch them remotely.
#[allow(clippy::too_many_arguments)]
async fn read_resolved_content(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    range: Option<Range<usize>>,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> Result<Bytes, StorageError> {
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
        // Safety: the capacity was just reserved, and read_defragment fully writes the range
        // before the buffer is read back.
        unsafe {
            target_buffer.set_len(range.len());
        }
        let target_size = target_buffer.len();
        let target = target_buffer.split();
        read_defragment(
            store, partition, address, range, fragment, buffer, target, options, 0, session,
        )
        .await?;
        if !target_buffer.try_reclaim(target_size) {
            return Err(StorageError::internal(
                "failed to reclaim buffer after defragmenting",
            ));
        }
        // Safety: try_reclaim just confirmed the split-off target bytes are back in this
        // buffer's capacity, and read_defragment initialized all of them.
        unsafe {
            target_buffer.set_len(target_size);
        }
        Ok(target_buffer.freeze())
    } else {
        Ok(buffer.slice(range))
    }
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

    async fn store_with_isolation(isolate_partitions: bool) -> (TempDir, Arc<dyn ImmutableStore>) {
        let dir = TempDir::new("lore-storage-isolation-test-");
        let store = LocalImmutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            ImmutableStoreSettings {
                isolate_partitions,
                ..Default::default()
            },
        )
        .await
        .expect("create test store");
        (dir, store)
    }

    /// Partitions are content namespacing, so the same bytes written by two tenants land on one
    /// address. Whether reading it back under a partition that never wrote it succeeds is the
    /// store's decision, not the caller's: a single-tenant client serves it, and a store holding
    /// content for everyone must not.
    #[tokio::test]
    async fn a_cross_partition_read_is_refused_only_by_an_isolated_store() {
        let stored_under = Partition::from([0x01; 16]);
        let asked_under = Partition::from([0x02; 16]);
        let payload = Bytes::from_static(b"content addressed by hash alone");
        let address = Address {
            hash: hash::hash_slice(payload.as_ref()),
            context: Context::from([0x03; 16]),
        };
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredLocal.bits(),
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };

        for isolate_partitions in [false, true] {
            let (_dir, store) = store_with_isolation(isolate_partitions).await;
            store
                .clone()
                .put(
                    stored_under,
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("put under the owning partition");

            let result = load_fragment(
                store,
                asked_under,
                address,
                ReadOptions::default().no_remote(),
                None,
            )
            .await;

            if isolate_partitions {
                assert!(
                    matches!(result, Err(StorageError::AddressNotFound(_))),
                    "an isolated store served content from another partition"
                );
            } else {
                let (_fragment, served) = result.expect("a non-isolated store serves by hash");
                assert_eq!(served, payload);
            }
        }
    }

    /// A defragment that fails part-way must not leave its temporary behind. The temporary is
    /// sized to the whole content before any of it arrives and is excluded from staging, so an
    /// orphan is a full-size file that no `status` will ever mention.
    #[tokio::test]
    async fn a_failed_defragment_leaves_no_temporary_file() {
        use zerocopy::IntoBytes;

        use crate::types::FragmentReference;

        let (dir, store) = make_test_store().await;
        let partition = Partition::from([0xA1; 16]);
        let context = Context::from([0xA1; 16]);

        // A list naming content that was never stored: the walk fails once it tries to load it.
        let missing = FragmentReference {
            hash: hash::hash_slice(b"never stored"),
            offset_content: 0,
        };
        let refs_payload = Bytes::copy_from_slice([missing].as_bytes());
        let root_address = Address {
            hash: hash::hash_slice(refs_payload.as_ref()),
            context,
        };
        store
            .clone()
            .put(
                partition,
                root_address,
                Fragment {
                    flags: FragmentFlags::PayloadFragmented.bits(),
                    size_payload: refs_payload.len() as u32,
                    size_content: 64,
                },
                Some(refs_payload),
                false,
            )
            .await
            .expect("put root list");

        let target = PathBuf::from(dir.as_ref()).join("content.bin");
        let result = read_into_file(
            store,
            partition,
            root_address,
            target.as_path(),
            ".~loretemp",
            None,
            ReadOptions::default().no_verify().no_remote(),
            None,
        )
        .await;

        assert!(result.is_err(), "a list naming missing content cannot read");

        let leftovers: Vec<String> = std::fs::read_dir(dir.as_ref())
            .expect("read temp dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".~loretemp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
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

    mod resolve_range {
        use super::*;

        #[test]
        fn none_is_the_whole_content() {
            assert_eq!(resolve_content_range(None, 100), 0..100);
        }

        #[test]
        fn an_inside_range_is_passed_through() {
            assert_eq!(resolve_content_range(Some(10..50), 100), 10..50);
        }

        #[test]
        fn an_end_past_the_content_is_clamped() {
            assert_eq!(resolve_content_range(Some(80..1000), 100), 80..100);
        }

        /// A start past the end is empty rather than an error: the storage layer has no way to
        /// tell a caller apart from a mistaken one, so it serves what exists and leaves the
        /// judgement to the API boundary, which knows what was asked for.
        #[test]
        fn a_start_past_the_content_is_empty() {
            assert_eq!(resolve_content_range(Some(200..300), 100), 100..100);
        }

        /// An inverted range would panic `Bytes::slice`, so it resolves to empty instead. It
        /// cannot arrive from the C API — `offset`/`length` can only describe a forward range —
        /// but `read` is a Rust entry point of its own.
        #[test]
        #[allow(clippy::reversed_empty_ranges, reason = "the input under test")]
        fn an_inverted_range_is_empty_rather_than_a_panic() {
            let resolved = resolve_content_range(Some(60..20), 100);
            assert!(resolved.is_empty());
            assert!(resolved.start <= resolved.end);
            assert_eq!(Bytes::from_static(&[0u8; 100]).slice(resolved).len(), 0);
        }
    }

    /// A two-level fragment tree over four 100-byte leaves, for the pruning tests.
    ///
    /// Returns the root address and every leaf payload concatenated. `store_all` false leaves
    /// the second subtree — its list *and* its leaves — out of the store, so a read that
    /// touches it fails and one that prunes it succeeds. That is the difference between
    /// fetching less and walking less, and only the absent subtree can tell them apart.
    mod tree {
        use zerocopy::IntoBytes;

        use super::*;
        use crate::types::FragmentReference;

        pub(super) const LEAF: usize = 100;
        pub(super) const LEAVES: usize = 4;
        pub(super) const CONTENT: usize = LEAF * LEAVES;

        async fn put_leaf(
            store: &Arc<dyn ImmutableStore>,
            partition: Partition,
            context: Context,
            payload: Vec<u8>,
        ) -> Address {
            let address = Address {
                hash: hash::hash_slice(&payload),
                context,
            };
            let fragment = Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            };
            store
                .clone()
                .put(
                    partition,
                    address,
                    fragment,
                    Some(Bytes::from(payload)),
                    false,
                )
                .await
                .expect("put leaf");
            address
        }

        /// Build a list fragment. Returns its address whether or not it was stored, so a
        /// caller can reference a list the store does not hold.
        async fn put_list(
            store: &Arc<dyn ImmutableStore>,
            partition: Partition,
            context: Context,
            refs: &[FragmentReference],
            size_content: u64,
            store_it: bool,
        ) -> Address {
            let payload = Bytes::copy_from_slice(refs.as_bytes());
            let address = Address {
                hash: hash::hash_slice(payload.as_ref()),
                context,
            };
            if store_it {
                store
                    .clone()
                    .put(
                        partition,
                        address,
                        Fragment {
                            flags: FragmentFlags::PayloadFragmented.bits(),
                            size_payload: payload.len() as u32,
                            size_content,
                        },
                        Some(payload),
                        false,
                    )
                    .await
                    .expect("put list");
            }
            address
        }

        pub(super) async fn build(
            store: &Arc<dyn ImmutableStore>,
            partition: Partition,
            context: Context,
            store_second_subtree: bool,
        ) -> (Address, Vec<u8>) {
            let payloads: Vec<Vec<u8>> = (0..LEAVES)
                .map(|leaf| vec![0xA0u8 + leaf as u8; LEAF])
                .collect();

            let mut leaves = Vec::with_capacity(LEAVES);
            for (leaf, payload) in payloads.iter().enumerate() {
                let in_second_subtree = leaf >= LEAVES / 2;
                if in_second_subtree && !store_second_subtree {
                    // Referenced but absent: reaching it is a read error.
                    leaves.push(Address {
                        hash: hash::hash_slice(payload),
                        context,
                    });
                    continue;
                }
                leaves.push(put_leaf(store, partition, context, payload.clone()).await);
            }

            let reference = |index: usize| FragmentReference {
                hash: leaves[index].hash,
                offset_content: (index * LEAF) as u64,
            };

            let sub_a = put_list(
                store,
                partition,
                context,
                &[reference(0), reference(1)],
                (2 * LEAF) as u64,
                true,
            )
            .await;
            let sub_b = put_list(
                store,
                partition,
                context,
                &[reference(2), reference(3)],
                (2 * LEAF) as u64,
                store_second_subtree,
            )
            .await;

            let root = put_list(
                store,
                partition,
                context,
                &[
                    FragmentReference {
                        hash: sub_a.hash,
                        offset_content: 0,
                    },
                    FragmentReference {
                        hash: sub_b.hash,
                        offset_content: (2 * LEAF) as u64,
                    },
                ],
                CONTENT as u64,
                true,
            )
            .await;

            (root, payloads.concat())
        }
    }

    /// A three-level tree over eight 100-byte leaves, built but not stored.
    ///
    /// ```text
    /// root ─┬─ mid[0] ─┬─ sub[0] ─┬─ leaf[0]   0..100
    ///       │          │          └─ leaf[1] 100..200
    ///       │          └─ sub[1] ─┬─ leaf[2] 200..300
    ///       │                     └─ leaf[3] 300..400
    ///       └─ mid[1] ─┬─ sub[2] ─┬─ leaf[4] 400..500
    ///                  │          └─ leaf[5] 500..600
    ///                  └─ sub[3] ─┬─ leaf[6] 600..700
    ///                             └─ leaf[7] 700..800
    /// ```
    ///
    /// Handing every piece back unstored is what lets a test put exactly the fragments a range
    /// should reach and nothing else: a walk that reached past them fails the read outright
    /// rather than merely doing more work than it needed to.
    mod three_level {
        use zerocopy::IntoBytes;

        use super::*;
        use crate::types::FragmentReference;

        pub(super) const LEAF: usize = 100;
        pub(super) const CONTENT: usize = LEAF * 8;

        pub(super) struct Piece {
            pub(super) address: Address,
            fragment: Fragment,
            payload: Bytes,
        }

        impl Piece {
            fn leaf(context: Context, payload: &[u8]) -> Self {
                let payload = Bytes::copy_from_slice(payload);
                Self {
                    address: Address {
                        hash: hash::hash_slice(payload.as_ref()),
                        context,
                    },
                    fragment: Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    },
                    payload,
                }
            }

            fn list(context: Context, children: &[(Address, u64)], size_content: u64) -> Self {
                let entries: Vec<FragmentReference> = children
                    .iter()
                    .map(|(address, offset_content)| FragmentReference {
                        hash: address.hash,
                        offset_content: *offset_content,
                    })
                    .collect();
                let payload = Bytes::copy_from_slice(entries.as_bytes());
                Self {
                    address: Address {
                        hash: hash::hash_slice(payload.as_ref()),
                        context,
                    },
                    fragment: Fragment {
                        flags: FragmentFlags::PayloadFragmented.bits(),
                        size_payload: payload.len() as u32,
                        size_content,
                    },
                    payload,
                }
            }

            pub(super) async fn put(&self, store: &Arc<dyn ImmutableStore>, partition: Partition) {
                store
                    .clone()
                    .put(
                        partition,
                        self.address,
                        self.fragment,
                        Some(self.payload.clone()),
                        false,
                    )
                    .await
                    .expect("put piece");
            }
        }

        pub(super) struct Tree {
            pub(super) root: Piece,
            pub(super) mid: Vec<Piece>,
            pub(super) sub: Vec<Piece>,
            pub(super) leaf: Vec<Piece>,
            pub(super) content: Vec<u8>,
        }

        pub(super) fn build(context: Context) -> Tree {
            let content: Vec<u8> = (0..CONTENT)
                .map(|byte| 0xA0 + (byte / LEAF) as u8)
                .collect();

            let leaf: Vec<Piece> = (0..8)
                .map(|index| Piece::leaf(context, &content[index * LEAF..(index + 1) * LEAF]))
                .collect();

            let sub: Vec<Piece> = (0..4)
                .map(|index| {
                    let first = 2 * index;
                    Piece::list(
                        context,
                        &[
                            (leaf[first].address, (first * LEAF) as u64),
                            (leaf[first + 1].address, ((first + 1) * LEAF) as u64),
                        ],
                        (2 * LEAF) as u64,
                    )
                })
                .collect();

            let mid: Vec<Piece> = (0..2)
                .map(|index| {
                    let first = 2 * index;
                    Piece::list(
                        context,
                        &[
                            (sub[first].address, (first * 2 * LEAF) as u64),
                            (sub[first + 1].address, ((first + 1) * 2 * LEAF) as u64),
                        ],
                        (4 * LEAF) as u64,
                    )
                })
                .collect();

            let root = Piece::list(
                context,
                &[(mid[0].address, 0), (mid[1].address, (4 * LEAF) as u64)],
                CONTENT as u64,
            );

            Tree {
                root,
                mid,
                sub,
                leaf,
                content,
            }
        }
    }

    fn no_remote() -> ReadOptions {
        ReadOptions::default().no_verify().no_remote()
    }

    /// `read` reports the whole content's fragment alongside the range's bytes. A caller
    /// cannot derive `size_content` from a ranged buffer, so the fragment is how it learns
    /// what it read part of.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_reports_the_whole_size_alongside_a_ranged_buffer() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x31; 16]);
        let context = Context::from([0x31; 16]);
        let (root, content) = tree::build(&store, partition, context, true).await;

        let (fragment, bytes) = read(
            store,
            partition,
            Address {
                hash: root.hash,
                context,
            },
            Some(150..250),
            no_remote(),
            None,
        )
        .await
        .expect("ranged read");

        assert_eq!(fragment.size_content, tree::CONTENT as u64);
        assert_eq!(bytes.as_ref(), &content[150..250]);
    }

    /// The subtree the range misses is never walked, so a tree missing it entirely still
    /// reads. The control below is what makes this a claim about pruning rather than about
    /// the tree happening to be readable.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ranged_read_never_walks_a_subtree_outside_the_range() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x32; 16]);
        let context = Context::from([0x32; 16]);
        let (root, content) = tree::build(&store, partition, context, false).await;
        let address = Address {
            hash: root.hash,
            context,
        };

        let (_fragment, bytes) = read(
            store.clone(),
            partition,
            address,
            Some(50..150),
            no_remote(),
            None,
        )
        .await
        .expect("a range inside the stored subtree reads");
        assert_eq!(bytes.as_ref(), &content[50..150]);

        read(store, partition, address, None, no_remote(), None)
            .await
            .expect_err("the whole content is not readable, so the range really was pruned");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_ranged_stream_delivers_exactly_the_range() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x33; 16]);
        let context = Context::from([0x33; 16]);
        let (root, content) = tree::build(&store, partition, context, true).await;

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
        let (fragment, streamed) = read_stream(
            store,
            partition,
            Address {
                hash: root.hash,
                context,
            },
            Some(120..330),
            no_remote(),
            sender,
            None,
        )
        .await
        .expect("ranged stream");

        assert_eq!(fragment.size_content, tree::CONTENT as u64);
        assert_eq!(streamed, 120..330);

        let mut delivered = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            let chunk = chunk.expect("stream chunk");
            delivered.extend_from_slice(chunk.as_ref());
        }
        assert_eq!(delivered, content[120..330]);
    }

    /// The streaming path prunes the same way the buffered one does — it is a different sink
    /// over the same walk, and this is the test that says so.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ranged_stream_never_walks_a_subtree_outside_the_range() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x34; 16]);
        let context = Context::from([0x34; 16]);
        let (root, content) = tree::build(&store, partition, context, false).await;

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
        let (_fragment, streamed) = read_stream(
            store,
            partition,
            Address {
                hash: root.hash,
                context,
            },
            Some(0..200),
            no_remote(),
            sender,
            None,
        )
        .await
        .expect("a range inside the stored subtree streams");
        assert_eq!(streamed, 0..200);

        let mut delivered = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            let chunk = chunk.expect("stream chunk");
            delivered.extend_from_slice(chunk.as_ref());
        }
        assert_eq!(delivered, content[0..200]);
    }

    /// Chunk boundaries follow the leaves, and the offsets a caller reconstructs from them
    /// have to tile the range from its own start.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ranged_stream_clips_only_its_first_and_last_chunk() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x35; 16]);
        let context = Context::from([0x35; 16]);
        let (root, _content) = tree::build(&store, partition, context, true).await;

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
        let (_fragment, streamed) = read_stream(
            store,
            partition,
            Address {
                hash: root.hash,
                context,
            },
            Some(50..350),
            no_remote(),
            sender,
            None,
        )
        .await
        .expect("ranged stream");

        let mut sizes = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            let chunk = chunk.expect("stream chunk");
            sizes.push(chunk.len());
        }
        // Leaves are 100 bytes at 0/100/200/300; 50..350 clips the first and last.
        assert_eq!(sizes, vec![50, 100, 100, 50]);
        assert_eq!(
            sizes.iter().sum::<usize>() as u64,
            streamed.end - streamed.start
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_starting_past_the_content_delivers_nothing() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x36; 16]);
        let context = Context::from([0x36; 16]);
        let (root, _content) = tree::build(&store, partition, context, true).await;

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
        let (fragment, streamed) = read_stream(
            store,
            partition,
            Address {
                hash: root.hash,
                context,
            },
            Some(tree::CONTENT..tree::CONTENT + 10),
            no_remote(),
            sender,
            None,
        )
        .await
        .expect("an empty range is not an error here");

        assert_eq!(fragment.size_content, tree::CONTENT as u64);
        assert!(streamed.is_empty());
        assert!(
            receiver.recv().await.is_none(),
            "nothing may be sent for an empty range, and the channel must close"
        );
    }

    /// The file holds the range and is sized to it, rather than being a sparse copy of the
    /// content with the range in place.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ranged_read_into_file_writes_only_the_range() {
        let (dir, store) = make_test_store().await;
        let partition = Partition::from([0x37; 16]);
        let context = Context::from([0x37; 16]);
        let (root, content) = tree::build(&store, partition, context, true).await;

        let target = PathBuf::from(dir.as_ref()).join("ranged.bin");
        let (fragment, _metadata) = read_into_file(
            store,
            partition,
            Address {
                hash: root.hash,
                context,
            },
            target.as_path(),
            ".~loretemp",
            Some(120..330),
            no_remote(),
            None,
        )
        .await
        .expect("ranged read into file");

        assert_eq!(fragment.size_content, tree::CONTENT as u64);
        let on_disk = std::fs::read(&target).expect("read target");
        assert_eq!(on_disk, content[120..330]);
    }

    /// A range counts content bytes, not stored bytes. The two are the same for everything
    /// else in this module, and differ exactly when a fragment is compressed — so this is the
    /// one shape that can tell a content offset from a payload offset.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_range_on_a_compressed_fragment_counts_content_bytes() {
        use crate::compress::CompressionMode;

        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x39; 16]);
        let context = Context::from([0x39; 16]);

        // Compressible enough that the payload is meaningfully shorter than the content,
        // which is what makes the two offset bases distinguishable.
        let content: Vec<u8> = (0..4096).map(|index| (index / 64) as u8).collect();
        let plain = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (fragment, payload) = crate::compress::compress(plain, &content, CompressionMode::Lz4)
            .expect("compress test content");
        assert!(
            (payload.len() as u64) < fragment.size_content,
            "test needs a payload shorter than its content, got {} of {}",
            payload.len(),
            fragment.size_content,
        );

        let address = Address {
            hash: hash::hash_slice(&content),
            context,
        };
        store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put compressed fragment");

        let (read_fragment, bytes) = read(
            store,
            partition,
            address,
            Some(1000..1200),
            no_remote(),
            None,
        )
        .await
        .expect("ranged read of compressed content");

        assert_eq!(read_fragment.size_content, content.len() as u64);
        assert_eq!(bytes.as_ref(), &content[1000..1200]);
    }

    /// A ranged read fetches the spine down to the leaves it needs and nothing else, three
    /// levels deep.
    ///
    /// The store holds exactly the five fragments the range reaches out of the tree's fifteen,
    /// so this is not a claim that the walk *tends* to skip work — anything it reached for
    /// beyond them is a missing address and a failed read. `250..320` lives in `leaf[2]`
    /// (200..300) and `leaf[3]` (300..400), so the spine is root → `mid[0]` → `sub[1]`.
    ///
    /// Both read paths are driven from the one sparse store because they agree on the set:
    /// `read` prunes in `read_defragment`, `read_stream` and `read_into_file` prune in the
    /// tree walker, and the level peeks the walker adds always land on entries the range
    /// already wanted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ranged_read_of_a_three_level_tree_touches_only_its_own_spine() {
        let (dir, store) = make_test_store().await;
        let partition = Partition::from([0x3A; 16]);
        let context = Context::from([0x3A; 16]);
        let tree = three_level::build(context);

        for piece in [
            &tree.root,
            &tree.mid[0],
            &tree.sub[1],
            &tree.leaf[2],
            &tree.leaf[3],
        ] {
            piece.put(&store, partition).await;
        }
        let address = tree.root.address;
        let expected = &tree.content[250..320];

        let (fragment, bytes) = read(
            store.clone(),
            partition,
            address,
            Some(250..320),
            no_remote(),
            None,
        )
        .await
        .expect("the spine the range needs is all it needs");
        assert_eq!(fragment.size_content, three_level::CONTENT as u64);
        assert_eq!(bytes.as_ref(), expected);

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(8);
        let (_fragment, streamed) = read_stream(
            store.clone(),
            partition,
            address,
            Some(250..320),
            no_remote(),
            sender,
            None,
        )
        .await
        .expect("the streaming walk prunes to the same spine");
        assert_eq!(streamed, 250..320);
        let mut delivered = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            let chunk = chunk.expect("stream chunk");
            delivered.extend_from_slice(chunk.as_ref());
        }
        assert_eq!(delivered, expected);

        let target = PathBuf::from(dir.as_ref()).join("three-level.bin");
        read_into_file(
            store.clone(),
            partition,
            address,
            target.as_path(),
            ".~loretemp",
            Some(250..320),
            no_remote(),
            None,
        )
        .await
        .expect("the file walk prunes to the same spine");
        assert_eq!(std::fs::read(&target).expect("read target"), expected);

        // The controls: the pieces left out really are missing, so the successes above are
        // pruning rather than a tree that happens to be wholly readable.
        read(store.clone(), partition, address, None, no_remote(), None)
            .await
            .expect_err("the whole content needs subtrees the store does not hold");

        read(store, partition, address, Some(650..700), no_remote(), None)
            .await
            .expect_err("a range under the absent subtree cannot read");
    }

    /// Content small enough to live in one fragment takes the direct-write path, which sizes
    /// the file from the buffer rather than from the sink.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ranged_read_into_file_writes_only_the_range_for_one_fragment() {
        let (dir, store) = make_test_store().await;
        let (partition, address, fragment, payload) = make_input(0x38);
        store
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("put single fragment");

        let target = PathBuf::from(dir.as_ref()).join("ranged-single.bin");
        read_into_file(
            store,
            partition,
            address,
            target.as_path(),
            ".~loretemp",
            Some(8..24),
            no_remote(),
            None,
        )
        .await
        .expect("ranged read into file");

        let on_disk = std::fs::read(&target).expect("read target");
        assert_eq!(on_disk, payload[8..24]);
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
