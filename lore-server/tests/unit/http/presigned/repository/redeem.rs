// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header::ACCEPT_RANGES;
use axum::http::header::ALLOW;
use axum::http::header::CONTENT_LENGTH;
use axum::http::header::CONTENT_RANGE;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::RANGE;
use axum_test::TestServer;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::fragment;
use lore_revision::immutable;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_server::auth::jwt::TokenVerification;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::http::presign_token::CURRENT_TOKEN_VERSION;
use lore_server::http::presign_token::PresignTokenPayload;
use lore_server::http::presign_token::sign;
use lore_server::http::security_headers::ContentTypePolicy;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::PresignConfig;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use lore_storage::WriteOptions;
use rand::random;

use crate::http::test_utils::content_type_policy;
use crate::http::test_utils::presign_config;
use crate::http::test_utils::presign_config_with_policy;
use crate::store::test_support::test_store_create;

fn valid_token(repository_id: &str, address: &str, config: &PresignConfig) -> String {
    token_with_content_type(repository_id, address, config, None)
}

fn token_with_content_type(
    repository_id: &str,
    address: &str,
    config: &PresignConfig,
    content_type: Option<&str>,
) -> String {
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let payload = PresignTokenPayload {
        version: CURRENT_TOKEN_VERSION,
        key_id: config.key_id.clone(),
        repository: repository_id.to_string(),
        address: address.to_string(),
        expires_at,
        content_type: content_type.map(String::from),
        content_encoding: None,
        content_disposition: None,
    };
    sign(&payload, &config.hmac_key)
}

fn build_test_server(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    config: PresignConfig,
) -> TestServer {
    let test_health = ServerHealth::new_without_availability(immutable_store.clone());
    let state = ServerState {
        immutable_store,
        mutable_store,
        token_verification: TokenVerification::Ignore,
        repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
        max_file_size: 100,
        presign_config: Some(config),
    };
    let settings = LoreHttpServerSettings::test_default();
    TestServer::new(create_router(state, test_health, &settings)).unwrap()
}

async fn redeem_with_content_type(content_type: Option<&str>) -> axum_test::TestResponse {
    redeem_with_policy(content_type, ContentTypePolicy::default()).await
}

/// Stores random content, then redeems it with a token carrying `content_type`
/// against a server whose allowlist comes from `policy`.
async fn redeem_with_policy(
    content_type: Option<&str>,
    policy: ContentTypePolicy,
) -> axum_test::TestResponse {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let content_type = content_type.map(String::from);
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<RepositoryId>();
            let (fragment_data, address, payload) = fragment::generate_random();
            immutable_store
                .clone()
                .put(repository, address, fragment_data, Some(payload), false)
                .await
                .expect("Failed to put data in immutable store");

            let config = presign_config_with_policy(policy);
            let repo_hex = format!("{repository}");
            let address_str = format!("{address}");
            let token =
                token_with_content_type(&repo_hex, &address_str, &config, content_type.as_deref());
            let server = build_test_server(immutable_store, mutable_store, config);

            server
                .get(&format!("/v1/presigned/{repo_hex}/{address_str}"))
                .add_query_param("token", token)
                .await
        })
        .await
}

#[tokio::test]
async fn returns_404_when_address_not_found() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<RepositoryId>();
            let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

            let config = presign_config();
            let repo_hex = format!("{repository}");
            let token = valid_token(&repo_hex, address, &config);

            let server = build_test_server(immutable_store, mutable_store, config);

            let response = server
                .get(&format!("/v1/presigned/{repo_hex}/{address}"))
                .add_query_param("token", token)
                .await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
        })
        .await;
}

#[tokio::test]
async fn returns_401_for_expired_token() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<RepositoryId>();
            let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

            let config = presign_config();
            let repo_hex = format!("{repository}");

            let payload = PresignTokenPayload {
                version: CURRENT_TOKEN_VERSION,
                key_id: config.key_id.clone(),
                repository: repo_hex.clone(),
                address: address.to_string(),
                expires_at: 1,
                content_type: None,
                content_encoding: None,
                content_disposition: None,
            };
            let token = sign(&payload, &config.hmac_key);

            let server = build_test_server(immutable_store, mutable_store, config);

            let response = server
                .get(&format!("/v1/presigned/{repo_hex}/{address}"))
                .add_query_param("token", token)
                .await;

            assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED);
        })
        .await;
}

