// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The token exchange against a stub provider: the carrier each template puts on the wire,
//! the send-check in front of a separate exchange host, and the mapping of refusals; and the
//! login exchange of an external credential.
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use lore_base::types::RepositoryId;
use lore_transport::Authentication;
use lore_transport::auth::oidc;
use lore_transport::auth::oidc::OidcAuthentication;
use lore_transport::auth::oidc::exchange::ACCESS_TOKEN_TYPE;
use lore_transport::auth::oidc::exchange::GRANT_TYPE;
use lore_transport::types::Oidc;

use super::provider::*;

const CLIENT_ID: &str = "lore-cli";
const PARTITION: &str = "00112233445566778899aabbccddeeff";
const RESOURCE_TEMPLATE: &str = "https://lore.example.com/partitions/{id}";
const SCOPE_TEMPLATE: &str = "partition:{id}";
const EXPIRES: u64 = 2_000_000_000;

fn partition() -> RepositoryId {
    PARTITION.parse().expect("a repository id")
}

fn oidc(issuer: &str) -> Oidc {
    Oidc {
        issuer: issuer.to_owned(),
        client_id: CLIENT_ID.to_owned(),
        ..Default::default()
    }
}

fn with_resource_template(issuer: &str) -> Oidc {
    Oidc {
        resource_template: Some(RESOURCE_TEMPLATE.to_owned()),
        token_exchange_issuer: Some(issuer.to_owned()),
        ..oidc(issuer)
    }
}

fn with_scope_template(issuer: &str) -> Oidc {
    Oidc {
        scope_template: Some(SCOPE_TEMPLATE.to_owned()),
        token_exchange_issuer: Some(issuer.to_owned()),
        ..oidc(issuer)
    }
}

/// A login token the issuer means for the hosts `root_domains` names.
fn login_token(issuer: &str, root_domains: &[&str]) -> String {
    jwt(&serde_json::json!({
        "iss": issuer,
        "sub": "user-1",
        "aud": ["lore-api"],
        "root_domains": root_domains,
        "exp": EXPIRES,
        "iat": 1_900_000_000,
    }))
}

fn exchanged_token(issuer: &str) -> String {
    jwt(&serde_json::json!({
        "iss": issuer,
        "sub": "user-1",
        "aud": ["https://lore.example.com"],
        "resources": [{"resource_id": format!("urc-{PARTITION}"), "permission": ["read"]}],
        "exp": EXPIRES + 60,
        "iat": 1_900_000_000,
    }))
}

