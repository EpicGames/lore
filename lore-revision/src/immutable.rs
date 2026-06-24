// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;
use serde::Deserialize;
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
use crate::lore::extend_lifetime;
use crate::lore_debug;
use crate::lore_trace;
use crate::repository::RepositoryContext;
use crate::store::ImmutableStore;
use crate::store::StoreError;
use crate::store::StoreMatch;

#[error_set]
pub enum ImmutableError {
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
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
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFragmentWriteEventData {
    /// The fragment that was written
    pub fragment: Fragment,
    /// Non-zero if the fragment already existed and was deduplicated
    pub deduplicated: u8,
}

/// Build a `WriteTracker`. When `stats` is set the tracker emits a
/// `FragmentWrite` event per fragment; otherwise it carries no observer so the
/// common commit path avoids the per-fragment overhead.
pub fn commit_write_tracker(stats: bool) -> Arc<lore_storage::write_tracker::WriteTracker> {
    if !stats {
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
        direct_file_io: repository.direct_file_io(),
        sync_data,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Session resolution helper
// ---------------------------------------------------------------------------

/// Build a lazily-resolved storage session for a repository context. The
/// underlying server-side session is established only when the session is
/// actually used — local-only command paths never drive the connect.
fn resolve_session(repository: &Arc<RepositoryContext>) -> Arc<StorageSession> {
    let repository = repository.clone();
    let correlation_id = crate::lore::execution_context()
        .globals()
        .correlation_id
        .to_string();
    Arc::new(StorageSession::pending(move || {
        let repository = repository.clone();
        let correlation_id = correlation_id.clone();
        async move {
            let remote = repository.remote().await?;
            remote.session(repository.id, &correlation_id).await
        }
    }))
}

// ---------------------------------------------------------------------------
// Local store helpers
// ---------------------------------------------------------------------------

/// Load a single raw fragment from local store with retry backoff.
pub async fn load_raw_store_retry(
    store: Arc<dyn ImmutableStore>,
    repository: Partition,
    address: Address,
    match_required: StoreMatch,
) -> Result<(Fragment, Bytes), ImmutableError> {
    lore_storage::read::read_raw(store, repository, address, match_required)
        .await
        .forward("loading raw fragment from store")
}

/// Write a single raw fragment to local store with retry backoff.
pub async fn store_raw_store_retry(
    store: Arc<dyn ImmutableStore>,
    repository: Partition,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> Result<(), ImmutableError> {
    lore_storage::write_raw(store, repository, address, fragment, payload)
        .await
        .forward("storing raw fragment to store")
}

/// Store a raw fragment to a remote session with retry on `SlowDown`.
pub async fn store_raw_remote_retry(
    remote_storage: Arc<StorageSession>,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> Result<(), ImmutableError> {
    let mut retry = lore_storage::retry(50, 10_000, 60);
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
pub async fn load_raw(
    repository: Arc<RepositoryContext>,
    address: Address,
    options: ReadOptions,
) -> Result<(Fragment, Bytes), ImmutableError> {
    let session = Some(resolve_session(&repository));

    lore_storage::load_fragment(
        repository.immutable_store(),
        repository.id,
        address,
        options,
        session,
    )
    .await
    .forward("loading fragment")
}

// ---------------------------------------------------------------------------
// Read path -- delegates to lore-storage with session
// ---------------------------------------------------------------------------

pub async fn read_stream(
    repository: Arc<RepositoryContext>,
    address: Address,
    options: ReadOptions,
    sender: Sender<Bytes>,
) -> Result<u64, ImmutableError> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let session = Some(resolve_session(&repository));
    lore_storage::read_stream(store, partition, address, options, sender, session)
        .await
        .forward("reading immutable data")
}

/// Read the given data range from a fragment which can be a large data set
/// stored as a fragment list. The function will reassemble and decompress
/// any data from the fragments holding the data range requested.
pub async fn read(
    repository: Arc<RepositoryContext>,
    address: Address,
    range: Option<Range<usize>>,
    options: ReadOptions,
) -> Result<Bytes, ImmutableError> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let session = Some(resolve_session(&repository));
    lore_storage::read(store, partition, address, range, options, session)
        .await
        .forward("reading immutable data")
}

pub async fn read_into(
    repository: Arc<RepositoryContext>,
    address: Address,
    range: Option<Range<usize>>,
    slice: &mut [u8],
    options: ReadOptions,
) -> Result<(), ImmutableError> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let session = Some(resolve_session(&repository));
    lore_storage::read_into(store, partition, address, range, slice, options, session)
        .await
        .forward("reading immutable data")
}

pub async fn read_into_file(
    repository: Arc<RepositoryContext>,
    address: Address,
    path: &Path,
    options: ReadOptions,
) -> Result<(Fragment, Option<std::fs::Metadata>), ImmutableError> {
    let store = repository.immutable_store();
    let partition = repository.id;
    let temp_ext = crate::repository::TEMP_FILE_EXTENSION;
    let session = Some(resolve_session(&repository));
    lore_storage::read_into_file(store, partition, address, path, temp_ext, options, session)
        .await
        .forward("reading immutable data")
}

// ---------------------------------------------------------------------------
// store_raw -- thin wrapper: store_fragment + session + event
// ---------------------------------------------------------------------------

/// Store a raw fragment: delegates to [`lore_storage::store_fragment`] with
/// an optional remote session for durable upload.
pub async fn store_raw(
    repository: Arc<RepositoryContext>,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_write: bool,
) -> Result<(Address, Fragment), ImmutableError> {
    store_raw_with_tracker(
        repository,
        address,
        fragment,
        buffer,
        cache_local,
        remote_write,
        None,
    )
    .await
}

/// Tracker-aware variant of [`store_raw`]. When `tracker` is `Some`, the
/// background leader task is dispatched into the tracker and the call returns
/// as soon as the address is known. Commit-level callers build the tracker
/// once per operation and pass it through.
#[allow(clippy::too_many_arguments)]
pub async fn store_raw_with_tracker(
    repository: Arc<RepositoryContext>,
    address: Address,
    fragment: Fragment,
    buffer: Bytes,
    cache_local: bool,
    remote_write: bool,
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> Result<(Address, Fragment), ImmutableError> {
    let session = if remote_write {
        Some(resolve_session(&repository))
    } else {
        None
    };

    let result = lore_storage::store_fragment(
        repository.immutable_store(),
        repository.id,
        address,
        fragment,
        buffer,
        cache_local,
        session,
        tracker,
        None,
    )
    .await
    .forward::<ImmutableError>("storing fragment")?;

    Ok((result.address, result.fragment))
}

// ---------------------------------------------------------------------------
// Write / write_from_file / hash_file -- delegate to lore-storage directly
// ---------------------------------------------------------------------------

pub async fn write(
    repository: Arc<RepositoryContext>,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
) -> Result<(Address, Fragment), ImmutableError> {
    write_with_tracker(repository, context, buffer, flags, None).await
}

/// Tracker-aware variant of [`write`].
pub async fn write_with_tracker(
    repository: Arc<RepositoryContext>,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> Result<(Address, Fragment), ImmutableError> {
    let session = if flags.remote_write {
        Some(resolve_session(&repository))
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
        tracker,
    )
    .await
    .forward("writing immutable content")
}

pub async fn write_from_file(
    repository: Arc<RepositoryContext>,
    path: &Path,
    context: Context,
    flags: WriteOptions,
) -> Result<(Address, Fragment), ImmutableError> {
    write_from_file_with_tracker(repository, path, context, flags, None).await
}

/// Tracker-aware variant of [`write_from_file`].
pub async fn write_from_file_with_tracker(
    repository: Arc<RepositoryContext>,
    path: &Path,
    context: Context,
    flags: WriteOptions,
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> Result<(Address, Fragment), ImmutableError> {
    let session = if flags.remote_write {
        Some(resolve_session(&repository))
    } else {
        None
    };
    lore_storage::write_from_file(
        repository.immutable_store(),
        repository.id,
        path,
        context,
        flags,
        session,
        tracker,
    )
    .await
    .forward("writing immutable content from file")
}

pub async fn hash_file(
    repository: Arc<RepositoryContext>,
    path: impl AsRef<Path>,
    previous: Option<Address>,
    previous_size: Option<usize>,
) -> Result<Hash, ImmutableError> {
    lore_storage::hash_file(
        repository.immutable_store(),
        repository.id,
        path,
        previous,
        previous_size,
        None,
    )
    .await
    .forward("hashing file")
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

        let mut query_tasks: JoinSet<Result<(Bytes, Vec<StoreMatch>), StoreError>> = JoinSet::new();
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
                let matches = repository
                    .immutable_store()
                    .exist_batch(
                        repository.id,
                        slice.as_type_slice::<Address>(),
                        StoreMatch::MatchHash,
                    )
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
                for (index, match_made) in matches.iter().enumerate() {
                    if *match_made != StoreMatch::MatchNone {
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
    if let Ok(query) = repository
        .immutable_store()
        .query(repository.id, address, StoreMatch::MatchHash)
        .await
    {
        lore_trace!("Query result {:?}", query);
        query.match_made != StoreMatch::MatchNone
            && (query.fragment.flags & FragmentFlags::PayloadStoredLocal) != 0
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ReadFromImmutable<SelfType = Self>
where
    SelfType: zerocopy::IntoBytes + zerocopy::Immutable + zerocopy::FromBytes + std::marker::Send,
{
    async fn read_from_immutable(
        repository: Arc<RepositoryContext>,
        address: Address,
        options: ReadOptions,
    ) -> Result<SelfType, ImmutableError> {
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

impl<T> ReadFromImmutable<T> for T where
    T: zerocopy::IntoBytes + zerocopy::Immutable + zerocopy::FromBytes + std::marker::Send
{
}

#[async_trait]
pub trait ReadBoxFromImmutable<SelfType = Self>
where
    SelfType: zerocopy::IntoBytes
        + zerocopy::FromBytes
        + zerocopy::Immutable
        + crate::lore::ZeroHeapAlloc
        + std::marker::Send,
{
    async fn read_box_from_immutable(
        repository: Arc<RepositoryContext>,
        address: Address,
        cache: bool,
    ) -> Result<Box<SelfType>, ImmutableError> {
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

#[async_trait]
pub trait WriteToImmutable: zerocopy::IntoBytes + zerocopy::Immutable + std::marker::Send {
    async fn write_to_immutable(
        &self,
        repository: Arc<RepositoryContext>,
        context: Context,
        flags: WriteOptions,
    ) -> Result<(Address, Fragment), ImmutableError> {
        let self_slice = self.as_bytes();
        // Unsafe extension of the lifetime of the self-as-buffer memory. Since
        // we await the task and no shared references of the buffer will be kept
        // around this is safe.
        let buffer = Bytes::from_static(unsafe { extend_lifetime(self_slice) });
        write(repository, context, buffer, flags).await
    }
}

impl<T> WriteToImmutable for T where T: zerocopy::IntoBytes + zerocopy::Immutable + std::marker::Send
{}
