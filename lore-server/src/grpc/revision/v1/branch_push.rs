// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1::BranchPushRequest;
use lore_proto::lore::revision::v1::BranchPushResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::REVISION;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::Level;
use tracing::debug;
use tracing::info;
use tracing::span;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_is_service_account;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::handlers::branch_push::PushAccess;
use crate::grpc::handlers::branch_push::PushResult;
use crate::grpc::handlers::branch_push::dispatch_response_message;
use crate::grpc::handlers::branch_push::extract_client_ip;
use crate::grpc::handlers::branch_push::load_branch_and_head;
use crate::grpc::handlers::branch_push::load_response_repository_metadata;
use crate::grpc::handlers::branch_push::push_loaded;
use crate::grpc::hook_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::hooks::context::METADATA_BRANCH_NAME;
use crate::hooks::context::METADATA_IS_SERVICE_ACCOUNT;
use crate::util::setup_execution;

/// `lore.revision.v1.RevisionService.BranchPush` handler.
///
/// Soft rejection (non-fast-forward without `force`/`fast_forward_merge`,
/// or fast-forward merge with conflicts) is conveyed via
/// `FailedPrecondition` with a detail message that distinguishes the
/// two cases and embeds the current branch latest. Pushing to a branch
/// id whose metadata is missing returns `NotFound`. Pushing to a
/// deleted branch reinstates the name → id mapping if the name is
/// still free, or returns `AlreadyExists` if claimed by a different
/// live branch. The pre-handlers run between reading the branch and
/// reinstating its mapping, so a rejected push writes nothing.
///
/// A fragment of the pushed revision the server does not hold is the
/// other `FailedPrecondition`, and the only one carrying an address in
/// the status details. `NotFound` names an absent branch alone, so a
/// caller reads it as one without inspecting the status further.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "BranchPush::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchPushRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchPushResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let is_service_account = get_is_service_account(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let repository_id = get_repository(request.metadata())?;

    let access = PushAccess::of(&authorizer, request.extensions(), repository_id).await?;

    let client_ip: Option<String> = extract_client_ip(&request).map(|ip| ip.to_string());
    let req = request.into_inner();
    let branch_id = BranchId::from(req.id);
    let revision = Hash::from(req.revision_signature);
    let force = req.force;
    let fast_forward_merge = req.fast_forward_merge;

    if revision.is_zero() {
        info!("Invalid branch push request, revision_signature is zero");
        return Err(Status::invalid_argument(
            "revision_signature must be non-zero",
        ));
    }

    debug!(
        {REVISION} = %revision,
        ?access,
        {BRANCH_ID} = %branch_id,
        force,
        fast_forward_merge,
        "Handling branch push request",
    );

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));
    let repository_id: RepositoryId = repository.id;
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(execution, async move {
            let (branch_metadata, loaded) =
                load_branch_and_head(repository.clone(), branch_id, revision).await;
            let branch_metadata = branch_metadata?;

            let mut ctx_builder = HookContext::builder()
                .correlation_id(correlation_id.clone())
                .hook_point(HookPoint::BranchPush)
                .repository(repository_id)
                .user(user_id.clone())
                .branch(branch_id)
                .revision(revision)
                .metadata(
                    METADATA_BRANCH_NAME,
                    branch::name(&branch_metadata).unwrap_or_default(),
                )
                .metadata(METADATA_IS_SERVICE_ACCOUNT, is_service_account.to_string());

            if let Some(ip) = client_ip {
                ctx_builder = ctx_builder.metadata("client_ip", ip);
            }

            let mut hook_ctx = ctx_builder.build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchPush, &hook_ctx)
                .await
                .map_err(hook_error_to_status)?;

            let (pushed, repository_metadata) = tokio::join!(
                push_loaded(
                    repository.clone(),
                    branch_id,
                    revision,
                    access,
                    force,
                    fast_forward_merge,
                    history_step_size,
                    acceleration,
                    branch_metadata,
                    loaded,
                ),
                load_response_repository_metadata(hook_dispatcher, repository.clone()),
            );
            let PushResult {
                success,
                fast_forward_merged,
                revision: resulting_revision,
                revision_number,
                branch_name,
            } = pushed?;

            instrument_provider
                .counter("num_branches_pushed")
                .add(1, &[]);

            if !success {
                let detail = if fast_forward_merge {
                    format!("Fast-forward merge has conflicts; branch latest: {resulting_revision}")
                } else {
                    format!(
                        "Branch push is not a fast-forward; branch latest: {resulting_revision}"
                    )
                };
                debug!(
                    {BRANCH_ID} = %branch_id,
                    branch_latest = %resulting_revision,
                    fast_forward_merge,
                    "Branch push rejected",
                );
                return Err(Status::failed_precondition(detail));
            }

            lore_spawn!({
                let user_id = user_id.clone();
                async move {
                    notification
                        .branch_pushed(
                            repository_id,
                            branch_id,
                            &user_id,
                            resulting_revision,
                            revision_number,
                        )
                        .instrument(span!(Level::DEBUG, "publish_notification"))
                        .await;
                }
                .in_current_span()
            });

            hook_ctx.set_revision_number(revision_number);
            hook_dispatcher.spawn_post(HookPoint::BranchPush, hook_ctx);

            let message = dispatch_response_message(
                hook_dispatcher,
                &correlation_id,
                &user_id,
                repository_id,
                branch_id,
                &branch_name,
                resulting_revision,
                repository_metadata.as_ref(),
            );

            debug!(
                {BRANCH_ID} = %branch_id,
                {REVISION} = %resulting_revision,
                revision_number,
                fast_forward_merged,
                "Branch push response",
            );

            Ok(Response::new(BranchPushResponse {
                revision_signature: resulting_revision.into(),
                revision_number,
                fast_forward_merged,
                message,
            }))
        })
        .await
}
