// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::RepositoryDeleteRequest;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::global_grants_authorizer::GlobalGrantsAuthorizer;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_registry::RepositoryRegistry;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::repository_delete::*;
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
    groups: &[&str],
    is_service_account: bool,
) -> Request<RepositoryDeleteRequest> {
    let mut request = delete_request(
        id,
        AuthorizationToken {
            identity: Some("mallory".to_string()),
            groups: Some(groups.iter().map(ToString::to_string).collect()),
            is_service_account: Some(is_service_account),
            ..Default::default()
        },
    );
    request
        .extensions_mut()
        .insert(RawToken("raw.jwt".to_string()));
    request
}

/// `owner` lets a non-creator delete; `is_service_account` alone no longer
/// does.
#[tokio::test]
async fn owner_not_service_account_lets_a_non_creator_delete() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let id = random::<RepositoryId>();
        seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;

        let status = handler(
            verified_delete_request(id, &["write"], true),
            &RepositoryRegistry::Unregistered,
            &global_grants(),
            immutable_store.clone(),
            mutable_store.clone(),
            &TestInstrumentProvider,
        )
        .await
        .expect_err("a service account without owner or admin may not delete");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);

        handler(
            verified_delete_request(id, &["owner"], false),
            &RepositoryRegistry::Unregistered,
            &global_grants(),
            immutable_store,
            mutable_store,
            &TestInstrumentProvider,
        )
        .await
        .expect("owner permits the delete");
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

/// A repository delete, by the handler or the v1 handler, releases every name's mapping, whatever
/// name the branch's metadata holds, and deletes the latest and metadata of every branch a name
/// maps to. The branch named `feature` in its metadata is mapped only from the name `alias`, while
/// `feature` maps to the branch `other`.
#[tokio::test]
async fn teardown_releases_every_name_mapping() {
    for v1 in [false, true] {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let id = random::<RepositoryId>();
            seed_repository(immutable_store.clone(), mutable_store.clone(), id, "alice").await;
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                id,
            ));
            let write_token = get_write_token();
            let mut branches = Vec::new();
            for name in ["feature", "other"] {
                let created = branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    name,
                    branch::default_category(),
                    "alice",
                    1,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create the branch");
                branch::store_latest(
                    repository.clone(),
                    created,
                    Hash::default(),
                    Hash::from([1; 32]),
                    BranchLatestStatus::Convergent,
                )
                .await
                .expect("Failed to store the latest revision");
                branches.push(created);
            }
            let [feature, other] = branches[..] else {
                unreachable!("two branches are created");
            };
            branch::claim_name(repository.clone(), "feature", feature, other)
                .await
                .expect("the name moves to the other branch");
            branch::claim_name(repository.clone(), "alias", Context::default(), feature)
                .await
                .expect("an absent name is claimed");

            let request = verified_delete_request(id, &["owner"], false);
            if v1 {
                let (metadata, extensions, request) = request.into_parts();
                lore_server::grpc::repository::v1::repository_delete::handler(
                    Request::from_parts(
                        metadata,
                        extensions,
                        lore_proto::lore::repository::v1::RepositoryDeleteRequest {
                            id: request.id,
                        },
                    ),
                    &RepositoryRegistry::Unregistered,
                    &global_grants(),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &TestInstrumentProvider,
                )
                .await
                .map(|_| ())
            } else {
                handler(
                    request,
                    &RepositoryRegistry::Unregistered,
                    &global_grants(),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &TestInstrumentProvider,
                )
                .await
                .map(|_| ())
            }
            .expect("owner permits the delete");

            for name in ["feature", "other", "alias"] {
                assert!(
                    branch::load_name_to_id_local(repository.clone(), name)
                        .await
                        .is_err_and(|err| err.is_branch_not_found()),
                    "v1 {v1}: the name {name} is released"
                );
            }
            for branch_id in [feature, other] {
                assert!(
                    branch::metadata_hash(repository.clone(), branch_id)
                        .await
                        .is_err_and(|err| err.is_branch_not_found()),
                    "v1 {v1}: the metadata of {branch_id} is deleted"
                );
                assert!(
                    branch::load_latest_local(repository.clone(), branch_id)
                        .await
                        .is_err_and(|err| err.is_branch_not_found()),
                    "v1 {v1}: the latest of {branch_id} is deleted"
                );
            }
        }))
        .await;
    }
}

mod zero_repository_id {
    use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;

    use super::*;

    /// The all-zero id names the namespace-root partition: the delete is
    /// refused and whatever is stored there stays in place.
    #[tokio::test]
    async fn is_refused_and_deletes_nothing() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let namespace_root = RepositoryId::default();
            seed_repository(
                immutable_store.clone(),
                mutable_store.clone(),
                namespace_root,
                "alice",
            )
            .await;

            let status = handler(
                delete_request(namespace_root, AuthorizationToken::default()),
                &RepositoryRegistry::Unregistered,
                &(Arc::new(AllowAllRepositoryAuthorizer) as Arc<dyn RepositoryAuthorizer>),
                immutable_store.clone(),
                mutable_store.clone(),
                &TestInstrumentProvider,
            )
            .await
            .expect_err("the zero id must be refused");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);

            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                namespace_root,
            ));
            repository::metadata_hash(repository)
                .await
                .expect("the stored metadata must survive");
        }))
        .await;
    }
}
