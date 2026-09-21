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
use crate::store_types::PayloadRead;
use crate::store_types::StoreGetData;
use crate::types::Address;
use crate::types::Fragment;
use crate::types::Partition;

/// Load a single raw fragment from store with retry backoff. How widely the store searches for it
/// is the store's own business - see [`ImmutableStore::read_scope`].
///
/// `verified_by_caller` states that the payload is hashed once this returns, which reports
/// corruption as an error to heal from and leaves the debug check here nothing to add.
pub async fn read_raw(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    verified_by_caller: bool,
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
                    verified_by_caller
                        || match hash::hash_fragment(fragment, payload.as_ref()) {
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
        let local_result = read_raw(store.clone(), partition, address, options.verify).await;

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

/// Whether the `size` bytes of content sitting in `dst` hash to the address they were read from.
///
/// A `dst` holding fewer than `size` bytes fails rather than being read past: the content it holds
/// is not the content the address names either way.
fn content_verifies(size: usize, address: Address, dst: &mut crate::CallerBuffer) -> bool {
    let Some(content) = dst.as_mut_slice().get_mut(..size) else {
        lore_base::lore_debug!(
            "Fragment {} claims {size} bytes of content, more than its destination holds",
            address.hash
        );
        return false;
    };

    let content_hash = hash::hash_slice(content);
    if content_hash != address.hash {
        lore_base::lore_debug!(
            "Fragment {} failed verification in caller buffer, got {content_hash}",
            address.hash
        );
        return false;
    }
    true
}

/// Copy `bytes` into the start of `dst`, refusing content the destination has no room for rather
/// than truncating it.
fn copy_into_buffer(bytes: &[u8], dst: &mut crate::CallerBuffer) -> Result<usize, StorageError> {
    let capacity = dst.len();
    let Some(target) = dst.as_mut_slice().get_mut(..bytes.len()) else {
        return Err(StorageError::from(crate::errors::Oversized {
            context: format!(
                "content of {} bytes exceeds the {capacity} byte destination buffer",
                bytes.len()
            ),
        }));
    };
    target.copy_from_slice(bytes);
    Ok(bytes.len())
}

/// Whether the remote may supply the root of a read into a caller-owned buffer.
///
/// An address the caller named is authoritative, so the remote may serve it. A hash a local mutable
/// mapping resolved to is not: the mapping is trusted as it stands and may name content the key has
/// since moved off, so reading that hash from the remote would answer with content the key no
/// longer names. [`read_resolved`] re-resolves against the remote instead, which costs the same
/// round trip and answers against the authoritative mapping.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RootSource {
    LocalOrRemote,
    LocalOnly,
}

/// The payload stored under `address`, put in `dst` when the store it came from could place it
/// there, and handed back as it is stored otherwise.
///
/// `Some((fragment, None))` means `dst` already holds the content. `Some((fragment, Some(payload)))`
/// leaves `dst` untouched and hands back stored bytes the reader still has to turn into content.
/// Only the local store can place a payload directly; the remote hands its bytes over as it stored
/// them, so a compressed one can be expanded into `dst` rather than into a buffer of its own.
///
/// The remote fetch asks for neither expansion nor verification, so what it delivers is measured
/// against what it claims before either is read as a bound, and the reader verifies the content once
/// it is in place. A local miss skips the local probe the fetch would otherwise repeat.
async fn content_payload_for_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    root: RootSource,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<Option<(Fragment, Option<Bytes>)>, StorageError> {
    if options.local {
        match store.clone().get_into(partition, address, dst).await {
            Ok((fragment, PayloadRead::IntoBuffer)) => return Ok(Some((fragment, None))),
            Ok((fragment, PayloadRead::Returned(payload))) => {
                return Ok(Some((fragment, Some(payload))));
            }
            // Forwarded rather than rebuilt: rebuilding put the old error's whole `Display` in
            // the new one's context, so the caller read "Oversized: Oversized: ...".
            Err(err) if err.is_oversized() => {
                return Err(err).forward("reading the content into the caller's buffer");
            }
            Err(err) => {
                lore_base::lore_trace!(
                    "Fragment {} failed loading into caller buffer: {err:?}",
                    address.hash
                );
            }
        }
    }

    if root == RootSource::LocalOnly || !options.remote || remote_session.is_none() {
        return Ok(None);
    }

    let stored = options.no_local().no_decompress().no_verify();
    match load_fragment(store, partition, address, stored, remote_session).await {
        Ok((fragment, payload)) => {
            if let Err(err) = crate::validate_fragment_payload(&fragment, payload.len()) {
                lore_base::lore_debug!(
                    "Fragment {} arrived with sizes its payload does not match: {err:?}",
                    address.hash
                );
                return Ok(None);
            }
            Ok(Some((fragment, Some(payload))))
        }
        Err(err) => {
            lore_base::lore_trace!(
                "Fragment {} failed fetching for the caller buffer: {err:?}",
                address.hash
            );
            Ok(None)
        }
    }
}

/// Read the whole content stored under `address` into `dst`, reporting the fragment describing it
/// and the number of bytes written.
///
/// Whatever holds the content serves it where it belongs: a payload that is the content lands in
/// `dst` as it is read, a compressed one expands into `dst`, and a fragment list is the root the
/// walk starts from, its leaves written into `dst` in place. No payload is read twice and none is
/// copied that could have landed where it belongs.
///
/// `None` leaves the read to the assembling path, which loads the root itself and so verifies and
/// heals: nothing in reach held the content, what it found did not survive expansion or
/// verification, or the content is larger than `options.max_content_size` allows, which that path
/// reports. A destination too small for the content is the caller's error and fails.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_content_into_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    root: RootSource,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<Option<(Fragment, usize)>, StorageError> {
    let options = options.with_decompress();
    let Some((fragment, payload)) = content_payload_for_buffer(
        store.clone(),
        partition,
        address,
        dst,
        options,
        root,
        remote_session.clone(),
    )
    .await?
    else {
        return Ok(None);
    };

    if let Some(max) = options.max_content_size
        && fragment.size_content > max
    {
        return Ok(None);
    }

    let size_content = fragment.size_content as usize;
    let Some(payload) = payload else {
        if options.verify && !content_verifies(size_content, address, dst) {
            return Ok(None);
        }
        return Ok(Some((fragment, size_content)));
    };

    if (fragment.flags & FragmentFlags::PayloadCompressed) != 0 {
        let capacity = dst.len();
        // Sized to the content rather than to the whole destination: the expansion is bounded by
        // the slice it is given, and that bound reaches the decompressor as a `c_int`.
        let Some(target) = dst.as_mut_slice().get_mut(..size_content) else {
            return Err(StorageError::from(crate::errors::Oversized {
                context: format!(
                    "content of {size_content} bytes exceeds the {capacity} byte destination buffer"
                ),
            }));
        };
        let expanded = match compress::decompress_into_slice(fragment, &payload, target) {
            Ok(expanded) => expanded,
            Err(err) => {
                lore_base::lore_debug!(
                    "Fragment {} failed decompression into caller buffer: {err:?}",
                    address.hash
                );
                return Ok(None);
            }
        };
        if options.verify && !content_verifies(expanded.size_content as usize, address, dst) {
            return Ok(None);
        }
        return Ok(Some((expanded, expanded.size_content as usize)));
    }

    if (fragment.flags & FragmentFlags::PayloadFragmented) != FragmentFlags::PayloadFragmented {
        // Sizes that disagree describe no payload that is the content, which the assembling path
        // reports against the fragment that claimed them.
        if !crate::payload_is_content(&fragment) {
            return Ok(None);
        }
        let written = copy_into_buffer(&payload, dst)?;
        if options.verify && !content_verifies(written, address, dst) {
            return Ok(None);
        }
        return Ok(Some((fragment, written)));
    }

    // Verified and expanded, the list is the root the walk starts from, so the walk never reads it
    // again.
    let (root, list) = match decompress_and_verify(fragment, payload, address, options).await {
        Ok(result) => result,
        Err(err) => {
            lore_base::lore_debug!(
                "Fragment {} failed to open as a list: {err:?}",
                address.hash
            );
            return Ok(None);
        }
    };

    let written = deliver_root_into_buffer(
        store,
        partition,
        address,
        None,
        root,
        list,
        dst,
        options,
        depth,
        remote_session,
    )
    .await?;
    Ok(Some((root, written)))
}

