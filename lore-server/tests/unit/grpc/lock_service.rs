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
        Duration::from_secs(60),
    )
}

fn lock_service(lock_store: store::MockMockLockStore) -> LoreLockService {
    lock_service_with(lock_store, Arc::new(AllowAllRepositoryAuthorizer))
}

mod store {
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
