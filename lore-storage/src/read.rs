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
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_error_set::prelude::*;
use lore_transport::StorageSession;

use crate::compress;
use crate::concurrency::file_count_limit_acquire;
use crate::defragment::DefragmentSink;
use crate::defragment::defragment_pipeline;
use crate::defragment::defragment_pipeline_leaves;
use crate::defragment::read_defragment;
use crate::error::StorageError;
use crate::errors::SlowDown;
use crate::fragment_flags::FragmentFlags;
use crate::hash;
use crate::immutable_store::ImmutableStore;
use crate::immutable_store::StoreError;
use crate::immutable_store::claim_for_task;
use crate::mutable_store::MutableStore;
use crate::options::ReadOptions;
use crate::store_types::PayloadRead;
use crate::types::Address;
use crate::types::Fragment;
use crate::types::Partition;

/// Load a single raw fragment from store with retry backoff. How widely the store searches for it
/// is the store's own business - see [`ImmutableStore::read_scope`]. The payload is returned as
/// stored, with the content the store expanded where it checked the payload on receipt; nothing
/// here hashes it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn read_raw(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> impl Future<Output = Result<Loaded, StorageError>> {
    async move {
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
                .and_then(|mut data| {
                    let content = data.content.take();
                    data.into_payload().map(|raw| Loaded { content, raw })
                }) {
                Ok(loaded) => return Ok(loaded),
                Err(StoreError::SlowDown(_)) => {}
                Err(StoreError::AddressNotFound(_) | StoreError::PayloadNotFound(_)) => {
                    return Err(StorageError::from(crate::errors::AddressNotFound::from(
                        address,
                    )));
                }
                Err(err) => return Err(err).forward::<StorageError>("store get failed"),
            }
            if !retry.wait().await {
                return Err(StorageError::from(SlowDown));
            }
        }
    }
}

/// The content `payload` holds and the fragment describing it, its sizes checked
/// ([`check_payload_sizes`]): a compressed payload expanded, any other as it is.
pub fn expand_payload(
    fragment: Fragment,
    payload: Bytes,
) -> Result<(Fragment, Bytes), StorageError> {
    check_payload_sizes(&fragment, payload.len())?;
    if (fragment.flags & FragmentFlags::PayloadCompressed) == 0 {
        return Ok((fragment, payload));
    }
    let (fragment, content) = compress::decompress(fragment, payload.as_ref())
        .forward::<StorageError>("failed to decompress fragment")?;
    Ok((fragment, content.freeze()))
}

/// Process-wide count of remote fetches in flight across every [`remote_get_retry`] path; shared by all concurrent operations, layer per-op attribution on top if needed.
pub static REMOTE_FETCH_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// See [`REMOTE_FETCH_INFLIGHT`].
pub fn remote_fetch_inflight() -> u64 {
    REMOTE_FETCH_INFLIGHT.load(Ordering::Relaxed)
}

/// Process-wide count of payload bytes fetched from a remote, by every [`load_fragment`] path and
/// every caller that fetches through a session directly and adds what it received.
pub static REMOTE_FETCH_BYTES: AtomicU64 = AtomicU64::new(0);

/// See [`REMOTE_FETCH_BYTES`].
pub fn remote_fetch_bytes() -> u64 {
    REMOTE_FETCH_BYTES.load(Ordering::Relaxed)
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

/// Fetch a fragment from a remote session. A `SlowDown` is retried with back-off, and a
/// `NotConnected` or a refusal, as
/// [`protocol_error_to_storage`](crate::error::protocol_error_to_storage) maps a failure of the
/// session, is retried on a fresh session as [`SessionRetries::retry`] bounds and paces it.
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
    #[cfg(feature = "test-util")]
    if let Some(served) = crate::test_util::remote_payload(address) {
        crate::test_util::remote_gate().await;
        return Ok(served);
    }
    let mut retry = crate::store_retry();
    let mut session_retries = SessionRetries::default();
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
                if !session_retries
                    .retry(&storage_err, session, &mut retry)
                    .await
                {
                    return Err(storage_err);
                }
            }
        }
    }
}

/// Bound on retries for `StorageError::NotConnected` in a remote read ([`SessionRetries`]).
/// Picked so a genuinely permanent server-side failure surfaces quickly
/// rather than looping through the full `store_retry` backoff schedule (60
/// attempts, up to 1 s apart).
const MAX_STALE_SESSION_RETRIES: u8 = 5;

/// The retries a remote read has made on a fresh session ([`StorageSession::invalidate`]).
#[derive(Default)]
struct SessionRetries {
    /// Retries of a `NotConnected`, a session the server may no longer know.
    stale: u8,
    /// Whether a refusal was retried.
    refused: bool,
}

impl SessionRetries {
    /// Whether a read that failed with `err` is retried on a fresh session, counting the retry: a
    /// `NotConnected` up to [`MAX_STALE_SESSION_RETRIES`] times, and a refusal once, since an
    /// expired token is refused and a fresh session can carry a fresh one.
    fn admit(&mut self, err: &StorageError) -> bool {
        match err {
            StorageError::NotConnected(_) if self.stale < MAX_STALE_SESSION_RETRIES => {
                self.stale += 1;
                true
            }
            StorageError::NotAuthenticated(_) | StorageError::NotAuthorized(_) if !self.refused => {
                self.refused = true;
                true
            }
            _ => false,
        }
    }

    /// Whether a read that failed with `err` is retried, readying the retry. An admitted failure
    /// ([`admit`](Self::admit)) invalidates `session`, so a lazy one starts its server-side
    /// sessions again. A `NotConnected` then waits on `back_off`, and is not retried once that is
    /// spent; a refusal is retried at once, as only a fresh session can change its outcome.
    async fn retry(
        &mut self,
        err: &StorageError,
        session: &StorageSession,
        back_off: &mut crate::Retry,
    ) -> bool {
        if !self.admit(err) {
            return false;
        }
        session.invalidate().await;
        !err.is_not_connected() || back_off.wait().await
    }
}

/// Outcome of probing the local store for a fragment, distinguishing a plain miss (nothing to
/// report, fall through to remote) from data that was found but fails its size check or to
/// decompress (the refetched payload overwrites the local copy), from a durable payload this build
/// no longer decodes (also refetched and overwritten), and from a fatal error that must
/// short-circuit the caller immediately rather than fall through.
enum LocalFailure {
    Corrupt,
    /// A durably stored payload in an encoding this build no longer decodes, which the remote
    /// holds re-encoded. Carries the decode error to report when there is no remote to fetch
    /// from.
    DurableMigrated(StorageError),
    Other,
    Fatal(StorageError),
}

