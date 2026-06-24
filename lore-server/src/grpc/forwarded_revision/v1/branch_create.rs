// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_proto::lore::revision::v1::BranchCreateRequest;
use lore_proto::lore::revision::v1::BranchCreateResponse;
use lore_revision::notification::NotificationSender;
use lore_telemetry::InstrumentProvider;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::revision::v1::branch_create::branch_create_implementation;
use crate::hooks::HookDispatcher;

/// Handler that takes a `BranchCreate` request forwarded on from peer's `RevisionService`
/// and executes it, returning the result to the other server for forwarding on to its
/// client
#[tracing::instrument(name = "ForwardedRevision::v1::BranchCreate::Handler", skip_all)]
pub async fn handler(
    request: Request<BranchCreateRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;

    branch_create_implementation(
        request.into_inner(),
        caller_context,
        immutable_store,
        mutable_store,
        notification_sender,
        hook_dispatcher,
        instrument_provider,
    )
    .await
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_proto::lore::revision::v1::BranchCreateRequest;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_telemetry::InstrumentProvider;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
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

    fn make_forwarded_request(
        repository: RepositoryId,
        branch_id: BranchId,
        name: &str,
    ) -> Request<BranchCreateRequest> {
        let mut request = Request::new(BranchCreateRequest {
            id: branch_id.into(),
            name: name.into(),
            creator: Some("alice".into()),
            category: "default".into(),
            stack: vec![],
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
            .metadata_mut()
            .insert("on-behalf-of-user-id", "alice".parse().unwrap());
        request
    }

    #[tokio::test]
    async fn missing_user_id_returns_internal_error() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            // No on-behalf-of-user-id set in metadata
            let mut request = Request::new(BranchCreateRequest {
                id: BranchId::from(uuid::Uuid::now_v7()).into(),
                name: "main".into(),
                creator: Some("alice".into()),
                category: "default".into(),
                stack: vec![],
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

    // Happy and Unhappy paths that whatever the original `branch_create` implementation
    // returns is forwarded on to the forwarded handler
    mod base_branch_create_handler {
        use super::*;

        #[tokio::test]
        async fn create_returns_full_branch_record() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let mut notification_sender = MockNotificationSender::new();
            notification_sender
                .expect_branch_created()
                .return_once(|_, _| ());
            let notification_sender = Arc::new(notification_sender);
            let instrument_provider = TestInstrumentProvider {};

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());
                let hook_dispatcher = HookDispatcher::empty();

                let response = handler(
                    make_forwarded_request(repository, branch_id, "main"),
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
                assert_eq!(branch.name, "main");
                assert_eq!(branch.creator, "alice");
                assert_eq!(branch.category, "default");
                assert!(!branch.deleted);
                assert!(branch.created > 0);
                assert!(!branch.id.is_empty());
                assert!(!branch.metadata.is_empty());
            }))
            .await;
        }

        #[tokio::test]
        async fn duplicate_id_returns_already_exists() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let mut notification_sender = MockNotificationSender::new();
            notification_sender
                .expect_branch_created()
                .return_once(|_, _| ());
            let notification_sender = Arc::new(notification_sender);
            let instrument_provider = TestInstrumentProvider {};

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());
                let hook_dispatcher = HookDispatcher::empty();

                handler(
                    make_forwarded_request(repository, branch_id, "main"),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    notification_sender.clone(),
                    &hook_dispatcher,
                    &instrument_provider,
                )
                .await
                .expect("first create should succeed");

                let err = handler(
                    make_forwarded_request(repository, branch_id, "main"),
                    immutable_store,
                    mutable_store,
                    notification_sender,
                    &hook_dispatcher,
                    &instrument_provider,
                )
                .await
                .expect_err("duplicate id should fail");

                assert_eq!(err.code(), tonic::Code::AlreadyExists);
            }))
            .await;
        }
    }
}
