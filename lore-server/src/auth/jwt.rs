// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use jsonwebtoken::DecodingKey;
use jsonwebtoken::Validation;
use jsonwebtoken::decode;
use jsonwebtoken::decode_header;
use serde::Deserialize;
use serde::Serialize;
use serde_with::OneOrMany;
use serde_with::formats::PreferMany;
use serde_with::serde_as;
use thiserror::Error;
use tracing::debug;
use tracing::warn;

use super::jwk::JWKServiceError;
use crate::auth::jwk::JWKService;
use crate::authnz::repository_authorizer::Action;

/// From Lore protos, but cannot derive deserialize on external type
#[derive(Debug, Deserialize, Clone, Serialize, PartialEq)]
pub struct ResourcePermission {
    pub resource_id: String,
    pub permission: Vec<String>,
}

impl ResourcePermission {
    pub fn is_wildcard_resource(&self, wildcard: &str) -> bool {
        self.resource_id == wildcard
    }

    pub fn matches_resource(&self, resource_id: &str, wildcard: &str) -> bool {
        self.resource_id == resource_id || self.is_wildcard_resource(wildcard)
    }
}

/// The legacy `UrcAuthApi` resource shape: the default for the
/// `resource_id_template` setting and for the fixed matcher the legacy
/// readers use.
pub const DEFAULT_RESOURCE_ID_TEMPLATE: &str = "urc-{id}";
/// The legacy wildcard, the `resource_wildcard` setting's default.
pub const DEFAULT_RESOURCE_WILDCARD: &str = "urc-*";
/// The claim an issuer puts the user identity in when `sub` is not it.
pub const IDENTITY_CLAIM: &str = "identity";
/// The namespaced alias of [`IDENTITY_CLAIM`], for providers that require
/// namespaced custom claims.
pub const IDENTITY_CLAIM_NAMESPACED: &str = "https://lore.org/claims/identity";

/// Renders repository ids into resource names and matches grant entries
/// against them. The defaults reproduce the legacy `UrcAuthApi` shape for
/// backwards compatibility.
#[derive(Clone, Debug)]
pub struct ResourceMatcher {
    resource_id_template: String,
    resource_wildcard: String,
}

impl Default for ResourceMatcher {
    fn default() -> Self {
        Self::new(
            DEFAULT_RESOURCE_ID_TEMPLATE.to_string(),
            DEFAULT_RESOURCE_WILDCARD.to_string(),
        )
    }
}

impl ResourceMatcher {
    pub fn new(resource_id_template: String, resource_wildcard: String) -> Self {
        Self {
            resource_id_template,
            resource_wildcard,
        }
    }

    /// The resource name `repository` renders to under the template.
    pub fn resource_for(&self, repository: lore_base::types::RepositoryId) -> String {
        self.resource_id_template
            .replace("{id}", &repository.to_string())
    }

    /// Whether any entry matches `repository`, wildcard included.
    pub fn any_match(
        &self,
        resources: &[ResourcePermission],
        repository: lore_base::types::RepositoryId,
    ) -> bool {
        let resource_id = self.resource_for(repository);
        resources
            .iter()
            .any(|entry| entry.matches_resource(&resource_id, &self.resource_wildcard))
    }

    /// Whether some entry matching `repository` grants `action`. Any matching
    /// entry grants `read`. The per-question form of
    /// [`merged_permissions`](Self::merged_permissions): it visits the entries
    /// in place rather than building the merged set.
    pub fn permits(
        &self,
        resources: &[ResourcePermission],
        repository: lore_base::types::RepositoryId,
        action: Action,
    ) -> bool {
        let resource_id = self.resource_for(repository);
        resources
            .iter()
            .filter(|entry| entry.matches_resource(&resource_id, &self.resource_wildcard))
            .any(|entry| {
                action == crate::authnz::repository_authorizer::Action::Read
                    || entry
                        .permission
                        .iter()
                        .any(|granted| granted == action.as_str())
            })
    }