/// Probe the local store for `address`: read, check the payload's sizes ([`check_payload_sizes`]),
/// and decompress when `options.decompress` asks, taking the content the store expanded where it
/// checked the payload. The payload is trusted as stored: nothing here hashes it. A payload the
/// store answers `NotSupported` for is fatal, as a non-durable payload this build cannot decode is,
/// and any other store failure is a miss. A caller disables it via `options.local`: a remote-only
/// handle, or a read that has probed already.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn probe_local(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
) -> impl Future<Output = Result<(Fragment, Bytes), LocalFailure>> {
    async move {
        if !options.local {
            return Err(LocalFailure::Other);
        }

        match read_raw(store, partition, address).await {
            Ok(loaded) => {
                let flags = loaded.raw.0.flags;
                let durable_oodle = (flags & FragmentFlags::PayloadCompressedOodle2) != 0
                    && (flags & FragmentFlags::PayloadStoredDurable) != 0;
                let loaded = if options.decompress {
                    loaded.take(true)
                } else {
                    check_payload_sizes(&loaded.raw.0, loaded.raw.1.len()).map(|()| loaded.raw)
                };
                match loaded {
                    Ok((fragment, buffer)) => Ok((fragment, buffer)),
                    // The remote re-encodes Oodle on ingress, so it holds a durable payload in a
                    // form this build decodes. A local-only one has no such copy to fall back on.
                    Err(err) if durable_oodle && matches!(err, StorageError::NotSupported(_)) => {
                        Err(LocalFailure::DurableMigrated(err))
                    }
                    Err(err) if matches!(err, StorageError::NotSupported(_)) => {
                        Err(LocalFailure::Fatal(err))
                    }
                    Err(err) => {
                        lore_base::lore_debug!("Fragment {} failed to load: {err}", address.hash);
                        Err(LocalFailure::Corrupt)
                    }
                }
            }
            Err(err) if err.is_not_supported() => Err(LocalFailure::Fatal(err)),
            Err(e) => {
                lore_base::lore_trace!(
                    "Fragment {} failed loading from local store: {e:?}",
                    address.hash
                );
                Err(LocalFailure::Other)
            }
        }
    }
}

/// The count of stores landed in `address`'s bucket, read before a local probe, for
/// [`stored_since`]; `None` where the load waits for no store, its local probe being off or
/// `options.never_wait` set. A hit needs no wait: a store publishes its entry only once the
/// payload is written, and the address names the content.
fn landed_before_probe(address: Address, options: ReadOptions) -> Option<u64> {
    (options.local && !options.never_wait).then(|| crate::write::stores_landed(address))
}

/// Whether a store of `address` may have written its payload since the count `landed` was read
/// ([`landed_before_probe`]): a store in flight is waited for, and one waited on or one landed
/// since may have written it, so only then is the local store worth probing again.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn stored_since(
    partition: Partition,
    address: Address,
    landed: Option<u64>,
) -> impl Future<Output = bool> {
    async move {
        let Some(landed) = landed else {
            return false;
        };
        crate::write::wait_if_in_flight(partition, address).await
            || crate::write::stores_landed(address) != landed
    }
}

/// Checks a remote payload on receipt, its sizes ([`check_payload_sizes`]) and its content against
/// `address`, and returns that content expanded.
pub fn verify_receipt(
    fragment: Fragment,
    buffer: Bytes,
    address: Address,
) -> Result<(Fragment, Bytes), StorageError> {
    let (fragment, content) = expand_payload(fragment, buffer)?;
    let content_hash = hash::hash_slice(content.as_ref());
    if content_hash != address.hash {
        return Err(StorageError::internal(format!(
            "fragment hash mismatch, got {content_hash}"
        )));
    }
    Ok((fragment, content))
}

/// Checks the sizes `fragment` gives against the payload's length `len` and, for a plain payload,
/// which is the content, against each other. Only the fragment is inspected, never the content, so
/// every payload has its sizes checked, whatever its source and however far that is trusted.
pub fn check_payload_sizes(fragment: &Fragment, len: usize) -> Result<(), StorageError> {
    crate::validate_fragment_payload(fragment, len)
        .and_then(|()| crate::immutable_store::validate_plain_payload_size(fragment))
        .forward::<StorageError>("fragment sizes disagree with its payload")
}

/// Checks `raw`, the payload at `address` as fetched, on receipt, and returns the content the check
/// expanded. From an untrusted remote that is [`verify_receipt`]. From a trusted one
/// ([`ReadOptions::trust_remote`]) only the sizes are checked, and the payload is expanded only
/// where `options.decompress` asks.
pub fn check_receipt(
    raw: &(Fragment, Bytes),
    address: Address,
    options: ReadOptions,
) -> Result<Option<(Fragment, Bytes)>, StorageError> {
    if !options.trust_remote {
        return verify_receipt(raw.0, raw.1.clone(), address).map(Some);
    }
    if !options.decompress {
        return check_payload_sizes(&raw.0, raw.1.len()).map(|()| None);
    }
    expand_payload(raw.0, raw.1.clone()).map(Some)
}

/// Fetches `address` from the remote through [`fetch_remote`], and stores it locally where
/// `options.cache`, `local_replace` or the fragment's local cache priority asks. A read
/// single-flight leader runs it for itself and its followers, and a follower whose leader failed on
/// another address runs it for itself; it answers the flight's form, so the flight awaits its
/// future directly rather than through a wrapper holding a second copy.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn fetch_and_store(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
    session: Arc<StorageSession>,
    local_replace: bool,
) -> impl Future<Output = Result<Loaded, StorageError>> {
    async move {
        let fetched = fetch_remote(session.as_ref(), address, options).await?;
        let (fragment, payload) = &fetched.raw;
        if options.cache
            || local_replace
            || (fragment.flags & FragmentFlags::PayloadLocalCachePriority) != 0
        {
            let _ = store
                .clone()
                .put(
                    partition,
                    address,
                    *fragment,
                    Some(payload.clone()),
                    local_replace,
                )
                .await;
        }
        Ok(fetched)
    }
}

/// Fetches `address` from the remote, retrying as [`remote_get_retry`] does, marks it durable, as
/// the remote holds it, and counts its payload bytes ([`remote_fetch_bytes`]): the payload as
/// fetched, unchecked.
async fn fetch_payload(
    session: &StorageSession,
    address: Address,
    priority: bool,
) -> Result<(Fragment, Bytes), StorageError> {
    let (mut fragment, buffer) = remote_get_retry(session, address, priority).await?;
    REMOTE_FETCH_BYTES.fetch_add(buffer.len() as u64, Ordering::Relaxed);
    fragment.flags |= FragmentFlags::PayloadStoredDurable;
    Ok((fragment, buffer))
}

