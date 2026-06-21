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

pub const PERMISSION_ADMIN: &str = "admin";
pub const PERMISSION_OWNER: &str = "owner";
pub const PERMISSION_READ: &str = "read";
pub const PERMISSION_WRITE: &str = "write";

#[serde_as]
#[derive(Debug, Deserialize, Clone, Serialize, PartialEq)]
pub struct JWTUserInfo {
    #[serde(rename = "sub")]
    pub user_id: String,
    #[serde(rename = "iss")]
    pub issuer: String,
    #[serde(rename = "iat")]
    pub issued_at: u64,
    #[serde_as(as = "OneOrMany<_, PreferMany>")]
    #[serde(rename = "aud")]
    pub audience: Vec<String>,
    pub env: String,
    pub name: String,
    pub preferred_username: String,
    pub is_service_account: Option<bool>,
    #[serde(rename = "exp")]
    pub expires: u64,
}

/// From Lore protos, but cannot derive deserialize on external type
#[derive(Debug, Deserialize, Clone, Serialize, PartialEq)]
pub struct ResourcePermission {
    pub resource_id: String,
    pub permission: Vec<String>,
}

impl ResourcePermission {
    pub fn is_wildcard_resource(&self) -> bool {
        self.resource_id == "urc-*"
    }

    pub fn matches_repository(&self, repository_id: &String) -> bool {
        self.resource_id == *repository_id || self.is_wildcard_resource()
    }

    pub fn has_any_permission(&self, permissions: &[&str]) -> bool {
        permissions
            .iter()
            .any(|permission| self.permission.iter().any(|item| item == permission))
    }
}

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
    pub env: String,
    pub name: String,
    pub preferred_username: String,
    pub resources: Option<Vec<ResourcePermission>>,
    pub groups: Option<Vec<String>>,
    pub is_service_account: Option<bool>,
    pub idp: String,
}

