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
use lore_revision::repository::RepositoryContext;
use lore_server::grpc::handlers::branch_create::*;
use lore_server::hooks::HookDispatcher;
use lore_server::hooks::HookPoint;
use lore_storage::StoreError;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use mockall::predicate::eq;
use opentelemetry::KeyValue;
use rand::random;
use tonic::Request;

use crate::notification::testing::MockNotificationSender;
use crate::store::test_support::FailingLoadStore;
use crate::store::test_support::test_store_create;

#[derive(Clone)]
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

        let hook_dispatcher = Arc::new(HookDispatcher::empty());
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

        let hook_dispatcher = Arc::new(HookDispatcher::empty());
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

/// A create that restores an archived branch, dropped while the restore reads the branch's
/// metadata, as one whose deadline passes or whose client leaves is, still restores it, notifies
/// of it and spawns its post hooks: the restore runs on in a task of its own, which notifies after
/// the name maps to the branch.
#[tokio::test]
async fn a_dropped_create_restores_the_branch() {
    let repository = random::<RepositoryId>();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let (notify, mut notified) = tokio::sync::mpsc::unbounded_channel();
    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_created()
        .with(eq(repository), eq(branch_id))
        .times(1)
        .returning(move |_, _| {
            notify
                .send(())
                .expect("the test waits for the notification");
        });
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        crate::util::test_support::create_test_branch(repository_context.clone(), branch_id).await;
        branch::delete(repository_context.clone(), branch_id)
            .await
            .expect("the branch is archived");
        let gated = crate::util::test_support::gate_metadata_loads(
            mutable_store.clone(),
            repository,
            branch_id,
            0,
        );
        let mut request = Request::new(BranchCreateRequest {
            branch: branch_id.into(),
            name: "test-name".into(),
            creator: "alice".into(),
            created: 1,
            category: "default".into(),
            stack: vec![],
            revision_deprecated: None,
            parent_deprecated: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let (hook_dispatcher, mut post_hooks) =
            crate::util::hook_recorder::post_recording_dispatcher(&[HookPoint::BranchCreate]);
        let authorizer = crate::util::test_support::allow_all_authorizer();
        crate::util::test_support::drop_at_gated_load(
            handler(
                request,
                &authorizer,
                immutable_store.clone(),
                gated.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            ),
            &gated,
        )
        .await;

        crate::util::test_support::wait_for_message(
            &mut notified,
            (),
            "the dropped create notifies of the restored branch",
        )
        .await;
        crate::util::test_support::wait_for_message(
            &mut post_hooks,
            HookPoint::BranchCreate,
            "the dropped create spawns its post hooks",
        )
        .await;
        assert_eq!(
            branch::load_name_to_id_local(repository_context, "test-name")
                .await
                .ok(),
            Some(branch_id),
            "the dropped create restores the branch"
        );
    }))
    .await;
}

/// A create whose read of the branch's metadata fails in the store fails as `INTERNAL`, a server
/// fault, rather than as the caller's invalid argument.
#[tokio::test]
async fn a_store_failure_fails_the_create_as_internal() {
    let repository = random::<RepositoryId>();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let notification_sender = Arc::new(MockNotificationSender::new());
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let (metadata_key, _key_type) = branch::mutable_key(
            lore_revision::repository::SALT_LORE,
            branch::METADATA,
            repository,
            branch_id,
        );
        let failing = FailingLoadStore::for_key(
            mutable_store.clone(),
            metadata_key,
            StoreError::internal("injected metadata failure"),
        );
        let mut request = Request::new(BranchCreateRequest {
            branch: branch_id.into(),
            name: "test-name".into(),
            creator: "alice".into(),
            created: 1,
            category: "default".into(),
            stack: vec![],
            revision_deprecated: None,
            parent_deprecated: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let hook_dispatcher = Arc::new(HookDispatcher::empty());
        let err = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store.clone(),
            failing,
            notification_sender.clone(),
            &hook_dispatcher,
            &instrument_provider,
        )
        .await
        .expect_err("the metadata read fails");

        assert_eq!(err.code(), tonic::Code::Internal, "got {err:?}");
    }))
    .await;
}