    /// The actions granted on `repository`, merged across every matching
    /// entry, wildcard included.
    pub fn merged_permissions(
        &self,
        resources: &[ResourcePermission],
        repository: lore_base::types::RepositoryId,
    ) -> Vec<String> {
        let resource_id = self.resource_for(repository);
        resources
            .iter()
            .filter(|entry| entry.matches_resource(&resource_id, &self.resource_wildcard))
            .flat_map(|entry| entry.permission.iter().cloned())
            .collect()
    }
}

/// The required set is `iss`, `sub`, `aud`, `exp`, `iat`.
#[serde_as]
#[derive(Debug, Deserialize, Clone, Serialize, PartialEq, Default)]
pub struct AuthorizationToken {
    #[serde(rename = "sub")]
    pub user_id: String,
    #[serde(rename = "iss")]
    pub issuer: String,
    #[serde(rename = "iat")]
    pub issued_at: u64,
    #[serde(rename = "exp")]
    pub expires: u64,
    #[serde_as(as = "OneOrMany<_, PreferMany>")]
    #[serde(rename = "aud")]
    pub audience: Vec<String>,
    pub env: Option<String>,
    pub name: Option<String>,
    pub preferred_username: Option<String>,
    pub client_id: Option<String>,
    pub resources: Option<Vec<ResourcePermission>>,
    pub groups: Option<Vec<String>>,
    pub is_service_account: Option<bool>,
    pub idp: Option<String>,
    /// Every claim the named fields do not consume, kept so configurable
    /// claim paths (`permission_claim = "realm_access.roles"`) can reach
    /// claims this struct does not name.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// The caller's identity when the token carries it in the `identity`
    /// claim or its namespaced alias; `None` when `sub` is the identity.
    /// Resolved by the verifier, never decoded.
    #[serde(skip)]
    pub identity: Option<String>,
}

impl AuthorizationToken {
    /// The identity the caller is recorded and compared as: the `identity`
    /// claim where the token carries one, `sub` otherwise.
    pub fn identity(&self) -> &str {
        self.identity.as_deref().unwrap_or(&self.user_id)
    }

    /// Resolve a dotted claim path (`realm_access.roles`) against the named
    /// fields first and then [`extra`](Self::extra). The value is returned by
    /// clone: named fields are not stored as JSON values, so a borrowed
    /// return cannot cover them.
    pub fn claim_at(&self, dotted_path: &str) -> Option<serde_json::Value> {
        let mut segments = dotted_path.split('.');
        let root = self.root_claim(segments.next()?)?;
        segments.try_fold(root, |value, segment| value.get(segment).cloned())
    }

    /// The value of a single top-level claim. Named fields shadow `extra`,
    /// which mirrors decoding: a claim a named field consumes never lands in
    /// `extra`, so the named field is the only truth for it.
    fn root_claim(&self, claim: &str) -> Option<serde_json::Value> {
        use serde_json::json;

        match claim {
            "sub" => Some(json!(self.user_id)),
            "iss" => Some(json!(self.issuer)),
            "iat" => Some(json!(self.issued_at)),
            "exp" => Some(json!(self.expires)),
            "aud" => Some(json!(self.audience)),
            "env" => self.env.as_ref().map(|v| json!(v)),
            "name" => self.name.as_ref().map(|v| json!(v)),
            "preferred_username" => self.preferred_username.as_ref().map(|v| json!(v)),
            "client_id" => self.client_id.as_ref().map(|v| json!(v)),
            "resources" => self.resources.as_ref().map(|v| json!(v)),
            "groups" => self.groups.as_ref().map(|v| json!(v)),
            "is_service_account" => self.is_service_account.map(|v| json!(v)),
            "idp" => self.idp.as_ref().map(|v| json!(v)),
            other => self.extra.get(other).cloned(),
        }
    }
}

