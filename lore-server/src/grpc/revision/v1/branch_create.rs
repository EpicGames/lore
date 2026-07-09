// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_proto::lore::revision::v1::BranchCreateRequest;
use lore_proto::lore::revision::v1::BranchCreateResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use super::branch_record::build_branch;
use crate::grpc::ServerResultExt;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

/// Reject oversized string fields early to prevent resource exhaustion.
fn validate_create_input(name: &str, category: &str, creator: &str) -> Result<(), Status> {
    if name.len() > branch::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Branch name exceeds maximum length of {} bytes",
            branch::MAX_NAME_LEN,
        )));
    }
    if category.len() > branch::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Branch category exceeds maximum length of {} bytes",
            branch::MAX_NAME_LEN,
        )));
    }
    if creator.len() > repository::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Creator exceeds maximum length of {} bytes",
            repository::MAX_NAME_LEN,
        )));
    }
    Ok(())
}

/// `lore.revision.v1.RevisionService.BranchCreate` handler.
///
/// The caller pre-generates `Branch.id` (retry idempotency). The
/// server assigns `created` and the response's full `Branch` record.
/// `creator` is hybrid: caller-set if permitted, otherwise the
/// authenticated JWT identity.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRevisionService`
#[tracing::instrument(name = "BranchCreate::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchCreateRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().revision_branch_create
    {
        forward_branch_create(req, caller_context, forwarded_requests).await
    } else {
        branch_create_implementation(
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

/// This `BranchCreateRequest` should be handled by another server
/// and the response forwarded on to the client
async fn forward_branch_create(
    req: BranchCreateRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<BranchCreateResponse>, Status> {
    let mut client = forwarded_requests.forwarded_revision_service();
    let request = context.to_forwarded_request(req)?;

    let branch_create_result = client
        .branch_create(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = branch_create_result?;
    Ok(response)
}

/// This `BranchCreateRequest` should be fulfilled by this server.
pub async fn branch_create_implementation(
    req: BranchCreateRequest,
    context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    let name = req.name;
    let category = req.category;
    let creator = req.creator.unwrap_or_else(|| context.user_id.clone());
    let stack: Vec<BranchPoint> = req.stack.into_iter().map(BranchPoint::from).collect();

    let branch = BranchId::from(req.id);

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();

    let execution = setup_execution(
        module_path!(),
        context.correlation_id.clone(),
        context.user_id.clone(),
    );
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        context.repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let hook_ctx = HookContext::builder()
                .correlation_id(&context.correlation_id)
                .hook_point(HookPoint::BranchCreate)
                .repository(context.repository_id)
                .user(&context.user_id)
                .branch(branch)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchCreate, &hook_ctx)
                .map_err(hook_error_to_status)?;

            validate_create_input(&name, &category, &creator)?;

            debug!({BRANCH_ID} = %branch, branch_name = %name, stack = ?stack, "Creating branch");

            let write_token = get_write_token();
            let branch_id = branch::create(
                repository.clone(),
                &write_token,
                branch,
                name.as_str(),
                category.as_str(),
                creator.as_str(),
                created,
                stack.clone(),
                false,
                false,
            )
            .await
            .map_err(|err| {
                if err.is_branch_already_exists() {
                    Status::already_exists(err.to_string())
                } else {
                    Status::invalid_argument(err.to_string())
                }
            })?;

            notification_sender
                .branch_created(context.repository_id, branch_id)
                .await;
            hook_dispatcher.spawn_post(HookPoint::BranchCreate, hook_ctx);

            let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;
            let metadata = branch::load_metadata(repository.clone(), metadata_hash)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;

            debug!({BRANCH_ID} = %branch_id, %name, "Created branch");

            let response_branch =
                build_branch(repository, branch_id, &metadata, metadata_hash, false).await?;

            instrument_provider
                .counter("num_branches_created")
                .add(1, &[]);

            Ok(Response::new(BranchCreateResponse {
                branch: Some(response_branch),
            }))
        })
        .await
}

#[cfg(test)]
mod test {
    mod input_length_validation {
        use lore_revision::branch;
        use lore_revision::repository;

        use super::super::*;

        #[test]
        fn accepts_valid_input() {
            validate_create_input("my-branch", "feature", "alice")
                .expect("valid input should pass");
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
    use lore_revision::lore::RepositoryId;
    use lore_telemetry::InstrumentProvider;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;
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

    mod direct_handling {
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

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());
                let mut request = Request::new(BranchCreateRequest {
                    id: branch_id.into(),
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
                let response = handler(
                    request,
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
        async fn empty_name_returns_invalid_argument() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let notification_sender = Arc::new(MockNotificationSender::new());
            let instrument_provider = TestInstrumentProvider {};

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());
                let mut request = Request::new(BranchCreateRequest {
                    id: branch_id.into(),
                    name: String::new(),
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
                    immutable_store.clone(),
                    mutable_store.clone(),
                    notification_sender.clone(),
                    &None, /* no forwarded requests */
                    &hook_dispatcher,
                    &instrument_provider,
                )
                .await
                .expect_err("empty name should fail");
                assert_eq!(err.code(), tonic::Code::InvalidArgument);
            }))
            .await;
        }

        #[tokio::test]
        async fn unset_creator_falls_back_to_jwt_identity() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let mut notification_sender = MockNotificationSender::new();
            notification_sender
                .expect_branch_created()
                .return_once(|_, _| ());
            let notification_sender = Arc::new(notification_sender);
            let instrument_provider = TestInstrumentProvider {};

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());
                let mut request = Request::new(BranchCreateRequest {
                    id: branch_id.into(),
                    name: "main".into(),
                    creator: None,
                    category: "default".into(),
                    stack: vec![],
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
                );
                request.extensions_mut().insert(AuthorizationToken {
                    user_id: "jwt-user".into(),
                    ..AuthorizationToken::default()
                });

                let hook_dispatcher = HookDispatcher::empty();
                let response = handler(
                    request,
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
                assert_eq!(branch.creator, "jwt-user");
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

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());
                let make_request = || {
                    let mut request = Request::new(BranchCreateRequest {
                        id: branch_id.into(),
                        name: "main".into(),
                        creator: Some("alice".into()),
                        category: "default".into(),
                        stack: vec![],
                    });
                    request.metadata_mut().insert_bin(
                        REPOSITORY_ID_KEY,
                        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
                    );
                    request
                };

                let hook_dispatcher = HookDispatcher::empty();
                handler(
                    make_request(),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    notification_sender.clone(),
                    &None, /* no forwarded requests */
                    &hook_dispatcher,
                    &instrument_provider,
                )
                .await
                .expect("first create should succeed");

                let err = handler(
                    make_request(),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    notification_sender.clone(),
                    &None, /* no forwarded requests */
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
            response: Arc<Mutex<Option<ForwardedRequestResult<BranchCreateResponse>>>>,
        }

        #[async_trait]
        impl ForwardedRevisionServiceClient for SingleShotClient {
            async fn branch_create(
                &mut self,
                _request: Request<BranchCreateRequest>,
            ) -> ForwardedRequestResult<BranchCreateResponse> {
                self.response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("branch_create called more than once")
            }

            async fn branch_delete(
                &mut self,
                _request: Request<lore_proto::lore::revision::v1::BranchDeleteRequest>,
            ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchDeleteResponse>
            {
                unreachable!("branch_delete should not be called in branch_create tests")
            }

            async fn branch_get(
                &mut self,
                _request: Request<lore_proto::lore::revision::v1::BranchGetRequest>,
            ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchGetResponse>
            {
                unreachable!("branch_get should not be called in branch_create tests")
            }
        }

        struct StubForwardedRequests {
            flags: RpcFlags,
            response: Arc<Mutex<Option<ForwardedRequestResult<BranchCreateResponse>>>>,
        }

        impl StubForwardedRequests {
            fn forwarding_enabled(
                response: ForwardedRequestResult<BranchCreateResponse>,
            ) -> Arc<Self> {
                Arc::new(Self {
                    flags: RpcFlags {
                        revision_branch_create: true,
                        ..Default::default()
                    },
                    response: Arc::new(Mutex::new(Some(response))),
                })
            }

            fn forwarding_disabled(
                response: ForwardedRequestResult<BranchCreateResponse>,
            ) -> Arc<Self> {
                Arc::new(Self {
                    flags: RpcFlags {
                        revision_branch_create: false,
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

        fn make_request(
            repository: RepositoryId,
            branch_id: BranchId,
        ) -> Request<BranchCreateRequest> {
            let mut request = Request::new(BranchCreateRequest {
                id: branch_id.into(),
                name: "main".into(),
                creator: Some("alice".into()),
                category: "default".into(),
                stack: vec![],
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            request
        }

        #[tokio::test]
        async fn delegates_to_remote_and_returns_response() {
            // When the flag is enabled the other server's response is returned directly;
            // branch_create_implementation is NOT called so the local store stays empty.
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let notification_sender = Arc::new(MockNotificationSender::new()); // no branch_created expected
            let instrument_provider = TestInstrumentProvider {};

            let branch_request = lore_proto::lore::model::v1::Branch {
                name: "main".into(),
                creator: "alice".into(),
                category: "default".into(),
                ..Default::default()
            };
            let branch_response = Ok(Ok(Response::new(BranchCreateResponse {
                branch: Some(branch_request),
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
                assert_eq!(branch.name, "main");
                assert_eq!(branch.creator, "alice");
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

            let forwarded_request_result = Ok(Err(Status::already_exists("test error forwarded")));
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

                assert_eq!(err.code(), tonic::Code::AlreadyExists);
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
            // When revision_branch_create is false the local path runs, even if a
            // ForwardedRequests is present. The stub client is not called.
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let mut notification_sender = MockNotificationSender::new();
            notification_sender
                .expect_branch_created()
                .return_once(|_, _| ());
            let notification_sender = Arc::new(notification_sender);
            let instrument_provider = TestInstrumentProvider {};

            // response is irrelevant — client must never be called
            let forwarded_result = Ok(Err(Status::internal("should not be called")));
            let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

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
                .expect("local execution should succeed");

                let branch = response
                    .into_inner()
                    .branch
                    .expect("response should include Branch");
                assert_eq!(branch.name, "main");
            }))
            .await;
        }
    }
}
