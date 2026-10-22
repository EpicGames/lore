// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_transport::auth::AuthMode;
use lore_transport::auth::AuthPath;
use lore_transport::auth::authentication;
use lore_transport::auth::authentication::Selected;
use lore_transport::auth::oidc;
use lore_transport::types::Endpoint;
use lore_transport::types::EnvironmentConfig;
use lore_transport::types::Oidc;

const AUTH_URL: &str = "https://auth.select.test.invalid";

fn issuer(test: &str) -> String {
    format!("https://issuer.select.test.invalid/{test}")
}

fn oidc(issuer: &str, preferred: bool) -> Oidc {
    Oidc {
        issuer: issuer.to_string(),
        client_id: "lore".to_string(),
        preferred,
        ..Default::default()
    }
}

fn environment(auth_url: Option<&str>, oidc: Option<Oidc>) -> EnvironmentConfig {
    EnvironmentConfig {
        endpoint: Some(Endpoint {
            auth_url: auth_url.map(str::to_string),
            ..Default::default()
        }),
        config: None,
        oidc,
    }
}

fn select(mode: AuthMode, environment: &EnvironmentConfig) -> Selected {
    authentication::select(mode, environment)
        .expect("the requested path is advertised")
        .expect("the server advertises authentication")
}

/// A server that advertises both, with no preference and a client that
/// expresses none, keeps the client on the gRPC path. Nobody moves until
/// someone asks.
#[test]
fn both_advertised_and_no_preference_anywhere_selects_grpc() {
    let issuer = issuer("no-preference");
    let environment = environment(Some(AUTH_URL), Some(oidc(&issuer, false)));

    let selected = select(AuthMode::Auto, &environment);

    assert_eq!(selected.path, AuthPath::Grpc);
    assert_eq!(selected.endpoint.url, AUTH_URL);
}

#[test]
fn a_client_asking_for_oidc_takes_it_whatever_the_server_prefers() {
    let issuer = issuer("client-asks");
    let environment = environment(Some(AUTH_URL), Some(oidc(&issuer, false)));

    let selected = select(AuthMode::Oidc, &environment);

    assert_eq!(selected.path, AuthPath::Oidc);
    assert_eq!(selected.endpoint.url, oidc::endpoint(&issuer));
}

#[test]
fn a_server_preferring_oidc_moves_a_client_that_expresses_no_preference() {
    let issuer = issuer("server-prefers");
    let environment = environment(Some(AUTH_URL), Some(oidc(&issuer, true)));

    let selected = select(AuthMode::Auto, &environment);

    assert_eq!(selected.path, AuthPath::Oidc);
    assert_eq!(selected.endpoint.url, oidc::endpoint(&issuer));
}

#[test]
fn a_client_asking_for_grpc_stays_whatever_the_server_prefers() {
    let issuer = issuer("client-stays");
    let environment = environment(Some(AUTH_URL), Some(oidc(&issuer, true)));

    let selected = select(AuthMode::Grpc, &environment);

    assert_eq!(selected.path, AuthPath::Grpc);
    assert_eq!(selected.endpoint.url, AUTH_URL);
}

/// The cheaper rollback: the operator clears `preferred`, and a client that
/// expresses no preference is back on the gRPC path with no change on its side.
#[test]
fn clearing_the_preference_sends_an_auto_client_back_to_grpc() {
    let issuer = issuer("rollback-preference");
    let preferred = environment(Some(AUTH_URL), Some(oidc(&issuer, true)));
    assert_eq!(select(AuthMode::Auto, &preferred).path, AuthPath::Oidc);

    let cleared = environment(Some(AUTH_URL), Some(oidc(&issuer, false)));

    assert_eq!(select(AuthMode::Auto, &cleared).path, AuthPath::Grpc);
}

/// The other rollback: the operator removes `[server.auth.oidc]`, and the
/// server advertises no issuer. An auto client and a grpc client are both on
/// the gRPC path with no change on their side.
#[test]
fn removing_the_issuer_leaves_every_client_that_did_not_ask_for_oidc_on_grpc() {
    let environment = environment(Some(AUTH_URL), None);

    for mode in [AuthMode::Auto, AuthMode::Grpc] {
        let selected = select(mode, &environment);
        assert_eq!(selected.path, AuthPath::Grpc, "{mode}");
        assert_eq!(selected.endpoint.url, AUTH_URL, "{mode}");
    }
}

/// Asking for a path the server does not advertise is a configuration
/// error, not a fall back to the other path.
#[test]
fn asking_for_oidc_from_a_server_with_no_issuer_fails() {
    let environment = environment(Some(AUTH_URL), None);

    let refused = authentication::select(AuthMode::Oidc, &environment)
        .expect_err("no issuer to take the OIDC path through");

    assert!(refused.is_not_supported(), "{refused}");
}

