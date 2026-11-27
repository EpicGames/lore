// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_revision::branch;
use lore_revision::branch::BranchError;
use lore_revision::interface::ExecutionContext;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::no_link_traversal;
use lore_storage::KeyValueStream;
use lore_storage::ReadConsistency;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use opentelemetry::metrics::Histogram;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::Span;
use tracing::debug;
use tracing::info;
use tracing::warn;

use super::branch_record::build_branch;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::log_server_error;
use crate::util::setup_execution;
use crate::util::shared_read::PendingRead;
use crate::util::shared_read::SharedRead;
use crate::util::shared_read::SharedReadMetrics;
use crate::util::shared_read::SharedReads;
use crate::util::shared_read::WindowTooLong;

pub type BranchListStream =
    Pin<Box<dyn Stream<Item = Result<BranchListResponse, Status>> + Send + 'static>>;

/// Branches a single `BranchList` request loads at once when the server
/// configuration does not set `branch_list_concurrency`.
pub const DEFAULT_BRANCH_LIST_CONCURRENCY: NonZeroUsize =
    NonZeroUsize::new(100).expect("a nonzero literal");

/// How long after it starts a read of a repository's branch entries serves other `BranchList`
/// calls when the server configuration does not set `branch_list_share_window_millis`.
pub const DEFAULT_BRANCH_LIST_SHARE_WINDOW: Duration = Duration::from_millis(1000);

/// The longest `branch_list_share_window_millis` the server configuration accepts. A server whose
/// configuration sets a longer window fails to start: a listing up to the window old is no use.
pub const MAX_BRANCH_LIST_SHARE_WINDOW: Duration = Duration::from_secs(60);

/// The reads of branch entries that the `BranchList` calls of one server process share, per
/// repository, as [`SharedReads`] describes.
pub struct BranchListReads {
    reads: Arc<SharedReads<RepositoryId, Arc<BranchEntries>, Status>>,
    listed: ListedEntriesMetrics,
}

/// Histograms of the entries each read lists, of each key type.
#[derive(Clone)]
struct ListedEntriesMetrics {
    ids: Histogram<u64>,
    metadata: Histogram<u64>,
    latest: Histogram<u64>,
}

impl ListedEntriesMetrics {
    fn record(&self, ids: usize, metadata: usize, latest: usize) {
        self.ids.record(ids as u64, &[]);
        self.metadata.record(metadata as u64, &[]);
        self.latest.record(latest as u64, &[]);
    }
}

impl BranchListReads {
    /// Reads shared for `window` after each starts, with instruments named `branch_list.read.*` in
    /// the namespace of `provider`. Fails for a window longer than the maximum of [`SharedReads`].
    pub fn new(
        window: Duration,
        provider: &impl InstrumentProvider,
    ) -> Result<Arc<Self>, WindowTooLong> {
        let entries = |name: &'static str| {
            provider.length_histogram(
                name,
                vec![
                    10., 100., 1_000., 5_000., 10_000., 50_000., 100_000., 500_000., 1_000_000.,
                ],
            )
        };
        Ok(Arc::new(Self {
            reads: SharedReads::new(window, SharedReadMetrics::new(provider, "branch_list.read"))?,
            listed: ListedEntriesMetrics {
                ids: entries("branch_list.read.ids"),
                metadata: entries("branch_list.read.metadata_entries"),
                latest: entries("branch_list.read.latest_entries"),
            },
        }))
    }

    /// The read of the branch entries of `repository` that a call uses.
    fn read(&self, repository: &Arc<RepositoryContext>) -> SharedRead<Arc<BranchEntries>, Status> {
        let listed = self.listed.clone();
        let read_repository = repository.clone();
        self.reads.read(repository.id, move |read_id| {
            read_in_own_execution(read_repository, listed, read_id)
        })
    }
}

/// The execution context of the shared read `read_id`: the read's id as its correlation id, and
/// no user. Built in the read's task, which runs without the cancellation of any request.
fn shared_read_execution(read_id: uuid::Uuid) -> Arc<ExecutionContext> {
    setup_execution(module_path!(), read_id.to_string(), String::new())
}