/// Asks the remote to heal `address`, whose payload failed the check on receipt with `err`, once
/// per fetch: `heal_attempted` is whether this fetch already did. Answers whether it healed, and so
/// whether to fetch the payload again. A payload this build cannot decode (`NotSupported`) is not
/// corrupt, and is not healed.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn heal_on_receipt(
    session: &StorageSession,
    address: Address,
    err: &StorageError,
    heal_attempted: bool,
) -> impl Future<Output = bool> {
    async move {
        if err.is_not_supported() {
            return false;
        }
        if heal_attempted {
            lore_base::lore_error!(
                "Fragment {} still corrupt after heal: {}",
                address.hash,
                err
            );
            return false;
        }
        lore_base::lore_warn!("Fragment {}: {}. Attempting heal.", address.hash, err);
        let healed = session
            .verify(&address, true)
            .await
            .is_ok_and(|r| r.healed == lore_base::types::HealResult::Healed);
        if healed {
            lore_base::lore_debug!("Server healed fragment {}, retrying fetch", address.hash);
        } else {
            lore_base::lore_error!("Server did not heal fragment {}", address.hash);
        }
        healed
    }
}

/// Fetches `address` as [`fetch_payload`] does, with `options.priority`, and checks it on receipt
/// as `options` asks ([`check_receipt`]). A payload that fails the check is fetched again once
/// where the server heals it ([`heal_on_receipt`]). The content the check expands is handed back,
/// so no caller decodes or hashes a fetched payload again.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn fetch_remote(
    session: &StorageSession,
    address: Address,
    options: ReadOptions,
) -> impl Future<Output = Result<Loaded, StorageError>> {
    async move {
        lore_base::lore_trace!("Fetch immutable fragment {} from remote", address);
        let mut heal_attempted = false;
        loop {
            let raw = fetch_payload(session, address, options.priority).await?;
            match check_receipt(&raw, address, options) {
                Ok(content) => return Ok(Loaded { content, raw }),
                Err(err) => {
                    if !heal_on_receipt(session, address, &err, heal_attempted).await {
                        return Err(err);
                    }
                    heal_attempted = true;
                }
            }
        }
    }
}

/// Unified fragment load: local -> decompress -> optional remote fallback -> check -> heal ->
/// cache.
///
/// The local copy is trusted as stored, and only its sizes are checked. When `remote_session` is
/// `Some`, the session is used for remote fetch if the local load fails (miss, or a payload that
/// fails its size check or does not decompress, which the fetched payload overwrites); a payload
/// the store answers `NotSupported` for fails the load, and a store failing for any other reason is
/// a miss, `AddressNotFound` with no remote in reach ([`probe_local`]). The remote payload is
/// checked on receipt as `options` asks ([`check_receipt`]), and if it fails, heal is attempted
/// once via `session.verify()` before retrying.
///
/// For local-only loading, pass `None`.
///
/// Concurrent misses of the same content join a read single-flight
/// (`read_flight::join_read_flight`), keyed by the content hash, by the partition where the store
/// isolates partitions, and by whether the remote is trusted, never by the context: the first
/// caller becomes the leader and runs `fetch_and_store`, and later callers await its outcome rather
/// than duplicating the remote fetch. A leader that vanishes without completing (e.g. its task is
/// cancelled) leaves no result behind; the next follower observing that promotes itself to leader
/// and retries. A local-only load (no session, or `options.remote` off) answers its miss with
/// `AddressNotFound` and never joins: following would wait on a remote fetch it may not make. With
/// `options.never_wait` a miss answers `AddressNotFound` at once, waiting neither for a store in
/// flight nor a flight.
///
/// The flight is boxed: only a miss reaching the remote takes it, and inline it would make the
/// future of every load, and of every future awaiting one, as large as the fetch.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn load_fragment(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Fragment, Bytes), StorageError>> {
    async move {
        if address.hash.is_zero() {
            return Ok((Fragment::default(), Bytes::default()));
        }

        let landed = landed_before_probe(address, options);
        let mut probed = probe_local(store.clone(), partition, address, options).await;
        if matches!(probed, Err(LocalFailure::Corrupt | LocalFailure::Other))
            && stored_since(partition, address, landed).await
        {
            probed = probe_local(store.clone(), partition, address, options).await;
        }
        // The failure is consumed here rather than kept, so the future does not carry it across the
        // fetch. A corrupt or undecodable local entry has to be overwritten, not deduplicated
        // against.
        let remote = remote_session.filter(|_| options.remote && !options.never_wait);
        let (session, local_replace) = match (remote, probed) {
            (_, Ok((fragment, payload))) => return Ok((fragment, payload)),
            // The payload is held but cannot be decoded, which a miss would misreport.
            (_, Err(LocalFailure::Fatal(err)))
            | (None, Err(LocalFailure::DurableMigrated(err))) => {
                return Err(err);
            }
            (Some(session), Err(LocalFailure::Corrupt | LocalFailure::DurableMigrated(_))) => {
                (session, true)
            }
            (Some(session), Err(LocalFailure::Other)) => (session, false),
            (None, Err(LocalFailure::Corrupt | LocalFailure::Other)) => {
                return Err(StorageError::from(crate::errors::AddressNotFound::from(
                    address,
                )));
            }
        };

        Box::pin(fetch_single_flight(
            store.clone(),
            partition,
            address,
            options,
            || {
                fetch_and_store(
                    store.clone(),
                    partition,
                    address,
                    options,
                    session.clone(),
                    local_replace,
                )
            },
            || {},
        ))
        .await
        .and_then(|flown| flown.take(options.decompress))
    }
}

/// A payload as loaded, from a store ([`read_raw`]) or a remote ([`fetch_remote`]), with the
/// content it expands to where the load expanded it.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The content the check on receipt expanded, and the fragment describing it: always from an
    /// untrusted remote, verified, from a trusted one only where the fetch asked to decompress, and
    /// from a store where it checked the payload ([`StoreGetData::content`]).
    ///
    /// [`StoreGetData::content`]: crate::store_types::StoreGetData::content
    pub content: Option<(Fragment, Bytes)>,
    /// The payload in its stored form, and its fragment, marked durable where [`fetch_remote`]
    /// fetched it.
    pub raw: (Fragment, Bytes),
}

impl Loaded {
    /// The content when `decompress`, expanded here where the load did not, and the payload as
    /// stored otherwise.
    pub fn take(self, decompress: bool) -> Result<(Fragment, Bytes), StorageError> {
        match (decompress, self.content) {
            (false, _) => Ok(self.raw),
            (true, Some(content)) => Ok(content),
            (true, None) => expand_payload(self.raw.0, self.raw.1),
        }
    }
}

/// What a fetch in a read flight came to ([`fetch_single_flight`]).
#[derive(Debug)]
pub enum Flown {
    /// A fetch checked on receipt, the caller's own or the one it followed.
    Fetched(Loaded),
    /// The local store's copy, in the form `options` asks, found after the leader failed on another
    /// address.
    Local((Fragment, Bytes)),
}

impl Flown {
    /// The fetch taken as [`Loaded::take`] does, or the local copy as found.
    pub fn take(self, decompress: bool) -> Result<(Fragment, Bytes), StorageError> {
        match self {
            Self::Fetched(fetched) => fetched.take(decompress),
            Self::Local(found) => Ok(found),
        }
    }
}

