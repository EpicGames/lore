// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use jsonwebtoken::TokenData;
use lore_base::lore_debug;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde_with::OneOrMany;
use serde_with::formats::PreferMany;
use serde_with::serde_as;

use crate::token_store::IdentityToken;

/// The claim an issuer puts the user identity in when `sub` is not it.
pub const IDENTITY_CLAIM: &str = "identity";
/// The namespaced alias of [`IDENTITY_CLAIM`], for providers that require namespaced custom
/// claims.
pub const IDENTITY_CLAIM_NAMESPACED: &str = "https://lore.org/claims/identity";

#[derive(Debug, Default, Clone)]
pub struct UserInfo {
    pub id: String,
    pub name: String,
    pub token: String,
    pub preferred_username: String,
    pub is_service_account: bool,
    /// JWT `iat` claim, in milliseconds since the UNIX epoch.
    pub issued_at: Option<u64>,
    /// Local wall-clock time when the token was received, in milliseconds since the UNIX epoch.
    pub received_at: Option<u64>,
    /// Remaining lifetime reported by the issuer at receipt (`expires_in`), in milliseconds.
    pub lifetime_ms: Option<u64>,
    pub expires: u64,
}

// An AuthN or an AuthZ token
#[serde_as]
#[derive(Debug, Deserialize, Clone)]
pub struct JWTUserInfo {
    #[serde(rename = "iss")]
    pub issuer: String,
    /// The subject. The identity this token names may be another claim, see
    /// [`JWTUserInfo::identity`].
    #[serde(rename = "sub")]
    pub user_id: String,
    /// `None` when the claim is absent; `Some` holds whatever was carried, a JSON `null`
    /// included, so presence decides (see [`JWTUserInfo::identity`]).
    #[serde(default, rename = "identity", deserialize_with = "present")]
    pub identity: Option<serde_json::Value>,
    #[serde(
        default,
        rename = "https://lore.org/claims/identity",
        deserialize_with = "present"
    )]
    pub identity_namespaced: Option<serde_json::Value>,
    pub name: Option<String>,
    pub preferred_username: Option<String>,
    pub is_service_account: Option<bool>,
    #[serde(default, rename = "iat", deserialize_with = "lenient_numeric_date")]
    pub issued_at: Option<u64>,
    #[serde(rename = "exp")]
    pub expires: u64,
    #[serde_as(as = "OneOrMany<_, PreferMany>")]
    #[serde(rename = "aud")]
    pub audience: Vec<String>,
    #[serde_as(as = "Option<OneOrMany<_, PreferMany>>")]
    #[serde(default)]
    pub root_domains: Option<Vec<String>>,
    #[serde_as(as = "Option<OneOrMany<_, PreferMany>>")]
    #[serde(default, rename = "https://lore.org/claims/root_domains")]
    pub root_domains_namespaced: Option<Vec<String>>,
}

/// Keeps a present claim present: a JSON `null` decodes as `Some(Value::Null)`, where
/// `Option<Value>` alone would read it as absent.
fn present<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// Decodes an optional RFC 7519 `NumericDate` (`iat`), rounding fractional seconds up so token
/// lifetimes are never overestimated, and falling back to `None` on unexpected types so a
/// malformed `iat` never rejects an otherwise usable token.
fn lenient_numeric_date<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let seconds = match value {
        serde_json::Value::Number(num) => num.as_u64().or_else(|| {
            let f = num.as_f64()?.ceil();
            (f.is_finite() && f >= 0.0 && f < (u64::MAX as f64)).then_some(f as u64)
        }),
        _ => None,
    };
    Ok(seconds)
}

impl JWTUserInfo {
    /// The identity this token names: the first of `identity`, its namespaced alias and `sub`
    /// the token carries, and only that one. `None` when that claim is empty, `null` or not a
    /// string, so the token names no one; the rule does not fall through to the next claim.
    pub fn identity(&self) -> Option<&str> {
        self.identity_source().1
    }

    /// The name of the claim that decides the identity, and its value when it is a non-empty
    /// string.
    pub fn identity_source(&self) -> (&'static str, Option<&str>) {
        let (claim, value) = [
            (IDENTITY_CLAIM, &self.identity),
            (IDENTITY_CLAIM_NAMESPACED, &self.identity_namespaced),
        ]
        .into_iter()
        .find_map(|(claim, value)| value.as_ref().map(|value| (claim, value.as_str())))
        .unwrap_or(("sub", Some(self.user_id.as_str())));
        (claim, value.filter(|value| !value.is_empty()))
    }

