// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use jsonwebtoken::DecodingKey;
use lore_base::types::Context;
use lore_revision::lore::RepositoryId;
use lore_server::auth::jwk::JWKService;
use lore_server::auth::jwk::JWKServiceError;
use lore_server::auth::jwt::*;
use lore_server::authnz::repository_authorizer::Action;
use serde::Serialize;

/// The in-place action check agrees with the merged set it stands in for:
/// only entries matching the partition count, the wildcard included.
#[test]
fn permits_reads_only_matching_entries() {
    let matcher = ResourceMatcher::default();
    let repository: RepositoryId = Context::from_str("0194b726b34e72b0b45550b88a967076")
        .unwrap()
        .into();
    let entry = |resource_id: &str, permissions: &[&str]| ResourcePermission {
        resource_id: resource_id.to_string(),
        permission: permissions.iter().map(ToString::to_string).collect(),
    };
    let resources = vec![
        entry(&format!("urc-{repository}"), &["read"]),
        entry("urc-somewhere-else", &["obliterate"]),
        entry("urc-*", &["migrate"]),
    ];
    for action in [
        Action::Read,
        Action::Migrate,
        Action::Obliterate,
        Action::Presign,
    ] {
        assert_eq!(
            matcher.permits(&resources, repository, action),
            matcher
                .merged_permissions(&resources, repository)
                .iter()
                .any(|granted| granted == action.as_str()),
            "{action}"
        );
    }
    assert!(matcher.permits(&resources, repository, Action::Migrate));
    assert!(!matcher.permits(&resources, repository, Action::Obliterate));
    assert!(!matcher.permits(&[], repository, Action::Read));
}

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
    assert!(wildcard_resource_permission.is_wildcard_resource("urc-*"));
    assert!(!non_wildcard_resource_permission.is_wildcard_resource("urc-*"));
    // The wildcard is configuration, not a literal.
    assert!(non_wildcard_resource_permission.is_wildcard_resource("urc-123456"));
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
    assert!(wildcard_resource_permission.matches_resource(&test_repository_id, "urc-*"));
    assert!(wildcard_resource_permission.matches_resource(&unrelated_repository_id, "urc-*"));
    assert!(regular_resource_permission.matches_resource(&test_repository_id, "urc-*"));
    assert!(!regular_resource_permission.matches_resource(&unrelated_repository_id, "urc-*"));
}

mod resource_matcher {
    use super::*;

    fn repository(id: &str) -> RepositoryId {
        Context::from_str(id).unwrap().into()
    }