/// Runs `fetch` for concurrent callers missing the content `address` names: the first becomes the
/// leader and fetches, and the rest follow its outcome, whatever their context, and whatever their
/// partition where the store does not isolate partitions, but only where they trust the remote as
/// the leader does ([`ReadOptions::trust_remote`]), so no caller takes a payload checked less than
/// it asks. A follower takes the leader's error only where it asked for the leader's own partition
/// and address; one whose leader failed on another address probes the local store, unless
/// `options.local` is off, and fetches itself when nothing is there.
///
/// Followers share the leader's fetch rather than copy it, and the leader hands it over only when
/// a follower is subscribed. `on_follow` runs when the caller first joins as a follower, before it
/// waits, so a caller holding what only its own fetch needs gives it back there.
pub async fn fetch_single_flight<F, Fut, G>(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
    fetch: F,
    on_follow: G,
) -> Result<Flown, StorageError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Loaded, StorageError>>,
    G: FnOnce(),
{
    let isolate_partitions = store.isolates_partitions();
    let mut on_follow = Some(on_follow);
    loop {
        match crate::read_flight::join_read_flight(
            partition,
            address,
            isolate_partitions,
            options.trust_remote,
        ) {
            crate::read_flight::ReadFlight::Leader(guard) => {
                let result = fetch().await;
                guard.complete(&result);
                return result.map(Flown::Fetched);
            }
            crate::read_flight::ReadFlight::Follower(follower) => {
                if let Some(on_follow) = on_follow.take() {
                    on_follow();
                }
                let own = follower.led_by(partition, address);
                match crate::read_flight::await_flight(follower).await {
                    Some(Ok(fetched)) => return Ok(Flown::Fetched(fetched)),
                    Some(Err(err)) if own => return Err(err),
                    Some(Err(_)) => {
                        if let Ok(found) =
                            probe_local(store.clone(), partition, address, options).await
                        {
                            return Ok(Flown::Local(found));
                        }
                        return fetch().await.map(Flown::Fetched);
                    }
                    None => {}
                }
            }
        }
    }
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

/// Refuses content larger than `max_content_size` allows, `fragment` describing it.
fn check_max_content_size(
    fragment: &Fragment,
    max_content_size: Option<u64>,
) -> Result<(), StorageError> {
    match max_content_size {
        Some(max) if fragment.size_content > max => {
            Err(StorageError::from(crate::errors::Oversized {
                context: format!(
                    "fragment size_content {} exceeds caller-supplied max {max}",
                    fragment.size_content
                ),
            }))
        }
        _ => Ok(()),
    }
}

/// The root fragment `address` names and its payload, loaded expanded ([`load_fragment`]), refusing
/// content larger than `options.max_content_size` allows.
///
/// Returns the load's future mapped through the check, without a future of its own.
fn load_root(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Fragment, Bytes), StorageError>> {
    let max_content_size = options.max_content_size;
    futures::FutureExt::map(
        load_fragment(
            store,
            partition,
            address,
            options.with_decompress(),
            remote_session,
        ),
        move |loaded| {
            loaded.and_then(|(fragment, buffer)| {
                check_max_content_size(&fragment, max_content_size).map(|()| (fragment, buffer))
            })
        },
    )
}

/// Read content (defragmenting if needed) into a `Bytes` buffer, returning the fragment
/// describing the whole content alongside the bytes the range asked for.
///
/// The fragment comes back because the bytes alone no longer say how much content there is:
/// with a range, `size_content` is what exists and the buffer length is what was asked for.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn read(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Fragment, Bytes), StorageError>> {
    async move {
        let (fragment, buffer) = load_root(
            store.clone(),
            partition,
            address,
            options,
            remote_session.clone(),
        )
        .await?;
        let content = read_root_content(
            store,
            partition,
            address,
            fragment,
            buffer,
            range,
            options.with_decompress(),
            remote_session,
        )
        .await?;
        Ok((fragment, content))
    }
}

/// Read the `range` of the content stored under `address` into `slice`, which the content the range
/// selects must fill exactly ([`fills_slice`]); a range selecting none leaves `slice` untouched.
/// A local root's fragment settles that before the root is decoded, and any root settles it before
/// the rest of the content is read.
///
/// Every write into `slice` happens within this future, so one dropped part-way leaves nothing
/// writing to it. A local compressed payload whose sizes check ([`check_payload_sizes`]) and whose
/// whole content the range covers is decompressed into `slice` ([`decompress_whole_into`]), unless
/// the store expanded it already; any other content is read as [`read`] reads it, into memory of
/// its own, and copied in once. A payload that expands is read from the local store once. The root
/// is loaded again ([`load_fragment`]) only for a miss or a payload that does not expand, and that
/// load probes the local store again for a held payload, or where a store of it may have landed
/// since ([`stored_since`]). A payload the store answers `NotSupported` for fails the read, as it
/// fails [`load_fragment`]. The zero hash names empty content and asks no store.
///
/// The contents of `slice` are unspecified when this fails.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn read_into(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    slice: &mut [u8],
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(), StorageError>> {
    async move {
        if address.hash.is_zero() {
            return Ok(());
        }
        let landed = landed_before_probe(address, options);
        let (held, expanded) = {
            let stored = if options.local {
                match read_raw(store.clone(), partition, address).await {
                    Err(err) if err.is_not_supported() => return Err(err),
                    loaded => loaded.ok(),
                }
            } else {
                None
            };
            if let Some(loaded) = &stored
                && check_payload_sizes(&loaded.raw.0, loaded.raw.1.len()).is_ok()
            {
                let (fragment, payload) = &loaded.raw;
                check_max_content_size(fragment, options.max_content_size)?;
                if !fills_slice(
                    slice,
                    resolve_content_range(range.clone(), fragment.size_content).len(),
                )? {
                    return Ok(());
                }
                if loaded.content.is_none()
                    && decompress_whole_into(*fragment, payload, range.clone(), slice)
                {
                    return Ok(());
                }
            }
            (
                stored.is_some(),
                stored.and_then(|loaded| loaded.take(true).ok()),
            )
        };
        let (fragment, buffer) = if let Some(content) = expanded {
            content
        } else {
            let local = held || stored_since(partition, address, landed).await;
            load_root(
                store.clone(),
                partition,
                address,
                if local { options } else { options.no_local() },
                remote_session.clone(),
            )
            .await?
        };
        if !fills_slice(
            slice,
            resolve_content_range(range.clone(), fragment.size_content).len(),
        )? {
            return Ok(());
        }
        let content = read_root_content(
            store,
            partition,
            address,
            fragment,
            buffer,
            range,
            options.with_decompress(),
            remote_session,
        )
        .await?;
        destination(slice, content.len())?.copy_from_slice(&content);
        Ok(())
    }
}