    /// The root domains this token may be sent to, read from claims the issuer signed.
    ///
    /// The `root_domains` claim, or its namespaced alias, wins. Otherwise `aud` entries with a
    /// usable shape contribute: an `https` URI its host, a domain-shaped entry a suffix. Names
    /// are fixed so an environment response cannot point the client at a different claim.
    pub fn acceptable_root_domains(&self) -> Vec<String> {
        let from_claim: Vec<String> = [&self.root_domains, &self.root_domains_namespaced]
            .into_iter()
            .flatten()
            .flatten()
            .filter(|domain| !domain.is_empty())
            .cloned()
            .collect();
        if !from_claim.is_empty() {
            return from_claim;
        }

        self.audience
            .iter()
            .filter_map(|aud| domain_from_audience(aud))
            .collect()
    }
}

fn domain_from_audience(aud: &str) -> Option<String> {
    if aud.contains("://") {
        let url = url::Url::parse(aud).ok()?;
        if url.scheme() != "https" {
            return None;
        }
        return url.host_str().map(str::to_string);
    }

    let apex = aud.strip_prefix('.').unwrap_or(aud);
    let domain_shaped = apex.contains('.')
        && apex.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
    domain_shaped.then(|| aud.to_string())
}

pub async fn user_info<P>(
    auth_url: &str,
    identity: &str,
    token_filter: P,
    identity_token: &str,
    access_token: &str,
) -> Option<UserInfo>
where
    P: FnMut(&&IdentityToken) -> bool,
{
    lore_debug!("Get user {identity} info from {auth_url}");

    let Ok(stored) = crate::token_store::load_stored_token(
        auth_url,
        identity,
        token_filter,
        identity_token,
        access_token,
    )
    .await
    else {
        return None;
    };

    let mut info = user_info_from_token(stored.token)?;
    info.received_at = stored.receipt.received_at;
    info.lifetime_ms = stored.receipt.lifetime_ms;
    Some(info)
}

pub fn identity_from_token(token: &str) -> String {
    user_info_from_token(token.to_string()).map_or_default(|info| info.id)
}

/// The claims Lore reads from `token`, decoded without verifying its signature.
/// A token without `exp` fails to decode.
pub fn insecure_decode_token(
    token: &str,
) -> Result<TokenData<JWTUserInfo>, jsonwebtoken::errors::Error> {
    insecure_decode(token, true)
}

/// Every claim of `token`, decoded without verifying its signature. Unlike
/// [`insecure_decode_token`], a token without `exp` decodes.
pub fn insecure_decode_claims(
    token: &str,
) -> Result<serde_json::Value, jsonwebtoken::errors::Error> {
    insecure_decode(token, false).map(|data| data.claims)
}

fn insecure_decode<T: serde::de::DeserializeOwned>(
    token: &str,
    require_exp: bool,
) -> Result<TokenData<T>, jsonwebtoken::errors::Error> {
    let header = jsonwebtoken::decode_header(token)?;
    let key = jsonwebtoken::DecodingKey::from_secret(&[]);
    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.insecure_disable_signature_validation();
    validation.validate_aud = false;
    validation.validate_exp = false;
    validation.validate_nbf = false;
    if !require_exp {
        validation.required_spec_claims.clear();
    }
    jsonwebtoken::decode::<T>(token, &key, &validation)
}

/// The `iss` and `aud` of an [`unsigned_identity_token`].
pub const UNSIGNED_IDENTITY_ISSUER: &str = "lore-unsigned-identity";

/// `exp` of an [`unsigned_identity_token`], 3000-01-01: the token proves nothing, so there
/// is nothing for an expiry to limit.
const UNSIGNED_IDENTITY_EXPIRES: u64 = 32_503_680_000;