    fn entry(resource_id: &str, permissions: &[&str]) -> ResourcePermission {
        ResourcePermission {
            resource_id: resource_id.to_string(),
            permission: permissions.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn default_template_reproduces_the_legacy_matching() {
        let repository_id = "0194b726b34e72b0b45550b88a967076";
        let matcher = ResourceMatcher::default();
        assert_eq!(
            matcher.resource_for(repository(repository_id)),
            format!("urc-{repository_id}")
        );
        let resources = vec![entry(&format!("urc-{repository_id}"), &["push"])];
        assert!(matcher.any_match(&resources, repository(repository_id)));
        assert!(!matcher.any_match(&resources, repository("0192ae48ccf17060bc1ba9d04f6acb2f")));
    }

    #[test]
    fn a_configured_template_and_wildcard_are_honoured() {
        let repository_id = "0194b726b34e72b0b45550b88a967076";
        let matcher = ResourceMatcher::new("repo:{id}".to_string(), "repo:all".to_string());
        let resources = vec![entry("repo:all", &["read"])];
        assert!(matcher.any_match(&resources, repository(repository_id)));
        assert_eq!(
            matcher.merged_permissions(&resources, repository(repository_id)),
            vec!["read".to_string()]
        );
        // The legacy literal is just another resource id under this config.
        let legacy = vec![entry("urc-*", &["read"])];
        assert!(!matcher.any_match(&legacy, repository(repository_id)));
    }

    #[test]
    fn permissions_merge_across_all_matching_entries() {
        let repository_id = "0194b726b34e72b0b45550b88a967076";
        let matcher = ResourceMatcher::default();
        let resources = vec![
            entry(&format!("urc-{repository_id}"), &["push"]),
            entry("urc-*", &["migrate"]),
            entry(&format!("urc-{repository_id}"), &["obliterate"]),
            entry("urc-0192ae48ccf17060bc1ba9d04f6acb2f", &["unrelated"]),
        ];
        let merged = matcher.merged_permissions(&resources, repository(repository_id));
        assert_eq!(
            merged,
            vec![
                "push".to_string(),
                "migrate".to_string(),
                "obliterate".to_string()
            ]
        );
    }
}

mod claim_at {
    use serde_json::json;

    use super::*;

    fn token_with_extra(extra: serde_json::Value) -> AuthorizationToken {
        let serde_json::Value::Object(extra) = extra else {
            panic!("extra claims must be a JSON object");
        };
        AuthorizationToken {
            user_id: "the u".to_string(),
            name: Some("the name".to_string()),
            groups: Some(vec!["readers".to_string()]),
            extra,
            ..Default::default()
        }
    }

    #[test]
    fn resolves_a_nested_path() {
        let token = token_with_extra(json!({
            "realm_access": { "roles": ["obliterate", "admin"] }
        }));
        assert_eq!(
            token.claim_at("realm_access.roles"),
            Some(json!(["obliterate", "admin"]))
        );
    }

    #[test]
    fn resolves_a_flat_named_field() {
        let token = token_with_extra(json!({}));
        assert_eq!(token.claim_at("groups"), Some(json!(["readers"])));
    }

    #[test]
    fn a_missing_path_is_none() {
        let token = token_with_extra(json!({}));
        assert_eq!(token.claim_at("realm_access.roles"), None);
        assert_eq!(token.claim_at("no_such_claim"), None);
    }

    #[test]
    fn a_path_through_a_non_object_is_none() {
        let token = token_with_extra(json!({ "realm_access": "a string" }));
        assert_eq!(token.claim_at("realm_access.roles"), None);
        assert_eq!(token.claim_at("name.first"), None);
    }

    #[test]
    fn a_named_field_shadows_extra() {
        // Decoding never puts a consumed claim into `extra`, but a
        // constructed token can. The named field must win.
        let token = token_with_extra(json!({ "name": "the impostor" }));
        assert_eq!(token.claim_at("name"), Some(json!("the name")));
    }

    /// The `identity` field is the verifier's, not the token's: a claim
    /// that happens to share the name is an unknown extra claim.
    #[test]
    fn a_claim_named_identity_does_not_populate_the_resolved_identity() {
        let token: AuthorizationToken = serde_json::from_value(json!({
            "iss": "the issuer",
            "sub": "the u",
            "aud": ["Lore"],
            "iat": 1,
            "exp": 2,
            "identity": "the impostor",
        }))
        .expect("decodes");
        assert_eq!(token.identity, None);
        assert_eq!(token.identity(), "the u");
        assert_eq!(token.claim_at("identity"), Some(json!("the impostor")));

        let serialized = serde_json::to_value(&token).expect("serializes");
        assert_eq!(serialized["identity"], json!("the impostor"));
    }

    /// Decode then re-serialize preserves unknown claims.
    #[test]
    fn unknown_claims_survive_a_round_trip() {
        let claims = json!({
            "iss": "the issuer",
            "sub": "the u",
            "aud": ["Lore"],
            "iat": 1,
            "exp": 2,
            "realm_access": { "roles": ["obliterate"] },
            "custom_scalar": 42,
        });

        let token: AuthorizationToken = serde_json::from_value(claims).unwrap();
        let reserialized = serde_json::to_value(&token).unwrap();

        assert_eq!(
            reserialized.get("realm_access"),
            Some(&json!({ "roles": ["obliterate"] }))
        );
        assert_eq!(reserialized.get("custom_scalar"), Some(&json!(42)));
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
        async fn get_key(&self, _kid: &str) -> Result<(DecodingKey, Algorithm), JWKServiceError> {
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
            jwt_typ: None,
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
            jwt_typ: None,
        }
    }

    /// Assemble a token with an arbitrary header, since `encode` will not produce the
    /// mismatches these tests are about.
    fn token_with_header(header_json: &str, claims: &impl Serialize, signature: &str) -> String {
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
            jwt_typ: None,
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
        let token = token_with_header(r#"{"alg":"none","typ":"JWT","kid":"the kid"}"#, &claims, "");

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
            env: Some("the env".to_string()),
            name: Some("the name".to_string()),
            preferred_username: Some("pu".to_string()),
            client_id: None,
            resources: None,
            groups: None,
            is_service_account: Some(false),
            expires: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .add(Duration::from_secs(5))
                .as_secs(),
            idp: Some("the idp".to_string()),
            extra: Default::default(),
            identity: None,
        }
    }

    fn make_authz_token_with_audience(audience: Vec<String>) -> (AuthorizationToken, String) {
        let jwt_claims = mock_authz_token(audience);
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
            jwt_typ: None,
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
            jwt_typ: None,
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
            jwt_typ: None,
        };
        let (original_authz_token, encoded_authz_token) =
            make_authz_token_with_audience(vec!["Lore".to_string()]);

        let verified_authz_token = verifier.verify_token(&encoded_authz_token).await?;
        assert_eq!(original_authz_token, verified_authz_token);

        Ok(())
    }

    fn verifier_with_issuers(issuers: Vec<String>) -> JwtVerifier {
        let mut service = MockTestJWKService::new();
        service.expect_get_key().returning(|_| {
            Ok((
                DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                AGREED_UPON_ALGORITHM,
            ))
        });

        JwtVerifier {
            jwk_service: Arc::new(service),
            jwt_issuer: Some(issuers),
            jwt_audience: Some(vec!["Lore".to_string()]),
            jwt_typ: None,
        }
    }

    fn token_with_issuer(issuer: &str) -> String {
        let mut claims = mock_authz_token(vec!["Lore".to_string()]);
        claims.issuer = issuer.to_string();
        encode_jwt(&claims)
    }

    /// The test that makes an issuer's `iss` cutover a rollout rather than
    /// an outage: with the old keyword and the new URL both configured,
    /// tokens carrying either verify.
    #[tokio::test]
    async fn either_of_two_configured_issuers_verifies() -> Result<(), Box<dyn Error>> {
        let verifier = verifier_with_issuers(vec![
            "LEGACY_AUTH_KEYWORD".to_string(),
            "https://auth.example.com/realms/lore".to_string(),
        ]);

        verifier
            .verify_token(&token_with_issuer("LEGACY_AUTH_KEYWORD"))
            .await?;
        verifier
            .verify_token(&token_with_issuer("https://auth.example.com/realms/lore"))
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn an_issuer_in_neither_entry_is_refused() {
        let verifier = verifier_with_issuers(vec![
            "LEGACY_AUTH_KEYWORD".to_string(),
            "https://auth.example.com/realms/lore".to_string(),
        ]);

        let error = verifier
            .verify_token(&token_with_issuer("https://attacker.example.com"))
            .await
            .expect_err("an unlisted issuer is refused");
        assert!(matches!(error, JwtVerifierError::ValidationFailed(_)));
    }

    #[tokio::test]
    async fn a_single_configured_issuer_still_verifies() -> Result<(), Box<dyn Error>> {
        let verifier = verifier_with_issuers(vec!["LEGACY_AUTH_KEYWORD".to_string()]);
        verifier
            .verify_token(&token_with_issuer("LEGACY_AUTH_KEYWORD"))
            .await?;
        Ok(())
    }

    /// A Keycloak-shaped token: only the required claims — `iss`, `sub`, `aud`, `exp`,
    /// `iat` — no `name`, no `env`, no `idp`. Everything else must default, not
    /// fail the decode.
    #[tokio::test]
    async fn verify_token_with_only_required_claims() -> Result<(), Box<dyn Error>> {
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
            jwt_audience: Some(vec!["Lore".to_string()]),
            jwt_typ: None,
        };

        let minimal_claims = json!({
            "iss": "https://keycloak.example.com/realms/lore",
            "sub": "f7d3a1c2-0000-0000-0000-000000000000",
            "aud": "Lore",
            "iat": 1,
            "exp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .add(Duration::from_secs(5))
                .as_secs(),
        });
        let encoded = encode_jwt(&minimal_claims);

        let verified = verifier.verify_token(&encoded).await?;
        assert_eq!(verified.user_id, "f7d3a1c2-0000-0000-0000-000000000000");
        assert_eq!(verified.name, None);
        assert_eq!(verified.env, None);
        assert_eq!(verified.idp, None);
        assert_eq!(verified.client_id, None);

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
            jwt_typ: None,
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
            jwt_typ: None,
        };

        let (original_token, encoded_token) =
            make_authz_token_with_audience(vec!["urc.example.com".to_string(), "Lore".to_string()]);

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
            jwt_typ: None,
        };

