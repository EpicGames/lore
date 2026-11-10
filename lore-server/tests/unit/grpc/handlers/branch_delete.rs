// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_proto::BranchDeleteRequest;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::branch::protect;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_delete::*;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::handlers::branch_push::PushHandler;
use lore_server::grpc::handlers::branch_push::PushReport;
use lore_server::hooks::HookDispatcher;
use lore_server::hooks::HookPoint;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use mockall::predicate::eq;
use opentelemetry::KeyValue;
use rand::random;
use tonic::Request;

use crate::notification::testing::MockNotificationSender;
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
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
        &mut PushReport::new(PushHandler::Legacy, Default::default()),
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

        assert_eq!(response.code(), tonic::Code::FailedPrecondition);
    }))
    .await;
}

/// A delete that settles a restore stopped after its claim, dropped while it reads the branch's
/// metadata, as one whose deadline passes or whose client leaves is, still settles it, notifies of
/// the deleted branch and spawns its post hooks: the delete runs on in a task of its own, which
/// notifies after the restore's name maps to nothing.
#[tokio::test]
async fn a_dropped_delete_settles_a_stopped_restore() {
    let repository = random::<RepositoryId>();
    let branch = BranchId::from(uuid::Uuid::now_v7());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let (notify, mut notified) = tokio::sync::mpsc::unbounded_channel();
    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_deleted()
        .with(eq(repository), eq(branch))
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
        create_test_branch(repository_context.clone(), branch).await;
        branch::delete(repository_context.clone(), branch)
            .await
            .expect("the branch is archived");
        crate::util::test_support::stop_restore_after_claim(
            &repository_context,
            branch,
            "stopped-name",
        )
        .await;
        let gated = crate::util::test_support::gate_metadata_loads(
            mutable_store.clone(),
            repository,
            branch,
            0,
        );
        let mut request = Request::new(BranchDeleteRequest {
            branch: branch.into(),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let (hook_dispatcher, mut post_hooks) =
            crate::util::hook_recorder::post_recording_dispatcher(&[HookPoint::BranchDelete]);
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
            "the dropped delete notifies of the deleted branch",
        )
        .await;
        crate::util::test_support::wait_for_message(
            &mut post_hooks,
            HookPoint::BranchDelete,
            "the dropped delete spawns its post hooks",
        )
        .await;
        assert_eq!(
            branch::load_name_to_id_local(repository_context, "stopped-name")
                .await
                .ok(),
            None,
            "the dropped delete settles the stopped restore"
        );
    }))
    .await;
}
