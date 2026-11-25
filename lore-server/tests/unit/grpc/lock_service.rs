// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::types::Hash;
use lore_proto::LockService;
use lore_proto::lock::Resource;
use lore_revision::lock::util::LOCK_BATCH_SIZE;
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

/// `count` resources on one branch, each with its own full-length hash.
fn resources(count: usize) -> Vec<Resource> {
    (0..count)
        .map(|index| {
            let mut hash = [0u8; size_of::<Hash>()];
            hash[..size_of::<usize>()].copy_from_slice(&index.to_le_bytes());
            Resource {
                branch: Default::default(),
                hash: hash.to_vec().into(),
                description: format!("/resource/{index}"),
            }
        })
        .collect()
}

/// `message` addressed to a random repository.
fn in_repository<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(random::<RepositoryId>().data()),
    );
    request
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

            fn max_lock_batch(&self) -> Option<usize>;
        }
    }

    /// A store naming `limit` as the most resources it takes in one request.
    pub fn with_limit(limit: Option<usize>) -> MockMockLockStore {
        let mut lock_store = MockMockLockStore::new();
        lock_store.expect_max_lock_batch().return_const(limit);
        lock_store
    }
}

mod lock {
    use lore_base::types::LockData;
    use lore_base::types::LockResource;
    use lore_proto::lock::Lock;
    use lore_proto::lock::LockRequest;
    use lore_proto::lock::LockResponse;
    use lore_server::grpc::forwarded_requests::CallerContext;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use tonic::Response;

    use super::forwarding::*;
    use super::*;

    #[tokio::test]
    async fn locks_in_the_local_store_when_lock_lock_is_not_enabled() {
        let repository = random::<RepositoryId>();
        let mut lock_store = store::with_limit(None);
        lock_store
            .expect_lock_resources()
            .withf(move |owner, repo, resources| {
                owner == "caller" && *repo == repository && resources.len() == 1
            })
            .return_once(|_, _, resources| {
                Ok(vec![LockData {
                    resource: resources[0].clone(),
                    owner: "caller".into(),
                    locked_at: 0,
                }])
            });
        let lock_service = lock_service_forwarding(
            lock_store,
            Arc::new(AllowAllRepositoryAuthorizer),
            RpcFlags {
                lock_lock: false,
                ..every_rpc()
            },
            MockLockClient::new(),
        );

        let response = lock_service
            .lock(from_caller(
                LockRequest {
                    resources: one_resource(),
                },
                repository,
            ))
            .await
            .expect("local lock should succeed");

        let owners: Vec<String> = response
            .into_inner()
            .locks
            .into_iter()
            .map(|lock| lock.owner)
            .collect();
        assert_eq!(owners, vec!["caller".to_string()]);
    }

    #[tokio::test]
    async fn forwards_the_lock_on_behalf_of_the_caller_and_returns_the_peer_answer() {
        let repository = random::<RepositoryId>();
        let sent = sent::<LockRequest>();
        let keep = Arc::clone(&sent);
        let mut client = MockLockClient::new();
        client.expect_lock().return_once(move |request| {
            *keep.lock().unwrap() = Some(request);
            Ok(Ok(Response::new(LockResponse {
                locks: vec![Lock {
                    resource: Some(LockResource::default().into()),
                    owner: "peer-owner".into(),
                    locked_at: None,
                }],
            })))
        });
        let lock_service = lock_service_forwarding(
            store::with_limit(None),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            client,
        );

        let response = lock_service
            .lock(from_caller(
                LockRequest {
                    resources: one_resource(),
                },
                repository,
            ))
            .await
            .expect("forwarded lock should succeed");

        let owners: Vec<String> = response
            .into_inner()
            .locks
            .into_iter()
            .map(|lock| lock.owner)
            .collect();
        assert_eq!(owners, vec!["peer-owner".to_string()]);

        let sent = take_sent(&sent);
        let caller = CallerContext::from_forwarded_request(&sent)
            .expect("the forwarded request should carry the caller");
        assert_eq!(caller.repository_id, repository);
        assert_eq!(caller.user_id, "caller");
        assert_eq!(sent.into_inner().resources, one_resource());
    }

    #[tokio::test]
    async fn empty_request_is_answered_without_forwarding() {
        let lock_service = lock_service_forwarding(
            store::MockMockLockStore::new(),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            MockLockClient::new(),
        );

        let response = lock_service
            .lock(from_caller(
                LockRequest { resources: vec![] },
                random::<RepositoryId>(),
            ))
            .await
            .expect("an empty lock should succeed");

        assert!(response.into_inner().locks.is_empty());
    }

