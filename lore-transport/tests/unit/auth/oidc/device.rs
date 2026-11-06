// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The device authorization grant against a stub provider: the shape of both requests,
//! and the state each answer of the token endpoint drives.
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use lore_transport::Authentication;
use lore_transport::auth::AuthMode;
use lore_transport::auth::authentication;
use lore_transport::auth::oidc;
use lore_transport::auth::oidc::MAX_RESPONSE_BYTES;
use lore_transport::auth::oidc::OidcAuthentication;
use lore_transport::auth::oidc::device;
use lore_transport::types::AuthSessionPoll;
use lore_transport::types::EnvironmentConfig;
use lore_transport::types::Oidc;

use super::provider::*;

const CLIENT_ID: &str = "lore-cli";
const DEVICE_CODE: &str = "device-code-1";
const USER_CODE: &str = "WDJB-MJHT";

fn oidc(issuer: &str) -> Oidc {
    Oidc {
        issuer: issuer.to_owned(),
        client_id: CLIENT_ID.to_owned(),
        scopes: vec!["openid".to_owned()],
        ..Default::default()
    }
}

fn authentication(issuer: &str) -> (OidcAuthentication, String) {
    (
        OidcAuthentication::new(oidc(issuer)),
        oidc::endpoint(issuer),
    )
}

fn device_authorization(issuer: &str, complete: bool) -> serde_json::Value {
    let mut response = serde_json::json!({
        "device_code": DEVICE_CODE,
        "user_code": USER_CODE,
        "verification_uri": format!("{issuer}/activate"),
        "expires_in": 600,
        "interval": 7,
    });
    if complete {
        response["verification_uri_complete"] =
            format!("{issuer}/activate?user_code={USER_CODE}").into();
    }
    response
}

fn refusal(error: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"error": error, "error_description": "as scripted"}).to_string(),
    )
        .into_response()
}

/// A provider answering discovery, the device authorization request, and the token
/// request with `token`.
async fn provider_answering_polls(
    token: impl Fn(&str, usize) -> Response + Clone + Send + Sync + 'static,
) -> RecordingProvider {
    spawn_recording_provider(move |issuer, request, nth| match request.path.as_str() {
        "/device" => json(&device_authorization(issuer, true)),
        "/token" => token(issuer, nth),
        _ => conforming(issuer, nth),
    })
    .await
}

async fn start(provider: &RecordingProvider) -> lore_transport::types::AuthSession {
    let (auth, endpoint) = authentication(&provider.issuer);
    auth.start_auth_session(&endpoint, "client-state", "corr")
        .await
        .expect("the session opens")
}

async fn poll(
    provider: &RecordingProvider,
) -> Result<AuthSessionPoll, lore_transport::ProtocolError> {
    let (auth, endpoint) = authentication(&provider.issuer);
    auth.poll_auth_session(&endpoint, "client-state", DEVICE_CODE, "corr")
        .await
}

/// RFC 8628 §3.1: a form POST naming the client and the scope, and §3.2's answer read into
/// the session.
#[tokio::test]
async fn a_login_opens_a_session_at_the_device_authorization_endpoint() {
    let provider = provider_answering_polls(|_, _| refusal("authorization_pending")).await;

    let session = start(&provider).await;

    let requests = provider.requests_to("/device");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, axum::http::Method::POST);
    assert_eq!(
        request.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    let form = request.form();
    assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(
        form.get("scope").map(String::as_str),
        Some("openid offline_access")
    );
    assert_eq!(form.len(), 2, "{form:?}");

    assert_eq!(session.session_code, DEVICE_CODE);
    assert_eq!(
        session.login_url,
        format!("{}/activate?user_code={USER_CODE}", provider.issuer)
    );
    assert_eq!(session.user_code, USER_CODE);
    assert_eq!(session.interval, Duration::from_secs(7));
    assert_eq!(session.expires_in, Duration::from_secs(600));
}

/// `offline_access` is asked for on every login, once, whatever the server's defaults.
#[test]
fn offline_access_is_requested_once() {
    let scopes = |scopes: &[&str]| Oidc {
        scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        ..Default::default()
    };
    assert_eq!(device::scope(&scopes(&[])), "offline_access");
    assert_eq!(
        device::scope(&scopes(&["openid", "profile"])),
        "openid profile offline_access"
    );
    assert_eq!(
        device::scope(&scopes(&["offline_access", "openid"])),
        "offline_access openid"
    );
    assert_eq!(device::scope(&scopes(&[""])), "offline_access");
}

