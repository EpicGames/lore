// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_proto::LockService;
use lore_revision::lore::RepositoryId;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::grpc::lock_service::LoreLockService;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Code;
use tonic::Request;

fn lock_service_with(
    lock_store: store::MockMockLockStore,
    authorizer: Arc<dyn RepositoryAuthorizer>,
) -> LoreLockService {
    LoreLockService::new(
        Arc::new(lock_store),
        Arc::new(lore_server::notification::local::NotificationSender::default()),
        authorizer,
        None,
        Duration::from_secs(60),
    )
}

fn lock_service(lock_store: store::MockMockLockStore) -> LoreLockService {
    lock_service_with(lock_store, Arc::new(AllowAllRepositoryAuthorizer))
}

pub(super) mod store {
    use async_trait::async_trait;
    use lore_base::types::LockData;
    use lore_base::types::LockResource;
    use lore_revision::lock::LockError;
    use lore_revision::lock::LockQuery;
    use lore_revision::lock::LockStore;
    use lore_revision::lore::RepositoryId;

    mockall::mock! {
         pub MockLockStore {}

         #[async_trait]
         impl LockStore for MockLockStore {

            async fn lock_resources(
                &self,
                owner_id: &str,
                repository: RepositoryId,
                resources: &[LockResource],
            ) -> Result<Vec<LockData>, LockError>;

            async fn query_locks(&self, query: LockQuery) -> Result<Vec<LockData>, LockError>;

            async fn check_locks_status(
                &self,
                repository: RepositoryId,
                resources: &[LockResource],
            ) -> Result<Vec<LockData>, LockError>;


            async fn unlock_resources(
                &self,
                owner_id: &str,
                validate_user: bool,
                repository: RepositoryId,
                resources: &[LockResource],
            ) -> Result<Vec<LockResource>, LockError>;
        }
    }
}

mod status {
    use lore_proto::lock::Resource;
    use lore_proto::lock::StatusRequest;

    use super::*;

