// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_credential::identity_from_token;
use lore_credential::user_info_from_token;

/// `{"iss":"lore","sub":"alice","name":"Alice","exp":2000000000,"aud":["example.com"]}`
const ALICE_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

#[test]
fn identity_from_jwt_is_its_subject() {
    assert_eq!(identity_from_token(ALICE_TOKEN), "alice");
}

#[test]
fn identity_from_undecodable_token_is_empty() {
    assert!(identity_from_token("").is_empty());
    assert!(identity_from_token("not-a-jwt").is_empty());
    // Well-formed base64 segments that are not JWT claims.
    assert!(identity_from_token("aaaa.bbbb.cccc").is_empty());
}

fn token_with_claims(claims: &str) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(claims);
    format!("{header}.{claims}.signature")
}

/// A Keycloak-shaped token carries no `name` claim. It must still decode,
/// with the subject as the id, rather than failing the whole decode.
#[test]
fn a_token_without_a_name_decodes_with_its_subject_as_the_id() {
    let token =
        token_with_claims(r#"{"iss":"lore","sub":"alice","exp":2000000000,"aud":["example.com"]}"#);

    let info = user_info_from_token(token).expect("a nameless token decodes");
    assert_eq!(info.id, "alice");
    assert!(info.name.is_empty());
}

/// Regression: a required `name` made `user_info_from_token` return `None`
/// for a nameless token, so callers reading `expires` silently skipped the
/// expiry check. An expired nameless token must report its expiry.
#[test]
fn an_expired_nameless_token_still_reports_its_expiry() {
    let token =
        token_with_claims(r#"{"iss":"lore","sub":"alice","exp":1000000000,"aud":["example.com"]}"#);

    let info = user_info_from_token(token).expect("an expired nameless token still decodes");
    assert_eq!(info.expires, 1_000_000_000_000, "expiry in milliseconds");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(info.expires < now_ms, "the token reads as expired");
}

fn domains_of(claims: &str) -> Vec<String> {
    let token = token_with_claims(claims);
    lore_credential::insecure_decode_token(&token)
        .expect("the token decodes")
        .claims
        .acceptable_root_domains()
}

#[test]
fn root_domains_claim_decodes_as_an_array_or_a_scalar() {
    let array = r#"{"iss":"i","sub":"s","exp":1,"aud":"lore-api","root_domains":["a.example.com","b.example.com"]}"#;
    assert_eq!(domains_of(array), vec!["a.example.com", "b.example.com"]);

    let scalar = r#"{"iss":"i","sub":"s","exp":1,"aud":"lore-api","root_domains":"a.example.com"}"#;
    assert_eq!(domains_of(scalar), vec!["a.example.com"]);
}

#[test]
fn namespaced_root_domains_claim_decodes_as_an_array_or_a_scalar() {
    let array = r#"{"iss":"i","sub":"s","exp":1,"aud":"lore-api","https://lore.org/claims/root_domains":["a.example.com","b.example.com"]}"#;
    assert_eq!(domains_of(array), vec!["a.example.com", "b.example.com"]);

    let scalar = r#"{"iss":"i","sub":"s","exp":1,"aud":"lore-api","https://lore.org/claims/root_domains":"a.example.com"}"#;
    assert_eq!(domains_of(scalar), vec!["a.example.com"]);
}

#[test]
fn root_domains_claim_wins_over_audience_on_the_wire() {
    let claims = r#"{"iss":"i","sub":"s","exp":1,"aud":["other.example.org"],"root_domains":["a.example.com"]}"#;
    assert_eq!(domains_of(claims), vec!["a.example.com"]);
}

#[test]
fn a_token_without_the_claims_falls_back_to_audience() {
    let claims = r#"{"iss":"i","sub":"s","exp":1,"aud":["URC",".epicgames.net"]}"#;
    assert_eq!(domains_of(claims), vec![".epicgames.net"]);
}

fn identity_of(claims: &str) -> Option<String> {
    user_info_from_token(token_with_claims(claims)).map(|info| info.id)
}

/// The identity is the first of `identity`, its namespaced alias and `sub` the token carries.
#[test]
fn the_identity_claim_wins_over_the_alias_and_the_subject() {
    let base = r#""iss":"i","sub":"pairwise-sub","exp":2000000000,"aud":["example.com"]"#;
    assert_eq!(
        identity_of(&format!(r#"{{{base}}}"#)).as_deref(),
        Some("pairwise-sub")
    );
    assert_eq!(
        identity_of(&format!(r#"{{{base},"identity":"stable-oid"}}"#)).as_deref(),
        Some("stable-oid")
    );
    assert_eq!(
        identity_of(&format!(
            r#"{{{base},"https://lore.org/claims/identity":"alias-oid"}}"#
        ))
        .as_deref(),
        Some("alias-oid")
    );
    assert_eq!(
        identity_of(&format!(
            r#"{{{base},"identity":"plain","https://lore.org/claims/identity":"alias"}}"#
        ))
        .as_deref(),
        Some("plain")
    );
    assert_eq!(
        identity_from_token(&token_with_claims(&format!(
            r#"{{{base},"identity":"stable-oid"}}"#
        ))),
        "stable-oid"
    );
}

/// An empty or non-string value at the deciding claim names no one; the rule does not fall
/// through to a valid alias or `sub`.
#[test]
fn an_empty_or_non_string_identity_names_no_one() {
    let base = r#""iss":"i","sub":"pairwise-sub","exp":2000000000,"aud":["example.com"]"#;
    for claims in [
        format!(r#"{{{base},"identity":""}}"#),
        format!(r#"{{{base},"identity":"","https://lore.org/claims/identity":"alias"}}"#),
        format!(r#"{{{base},"identity":7}}"#),
        format!(r#"{{{base},"identity":null}}"#),
        format!(r#"{{{base},"identity":null,"https://lore.org/claims/identity":"alias"}}"#),
        format!(r#"{{{base},"https://lore.org/claims/identity":""}}"#),
        format!(r#"{{{base},"https://lore.org/claims/identity":null}}"#),
        r#"{"iss":"i","sub":"","exp":2000000000,"aud":["example.com"]}"#.to_string(),
    ] {
        assert_eq!(identity_of(&claims), None, "{claims}");
        assert!(identity_from_token(&token_with_claims(&claims)).is_empty());
    }
}

#[test]
fn an_unsigned_identity_token_names_its_identity() {
    let token = lore_credential::unsigned_identity_token("alice").expect("encodes");

    let info = user_info_from_token(token).expect("decodes");
    assert_eq!(info.id, "alice");
    assert_eq!(info.preferred_username, "alice");
}
