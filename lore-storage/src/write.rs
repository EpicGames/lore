// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use bytes::BytesMut;
use dashmap::DashMap;
use dashmap::Entry;
use lore_base::types::KeyType;
use lore_error_set::prelude::*;
use lore_transport::StorageSession;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zerocopy::FromZeros;

use crate::compress::COMPRESSION_MODE;
use crate::concurrency::file_count_limit_acquire;
use crate::error::StorageError;
use crate::errors::SlowDown;
use crate::fragment_engine::write_fragmented;
use crate::fragment_flags::FragmentFlags;
use crate::hash;
use crate::immutable_store::ImmutableStore;
use crate::immutable_store::StoreError;
use crate::immutable_store::query_one;
use crate::mutable_store::MutableStore;
use crate::options::ReadOptions;
use crate::options::WriteOptions;
use crate::read::load_fragment;
use crate::store_types::StoreGetData;
use crate::store_types::StoreMatch;
use crate::store_types::StoreMatchResult;
use crate::typed_bytes::TypedBytes;
use crate::types::Address;
use crate::types::Context;
use crate::types::Fragment;
use crate::types::FragmentReference;
use crate::types::Hash;
use crate::types::Partition;
use crate::write_stats::FragmentWriteStats;
use crate::write_tracker::WriteContext;
use crate::write_tracker::WriteTracker;

