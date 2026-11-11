// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use bytes::BytesMut;
use dashmap::DashMap;
use dashmap::Entry;
use futures::FutureExt;
use lore_base::types::KeyType;
use lore_error_set::prelude::*;
use lore_transport::StorageSession;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zerocopy::FromZeros;

use crate::compress::COMPRESSION_MODE;
use crate::concurrency::file_count_limit_acquire;
use crate::content::ContentHandle;
use crate::content::ContentSource;
use crate::content::VirtualContent;
use crate::content::WindowRead;
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

/// Writes in flight, keyed by the store object, partition and address: each entry holds the token
/// cancelled when its guard drops, landed or not, and the store object it writes, so that no other
/// store object takes the object address its key holds while the entry exists.
static STORE_IN_FLIGHT: OnceLock<DashMap<StoreInFlightKey, StoreInFlight>> = OnceLock::new();

/// A write in flight: the token cancelled when its guard drops, and the store object it writes.
struct StoreInFlight {
    token: CancellationToken,
    _store: Arc<dyn ImmutableStore>,
}

/// The store a single-flight writes or reads into, by the address of the store object.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct FlightStore(std::num::NonZeroUsize);

impl FlightStore {
    /// The identity of `store`.
    #[lore_macro::test_pub]
    pub(crate) fn of(store: &dyn ImmutableStore) -> Self {
        Self(std::ptr::NonNull::from(store).cast::<()>().addr())
    }
}

/// A store object, partition and address: the key of a write in flight, and the association a
/// read single-flight reads into.
#[lore_macro::test_pub]
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct StoreInFlightKey {
    pub store: FlightStore,
    pub partition: Partition,
    pub address: Address,
}

impl StoreInFlightKey {
    /// The key of `address` in `partition` of `store`.
    fn of(store: &dyn ImmutableStore, partition: Partition, address: Address) -> Self {
        Self {
            store: FlightStore::of(store),
            partition,
            address,
        }
    }
}

/// Stores that have left [`STORE_IN_FLIGHT`], counted per bucket of addresses before each entry
/// goes: a reader that read its bucket's count before a local probe that missed, and finds no
/// entry afterwards, tells from the count whether a store may have landed in between.
static STORES_LANDED: [AtomicU64; LANDED_BUCKETS] = [const { AtomicU64::new(0) }; LANDED_BUCKETS];
const LANDED_BUCKETS: usize = 1024;

fn landed_bucket(address: Address) -> &'static AtomicU64 {
    let hash = address.hash.as_ref();
    &STORES_LANDED[u16::from_le_bytes([hash[0], hash[1]]) as usize % LANDED_BUCKETS]
}

/// How many stores of addresses in `address`'s bucket have left [`STORE_IN_FLIGHT`].
#[lore_macro::test_pub]
pub(crate) fn stores_landed(address: Address) -> u64 {
    landed_bucket(address).load(Ordering::SeqCst)
}

/// RAII guard that removes the in-flight entry and notifies waiters on drop.
pub struct StoreInFlightGuard {
    key: StoreInFlightKey,
}

impl Drop for StoreInFlightGuard {
    fn drop(&mut self) {
        // Before the entry goes, so a reader that finds it gone sees the count moved.
        landed_bucket(self.key.address).fetch_add(1, Ordering::SeqCst);
        #[cfg(feature = "test-util")]
        crate::test_util::guard_midpoint(self.key.address, || {
            STORE_IN_FLIGHT
                .get()
                .is_some_and(|in_flight| in_flight.contains_key(&self.key))
        });
        if let Some(in_flight) = STORE_IN_FLIGHT.get()
            && let Some((_, flight)) = in_flight.remove(&self.key)
        {
            // Let waiters know we have finished the request that was in flight
            flight.token.cancel();
        }
    }
}