/// `verification_uri_complete` and `interval` are optional (§3.2). Without the first the
/// user opens the plain URI and types the code; without the second the client polls at
/// the RFC's default.
#[tokio::test]
async fn without_a_complete_verification_uri_the_user_enters_the_code_at_the_plain_one() {
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/device" => {
            let mut response = device_authorization(issuer, false);
            response.as_object_mut().unwrap().remove("interval");
            json(&response)
        }
        _ => conforming(issuer, nth),
    })
    .await;

    let session = start(&provider).await;

    assert_eq!(session.login_url, format!("{}/activate", provider.issuer));
    assert_eq!(session.user_code, USER_CODE);
    assert_eq!(session.interval, Duration::from_secs(5));
}

/// The login URL is handed to the browser, so it must be a web page: `https`, or `http` to
/// a loopback host when the issuer is one. A provider answering anything else, such as a
/// scheme a local handler would open, is refused.
#[tokio::test]
async fn a_verification_url_that_is_not_a_web_page_is_refused() {
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/device" => {
            let mut response = device_authorization(issuer, false);
            response["verification_uri"] = match nth {
                0 => "file:///etc/hosts",
                1 => "mailto:alice@example.com",
                _ => "http://idp.example.com/activate",
            }
            .into();
            json(&response)
        }
        _ => conforming(issuer, nth),
    })
    .await;
    let (auth, endpoint) = authentication(&provider.issuer);

    for what in ["a file URL", "a mailto URL", "plain http off the loopback"] {
        let refused = auth
            .start_auth_session(&endpoint, "client-state", "corr")
            .await
            .expect_err(what);
        assert!(refused.is_internal(), "{what}: {refused}");
    }
}

#[tokio::test]
async fn a_provider_without_a_device_authorization_endpoint_is_refused() {
    let provider = spawn_recording_provider(|issuer, _, _| {
        json(&serde_json::json!({
            "issuer": issuer,
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
        }))
    })
    .await;
    let (auth, endpoint) = authentication(&provider.issuer);

    let refused = auth
        .start_auth_session(&endpoint, "client-state", "corr")
        .await
        .expect_err("no endpoint to open a session at");

    assert!(refused.is_not_supported(), "{refused}");
    assert!(provider.requests_to("/device").is_empty());
}

/// A refusal of the authorization request itself (§3.2 names RFC 6749 §5.2's errors) is a
/// failed login, not a session.
#[tokio::test]
async fn a_refused_device_authorization_fails_the_login() {
    let provider = spawn_recording_provider(|issuer, request, nth| match request.path.as_str() {
        "/device" => refusal("invalid_scope"),
        _ => conforming(issuer, nth),
    })
    .await;
    let (auth, endpoint) = authentication(&provider.issuer);

    let refused = auth
        .start_auth_session(&endpoint, "client-state", "corr")
        .await
        .expect_err("the provider refused");

    assert!(refused.is_internal(), "{refused}");
}

/// RFC 8628 §3.4: the token request names the grant, the device code and the client, and
/// carries no credential.
#[tokio::test]
async fn a_poll_posts_the_device_code_grant_to_the_token_endpoint() {
    let provider = provider_answering_polls(|_, _| refusal("authorization_pending")).await;

    poll(&provider).await.expect("pending is a state");

    let requests = provider.requests_to("/token");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, axum::http::Method::POST);
    assert_eq!(request.header("authorization"), None);
    let form = request.form();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("urn:ietf:params:oauth:grant-type:device_code")
    );
    assert_eq!(
        form.get("device_code").map(String::as_str),
        Some(DEVICE_CODE)
    );
    assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(form.len(), 3, "{form:?}");
}

/// §3.5: `authorization_pending` and `slow_down` are states of the poll, `access_denied`
/// and `expired_token` end it, each as its own error, and an error the RFC does not name
/// ends it too.
#[tokio::test]
async fn each_refusal_of_the_poll_drives_its_state() {
    let provider = provider_answering_polls(|_, nth| {
        refusal(match nth {
            0 => "authorization_pending",
            1 => "slow_down",
            2 => "access_denied",
            3 => "expired_token",
            _ => "invalid_grant",
        })
    })
    .await;

    assert!(matches!(
        poll(&provider).await,
        Ok(AuthSessionPoll::Pending)
    ));
    assert!(matches!(
        poll(&provider).await,
        Ok(AuthSessionPoll::SlowDown)
    ));

    let denied = poll(&provider).await.expect_err("the user declined");
    assert!(denied.is_not_authenticated(), "{denied}");

    let expired = poll(&provider).await.expect_err("the session expired");
    assert!(expired.is_internal(), "{expired}");
    assert!(!expired.is_not_authenticated(), "{expired}");

    let unknown = poll(&provider).await.expect_err("the grant was refused");
    assert!(unknown.is_internal(), "{unknown}");
}