/// The span of one call's listing, which names the shared read the call uses by its correlation
/// id, and how the call was served.
fn listing_span(read: &SharedRead<Arc<BranchEntries>, Status>) -> Span {
    tracing::info_span!(
        "BranchList::v1::listing",
        read_correlation_id = %read.id(),
        read_served = read.served().as_str(),
    )
}

/// Read the branch entries of `repository` in the execution context and span of the shared read
/// `read_id`, and record in `listed` how many it listed of each key type.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn read_in_own_execution(
    repository: Arc<RepositoryContext>,
    listed: ListedEntriesMetrics,
    read_id: uuid::Uuid,
) -> impl Future<Output = Result<Arc<BranchEntries>, Status>> + Send + 'static {
    let span = tracing::info_span!(
        parent: None,
        "BranchList::v1::read",
        { CORRELATION_ID } = %read_id,
    );
    async move {
        let read = async {
            read_branch_entries(&repository, &listed)
                .await
                .map(Arc::new)
        };
        LORE_CONTEXT
            .scope(shared_read_execution(read_id), read)
            .await
    }
    .instrument(span)
}

/// `lore.revision.v1.RevisionService.BranchList` handler.
///
/// Server-streams one `BranchListResponse` per matching branch, in no
/// particular order. Live branches are always emitted; deleted branches
/// require `include_deleted = true`. The optional `creator` filter is an
/// exact byte-for-byte case-sensitive match on `Branch.creator`.
///
/// The handler reads the repository's `BranchId`, `BranchMetadata` and
/// `BranchLatestPointer` keys with one eventually consistent range read
/// each, at the same time, and makes no single-item mutable store read.
/// Calls for one repository share these reads through `reads`, whatever
/// their `creator` and `include_deleted`. The live branches are the ids the
/// name keys map to. A live branch whose metadata entry the read misses is
/// left out, and one whose latest entry it misses is listed with a zero
/// latest. Deleted branches are the metadata entries no live branch claims:
/// the metadata record preserves the branch id after the name key is removed
/// on delete.
///
/// The first response is sent once the three range reads have finished. Each
/// branch's metadata record is then loaded from the immutable store, up to
/// `concurrency` at once, and sent as it loads.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRevisionService`
#[tracing::instrument(name = "BranchList::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
    concurrency: NonZeroUsize,
    reads: &BranchListReads,
) -> Result<Response<BranchListStream>, Status> {
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().revision_branch_list
    {
        return forward_branch_list(req, caller_context, forwarded_requests).await;
    }
    branch_list_implementation(
        req,
        caller_context,
        immutable_store,
        mutable_store,
        concurrency,
        reads,
    )
    .await
}

/// This `BranchListRequest` should be handled by another server and the response stream
/// forwarded on to the client.
async fn forward_branch_list(
    req: BranchListRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<BranchListStream>, Status> {
    let mut client = forwarded_requests.forwarded_revision_service();
    let request = context.to_forwarded_request(req)?;

    let branch_list_result = client
        .branch_list(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = branch_list_result?;
    Ok(response)
}

/// This `BranchListRequest` should be fulfilled by this server, from the read of `reads` for its
/// repository.
pub async fn branch_list_implementation(
    req: BranchListRequest,
    caller_context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    concurrency: NonZeroUsize,
    reads: &BranchListReads,
) -> Result<Response<BranchListStream>, Status> {
    let creator_filter = req.creator.map(Arc::from);
    let include_deleted = req.include_deleted;

    let execution = setup_execution(
        module_path!(),
        caller_context.correlation_id,
        caller_context.user_id,
    );
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        caller_context.repository_id,
        no_link_traversal(),
    ));
    let read = reads.read(&repository);
    let span = listing_span(&read);

    let (tx, rx) = mpsc::channel(64);

    lore_spawn!(
        LORE_CONTEXT
            .scope(
                execution,
                stream_branches(
                    repository,
                    read.into_pending(),
                    creator_filter,
                    include_deleted,
                    concurrency,
                    tx,
                ),
            )
            .instrument(span)
    );

    Ok(Response::new(Box::pin(ReceiverStream::from(rx))))
}

