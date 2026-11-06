// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_proto::lore::environment::v1 as proto;
use lore_transport::types::EnvironmentConfig;
use lore_transport::types::Oidc;

fn environment_with(oidc: Option<proto::Oidc>) -> EnvironmentConfig {
    proto::Environment {
        endpoint: Some(proto::Endpoint {
            auth_url: "ucs-auth://auth.example.com".to_string(),
            ..Default::default()
        }),
        config: None,
        oidc,
    }
    .into()
}

/// What an old server sends: `auth_url` only, so the client stays on the legacy path.
#[test]
fn a_server_advertising_no_provider_maps_to_none() {
    let environment = environment_with(None);
    assert_eq!(environment.oidc, None);
    assert_eq!(
        environment
            .endpoint
            .and_then(|endpoint| endpoint.auth_url)
            .as_deref(),
        Some("ucs-auth://auth.example.com")
    );
}

#[test]
fn a_provider_without_an_issuer_maps_to_none() {
    let environment = environment_with(Some(proto::Oidc {
        client_id: "lore-cli".to_string(),
        ..Default::default()
    }));
    assert_eq!(environment.oidc, None);
}

#[test]
fn an_advertised_provider_maps_empty_strings_to_none() {
    let environment = environment_with(Some(proto::Oidc {
        issuer: "https://auth.example.com".to_string(),
        client_id: "lore-cli".to_string(),
        scopes: vec!["openid".to_string()],
        preferred: true,
        resource_template: "https://lore.example.com/partitions/{id}".to_string(),
        scope_template: String::new(),
        token_exchange_issuer: "https://auth.example.com".to_string(),
        client_credentials_scopes: vec!["lore".to_string()],
        ..Default::default()
    }));
    assert_eq!(
        environment.oidc,
        Some(Oidc {
            issuer: "https://auth.example.com".to_string(),
            client_id: "lore-cli".to_string(),
            scopes: vec!["openid".to_string()],
            preferred: true,
            resource_template: Some("https://lore.example.com/partitions/{id}".to_string()),
            scope_template: None,
            token_exchange_issuer: Some("https://auth.example.com".to_string()),
            client_credentials_scopes: vec!["lore".to_string()],
        })
    );
}

/// An older server still fills the deprecated `identity_claim` (8); the client
/// decodes the message and reads nothing from it.
#[test]
#[allow(deprecated)]
fn an_advertised_identity_claim_is_ignored() {
    let environment = environment_with(Some(proto::Oidc {
        issuer: "https://auth.example.com".to_string(),
        client_id: "lore-cli".to_string(),
        identity_claim: "oid".to_string(),
        ..Default::default()
    }));
    assert_eq!(
        environment.oidc,
        Some(Oidc {
            issuer: "https://auth.example.com".to_string(),
            client_id: "lore-cli".to_string(),
            ..Default::default()
        })
    );
}