/// Deliver `range` of the content rooted at `fragment` and `buffer` into `dst`, reporting the bytes
/// written.
///
/// Content spread across fragments is walked into `dst` in place, each leaf writing where it
/// belongs, so it is assembled nowhere else first. Content one fragment holds is already in
/// `buffer`, so the range is cut from it and copied in once.
#[allow(clippy::too_many_arguments)]
async fn deliver_root_into_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    fragment: Fragment,
    buffer: Bytes,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<usize, StorageError> {
    let content = resolve_content_range(range, fragment.size_content);
    if content.is_empty() {
        return Ok(0);
    }

    if (fragment.flags & FragmentFlags::PayloadFragmented) != FragmentFlags::PayloadFragmented {
        let Some(bytes) = buffer.get(content.clone()) else {
            return Err(StorageError::internal(format!(
                "content range {content:?} reaches past the {} byte root",
                buffer.len()
            )));
        };
        return copy_into_buffer(bytes, dst);
    }

    let capacity = dst.len();
    let Some(assembled) = dst.as_mut_slice().get_mut(..content.len()) else {
        return Err(StorageError::from(crate::errors::Oversized {
            context: format!(
                "content of {} bytes exceeds the {capacity} byte destination buffer",
                content.len()
            ),
        }));
    };

    // SAFETY: `dst` is borrowed for this whole call, so the memory the walk divides among the
    // leaves stays valid and reaches nobody else, and the slice it is taken from bounds it.
    let target = unsafe { crate::CallerBuffer::new(assembled.as_mut_ptr(), assembled.len()) };
    read_defragment(
        store,
        partition,
        address,
        content.clone(),
        fragment,
        buffer,
        target,
        options,
        depth,
        remote_session,
    )
    .await?;

    Ok(content.len())
}

