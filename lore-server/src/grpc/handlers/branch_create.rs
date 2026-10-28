// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_proto::BranchCreateRequest;
use lore_proto::BranchCreateResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::grpc::require_action;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

// Reject oversized string fields early to prevent resource exhaustion.
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

// Branch existence checks and write ordering are implemented in
// urc-core/src/branch.rs::create(). See the documentation there for the
// full decision matrix. The query handler in branch_query.rs uses the
// same existence model.

#[tracing::instrument(name = "BranchCreate::handle", skip_all)]
pub async fn handler(
    request: Request<BranchCreateRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    require_action(authorizer, &request, Action::Write).await?;
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let branch = BranchId::from(req.branch);
    let category = req.category;
    let name = req.name;
    let creator = req.creator;
    let created = req.created;

    let mut stack = req.stack;
    if stack.is_empty()
        && let Some(branch) = req.parent_deprecated
        && let Some(revision) = req.revision_deprecated
    {
        stack.push(lore_proto::BranchPoint { branch, revision });
    }

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
                .hook_point(HookPoint::BranchCreate)
                .repository(repository_id)
                .user(user_id)
                .branch(branch)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchCreate, &hook_ctx)
                .await
                .map_err(hook_error_to_status)?;

            validate_create_input(&name, &category, &creator)?;

            let stack: Vec<BranchPoint> = stack.into_iter().map(BranchPoint::from).collect();
            debug!({BRANCH_ID} = %branch, branch_name = name, stack = ?stack, "Creating branch");

            let write_token = get_write_token();
            let branch = lore_revision::branch::create(
                repository.clone(),
                &write_token,
                branch,
                name.as_str(),
                category.as_str(),
                creator.as_str(),
                created,
                stack,
                false,
                false, /* Do not create linked repository branches */
            )
            .await
            .filter_slow_down()?
            .map_err(|err| Status::invalid_argument(err.to_string()))?;

            notification_sender
                .branch_created(repository_id, branch)
                .await;

            hook_dispatcher.spawn_post(HookPoint::BranchCreate, hook_ctx);

            let revision = branch::load_latest(repository, branch)
                .await
                // no filter_slow_down()? usage here: the branch is already
                // created, so a retryable status would invite a retry that can
                // only fail with BranchAlreadyExists.
                .unwrap_or_default();

            debug!("Created branch {name} ({branch}) at revision {revision}");
            let num_branches_created = instrument_provider.counter("num_branches_created");
            num_branches_created.add(1, &[]);

            Ok(Response::new(BranchCreateResponse {
                revision: revision.into(),
            }))
        })
        .await
}
