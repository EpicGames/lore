// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PresignTokenPayload {
    pub version: u8,
    pub key_id: String,
    pub repository: String,
    pub address: String,
    /// Unix timestamp (seconds) after which the token is invalid.
    pub expires_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_disposition: Option<String>,
    /// Logical byte length after Lore defragmentation/decompression. Optional
    /// so URLs issued before this field existed remain redeemable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
}

#[derive(Debug, Error, PartialEq)]
pub enum PresignTokenError {
    #[error("invalid token format")]
    InvalidFormat,
    #[error("invalid token signature")]
    InvalidSignature,
    #[error("unknown token version: {0}")]
    UnknownVersion(u8),
    #[error("token was signed by a different key")]
    KeyIdMismatch,
    #[error("token has expired")]
    Expired,
}

pub const CURRENT_TOKEN_VERSION: u8 = 1;

/// Signs `payload` and returns `<base64url(json)>.<base64url(signature)>`.
pub fn sign(payload: &PresignTokenPayload, key: &hmac::Key) -> String {
    let json = serde_json::to_string(payload).expect("PresignTokenPayload is always serializable");
    let encoded_payload = URL_SAFE_NO_PAD.encode(json.as_bytes());
    let signature = hmac::sign(key, encoded_payload.as_bytes());
    let encoded_sig = URL_SAFE_NO_PAD.encode(signature.as_ref());
    format!("{encoded_payload}.{encoded_sig}")
}

/// Verifies a token and returns the payload if valid.
///
/// Checks (in order): format, signature, version, `key_id`, expiry.
pub fn verify(
    token: &str,
    key: &hmac::Key,
    key_id: &str,
    now_unix: u64,
) -> Result<PresignTokenPayload, PresignTokenError> {
    let (encoded_payload, encoded_sig) = token
        .split_once('.')
        .ok_or(PresignTokenError::InvalidFormat)?;

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(encoded_sig)
        .map_err(|_e| PresignTokenError::InvalidFormat)?;

    hmac::verify(key, encoded_payload.as_bytes(), &sig_bytes)
        .map_err(|_e| PresignTokenError::InvalidSignature)?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(encoded_payload)
        .map_err(|_e| PresignTokenError::InvalidFormat)?;

    let payload: PresignTokenPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_e| PresignTokenError::InvalidFormat)?;

    if payload.version != CURRENT_TOKEN_VERSION {
        return Err(PresignTokenError::UnknownVersion(payload.version));
    }

    if payload.key_id != key_id {
        return Err(PresignTokenError::KeyIdMismatch);
    }

    if now_unix >= payload.expires_at {
        return Err(PresignTokenError::Expired);
    }

    Ok(payload)
}

#[cfg(test)]
mod tests {
    use ring::hmac;

    use super::*;

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
            content_length: None,
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
            Err(PresignTokenError::Expired)
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
        payload.content_length = Some(42);
        let token = sign(&payload, &key);
        let result = verify(&token, &key, "test_key_id", 0 /* now_unix */).unwrap();
        assert_eq!(result.content_type.as_deref(), Some("image/png"));
        assert_eq!(result.content_encoding.as_deref(), Some("gzip"));
        assert_eq!(result.content_disposition.as_deref(), Some("inline"));
        assert_eq!(result.content_length, Some(42));
    }
}