/// The message of the `RESOURCE_EXHAUSTED` status that ends a throttled `BranchList` call. It tells
/// the caller what to do in place of another call.
#[lore_macro::test_pub]
const OVERLOADED: &str = "Store overloaded. Do not list branches in a loop. For one branch, use `lore branch info --remote <branch>`.";

/// Send one response per branch, from the branch entries of the repository that `read` lists, and
/// end the stream with the status of a failure.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn stream_branches(
    repository: Arc<RepositoryContext>,
    read: PendingRead<Arc<BranchEntries>, Status>,
    creator_filter: Option<Arc<str>>,
    include_deleted: bool,
    concurrency: NonZeroUsize,
    tx: mpsc::Sender<Result<BranchListResponse, Status>>,
) -> impl Future<Output = ()> + Send + 'static {
    async move {
        debug!(
            creator = ?creator_filter,
            include_deleted,
            "Listing branches",
        );
        let listed = match read.result().await {
            Ok(entries) => {
                send_branches(
                    &repository,
                    &entries,
                    &creator_filter,
                    include_deleted,
                    concurrency,
                    &tx,
                )
                .await
            }
            Err(status) => Err(status),
        };
        if let Err(status) = listed {
            // The only source of this code in the listing is a store that asks for a slow down.
            let status = if status.code() == tonic::Code::ResourceExhausted {
                Status::resource_exhausted(OVERLOADED)
            } else {
                status
            };
            log_server_error(&status);
            if let Ok(permit) = tx.reserve().await {
                permit.send(Err(status));
            }
        }
    }
}

/// A live branch whose metadata entry a read listed.
#[derive(Clone, Copy)]
struct LiveBranch {
    id: BranchId,
    metadata_hash: Hash,
    /// Zero where the read listed no latest entry for the branch.
    latest: Hash,
}

/// The `BranchId`, `BranchMetadata` and `BranchLatestPointer` entries of a repository, as one range
/// read of each key type reports them, joined by branch id.
struct BranchEntries {
    /// The ids the branch names map to, each once: two names can map to one branch.
    ids: HashSet<BranchId>,
    /// The ids whose metadata entry is listed. An id with none is left out.
    live: Vec<LiveBranch>,
    /// The metadata hashes of the metadata entries whose key is the metadata key of no id.
    unclaimed_metadata: Vec<Hash>,
    /// The latest entries whose key is the latest key of no id, by key in the form `list` reports
    /// it.
    unclaimed_latest: HashMap<Hash, Hash>,
}

impl BranchEntries {
    /// Join the listed `metadata` and `latest` entries of `repository` to the listed `ids`.
    fn join(
        repository: &RepositoryContext,
        ids: HashSet<BranchId>,
        mut metadata: HashMap<Hash, Hash>,
        mut latest: HashMap<Hash, Hash>,
    ) -> Self {
        let mut live = Vec::with_capacity(ids.len());
        for &id in &ids {
            let Some(metadata_hash) =
                metadata.remove(&listed_key(repository, branch::METADATA, id))
            else {
                continue;
            };
            live.push(LiveBranch {
                id,
                metadata_hash,
                latest: latest
                    .remove(&listed_key(repository, branch::LATEST, id))
                    .unwrap_or_default(),
            });
        }
        let without_metadata = ids.len() - live.len();
        if without_metadata > 0 {
            info!(
                without_metadata,
                "Leaving out branches whose metadata entry is not listed"
            );
        }
        Self {
            ids,
            live,
            unclaimed_metadata: metadata.into_values().collect(),
            unclaimed_latest: latest,
        }
    }

    /// The latest revision of `branch_id`, an id of no name, or zero where the latest entries hold
    /// none for it.
    fn unclaimed_latest_of(&self, repository: &RepositoryContext, branch_id: BranchId) -> Hash {
        self.unclaimed_latest
            .get(&listed_key(repository, branch::LATEST, branch_id))
            .copied()
            .unwrap_or_default()
    }
}

