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

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::hook_error_to_status;
use crate::grpc::require_write_permission;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchDelete::handle", skip_all)]
pub async fn handler(
    request: Request<BranchDeleteRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchDeleteResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    require_write_permission(request.extensions(), repository_id)?;
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
                .map_err(hook_error_to_status)?;

            match branch::delete(repository, branch).await {
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

#[cfg(test)]
mod test {

    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchPoint;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::branch::protect;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use mockall::predicate::eq;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::hooks::HookDispatcher;
    use crate::notification::testing::MockNotificationSender;
    use crate::store::test_store_create;

    struct TestInstrumentProvider {}

    impl InstrumentProvider for TestInstrumentProvider {
        fn namespace(&self) -> &'static str {
            "test"
        }
        fn labels(&self) -> &[KeyValue] {
            &[]
        }
    }

    async fn create_test_branch(repository_context: Arc<RepositoryContext>, branch: BranchId) {
        let write_token = get_write_token();
        let main = lore_revision::branch::create(
            repository_context.clone(),
            &write_token,
            BranchId::from(uuid::Uuid::now_v7()),
            branch::DEFAULT_DEFAULT_NAME,
            branch::default_category(),
            "test-creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("Could not create main branch");

        // create a revision in main to branch from
        let state = state::State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        let state_hash = state
            .serialize(repository_context.clone(), &write_token)
            .await
            .expect("Failed to serialize state");

        let head = branch_push::push(
            repository_context.clone(),
            main,
            state_hash,
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision")
        .revision;

        lore_revision::branch::create(
            repository_context.clone(),
            &write_token,
            branch,
            "test-name",
            branch::personal_category(),
            "BranchCreator",
            12345,
            vec![BranchPoint {
                branch: main,
                revision: head,
            }],
            false,
            false,
        )
        .await
        .expect("Could not create test branch");
    }

    #[tokio::test]
    async fn sends_delete_notification_for_deleted_branch() {
        let repository = random::<RepositoryId>();
        let branch = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_deleted()
            .with(eq(repository), eq(branch))
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));

            create_test_branch(repository_context.clone(), branch).await;

            let mut request = Request::new(BranchDeleteRequest {
                branch: branch.into(),
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let hook_dispatcher = HookDispatcher::empty();
            handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("Request failed");
        }))
        .await;
    }

    #[tokio::test]
    async fn no_delete_notification_for_branch_not_exists() {
        let repository = random::<RepositoryId>();

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        // no notifications sent, so no expectations required
        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let mut request = Request::new(BranchDeleteRequest {
                branch: BranchId::from(uuid::Uuid::now_v7()).into(),
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let hook_dispatcher = HookDispatcher::empty();
            handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("Request failed");
        }))
        .await;
    }

    #[tokio::test]
    async fn no_delete_notification_for_branch_delete_errors() {
        let repository = random::<RepositoryId>();
        let branch = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        // no notifications sent, so no expectations required
        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));

            create_test_branch(repository_context.clone(), branch).await;
            // protecting the branch will prevent it from deletion
            protect(repository_context.clone(), branch)
                .await
                .expect("should protect");

            let mut request = Request::new(BranchDeleteRequest {
                branch: branch.into(),
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let hook_dispatcher = HookDispatcher::empty();
            let response = handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .unwrap_err();

            assert_eq!(response.code(), tonic::Code::FailedPrecondition);
        }))
        .await;
    }
}
