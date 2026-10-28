// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::repository::v1::RepositoryGetRequest;
use lore_proto::lore::repository::v1::RepositoryGetResponse;
use lore_proto::lore::repository::v1::repository_get_request::Query;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::forwarded_requests::ForwardedRequests;
use tonic::Request;
use tonic::Response;
use tonic::Status;
mod forwarded_request {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lore_proto::lore::repository::v1::RepositoryCreateRequest;
    use lore_proto::lore::repository::v1::RepositoryCreateResponse;
    use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use lore_server::grpc::forwarded_requests::ForwardedRequestResult;
    use lore_server::grpc::forwarded_requests::InternalClientError;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use lore_server::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
    use lore_server::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
    use lore_server::grpc::repository::v1::repository_get::*;
    use rand::random;

    use super::*;
    use crate::store::test_support::test_store_create;

    /// Single-use client that returns a pre-configured result on its one call.
    struct SingleShotClient {
        response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryGetResponse>>>>,
    }

    #[async_trait]
    impl ForwardedRepositoryServiceClient for SingleShotClient {
        async fn repository_create(
            &mut self,
            _request: Request<RepositoryCreateRequest>,
        ) -> ForwardedRequestResult<RepositoryCreateResponse> {
            unreachable!("repository_create should not be called in repository_get tests")
        }

        async fn repository_get(
            &mut self,
            _request: Request<RepositoryGetRequest>,
        ) -> ForwardedRequestResult<RepositoryGetResponse> {
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("repository_get called more than once")
        }
    }

    struct StubForwardedRequests {
        flags: RpcFlags,
        response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryGetResponse>>>>,
    }

    impl StubForwardedRequests {
        fn new(
            repository_get: bool,
            response: ForwardedRequestResult<RepositoryGetResponse>,
        ) -> Arc<Self> {
            Arc::new(Self {
                flags: RpcFlags {
                    repository_get,
                    ..Default::default()
                },
                response: Arc::new(Mutex::new(Some(response))),
            })
        }

        fn forwarding_enabled(
            response: ForwardedRequestResult<RepositoryGetResponse>,
        ) -> Arc<Self> {
            Self::new(true, response)
        }

        fn forwarding_disabled(
            response: ForwardedRequestResult<RepositoryGetResponse>,
        ) -> Arc<Self> {
            Self::new(false, response)
        }
    }

    impl ForwardedRequests for StubForwardedRequests {
        fn rpc_flags(&self) -> &RpcFlags {
            &self.flags
        }

        fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
            unreachable!("forwarded_revision_service should not be called in repository_get tests")
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
            unreachable!("forwarded_lock_service should not be called in repository_get tests")
        }
    }

    fn make_request(name: &str) -> Request<RepositoryGetRequest> {
        Request::new(RepositoryGetRequest {
            query: Some(Query::Name(name.into())),
        })
    }

    /// Writes the metadata blob, its pointer and the name → id mapping so a
    /// local lookup of `name` resolves.
    async fn seed_repository(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        id: RepositoryId,
        name: &str,
    ) {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            id,
        ));
        let metadata_hash = repository::metadata_store(
            repository.clone(),
            RepositoryMetadata {
                name: name.to_string(),
                creator: "alice".into(),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to store repository metadata");
        repository::metadata_store_hash(repository.clone(), metadata_hash)
            .await
            .expect("Failed to store repository metadata hash");
        repository::store_name_to_id(repository, name, id)
            .await
            .expect("Failed to store repository name to id mapping");
    }

    #[tokio::test]
    async fn delegates_to_remote_and_returns_response() {
        // When the flag is enabled the other server's response is returned directly;
        // repository_get_implementation is NOT called so the local store is not read.
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repo_record = lore_proto::lore::model::v1::Repository {
            name: "test-repo".into(),
            ..Default::default()
        };
        let repo_response = Ok(Ok(Response::new(RepositoryGetResponse {
            repository: Some(repo_record),
        })));
        let forwarded_requests = StubForwardedRequests::forwarding_enabled(repo_response);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let response = handler(
                make_request("test-repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
            )
            .await
            .expect("should succeed");

            let repository = response
                .into_inner()
                .repository
                .expect("response should include Repository");
            assert_eq!(repository.name, "test-repo");
        }))
        .await;
    }

    #[tokio::test]
    async fn error_status_returned_to_caller() {
        // An error status from the forwarded server is forwarded directly to the original caller.
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let forwarded_request_result = Ok(Err(Status::not_found("test error forwarded")));
        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(forwarded_request_result);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let err = handler(
                make_request("test-repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
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
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(Err(InternalClientError::internal("oops")));

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let err = handler(
                make_request("test-repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
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
        // When repository_get is false the local path runs, even if a
        // ForwardedRequests is present. The stub client is not called.
        let id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        // response is irrelevant — client must never be called
        let forwarded_result = Ok(Err(Status::internal("should not be called")));
        let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            seed_repository(
                immutable_store.clone(),
                mutable_store.clone(),
                id,
                "my-repo",
            )
            .await;

            let response = handler(
                make_request("my-repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
            )
            .await
            .expect("local execution should succeed");

            let repository = response
                .into_inner()
                .repository
                .expect("response should include Repository");
            assert_eq!(repository.name, "my-repo");
            assert_eq!(repository.id, bytes::Bytes::from(id));
        }))
        .await;
    }

    /// Denial answers `RepositoryNotFound`, not `permission_denied`: a
    /// distinct status would let an unauthorized caller distinguish a
    /// partition that exists from one that does not.
    #[tokio::test]
    async fn denied_get_answers_not_found_for_an_existing_repository() {
        struct DenyAllRepositoryAuthorizer;

        #[async_trait]
        impl RepositoryAuthorizer for DenyAllRepositoryAuthorizer {
            async fn check_repository_access(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: lore_base::types::RepositoryId,
                _action: Action,
            ) -> Result<(), Status> {
                Err(Status::permission_denied("denied"))
            }
        }

        let id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            seed_repository(
                immutable_store.clone(),
                mutable_store.clone(),
                id,
                "my-repo",
            )
            .await;

            let err = handler(
                make_request("my-repo"),
                Arc::new(DenyAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
                &None,
            )
            .await
            .expect_err("denied get must fail");

            assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");
        }))
        .await;
    }
}
