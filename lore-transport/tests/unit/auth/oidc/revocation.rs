// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Refresh token revocation against a stub provider: the request, and the mapping of the
//! answers.
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use lore_transport::Authentication;
use lore_transport::ProtocolError;
use lore_transport::auth::oidc;
use lore_transport::auth::oidc::OidcAuthentication;
use lore_transport::types::Oidc;

use super::provider::*;

const CLIENT_ID: &str = "lore-cli";
const REFRESH_TOKEN: &str = "refresh-1";

fn refusal(status: StatusCode, error: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"error": error, "error_description": "as scripted"}).to_string(),
    )
        .into_response()
}

/// A provider whose revocation endpoint answers with `revoke`.
async fn provider(
    revoke: impl Fn() -> Response + Clone + Send + Sync + 'static,
) -> RecordingProvider {
    spawn_recording_provider(move |issuer, request, _| {
        if request.path == "/revoke" {
            return revoke();
        }
        json(&document(issuer, issuer))
    })
    .await
}

fn oidc(issuer: &str) -> Oidc {
    Oidc {
        issuer: issuer.to_owned(),
        client_id: CLIENT_ID.to_owned(),
        scopes: vec!["openid".to_owned()],
        ..Default::default()
    }
}

async fn revoke(provider: &RecordingProvider) -> Result<(), ProtocolError> {
    OidcAuthentication::new(oidc(&provider.issuer))
        .revoke_refresh_token(&oidc::endpoint(&provider.issuer), REFRESH_TOKEN, "corr")
        .await
}

/// RFC 7009 §2.1 for a public client: the token, its hint and the client ID, and no client
/// authentication.
#[tokio::test]
async fn the_refresh_token_is_revoked_as_the_public_client() {
    let provider = provider(|| StatusCode::OK.into_response()).await;

    revoke(&provider).await.expect("the token is revoked");

    let requests = provider.requests_to("/revoke");
    assert_eq!(requests.len(), 1);
    let form = requests[0].form();
    assert_eq!(form.get("token").map(String::as_str), Some(REFRESH_TOKEN));
    assert_eq!(
        form.get("token_type_hint").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(form.len(), 3, "{form:?}");
    assert!(requests[0].header("authorization").is_none());
}

#[tokio::test]
async fn a_provider_without_a_revocation_endpoint_is_not_supported() {
    let provider = spawn_recording_provider(|issuer, _, _| {
        let mut document = document(issuer, issuer);
        document
            .as_object_mut()
            .expect("the document is an object")
            .remove("revocation_endpoint");
        json(&document)
    })
    .await;

    let refused = revoke(&provider).await.expect_err("there is no endpoint");

    assert!(refused.is_not_supported(), "{refused}");
    assert!(provider.requests_to("/revoke").is_empty());
}

/// Every refusal fails the revocation, `unsupported_token_type` included: §2 requires a
/// provider to revoke refresh tokens. The error carries the provider's description, and not
/// the token.
#[tokio::test]
async fn a_refusal_carries_the_providers_description() {
    for (status, error) in [
        (StatusCode::UNAUTHORIZED, "invalid_client"),
        (StatusCode::BAD_REQUEST, "unsupported_token_type"),
    ] {
        let provider = provider(move || refusal(status, error)).await;

        let refused = revoke(&provider).await.expect_err("the request is refused");

        let message = refused.to_string();
        assert!(refused.is_internal(), "{message}");
        assert!(message.contains(error), "{message}");
        assert!(!message.contains(REFRESH_TOKEN), "{message}");
    }
}

/// §2.2.1: a provider that cannot serve the request now answers 503.
#[tokio::test]
async fn an_unavailable_provider_fails_the_revocation() {
    let provider = provider(|| (StatusCode::SERVICE_UNAVAILABLE, "later").into_response()).await;

    let failed = revoke(&provider)
        .await
        .expect_err("the provider is unavailable");

    assert!(failed.is_internal(), "{failed}");
    assert!(failed.to_string().contains("503"), "{failed}");
}

fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
    listener.local_addr().expect("the bound address").port()
}

#[tokio::test]
async fn an_unreachable_revocation_endpoint_is_disconnected() {
    let endpoint_base = format!("http://127.0.0.1:{}", closed_port());
    let provider =
        spawn_recording_provider(move |issuer, _, _| json(&document(issuer, &endpoint_base))).await;

    let failed = revoke(&provider).await.expect_err("nothing listens");

    assert!(failed.is_disconnected(), "{failed}");
}

/// A provider that cannot be discovered may advertise a revocation endpoint, so the failure is
/// reported rather than taken for a provider without one.
#[tokio::test]
async fn an_undiscoverable_provider_fails_the_revocation() {
    let provider = spawn_recording_provider(|_, _, _| {
        (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response()
    })
    .await;

    let failed = revoke(&provider).await.expect_err("discovery fails");

    assert!(failed.is_internal(), "{failed}");
    assert!(provider.requests_to("/revoke").is_empty());
}
