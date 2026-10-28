// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::revision::v1::BranchDeleteRequest;
use lore_proto::lore::revision::v1::BranchDeleteResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;
use tracing::warn;

use super::branch_record::build_branch;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::hook_error_to_status;
use crate::grpc::require_action;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::hooks::context::METADATA_BRANCH_NAME;
use crate::hooks::context::METADATA_IS_SERVICE_ACCOUNT;
use crate::util::setup_execution;

/// `lore.revision.v1.RevisionService.BranchDelete` handler.
///
/// Returns the full deleted `Branch` record. Idempotent on
/// already-deleted branches — repeated calls succeed with the same
/// record. Branches that never existed return `NotFound`.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRevisionService`
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "BranchDelete::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchDeleteRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchDeleteResponse>, Status> {
    require_action(authorizer, &request, Action::Write).await?;
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().revision_branch_delete
    {
        forward_branch_delete(req, caller_context, forwarded_requests).await
    } else {
        branch_delete_implementation(
            req,
            caller_context,
            immutable_store,
            mutable_store,
            notification_sender,
            hook_dispatcher,
            instrument_provider,
        )
        .await
    }
}

/// This `BranchDeleteRequest` should be handled by another server
/// and the response forwarded on to the client
async fn forward_branch_delete(
    req: BranchDeleteRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<BranchDeleteResponse>, Status> {
    let mut client = forwarded_requests.forwarded_revision_service();
    let request = context.to_forwarded_request(req)?;

    let branch_delete_result = client
        .branch_delete(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = branch_delete_result?;
    Ok(response)
}

/// This `BranchDeleteRequest` should be fulfilled by this server.
pub async fn branch_delete_implementation(
    req: BranchDeleteRequest,
    caller_context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchDeleteResponse>, Status> {
    let repository_id = caller_context.repository_id;
    let user_id = caller_context.user_id;
    let is_service_account = caller_context.is_service_account;
    let correlation_id = caller_context.correlation_id;
    let branch_id = BranchId::from(req.id);

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // Load before delete so the idempotent already-deleted path
            // can still build the response from the preserved metadata.
            let pre_metadata = branch::metadata(repository.clone(), branch_id)
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::not_found(format!("Branch {branch_id} not found")))?;

            let hook_ctx = HookContext::builder()
                .correlation_id(correlation_id)
                .hook_point(HookPoint::BranchDelete)
                .repository(repository_id)
                .user(user_id)
                .branch(branch_id)
                .metadata(
                    METADATA_BRANCH_NAME,
                    branch::name(&pre_metadata).unwrap_or_default(),
                )
                .metadata(METADATA_IS_SERVICE_ACCOUNT, is_service_account.to_string())
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchDelete, &hook_ctx)
                .await
                .map_err(hook_error_to_status)?;

            debug!({BRANCH_ID} = %branch_id, "Deleting branch");

            let delete_result = branch::delete(repository.clone(), branch_id)
                .await
                .filter_slow_down()?;
            let actually_deleted = match delete_result {
                Ok(()) => true,
                Err(err) if err.is_branch_not_found() => {
                    info!({BRANCH_ID} = %branch_id, "Branch already deleted");
                    false
                }
                Err(err) if err.is_delete_protected() => {
                    info!({BRANCH_ID} = %branch_id, "Branch is delete-protected");
                    return Err(Status::failed_precondition("Branch is delete protected"));
                }
                Err(err) if err.is_delete_current() => {
                    info!({BRANCH_ID} = %branch_id, "Cannot delete currently-checked-out branch");
                    return Err(Status::failed_precondition(
                        "Branch is currently checked out",
                    ));
                }
                Err(err) if err.is_delete_default() => {
                    info!({BRANCH_ID} = %branch_id, "Cannot delete default branch");
                    return Err(Status::failed_precondition("Branch is the default branch"));
                }
                Err(err) => {
                    warn!({BRANCH_ID} = %branch_id, error = ?err, "Failed to delete branch");
                    return Err(Status::internal(err.to_string()));
                }
            };

            if actually_deleted {
                debug!({BRANCH_ID} = %branch_id, "Branch deleted");
                instrument_provider
                    .counter("num_branches_deleted")
                    .add(1, &[]);
                notification_sender
                    .branch_deleted(repository_id, branch_id)
                    .await;
                hook_dispatcher.spawn_post(HookPoint::BranchDelete, hook_ctx);
            }

            // no filter_slow_down()? usage here: the delete has already
            // happened, so this response read must not return a retryable
            // status.
            let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;

            // no filter_slow_down()? usage here: the delete has already
            // happened, so an unreadable latest must not fail it.
            let latest = branch::load_latest(repository, branch_id)
                .await
                .unwrap_or_default();
            let response_branch =
                build_branch(branch_id, &pre_metadata, metadata_hash, true, latest);

            Ok(Response::new(BranchDeleteResponse {
                branch: Some(response_branch),
            }))
        })
        .await
}
