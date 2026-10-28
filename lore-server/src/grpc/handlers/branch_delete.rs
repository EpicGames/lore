// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchDeleteRequest;
use lore_proto::BranchDeleteResponse;
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

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::hook_error_to_status;
use crate::grpc::require_action;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchDelete::handle", skip_all)]
pub async fn handler(
    request: Request<BranchDeleteRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchDeleteResponse>, Status> {
    require_action(authorizer, &request, Action::Write).await?;
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let branch = BranchId::from(req.branch);

    debug!({BRANCH_ID} = %branch, "Handling branch delete");

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));
    LORE_CONTEXT
        .scope(execution, async move {
            let hook_ctx = HookContext::builder()
                .correlation_id(correlation_id)
                .hook_point(HookPoint::BranchDelete)
                .repository(repository_id)
                .user(user_id)
                .branch(branch)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchDelete, &hook_ctx)
                .await
                .map_err(hook_error_to_status)?;

            match branch::delete(repository, branch)
                .await
                .filter_slow_down()?
            {
                Ok(_) => {
                    debug!({BRANCH_ID} = %branch, "Branch deleted");
                    let num_branches_deleted = instrument_provider.counter("num_branches_deleted");
                    num_branches_deleted.add(1, &[]);

                    notification_sender
                        .branch_deleted(repository_id, branch)
                        .await;

                    hook_dispatcher.spawn_post(HookPoint::BranchDelete, hook_ctx);

                    Ok(Response::new(BranchDeleteResponse {}))
                }
                Err(err) if err.is_branch_not_found() => {
                    info!({BRANCH_ID} = %branch, "Failed to delete branch - does not exist");
                    Ok(Response::new(BranchDeleteResponse {}))
                }
                Err(err) if err.is_delete_protected() => {
                    info!({BRANCH_ID} = %branch, "Failed to delete branch - DeleteProtected");
                    Err(Status::failed_precondition("Branch is delete protected"))
                }
                Err(err) => {
                    warn!({BRANCH_ID} = %branch, error = ?err, "Failed to delete branch");
                    Err(Status::internal(err.to_string()))
                }
            }
        })
        .await
}
