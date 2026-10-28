// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1::RepositoryDeleteRequest;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::global_grants_authorizer::GlobalGrantsAuthorizer;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_registry::RepositoryRegistry;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use lore_server::grpc::repository::v1::repository_delete::*;
use lore_telemetry::InstrumentProvider;
use rand::random;
use tonic::Request;

use crate::store::test_support::test_store_create;

struct TestInstrumentProvider;

impl InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
}

async fn seed_repository(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    id: RepositoryId,
    creator: &str,
) {
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        id,
    ));
    let metadata_hash = repository::metadata_store(
        repository.clone(),
        RepositoryMetadata {
            name: "the-repository".to_string(),
            creator: creator.to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("Failed to store repository metadata");
    repository::metadata_store_hash(repository.clone(), metadata_hash)
        .await
        .expect("Failed to store repository metadata hash");
    repository::store_name_to_id(repository, "the-repository", id)
        .await
        .expect("Failed to store repository name to id mapping");
}

fn delete_request(id: RepositoryId, token: AuthorizationToken) -> Request<RepositoryDeleteRequest> {
    let id_bytes: Context = id.into();
    let mut request = Request::new(RepositoryDeleteRequest {
        id: id_bytes.into(),
    });
    request.extensions_mut().insert(token);
    request
}

fn allow_all() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(AllowAllRepositoryAuthorizer)
}

/// A verifier configured with global grants read from `groups`.
fn global_grants() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(GlobalGrantsAuthorizer::new(
        Some("groups".to_string()),
        vec![Action::Read, Action::Write],
    ))
}

/// A Tier 2 verifier reading per-repository grants from `resources`.
fn resource_grants() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(ResourceGrantsAuthorizer::new(
        "resources".to_string(),
        "resource_id".to_string(),
        None,
        "urc-{id}".to_string(),
        "urc-*".to_string(),
    ))
}

/// A request to delete `repository` by its creator `alice`, whose verified
/// token holds `read` on `granted` only.
fn creator_request(
    repository: RepositoryId,
    granted: RepositoryId,
) -> Request<RepositoryDeleteRequest> {
    let mut request = delete_request(
        repository,
        AuthorizationToken {
            identity: Some("alice".to_string()),
            resources: Some(vec![ResourcePermission {
                resource_id: format!("urc-{granted}"),
                permission: vec!["write".to_string()],
            }]),
            ..Default::default()
        },
    );
    request
        .extensions_mut()
        .insert(RawToken("raw.jwt".to_string()));
    request
}

fn verified_delete_request(
    id: RepositoryId,
    token: AuthorizationToken,
) -> Request<RepositoryDeleteRequest> {
    let mut request = delete_request(id, token);
    request
        .extensions_mut()
        .insert(RawToken("raw.jwt".to_string()));
    request
}

fn non_creator(groups: &[&str]) -> AuthorizationToken {
    AuthorizationToken {
        identity: Some("mallory".to_string()),
        groups: Some(groups.iter().map(ToString::to_string).collect()),
        ..Default::default()
    }
}

/// The creator check compares the recorded creator against the identity the
/// verifier resolved, so an issuer minting `preferred_username` into the
/// `identity` claim lets the same user delete, while the subject minted
/// alongside is not what is compared.
#[tokio::test]
async fn the_creator_check_compares_the_resolved_identity() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let id = random::<RepositoryId>();
        seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;

        let by_subject = AuthorizationToken {
            user_id: "f7d3a1c2-0000-0000-0000-000000000000".to_string(),
            preferred_username: Some("alice".to_string()),
            ..Default::default()
        };
        let status = handler(
            verified_delete_request(id, by_subject.clone()),
            &RepositoryRegistry::Unregistered,
            &global_grants(),
            immutable_store.clone(),
            mutable_store.clone(),
            &TestInstrumentProvider,
        )
        .await
        .expect_err("the subject is not the recorded creator");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);

        let by_username = AuthorizationToken {
            identity: Some("alice".to_string()),
            ..by_subject
        };
        handler(
            verified_delete_request(id, by_username),
            &RepositoryRegistry::Unregistered,
            &global_grants(),
            immutable_store,
            mutable_store,
            &TestInstrumentProvider,
        )
        .await
        .expect("the identity claim matches the recorded creator");
    }))
    .await;
}

