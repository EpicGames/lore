// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use lore_server::auth::jwk::JWKServiceError;
use lore_server::auth::oidc::*;
use lore_server::settings::AuthSettings;
use lore_transport::Oidc;
use tokio::net::TcpListener;

/// Serves a discovery document naming `issuer`, or the stub's own base URL
/// when `None`. Returns the base URL.
async fn spawn_provider(issuer: Option<&str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind discovery provider");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("discovery provider address")
    );
    let issuer = issuer.map_or_else(|| base.clone(), str::to_string);
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(|State(issuer): State<String>| async move {
                format!(r#"{{"issuer":"{issuer}","jwks_uri":"{issuer}/jwks"}}"#)
            }),
        )
        .with_state(issuer);
    lore_base::lore_spawn!(async move {
        axum::serve(listener, app)
            .await
            .expect("serve discovery provider");
    });
    base
}

const AUTH_URL: &str = "ucs-auth://auth.example.com";

fn auth_settings(issuer: &str, oidc_keys: Option<&str>) -> AuthSettings {
    let oidc = oidc_keys.map_or_else(String::new, |keys| format!("[oidc]\n{keys}"));
    let config = format!(
        r#"
            jwt_issuer = "{issuer}"
            jwt_audience = ["lore-service"]
            {oidc}
            "#
    );
    toml::from_str(&config).expect("[server.auth] must parse")
}

#[tokio::test]
async fn nothing_is_advertised_without_the_oidc_table() {
    let auth = auth_settings("https://unreachable.invalid", None);
    let advertised = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect("no table means no discovery");
    assert_eq!(advertised, None);
}

#[tokio::test]
async fn the_discovered_issuer_is_advertised_with_the_configured_fields() {
    let base = spawn_provider(None).await;
    let auth = auth_settings(
        &base,
        Some(
            r#"
                client_id = "lore-cli"
                scopes = ["openid", "offline_access"]
                preferred = true
                token_exchange_issuer = "https://sts.example.com"
                scope_template = "partition:{id}"
                client_credentials_scopes = ["lore"]
                "#,
        ),
    );

    let advertised = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect("discovery succeeds")
        .expect("the table is present");

    assert_eq!(
        advertised,
        Oidc {
            issuer: base,
            client_id: "lore-cli".to_string(),
            scopes: vec!["openid".to_string(), "offline_access".to_string()],
            preferred: true,
            resource_template: None,
            scope_template: Some("partition:{id}".to_string()),
            token_exchange_issuer: Some("https://sts.example.com".to_string()),
            client_credentials_scopes: vec!["lore".to_string()],
        }
    );
}

#[tokio::test]
async fn tier_2_exchanges_at_the_provider_unless_told_otherwise() {
    let base = spawn_provider(None).await;
    let auth = auth_settings(
        &base,
        Some(
            r#"
                client_id = "lore-cli"
                resource_template = "https://lore.example.com/partitions/{id}"
                "#,
        ),
    );

    let advertised = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect("discovery succeeds")
        .expect("the table is present");

    assert_eq!(advertised.token_exchange_issuer, Some(base));
}

/// An empty `token_exchange_issuer` passes validation as unset, so it must
/// default here too rather than reach clients as an empty string.
#[tokio::test]
async fn an_empty_token_exchange_issuer_defaults_to_the_provider() {
    let base = spawn_provider(None).await;
    let auth = auth_settings(
        &base,
        Some(
            r#"
                client_id = "lore-cli"
                token_exchange_issuer = ""
                scope_template = "partition:{id}"
                "#,
        ),
    );

    let advertised = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect("discovery succeeds")
        .expect("the table is present");

    assert_eq!(advertised.token_exchange_issuer, Some(base));
}

#[tokio::test]
async fn empty_templates_advertise_tier_1() {
    let base = spawn_provider(None).await;
    let auth = auth_settings(
        &base,
        Some(
            r#"
                client_id = "lore-cli"
                resource_template = ""
                scope_template = ""
                "#,
        ),
    );

    let advertised = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect("discovery succeeds")
        .expect("the table is present");

    assert_eq!(advertised.resource_template, None);
    assert_eq!(advertised.scope_template, None);
    assert_eq!(advertised.token_exchange_issuer, None);
}

#[tokio::test]
async fn tier_1_advertises_no_token_exchange() {
    let base = spawn_provider(None).await;
    let auth = auth_settings(&base, Some(r#"client_id = "lore-cli""#));

    let advertised = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect("discovery succeeds")
        .expect("the table is present");

    assert_eq!(advertised.token_exchange_issuer, None);
}

/// Beside `auth_url`, `preferred` is the operator's switch. Without it,
/// the provider is the only path and is always preferred.
#[tokio::test]
async fn the_provider_is_preferred_when_it_is_the_only_path() {
    let base = spawn_provider(None).await;
    let auth = auth_settings(&base, Some(r#"client_id = "lore-cli""#));

    let beside_auth_url = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect("discovery succeeds")
        .expect("the table is present");
    let alone = advertised_oidc(&auth, None)
        .await
        .expect("discovery succeeds")
        .expect("the table is present");

    assert!(!beside_auth_url.preferred);
    assert!(alone.preferred);
}

/// A document vouching for another issuer is never passed on to clients.
#[tokio::test]
async fn an_issuer_the_document_does_not_vouch_for_is_not_advertised() {
    let base = spawn_provider(Some("https://impostor.example.com")).await;
    let auth = auth_settings(&base, Some(r#"client_id = "lore-cli""#));

    let error = advertised_oidc(&auth, Some(AUTH_URL))
        .await
        .expect_err("a mismatched issuer must fail");

    assert!(
        matches!(error, JWKServiceError::DiscoveryIssuerMismatch { .. }),
        "{error:?}"
    );
}
