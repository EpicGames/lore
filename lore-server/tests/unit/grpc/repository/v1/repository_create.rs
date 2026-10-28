// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod input_length_validation {
    use lore_revision::repository;
    use lore_server::grpc::repository::v1::repository_create::*;

    #[test]
    fn accepts_valid_input() {
        validate_create_input("my-repo", "a description", "main", "alice")
            .expect("valid input should pass");
    }

    #[test]
    fn accepts_name_at_max_length() {
        let name = "a".repeat(repository::MAX_NAME_LEN);
        validate_create_input(&name, "desc", "main", "alice")
            .expect("name at exactly MAX_NAME_LEN should pass");
    }

    #[test]
    fn rejects_oversized_repository_name() {
        let long_name = "a".repeat(repository::MAX_NAME_LEN + 1);
        let err = validate_create_input(&long_name, "desc", "main", "alice")
            .expect_err("should reject oversized name");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message()
                .contains("Repository name exceeds maximum length")
        );
    }

    #[test]
    fn rejects_oversized_description() {
        let long_desc = "a".repeat(repository::MAX_DESCRIPTION_LEN + 1);
        let err = validate_create_input("my-repo", &long_desc, "main", "alice")
            .expect_err("should reject oversized description");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("description exceeds maximum length"));
    }

    #[test]
    fn rejects_oversized_branch_name() {
        let long_branch = "a".repeat(repository::MAX_NAME_LEN + 1);
        let err = validate_create_input("my-repo", "desc", &long_branch, "alice")
            .expect_err("should reject oversized branch name");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("Branch name exceeds maximum length"));
    }

    #[test]
    fn rejects_oversized_creator() {
        let long_creator = "a".repeat(repository::MAX_NAME_LEN + 1);
        let err = validate_create_input("my-repo", "desc", "main", &long_creator)
            .expect_err("should reject oversized creator");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("Creator exceeds maximum length"));
    }
}

mod forwarded_request {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_proto::lore::repository::v1::RepositoryCreateRequest;
    use lore_proto::lore::repository::v1::RepositoryCreateResponse;
    use lore_revision::lore::RepositoryId;
    use lore_server::authnz::repository_registry::RepositoryRegistry;
    use lore_server::grpc::forwarded_requests::ForwardedRequestResult;
    use lore_server::grpc::forwarded_requests::ForwardedRequests;
    use lore_server::grpc::forwarded_requests::InternalClientError;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use lore_server::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
    use lore_server::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
    use lore_server::grpc::repository::v1::repository_create::*;
    use lore_server::hooks::HookDispatcher;
    use rand::random;
    use tonic::Request;
    use tonic::Response;
    use tonic::Status;

    use crate::store::test_support::test_store_create;

    struct TestInstrumentProvider;