/// [`read`] delivering into a caller-owned buffer: the root is loaded once and the content taken
/// from it, so content spanning fragments is assembled nowhere but in `dst`.
#[allow(clippy::too_many_arguments)]
async fn read_range_into_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(Fragment, usize), StorageError> {
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

    let written = deliver_root_into_buffer(
        store,
        partition,
        address,
        range,
        fragment,
        buffer,
        dst,
        options,
        depth,
        remote_session,
    )
    .await?;
    Ok((fragment, written))
}

/// [`read`] delivering the content into a caller-owned buffer.
///
/// `dst.len()` states the capacity and bounds the read: content exceeding it fails with `Oversized`
/// rather than truncating. Returns the fragment describing the whole content and the number of
/// bytes written.
///
/// A whole read of content one fragment holds goes straight into `dst`, allocating nothing beyond
/// the compressed payload where there is one. Content spanning fragments is walked into `dst` leaf
/// by leaf. A range of content one fragment holds is cut from that fragment and copied in once.
/// Every path verifies against the address when `options.verify` is set.
///
/// The contents of `dst` are unspecified when this fails.
pub async fn read_into_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(Fragment, usize), StorageError> {
    // A range is cut from assembled content, so only a whole read is served by the payload as the
    // store holds it.
    if range.is_none()
        && let Some(read) = read_content_into_buffer(
            store.clone(),
            partition,
            address,
            dst,
            options,
            RootSource::LocalOrRemote,
            0,
            remote_session.clone(),
        )
        .await?
    {
        return Ok(read);
    }

    read_range_into_buffer(
        store,
        partition,
        address,
        range,
        dst,
        options,
        0,
        remote_session,
    )
    .await
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

    let metadata = write_root_to_file(
        store,
        partition,
        address,
        fragment,
        buffer,
        path,
        temp_file_extension,
        range,
        options,
        remote_session,
    )
    .await?;

    Ok((fragment, metadata))
}

/// Write the content a loaded root fragment stands for into `path`: the half of
/// [`read_into_file`] that follows the load, and the half [`read_resolved_into_file`] reaches
/// through a resolve rather than through an address.
///
/// A single fragment's payload is already in `buffer` and goes out in one whole-file write, whose
/// open handle answers the metadata the caller gets back. A fragment list streams through the
/// defragment pipeline into a file sized to the range up front, so each leaf lands at its own
/// offset and peak memory follows the leaf rather than the content — and no metadata comes back,
/// because the handle moves into the pipeline. That path stages through
/// `<path><temp_file_extension>` and renames, unless `options.direct_write` asks for the target to
/// be written in place.
///
/// A range starting past the end of the content selects nothing that exists, and `path` is left
/// alone: the file is not opened, so a destination that was already there survives a request the
/// caller got wrong. Nothing is written and no metadata comes back, and the caller decides what an
/// empty selection means from the fragment it holds. A start exactly at the end is a legitimate
/// empty read and does produce an empty file.
///
/// `options` must already carry `with_decompress`; both entry points load their root with it.
#[allow(clippy::too_many_arguments)]
async fn write_root_to_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    path: &Path,
    temp_file_extension: &str,
    range: Option<Range<usize>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<Option<std::fs::Metadata>, StorageError> {
    if range
        .as_ref()
        .is_some_and(|range| range.start as u64 > fragment.size_content)
    {
        return Ok(None);
    }

    let range = resolve_content_range(range, fragment.size_content);

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
            let rename_err_msg = format!("rename {} -> {}", file_path.display(), path.display());
            lore_io::IoDriver::global()
                .rename(file_path.as_path(), path)
                .await
                .map_err(|e| StorageError::internal_with_context(e, &rename_err_msg))?;

            if let Some(temporary) = temporary.as_mut() {
                temporary.renamed();
            }
        }

        Ok(None)
    } else {
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
        Ok(Some(metadata))
    }
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

