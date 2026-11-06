// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_proto::lore::environment::v1::Oidc;
use lore_proto::lore::environment::v1::environment_service_server::EnvironmentServiceServer;
use lore_server::grpc::environment::v1::service::*;

#[allow(dead_code)]
fn assert_implements_trait(
    service: LoreEnvironmentV1Service,
) -> EnvironmentServiceServer<LoreEnvironmentV1Service> {
    EnvironmentServiceServer::new(service)
}

#[test]
fn the_provider_is_advertised_only_when_resolved() {
    let mut environment = lore_revision::environment::EnvironmentConfig::default();
    assert_eq!(environment_to_proto(&environment).oidc, None);

    environment.oidc = Some(lore_transport::Oidc {
        issuer: "https://auth.example.com".to_string(),
        client_id: "lore-cli".to_string(),
        scopes: vec!["openid".to_string()],
        client_credentials_scopes: vec!["lore".to_string()],
        ..Default::default()
    });
    assert_eq!(
        environment_to_proto(&environment).oidc,
        Some(Oidc {
            issuer: "https://auth.example.com".to_string(),
            client_id: "lore-cli".to_string(),
            scopes: vec!["openid".to_string()],
            client_credentials_scopes: vec!["lore".to_string()],
            ..Default::default()
        })
    );
}
