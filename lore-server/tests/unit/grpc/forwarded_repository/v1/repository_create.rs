// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1::RepositoryCreateRequest;
use lore_revision::lore::RepositoryId;
use lore_server::authnz::repository_registry::RepositoryRegistry;
use lore_server::grpc::forwarded_repository::v1::repository_create::*;
use lore_server::grpc::forwarded_requests::CallerContext;
use lore_server::hooks::HookDispatcher;
use lore_telemetry::InstrumentProvider;
use rand::random;
use tonic::Request;

use crate::store::test_support::test_store_create;

struct TestInstrumentProvider;

impl InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
}

fn make_body(repository_id: RepositoryId, name: &str) -> RepositoryCreateRequest {
    let id_bytes: Context = repository_id.into();
    RepositoryCreateRequest {
        id: bytes::Bytes::from(id_bytes),
        name: name.into(),
        description: String::new(),
        default_branch_id: bytes::Bytes::from(Context::from(uuid::Uuid::now_v7())),
        default_branch_name: "main".into(),
        creator: Some("alice".into()),
    }
}

fn make_forwarded_request(
    repository_id: RepositoryId,
    name: &str,
) -> Request<RepositoryCreateRequest> {
    CallerContext {
        repository_id,
        user_id: "alice".into(),
        is_service_account: false,
        correlation_id: String::new(),
        authorization: None,
    }
    .to_forwarded_request(make_body(repository_id, name))
    .expect("CallerContext::to_forwarded_request failed in test")
}

#[tokio::test]
async fn missing_user_id_returns_internal_error() {
    let repository_id = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        // Deliberately omit on-behalf-of-user-id to test the missing-field error
        let request = Request::new(make_body(repository_id, "my-repo"));

        let hook_dispatcher = HookDispatcher::empty();
        let err = handler(
            request,
            &RepositoryRegistry::Unregistered,
            immutable_store,
            mutable_store,
            &hook_dispatcher,
            &TestInstrumentProvider,
        )
        .await
        .expect_err("missing user id should fail");

        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(err.message().contains("on-behalf-of-user-id"));
    }))
    .await;
}

// Happy and unhappy paths verify that whatever the underlying
// `repository_create_implementation` returns is forwarded on correctly.
mod base_repository_create_handler {
    use super::*;

    #[tokio::test]
    async fn create_returns_full_repository_record() {
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let hook_dispatcher = HookDispatcher::empty();

            let response = handler(
                make_forwarded_request(repository_id, "my-repo"),
                &RepositoryRegistry::Unregistered,
                immutable_store,
                mutable_store,
                &hook_dispatcher,
                &TestInstrumentProvider,
            )
            .await
            .expect("Request failed");

            let repo = response
                .into_inner()
                .repository
                .expect("response should include Repository");
            assert_eq!(repo.name, "my-repo");
            assert_eq!(repo.creator, "alice");
            assert!(repo.created > 0);
            assert!(!repo.id.is_empty());
        }))
        .await;
    }

    #[tokio::test]
    async fn duplicate_id_returns_already_exists() {
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let hook_dispatcher = HookDispatcher::empty();

            handler(
                make_forwarded_request(repository_id, "my-repo"),
                &RepositoryRegistry::Unregistered,
                immutable_store.clone(),
                mutable_store.clone(),
                &hook_dispatcher,
                &TestInstrumentProvider,
            )
            .await
            .expect("first create should succeed");

            // Same id, different name — should be AlreadyExists
            let err = handler(
                make_forwarded_request(repository_id, "other-name"),
                &RepositoryRegistry::Unregistered,
                immutable_store,
                mutable_store,
                &hook_dispatcher,
                &TestInstrumentProvider,
            )
            .await
            .expect_err("duplicate id with different name should fail");
            assert_eq!(err.code(), tonic::Code::AlreadyExists);
        }))
        .await;
    }
}