    #[tokio::test]
    async fn resource_count_exceeds_limit() {
        let lock_store = super::store::MockMockLockStore::new();

        let lock_service = super::lock_service(lock_store);

        let resources: Vec<Resource> = (0..101)
            .map(|_| Resource {
                branch: Default::default(),
                hash: Default::default(),
                description: "".to_string(),
            })
            .collect();

        let mut request = Request::new(StatusRequest { resources });
        let repository = random::<RepositoryId>();
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let error_status = lock_service
            .status(request)
            .await
            .expect_err("Status should fail when resource count exceeds limit");

        assert_eq!(error_status.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn resource_count_at_limit() {
        let mut lock_store = super::store::MockMockLockStore::new();
        lock_store
            .expect_check_locks_status()
            .return_once(|_, _| Ok(vec![]));

        let lock_service = super::lock_service(lock_store);

        let resources: Vec<Resource> = (0..100)
            .map(|_| Resource {
                branch: Default::default(),
                hash: Default::default(),
                description: "".to_string(),
            })
            .collect();

        let mut request = Request::new(StatusRequest { resources });
        let repository = random::<RepositoryId>();
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let _ = lock_service
            .status(request)
            .await
            .expect("Status should succeed when resource count is at limit");
    }
}

mod unlock {
    use lore_proto::lock::AdminLockRequest;
    use lore_proto::lock::LockRequest;
    use lore_proto::lock::Resource;
    use lore_proto::lock::StatusRequest;
    use lore_proto::lock::UnlockRequest;

    use super::*;

    #[tokio::test]
    async fn lock_zero_resources() {
        let lock_store = super::store::MockMockLockStore::new();

        let lock_service = super::lock_service(lock_store);

        let mut request = Request::new(LockRequest { resources: vec![] });
        let repository = random::<RepositoryId>();
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let _ = lock_service
            .lock(request)
            .await
            .expect("LockData did not return ok status");
    }

    #[tokio::test]
    async fn unlock_zero_resources() {
        let lock_store = super::store::MockMockLockStore::new();

        let lock_service = super::lock_service(lock_store);

        let mut request = Request::new(UnlockRequest { resources: vec![] });
        let repository = random::<RepositoryId>();
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let _ = lock_service
            .unlock(request)
            .await
            .expect("Unlock did not return ok status");
    }

    #[tokio::test]
    async fn status_zero_resources() {
        let lock_store = super::store::MockMockLockStore::new();

        let lock_service = super::lock_service(lock_store);

        let mut request = Request::new(StatusRequest { resources: vec![] });
        let repository = random::<RepositoryId>();
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let _ = lock_service
            .status(request)
            .await
            .expect("Status did not return ok status");
    }

    /// `migrate` is checked before an empty request short-circuits, so an
    /// empty admin lock is refused without the action like any other.
    #[tokio::test]
    async fn admin_lock_checks_migrate_before_the_empty_short_circuit() {
        let lock_store = super::store::MockMockLockStore::new();

        let lock_service = super::lock_service(lock_store);

        let mut request = Request::new(AdminLockRequest {
            resources: vec![],
            owner: "".to_string(),
        });
        let repository = random::<RepositoryId>();
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let denied = lock_service
            .admin_lock(request)
            .await
            .expect_err("allow-all does not grant `migrate`");
        assert_eq!(denied.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn unlock_fails_for_other_owner() {
        let mut lock_store = super::store::MockMockLockStore::new();
        lock_store
            .expect_unlock_resources()
            .return_once(|_, _, _, _| Err(lore_base::error::LockNotOwned.into()));

        let lock_service = super::lock_service(lock_store);

        let mut request = Request::new(UnlockRequest {
            resources: vec![Resource {
                branch: Default::default(),
                hash: Default::default(),
                description: "".to_string(),
            }],
        });
        let repository = random::<RepositoryId>();
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let error_status = lock_service
            .unlock(request)
            .await
            .expect_err("Unlock did not return error status");

        assert_eq!(error_status.code(), Code::FailedPrecondition);
    }
}

/// Elevated action checks: unlock elevates on `owner`/`admin`,
/// admin-lock requires `migrate`, both answered by the configured
/// authorizer from the verified token — on both OIDC tiers.
mod actions {
    use lore_proto::lock::AdminLockRequest;
    use lore_proto::lock::Resource;
    use lore_proto::lock::UnlockRequest;
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::auth::jwt::ResourcePermission;
    use lore_server::authnz::global_grants_authorizer::GlobalGrantsAuthorizer;
    use lore_server::authnz::repository_authorizer::RawToken;
    use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
    use serde_json::json;

    use super::*;

    /// Tier 1, reading the actions from a dotted global claim.
    fn tier1() -> Arc<dyn RepositoryAuthorizer> {
        Arc::new(GlobalGrantsAuthorizer::new(
            Some("realm_access.roles".to_string()),
            vec![Action::Read, Action::Write],
        ))
    }

    /// Tier 2, reading per-repository grants from the legacy-shaped
    /// `resources` claim.
    fn tier2() -> Arc<dyn RepositoryAuthorizer> {
        Arc::new(ResourceGrantsAuthorizer::new(
            "resources".to_string(),
            "resource_id".to_string(),
            None,
            "urc-{id}".to_string(),
            "urc-*".to_string(),
        ))
    }

    /// A token granting `actions` in the shape both tiers read: globally
    /// under `realm_access.roles`, and per-repository under `resources`.
    fn token_granting(repository: RepositoryId, actions: &[&str]) -> AuthorizationToken {
        let serde_json::Value::Object(extra) = json!({ "realm_access": { "roles": actions } })
        else {
            unreachable!()
        };
        AuthorizationToken {
            user_id: "the u".to_string(),
            resources: Some(vec![ResourcePermission {
                resource_id: format!("urc-{repository}"),
                permission: actions.iter().map(ToString::to_string).collect(),
            }]),
            extra,
            ..Default::default()
        }
    }

    fn request_with_token<T>(
        message: T,
        repository: RepositoryId,
        token: Option<AuthorizationToken>,
    ) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        if let Some(token) = token {
            request.extensions_mut().insert(token);
            request.extensions_mut().insert(RawToken("raw.jwt".into()));
        }
        request
    }

    fn one_resource() -> Vec<Resource> {
        vec![Resource {
            branch: Default::default(),
            hash: Default::default(),
            description: "".to_string(),
        }]
    }

    /// The store records the `validate_user` flag it was called with,
    /// which is what elevation waives.
    fn store_expecting_validate_user(validate: bool) -> store::MockMockLockStore {
        let mut lock_store = store::MockMockLockStore::new();
        lock_store
            .expect_unlock_resources()
            .withf(move |_, validate_user, _, _| *validate_user == validate)
            .return_once(|_, _, _, _| Ok(vec![]));
        lock_store
    }

    #[tokio::test]
    async fn unlock_elevates_with_the_action_on_both_tiers() {
        for (tier, action) in [
            (tier1(), "owner"),
            (tier1(), "admin"),
            (tier2(), "owner"),
            (tier2(), "admin"),
        ] {
            let repository = random::<RepositoryId>();
            let lock_service = lock_service_with(store_expecting_validate_user(false), tier);
            let request = request_with_token(
                UnlockRequest {
                    resources: one_resource(),
                },
                repository,
                Some(token_granting(repository, &[action])),
            );
            lock_service.unlock(request).await.unwrap();
        }
    }

    #[tokio::test]
    async fn unlock_stays_owner_validated_without_the_action_on_both_tiers() {
        for tier in [tier1(), tier2()] {
            let repository = random::<RepositoryId>();
            let lock_service = lock_service_with(store_expecting_validate_user(true), tier);
            let request = request_with_token(
                UnlockRequest {
                    resources: one_resource(),
                },
                repository,
                Some(token_granting(repository, &["write"])),
            );
            lock_service.unlock(request).await.unwrap();
        }
    }

    /// Neither elevated nor holding `write`: refused before the store is
    /// touched. Tier 2 only, since Tier 1 grants `write` to every token.
    #[tokio::test]
    async fn unlock_without_write_or_elevation_is_refused() {
        let repository = random::<RepositoryId>();
        let lock_service = lock_service_with(store::MockMockLockStore::new(), tier2());
        let denied = lock_service
            .unlock(request_with_token(
                UnlockRequest {
                    resources: one_resource(),
                },
                repository,
                Some(token_granting(repository, &["read"])),
            ))
            .await
            .expect_err("unlock without `write` is refused");
        assert_eq!(denied.code(), Code::PermissionDenied);
    }

    #[tokio::test]
    async fn admin_lock_requires_migrate_on_both_tiers() {
        for tier in [tier1(), tier2()] {
            let repository = random::<RepositoryId>();
            let mut lock_store = store::MockMockLockStore::new();
            lock_store
                .expect_lock_resources()
                .return_once(|_, _, _| Ok(vec![]));
            let lock_service = lock_service_with(lock_store, tier.clone());

            let denied = lock_service
                .admin_lock(request_with_token(
                    AdminLockRequest {
                        resources: one_resource(),
                        owner: "someone".to_string(),
                    },
                    repository,
                    Some(token_granting(repository, &["push"])),
                ))
                .await
                .expect_err("admin lock without `migrate` is denied");
            assert_eq!(denied.code(), Code::PermissionDenied);

            lock_service
                .admin_lock(request_with_token(
                    AdminLockRequest {
                        resources: one_resource(),
                        owner: "someone".to_string(),
                    },
                    repository,
                    Some(token_granting(repository, &["migrate"])),
                ))
                .await
                .expect("admin lock with `migrate` is permitted");
        }
    }

    /// Without a token the authorizer decides. A token-reading tier grants
    /// nothing, so unlock is refused. Allow-all grants `write` but neither
    /// `owner` nor `migrate`: unlock releases the caller's own locks only,
    /// and admin lock is denied.
    #[tokio::test]
    async fn no_token_unlocks_own_locks_only_under_allow_all_and_never_admin_locks() {
        let repository = random::<RepositoryId>();
        let lock_service = lock_service_with(store::MockMockLockStore::new(), tier1());
        let denied = lock_service
            .unlock(request_with_token(
                UnlockRequest {
                    resources: one_resource(),
                },
                repository,
                None,
            ))
            .await
            .expect_err("a token-reading tier grants nothing without a token");
        assert_eq!(denied.code(), Code::PermissionDenied);

        let lock_service = super::lock_service(store_expecting_validate_user(true));
        lock_service
            .unlock(request_with_token(
                UnlockRequest {
                    resources: one_resource(),
                },
                repository,
                None,
            ))
            .await
            .unwrap();

        let lock_service = super::lock_service(store::MockMockLockStore::new());
        let denied = lock_service
            .admin_lock(request_with_token(
                AdminLockRequest {
                    resources: one_resource(),
                    owner: "someone".to_string(),
                },
                repository,
                None,
            ))
            .await
            .expect_err("admin lock without a token is denied even under allow-all");
        assert_eq!(denied.code(), Code::PermissionDenied);
    }
}

mod query {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lore_base::types::LockData;
    use lore_base::types::LockResource;
    use lore_proto::lock::Lock;
    use lore_proto::lock::QueryRequest;
    use lore_proto::lock::QueryResponse;
    use lore_revision::lock::LockQuery;
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::grpc::forwarded_requests::CallerContext;
    use lore_server::grpc::forwarded_requests::ForwardedRequestResult;
    use lore_server::grpc::forwarded_requests::ForwardedRequests;
    use lore_server::grpc::forwarded_requests::InternalClientError;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use lore_server::grpc::forwarded_requests::lock_service::ForwardedLockServiceClient;
    use lore_server::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
    use lore_server::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
    use tonic::Response;
    use tonic::Status;

    use super::*;

    type SharedResponse = Arc<Mutex<Option<ForwardedRequestResult<QueryResponse>>>>;
    type SharedRequest = Arc<Mutex<Option<Request<QueryRequest>>>>;

    /// Answers its one call with a pre-configured result and keeps the request
    /// it was sent.
    struct SingleShotClient {
        response: SharedResponse,
        sent: SharedRequest,
    }

    #[async_trait]
    impl ForwardedLockServiceClient for SingleShotClient {
        async fn query(
            &mut self,
            request: Request<QueryRequest>,
        ) -> ForwardedRequestResult<QueryResponse> {
            *self.sent.lock().unwrap() = Some(request);
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("query called more than once")
        }
    }

    struct StubForwardedRequests {
        flags: RpcFlags,
        response: SharedResponse,
        sent: SharedRequest,
    }

    impl StubForwardedRequests {
        fn new(flags: RpcFlags, response: ForwardedRequestResult<QueryResponse>) -> Arc<Self> {
            Arc::new(Self {
                flags,
                response: Arc::new(Mutex::new(Some(response))),
                sent: Arc::new(Mutex::new(None)),
            })
        }

        fn forwarding_enabled(response: ForwardedRequestResult<QueryResponse>) -> Arc<Self> {
            Self::new(
                RpcFlags {
                    lock_query: true,
                    ..Default::default()
                },
                response,
            )
        }

        fn sent_request(&self) -> Request<QueryRequest> {
            self.sent
                .lock()
                .unwrap()
                .take()
                .expect("no request was forwarded")
        }
    }

    impl ForwardedRequests for StubForwardedRequests {
        fn rpc_flags(&self) -> &RpcFlags {
            &self.flags
        }

        fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
            unreachable!("forwarded_revision_service should not be called in query tests")
        }

        fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient> {
            unreachable!("forwarded_repository_service should not be called in query tests")
        }

        fn forwarded_lock_service(&self) -> Box<dyn ForwardedLockServiceClient> {
            Box::new(SingleShotClient {
                response: Arc::clone(&self.response),
                sent: Arc::clone(&self.sent),
            })
        }
    }

    fn lock_service_forwarding(
        lock_store: store::MockMockLockStore,
        forwarded_requests: Arc<StubForwardedRequests>,
    ) -> LoreLockService {
        LoreLockService::new(
            Arc::new(lock_store),
            Arc::new(lore_server::notification::local::NotificationSender::default()),
            Arc::new(AllowAllRepositoryAuthorizer),
            Some(forwarded_requests),
            Duration::from_secs(60),
        )
    }

    fn query_by_owner(repository: RepositoryId, owner: &str) -> Request<QueryRequest> {
        let mut request = Request::new(QueryRequest {
            branch: None,
            owner: Some(owner.to_string()),
            description: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request.extensions_mut().insert(AuthorizationToken {
            user_id: "caller".into(),
            ..AuthorizationToken::default()
        });
        request
    }

    fn store_answering(repository: RepositoryId, owner: &'static str) -> store::MockMockLockStore {
        let mut lock_store = store::MockMockLockStore::new();
        lock_store
            .expect_query_locks()
            .withf(move |query| *query == LockQuery::OwnerRepository(owner.into(), repository))
            .return_once(move |_| {
                Ok(vec![LockData {
                    resource: LockResource::default(),
                    owner: owner.into(),
                    locked_at: 0,
                }])
            });
        lock_store
    }

    fn owners(response: Response<QueryResponse>) -> Vec<String> {
        response
            .into_inner()
            .result
            .into_iter()
            .map(|lock| lock.owner)
            .collect()
    }

    #[tokio::test]
    async fn answers_from_the_local_store_without_forwarding_configured() {
        let repository = random::<RepositoryId>();
        let lock_service = super::lock_service(store_answering(repository, "alice"));

        let response = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect("local query should succeed");

        assert_eq!(owners(response), vec!["alice".to_string()]);
    }

    #[tokio::test]
    async fn answers_from_the_local_store_when_lock_query_is_not_enabled() {
        let repository = random::<RepositoryId>();
        // Every other forwardable RPC is on, so only `lock_query` decides.
        let forwarded_requests = StubForwardedRequests::new(
            RpcFlags {
                revision_branch_create: true,
                revision_branch_delete: true,
                revision_branch_get: true,
                revision_branch_list: true,
                repository_create: true,
                repository_get: true,
                lock_query: false,
            },
            Err(InternalClientError::internal("must not be forwarded")),
        );
        let lock_service =
            lock_service_forwarding(store_answering(repository, "alice"), forwarded_requests);

        let response = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect("local query should succeed");

        assert_eq!(owners(response), vec!["alice".to_string()]);
    }

    #[tokio::test]
    async fn returns_the_peer_answer_without_reading_the_local_store() {
        let repository = random::<RepositoryId>();
        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(Ok(Ok(Response::new(QueryResponse {
                result: vec![Lock {
                    resource: None,
                    owner: "peer-owner".into(),
                    locked_at: None,
                }],
            }))));
        // A mock with no expectations panics on any call.
        let lock_service = lock_service_forwarding(
            store::MockMockLockStore::new(),
            Arc::clone(&forwarded_requests),
        );

        let response = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect("forwarded query should succeed");

        assert_eq!(owners(response), vec!["peer-owner".to_string()]);
    }

    #[tokio::test]
    async fn forwards_the_query_on_behalf_of_the_caller() {
        let repository = random::<RepositoryId>();
        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(Ok(Ok(Response::new(QueryResponse {
                result: vec![],
            }))));
        let lock_service = lock_service_forwarding(
            store::MockMockLockStore::new(),
            Arc::clone(&forwarded_requests),
        );

        lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect("forwarded query should succeed");

        let sent = forwarded_requests.sent_request();
        let caller = CallerContext::from_forwarded_request(&sent)
            .expect("the forwarded request should carry the caller");
        assert_eq!(caller.repository_id, repository);
        assert_eq!(caller.user_id, "caller");
        assert_eq!(sent.into_inner().owner.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn peer_error_reaches_the_client_as_the_peer_wrote_it() {
        let repository = random::<RepositoryId>();
        let forwarded_requests = StubForwardedRequests::forwarding_enabled(Ok(Err(
            Status::invalid_argument("unsupported lock query combination"),
        )));
        let lock_service =
            lock_service_forwarding(store::MockMockLockStore::new(), forwarded_requests);

        let err = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect_err("the peer's error should reach the client");

        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(err.message(), "unsupported lock query combination");
    }

    #[tokio::test]
    async fn unreachable_peer_is_the_origin_own_internal_error() {
        let repository = random::<RepositoryId>();
        let forwarded_requests = StubForwardedRequests::forwarding_enabled(Err(
            InternalClientError::internal("peer unreachable"),
        ));
        let lock_service =
            lock_service_forwarding(store::MockMockLockStore::new(), forwarded_requests);

        let err = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect_err("a transport failure should fail the query");

        assert_eq!(err.code(), Code::Internal);
        assert_eq!(err.message(), "Error making forwarded request");
    }
}
