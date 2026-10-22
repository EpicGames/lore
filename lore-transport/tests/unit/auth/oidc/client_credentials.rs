// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The client credentials grant against a stub provider: the request under each client
//! authentication method the provider names, and the mapping of refusals.
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use base64::Engine;
use lore_transport::Authentication;
use lore_transport::auth::oidc;
use lore_transport::auth::oidc::OidcAuthentication;
use lore_transport::auth::oidc::client_credentials::GRANT_TYPE;
use lore_transport::auth::oidc::client_credentials::basic_authorization;
use lore_transport::types::AuthenticationToken;
use lore_transport::types::Oidc;

use super::provider::*;

const CLIENT_ID: &str = "ci-bot";
const CLIENT_SECRET: &str = "s3cr3t";
const SERVICE_ACCOUNT: &str = "service-account-1";

fn token_response(issuer: &str) -> Response {
    json(&serde_json::json!({
        "access_token": access_token(issuer, SERVICE_ACCOUNT, "CI Bot"),
        "token_type": "Bearer",
        "expires_in": 300,
    }))
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"error": error, "error_description": "as scripted"}).to_string(),
    )
        .into_response()
}

/// A provider whose discovery document lists `methods` as
/// `token_endpoint_auth_methods_supported`, or leaves it out for `None`, and whose token
/// endpoint answers with `token`.
async fn provider(
    methods: Option<&'static [&'static str]>,
    token: impl Fn(&str) -> Response + Clone + Send + Sync + 'static,
) -> RecordingProvider {
    spawn_recording_provider(move |issuer, request, _| {
        if request.path == "/token" {
            return token(issuer);
        }
        let mut document = document(issuer, issuer);
        if let Some(methods) = methods {
            document["token_endpoint_auth_methods_supported"] = methods.into();
        }
        json(&document)
    })
    .await
}

async fn login(
    provider: &RecordingProvider,
) -> Result<AuthenticationToken, lore_transport::ProtocolError> {
    let oidc = Oidc {
        issuer: provider.issuer.clone(),
        client_id: "lore-cli".to_owned(),
        scopes: vec!["openid".to_owned()],
        ..Default::default()
    };
    OidcAuthentication::new(oidc)
        .authenticate_client(
            &oidc::endpoint(&provider.issuer),
            CLIENT_ID,
            CLIENT_SECRET,
            "corr",
        )
        .await
}

fn expected_basic(client_id: &str, client_secret: &str) -> String {
    let credentials = format!("{client_id}:{client_secret}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

/// RFC 6749 §4.4.2 under `client_secret_basic`, the RFC 8414 default when the provider
/// lists no methods: the form carries the grant alone, the credentials travel in the
/// header, and neither the server's public client nor its scopes are sent.
#[tokio::test]
async fn without_listed_methods_the_credentials_go_in_the_basic_header() {
    let provider = provider(None, token_response).await;

    let authn = login(&provider).await.expect("the login succeeds");

    let requests = provider.requests_to("/token");
    assert_eq!(requests.len(), 1);
    let form = requests[0].form();
    assert_eq!(form.get("grant_type").map(String::as_str), Some(GRANT_TYPE));
    assert_eq!(form.len(), 1, "{form:?}");
    assert_eq!(
        requests[0].header("authorization"),
        Some(expected_basic(CLIENT_ID, CLIENT_SECRET).as_str())
    );

    assert_eq!(authn.user_id, SERVICE_ACCOUNT);
    assert_eq!(authn.user_name, "CI Bot");
    assert_eq!(
        authn.token,
        access_token(&provider.issuer, SERVICE_ACCOUNT, "CI Bot")
    );
}

/// A provider listing only `client_secret_post` gets the credentials in the form, and no
/// `Authorization` header.
#[tokio::test]
async fn a_provider_listing_only_post_gets_the_credentials_in_the_form() {
    let provider = provider(Some(&["client_secret_post"]), token_response).await;

    login(&provider).await.expect("the login succeeds");

    let requests = provider.requests_to("/token");
    assert_eq!(requests.len(), 1);
    let form = requests[0].form();
    assert_eq!(form.get("grant_type").map(String::as_str), Some(GRANT_TYPE));
    assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(
        form.get("client_secret").map(String::as_str),
        Some(CLIENT_SECRET)
    );
    assert_eq!(form.len(), 3, "{form:?}");
    assert!(requests[0].header("authorization").is_none());
}

/// RFC 6749 §2.3.1 has the client support Basic, so it is chosen whenever it is listed.
#[tokio::test]
async fn basic_is_chosen_when_both_methods_are_listed() {
    let provider = provider(
        Some(&[
            "private_key_jwt",
            "client_secret_post",
            "client_secret_basic",
        ]),
        token_response,
    )
    .await;

    login(&provider).await.expect("the login succeeds");

    let requests = provider.requests_to("/token");
    assert!(requests[0].header("authorization").is_some());
    assert!(!requests[0].form().contains_key("client_secret"));
}

/// A provider accepting no client secret method is refused before the secret is sent.
#[tokio::test]
async fn a_provider_accepting_no_secret_method_is_refused_before_any_request() {
    let provider = provider(Some(&["private_key_jwt"]), token_response).await;

    let refused = login(&provider).await.expect_err("no method fits");

    assert!(refused.is_not_supported(), "{refused}");
    assert!(
        refused.to_string().contains("client_secret_basic"),
        "{refused}"
    );
    assert!(provider.requests_to("/token").is_empty());
}

/// Both parts are form-encoded before they are joined, so a `:` in the ID does not move
/// the boundary between ID and secret.
#[test]
fn basic_credentials_are_form_encoded_before_joining() {
    assert_eq!(
        basic_authorization("ci:bot", "p%ss w:rd"),
        expected_basic("ci%3Abot", "p%25ss+w%3Ard")
    );
}

/// `invalid_client` refuses the credentials, so the caller holds no identity.
#[tokio::test]
async fn refused_credentials_are_not_authenticated() {
    let provider = provider(None, |_| {
        refusal(StatusCode::UNAUTHORIZED, "invalid_client")
    })
    .await;

    let refused = login(&provider)
        .await
        .expect_err("the credentials are refused");

    assert!(refused.is_not_authenticated(), "{refused}");
}

/// Any other refusal, such as a client not allowed this grant, carries the provider's
/// description.
#[tokio::test]
async fn other_refusals_carry_the_providers_description() {
    let provider = provider(None, |_| {
        refusal(StatusCode::BAD_REQUEST, "unauthorized_client")
    })
    .await;

    let refused = login(&provider).await.expect_err("the grant is refused");

    let message = refused.to_string();
    assert!(refused.is_internal(), "{message}");
    assert!(message.contains("unauthorized_client"), "{message}");
    assert!(message.contains("as scripted"), "{message}");
    assert!(!message.contains(CLIENT_SECRET), "{message}");
}

/// A refresh token the provider sends is dropped: refreshing it would need the client's
/// secret, which is not stored.
#[tokio::test]
async fn a_refresh_token_in_the_answer_is_dropped() {
    let provider = provider(None, |issuer| {
        json(&serde_json::json!({
            "access_token": access_token(issuer, SERVICE_ACCOUNT, "CI Bot"),
            "token_type": "Bearer",
            "refresh_token": "not-for-a-public-client",
        }))
    })
    .await;

    let authn = login(&provider).await.expect("the login succeeds");

    assert!(authn.refresh_token.is_none());
}
