// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::RepositoryId;
use lore_proto::RepositoryQueryRequest;
use lore_revision::repository;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::handlers::repository_query::*;
use rand::random;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::authnz::repository_authorizer::RefusingAuthorizer;
use crate::store::test_support::RacedNameWriteStore;
use crate::store::test_support::bind_without_metadata;
use crate::store::test_support::bound_id;
use crate::store::test_support::seed_metadata;
use crate::store::test_support::test_store_create;
use crate::store::test_support::unbound;

/// Denies every request.
struct DenyAllRepositoryAuthorizer;

#[async_trait::async_trait]
impl RepositoryAuthorizer for DenyAllRepositoryAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        _action: Action,
    ) -> Result<(), Status> {
        Err(Status::permission_denied("denied"))
    }
}

/// Writes the metadata blob, its pointer and the name → id mapping so a
/// query for `name` or `id` resolves.
async fn seed_repository(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    id: RepositoryId,
    name: &str,
) {
    let repository = seed_metadata(immutable_store, mutable_store, id, name).await;
    repository::store_name_to_id(repository, name, id)
        .await
        .expect("Failed to store repository name to id mapping");
}

fn query_by_id(id: RepositoryId) -> Request<RepositoryQueryRequest> {
    Request::new(RepositoryQueryRequest {
        query: Some(lore_proto::repository_query_request::Query::Id(id.into())),
    })
}

fn query_by_name(name: &str) -> Request<RepositoryQueryRequest> {
    Request::new(RepositoryQueryRequest {
        query: Some(lore_proto::repository_query_request::Query::Name(
            name.into(),
        )),
    })
}

/// Denial answers `RepositoryNotFound`, not `permission_denied`: a
/// distinct status would let an unauthorized caller distinguish a
/// partition that exists from one that does not.
#[tokio::test]
async fn denied_query_answers_not_found_for_an_existing_repository() {
    let id = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            seed_repository(immutable_store.clone(), mutable_store.clone(), id, "repo").await;
            for request in [query_by_id(id), query_by_name("repo")] {
                let err = handler(
                    request,
                    Arc::new(DenyAllRepositoryAuthorizer),
                    immutable_store.clone(),
                    mutable_store.clone(),
                )
                .await
                .expect_err("denied query must fail");
                assert_eq!(err.code(), Code::NotFound, "{err:?}");
            }
        })
        .await;
}

/// A check that could not be made answers its failure, by ID and by name,
/// rather than `NOT_FOUND`.
#[tokio::test]
async fn failed_authorization_check_answers_its_failure() {
    let id = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            seed_repository(immutable_store.clone(), mutable_store.clone(), id, "repo").await;
            for (failure, code) in [
                (
                    Status::resource_exhausted("rate limited"),
                    Code::ResourceExhausted,
                ),
                (Status::internal("failed"), Code::Internal),
            ] {
                for request in [query_by_id(id), query_by_name("repo")] {
                    let err = handler(
                        request,
                        Arc::new(RefusingAuthorizer(failure.clone())),
                        immutable_store.clone(),
                        mutable_store.clone(),
                    )
                    .await
                    .expect_err("a failed check must fail the query");
                    assert_eq!(err.code(), code, "{err:?}");
                }
            }
        })
        .await;
}

/// A permitted lookup of a repository that does not exist still answers
/// `NOT_FOUND`, by ID and by name.
#[tokio::test]
async fn a_missing_repository_answers_not_found() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            for request in [
                query_by_id(random::<RepositoryId>()),
                query_by_name("missing"),
            ] {
                let err = handler(
                    request,
                    Arc::new(AllowAllRepositoryAuthorizer),
                    immutable_store.clone(),
                    mutable_store.clone(),
                )
                .await
                .expect_err("a missing repository must not resolve");
                assert_eq!(err.code(), Code::NotFound, "{err:?}");
            }
        })
        .await;
}

#[tokio::test]
async fn permitted_query_resolves_by_id_and_by_name() {
    let id = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            seed_repository(immutable_store.clone(), mutable_store.clone(), id, "repo").await;
            for request in [query_by_id(id), query_by_name("repo")] {
                let response = handler(
                    request,
                    Arc::new(AllowAllRepositoryAuthorizer),
                    immutable_store.clone(),
                    mutable_store.clone(),
                )
                .await
                .expect("permitted query must succeed");
                let repository = response
                    .into_inner()
                    .repository
                    .expect("response should include Repository");
                assert_eq!(repository.name, "repo");
            }
        })
        .await;
}