/// Write a single raw fragment to the local store with retry backoff.
pub async fn write_raw(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> Result<(), StorageError> {
    let mut retry = crate::store_retry();
    loop {
        match store
            .clone()
            .put(partition, address, fragment, payload.clone(), false)
            .await
        {
            Ok(_) => {
                return Ok(());
            }
            Err(StoreError::SlowDown(_)) => {
                if !retry.wait().await {
                    return Err(StorageError::from(SlowDown));
                }
            }
            Err(err) => {
                return Err(err).forward("store put failed");
            }
        }
    }
}

// This map holds the set of unique (partition, address) pairs that are currently
// in flight to be stored locally, and a token to wait for completion
static STORE_IN_FLIGHT: OnceLock<DashMap<StoreInFlightKey, CancellationToken>> = OnceLock::new();

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct StoreInFlightKey {
    pub partition: Partition,
    pub address: Address,
}

/// RAII guard that removes the in-flight entry and notifies waiters on drop.
pub struct StoreInFlightGuard {
    key: StoreInFlightKey,
}

impl Drop for StoreInFlightGuard {
    fn drop(&mut self) {
        if let Some(in_flight) = STORE_IN_FLIGHT.get()
            && let Some((_, token)) = in_flight.remove(&self.key)
        {
            // Let waiters know we have finished the request that was in flight
            token.cancel();
        }
    }
}

// Either returns a new in-flight token if request was not in flight, or waits for the request
// to finish and then return none if already in-flight
pub async fn stored_in_flight(
    partition: Partition,
    address: Address,
) -> Option<StoreInFlightGuard> {
    match try_acquire_in_flight(partition, address) {
        Ok(guard) => Some(guard),
        Err(token) => {
            token.cancelled().await;
            None
        }
    }
}

/// Non-blocking attempt to acquire the in-flight guard for `(partition, address)`.
///
/// Returns `Ok(guard)` if no one else is currently writing this address — the
/// caller becomes the leader and must drop the guard when the terminal store
/// entry is written.
///
/// Returns `Err(token)` if another task already holds the guard. The token is
/// cancelled when that task drops its guard; callers that want to observe the
/// leader's outcome should await the token and then query the store.
pub fn try_acquire_in_flight(
    partition: Partition,
    address: Address,
) -> Result<StoreInFlightGuard, CancellationToken> {
    let key = StoreInFlightKey { partition, address };
    let in_flight = STORE_IN_FLIGHT.get_or_init(DashMap::new);
    // `DashMap::entry` is safe here as it is not held across any awaits and no other locks are acquired while held
    #[allow(clippy::disallowed_methods)]
    match in_flight.entry(key.clone()) {
        Entry::Occupied(entry) => Err(entry.get().clone()),
        Entry::Vacant(entry) => {
            entry.insert(CancellationToken::new());
            Ok(StoreInFlightGuard { key })
        }
    }
}

/// If another task is currently writing `(partition, address)` via the tracker
/// path, wait for its cancellation token so subsequent reads observe the
/// terminal store entry the leader produces. Returns immediately when no
/// write is in flight.
///
/// Readers call this before hitting the store so a same-operation commit that
/// dispatches a leader and then reads the just-written fragment back (e.g.,
/// `weave_history` loading the delta block that `generate_delta_block` just
/// handed to the tracker) doesn't race ahead of the background write.
pub async fn wait_if_in_flight(partition: Partition, address: Address) {
    let Some(in_flight) = STORE_IN_FLIGHT.get() else {
        return;
    };
    let key = StoreInFlightKey { partition, address };
    let token = in_flight.get(&key).map(|entry| entry.value().clone());
    if let Some(token) = token {
        token.cancelled().await;
    }
}

/// Result of a [`store_fragment`] operation.
///
/// The stored representation is deliberately not reported. A dispatched write returns before its
/// leader has compressed anything, so the only representation available at that point is the one
/// the caller passed in, and handing that back says nothing the caller did not already know. What
/// the caller cannot know is where the payload ended up, which is what this carries instead.
///
/// The two storage flags describe the state as of this call returning, so a dispatched write
/// reports both as `false`: its leader has not run yet.
pub struct StoreResult {
    pub address: Address,
    /// Size of the uncompressed and reassembled content the address stands for. Invariant across
    /// compression, chunking and deduplication, so it is the one size worth reporting.
    pub size_content: u64,
    /// Whether the local store holds the payload.
    pub stored_local: bool,
    /// Whether the payload reached durable storage.
    pub stored_durable: bool,
    /// Whether the content was already stored, so no upload was needed.
    pub deduplicated: bool,
    /// Whether a `KeyType::Resolve` mapping was published in the same remote command that
    /// uploaded the content. Only a write that asked to publish can set this, and only when it
    /// performed the upload itself -- content already durable uploads nothing, so its key still
    /// needs a mapping write of its own.
    pub published: bool,
}

/// [`write_content`] plus publication of `key` as a `KeyType::Resolve` mapping to the content's
/// hash — the write [`crate::read::read_resolved`] reads back.
///
/// The local store always receives both the content and the mapping. A `remote_session` also
/// publishes them remotely — supplying one *is* the request to go remote, decided by the caller
/// one layer up rather than by any flag in `flags`. Publication takes one of two routes,
/// depending on how the content fragments:
///
/// - A buffer that fits one fragment goes up as a single `put_resolved`, so the content and the
///   mapping travel in one command — one round trip instead of two, which is the case this
///   operation exists for. The upload happens inside the ordinary write pipeline rather than
///   after it, so the content is compressed once and the local store is written once, already
///   carrying the durable flag and the `local_cache_priority` retention decision. Content that is
///   already durable uploads nothing, so its key follows as a `mutable_store` instead — still one
///   round trip, without re-sending a payload the server has.
/// - A fragmented buffer goes through the ordinary path so its leaves upload as usual, and the
///   mapping follows as a `mutable_store` — but only once the aggregate placement confirms every
///   fragment reached the remote. Fusing the *root* instead would publish the key when the root
///   stores, while a leaf may still have failed.
///
/// Either way the mapping is only published remotely once the content it names is there, so a key
/// never resolves to content the server does not hold. A content upload that fails still leaves a
/// successful local write: the remote leg is best-effort, so its failure is warned rather than
/// returned, and the caller reads `stored_durable == false` to tell the difference. The local
/// mapping is stored regardless, which is what makes the content readable back on this host.
///
/// An empty `buffer` **removes** the mapping rather than publishing one, which is the same
/// operation with no content: the zero hash is the mutable store's tombstone, and
/// [`crate::read::read_resolved`] already reports a zero resolved value as a miss. A delete
/// clears the local mapping first, inverting the publish ordering: if the remote call then
/// fails, the read falls through to the remote, which still holds the live mapping, rather than
/// this store serving a mapping the server has already dropped.
#[allow(clippy::too_many_arguments)]
pub async fn write_resolved(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
) -> Result<StoreResult, StorageError> {
    if key.is_zero() {
        return Err(StorageError::internal(
            "a zero key cannot be published; it is the mutable store's tombstone value",
        ));
    }

    if buffer.is_empty() {
        let address = Address {
            hash: Hash::default(),
            context,
        };
        mutable
            .store(partition, key, Hash::default(), KeyType::Resolve)
            .await
            .map_err(|err| {
                StorageError::internal_with_context(err, "failed to remove local resolve mapping")
            })?;
        let mut remote_cleared = false;
        if let Some(session) = remote_session {
            session
                .put_resolved(&key, address, Fragment::default(), None)
                .await
                .map_err(|err| crate::error::protocol_error_to_storage(err, address))?;
            remote_cleared = true;
        }
        return Ok(StoreResult {
            address,
            size_content: 0,
            stored_local: true,
            stored_durable: remote_cleared,
            deduplicated: false,
            published: false,
        });
    }

    let fuse_with_mapping =
        buffer.len() <= crate::compress::FRAGMENT_SIZE_THRESHOLD && remote_session.is_some();

    let written = if fuse_with_mapping {
        write_content_publishing(
            store,
            partition,
            context,
            buffer,
            flags,
            remote_session.clone(),
            writes,
            None,
            key,
        )
        .await?
    } else {
        write_content(
            store,
            partition,
            context,
            buffer,
            flags,
            remote_session.clone(),
            writes,
            None,
        )
        .await?
    };
    let address = written.address;
    let stored_local = written.stored_local;
    let stored_durable = written.stored_durable;

    if let Some(session) = remote_session {
        if written.published {
            lore_base::lore_trace!("Key {key} published with the upload of {address}");
        } else if stored_durable {
            session
                .mutable_store(key, address.hash, KeyType::Resolve)
                .await
                .map_err(|err| crate::error::protocol_error_to_storage(err, address))?;
        } else {
            lore_base::lore_warn!(
                "Key {key} not published remotely: content {address} is not stored remotely"
            );
        }
    }

    mutable
        .store(partition, key, address.hash, KeyType::Resolve)
        .await
        .map_err(|err| {
            StorageError::internal_with_context(err, "failed to publish local resolve mapping")
        })?;

    Ok(StoreResult {
        address,
        size_content: written.size_content,
        stored_local,
        stored_durable,
        deduplicated: written.deduplicated,
        published: false,
    })
}

/// Put a fragment to a remote session with retry on `SlowDown`.
///
/// Takes an owned `Arc<StorageSession>` so callers can spawn this into a
/// background task (the returned future must be `'static`).
/// [`remote_put_retry`] for the command that uploads a fragment and publishes `key` against it in
/// one round trip. Same back-off, because a resolved write is throttled by the server exactly as
/// an ordinary upload is.
async fn remote_put_resolved_retry(
    session: Arc<StorageSession>,
    key: Hash,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> Result<(), StorageError> {
    let mut retry = crate::store_retry();
    loop {
        match session
            .put_resolved(&key, address, fragment, payload.clone())
            .await
        {
            Ok(_) => return Ok(()),
            Err(ref e) if e.is_slow_down() => {
                if !retry.wait().await {
                    return Err(StorageError::from(SlowDown));
                }
            }
            Err(err) => return Err(crate::error::protocol_error_to_storage(err, address)),
        }
    }
}

async fn remote_put_retry(
    session: Arc<StorageSession>,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> Result<(), StorageError> {
    let mut retry = crate::store_retry();
    loop {
        match session.put(address, fragment, payload.clone()).await {
            Ok(_) => return Ok(()),
            Err(ref e) if e.is_slow_down() => {
                if !retry.wait().await {
                    return Err(StorageError::from(SlowDown));
                }
            }
            Err(err) => return Err(crate::error::protocol_error_to_storage(err, address)),
        }
    }
}

/// Unified fragment store: dedup -> load existing -> compress -> optional remote -> local store.
///
/// When `remote_session` is `Some`, the session is used after compression to
/// attempt a durable remote write via `session.put()`. The durable status
/// affects the `PayloadStoredDurable` flag and whether the payload is cached
/// locally (payload is always cached when not yet durable, as a safety net).
///
/// For local-only storage, pass `None` for `remote_session`.
///
/// When `writes` carries a tracker, the work after the synchronous dedup/pre-check
/// is handed off to a background leader task owned by that tracker; the call
/// returns as soon as the address and input fragment are known. If another
/// task is already writing the same address, this call registers a lightweight
/// follower future on the tracker that resolves once the leader finishes.
///
/// Without a tracker the work runs inline (backward-compatible synchronous
/// behavior). Counters on `writes` are reported into either way, including from
/// the leader task, where compression and placement become known.
///
/// `permit` is the caller-held memory permit associated with `buffer`. If a
/// leader is spawned, the permit moves into the leader task; if the call
/// becomes a follower or short-circuits, the permit is dropped immediately.
#[allow(clippy::too_many_arguments)]
pub async fn store_fragment(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<StoreResult, StorageError> {
    if address.hash.is_zero() || buffer.is_empty() || fragment.size_payload == 0 {
        return Err(StorageError::internal(
            "zero size or zero hash buffers can not be stored",
        ));
    }
    if (fragment.size_payload as usize) > crate::compress::FRAGMENT_SIZE_THRESHOLD {
        return Err(StorageError::from(crate::errors::Oversized {
            context: format!(
                "fragment size_payload {} exceeds FRAGMENT_SIZE_THRESHOLD {} on store_fragment",
                fragment.size_payload,
                crate::compress::FRAGMENT_SIZE_THRESHOLD
            ),
        }));
    }
    if fragment.size_payload as usize != buffer.len() {
        return Err(StorageError::internal(format!(
            "store_fragment buffer length mismatch: buffer {} vs size_payload {}",
            buffer.len(),
            fragment.size_payload
        )));
    }

    writes.count(|stats| stats.fragment_produced(&fragment));

    let result = match writes.tracker().cloned() {
        None => {
            store_fragment_inline(
                store,
                partition,
                address,
                fragment,
                buffer,
                cache_local,
                remote_session,
                &writes,
                permit,
                None,
            )
            .await
        }
        Some(tracker) => {
            store_fragment_dispatched(
                store,
                partition,
                address,
                fragment,
                buffer,
                cache_local,
                remote_session,
                &tracker,
                &writes,
                permit,
            )
            .await
        }
    };

    if let (Some(tracker), Ok(result)) = (writes.tracker(), &result) {
        tracker.notify_fragment(&observed_fragment(fragment, result), result.deduplicated);
    }
    result
}

/// The fragment to hand a write observer: the caller's representation, marked with where the
/// payload ended up.
///
/// The representation has to come from the caller. An observer is only installed on a tracker, a
/// tracker always dispatches, and a dispatched write reports before its leader compresses, so
/// there is no stored representation to report yet.
fn observed_fragment(fragment: Fragment, result: &StoreResult) -> Fragment {
    let mut flags = fragment.flags;
    if result.stored_local {
        flags |= FragmentFlags::PayloadStoredLocal;
    }
    if result.stored_durable {
        flags |= FragmentFlags::PayloadStoredDurable;
    }
    Fragment { flags, ..fragment }
}

/// Backward-compatible synchronous fragment store. Acquires the in-flight
/// guard (blocking if another task holds it), runs the full store pipeline
/// inline, and returns only after the terminal store entry is written.
///
/// When `remote_session` is `None`, the in-flight machinery is bypassed entirely: it exists
/// to coordinate concurrent uploads to the same address (so duplicate uploads collapse onto
/// one wire call), which is moot for pure-local writes. Concurrent local writers may briefly
/// do duplicate compression work, but the bucket-level write is content-addressed and
/// idempotent. Items with no remote consult must not enter the dedup tracker.
#[allow(clippy::too_many_arguments)]
async fn store_fragment_inline(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    writes: &WriteContext,
    permit: Option<OwnedSemaphorePermit>,
    publish: Option<Hash>,
) -> Result<StoreResult, StorageError> {
    let query = resolve_or_absent(&store, partition, address).await;
    let deduplicated = query.match_made != StoreMatch::MatchNone;
    let (stored_local, stored_durable) = stored_flags(&query);

    if is_fully_satisfied(
        &query,
        cache_local,
        stored_local,
        &remote_session,
        stored_durable,
    ) {
        writes.count(|stats| stats.fragment_deduplicated(&fragment));
        return Ok(StoreResult {
            address,
            size_content: fragment.size_content,
            stored_local,
            stored_durable,
            deduplicated: true,
            published: false,
        });
    }

    // Local-only fast path: skip STORE_IN_FLIGHT entirely. No follower notification needed,
    // no leader-token rendezvous — just compress+write inline.
    if remote_session.is_none() || publish.is_some() {
        let placement = leader_body(
            store,
            partition,
            address,
            fragment,
            buffer,
            cache_local,
            remote_session,
            query,
            None,
            writes.stats(),
            permit,
            publish,
        )
        .await?;
        return Ok(StoreResult {
            address,
            size_content: fragment.size_content,
            stored_local: placement.local,
            stored_durable: placement.durable,
            deduplicated,
            published: placement.published,
        });
    }

    // Remote-coupled path: acquire the in-flight guard so a concurrent writer to the same
    // address dedupes onto one upload.
    let guard = stored_in_flight(partition, address).await;
    let Some(guard) = guard else {
        // We waited on another task that finished without satisfying our
        // preconditions (e.g., they wrote durable but we want local).
        // Preserve legacy behaviour by returning the current store state.
        drop(permit);
        writes.count(|stats| stats.fragment_deduplicated(&fragment));
        return Ok(StoreResult {
            address,
            size_content: fragment.size_content,
            stored_local,
            stored_durable,
            deduplicated: true,
            published: false,
        });
    };

    let placement = leader_body(
        store,
        partition,
        address,
        fragment,
        buffer,
        cache_local,
        remote_session,
        query,
        Some(guard),
        writes.stats(),
        permit,
        None,
    )
    .await?;
    Ok(StoreResult {
        address,
        size_content: fragment.size_content,
        stored_local: placement.local,
        stored_durable: placement.durable,
        deduplicated,
        published: placement.published,
    })
}

/// Tracker-dispatched fragment store: non-blocking in-flight check, spawns a
/// leader or registers a follower on the tracker, and returns immediately.
#[allow(clippy::too_many_arguments)]
async fn store_fragment_dispatched(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    tracker: &WriteTracker,
    writes: &WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<StoreResult, StorageError> {
    let guard = match try_acquire_in_flight(partition, address) {
        Ok(guard) => guard,
        Err(token) => {
            // Follower path: drop buffer and permit, register on the tracker.
            drop(buffer);
            drop(permit);
            tracker.register_follower(follower_future(store.clone(), partition, address, token));
            writes.count(|stats| stats.fragment_deduplicated(&fragment));
            return Ok(StoreResult {
                address,
                size_content: fragment.size_content,
                stored_local: false,
                stored_durable: false,
                deduplicated: true,
                published: false,
            });
        }
    };

    let query = resolve_or_absent(&store, partition, address).await;
    let (stored_local, stored_durable) = stored_flags(&query);

    if is_fully_satisfied(
        &query,
        cache_local,
        stored_local,
        &remote_session,
        stored_durable,
    ) {
        drop(guard);
        drop(buffer);
        drop(permit);
        writes.count(|stats| stats.fragment_deduplicated(&fragment));
        return Ok(StoreResult {
            address,
            size_content: fragment.size_content,
            stored_local,
            stored_durable,
            deduplicated: true,
            published: false,
        });
    }

    let deduplicated = query.match_made != StoreMatch::MatchNone;
    let store_clone = store.clone();
    // The leader takes the counters alone, never the tracker: the tracker is what
    // awaits this task, and `await_all` requires its handle to be the only one.
    let stats = writes.stats();
    tracker.spawn_leader(async move {
        leader_body(
            store_clone,
            partition,
            address,
            fragment,
            buffer,
            cache_local,
            remote_session,
            query,
            Some(guard),
            stats,
            permit,
            None,
        )
        .await
        .map(|_stored| ())
    });
    Ok(StoreResult {
        address,
        size_content: fragment.size_content,
        stored_local: false,
        stored_durable: false,
        deduplicated,
        published: false,
    })
}

async fn resolve_or_absent(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> StoreMatchResult {
    query_one(store, partition, address)
        .await
        .unwrap_or_default()
}

/// Uploads never sent, because an association the peer already held was duplicated instead.
/// Process-wide, like [`CONTENT_WRITE_INFLIGHT`], and counted per fragment.
static REMOTE_COPIES: AtomicUsize = AtomicUsize::new(0);

/// See [`REMOTE_COPIES`].
pub fn remote_copies() -> usize {
    REMOTE_COPIES.load(Ordering::Relaxed)
}

/// Drops the count to zero, so what follows is measured on its own.
pub fn reset_remote_copies() {
    REMOTE_COPIES.store(0, Ordering::Relaxed);
}

/// An association the peer already holds, which a copy duplicates into the address being written.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct CopySource {
    partition: Partition,
    address: Address,
}

/// The association to copy from, or `None` where the payload has to be transferred instead.
///
/// A partial match is the level that names another association; `stored_durable` on it is what says
/// the peer holds that one and not merely the local store; and a copy cannot be aimed at a
/// partition nobody named. The context is passed through as found — an exact one the peer confirms
/// with a keyed read, an unnamed one it searches the partition for.
fn copy_source(resolved: &StoreMatchResult, address: Address) -> Option<CopySource> {
    if !matches!(
        resolved.match_made,
        StoreMatch::MatchPartition | StoreMatch::MatchHash
    ) {
        return None;
    }
    if !resolved.stored_durable || resolved.partition.is_zero() {
        return None;
    }
    Some(CopySource {
        partition: resolved.partition,
        address: resolved.source_address(address.hash),
    })
}

/// Duplicate the association `source` names into `address` on the session's partition, reporting
/// whether the peer now holds it durably.
///
/// A refusal is an outcome rather than an error — the source may be gone, the peer may never have
/// had it, or the caller may hold no claim to its partition — and the upload the caller falls back
/// to does everything this would have.
async fn copy_association(
    session: &Arc<StorageSession>,
    source: CopySource,
    address: Address,
) -> bool {
    if !session.can_copy_from(source.partition).await {
        lore_base::lore_trace!(
            "No claim to partition {} to copy {} from, uploading instead",
            source.partition,
            address.hash
        );
        return false;
    }

    match session
        .copy(source.partition, source.address, address.context)
        .await
    {
        Ok(()) => {
            REMOTE_COPIES.fetch_add(1, Ordering::Relaxed);
            lore_base::lore_trace!(
                "Copied {} from partition {} instead of uploading its payload",
                address,
                source.partition
            );
            true
        }
        Err(err) => {
            lore_base::lore_trace!(
                "Copy of {} from partition {} refused ({err:?}), uploading instead",
                address,
                source.partition
            );
            false
        }
    }
}

/// Durability only counts for this address when this address is what matched. The same content
/// under another partition is durable without our association being, and an upload skipped on that
/// basis would leave the address registered nowhere.
fn stored_flags(resolved: &StoreMatchResult) -> (bool, bool) {
    let stored_durable = resolved.match_made == StoreMatch::MatchFull && resolved.stored_durable;
    (resolved.stored_local, stored_durable)
}

fn is_fully_satisfied(
    resolved: &StoreMatchResult,
    cache_local: bool,
    stored_local: bool,
    remote_session: &Option<Arc<StorageSession>>,
    stored_durable: bool,
) -> bool {
    resolved.match_made == StoreMatch::MatchFull
        && (!cache_local || stored_local)
        && (remote_session.is_none() || stored_durable)
}

/// The "work" portion of [`store_fragment`]: optionally duplicate an association the peer already
/// holds, else load existing local payload, compress and upload, then write the terminal entry.
///
/// Returns where the payload ended up, as `(stored_local, stored_durable)`.
///
/// `guard` is the in-flight token the caller acquired before invoking this function. When
/// `None`, no in-flight machinery is in play (the local-only fast path that bypasses the
/// dedup token entirely — see [`store_fragment_inline`]). When `Some`, dropping the guard at
/// the end cancels the token and wakes any followers subscribed to this write.
/// Where a fragment ended up once the leader finished with it.
///
/// `published` is separate from `durable` because a key rides along with an *upload*: content
/// already durable performs none, so its mapping still has to be written on its own.
struct Placement {
    local: bool,
    durable: bool,
    published: bool,
}

#[allow(clippy::too_many_arguments)]
async fn leader_body(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    mut fragment: Fragment,
    mut buffer: Bytes,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    query: StoreMatchResult,
    guard: Option<StoreInFlightGuard>,
    stats: Option<Arc<FragmentWriteStats>>,
    permit: Option<OwnedSemaphorePermit>,
    publish: Option<Hash>,
) -> Result<Placement, StorageError> {
    let (mut stored_local, mut stored_durable) = stored_flags(&query);
    let mut published = false;
    let stats = stats.as_deref();
    let mut registered_remotely = false;

    if let Some(stats) = stats {
        stats.fragment_processed(&fragment);
    }

    // Before the payload is prepared: succeeding means neither the load nor the compression below
    // is work this fragment has to pay for.
    if !stored_durable
        && let Some(session) = remote_session.as_ref()
        && let Some(source) = copy_source(&query, address)
    {
        stored_durable = copy_association(session, source, address).await;
        if stored_durable {
            registered_remotely = true;
            if let Some(stats) = stats {
                stats.remote_copy();
            }
        }
    }

    let payload_wanted = !stored_durable || cache_local;

    // For a partial match try loading the payload from local store instead of recompressing
    if payload_wanted && stored_local {
        if let Ok((stored_fragment, stored_buffer)) = store
            .clone()
            .get(partition, address)
            .await
            .and_then(StoreGetData::into_payload)
        {
            let loaded_hash =
                hash::hash_fragment(stored_fragment, stored_buffer.as_ref()).unwrap_or_default();
            debug_assert!(
                loaded_hash == address.hash,
                "Local store had corrupt data when loading previous representation during store_raw"
            );
            if address.hash == loaded_hash {
                fragment = stored_fragment;
                buffer = stored_buffer;
            } else {
                stored_local = false;
            }
        } else {
            stored_local = false;
        }
    }

    // If we could not load from local store, try compressing the data
    let mode = crate::compress::CompressionMode::from_u32(COMPRESSION_MODE.load(Ordering::Relaxed));
    if payload_wanted && !stored_local && mode != crate::compress::CompressionMode::NoCompression {
        let _compress_permit = crate::concurrency::compress_limit_acquire().await;
        if let Ok((compressed_fragment, compressed_buffer)) = crate::compress::compress(
            fragment,
            &buffer.as_ref()[..fragment.size_payload as usize],
            mode,
        ) {
            lore_base::lore_trace!(
                "Compressed {} bytes to {} bytes",
                fragment.size_payload,
                compressed_fragment.size_payload
            );
            fragment = compressed_fragment;
            buffer = compressed_buffer;
        }
    }

    if let Some(stats) = stats {
        if payload_wanted {
            stats.payload_prepared(&fragment);
        } else {
            stats.payload_not_prepared(&fragment);
        }
    }

    // Remote upload if session provided and not already durable
    if !stored_durable && let Some(session) = remote_session.clone() {
        stored_durable = match publish {
            Some(key) => {
                published = remote_put_resolved_retry(
                    session,
                    key,
                    address,
                    fragment,
                    Some(buffer.clone()),
                )
                .await
                .is_ok();
                published
            }
            None => remote_put_retry(session, address, fragment, Some(buffer.clone()))
                .await
                .is_ok(),
        };
        if stored_durable {
            registered_remotely = true;
            if let Some(stats) = stats {
                stats.remote_put(u64::from(fragment.size_payload));
            }
        }
    }

    if let Some(stats) = stats
        && !registered_remotely
    {
        if remote_session.is_none() {
            stats.local_only_write();
        } else if stored_durable {
            stats.remote_already_durable();
        } else {
            stats.remote_upload_failed();
        }
    }

    if stored_durable {
        fragment.flags |= FragmentFlags::PayloadStoredDurable;
    } else {
        fragment.flags &= !FragmentFlags::PayloadStoredDurable;
    }

    let (payload, permit) = if !stored_durable || cache_local {
        (Some(buffer), permit)
    } else {
        drop(buffer);
        drop(permit);
        (None, None)
    };
    stored_local |= payload.is_some();

    let payload_bytes = payload.as_ref().map(|payload| payload.len() as u64);
    write_raw(store, partition, address, fragment, payload).await?;
    if let Some(stats) = stats {
        stats.local_write(payload_bytes);
    }

    drop(permit);
    drop(guard);
    Ok(Placement {
        local: stored_local,
        durable: stored_durable,
        published,
    })
}

/// Store a raw fragment locally (no remote, no event emission).
/// Thin wrapper around [`store_fragment`] with no remote session.
pub async fn store_raw_local(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
) -> Result<Address, StorageError> {
    let result = store_fragment(
        store,
        partition,
        address,
        fragment,
        buffer,
        cache_local,
        None,
        WriteContext::none(),
        None,
    )
    .await?;
    Ok(result.address)
}

/// Content writes running now, and the most that have run at once since the peak was reset.
///
/// Process-wide across every caller, like `REMOTE_FETCH_INFLIGHT` on the read side: total
/// pressure rather than one operation's share. Counted per whole content write — one buffer or
/// one file — not per fragment.
static CONTENT_WRITE_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static CONTENT_WRITE_PEAK: AtomicUsize = AtomicUsize::new(0);

/// See [`CONTENT_WRITE_INFLIGHT`].
pub fn content_write_inflight() -> usize {
    CONTENT_WRITE_INFLIGHT.load(Ordering::Relaxed)
}

/// See [`CONTENT_WRITE_PEAK`].
pub fn content_write_peak() -> usize {
    CONTENT_WRITE_PEAK.load(Ordering::Relaxed)
}

/// Drops the peak to the count in flight now, so what follows is measured on its own.
pub fn reset_content_write_peak() {
    CONTENT_WRITE_PEAK.store(content_write_inflight(), Ordering::Relaxed);
}

/// Counts one content write while it runs, so an early return or a panic cannot leak the count.
struct ContentWriteGuard;

impl ContentWriteGuard {
    fn new() -> Self {
        let in_flight = CONTENT_WRITE_INFLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
        CONTENT_WRITE_PEAK.fetch_max(in_flight, Ordering::Relaxed);
        Self
    }
}

impl Drop for ContentWriteGuard {
    fn drop(&mut self) {
        CONTENT_WRITE_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The address and fragment header for content that fits one fragment.
///
/// Shared by [`write_content`] and [`write_content_publishing`] so the two cannot disagree on what
/// a single-fragment write is addressed as.
fn single_fragment(context: Context, buffer: &Bytes, flags: WriteOptions) -> (Address, Fragment) {
    (
        Address {
            context,
            hash: hash::hash_slice(buffer.as_ref()),
        },
        Fragment {
            flags: flags.into(),
            size_payload: buffer.len() as u32,
            size_content: buffer.len() as u64,
        },
    )
}

/// [`write_content`] for content that fits one fragment and whose upload should also publish
/// `key` as a `KeyType::Resolve` mapping — the single round trip `write_resolved` exists for.
///
/// The write goes through the same leader body an ordinary upload does, so the content is
/// compressed once and the local store is written once, with the durable flag and the
/// `cache_local` retention decision already correct. The alternative — write locally, read the
/// stored representation back, upload it, then rewrite the entry — costs two extra local store
/// operations on every published write.
///
/// A publishing write never dedupes onto a concurrent writer's upload: the other writer is
/// publishing a different key, or none, so both upload.
///
/// `published` is false when the content was already durable: no upload happened for the key to
/// ride on, so the caller still owes it a mapping write.
#[allow(clippy::too_many_arguments)]
pub async fn write_content_publishing(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
    key: Hash,
) -> Result<StoreResult, StorageError> {
    debug_assert!(
        buffer.len() <= crate::compress::FRAGMENT_SIZE_THRESHOLD,
        "content that fragments cannot fuse its mapping into a leaf upload",
    );
    let _in_flight = ContentWriteGuard::new();
    let (address, fragment) = single_fragment(context, &buffer, flags);
    let permit = match permit {
        Some(permit) => Some(permit),
        None => crate::concurrency::acquire_fragment_memory_permit(buffer.len()).await,
    };
    writes.count(|stats| stats.fragment_produced(&fragment));
    store_fragment_inline(
        store,
        partition,
        address,
        fragment,
        buffer,
        flags.local_cache_priority,
        remote_session,
        &writes,
        permit,
        Some(key),
    )
    .await
}

/// Write content (fragmenting if needed).
///
/// Takes a store, partition, and optional remote session directly instead of a
/// closure. Internally calls [`store_fragment`] for small buffers or
/// [`write_fragmented`] for buffers exceeding `FRAGMENT_SIZE_THRESHOLD`.
///
/// Reports where the content came to rest, not just its address. For a fragment tree that is the
/// intersection across every leaf and intermediate node, so a single leaf that failed to upload
/// leaves the whole tree reported as not durable — which is what lets a caller publishing a key
/// refuse to name content the server holds only part of.
#[allow(clippy::too_many_arguments)]
pub async fn write_content(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<StoreResult, StorageError> {
    let _in_flight = ContentWriteGuard::new();
    // Check if data should be a single fragment
    if buffer.len() <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
        let (address, fragment) = single_fragment(context, &buffer, flags);
        // Reuse the caller's read reservation if provided, else reserve here.
        let permit = match permit {
            Some(permit) => Some(permit),
            None => crate::concurrency::acquire_fragment_memory_permit(buffer.len()).await,
        };
        let result = store_fragment(
            store,
            partition,
            address,
            fragment,
            buffer,
            flags.local_cache_priority,
            remote_session,
            writes,
            permit,
        )
        .await?;
        Ok(result)
    } else {
        let size_content = buffer.len() as u64;
        let (address, stored_local, stored_durable) = write_fragmented(
            store,
            partition,
            context,
            buffer,
            flags,
            false,
            remote_session,
            writes,
            permit,
        )
        .await?;
        Ok(StoreResult {
            address,
            size_content,
            stored_local,
            stored_durable,
            deduplicated: false,
            published: false,
        })
    }
}

/// Write content from a file.
///
/// Takes a store, partition, and optional remote session directly.
///
/// Returns the address and the size of the content behind it. The size is reported because a
/// caller that hands over a path has no other way to learn what was actually written: stating the
/// file again afterwards answers for the file as it is then, not for the bytes this address
/// stands for.
#[allow(clippy::too_many_arguments)]
pub async fn write_from_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    path: &Path,
    context: Context,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
) -> Result<StoreResult, StorageError> {
    let _in_flight = ContentWriteGuard::new();
    let _count_permit = file_count_limit_acquire()
        .await
        .forward::<StorageError>("permit failed")?;
    let mut retry = crate::retry(10, 10_000, 10);
    let (file, size) = loop {
        match crate::chunker::open_read(path).await {
            Ok(result) => break result,
            Err(err) => {
                if !retry.wait().await {
                    return Err(StorageError::internal_with_context(
                        err,
                        &format!("open file: {}", path.display()),
                    ));
                }
            }
        }
    };

    lore_base::lore_trace!(
        "Opened file to read from for immutable data write: {} size {size}",
        path.display(),
    );

    if size == 0 {
        return Ok(StoreResult {
            address: Address {
                context,
                hash: Hash::new_zeroed(),
            },
            size_content: 0,
            stored_local: false,
            stored_durable: false,
            deduplicated: false,
            published: false,
        });
    }

    // Anything larger than one fragment streams, so the scan never holds a file resident.
    let size = size as usize;
    if size <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
        let read_permit = crate::concurrency::acquire_fragment_memory_permit(size).await;
        let buffer = file.read_exact_at(size, 0).await.map_err(|e| {
            StorageError::internal_with_context(e, &format!("read file: {}", path.display()))
        })?;
        let address = write_content(
            store,
            partition,
            context,
            buffer,
            flags,
            remote_session,
            writes,
            read_permit,
        )
        .await?;
        return Ok(StoreResult {
            size_content: size as u64,
            ..address
        });
    }

    let (address, _stored_local, _stored_durable) =
        crate::fragment_engine::write_fragmented_from_file(
            store,
            partition,
            context,
            file,
            size,
            flags,
            false,
            remote_session,
            writes,
        )
        .await?;
    Ok(StoreResult {
        address,
        size_content: size as u64,
        stored_local: _stored_local,
        stored_durable: _stored_durable,
        deduplicated: false,
        published: false,
    })
}

/// Whether a file on disk holds the content a stored object addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMatch {
    /// The file is the stored content.
    Match,
    /// The file is not the stored content.
    Differs,
    /// The stored object could not be described or walked, so nothing was established
    /// about the file either way.
    Indeterminate,
}

/// Whether `path` still holds the content `previous` addresses.
///
/// Transfers fragment metadata only: the stored object's header and, when it is fragmented,
/// its fragment lists. Content payloads are never fetched — chunks are compared by hashing
/// the file's own bytes over the ranges the stored list records, so the cost is bounded by
/// the file and its metadata however large the object is.
///
/// A file at or below the fragment threshold is one fragment under the current chunking, so
/// its buffer hash is tried first and settles the question when it matches, without touching
/// the store at all. A mismatch settles nothing on its own: a commit may reuse a previous
/// fragmentation, so a stored object of any size may be a fragment list, and no buffer hash
/// equals a fragment list hash. The stored header then says which it is, and a list is
/// walked like any other.
///
/// A stored object that cannot be described or walked falls back to
/// [`hashed_under_current_chunking`], which reads nothing but the file.
pub async fn file_matches(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    path: impl AsRef<Path>,
    previous: Address,
    previous_size: Option<usize>,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<FileMatch, StorageError> {
    let _count_permit = file_count_limit_acquire()
        .await
        .forward::<StorageError>("permit failed")?;

    let path = path.as_ref();
    let Ok(metadata) = lore_io::IoDriver::global().metadata(path).await else {
        return Err(StorageError::internal(format!(
            "failed to query file metadata: {}",
            path.display()
        )));
    };
    let file_size = metadata.len() as usize;

    if previous_size.is_some_and(|size| size != file_size) {
        return Ok(FileMatch::Differs);
    }
    if file_size == 0 {
        // Empty is empty under any fragmentation.
        return Ok(if previous.hash.is_zero() {
            FileMatch::Match
        } else {
            FileMatch::Differs
        });
    }
    if previous.is_zero() {
        return Ok(FileMatch::Differs);
    }

    if file_size <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
        let data = lore_io::IoDriver::global()
            .read_file_bytes(path)
            .await
            .map_err(|e| {
                StorageError::internal_with_context(e, &format!("read file: {}", path.display()))
            })?;
        if Hash::hash_buffer(&data) == previous.hash {
            return Ok(FileMatch::Match);
        }

        // The header says whether the stored object is a list this buffer hash could never
        // have equalled, or a single fragment whose hash it was directly comparable with.
        let Ok(described) = store.clone().get_metadata(partition, previous).await else {
            return Ok(FileMatch::Indeterminate);
        };
        if described.match_made == StoreMatch::MatchNone {
            return Ok(FileMatch::Indeterminate);
        }
        if described.fragment.flags & FragmentFlags::PayloadFragmented == 0 {
            return Ok(FileMatch::Differs);
        }
        if described.fragment.size_content != file_size as u64 {
            return Ok(FileMatch::Differs);
        }
    }

    let options = ReadOptions::default().no_decompress().no_verify();
    let loaded = load_fragment(
        store.clone(),
        partition,
        previous,
        options,
        remote_session.clone(),
    )
    .await
    .ok();

    if let Some((fragment, _)) = loaded.as_ref()
        && fragment.size_content != file_size as u64
    {
        return Ok(FileMatch::Differs);
    }

    let file = open_for_compare(path).await?;

    if let Some((fragment, payload)) = loaded
        && fragment.flags & FragmentFlags::PayloadFragmented != 0
    {
        let fragment_list = payload.to_aligned::<FragmentReference>();
        let previous_fragmentation = fragment_list.as_type_slice::<FragmentReference>();
        if !previous_fragmentation.is_empty() {
            match compare_previous_chunks(
                SublistSource {
                    store: &store,
                    partition,
                    context: previous.context,
                    remote_session: &remote_session,
                },
                path,
                &file,
                file_size as u64,
                previous_fragmentation,
            )
            .await?
            {
                FileMatch::Match => return Ok(FileMatch::Match),
                FileMatch::Differs => return Ok(FileMatch::Differs),
                FileMatch::Indeterminate => {}
            }
        }
    }

    hashed_under_current_chunking(store, partition, previous, file, file_size).await
}

/// Whether hashing the file under the current chunking reproduces `previous`.
///
/// The fallback for a stored object that could not be described or walked, which is what a
/// clone into a directory of existing files sees: nothing is in the local store yet, so
/// there is no fragmentation to measure against. A file the current chunker was what stored
/// still hashes to the address it was stored under, and that settles it while reading
/// nothing but the file. A different hash settles nothing, since the stored object may have
/// been chunked another way.
async fn hashed_under_current_chunking(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    previous: Address,
    file: lore_io::IoFile,
    file_size: usize,
) -> Result<FileMatch, StorageError> {
    if file_size <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
        // One fragment covers the file, so its buffer hash was already the whole answer.
        return Ok(FileMatch::Indeterminate);
    }

    let address = crate::fragment_engine::write_fragmented_from_file(
        store,
        partition,
        previous.context,
        file,
        file_size,
        WriteOptions::default().no_remote_write(),
        true,
        None,
        WriteContext::none(),
    )
    .await?;

    Ok(if address.0.hash == previous.hash {
        FileMatch::Match
    } else {
        FileMatch::Indeterminate
    })
}

/// Hash a file's content, using previous fragmentation hints when available.
///
/// Takes a store, partition, and optional remote session directly. Internally
/// uses [`load_fragment`] for loading fragments and calls [`store_fragment`] /
/// [`write_fragmented`] for storing.
pub async fn hash_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    path: impl AsRef<Path>,
    previous: Option<Address>,
    previous_size: Option<usize>,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<Hash, StorageError> {
    let _count_permit = file_count_limit_acquire()
        .await
        .forward::<StorageError>("permit failed")?;

    let path = path.as_ref();
    let Ok(metadata) = lore_io::IoDriver::global().metadata(path).await else {
        return Err(StorageError::internal(format!(
            "failed to query file metadata: {}",
            path.display()
        )));
    };

    let file_size = metadata.len() as usize;

    lore_base::lore_trace!("Hash file {} previous address {previous:?}", path.display());

    // Files that fit in a single fragment: just read and hash directly
    if file_size == 0 {
        return Ok(Hash::new_zeroed());
    }
    if file_size <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
        let data = lore_io::IoDriver::global()
            .read_file_bytes(path)
            .await
            .map_err(|e| {
                StorageError::internal_with_context(e, &format!("read file: {}", path.display()))
            })?;
        return Ok(Hash::hash_buffer(&data));
    }

    // Large files: try loading previous fragmentation to compare chunk hashes.
    // Only attempt if the previous size matches (or is unknown) — different sizes
    // require re-fragmentation anyway.
    // TODO: once write_fragmented supports partial fragment reuse, we could
    // attempt matching even when sizes differ.
    let previous = previous.unwrap_or_default();
    let mut fragment_list = None;
    let size_matches = previous_size.is_none() || previous_size == Some(file_size);
    if !previous.is_zero() && size_matches {
        let options = ReadOptions::default().no_decompress().no_verify();
        if let Ok((fragment, payload)) = load_fragment(
            store.clone(),
            partition,
            previous,
            options,
            remote_session.clone(),
        )
        .await
        {
            // Double-check that the stored content size matches the current file.
            // TODO: once write_fragmented supports partial fragment reuse, we
            // could attempt matching even when sizes differ.
            if fragment.flags & FragmentFlags::PayloadFragmented != 0
                && fragment.size_content == file_size as u64
            {
                fragment_list = Some(payload.to_aligned::<FragmentReference>());
            }
        }
        // Failed to load or size mismatch — fall through to re-fragment
    }

    // Chunks are read on demand, so a mismatch early in the list stops after reading only
    // the chunks it compared.
    // Opened rather than measured again: the size above is the one the chunker is opened at.
    let file = open_for_compare(path).await?;

    // If we have a non-empty previous fragment list, check if chunks still match
    if let Some(ref frag_bytes) = fragment_list {
        let previous_fragmentation = frag_bytes.as_type_slice::<FragmentReference>();
        if !previous_fragmentation.is_empty()
            && compare_previous_chunks(
                SublistSource {
                    store: &store,
                    partition,
                    context: previous.context,
                    remote_session: &remote_session,
                },
                path,
                &file,
                file_size as u64,
                previous_fragmentation,
            )
            .await?
                == FileMatch::Match
        {
            return Ok(previous.hash);
        }
    }

    let address = crate::fragment_engine::write_fragmented_from_file(
        store,
        partition,
        previous.context,
        file,
        file_size,
        WriteOptions::default().no_remote_write(),
        true,
        None,
        WriteContext::none(),
    )
    .await?;

    Ok(address.0.hash)
}

/// Open `path` for the chunk walk, retrying a file another process may still be closing.
async fn open_for_compare(path: &Path) -> Result<lore_io::IoFile, StorageError> {
    let mut retry = crate::retry(10, 10_000, 10);
    loop {
        match lore_io::IoDriver::global()
            .open(path, &lore_io::OpenOptions::new().read(true))
            .await
        {
            Ok(file) => return Ok(file),
            Err(err) => {
                if !retry.wait().await {
                    return Err(StorageError::internal_with_context(
                        err,
                        &format!("open file: {}", path.display()),
                    ));
                }
            }
        }
    }
}

/// One read covering several consecutive chunks. Sized like the chunker's window and for
/// the same reason: nothing compared against a window exceeds
/// [`FRAGMENT_SIZE_THRESHOLD`](crate::compress::FRAGMENT_SIZE_THRESHOLD), so a window
/// starting on a chunk boundary always holds at least one whole chunk and the walk cannot
/// stall.
const HASH_WINDOW_SIZE: usize = 2 * crate::compress::FRAGMENT_SIZE_THRESHOLD;

/// Bytes of the file that are resident, and where they start in it.
struct HashWindow {
    offset: u64,
    data: BytesMut,
}

/// The buffer for a read of `want` bytes: the window just retired, or a fresh one sized to the
/// largest this file needs. `capacity` bounds every window, so a retired one always fits.
fn take_window(spare: &mut Option<BytesMut>, capacity: usize, want: usize) -> BytesMut {
    let mut buffer = spare
        .take()
        // SAFETY: the read below fills the buffer before anything reads a byte of it.
        .unwrap_or_else(|| unsafe { lore_io::uninit_buffer(capacity) });
    debug_assert!(buffer.capacity() >= want, "window smaller than the read");
    // SAFETY: capacity covers `want`, and the read fills all of it before it is hashed.
    unsafe { buffer.set_len(want) };
    buffer
}

/// Start a window read without waiting for it, so it overlaps the hashing of the window
/// already held. The buffer is filled whole, the walk having no headroom to carry, and the read
/// hands it back. Dropping the handle detaches the read, which the engine completes and then
/// frees its buffer.
fn start_window_read(
    file: &lore_io::IoFile,
    buffer: BytesMut,
    offset: u64,
) -> JoinHandle<std::io::Result<BytesMut>> {
    let file = file.clone();
    let want = buffer.len();
    lore_base::lore_spawn!(async move {
        let window = file
            .read_exact_vectored_at(
                crate::chunker::WindowRead {
                    buffer,
                    start: 0,
                    want,
                },
                offset,
            )
            .await?;
        Ok(window.buffer)
    })
}

impl HashWindow {
    /// Whether `[start, end)` is held whole, and so can be hashed without reading.
    fn holds(&self, start: u64, end: u64) -> bool {
        start >= self.offset && end <= self.end()
    }

    fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }

    fn slice(&self, start: u64, end: u64) -> &[u8] {
        let base = (start - self.offset) as usize;
        &self.data[base..base + (end - start) as usize]
    }
}

/// Where chunk `index` ends: where the next one starts, or the end of the file for the
/// last. `None` if the list does not ascend, which means it does not describe this file —
/// a subtraction that used to underflow instead.
fn chunk_end(chunks: &[FragmentReference], index: usize, file_size: u64) -> Option<u64> {
    let end = match chunks.get(index + 1) {
        Some(next) => next.offset_content,
        None => file_size,
    };
    (end >= chunks[index].offset_content).then_some(end)
}

/// Where the read after a window ending at `window_end` must start: the first chunk from
/// `index` on that the window does not hold whole. `None` when the window already reaches
/// the last chunk, or when that chunk is one this walk will not read — either fragmented
/// further, so comparing it means loading its sublist, or outside the file, so the walk is
/// about to stop.
fn next_window_offset(
    chunks: &[FragmentReference],
    index: usize,
    window_end: u64,
    file_size: u64,
) -> Option<u64> {
    for (position, chunk) in chunks.iter().enumerate().skip(index) {
        let end = chunk_end(chunks, position, file_size)?;
        if end <= window_end {
            continue;
        }
        let readable = end <= file_size
            && end - chunk.offset_content <= crate::compress::FRAGMENT_SIZE_THRESHOLD as u64;
        return readable.then_some(chunk.offset_content);
    }
    None
}

/// Where a chunk that turns out to be fragmented further has its own list loaded from.
/// Only the context is taken from the previous address: the walk names each sublist by the
/// hash recorded for it in the list above.
struct SublistSource<'a> {
    store: &'a Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    remote_session: &'a Option<Arc<StorageSession>>,
}