    #[tokio::test]
    async fn a_lock_past_the_store_limit_is_refused_without_forwarding() {
        let lock_service = lock_service_forwarding(
            store::with_limit(Some(2)),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            MockLockClient::new(),
        );

        let refused = lock_service
            .lock(from_caller(
                LockRequest {
                    resources: resources(3),
                },
                random::<RepositoryId>(),
            ))
            .await
            .expect_err("a lock past the store's limit is refused");

        assert_eq!(refused.code(), Code::OutOfRange);
    }
}

mod status {
    use lore_proto::lock::StatusRequest;
    use lore_proto::lock::StatusResponse;
    use lore_server::grpc::forwarded_requests::CallerContext;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use tonic::Response;

    use super::forwarding::*;
    use super::*;

    #[tokio::test]
    async fn resource_count_exceeds_limit() {
        let lock_service = super::lock_service(store::with_limit(None));

        let error_status = lock_service
            .status(in_repository(StatusRequest {
                resources: resources(LOCK_BATCH_SIZE + 1),
            }))
            .await
            .expect_err("Status should fail when resource count exceeds limit");

        assert_eq!(error_status.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn resource_count_at_limit() {
        let mut lock_store = store::with_limit(None);
        lock_store
            .expect_check_locks_status()
            .return_once(|_, _| Ok(vec![]));

        let lock_service = super::lock_service(lock_store);

        let _ = lock_service
            .status(in_repository(StatusRequest {
                resources: resources(LOCK_BATCH_SIZE),
            }))
            .await
            .expect("Status should succeed when resource count is at limit");
    }

    /// A resource is keyed by branch and hash; two entries differing only in description name one
    /// lock.
    #[tokio::test]
    async fn a_resource_named_twice_is_refused() {
        let lock_service = super::lock_service(store::with_limit(None));

        let mut duplicated = resources(1);
        duplicated.push(Resource {
            description: "/another/name.txt".to_owned(),
            ..duplicated[0].clone()
        });

        let refused = lock_service
            .status(in_repository(StatusRequest {
                resources: duplicated,
            }))
            .await
            .expect_err("a batch naming one key twice is refused");

        assert_eq!(refused.code(), Code::InvalidArgument);
    }

    /// A hash shorter than a full hash converts to the default hash, so two different short hashes
    /// name one key.
    #[tokio::test]
    async fn short_hashes_naming_one_key_are_refused_as_duplicates() {
        let lock_service = super::lock_service(store::with_limit(None));

        let short = |byte: u8| Resource {
            branch: Default::default(),
            hash: vec![byte].into(),
            description: format!("/short/{byte}"),
        };

        let refused = lock_service
            .status(in_repository(StatusRequest {
                resources: vec![short(1), short(2)],
            }))
            .await
            .expect_err("two hashes the store reads as one key are refused");

        assert_eq!(refused.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn the_limit_follows_what_the_store_reports() {
        let mut lock_store = store::with_limit(Some(2));
        lock_store
            .expect_check_locks_status()
            .return_once(|_, _| Ok(vec![]));

        let lock_service = super::lock_service(lock_store);

        lock_service
            .status(in_repository(StatusRequest {
                resources: resources(2),
            }))
            .await
            .expect("the store's own limit is accepted");

        let refused = lock_service
            .status(in_repository(StatusRequest {
                resources: resources(3),
            }))
            .await
            .expect_err("one past the store's limit is refused");

        assert_eq!(refused.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn reads_the_local_store_when_lock_status_is_not_enabled() {
        let repository = random::<RepositoryId>();
        let mut lock_store = store::with_limit(None);
        lock_store
            .expect_check_locks_status()
            .withf(move |repo, resources| *repo == repository && resources.len() == 1)
            .return_once(|_, _| Ok(vec![]));
        let lock_service = lock_service_forwarding(
            lock_store,
            Arc::new(AllowAllRepositoryAuthorizer),
            RpcFlags {
                lock_status: false,
                ..every_rpc()
            },
            MockLockClient::new(),
        );

        lock_service
            .status(from_caller(
                StatusRequest {
                    resources: one_resource(),
                },
                repository,
            ))
            .await
            .expect("local status should succeed");
    }

    #[tokio::test]
    async fn forwards_the_status_on_behalf_of_the_caller() {
        let repository = random::<RepositoryId>();
        let sent = sent::<StatusRequest>();
        let keep = Arc::clone(&sent);
        let mut client = MockLockClient::new();
        client.expect_status().return_once(move |request| {
            *keep.lock().unwrap() = Some(request);
            Ok(Ok(Response::new(StatusResponse { locks: vec![] })))
        });
        let lock_service = lock_service_forwarding(
            store::with_limit(None),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            client,
        );

        lock_service
            .status(from_caller(
                StatusRequest {
                    resources: one_resource(),
                },
                repository,
            ))
            .await
            .expect("forwarded status should succeed");

        let sent = take_sent(&sent);
        let caller = CallerContext::from_forwarded_request(&sent)
            .expect("the forwarded request should carry the caller");
        assert_eq!(caller.repository_id, repository);
        assert_eq!(caller.user_id, "caller");
        assert_eq!(sent.into_inner().resources, one_resource());
    }

    #[tokio::test]
    async fn resource_count_over_the_limit_is_refused_without_forwarding() {
        let repository = random::<RepositoryId>();
        let lock_service = lock_service_forwarding(
            store::with_limit(None),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            MockLockClient::new(),
        );
        let resources: Vec<Resource> = (0..101).flat_map(|_| one_resource()).collect();

        let err = lock_service
            .status(from_caller(StatusRequest { resources }, repository))
            .await
            .expect_err("status over the resource limit should fail");

        assert_eq!(err.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn empty_request_is_answered_without_forwarding() {
        let lock_service = lock_service_forwarding(
            store::MockMockLockStore::new(),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            MockLockClient::new(),
        );

        let response = lock_service
            .status(from_caller(
                StatusRequest { resources: vec![] },
                random::<RepositoryId>(),
            ))
            .await
            .expect("an empty status should succeed");

        assert!(response.into_inner().locks.is_empty());
    }
}

/// Lock, unlock and admin lock refuse an oversized request and a repeated key before asking the
/// store to apply it.
mod resource_checks {
    use lore_proto::lock::AdminLockRequest;
    use lore_proto::lock::LockRequest;
    use lore_proto::lock::UnlockRequest;

    use super::*;

    /// Two resources naming one key under different descriptions.
    fn duplicated() -> Vec<Resource> {
        let mut repeated = resources(2);
        repeated[1].hash = repeated[0].hash.clone();
        repeated
    }

    /// An admin lock of `resources` from a caller granted `migrate`, which the service checks
    /// before it looks at the resources.
    fn migrating_admin_lock(resources: Vec<Resource>) -> Request<AdminLockRequest> {
        let repository = random::<RepositoryId>();
        actions::request_with_token(
            AdminLockRequest {
                resources,
                owner: "someone".to_string(),
            },
            repository,
            Some(actions::token_granting(repository, &["migrate"])),
        )
    }

    #[tokio::test]
    async fn lock_past_the_store_limit_is_out_of_range() {
        let lock_service = super::lock_service(store::with_limit(Some(2)));

        let refused = lock_service
            .lock(in_repository(LockRequest {
                resources: resources(3),
            }))
            .await
            .expect_err("a lock request past the store's limit is refused");

        assert_eq!(refused.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn lock_past_the_default_limit_is_out_of_range() {
        let lock_service = super::lock_service(store::with_limit(None));

        let refused = lock_service
            .lock(in_repository(LockRequest {
                resources: resources(LOCK_BATCH_SIZE + 1),
            }))
            .await
            .expect_err("a lock request past the default limit is refused");

        assert_eq!(refused.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn lock_naming_one_key_twice_is_refused() {
        let lock_service = super::lock_service(store::with_limit(None));

        let refused = lock_service
            .lock(in_repository(LockRequest {
                resources: duplicated(),
            }))
            .await
            .expect_err("a lock request naming one key twice is refused");

        assert_eq!(refused.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn unlock_past_the_store_limit_is_out_of_range() {
        let lock_service = super::lock_service(store::with_limit(Some(2)));

        let refused = lock_service
            .unlock(in_repository(UnlockRequest {
                resources: resources(3),
            }))
            .await
            .expect_err("an unlock request past the store's limit is refused");

        assert_eq!(refused.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn unlock_naming_one_key_twice_is_refused() {
        let lock_service = super::lock_service(store::with_limit(None));

        let refused = lock_service
            .unlock(in_repository(UnlockRequest {
                resources: duplicated(),
            }))
            .await
            .expect_err("an unlock request naming one key twice is refused");

        assert_eq!(refused.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn admin_lock_past_the_store_limit_is_out_of_range() {
        let lock_service = lock_service_with(store::with_limit(Some(2)), actions::tier1());

        let refused = lock_service
            .admin_lock(migrating_admin_lock(resources(3)))
            .await
            .expect_err("an admin lock request past the store's limit is refused");

        assert_eq!(refused.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn admin_lock_naming_one_key_twice_is_refused() {
        let lock_service = lock_service_with(store::with_limit(None), actions::tier1());

        let refused = lock_service
            .admin_lock(migrating_admin_lock(duplicated()))
            .await
            .expect_err("an admin lock request naming one key twice is refused");

        assert_eq!(refused.code(), Code::InvalidArgument);
    }
}

mod unlock {
    use lore_proto::lock::AdminLockRequest;
    use lore_proto::lock::ForwardedUnlockRequest;
    use lore_proto::lock::LockRequest;
    use lore_proto::lock::Resource;
    use lore_proto::lock::StatusRequest;
    use lore_proto::lock::UnlockRequest;
    use lore_proto::lock::UnlockResponse;
    use lore_server::grpc::forwarded_requests::CallerContext;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use tonic::Response;

    use super::forwarding::*;
    use super::*;

    #[tokio::test]
    async fn lock_zero_resources() {
        let lock_store = store::with_limit(None);

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
        let lock_store = store::with_limit(None);

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
        let lock_store = store::with_limit(None);

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
        let lock_store = store::with_limit(None);

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
        let mut lock_store = store::with_limit(None);
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

    #[tokio::test]
    async fn unlocks_in_the_local_store_when_lock_unlock_is_not_enabled() {
        let repository = random::<RepositoryId>();
        let mut lock_store = store::with_limit(None);
        lock_store
            .expect_unlock_resources()
            .withf(move |owner, validate_user, repo, resources| {
                owner == "caller" && *validate_user && *repo == repository && resources.len() == 1
            })
            .return_once(|_, _, _, _| Ok(vec![]));
        let lock_service = lock_service_forwarding(
            lock_store,
            Arc::new(AllowAllRepositoryAuthorizer),
            RpcFlags {
                lock_unlock: false,
                ..every_rpc()
            },
            MockLockClient::new(),
        );

        lock_service
            .unlock(from_caller(
                UnlockRequest {
                    resources: one_resource(),
                },
                repository,
            ))
            .await
            .expect("local unlock should succeed");
    }

    #[tokio::test]
    async fn forwards_the_unlock_on_behalf_of_the_caller_and_returns_the_peer_answer() {
        let repository = random::<RepositoryId>();
        let sent = sent::<ForwardedUnlockRequest>();
        let keep = Arc::clone(&sent);
        let mut client = MockLockClient::new();
        client.expect_unlock().return_once(move |request| {
            *keep.lock().unwrap() = Some(request);
            Ok(Ok(Response::new(UnlockResponse {
                resources: one_resource(),
            })))
        });
        let lock_service = lock_service_forwarding(
            store::with_limit(None),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            client,
        );

        let response = lock_service
            .unlock(from_caller(
                UnlockRequest {
                    resources: one_resource(),
                },
                repository,
            ))
            .await
            .expect("forwarded unlock should succeed");

        assert_eq!(response.into_inner().resources, one_resource());
        let sent = take_sent(&sent);
        let caller = CallerContext::from_forwarded_request(&sent)
            .expect("the forwarded request should carry the caller");
        assert_eq!(caller.repository_id, repository);
        assert_eq!(caller.user_id, "caller");
        assert_eq!(sent.into_inner().resources, one_resource());
    }

    #[tokio::test]
    async fn empty_request_is_answered_without_forwarding() {
        let lock_service = lock_service_forwarding(
            store::MockMockLockStore::new(),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            MockLockClient::new(),
        );

        let response = lock_service
            .unlock(from_caller(
                UnlockRequest { resources: vec![] },
                random::<RepositoryId>(),
            ))
            .await
            .expect("an empty unlock should succeed");

        assert!(response.into_inner().resources.is_empty());
    }

    #[tokio::test]
    async fn an_unlock_past_the_store_limit_is_refused_without_forwarding() {
        let lock_service = lock_service_forwarding(
            store::with_limit(Some(2)),
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            MockLockClient::new(),
        );

        let refused = lock_service
            .unlock(from_caller(
                UnlockRequest {
                    resources: resources(3),
                },
                random::<RepositoryId>(),
            ))
            .await
            .expect_err("an unlock past the store's limit is refused");

        assert_eq!(refused.code(), Code::OutOfRange);
    }
}

mod cancelled_request {
    use lore_proto::lock::LockRequest;
    use lore_proto::lock::Resource;
    use lore_proto::lock::UnlockRequest;
    use lore_revision::runtime::Cancellation;
    use lore_server::util::core_hop::with_request_cancellation;

    use super::*;

    fn one_resource() -> Vec<Resource> {
        vec![Resource {
            branch: Default::default(),
            hash: Default::default(),
            description: "".to_string(),
        }]
    }

    fn with_repository<T>(message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(random::<RepositoryId>().data()),
        );
        request
    }

    fn cancelled() -> Cancellation {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        cancellation
    }

    /// A cancelled lock answers CANCELLED without asking the lock store for a lock, which the mock
    /// refuses.
    #[tokio::test]
    async fn a_cancelled_lock_takes_no_lock() {
        let lock_service = super::lock_service(super::store::with_limit(None));

        let status = with_request_cancellation(
            cancelled(),
            lock_service.lock(with_repository(LockRequest {
                resources: one_resource(),
            })),
        )
        .await
        .expect_err("a cancelled lock stops before it locks");
        assert_eq!(status.code(), Code::Cancelled, "{status:?}");
    }

    /// A cancelled unlock answers CANCELLED without asking the lock store to unlock, which the mock
    /// refuses.
    #[tokio::test]
    async fn a_cancelled_unlock_releases_no_lock() {
        let lock_service = super::lock_service(super::store::with_limit(None));

        let status = with_request_cancellation(
            cancelled(),
            lock_service.unlock(with_repository(UnlockRequest {
                resources: one_resource(),
            })),
        )
        .await
        .expect_err("a cancelled unlock stops before it unlocks");
        assert_eq!(status.code(), Code::Cancelled, "{status:?}");
    }
}

/// Elevated action checks: unlock elevates on `owner`/`admin`,
/// admin-lock requires `migrate`, both answered by the configured
/// authorizer from the verified token — on both OIDC tiers.
mod actions {
    use lore_proto::lock::AdminLockRequest;
    use lore_proto::lock::UnlockRequest;
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::auth::jwt::ResourcePermission;
    use lore_server::authnz::global_grants_authorizer::GlobalGrantsAuthorizer;
    use lore_server::authnz::repository_authorizer::RawToken;
    use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
    use serde_json::json;

    use super::forwarding::one_resource;
    use super::*;

    /// Tier 1, reading the actions from a dotted global claim.
    pub(super) fn tier1() -> Arc<dyn RepositoryAuthorizer> {
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
    pub(super) fn token_granting(repository: RepositoryId, actions: &[&str]) -> AuthorizationToken {
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

    pub(super) fn request_with_token<T>(
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

    /// The store records the `validate_user` flag it was called with,
    /// which is what elevation waives.
    fn store_expecting_validate_user(validate: bool) -> store::MockMockLockStore {
        let mut lock_store = store::with_limit(None);
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

    /// The peer can't see the caller's grants, so the forwarded unlock
    /// carries the origin's elevation decision.
    #[tokio::test]
    async fn forwarded_unlock_carries_whether_the_caller_is_elevated() {
        for (action, elevated_caller) in [("owner", true), ("admin", true), ("write", false)] {
            let repository = random::<RepositoryId>();
            let mut client = super::forwarding::MockLockClient::new();
            client
                .expect_unlock()
                .withf(move |request| request.get_ref().elevated_caller == elevated_caller)
                .return_once(|_| {
                    Ok(Ok(tonic::Response::new(lore_proto::lock::UnlockResponse {
                        resources: vec![],
                    })))
                });
            let lock_service = super::forwarding::lock_service_forwarding(
                store::with_limit(None),
                tier2(),
                super::forwarding::every_rpc(),
                client,
            );

            lock_service
                .unlock(request_with_token(
                    UnlockRequest {
                        resources: one_resource(),
                    },
                    repository,
                    Some(token_granting(repository, &[action])),
                ))
                .await
                .expect("forwarded unlock should succeed");
        }
    }

    /// The origin authorizes before forwarding: a caller without `write` is
    /// refused there and nothing reaches the peer.
    #[tokio::test]
    async fn forwarded_lock_and_unlock_without_write_are_refused_before_forwarding() {
        let repository = random::<RepositoryId>();
        let lock_service = super::forwarding::lock_service_forwarding(
            store::MockMockLockStore::new(),
            tier2(),
            super::forwarding::every_rpc(),
            super::forwarding::MockLockClient::new(),
        );

        let denied = lock_service
            .lock(request_with_token(
                lore_proto::lock::LockRequest {
                    resources: one_resource(),
                },
                repository,
                Some(token_granting(repository, &["read"])),
            ))
            .await
            .expect_err("lock without `write` is refused");
        assert_eq!(denied.code(), Code::PermissionDenied);

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
            let mut lock_store = store::with_limit(None);
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

        let lock_service = super::lock_service(store::with_limit(None));
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

/// A peer for the forwarding tests: `RpcFlags` decide which RPCs forward, and
/// a mock client answers the one call a test forwards. A mock with no
/// expectations panics on any call, so it also checks nothing was forwarded.
pub(super) mod forwarding {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lore_proto::lock::ForwardedUnlockRequest;
    use lore_proto::lock::LockRequest;
    use lore_proto::lock::LockResponse;
    use lore_proto::lock::QueryRequest;
    use lore_proto::lock::QueryResponse;
    use lore_proto::lock::StatusRequest;
    use lore_proto::lock::StatusResponse;
    use lore_proto::lock::UnlockResponse;
    use lore_server::grpc::forwarded_requests::ForwardedRequestResult;
    use lore_server::grpc::forwarded_requests::ForwardedRequests;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use lore_server::grpc::forwarded_requests::lock_service::ForwardedLockServiceClient;
    use lore_server::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
    use lore_server::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;

    use super::*;

    mockall::mock! {
        pub LockClient {}

        #[async_trait]
        impl ForwardedLockServiceClient for LockClient {
            async fn query(
                &mut self,
                request: Request<QueryRequest>,
            ) -> ForwardedRequestResult<QueryResponse>;

            async fn lock(
                &mut self,
                request: Request<LockRequest>,
            ) -> ForwardedRequestResult<LockResponse>;

            async fn status(
                &mut self,
                request: Request<StatusRequest>,
            ) -> ForwardedRequestResult<StatusResponse>;

            async fn unlock(
                &mut self,
                request: Request<ForwardedUnlockRequest>,
            ) -> ForwardedRequestResult<UnlockResponse>;
        }
    }

    pub struct StubForwardedRequests {
        flags: RpcFlags,
        client: Mutex<Option<MockLockClient>>,
    }

    impl ForwardedRequests for StubForwardedRequests {
        fn rpc_flags(&self) -> &RpcFlags {
            &self.flags
        }

        fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
            unreachable!("forwarded_revision_service should not be called in lock tests")
        }

        fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient> {
            unreachable!("forwarded_repository_service should not be called in lock tests")
        }

        fn forwarded_lock_service(&self) -> Box<dyn ForwardedLockServiceClient> {
            Box::new(
                self.client
                    .lock()
                    .unwrap()
                    .take()
                    .expect("a lock RPC was forwarded more than once"),
            )
        }
    }

    /// Flags with every forwardable RPC on.
    pub fn every_rpc() -> RpcFlags {
        RpcFlags {
            revision_branch_create: true,
            revision_branch_delete: true,
            revision_branch_get: true,
            revision_branch_list: true,
            repository_create: true,
            repository_get: true,
            lock_query: true,
            lock_lock: true,
            lock_status: true,
            lock_unlock: true,
        }
    }

    pub fn lock_service_forwarding(
        lock_store: store::MockMockLockStore,
        authorizer: Arc<dyn RepositoryAuthorizer>,
        flags: RpcFlags,
        client: MockLockClient,
    ) -> LoreLockService {
        LoreLockService::new(
            Arc::new(lock_store),
            Arc::new(lore_server::notification::local::NotificationSender::default()),
            authorizer,
            Some(Arc::new(StubForwardedRequests {
                flags,
                client: Mutex::new(Some(client)),
            })),
            Duration::from_secs(60),
        )
    }

    /// A request from the authenticated user `caller` against `repository`.
    pub fn from_caller<T>(body: T, repository: RepositoryId) -> Request<T> {
        let mut request = Request::new(body);
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
            .extensions_mut()
            .insert(lore_server::auth::jwt::AuthorizationToken {
                user_id: "caller".into(),
                ..Default::default()
            });
        request
    }

    /// One resource in the shape a server returns it, so it compares equal
    /// after a trip through the lock types.
    pub fn one_resource() -> Vec<lore_proto::lock::Resource> {
        vec![
            lore_base::types::LockResource {
                description: "a/file.uasset".to_string(),
                ..Default::default()
            }
            .into(),
        ]
    }

    /// Holds the request a mock client was sent.
    pub type Sent<T> = Arc<Mutex<Option<Request<T>>>>;

    pub fn sent<T>() -> Sent<T> {
        Arc::new(Mutex::new(None))
    }

    pub fn take_sent<T>(sent: &Sent<T>) -> Request<T> {
        sent.lock()
            .unwrap()
            .take()
            .expect("no request was forwarded")
    }
}

mod query {
    use lore_base::types::LockData;
    use lore_base::types::LockResource;
    use lore_proto::lock::Lock;
    use lore_proto::lock::QueryRequest;
    use lore_proto::lock::QueryResponse;
    use lore_revision::lock::LockQuery;
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::grpc::forwarded_requests::CallerContext;
    use lore_server::grpc::forwarded_requests::InternalClientError;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use tonic::Response;
    use tonic::Status;

    use super::forwarding::*;
    use super::*;

    fn forwarding_query(
        lock_store: store::MockMockLockStore,
        client: MockLockClient,
    ) -> LoreLockService {
        lock_service_forwarding(
            lock_store,
            Arc::new(AllowAllRepositoryAuthorizer),
            every_rpc(),
            client,
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
        let lock_service = lock_service_forwarding(
            store_answering(repository, "alice"),
            Arc::new(AllowAllRepositoryAuthorizer),
            RpcFlags {
                lock_query: false,
                ..every_rpc()
            },
            MockLockClient::new(),
        );

        let response = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect("local query should succeed");

        assert_eq!(owners(response), vec!["alice".to_string()]);
    }

    #[tokio::test]
    async fn returns_the_peer_answer_without_reading_the_local_store() {
        let repository = random::<RepositoryId>();
        let mut client = MockLockClient::new();
        client.expect_query().return_once(|_| {
            Ok(Ok(Response::new(QueryResponse {
                result: vec![Lock {
                    resource: None,
                    owner: "peer-owner".into(),
                    locked_at: None,
                }],
            })))
        });
        let lock_service = forwarding_query(store::MockMockLockStore::new(), client);

        let response = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect("forwarded query should succeed");

        assert_eq!(owners(response), vec!["peer-owner".to_string()]);
    }

    #[tokio::test]
    async fn forwards_the_query_on_behalf_of_the_caller() {
        let repository = random::<RepositoryId>();
        let sent = sent::<QueryRequest>();
        let mut client = MockLockClient::new();
        let keep = Arc::clone(&sent);
        client.expect_query().return_once(move |request| {
            *keep.lock().unwrap() = Some(request);
            Ok(Ok(Response::new(QueryResponse { result: vec![] })))
        });
        let lock_service = forwarding_query(store::MockMockLockStore::new(), client);

        lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect("forwarded query should succeed");

        let sent = take_sent(&sent);
        let caller = CallerContext::from_forwarded_request(&sent)
            .expect("the forwarded request should carry the caller");
        assert_eq!(caller.repository_id, repository);
        assert_eq!(caller.user_id, "caller");
        assert_eq!(sent.into_inner().owner.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn peer_error_reaches_the_client_as_the_peer_wrote_it() {
        let repository = random::<RepositoryId>();
        let mut client = MockLockClient::new();
        client.expect_query().return_once(|_| {
            Ok(Err(Status::invalid_argument(
                "unsupported lock query combination",
            )))
        });
        let lock_service = forwarding_query(store::MockMockLockStore::new(), client);

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
        let mut client = MockLockClient::new();
        client
            .expect_query()
            .return_once(|_| Err(InternalClientError::internal("peer unreachable")));
        let lock_service = forwarding_query(store::MockMockLockStore::new(), client);

        let err = lock_service
            .query(query_by_owner(repository, "alice"))
            .await
            .expect_err("a transport failure should fail the query");

        assert_eq!(err.code(), Code::Internal);
        assert_eq!(err.message(), "Error making forwarded request");
    }
}
