// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::authnz::rebac::rebac_endpoint;

/// Dialling `ucs-auth://` unrewritten gets no TLS.
#[test]
fn ucs_auth_scheme_dials_https() {
    let endpoint = rebac_endpoint("ucs-auth://auth.example.com:8444").expect("endpoint");
    assert_eq!(endpoint.uri().scheme_str(), Some("https"));
    assert_eq!(
        endpoint.uri().authority().map(|a| a.as_str()),
        Some("auth.example.com:8444")
    );
}

#[test]
fn loopback_http_stays_plaintext() {
    let endpoint = rebac_endpoint("http://127.0.0.1:41339").expect("endpoint");
    assert_eq!(endpoint.uri().scheme_str(), Some("http"));
}