#[tokio::test]
async fn returns_200_with_content_for_valid_token() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<RepositoryId>();
            let (fragment_data, address, payload) = fragment::generate_random();

            immutable_store
                .clone()
                .put(
                    repository,
                    address,
                    fragment_data,
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("Failed to put data in immutable store");

            let config = presign_config();
            let repo_hex = format!("{repository}");
            let address_str = format!("{address}");
            let token = valid_token(&repo_hex, &address_str, &config);

            let server = build_test_server(immutable_store, mutable_store, config);

            let response = server
                .get(&format!("/v1/presigned/{repo_hex}/{address_str}"))
                .add_query_param("token", token)
                .await;

            assert_eq!(response.status_code(), StatusCode::OK);
            assert_eq!(response.as_bytes(), &payload[..]);

            let cache_control = response
                .headers()
                .get("cache-control")
                .expect("missing Cache-Control header")
                .to_str()
                .unwrap();
            assert!(
                cache_control.starts_with("public, immutable, max-age="),
                "unexpected Cache-Control value: {cache_control}"
            );
            let max_age: u64 = cache_control
                .strip_prefix("public, immutable, max-age=")
                .unwrap()
                .parse()
                .expect("max-age is not a number");
            assert!(
                max_age > 3590 && max_age <= 3600,
                "max-age {max_age} not in expected range"
            );
        })
        .await;
}

#[tokio::test]
async fn returns_400_when_no_token_query_param() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<RepositoryId>();
            let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

            let config = presign_config();
            let repo_hex = format!("{repository}");

            let server = build_test_server(immutable_store, mutable_store, config);

            let response = server
                .get(&format!("/v1/presigned/{repo_hex}/{address}"))
                .await;

            assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.text(),
                "Failed to deserialize query string: missing field `token`"
            );
        })
    .await;
}

#[tokio::test]
async fn head_is_rejected_without_redeeming_content() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");
    let server = build_test_server(immutable_store, mutable_store, presign_config());

    let response = server
        .method(
            Method::HEAD,
            "/v1/presigned/not-a-repository/not-an-address",
        )
        .await;

    assert_eq!(response.status_code(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers().get(ALLOW).unwrap(), "GET");
}

#[tokio::test]
async fn non_get_methods_advertise_get_only() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");
    let server = build_test_server(immutable_store, mutable_store, presign_config());

    for method in [
        Method::OPTIONS,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::TRACE,
        Method::CONNECT,
    ] {
        let response = server
            .method(method, "/v1/presigned/not-a-repository/not-an-address")
            .await;

        assert_eq!(response.status_code(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers().get(ALLOW).unwrap(), "GET");
    }
}

#[tokio::test]
async fn redeem_serves_allowlisted_content_type_verbatim() {
    let response = redeem_with_content_type(Some("image/png")).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "image/png");
}

/// The S3 default type must round-trip verbatim, so a caller that mints
/// with forwarded S3 metadata reads back the same value it sent.
#[tokio::test]
async fn redeem_serves_s3_binary_octet_stream_verbatim() {
    let response = redeem_with_content_type(Some("binary/octet-stream")).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "binary/octet-stream"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        response.headers().get("content-security-policy").unwrap(),
        "default-src 'none'; sandbox"
    );
}

#[tokio::test]
async fn redeem_coerces_disallowed_content_type_to_octet_stream() {
    let response = redeem_with_content_type(Some("text/html")).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    // The dangerous input must not also strip the protections.
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        response.headers().get("content-security-policy").unwrap(),
        "default-src 'none'; sandbox"
    );
}

#[tokio::test]
async fn redeem_coerces_missing_content_type_to_octet_stream() {
    let response = redeem_with_content_type(None).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
}

#[tokio::test]
async fn redeem_falls_back_to_octet_stream_for_unserializable_allowed_type() {
    // A legacy token whose allowed media type carries a control-char
    // parameter must not 500 — redeem falls back to octet-stream.
    let response = redeem_with_content_type(Some("image/png; x=\u{7}")).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
}

/// A type added through `presigned_url_extra_content_types` is served verbatim,
/// and the protections still apply to it.
#[tokio::test]
async fn redeem_serves_configured_extra_content_type_verbatim() {
    let response = redeem_with_policy(
        Some("application/zip"),
        content_type_policy(&["application/zip"], &[]),
    )
    .await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/zip"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        response.headers().get("content-security-policy").unwrap(),
        "default-src 'none'; sandbox"
    );
}

/// A built-in type removed through `presigned_url_denied_content_types` is
/// coerced, even though an already-issued token carries it.
#[tokio::test]
async fn redeem_coerces_configured_denied_content_type() {
    let response =
        redeem_with_policy(Some("image/png"), content_type_policy(&[], &["image/png"])).await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
}

#[tokio::test]
async fn redeem_sets_security_headers() {
    let response = redeem_with_content_type(Some("image/png")).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        response.headers().get("content-security-policy").unwrap(),
        "default-src 'none'; sandbox"
    );
}

