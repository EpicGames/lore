// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::lore::RepositoryId;
use lore_server::grpc::forwarded_requests::CallerContext;
use tonic::Request;

use crate::grpc::lock_service::forwarding::one_resource;

/// `body` as a peer forwards it on behalf of `alice`.
fn forwarded<T>(repository: RepositoryId, body: T) -> Request<T> {
    CallerContext {
        repository_id: repository,
        user_id: "alice".into(),
        is_service_account: false,
        correlation_id: String::new(),
        authorization: None,
    }
    .to_forwarded_request(body)
    .expect("CallerContext::to_forwarded_request failed in test")
}

mod query_handler {
    use lore_base::types::LockData;
    use lore_base::types::LockResource;
    use lore_proto::lock::QueryRequest;
    use lore_revision::lock::LockQuery;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_server::grpc::forwarded_lock_service::query_handler;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Code;
    use tonic::Request;

    use super::forwarded;
    use crate::grpc::lock_service::store::MockMockLockStore;

    #[tokio::test]
    async fn missing_user_id_returns_internal_error() {
        let repository = random::<RepositoryId>();
        let mut request = Request::new(QueryRequest::default());
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let err = query_handler(request, &MockMockLockStore::new())
            .await
            .expect_err("a request without the caller should fail");

        assert_eq!(err.code(), Code::Internal);
        assert!(err.message().contains("on-behalf-of-user-id"));
    }

    /// The repository comes from the forwarded metadata, which the origin
    /// stamped from the partition it authorized the caller for.
    #[tokio::test]
    async fn queries_the_repository_named_in_the_forwarded_metadata() {
        let repository = random::<RepositoryId>();
        let branch = random::<BranchId>();
        let mut lock_store = MockMockLockStore::new();
        lock_store
            .expect_query_locks()
            .withf(move |query| {
                *query == LockQuery::OwnerRepositoryBranch("bob".into(), repository, branch)
            })
            .return_once(|_| {
                Ok(vec![LockData {
                    resource: LockResource::default(),
                    owner: "bob".into(),
                    locked_at: 0,
                }])
            });

        let response = query_handler(
            forwarded(
                repository,
                QueryRequest {
                    branch: Some(branch.into()),
                    owner: Some("bob".into()),
                    description: None,
                },
            ),
            &lock_store,
        )
        .await
        .expect("forwarded query should succeed");

        let owners: Vec<String> = response
            .into_inner()
            .result
            .into_iter()
            .map(|lock| lock.owner)
            .collect();
        assert_eq!(owners, vec!["bob".to_string()]);
    }

    #[tokio::test]
    async fn unsupported_combination_is_invalid_argument() {
        let repository = random::<RepositoryId>();

        let err = query_handler(
            forwarded(
                repository,
                QueryRequest {
                    branch: None,
                    owner: Some("bob".into()),
                    description: Some("a/file.uasset".into()),
                },
            ),
            &MockMockLockStore::new(),
        )
        .await
        .expect_err("owner with description is not a supported query");

        assert_eq!(err.code(), Code::InvalidArgument);
    }
}

mod lock_handler {
    use lore_base::types::LockData;
    use lore_proto::lock::LockRequest;
    use lore_server::grpc::forwarded_lock_service::lock_handler;
    use rand::random;
    use tonic::Code;

    use super::*;
    use crate::grpc::lock_service::store::with_limit;
    use crate::notification::testing::MockNotificationSender;

    /// The peer locks as the forwarded caller, in the forwarded repository,
    /// and announces the locks itself.
    #[tokio::test]
    async fn locks_for_the_forwarded_caller_and_announces_the_locks() {
        let repository = random::<RepositoryId>();
        let mut lock_store = with_limit(None);
        lock_store
            .expect_lock_resources()
            .withf(move |owner, repo, resources| {
                owner == "alice" && *repo == repository && resources.len() == 1
            })
            .return_once(|_, _, resources| {
                Ok(vec![LockData {
                    resource: resources[0].clone(),
                    owner: "alice".into(),
                    locked_at: 0,
                }])
            });
        let mut notification = MockNotificationSender::new();
        notification
            .expect_resource_locked()
            .withf(move |repo, _, user, resources| {
                *repo == repository && user == "alice" && resources.len() == 1
            })
            .times(1)
            .return_const(());

        let response = lock_handler(
            forwarded(
                repository,
                LockRequest {
                    resources: one_resource(),
                },
            ),
            &lock_store,
            &notification,
        )
        .await
        .expect("forwarded lock should succeed");

        let owners: Vec<String> = response
            .into_inner()
            .locks
            .into_iter()
            .map(|lock| lock.owner)
            .collect();
        assert_eq!(owners, vec!["alice".to_string()]);
    }