/// Measure the file against `previous_fragmentation` chunk for chunk.
///
/// Reads cover as many consecutive chunks as a window holds and run one window ahead of
/// the hashing, which is then taken in place. This is the *unchanged* file path for
/// `status` and `commit`, so the cost per chunk is paid on every file that has not
/// changed: one blocking read per chunk would be ~16,384 sequential dispatches per GiB,
/// each allocating and filling its own buffer.
///
/// A chunk that no longer matches returns [`FileMatch::Differs`] immediately, and a walk
/// that cannot proceed at all — a sublist that fails to load or is not a list — returns
/// [`FileMatch::Indeterminate`]. A list that merely misdescribes the content reads as a
/// difference rather than as indeterminate, since the list is what defines the ranges being
/// hashed: wrong offsets simply hash the wrong bytes.
///
/// The walk stops having read at most one window more than it compared, where reading per
/// chunk stopped exactly at the mismatch — the cost of not paying a round trip per chunk on
/// every unchanged file.
async fn compare_previous_chunks(
    sublists: SublistSource<'_>,
    path: &Path,
    file: &lore_io::IoFile,
    file_size: u64,
    previous_fragmentation: &[FragmentReference],
) -> Result<FileMatch, StorageError> {
    // Recursive fragmentation is spliced in as it is found, so the list grows.
    let mut chunks = previous_fragmentation.to_vec();

    // Released here rather than held into the re-fragmentation the caller may fall through
    // to, which reserves its own windows.
    let capacity = file_size.min(HASH_WINDOW_SIZE as u64) as usize;
    let windows = if file_size <= HASH_WINDOW_SIZE as u64 {
        1
    } else {
        2
    };
    let _reservation = crate::concurrency::acquire_fragment_memory_permit(windows * capacity).await;

    let window_length = |offset: u64| (file_size - offset).min(HASH_WINDOW_SIZE as u64) as usize;

    let mut window: Option<HashWindow> = None;
    let mut pending: Option<(JoinHandle<std::io::Result<BytesMut>>, u64)> = None;
    let mut spare: Option<BytesMut> = None;
    let mut index = 0;

    while index < chunks.len() {
        let current = chunks[index];
        let start = current.offset_content;
        let Some(end) = chunk_end(&chunks, index, file_size) else {
            lore_base::lore_trace!(
                "Previous chunk {index} at offset {start} does not ascend, cannot compare {}",
                path.display()
            );
            return Ok(FileMatch::Indeterminate);
        };
        let chunk_size = end - start;

        lore_base::lore_trace!(
            "Chunk {index} offset {start} to next offset {end}, size {chunk_size} in {}",
            path.display()
        );

        if chunk_size > crate::compress::FRAGMENT_SIZE_THRESHOLD as u64 {
            lore_base::lore_trace!("Hash checking recursively fragmented chunks");
            let sub_options = ReadOptions::default().no_decompress().no_verify();
            let Ok((sub_fragment, sub_payload)) = load_fragment(
                Arc::clone(sublists.store),
                sublists.partition,
                Address {
                    context: sublists.context,
                    hash: current.hash,
                },
                sub_options,
                sublists.remote_session.clone(),
            )
            .await
            else {
                return Ok(FileMatch::Indeterminate);
            };

            if sub_fragment.flags & FragmentFlags::PayloadFragmented == 0 {
                lore_base::lore_warn!("Subfragment was not expected fragment list");
                return Ok(FileMatch::Indeterminate);
            }

            // A window already covering these bytes stays usable: the sublist tiles the
            // range the window was filled with.
            let sub_payload = sub_payload.to_aligned::<FragmentReference>();
            let subfragment_list = sub_payload.as_type_slice::<FragmentReference>();
            let mut remain = if index < chunks.len() - 1 {
                chunks.split_off(index + 1)
            } else {
                vec![]
            };
            chunks.pop();
            chunks.extend_from_slice(subfragment_list);
            chunks.append(&mut remain);
            lore_base::lore_trace!(
                "Added {} chunks for recursive checking",
                subfragment_list.len()
            );
            continue;
        }

        if end > file_size {
            lore_base::lore_trace!(
                "Previous chunk {index} [{start}..{end}] extends beyond file end, cannot compare {}",
                path.display()
            );
            return Ok(FileMatch::Indeterminate);
        }

        let resident = match window.take() {
            Some(resident) if resident.holds(start, end) => resident,
            stale_window => {
                // The window it held is the one the next read fills.
                if let Some(stale) = stale_window {
                    spare = Some(stale.data);
                }
                let read = match pending.take() {
                    Some((task, offset)) if offset == start => task,
                    other => {
                        // Unreachable while the list ascends, since a spliced sublist tiles
                        // the range it replaces. Kept because the failure it would allow is
                        // silent: "unchanged" for a file never compared. The detached read
                        // takes its buffer with it, so this one starts from a fresh window.
                        drop(other);
                        let buffer = take_window(&mut spare, capacity, window_length(start));
                        start_window_read(file, buffer, start)
                    }
                };
                let data = read
                    .await
                    .map_err(|e| {
                        StorageError::internal_with_context(e, "hash compare read task failure")
                    })?
                    .map_err(|e| {
                        StorageError::internal_with_context(
                            e,
                            &format!("read file: {}", path.display()),
                        )
                    })?;
                let resident = HashWindow {
                    offset: start,
                    data,
                };

                // Started before anything in this window is hashed, so the two overlap.
                if let Some(offset) = next_window_offset(&chunks, index, resident.end(), file_size)
                {
                    let buffer = take_window(&mut spare, capacity, window_length(offset));
                    pending = Some((start_window_read(file, buffer, offset), offset));
                }
                resident
            }
        };

        if Hash::hash_buffer(resident.slice(start, end)) != current.hash {
            lore_base::lore_trace!(
                "Checking previous chunk {index} [{start}..{end}] hash yielded different file hash, abandon {}",
                path.display()
            );
            return Ok(FileMatch::Differs);
        }
        lore_base::lore_trace!(
            "Checking previous chunk {index} [{start}..{end}] hash yielded same file hash, continue {}",
            path.display()
        );

        window = Some(resident);
        index += 1;
    }

    Ok(FileMatch::Match)
}