/// An approved poll yields the access token as the credential, the identity from its
/// claims, the refresh token and the granted scope. The ID token names the user for
/// display and reaches nothing else: not the token, not the refresh token, not any field.
#[tokio::test]
async fn an_approved_poll_carries_the_access_token_and_never_the_id_token() {
    let id_token = |issuer: &str| {
        jwt(&serde_json::json!({
            "iss": issuer,
            "sub": "alice",
            "aud": CLIENT_ID,
            "exp": 2_000_000_000,
            "name": "Alice From The ID Token",
            "preferred_username": "alice.id",
        }))
    };
    let provider = provider_answering_polls(move |issuer, _| {
        json(&serde_json::json!({
            "access_token": access_token(issuer, "alice", "Alice From The Access Token"),
            "token_type": "Bearer",
            "expires_in": 300,
            "refresh_token": "refresh-1",
            "scope": "openid offline_access",
            "id_token": id_token(issuer),
        }))
    })
    .await;

    let Ok(AuthSessionPoll::Complete(token)) = poll(&provider).await else {
        panic!("the poll completes");
    };

    assert_eq!(
        token.token,
        access_token(&provider.issuer, "alice", "Alice From The Access Token")
    );
    assert_eq!(token.user_id, "alice");
    assert_eq!(token.user_name, "Alice From The ID Token");
    assert_eq!(token.expires_ms, 2_000_000_000_000);
    assert_eq!(token.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(token.scope.as_deref(), Some("openid offline_access"));
    assert!(token.acceptable_root_domains.is_empty());
    let carried = format!("{token:?}");
    assert!(
        !carried.contains(&id_token(&provider.issuer)),
        "the ID token must reach no field: {carried}"
    );
}

/// Without an ID token the display name comes from the access token's claims, and a
/// provider that puts a display name in neither leaves it empty.
#[tokio::test]
async fn the_display_name_falls_back_to_the_access_tokens_claims() {
    let provider = provider_answering_polls(|issuer, nth| {
        let access_token = match nth {
            0 => access_token(issuer, "alice", "Alice From The Access Token"),
            _ => jwt(&serde_json::json!({
                "iss": issuer, "sub": "alice", "aud": ["lore.test.invalid"],
                "exp": 2_000_000_000,
            })),
        };
        json(&serde_json::json!({
            "access_token": access_token,
            "token_type": "bearer",
        }))
    })
    .await;

    let Ok(AuthSessionPoll::Complete(named)) = poll(&provider).await else {
        panic!("the poll completes");
    };
    assert_eq!(named.user_name, "Alice From The Access Token");
    assert_eq!(named.refresh_token, None);
    assert_eq!(named.scope, None);

    let Ok(AuthSessionPoll::Complete(nameless)) = poll(&provider).await else {
        panic!("the poll completes");
    };
    assert_eq!(nameless.user_id, "alice");
    assert_eq!(nameless.user_name, "");
}

/// The identity is the access token's `identity` claim where it carries one, `sub` otherwise,
/// and a token whose `identity` is empty is refused.
#[tokio::test]
async fn the_identity_claim_names_the_user() {
    for (identity, expected) in [
        (None, Some("pairwise-sub")),
        (Some(serde_json::json!("stable-oid")), Some("stable-oid")),
        (Some(serde_json::json!("")), None),
        (Some(serde_json::Value::Null), None),
        (Some(serde_json::json!(7)), None),
    ] {
        let carried = identity.clone();
        let provider = provider_answering_polls(move |issuer, _| {
            let mut claims = serde_json::json!({
                "iss": issuer, "sub": "pairwise-sub",
                "aud": ["lore.test.invalid"], "exp": 2_000_000_000,
            });
            if let Some(identity) = &carried {
                claims["identity"] = identity.clone();
            }
            json(&serde_json::json!({ "access_token": jwt(&claims), "token_type": "Bearer" }))
        })
        .await;

        match (poll(&provider).await, expected) {
            (Ok(AuthSessionPoll::Complete(token)), Some(expected)) => {
                assert_eq!(token.user_id, expected, "{identity:?}");
            }
            (Err(refused), None) => assert!(refused.is_internal(), "{refused}"),
            (other, _) => panic!("{identity:?}: {other:?}"),
        }
    }
}

/// The client presents access tokens as `Bearer` and reads its identity from their claims
/// (the LEP's P1), so any other token type, and an opaque token, are refused.
#[tokio::test]
async fn a_token_the_client_cannot_use_is_refused() {
    let provider = provider_answering_polls(|issuer, nth| match nth {
        0 => json(&serde_json::json!({
            "access_token": access_token(issuer, "alice", "Alice"),
            "token_type": "MAC",
        })),
        _ => json(&serde_json::json!({
            "access_token": "opaque",
            "token_type": "Bearer",
        })),
    })
    .await;

    for what in ["a MAC token", "an opaque token"] {
        let refused = poll(&provider).await.expect_err(what);
        assert!(refused.is_internal(), "{what}: {refused}");
    }
}

/// A token endpoint that fails, or answers in no shape the client knows, fails the poll
/// rather than being read as a state.
#[tokio::test]
async fn an_answer_in_no_known_shape_fails_the_poll() {
    let provider = provider_answering_polls(|_, nth| match nth {
        0 => (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response(),
        1 => (StatusCode::BAD_REQUEST, "not json").into_response(),
        2 => json(&serde_json::json!({"token_type": "Bearer"})),
        _ => (StatusCode::OK, "x".repeat(MAX_RESPONSE_BYTES + 1)).into_response(),
    })
    .await;

    for what in [
        "a server error",
        "a refusal in no shape",
        "a success without a token",
        "an oversized answer",
    ] {
        let failed = poll(&provider).await.expect_err(what);
        assert!(failed.is_internal(), "{what}: {failed}");
    }
}

/// A provider whose endpoint cannot be reached is a disconnection, as an unreachable
/// gRPC auth service is.
#[tokio::test]
async fn an_unreachable_endpoint_is_a_disconnection() {
    let closed = closed_port();
    let unreachable = closed.url.clone();
    let provider = spawn_recording_provider(move |issuer, _, _| {
        json(&serde_json::json!({
            "issuer": issuer,
            "token_endpoint": format!("{unreachable}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "device_authorization_endpoint": format!("{unreachable}/device"),
        }))
    })
    .await;
    let (auth, endpoint) = authentication(&provider.issuer);

    let failed = auth
        .start_auth_session(&endpoint, "client-state", "corr")
        .await
        .expect_err("nothing listens");

    assert!(failed.is_disconnected(), "{failed}");
}

/// The implementation serves the issuer it was selected for. An endpoint naming another
/// is a programming error, refused before any request.
#[tokio::test]
async fn an_endpoint_naming_another_issuer_is_refused() {
    let provider = provider_answering_polls(|_, _| refusal("authorization_pending")).await;
    let auth = OidcAuthentication::new(oidc(&provider.issuer));

    let refused = auth
        .start_auth_session(
            &oidc::endpoint("https://other.test.invalid"),
            "client-state",
            "corr",
        )
        .await
        .expect_err("another issuer");

    assert!(refused.is_internal(), "{refused}");
    assert!(provider.received.lock().unwrap().is_empty());
}

/// Selecting the OIDC path binds the implementation to the client and scopes the server
/// advertises, which is what the grant then presents.
#[tokio::test]
async fn the_selected_path_presents_the_advertised_client() {
    let provider = provider_answering_polls(|_, _| refusal("authorization_pending")).await;
    let environment = EnvironmentConfig {
        endpoint: None,
        config: None,
        oidc: Some(Oidc {
            issuer: provider.issuer.clone(),
            client_id: "advertised-client".to_owned(),
            scopes: vec!["openid".to_owned(), "profile".to_owned()],
            ..Default::default()
        }),
    };
    let selected = authentication::select(AuthMode::Auto, &environment)
        .expect("the one path advertised")
        .expect("authentication is advertised")
        .endpoint;

    let session = selected
        .auth
        .start_auth_session(&selected.url, "client-state", "corr")
        .await
        .expect("the session opens");
    let poll = selected
        .auth
        .poll_auth_session(&selected.url, "client-state", &session.session_code, "corr")
        .await
        .expect("pending is a state");

    assert!(matches!(poll, AuthSessionPoll::Pending));
    let device = provider.requests_to("/device");
    assert_eq!(
        device[0].form().get("client_id").map(String::as_str),
        Some("advertised-client")
    );
    assert_eq!(
        device[0].form().get("scope").map(String::as_str),
        Some("openid profile offline_access")
    );
    let token = provider.requests_to("/token");
    assert_eq!(
        token[0].form().get("client_id").map(String::as_str),
        Some("advertised-client")
    );
}