/// Size of the content the range tests store. With 4 KiB chunks it is spread
/// over many fragments, so a range crosses fragment boundaries.
const RANGED_CONTENT_SIZE: usize = 1024 * 1024;

/// Writes `RANGED_CONTENT_SIZE` random bytes as fragmented content, then
/// redeems it with `range` as the request's `Range` header.
async fn redeem_range(range: Option<&str>) -> (axum_test::TestResponse, Bytes) {
    let ranges: Vec<&str> = range.into_iter().collect();
    redeem_ranges_with(&ranges, valid_token).await
}

/// Like `redeem_range`, with every value in `ranges` sent as its own `Range`
/// header and the token from `token(repository, address, config)`.
async fn redeem_ranges_with(
    ranges: &[&str],
    token: fn(&str, &str, &PresignConfig) -> String,
) -> (axum_test::TestResponse, Bytes) {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let ranges: Vec<String> = ranges.iter().map(|range| range.to_string()).collect();
    LORE_CONTEXT
        .scope(execution, async move {
            let repository_id = random::<RepositoryId>();
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let payload: Vec<u8> = (0..RANGED_CONTENT_SIZE).map(|_| random::<u8>()).collect();
            let payload = Bytes::from(payload);
            let address = immutable::write(
                repository,
                random::<Context>(),
                payload.clone(),
                WriteOptions::default().with_fixed_size_chunk(4096),
            )
            .await
            .expect("Failed to write content");

            let config = presign_config();
            let repo_hex = format!("{repository_id}");
            let address_str = format!("{address}");
            let token = token(&repo_hex, &address_str, &config);
            let server = build_test_server(immutable_store, mutable_store, config);

            let mut request = server
                .get(&format!("/v1/presigned/{repo_hex}/{address_str}"))
                .add_query_param("token", token);
            for range in ranges {
                request = request.add_header(RANGE, range);
            }
            (request.await, payload)
        })
        .await
}

#[tokio::test]
async fn redeem_serves_a_range_spanning_fragments_as_partial_content() {
    let (response, payload) = redeem_range(Some("bytes=10000-209999")).await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!("bytes 10000-209999/{RANGED_CONTENT_SIZE}")
    );
    assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "200000");
    assert_eq!(response.as_bytes(), &payload[10000..210000]);
}

#[tokio::test]
async fn redeem_serves_the_last_bytes_of_the_content() {
    let (response, payload) = redeem_range(Some("bytes=-5000")).await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!(
            "bytes {}-{}/{RANGED_CONTENT_SIZE}",
            RANGED_CONTENT_SIZE - 5000,
            RANGED_CONTENT_SIZE - 1
        )
    );
    assert_eq!(response.as_bytes(), &payload[RANGED_CONTENT_SIZE - 5000..]);
}

#[tokio::test]
async fn redeem_serves_an_open_range_to_the_end() {
    let (response, payload) = redeem_range(Some("bytes=1000000-")).await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.as_bytes(), &payload[1_000_000..]);
}

#[tokio::test]
async fn redeem_returns_416_for_a_range_past_the_end() {
    let (response, _) = redeem_range(Some(&format!("bytes={RANGED_CONTENT_SIZE}-"))).await;

    assert_eq!(response.status_code(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!("bytes */{RANGED_CONTENT_SIZE}")
    );
}

#[tokio::test]
async fn redeem_serves_the_whole_content_for_several_ranges() {
    let (response, payload) = redeem_range(Some("bytes=0-9,100-109")).await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert!(response.headers().get(CONTENT_RANGE).is_none());
    assert_eq!(response.as_bytes(), &payload[..]);
}

#[tokio::test]
async fn redeem_advertises_byte_ranges() {
    let (response, payload) = redeem_range(None).await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(response.as_bytes(), &payload[..]);
}

#[tokio::test]
async fn redeem_keeps_the_protections_on_a_partial_response() {
    let (response, _) = redeem_range(Some("bytes=0-99")).await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert!(
        response
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("public, immutable, max-age=")
    );
}

fn expired_token(repository_id: &str, address: &str, config: &PresignConfig) -> String {
    let payload = PresignTokenPayload {
        version: CURRENT_TOKEN_VERSION,
        key_id: config.key_id.clone(),
        repository: repository_id.to_string(),
        address: address.to_string(),
        expires_at: 1,
        content_type: None,
        content_encoding: None,
        content_disposition: None,
    };
    sign(&payload, &config.hmac_key)
}

/// An address the range tests never store.
const MISSING_ADDRESS: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

#[tokio::test]
async fn redeem_checks_the_token_before_the_range() {
    let tokens: [fn(&str, &str, &PresignConfig) -> String; 4] = [
        expired_token,
        |repository, _, config| valid_token(repository, MISSING_ADDRESS, config),
        |repository, address, config| {
            let token = valid_token(repository, address, config);
            let middle = token.len() / 2;
            let flipped = if &token[middle..=middle] == "A" {
                "B"
            } else {
                "A"
            };
            format!("{}{flipped}{}", &token[..middle], &token[middle + 1..])
        },
        |_, _, _| String::new(),
    ];
    for token in tokens {
        for range in ["bytes=0-9", "bytes=99999999-"] {
            let (response, _) = redeem_ranges_with(&[range], token).await;

            assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED, "{range}");
            assert!(response.headers().get(CONTENT_RANGE).is_none(), "{range}");
            assert!(response.headers().get(ACCEPT_RANGES).is_none(), "{range}");
        }
    }
}

