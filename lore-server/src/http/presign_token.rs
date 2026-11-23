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
    /// The token is authentic but past its expiry; carries its payload.
    #[error("token has expired")]
    Expired(Box<PresignTokenPayload>),
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

/// The hex-encoded SHA-256 digest of `token`, which names a token in logs without revealing it.
pub fn token_hash(token: &str) -> String {
    hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        token.as_bytes(),
    ))
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
        return Err(PresignTokenError::Expired(Box::new(payload)));
    }

    Ok(payload)
}
