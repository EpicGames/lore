// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures::FutureExt;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;
use serde::Serialize;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinError;
use tokio::task::JoinSet;
use zerocopy::FromZeros;

use crate::errors::*;
use crate::fragment::FragmentFlags;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Fragment;
use crate::lore::FragmentReference;
use crate::lore::Hash;
use crate::lore::Partition;
use crate::lore::TypedBytes;
use crate::lore::VecBytes;
use crate::lore_debug;
use crate::lore_trace;
use crate::lore_warn;
use crate::repository::RepositoryContext;
use crate::store::ImmutableStore;
use crate::store::StoreError;
use crate::store::StoreMatch;
use crate::store::StoreMatchResult;
use crate::store::query_one;

#[error_set]
pub enum ImmutableError {
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    InvalidArguments,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    SlowDown,
}

use lore_storage::options::ReadOptions;
use lore_storage::options::WriteOptions;
use lore_storage::read::Flown;

/// Event data reporting a single fragment written or deduplicated.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFragmentWriteEventData {
    /// The fragment that was written
    pub fragment: Fragment,
    /// Non-zero if the fragment already existed and was deduplicated
    pub deduplicated: u8,
}

/// Build a `WriteTracker`. When `per_fragment` is set the tracker emits a
/// `FragmentWrite` event per fragment; otherwise it carries no observer.
///
/// Aggregate counters live on the execution context rather than the tracker, so
/// this flag governs only the per-fragment events.
pub fn commit_write_tracker(per_fragment: bool) -> Arc<lore_storage::write_tracker::WriteTracker> {
    if !per_fragment {
        return Arc::new(lore_storage::write_tracker::WriteTracker::new());
    }
    Arc::new(lore_storage::write_tracker::WriteTracker::with_observer(
        Arc::new(|fragment: &Fragment, deduplicated: bool| {
            crate::event::LoreEvent::FragmentWrite(LoreFragmentWriteEventData {
                fragment: *fragment,
                deduplicated: deduplicated as u8,
            })
            .send();
        }),
    ))
}

/// The counters this call's writes report into, or `None` where nothing counts
/// them: statistics level zero, or outside a call.
///
/// Read at each write rather than threaded through the callers, so that a write
/// carrying no tracker still lands in the operation's totals. At level zero the
/// whole `lore-storage` write pipeline holds no counters and takes no atomic add
/// per fragment.
fn ambient_fragment_stats() -> Option<Arc<lore_storage::FragmentWriteStats>> {
    let context = crate::runtime::try_execution_context()?;
    if !context.globals().stats() {
        return None;
    }
    Some(context.fragment_stats().clone())
}

