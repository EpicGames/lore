// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::revision::v1::BranchDeleteRequest;
use lore_proto::lore::revision::v1::BranchDeleteResponse;
use lore_revision::branch;
use lore_revision::branch::protect;
use lore_revision::instance::store_current_anchor_branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_server::grpc::get_write_token;
use lore_server::grpc::revision::v1::branch_delete::*;
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
use crate::util::hook_recorder::Seen;
use crate::util::hook_recorder::post_recording_dispatcher;
use crate::util::hook_recorder::recording_dispatcher;
use crate::util::test_support::create_test_branch;
use crate::util::test_support::drop_at_gated_load;
use crate::util::test_support::gate_metadata_loads;
use crate::util::test_support::stop_restore_after_claim;
use crate::util::test_support::wait_for_message;

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

/// Creates a single root-style branch (empty stack) — useful for
/// exercising the default-branch precondition path in `branch::delete`.
async fn create_root_branch(repository_context: Arc<RepositoryContext>, branch: BranchId) {
    let write_token = get_write_token();
    lore_revision::branch::create(
        repository_context,
        &write_token,
        branch,
        "root-branch",
        branch::default_category(),
        "test-creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("Could not create root branch");
}

fn make_request(repository: RepositoryId, branch: BranchId) -> Request<BranchDeleteRequest> {
    let mut request = Request::new(BranchDeleteRequest { id: branch.into() });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

mod direct_handling {
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

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let latest = create_test_branch(repository_context, branch_id).await;

            let hook_dispatcher = Arc::new(HookDispatcher::empty());
            let response = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
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
            assert_eq!(branch.category, branch::personal_category());
            assert_eq!(branch.created, 12345);
            assert!(!branch.id.is_empty());
            assert!(!branch.metadata.is_empty());
            assert_eq!(branch.latest, bytes::Bytes::from(latest));
            assert_eq!(branch.stack.len(), 1);
        }))
        .await;
    }

    /// A delete of a branch whose restore stopped after it claimed the name, as a server that
    /// stops there leaves it, settles the restore: it succeeds as a delete does, notifying once,
    /// and leaves the name unmapped and the metadata without the restore's record.
    #[tokio::test]
    async fn delete_settles_a_restore_that_stopped_after_its_claim() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_deleted()
            .times(1)
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context.clone(), branch_id).await;
            branch::delete(repository_context.clone(), branch_id)
                .await
                .expect("the branch is archived");
            stop_restore_after_claim(&repository_context, branch_id, "test-name").await;
            assert_eq!(
                branch::load_name_to_id_local(repository_context.clone(), "test-name")
                    .await
                    .ok(),
                Some(branch_id)
            );

            let response = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &Arc::new(HookDispatcher::empty()),
                &instrument_provider,
            )
            .await
            .expect("the delete settles the restore");

            assert!(response.into_inner().branch.expect("a branch").deleted);
            let mapping =
                branch::load_name_to_id_local(repository_context.clone(), "test-name").await;
            assert!(
                mapping.as_ref().is_err_and(|err| err.is_branch_not_found()),
                "got {mapping:?}"
            );
            let metadata = branch::metadata(repository_context, branch_id)
                .await
                .expect("the archived branch keeps its metadata");
            assert!(metadata.get_string(branch::RESTORING).is_err());
        }))
        .await;
    }

    /// A delete that settles a restore stopped after its claim, dropped while it reads the
    /// branch's metadata, as one whose deadline passes or whose client leaves is, still settles
    /// it, notifies of the deleted branch and spawns its post hooks: the delete runs on in a task of
    /// its own, which notifies after the restore's name maps to nothing. The handler's own read of
    /// the metadata, before the delete, goes through.
    #[tokio::test]
    async fn a_dropped_delete_settles_a_stopped_restore() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let (notify, mut notified) = tokio::sync::mpsc::unbounded_channel();
        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_deleted()
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
            create_test_branch(repository_context.clone(), branch_id).await;
            branch::delete(repository_context.clone(), branch_id)
                .await
                .expect("the branch is archived");
            stop_restore_after_claim(&repository_context, branch_id, "stopped-name").await;
            let gated = gate_metadata_loads(mutable_store.clone(), repository, branch_id, 1);

            let (hook_dispatcher, mut post_hooks) =
                post_recording_dispatcher(&[HookPoint::BranchDelete]);
            let authorizer = crate::util::test_support::allow_all_authorizer();
            drop_at_gated_load(
                handler(
                    make_request(repository, branch_id),
                    &authorizer,
                    immutable_store.clone(),
                    gated.clone(),
                    notification_sender.clone(),
                    &None, /* no forwarded requests */
                    &hook_dispatcher,
                    &instrument_provider,
                ),
                &gated,
            )
            .await;

            wait_for_message(
                &mut notified,
                (),
                "the dropped delete notifies of the deleted branch",
            )
            .await;
            wait_for_message(
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

    #[tokio::test]
    async fn delete_is_idempotent_on_already_deleted() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        // Notification fires once, on the first (real) delete only.
        // `return_once` is also a negative assertion: a second call would
        // panic (the closure is already consumed), proving the idempotent
        // path skips the notification.
        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_deleted()
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context, branch_id).await;

            let hook_dispatcher = Arc::new(HookDispatcher::empty());
            let first = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("first delete should succeed");
            assert!(first.into_inner().branch.unwrap().deleted);

            let second = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("second delete should succeed (idempotent)");
            let branch = second.into_inner().branch.unwrap();
            assert!(branch.deleted);
            assert_eq!(branch.name, "test-name");
        }))
        .await;
    }

    #[tokio::test]
    async fn delete_unknown_branch_returns_not_found() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let unknown = BranchId::from(uuid::Uuid::now_v7());

            let hook_dispatcher = Arc::new(HookDispatcher::empty());
            let err = handler(
                make_request(repository, unknown),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("delete on unknown branch should fail");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn delete_default_branch_returns_failed_precondition() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // Branches with empty stack are treated as the default branch.
            create_root_branch(repository_context, branch_id).await;

            let hook_dispatcher = Arc::new(HookDispatcher::empty());
            let err = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("default branch delete should fail");
            assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        }))
        .await;
    }

    #[tokio::test]
    async fn delete_current_branch_returns_failed_precondition() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context.clone(), branch_id).await;
            // Pin the test branch as the current anchor to trigger
            // BranchError::DeleteCurrent.
            store_current_anchor_branch(&repository_context, branch_id)
                .await
                .expect("should set current anchor");

            let hook_dispatcher = Arc::new(HookDispatcher::empty());
            let err = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("current-branch delete should fail");
            assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        }))
        .await;
    }

    #[tokio::test]
    async fn delete_protected_branch_returns_failed_precondition() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context.clone(), branch_id).await;
            protect(repository_context, branch_id)
                .await
                .expect("should protect");

            let hook_dispatcher = Arc::new(HookDispatcher::empty());
            let err = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("protected delete should fail");
            assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        }))
        .await;
    }

    #[tokio::test]
    async fn pre_hook_sees_branch_name_and_service_account() {
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

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context, branch_id).await;

            let (hook_dispatcher, seen) = recording_dispatcher(false);
            handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("delete should succeed");

            assert_eq!(
                *seen.lock(),
                vec![Seen {
                    branch_name: Some("test-name".into()),
                    is_service_account: Some("false".into()),
                }]
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn rejected_delete_keeps_branch() {
        let repository = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context.clone(), branch_id).await;

            let (hook_dispatcher, _seen) = recording_dispatcher(true);
            let err = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &None, /* no forwarded requests */
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("a rejected delete should fail");

            assert_eq!(err.code(), tonic::Code::PermissionDenied);
            let mapped = branch::load_name_to_id_local(repository_context, "test-name")
                .await
                .expect("the branch name should still be mapped");
            assert_eq!(BranchId::from(mapped), branch_id);
        }))
        .await;
    }
}

