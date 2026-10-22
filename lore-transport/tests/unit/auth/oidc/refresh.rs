// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The refresh token grant against a stub provider: the request, the rotated refresh token in
//! the answer, and the mapping of refusals.
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use lore_transport::Authentication;
use lore_transport::auth::oidc;
use lore_transport::auth::oidc::OidcAuthentication;
use lore_transport::auth::oidc::refresh::GRANT_TYPE;
use lore_transport::types::AuthenticationToken;
use lore_transport::types::Oidc;

use super::provider::*;

const CLIENT_ID: &str = "lore-cli";
const REFRESH_TOKEN: &str = "refresh-1";

fn token_response(issuer: &str) -> Response {
    json(&serde_json::json!({
        "access_token": access_token(issuer, "alice", "Alice"),
        "token_type": "Bearer",
        "expires_in": 300,
        "refresh_token": "refresh-2",
        "scope": "openid offline_access",
    }))
}

fn refusal(error: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"error": error, "error_description": "as scripted"}).to_string(),
    )
        .into_response()
}

/// A provider whose token endpoint answers with `token`.
async fn provider(
    token: impl Fn(&str) -> Response + Clone + Send + Sync + 'static,
) -> RecordingProvider {
    spawn_recording_provider(move |issuer, request, _| {
        if request.path == "/token" {
            return token(issuer);
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

async fn refresh(
    provider: &RecordingProvider,
) -> Result<AuthenticationToken, lore_transport::ProtocolError> {
    OidcAuthentication::new(oidc(&provider.issuer))
        .refresh_authentication(&oidc::endpoint(&provider.issuer), REFRESH_TOKEN, "corr")
        .await
}

/// RFC 6749 §6 for a public client: the grant, the refresh token and the client ID, and no
/// scope, so the provider grants the login's. The answer carries the new access token and the
/// rotated refresh token.
#[tokio::test]
async fn the_refresh_token_is_traded_for_a_new_token_and_its_successor() {
    let provider = provider(token_response).await;

    let authn = refresh(&provider).await.expect("the refresh succeeds");

    let requests = provider.requests_to("/token");
    assert_eq!(requests.len(), 1);
    let form = requests[0].form();
    assert_eq!(form.get("grant_type").map(String::as_str), Some(GRANT_TYPE));
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some(REFRESH_TOKEN)
    );
    assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(form.len(), 3, "{form:?}");
    assert!(requests[0].header("authorization").is_none());

    assert_eq!(
        authn.token,
        access_token(&provider.issuer, "alice", "Alice")
    );
    assert_eq!(authn.user_id, "alice");
    assert_eq!(authn.refresh_token.as_deref(), Some("refresh-2"));
    assert_eq!(authn.scope.as_deref(), Some("openid offline_access"));
}

/// `invalid_grant` refuses the refresh token as spent, expired or revoked, so the caller holds
/// no identity until it logs in again.
#[tokio::test]
async fn a_refused_refresh_token_is_not_authenticated() {
    let provider = provider(|_| refusal("invalid_grant")).await;

    let refused = refresh(&provider)
        .await
        .expect_err("the refresh token is refused");

    assert!(refused.is_not_authenticated(), "{refused}");
}

/// Any other refusal, such as a client not allowed the grant, carries the provider's
/// description, and not the refresh token.
#[tokio::test]
async fn other_refusals_carry_the_providers_description() {
    let provider = provider(|_| refusal("unauthorized_client")).await;

    let refused = refresh(&provider).await.expect_err("the grant is refused");

    let message = refused.to_string();
    assert!(refused.is_internal(), "{message}");
    assert!(message.contains("unauthorized_client"), "{message}");
    assert!(!message.contains(REFRESH_TOKEN), "{message}");
}

/// An endpoint naming another issuer is refused before any request, so a refresh token never
/// reaches a provider other than the one selected. Nothing was sent, so it is `Disconnected`.
#[tokio::test]
async fn an_endpoint_naming_another_issuer_is_refused() {
    let provider = provider(token_response).await;

    let refused = OidcAuthentication::new(oidc(&provider.issuer))
        .refresh_authentication(
            &oidc::endpoint("https://other.test.invalid"),
            REFRESH_TOKEN,
            "corr",
        )
        .await
        .expect_err("the endpoint names another issuer");

    assert!(refused.is_disconnected(), "{refused}");
    assert!(provider.requests_to("/token").is_empty());
}

/// A port nothing listens on, for an endpoint that refuses the connection.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
    listener.local_addr().expect("the bound address").port()
}

/// A token endpoint that refuses the connection received nothing, so the failure is
/// `Disconnected` and the refresh token unspent.
#[tokio::test]
async fn a_grant_that_cannot_connect_is_disconnected() {
    let token_endpoint_base = format!("http://127.0.0.1:{}", closed_port());
    let provider =
        spawn_recording_provider(move |issuer, _, _| json(&document(issuer, &token_endpoint_base)))
            .await;

    let failed = refresh(&provider).await.expect_err("nothing listens");

    assert!(failed.is_disconnected(), "{failed}");
}

/// A provider that cannot be discovered is sent no grant, whatever the cause, so the failure is
/// `Disconnected`.
#[tokio::test]
async fn an_undiscoverable_provider_is_disconnected() {
    let provider = spawn_recording_provider(|_, _, _| {
        (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response()
    })
    .await;

    let failed = refresh(&provider).await.expect_err("discovery fails");

    assert!(failed.is_disconnected(), "{failed}");
    assert!(provider.requests_to("/token").is_empty());
}

/// An answer that arrived, but that the client cannot use, may come from a provider that has
/// consumed the refresh token, so the failure is not `Disconnected`.
#[tokio::test]
async fn an_answer_from_the_provider_is_never_disconnected() {
    let unusable = provider(|_| {
        json(&serde_json::json!({
            "access_token": "not-a-jwt",
            "token_type": "Bearer",
            "refresh_token": "refresh-2",
        }))
    })
    .await;
    let server_error =
        provider(|_| (StatusCode::INTERNAL_SERVER_ERROR, "after consuming it").into_response())
            .await;

    for provider in [unusable, server_error] {
        let failed = refresh(&provider)
            .await
            .expect_err("the answer is unusable");

        assert!(!failed.is_disconnected(), "{failed}");
        assert_eq!(provider.requests_to("/token").len(), 1);
    }
}