#[test]
fn asking_for_grpc_from_a_server_with_no_auth_url_fails() {
    let issuer = issuer("no-auth-url");
    let environment = environment(None, Some(oidc(&issuer, false)));

    let refused = authentication::select(AuthMode::Grpc, &environment)
        .expect_err("no auth URL to take the gRPC path through");

    assert!(refused.is_not_supported(), "{refused}");
}

/// A server that advertises only an issuer has one path, and a client with no
/// preference takes it whatever the server says it prefers.
#[test]
fn an_auto_client_takes_the_only_path_a_server_advertises() {
    let issuer = issuer("only-oidc");
    let environment = environment(None, Some(oidc(&issuer, false)));

    let selected = select(AuthMode::Auto, &environment);

    assert_eq!(selected.path, AuthPath::Oidc);
    assert_eq!(selected.endpoint.url, oidc::endpoint(&issuer));
}

/// An empty `auth_url` is no auth URL, as it is everywhere else the
/// environment is read.
#[test]
fn an_empty_auth_url_is_not_advertised() {
    let issuer = issuer("empty-auth-url");
    let environment = environment(Some(""), Some(oidc(&issuer, false)));

    assert_eq!(select(AuthMode::Auto, &environment).path, AuthPath::Oidc);
    assert!(
        authentication::select(AuthMode::Grpc, &environment)
            .expect_err("an empty auth URL advertises no gRPC path")
            .is_not_supported()
    );
}

#[test]
fn a_server_advertising_no_authentication_selects_nothing() {
    for environment in [
        environment(None, None),
        environment(Some(""), None),
        EnvironmentConfig {
            endpoint: None,
            config: None,
            oidc: None,
        },
    ] {
        for mode in [AuthMode::Auto, AuthMode::Grpc, AuthMode::Oidc] {
            let selected = authentication::select(mode, &environment)
                .expect("no path asked for can be missing when none is advertised");
            assert!(selected.is_none(), "{mode}");
        }
    }
}

/// The issuer is an `https` URL, which the scheme registry serves with the
/// gRPC implementation, so the OIDC endpoint is the issuer behind the
/// client's own scheme tag: the bare issuer still finds the gRPC
/// implementation, and the token store keys the two apart. The OIDC
/// implementation itself comes from the selection, bound to the advertised
/// client, which the endpoint alone does not name.
#[test]
fn the_oidc_endpoint_names_the_issuer_under_the_oidc_scheme() {
    let issuer = issuer("tagged");
    let environment = environment(Some(AUTH_URL), Some(oidc(&issuer, false)));

    let selected = select(AuthMode::Oidc, &environment);

    assert_eq!(selected.endpoint.url, format!("oidc+{issuer}"));
    assert_eq!(oidc::issuer(&selected.endpoint.url), issuer);
    let by_issuer = authentication::find(&issuer).expect("the https scheme");
    assert!(!Arc::ptr_eq(&selected.endpoint.auth, &by_issuer));
    assert!(authentication::find(&selected.endpoint.url).is_err());
}

/// A server whose gRPC auth URL and OIDC issuer are the same string, which a
/// UCS Auth deployment that becomes its own issuer can be. The two paths
/// must stay two paths: distinct endpoints, so distinct token store keys,
/// and each endpoint finding its own implementation whatever was selected
/// before it in the process.
#[test]
fn an_issuer_equal_to_the_grpc_auth_url_is_still_a_distinct_path() {
    let shared = issuer("shared");
    let environment = environment(Some(&shared), Some(oidc(&shared, false)));

    let over_oidc = select(AuthMode::Oidc, &environment);
    let over_grpc = select(AuthMode::Grpc, &environment);

    assert_eq!(over_grpc.endpoint.url, shared);
    assert_ne!(over_oidc.endpoint.url, over_grpc.endpoint.url);
    assert!(!Arc::ptr_eq(
        &over_oidc.endpoint.auth,
        &over_grpc.endpoint.auth
    ));
    assert!(Arc::ptr_eq(
        &over_grpc.endpoint.auth,
        &authentication::find(&shared).expect("the https scheme")
    ));
}

/// A gRPC selection takes the auth URL as the server set it, so it keeps
/// resolving by the server's scheme.
#[test]
fn a_grpc_selection_resolves_by_the_servers_scheme() {
    let environment = environment(Some(AUTH_URL), None);

    let selected = select(AuthMode::Grpc, &environment);
    let found = authentication::find(AUTH_URL).expect("the https scheme");

    assert_eq!(selected.endpoint.url, AUTH_URL);
    assert!(Arc::ptr_eq(&selected.endpoint.auth, &found));
}