mod forwarded_request {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lore_server::grpc::forwarded_requests::ForwardedRequestResult;
    use lore_server::grpc::forwarded_requests::ForwardedRequests;
    use lore_server::grpc::forwarded_requests::InternalClientError;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use lore_server::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
    use tonic::Response;
    use tonic::Status;

    use super::*;

    /// Single-use client that returns a pre-configured result on its one call.
    struct SingleShotClient {
        response: Arc<Mutex<Option<ForwardedRequestResult<BranchDeleteResponse>>>>,
    }

    #[async_trait]
    impl ForwardedRevisionServiceClient for SingleShotClient {
        async fn branch_create(
            &mut self,
            _request: Request<lore_proto::lore::revision::v1::BranchCreateRequest>,
        ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchCreateResponse> {
            unreachable!("branch_create should not be called in branch_delete tests")
        }

        async fn branch_delete(
            &mut self,
            _request: Request<BranchDeleteRequest>,
        ) -> ForwardedRequestResult<BranchDeleteResponse> {
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("branch_delete called more than once")
        }

        async fn branch_get(
            &mut self,
            _request: Request<lore_proto::lore::revision::v1::BranchGetRequest>,
        ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchGetResponse> {
            unreachable!("branch_get should not be called in branch_delete tests")
        }

        async fn branch_list(
            &mut self,
            _request: Request<lore_proto::lore::revision::v1::BranchListRequest>,
        ) -> ForwardedRequestResult<lore_server::grpc::revision::v1::branch_list::BranchListStream>
        {
            unreachable!("branch_list should not be called in branch_delete tests")
        }
    }

    struct StubForwardedRequests {
        flags: RpcFlags,
        response: Arc<Mutex<Option<ForwardedRequestResult<BranchDeleteResponse>>>>,
    }

    impl StubForwardedRequests {
        fn forwarding_enabled(response: ForwardedRequestResult<BranchDeleteResponse>) -> Arc<Self> {
            Arc::new(Self {
                flags: RpcFlags {
                    revision_branch_delete: true,
                    ..Default::default()
                },
                response: Arc::new(Mutex::new(Some(response))),
            })
        }

        fn forwarding_disabled(
            response: ForwardedRequestResult<BranchDeleteResponse>,
        ) -> Arc<Self> {
            Arc::new(Self {
                flags: RpcFlags {
                    revision_branch_delete: false,
                    ..Default::default()
                },
                response: Arc::new(Mutex::new(Some(response))),
            })
        }
    }

    impl ForwardedRequests for StubForwardedRequests {
        fn rpc_flags(&self) -> &RpcFlags {
            &self.flags
        }

        fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
            Box::new(SingleShotClient {
                response: Arc::clone(&self.response),
            })
        }

        fn forwarded_repository_service(
            &self,
        ) -> Box<dyn lore_server::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient>
{
            unreachable!("forwarded_repository_service should not be called in branch_create tests")
        }

        fn forwarded_lock_service(
            &self,
        ) -> Box<dyn lore_server::grpc::forwarded_requests::lock_service::ForwardedLockServiceClient>
        {
            unreachable!("forwarded_lock_service should not be called in branch_delete tests")
        }
    }