/// With either registry the handler authorizes the delete from the token
/// before asking the registry: a non-creator without `owner` or `admin` is
/// refused locally, and one with `owner` goes on to the registry, where a
/// failed remove leaves the repository in place. Nothing listens at the
/// registry's address.
#[tokio::test]
async fn a_registry_is_asked_only_after_the_token_check() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        for registry in [
            RepositoryRegistry::AuthService("http://127.0.0.1:1".to_string()),
            RepositoryRegistry::UserService("http://127.0.0.1:1".to_string()),
        ] {
            let id = random::<RepositoryId>();
            seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;

            let status = handler(
                verified_delete_request(id, non_creator(&["write"])),
                &registry,
                &global_grants(),
                immutable_store.clone(),
                mutable_store.clone(),
                &TestInstrumentProvider,
            )
            .await
            .expect_err("write does not permit the delete");
            assert_eq!(
                status.code(),
                tonic::Code::PermissionDenied,
                "{registry}: refused before asking the registry"
            );

            let status = handler(
                verified_delete_request(id, non_creator(&["owner"])),
                &registry,
                &global_grants(),
                immutable_store.clone(),
                mutable_store.clone(),
                &TestInstrumentProvider,
            )
            .await
            .expect_err("the unreachable registry fails the delete");
            assert_ne!(
                status.code(),
                tonic::Code::PermissionDenied,
                "{registry}: owner passes the local check and reaches the registry"
            );

            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                id,
            ));
            assert_eq!(
                repository::id_from_name(repository, "the-repository")
                    .await
                    .expect("the name mapping survives a failed delete"),
                id
            );
        }
    }))
    .await;
}

/// With no verifier there is no identity to compare — every caller is
/// `<unknown>`, the recorded creator is whatever the client sent — so the
/// `write` allow-all grants decides alone.
#[tokio::test]
async fn without_a_verifier_write_alone_permits_the_delete() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let id = random::<RepositoryId>();
        seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;

        handler(
            delete_request(id, non_creator(&[])),
            &RepositoryRegistry::Unregistered,
            &allow_all(),
            immutable_store,
            mutable_store,
            &TestInstrumentProvider,
        )
        .await
        .expect("allow-all grants write");
    }))
    .await;
}

#[tokio::test]
async fn owner_or_admin_lets_a_non_creator_delete() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        for action in ["owner", "admin"] {
            let id = random::<RepositoryId>();
            seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;

            handler(
                verified_delete_request(id, non_creator(&[action])),
                &RepositoryRegistry::Unregistered,
                &global_grants(),
                immutable_store.clone(),
                mutable_store.clone(),
                &TestInstrumentProvider,
            )
            .await
            .unwrap_or_else(|err| panic!("{action} permits the delete: {err}"));
        }
    }))
    .await;
}

/// `is_service_account` no longer waives the creator check.
#[tokio::test]
async fn a_non_creator_without_owner_or_admin_is_refused() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let id = random::<RepositoryId>();
        seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;
        let token = AuthorizationToken {
            is_service_account: Some(true),
            ..non_creator(&["write"])
        };

        let status = handler(
            verified_delete_request(id, token),
            &RepositoryRegistry::Unregistered,
            &global_grants(),
            immutable_store,
            mutable_store,
            &TestInstrumentProvider,
        )
        .await
        .expect_err("write does not permit the delete");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }))
    .await;
}

#[tokio::test]
async fn the_creator_may_delete_without_owner_or_admin() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let id = random::<RepositoryId>();
        seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;
        let token = AuthorizationToken {
            identity: Some("alice".to_string()),
            ..non_creator(&[])
        };

        handler(
            verified_delete_request(id, token),
            &RepositoryRegistry::Unregistered,
            &global_grants(),
            immutable_store,
            mutable_store,
            &TestInstrumentProvider,
        )
        .await
        .expect("the creator may delete");
    }))
    .await;
}

/// The creator may delete only while they hold `write` on the repository:
/// on Tier 2 a creator whose token grants another repository only is
/// refused, and `write` on this one restores the creator's right.
#[tokio::test]
async fn the_creator_must_reach_the_repository_to_delete() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let id = random::<RepositoryId>();
        seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;

        let status = handler(
            creator_request(id, random::<RepositoryId>()),
            &RepositoryRegistry::Unregistered,
            &resource_grants(),
            immutable_store.clone(),
            mutable_store.clone(),
            &TestInstrumentProvider,
        )
        .await
        .expect_err("a creator with no grant on the repository may not delete");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);

        handler(
            creator_request(id, id),
            &RepositoryRegistry::Unregistered,
            &resource_grants(),
            immutable_store,
            mutable_store,
            &TestInstrumentProvider,
        )
        .await
        .expect("a creator who reaches the repository may delete");
    }))
    .await;
}