    /// The peer refuses a lock its own store cannot take, whatever the forwarding server checked.
    #[tokio::test]
    async fn a_lock_past_the_store_limit_is_refused() {
        let resources = (0..3).flat_map(|_| one_resource()).collect();

        let err = lock_handler(
            forwarded(random::<RepositoryId>(), LockRequest { resources }),
            &with_limit(Some(2)),
            &MockNotificationSender::new(),
        )
        .await
        .expect_err("a lock past the store's limit should fail");

        assert_eq!(err.code(), Code::OutOfRange);
    }
}

mod status_handler {
    use lore_base::types::LockData;
    use lore_proto::lock::StatusRequest;
    use lore_server::grpc::forwarded_lock_service::status_handler;
    use rand::random;
    use tonic::Code;

    use super::*;
    use crate::grpc::lock_service::store::with_limit;

    /// The peer bounds the resources it reads itself, whatever the
    /// forwarding server checked.
    #[tokio::test]
    async fn resource_count_over_the_limit_is_refused() {
        let repository = random::<RepositoryId>();
        let resources = (0..101).flat_map(|_| one_resource()).collect();

        let err = status_handler(
            forwarded(repository, StatusRequest { resources }),
            &with_limit(None),
        )
        .await
        .expect_err("status over the resource limit should fail");

        assert_eq!(err.code(), Code::OutOfRange);
    }

    #[tokio::test]
    async fn answers_from_the_store_for_the_forwarded_repository() {
        let repository = random::<RepositoryId>();
        let mut lock_store = with_limit(None);
        lock_store
            .expect_check_locks_status()
            .withf(move |repo, resources| *repo == repository && resources.len() == 1)
            .return_once(|_, resources| {
                Ok(vec![LockData {
                    resource: resources[0].clone(),
                    owner: "bob".into(),
                    locked_at: 0,
                }])
            });

        let response = status_handler(
            forwarded(
                repository,
                StatusRequest {
                    resources: one_resource(),
                },
            ),
            &lock_store,
        )
        .await
        .expect("forwarded status should succeed");

        let owners: Vec<String> = response
            .into_inner()
            .locks
            .into_iter()
            .map(|lock| lock.owner)
            .collect();
        assert_eq!(owners, vec!["bob".to_string()]);
    }
}

mod unlock_handler {
    use lore_proto::lock::ForwardedUnlockRequest;
    use lore_server::grpc::forwarded_lock_service::unlock_handler;
    use rand::random;
    use tonic::Code;

    use super::*;
    use crate::grpc::lock_service::store::with_limit;
    use crate::notification::testing::MockNotificationSender;

    /// Unless the origin marked the caller elevated, the store checks that
    /// the caller holds each lock.
    #[tokio::test]
    async fn checks_ownership_unless_the_caller_is_elevated() {
        for (elevated_caller, validate) in [(false, true), (true, false)] {
            let repository = random::<RepositoryId>();
            let mut lock_store = with_limit(None);
            lock_store
                .expect_unlock_resources()
                .withf(move |owner, validate_user, repo, _| {
                    owner == "alice" && *validate_user == validate && *repo == repository
                })
                .return_once(|_, _, _, _| Ok(vec![]));

            unlock_handler(
                forwarded(
                    repository,
                    ForwardedUnlockRequest {
                        resources: one_resource(),
                        elevated_caller,
                    },
                ),
                &lock_store,
                &MockNotificationSender::new(),
            )
            .await
            .expect("forwarded unlock should succeed");
        }
    }

    /// The peer announces the resources it released, as the caller.
    #[tokio::test]
    async fn announces_the_released_resources() {
        let repository = random::<RepositoryId>();
        let mut lock_store = with_limit(None);
        lock_store
            .expect_unlock_resources()
            .return_once(|_, _, _, resources| Ok(resources.to_vec()));
        let mut notification = MockNotificationSender::new();
        notification
            .expect_resource_unlocked()
            .withf(move |repo, _, user, resources| {
                *repo == repository && user == "alice" && resources.len() == 1
            })
            .times(1)
            .return_const(());

        let response = unlock_handler(
            forwarded(
                repository,
                ForwardedUnlockRequest {
                    resources: one_resource(),
                    elevated_caller: false,
                },
            ),
            &lock_store,
            &notification,
        )
        .await
        .expect("forwarded unlock should succeed");

        assert_eq!(response.into_inner().resources, one_resource());
    }

    /// The peer refuses an unlock its own store cannot take, whatever the forwarding server
    /// checked.
    #[tokio::test]
    async fn an_unlock_past_the_store_limit_is_refused() {
        let resources = (0..3).flat_map(|_| one_resource()).collect();

        let err = unlock_handler(
            forwarded(
                random::<RepositoryId>(),
                ForwardedUnlockRequest {
                    resources,
                    elevated_caller: false,
                },
            ),
            &with_limit(Some(2)),
            &MockNotificationSender::new(),
        )
        .await
        .expect_err("an unlock past the store's limit should fail");

        assert_eq!(err.code(), Code::OutOfRange);
    }
}