/// The write context for a call that dispatches background writes into `tracker`.
pub fn write_context(
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> lore_storage::WriteContext {
    lore_storage::WriteContext::tracked(tracker, ambient_fragment_stats())
}

/// The write context for a call that has no tracker, so its writes run inline
/// but still land in this call's totals.
pub fn counted_write_context() -> lore_storage::WriteContext {
    lore_storage::WriteContext::counted(ambient_fragment_stats())
}

/// Construct [`WriteOptions`] from a repository context. Writes reach the remote only where the
/// context uploads and the call is not a dry run.
pub fn write_options_from_repository(repository: Arc<RepositoryContext>) -> WriteOptions {
    let flags = WriteOptions::default();
    let dry_run =
        crate::runtime::try_execution_context().is_some_and(|context| context.globals().dry_run());
    if !repository.disable_upload() && !dry_run {
        flags.with_remote_write()
    } else {
        flags
    }
}

/// Construct [`ReadOptions`] from a repository context and the command's globals.
pub fn read_options_from_repository(repository: &RepositoryContext) -> ReadOptions {
    let (sync_data, trust_remote) =
        crate::runtime::try_execution_context().map_or((false, false), |context| {
            let globals = context.globals();
            (globals.sync_data(), globals.trust_remote())
        });
    ReadOptions {
        cache: !repository.disable_cache(),
        direct_write: repository.direct_file_write(),
        sync_data,
        trust_remote,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Session resolution helper
// ---------------------------------------------------------------------------

/// A storage session for a repository context, or `None` where the context holds
/// no remote and nothing can come of one.
///
/// The session is lazy: the server-side session is established only when it is
/// actually used, so a command that reads and writes nothing remotely never drives
/// the connect. It stays lazy rather than resolving here because the read path
/// recovers a rotated server session map by invalidating the session and retrying
/// that same one, and only a lazy session resolves again — see
/// [`StorageSession::is_lazy`].
///
/// The correlation id is read here rather than inside the resolver because the
/// resolver runs wherever the read or write ends up, which is not necessarily
/// under this command's execution context. The context holds one session per
/// correlation id at a time, so a call never takes a session another call's id
/// started.
///
/// The context holds the session it is handed, so the resolver holds the context
/// weakly: a strong reference there is a cycle neither end can free, and a context
/// gone by the time a session resolves has no pool left to take sessions from. A call that
/// takes the session holds the context until it completes.
#[lore_macro::test_pub]
pub(crate) fn resolve_session(repository: &Arc<RepositoryContext>) -> Option<Arc<StorageSession>> {
    if repository.is_offline() {
        return None;
    }
    let execution = crate::lore::execution_context();
    let correlation_id = execution.globals().correlation_id.as_str();
    Some(repository.lazy_session(correlation_id, |correlation_id| {
        let repository = Arc::downgrade(repository);
        Arc::new(StorageSession::pending(move || {
            let repository = repository.clone();
            let correlation_id = correlation_id.clone();
            async move {
                let repository = repository
                    .upgrade()
                    .ok_or_else(|| ProtocolError::from(lore_base::error::NoRemote))?;
                pooled_session(&repository, &correlation_id).await
            }
        }))
    }))
}

/// A session over the pool the repository context holds, resolving the pool on first use.
///
/// Each operation takes the next session of the pool, so the reads of a command spread over
/// every storage connection.
///
/// Going through [`RepositoryContext::session_pool`] rather than the connection is
/// the point: the context holds the pool it resolved, so a command's operations take it
/// without a lookup on the connection, and the context decides whether the connection pins
/// it as well.
#[lore_macro::test_pub]
async fn pooled_session(
    repository: &Arc<RepositoryContext>,
    correlation_id: &str,
) -> Result<Arc<StorageSession>, ProtocolError> {
    Ok(Arc::new(StorageSession::pooled(
        repository.session_pool(correlation_id).await?,
    )))
}

// ---------------------------------------------------------------------------
// Local store helpers
// ---------------------------------------------------------------------------

/// Load a single raw fragment from local store with retry backoff.
///
/// Returns [`lore_storage::read::read_raw`]'s future with its error forwarded, without a future of
/// its own.
pub fn load_raw_store_retry(
    store: Arc<dyn ImmutableStore>,
    repository: Partition,
    address: Address,
) -> impl Future<Output = Result<(Fragment, Bytes), ImmutableError>> {
    lore_storage::read::read_raw(store, repository, address).map(|loaded| {
        loaded
            .map(|loaded| loaded.raw)
            .forward::<ImmutableError>("loading raw fragment from store")
    })
}

/// Write a single raw fragment to local store with retry backoff.
///
/// Returns [`lore_storage::write_raw`]'s future with its error forwarded, without a future of its
/// own.
pub fn store_raw_store_retry(
    store: Arc<dyn ImmutableStore>,
    repository: Partition,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> impl Future<Output = Result<(), ImmutableError>> {
    lore_storage::write_raw(store, repository, address, fragment, payload)
        .map(|stored| stored.forward::<ImmutableError>("storing raw fragment to store"))
}

/// Store a raw fragment to a remote session with retry on `SlowDown`.
pub async fn store_raw_remote_retry(
    remote_storage: Arc<StorageSession>,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> Result<(), ImmutableError> {
    let mut retry = lore_storage::store_retry();
    loop {
        match remote_storage.put(address, fragment, payload.clone()).await {
            Ok(_) => return Ok(()),
            Err(ProtocolError::SlowDown(_)) => {
                if !retry.wait().await {
                    return Err(ImmutableError::internal(
                        "Failed to store fragments, remote error",
                    ));
                }
            }
            Err(ProtocolError::Disconnected(_)) => {
                return Err(Disconnected.into());
            }
            Err(err) => {
                return Err(ImmutableError::internal_with_context(
                    err,
                    "Failed to store fragments, remote error",
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// load_raw -- thin wrapper that resolves session and delegates to lore-storage
// ---------------------------------------------------------------------------

/// Load a single raw fragment, optionally decompressing the data.
/// Resolves the remote session from the repository and delegates to
/// [`lore_storage::load_fragment`] which handles remote fetch, check and heal.
///
/// Returns that future with its error forwarded, holding `repository` until it completes, without
/// a future of its own.
#[inline]
pub fn load_raw(
    repository: Arc<RepositoryContext>,
    address: Address,
    options: ReadOptions,
) -> impl Future<Output = Result<(Fragment, Bytes), ImmutableError>> {
    let session = resolve_session(&repository);
    lore_storage::load_fragment(
        repository.immutable_store(),
        repository.id,
        address,
        options,
        session,
    )
    .map(move |loaded| {
        drop(repository);
        loaded.forward::<ImmutableError>("loading fragment")
    })
}

// ---------------------------------------------------------------------------
// Read path -- delegates to lore-storage with session
// ---------------------------------------------------------------------------

/// Stream the given data range to `sender`, returning the number of bytes that will arrive.
///
/// `None` streams the whole content, so the count is `size_content` as it always was. A range
/// is clamped to the content and only the fragments it touches are fetched.
///
/// Returns [`lore_storage::read_stream`]'s future mapped to the count, holding `repository` until it
/// completes, without a future of its own.
#[inline]
pub fn read_stream(
    repository: Arc<RepositoryContext>,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
    sender: Sender<Result<Bytes, lore_storage::StorageError>>,
) -> impl Future<Output = Result<u64, ImmutableError>> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let session = resolve_session(&repository);
    lore_storage::read_stream(store, partition, address, range, options, sender, session).map(
        move |streamed| {
            drop(repository);
            streamed
                .map(|(_fragment, streamed)| streamed.end - streamed.start)
                .forward::<ImmutableError>("reading immutable data")
        },
    )
}

/// Read the given data range from a fragment which can be a large data set
/// stored as a fragment list. The function will reassemble and decompress
/// any data from the fragments holding the data range requested.
///
/// Returns [`lore_storage::read`]'s future mapped to the data, holding `repository` until it
/// completes, without a future of its own.
#[inline]
pub fn read(
    repository: Arc<RepositoryContext>,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
) -> impl Future<Output = Result<Bytes, ImmutableError>> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let session = resolve_session(&repository);
    lore_storage::read(store, partition, address, range, options, session).map(move |read| {
        drop(repository);
        read.map(|(_fragment, bytes)| bytes)
            .forward::<ImmutableError>("reading immutable data")
    })
}

/// Read the given data range into `slice`.
///
/// Returns [`lore_storage::read_into`]'s future with its error forwarded, holding `repository` until
/// it completes, without a future of its own.
#[inline]
pub fn read_into(
    repository: Arc<RepositoryContext>,
    address: Address,
    range: Option<Range<usize>>,
    slice: &mut [u8],
    options: ReadOptions,
) -> impl Future<Output = Result<(), ImmutableError>> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let session = resolve_session(&repository);
    lore_storage::read_into(store, partition, address, range, slice, options, session).map(
        move |read| {
            drop(repository);
            read.forward::<ImmutableError>("reading immutable data")
        },
    )
}

/// Write the given data range to `path`. The file holds exactly that range starting at its
/// first byte; `None` writes the whole content.
///
/// Returns [`lore_storage::read_into_file`]'s future with its error forwarded, holding `repository`
/// until it completes, without a future of its own.
#[inline]
pub fn read_into_file(
    repository: Arc<RepositoryContext>,
    address: Address,
    path: &Path,
    range: Option<Range<usize>>,
    options: ReadOptions,
) -> impl Future<Output = Result<(Fragment, Option<std::fs::Metadata>), ImmutableError>> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let temp_ext = crate::repository::TEMP_FILE_EXTENSION;
    let session = resolve_session(&repository);
    lore_storage::read_into_file(
        store, partition, address, path, temp_ext, range, options, session,
    )
    .map(move |read| {
        drop(repository);
        read.forward::<ImmutableError>("reading immutable data")
    })
}

// ---------------------------------------------------------------------------
// store_raw -- thin wrapper: store_fragment + session + event
// ---------------------------------------------------------------------------

/// Store a raw fragment: delegates to [`lore_storage::store_fragment`] with
/// an optional remote session for durable upload.
///
/// Returns [`store_raw_with_tracker`]'s future itself: a future of its own would hold the arguments
/// again beside it.
pub fn store_raw(
    repository: Arc<RepositoryContext>,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_write: bool,
) -> impl Future<Output = Result<Address, ImmutableError>> {
    store_raw_with_tracker(
        repository,
        address,
        fragment,
        buffer,
        cache_local,
        remote_write,
        None,
    )
}

/// Tracker-aware variant of [`store_raw`]. When `tracker` is `Some`, the
/// background leader task is dispatched into the tracker and the call returns
/// as soon as the address is known. Commit-level callers build the tracker
/// once per operation and pass it through.
///
/// Returns [`lore_storage::store_fragment`]'s future mapped to the address, holding `repository`
/// until it completes, without a future of its own.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn store_raw_with_tracker(
    repository: Arc<RepositoryContext>,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_write: bool,
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> impl Future<Output = Result<Address, ImmutableError>> {
    let session = if remote_write {
        resolve_session(&repository)
    } else {
        None
    };
    lore_storage::store_fragment(
        repository.immutable_store(),
        repository.id,
        address,
        fragment,
        buffer,
        cache_local,
        session,
        write_context(tracker),
        None,
    )
    .map(move |stored| {
        drop(repository);
        stored
            .map(|stored| stored.address)
            .forward::<ImmutableError>("storing fragment")
    })
}

// ---------------------------------------------------------------------------
// Write / write_from_file / hash_file -- delegate to lore-storage directly
// ---------------------------------------------------------------------------

/// Write content to the immutable store, returning its address.
///
/// Returns [`write_with_tracker`]'s future itself: a future of its own would hold the arguments
/// again beside it.
pub fn write(
    repository: Arc<RepositoryContext>,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
) -> impl Future<Output = Result<Address, ImmutableError>> {
    write_with_tracker(repository, context, buffer, flags, None)
}

/// [`write`] for content the caller lends for the write's duration.
///
/// Returns [`lore_storage::write_content_borrowed`]'s future mapped to the address, holding
/// `repository` until it completes, without a future of its own. The write reads `buffer` in place
/// and holds no reference to it once done.
#[inline]
pub fn write_borrowed(
    repository: Arc<RepositoryContext>,
    context: Context,
    buffer: &[u8],
    flags: WriteOptions,
) -> impl Future<Output = Result<Address, ImmutableError>> {
    let session = if flags.remote_write {
        resolve_session(&repository)
    } else {
        None
    };
    lore_storage::write_content_borrowed(
        repository.immutable_store(),
        repository.id,
        context,
        buffer,
        flags,
        session,
        write_context(None),
        None,
    )
    .map(move |written| {
        drop(repository);
        written
            .map(|written| written.address)
            .forward::<ImmutableError>("writing immutable content")
    })
}

/// Tracker-aware variant of [`write`].
///
/// Returns [`lore_storage::write_content`]'s future mapped to the address, holding `repository`
/// until it completes, without a future of its own.
#[inline]
pub fn write_with_tracker(
    repository: Arc<RepositoryContext>,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> impl Future<Output = Result<Address, ImmutableError>> {
    let session = if flags.remote_write {
        resolve_session(&repository)
    } else {
        None
    };
    lore_storage::write_content(
        repository.immutable_store(),
        repository.id,
        context,
        buffer,
        flags,
        session,
        write_context(tracker),
        None,
    )
    .map(move |written| {
        drop(repository);
        written
            .map(|written| written.address)
            .forward::<ImmutableError>("writing immutable content")
    })
}

/// Write a file to the immutable store, returning its address and the size of the content that
/// address stands for.
///
/// Returns [`write_from_file_with_tracker`]'s future itself: a future of its own would hold the
/// arguments again beside it.
pub fn write_from_file(
    repository: Arc<RepositoryContext>,
    source: &lore_storage::ContentSource<'_>,
    context: Context,
    flags: WriteOptions,
) -> impl Future<Output = Result<(Address, u64), ImmutableError>> {
    write_from_file_with_tracker(repository, source, context, flags, None)
}

/// Tracker-aware variant of [`write_from_file`].
///
/// Returns [`lore_storage::write_from_file`]'s future mapped to the address and size, holding
/// `repository` until it completes, without a future of its own.
#[inline]
pub fn write_from_file_with_tracker(
    repository: Arc<RepositoryContext>,
    source: &lore_storage::ContentSource<'_>,
    context: Context,
    flags: WriteOptions,
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> impl Future<Output = Result<(Address, u64), ImmutableError>> {
    let session = if flags.remote_write {
        resolve_session(&repository)
    } else {
        None
    };
    lore_storage::write_from_file(
        repository.immutable_store(),
        repository.id,
        source,
        context,
        flags,
        session,
        write_context(tracker),
    )
    .map(move |written| {
        drop(repository);
        written
            .map(|written| (written.address, written.size_content))
            .forward::<ImmutableError>("writing immutable content from file")
    })
}

/// The hash of the address the content of `source` would be stored under.
///
/// Whether a file still holds content already stored is [`file_matches`], which measures against
/// the fragmentation that content was stored under.
///
/// Returns [`lore_storage::hash_file`]'s future with its error forwarded, without a future of its
/// own.
#[inline]
pub fn hash_file(
    repository: Arc<RepositoryContext>,
    source: &lore_storage::ContentSource<'_>,
) -> impl Future<Output = Result<Hash, ImmutableError>> {
    lore_storage::hash_file(repository.immutable_store(), repository.id, source, None)
        .map(|hashed| hashed.forward::<ImmutableError>("hashing file"))
}

/// Whether `source` still holds the content `previous` addresses, fetching fragment metadata
/// but never content payloads.
///
/// Measured against the fragmentation the content was stored under, which is the only one that
/// answers for it, so the comparison reaches the remote where the local store no longer holds it.
///
/// Returns [`lore_storage::file_matches`]'s future with its error forwarded, holding `repository`
/// until it completes, without a future of its own.
#[inline]
pub fn file_matches(
    repository: Arc<RepositoryContext>,
    previous: Address,
    previous_size: Option<usize>,
    source: &lore_storage::ContentSource<'_>,
    established: &lore_storage::ContentHashes,
) -> impl Future<Output = Result<lore_storage::FileMatch, ImmutableError>> {
    let remote_session = resolve_session(&repository);
    let trust_remote = read_options_from_repository(&repository).trust_remote;
    lore_storage::file_matches(
        repository.immutable_store(),
        repository.id,
        previous,
        previous_size,
        remote_session,
        trust_remote,
        source,
        established,
    )
    .map(move |matched| {
        drop(repository);
        matched.forward::<ImmutableError>("comparing file against stored content")
    })
}

// ---------------------------------------------------------------------------
// Cache and query helpers
// ---------------------------------------------------------------------------

/// Default cap on the gets [`cache`] calls have in flight on one connection to a remote together, so
/// they put no unbounded backlog of gets ahead of a demand read's priority get, at the server and on
/// the link.
#[lore_macro::test_pub]
const CACHE_GETS: usize = 512;

/// The cap on cache gets in flight on one connection to a remote, from `LORE_CACHE_GETS`, read and
/// logged once ([`cache_gets_from`]). Zero means no cap.
fn cache_gets() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        let cap = cache_gets_from(std::env::var("LORE_CACHE_GETS").ok());
        if cap == 0 {
            lore_debug!("Cache gets are not capped");
        } else {
            lore_debug!("Cache gets are capped at {cap} per connection");
        }
        cap
    })
}

/// The cap a `LORE_CACHE_GETS` of `value` sets: the count; [`CACHE_GETS`] where it is
/// unset or, with a warning, not a count.
#[lore_macro::test_pub]
fn cache_gets_from(value: Option<String>) -> usize {
    let Some(value) = value else {
        return CACHE_GETS;
    };
    let Ok(cap) = value.trim().parse::<usize>() else {
        lore_warn!(
            "LORE_CACHE_GETS={value} is not a count; cache gets take the default cap \
             of {CACHE_GETS}"
        );
        return CACHE_GETS;
    };
    cap
}

/// The connection's cache get permits, which every [`cache_through`] fetch takes; `None` with no
/// cap.
pub(crate) type CacheGets = Option<Arc<tokio::sync::Semaphore>>;

/// What one [`cache_through`] fetch came to.
#[lore_macro::test_pub]
enum Fetch {
    /// A payload fetched: its fragment, marked durable as the remote holds it, the payload as
    /// fetched, for a fragment list the list it expands to, and whether it overwrites a corrupt
    /// local copy.
    Fetched {
        address: Address,
        fragment: Fragment,
        payload: Bytes,
        list: Option<Bytes>,
        replace: bool,
    },
    /// The fragment at this address turned local while the get waited for its permit, so nothing
    /// was fetched. With `cache_fragmented` the next round queries it again, which walks a list
    /// stored meanwhile, as a demand read stores a root alone.
    Local(Address),
    /// The fetch failed, or a payload it checked failed its check.
    Failed(Box<ImmutableError>),
}

/// What the local store holds at an address [`cache_through`] looks up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LocalCopy {
    /// A payload, which for a fragment list reads and expands.
    Held,
    /// No entry, or an entry without its payload, as eviction leaves one: nothing to serve or walk.
    Lacking,
    /// A fragment list that does not read or expand, fetched again to overwrite it.
    Corrupt,
}

/// Whether `resolved` holds a payload locally. An entry without its payload, as eviction leaves
/// one, holds nothing to serve or walk, so it counts as lacking.
fn held_locally(resolved: &StoreMatchResult) -> bool {
    resolved.match_made != StoreMatch::MatchNone && resolved.stored_local
}

/// The flags of the fragment the local store holds a payload for at `address`, in one lookup:
/// zero where it holds none, so an entry without its payload counts as lacking.
async fn held_flags(
    repository: &Arc<RepositoryContext>,
    address: Address,
) -> Result<u32, StoreError> {
    let metadata = repository
        .immutable_store()
        .get_metadata(repository.id, address)
        .await?;
    let held = metadata.match_made != StoreMatch::MatchNone
        && metadata.fragment.flags & FragmentFlags::PayloadStoredLocal != 0;
    Ok(if held { metadata.fragment.flags } else { 0 })
}

/// Pushes the address of every fragment `list` references, in `context`, onto `into`.
fn push_subfragments(list: Bytes, context: Context, into: &mut Vec<Address>) {
    let list = list.to_aligned::<FragmentReference>();
    into.extend(
        list.as_type_slice::<FragmentReference>()
            .iter()
            .map(|reference| Address {
                context,
                hash: reference.hash,
            }),
    );
}

/// Fetches `address` through `remote_storage` ([`lore_storage::read::fetch_remote`]) in the read
/// flight for its content ([`lore_storage::read::fetch_single_flight`]), so a read or a fetch of the
/// same content in flight shares one get, checked on receipt and healed once as a read's fetch is,
/// unless the command trusts the remote ([`read_options_from_repository`]), and never decompressed
/// beyond what that check or a fragment list's walk needs. It joins with the local probe off, as
/// the cache looked locally already.
///
/// The fetch holds one of `permits`, taken before it joins, so a read following it never waits for
/// one, and gives it back on joining as a follower, which makes no get of its own; a follower whose
/// leader fails on another address fetches without it. A fetch that had to wait asks the local store
/// first, since a demand read may have stored the fragment meanwhile, unless it is to `replace` a
/// corrupt local copy, which that store holds.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn fetch_to_cache(
    repository: Arc<RepositoryContext>,
    remote_storage: Arc<StorageSession>,
    permits: CacheGets,
    address: Address,
    replace: bool,
) -> impl Future<Output = Fetch> {
    async move {
        let fetched = {
            let mut permit = match &permits {
                None => None,
                Some(permits) => {
                    if let Ok(permit) = permits.try_acquire() {
                        Some(permit)
                    } else {
                        let permit = permits.acquire().await.ok();
                        if !replace && is_stored_local(repository.clone(), address).await {
                            return Fetch::Local(address);
                        }
                        permit
                    }
                }
            };
            let options = ReadOptions {
                local: false,
                decompress: false,
                ..read_options_from_repository(&repository)
            };
            match lore_storage::read::fetch_single_flight(
                repository.immutable_store(),
                repository.id,
                address,
                options,
                || lore_storage::read::fetch_remote(&remote_storage, address, options),
                || drop(permit.take()),
            )
            .await
            {
                Ok(Flown::Local(_)) => return Fetch::Local(address),
                Ok(Flown::Fetched(fetched)) => {
                    if fetched.raw.0.flags & FragmentFlags::PayloadFragmented == 0 {
                        Ok((fetched.raw, None))
                    } else {
                        let raw = fetched.raw.clone();
                        fetched.take(true).map(|(_, list)| (raw, Some(list)))
                    }
                }
                Err(error) => Err(error),
            }
        };
        match fetched {
            Ok(((fragment, payload), list)) => Fetch::Fetched {
                address,
                fragment,
                payload,
                list,
                replace,
            },
            Err(error) => Fetch::Failed(Box::new(error.forward("fetching fragment to cache"))),
        }
    }
}

/// One batch [`cache_through`] looked up locally: its addresses, what the store holds of each, and
/// the subfragments of those it holds locally as fragment lists.
type QueriedBatch = Result<(Bytes, Vec<LocalCopy>, Vec<Address>), StoreError>;

/// Pushes the subfragments of the fragment list the local store holds at `address` onto
/// `children`, answering whether the list read and expanded; one that does not pushes nothing. The
/// list is read raw, as a list read is no reason to join a read flight.
async fn push_local_list_children(
    repository: &Arc<RepositoryContext>,
    address: Address,
    children: &mut Vec<Address>,
) -> bool {
    let Ok((_, list)) =
        lore_storage::read::read_raw(repository.immutable_store(), repository.id, address)
            .await
            .and_then(|loaded| loaded.take(true))
    else {
        return false;
    };
    push_subfragments(list, address.context, children);
    true
}

/// Fetches every fragment at `address` the local store lacks from the remote and stores it, then,
/// when `cache_fragmented` is set, the subfragments of fragment lists, fetched or held locally. A
/// fragment the store indexes without a local payload counts as lacking, and a local list it
/// cannot read or expand is fetched again and overwritten, as a read replaces a corrupt fragment,
/// and the fetched list walked. Every fetched payload is checked on receipt as a read's is
/// ([`fetch_to_cache`]). Once connected, every fragment that can be cached is, and the answer is
/// the failure to connect, the first failure to query, fetch, check or store a fragment, or else
/// how many fragments it fetched, by its own get or one it joined: a fragment found local after
/// waiting for a cache get permit is not fetched.
pub async fn cache(
    repository: Arc<RepositoryContext>,
    address: Vec<Address>,
    cache_fragmented: bool,
) -> Result<usize, ImmutableError> {
    cache_counted(repository, address, cache_fragmented)
        .await?
        .into_result()
}

/// What [`cache_counted`] came to.
#[derive(Debug, Default)]
pub struct Cached {
    /// Fragments gone to the remote for, by its own get or one it joined, whether or not the fetch
    /// succeeded.
    pub fragments: usize,
    /// Payload bytes of the fragments fetched.
    pub bytes: u64,
    /// Fragments found local after waiting for a cache get permit, and so not fetched.
    pub already_local: usize,
    /// Fragments looked up locally, at every level the walk reached.
    pub queried: usize,
    /// The first failure to query, fetch, check or store a fragment.
    pub failure: Option<Box<ImmutableError>>,
}

impl Cached {
    /// The fragments fetched, or the first failure, as [`cache`] answers.
    pub fn into_result(self) -> Result<usize, ImmutableError> {
        self.failure
            .map_or(Ok(self.fragments), |failure| Err(*failure))
    }
}

/// [`cache`], answering everything it fetched ([`Cached`]) rather than its first failure; only a
/// failure to connect to the remote is an error, and a session that cannot be started fails each
/// fetch.
pub async fn cache_counted(
    repository: Arc<RepositoryContext>,
    address: Vec<Address>,
    cache_fragmented: bool,
) -> Result<Cached, ImmutableError> {
    let (remote_storage, permits) = cache_session(&repository).await?;
    let start = Instant::now();
    let cached = cache_through(
        repository,
        remote_storage,
        permits,
        address,
        cache_fragmented,
    )
    .await;
    lore_debug!(
        "Fetched {} / {} immutable fragments in {:.3}s",
        cached.fragments,
        cached.queried,
        start.elapsed().as_secs_f64()
    );
    Ok(cached)
}

/// The session `repository`'s cache fetches go through, its lazy one ([`resolve_session`]), which
/// starts its server-side sessions again once a fetch invalidates them, and the connection's cache
/// get permits they take ([`cache_through`]).
pub(crate) async fn cache_session(
    repository: &Arc<RepositoryContext>,
) -> Result<(Arc<StorageSession>, CacheGets), ImmutableError> {
    let remote_result: Result<_, ImmutableError> = repository
        .remote()
        .await
        .forward("connecting to remote for cache");
    let remote = remote_result?;
    let Some(session) = resolve_session(repository) else {
        return Err(ImmutableError::from(NoRemote));
    };
    Ok((session, remote.cache_get_permits(cache_gets())))
}

/// [`cache_counted`] once connected, fetching through `remote_storage`, each get holding one of
/// `permits`.
///
/// The walk descends one level of the tree a round, and queries a fragment found local after
/// waiting for its permit once more the round after, so a tree no deeper than a read walks, lists
/// down to [`MAX_FRAGMENT_TREE_DEPTH`] below the root and their subfragments, is cached within
/// twice its levels in rounds. A walk that takes more, as one through a list naming itself or an
/// ancestor would, fails.
///
/// A function of its own because its batches live across several awaits: kept in
/// [`cache_counted`] they would take space in its future while the remote is connected as well.
///
/// [`MAX_FRAGMENT_TREE_DEPTH`]: lore_storage::defragment::MAX_FRAGMENT_TREE_DEPTH
#[lore_macro::test_pub]
pub(crate) async fn cache_through(
    repository: Arc<RepositoryContext>,
    remote_storage: Arc<StorageSession>,
    permits: CacheGets,
    address: Vec<Address>,
    cache_fragmented: bool,
) -> Cached {
    const MAX_REQUEST_COUNT: usize = 1000;
    const MAX_ROUNDS: usize = 2 * (lore_storage::defragment::MAX_FRAGMENT_TREE_DEPTH + 2);

    let mut query_address = address;
    query_address.sort_unstable();
    query_address.dedup();

    let mut query_address = Bytes::from_owner(VecBytes(query_address));
    if !query_address.is_empty() && query_address.as_type_slice::<Address>()[0].is_zero() {
        let _ = query_address.split_to(size_of::<Address>());
    }

    let mut total_fetches = 0;
    let mut total_already_local = 0;
    let mut total_bytes = 0;
    let mut total_query_count = 0;
    let mut failure = None;
    let mut rounds = 0;

    while !query_address.is_empty() {
        if rounds == MAX_ROUNDS {
            failure.get_or_insert_with(|| {
                Box::new(ImmutableError::internal(format!(
                    "fragment tree deeper than {MAX_ROUNDS} cache rounds reach"
                )))
            });
            break;
        }
        rounds += 1;
        let query_count = query_address.count::<Address>();
        total_query_count += query_count;
        lore_trace!("Query and cache {query_count} immutable fragments from remote");

        let mut query_tasks: JoinSet<QueriedBatch> = JoinSet::new();
        while !query_address.is_empty() {
            // Cap number of tasks to a reasonable batch size
            const BATCH_COUNT: usize = 100;
            let to_split = std::cmp::min(query_address.count::<Address>(), BATCH_COUNT);
            let slice =
                query_address.split_off(query_address.len() - to_split * size_of::<Address>());

            lore_trace!(
                "Query {} immutable fragments in local store ({} remains)",
                slice.count::<Address>(),
                query_address.count::<Address>(),
            );

            let repository = repository.clone();
            lore_spawn!(query_tasks, async move {
                let addresses = slice.as_type_slice::<Address>();
                let mut children = Vec::new();
                let held = if cache_fragmented {
                    let mut held = Vec::with_capacity(addresses.len());
                    for address in addresses {
                        let flags = held_flags(&repository, *address).await?;
                        held.push(if flags == 0 {
                            LocalCopy::Lacking
                        } else if flags & FragmentFlags::PayloadFragmented == 0
                            || push_local_list_children(&repository, *address, &mut children).await
                        {
                            LocalCopy::Held
                        } else {
                            LocalCopy::Corrupt
                        });
                    }
                    held
                } else {
                    let mut matches = vec![StoreMatchResult::default(); addresses.len()];
                    repository
                        .immutable_store()
                        .query(repository.id, addresses, &mut matches)
                        .await?;
                    matches
                        .iter()
                        .map(|resolved| {
                            if held_locally(resolved) {
                                LocalCopy::Held
                            } else {
                                LocalCopy::Lacking
                            }
                        })
                        .collect()
                };
                Ok((slice, held, children))
            });
        }

        let mut fetch_tasks = JoinSet::new();
        let mut store_tasks = JoinSet::new();
        let mut fetch_count = 0;
        let mut additional_address = Vec::with_capacity(query_count);

        let mut process_fetch =
            |result: Result<Fetch, JoinError>,
             store_tasks: &mut JoinSet<Result<(), StoreError>>,
             additional_address: &mut Vec<Address>,
             failure: &mut Option<Box<ImmutableError>>| {
                let (address, fragment, payload, list, replace) = match result {
                    Ok(Fetch::Fetched {
                        address,
                        fragment,
                        payload,
                        list,
                        replace,
                    }) => {
                        total_bytes += payload.len() as u64;
                        (address, fragment, payload, list, replace)
                    }
                    Ok(Fetch::Local(address)) => {
                        total_already_local += 1;
                        if cache_fragmented {
                            additional_address.push(address);
                        }
                        return;
                    }
                    Ok(Fetch::Failed(error)) => {
                        failure.get_or_insert(error);
                        return;
                    }
                    Err(error) => {
                        failure.get_or_insert_with(|| {
                            Box::new(ImmutableError::internal_with_context(
                                error,
                                "fetching fragment to cache",
                            ))
                        });
                        return;
                    }
                };

                if cache_fragmented && let Some(list) = list {
                    push_subfragments(list, address.context, additional_address);
                }

                let repository = repository.clone();
                lore_spawn!(store_tasks, async move {
                    repository
                        .immutable_store()
                        .put(repository.id, address, fragment, Some(payload), replace)
                        .await
                });
            };

        while let Some(result) = query_tasks.join_next().await {
            let (address, held, children) = match result {
                Ok(Ok(queried)) => queried,
                Ok(Err(error)) => {
                    failure.get_or_insert_with(|| {
                        Box::new(error.forward("querying fragments to cache"))
                    });
                    continue;
                }
                Err(error) => {
                    failure.get_or_insert_with(|| {
                        Box::new(ImmutableError::internal_with_context(
                            error,
                            "querying fragments to cache",
                        ))
                    });
                    continue;
                }
            };
            additional_address.extend(children);
            let address = address.as_type_slice::<Address>();
            for (index, &local) in held.iter().enumerate() {
                if local == LocalCopy::Held {
                    continue;
                }

                fetch_count += 1;

                lore_spawn!(
                    fetch_tasks,
                    fetch_to_cache(
                        repository.clone(),
                        remote_storage.clone(),
                        permits.clone(),
                        address[index],
                        local == LocalCopy::Corrupt,
                    )
                );

                {
                    while let Some(result) = fetch_tasks.try_join_next() {
                        process_fetch(
                            result,
                            &mut store_tasks,
                            &mut additional_address,
                            &mut failure,
                        );
                    }

                    while fetch_tasks.len() > MAX_REQUEST_COUNT
                        && let Some(result) = fetch_tasks.join_next().await
                    {
                        process_fetch(
                            result,
                            &mut store_tasks,
                            &mut additional_address,
                            &mut failure,
                        );
                    }
                }

                while store_tasks.len() > MAX_REQUEST_COUNT
                    && let Some(result) = store_tasks.join_next().await
                {
                    keep_store_failure(&mut failure, result);
                }
            }
        }

        if fetch_count > 0 {
            lore_trace!(
                "Fetch and store {fetch_count} / {query_count} immutable fragments from remote"
            );
        }
        while let Some(result) = fetch_tasks.join_next().await {
            process_fetch(
                result,
                &mut store_tasks,
                &mut additional_address,
                &mut failure,
            );

            while store_tasks.len() > MAX_REQUEST_COUNT
                && let Some(result) = store_tasks.join_next().await
            {
                keep_store_failure(&mut failure, result);
            }
        }

        if !store_tasks.is_empty() {
            lore_trace!(
                "Wait for {} immutable fragments to be stored",
                store_tasks.len(),
            );
        }
        while let Some(result) = store_tasks.join_next().await {
            keep_store_failure(&mut failure, result);
        }

        total_fetches += fetch_count;

        additional_address.sort_unstable();
        additional_address.dedup();

        query_address = Bytes::from_owner(VecBytes(additional_address));
        if !query_address.is_empty() && query_address.as_type_slice::<Address>()[0].is_zero() {
            let _ = query_address.split_to(size_of::<Address>());
        }
    }

    Cached {
        fragments: total_fetches - total_already_local,
        bytes: total_bytes,
        already_local: total_already_local,
        queried: total_query_count,
        failure,
    }
}

/// Keeps the failure of a [`cache_through`] store task in `failure`, unless it holds an earlier one.
fn keep_store_failure(
    failure: &mut Option<Box<ImmutableError>>,
    result: Result<Result<(), StoreError>, JoinError>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            failure.get_or_insert_with(|| Box::new(error.forward("storing cached fragment")));
        }
        Err(error) => {
            failure.get_or_insert_with(|| {
                Box::new(ImmutableError::internal_with_context(
                    error,
                    "storing cached fragment",
                ))
            });
        }
    }
}

pub async fn is_stored_local(repository: Arc<RepositoryContext>, address: Address) -> bool {
    lore_trace!("Check if {} is cached in local store", address);
    if let Ok(resolved) = query_one(&repository.immutable_store(), repository.id, address).await {
        lore_trace!("Resolve result {:?}", resolved);
        held_locally(&resolved)
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

pub trait ReadFromImmutable<SelfType = Self>
where
    SelfType: zerocopy::IntoBytes + zerocopy::Immutable + zerocopy::FromBytes + std::marker::Send,
{
    /// Reads the value stored at `address`, zeroed for a zero hash.
    ///
    /// The future is not boxed, and is as large as the read. A caller whose own future many
    /// others hold, such as [`State::deserialize`](crate::state::State::deserialize), boxes it.
    fn read_from_immutable(
        repository: Arc<RepositoryContext>,
        address: Address,
        options: ReadOptions,
    ) -> impl Future<Output = Result<SelfType, ImmutableError>> + Send {
        async move {
            // This uninit is safe. It either reads all the bytes of the type, or zeroes
            // out the memory before the data is dropped in case of error
            let mut elem = std::mem::MaybeUninit::<SelfType>::uninit();
            // Zero hash returns empty data from load_raw, so zero-init to avoid
            // uninitialized memory (safe since SelfType: FromBytes)
            if address.hash.is_zero() {
                elem.zero();
            } else {
                let slice = unsafe {
                    std::slice::from_raw_parts_mut(
                        elem.as_mut_ptr().cast::<u8>(),
                        std::mem::size_of::<SelfType>(),
                    )
                };

                // Bound the read by the compile-time size of the target type so a
                // corrupt or hostile fragment cannot trigger a large allocation
                // even if the caller did not supply a cap in `options`.
                let options = options.with_max_content_size(std::mem::size_of::<SelfType>() as u64);

                read_into(
                    repository, address, None, /* Read full object */
                    slice, options,
                )
                .await
                .inspect_err(|_err| {
                    elem.zero();
                })?;
            }

            Ok(unsafe { elem.assume_init() })
        }
    }
}

impl<T> ReadFromImmutable<T> for T where
    T: zerocopy::IntoBytes + zerocopy::Immutable + zerocopy::FromBytes + std::marker::Send
{
}

pub trait ReadBoxFromImmutable<SelfType = Self>
where
    SelfType: zerocopy::IntoBytes
        + zerocopy::FromBytes
        + zerocopy::Immutable
        + crate::lore::ZeroHeapAlloc
        + std::marker::Send,
{
    /// Reads the value stored at `address` into a zeroed heap allocation.
    ///
    /// The future is not boxed, and is as large as the read.
    fn read_box_from_immutable(
        repository: Arc<RepositoryContext>,
        address: Address,
        cache: bool,
    ) -> impl Future<Output = Result<lore_base::allocator::HeapBox<SelfType>, ImmutableError>> + Send
    {
        async move {
            let mut elem = SelfType::new_from_heap_zeroed();
            let slice = unsafe {
                std::slice::from_raw_parts_mut(
                    elem.as_mut_bytes().as_mut_ptr(),
                    std::mem::size_of::<SelfType>(),
                )
            };
            // Target type size bounds the legal content size. Anything larger is
            // a corrupt or hostile fragment and is rejected before any defragment
            // buffer is allocated.
            let options = read_options_from_repository(&repository)
                .optional_cache(cache)
                .with_priority()
                .with_max_content_size(std::mem::size_of::<SelfType>() as u64);

            read_into(
                repository, address, None, /* Read full object */
                slice, options,
            )
            .await
            .inspect_err(|_err| {
                elem.zero();
            })?;

            Ok(elem)
        }
    }
}

pub trait WriteToImmutable: zerocopy::IntoBytes + zerocopy::Immutable {
    /// Writes the value's bytes to the immutable store, returning their address.
    ///
    /// Returns [`write_borrowed`]'s future, not boxed. It reads the value in place for as long as
    /// the write runs, and borrows it.
    fn write_to_immutable(
        &self,
        repository: Arc<RepositoryContext>,
        context: Context,
        flags: WriteOptions,
    ) -> impl Future<Output = Result<Address, ImmutableError>> + Send {
        write_borrowed(repository, context, self.as_bytes(), flags)
    }
}

impl<T> WriteToImmutable for T where T: zerocopy::IntoBytes + zerocopy::Immutable {}