#[derive(Debug, Error)]
pub enum JwtVerifierError {
    #[error("JWT header does not contain a kid")]
    HeaderKIDMissing,
    #[error("JWT header could not be parsed")]
    KeyNotFound(#[from] JWKServiceError),
    #[error("JWT validation failed")]
    ValidationFailed(#[from] jsonwebtoken::errors::Error),
    #[error("JWT authorization failed")]
    NotAuthorized,
}

#[derive(Clone)]
pub struct JwtVerifier {
    pub jwk_service: Arc<dyn JWKService>,
    pub jwt_issuer: Option<String>,
    pub jwt_audience: Option<Vec<String>>,
}

impl JwtVerifier {
    pub async fn verify_token(&self, token: &str) -> Result<AuthorizationToken, JwtVerifierError> {
        let header = decode_header(token).map_err(JwtVerifierError::ValidationFailed)?;
        let kid = header.kid.ok_or(JwtVerifierError::HeaderKIDMissing)?;

        let (key, alg) = self
            .jwk_service
            .get_key(&kid)
            .await
            .map_err(JwtVerifierError::KeyNotFound)?;

        self.verify_token_internal(token, &key, &alg)
    }

    fn verify_token_internal(
        &self,
        token: &str,
        key: &DecodingKey,
        alg: &jsonwebtoken::Algorithm,
    ) -> Result<AuthorizationToken, JwtVerifierError> {
        let mut validation = Validation::new(*alg);
        if let Some(iss) = self.jwt_issuer.as_ref() {
            validation.set_issuer(&[iss]);
        }
        if let Some(aud) = self.jwt_audience.as_ref() {
            validation.set_audience(aud);
        }

        validation.validate_exp = true;

        debug!("Decoding JWT token");

        if let Ok(token_data) = decode::<AuthorizationToken>(token, key, &validation) {
            debug!("Decoded user info: {:?}", token_data.claims);
            Ok(token_data.claims)
        } else {
            let token_data = decode::<JWTUserInfo>(token, key, &validation).map_err(|error| {
                if matches!(
                    error.kind(),
                    jsonwebtoken::errors::ErrorKind::ExpiredSignature
                ) {
                    debug!(error = ?error, "Allowable error decoding JWT AuthN token");
                } else {
                    warn!(error = ?error, "Unexpected error decoding JWT AuthN token");
                }
                JwtVerifierError::ValidationFailed(error)
            })?;

            let token = token_data.claims;
            Ok(AuthorizationToken {
                user_id: token.user_id,
                issuer: token.issuer,
                issued_at: token.issued_at,
                expires: token.expires,
                audience: token.audience,
                env: token.env,
                name: token.name,
                preferred_username: token.preferred_username,
                resources: None,
                groups: None,
                is_service_account: token.is_service_account,
                idp: String::default(),
            })
        }
    }
}

pub fn verify_authorization(
    authorization: &AuthorizationToken,
    repository: lore_revision::lore::RepositoryId,
) -> Result<(), JwtVerifierError> {
    if has_repository_read_permission(authorization, repository) {
        return Ok(());
    }

    Err(JwtVerifierError::NotAuthorized)
}

pub fn has_repository_read_permission(
    authorization: &AuthorizationToken,
    repository: lore_revision::lore::RepositoryId,
) -> bool {
    has_any_repository_permission(
        authorization,
        repository,
        &[
            PERMISSION_READ,
            PERMISSION_WRITE,
            PERMISSION_ADMIN,
            PERMISSION_OWNER,
        ],
    )
}

pub fn has_repository_write_permission(
    authorization: &AuthorizationToken,
    repository: lore_revision::lore::RepositoryId,
) -> bool {
    has_any_repository_permission(
        authorization,
        repository,
        &[PERMISSION_WRITE, PERMISSION_ADMIN, PERMISSION_OWNER],
    )
}

pub fn has_any_repository_permission(
    authorization: &AuthorizationToken,
    repository: lore_revision::lore::RepositoryId,
    permissions: &[&str],
) -> bool {
    if let Some(resources) = authorization.resources.as_ref() {
        let checked_repository = format!("urc-{repository}");
        for authorized_resource in resources.iter() {
            if authorized_resource.matches_repository(&checked_repository)
                && authorized_resource.has_any_permission(permissions)
            {
                return true;
            }
        }
    }

    false
}

pub fn verify_repository_write_authorization(
    authorization: &AuthorizationToken,
    repository: lore_revision::lore::RepositoryId,
) -> Result<(), JwtVerifierError> {
    if has_repository_write_permission(authorization, repository) {
        return Ok(());
    }

    Err(JwtVerifierError::NotAuthorized)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use lore_base::types::Context;
    use lore_revision::lore::RepositoryId;

    use super::*;

    #[test]
    fn resource_permission_matches_wildcard_resource() {
        let wildcard_resource_permission = ResourcePermission {
            permission: vec![],
            resource_id: "urc-*".to_string(),
        };
        let non_wildcard_resource_permission = ResourcePermission {
            permission: vec![],
            resource_id: "urc-123456".to_string(),
        };
        assert!(wildcard_resource_permission.is_wildcard_resource());
        assert!(!non_wildcard_resource_permission.is_wildcard_resource());
    }

    #[test]
    fn resource_permission_matches_repository() {
        let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
        let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
        let wildcard_resource_permission = ResourcePermission {
            permission: vec![],
            resource_id: "urc-*".to_string(),
        };
        let regular_resource_permission = ResourcePermission {
            permission: vec![],
            resource_id: test_repository_id.clone(),
        };
        assert!(wildcard_resource_permission.matches_repository(&test_repository_id));
        assert!(wildcard_resource_permission.matches_repository(&unrelated_repository_id));
        assert!(regular_resource_permission.matches_repository(&test_repository_id));
        assert!(!regular_resource_permission.matches_repository(&unrelated_repository_id));
    }

    #[test]
    fn verify_authorization_allows_repo_from_token() {
        let allowed_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
        let resource_permission = ResourcePermission {
            permission: vec![PERMISSION_READ.to_string()],
            resource_id: allowed_repository_id.clone(),
        };
        let authorization_token = AuthorizationToken {
            audience: vec!["test".to_string()],
            env: "test".to_string(),
            expires: 1234,
            user_id: "test".to_string(),
            idp: "test".to_string(),
            issuer: "test".to_string(),
            name: "test".to_string(),
            preferred_username: "test".to_string(),
            groups: None,
            is_service_account: Some(false),
            issued_at: 123,
            resources: Some(vec![resource_permission]),
        };
        let allowed_context: RepositoryId = Context::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into();
        let unexpected_context: RepositoryId =
            Context::from_str("f6ca55437aa34198ba0f0fdc33154d51")
                .unwrap()
                .into();
        verify_authorization(&authorization_token, allowed_context).expect("verify auth failed");
        verify_authorization(&authorization_token, unexpected_context)
            .expect_err("verify auth should have failed");
    }

    #[test]
    fn verify_authorization_allows_all_repos_for_wildcard_token() {
        let resource_permission = ResourcePermission {
            permission: vec![PERMISSION_READ.to_string()],
            resource_id: "urc-*".to_string(),
        };
        let wildcard_authorization_token = AuthorizationToken {
            audience: vec!["test".to_string()],
            env: "test".to_string(),
            expires: 1234,
            user_id: "test".to_string(),
            idp: "test".to_string(),
            issuer: "test".to_string(),
            name: "test".to_string(),
            preferred_username: "test".to_string(),
            groups: None,
            is_service_account: Some(false),
            issued_at: 123,
            resources: Some(vec![resource_permission]),
        };
        let test_contexts: Vec<RepositoryId> = vec![
            Context::from_str("0194b726b34e72b0b45550b88a967076")
                .unwrap()
                .into(),
            Context::from_str("f6ca55437aa34198ba0f0fdc33154d51")
                .unwrap()
                .into(),
            Context::from_str("54006a8ca619475881f7083d625a7947")
                .unwrap()
                .into(),
        ];

        for context in test_contexts {
            verify_authorization(&wildcard_authorization_token, context)
                .expect("verify auth failed");
        }
    }

    #[test]
    fn verify_authorization_rejects_repo_without_read_permission() {
        let repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
        let resource_permission = ResourcePermission {
            permission: vec![],
            resource_id: repository_id.clone(),
        };
        let authorization_token = AuthorizationToken {
            audience: vec!["test".to_string()],
            env: "test".to_string(),
            expires: 1234,
            user_id: "test".to_string(),
            idp: "test".to_string(),
            issuer: "test".to_string(),
            name: "test".to_string(),
            preferred_username: "test".to_string(),
            groups: None,
            is_service_account: Some(false),
            issued_at: 123,
            resources: Some(vec![resource_permission]),
        };
        let repository: RepositoryId = Context::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into();

        verify_authorization(&authorization_token, repository)
            .expect_err("resource without read-like permission must be rejected");
    }

    mod jwt_verifier {

        use std::error::Error;
        use std::ops::Add;
        use std::time::Duration;

        use jsonwebtoken::Algorithm;
        use jsonwebtoken::EncodingKey;
        use jsonwebtoken::Header;
        use jsonwebtoken::encode;
        use serde_json::json;

        use super::*;

        const AGREED_UPON_ALGORITHM: Algorithm = Algorithm::HS256;
        const AGREED_UPON_SIGNING_SECRET: &str = "the-secret";

        mockall::mock! {

            #[derive(Debug)]
            pub TestJWKService {}

            #[async_trait::async_trait]
            impl JWKService for TestJWKService {
                async fn get_key(
            &self,
            kid: &str,
        ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError>;
            }
        }

        fn encode_jwt<T>(jwt_claims: &T) -> String
        where
            T: Serialize,
        {
            let jwt_key = EncodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref());
            let jwt_header = {
                let mut header = Header::new(AGREED_UPON_ALGORITHM);
                header.kid = Some("the kid".into());
                header
            };

            encode(&jwt_header, &jwt_claims, &jwt_key).unwrap()
        }

        fn mock_authz_token(audience: Vec<String>) -> AuthorizationToken {
            AuthorizationToken {
                user_id: "the u".to_string(),
                issuer: "the issuer".to_string(),
                issued_at: 1,
                audience,
                env: "the env".to_string(),
                name: "the name".to_string(),
                preferred_username: "pu".to_string(),
                resources: None,
                groups: None,
                is_service_account: Some(false),
                expires: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .add(Duration::from_secs(5))
                    .as_secs(),
                idp: "the idp".to_string(),
            }
        }

        fn mock_authn_token(audience: Vec<String>) -> JWTUserInfo {
            JWTUserInfo {
                user_id: "the u".to_string(),
                issuer: "the issuer".to_string(),
                issued_at: 1,
                audience,
                env: "the env".to_string(),
                name: "the name".to_string(),
                preferred_username: "pu".to_string(),
                is_service_account: Some(false),
                expires: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .add(Duration::from_secs(5))
                    .as_secs(),
            }
        }

        fn make_authz_token_with_audience(audience: Vec<String>) -> (AuthorizationToken, String) {
            let jwt_claims = mock_authz_token(audience);
            let encoded = encode_jwt(&jwt_claims);
            (jwt_claims, encoded)
        }

        fn make_authn_token_with_audience(audience: Vec<String>) -> (JWTUserInfo, String) {
            let jwt_claims = mock_authn_token(audience);
            let encoded = encode_jwt(&jwt_claims);
            (jwt_claims, encoded)
        }

        // a legacy token verified against an updated server with multiple audiences allowed
        #[tokio::test]
        async fn verify_string_audience_in_authn_token_against_multiple_allowed()
        -> Result<(), Box<dyn Error>> {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().returning(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });

            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["urc.example.com".to_string(), "URC_test".to_string()]),
            };

            let authn_string_audience = json!({
                "sub": "the u".to_string(),
                "iss": "the issuer".to_string(),
                "iat": 1,
                "aud": "URC_test", // crucial bit
                "env": "the env".to_string(),
                "name": "the name".to_string(),
                "preferred_username": "pu".to_string(),
                "is_service_account": false,
                "exp": SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .add(Duration::from_secs(5))
                    .as_secs(),
            });
            let encoded = encode_jwt(&authn_string_audience);
            let verified_authn_token = verifier.verify_token(&encoded).await?;
            assert_eq!(verified_authn_token.audience, vec!["URC_test".to_string()]);

            Ok(())
        }

