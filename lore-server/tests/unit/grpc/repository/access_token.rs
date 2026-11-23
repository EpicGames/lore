// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Repository lookup, v0 and v1, called with an access token on the auth-service tier and on
//! Tier 2, where the token's `resources` claim answers: the repository the claim names resolves,
//! and any other is answered as a missing one.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1 as proto_v1;
use lore_proto::lore::repository::v1::repository_get_request::Query;
use lore_proto::repository_query_request::Query as QueryV0;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_revision::state::allow_all_repositories;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::repository_authorizer::AuthClientAuthorizer;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use lore_server::grpc::handlers;
use lore_server::grpc::repository::v1::repository_get;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use rand::random;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::store::test_support::test_store_create;

/// The auth-service authorizer, which answers an access token from its claim with no call
/// (nothing listens on the URL), and the Tier 2 authorizer reading the same claim.
fn authorizers() -> [Arc<dyn RepositoryAuthorizer>; 2] {
    [
        Arc::new(AuthClientAuthorizer::new(
            "https://auth.invalid".to_string(),
        )),
        Arc::new(ResourceGrantsAuthorizer::new(
            "resources".to_string(),
            "resource_id".to_string(),
            None,
            "urc-{id}".to_string(),
            "urc-*".to_string(),
        )),
    ]
}

fn access_token(granted: RepositoryId) -> AuthorizationToken {
    AuthorizationToken {
        user_id: "alice".to_string(),
        resources: Some(vec![ResourcePermission {
            resource_id: format!("urc-{granted}"),
            permission: vec!["read".to_string(), "write".to_string()],
        }]),
        ..Default::default()
    }
}

fn with_token<T>(message: T, token: AuthorizationToken) -> Request<T> {
    let mut request = Request::new(message);
    request.extensions_mut().insert(token);
    request
        .extensions_mut()
        .insert(RawToken("raw.jwt".to_string()));
    request
}

async fn seed_repository(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    id: RepositoryId,
    name: &str,
) {
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        id,
        allow_all_repositories(),
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

fn id_bytes(id: RepositoryId) -> bytes::Bytes {
    Context::from(id).into()
}

fn id_of(bytes: bytes::Bytes) -> RepositoryId {
    RepositoryId::from(Context::from(bytes))
}

/// The v1 `RepositoryGet` of `query`, answered as the id it resolves to.
async fn get_v1(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    stores: &(Arc<dyn ImmutableStore>, Arc<dyn MutableStore>),
    token: AuthorizationToken,
    query: Query,
) -> Result<RepositoryId, Status> {
    let response = repository_get::handler(
        with_token(proto_v1::RepositoryGetRequest { query: Some(query) }, token),
        authorizer.clone(),
        stores.0.clone(),
        stores.1.clone(),
        &None,
    )
    .await?;
    Ok(id_of(
        response.into_inner().repository.expect("a record").id,
    ))
}

/// The v0 `RepositoryQuery` of `query`, answered as the id it resolves to.
async fn query_v0(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    stores: &(Arc<dyn ImmutableStore>, Arc<dyn MutableStore>),
    token: AuthorizationToken,
    query: QueryV0,
) -> Result<RepositoryId, Status> {
    let response = handlers::repository_query::handler(
        with_token(
            lore_proto::RepositoryQueryRequest { query: Some(query) },
            token,
        ),
        authorizer.clone(),
        stores.0.clone(),
        stores.1.clone(),
    )
    .await?;
    Ok(id_of(
        response.into_inner().repository.expect("a record").id,
    ))
}

#[tokio::test]
async fn an_access_token_resolves_its_repository_by_name_and_by_id() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let stores = (immutable_store, mutable_store);
    let granted = random::<RepositoryId>();

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        seed_repository(stores.0.clone(), stores.1.clone(), granted, "granted").await;
        for authorizer in authorizers() {
            for query in [Query::Name("granted".into()), Query::Id(id_bytes(granted))] {
                let resolved = get_v1(&authorizer, &stores, access_token(granted), query).await;
                assert_eq!(resolved.unwrap(), granted);
            }
            for query in [
                QueryV0::Name("granted".into()),
                QueryV0::Id(id_bytes(granted)),
            ] {
                let resolved = query_v0(&authorizer, &stores, access_token(granted), query).await;
                assert_eq!(resolved.unwrap(), granted);
            }
        }
    }))
    .await;
}

/// The answer for an existing repository the token does not name has the code and the message of
/// the answer for a missing one, so the token cannot tell which names and ids exist.
#[tokio::test]
async fn another_repository_is_answered_as_a_missing_one() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let stores = (immutable_store, mutable_store);
    let granted = random::<RepositoryId>();
    let other = random::<RepositoryId>();
    let missing = random::<RepositoryId>();

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        seed_repository(stores.0.clone(), stores.1.clone(), granted, "granted").await;
        seed_repository(stores.0.clone(), stores.1.clone(), other, "other").await;
        let not_found = |status: Status, queried: &str| {
            assert_eq!(status.code(), Code::NotFound);
            status.message().replace(queried, "<queried>")
        };

        for authorizer in authorizers() {
            for (existing, absent) in [
                ("other".to_string(), "missing".to_string()),
                (other.to_string(), missing.to_string()),
            ] {
                let mut answers = Vec::new();
                for queried in [&existing, &absent] {
                    // A name that parses as an id is looked up as one, in both handlers.
                    let v1 = get_v1(
                        &authorizer,
                        &stores,
                        access_token(granted),
                        Query::Name(queried.clone()),
                    )
                    .await
                    .unwrap_err();
                    let v0 = query_v0(
                        &authorizer,
                        &stores,
                        access_token(granted),
                        QueryV0::Name(queried.clone()),
                    )
                    .await
                    .unwrap_err();
                    answers.push([not_found(v1, queried), not_found(v0, queried)]);
                }
                assert_eq!(answers[0], answers[1]);
            }

            let mut answers = Vec::new();
            for id in [other, missing] {
                let v1 = get_v1(
                    &authorizer,
                    &stores,
                    access_token(granted),
                    Query::Id(id_bytes(id)),
                )
                .await
                .unwrap_err();
                let v0 = query_v0(
                    &authorizer,
                    &stores,
                    access_token(granted),
                    QueryV0::Id(id_bytes(id)),
                )
                .await
                .unwrap_err();
                let id = id.to_string();
                answers.push([not_found(v1, &id), not_found(v0, &id)]);
            }
            assert_eq!(answers[0], answers[1]);
        }
    }))
    .await;
}