        let (_, encoded_token) = make_authz_token_with_audience(vec!["Lore".to_string()]);

        let verify_error = verifier.verify_token(&encoded_token).await.unwrap_err();
        assert!(matches!(
            verify_error,
            JwtVerifierError::ValidationFailed(_)
        ));

        Ok(())
    }

    mod identity {
        use super::*;

        fn verifier() -> JwtVerifier {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().returning(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });
            JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
                jwt_typ: None,
            }
        }

        /// `mock_authz_token` carries `sub = "the u"`; `extra` adds the
        /// claims a test is about.
        fn token_with(extra: &[(&str, serde_json::Value)]) -> String {
            let mut claims = mock_authz_token(vec!["Lore".to_string()]);
            for (claim, value) in extra {
                claims.extra.insert((*claim).to_string(), value.clone());
            }
            encode_jwt(&claims)
        }

        #[tokio::test]
        async fn without_the_claim_the_subject_is_the_identity() {
            let verified = verifier()
                .verify_token(&token_with(&[]))
                .await
                .expect("verifies");
            assert_eq!(verified.identity, None);
            assert_eq!(verified.identity(), "the u");
        }

        #[tokio::test]
        async fn the_identity_claim_is_recorded_beside_the_subject() {
            let verified = verifier()
                .verify_token(&token_with(&[(IDENTITY_CLAIM, json!("stable-oid"))]))
                .await
                .expect("verifies");
            assert_eq!(verified.identity(), "stable-oid");
            assert_eq!(verified.user_id, "the u");
            assert_eq!(verified.claim_at("sub"), Some(json!("the u")));
        }

        #[tokio::test]
        async fn the_namespaced_alias_is_read_when_the_plain_claim_is_absent() {
            let verified = verifier()
                .verify_token(&token_with(&[(
                    IDENTITY_CLAIM_NAMESPACED,
                    json!("alias-oid"),
                )]))
                .await
                .expect("verifies");
            assert_eq!(verified.identity(), "alias-oid");
        }

        /// The first name present decides, whatever the alias says.
        #[tokio::test]
        async fn the_plain_claim_wins_over_the_alias() {
            let verified = verifier()
                .verify_token(&token_with(&[
                    (IDENTITY_CLAIM, json!("plain")),
                    (IDENTITY_CLAIM_NAMESPACED, json!("alias")),
                ]))
                .await
                .expect("verifies");
            assert_eq!(verified.identity(), "plain");
        }

        /// An empty or non-string value at the deciding claim refuses the
        /// token; the rule does not fall through to a valid alias or `sub`.
        #[tokio::test]
        async fn an_empty_or_non_string_identity_is_refused() {
            for (value, beside) in [
                (json!(""), vec![]),
                (json!(""), vec![(IDENTITY_CLAIM_NAMESPACED, json!("alias"))]),
                (json!(7), vec![]),
                (json!({"id": "x"}), vec![]),
            ] {
                let mut extra = vec![(IDENTITY_CLAIM, value.clone())];
                extra.extend(beside);
                let error = verifier()
                    .verify_token(&token_with(&extra))
                    .await
                    .expect_err("no identity to attribute the caller by");
                assert!(
                    matches!(error, JwtVerifierError::IdentityClaimMissing { ref claim } if claim == IDENTITY_CLAIM),
                    "{value}: {error:?}"
                );
            }
        }

        #[tokio::test]
        async fn an_empty_alias_is_refused_too() {
            let error = verifier()
                .verify_token(&token_with(&[(IDENTITY_CLAIM_NAMESPACED, json!(""))]))
                .await
                .expect_err("an empty alias names no one");
            assert!(matches!(
                error,
                JwtVerifierError::IdentityClaimMissing { ref claim } if claim == IDENTITY_CLAIM_NAMESPACED
            ));
        }

        #[tokio::test]
        async fn an_empty_subject_is_refused() {
            let mut claims = mock_authz_token(vec!["Lore".to_string()]);
            claims.user_id = String::new();
            let error = verifier()
                .verify_token(&encode_jwt(&claims))
                .await
                .expect_err("an empty subject names no one");
            assert!(matches!(
                error,
                JwtVerifierError::IdentityClaimMissing { ref claim } if claim == "sub"
            ));
        }

        /// The cached path resolves the identity as the async path does.
        #[test]
        fn the_cached_path_resolves_the_identity_too() {
            let mut service = MockTestJWKService::new();
            service.expect_get_cached_key().returning(|_| {
                Some((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });
            let verifier = JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
                jwt_typ: None,
            };
            let verified = verifier
                .try_verify_token_cached(&token_with(&[(IDENTITY_CLAIM, json!("stable-oid"))]))
                .expect("verifies")
                .expect("the cache answers");
            assert_eq!(verified.identity(), "stable-oid");
        }
    }

    mod jwt_typ {
        use super::*;

        fn accepted(types: &[&str]) -> Option<Vec<String>> {
            Some(types.iter().map(ToString::to_string).collect())
        }

        fn verifier_with_typ(jwt_typ: Option<Vec<String>>) -> JwtVerifier {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().returning(|_| {
                Ok((
                    DecodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
                    AGREED_UPON_ALGORITHM,
                ))
            });
            JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
                jwt_typ,
            }
        }

        /// A verifier whose key service must never be asked, on either path.
        fn verifier_expecting_no_key_lookup(jwt_typ: Option<Vec<String>>) -> JwtVerifier {
            let mut service = MockTestJWKService::new();
            service.expect_get_key().times(0);
            service.expect_get_cached_key().times(0);
            service.expect_refresh_key().times(0);
            JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
                jwt_typ,
            }
        }

        /// `Header::new` sets `typ: "JWT"`; `None` here clears it, as a
        /// header with no `typ` at all.
        fn encode_jwt_with_typ(typ: Option<&str>) -> String {
            let mut header = Header::new(AGREED_UPON_ALGORITHM);
            header.kid = Some("the kid".into());
            header.typ = typ.map(str::to_string);
            encode(
                &header,
                &mock_authz_token(vec!["Lore".to_string()]),
                &EncodingKey::from_secret(AGREED_UPON_SIGNING_SECRET.as_ref()),
            )
            .unwrap()
        }

        /// Off by default: the legacy auth service's tokens carry `typ: JWT`,
        /// and a token with no `typ` is a valid JWS.
        #[tokio::test]
        async fn unset_accepts_any_typ_and_none() {
            let verifier = verifier_with_typ(None);
            for typ in [None, Some("JWT"), Some("at+jwt")] {
                verifier
                    .verify_token(&encode_jwt_with_typ(typ))
                    .await
                    .unwrap_or_else(|error| panic!("{typ:?} must verify: {error}"));
            }
        }

        /// The RFC 9068 §4 rule: no `typ` and `typ: JWT` are refused,
        /// `at+jwt` is accepted.
        #[tokio::test]
        async fn at_jwt_refuses_a_missing_or_plain_jwt_typ_without_a_key_lookup() {
            let verifier = verifier_expecting_no_key_lookup(accepted(&["at+jwt"]));
            for typ in [None, Some("JWT")] {
                let Err(error) = verifier.verify_token(&encode_jwt_with_typ(typ)).await else {
                    panic!("{typ:?} must be refused");
                };
                assert!(matches!(error, JwtVerifierError::TypNotAccepted));
            }
        }

        #[tokio::test]
        async fn at_jwt_verifies_under_the_rfc_9068_rule() {
            verifier_with_typ(accepted(&["at+jwt"]))
                .verify_token(&encode_jwt_with_typ(Some("at+jwt")))
                .await
                .expect("at+jwt verifies");
        }

        /// RFC 7515 §4.1.9: `typ` is a media type, so the `application/`
        /// prefix is optional on either side and case does not matter.
        #[tokio::test]
        async fn typ_compares_as_a_media_type() {
            let bare = verifier_with_typ(accepted(&["at+jwt"]));
            for typ in ["application/at+jwt", "AT+JWT", "Application/AT+JWT"] {
                bare.verify_token(&encode_jwt_with_typ(Some(typ)))
                    .await
                    .unwrap_or_else(|error| panic!("{typ} must verify: {error}"));
            }
            verifier_with_typ(accepted(&["application/at+jwt"]))
                .verify_token(&encode_jwt_with_typ(Some("at+jwt")))
                .await
                .expect("a prefixed configuration accepts the bare header");
        }

        /// Case and the prefix are the only tolerated variations. A header
        /// padded with whitespace names no media type and is refused, so
        /// this verifier agrees with a consumer reading `typ` strictly.
        #[tokio::test]
        async fn whitespace_around_the_typ_is_not_forgiven() {
            let verifier = verifier_expecting_no_key_lookup(accepted(&["at+jwt"]));
            for typ in [" at+jwt", "at+jwt ", " at+jwt ", "at+jwt\t"] {
                let Err(error) = verifier.verify_token(&encode_jwt_with_typ(Some(typ))).await
                else {
                    panic!("{typ:?} must be refused");
                };
                assert!(matches!(error, JwtVerifierError::TypNotAccepted));
            }
        }

        /// A provider with its own convention lists what it emits, and the
        /// list is exhaustive: anything else is refused.
        #[tokio::test]
        async fn a_listed_convention_is_accepted_and_nothing_else() {
            let verifier = verifier_with_typ(accepted(&["JWT", "at+jwt"]));
            for typ in ["JWT", "at+jwt"] {
                verifier
                    .verify_token(&encode_jwt_with_typ(Some(typ)))
                    .await
                    .unwrap_or_else(|error| panic!("{typ} must verify: {error}"));
            }
            let error = verifier_expecting_no_key_lookup(accepted(&["JWT", "at+jwt"]))
                .verify_token(&encode_jwt_with_typ(Some("id+jwt")))
                .await
                .expect_err("an unlisted type is refused");
            assert!(matches!(error, JwtVerifierError::TypNotAccepted));
        }

        /// The interceptor's synchronous path refuses outright rather than
        /// deferring: no key could change the verdict.
        #[test]
        fn the_cached_path_refuses_the_typ_too() {
            let error = verifier_expecting_no_key_lookup(accepted(&["at+jwt"]))
                .try_verify_token_cached(&encode_jwt_with_typ(Some("JWT")))
                .expect_err("refused, not deferred");
            assert!(matches!(error, JwtVerifierError::TypNotAccepted));
        }
    }
}