/// Follower future: waits for the leader token to fire, then observes the
/// terminal store state for `address`.
///
/// Returns `Ok(())` if the store now holds a full-match entry
/// with either [`PayloadStoredDurable`](FragmentFlags::PayloadStoredDurable) or
/// [`PayloadStoredLocal`](FragmentFlags::PayloadStoredLocal) set. Returns an
/// internal error if no terminal entry exists — that means the leader errored
/// out and we have nothing to dedup against.
///
/// The follower holds no memory permit and no buffer; the caller is expected
/// to have dropped both before invoking this future.
pub async fn follower_future(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    token: CancellationToken,
) -> Result<(), StorageError> {
    token.cancelled().await;
    match query_one(&store, partition, address).await {
        Ok(resolved)
            if resolved.match_made == StoreMatch::MatchFull
                && (resolved.stored_local || resolved.stored_durable) =>
        {
            Ok(())
        }
        _ => Err(StorageError::internal(format!(
            "leader upload failed for {address}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::local::immutable_store::ImmutableStoreSettings;
    use crate::local::immutable_store::LocalImmutableStore;
    use crate::test_util::TempDir;
    use crate::types::Partition;

    #[test]
    fn remote_put_retry_accepts_arc_storage_session_and_is_send() {
        // Compile-only: asserts remote_put_retry's signature takes
        // Arc<StorageSession> and returns a Send + 'static future, which is
        // required to call it from inside tokio::spawn in a leader task.
        fn ensure_spawn_ok<F, Fut>(_f: F)
        where
            F: FnOnce(Arc<StorageSession>, Address, Fragment, Option<Bytes>) -> Fut,
            Fut: std::future::Future<Output = Result<(), StorageError>> + Send + 'static,
        {
        }
        ensure_spawn_ok(remote_put_retry);
    }

    async fn make_test_store() -> (TempDir, Arc<dyn ImmutableStore>) {
        let dir = TempDir::new("lore-storage-follower-test-");
        let store = LocalImmutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create test store");
        (dir, store)
    }

    fn make_address(seed: u8) -> (Partition, Address) {
        let payload = vec![seed; 64];
        let hash = crate::hash::hash_slice(&payload);
        (
            Partition::from([seed; 16]),
            Address {
                hash,
                context: Context::from([seed; 16]),
            },
        )
    }

    #[tokio::test]
    async fn follower_returns_ok_when_leader_wrote_terminal_entry() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = make_address(0xAA);
        let payload = vec![0xAA; 64];
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
                Some(Bytes::from(payload)),
                false,
            )
            .await
            .expect("put terminal entry");

        let token = CancellationToken::new();
        token.cancel();
        follower_future(store, partition, address, token)
            .await
            .expect("follower should observe terminal entry stored locally");
    }

    #[tokio::test]
    async fn follower_returns_err_when_no_entry_exists() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = make_address(0xBB);

        let token = CancellationToken::new();
        token.cancel();
        let err = follower_future(store, partition, address, token)
            .await
            .expect_err("follower should fail when no terminal entry");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("leader upload failed"),
            "expected leader-fail diagnostic, got: {msg}"
        );
    }

    #[tokio::test]
    async fn follower_waits_for_token_before_querying() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = make_address(0xCC);
        let payload = vec![0xCC; 64];
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredDurable.bits(),
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };

        let token = CancellationToken::new();
        let follower = lore_base::lore_spawn!(follower_future(
            store.clone(),
            partition,
            address,
            token.clone(),
        ));

        // Follower is waiting on the token. Write the entry AFTER spawn, THEN cancel.
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
            .expect("put terminal entry");
        token.cancel();

        follower
            .await
            .expect("join follower")
            .expect("follower observed terminal entry stored durably");
    }

    /// Which resolutions name a source worth copying from. Everything here is about not naming one
    /// that would cost a refused round trip and an upload afterwards.
    mod copy_source_selection {
        use super::*;

        fn address() -> Address {
            Address {
                hash: crate::hash::hash_slice(b"copy source selection"),
                context: Context::from([0x01u8; 16]),
            }
        }

        fn durable(match_made: StoreMatch) -> StoreMatchResult {
            StoreMatchResult {
                match_made,
                partition: Partition::from([0x02u8; 16]),
                context: Context::from([0x03u8; 16]),
                stored_local: true,
                stored_durable: true,
            }
        }

        #[test]
        fn a_partition_match_names_what_the_resolution_found() {
            let source = copy_source(&durable(StoreMatch::MatchPartition), address())
                .expect("a durable partition match names a source");
            assert_eq!(source.partition, Partition::from([0x02u8; 16]));
            assert_eq!(source.address.hash, address().hash);
            assert_eq!(source.address.context, Context::from([0x03u8; 16]));
        }

        #[test]
        fn a_hash_match_names_the_partition_it_was_found_in() {
            let source = copy_source(&durable(StoreMatch::MatchHash), address())
                .expect("a durable hash match names a source");
            assert_eq!(source.partition, Partition::from([0x02u8; 16]));
        }

        /// A resolution that named no context leaves the source naming none either, which is what
        /// the store reads as any association in the partition.
        #[test]
        fn an_unnamed_context_stays_unnamed() {
            let resolved = StoreMatchResult {
                context: Context::default(),
                ..durable(StoreMatch::MatchPartition)
            };
            let source = copy_source(&resolved, address()).expect("still names a source");
            assert!(source.address.context.is_zero());
        }

        #[test]
        fn a_full_match_names_nothing() {
            assert!(copy_source(&durable(StoreMatch::MatchFull), address()).is_none());
        }

        #[test]
        fn no_match_names_nothing() {
            assert!(copy_source(&durable(StoreMatch::MatchNone), address()).is_none());
        }

        /// The local store holding an association says nothing about the peer holding it, and a
        /// copy naming a source the peer never received is a round trip that can only fail.
        #[test]
        fn a_match_the_peer_never_received_names_nothing() {
            let resolved = StoreMatchResult {
                stored_durable: false,
                ..durable(StoreMatch::MatchPartition)
            };
            assert!(copy_source(&resolved, address()).is_none());
        }

        #[test]
        fn a_match_without_a_partition_names_nothing() {
            let resolved = StoreMatchResult {
                partition: Partition::default(),
                ..durable(StoreMatch::MatchPartition)
            };
            assert!(copy_source(&resolved, address()).is_none());
        }

        /// A destination context of zero is not a self-copy: the partition match is the statement
        /// that this tuple is not one of the associations the source names.
        #[test]
        fn a_zero_destination_context_still_names_a_source() {
            let destination = Address {
                hash: address().hash,
                context: Context::default(),
            };
            assert!(copy_source(&durable(StoreMatch::MatchPartition), destination).is_some());
        }
    }

    fn make_input(seed: u8) -> (Partition, Address, Fragment, Bytes) {
        let payload = vec![seed; 64];
        let hash = crate::hash::hash_slice(&payload);
        let partition = Partition::from([seed; 16]);
        let address = Address {
            hash,
            context: Context::from([seed; 16]),
        };
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        (partition, address, fragment, Bytes::from(payload))
    }

    /// [`make_input`] rehomed under `partition`.
    ///
    /// [`STORE_IN_FLIGHT`] is keyed on partition and address alone and carries no store identity,
    /// so every test deriving its partition from the same seeds shares in-flight entries with the
    /// rest of the process. A partition of its own keeps a test's leaders and followers to itself.
    fn make_input_in(partition: Partition, seed: u8) -> (Partition, Address, Fragment, Bytes) {
        let (_, address, fragment, buffer) = make_input(seed);
        (partition, address, fragment, buffer)
    }

    #[tokio::test]
    async fn store_fragment_no_tracker_writes_synchronously() {
        let (_dir, store) = make_test_store().await;
        let (partition, address, fragment, buffer) = make_input(0x10);

        let result = store_fragment(
            store.clone(),
            partition,
            address,
            fragment,
            buffer,
            true,
            None,
            WriteContext::none(),
            None,
        )
        .await
        .expect("synchronous store_fragment");

        assert_eq!(result.address, address);
        assert!(!result.deduplicated);

        // Entry should be present in the store after the call returns.
        let query = store
            .get_metadata(partition, address)
            .await
            .expect("query after sync write");
        assert_eq!(query.match_made, StoreMatch::MatchFull);
        assert_ne!(
            query.fragment.flags & FragmentFlags::PayloadStoredLocal.bits(),
            0,
            "sync write should leave PayloadStoredLocal set"
        );
    }

    #[tokio::test]
    async fn store_fragment_already_durable_short_circuits() {
        let (_dir, store) = make_test_store().await;
        let (partition, address, mut fragment, buffer) = make_input(0x20);
        // Pre-populate with a durable entry.
        fragment.flags = FragmentFlags::PayloadStoredDurable.bits();
        store
            .clone()
            .put(partition, address, fragment, Some(buffer.clone()), false)
            .await
            .expect("pre-populate durable entry");

        let tracker = Arc::new(WriteTracker::new());
        let fresh_fragment = Fragment {
            flags: 0,
            size_payload: buffer.len() as u32,
            size_content: buffer.len() as u64,
        };
        let result = store_fragment(
            store.clone(),
            partition,
            address,
            fresh_fragment,
            buffer.clone(),
            false,
            None,
            WriteContext::tracked(Some(tracker.clone()), None),
            None,
        )
        .await
        .expect("store_fragment against already-durable entry");

        assert!(result.deduplicated, "should dedup on already-durable");
        assert!(
            result.stored_durable,
            "result should report the entry as durable"
        );
        // Tracker should have no outstanding work.
        assert!(tracker.await_all().await.is_ok());
    }

    #[tokio::test]
    async fn store_fragment_follower_path_registers_in_tracker() {
        let (_dir, store) = make_test_store().await;
        let (partition, address, fragment, buffer) = make_input(0x30);

        // Manually hold a STORE_IN_FLIGHT guard to force the follower path.
        let held_guard =
            try_acquire_in_flight(partition, address).expect("acquire in-flight guard");

        let tracker = Arc::new(WriteTracker::new());
        let result = store_fragment(
            store.clone(),
            partition,
            address,
            fragment,
            buffer,
            false,
            None,
            WriteContext::tracked(Some(tracker.clone()), None),
            None,
        )
        .await
        .expect("store_fragment in follower path");
        assert!(result.deduplicated, "follower path should report dedup");

        // Drop the guard — this cancels the token. Follower queries the store
        // and sees no entry → returns an error.
        drop(held_guard);

        let await_result = tracker.await_all().await;
        let err = await_result.expect_err("follower sees no entry, errors");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("leader upload failed"),
            "expected follower's leader-fail error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn store_fragment_leader_path_spawns_into_tracker_no_remote() {
        let (_dir, store) = make_test_store().await;
        let (partition, address, fragment, buffer) = make_input(0x40);

        let tracker = Arc::new(WriteTracker::new());
        let result = store_fragment(
            store.clone(),
            partition,
            address,
            fragment,
            buffer,
            true,
            None,
            WriteContext::tracked(Some(tracker.clone()), None),
            None,
        )
        .await
        .expect("store_fragment leader spawn");
        assert!(!result.deduplicated);

        // Leader hasn't necessarily finished yet. Await tracker to drain.
        tracker.await_all().await.expect("tracker await_all");

        // After await_all, the entry should be in the store.
        let query = store
            .get_metadata(partition, address)
            .await
            .expect("query after leader completed");
        assert_eq!(query.match_made, StoreMatch::MatchFull);
        assert_ne!(
            query.fragment.flags & FragmentFlags::PayloadStoredLocal.bits(),
            0,
            "leader (no remote) should leave PayloadStoredLocal set"
        );
    }

    /// Wrapper that delegates to an inner `ImmutableStore` but forces `put`
    /// to fail. Exercises the error-terminal lifecycle state: a leader
    /// task whose terminal write fails surfaces the error through the
    /// tracker's `await_all`.
    struct FailingPutStore {
        inner: Arc<dyn ImmutableStore>,
    }

    #[async_trait::async_trait]
    impl ImmutableStore for FailingPutStore {
        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        fn is_local(&self) -> bool {
            self.inner.clone().is_local()
        }

        async fn query(
            self: Arc<Self>,
            partition: Partition,
            addresses: &[Address],
            results: &mut [StoreMatchResult],
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn get(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get(partition, address).await
        }

        async fn put(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _fragment: Fragment,
            _payload: Option<Bytes>,
            _force: bool,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("FailingPutStore: put disabled"))
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

    #[tokio::test]
    async fn leader_error_surfaces_through_tracker_await_all() {
        let (_dir, inner) = make_test_store().await;
        let failing: Arc<dyn ImmutableStore> = Arc::new(FailingPutStore { inner });
        let (partition, address, fragment, buffer) = make_input(0x60);
        let tracker = Arc::new(WriteTracker::new());

        // Sync path returns before the leader has run. write_raw (which calls
        // put) is inside the leader task; its error surfaces via await_all.
        let result = store_fragment(
            failing.clone(),
            partition,
            address,
            fragment,
            buffer,
            true,
            None,
            WriteContext::tracked(Some(tracker.clone()), None),
            None,
        )
        .await
        .expect("sync path returns Ok — work is deferred to the leader");
        assert!(!result.deduplicated);

        // Await the tracker. The leader's write_raw must fail and the error
        // must propagate through the tracker.
        let err = tracker
            .await_all()
            .await
            .expect_err("leader put fails; tracker surfaces error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("FailingPutStore") || msg.contains("put"),
            "expected diagnostic mentioning the failing put, got: {msg}"
        );

        // After await_all returns, no terminal entry exists — confirming the
        // leader's failure left the store in its original empty state.
        let resolved = query_one(&(failing as Arc<dyn ImmutableStore>), partition, address)
            .await
            .expect("resolve on empty store");
        assert_eq!(resolved.match_made, StoreMatch::MatchNone);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_writers_of_same_address_dedup_through_tracker() {
        // Two concurrent store_fragment calls for the same (partition, address)
        // should produce exactly one leader task and one follower — both calls
        // must succeed, and the store must end up with one terminal entry.
        let (_dir, store) = make_test_store().await;
        let (partition, address, fragment, buffer) = make_input(0x50);
        let tracker = Arc::new(WriteTracker::new());

        let call = |cache_local| {
            let store = store.clone();
            let buffer = buffer.clone();
            let tracker = tracker.clone();
            async move {
                store_fragment(
                    store,
                    partition,
                    address,
                    fragment,
                    buffer,
                    cache_local,
                    None,
                    WriteContext::tracked(Some(tracker), None),
                    None,
                )
                .await
            }
        };

        let (r1, r2) = tokio::join!(call(true), call(true));
        let r1 = r1.expect("first writer");
        let r2 = r2.expect("second writer");

        // Exactly one of the two calls is the leader (deduplicated == false);
        // the other is a follower (deduplicated == true via the in-flight
        // short-circuit).
        let leader_count = usize::from(!r1.deduplicated) + usize::from(!r2.deduplicated);
        assert_eq!(
            leader_count, 1,
            "expected exactly one leader, got {leader_count} (r1.dedup={}, r2.dedup={})",
            r1.deduplicated, r2.deduplicated
        );

        // Both calls return the same address.
        assert_eq!(r1.address, address);
        assert_eq!(r2.address, address);

        // Drain the tracker so the leader task and follower future complete.
        tracker
            .await_all()
            .await
            .expect("tracker await_all succeeds");

        // Exactly one terminal entry exists in the store.
        let query = store
            .get_metadata(partition, address)
            .await
            .expect("query after concurrent writers");
        assert_eq!(query.match_made, StoreMatch::MatchFull);
        assert_ne!(
            query.fragment.flags & FragmentFlags::PayloadStoredLocal.bits(),
            0,
            "terminal entry should carry PayloadStoredLocal"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn many_concurrent_writers_all_succeed_with_single_upload() {
        // Generalisation of the 2-writer case: N concurrent writers on the
        // same address all succeed, exactly one becomes the leader.
        let (_dir, store) = make_test_store().await;
        let (partition, address, fragment, buffer) = make_input(0x51);
        let tracker = Arc::new(WriteTracker::new());

        const N: usize = 64;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let store = store.clone();
            let buffer = buffer.clone();
            let tracker = tracker.clone();
            handles.push(lore_base::lore_spawn!(async move {
                store_fragment(
                    store,
                    partition,
                    address,
                    fragment,
                    buffer,
                    true,
                    None,
                    WriteContext::tracked(Some(tracker), None),
                    None,
                )
                .await
            }));
        }

        let mut leader_count = 0usize;
        for h in handles {
            let r = h.await.expect("join").expect("store_fragment success");
            if !r.deduplicated {
                leader_count += 1;
            }
        }
        assert_eq!(leader_count, 1, "expected 1 leader across {N} writers");
        tracker
            .await_all()
            .await
            .expect("tracker await_all succeeds");

        let query = store.get_metadata(partition, address).await.expect("query");
        assert_eq!(query.match_made, StoreMatch::MatchFull);
    }

    /// Wrapper that delegates to an inner `ImmutableStore`, holding `put` back
    /// so a test can say when one finishes, and counting the ones that have.
    ///
    /// `delay` sleeps inside `put`, simulating a slow backing store or, by
    /// analogy, a high-RTT remote. `gate` instead parks `put` until the test
    /// hands out a permit, which makes "no put has finished" a fact a counter
    /// reports rather than a wall-clock comparison: on a loaded machine the
    /// time a call takes says more about the machine than about the code.
    struct DelayingPutStore {
        inner: Arc<dyn ImmutableStore>,
        delay: std::time::Duration,
        gate: Option<Arc<tokio::sync::Semaphore>>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ImmutableStore for DelayingPutStore {
        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        fn is_local(&self) -> bool {
            self.inner.clone().is_local()
        }

        async fn query(
            self: Arc<Self>,
            partition: Partition,
            addresses: &[Address],
            results: &mut [StoreMatchResult],
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn get(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get(partition, address).await
        }

        async fn put(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            fragment: Fragment,
            payload: Option<Bytes>,
            force: bool,
        ) -> Result<(), StoreError> {
            tokio::time::sleep(self.delay).await;
            if let Some(gate) = self.gate.clone() {
                gate.acquire().await.expect("gate closed").forget();
            }
            let result = self
                .inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await;
            self.completed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            result
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

    /// The tracker's win is that `store_fragment` hands the write to a leader
    /// task instead of awaiting it, so a caller's cost stops scaling with the
    /// store's latency.
    ///
    /// The inline half pays that latency and is measured: `put` really sleeps,
    /// and a loaded machine only makes the wait longer, so the lower bound
    /// holds however busy the machine is.
    ///
    /// The deferred half is not measured. Every `put` parks on a gate holding
    /// no permits, so the run asserts that all `N` calls returned while nothing
    /// had been written — true whatever the machine does with the tasks in the
    /// meantime — and then releases the gate and drains. Comparing the two
    /// wall-clock times instead would assert a ratio between a path that waits
    /// on timers and one that waits on the scheduler, which contention moves
    /// by two orders of magnitude in opposite directions.
    ///
    /// Both halves write under a partition of this test's own, so a gated
    /// leader parked here can neither be joined by another test's write nor
    /// stand in for one.
    ///
    /// `GATE_GUARD` bounds the parked half so a regression that put inline
    /// under a tracker fails rather than hanging. It is not a latency
    /// assertion: the calls it covers await no timer, and the budget is orders
    /// of magnitude above what they take.
    #[tokio::test(flavor = "multi_thread")]
    async fn tracker_parallelises_writes_vs_inline_serialisation() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        const N: usize = 100;
        const PUT_DELAY: Duration = Duration::from_millis(10);
        const GATE_GUARD: Duration = Duration::from_secs(120);

        let test_partition = Partition::from([0xB4u8; 16]);

        let (_dir, inner) = make_test_store().await;
        let inline_completed = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn ImmutableStore> = Arc::new(DelayingPutStore {
            inner,
            delay: PUT_DELAY,
            gate: None,
            completed: inline_completed.clone(),
        });

        let inline_start = tokio::time::Instant::now();
        for i in 0..N {
            let (partition, address, fragment, buffer) = make_input_in(test_partition, i as u8);
            store_fragment(
                store.clone(),
                partition,
                address,
                fragment,
                buffer,
                true,
                None,
                WriteContext::none(), // no tracker → inline path awaits the slow put.
                None,
            )
            .await
            .expect("inline store_fragment");
        }
        let inline_elapsed = inline_start.elapsed();

        assert_eq!(
            inline_completed.load(Ordering::SeqCst),
            N,
            "inline store_fragment must return with its put finished"
        );
        assert!(
            inline_elapsed >= PUT_DELAY * N as u32 / 2,
            "inline baseline too fast; got {inline_elapsed:?}, expected at least ~{:?}",
            PUT_DELAY * N as u32 / 2
        );

        let (_dir2, inner2) = make_test_store().await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let deferred_completed = Arc::new(AtomicUsize::new(0));
        let store2: Arc<dyn ImmutableStore> = Arc::new(DelayingPutStore {
            inner: inner2,
            delay: Duration::ZERO,
            gate: Some(gate.clone()),
            completed: deferred_completed.clone(),
        });
        let tracker = Arc::new(WriteTracker::new());

        let deferred_start = tokio::time::Instant::now();
        tokio::time::timeout(GATE_GUARD, async {
            for i in 0..N {
                let (partition, address, fragment, buffer) = make_input_in(test_partition, i as u8);
                store_fragment(
                    store2.clone(),
                    partition,
                    address,
                    fragment,
                    buffer,
                    true,
                    None,
                    WriteContext::tracked(Some(tracker.clone()), None),
                    None,
                )
                .await
                .expect("deferred store_fragment sync return");
            }
        })
        .await
        .expect("deferred store_fragment blocked on a put that cannot finish");
        let sync_return_elapsed = deferred_start.elapsed();

        assert_eq!(
            deferred_completed.load(Ordering::SeqCst),
            0,
            "deferred store_fragment must return before the store has written anything"
        );

        gate.add_permits(N);
        tracker.await_all().await.expect("tracker await_all");
        let deferred_total_elapsed = deferred_start.elapsed();

        assert_eq!(
            deferred_completed.load(Ordering::SeqCst),
            N,
            "await_all must drain every leader the tracker took on"
        );

        eprintln!(
            "latency bench N={N} delay={PUT_DELAY:?}: inline={inline_elapsed:?} \
             deferred_sync_return={sync_return_elapsed:?} deferred_total={deferred_total_elapsed:?}"
        );
    }

    /// Wrapper that delegates to an inner `ImmutableStore` and tracks how many
    /// `put` calls are in flight at any moment, plus the peak. Used by the
    /// permit-stress test to verify the budget invariant: peak concurrent
    /// buffers in the put pipeline never exceeds what the semaphore allows.
    ///
    /// The sleep inside `put` ensures multiple leaders actually overlap so the
    /// peak is observable — without it, puts can serialize fast enough that a
    /// passing test wouldn't prove anything.
    struct CountingPutStore {
        inner: Arc<dyn ImmutableStore>,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
        put_delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl ImmutableStore for CountingPutStore {
        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        fn is_local(&self) -> bool {
            self.inner.clone().is_local()
        }

        async fn query(
            self: Arc<Self>,
            partition: Partition,
            addresses: &[Address],
            results: &mut [StoreMatchResult],
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn get(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get(partition, address).await
        }

        async fn put(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            fragment: Fragment,
            payload: Option<Bytes>,
            force: bool,
        ) -> Result<(), StoreError> {
            use std::sync::atomic::Ordering as AtomicOrdering;
            let current = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.peak.fetch_max(current, AtomicOrdering::SeqCst);
            tokio::time::sleep(self.put_delay).await;
            let result = self
                .inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await;
            self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
            result
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

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_permit_stress_caps_concurrent_leaders_under_budget() {
        // REQ-F-2 (spec / plan task #15): configure the memory budget to a
        // small value, spawn leader tasks that would collectively need ~10x
        // the budget, and assert:
        //   (a) all spawned tasks reach a terminal state,
        //   (b) peak concurrent `put` calls ≤ budget / per-task permit cost,
        //   (c) every permit is released by the time await_all returns.
        //
        // A dedicated Arc<Semaphore> stands in for the global fragment
        // limiter (which is a process-wide OnceLock). `store_fragment`
        // accepts a pre-acquired OwnedSemaphorePermit, so callers can
        // transparently substitute any semaphore — the leader still owns
        // the permit for the duration of the buffer, which is what matters.
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering as AtomicOrdering;

        use tokio::sync::Semaphore;

        const PER_TASK_COST: u32 = crate::concurrency::FRAGMENT_MINIMUM_COST_KIB;
        const MAX_CONCURRENT: usize = 16;
        const BUDGET_PERMITS: usize = MAX_CONCURRENT * PER_TASK_COST as usize;
        const N: usize = MAX_CONCURRENT * 10;
        const PUT_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

        let semaphore = Arc::new(Semaphore::new(BUDGET_PERMITS));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let (_dir, inner) = make_test_store().await;
        let store: Arc<dyn ImmutableStore> = Arc::new(CountingPutStore {
            inner,
            in_flight: in_flight.clone(),
            peak: peak.clone(),
            put_delay: PUT_DELAY,
        });
        let tracker = Arc::new(WriteTracker::new());

        // Dedicated per-test partition so the process-global STORE_IN_FLIGHT
        // map cannot collide with other tests running in parallel. Each task
        // within this test gets a unique `context` — combined with the
        // payload-derived hash, that produces N globally-unique addresses.
        let test_partition = Partition::from([0xA7u8; 16]);

        // Spawn N call-site coroutines. Each acquires its own permit from the
        // dedicated semaphore before calling store_fragment — mirroring the
        // production call pattern in write_content / write_fragmented.
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let semaphore = semaphore.clone();
            let store = store.clone();
            let tracker = tracker.clone();
            handles.push(lore_base::lore_spawn!(async move {
                // 64-byte buffers clamp to PER_TASK_COST permits via
                // fragment_permit_count. Distinct context per task keeps
                // addresses unique inside `test_partition`.
                let seed = i as u8;
                let payload = vec![seed; 64];
                let hash = crate::hash::hash_slice(&payload);
                let address = Address {
                    hash,
                    context: Context::from([seed; 16]),
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                let buffer = Bytes::from(payload);
                let permit = semaphore
                    .acquire_many_owned(PER_TASK_COST)
                    .await
                    .expect("semaphore not closed");
                store_fragment(
                    store,
                    test_partition,
                    address,
                    fragment,
                    buffer,
                    true,
                    None,
                    WriteContext::tracked(Some(tracker), None),
                    Some(permit),
                )
                .await
            }));
        }

        // All sync-path returns must succeed — the store hasn't errored, the
        // semaphore is large enough to eventually admit every task.
        for h in handles {
            h.await
                .expect("join spawner")
                .expect("store_fragment sync return");
        }

        // (a) All leaders reach a terminal state.
        tracker
            .await_all()
            .await
            .expect("await_all drains every leader without error");

        // (b) Peak concurrent `put` calls must not exceed the budget. This is
        // the safety property: more simultaneous buffers than the budget
        // allows would mean the permit stopped bounding memory.
        let observed_peak = peak.load(AtomicOrdering::SeqCst);
        assert!(
            observed_peak <= MAX_CONCURRENT,
            "peak concurrent put ({observed_peak}) exceeded budget ({MAX_CONCURRENT})"
        );
        // Sanity check: the test actually stressed the semaphore. If peak is
        // 1 the sleep/scheduling didn't produce overlap and the upper-bound
        // assertion above is vacuous.
        assert!(
            observed_peak > 1,
            "peak ({observed_peak}) too low to prove concurrency was exercised; \
             the test is not meaningfully validating the budget"
        );

        // (c) All permits are released. Every leader dropped its permit when
        // it dropped its buffer.
        assert_eq!(
            in_flight.load(AtomicOrdering::SeqCst),
            0,
            "puts still in flight after await_all"
        );
        assert_eq!(
            semaphore.available_permits(),
            BUDGET_PERMITS,
            "all permits must be released back to the semaphore"
        );
    }

    /// Deterministic and non-repeating, so a window read or sliced at the wrong offset
    /// changes the hash of every chunk it touches instead of comparing equal by accident.
    fn hash_test_content(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect()
    }

    /// The fragment list for `content` cut at `sizes`, hashed the way the compare path
    /// hashes the file.
    fn fragment_list_for(content: &[u8], sizes: &[usize]) -> Vec<FragmentReference> {
        let mut list = Vec::new();
        let mut offset = 0;
        for &size in sizes {
            list.push(FragmentReference {
                hash: Hash::hash_buffer(&content[offset..offset + size]),
                offset_content: offset as u64,
            });
            offset += size;
        }
        assert_eq!(
            offset,
            content.len(),
            "sizes must cover the content exactly"
        );
        list
    }

    /// Chunk sizes covering `length` in `size` steps plus whatever remains. A step that
    /// does not divide the window is the interesting case: chunks then straddle window
    /// boundaries, which is what the read-ahead has to stitch together.
    fn chunk_sizes(length: usize, size: usize) -> Vec<usize> {
        let mut sizes = vec![size; length / size];
        if !length.is_multiple_of(size) {
            sizes.push(length % size);
        }
        sizes
    }

    async fn compare_file(content: &[u8], chunks: &[FragmentReference]) -> FileMatch {
        let (dir, store) = make_test_store().await;
        let path = PathBuf::from(dir.as_ref()).join("hash-compare.bin");
        std::fs::write(&path, content).expect("write test file");
        let (file, file_size) = crate::chunker::open_read(&path).await.expect("open");

        compare_previous_chunks(
            SublistSource {
                store: &store,
                partition: Partition::from([7u8; 16]),
                context: Address::default().context,
                remote_session: &None,
            },
            &path,
            &file,
            file_size,
            chunks,
        )
        .await
        .expect("compare must not error on a readable file")
    }

    #[tokio::test]
    async fn an_unchanged_file_matches_across_every_window() {
        // Five windows' worth, cut so no chunk boundary lands on a window boundary.
        let content = hash_test_content(5 * HASH_WINDOW_SIZE + 4_321);
        let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));
        assert!(chunks.len() > 20, "test wants many chunks per window");

        assert_eq!(
            compare_file(&content, &chunks).await,
            FileMatch::Match,
            "unchanged file must match its own fragment list"
        );
    }

    #[tokio::test]
    async fn a_single_window_file_matches() {
        let content = hash_test_content(HASH_WINDOW_SIZE - 17);
        let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 200_000));

        assert_eq!(
            compare_file(&content, &chunks).await,
            FileMatch::Match,
            "file smaller than one window must match"
        );
    }

    /// The chunk is in the third window, so detecting it proves later windows are read at
    /// the offset their chunks are hashed against — a window off by even one byte here
    /// mismatches for a file that is in fact unchanged, and every `status` would
    /// re-fragment it.
    #[tokio::test]
    async fn a_byte_changed_in_a_late_chunk_differs() {
        let content = hash_test_content(5 * HASH_WINDOW_SIZE + 4_321);
        let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));

        let mut changed = content.clone();
        let victim = 2 * HASH_WINDOW_SIZE + 11;
        changed[victim] ^= 0xff;

        assert_eq!(
            compare_file(&changed, &chunks).await,
            FileMatch::Differs,
            "a changed byte in the third window is a difference in content"
        );
    }

    /// The same list against a shorter file. The last chunk is measured to the end of the
    /// file rather than to the offset the list records, so the missing bytes show up as the
    /// content difference they are.
    #[tokio::test]
    async fn a_file_shorter_than_its_list_differs() {
        let content = hash_test_content(3 * HASH_WINDOW_SIZE);
        let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));

        assert_eq!(
            compare_file(&content[..content.len() - 1_000], &chunks).await,
            FileMatch::Differs,
        );
    }

    /// A list whose offsets do not ascend describes something other than this file, and the
    /// walk reports that as a difference: the list is what defines the ranges being hashed,
    /// so one that misdescribes the content is indistinguishable from content that changed.
    /// The chunk size used to be an unchecked subtraction, which underflowed on it.
    #[tokio::test]
    async fn a_list_that_does_not_ascend_differs_without_underflowing() {
        let content = hash_test_content(3 * HASH_WINDOW_SIZE);
        let mut chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));
        chunks.swap(1, 2);

        assert_eq!(compare_file(&content, &chunks).await, FileMatch::Differs);
    }

    /// A chunk over the threshold is itself a fragment list, and the walk compares that
    /// list's chunks instead. Those bytes are already in the resident window, which is why
    /// the splice does not invalidate it.
    #[tokio::test]
    async fn a_recursively_fragmented_chunk_is_compared_through_its_sublist() {
        let content = hash_test_content(3 * HASH_WINDOW_SIZE + 4_321);
        let nested = 300 * 1024;
        let (chunks, store, partition, path, _dir) =
            recursive_case(&content, nested, &chunk_sizes(nested, 100 * 1024)).await;

        assert_eq!(
            compare_recursive(&store, partition, &path, &content, &chunks).await,
            FileMatch::Match,
            "unchanged file must match through the sublist"
        );
    }

    /// A nested entry pointing at a sublist nothing stored leaves the walk unable to read
    /// the bytes it would have compared. Reporting that as a content difference would drop
    /// the caller's content-comparison fallback for a file that may well be unchanged.
    #[tokio::test]
    async fn a_sublist_that_cannot_be_loaded_is_indeterminate() {
        let content = hash_test_content(3 * HASH_WINDOW_SIZE + 4_321);
        let nested = 300 * 1024;
        let (mut chunks, store, partition, path, _dir) =
            recursive_case(&content, nested, &chunk_sizes(nested, 100 * 1024)).await;
        chunks[0].hash = Hash::hash_buffer(b"a sublist that was never stored");

        assert_eq!(
            compare_recursive(&store, partition, &path, &content, &chunks).await,
            FileMatch::Indeterminate,
            "an unloadable sublist settles nothing about the content"
        );
    }

    /// Proves the sublist is genuinely compared rather than accepted because its parent
    /// entry loaded: the changed byte is only covered by a sub-chunk hash.
    #[tokio::test]
    async fn a_byte_changed_inside_a_recursively_fragmented_chunk_differs() {
        let content = hash_test_content(3 * HASH_WINDOW_SIZE + 4_321);
        let nested = 300 * 1024;
        let (chunks, store, partition, path, _dir) =
            recursive_case(&content, nested, &chunk_sizes(nested, 100 * 1024)).await;

        let mut changed = content.clone();
        changed[250 * 1024] ^= 0xff;
        std::fs::write(&path, &changed).expect("rewrite test file");

        assert_eq!(
            compare_recursive(&store, partition, &path, &changed, &chunks).await,
            FileMatch::Differs,
            "a changed byte inside the nested range is a difference in content"
        );
    }

    /// A file whose first `nested` bytes are one fragmented chunk, cut into `sub_sizes`,
    /// with that sublist stored so the walk can load it. Sublist offsets are absolute in
    /// the whole content, which is what lets the splice produce a flat list.
    async fn recursive_case(
        content: &[u8],
        nested: usize,
        sub_sizes: &[usize],
    ) -> (
        Vec<FragmentReference>,
        Arc<dyn ImmutableStore>,
        Partition,
        PathBuf,
        TempDir,
    ) {
        use zerocopy::IntoBytes;

        let (dir, store) = make_test_store().await;
        let path = PathBuf::from(dir.as_ref()).join("hash-compare-nested.bin");
        std::fs::write(&path, content).expect("write test file");
        let partition = Partition::from([7u8; 16]);

        let sublist = fragment_list_for(&content[..nested], sub_sizes);
        let payload = Bytes::copy_from_slice(sublist.as_slice().as_bytes());
        let sublist_hash = crate::hash::hash_slice(&payload);
        store_fragment(
            Arc::clone(&store),
            partition,
            Address {
                context: Address::default().context,
                hash: sublist_hash,
            },
            Fragment {
                flags: FragmentFlags::PayloadFragmented.bits(),
                size_payload: payload.len() as u32,
                size_content: nested as u64,
            },
            payload,
            true,
            None,
            WriteContext::none(),
            None,
        )
        .await
        .expect("store sublist");

        // The nested chunk stands in for its whole range, followed by ordinary chunks.
        let mut chunks = vec![FragmentReference {
            hash: sublist_hash,
            offset_content: 0,
        }];
        let mut offset = nested;
        for size in chunk_sizes(content.len() - nested, 100_003) {
            chunks.push(FragmentReference {
                hash: Hash::hash_buffer(&content[offset..offset + size]),
                offset_content: offset as u64,
            });
            offset += size;
        }

        (chunks, store, partition, path, dir)
    }

    async fn compare_recursive(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        path: &Path,
        content: &[u8],
        chunks: &[FragmentReference],
    ) -> FileMatch {
        let (file, file_size) = crate::chunker::open_read(path).await.expect("open");
        assert_eq!(file_size, content.len() as u64);

        compare_previous_chunks(
            SublistSource {
                store,
                partition,
                context: Address::default().context,
                remote_session: &None,
            },
            path,
            &file,
            file_size,
            chunks,
        )
        .await
        .expect("compare must not error on a readable file")
    }

    #[test]
    fn the_read_ahead_starts_at_the_first_chunk_the_window_does_not_hold() {
        let content = hash_test_content(3 * HASH_WINDOW_SIZE);
        let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));
        let file_size = content.len() as u64;

        let offset = next_window_offset(&chunks, 0, HASH_WINDOW_SIZE as u64, file_size)
            .expect("a chunk must straddle the first window boundary");
        let straddling = chunks
            .iter()
            .position(|chunk| chunk.offset_content == offset)
            .expect("offset must be a chunk boundary");
        assert!(
            offset < HASH_WINDOW_SIZE as u64,
            "the chunk starts inside the window it is not held by"
        );
        assert!(
            chunk_end(&chunks, straddling, file_size).expect("ascending") > HASH_WINDOW_SIZE as u64,
            "and ends past it"
        );
    }

    #[test]
    fn there_is_nothing_to_read_ahead_at_the_end_of_the_list() {
        let content = hash_test_content(HASH_WINDOW_SIZE);
        let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));

        assert_eq!(
            next_window_offset(&chunks, 0, content.len() as u64, content.len() as u64),
            None,
            "a window reaching the end of the file has no successor"
        );
    }

    /// A chunk over the threshold is fragmented further, so the walk loads its sublist
    /// rather than reading those bytes. Reading ahead there would read a window nothing
    /// asks for.
    #[test]
    fn there_is_nothing_to_read_ahead_before_a_recursively_fragmented_chunk() {
        let content = hash_test_content(2 * HASH_WINDOW_SIZE);
        let sizes = vec![
            crate::compress::FRAGMENT_SIZE_THRESHOLD,
            content.len() - crate::compress::FRAGMENT_SIZE_THRESHOLD,
        ];
        let chunks = fragment_list_for(&content, &sizes);

        assert_eq!(
            next_window_offset(
                &chunks,
                0,
                crate::compress::FRAGMENT_SIZE_THRESHOLD as u64,
                content.len() as u64
            ),
            None,
            "the next chunk is over the threshold and is not read as bytes"
        );
    }
}