#[derive(Debug, Error)]
pub enum JwtVerifierError {
    #[error("JWT header does not contain a kid")]
    HeaderKIDMissing,
    #[error("JWT header could not be parsed")]
    KeyNotFound(#[from] JWKServiceError),
    #[error("JWT validation failed")]
    ValidationFailed(#[from] jsonwebtoken::errors::Error),
    #[error("JWT carries an empty or non-string value at the identity claim `{claim}`")]
    IdentityClaimMissing { claim: String },
    #[error("JWT header `typ` is absent or not an accepted type")]
    TypNotAccepted,
}

#[derive(Clone)]
pub struct JwtVerifier {
    pub jwk_service: Arc<dyn JWKService>,
    /// Every `iss` value verification accepts. Two entries during an issuer's
    /// cutover, one otherwise (see [`AuthSettings::jwt_issuer`](crate::settings::AuthSettings)).
    pub jwt_issuer: Option<Vec<String>>,
    pub jwt_audience: Option<Vec<String>>,
    /// Accepted `typ` header values.
    /// `None` skips the check (see [`AuthSettings::jwt_typ`](crate::settings::AuthSettings)).
    pub jwt_typ: Option<Vec<String>>,
}

/// The comparison form of a `typ` header value. RFC 7515 §4.1.9 makes the
/// value a media type, so it is case-insensitive and may omit the
/// `application/` prefix: `at+jwt`, `application/at+jwt` and `AT+JWT` are
/// all the same type. Nothing else is forgiven: surrounding whitespace is
/// not part of a media type, so ` at+jwt ` is not `at+jwt`.
fn normalize_typ(typ: &str) -> String {
    let typ = typ.to_ascii_lowercase();
    typ.strip_prefix("application/").unwrap_or(&typ).to_string()
}

/// Whether a verification failure could be the signing key's fault rather than the token's.
///
/// A key rotated under an unchanged key id presents exactly this way, and it is the only
/// failure worth re-fetching keys for: a token that has expired, or that names another
/// audience or issuer, fails identically against every key that could ever be served. That
/// distinction is what keeps an invalid token from being a way to ask for network work.
fn key_may_be_stale(error: &JwtVerifierError) -> bool {
    matches!(error, JwtVerifierError::ValidationFailed(inner) if matches!(
        inner.kind(),
        jsonwebtoken::errors::ErrorKind::InvalidSignature
            | jsonwebtoken::errors::ErrorKind::InvalidAlgorithm
    ))
}

impl JwtVerifier {
    /// Verify a token, re-fetching the signing key once if the cached one looks stale.
    ///
    /// The retry is what makes a key rotated under an unchanged key id recoverable. Without
    /// it the cache holds a key for the id, every lookup is satisfied by it, and every token
    /// signed with the new material fails until the process restarts.
    pub async fn verify_token(&self, token: &str) -> Result<AuthorizationToken, JwtVerifierError> {
        let header = decode_header(token).map_err(JwtVerifierError::ValidationFailed)?;
        self.check_typ(&header)?;
        let kid = header.kid.ok_or(JwtVerifierError::HeaderKIDMissing)?;

        let (key, alg) = self
            .jwk_service
            .get_key(&kid)
            .await
            .map_err(JwtVerifierError::KeyNotFound)?;

        let stale_failure = match self.verify_token_internal(token, &key, &alg) {
            Err(failure) if key_may_be_stale(&failure) => failure,
            result => return result,
        };

        // `None` covers both unchanged material and a declined fetch, so the original failure
        // stands rather than being re-derived from the same key.
        let Some((key, alg)) = self
            .jwk_service
            .refresh_key(&kid)
            .await
            .map_err(JwtVerifierError::KeyNotFound)?
        else {
            return Err(stale_failure);
        };

        self.verify_token_internal(token, &key, &alg)
    }

    /// Verify a token using only the JWK cache, without any `.await`. `Ok(Some(_))` on
    /// success; `Err` when the token itself is at fault; `Ok(None)` when the cache cannot
    /// answer and the caller must fall back to the async [`verify_token`].
    ///
    /// A signature that does not match the cached key is `Ok(None)`, not `Err`: the cached
    /// key may be a rotated-out one, and only the async path can replace it. Reporting it as
    /// a failure here is what left a rotated key broken until restart even though the
    /// refresh existed.
    pub fn try_verify_token_cached(
        &self,
        token: &str,
    ) -> Result<Option<AuthorizationToken>, JwtVerifierError> {
        let header = decode_header(token).map_err(JwtVerifierError::ValidationFailed)?;
        self.check_typ(&header)?;
        let kid = header.kid.ok_or(JwtVerifierError::HeaderKIDMissing)?;

        let Some((key, alg)) = self.jwk_service.get_cached_key(&kid) else {
            return Ok(None);
        };

        match self.verify_token_internal(token, &key, &alg) {
            Err(failure) if key_may_be_stale(&failure) => Ok(None),
            result => result.map(Some),
        }
    }

    fn verify_token_internal(
        &self,
        token: &str,
        key: &DecodingKey,
        alg: &jsonwebtoken::Algorithm,
    ) -> Result<AuthorizationToken, JwtVerifierError> {
        let mut validation = Validation::new(*alg);
        if let Some(iss) = self.jwt_issuer.as_ref() {
            validation.set_issuer(iss);
        }
        if let Some(aud) = self.jwt_audience.as_ref() {
            validation.set_audience(aud);
        }

        validation.validate_exp = true;

        debug!("Decoding JWT token");

        let token_data =
            decode::<AuthorizationToken>(token, key, &validation).map_err(|error| {
                if matches!(
                    error.kind(),
                    jsonwebtoken::errors::ErrorKind::ExpiredSignature
                ) {
                    debug!(error = ?error, "Allowable error decoding JWT token");
                } else {
                    warn!(error = ?error, "Unexpected error decoding JWT token");
                }
                JwtVerifierError::ValidationFailed(error)
            })?;

        debug!("Decoded user info: {:?}", token_data.claims);
        let mut claims = token_data.claims;
        claims.identity = resolve_identity(&claims)?;
        Ok(claims)
    }

    fn check_typ(&self, header: &jsonwebtoken::Header) -> Result<(), JwtVerifierError> {
        let Some(accepted) = self.jwt_typ.as_ref() else {
            return Ok(());
        };
        let presented = header.typ.as_deref().map(normalize_typ);
        let is_accepted = presented.is_some_and(|presented| {
            accepted
                .iter()
                .any(|accepted| normalize_typ(accepted) == presented)
        });
        if is_accepted {
            return Ok(());
        }
        warn!("Rejecting token: the `typ` header is absent or not an accepted type");
        Err(JwtVerifierError::TypNotAccepted)
    }
}

/// The identity `claims` name: the first of `identity`, its namespaced alias
/// and `sub` the token carries, and only that one. `None` stands for `sub`,
/// which `user_id` holds. An empty or non-string value at the claim that
/// decides is an error; the rule does not fall through to the next claim.
fn resolve_identity(claims: &AuthorizationToken) -> Result<Option<String>, JwtVerifierError> {
    let refused = |claim: &str| {
        warn!(
            claim,
            "Rejecting token: the identity claim is empty or not a string"
        );
        Err(JwtVerifierError::IdentityClaimMissing {
            claim: claim.to_string(),
        })
    };
    let Some((claim, value)) = [IDENTITY_CLAIM, IDENTITY_CLAIM_NAMESPACED]
        .into_iter()
        .find_map(|claim| claims.extra.get(claim).map(|value| (claim, value)))
    else {
        return if claims.user_id.is_empty() {
            refused("sub")
        } else {
            Ok(None)
        };
    };
    match value {
        serde_json::Value::String(identity) if !identity.is_empty() => Ok(Some(identity.clone())),
        _ => refused(claim),
    }
}
