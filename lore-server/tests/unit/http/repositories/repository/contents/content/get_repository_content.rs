// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum_test::TestServer;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use jsonwebtoken::encode;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::fragment;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::TokenVerification;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use rand::random;

use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_server_is_up_and_listening() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                token_verification: TokenVerification::Ignore,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };

            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();

            let response = test_server.get("/does-not-exist").expect_failure().await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
        })
        .await;
}

#[tokio::test]
async fn test_address_in_wrong_format() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                token_verification: TokenVerification::Ignore,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();

            // Create the server and test the request
            let non_existing_address = "/v1/repository/fffff/content/fffff-ff"; // Wrong lengths
            let response = test_server.get(non_existing_address).expect_failure().await;

            assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
        })
        .await;
}

#[tokio::test]
async fn test_address_not_found() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                token_verification: TokenVerification::Ignore,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();

            // Create the server and test the request
            let non_existing_address = "/v1/repository/ffffffffffffffffffffffffffffffff/content/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";
            let response = test_server.get(non_existing_address).expect_failure().await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            assert_eq!(response.text(), "address not found");
        }).await;
}

#[tokio::test]
async fn test_address_returned_correctly() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create a fragment on a repository in the store
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();

            immutable_store
                .clone()
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put data in immutable store");

            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                token_verification: TokenVerification::Ignore,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();
            let valid_url = format!("/v1/repository/{repository}/content/{address}");

            let response = test_server.get(valid_url.as_str()).await;

            assert_eq!(response.status_code(), StatusCode::OK);
        })
        .await;
}

#[tokio::test]
async fn test_address_returned_correctly_with_headers() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create a fragment on a repository in the store
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();

            immutable_store
                .clone()
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put data in immutable store");

            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                token_verification: TokenVerification::Ignore,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();
            let valid_url = format!("/v1/repository/{repository}/content/{address}");

            let response = test_server
                .get(valid_url.as_str())
                .add_query_param("content_type", "image/png")
                .add_query_param("content_encoding", "gzip")
                .add_query_param("content_disposition", "inline")
                .await;

            assert_eq!(response.status_code(), StatusCode::OK);
            assert_eq!(
                response.headers().get("content-type").unwrap(),
                HeaderValue::from_static("image/png")
            );
            assert_eq!(
                response.headers().get("content-encoding").unwrap(),
                HeaderValue::from_static("gzip")
            );
            assert_eq!(
                response.headers().get("content-disposition").unwrap(),
                HeaderValue::from_static("inline")
            );
        })
        .await;
}

#[tokio::test]
async fn test_address_works_with_jwt_verifier_and_good_token() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            // Create a fragment on a repository in the store
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();

            immutable_store
                .clone()
                .put(
                    repository.into(),
                    address,
                    fragment,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put data in immutable store");

            // Create the server and test the request
            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let test_shared_state = ServerState {
                immutable_store,
                mutable_store,
                token_verification: TokenVerification::Ignore,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: None,
            };
            let settings = LoreHttpServerSettings::test_default();
            let app = create_router(test_shared_state, test_health, &settings);
            let test_server = TestServer::new(app).unwrap();
            let valid_url = format!("/v1/repository/{repository}/content/{address}");

            // Create a valid token, with a kid (which is what's checked for now, so it breaks if/when we check further)
            let auth_header = HeaderName::from_static("authorization");
            let jwt_header = Header {
                kid: Some("a_kid".to_owned()),
                ..Default::default()
            };

            let expiration = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .add(Duration::from_secs(60))
                .as_secs();

            let jwt_claims = AuthorizationToken {
                user_id: "qweasdzxc123".to_string(),
                issuer: "test".to_string(),
                issued_at: 123456700,
                audience: vec!["Lore".to_string()],
                env: Some("DEV".to_string()),
                name: Some("test".to_string()),
                preferred_username: Some("test".to_string()),
                client_id: None,
                resources: None,
                groups: None,
                is_service_account: Some(false),
                expires: expiration,
                idp: Some("test".to_string()),
                extra: Default::default(),
                identity: None,
            };
            let jwt_key = EncodingKey::from_secret("test-secret".as_ref());
            let bearer = encode(&jwt_header, &jwt_claims, &jwt_key).unwrap();
            let bearer_header_string = format!("Bearer {bearer}");
            let bearer_header = HeaderValue::from_str(bearer_header_string.as_str()).unwrap();
            let response = test_server
                .get(valid_url.as_str())
                .add_header(auth_header, bearer_header)
                .await;

            assert_eq!(response.status_code(), StatusCode::OK);
        })
        .await;
}
