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
use crate::mutable_store::MutableStore;
use crate::options::ReadOptions;
use crate::store_types::StoreMatch;
use crate::types::Address;
use crate::types::Context;
use crate::types::Fragment;
use crate::types::Hash;
use crate::types::KeyType;
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

/// [`remote_get_retry`] for `get_resolved`: back off on `SlowDown`, recover from a stale
/// session id by invalidating and retrying. `key` supplies error context only.
async fn remote_get_resolved_retry(
    session: &StorageSession,
    key: Hash,
    context: Context,
    flags: u32,
) -> Result<(Hash, Fragment, Bytes), StorageError> {
    let _guard = RemoteFetchGuard::new();
    let mut retry = store_retry();
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
            // The local store answered for the *root*, but a fragment list's leaves may still
            // only exist remotely, so the caller's session has to travel with it.
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

    // Verification failure gets one heal attempt then a re-resolve, as `load_fragment` does, so a
    // corrupt server-side root is no less recoverable through `get_resolved` than through `get`.
    // The retry re-resolves rather than re-reading: the heal targets the resolved address, and a
    // fresh resolve costs the same single round trip.
    let mut heal_attempted = false;
    let (resolved, address, fragment, buffer) = loop {
        let (resolved, mut fragment, buffer) =
            remote_get_resolved_retry(session.as_ref(), key, context, flags).await?;

        if resolved.is_zero() {
            // A zero value means the key was deleted; treat as a miss rather than reading
            // the zero address.
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
        // The root is the fragment list, so its own bytes are not content; the leaves are.
        let address = root.address;
        let fragment = root.fragment;
        let buffer = root.buffer;
        let remote_session = root.session;
        let report = sender.clone();
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
                lore_base::lore_warn!(
                    "error while defragmenting during read_resolved_stream: {0}",
                    err
                );
                // The size was returned before the leaves flowed, so this is the only route by
                // which the caller can learn the content it received is short.
                let _ = report.send(Err(err)).await;
            }
        });
    } else {
        sender
            .send(Ok(root.buffer))
            .await
            .map_err(|_err| StorageError::internal("stream send failed"))?;
    }

    Ok((root.resolved, root.fragment.size_content))
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
    sender: tokio::sync::mpsc::Sender<Result<Bytes, StorageError>>,
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
        let report = sender.clone();
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
                // The size was returned before the leaves flowed, so this is the only route by
                // which the caller can learn the content it received is short.
                let _ = report.send(Err(err)).await;
            }
        });

        Ok(fragment.size_content)
    } else {
        sender
            .send(Ok(buffer))
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
    use crate::local::mutable_store::LocalMutableStore;
    use crate::local::mutable_store::MutableStoreSettings;
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

    async fn make_test_stores() -> (TempDir, Arc<dyn ImmutableStore>, Arc<dyn MutableStore>) {
        let dir = TempDir::new("lore-storage-resolve-test-");
        let immutable = LocalImmutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create test immutable store");
        let mutable: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(PathBuf::from(dir.as_ref())),
                MutableStoreSettings::default(),
                immutable.clone(),
            )
            .await
            .expect("create test mutable store"),
        );
        (dir, immutable, mutable)
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

    /// A key cached under [`KeyType::Resolve`] whose root is in the local store must be served
    /// without ever needing a session, which is the whole point of caching the mapping.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resolved_serves_local_mapping_without_session() {
        let (_dir, immutable, mutable) = make_test_stores().await;
        let (partition, address, fragment, payload) = make_input(0x51);
        let key = hash::hash_slice(b"resolve-key");

        immutable
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("store root");
        mutable
            .clone()
            .store(partition, key, address.hash, KeyType::Resolve)
            .await
            .expect("store resolve mapping");

        let (resolved, bytes) = read_resolved(
            immutable,
            mutable,
            partition,
            key,
            address.context,
            0,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .expect("local mapping and root are both cached");

        assert_eq!(resolved, address.hash);
        assert_eq!(bytes.as_ref(), payload.as_ref());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_resolved_without_local_mapping_is_not_found() {
        let (_dir, immutable, mutable) = make_test_stores().await;
        let (partition, address, fragment, payload) = make_input(0x52);
        let key = hash::hash_slice(b"unmapped-key");

        immutable
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("store root");

        let err = read_resolved(
            immutable,
            mutable,
            partition,
            key,
            address.context,
            0,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .expect_err("the root is cached but nothing maps the key to it");
        assert!(
            matches!(err, StorageError::AddressNotFound(_)),
            "expected AddressNotFound, got {err:?}"
        );
    }

    /// A cached mapping whose root is absent locally must fall through to the remote resolve
    /// rather than remote-reading the cached hash, so with no session it is a plain miss.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resolved_falls_through_when_local_root_is_absent() {
        let (_dir, immutable, mutable) = make_test_stores().await;
        let (partition, address, _fragment, _payload) = make_input(0x53);
        let key = hash::hash_slice(b"dangling-resolve-key");

        mutable
            .clone()
            .store(partition, key, address.hash, KeyType::Resolve)
            .await
            .expect("store resolve mapping");

        let err = read_resolved(
            immutable,
            mutable,
            partition,
            key,
            address.context,
            0,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .expect_err("mapping resolves but its root was never stored");
        assert!(
            matches!(err, StorageError::AddressNotFound(_)),
            "expected AddressNotFound, got {err:?}"
        );
    }

    /// `no_local` handles must bypass the local mutable probe exactly as they bypass the local
    /// immutable probe, so a perfectly good local mapping is still not consulted.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_resolved_skips_local_mapping_when_local_is_disabled() {
        let (_dir, immutable, mutable) = make_test_stores().await;
        let (partition, address, fragment, payload) = make_input(0x54);
        let key = hash::hash_slice(b"bypassed-resolve-key");

        immutable
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("store root");
        mutable
            .clone()
            .store(partition, key, address.hash, KeyType::Resolve)
            .await
            .expect("store resolve mapping");

        let err = read_resolved(
            immutable,
            mutable,
            partition,
            key,
            address.context,
            0,
            None,
            ReadOptions::default().no_local(),
            None,
        )
        .await
        .expect_err("no_local must not consult the local mutable store");
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

    /// A leaf that vanishes partway through the tree must reach the consumer as an error item.
    ///
    /// `read_stream` returns the content size before any leaf flows, so a failure after that
    /// point has only the channel left to travel on. While the sink carried bare `Bytes` the
    /// channel simply closed early, and a caller could not tell a truncated read from a
    /// complete one — the pipeline's error reached the log and nothing else.
    #[tokio::test(flavor = "multi_thread")]
    async fn stream_reports_a_missing_leaf_rather_than_closing_short() {
        use lore_base::types::FragmentReference;

        use crate::typed_bytes::TypedBytes;

        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([0xA7; 16]);
        let context = Context::default();

        let payload = Bytes::from(vec![0x5Au8; 512 * 1024]);
        let written = crate::write::write_content(
            store.clone(),
            partition,
            context,
            payload.clone(),
            crate::options::WriteOptions::default().with_fixed_size_chunk(64 * 1024),
            None,
            None,
            None,
        )
        .await
        .expect("write fragmented content");

        let (root, list) = load_fragment(
            store.clone(),
            partition,
            written.address,
            ReadOptions::default(),
            None,
        )
        .await
        .expect("load the root");
        assert_eq!(
            root.flags & FragmentFlags::PayloadFragmented,
            FragmentFlags::PayloadFragmented,
            "this test needs a fragment tree, not a single fragment",
        );

        let list = list.to_aligned::<FragmentReference>();
        let leaves = list.as_type_slice::<FragmentReference>();
        assert!(leaves.len() > 1, "this test needs more than one leaf");
        let missing = Address {
            hash: leaves[1].hash,
            context,
        };
        store
            .clone()
            .obliterate(partition, missing, Arc::default())
            .await
            .expect("remove one leaf");

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let size = read_stream(
            store.clone(),
            partition,
            written.address,
            ReadOptions::default(),
            tx,
            None,
        )
        .await
        .expect("the root still loads, so the read starts");
        assert_eq!(size, payload.len() as u64);

        let mut delivered = 0usize;
        let mut reported = false;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(chunk) => delivered += chunk.len(),
                Err(_) => {
                    reported = true;
                    break;
                }
            }
        }
        assert!(
            reported,
            "a missing leaf must arrive as an error item; got {delivered} bytes then a clean close",
        );
    }
}
