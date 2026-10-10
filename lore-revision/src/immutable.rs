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

/// Construct [`WriteOptions`] from a repository context.
pub fn write_options_from_repository(repository: Arc<RepositoryContext>) -> WriteOptions {
    let flags = WriteOptions::default();
    if !repository.disable_upload() {
        flags.with_remote_write()
    } else {
        flags
    }
}

/// Construct [`ReadOptions`] from a repository context.
pub fn read_options_from_repository(repository: &RepositoryContext) -> ReadOptions {
    let sync_data = crate::runtime::try_execution_context()
        .is_some_and(|context| context.globals().sync_data());
    ReadOptions {
        cache: !repository.disable_cache(),
        direct_write: repository.direct_file_write(),
        sync_data,
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
/// under this command's execution context.
///
/// The context holds the session it is handed, so the resolver holds the context
/// weakly: a strong reference there is a cycle neither end can free, and a context
/// gone by the time a session resolves has no pool left to take sessions from. A call that
/// takes the session holds the context until it completes.
#[lore_macro::test_pub]
fn resolve_session(repository: &Arc<RepositoryContext>) -> Option<Arc<StorageSession>> {
    if repository.is_offline() {
        return None;
    }
    Some(repository.lazy_session(|| {
        let repository = Arc::downgrade(repository);
        let correlation_id = crate::lore::execution_context()
            .globals()
            .correlation_id
            .to_string();
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
/// the point: the connection's own lookup owns a key and re-pins the pool, and
/// every call in a command carries the same key, so all of them land on the one
/// shard it hashes to.
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
    lore_storage::read::read_raw(store, repository, address, false)
        .map(|loaded| loaded.forward::<ImmutableError>("loading raw fragment from store"))
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
                debug_assert!(false, "Remote server responded with error on put: {err}");
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

/// Load a single raw fragment, optionally decompressing and verifying the data.
/// Resolves the remote session from the repository and delegates to
/// [`lore_storage::load_fragment`] which handles remote fetch and heal.
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
    lore_storage::file_matches(
        repository.immutable_store(),
        repository.id,
        previous,
        previous_size,
        remote_session,
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

pub async fn cache(
    repository: Arc<RepositoryContext>,
    address: Vec<Address>,
    cache_fragmented: bool,
) -> Result<usize, ImmutableError> {
    let remote_result: Result<_, ImmutableError> = repository
        .remote()
        .await
        .forward("connecting to remote for cache");
    let remote = remote_result?;
    let correlation_id = crate::lore::execution_context()
        .globals()
        .correlation_id
        .to_string();
    let storage_result: Result<_, ImmutableError> = remote
        .session(repository.id, &correlation_id)
        .await
        .forward("connecting to remote storage for cache");
    let remote_storage = storage_result?;

    cache_through(repository, remote_storage, address, cache_fragmented).await
}

/// [`cache`] once connected: fetches every fragment at `address` the local store lacks
/// through `remote_storage` and stores it, then the subfragments of fragmented ones when
/// `cache_fragmented` is set.
///
/// A function of its own because its batches live across several awaits: kept in [`cache`]
/// they would take space in its future while the remote is connected as well.
async fn cache_through(
    repository: Arc<RepositoryContext>,
    remote_storage: Arc<StorageSession>,
    address: Vec<Address>,
    cache_fragmented: bool,
) -> Result<usize, ImmutableError> {
    const MAX_REQUEST_COUNT: usize = 1000;

    let mut query_address = address;
    query_address.sort_unstable();
    query_address.dedup();

    let mut query_address = Bytes::from_owner(VecBytes(query_address));
    if !query_address.is_empty() && query_address.as_type_slice::<Address>()[0].is_zero() {
        let _ = query_address.split_to(size_of::<Address>());
    }

    let start = Instant::now();
    let mut total_store_count = 0;
    let mut total_query_count = 0;

    while !query_address.is_empty() {
        let query_count = query_address.count::<Address>();
        total_query_count += query_count;
        lore_trace!("Query and cache {query_count} immutable fragments from remote");

        let mut query_tasks: JoinSet<Result<(Bytes, Vec<StoreMatchResult>), StoreError>> =
            JoinSet::new();
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
                let mut matches = vec![StoreMatchResult::default(); addresses.len()];
                repository
                    .immutable_store()
                    .query(repository.id, addresses, &mut matches)
                    .await?;
                Ok((slice, matches))
            });
        }

        let mut fetch_tasks = JoinSet::new();
        let mut store_tasks = JoinSet::new();
        let mut fetch_count = 0;
        let mut additional_address = Vec::with_capacity(query_count);

        let mut process_fetch =
            |result: Result<Result<(Address, Fragment, Bytes), ProtocolError>, _>,
             store_tasks: &mut JoinSet<Result<(), StoreError>>| {
                // Cache is best effort, ignore errors
                let Ok(result) = result else {
                    return;
                };
                let Ok((address, fragment, mut buffer)) = result else {
                    return;
                };

                // If the data is fragmented and we should cache subfragments, queue additional fragments
                if cache_fragmented && (fragment.flags & FragmentFlags::PayloadFragmented) != 0 {
                    // Fragment lists are always uncompressed
                    buffer = buffer.to_aligned::<FragmentReference>();
                    let fragment_list = buffer.as_type_slice::<FragmentReference>();
                    for fragment_ref in fragment_list {
                        additional_address.push(Address {
                            context: address.context,
                            hash: fragment_ref.hash,
                        });
                    }
                }

                let repository = repository.clone();
                lore_spawn!(store_tasks, async move {
                    repository
                        .immutable_store()
                        .put(repository.id, address, fragment, Some(buffer), false)
                        .await
                });
            };

        while let Some(result) = query_tasks.join_next().await {
            if let Ok(Ok((address, matches))) = result
                && address.count::<Address>() == matches.len()
            {
                let address = address.as_type_slice::<Address>();
                for (index, resolved) in matches.iter().enumerate() {
                    if resolved.match_made != StoreMatch::MatchNone {
                        continue;
                    }

                    fetch_count += 1;

                    let remote_storage = remote_storage.clone();
                    let address = address[index];
                    lore_spawn!(fetch_tasks, async move {
                        remote_storage
                            .get(&address)
                            .await
                            .map(|(fragment, buffer)| (address, fragment, buffer))
                    });

                    {
                        while let Some(result) = fetch_tasks.try_join_next() {
                            process_fetch(result, &mut store_tasks);
                        }

                        while fetch_tasks.len() > MAX_REQUEST_COUNT
                            && let Some(result) = fetch_tasks.join_next().await
                        {
                            process_fetch(result, &mut store_tasks);
                        }
                    }

                    while store_tasks.len() > MAX_REQUEST_COUNT {
                        let _ = store_tasks.join_next().await;
                    }
                }
            }
        }

        if fetch_count > 0 {
            lore_trace!(
                "Fetch and store {fetch_count} / {query_count} immutable fragments from remote"
            );
        }
        while let Some(result) = fetch_tasks.join_next().await {
            process_fetch(result, &mut store_tasks);

            while store_tasks.len() > MAX_REQUEST_COUNT {
                let _ = store_tasks.join_next().await;
            }
        }

        if !store_tasks.is_empty() {
            lore_trace!(
                "Wait for {} immutable fragments to be stored",
                store_tasks.len(),
            );
        }
        while store_tasks.join_next().await.is_some() {}

        total_store_count += fetch_count;

        additional_address.sort_unstable();
        additional_address.dedup();

        query_address = Bytes::from_owner(VecBytes(additional_address));
        if !query_address.is_empty() && query_address.as_type_slice::<Address>()[0].is_zero() {
            let _ = query_address.split_to(size_of::<Address>());
        }
    }

    lore_debug!(
        "Cached {total_store_count} / {total_query_count} immutable fragments from remote in {:.3}s",
        start.elapsed().as_secs_f64()
    );

    Ok(total_store_count)
}

pub async fn is_stored_local(repository: Arc<RepositoryContext>, address: Address) -> bool {
    lore_trace!("Check if {} is cached in local store", address);
    if let Ok(resolved) = query_one(&repository.immutable_store(), repository.id, address).await {
        lore_trace!("Resolve result {:?}", resolved);
        resolved.stored_local
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
