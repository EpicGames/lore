// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_revision::branch;
use lore_revision::branch::BranchError;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
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

pub type BranchListStream =
    Pin<Box<dyn Stream<Item = Result<BranchListResponse, Status>> + Send + 'static>>;

/// Branches a single `BranchList` request loads at once when the server
/// configuration does not set `branch_list_concurrency`.
pub const DEFAULT_BRANCH_LIST_CONCURRENCY: NonZeroUsize =
    NonZeroUsize::new(100).expect("a nonzero literal");

/// `lore.revision.v1.RevisionService.BranchList` handler.
///
/// Server-streams one `BranchListResponse` per matching branch, in no
/// particular order. Live branches are always emitted; deleted branches
/// require `include_deleted = true`. The optional `creator` filter is an
/// exact byte-for-byte case-sensitive match on `Branch.creator`. Up to
/// `concurrency` branches are loaded from the store at once.
///
/// Live branches are enumerated from the name → id (`BranchId`) keys —
/// the same source of truth the local client and legacy handler use.
/// This matters for repositories whose branches have a name → id mapping
/// but no `BranchMetadata` key in the typed list index (legacy or
/// partially-provisioned repos): such branches are still point-loadable
/// by id, so they appear in the live listing here but would be missed by
/// a `BranchMetadata` scan.
///
/// Deleted branches are only surfaced when `include_deleted = true`, via
/// a supplementary `BranchMetadata` scan: the metadata blob preserves the
/// branch id even after the name → id mapping is erased on delete, and
/// any id already seen in the live pass is skipped.
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

/// This `BranchListRequest` should be fulfilled by this server.
pub async fn branch_list_implementation(
    req: BranchListRequest,
    caller_context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    concurrency: NonZeroUsize,
) -> Result<Response<BranchListStream>, Status> {
    let creator_filter = req.creator;
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
    ));

    let (tx, rx) = mpsc::channel(64);

    lore_spawn!(
        LORE_CONTEXT
            .scope(
                execution,
                stream_branches(repository, creator_filter, include_deleted, concurrency, tx),
            )
            .in_current_span()
    );

    Ok(Response::new(Box::pin(ReceiverStream::from(rx))))
}

async fn stream_branches(
    repository: Arc<RepositoryContext>,
    creator_filter: Option<String>,
    include_deleted: bool,
    concurrency: NonZeroUsize,
    tx: mpsc::Sender<Result<BranchListResponse, Status>>,
) {
    if let Err(status) = produce_branches(
        repository,
        creator_filter.map(Arc::from),
        include_deleted,
        concurrency,
        &tx,
    )
    .await
    {
        log_server_error(&status);
        let _ = tx.send(Err(status)).await;
    }
}