/// The key of the `function` value of `branch_id` in the mutable store of `repository`, in the form
/// the store's `list` reports it.
fn listed_key(repository: &RepositoryContext, function: &str, branch_id: BranchId) -> Hash {
    let (key, key_type) =
        branch::mutable_key(repository.salt(), function, repository.id, branch_id);
    repository
        .read_mutable_store()
        .stored_typed_key(key, key_type)
}

/// Read the branch entries of `repository` with one eventually consistent range read of each key
/// type, the three at the same time, record in `listed` how many each listed, and join them.
async fn read_branch_entries(
    repository: &RepositoryContext,
    listed: &ListedEntriesMetrics,
) -> Result<BranchEntries, Status> {
    let (ids, metadata, latest) = tokio::try_join!(
        list_entries(repository, KeyType::BranchId),
        list_entries(repository, KeyType::BranchMetadata),
        list_entries(repository, KeyType::BranchLatestPointer),
    )?;
    let ids: HashSet<BranchId> = ids.map(|(_key, id)| id.to_context()).collect().await;
    let metadata: HashMap<Hash, Hash> = metadata.collect().await;
    let latest: HashMap<Hash, Hash> = latest.collect().await;
    listed.record(ids.len(), metadata.len(), latest.len());
    Ok(BranchEntries::join(repository, ids, metadata, latest))
}

/// List the entries of `key_type` in `repository`, eventually consistent.
async fn list_entries(
    repository: &RepositoryContext,
    key_type: KeyType,
) -> Result<KeyValueStream, Status> {
    repository
        .read_mutable_store()
        .list(repository.id, key_type, ReadConsistency::Eventual)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            warn!(?err, ?key_type, "Failed to list branch keys");
            Status::internal(err.to_string())
        })
}

/// Send one response per branch of `entries` that passes `creator_filter`, deleted branches only
/// with `include_deleted`, loading up to `concurrency` metadata records at once.
///
/// The live branches are those of `entries.live`. A deleted branch is an unclaimed metadata entry
/// whose record names no listed id, so a live branch is never also sent as deleted.
///
/// A per-branch metadata failure is logged and that branch skipped; a failure the client must see,
/// a store asking the caller to back off above all, ends the stream instead.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn send_branches<'a>(
    repository: &'a Arc<RepositoryContext>,
    entries: &'a Arc<BranchEntries>,
    creator_filter: &'a Option<Arc<str>>,
    include_deleted: bool,
    concurrency: NonZeroUsize,
    tx: &'a mpsc::Sender<Result<BranchListResponse, Status>>,
) -> impl Future<Output = Result<(), Status>> + Send + 'a {
    async move {
        let mut emitted: u64 = 0;

        let live_pass = run_loads(
            stream::iter(entries.live.iter().copied()),
            concurrency,
            |live| {
                load_live_branch(
                    repository.clone(),
                    live.id,
                    live.metadata_hash,
                    live.latest,
                    creator_filter.clone(),
                )
            },
            tx,
            &mut emitted,
        );
        if !live_pass.await? {
            return Ok(());
        }

        if include_deleted {
            let deleted_pass = run_loads(
                stream::iter(entries.unclaimed_metadata.iter().copied()),
                concurrency,
                |metadata_hash| {
                    load_deleted_branch(
                        repository.clone(),
                        entries.clone(),
                        metadata_hash,
                        creator_filter.clone(),
                    )
                },
                tx,
                &mut emitted,
            );
            if !deleted_pass.await? {
                return Ok(());
            }
        }

        debug!(emitted, "BranchList complete");
        Ok(())
    }
}

