// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::http::presign_token::*;
use ring::hmac;

fn test_key() -> hmac::Key {
    hmac::Key::new(hmac::HMAC_SHA256, &[0u8; 32])
}

fn test_payload(expires_at: u64) -> PresignTokenPayload {
    PresignTokenPayload {
        version: CURRENT_TOKEN_VERSION,
        key_id: "test_key_id".to_string(),
        repository: "ffffffffffffffffffffffffffffffff".to_string(),
        address: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff".to_string(),
        expires_at,
        content_type: None,
        content_encoding: None,
        content_disposition: None,
    }
}

#[test]
fn round_trip_succeeds() {
    let key = test_key();
    let payload = test_payload(9999999999 /* expires_at */);
    let token = sign(&payload, &key);
    let result = verify(&token, &key, "test_key_id", 0 /* now_unix */);
    assert_eq!(result.unwrap(), payload);
}

#[test]
fn altered_signature_returns_invalid_signature() {
    let key = test_key();
    let token = sign(&test_payload(9999999999 /* expires_at */), &key);
    let (payload_part, _) = token.split_once('.').unwrap();
    let bad_token = format!("{payload_part}.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(
        verify(&bad_token, &key, "test_key_id", 0 /* now_unix */),
        Err(PresignTokenError::InvalidSignature)
    );
}

#[test]
fn expired_token_returns_expired() {
    let key = test_key();
    let payload = test_payload(100 /* expires_at */);
    let token = sign(&payload, &key);
    // now_unix == expires_at is already expired
    assert_eq!(
        verify(&token, &key, "test_key_id", 100 /* now_unix */),
        Err(PresignTokenError::Expired(Box::new(payload)))
    );
}

#[test]
fn unknown_version_returns_unknown_version() {
    let key = test_key();
    let mut payload = test_payload(9999999999 /* expires_at */);
    payload.version = 99;
    let token = sign(&payload, &key);
    assert_eq!(
        verify(&token, &key, "test_key_id", 0 /* now_unix */),
        Err(PresignTokenError::UnknownVersion(99))
    );
}

#[test]
fn wrong_key_id_returns_key_id_mismatch() {
    let key = test_key();
    let token = sign(&test_payload(9999999999 /* expires_at */), &key);
    assert_eq!(
        verify(&token, &key, "different_key_id", 0 /* now_unix */),
        Err(PresignTokenError::KeyIdMismatch)
    );
}

#[test]
fn missing_dot_returns_invalid_format() {
    let key = test_key();
    assert_eq!(
        verify("nodotinhere", &key, "test_key_id", 0 /* now_unix */),
        Err(PresignTokenError::InvalidFormat)
    );
}

#[test]
fn content_headers_round_trip() {
    let key = test_key();
    let mut payload = test_payload(9999999999 /* expires_at */);
    payload.content_type = Some("image/png".to_string());
    payload.content_encoding = Some("gzip".to_string());
    payload.content_disposition = Some("inline".to_string());
    let token = sign(&payload, &key);
    let result = verify(&token, &key, "test_key_id", 0 /* now_unix */).unwrap();
    assert_eq!(result.content_type.as_deref(), Some("image/png"));
    assert_eq!(result.content_encoding.as_deref(), Some("gzip"));
    assert_eq!(result.content_disposition.as_deref(), Some("inline"));
}
