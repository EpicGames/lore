// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::Extension;
use axum::body::to_bytes;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header::ACCEPT_RANGES;
use axum::http::header::CONTENT_LENGTH;
use axum::http::header::CONTENT_RANGE;
use axum::http::header::RANGE;
use axum::response::IntoResponse;
use axum::response::Response;
use bytes::Bytes;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use jsonwebtoken::encode;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_revision::fragment;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::http::repositories::repository::contents::content::get_repository_content::handler;
use lore_server::http::server::ServerState;
use rand::random;

use crate::http::test_utils::MISSING_ADDRESS;
use crate::http::test_utils::RANGED_CONTENT_SIZE;
use crate::http::test_utils::build_test_server;
use crate::http::test_utils::put_compressed_content;
use crate::http::test_utils::test_state;
use crate::http::test_utils::write_fragmented_content;
use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_server_is_up_and_listening() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let test_server = build_test_server(immutable_store, mutable_store, None);

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
            let test_server = build_test_server(immutable_store, mutable_store, None);
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
            let test_server = build_test_server(immutable_store, mutable_store, None);

            let response = test_server
                .get(&format!(
                    "/v1/repository/ffffffffffffffffffffffffffffffff/content/{MISSING_ADDRESS}"
                ))
                .add_header(RANGE, "bytes=99999999-")
                .expect_failure()
                .await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            assert_eq!(response.text(), "address not found");
            assert!(response.headers().get(CONTENT_RANGE).is_none());
        })
        .await;
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

            let test_server = build_test_server(immutable_store, mutable_store, None);
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

            let test_server = build_test_server(immutable_store, mutable_store, None);
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

            let test_server = build_test_server(immutable_store, mutable_store, None);
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

/// Writes fragmented content, then requests it with `method`, sending `range`
/// as its `Range` header.
async fn request_range(method: Method, range: &str) -> (axum_test::TestResponse, Bytes) {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let range = range.to_string();
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let (address, content) = write_fragmented_content(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            )
            .await;
            let test_server = build_test_server(immutable_store, mutable_store, None);

            let response = test_server
                .method(
                    method,
                    &format!("/v1/repository/{repository}/content/{address}"),
                )
                .add_header(RANGE, range)
                .await;
            (response, content)
        })
        .await
}

#[tokio::test]
async fn test_range_returns_partial_content() {
    let (response, content) = request_range(Method::GET, "bytes=5000-13999").await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!("bytes 5000-13999/{RANGED_CONTENT_SIZE}")
    );
    assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "9000");
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(response.as_bytes(), &content[5000..14000]);
}

#[tokio::test]
async fn test_range_past_the_end_is_not_satisfiable() {
    let (response, _) = request_range(Method::GET, "bytes=2000000-2000010").await;

    assert_eq!(response.status_code(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!("bytes */{RANGED_CONTENT_SIZE}")
    );
}

#[tokio::test]
async fn test_several_ranges_return_whole_content() {
    let (response, content) = request_range(Method::GET, "bytes=0-9,100-109").await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert!(response.headers().get(CONTENT_RANGE).is_none());
    assert_eq!(response.as_bytes(), &content[..]);
}

#[tokio::test]
async fn test_head_with_range_is_not_partial_content() {
    let (response, _) = request_range(Method::HEAD, "bytes=0-9").await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert!(response.headers().get(CONTENT_RANGE).is_none());
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(
        response.headers().get(CONTENT_LENGTH).unwrap(),
        &RANGED_CONTENT_SIZE.to_string()
    );
    assert!(response.as_bytes().is_empty());
}

/// Calls the handler directly with `method` and `headers`, for content `address` in `repository`.
/// It runs on the test's runtime, and its response keeps the body a router strips for `HEAD`.
async fn call_handler(
    state: Arc<ServerState>,
    method: Method,
    repository: Context,
    address: Address,
    headers: HeaderMap,
) -> Response {
    handler(
        method,
        State(state),
        Query::try_from_uri(&Uri::from_static("/")).unwrap(),
        Path((repository.to_string(), address.to_string())),
        Extension(None),
        headers,
    )
    .await
    .into_response()
}

#[tokio::test]
async fn test_head_streams_no_content() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let (address, _) = write_fragmented_content(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            )
            .await;
            let state = Arc::new(test_state(immutable_store, mutable_store, None));

            for (method, streamed) in [(Method::HEAD, 0), (Method::GET, RANGED_CONTENT_SIZE)] {
                let response = call_handler(
                    state.clone(),
                    method.clone(),
                    repository,
                    address,
                    HeaderMap::new(),
                )
                .await;

                assert_eq!(response.status(), StatusCode::OK, "{method}");
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                assert_eq!(body.len(), streamed, "{method}");
            }
        })
        .await;
}

#[tokio::test]
async fn test_a_request_decodes_the_root_once() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let address = put_compressed_content(immutable_store.clone(), repository.into()).await;
            let state = Arc::new(test_state(immutable_store, mutable_store, None));

            for range in [None, Some("bytes=0-9")] {
                let mut headers = HeaderMap::new();
                if let Some(range) = range {
                    headers.insert(RANGE, HeaderValue::from_static(range));
                }
                lore_storage::test_util::reset_counts();
                let response =
                    call_handler(state.clone(), Method::GET, repository, address, headers).await;

                assert!(response.status().is_success(), "{range:?}");
                assert_eq!(lore_storage::test_util::counts().1, 1, "{range:?}");
            }
        })
        .await;
}