/// Decompresses a compressed payload other than a fragment list into `slice` where `range` covers
/// its whole content and `slice` holds exactly that, answering whether it did.
fn decompress_whole_into(
    fragment: Fragment,
    payload: &[u8],
    range: Option<Range<usize>>,
    slice: &mut [u8],
) -> bool {
    let size_content = fragment.size_content as usize;
    (fragment.flags & FragmentFlags::PayloadCompressed) != 0
        && (fragment.flags & FragmentFlags::PayloadFragmented) != FragmentFlags::PayloadFragmented
        && slice.len() == size_content
        && resolve_content_range(range, fragment.size_content).len() == size_content
        && compress::decompress_into_slice(fragment, payload, slice).is_ok()
}

/// Whether content of `len` bytes fills `slice` exactly. No content answers `false`, content longer
/// than `slice` is `Oversized`, as [`destination`] refuses it, and shorter content fails.
fn fills_slice(slice: &mut [u8], len: usize) -> Result<bool, StorageError> {
    let capacity = slice.len();
    match len {
        0 => Ok(false),
        len if len < capacity => Err(StorageError::internal(format!(
            "unexpected size: slice {capacity} vs content {len}"
        ))),
        len => destination(slice, len).map(|_| true),
    }
}

/// The first `len` bytes of `dst`, refusing content the destination has no room for rather than
/// truncating it ([`crate::validate_buffer_capacity`]).
fn destination(dst: &mut [u8], len: usize) -> Result<&mut [u8], StorageError> {
    crate::validate_buffer_capacity(len, dst.len())
        .forward::<StorageError>("writing content into the caller's memory")?;
    Ok(&mut dst[..len])
}

/// Copy `bytes` into the start of `dst`, as [`destination`] bounds it.
fn copy_into_buffer(bytes: &[u8], dst: &mut crate::CallerBuffer) -> Result<usize, StorageError> {
    destination(dst.as_mut_slice(), bytes.len())?.copy_from_slice(bytes);
    Ok(bytes.len())
}

/// Whether the remote may supply the root of a read into a caller-owned buffer.
///
/// An address the caller named is authoritative, so the remote may serve it. A hash a local mutable
/// mapping resolved to is not: the mapping is trusted as it stands and may name content the key has
/// since moved off, so reading that hash from the remote would answer with content the key no
/// longer names. [`resolve_root`] re-resolves against the remote instead, which costs the same
/// round trip and answers against the authoritative mapping.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RootSource {
    LocalOrRemote,
    LocalOnly,
}

/// Where [`content_payload_for_buffer`] left the payload stored under an address.
enum BufferPayload {
    /// `dst` holds the content.
    Placed,
    /// The bytes the store returned, as stored or as it expanded them on receipt, still to be
    /// turned into content.
    Stored(Bytes),
    /// Content [`load_fragment`] expanded: read locally again ([`content_payload_for_buffer`]), or
    /// fetched and checked on receipt.
    Fetched(Bytes),
}

/// The payload stored under `address`, put in `dst` when the local store could place it there, and
/// handed back otherwise: as the store returned it, or as content [`load_fragment`] expanded.
/// `None` for a root only the local store may supply ([`RootSource::LocalOnly`]) that it did not
/// place or return, with no wait and no second probe.
///
/// For any other root, a local miss waits for a store of the address in flight, as [`stored_since`]
/// describes, and probes the local store again through [`load_fragment`] only where one may have
/// landed, fetching from the remote otherwise where one is in reach. A placement the store answers
/// `SlowDown` is probed again through it as well, which retries; one it answers `Oversized` or
/// `NotSupported` fails the read, and one that fails for any other reason is a miss. That load is
/// boxed, as only a miss or a failed placement takes it. A miss with no remote in reach is
/// `AddressNotFound`, and a remote fetch that fails is an error, since it already checked and
/// healed.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn content_payload_for_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    root: RootSource,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<Option<(Fragment, BufferPayload)>, StorageError>> {
    async move {
        let landed = landed_before_probe(address, options);
        let mut reprobe = false;
        if options.local {
            match store.clone().get_into(partition, address, dst).await {
                Ok((fragment, PayloadRead::IntoBuffer)) => {
                    return Ok(Some((fragment, BufferPayload::Placed)));
                }
                Ok((fragment, PayloadRead::Returned(payload))) => {
                    return Ok(Some((fragment, BufferPayload::Stored(payload))));
                }
                // Forwarded, not rebuilt, so the error's text appears once rather than nested in a
                // context of its own.
                Err(err) if err.is_oversized() || err.is_not_supported() => {
                    return Err(err).forward("reading the content into the caller's buffer");
                }
                Err(err) => {
                    lore_base::lore_trace!(
                        "Fragment {} failed loading into caller buffer: {err:?}",
                        address.hash
                    );
                    reprobe = err.is_slow_down();
                }
            }
        }

        if root == RootSource::LocalOnly {
            return Ok(None);
        }

        let local = reprobe || stored_since(partition, address, landed).await;
        let (fragment, content) = Box::pin(load_fragment(
            store,
            partition,
            address,
            if local { options } else { options.no_local() },
            remote_session,
        ))
        .await?;
        Ok(Some((fragment, BufferPayload::Fetched(content))))
    }
}

/// Read the whole content stored under `address` into `dst`, reporting the fragment describing it
/// and the number of bytes written.
///
/// A local payload lands where it belongs: one that is the content is read into `dst`, a
/// compressed one expands into `dst`, and a fragment list is the root the walk starts from, its
/// leaves written into `dst` in place. Remote content arrives expanded and checked on receipt and
/// is copied into `dst` once. No payload placed or expanded here is read again.
///
/// `None` leaves the read to the assembling path, which loads the root itself and so refetches and
/// heals: the local store did not place or return a root only it may supply, or what it held fails
/// its size check or did not survive expansion. A miss with no remote in reach, a remote fetch that
/// fails, a payload the store answers `NotSupported` for, content larger than
/// `options.max_content_size` allows, and a destination too small for the content, fail the read.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub(crate) fn read_content_into_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    root: RootSource,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<Option<(Fragment, usize)>, StorageError>> {
    async move {
        let Some((fragment, payload)) = content_payload_for_buffer(
            store.clone(),
            partition,
            address,
            dst,
            options.with_decompress(),
            root,
            remote_session.clone(),
        )
        .await?
        else {
            return Ok(None);
        };
        if let BufferPayload::Stored(stored) = &payload
            && check_payload_sizes(&fragment, stored.len()).is_err()
        {
            return Ok(None);
        }

        check_max_content_size(&fragment, options.max_content_size)?;

        let size_content = fragment.size_content as usize;
        let payload = match payload {
            BufferPayload::Placed => return Ok(Some((fragment, size_content))),
            BufferPayload::Fetched(content) => {
                let written = deliver_root_into_buffer(
                    store,
                    partition,
                    address,
                    None,
                    fragment,
                    content,
                    dst,
                    options.with_decompress(),
                    depth,
                    remote_session,
                )
                .await?;
                return Ok(Some((fragment, written)));
            }
            BufferPayload::Stored(payload) => payload,
        };

        if (fragment.flags & FragmentFlags::PayloadCompressed) != 0 {
            // Sized to the content rather than to the whole destination: the expansion is bounded
            // by the slice it is given, and that bound reaches the decompressor as a `c_int`.
            let target = destination(dst.as_mut_slice(), size_content)?;
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
            return Ok(Some((expanded, expanded.size_content as usize)));
        }

        if (fragment.flags & FragmentFlags::PayloadFragmented) != FragmentFlags::PayloadFragmented {
            let written = copy_into_buffer(&payload, dst)?;
            return Ok(Some((fragment, written)));
        }

        let (root, list) = match expand_payload(fragment, payload) {
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
            options.with_decompress(),
            depth,
            remote_session,
        )
        .await?;
        Ok(Some((root, written)))
    }
}

