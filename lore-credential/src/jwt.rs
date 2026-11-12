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

    let Ok(token) = crate::token_store::load_user_token(
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

    user_info_from_token(token)
}

pub fn identity_from_token(token: &str) -> String {
    user_info_from_token(token.to_string())
        .map(|info| info.id)
        .unwrap_or_default()
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
        // JWT has number of seconds since UNIX epoch in UTC - we want milliseconds like
        // all other timestamps in Lore, also in UTC
        expires: token_data.claims.expires * 1000,
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
