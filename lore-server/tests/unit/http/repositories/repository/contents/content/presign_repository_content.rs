// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use axum::http::StatusCode;
use axum_test::TestServer;
use lore_base::runtime::LORE_CONTEXT;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::http::security_headers::ContentTypePolicy;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use rand::random;
use serde_json::json;

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
            let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

            let test_health = ServerHealth::new_without_availability(immutable_store.clone());
            let state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: None,
                repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
                max_file_size: 100,
                presign_config: Some(presign_config_with_policy(policy)),
            };
            let settings = LoreHttpServerSettings::test_default();
            let server =
                TestServer::new(create_router(state, test_health, &settings)).unwrap();

            server
                .post(&format!("/v1/repository/{repo_hex}/content/{address}/presign"))
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