    #[tokio::test]
    async fn delegates_to_remote_and_returns_response() {
        // When the flag is enabled the other server's response is returned directly;
        // branch_delete_implementation is NOT called so the local store is untouched.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let notification_sender = Arc::new(MockNotificationSender::new()); // no branch_deleted expected
        let instrument_provider = TestInstrumentProvider {};

        let deleted_branch = lore_proto::lore::model::v1::Branch {
            name: "test-name".into(),
            deleted: true,
            ..Default::default()
        };
        let branch_response = Ok(Ok(Response::new(BranchDeleteResponse {
            branch: Some(deleted_branch),
        })));
        let forwarded_requests = StubForwardedRequests::forwarding_enabled(branch_response);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let hook_dispatcher = Arc::new(HookDispatcher::empty());

            let response = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store,
                mutable_store,
                notification_sender,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("should succeed");

            let branch = response
                .into_inner()
                .branch
                .expect("response should include Branch");
            assert_eq!(branch.name, "test-name");
            assert!(branch.deleted);
        }))
        .await;
    }

    #[tokio::test]
    async fn error_status_returned_to_caller() {
        // An error status from the forwarded server is forwarded directly to the original caller.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        let forwarded_request_result = Ok(Err(Status::not_found("test error forwarded")));
        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(forwarded_request_result);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let hook_dispatcher = Arc::new(HookDispatcher::empty());

            let err = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store,
                mutable_store,
                notification_sender,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("forwarded error should propagate");

            assert_eq!(err.code(), tonic::Code::NotFound);
            assert!(err.message().contains("test error forwarded"));
        }))
        .await;
    }

    #[tokio::test]
    async fn internal_client_error_maps_to_internal_status() {
        // A transport-level failure (InternalClientError) is mapped to Status::internal.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(Err(InternalClientError::internal("oops")));

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let hook_dispatcher = Arc::new(HookDispatcher::empty());

            let err = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store,
                mutable_store,
                notification_sender,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("transport error should become internal status");

            assert_eq!(err.code(), tonic::Code::Internal);
            assert!(err.message().contains("Error making forwarded request"));
        }))
        .await;
    }

    #[tokio::test]
    async fn flag_disabled_falls_through_to_local_execution() {
        // When revision_branch_delete is false the local path runs, even if a
        // ForwardedRequests is present. The stub client is not called.
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

        // response is irrelevant — client must never be called
        let forwarded_result = Ok(Err(Status::internal("should not be called")));
        let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            create_test_branch(repository_context, branch_id).await;

            let hook_dispatcher = Arc::new(HookDispatcher::empty());

            let response = handler(
                make_request(repository, branch_id),
                &crate::util::test_support::allow_all_authorizer(),
                immutable_store,
                mutable_store,
                notification_sender,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("local execution should succeed");

            let branch = response
                .into_inner()
                .branch
                .expect("response should include Branch");
            assert!(branch.deleted);
            assert_eq!(branch.name, "test-name");
        }))
        .await;
    }
}
