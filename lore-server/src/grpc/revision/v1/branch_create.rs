// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_proto::lore::revision::v1::BranchCreateRequest;
use lore_proto::lore::revision::v1::BranchCreateResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::util;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use super::branch_record::build_branch;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::grpc::require_action;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::hooks::context::METADATA_BRANCH_NAME;
use crate::hooks::context::METADATA_IS_SERVICE_ACCOUNT;
use crate::util::setup_execution;

/// Reject oversized string fields early to prevent resource exhaustion.
#[lore_macro::test_pub]
fn validate_create_input(name: &str, category: &str, creator: &str) -> Result<(), Status> {
    if name.len() > branch::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Branch name exceeds maximum length of {} bytes",
            branch::MAX_NAME_LEN,
        )));
    }
    if category.len() > branch::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Branch category exceeds maximum length of {} bytes",
            branch::MAX_NAME_LEN,
        )));
    }
    if creator.len() > repository::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Creator exceeds maximum length of {} bytes",
            repository::MAX_NAME_LEN,
        )));
    }
    Ok(())
}

/// `lore.revision.v1.RevisionService.BranchCreate` handler.
///
/// The caller pre-generates `Branch.id` (retry idempotency). The
/// server assigns `created` and the response's full `Branch` record.
/// `creator` is hybrid: caller-set if permitted, otherwise the
/// authenticated JWT identity.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRevisionService`
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "BranchCreate::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchCreateRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    require_action(authorizer, &request, Action::Write).await?;
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().revision_branch_create
    {
        forward_branch_create(req, caller_context, forwarded_requests).await
    } else {
        branch_create_implementation(
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

/// This `BranchCreateRequest` should be handled by another server
/// and the response forwarded on to the client
async fn forward_branch_create(
    req: BranchCreateRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<BranchCreateResponse>, Status> {
    let mut client = forwarded_requests.forwarded_revision_service();
    let request = context.to_forwarded_request(req)?;

    let branch_create_result = client
        .branch_create(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = branch_create_result?;
    Ok(response)
}

/// This `BranchCreateRequest` should be fulfilled by this server.
pub async fn branch_create_implementation(
    req: BranchCreateRequest,
    context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    let name = req.name;
    let category = req.category;
    let creator = req.creator.unwrap_or_else(|| context.user_id.clone());
    let stack: Vec<BranchPoint> = req.stack.into_iter().map(BranchPoint::from).collect();

    let branch = BranchId::from(req.id);

    let created = util::time::timestamp();

    let execution = setup_execution(
        module_path!(),
        context.correlation_id.clone(),
        context.user_id.clone(),
    );
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        context.repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            validate_create_input(&name, &category, &creator)?;

            let hook_ctx = HookContext::builder()
                .correlation_id(&context.correlation_id)
                .hook_point(HookPoint::BranchCreate)
                .repository(context.repository_id)
                .user(&context.user_id)
                .branch(branch)
                .metadata(METADATA_BRANCH_NAME, &name)
                .metadata(
                    METADATA_IS_SERVICE_ACCOUNT,
                    context.is_service_account.to_string(),
                )
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchCreate, &hook_ctx)
                .await
                .map_err(hook_error_to_status)?;

            debug!({BRANCH_ID} = %branch, branch_name = %name, stack = ?stack, "Creating branch");

            let write_token = get_write_token();
            let branch_id = branch::create(
                repository.clone(),
                &write_token,
                branch,
                name.as_str(),
                category.as_str(),
                creator.as_str(),
                created,
                stack.clone(),
                false,
                false,
            )
            .await
            .filter_slow_down()?
            .map_err(|err| {
                if err.is_branch_already_exists() {
                    Status::already_exists(err.to_string())
                } else {
                    Status::invalid_argument(err.to_string())
                }
            })?;

            notification_sender
                .branch_created(context.repository_id, branch_id)
                .await;
            hook_dispatcher.spawn_post(HookPoint::BranchCreate, hook_ctx);

            // no filter_slow_down()? usage here: the branch is already created,
            // so these response reads must not return a retryable status — the
            // retry can only fail with BranchAlreadyExists.
            let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;
            let metadata = branch::load_metadata(repository.clone(), metadata_hash)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;

            debug!({BRANCH_ID} = %branch_id, %name, "Created branch");

            // no filter_slow_down()? usage here: the branch exists, so an
            // unreadable latest must not fail a create that has already
            // committed.
            let latest = branch::load_latest(repository, branch_id)
                .await
                .unwrap_or_default();
            let response_branch = build_branch(branch_id, &metadata, metadata_hash, false, latest);

            instrument_provider
                .counter("num_branches_created")
                .add(1, &[]);

            Ok(Response::new(BranchCreateResponse {
                branch: Some(response_branch),
            }))
        })
        .await
}