/// A lookup by ID finds no mapping for the name in the metadata, and another repository binds the
/// name before the lookup writes it. The lookup keeps that binding and answers `NOT_FOUND`, as it
/// does for a name mapped to another repository.
#[tokio::test]
async fn a_lookup_by_id_keeps_a_name_another_repository_bound_since() {
    let id = random::<RepositoryId>();
    let other = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            seed_metadata(immutable_store.clone(), mutable_store.clone(), id, "repo").await;
            let err = handler(
                query_by_id(id),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store.clone(),
                RacedNameWriteStore::binding(mutable_store.clone(), other),
            )
            .await
            .expect_err("the name maps to another repository");
            assert_eq!(err.code(), Code::NotFound, "{err:?}");
            assert_eq!(
                bound_id(immutable_store, mutable_store, "repo").await,
                other
            );
        })
        .await;
}

#[tokio::test]
async fn a_lookup_by_id_binds_a_missing_name() {
    let id = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository =
                seed_metadata(immutable_store.clone(), mutable_store.clone(), id, "repo").await;
            handler(
                query_by_id(id),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
            )
            .await
            .expect("the lookup by ID resolves");
            assert_eq!(
                repository::id_from_name(repository, "repo")
                    .await
                    .expect("the lookup binds the name"),
                id
            );
        })
        .await;
}

/// A lookup by name finds the name mapped to a repository whose metadata holds another name, and
/// another repository binds the name before the lookup removes the mapping. The lookup keeps that
/// binding.
#[tokio::test]
async fn a_lookup_by_name_keeps_a_mapping_that_changed_since() {
    let id = random::<RepositoryId>();
    let other = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = seed_metadata(
                immutable_store.clone(),
                mutable_store.clone(),
                id,
                "renamed",
            )
            .await;
            repository::store_name_to_id(repository.clone(), "repo", id)
                .await
                .expect("Failed to store the stale mapping");
            let err = handler(
                query_by_name("repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store.clone(),
                RacedNameWriteStore::binding(mutable_store.clone(), other),
            )
            .await
            .expect_err("the mapping is stale");
            assert_eq!(err.code(), Code::NotFound, "{err:?}");
            assert_eq!(
                bound_id(immutable_store, mutable_store, "repo").await,
                other
            );
        })
        .await;
}

#[tokio::test]
async fn a_lookup_by_name_removes_a_stale_mapping() {
    let id = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = seed_metadata(
                immutable_store.clone(),
                mutable_store.clone(),
                id,
                "renamed",
            )
            .await;
            repository::store_name_to_id(repository.clone(), "repo", id)
                .await
                .expect("Failed to store the stale mapping");
            let err = handler(
                query_by_name("repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("the mapping is stale");
            assert_eq!(err.code(), Code::NotFound, "{err:?}");
            let err = repository::id_from_name(repository, "repo")
                .await
                .expect_err("the stale mapping is removed");
            assert!(err.is_address_not_found(), "{err:?}");
        })
        .await;
}

/// A lookup by name finds the name mapped to a repository that has no metadata, as a delete that
/// raced a lookup by ID can leave it. The lookup removes the mapping.
#[tokio::test]
async fn a_lookup_by_name_removes_a_mapping_to_a_repository_without_metadata() {
    let id = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            bind_without_metadata(immutable_store.clone(), mutable_store.clone(), "repo", id).await;
            let err = handler(
                query_by_name("repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store.clone(),
                mutable_store.clone(),
            )
            .await
            .expect_err("the repository has no metadata");
            assert_eq!(err.code(), Code::NotFound, "{err:?}");
            assert!(unbound(immutable_store, mutable_store, "repo").await);
        })
        .await;
}

/// A lookup by name finds the name mapped to a repository that has no metadata, and another
/// repository binds the name before the lookup removes the mapping. The lookup keeps that binding.
#[tokio::test]
async fn a_lookup_by_name_keeps_a_mapping_to_a_repository_without_metadata_that_changed_since() {
    let id = random::<RepositoryId>();
    let other = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            bind_without_metadata(immutable_store.clone(), mutable_store.clone(), "repo", id).await;
            let err = handler(
                query_by_name("repo"),
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store.clone(),
                RacedNameWriteStore::binding(mutable_store.clone(), other),
            )
            .await
            .expect_err("the repository has no metadata");
            assert_eq!(err.code(), Code::NotFound, "{err:?}");
            assert_eq!(
                bound_id(immutable_store, mutable_store, "repo").await,
                other
            );
        })
        .await;
}
