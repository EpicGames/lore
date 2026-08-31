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
    if let Some(resources) = authorization.resources.as_ref() {
        let checked_repository = format!("urc-{repository}");
        for authorized_resource in resources.iter() {
            if authorized_resource.matches_repository(&checked_repository) {
                return Ok(());
            }
        }
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
            permission: vec![],
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
            permission: vec![],
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

                fn get_cached_key(
            &self,
            kid: &str,
        ) -> Option<(DecodingKey, jsonwebtoken::Algorithm)>;

                async fn refresh_key(
            &self,
            kid: &str,
        ) -> Result<Option<(DecodingKey, jsonwebtoken::Algorithm)>, JWKServiceError>;
            }
        }

        fn encode_jwt<T>(jwt_claims: &T) -> String
        where
            T: Serialize,
        {
            encode_jwt_signed_with(AGREED_UPON_SIGNING_SECRET, jwt_claims)
        }

        fn encode_jwt_signed_with<T>(secret: &str, jwt_claims: &T) -> String
        where
            T: Serialize,
        {
            let jwt_key = EncodingKey::from_secret(secret.as_ref());
            let jwt_header = {
                let mut header = Header::new(AGREED_UPON_ALGORITHM);
                header.kid = Some("the kid".into());
                header
            };

            encode(&jwt_header, &jwt_claims, &jwt_key).unwrap()
        }

        /// A key service whose material changes under an unchanged key id, which is the
        /// rotation a cache keyed on the id alone cannot see. Refreshes are counted so a
        /// test can assert that a failure no key could explain never asks for one.
        struct RotatingJWKService {
            served: std::sync::Mutex<String>,
            rotates_to: Option<String>,
            refreshes: std::sync::atomic::AtomicUsize,
        }

        impl RotatingJWKService {
            fn new(served: &str, rotates_to: Option<&str>) -> Self {
                RotatingJWKService {
                    served: std::sync::Mutex::new(served.to_string()),
                    rotates_to: rotates_to.map(str::to_string),
                    refreshes: std::sync::atomic::AtomicUsize::new(0),
                }
            }

            fn refreshes(&self) -> usize {
                self.refreshes.load(std::sync::atomic::Ordering::Relaxed)
            }

            fn current(&self) -> (DecodingKey, Algorithm) {
                let served = self.served.lock().expect("served key");
                (
                    DecodingKey::from_secret(served.as_bytes()),
                    AGREED_UPON_ALGORITHM,
                )
            }
        }

        #[async_trait::async_trait]
        impl JWKService for RotatingJWKService {
            async fn get_key(
                &self,
                _kid: &str,
            ) -> Result<(DecodingKey, Algorithm), JWKServiceError> {
                Ok(self.current())
            }

            fn get_cached_key(&self, _kid: &str) -> Option<(DecodingKey, Algorithm)> {
                Some(self.current())
            }

            async fn refresh_key(
                &self,
                _kid: &str,
            ) -> Result<Option<(DecodingKey, Algorithm)>, JWKServiceError> {
                self.refreshes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(rotated) = self.rotates_to.as_ref() else {
                    return Ok(None);
                };
                let mut served = self.served.lock().expect("served key");
                if *served == *rotated {
                    return Ok(None);
                }
                served.clone_from(rotated);
                Ok(Some((
                    DecodingKey::from_secret(served.as_bytes()),
                    AGREED_UPON_ALGORITHM,
                )))
            }
        }

        fn verifier_for(service: Arc<RotatingJWKService>) -> JwtVerifier {
            JwtVerifier {
                jwk_service: service,
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
            }
        }

        /// Well past `Validation`'s default 60-second leeway, so the expiry is what fails.
        fn expired_authz_token() -> AuthorizationToken {
            let mut claims = mock_authz_token(vec!["Lore".to_string()]);
            claims.expires = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 3600;
            claims
        }

        /// The finding: the identity provider replaces the material behind a key id without
        /// changing the id. Every lookup is satisfied by the cached key, so every token
        /// signed with the new one fails until the process restarts.
        #[tokio::test]
        async fn a_key_rotated_under_the_same_kid_is_picked_up() {
            let service = Arc::new(RotatingJWKService::new(
                "rotated-out-secret",
                Some(AGREED_UPON_SIGNING_SECRET),
            ));
            let verifier = verifier_for(service.clone());
            let (expected, encoded) = make_authz_token_with_audience(vec!["Lore".to_string()]);

            let verified = verifier
                .verify_token(&encoded)
                .await
                .expect("a rotated key is refetched");

            assert_eq!(verified, expected);
            assert_eq!(service.refreshes(), 1);
        }

        /// The interceptor's synchronous path has to defer rather than deny, or the retry
        /// above is never reached for gRPC traffic.
        #[test]
        fn the_cached_path_defers_a_signature_failure_to_the_async_path() {
            let service = Arc::new(RotatingJWKService::new("rotated-out-secret", None));
            let verifier = verifier_for(service.clone());
            let (_, encoded) = make_authz_token_with_audience(vec!["Lore".to_string()]);

            let verdict = verifier
                .try_verify_token_cached(&encoded)
                .expect("a signature failure is not the token's fault");

            assert!(verdict.is_none(), "must fall through to the async path");
        }

        /// A key that has not in fact rotated must cost exactly one refresh, not one per
        /// verification attempt and not a retry loop.
        #[tokio::test]
        async fn a_key_that_did_not_rotate_is_refreshed_once_and_then_fails() {
            let service = Arc::new(RotatingJWKService::new("wrong-secret", None));
            let verifier = verifier_for(service.clone());
            let (_, encoded) = make_authz_token_with_audience(vec!["Lore".to_string()]);

            let error = verifier
                .verify_token(&encoded)
                .await
                .expect_err("no key can verify this token");

            assert!(matches!(error, JwtVerifierError::ValidationFailed(_)));
            assert_eq!(service.refreshes(), 1);
        }

        /// An expired token is the token's fault. Refreshing keys cannot change the verdict,
        /// and anyone can present one — so it must not reach the refresh at all, on either
        /// path. This is the bound on using invalid tokens to drive outbound requests.
        #[tokio::test]
        async fn a_token_that_no_key_could_rescue_never_asks_for_a_refresh() {
            let service = Arc::new(RotatingJWKService::new(
                AGREED_UPON_SIGNING_SECRET,
                Some(AGREED_UPON_SIGNING_SECRET),
            ));
            let verifier = verifier_for(service.clone());
            let encoded = encode_jwt(&expired_authz_token());

            verifier
                .verify_token(&encoded)
                .await
                .expect_err("an expired token stays rejected");
            verifier
                .try_verify_token_cached(&encoded)
                .expect_err("and is rejected outright, not deferred");

            assert_eq!(service.refreshes(), 0);
        }

        /// The same for a token whose audience is wrong, which is the other failure an
        /// unauthenticated caller can produce at will against a perfectly good key.
        #[tokio::test]
        async fn a_wrong_audience_never_asks_for_a_refresh() {
            let service = Arc::new(RotatingJWKService::new(
                AGREED_UPON_SIGNING_SECRET,
                Some(AGREED_UPON_SIGNING_SECRET),
            ));
            let verifier = verifier_for(service.clone());
            let (_, encoded) = make_authz_token_with_audience(vec!["not-lore".to_string()]);

            verifier
                .verify_token(&encoded)
                .await
                .expect_err("wrong audience stays rejected");

            assert_eq!(service.refreshes(), 0);
        }

        /// Modulus and exponent of the RSA example key from RFC 7515 Appendix A.2. Public
        /// values — which is the whole point of the test below.
        const RSA_N: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4\
                             cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiF\
                             V4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6C\
                             f0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9\
                             c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTW\
                             hAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1\
                             jF44-csFCur-kEgU8awapJzKnqDKgw";
        const RSA_E: &str = "AQAB";

        /// A key service serving one RSA public key under `the kid`, as a real provider would.
        fn rsa_verifier() -> JwtVerifier {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().returning(|_| {
                Ok((
                    DecodingKey::from_rsa_components(RSA_N, RSA_E).expect("rsa decoding key"),
                    Algorithm::RS256,
                ))
            });
            // A rejected signature looks like a possible rotation, so the retry is reached.
            // Serving no replacement keeps these tests about the first verdict.
            service.expect_refresh_key().returning(|_| Ok(None));

            JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
            }
        }

        /// Assemble a token with an arbitrary header, since `encode` will not produce the
        /// mismatches these tests are about.
        fn token_with_header(
            header_json: &str,
            claims: &impl Serialize,
            signature: &str,
        ) -> String {
            use base64::Engine;
            use base64::engine::general_purpose::URL_SAFE_NO_PAD;

            let header = URL_SAFE_NO_PAD.encode(header_json);
            let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims"));
            format!("{header}.{claims}.{signature}")
        }

        /// The algorithm-confusion forgery, and the reason the algorithm comes from the JWK
        /// rather than the token.
        ///
        /// An RSA public key is published to the world in the JWKS. If the header could choose
        /// the algorithm, an attacker would sign with HS256 using that public modulus as the
        /// shared secret, and the server — holding the same public value — would agree. Nobody
        /// needs the private key for this. The signature here is genuinely valid for the
        /// algorithm the token claims; it is refused because the token does not get a say.
        #[tokio::test]
        async fn a_public_rsa_key_is_never_accepted_as_an_hmac_secret() {
            let verifier = rsa_verifier();
            let claims = mock_authz_token(vec!["Lore".to_string()]);

            let forged = {
                let jwt_key = EncodingKey::from_secret(RSA_N.as_bytes());
                let mut header = Header::new(Algorithm::HS256);
                header.kid = Some("the kid".into());
                encode(&header, &claims, &jwt_key).expect("attacker signs with the public modulus")
            };

            let error = verifier
                .verify_token(&forged)
                .await
                .expect_err("an RSA key must never verify an HMAC signature");

            // Specifically the algorithm, not some incidental claim failure — otherwise this
            // would still pass with the pin removed.
            let JwtVerifierError::ValidationFailed(inner) = &error else {
                panic!("expected a validation failure, got {error:?}");
            };
            assert!(
                matches!(
                    inner.kind(),
                    jsonwebtoken::errors::ErrorKind::InvalidAlgorithm
                ),
                "the header's algorithm is refused, not merely the signature: {inner:?}"
            );
        }

        /// The same refusal on the synchronous interceptor path, which must not be a way
        /// around the async one.
        #[test]
        fn the_cached_path_also_refuses_an_hmac_signature_against_an_rsa_key() {
            let mut service = MockTestJWKService::new();
            // `times(1)` matters: `Ok(None)` is also what a cache miss produces, so without
            // proving the key was served this would pass against a mock that returned nothing.
            service.expect_get_cached_key().times(1).returning(|_| {
                Some((
                    DecodingKey::from_rsa_components(RSA_N, RSA_E).expect("rsa decoding key"),
                    Algorithm::RS256,
                ))
            });
            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
            };

            let forged = {
                let jwt_key = EncodingKey::from_secret(RSA_N.as_bytes());
                let mut header = Header::new(Algorithm::HS256);
                header.kid = Some("the kid".into());
                encode(&header, &jwt_claims_for_forgery(), &jwt_key).expect("forge")
            };

            // Deferred rather than denied outright, because a rejected signature is how a
            // rotated key presents — but never accepted.
            let verdict = verifier
                .try_verify_token_cached(&forged)
                .expect("not the token's own fault");
            assert!(verdict.is_none(), "must never verify, on any path");
        }

        fn jwt_claims_for_forgery() -> AuthorizationToken {
            mock_authz_token(vec!["Lore".to_string()])
        }

        /// A token naming a different RSA algorithm than the key does is refused too. The
        /// signature is nonsense, but the algorithm check fires before it is ever examined,
        /// which is what makes it a pin rather than a preference.
        #[tokio::test]
        async fn a_token_naming_another_algorithm_for_the_same_key_is_refused() {
            let verifier = rsa_verifier();
            let claims = mock_authz_token(vec!["Lore".to_string()]);
            let token = token_with_header(
                r#"{"alg":"RS512","typ":"JWT","kid":"the kid"}"#,
                &claims,
                "bm90LWEtc2lnbmF0dXJl",
            );

            let error = verifier
                .verify_token(&token)
                .await
                .expect_err("the key is pinned to RS256");
            let JwtVerifierError::ValidationFailed(inner) = &error else {
                panic!("expected a validation failure, got {error:?}");
            };
            assert!(
                matches!(
                    inner.kind(),
                    jsonwebtoken::errors::ErrorKind::InvalidAlgorithm
                ),
                "the algorithm is refused before the signature is looked at: {inner:?}"
            );
        }

        /// `alg: none` is the other half of the classic pair. It has no `Algorithm` at all, so
        /// it cannot match the pinned one and the token is thrown out at the header.
        #[tokio::test]
        async fn a_token_claiming_no_algorithm_is_refused() {
            let verifier = rsa_verifier();
            let claims = mock_authz_token(vec!["Lore".to_string()]);
            let token =
                token_with_header(r#"{"alg":"none","typ":"JWT","kid":"the kid"}"#, &claims, "");

            verifier
                .verify_token(&token)
                .await
                .expect_err("an unsigned token is never acceptable");
        }

        /// A token signed by a key that was never served must still be refused after the
        /// refresh, or the retry would be a way around verification rather than a way to
        /// pick up a rotation.
        #[tokio::test]
        async fn a_token_signed_by_an_unknown_key_is_still_refused_after_a_refresh() {
            let service = Arc::new(RotatingJWKService::new(
                "rotated-out-secret",
                Some(AGREED_UPON_SIGNING_SECRET),
            ));
            let verifier = verifier_for(service.clone());
            let encoded = encode_jwt_signed_with(
                "an-attackers-secret",
                &mock_authz_token(vec!["Lore".to_string()]),
            );

            verifier
                .verify_token(&encoded)
                .await
                .expect_err("a forged token is refused even though the key rotated");
            assert_eq!(service.refreshes(), 1);
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
