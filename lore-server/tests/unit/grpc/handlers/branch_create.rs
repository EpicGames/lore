// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod input_length_validation {
    use lore_revision::branch;
    use lore_revision::repository;
    use lore_server::grpc::handlers::branch_create::*;

    #[test]
    fn accepts_valid_input() {
        validate_create_input("my-branch", "feature", "alice").expect("valid input should pass");
    }

    #[test]
    fn accepts_name_at_max_length() {
        let name = "a".repeat(branch::MAX_NAME_LEN);
        validate_create_input(&name, "feature", "alice")
            .expect("name at exactly MAX_NAME_LEN should pass");
    }

    #[test]
    fn rejects_oversized_branch_name() {
        let long_name = "a".repeat(branch::MAX_NAME_LEN + 1);
        let err = validate_create_input(&long_name, "feature", "alice")
            .expect_err("should reject oversized name");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("Branch name exceeds maximum length"));
    }

    #[test]
    fn rejects_oversized_category() {
        let long_cat = "a".repeat(branch::MAX_NAME_LEN + 1);
        let err = validate_create_input("my-branch", &long_cat, "alice")
            .expect_err("should reject oversized category");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message()
                .contains("Branch category exceeds maximum length")
        );
    }

    #[test]
    fn rejects_oversized_creator() {
        let long_creator = "a".repeat(repository::MAX_NAME_LEN + 1);
        let err = validate_create_input("my-branch", "feature", &long_creator)
            .expect_err("should reject oversized creator");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("Creator exceeds maximum length"));
    }
}

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchCreateRequest;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_server::grpc::handlers::branch_create::*;
use lore_server::hooks::HookDispatcher;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use mockall::predicate::eq;
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

#[tokio::test]
async fn sends_created_notification_for_created_branch() {
    let repository = random::<RepositoryId>();
    let branch_context = BranchId::from(uuid::Uuid::now_v7());

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_created()
        .with(eq(repository), eq(branch_context))
        .return_once(|_, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut request = Request::new(BranchCreateRequest {
            branch: branch_context.into(),
            name: branch::DEFAULT_DEFAULT_NAME.into(),
            creator: "creator".into(),
            created: 1,
            category: "category".into(),
            stack: vec![],
            revision_deprecated: None,
            parent_deprecated: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let hook_dispatcher = HookDispatcher::empty();
        handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
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
async fn no_created_notification_for_branch_create_errors() {
    let repository = random::<RepositoryId>();
    let branch_context = BranchId::from(uuid::Uuid::now_v7());

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    // no notifications sent, so no expectations required
    let notification_sender = Arc::new(MockNotificationSender::new());
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut request = Request::new(BranchCreateRequest {
            branch: branch_context.into(),
            // an invalid branch name that will cause core branch_create
            // logic to not create the branch
            name: "".into(),
            creator: "creator".into(),
            created: 1,
            category: "category".into(),
            stack: vec![],
            revision_deprecated: None,
            parent_deprecated: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let hook_dispatcher = HookDispatcher::empty();
        let response = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            &instrument_provider,
        )
        .await
        .unwrap_err();

        assert_eq!(response.code(), tonic::Code::InvalidArgument);
    }))
    .await;
}