/// The guard of a write of `address` in `partition` of `store`, where no other write of it is in
/// flight; otherwise waits for the one in flight to land and answers `None`.
pub async fn stored_in_flight(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> Option<StoreInFlightGuard> {
    match try_acquire_in_flight(store, partition, address) {
        Ok(guard) => Some(guard),
        Err(token) => {
            token.cancelled().await;
            None
        }
    }
}

/// Non-blocking attempt to acquire the in-flight guard for `address` in `partition` of `store`.
///
/// Returns `Ok(guard)` if no one else is currently writing this address to this store — the
/// caller becomes the leader and must drop the guard when the terminal store entry is written.
///
/// Returns `Err(token)` if another task already holds the guard. The token is
/// cancelled when that task drops its guard; callers that want to observe the
/// leader's outcome should await the token and then query the store.
pub fn try_acquire_in_flight(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> Result<StoreInFlightGuard, CancellationToken> {
    let key = StoreInFlightKey::of(store.as_ref(), partition, address);
    let in_flight = STORE_IN_FLIGHT.get_or_init(DashMap::new);
    // `DashMap::entry` is safe here as it is not held across any awaits and no other locks are acquired while held
    #[allow(clippy::disallowed_methods)]
    match in_flight.entry(key.clone()) {
        Entry::Occupied(entry) => Err(entry.get().token.clone()),
        Entry::Vacant(entry) => {
            entry.insert(StoreInFlight {
                token: CancellationToken::new(),
                _store: store.clone(),
            });
            Ok(StoreInFlightGuard { key })
        }
    }
}

/// If another task is currently writing `address` in `partition` of `store`, wait for its
/// cancellation token so subsequent reads observe the terminal store entry the leader produces,
/// and answer true. Returns false immediately when no write is in flight. Looks for the write at
/// the call, so the future holds none of the arguments.
pub fn wait_if_in_flight(
    store: &dyn ImmutableStore,
    partition: Partition,
    address: Address,
) -> impl Future<Output = bool> + use<> {
    let key = StoreInFlightKey::of(store, partition, address);
    let token = STORE_IN_FLIGHT
        .get()
        .and_then(|in_flight| in_flight.get(&key).map(|entry| entry.value().token.clone()));
    futures::future::OptionFuture::from(token.map(CancellationToken::cancelled_owned))
        .map(|waited| waited.is_some())
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
/// one layer up rather than by any flag in `flags`. Publication costs no round trip of its own
/// where there is an upload for it to ride on; see [`write_resolved_content`] for the routing and
/// [`publish_resolved_mapping`] for the case there is not.
///
/// The mapping is only published remotely once the content it names is there, so a key never
/// resolves to content the server does not hold. A content upload that fails still leaves a
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

    if flags.hash_only {
        return Err(StorageError::internal(
            "a resolved write publishes a mapping, so it cannot address content without storing it",
        ));
    }

    if buffer.is_empty() {
        return retract_resolved_mapping(mutable, partition, key, context, remote_session).await;
    }

    let written = write_resolved_content(
        store,
        partition,
        key,
        context,
        buffer,
        flags,
        remote_session.clone(),
        writes,
        None,
    )
    .await?;

    publish_resolved_mapping(mutable, partition, key, written, remote_session).await
}

/// Store `buffer`, fusing a `KeyType::Resolve` mapping to `key` into whichever remote command
/// carries the content's top-level fragment. The content half of [`write_resolved`], shared with
/// [`write_resolved_from_file`] for a file small enough to become one fragment; the caller
/// publishes the mapping afterwards.
///
/// Three routes, by what the content needs:
/// - No session: nothing to fuse into, so an ordinary [`write_content`].
/// - One fragment: a single `put_resolved` carrying content and mapping together — the case
///   `write_resolved` exists for. The upload happens inside the ordinary write pipeline rather
///   than after it, so the content is compressed once and the local store is written once,
///   already carrying the durable flag and the `local_cache_priority` retention decision.
/// - Fragmented: the leaves upload through the ordinary path and the mapping fuses into the
///   upload of the fragment list's *root*, which is stored last — by then every leaf's placement
///   is known, and a leaf that missed the remote withdraws the key on the way down. See
///   [`FusedPublish`].
///
/// `permit` is the caller's memory reservation for `buffer`, or `None` to let the write reserve
/// its own.
#[allow(clippy::too_many_arguments)]
async fn write_resolved_content(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<StoreResult, StorageError> {
    if remote_session.is_none() {
        return write_content(
            store, partition, context, buffer, flags, None, writes, permit,
        )
        .await;
    }

    if buffer.len() <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
        return write_content_publishing(
            store,
            partition,
            context,
            buffer,
            flags,
            remote_session,
            writes,
            permit,
            key,
        )
        .await;
    }

    let size_content = buffer.len() as u64;
    let publish = FusedPublish::new(key);
    let (address, stored_local, stored_durable) = write_fragmented(
        store,
        partition,
        context,
        buffer,
        flags,
        remote_session,
        writes,
        permit,
        Some(publish.clone()),
    )
    .await?;
    Ok(StoreResult {
        address,
        size_content,
        stored_local,
        stored_durable,
        deduplicated: false,
        published: publish.published(),
    })
}

/// Retract `key` — the publish with nothing to publish, shared by the empty buffer
/// [`write_resolved`] takes and the empty file [`write_resolved_from_file`] takes.
///
/// The zero hash is the mutable store's tombstone, so removal is a store of it rather than a verb
/// of its own. The local mapping is cleared *first*, inverting the publish ordering deliberately:
/// if the remote call then fails, a read falls through to the remote — which still holds the live
/// mapping — instead of this store answering with a mapping the server has already dropped.
///
/// `stored_local` is true because the local mapping is gone by the time this returns;
/// `stored_durable` reports whether the remote was told, which only a caller that supplied a
/// session can expect.
async fn retract_resolved_mapping(
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<StoreResult, StorageError> {
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
    Ok(StoreResult {
        address,
        size_content: 0,
        stored_local: true,
        stored_durable: remote_cleared,
        deduplicated: false,
        published: false,
    })
}

/// Publish `key` as a `KeyType::Resolve` mapping to the content `written` came to rest at — the
/// tail both [`write_resolved`] and [`write_resolved_from_file`] end in, so a key published from a
/// buffer and one published from a file are published under the same rules.
///
/// Remotely, the mapping is only written once the content it names is there. `published` already
/// says the mapping rode along with the upload, so nothing more is owed; otherwise a durable
/// upload earns a `mutable_store` of its own — the case content already on the server takes, since
/// it uploads nothing for a key to ride on — and content that did not reach the remote earns
/// nothing but a warning: a failed upload leaves a good local write, so refusing to publish is
/// better than naming content the server does not hold. The caller reads `stored_durable` to tell
/// the two apart, and `published` to tell whether the mapping cost a round trip of its own.
///
/// The local mapping is stored unconditionally: it is what makes the content readable back on this
/// host, and it names content the local store took.
async fn publish_resolved_mapping(
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    written: StoreResult,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<StoreResult, StorageError> {
    let address = written.address;

    if let Some(session) = remote_session {
        if written.published {
            lore_base::lore_trace!("Key {key} published with the upload of {address}");
        } else if written.stored_durable {
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

    Ok(written)
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

#[lore_macro::test_pub]
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
/// When `remote_session` is `Some`, the fragment is made durable on the peer where
/// possible, by duplicating an association the peer already holds or else by
/// uploading the compressed payload. Durability affects the `PayloadStoredDurable`
/// flag and whether the payload is cached locally. Until the fragment is durable,
/// the payload is always cached locally so it isn't lost.
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
///
/// Returns `store_fragment_publishing`'s future itself: a future of its own would hold the
/// arguments again beside it.
#[allow(clippy::too_many_arguments)]
pub fn store_fragment(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> impl Future<Output = Result<StoreResult, StorageError>> {
    store_fragment_publishing(
        store,
        partition,
        address,
        fragment,
        Payload::Shared(buffer),
        cache_local,
        remote_session,
        writes,
        permit,
        None,
    )
}

/// The payload a store writes: a buffer the stores and transports can keep, or content its caller
/// lends for the store's duration.
///
/// Lent content is only read in place. The stores and transports take `Bytes` and receive the
/// compressed payload, the payload loaded back from the local store, or a copy of the content.
#[lore_macro::test_pub]
pub(crate) enum Payload<'a> {
    Shared(Bytes),
    Lent(&'a [u8]),
}

impl Payload<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Payload::Shared(buffer) => buffer,
            Payload::Lent(content) => content,
        }
    }

    /// A buffer the stores and transports can keep. Lent content is copied into one once, and the
    /// payload holds that copy from then on.
    #[lore_macro::test_pub]
    fn share(&mut self) -> Bytes {
        let buffer = match self {
            Payload::Shared(buffer) => return buffer.clone(),
            Payload::Lent(content) => Bytes::copy_from_slice(content),
        };
        *self = Payload::Shared(buffer.clone());
        buffer
    }

    /// The payload as a buffer the stores and transports can keep, copying lent content.
    #[lore_macro::test_pub]
    fn into_shared(self) -> Bytes {
        match self {
            Payload::Shared(buffer) => buffer,
            Payload::Lent(content) => Bytes::copy_from_slice(content),
        }
    }
}

/// A `KeyType::Resolve` mapping to publish in the same remote command that uploads a tree's
/// top-level fragment, and the report of whether it got there.
///
/// One shared value threaded down the fragmentation recursion, rather than a parameter going down
/// and a return field coming back: the key travels to whichever frame stores the root, and the
/// answer has to travel back up through every frame in between — none of which has anything of its
/// own to say about it.
///
/// A level **withdraws** the key instead of passing it on when it finds a child that did not reach
/// the remote, so the frame that stores the root only ever fuses a key naming a tree the server
/// holds whole. That is what makes the fusion safe: the root is stored last, and every descendant's
/// placement is already known by the time it is.
pub struct FusedPublish {
    key: Hash,
    published: AtomicBool,
}

impl FusedPublish {
    /// A request to publish `key` with the upload of the tree's top-level fragment.
    pub fn new(key: Hash) -> Arc<Self> {
        Arc::new(Self {
            key,
            published: AtomicBool::new(false),
        })
    }

    /// The key to fuse into the top-level fragment's upload.
    pub(crate) fn key(&self) -> Hash {
        self.key
    }

    /// Record that the upload carried the key, so the caller knows it owes no mapping write.
    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
    }

    /// Whether the key was published as part of an upload. False when the top-level fragment was
    /// already durable — no upload happened for the key to ride on — or when a level withdrew the
    /// key because the tree did not reach the remote whole.
    pub fn published(&self) -> bool {
        self.published.load(Ordering::Acquire)
    }
}

/// [`store_fragment`] for the one fragment whose upload should also publish `publish` as a
/// `KeyType::Resolve` mapping naming it: the single fragment of content that does not fragment, or
/// the top-level fragment of a tree that does. `None` is an ordinary store.
///
/// A publishing write is never dispatched into the tracker, whatever the caller's `writes` says. A
/// dispatched write returns before its leader has uploaded anything, so there would be no upload
/// for the key to ride on and no placement to report — the two are incompatible by construction
/// rather than by policy. It does take the in-flight guard, so concurrent writers of one address to
/// one store collapse onto a single upload; a publishing write whose leader left the content durable reports
/// `published = false` and its key follows as a `mutable_store`, the same round trip its own upload
/// would have cost and none of the payload. A leader that left the content *not* durable — one
/// writing locally, or one whose upload failed — cannot satisfy a publish, so this call uploads
/// unguarded rather than inherit a placement it needs and the leader never wanted.
///
/// `StoreResult::published` is false whenever no upload of this call's own carried the key —
/// content already durable, or another writer's upload deduplicated this one — so the caller still
/// owes the key a mapping write of its own.
///
/// A lent payload is stored inline too, as the dispatched leader outlives the call.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub(crate) fn store_fragment_publishing(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Payload<'_>,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
    publish: Option<Hash>,
) -> impl Future<Output = Result<StoreResult, StorageError>> {
    async move {
        if address.hash.is_zero() || buffer.as_slice().is_empty() || fragment.size_payload == 0 {
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
        if fragment.size_payload as usize != buffer.as_slice().len() {
            return Err(StorageError::internal(format!(
                "store_fragment buffer length mismatch: buffer {} vs size_payload {}",
                buffer.as_slice().len(),
                fragment.size_payload
            )));
        }

        writes.count(|stats| stats.fragment_produced(&fragment));

        let tracker = writes.tracker().cloned().filter(|_| publish.is_none());
        let result = if let Some(tracker) = tracker
            && let Payload::Shared(buffer) = buffer
        {
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
        } else {
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
                publish,
            )
            .await
        };

        if let (Some(tracker), Ok(result)) = (writes.tracker(), &result) {
            tracker.notify_fragment(&observed_fragment(fragment, result), result.deduplicated);
        }
        result
    }
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
/// to coordinate concurrent uploads of the same address from one store (so duplicate uploads
/// collapse onto one wire call), which is moot for pure-local writes. Concurrent local writers
/// may briefly do duplicate compression work, but the bucket-level write is content-addressed
/// and idempotent. Items with no remote consult must not enter the dedup tracker.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn store_fragment_inline(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Payload<'_>,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    writes: &WriteContext,
    permit: Option<OwnedSemaphorePermit>,
    publish: Option<Hash>,
) -> impl Future<Output = Result<StoreResult, StorageError>> {
    async move {
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
        if remote_session.is_none() {
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

        // Remote-coupled path: acquire the in-flight guard so a concurrent writer of the same
        // address to this store dedupes onto one upload.
        let guard = stored_in_flight(&store, partition, address).await;
        if guard.is_none()
            && let Some((stored_local, stored_durable)) =
                inherited_placement(&store, partition, address, publish).await
        {
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

        let placement = leader_body(
            store,
            partition,
            address,
            fragment,
            buffer,
            cache_local,
            remote_session,
            query,
            guard,
            writes.stats(),
            permit,
            publish,
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
}

/// The placement a write inherits from the task that was already storing this address, or `None`
/// when it has to store the content itself after all.
///
/// The flags are read after the wait rather than carried across it: the read taken before describes
/// a store the leader had not written yet, which reports a tree of identical leaves as partly
/// absent. A publishing write needs the content durable before its key may name it, and a leader
/// writing locally or failing its upload cannot supply that, so such a write declines to follow.
async fn inherited_placement(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    publish: Option<Hash>,
) -> Option<(bool, bool)> {
    let (stored_local, stored_durable) =
        stored_flags(&resolve_or_absent(store, partition, address).await);
    (publish.is_none() || stored_durable).then_some((stored_local, stored_durable))
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
    tracker.claim(&store).await;
    let guard = match try_acquire_in_flight(&store, partition, address) {
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
    // The leader takes the counters alone, never the tracker: the tracker is what
    // awaits this task, and `await_all` requires its handle to be the only one.
    let stats = writes.stats();
    tracker.spawn_leader(leader_body(
        store,
        partition,
        address,
        fragment,
        Payload::Shared(buffer),
        cache_local,
        remote_session,
        query,
        Some(guard),
        stats,
        permit,
        None,
    ));
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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

/// The outcome of asking a peer to duplicate an association it already holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The peer now holds the association durably.
    Copied,
    /// The request failed, the peer answered with something other than `SlowDown`, or the claim to
    /// the source partition couldn't be confirmed. Uploading the payload has the same effect as
    /// copying it.
    Refused,
    /// The peer was still answering `SlowDown` when the retries ran out.
    Throttled,
}

/// Ask the session's peer to duplicate the association `source_address` names in
/// `source_partition` into `context` on the session's partition, without transferring a payload.
///
/// A `SlowDown` is retried with the same back-off as an upload.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn remote_copy_retry(
    session: &StorageSession,
    source_partition: Partition,
    source_address: Address,
    context: Context,
) -> impl Future<Output = CopyOutcome> {
    async move {
        if !session.can_copy_from(source_partition).await {
            lore_base::lore_trace!(
                "No claim to partition {source_partition} to copy {} into context {context} from",
                source_address.hash
            );
            return CopyOutcome::Refused;
        }

        let mut retry = crate::store_retry();
        loop {
            match session
                .copy(source_partition, source_address, context)
                .await
            {
                Ok(()) => return CopyOutcome::Copied,
                Err(ref err) if err.is_slow_down() => {
                    if !retry.wait().await {
                        lore_base::lore_debug!(
                            "Copy of {} into context {context} from partition {source_partition} still throttled after {} retries",
                            source_address.hash,
                            retry.counter()
                        );
                        return CopyOutcome::Throttled;
                    }
                }
                Err(err) => {
                    lore_base::lore_debug!(
                        "Copy of {} into context {context} from partition {source_partition} refused ({err:?})",
                        source_address.hash
                    );
                    return CopyOutcome::Refused;
                }
            }
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

/// Where a fragment ended up once the leader finished with it.
///
/// `published` is separate from `durable` because a key rides along with an *upload*: content
/// already durable performs none, so its mapping still has to be written on its own.
struct Placement {
    local: bool,
    durable: bool,
    published: bool,
}

/// Performs the work for [`store_fragment`]. Optionally duplicates an association the peer already
/// holds. Otherwise, prepares a payload by loading an existing local payload whose sizes pass
/// [`crate::read::check_payload_sizes`], or by compressing the content. Uploads the payload unless
/// the peer throttled the copy, then writes the terminal entry.
///
/// Returns where the payload ended up.
///
/// `guard` is the in-flight token the caller acquired before invoking this function. When
/// `None`, no in-flight machinery is in play (the local-only fast path that bypasses the
/// dedup token entirely — see [`store_fragment_inline`]). When `Some`, dropping the guard at
/// the end cancels the token and wakes any followers subscribed to this write.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn leader_body(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    mut fragment: Fragment,
    mut buffer: Payload<'_>,
    cache_local: bool,
    remote_session: Option<Arc<StorageSession>>,
    query: StoreMatchResult,
    guard: Option<StoreInFlightGuard>,
    stats: Option<Arc<FragmentWriteStats>>,
    permit: Option<OwnedSemaphorePermit>,
    publish: Option<Hash>,
) -> impl Future<Output = Result<Placement, StorageError>> {
    async move {
        let (mut stored_local, mut stored_durable) = stored_flags(&query);
        let mut published = false;
        let stats = stats.as_deref();
        let mut registered_remotely = false;
        let mut copy_throttled = false;

        if let Some(stats) = stats {
            stats.fragment_processed(&fragment);
        }

        // Before the payload is prepared: succeeding means neither the load nor the compression below
        // is work this fragment has to pay for.
        if !stored_durable
            && let Some(session) = remote_session.as_ref()
            && let Some(source) = copy_source(&query, address)
        {
            match remote_copy_retry(session, source.partition, source.address, address.context)
                .await
            {
                CopyOutcome::Copied => {
                    REMOTE_COPIES.fetch_add(1, Ordering::Relaxed);
                    stored_durable = true;
                    registered_remotely = true;
                    if let Some(stats) = stats {
                        stats.remote_copy();
                    }
                }
                CopyOutcome::Refused => {}
                CopyOutcome::Throttled => copy_throttled = true,
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
                && crate::read::check_payload_sizes(&stored_fragment, stored_buffer.len()).is_ok()
            {
                fragment = stored_fragment;
                buffer = Payload::Shared(stored_buffer);
            } else {
                stored_local = false;
            }
        }

        // If we could not load from local store, try compressing the data
        let mode =
            crate::compress::CompressionMode::from_u32(COMPRESSION_MODE.load(Ordering::Relaxed));
        if payload_wanted
            && !stored_local
            && mode != crate::compress::CompressionMode::NoCompression
        {
            let _compress_permit = crate::concurrency::compress_limit_acquire().await;
            if let Ok((compressed_fragment, compressed_buffer)) = crate::compress::compress(
                fragment,
                &buffer.as_slice()[..fragment.size_payload as usize],
                mode,
            ) {
                lore_base::lore_trace!(
                    "Compressed {} bytes to {} bytes",
                    fragment.size_payload,
                    compressed_fragment.size_payload
                );
                fragment = compressed_fragment;
                buffer = Payload::Shared(compressed_buffer);
            }
        }

        if let Some(stats) = stats {
            if payload_wanted {
                stats.payload_prepared(&fragment);
            } else {
                stats.payload_not_prepared(&fragment);
            }
        }

        // The peer was still asking for back-off when the copy gave up, and an upload would send
        // it the whole payload. Keep it local and not durable, as after a failed upload.
        if !stored_durable
            && !copy_throttled
            && let Some(session) = remote_session.clone()
        {
            stored_durable = match publish {
                Some(key) => {
                    published = remote_put_resolved_retry(
                        session,
                        key,
                        address,
                        fragment,
                        Some(buffer.share()),
                    )
                    .await
                    .is_ok();
                    published
                }
                None => remote_put_retry(session, address, fragment, Some(buffer.share()))
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
            (Some(buffer.into_shared()), permit)
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
fn single_fragment(context: Context, buffer: &[u8], flags: WriteOptions) -> (Address, Fragment) {
    (
        Address {
            context,
            hash: hash::hash_slice(buffer),
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
/// Content larger than one fragment is rejected by [`store_fragment_publishing`] as oversized: it
/// has no single upload for the key to ride on, and reaches [`write_fragmented`] instead.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_content_publishing(
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
    let _in_flight = ContentWriteGuard::new();
    let (address, fragment) = single_fragment(context, &buffer, flags);
    let permit = match permit {
        Some(permit) => Some(permit),
        None => crate::concurrency::acquire_fragment_memory_permit(buffer.len()).await,
    };
    store_fragment_publishing(
        store,
        partition,
        address,
        fragment,
        Payload::Shared(buffer),
        flags.cache_local(),
        remote_session,
        writes,
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
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn write_content(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> impl Future<Output = Result<StoreResult, StorageError>> {
    async move {
        let _in_flight = (!flags.hash_only).then(ContentWriteGuard::new);
        // Check if data should be a single fragment
        if buffer.len() <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
            write_single_fragment(
                store,
                partition,
                context,
                Payload::Shared(buffer),
                flags,
                remote_session,
                writes,
                permit,
            )
            .await
        } else {
            let size_content = buffer.len() as u64;
            let (address, stored_local, stored_durable) = write_fragmented(
                store,
                partition,
                context,
                buffer,
                flags,
                remote_session,
                writes,
                permit,
                None,
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
}

/// [`write_content`] for content that fits one fragment.
///
/// [`write_from_file`] calls it directly for a file that fits one fragment, so that its future does
/// not hold the fragmented write of [`write_content`].
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn write_single_fragment(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Payload<'_>,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> impl Future<Output = Result<StoreResult, StorageError>> {
    async move {
        let (address, fragment) = single_fragment(context, buffer.as_slice(), flags);
        if flags.hash_only {
            return Ok(StoreResult {
                address,
                size_content: buffer.as_slice().len() as u64,
                stored_local: false,
                stored_durable: false,
                deduplicated: false,
                published: false,
            });
        }

        // Reuse the caller's read reservation if provided, else reserve here.
        let permit = match permit {
            Some(permit) => Some(permit),
            None => {
                crate::concurrency::acquire_fragment_memory_permit(buffer.as_slice().len()).await
            }
        };
        store_fragment_publishing(
            store,
            partition,
            address,
            fragment,
            buffer,
            flags.cache_local(),
            remote_session,
            writes,
            permit,
            None,
        )
        .await
    }
}

/// [`write_content`] for content its caller lends for the write's duration.
///
/// The content is only read in place, and the write holds no reference to it once done: the stores
/// and transports receive the compressed payload, the payload loaded back from the local store, or a
/// copy. Content larger than one fragment is copied first, as its chunks are stored by tasks that can
/// outlive the write; that path is cold, so it is boxed.
#[allow(clippy::too_many_arguments)]
pub async fn write_content_borrowed(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: &[u8],
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<StoreResult, StorageError> {
    if buffer.len() > crate::compress::FRAGMENT_SIZE_THRESHOLD {
        return Box::pin(write_content(
            store,
            partition,
            context,
            Bytes::copy_from_slice(buffer),
            flags,
            remote_session,
            writes,
            permit,
        ))
        .await;
    }
    let _in_flight = (!flags.hash_only).then(ContentWriteGuard::new);
    write_single_fragment(
        store,
        partition,
        context,
        Payload::Lent(buffer),
        flags,
        remote_session,
        writes,
        permit,
    )
    .await
}

/// Write content from a file.
///
/// Takes a store, partition, and optional remote session directly.
///
/// Returns the address and the size of the content behind it. The size is reported because a
/// caller that hands over a path has no other way to learn what was actually written: stating the
/// file again afterwards answers for the file as it is then, not for the bytes this address
/// stands for.
///
/// A `path` that does not exist or does not name a regular file is `InvalidArguments`; see
/// [`ContentSource::open`]. A zero-length file yields the zero-hash address without being read.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn write_from_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    source: &ContentSource<'_>,
    context: Context,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
) -> impl Future<Output = Result<StoreResult, StorageError>> {
    async move {
        let _in_flight = (!flags.hash_only).then(ContentWriteGuard::new);
        let _count_permit = file_count_limit_acquire()
            .await
            .forward::<StorageError>("permit failed")?;
        let (handle, size) = source.open().await?;

        lore_base::lore_trace!(
            "Opened content to read for immutable data write: {source} size {size}"
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
            let buffer = handle.read_all(size).await.map_err(|err| {
                StorageError::internal_with_context(err, &format!("read content: {source}"))
            })?;
            let address = write_single_fragment(
                store,
                partition,
                context,
                Payload::Shared(buffer),
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
                handle,
                size,
                flags,
                remote_session,
                writes,
                None,
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
}

/// [`write_from_file`] plus publication of `key` as a `KeyType::Resolve` mapping to what the file
/// stored as — [`write_resolved`] taking its content from a path instead of a buffer, so a caller
/// publishing a file never has to hold it.
///
/// Only the fragment being written is resident. A file at or below
/// [`crate::compress::FRAGMENT_SIZE_THRESHOLD`] is read once into the one fragment it becomes and
/// takes [`write_resolved_content`]'s routing from there. A larger file chunks straight off disk
/// through [`crate::fragment_engine::write_fragmented_from_file`], so memory follows the leaf
/// rather than the file however large it is, and the mapping fuses into the upload of the fragment
/// list's root under the same rules.
///
/// An empty file **retracts** `key`, the same way an empty buffer does in [`write_resolved`]: a
/// mapping to the zero hash is the mutable store's tombstone, so there is no distinction to draw
/// between publishing empty content and publishing none.
///
/// A `path` that does not exist or does not name a regular file is `InvalidArguments` rather than a
/// retraction; see [`ContentSource::open`]. That check is what keeps a directory — whose reported size
/// is whatever the filesystem chooses, and may be zero — from retracting a live key.
#[allow(clippy::too_many_arguments)]
pub async fn write_resolved_from_file(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    path: &Path,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    writes: WriteContext,
) -> Result<StoreResult, StorageError> {
    if key.is_zero() {
        return Err(StorageError::internal(
            "a zero key cannot be published; it is the mutable store's tombstone value",
        ));
    }

    if flags.hash_only {
        return Err(StorageError::internal(
            "a resolved write publishes a mapping, so it cannot address content without storing it",
        ));
    }

    let _in_flight = ContentWriteGuard::new();
    let _count_permit = file_count_limit_acquire()
        .await
        .forward::<StorageError>("permit failed")?;
    let source = ContentSource::file(path);
    let (handle, size) = source.open().await?;

    lore_base::lore_trace!(
        "Opened file to publish under key {key}: {} size {size}",
        path.display(),
    );

    if size == 0 {
        return retract_resolved_mapping(mutable, partition, key, context, remote_session).await;
    }

    let size = size as usize;
    let written = if size <= crate::compress::FRAGMENT_SIZE_THRESHOLD {
        let read_permit = crate::concurrency::acquire_fragment_memory_permit(size).await;
        let buffer = handle.read_all(size).await.map_err(|err| {
            StorageError::internal_with_context(err, &format!("read content: {source}"))
        })?;
        write_resolved_content(
            store,
            partition,
            key,
            context,
            buffer,
            flags,
            remote_session.clone(),
            writes,
            read_permit,
        )
        .await?
    } else {
        let publish = remote_session.as_ref().map(|_| FusedPublish::new(key));
        let (address, stored_local, stored_durable) =
            crate::fragment_engine::write_fragmented_from_file(
                store,
                partition,
                context,
                handle,
                size,
                flags,
                remote_session.clone(),
                writes,
                publish.clone(),
            )
            .await?;
        StoreResult {
            address,
            size_content: size as u64,
            stored_local,
            stored_durable,
            deduplicated: false,
            published: publish.is_some_and(|publish| publish.published()),
        }
    };

    publish_resolved_mapping(mutable, partition, key, written, remote_session).await
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
    /// The content could not be read, so the comparison never happened. Distinct from an error,
    /// which is the comparison itself failing rather than the content being beyond reach.
    Unreadable,
}

/// Whether the file `content` answers for still holds the content `previous` addresses.
///
/// Transfers fragment metadata only: the stored object's header and, when it is fragmented,
/// its fragment lists. Content payloads are never fetched — chunks are compared by hashing
/// the file's own bytes over the ranges the stored list records, so the cost is bounded by
/// the file and its metadata however large the object is.
///
/// Below the minimum cut the content is one fragment whatever cut it, so its own hash is the
/// address and settles the question without touching the store. Up to the threshold it may be
/// either, and the stored header says which: one fragment is settled by the content hash, a
/// list by the chunking it records. Larger content is always a list, which one raw read takes
/// along with its header.
///
/// A stored object that cannot be read falls back to [`hashed_under_current_chunking`],
/// which reads nothing but the file. Virtual immutable content is settled by the hash of the
/// address it names, reading nothing. Metadata fetched from the remote is checked on receipt
/// unless the reader trusts the remote (`trust_remote`).
#[allow(clippy::too_many_arguments)]
pub async fn file_matches(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    previous: Address,
    previous_size: Option<usize>,
    remote_session: Option<Arc<StorageSession>>,
    trust_remote: bool,
    source: &ContentSource<'_>,
    established: &ContentHashes,
) -> Result<FileMatch, StorageError> {
    let _count_permit = file_count_limit_acquire()
        .await
        .forward::<StorageError>("permit failed")?;

    let content = ContentHashMemo::new(source, established);
    let Ok(file_size) = source.size().await else {
        return Ok(FileMatch::Unreadable);
    };
    let file_size = file_size as usize;

    if let Some(settled) = established.decides(
        previous,
        previous_size.map(|size| size as u64),
        file_size as u64,
    ) {
        return Ok(settled);
    }

    if let ContentSource::Virtual(VirtualContent::Immutable { address, .. }) = source {
        return Ok(if address.hash == previous.hash {
            FileMatch::Match
        } else {
            FileMatch::Differs
        });
    }

    if file_size <= crate::concurrency::FRAGMENT_SIZE_MINIMUM
        || (file_size <= crate::compress::FRAGMENT_SIZE_THRESHOLD
            && stored_as_one_fragment(&store, partition, previous).await)
    {
        return Ok(content
            .whole_content_matches(file_size, previous.hash)
            .await);
    }

    let options = ReadOptions {
        trust_remote,
        ..ReadOptions::default()
    }
    .no_decompress();
    let Some((fragment, payload)) = load_fragment(
        store.clone(),
        partition,
        previous,
        options,
        remote_session.clone(),
    )
    .await
    .ok() else {
        if file_size <= crate::compress::FRAGMENT_SIZE_THRESHOLD
            && content
                .whole_content_matches(file_size, previous.hash)
                .await
                == FileMatch::Match
        {
            return Ok(FileMatch::Match);
        }
        return hashed_under_current_chunking(store, partition, previous, file_size, &content)
            .await;
    };

    if fragment.size_content != file_size as u64 {
        return Ok(FileMatch::Differs);
    }

    if fragment.flags & FragmentFlags::PayloadFragmented == 0 {
        return Ok(content
            .whole_content_matches(file_size, previous.hash)
            .await);
    }

    let fragment_list = payload.to_aligned::<FragmentReference>();
    let previous_fragmentation = fragment_list.as_type_slice::<FragmentReference>();
    if !previous_fragmentation.is_empty() {
        let Ok((file, _)) = source.open().await else {
            return Ok(FileMatch::Unreadable);
        };
        match compare_previous_chunks(
            SublistSource {
                store: &store,
                partition,
                context: previous.context,
                remote_session: &remote_session,
                trust_remote,
            },
            source,
            &file,
            file_size as u64,
            previous_fragmentation,
        )
        .await?
        {
            settled @ (FileMatch::Match | FileMatch::Differs | FileMatch::Unreadable) => {
                return Ok(settled);
            }
            FileMatch::Indeterminate => {}
        }
    }

    hashed_under_current_chunking(store, partition, previous, file_size, &content).await
}

/// Whether the store describes `previous` as one fragment, whose payload is the content
/// itself. `false` where it is a list or where nothing describes it, both of which the header
/// alone cannot settle.
async fn stored_as_one_fragment(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    previous: Address,
) -> bool {
    store
        .clone()
        .get_metadata(partition, previous)
        .await
        .is_ok_and(|described| {
            described.match_made != StoreMatch::MatchNone
                && described.fragment.flags & FragmentFlags::PayloadFragmented == 0
        })
}

/// What comparing a file established about its content, each computed at most once however many
/// addresses the file is measured against: the hash of the whole content, which answers for
/// content stored as a single fragment, and the hash the current chunking produces, which answers
/// where nothing describes the stored object.
///
/// Both are functions of the content alone, so neither is keyed by the address that prompted it.
/// Neither answers for a list, so a comparison holding one still walks the chunking that list
/// records.
///
/// A caller measuring one file against several addresses holds one of these across them, and
/// holds nothing it can read: what the cells contain is for the comparison to fill and consult.
///
/// Each cell is written by whichever comparison computes it first and read by every comparison
/// after. Two comparisons racing for the same cell both compute it and agree, since each value is
/// a function of the content; the memo spares work rather than serialising it, so sharing one
/// across tasks costs the work it was held to save.
///
/// What is established answers for the content as it was read. A caller that writes the file
/// starts a new one, or the comparisons that follow answer for content that is gone. The file's
/// size is not among it, so a file deleted under a run of comparisons is still seen.
#[lore_macro::test_pub]
#[derive(Default)]
pub struct ContentHashes {
    whole: std::sync::OnceLock<Hash>,
    chunked: std::sync::OnceLock<Hash>,
}

impl ContentHashes {
    /// The answer for `previous` where the size, the address or what is already established
    /// decides it, reaching neither the content nor the store. `None` is the question still worth
    /// asking.
    ///
    /// `file_size` is the size the caller measured, which the answer is only as current as.
    pub fn decides(
        &self,
        previous: Address,
        previous_size: Option<u64>,
        file_size: u64,
    ) -> Option<FileMatch> {
        if previous_size.is_some_and(|size| size != file_size) {
            return Some(FileMatch::Differs);
        }
        if file_size == 0 {
            // Empty is empty under any fragmentation.
            return Some(if previous.hash.is_zero() {
                FileMatch::Match
            } else {
                FileMatch::Differs
            });
        }
        if previous.is_zero() {
            return Some(FileMatch::Differs);
        }
        // Below the minimum cut the content is one chunk however it was stored, so the whole
        // content's hash answers for any address.
        if file_size as usize <= crate::concurrency::FRAGMENT_SIZE_MINIMUM {
            return Some(if *self.whole.get()? == previous.hash {
                FileMatch::Match
            } else {
                FileMatch::Differs
            });
        }
        None
    }
}

/// A file's content paired with what comparing it has established, which is what one comparison
/// reads through.
struct ContentHashMemo<'a> {
    source: &'a ContentSource<'a>,
    established: &'a ContentHashes,
}

impl<'a> ContentHashMemo<'a> {
    fn new(source: &'a ContentSource<'a>, established: &'a ContentHashes) -> Self {
        Self {
            source,
            established,
        }
    }

    /// The hash of the whole content taken as one buffer, which is what answers for content
    /// stored as a single fragment. Computed at most once per [`ContentHashes`], and `None` where
    /// the content could not be read.
    ///
    /// The whole content is resident while it is hashed, so the budget for it comes from the
    /// fragment limiter that bounds every other buffer of a fragment's size.
    async fn whole_content_hash(&self, file_size: usize) -> Option<Hash> {
        if let Some(hash) = self.established.whole.get() {
            return Some(*hash);
        }

        let hash = {
            let _memory_permit =
                crate::concurrency::acquire_fragment_memory_permit(file_size).await;
            Hash::hash_buffer(&self.source.read_all().await.ok()?)
        };
        Some(*self.established.whole.get_or_init(|| hash))
    }

    /// How the whole content compares to `previous`, which is what answers for content stored as
    /// a single fragment. Unreadable content is no answer rather than either one.
    async fn whole_content_matches(&self, file_size: usize, previous: Hash) -> FileMatch {
        match self.whole_content_hash(file_size).await {
            Some(hash) if hash == previous => FileMatch::Match,
            Some(_) => FileMatch::Differs,
            None => FileMatch::Unreadable,
        }
    }

    /// The address the current chunking produces from the content, which is what answers where
    /// nothing describes the stored object. Computed at most once per [`ContentHashes`], and
    /// `None` where the content could not be opened.
    ///
    /// Chunks are read on demand and none is stored, so the cost is the content read once.
    async fn chunked_content_hash(
        &self,
        store: Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        file_size: usize,
    ) -> Result<Option<Hash>, StorageError> {
        if let Some(hash) = self.established.chunked.get() {
            return Ok(Some(*hash));
        }

        let Ok((file, _)) = self.source.open().await else {
            return Ok(None);
        };
        let address = crate::fragment_engine::write_fragmented_from_file(
            store,
            partition,
            context,
            file,
            file_size,
            WriteOptions::default().no_remote_write().hash_only(),
            None,
            WriteContext::none(),
            None,
        )
        .await?;
        Ok(Some(
            *self.established.chunked.get_or_init(|| address.0.hash),
        ))
    }
}

/// Whether hashing the file under the current chunking reproduces `previous`.
///
/// The fallback for a stored object that could not be described or walked, which is what a
/// clone into a directory of existing files sees: nothing is in the local store yet, so
/// there is no fragmentation to measure against. A file the current chunker was what stored
/// still hashes to the address it was stored under, and that settles it while reading
/// nothing but the file. A different hash settles nothing, since the stored object may have
/// been chunked another way.
///
/// Called only above the minimum cut, where the content may be a list.
async fn hashed_under_current_chunking(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    previous: Address,
    file_size: usize,
    content: &ContentHashMemo<'_>,
) -> Result<FileMatch, StorageError> {
    let Some(hash) = content
        .chunked_content_hash(store, partition, previous.context, file_size)
        .await?
    else {
        return Ok(FileMatch::Unreadable);
    };

    Ok(if hash == previous.hash {
        FileMatch::Match
    } else {
        FileMatch::Indeterminate
    })
}

/// The hash of the address the content of `source` would be stored under.
///
/// Addressed by the same rule that stores it, so the answer is the address the content has:
/// [`write_from_file`] with nothing written. Content is cut and hashed either way, since an
/// address is a function of the chunking as much as of the content.
///
/// Whether a file still holds content already stored is [`file_matches`], which measures against
/// the fragmentation that content was stored under rather than the one cutting it now produces.
///
/// Returns [`write_from_file`]'s future mapped to the hash, without a future of its own.
pub fn hash_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    source: &ContentSource<'_>,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<Hash, StorageError>> {
    write_from_file(
        store,
        partition,
        source,
        Context::default(),
        WriteOptions::default().no_remote_write().hash_only(),
        remote_session,
        WriteContext::none(),
    )
    .map(|written| written.map(|written| written.address.hash))
}

/// One read covering several consecutive chunks. Sized like the chunker's window and for
/// the same reason: nothing compared against a window exceeds
/// [`FRAGMENT_SIZE_THRESHOLD`](crate::compress::FRAGMENT_SIZE_THRESHOLD), so a window
/// starting on a chunk boundary always holds at least one whole chunk and the walk cannot
/// stall.
#[lore_macro::test_pub]
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
    handle: &ContentHandle,
    buffer: BytesMut,
    offset: u64,
) -> JoinHandle<std::io::Result<BytesMut>> {
    let handle = handle.clone();
    let want = buffer.len();
    lore_base::lore_spawn!(async move {
        handle
            .read_window(
                WindowRead {
                    buffer,
                    start: 0,
                    want,
                },
                offset,
            )
            .await
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
struct SublistSource<'a> {
    store: &'a Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    remote_session: &'a Option<Arc<StorageSession>>,
    trust_remote: bool,
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
#[lore_macro::test_pub]
async fn compare_previous_chunks(
    sublists: SublistSource<'_>,
    source: &ContentSource<'_>,
    handle: &ContentHandle,
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
                source
            );
            return Ok(FileMatch::Indeterminate);
        };
        let chunk_size = end - start;

        lore_base::lore_trace!(
            "Chunk {index} offset {start} to next offset {end}, size {chunk_size} in {}",
            source
        );

        if chunk_size > crate::compress::FRAGMENT_SIZE_THRESHOLD as u64 {
            lore_base::lore_trace!("Hash checking recursively fragmented chunks");
            let sub_options = ReadOptions {
                trust_remote: sublists.trust_remote,
                ..ReadOptions::default()
            }
            .no_decompress();
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
                source
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
                        start_window_read(handle, buffer, start)
                    }
                };
                let data = read
                    .await
                    .map_err(|e| {
                        StorageError::internal_with_context(e, "hash compare read task failure")
                    })?
                    .map_err(|e| {
                        StorageError::internal_with_context(e, &format!("read file: {source}"))
                    })?;
                let resident = HashWindow {
                    offset: start,
                    data,
                };

                // Started before anything in this window is hashed, so the two overlap.
                if let Some(offset) = next_window_offset(&chunks, index, resident.end(), file_size)
                {
                    let buffer = take_window(&mut spare, capacity, window_length(offset));
                    pending = Some((start_window_read(handle, buffer, offset), offset));
                }
                resident
            }
        };

        if Hash::hash_buffer(resident.slice(start, end)) != current.hash {
            lore_base::lore_trace!(
                "Checking previous chunk {index} [{start}..{end}] hash yielded different file hash, abandon {}",
                source
            );
            return Ok(FileMatch::Differs);
        }
        lore_base::lore_trace!(
            "Checking previous chunk {index} [{start}..{end}] hash yielded same file hash, continue {}",
            source
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
