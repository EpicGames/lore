// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1::BranchDeleteRequest;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_server::grpc::forwarded_requests::CallerContext;
use lore_server::grpc::forwarded_revision::v1::branch_delete::*;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::hooks::HookDispatcher;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use opentelemetry::KeyValue;
use rand::random;
use tonic::Request;

use crate::notification::testing::MockNotificationSender;
use crate::store::test_support::test_store_create;

struct TestInstrumentProvider {}

impl InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
    fn labels(&self) -> &[KeyValue] {
        &[]
    }
}

async fn create_test_branch(repository_context: Arc<RepositoryContext>, branch: BranchId) -> Hash {
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

    let state = lore_revision::state::State::new();
    state.set_parent_self(Hash::default());
    state.set_revision_number(1);
    let state_hash = state
        .serialize(repository_context.clone(), &write_token)
        .await
        .expect("Failed to serialize state");

    let latest = branch_push::push(
        repository_context.clone(),
        main,
        state_hash,
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
    )
    .await
    .expect("Failed to push latest revision")
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
            revision: latest,
        }],
        false,
        false,
    )
    .await
    .expect("Could not create test branch");

    latest
}

fn make_forwarded_request(
    repository: RepositoryId,
    branch_id: BranchId,
) -> Request<BranchDeleteRequest> {
    CallerContext {
        repository_id: repository,
        user_id: "alice".into(),
        is_service_account: false,
        correlation_id: String::new(),
        authorization: None,
    }
    .to_forwarded_request(BranchDeleteRequest {
        id: branch_id.into(),
    })
    .expect("CallerContext::to_forwarded_request failed in test")
}

#[tokio::test]
async fn missing_user_id_returns_internal_error() {
    let repository = random::<RepositoryId>();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let notification_sender = Arc::new(MockNotificationSender::new());
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        // No on-behalf-of-user-id in metadata
        let mut request = Request::new(BranchDeleteRequest {
            id: branch_id.into(),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let hook_dispatcher = HookDispatcher::empty();
        let err = handler(
            request,
            immutable_store,
            mutable_store,
            notification_sender,
            &hook_dispatcher,
            &instrument_provider,
        )
        .await
        .expect_err("missing user id should fail");

        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(err.message().contains("on-behalf-of-user-id"));
    }))
    .await;
}

// Happy and unhappy paths verify that whatever the underlying
// `branch_delete_implementation` returns is forwarded on correctly.
mod base_branch_delete_handler {
    use super::*;

    #[tokio::test]
    async fn delete_returns_deleted_branch_record() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_deleted()
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context, branch_id).await;

            let hook_dispatcher = HookDispatcher::empty();
            let response = handler(
                make_forwarded_request(repository, branch_id),
                immutable_store,
                mutable_store,
                notification_sender,
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("Request failed");

            let branch = response
                .into_inner()
                .branch
                .expect("response should include Branch");
            assert!(branch.deleted);
            assert_eq!(branch.name, "test-name");
            assert_eq!(branch.creator, "BranchCreator");
        }))
        .await;
    }

    #[tokio::test]
    async fn delete_unknown_branch_returns_not_found() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let hook_dispatcher = HookDispatcher::empty();
            let err = handler(
                make_forwarded_request(repository, branch_id),
                immutable_store,
                mutable_store,
                notification_sender,
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("unknown branch should fail");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }
}
