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
    #[serde(rename = "sub")]
    pub user_id: String,
    pub name: String,
    pub preferred_username: Option<String>,
    pub is_service_account: Option<bool>,
    #[serde(rename = "exp")]
    pub expires: u64,
    #[serde_as(as = "OneOrMany<_, PreferMany>")]
    #[serde(rename = "aud")]
    pub audience: Vec<String>,
}

impl JWTUserInfo {
    /// All the root domains this token are intended for
    pub fn acceptable_root_domains(&self) -> Vec<String> {
        [
            // Whoever issued it already knows about the token and thus
            // the token can be given back to that endpoint.
            std::slice::from_ref(&self.issuer),
            // The Auth Service defines audienes as a list of root domains
            self.audience.as_slice(),
        ]
        .concat()
    }
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

pub fn insecure_decode_token(
    token: &str,
) -> Result<TokenData<JWTUserInfo>, jsonwebtoken::errors::Error> {
    let header = jsonwebtoken::decode_header(token)?;
    let key = jsonwebtoken::DecodingKey::from_secret(&[]);
    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.insecure_disable_signature_validation();
    validation.validate_aud = false;
    validation.validate_exp = false;
    validation.validate_nbf = false;
    jsonwebtoken::decode::<JWTUserInfo>(token, &key, &validation)
}

pub fn user_info_from_token(token: String) -> Option<UserInfo> {
    let Ok(token_data) = insecure_decode_token(&token) else {
        return None;
    };
    Some(UserInfo {
        id: token_data.claims.user_id.clone(),
        name: token_data.claims.name.clone(),
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
    if domain_in_root_domains(remote_domain, &root_domains) {
        return Ok(());
    }

    lore_debug!(
        "JWT acceptable domains '{root_domains:?}' does not contain '{remote_domain}' - forbidding JWT leak"
    );
    Err(JwtUsageError::internal(format!(
        "JWT 'aud' does not specify remote domain '{remote_domain}'"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
