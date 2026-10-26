// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::KeyType;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
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

/// `lore.revision.v1.RevisionService.BranchList` handler.
///
/// Server-streams one `BranchListResponse` per matching branch. Live
/// branches are always emitted; deleted branches require
/// `include_deleted = true`. The optional `creator` filter is an exact
/// byte-for-byte case-sensitive match on `Branch.creator`.
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
) -> Result<Response<BranchListStream>, Status> {
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().revision_branch_list
    {
        return forward_branch_list(req, caller_context, forwarded_requests).await;
    }
    branch_list_implementation(req, caller_context, immutable_store, mutable_store).await
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

    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        stream_branches(repository, creator_filter, include_deleted, tx).await;
    }));

    Ok(Response::new(Box::pin(ReceiverStream::from(rx))))
}

async fn stream_branches(
    repository: Arc<RepositoryContext>,
    creator_filter: Option<String>,
    include_deleted: bool,
    tx: mpsc::Sender<Result<BranchListResponse, Status>>,
) {
    if let Err(status) = produce_branches(repository, creator_filter, include_deleted, &tx).await {
        log_server_error(&status);
        let _ = tx.send(Err(status)).await;
    }
}

/// Emit one response per branch. A per-branch metadata failure is logged and
/// that branch skipped; a per-branch failure the client must see — a store
/// asking the caller to back off above all — ends the stream instead.
async fn produce_branches(
    repository: Arc<RepositoryContext>,
    creator_filter: Option<String>,
    include_deleted: bool,
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
    let mut ids = UnboundedReceiverStream::new(id_stream.channel());

    while let Some((_key, id)) = ids.next().await {
        let branch_id: BranchId = id.to_context();
        live_ids.insert(branch_id);

        let metadata_hash = match branch::metadata_hash(repository.clone(), branch_id)
            .await
            .filter_slow_down()?
        {
            Ok(hash) => hash,
            Err(err) => {
                info!({BRANCH_ID} = %branch_id, ?err, "Skipping branch: metadata hash load failed");
                continue;
            }
        };
        let metadata = match branch::load_metadata(repository.clone(), metadata_hash)
            .await
            .filter_slow_down()?
        {
            Ok(metadata) => metadata,
            Err(err) => {
                info!({BRANCH_ID} = %branch_id, ?err, "Skipping branch: metadata load failed");
                continue;
            }
        };

        if let Some(ref required) = creator_filter {
            let creator = branch::creator(&metadata).unwrap_or_default();
            if creator != required.as_str() {
                continue;
            }
        }

        if !emit_branch(
            &repository,
            branch_id,
            &metadata,
            metadata_hash,
            false,
            tx,
            &mut emitted,
        )
        .await?
        {
            return Ok(());
        }
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
        let mut entries = UnboundedReceiverStream::new(metadata_stream.channel());

        while let Some((_key, metadata_hash)) = entries.next().await {
            let metadata = match branch::load_metadata(repository.clone(), metadata_hash)
                .await
                .filter_slow_down()?
            {
                Ok(metadata) => metadata,
                Err(err) => {
                    info!(?err, "Skipping entry: metadata load failed");
                    continue;
                }
            };

            let Ok(id_bytes) = metadata.get_binary(branch::ID) else {
                continue;
            };
            let branch_id: BranchId = id_bytes.into();

            if live_ids.contains(&branch_id) {
                continue;
            }

            if let Some(ref required) = creator_filter {
                let creator = branch::creator(&metadata).unwrap_or_default();
                if creator != required.as_str() {
                    continue;
                }
            }

            if !emit_branch(
                &repository,
                branch_id,
                &metadata,
                metadata_hash,
                true,
                tx,
                &mut emitted,
            )
            .await?
            {
                return Ok(());
            }
        }
    }

    debug!(emitted, "BranchList complete");
    Ok(())
}

/// Build and send one branch record. Returns `Ok(false)` if the receiver has
/// been dropped. An unreadable latest revision defaults to zero; a store asking
/// the caller to back off ends the stream with that signal instead.
async fn emit_branch(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    metadata: &lore_revision::metadata::Metadata,
    metadata_hash: lore_base::types::Hash,
    deleted: bool,
    tx: &mpsc::Sender<Result<BranchListResponse, Status>>,
    emitted: &mut u64,
) -> Result<bool, Status> {
    // A throttled latest read ends the stream rather than omitting this branch;
    // any other unreadable latest is reported as a zero latest, as before.
    let latest = branch::load_latest(repository.clone(), branch_id)
        .await
        .filter_slow_down()?
        .unwrap_or_default();
    let response_branch = build_branch(branch_id, metadata, metadata_hash, deleted, latest);

    if tx
        .send(Ok(BranchListResponse {
            branch: Some(response_branch),
        }))
        .await
        .is_err()
    {
        // Client dropped the stream; stop producing.
        debug!(emitted = *emitted, "BranchList receiver dropped");
        return Ok(false);
    }
    *emitted += 1;
    Ok(true)
}