/// A token that names `identity` to a server with `server.trust_unsigned_identity`. Its
/// HS256 signature uses an empty key and proves nothing: `jsonwebtoken` refuses
/// `alg: none` when it reads the header, so the token carries a real algorithm to stay
/// readable by [`insecure_decode_token`] and the server.
pub fn unsigned_identity_token(identity: &str) -> Result<String, jsonwebtoken::errors::Error> {
    let issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let claims = serde_json::json!({
        "iss": UNSIGNED_IDENTITY_ISSUER,
        "aud": [UNSIGNED_IDENTITY_ISSUER],
        "sub": identity,
        "name": identity,
        "preferred_username": identity,
        "iat": issued_at,
        "exp": UNSIGNED_IDENTITY_EXPIRES,
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(&[]),
    )
}

/// The user a token names. `None` when the token is not a JWT with the standard claims, or
/// names no identity (see [`JWTUserInfo::identity`]).
pub fn user_info_from_token(token: String) -> Option<UserInfo> {
    let Ok(token_data) = insecure_decode_token(&token) else {
        return None;
    };
    let id = token_data.claims.identity()?.to_string();
    Some(UserInfo {
        id,
        name: token_data.claims.name.clone().unwrap_or_default(),
        token,
        preferred_username: token_data.claims.preferred_username.unwrap_or_default(),
        is_service_account: token_data.claims.is_service_account.unwrap_or_default(),
        issued_at: token_data
            .claims
            .issued_at
            .map(|iat| iat.saturating_mul(1000)),
        received_at: None,
        lifetime_ms: None,
        // JWT has number of seconds since UNIX epoch in UTC - we want milliseconds like
        // all other timestamps in Lore, also in UTC
        expires: token_data.claims.expires.saturating_mul(1000),
    })
}

#[error_set]
pub enum JwtUsageError {}

pub fn domain_in_root_domains(domain: &str, root_domains: &[String]) -> bool {
    root_domains.iter().any(|acceptable_root| {
        // Require a label boundary, not a raw suffix, so `epicgames.net`
        // rejects a look-alike such as `evilepicgames.net`. A leading `.`
        // is optional and does not change the match.
        let apex = acceptable_root.strip_prefix('.').unwrap_or(acceptable_root);
        domain == apex || domain.ends_with(&format!(".{apex}"))
    })
}

/// Early-refresh margin in milliseconds so a token is not presented seconds before it expires.
pub const EARLY_REFRESH_MARGIN_MS: u64 = 10_000;

/// Current local wall-clock time in milliseconds since the UNIX epoch, or 0 if the clock is
/// before the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Checks whether a deadline `expires` (ms since UNIX epoch) has passed at `now_ms`.
fn is_expired_at(expires: u64, now_ms: u64) -> bool {
    now_ms >= expires
}

/// Checks whether a deadline `expires` (ms since UNIX epoch) has passed on the local wall clock.
pub fn is_expired(expires: u64) -> bool {
    is_expired_at(expires, now_ms())
}

/// Checks whether a token has expired or is within its early-refresh margin at `now_ms`.
///
/// When `received_at` is known and the local clock has not stepped backwards past it, expiry is
/// measured as elapsed local time since `received_at` against the token's remaining lifetime:
///
/// 1. `lifetime_ms` (from the issuer's `expires_in` response) is preferred because it reflects the
///    remaining lifetime at receipt, unaffected by clock skew, backdated `iat` claims, or omitted
///    `iat`.
/// 2. Otherwise, `exp - iat` is used when `0 < iat < exp`. When `received_at` is within 10 minutes
///    after `iat`, the effective lifetime is capped by `exp.saturating_sub(received_at)` so a
///    backdated `iat` or an already-expired token on a synced clock does not extend past `exp`.
///
/// The early-refresh margin is `EARLY_REFRESH_MARGIN_MS` capped at half the lifetime so
/// short-lived tokens remain usable right after issue. When no receipt-relative lifetime is
/// available (or the clock stepped backwards past `received_at`), falls back to comparing `exp`
/// against `now_ms`. A backward clock step smaller than the time since `received_at` is not
/// detected.
pub fn is_token_expired_at(
    issued_at: Option<u64>,
    expires: u64,
    received_at: Option<u64>,
    lifetime_ms: Option<u64>,
    now_ms: u64,
) -> bool {
    // Issuers like Entra ID backdate `iat` by ~5 min; treat a gap beyond 10 min as clock skew
    // rather than backdating.
    const BACKDATED_IAT_TOLERANCE_MS: u64 = 10 * 60 * 1000;

    if let Some(received_at) = received_at
        && let Some(elapsed) = now_ms.checked_sub(received_at)
    {
        let effective_lifetime = lifetime_ms.or_else(|| {
            let iat = issued_at.filter(|&iat| iat > 0 && iat < expires)?;
            let claim_lifetime = expires - iat;
            if received_at >= iat && received_at - iat <= BACKDATED_IAT_TOLERANCE_MS {
                Some(claim_lifetime.min(expires.saturating_sub(received_at)))
            } else {
                Some(claim_lifetime)
            }
        });
        if let Some(lifetime) = effective_lifetime {
            let margin = EARLY_REFRESH_MARGIN_MS.min(lifetime / 2);
            return elapsed.saturating_add(margin) >= lifetime;
        }
    }
    is_expired_at(expires, now_ms)
}

/// Checks whether a token has expired at the current local wall clock.
pub fn is_token_expired(
    issued_at: Option<u64>,
    expires: u64,
    received_at: Option<u64>,
    lifetime_ms: Option<u64>,
) -> bool {
    is_token_expired_at(issued_at, expires, received_at, lifetime_ms, now_ms())
}

pub fn verify_jwt_usage_for_remote(
    jwt: &JWTUserInfo,
    remote_domain: &str,
) -> Result<(), JwtUsageError> {
    let root_domains = jwt.acceptable_root_domains();
    if root_domains.is_empty() {
        return Err(JwtUsageError::internal(
            "JWT names no domain it may be sent to: have the issuer add a 'root_domains' claim, \
             or set the token audience to the deployment's https URL or domain",
        ));
    }
    if domain_in_root_domains(remote_domain, &root_domains) {
        return Ok(());
    }

    lore_debug!(
        "JWT acceptable domains '{root_domains:?}' does not contain '{remote_domain}' - forbidding JWT leak"
    );
    Err(JwtUsageError::internal(format!(
        "JWT does not specify remote domain '{remote_domain}'"
    )))
}
