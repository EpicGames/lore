// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use axum::http::StatusCode;
use lore_base::runtime::LORE_CONTEXT;
use lore_server::http::security_headers::ContentTypePolicy;
use rand::random;
use serde_json::json;

use crate::http::test_utils::MISSING_ADDRESS;
use crate::http::test_utils::build_test_server;
use crate::http::test_utils::content_type_policy;
use crate::http::test_utils::presign_config_with_policy;
use crate::store::test_support::test_store_create;

async fn mint(body: serde_json::Value) -> axum_test::TestResponse {
    mint_with_policy(body, ContentTypePolicy::default()).await
}

/// Posts `body` to the mint endpoint of a server whose allowlist comes from
/// `policy`. The store is fresh, so the address does not exist and requests
/// that pass validation reach the existence check.
async fn mint_with_policy(
    body: serde_json::Value,
    policy: ContentTypePolicy,
) -> axum_test::TestResponse {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<lore_revision::lore::RepositoryId>();
            let repo_hex = format!("{repository}");
            let address = MISSING_ADDRESS;

            let server = build_test_server(
                immutable_store,
                mutable_store,
                Some(presign_config_with_policy(policy)),
            );

            server
                .post(&format!(
                    "/v1/repository/{repo_hex}/content/{address}/presign"
                ))
                .json(&body)
                .await
        })
        .await
}

#[tokio::test]
async fn returns_404_when_address_not_found() {
    let response = mint(json!({"ttl_seconds": 3600})).await;
    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
}

/// The S3 default type passes the allowlist, so it reaches the existence
/// check and returns 404 rather than 400.
#[tokio::test]
async fn accepts_s3_binary_octet_stream() {
    let response = mint(json!({"content_type": "binary/octet-stream"})).await;
    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn returns_400_for_disallowed_content_type() {
    let response = mint(json!({"content_type": "text/html"})).await;
    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
}

/// A type added through config passes the allowlist, so it reaches the
/// existence check and returns 404 rather than 400.
#[tokio::test]
async fn accepts_configured_extra_content_type() {
    let response = mint_with_policy(
        json!({"content_type": "application/zip"}),
        content_type_policy(&["application/zip"], &[]),
    )
    .await;

    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
}

/// A built-in type removed through config is rejected at mint.
#[tokio::test]
async fn returns_400_for_configured_denied_content_type() {
    let response = mint_with_policy(
        json!({"content_type": "application/pdf"}),
        content_type_policy(&[], &["application/pdf"]),
    )
    .await;

    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn returns_400_for_unserializable_header_value() {
    // Allowlisted media type, but a control char in the parameter makes it
    // an invalid header value; mint must reject rather than let redeem 500.
    let response = mint(json!({"content_type": "image/png; x=\u{7}"})).await;
    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
}

mod handler {
    use std::sync::Arc;

    use axum::Extension;
    use axum::Json;
    use axum::extract::Path;
    use axum::extract::State;
    use axum::http::Extensions;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt as _;
    use lore_server::http::presign_token::token_hash;
    use lore_server::http::repositories::repository::contents::content::presign_repository_content::handler;

    use super::*;
    use crate::http::test_utils::presign_config;
    use crate::http::test_utils::put_compressed_content;
    use crate::http::test_utils::test_state;
    use crate::log_capture::CapturedLogs;

    /// Minting logs the same token hash a redemption of the URL logs, beside the minter.
    #[tokio::test]
    async fn a_minted_url_is_logged_with_its_token_hash_and_minter() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = random::<lore_revision::lore::RepositoryId>();
                let address = put_compressed_content(immutable_store.clone(), repository).await;
                let (repository, address) = (format!("{repository}"), format!("{address}"));
                let state = test_state(immutable_store, mutable_store, Some(presign_config()));

                let (logs, _guard) = CapturedLogs::start();
                let response = handler(
                    State(Arc::new(state)),
                    Path((repository.clone(), address.clone())),
                    Extension(None),
                    Extensions::new(),
                    HeaderMap::new(),
                    Json(serde_json::from_value(json!({})).unwrap()),
                )
                .await
                .into_response();

                assert_eq!(response.status(), StatusCode::OK);
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let url_suffix = body["url_suffix"].as_str().expect("url_suffix");
                let (_, token) = url_suffix.split_once("?token=").expect("token in URL");
                let fields = logs.event("Minted presigned URL");
                assert_eq!(fields["token_hash"], token_hash(token));
                assert_eq!(fields["user_id"], "<unknown>");
                assert_eq!(fields["key_id"], "test_key_id_1234");
                assert_eq!(fields["repository"], repository);
                assert_eq!(fields["address"], address);
                assert_eq!(fields["expires_at"], body["expires_at"]);
                assert!(!logs.text().contains(token), "{}", logs.text());
            })
            .await;
    }
}