/// Emit one response per branch. A per-branch metadata failure is logged and
/// that branch skipped; a per-branch failure the client must see — a store
/// asking the caller to back off above all — ends the stream instead.
async fn produce_branches(
    repository: Arc<RepositoryContext>,
    creator_filter: Option<Arc<str>>,
    include_deleted: bool,
    concurrency: NonZeroUsize,
    tx: &mpsc::Sender<Result<BranchListResponse, Status>>,
) -> Result<(), Status> {
    debug!(
        creator = ?creator_filter,
        include_deleted,
        "Listing branches",
    );

    let mut emitted: u64 = 0;
    let mut live_ids: HashSet<BranchId> = HashSet::new();

    let id_stream = repository
        .read_mutable_store()
        .list(repository.id, KeyType::BranchId)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            warn!(?err, "Failed to list branch id keys");
            Status::internal(err.to_string())
        })?;
    let ids = UnboundedReceiverStream::new(id_stream.channel());
    let live_pass = run_loads(
        ids,
        concurrency,
        |(_key, id)| {
            let branch_id: BranchId = id.to_context();
            live_ids.insert(branch_id);
            load_live_branch(repository.clone(), branch_id, creator_filter.clone())
        },
        tx,
        &mut emitted,
    );
    if !live_pass.await? {
        return Ok(());
    }

    if include_deleted {
        let metadata_stream = repository
            .read_mutable_store()
            .list(repository.id, KeyType::BranchMetadata)
            .await
            .filter_slow_down()?
            .map_err(|err| {
                warn!(?err, "Failed to list branch metadata keys");
                Status::internal(err.to_string())
            })?;
        let entries = UnboundedReceiverStream::new(metadata_stream.channel());
        let live_ids = Arc::new(live_ids);
        let deleted_pass = run_loads(
            entries,
            concurrency,
            |(_key, metadata_hash)| {
                load_deleted_branch(
                    repository.clone(),
                    metadata_hash,
                    creator_filter.clone(),
                    live_ids.clone(),
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

/// Start a load per key with `start`, keep at most `concurrency` running, and
/// send each record as its load finishes. Returns `Ok(false)` if the receiver
/// has been dropped.
///
/// Each load runs as its own task, so loads keep making progress while the
/// client is slow to read.
async fn run_loads<K, F, Fut>(
    mut keys: impl Stream<Item = K> + Unpin,
    concurrency: NonZeroUsize,
    mut start: F,
    tx: &mpsc::Sender<Result<BranchListResponse, Status>>,
    emitted: &mut u64,
) -> Result<bool, Status>
where
    F: FnMut(K) -> Fut,
    Fut: Future<Output = Result<Option<model_v1::Branch>, Status>> + Send + 'static,
{
    // Dropped on any early return, which aborts the loads still running.
    let mut loads = JoinSet::new();
    let mut keys_done = false;
    loop {
        if keys_done && loads.is_empty() {
            return Ok(true);
        }
        let can_start = !keys_done && loads.len() < concurrency.get();
        let finished = tokio::select! {
            biased;
            // Polled first on every pass so a disconnected client stops the loads
            // even while all loads are pending.
            () = tx.closed() => {
                debug!(emitted = *emitted, "BranchList receiver dropped");
                return Ok(false);
            }
            Some(finished) = loads.join_next() => finished,
            key = keys.next(), if can_start => {
                match key {
                    Some(key) => {
                        lore_spawn!(loads, start(key).in_current_span());
                    }
                    None => keys_done = true,
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

/// Load the record of the live branch `branch_id`. Resolves to `None` when the
/// branch is skipped.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn load_live_branch(
    repository: Arc<RepositoryContext>,
    branch_id: BranchId,
    creator_filter: Option<Arc<str>>,
) -> impl Future<Output = Result<Option<model_v1::Branch>, Status>> {
    async move {
        // A creator filter can skip most branches, so each branch's latest
        // revision is read only after the branch passes the filter, rather than
        // alongside its metadata.
        let (metadata, latest) = if creator_filter.is_none() {
            let (metadata, latest) = tokio::join!(
                load_live_metadata(repository.clone(), branch_id),
                branch::load_latest(repository.clone(), branch_id),
            );
            (metadata, Some(latest))
        } else {
            (
                load_live_metadata(repository.clone(), branch_id).await,
                None,
            )
        };
        let Some((metadata_hash, metadata)) = metadata? else {
            return Ok(None);
        };
        if !creator_matches(&metadata, creator_filter.as_deref()) {
            return Ok(None);
        }
        // Used only once the branch is known to be listed, so a throttled read
        // never ends the stream for a branch that is skipped.
        let latest = match latest {
            Some(latest) => latest,
            None => branch::load_latest(repository, branch_id).await,
        };
        build_record(latest, branch_id, &metadata, metadata_hash, false).map(Some)
    }
}

/// Load the metadata hash and metadata of the live branch `branch_id`.
/// Resolves to `None` when either is unreadable.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn load_live_metadata(
    repository: Arc<RepositoryContext>,
    branch_id: BranchId,
) -> impl Future<Output = Result<Option<(Hash, Metadata)>, Status>> {
    async move {
        let metadata_hash = match branch::metadata_hash(repository.clone(), branch_id)
            .await
            .filter_slow_down()?
        {
            Ok(hash) => hash,
            Err(err) => {
                info!({BRANCH_ID} = %branch_id, ?err, "Skipping branch: metadata hash load failed");
                return Ok(None);
            }
        };
        match branch::load_metadata(repository, metadata_hash)
            .await
            .filter_slow_down()?
        {
            Ok(metadata) => Ok(Some((metadata_hash, metadata))),
            Err(err) => {
                info!({BRANCH_ID} = %branch_id, ?err, "Skipping branch: metadata load failed");
                Ok(None)
            }
        }
    }
}

/// Load the record of the deleted branch whose metadata is stored at
/// `metadata_hash`. Resolves to `None` when the entry is skipped.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn load_deleted_branch(
    repository: Arc<RepositoryContext>,
    metadata_hash: Hash,
    creator_filter: Option<Arc<str>>,
    live_ids: Arc<HashSet<BranchId>>,
) -> impl Future<Output = Result<Option<model_v1::Branch>, Status>> {
    async move {
        let metadata = match branch::load_metadata(repository.clone(), metadata_hash)
            .await
            .filter_slow_down()?
        {
            Ok(metadata) => metadata,
            Err(err) => {
                info!(?err, "Skipping entry: metadata load failed");
                return Ok(None);
            }
        };
        let Ok(id_bytes) = metadata.get_binary(branch::ID) else {
            return Ok(None);
        };
        let branch_id: BranchId = id_bytes.into();
        if live_ids.contains(&branch_id) || !creator_matches(&metadata, creator_filter.as_deref()) {
            return Ok(None);
        }
        let latest = branch::load_latest(repository, branch_id).await;
        build_record(latest, branch_id, &metadata, metadata_hash, true).map(Some)
    }
}

fn creator_matches(metadata: &Metadata, creator_filter: Option<&str>) -> bool {
    creator_filter.is_none_or(|required| branch::creator(metadata).unwrap_or_default() == required)
}

/// Build one branch record from its metadata and the result of reading its
/// latest revision.
fn build_record(
    latest: Result<Hash, BranchError>,
    branch_id: BranchId,
    metadata: &Metadata,
    metadata_hash: Hash,
    deleted: bool,
) -> Result<model_v1::Branch, Status> {
    // A throttled read ends the stream rather than omitting this branch. If the
    // latest revision can't be read for any other reason, the branch is still
    // listed with a zero latest.
    let latest = latest.filter_slow_down()?.unwrap_or_default();
    Ok(build_branch(
        branch_id,
        metadata,
        metadata_hash,
        deleted,
        latest,
    ))
}