#[tokio::test]
async fn redeem_returns_404_not_416_for_a_missing_address() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<RepositoryId>();
            let config = presign_config();
            let repo_hex = format!("{repository}");
            let token = valid_token(&repo_hex, MISSING_ADDRESS, &config);
            let server = build_test_server(immutable_store, mutable_store, config);

            let response = server
                .get(&format!("/v1/presigned/{repo_hex}/{MISSING_ADDRESS}"))
                .add_query_param("token", token)
                .add_header(RANGE, "bytes=99999999-")
                .await;

            assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            assert!(response.headers().get(CONTENT_RANGE).is_none());
        })
        .await;
}

#[tokio::test]
async fn redeem_serves_the_first_and_the_last_byte() {
    let last = RANGED_CONTENT_SIZE - 1;
    for (range, start) in [
        ("bytes=0-0".to_string(), 0),
        (format!("bytes={last}-{last}"), last),
    ] {
        let (response, payload) = redeem_range(Some(&range)).await;

        assert_eq!(
            response.status_code(),
            StatusCode::PARTIAL_CONTENT,
            "{range}"
        );
        assert_eq!(
            response.headers().get(CONTENT_LENGTH).unwrap(),
            "1",
            "{range}"
        );
        assert_eq!(response.as_bytes(), &payload[start..=start], "{range}");
    }
}

#[tokio::test]
async fn redeem_trims_an_end_at_the_limit_of_u64() {
    let (response, payload) = redeem_range(Some("bytes=0-18446744073709551615")).await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_RANGE).unwrap(),
        &format!("bytes 0-{}/{RANGED_CONTENT_SIZE}", RANGED_CONTENT_SIZE - 1)
    );
    assert_eq!(response.as_bytes(), &payload[..]);
}

#[tokio::test]
async fn redeem_returns_416_for_a_start_at_the_limit_of_u64() {
    let (response, _) = redeem_range(Some("bytes=18446744073709551615-")).await;

    assert_eq!(response.status_code(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(response.as_bytes().is_empty());
}

#[tokio::test]
async fn redeem_serves_the_whole_content_for_a_malformed_range() {
    let flood = format!("bytes={}", vec!["0-0"; 10_000].join(","));
    for range in [
        "bytes=-5-10",
        "bytes=+0-9",
        "bytes=99999999999999999999-",
        flood.as_str(),
    ] {
        let (response, payload) = redeem_range(Some(range)).await;

        assert_eq!(response.status_code(), StatusCode::OK);
        assert!(response.headers().get(CONTENT_RANGE).is_none());
        assert_eq!(response.as_bytes(), &payload[..]);
    }
}

#[tokio::test]
async fn redeem_serves_the_whole_content_for_several_range_headers() {
    let (response, payload) = redeem_ranges_with(&["bytes=0-9", "bytes=10-19"], valid_token).await;

    assert_eq!(response.status_code(), StatusCode::OK);
    assert!(response.headers().get(CONTENT_RANGE).is_none());
    assert_eq!(response.as_bytes(), &payload[..]);
}

#[tokio::test]
async fn redeem_coerces_a_disallowed_content_type_on_a_partial_response() {
    let (response, _) = redeem_ranges_with(&["bytes=0-99"], |repository, address, config| {
        token_with_content_type(repository, address, config, Some("text/html"))
    })
    .await;

    assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    assert_eq!(
        response.headers().get("content-security-policy").unwrap(),
        "default-src 'none'; sandbox"
    );
}

#[tokio::test]
async fn head_with_a_range_is_rejected() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");
    let server = build_test_server(immutable_store, mutable_store, presign_config());

    let response = server
        .method(
            Method::HEAD,
            "/v1/presigned/not-a-repository/not-an-address",
        )
        .add_header(RANGE, "bytes=0-9")
        .await;

    assert_eq!(response.status_code(), StatusCode::METHOD_NOT_ALLOWED);
}