        #[tokio::test]
        async fn verify_string_audience_in_authz_token_against_multiple_allowed()
        -> Result<(), Box<dyn Error>> {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().returning(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });

            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["urc.example.com".to_string(), "URC_test".to_string()]),
            };

            let base_authz_token = mock_authz_token(vec!["URC_test".to_string()]);
            let authz_string_audience = json!({
                "idp": base_authz_token.idp,
                "sub": base_authz_token.user_id,
                "iss": base_authz_token.issuer,
                "iat":base_authz_token.issued_at,
                "aud": "URC_test", // crucial bit
                "env": base_authz_token.env,
                "name": base_authz_token.name,
                "preferred_username": base_authz_token.preferred_username,
                "is_service_account": false,
                "exp": base_authz_token.expires
            });
            let encoded = encode_jwt(&authz_string_audience);
            let verified_authz_token = verifier.verify_token(&encoded).await?;
            assert_eq!(verified_authz_token, base_authz_token);

            Ok(())
        }

        #[tokio::test]
        async fn verify_single_audience_against_multiple_allowed() -> Result<(), Box<dyn Error>> {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().returning(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });

            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["urc.example.com".to_string(), "Lore".to_string()]),
            };
            let (original_authz_token, encoded_authz_token) =
                make_authz_token_with_audience(vec!["Lore".to_string()]);
            let (original_authn_token, encoded_authn_token) =
                make_authn_token_with_audience(vec!["Lore".to_string()]);

            let verified_authz_token = verifier.verify_token(&encoded_authz_token).await?;
            let verified_authn_token = verifier.verify_token(&encoded_authn_token).await?;
            assert_eq!(original_authz_token, verified_authz_token);
            assert_eq!(
                original_authn_token.audience,
                verified_authn_token.audience.clone()
            );

            Ok(())
        }

        // an updated token verified against an updated server with multiple audiences allowed
        #[tokio::test]
        async fn verify_multiple_audience_against_multiple_allowed() -> Result<(), Box<dyn Error>> {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().return_once(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });

            let common_audience = vec!["urc.example.com".to_string(), "Lore".to_string()];

            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(common_audience.clone()),
            };

            let (original_token, encoded_token) = make_authz_token_with_audience(common_audience);

            let verified_token = verifier.verify_token(&encoded_token).await?;
            assert_eq!(original_token, verified_token);

            Ok(())
        }

        // an updated token verified against a old server config with a single audience allowed
        #[tokio::test]
        async fn verify_multiple_audience_against_single_allowed() -> Result<(), Box<dyn Error>> {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().return_once(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });

            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
            };

            let (original_token, encoded_token) = make_authz_token_with_audience(vec![
                "urc.example.com".to_string(),
                "Lore".to_string(),
            ]);

            let verified_token = verifier.verify_token(&encoded_token).await?;
            assert_eq!(original_token, verified_token);

            Ok(())
        }

        #[tokio::test]
        async fn rejects_unrecognised_audience() -> Result<(), Box<dyn Error>> {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().return_once(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });

            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["skein".to_string()]),
            };

            let (_, encoded_token) = make_authz_token_with_audience(vec!["Lore".to_string()]);

            let verify_error = verifier.verify_token(&encoded_token).await.unwrap_err();
            assert!(matches!(
                verify_error,
                JwtVerifierError::ValidationFailed(_)
            ));

            Ok(())
        }
    }
}
