// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod client_credentials;
mod device;
mod discovery;
mod exchange;
mod refresh;
mod revocation;

#[path = "../../support/oidc_provider.rs"]
mod provider;

use lore_transport::auth::oidc::*;

/// A body longer than the log excerpt is cut, and says so.
#[test]
fn a_long_body_is_excerpted_for_the_log() {
    let short = "not a document";
    assert_eq!(body_excerpt(short), short);

    let long = "x".repeat(LOGGED_BODY_LIMIT * 3);
    let excerpt = body_excerpt(&long);
    assert!(excerpt.len() < long.len());
    assert!(excerpt.contains(&format!("({} bytes total)", long.len())));
}

/// Multi-byte characters must not be split when excerpting, or the log line panics.
#[test]
fn excerpting_respects_character_boundaries() {
    let body = "é".repeat(LOGGED_BODY_LIMIT * 2);
    let excerpt = body_excerpt(&body);
    assert!(excerpt.contains('é'));
}

/// The endpoint is the issuer behind the client's scheme tag, and the issuer
/// is read back out of it. A bare issuer reads back as itself.
#[test]
fn the_endpoint_round_trips_the_issuer() {
    let https = "https://issuer.example/realms/lore";
    assert_eq!(endpoint(https), "oidc+https://issuer.example/realms/lore");
    assert_eq!(issuer(&endpoint(https)), https);

    let loopback = "http://localhost:8080";
    assert_eq!(endpoint(loopback), "oidc+http://localhost:8080");
    assert_eq!(issuer(&endpoint(loopback)), loopback);

    assert_eq!(issuer(https), https);
}

/// The endpoint names the issuer and not the client the server advertises,
/// so the registry, which has only the endpoint to go on, serves no OIDC
/// scheme: the implementation comes from the selection.
#[test]
fn no_endpoint_scheme_is_served_by_the_registry() {
    for issuer in ["https://issuer.example", "http://127.0.0.1:8080"] {
        let Err(refused) = lore_transport::auth::authentication::find(&endpoint(issuer)) else {
            panic!("the registry serves the gRPC path only");
        };
        assert!(refused.is_internal(), "{refused}");
    }
}