/// [`read_into_buffer`] reached by key rather than by address.
///
/// `dst.len()` states the capacity and bounds the read: content exceeding it fails with `Oversized`
/// rather than truncating. Returns the resolved hash and the number of bytes written.
///
/// A key resolving locally to content one fragment holds reads straight into `dst`. Anything else
/// falls back to [`read_resolved`], which assembles no more than `dst` can hold.
///
/// The contents of `dst` are unspecified when this fails.
#[allow(clippy::too_many_arguments)]
pub async fn read_resolved_into_buffer(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> Result<(Hash, usize), StorageError> {
    // [`read_resolved`] resolves the key again, so resolve here only while the direct read is open.
    // The root has to be local: the mapping just read is trusted as it stands, and reading the hash
    // it names from the remote would answer with content the key may have moved off. A local root's
    // leaves may still be remote, as [`resolve_root`] allows.
    if options.local
        && let Ok(resolved) = mutable.clone().load(partition, key, KeyType::Resolve).await
        && !resolved.is_zero()
    {
        let address = Address {
            hash: resolved,
            context,
        };
        if let Some((_fragment, written)) = read_content_into_buffer(
            store.clone(),
            partition,
            address,
            dst,
            options,
            RootSource::LocalOnly,
            0,
            session.clone(),
        )
        .await?
        {
            return Ok((resolved, written));
        }
    }

    // The resolve here is authoritative, so the address it answers with is read the way any other
    // named address is.
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

    let options = options.with_decompress();
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

    let written = deliver_root_into_buffer(
        store,
        partition,
        root.address,
        None,
        root.fragment,
        root.buffer,
        dst,
        options,
        0,
        root.session,
    )
    .await?;
    Ok((root.resolved, written))
}

/// [`read_resolved`] writing the content into a file instead of reassembling it in memory —
/// [`read_into_file`] reached by key rather than by address.
///
/// Returns the resolved hash and the fragment describing the whole content, so a caller can
/// report the address the key names and check its range against what exists without stating the
/// file it just wrote.
///
/// The round trip is the one [`resolve_root`] pays: the key and the root fragment it names come
/// back together, so nothing is spent resolving before the read starts. From there the content
/// goes to disk the way [`read_into_file`] puts it there — a single fragment in one write, a
/// fragment list leaf by leaf at its own offset through the defragment pipeline — so neither the
/// caller nor this library ever holds the whole content, and the file is the only place it is
/// assembled.
///
/// `range` selects the content to write and the file holds exactly that range from its first
/// byte, as [`read_into_file`]'s does; only the leaves the range covers are fetched.
///
/// A key with no mapping, or one naming content that cannot be read, fails without touching
/// `path` — there is no zero-hash truncation here, because a resolve that finds nothing is a miss
/// rather than an address for empty content.
#[allow(clippy::too_many_arguments)]
pub async fn read_resolved_into_file(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    path: &Path,
    temp_file_extension: &str,
    range: Option<Range<usize>>,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> Result<(Hash, Fragment), StorageError> {
    let _count_permit = file_count_limit_acquire()
        .await
        .forward::<StorageError>("permit failed")?;

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

    write_root_to_file(
        store,
        partition,
        root.address,
        root.fragment,
        root.buffer,
        path,
        temp_file_extension,
        range,
        options,
        root.session,
    )
    .await?;

    Ok((root.resolved, root.fragment))
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
    use crate::CallerBuffer;
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

    /// Store `payload` as one unfragmented, uncompressed fragment addressed by its own hash.
    async fn put_whole(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        payload: &Bytes,
    ) -> Address {
        let address = Address {
            hash: hash::hash_slice(payload.as_ref()),
            context,
        };
        store
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put content");
        address
    }

    /// Store `size` bytes of compressible content as one compressed fragment, reporting its address
    /// and the content it expands to.
    async fn put_compressed(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        size: usize,
    ) -> (Address, Vec<u8>) {
        let content: Vec<u8> = (0..size).map(|index| (index / 64) as u8).collect();
        let plain = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (fragment, payload) =
            crate::compress::compress(plain, &content, crate::compress::CompressionMode::Lz4)
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
        (address, content)
    }

    /// Store content spanning two leaves and the list naming them, reporting the list's address and
    /// the content the two leaves reassemble to.
    async fn put_two_leaf_list(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
    ) -> (Address, Vec<u8>) {
        use zerocopy::IntoBytes;

        use crate::types::FragmentReference;

        let first = Bytes::from((0u8..64).collect::<Vec<u8>>());
        let second = Bytes::from((64u8..128).collect::<Vec<u8>>());
        let first_address = put_whole(store, partition, context, &first).await;
        let second_address = put_whole(store, partition, context, &second).await;

        let refs_payload = Bytes::copy_from_slice(
            [
                FragmentReference {
                    hash: first_address.hash,
                    offset_content: 0,
                },
                FragmentReference {
                    hash: second_address.hash,
                    offset_content: first.len() as u64,
                },
            ]
            .as_bytes(),
        );
        let content_size = (first.len() + second.len()) as u64;
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
                    size_content: content_size,
                },
                Some(refs_payload),
                false,
            )
            .await
            .expect("put the fragment list");

        let mut content = first.to_vec();
        content.extend_from_slice(&second);
        (root_address, content)
    }

    /// Read through [`read_into_buffer`] into a `capacity` byte buffer, reporting the buffer
    /// alongside the number of bytes written.
    async fn read_into_vec(
        store: Arc<dyn ImmutableStore>,
        partition: Partition,
        address: Address,
        range: Option<Range<usize>>,
        capacity: usize,
        options: ReadOptions,
    ) -> Result<(Vec<u8>, usize), StorageError> {
        let mut buffer = vec![0u8; capacity];
        // SAFETY: the buffer outlives the read and nothing else touches it.
        let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
        let (_fragment, written) =
            read_into_buffer(store, partition, address, range, &mut dst, options, None).await?;
        Ok((buffer, written))
    }

    /// Delegating store counting how each payload reached the reader: `get` hands back a buffer it
    /// allocated, `get_into` places the bytes where the reader asked for them.
    ///
    /// The bytes delivered are the same either way, so a read that stopped landing in the caller's
    /// buffer would still return the right content. The counts are what tells the two apart.
    struct CountingReadStore {
        inner: Arc<dyn ImmutableStore>,
        gets: Arc<std::sync::atomic::AtomicUsize>,
        gets_into: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingReadStore {
        fn wrap(inner: Arc<dyn ImmutableStore>) -> (Arc<dyn ImmutableStore>, Self) {
            let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let gets_into = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counts = CountingReadStore {
                inner: inner.clone(),
                gets: gets.clone(),
                gets_into: gets_into.clone(),
            };
            let store: Arc<dyn ImmutableStore> = Arc::new(CountingReadStore {
                inner,
                gets,
                gets_into,
            });
            (store, counts)
        }

        fn gets(&self) -> usize {
            self.gets.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn gets_into(&self) -> usize {
            self.gets_into.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ImmutableStore for CountingReadStore {
        async fn get(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<crate::store_types::StoreGetData, StoreError> {
            self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.clone().get(partition, address).await
        }

        async fn get_into(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            dst: &mut crate::CallerBuffer,
        ) -> Result<(Fragment, PayloadRead), StoreError> {
            self.gets_into
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.clone().get_into(partition, address, dst).await
        }

        fn is_local(&self) -> bool {
            self.inner.clone().is_local()
        }

        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<crate::store_types::StoreGetData, StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        async fn query(
            self: Arc<Self>,
            partition: Partition,
            addresses: &[Address],
            results: &mut [crate::store_types::StoreMatchResult],
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn put(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            fragment: Fragment,
            payload: Option<Bytes>,
            force: bool,
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await
        }

        async fn obliterate(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            stats: Arc<crate::store_types::StoreObliterateStats>,
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .obliterate(partition, address, stats)
                .await
        }

        async fn evict(
            self: Arc<Self>,
            max_capacity: usize,
            sync_data: bool,
            sink: Option<crate::gc_event::GcEventSinkRef>,
        ) -> Result<usize, StoreError> {
            self.inner
                .clone()
                .evict(max_capacity, sync_data, sink)
                .await
        }

        async fn compact(
            self: Arc<Self>,
            max_size: usize,
            at: Option<usize>,
            sync_data: bool,
            sink: Option<crate::gc_event::GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            self.inner
                .clone()
                .compact(max_size, at, sync_data, sink)
                .await
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            self.inner.clone().compact_resume_at().await
        }

        fn max_query_batch(&self) -> Option<usize> {
            None
        }

        async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
            self.inner.clone().flush(sync_data).await
        }

        async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
            self.inner.clone().verify(heal).await
        }

        async fn copy(
            self: Arc<Self>,
            source_partition: Partition,
            source_address: Address,
            destination_partition: Partition,
            destination_context: Context,
            durable: bool,
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .copy(
                    source_partition,
                    source_address,
                    destination_partition,
                    destination_context,
                    durable,
                )
                .await
        }
    }

    /// A payload that is the content reaches the caller's buffer without a buffer being allocated
    /// for it anywhere: the store places it, and nothing asks for it a second time.
    #[tokio::test]
    async fn a_whole_read_allocates_no_buffer_for_the_payload() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x81; 16]);
        let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
        let address = put_whole(&store, partition, Context::from([0x81; 16]), &payload).await;

        let (counting, counts) = CountingReadStore::wrap(store);
        let (buffer, written) = read_into_vec(
            counting,
            partition,
            address,
            None,
            payload.len(),
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read into the caller buffer");

        assert_eq!(buffer.as_slice(), payload.as_ref());
        assert_eq!(written, payload.len());
        assert_eq!(
            counts.gets_into(),
            1,
            "the payload was placed more than once"
        );
        assert_eq!(
            counts.gets(),
            0,
            "a buffer was allocated for a payload the store could have placed"
        );
    }

    /// A compressed payload is read once and expanded into the caller's buffer, so nothing reads it
    /// again to expand it elsewhere.
    #[tokio::test]
    async fn a_compressed_read_reads_its_payload_once() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x82; 16]);
        let (address, content) =
            put_compressed(&store, partition, Context::from([0x82; 16]), 4096).await;

        let (counting, counts) = CountingReadStore::wrap(store);
        let (buffer, written) = read_into_vec(
            counting,
            partition,
            address,
            None,
            content.len(),
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read compressed content into the caller buffer");

        assert_eq!(buffer, content);
        assert_eq!(written, content.len());
        assert_eq!(counts.gets_into(), 1, "the payload was read more than once");
        assert_eq!(
            counts.gets(),
            0,
            "the compressed payload was fetched a second time to expand it"
        );
    }

    /// The list the walk starts from is the one the lookup read, and each leaf is read into its own
    /// place in the caller's buffer, so a fragmented read hands no payload back in a buffer of its
    /// own.
    #[tokio::test]
    async fn a_fragmented_read_places_every_payload() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x83; 16]);
        let context = Context::from([0x83; 16]);
        let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

        let (counting, counts) = CountingReadStore::wrap(store);
        let (buffer, written) = read_into_vec(
            counting,
            partition,
            root_address,
            None,
            content.len(),
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read fragmented content into the caller buffer");

        assert_eq!(buffer, content);
        assert_eq!(written, content.len());
        assert_eq!(
            counts.gets_into(),
            3,
            "the list and its two leaves are each read once"
        );
        assert_eq!(
            counts.gets(),
            0,
            "a buffer was allocated for a payload the store could have placed"
        );
    }

    /// A range spanning leaves is walked into the caller's buffer, which holds the range from its
    /// first byte rather than the content the range was cut from. Both leaves are clipped, so both
    /// are loaded and cut.
    #[tokio::test]
    async fn a_ranged_fragmented_read_assembles_into_the_caller_buffer() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x88; 16]);
        let context = Context::from([0x88; 16]);
        let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            root_address,
            Some(32..96),
            64,
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read a range of fragmented content into the caller buffer");

        assert_eq!(written, 64);
        assert_eq!(buffer.as_slice(), &content[32..96]);
    }

    /// A range covering a whole leaf reads that leaf into place, and reads no leaf the range does
    /// not cover.
    #[tokio::test]
    async fn a_ranged_fragmented_read_places_a_whole_leaf() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x8a; 16]);
        let context = Context::from([0x8a; 16]);
        let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

        let (counting, counts) = CountingReadStore::wrap(store);
        let (buffer, written) = read_into_vec(
            counting,
            partition,
            root_address,
            Some(64..128),
            64,
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read a whole leaf of fragmented content into the caller buffer");

        assert_eq!(written, 64);
        assert_eq!(buffer.as_slice(), &content[64..128]);
        assert_eq!(
            counts.gets_into(),
            1,
            "the leaf the range covers was not read into place"
        );
        assert_eq!(
            counts.gets(),
            1,
            "only the list itself is handed back in a buffer of its own"
        );
    }

    /// A range of content one fragment holds is cut from that fragment once it is expanded, and the
    /// buffer holds the range from its first byte.
    #[tokio::test]
    async fn a_ranged_compressed_read_delivers_only_the_range() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x89; 16]);
        let (address, content) =
            put_compressed(&store, partition, Context::from([0x89; 16]), 4096).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            address,
            Some(1000..1200),
            200,
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read a range of compressed content into the caller buffer");

        assert_eq!(written, 200);
        assert_eq!(buffer.as_slice(), &content[1000..1200]);
    }

    /// The payload of an unfragmented, uncompressed fragment is the content, so a whole read lands
    /// in the caller's buffer directly.
    #[tokio::test]
    async fn a_whole_read_lands_in_the_caller_buffer() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x11; 16]);
        let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
        let address = put_whole(&store, partition, Context::from([0x11; 16]), &payload).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            address,
            None,
            payload.len(),
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read into the caller buffer");

        assert_eq!(written, payload.len());
        assert_eq!(buffer.as_slice(), payload.as_ref());
    }

    /// A buffer short of the content is refused rather than filled with a prefix of it.
    #[tokio::test]
    async fn a_buffer_shorter_than_the_content_is_refused() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x22; 16]);
        let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
        let address = put_whole(&store, partition, Context::from([0x22; 16]), &payload).await;

        let mut buffer = vec![0u8; payload.len() - 1];
        // SAFETY: the buffer outlives the read and nothing else touches it.
        let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
        let result = read_into_buffer(
            store,
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default().no_remote(),
            None,
        )
        .await;

        assert!(
            matches!(result, Err(StorageError::Oversized(_))),
            "a buffer short of the content was not refused"
        );
        assert!(
            buffer.iter().all(|byte| *byte == 0),
            "a refused read wrote a prefix into the buffer"
        );
    }

    /// A range is cut from the content, so the buffer holds the range rather than the whole.
    #[tokio::test]
    async fn a_ranged_read_delivers_only_the_range() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x33; 16]);
        let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
        let address = put_whole(&store, partition, Context::from([0x33; 16]), &payload).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            address,
            Some(8..24),
            16,
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read a range into the caller buffer");

        assert_eq!(written, 16);
        assert_eq!(buffer.as_slice(), &payload[8..24]);
    }

    /// A compressed payload is not the content, so it comes back as its own buffer and expands
    /// straight into the caller's, without a second buffer for the content in between.
    #[tokio::test]
    async fn a_compressed_read_expands_into_the_caller_buffer() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x77; 16]);
        let (address, content) =
            put_compressed(&store, partition, Context::from([0x77; 16]), 4096).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            address,
            None,
            content.len(),
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read compressed content into the caller buffer");

        assert_eq!(written, content.len());
        assert_eq!(buffer, content);
    }

    /// Content spread across leaves is written into the caller's buffer in place, each leaf at its
    /// own offset, so the buffer holds the whole content in content order.
    #[tokio::test]
    async fn a_fragmented_read_is_assembled_into_the_caller_buffer() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x44; 16]);
        let context = Context::from([0x44; 16]);
        let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            root_address,
            None,
            content.len(),
            ReadOptions::default().no_remote(),
        )
        .await
        .expect("read fragmented content into the caller buffer");

        assert_eq!(written, content.len());
        assert_eq!(buffer, content);
    }

    /// `no_verify` delivers the stored bytes without hashing them. A verifying read of the same
    /// address rejects them, so the flag has to reach the read that fills the buffer.
    #[tokio::test]
    async fn an_unverified_read_skips_the_hash_check() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x55; 16]);
        let payload = Bytes::from_static(b"bytes that do not hash to their address");
        let address = Address {
            hash: hash::hash_slice(b"other content"),
            context: Context::from([0x55; 16]),
        };
        store
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put content under a mismatched address");

        let (buffer, written) = read_into_vec(
            store,
            partition,
            address,
            None,
            payload.len(),
            ReadOptions::default().no_remote().no_verify(),
        )
        .await
        .expect("an unverified read delivers the stored bytes");

        assert_eq!(written, payload.len());
        assert_eq!(buffer.as_slice(), payload.as_ref());
    }

    /// A compressed payload expands into the caller's buffer whether or not the content is hashed
    /// afterwards.
    #[tokio::test]
    async fn an_unverified_compressed_read_expands_into_the_caller_buffer() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x84; 16]);
        let (address, content) =
            put_compressed(&store, partition, Context::from([0x84; 16]), 4096).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            address,
            None,
            content.len(),
            ReadOptions::default().no_remote().no_verify(),
        )
        .await
        .expect("read compressed content without verifying it");

        assert_eq!(written, content.len());
        assert_eq!(buffer, content);
    }

    /// Leaves are verified as they are loaded, so a list walk delivers the same content whether or
    /// not the caller asked for verification.
    #[tokio::test]
    async fn an_unverified_fragmented_read_assembles_into_the_caller_buffer() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x85; 16]);
        let context = Context::from([0x85; 16]);
        let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

        let (buffer, written) = read_into_vec(
            store,
            partition,
            root_address,
            None,
            content.len(),
            ReadOptions::default().no_remote().no_verify(),
        )
        .await
        .expect("read fragmented content without verifying it");

        assert_eq!(written, content.len());
        assert_eq!(buffer, content);
    }

    /// The capacity is measured against the content a compressed payload expands to, not against
    /// the payload, so a buffer that only fits the compressed form is refused before it expands.
    #[tokio::test]
    async fn a_buffer_shorter_than_expanded_content_is_refused() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x86; 16]);
        let (address, content) =
            put_compressed(&store, partition, Context::from([0x86; 16]), 4096).await;

        let mut buffer = vec![0u8; content.len() - 1];
        // SAFETY: the buffer outlives the read and nothing else touches it.
        let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
        let result = read_into_buffer(
            store,
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default().no_remote(),
            None,
        )
        .await;

        assert!(
            matches!(result, Err(StorageError::Oversized(_))),
            "a buffer short of the expanded content was not refused"
        );
        assert!(
            buffer.iter().all(|byte| *byte == 0),
            "a refused read expanded into the buffer"
        );
    }

    /// A list is refused against the content it reassembles to, before any leaf is fetched for a
    /// buffer that cannot hold them.
    #[tokio::test]
    async fn a_buffer_shorter_than_assembled_content_is_refused() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0x87; 16]);
        let context = Context::from([0x87; 16]);
        let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

        let mut buffer = vec![0u8; content.len() - 1];
        // SAFETY: the buffer outlives the read and nothing else touches it.
        let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
        let result = read_into_buffer(
            store,
            partition,
            root_address,
            None,
            &mut dst,
            ReadOptions::default().no_remote(),
            None,
        )
        .await;

        assert!(
            matches!(result, Err(StorageError::Oversized(_))),
            "a buffer short of the assembled content was not refused"
        );
        assert!(
            buffer.iter().all(|byte| *byte == 0),
            "a refused read assembled into the buffer"
        );
    }

    /// A key resolving locally to a payload that is already the content reads straight into the
    /// caller's buffer, reporting the hash it resolved to.
    #[tokio::test]
    async fn a_resolved_read_lands_in_the_caller_buffer() {
        use crate::local::mutable_store::LocalMutableStore;
        use crate::local::mutable_store::MutableStoreSettings;

        let (dir, store) = make_test_store().await;
        let partition = Partition::from([0x66; 16]);
        let context = Context::from([0x66; 16]);
        let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
        let address = put_whole(&store, partition, context, &payload).await;

        let mutable: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(PathBuf::from(dir.as_ref())),
                MutableStoreSettings::default(),
                store.clone(),
            )
            .await
            .expect("create mutable store"),
        );
        let key = hash::hash_slice(b"a key naming the content");
        mutable
            .clone()
            .store(partition, key, address.hash, KeyType::Resolve)
            .await
            .expect("publish the mapping");

        let mut buffer = vec![0u8; payload.len()];
        // SAFETY: the buffer outlives the read and nothing else touches it.
        let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
        let (resolved, written) = read_resolved_into_buffer(
            store,
            mutable,
            partition,
            key,
            context,
            0,
            &mut dst,
            ReadOptions::default().no_remote(),
            None,
        )
        .await
        .expect("read the resolved content into the caller buffer");

        assert_eq!(resolved, address.hash);
        assert_eq!(written, payload.len());
        assert_eq!(buffer.as_slice(), payload.as_ref());
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
