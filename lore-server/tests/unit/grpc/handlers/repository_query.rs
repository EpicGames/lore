// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::RepositoryId;
use lore_proto::RepositoryQueryRequest;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::handlers::repository_query::*;
use rand::random;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::store::test_support::test_store_create;

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
