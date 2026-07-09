// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::revision::v1::BranchDeleteRequest;
use lore_proto::lore::revision::v1::BranchDeleteResponse;
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

use super::branch_record::build_branch;
use crate::grpc::ServerResultExt;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::hook_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

/// `lore.revision.v1.RevisionService.BranchDelete` handler.
///
/// Returns the full deleted `Branch` record. Idempotent on
/// already-deleted branches — repeated calls succeed with the same
/// record. Branches that never existed return `NotFound`.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRevisionService`
#[tracing::instrument(name = "BranchDelete::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchDeleteRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchDeleteResponse>, Status> {
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().revision_branch_delete
    {
        forward_branch_delete(req, caller_context, forwarded_requests).await
    } else {
        branch_delete_implementation(
            req,
            caller_context,
            immutable_store,
            mutable_store,
            notification_sender,
            hook_dispatcher,
            instrument_provider,
        )
        .await
    }
}

/// This `BranchDeleteRequest` should be handled by another server
/// and the response forwarded on to the client
async fn forward_branch_delete(
    req: BranchDeleteRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<BranchDeleteResponse>, Status> {
    let mut client = forwarded_requests.forwarded_revision_service();
    let request = context.to_forwarded_request(req)?;

    let branch_delete_result = client
        .branch_delete(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = branch_delete_result?;
    Ok(response)
}

/// This `BranchDeleteRequest` should be fulfilled by this server.
pub async fn branch_delete_implementation(
    req: BranchDeleteRequest,
    caller_context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchDeleteResponse>, Status> {
    let repository_id = caller_context.repository_id;
    let user_id = caller_context.user_id;
    let correlation_id = caller_context.correlation_id;
    let branch_id = BranchId::from(req.id);

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
                .branch(branch_id)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchDelete, &hook_ctx)
                .map_err(hook_error_to_status)?;

            // Load before delete so the idempotent already-deleted path
            // can still build the response from the preserved metadata.
            let pre_metadata = branch::metadata(repository.clone(), branch_id)
                .await
                .map_err(|_err| Status::not_found(format!("Branch {branch_id} not found")))?;

            debug!({BRANCH_ID} = %branch_id, "Deleting branch");

            let delete_result = branch::delete(repository.clone(), branch_id).await;
            let actually_deleted = match delete_result {
                Ok(()) => true,
                Err(err) if err.is_branch_not_found() => {
                    info!({BRANCH_ID} = %branch_id, "Branch already deleted");
                    false
                }
                Err(err) if err.is_delete_protected() => {
                    info!({BRANCH_ID} = %branch_id, "Branch is delete-protected");
                    return Err(Status::failed_precondition("Branch is delete protected"));
                }
                Err(err) if err.is_delete_current() => {
                    info!({BRANCH_ID} = %branch_id, "Cannot delete currently-checked-out branch");
                    return Err(Status::failed_precondition(
                        "Branch is currently checked out",
                    ));
                }
                Err(err) if err.is_delete_default() => {
                    info!({BRANCH_ID} = %branch_id, "Cannot delete default branch");
                    return Err(Status::failed_precondition("Branch is the default branch"));
                }
                Err(err) => {
                    warn!({BRANCH_ID} = %branch_id, error = ?err, "Failed to delete branch");
                    return Err(Status::internal(err.to_string()));
                }
            };

            if actually_deleted {
                debug!({BRANCH_ID} = %branch_id, "Branch deleted");
                instrument_provider
                    .counter("num_branches_deleted")
                    .add(1, &[]);
                notification_sender
                    .branch_deleted(repository_id, branch_id)
                    .await;
                hook_dispatcher.spawn_post(HookPoint::BranchDelete, hook_ctx);
            }

            let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;

            let response_branch =
                build_branch(repository, branch_id, &pre_metadata, metadata_hash, true).await?;

            Ok(Response::new(BranchDeleteResponse {
                branch: Some(response_branch),
            }))
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
    use lore_revision::instance::store_current_anchor_branch;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
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

    /// Returns the latest revision the test branch was forked at, so
    /// callers can assert against the resulting `latest` field.
    async fn create_test_branch(
        repository_context: Arc<RepositoryContext>,
        branch: BranchId,
    ) -> Hash {
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

        let state = state::State::new();
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
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
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

                let hook_dispatcher = HookDispatcher::empty();
                let response = handler(
                    make_request(repository, branch_id),
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

                let hook_dispatcher = HookDispatcher::empty();
                let first = handler(
                    make_request(repository, branch_id),
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

                let hook_dispatcher = HookDispatcher::empty();
                let err = handler(
                    make_request(repository, unknown),
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

                let hook_dispatcher = HookDispatcher::empty();
                let err = handler(
                    make_request(repository, branch_id),
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

                let hook_dispatcher = HookDispatcher::empty();
                let err = handler(
                    make_request(repository, branch_id),
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

                let hook_dispatcher = HookDispatcher::empty();
                let err = handler(
                    make_request(repository, branch_id),
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
    }

    mod forwarded_request {
        use std::sync::Mutex;

        use async_trait::async_trait;
        use tonic::Response;
        use tonic::Status;

        use super::*;
        use crate::grpc::forwarded_requests::ForwardedRequestResult;
        use crate::grpc::forwarded_requests::ForwardedRequests;
        use crate::grpc::forwarded_requests::InternalClientError;
        use crate::grpc::forwarded_requests::RpcFlags;
        use crate::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;

        /// Single-use client that returns a pre-configured result on its one call.
        struct SingleShotClient {
            response: Arc<Mutex<Option<ForwardedRequestResult<BranchDeleteResponse>>>>,
        }

        #[async_trait]
        impl ForwardedRevisionServiceClient for SingleShotClient {
            async fn branch_create(
                &mut self,
                _request: Request<lore_proto::lore::revision::v1::BranchCreateRequest>,
            ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchCreateResponse>
            {
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
            ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchGetResponse>
            {
                unreachable!("branch_get should not be called in branch_delete tests")
            }
        }

        struct StubForwardedRequests {
            flags: RpcFlags,
            response: Arc<Mutex<Option<ForwardedRequestResult<BranchDeleteResponse>>>>,
        }

        impl StubForwardedRequests {
            fn forwarding_enabled(
                response: ForwardedRequestResult<BranchDeleteResponse>,
            ) -> Arc<Self> {
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
                let hook_dispatcher = HookDispatcher::empty();

                let response = handler(
                    make_request(repository, branch_id),
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
                let hook_dispatcher = HookDispatcher::empty();

                let err = handler(
                    make_request(repository, branch_id),
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

            let forwarded_requests = StubForwardedRequests::forwarding_enabled(Err(
                InternalClientError::internal("oops"),
            ));

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());
                let hook_dispatcher = HookDispatcher::empty();

                let err = handler(
                    make_request(repository, branch_id),
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

                let hook_dispatcher = HookDispatcher::empty();

                let response = handler(
                    make_request(repository, branch_id),
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
}