/// Deliver `range` of the content rooted at `fragment` and `buffer` into `dst`, reporting the bytes
/// written.
///
/// Content spread across fragments is walked into `dst` in place, each leaf writing where it
/// belongs, so it is assembled nowhere else first. Content one fragment holds is already in
/// `buffer`, so the range is cut from it and copied in once.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn deliver_root_into_buffer(
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
) -> impl Future<Output = Result<usize, StorageError>> {
    async move {
        let content = resolve_content_range(range, fragment.size_content);
        if content.is_empty() {
            return Ok(0);
        }

        if (fragment.flags & FragmentFlags::PayloadFragmented) != FragmentFlags::PayloadFragmented {
            return copy_into_buffer(&cut_content(&buffer, content)?, dst);
        }

        crate::validate_buffer_capacity(content.len(), dst.len())
            .forward::<StorageError>("writing content into the caller's memory")?;
        read_defragment(
            store,
            partition,
            address,
            content.clone(),
            fragment,
            buffer,
            // SAFETY: `content.len()` is within `dst`, as checked above, and the holder of `dst`
            // writes nothing through it while the prefix lives, as `CallerBuffer` requires.
            unsafe { dst.prefix(content.len()) },
            options,
            depth,
            remote_session,
        )
        .await?;

        Ok(content.len())
    }
}

/// [`read`] delivering into a caller-owned buffer: the root is loaded once and the content taken
/// from it, so content spanning fragments is assembled nowhere but in `dst`.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn read_range_into_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Fragment, usize), StorageError>> {
    async move {
        let (fragment, buffer) = load_root(
            store.clone(),
            partition,
            address,
            options,
            remote_session.clone(),
        )
        .await?;

        let written = deliver_root_into_buffer(
            store,
            partition,
            address,
            range,
            fragment,
            buffer,
            dst,
            options.with_decompress(),
            depth,
            remote_session,
        )
        .await?;
        Ok((fragment, written))
    }
}