/// Start a load per item of `items` with `start`, keep at most `concurrency`
/// running, and send each record as its load finishes. Returns `Ok(false)` if the receiver
/// has been dropped.
///
/// Each load runs as its own task, so loads keep making progress while the
/// client is slow to read.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn run_loads<K, F, Fut>(
    mut items: impl Stream<Item = K> + Unpin,
    concurrency: NonZeroUsize,
    mut start: F,
    tx: &mpsc::Sender<Result<BranchListResponse, Status>>,
    emitted: &mut u64,
) -> impl Future<Output = Result<bool, Status>>
where
    F: FnMut(K) -> Fut,
    Fut: Future<Output = Result<Option<model_v1::Branch>, Status>> + Send + 'static,
{
    async move {
        // Dropped on any early return, which aborts the loads still running.
        let mut loads = JoinSet::new();
        let mut items_done = false;
        loop {
            if items_done && loads.is_empty() {
                return Ok(true);
            }
            let can_start = !items_done && loads.len() < concurrency.get();
            let finished = tokio::select! {
                biased;
                // Polled first on every pass so a disconnected client stops the loads
                // even while all loads are pending.
                () = tx.closed() => {
                    debug!(emitted = *emitted, "BranchList receiver dropped");
                    return Ok(false);
                }
                Some(finished) = loads.join_next() => finished,
                item = items.next(), if can_start => {
                    match item {
                        Some(item) => {
                            lore_spawn!(loads, start(item).in_current_span());
                        }
                        None => items_done = true,
                    }
                    continue;
                }
            };
            let record = finished.map_err(|_err| Status::internal("Branch load task failed"))?;
            let Some(branch) = record? else {
                continue;
            };
            let Ok(permit) = tx.reserve().await else {
                debug!(emitted = *emitted, "BranchList receiver dropped");
                return Ok(false);
            };
            permit.send(Ok(BranchListResponse {
                branch: Some(branch),
            }));
            *emitted += 1;
        }
    }
}

/// Load the record of the live branch `branch_id`, whose metadata is stored at `metadata_hash`.
/// Resolves to `None` when the branch is skipped.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn load_live_branch(
    repository: Arc<RepositoryContext>,
    branch_id: BranchId,
    metadata_hash: Hash,
    latest: Hash,
    creator_filter: Option<Arc<str>>,
) -> impl Future<Output = Result<Option<model_v1::Branch>, Status>> {
    async move {
        let loaded = branch::load_metadata(repository, metadata_hash).await;
        let Some(metadata) = loaded_metadata(loaded, metadata_hash)? else {
            return Ok(None);
        };
        if !branch::metadata_creator_matches(creator_filter.as_deref(), &metadata) {
            return Ok(None);
        }
        Ok(Some(build_branch(
            branch_id,
            &metadata,
            metadata_hash,
            false,
            latest,
        )))
    }
}

/// Load the record of the deleted branch whose metadata is stored at `metadata_hash`, with its
/// latest revision from `entries`. Resolves to `None` when the entry is skipped.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn load_deleted_branch(
    repository: Arc<RepositoryContext>,
    entries: Arc<BranchEntries>,
    metadata_hash: Hash,
    creator_filter: Option<Arc<str>>,
) -> impl Future<Output = Result<Option<model_v1::Branch>, Status>> {
    async move {
        let loaded = branch::load_metadata(repository.clone(), metadata_hash).await;
        let Some(metadata) = loaded_metadata(loaded, metadata_hash)? else {
            return Ok(None);
        };
        let Ok(branch_id) = branch::id(&metadata) else {
            return Ok(None);
        };
        // A record that names a listed id is that live branch's, which the live pass sends.
        if entries.ids.contains(&branch_id)
            || !branch::metadata_creator_matches(creator_filter.as_deref(), &metadata)
        {
            return Ok(None);
        }
        let latest = entries.unclaimed_latest_of(&repository, branch_id);
        Ok(Some(build_branch(
            branch_id,
            &metadata,
            metadata_hash,
            true,
            latest,
        )))
    }
}

/// The metadata record a load of `metadata_hash` read, or `None` where it is unreadable. A store's
/// request to back off fails instead, though the read layer reports a throttled immutable read as
/// not found, so such a branch is skipped.
fn loaded_metadata(
    loaded: Result<Metadata, BranchError>,
    metadata_hash: Hash,
) -> Result<Option<Metadata>, Status> {
    match loaded.filter_slow_down()? {
        Ok(metadata) => Ok(Some(metadata)),
        Err(err) => {
            info!(metadata = %metadata_hash, ?err, "Skipping branch: metadata load failed");
            Ok(None)
        }
    }
}
