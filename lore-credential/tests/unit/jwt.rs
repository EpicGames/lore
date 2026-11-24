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

#[test]
fn claims_decode_with_every_claim_the_token_carries() {
    let claims = r#"{"iss":"i","sub":"alice","exp":1,"realm_access":{"roles":["write","push-protected"]},"resources":[{"resource_id":"urc-*","permission":["read"]}]}"#;

    let decoded = lore_credential::insecure_decode_claims(&token_with_claims(claims))
        .expect("the token decodes");
    assert_eq!(
        decoded,
        serde_json::from_str::<serde_json::Value>(claims).unwrap()
    );
}

#[test]
fn claims_of_a_token_without_an_expiry_decode() {
    let claims = r#"{"iss":"i","sub":"alice"}"#;

    let decoded = lore_credential::insecure_decode_claims(&token_with_claims(claims))
        .expect("the token decodes");
    assert_eq!(
        decoded,
        serde_json::from_str::<serde_json::Value>(claims).unwrap()
    );
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

#[test]
fn iat_decodes_integers_rounds_floats_up_and_ignores_invalid_values() {
    let base = r#""iss":"i","sub":"alice","exp":2000000000,"aud":["example.com"]"#;

    let integer = user_info_from_token(token_with_claims(&format!(
        r#"{{{base},"iat":1700000000}}"#
    )))
    .expect("decodes");
    assert_eq!(integer.issued_at, Some(1_700_000_000_000));

    let fractional = user_info_from_token(token_with_claims(&format!(
        r#"{{{base},"iat":1700000000.75}}"#
    )))
    .expect("decodes");
    assert_eq!(fractional.issued_at, Some(1_700_000_001_000));

    for bad_iat in [r#""not-a-number""#, "-5", "null", "true"] {
        let info =
            user_info_from_token(token_with_claims(&format!(r#"{{{base},"iat":{bad_iat}}}"#)))
                .expect("invalid iat does not reject token");
        assert_eq!(info.issued_at, None, "iat={bad_iat}");
    }
}

#[test]
fn token_expiry_uses_expires_in_and_iat_lifetime_handles_edge_cases_and_caps_margin() {
    use lore_credential::EARLY_REFRESH_MARGIN_MS;
    use lore_credential::is_token_expired_at;

    let margin = EARLY_REFRESH_MARGIN_MS;
    let now = 2_000_000_000_000u64;

    // 1. Issuer-reported lifetime_ms (expires_in) takes precedence over both exp and iat.
    // Client clock 2 hours ahead of issuer, token omits iat entirely: still valid for 1 hour.
    let received_at = now - 60_000;
    assert!(!is_token_expired_at(
        None,
        now - 3_600_000,
        Some(received_at),
        Some(3_600_000),
        now
    ));
    assert!(is_token_expired_at(
        None,
        now - 3_600_000,
        Some(received_at),
        Some(3_600_000),
        received_at + 3_600_000 - margin
    ));
    // Backdated iat (Entra ID style: iat 5 min before issue, exp 60 min after issue -> exp - iat
    // = 65 min) does not stretch validity past expires_in (60 min).
    let entra_iat = received_at - 300_000;
    let entra_exp = received_at + 3_600_000;
    assert!(is_token_expired_at(
        Some(entra_iat),
        entra_exp,
        Some(received_at),
        Some(3_600_000),
        received_at + 3_600_000 - margin
    ));

    // 2. Without lifetime_ms, falls back to exp - iat when clock skew is present.
    // Client clock 2 hours ahead of issuer: exp is in the past locally, but 1-hour lifetime was
    // just received.
    let iat = now - 7_200_000;
    let exp = iat + 3_600_000;
    assert!(!is_token_expired_at(
        Some(iat),
        exp,
        Some(received_at),
        None,
        now
    ));
    let near_end = received_at + 3_600_000 - margin;
    assert!(is_token_expired_at(
        Some(iat),
        exp,
        Some(received_at),
        None,
        near_end
    ));
    assert!(!is_token_expired_at(
        Some(iat),
        exp,
        Some(received_at),
        None,
        near_end - 1
    ));

    // Client clock 2 hours behind issuer: exp is far in the future locally, but lifetime elapsed.
    let iat_future = now + 7_200_000;
    let exp_future = iat_future + 3_600_000;
    let received_long_ago = now - 3_600_000;
    assert!(is_token_expired_at(
        Some(iat_future),
        exp_future,
        Some(received_long_ago),
        None,
        now
    ));

    // Backdated iat (5 min) on a synced clock without lifetime_ms: capped by exp - received_at
    // so the token does not outlive exp.
    assert!(is_token_expired_at(
        Some(entra_iat),
        entra_exp,
        Some(received_at),
        None,
        entra_exp - margin
    ));
    assert!(!is_token_expired_at(
        Some(entra_iat),
        entra_exp,
        Some(received_at),
        None,
        entra_exp - margin - 1
    ));

    // Already-expired token received on a synced clock (iat = R-6m, exp = R-1m): capped to 0,
    // not treated as valid for 5 more minutes.
    assert!(is_token_expired_at(
        Some(received_at - 360_000),
        received_at - 60_000,
        Some(received_at),
        None,
        received_at
    ));

    // Bogus iat == 0 without lifetime_ms falls back to exp rather than producing a 56-year lifetime.
    assert!(is_token_expired_at(
        Some(0),
        now - 1_000,
        Some(received_at),
        None,
        now
    ));

    // Bogus iat >= exp (e.g. issuer sent iat in milliseconds) falls back to exp instead of
    // reporting an unexpired token as expired.
    assert!(!is_token_expired_at(
        Some(now + 10_000),
        now + 3_600_000,
        Some(now),
        None,
        now
    ));
    assert!(is_token_expired_at(
        Some(now + 10_000),
        now,
        Some(now),
        None,
        now
    ));

    // Clock stepped backwards past received_at: fall back to exp rather than clamping elapsed to 0.
    assert!(is_token_expired_at(
        Some(now - 7_200_000),
        now - 3_600_000,
        Some(now + 10_000),
        Some(3_600_000),
        now
    ));
    assert!(!is_token_expired_at(
        Some(now - 60_000),
        now + 3_600_000,
        Some(now + 10_000),
        Some(3_600_000),
        now
    ));

    // iat > exp gives zero lifetime -> expired immediately.
    assert!(is_token_expired_at(
        Some(now + 10_000),
        now,
        Some(now),
        None,
        now
    ));

    // Missing receipt metadata falls back to comparing exp directly with now_ms (no uncapped margin).
    assert!(is_token_expired_at(None, now, None, None, now));
    assert!(!is_token_expired_at(None, now + 1, None, None, now));
    assert!(is_token_expired_at(
        Some(now - 60_000),
        now,
        None,
        None,
        now
    ));
    assert!(!is_token_expired_at(
        Some(now - 60_000),
        now + 1,
        None,
        None,
        now
    ));

    // Short-lived token caps the early-refresh margin at half its lifetime.
    let short_exp = now + 3_000;
    assert!(!is_token_expired_at(
        Some(now),
        short_exp,
        Some(now),
        Some(3_000),
        now
    ));
    assert!(!is_token_expired_at(
        Some(now),
        short_exp,
        Some(now),
        Some(3_000),
        now + 1_499
    ));
    assert!(is_token_expired_at(
        Some(now),
        short_exp,
        Some(now),
        Some(3_000),
        now + 1_500
    ));
}
