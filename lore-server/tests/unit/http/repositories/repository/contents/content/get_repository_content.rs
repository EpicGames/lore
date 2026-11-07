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
use axum::http::header::ACCEPT_RANGES;
use axum::http::header::CONTENT_RANGE;
use axum::http::header::RANGE;
use axum_test::TestServer;
use bytes::Bytes;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use jsonwebtoken::encode;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::fragment;
use lore_revision::immutable;
use lore_revision::repository::RepositoryContext;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::TokenVerification;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use lore_storage::WriteOptions;
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

/// Size of the content the range tests store. With 4 KiB chunks it is spread
/// over many fragments, so a range crosses fragment boundaries.
const RANGED_CONTENT_SIZE: usize = 1024 * 1024;

/// Writes `RANGED_CONTENT_SIZE` random bytes as fragmented content, then gets
/// it with `range` as the request's `Range` header.
async fn get_range(range: &str) -> (axum_test::TestResponse, Bytes) {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let range = range.to_string();
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let repository = random::<Context>();
            let context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));
            let payload: Vec<u8> = (0..RANGED_CONTENT_SIZE).map(|_| random::<u8>()).collect();
            let payload = Bytes::from(payload);
            let address = immutable::write(
                context,
                random::<Context>(),
                payload.clone(),
                WriteOptions::default().with_fixed_size_chunk(4096),
            )
            .await
            .expect("Failed to write content");

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

            let response = test_server
                .get(&format!("/v1/repository/{repository}/content/{address}"))
                .add_header(RANGE, range)
                .await;
            (response, payload)
        })
        .await
}

#[tokio::test]
async fn test_range_returns_partial_content() {
    let (response, payload) = get_range("bytes=500000-899999").await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!("bytes 500000-899999/{RANGED_CONTENT_SIZE}")
    );
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(response.as_bytes(), &payload[500000..900000]);
}

#[tokio::test]
async fn test_range_past_the_end_is_not_satisfiable() {
    let (response, _) = get_range("bytes=2000000-2000010").await;

    assert_eq!(response.status_code(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!("bytes */{RANGED_CONTENT_SIZE}")
    );
}

#[tokio::test]
async fn test_range_serves_the_first_and_the_last_byte() {
    let last = RANGED_CONTENT_SIZE - 1;
    for (range, start) in [
        ("bytes=0-0".to_string(), 0),
        (format!("bytes={last}-{last}"), last),
    ] {
        let (response, payload) = get_range(&range).await;

        assert_eq!(
            response.status_code(),
            StatusCode::PARTIAL_CONTENT,
            "{range}"
        );
        assert_eq!(response.as_bytes(), &payload[start..=start], "{range}");
    }
}

#[tokio::test]
async fn test_malformed_range_returns_whole_content() {
    let flood = format!("bytes={}", vec!["0-0"; 10_000].join(","));
    for range in ["bytes=-5-10", "bytes=+0-9", "items=0-9", flood.as_str()] {
        let (response, payload) = get_range(range).await;

        assert_eq!(response.status_code(), StatusCode::OK);
        assert!(response.headers().get(CONTENT_RANGE).is_none());
        assert_eq!(response.as_bytes(), &payload[..]);
    }
}

#[tokio::test]
async fn test_range_on_missing_address_is_not_found() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
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
            let repository = random::<Context>();
            let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

            let response = test_server
                .get(&format!("/v1/repository/{repository}/content/{address}"))
                .add_header(RANGE, "bytes=99999999-")
                .await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            assert!(response.headers().get(CONTENT_RANGE).is_none());
        })
        .await;
}