    impl lore_telemetry::InstrumentProvider for TestInstrumentProvider {
        fn namespace(&self) -> &'static str {
            "test"
        }
    }

    /// Single-use client that returns a pre-configured result on its one call.
    struct SingleShotClient {
        response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryCreateResponse>>>>,
    }

    #[async_trait]
    impl ForwardedRepositoryServiceClient for SingleShotClient {
        async fn repository_create(
            &mut self,
            _request: Request<RepositoryCreateRequest>,
        ) -> ForwardedRequestResult<RepositoryCreateResponse> {
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("repository_create called more than once")
        }

        async fn repository_get(
            &mut self,
            _request: Request<lore_proto::lore::repository::v1::RepositoryGetRequest>,
        ) -> ForwardedRequestResult<lore_proto::lore::repository::v1::RepositoryGetResponse>
        {
            unreachable!("repository_get should not be called in repository_create tests")
        }
    }

    struct StubForwardedRequests {
        flags: RpcFlags,
        response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryCreateResponse>>>>,
    }

    impl StubForwardedRequests {
        fn forwarding_enabled(
            response: ForwardedRequestResult<RepositoryCreateResponse>,
        ) -> Arc<Self> {
            Arc::new(Self {
                flags: RpcFlags {
                    repository_create: true,
                    ..Default::default()
                },
                response: Arc::new(Mutex::new(Some(response))),
            })
        }

        fn forwarding_disabled(
            response: ForwardedRequestResult<RepositoryCreateResponse>,
        ) -> Arc<Self> {
            Arc::new(Self {
                flags: RpcFlags {
                    repository_create: false,
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
            unreachable!(
                "forwarded_revision_service should not be called in repository_create tests"
            )
        }

        fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient> {
            Box::new(SingleShotClient {
                response: Arc::clone(&self.response),
            })
        }

        fn forwarded_lock_service(
            &self,
        ) -> Box<dyn lore_server::grpc::forwarded_requests::lock_service::ForwardedLockServiceClient>
        {
            unreachable!("forwarded_lock_service should not be called in repository_create tests")
        }
    }

    fn make_request(repository_id: RepositoryId, name: &str) -> Request<RepositoryCreateRequest> {
        let id_bytes: lore_base::types::Context = repository_id.into();
        Request::new(RepositoryCreateRequest {
            id: bytes::Bytes::from(id_bytes),
            name: name.into(),
            description: String::new(),
            default_branch_id: bytes::Bytes::from(lore_base::types::Context::from(
                uuid::Uuid::now_v7(),
            )),
            default_branch_name: "main".into(),
            creator: Some("alice".into()),
        })
    }

    #[tokio::test]
    async fn delegates_to_remote_and_returns_response() {
        // When the flag is enabled the other server's response is returned directly;
        // repository_create_implementation is NOT called so the local store stays empty.
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repo_record = lore_proto::lore::model::v1::Repository {
            name: "test-repo".into(),
            ..Default::default()
        };
        let repo_response = Ok(Ok(Response::new(RepositoryCreateResponse {
            repository: Some(repo_record),
        })));
        let forwarded_requests = StubForwardedRequests::forwarding_enabled(repo_response);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let hook_dispatcher = HookDispatcher::empty();

            let response = handler(
                make_request(repository_id, "test-repo"),
                &RepositoryRegistry::Unregistered,
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &TestInstrumentProvider,
            )
            .await
            .expect("should succeed");

            let repo = response
                .into_inner()
                .repository
                .expect("response should include Repository");
            assert_eq!(repo.name, "test-repo");
        }))
        .await;
    }

    #[tokio::test]
    async fn error_status_returned_to_caller() {
        // An error status from the forwarded server is forwarded directly to the original caller.
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let forwarded_request_result = Ok(Err(Status::already_exists("test error forwarded")));
        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(forwarded_request_result);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let hook_dispatcher = HookDispatcher::empty();

            let err = handler(
                make_request(repository_id, "test-repo"),
                &RepositoryRegistry::Unregistered,
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &TestInstrumentProvider,
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
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(Err(InternalClientError::internal("oops")));

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let hook_dispatcher = HookDispatcher::empty();

            let err = handler(
                make_request(repository_id, "test-repo"),
                &RepositoryRegistry::Unregistered,
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &TestInstrumentProvider,
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
        // When repository_create is false the local path runs, even if a
        // ForwardedRequests is present. The stub client is not called.
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        // response is irrelevant — client must never be called
        let forwarded_result = Ok(Err(Status::internal("should not be called")));
        let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let hook_dispatcher = HookDispatcher::empty();

            let response = handler(
                make_request(repository_id, "my-repo"),
                &RepositoryRegistry::Unregistered,
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                &hook_dispatcher,
                &TestInstrumentProvider,
            )
            .await
            .expect("local execution should succeed");

            let repo = response
                .into_inner()
                .repository
                .expect("response should include Repository");
            assert_eq!(repo.name, "my-repo");
        }))
        .await;
    }
}