/// [`read`] delivering the content into a caller-owned buffer.
///
/// `dst.len()` states the capacity and bounds the read: content exceeding it fails with `Oversized`
/// rather than truncating. Returns the fragment describing the whole content and the number of
/// bytes written.
///
/// A whole read of content one local fragment holds goes straight into `dst`, allocating nothing
/// beyond the compressed payload where there is one; remote content arrives expanded on receipt
/// and is copied in once. Content spanning fragments is walked into `dst` leaf by leaf. A range of
/// content one fragment holds is cut from that fragment and copied in once. The zero hash names
/// empty content, which writes nothing and asks no store.
///
/// The contents of `dst` are unspecified when this fails. A read dropped part-way may still write
/// `dst` until the loan on it ([`CallerBuffer::loan`](crate::CallerBuffer::loan)) is returned.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn read_into_buffer(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Fragment, usize), StorageError>> {
    async move {
        if address.hash.is_zero() {
            return Ok((Fragment::default(), 0));
        }
        // A range is cut from assembled content, so only a whole read is served by the payload as
        // the store holds it.
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
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn read_stream(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
    sender: tokio::sync::mpsc::Sender<Result<Bytes, StorageError>>,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Fragment, Range<u64>), StorageError>> {
    async move {
        let (fragment, buffer) = load_root(
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
            let joined = store.try_join_claim();
            lore_base::lore_spawn!(async move {
                let claim = claim_for_task(&store, joined).await.map(Arc::new);
                let result = defragment_pipeline(
                    store,
                    partition,
                    address,
                    fragment,
                    buffer,
                    pipeline_range,
                    DefragmentSink::Stream { sender },
                    options.with_decompress(),
                    remote_session,
                    claim,
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
                .send(Ok(cut_content(&buffer, range)?))
                .await
                .map_err(|_err| StorageError::internal("read stream closed"))?;
            Ok((fragment, streamed))
        }
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
            && !matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            )
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
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn read_into_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    path: &Path,
    temp_file_extension: &str,
    range: Option<Range<usize>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Fragment, Option<std::fs::Metadata>), StorageError>> {
    async move {
        let _count_permit = file_count_limit_acquire()
            .await
            .forward::<StorageError>("permit failed")?;

        let (fragment, buffer) = load_root(
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
            options.with_decompress(),
            remote_session,
        )
        .await?;

        Ok((fragment, metadata))
    }
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
/// Where `options.create_parent` asks for it, a write that finds the directory `path` goes in
/// missing creates that directory, with its missing ancestors, and opens again at once. A
/// directory that cannot be created fails the write. A directory that is there costs nothing.
///
/// A range starting past the end of the content selects nothing that exists, and `path` is left
/// alone: the file is not opened, so a destination that was already there survives a request the
/// caller got wrong. Nothing is written and no metadata comes back, and the caller decides what an
/// empty selection means from the fragment it holds. A start exactly at the end is a legitimate
/// empty read and does produce an empty file.
///
/// `options` must already carry `with_decompress`; both entry points load their root with it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn write_root_to_file(
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
) -> impl Future<Output = Result<Option<std::fs::Metadata>, StorageError>> {
    async move {
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

            let mut create_parent = options.create_parent;
            let file = loop {
                let directory_missing = {
                    let err =
                        match crate::defragment::open_file_write(file_path.as_path(), range.len())
                            .await
                        {
                            Ok(file) => break file,
                            Err(err) => err,
                        };
                    if create_parent && names_a_missing_directory(&err) {
                        true
                    } else if retry.wait().await {
                        false
                    } else {
                        return Err(StorageError::internal_with_context(
                            err,
                            &format!("failed to open file: {}", path.display()),
                        ));
                    }
                };
                if directory_missing {
                    create_parent = false;
                    create_parent_directory(path).await?;
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

            let claim = claim_for_task(&store, store.try_join_claim())
                .await
                .map(Arc::new);
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
                claim,
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

            Ok(None)
        } else {
            let mut retry = crate::retry(10, 10_000, 10);
            let buffer = cut_content(&buffer, range)?;
            let mut create_parent = options.create_parent;
            let metadata = loop {
                let directory_missing = {
                    let err = match write_all_to_file(path, buffer.clone(), options.sync_data).await
                    {
                        Ok(meta) => break meta,
                        Err(err) => err,
                    };
                    if create_parent && names_a_missing_directory(&err) {
                        true
                    } else if retry.wait().await {
                        false
                    } else {
                        return Err(StorageError::internal_with_context(
                            err,
                            &format!("write to file: {}", path.display()),
                        ));
                    }
                };
                if directory_missing {
                    create_parent = false;
                    create_parent_directory(path).await?;
                }
            };
            Ok(Some(metadata))
        }
    }
}

/// Whether a failed open of a file names a directory on its path that is missing, or that is
/// not a directory. Creating the directory the file goes in mends the first and reports the
/// second.
fn names_a_missing_directory(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Creates the directory `path` goes in, with its missing ancestors.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn create_parent_directory(path: &Path) -> impl Future<Output = Result<(), StorageError>> + '_ {
    async move {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        lore_io::IoDriver::global()
            .create_dir_all(parent)
            .await
            .map_err(|err| {
                StorageError::internal_with_context(
                    err,
                    &format!("creating the directory of {}", path.display()),
                )
            })
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
#[lore_macro::test_pub]
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
/// A failure means the caller should ask the remote instead: the mapping is absent, it is a
/// tombstone, or its root is not cached locally or does not load. The root is loaded as
/// [`load_fragment`] loads it locally, so the failure tells a root held broken, failing its size
/// check or not decompressing, from one the store lacks.
///
/// A mapping that *is* present is trusted as-is and not revalidated. Because the key is mutable,
/// that is a weaker guarantee than the immutable [`load_fragment`] path gives: a cached mapping
/// can name a hash the key has since moved off. Freshness is the caller's choice through the same
/// flags a `get` uses — `remote` resolves authoritatively, the default prefers whatever is local.
///
/// On the fall-through it deliberately does not remote-read the locally cached hash. A remote
/// `get_resolved` answers the mapping and the root in one round trip, so re-resolving costs
/// nothing extra and answers against the authoritative mapping.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn load_resolved_local(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    options: ReadOptions,
) -> impl Future<Output = Result<(Hash, Fragment, Bytes), LocalFailure>> {
    async move {
        let resolved = match mutable.load(partition, key, KeyType::Resolve).await {
            Ok(resolved) if !resolved.is_zero() => resolved,
            Ok(_) => return Err(LocalFailure::Other),
            Err(err) => {
                lore_base::lore_trace!(
                    "Key {key} failed to resolve from local mutable store: {err:?}"
                );
                return Err(LocalFailure::Other);
            }
        };

        let address = Address {
            hash: resolved,
            context,
        };
        let landed = landed_before_probe(address, options);
        let mut loaded = probe_local(store.clone(), partition, address, options).await;
        if matches!(loaded, Err(LocalFailure::Corrupt | LocalFailure::Other))
            && stored_since(partition, address, landed).await
        {
            loaded = probe_local(store, partition, address, options).await;
        }
        if loaded.is_err() {
            lore_base::lore_trace!(
                "Key {key} resolved locally to {resolved}, whose root does not load"
            );
        }
        loaded.map(|(fragment, buffer)| (resolved, fragment, buffer))
    }
}

/// Resolve `key` to the root fragment it names, sharing one round trip with the read of that root
/// whenever the answer is not already local.
///
/// The key is always read as [`KeyType::Resolve`], locally and remotely alike.
///
/// Local-first like [`read`]: [`load_resolved_local`] tries the local mutable store and the local
/// copy of the root it names, and only a miss there reaches the remote. A local root this build
/// cannot decode fails the resolve as it fails [`load_fragment`]: at once, unless it is durably
/// stored and a remote can supply it. A fragment list's leaves
/// go through [`load_fragment`] either way, so they keep their own local-then-remote fallback and
/// local caching.
///
/// On a remote resolve the key->hash mapping is written back to the local mutable store once the
/// payload write-back succeeds, under the same gate — so a later call can be served entirely
/// locally, and the mapping is never left pointing at a root this store does not hold. Where the
/// local root is held broken, the root the remote resolves to is stored replacing any copy, whether
/// or not the read caches, as [`load_fragment`] stores a refetched payload: over the broken one
/// where the key still names it.
///
/// A local root travels with the caller's session anyway: a fragment list's leaves may still
/// exist only remotely.
///
/// Checks every remote root on receipt as `options` asks ([`check_receipt`]). A failed check gets
/// one heal attempt ([`heal_on_receipt`]) then a re-resolve, as [`load_fragment`] does. The retry
/// re-resolves rather than re-reads, since the heal targets the resolved address and a fresh
/// resolve costs the same single round trip.
///
/// The root is expanded when `options.decompress` is set, and left as stored otherwise. Content
/// larger than `options.max_content_size` allows fails.
///
/// `flags` is a reserved bitmask forwarded to the server; 0 for default behaviour.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn resolve_root(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<ResolvedRoot, StorageError>> {
    async move {
        let key_address = Address { hash: key, context };

        let replace = if options.local {
            match load_resolved_local(
                store.clone(),
                mutable.clone(),
                partition,
                key,
                context,
                options,
            )
            .await
            {
                Ok((resolved, fragment, buffer)) => {
                    check_max_content_size(&fragment, options.max_content_size)?;
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
                Err(LocalFailure::Fatal(err)) => return Err(err),
                Err(LocalFailure::DurableMigrated(err)) if !options.remote || session.is_none() => {
                    return Err(err);
                }
                Err(failure) => matches!(
                    failure,
                    LocalFailure::Corrupt | LocalFailure::DurableMigrated(_)
                ),
            }
        } else {
            false
        };

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
            REMOTE_FETCH_BYTES.fetch_add(buffer.len() as u64, Ordering::Relaxed);

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

            check_max_content_size(&fragment, options.max_content_size)?;
            let raw = (fragment, buffer);
            match check_receipt(&raw, address, options) {
                Ok(content) => {
                    let should_store = options.cache
                        || replace
                        || (fragment.flags & FragmentFlags::PayloadLocalCachePriority) != 0;
                    if should_store
                        && store
                            .clone()
                            .put(partition, address, fragment, Some(raw.1.clone()), replace)
                            .await
                            .is_ok()
                    {
                        let _ = mutable
                            .store(partition, key, resolved, KeyType::Resolve)
                            .await;
                    }
                    let (fragment, buffer) = Loaded { content, raw }.take(options.decompress)?;
                    break (resolved, address, fragment, buffer);
                }
                Err(err) => {
                    if !heal_on_receipt(session.as_ref(), address, &err, heal_attempted).await {
                        return Err(err);
                    }
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
}

/// `mutable_load(key)` + [`read`] of the resulting address, resolved in one round trip when the
/// remote answers. Returns the resolved hash alongside the content.
///
/// See [`resolve_root`] for how the key is resolved; this reassembles the whole content into one
/// buffer. [`read_resolved_stream`] delivers it fragment by fragment instead.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn read_resolved(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    range: Option<Range<usize>>,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Hash, Bytes), StorageError>> {
    async move {
        let root = resolve_root(
            store.clone(),
            mutable,
            partition,
            key,
            context,
            flags,
            options.with_decompress(),
            session,
        )
        .await?;

        let bytes = read_root_content(
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
}

/// [`read_resolved`] delivering the content through `sender` one leaf at a time instead of
/// reassembling it, mirroring what [`read_stream`] does for an address. Each leaf arrives with its
/// fragment, expanded when `options.decompress` is set and as stored otherwise.
///
/// Returns the resolved hash and the content's total size; the leaves follow on the channel. Peak
/// memory is bounded by the channel depth rather than by the content, which is what makes this
/// usable for a key naming something large.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn read_resolved_stream(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    options: ReadOptions,
    sender: tokio::sync::mpsc::Sender<Result<(Fragment, Bytes), StorageError>>,
    session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Hash, u64), StorageError>> {
    async move {
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

        if (root.fragment.flags & FragmentFlags::PayloadFragmented)
            == FragmentFlags::PayloadFragmented
        {
            let address = root.address;
            let fragment = root.fragment;
            let buffer = root.buffer;
            let remote_session = root.session;
            let report = sender.clone();
            let joined = store.try_join_claim();
            lore_base::lore_spawn!(async move {
                let claim = claim_for_task(&store, joined).await.map(Arc::new);
                let result = defragment_pipeline_leaves(
                    store,
                    partition,
                    address,
                    fragment,
                    buffer,
                    sender,
                    options,
                    remote_session,
                    claim,
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
                .send(Ok((root.fragment, root.buffer)))
                .await
                .map_err(|_err| StorageError::internal("read stream closed"))?;
        }

        Ok((root.resolved, root.fragment.size_content))
    }
}

/// [`read_into_buffer`] reached by key rather than by address.
///
/// `dst.len()` states the capacity and bounds the read: content exceeding it fails with `Oversized`
/// rather than truncating. Returns the resolved hash and the number of bytes written.
///
/// A key resolving locally to a root the local store holds reads into `dst` as [`read_into_buffer`]
/// does. Anything else is resolved through [`resolve_root`] and delivered into `dst` by
/// [`deliver_root_into_buffer`], assembled nowhere else.
///
/// The contents of `dst` are unspecified when this fails. A read dropped part-way may still write
/// `dst` until the loan on it ([`CallerBuffer::loan`](crate::CallerBuffer::loan)) is returned.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn read_resolved_into_buffer(
    store: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    context: Context,
    flags: u32,
    dst: &mut crate::CallerBuffer,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(Hash, usize), StorageError>> {
    async move {
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

        let root = resolve_root(
            store.clone(),
            mutable,
            partition,
            key,
            context,
            flags,
            options.with_decompress(),
            session,
        )
        .await?;

        let written = deliver_root_into_buffer(
            store,
            partition,
            root.address,
            None,
            root.fragment,
            root.buffer,
            dst,
            options.with_decompress(),
            0,
            root.session,
        )
        .await?;
        Ok((root.resolved, written))
    }
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
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
pub fn read_resolved_into_file(
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
) -> impl Future<Output = Result<(Hash, Fragment), StorageError>> {
    async move {
        let _count_permit = file_count_limit_acquire()
            .await
            .forward::<StorageError>("permit failed")?;

        let root = resolve_root(
            store.clone(),
            mutable,
            partition,
            key,
            context,
            flags,
            options.with_decompress(),
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
            options.with_decompress(),
            root.session,
        )
        .await?;

        Ok((root.resolved, root.fragment))
    }
}

/// [`remote_get_retry`] for `get_resolved`: back off on `SlowDown`, and retry a stale session id
/// or a refusal on a fresh session as [`SessionRetries::retry`] bounds and paces them. `key`
/// supplies error context only.
async fn remote_get_resolved_retry(
    session: &StorageSession,
    key: Hash,
    context: Context,
    flags: u32,
) -> Result<(Hash, Fragment, Bytes), StorageError> {
    let _guard = RemoteFetchGuard::new();
    #[cfg(feature = "test-util")]
    if let Some(served) = crate::test_util::remote_resolved(key) {
        return Ok(served);
    }
    let mut retry = crate::store_retry();
    let mut session_retries = SessionRetries::default();
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
                if !session_retries
                    .retry(&storage_err, session, &mut retry)
                    .await
                {
                    return Err(storage_err);
                }
            }
        }
    }
}

/// The `range` of the content rooted at `fragment` and `buffer`, a root loaded expanded and within
/// `options.max_content_size`.
///
/// The range is cut from `buffer` where one fragment holds the content ([`cut_content`]), and
/// otherwise reassembled from the list's leaves through [`load_fragment`], which may fetch them
/// remotely.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn read_root_content(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    range: Option<Range<usize>>,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<Bytes, StorageError>> {
    async move {
        let range = resolve_content_range(range, fragment.size_content);
        if range.is_empty() {
            return Ok(Bytes::default());
        }

        if (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented {
            let size = range.len();
            let mut content = crate::CallerBuffer::allocate(size);
            read_defragment(
                store,
                partition,
                address,
                range,
                fragment,
                buffer,
                // SAFETY: `size` is the length of `content`, which nothing writes through: the
                // walk returns once every handle it gave out is gone, and `content` drops with
                // this future.
                unsafe { content.prefix(size) },
                options,
                0,
                session,
            )
            .await?;
            assembled(content)
        } else {
            cut_content(&buffer, range)
        }
    }
}

/// The content a walk assembled into `content`, which a walk that succeeds writes whole, handed
/// back without a copy.
fn assembled(content: crate::CallerBuffer) -> Result<Bytes, StorageError> {
    // SAFETY: the walk succeeded, so it wrote every byte of its range.
    unsafe { content.into_bytes() }
        .ok_or_else(|| StorageError::internal("failed to reclaim buffer after defragmenting"))
}

/// The `range` of `buffer`, content one fragment holds, failing rather than panicking where the
/// range reaches past it. The size checks and the decoder give a root exactly `size_content` bytes,
/// so every range [`resolve_content_range`] answers for it lies within.
fn cut_content(buffer: &Bytes, range: Range<usize>) -> Result<Bytes, StorageError> {
    if range.end > buffer.len() {
        return Err(StorageError::internal(format!(
            "content range {range:?} reaches past the {} byte root",
            buffer.len()
        )));
    }
    Ok(buffer.slice(range))
}