fn exchange_response(issuer: &str) -> Response {
    json(&serde_json::json!({
        "access_token": exchanged_token(issuer),
        "issued_token_type": ACCESS_TOKEN_TYPE,
        "token_type": "Bearer",
        "expires_in": 300,
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

/// A provider answering discovery, and the token endpoint with `token`.
async fn provider_answering(
    token: impl Fn(&str) -> Response + Clone + Send + Sync + 'static,
) -> RecordingProvider {
    spawn_recording_provider(move |issuer, request, nth| match request.path.as_str() {
        "/token" => token(issuer),
        _ => conforming(issuer, nth),
    })
    .await
}

async fn exchange_for_partition(
    oidc: Oidc,
    authn_token: &str,
) -> Result<lore_transport::types::AuthorizationToken, lore_transport::ProtocolError> {
    let endpoint = oidc::endpoint(&oidc.issuer);
    OidcAuthentication::new(oidc)
        .exchange_for_repository(&endpoint, authn_token, partition(), "corr")
        .await
}

/// The form fields of `request`, each with every value it was sent with.
fn fields(request: &Received) -> Vec<(String, String)> {
    url::form_urlencoded::parse(request.body.as_bytes())
        .into_owned()
        .collect()
}

fn values<'a>(fields: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    fields
        .iter()
        .filter(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
        .collect()
}

/// RFC 8693 §2.1 with one RFC 8707 `resource`: what `resource_template` renders, and no
/// `scope` beside it.
#[tokio::test]
async fn an_exchange_names_the_partition_as_one_resource() {
    let provider = provider_answering(exchange_response).await;
    let authn = login_token(&provider.issuer, &[]);

    let authz = exchange_for_partition(with_resource_template(&provider.issuer), &authn)
        .await
        .expect("the exchange succeeds");

    let requests = provider.requests_to("/token");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, axum::http::Method::POST);
    let fields = fields(&requests[0]);
    assert_eq!(
        values(&fields, "resource"),
        [format!("https://lore.example.com/partitions/{PARTITION}")]
    );
    assert!(values(&fields, "scope").is_empty(), "{fields:?}");
    assert_eq!(values(&fields, "grant_type"), [GRANT_TYPE]);
    assert_eq!(values(&fields, "client_id"), [CLIENT_ID]);
    assert_eq!(values(&fields, "subject_token"), [authn.as_str()]);
    assert_eq!(values(&fields, "subject_token_type"), [ACCESS_TOKEN_TYPE]);
    assert_eq!(values(&fields, "requested_token_type"), [ACCESS_TOKEN_TYPE]);
    assert_eq!(fields.len(), 6, "{fields:?}");
    assert!(requests[0].header("authorization").is_none());

    assert_eq!(authz.token, exchanged_token(&provider.issuer));
    assert_eq!(authz.expires_ms, (EXPIRES + 60) * 1000);
}

/// With `scope_template`, the partition is the one `scope` value and no `resource` is sent.
#[tokio::test]
async fn an_exchange_names_the_partition_as_a_scope() {
    let provider = provider_answering(exchange_response).await;
    let authn = login_token(&provider.issuer, &[]);

    exchange_for_partition(with_scope_template(&provider.issuer), &authn)
        .await
        .expect("the exchange succeeds");

    let requests = provider.requests_to("/token");
    assert_eq!(requests.len(), 1);
    let fields = fields(&requests[0]);
    assert_eq!(values(&fields, "scope"), [format!("partition:{PARTITION}")]);
    assert!(values(&fields, "resource").is_empty(), "{fields:?}");
    assert_eq!(fields.len(), 6, "{fields:?}");
}

/// Custom resources pass through verbatim, one `resource` each in a single exchange,
/// whichever template the deployment names partitions with.
#[tokio::test]
async fn custom_resources_are_sent_verbatim() {
    let provider = provider_answering(exchange_response).await;
    let authn = login_token(&provider.issuer, &[]);
    let oidc = with_scope_template(&provider.issuer);
    let endpoint = oidc::endpoint(&oidc.issuer);

    OidcAuthentication::new(oidc)
        .exchange_for_custom_resources(
            &endpoint,
            &authn,
            &["urc-user-service", "stream:a:*"],
            "corr",
        )
        .await
        .expect("the exchange succeeds");

    let requests = provider.requests_to("/token");
    assert_eq!(requests.len(), 1);
    let fields = fields(&requests[0]);
    assert_eq!(
        values(&fields, "resource"),
        ["urc-user-service", "stream:a:*"]
    );
    assert!(values(&fields, "scope").is_empty(), "{fields:?}");
}

/// An exchange naming no resource would carry no target, so nothing is sent, on Tier 2 or
/// on Tier 1.
#[tokio::test]
async fn custom_resources_without_a_resource_send_nothing() {
    let provider = provider_answering(exchange_response).await;
    let authn = login_token(&provider.issuer, &[]);

    for oidc in [
        with_scope_template(&provider.issuer),
        oidc(&provider.issuer),
    ] {
        let endpoint = oidc::endpoint(&oidc.issuer);
        let result = OidcAuthentication::new(oidc)
            .exchange_for_custom_resources(&endpoint, &authn, &[], "corr")
            .await;
        assert!(result.is_err());
    }

    assert!(provider.received.lock().unwrap().is_empty());
}

/// Without a template the deployment is on Tier 1: the login token is the authorization,
/// and nothing is asked of the provider.
#[tokio::test]
async fn without_a_template_the_login_token_authorizes() {
    let provider = provider_answering(exchange_response).await;
    let authn = login_token(&provider.issuer, &[]);
    let oidc = oidc(&provider.issuer);
    let endpoint = oidc::endpoint(&oidc.issuer);
    let auth = OidcAuthentication::new(oidc);

    let authz = auth
        .exchange_for_repository(&endpoint, &authn, partition(), "corr")
        .await
        .expect("the login token authorizes");
    assert_eq!(authz.token, authn);
    assert_eq!(authz.expires_ms, EXPIRES * 1000);

    let custom = auth
        .exchange_for_custom_resources(&endpoint, &authn, &["urc-user-service"], "corr")
        .await
        .expect("the login token authorizes");
    assert_eq!(custom.token, authn);

    assert!(provider.received.lock().unwrap().is_empty());
}

/// Both templates is a configuration the server refuses, so a client refuses it too rather
/// than pick one.
#[tokio::test]
async fn both_templates_are_refused_before_any_request() {
    let provider = provider_answering(exchange_response).await;
    let authn = login_token(&provider.issuer, &[]);
    let oidc = Oidc {
        scope_template: Some(SCOPE_TEMPLATE.to_owned()),
        ..with_resource_template(&provider.issuer)
    };

    let refused = exchange_for_partition(oidc, &authn)
        .await
        .expect_err("two carriers are refused");

    assert!(refused.is_internal(), "{refused}");
    assert!(refused.to_string().contains("scope_template"), "{refused}");
    assert!(provider.received.lock().unwrap().is_empty());
}

/// A template without `{id}` would ask for the same resource or scope for every partition,
/// so it is refused before any request.
#[tokio::test]
async fn a_template_without_the_partition_placeholder_is_refused() {
    let provider = provider_answering(exchange_response).await;
    let authn = login_token(&provider.issuer, &[]);
    let cases = [
        Oidc {
            resource_template: Some("https://lore.example.com/partitions".to_owned()),
            ..with_resource_template(&provider.issuer)
        },
        Oidc {
            scope_template: Some("partition".to_owned()),
            ..with_scope_template(&provider.issuer)
        },
    ];

    for oidc in cases {
        let refused = exchange_for_partition(oidc, &authn)
            .await
            .expect_err("a template naming no partition is refused");
        let message = refused.to_string();
        assert!(refused.is_internal(), "{message}");
        assert!(message.contains("{id}"), "{message}");
    }
    assert!(provider.received.lock().unwrap().is_empty());
}

/// An exchange host other than the login issuer's receives the login token only if the
/// token's `root_domains` claim names it. When it does not, the exchange fails before any
/// request, naming the host and the claim.
#[tokio::test]
async fn a_separate_exchange_host_needs_the_tokens_root_domains() {
    let login = provider_answering(exchange_response).await;
    let exchange_host = spawn_recording_provider_named("localhost", |issuer, request, nth| {
        match request.path.as_str() {
            "/token" => exchange_response(issuer),
            _ => conforming(issuer, nth),
        }
    })
    .await;
    let oidc = Oidc {
        token_exchange_issuer: Some(exchange_host.issuer.clone()),
        ..with_resource_template(&login.issuer)
    };

    let unnamed = login_token(&login.issuer, &["127.0.0.1"]);
    let refused = exchange_for_partition(oidc.clone(), &unnamed)
        .await
        .expect_err("the exchange host is not named");
    let message = refused.to_string();
    assert!(message.contains("'localhost'"), "{message}");
    assert!(message.contains("'root_domains'"), "{message}");
    assert!(exchange_host.received.lock().unwrap().is_empty());
    assert!(login.received.lock().unwrap().is_empty());

    let named = login_token(&login.issuer, &["127.0.0.1", "localhost"]);
    let authz = exchange_for_partition(oidc, &named)
        .await
        .expect("the exchange host is named");
    assert_eq!(authz.token, exchanged_token(&exchange_host.issuer));
    let requests = exchange_host.requests_to("/token");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        values(&fields(&requests[0]), "subject_token"),
        [named.as_str()]
    );
    assert!(login.requests_to("/token").is_empty());
}

/// A separate exchange issuer whose discovery document puts the token endpoint on a host
/// the token does not name is refused too, after discovery and before the token is sent.
#[tokio::test]
async fn a_separate_issuers_token_endpoint_needs_the_tokens_root_domains() {
    let login = provider_answering(exchange_response).await;
    let elsewhere = login.issuer.clone();
    let exchange_host =
        spawn_recording_provider_named("localhost", move |issuer, _request, _nth| {
            json(&document(
                issuer,
                &elsewhere.replace("127.0.0.1", "127.0.0.2"),
            ))
        })
        .await;
    let oidc = Oidc {
        token_exchange_issuer: Some(exchange_host.issuer.clone()),
        ..with_resource_template(&login.issuer)
    };

    let refused = exchange_for_partition(oidc, &login_token(&login.issuer, &["localhost"]))
        .await
        .expect_err("the token endpoint's host is not named");

    assert!(refused.to_string().contains("'127.0.0.2'"), "{refused}");
    assert!(exchange_host.requests_to("/token").is_empty());
    assert!(login.requests_to("/token").is_empty());
}

/// A refused login token means the caller must authenticate again; a refused partition
/// means the caller holds no grant on it; anything else is an error naming the refusal.
#[tokio::test]
async fn refusals_map_to_the_state_they_describe() {
    for (error, check) in [
        (
            "invalid_grant",
            lore_transport::ProtocolError::is_not_authenticated
                as fn(&lore_transport::ProtocolError) -> bool,
        ),
        (
            "invalid_target",
            lore_transport::ProtocolError::is_not_authorized,
        ),
        (
            "invalid_scope",
            lore_transport::ProtocolError::is_not_authorized,
        ),
        (
            "access_denied",
            lore_transport::ProtocolError::is_not_authorized,
        ),
        (
            "unsupported_grant_type",
            lore_transport::ProtocolError::is_internal,
        ),
    ] {
        let provider = provider_answering(move |_| refusal(error)).await;
        let authn = login_token(&provider.issuer, &[]);

        let refused = exchange_for_partition(with_resource_template(&provider.issuer), &authn)
            .await
            .expect_err("the exchange is refused");

        assert!(check(&refused), "{error}: {refused}");
    }
}

/// Only a Bearer JWT access token is accepted from the exchange.
#[tokio::test]
async fn an_unusable_exchanged_token_is_an_error() {
    let unusable = [
        serde_json::json!({
            "access_token": "opaque", "issued_token_type": ACCESS_TOKEN_TYPE,
            "token_type": "Bearer",
        }),
        serde_json::json!({
            "access_token": exchanged_token("x"), "issued_token_type": ACCESS_TOKEN_TYPE,
            "token_type": "DPoP",
        }),
        serde_json::json!({
            "access_token": exchanged_token("x"),
            "issued_token_type": "urn:ietf:params:oauth:token-type:refresh_token",
            "token_type": "Bearer",
        }),
    ];
    for answer in unusable {
        let provider = provider_answering({
            let answer = answer.clone();
            move |_| json(&answer)
        })
        .await;
        let authn = login_token(&provider.issuer, &[]);

        let refused = exchange_for_partition(with_resource_template(&provider.issuer), &authn)
            .await
            .expect_err("the token is refused");

        assert!(refused.is_internal(), "{answer}: {refused}");
    }
}

const API_KEY: &str = "key-id.secret";

/// A login answer in the device grant's shape, for the user the API key is bound to.
fn login_response(issuer: &str) -> Response {
    json(&serde_json::json!({
        "access_token": access_token(issuer, "key-user", "Key User"),
        "issued_token_type": ACCESS_TOKEN_TYPE,
        "token_type": "Bearer",
        "expires_in": 300,
        "refresh_token": "refresh-1",
    }))
}

async fn login_with(
    oidc: Oidc,
    token_type: &str,
) -> Result<lore_transport::types::AuthenticationToken, lore_transport::ProtocolError> {
    let endpoint = oidc::endpoint(&oidc.issuer);
    OidcAuthentication::new(oidc)
        .exchange_external_token(&endpoint, API_KEY, token_type, "corr")
        .await
}

/// RFC 8693 §2.1 with the key as an opaque `subject_token` and the type verbatim as
/// `subject_token_type`, whatever its spelling, and no target. It goes to the login issuer
/// even when the server names a separate exchange issuer.
#[tokio::test]
async fn an_external_token_login_sends_the_type_verbatim() {
    for token_type in [
        "api-key",
        "urn:lore:params:oauth:token-type:api-key",
        "urn:example:token-type:other",
    ] {
        let provider = provider_answering(login_response).await;
        let oidc = Oidc {
            token_exchange_issuer: Some("https://exchange.invalid".to_owned()),
            ..with_resource_template(&provider.issuer)
        };

        let authn = login_with(oidc, token_type)
            .await
            .expect("the login succeeds");

        let requests = provider.requests_to("/token");
        assert_eq!(requests.len(), 1, "{token_type}");
        let fields = fields(&requests[0]);
        assert_eq!(
            fields,
            [
                ("grant_type", GRANT_TYPE),
                ("client_id", CLIENT_ID),
                ("subject_token", API_KEY),
                ("subject_token_type", token_type),
                ("requested_token_type", ACCESS_TOKEN_TYPE),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned())),
            "{token_type}"
        );
        assert!(requests[0].header("authorization").is_none());

        assert_eq!(
            authn.token,
            access_token(&provider.issuer, "key-user", "Key User")
        );
        assert_eq!(authn.user_id, "key-user");
        assert_eq!(authn.user_name, "Key User");
        assert_eq!(authn.refresh_token.as_deref(), Some("refresh-1"));
    }
}

/// A refused credential means the caller is not authenticated; any other refusal, such as
/// a type the issuer has not enabled, is an error carrying the provider's description.
#[tokio::test]
async fn login_refusals_map_to_the_state_they_describe() {
    let provider = provider_answering(|_| refusal("invalid_grant")).await;
    let refused = login_with(oidc(&provider.issuer), "api-key")
        .await
        .expect_err("the key is refused");
    assert!(refused.is_not_authenticated(), "{refused}");

    let provider = provider_answering(|_| refusal("invalid_request")).await;
    let refused = login_with(oidc(&provider.issuer), "api-key")
        .await
        .expect_err("the type is refused");
    let message = refused.to_string();
    assert!(refused.is_internal(), "{message}");
    assert!(
        message.contains("invalid_request: as scripted"),
        "{message}"
    );
    assert!(message.contains("api-key"), "{message}");
}

/// The login token must be a Bearer JWT naming someone, as the device grant's must.
#[tokio::test]
async fn an_unusable_login_token_is_an_error() {
    let provider = provider_answering(|_| {
        json(&serde_json::json!({"access_token": "opaque", "token_type": "Bearer"}))
    })
    .await;

    let refused = login_with(oidc(&provider.issuer), "api-key")
        .await
        .expect_err("the token is refused");

    assert!(refused.is_internal(), "{refused}");
}
