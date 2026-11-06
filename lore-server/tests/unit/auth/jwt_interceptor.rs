// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use jsonwebtoken::Algorithm;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use jsonwebtoken::encode;
use lore_base::types::Context;
use lore_revision::lore::RepositoryId;
use lore_server::auth::jwk::JWKService;
use lore_server::auth::jwk::JWKServiceError;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::JwtVerifier;
use lore_server::auth::jwt::TokenVerification;
use lore_server::auth::jwt_interceptor::*;
use lore_server::authnz::repository_authorizer::RawToken;
use serde_json::json;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;

const SIGNING_SECRET: &str = "the-secret";

/// Serves the signing key from the cache, so [`authorize`] answers on its synchronous
/// path and these tests need no runtime.
#[derive(Debug)]
struct CachedJWKService;

#[async_trait::async_trait]
impl JWKService for CachedJWKService {
    async fn get_key(&self, _kid: &str) -> Result<(DecodingKey, Algorithm), JWKServiceError> {
        Ok(self.get_cached_key("").expect("always cached"))
    }

    fn get_cached_key(&self, _kid: &str) -> Option<(DecodingKey, Algorithm)> {
        Some((
            DecodingKey::from_secret(SIGNING_SECRET.as_ref()),
            Algorithm::HS256,
        ))
    }

    async fn refresh_key(
        &self,
        _kid: &str,
    ) -> Result<Option<(DecodingKey, Algorithm)>, JWKServiceError> {
        Ok(None)
    }
}

fn interceptor() -> JWTInterceptor {
    JWTInterceptor::new(TokenVerification::Verify(JwtVerifier {
        jwk_service: Arc::new(CachedJWKService),
        jwt_issuer: None,
        jwt_audience: Some(vec!["Lore".to_string()]),
        jwt_typ: None,
        exchange_issuer: None,
    }))
}

fn encode_token(claims: &serde_json::Value) -> String {
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("the kid".into());
    encode(
        &header,
        claims,
        &EncodingKey::from_secret(SIGNING_SECRET.as_ref()),
    )
    .unwrap()
}

/// A verifiable token carrying no resource claim, so it holds a grant for no
/// partition — the shape a Tier 1 provider mints.
fn grantless_token() -> String {
    encode_token(&json!({
        "iss": "the issuer",
        "sub": "the u",
        "aud": "Lore",
        "iat": 1,
        "exp": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .checked_add(Duration::from_secs(60))
            .unwrap()
            .as_secs(),
    }))
}

fn request_with(token: &str, partition: Option<RepositoryId>) -> tonic::Request<()> {
    let mut request = tonic::Request::new(());
    request.metadata_mut().insert(
        "authorization",
        MetadataValue::try_from(format!("Bearer {token}")).unwrap(),
    );
    if let Some(repository) = partition {
        request.metadata_mut().append_bin(
            lore_transport::grpc::PARTITION_ID_KEY,
            MetadataValue::from_bytes(repository.data()),
        );
    }
    request
}

/// The interceptor is authentication-only: an authenticated token with no grant for
/// the partition passes through, and what denies it is the configured authorizer
/// asked at the next stage — the `PartitionAccessService` wrapping each
/// partition-scoped service, or the handler itself where the partition arrives in the
/// request body. This assertion is one half of a pair that keeps the two stages from
/// silently both going missing: a partition check here fails this test, and dropping
/// the layer or the handler checks fails their tests and the per-service smoke
/// probes.
#[test]
fn an_authenticated_token_with_no_partition_grant_passes_the_interceptor() {
    let partition: RepositoryId = Context::from_str("0194b726b34e72b0b45550b88a967076")
        .unwrap()
        .into();

    let request = interceptor()
        .call(request_with(&grantless_token(), Some(partition)))
        .expect("authn-only: the partition decision is the authorizer's, downstream");

    // The downstream check rebuilds a `VerifiedToken` from these two
    // extensions, so both halves must ride along.
    assert!(request.extensions().get::<AuthorizationToken>().is_some());
    assert!(request.extensions().get::<RawToken>().is_some());
}

/// The identity every handler records and compares comes out of the extensions the
/// interceptor inserts, so this is where the token's `identity` claim takes effect end to end.
#[test]
fn the_tokens_identity_claim_decides_what_handlers_read_as_the_user_id() {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .checked_add(Duration::from_secs(60))
        .unwrap()
        .as_secs();
    let claims = json!({
        "iss": "the issuer",
        "sub": "f7d3a1c2-0000-0000-0000-000000000000",
        "preferred_username": "alice",
        "aud": "Lore",
        "iat": 1,
        "exp": exp,
    });

    let request = interceptor()
        .call(request_with(&encode_token(&claims), None))
        .expect("verifies");
    assert_eq!(
        lore_server::grpc::get_user_id(request.extensions()),
        "f7d3a1c2-0000-0000-0000-000000000000"
    );

    let mut with_identity = claims.clone();
    with_identity["identity"] = json!("stable-oid");
    let request = interceptor()
        .call(request_with(&encode_token(&with_identity), None))
        .expect("verifies");
    assert_eq!(
        lore_server::grpc::get_user_id(request.extensions()),
        "stable-oid"
    );

    let mut empty_identity = claims;
    empty_identity["identity"] = json!("");
    let status = interceptor()
        .call(request_with(&encode_token(&empty_identity), None))
        .expect_err("an empty identity attributes the caller to no one");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(status.message(), "Not allowed");
}

/// A request carrying no partition metadata needs no claim naming the zero partition
/// id: naming no partition means no partition decision, not a decision about
/// partition zero.
#[test]
fn a_request_with_no_partition_metadata_needs_no_zero_partition_claim() {
    interceptor()
        .call(request_with(&grantless_token(), None))
        .expect("no partition metadata means no partition decision, not partition zero");
}

#[test]
fn a_request_with_no_bearer_token_is_unauthenticated() {
    let status = interceptor()
        .call(tonic::Request::new(()))
        .expect_err("no bearer token");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

/// The no-auth server's registration path: with no verifier the interceptor stands
/// aside. A request without a token passes, and one carrying a token that would
/// never verify passes too, with nothing inserted for a handler to mistake for a
/// verified caller.
#[test]
fn without_a_verifier_every_request_passes_untouched() {
    let mut interceptor = JWTInterceptor::new(TokenVerification::Ignore);

    let request = interceptor
        .call(tonic::Request::new(()))
        .expect("no verifier means no token is required");
    assert!(request.extensions().get::<AuthorizationToken>().is_none());
    assert!(request.extensions().get::<RawToken>().is_none());

    let request = interceptor
        .call(request_with("not.a.jwt", None))
        .expect("no verifier means the token is not read");
    assert!(request.extensions().get::<AuthorizationToken>().is_none());
    assert!(request.extensions().get::<RawToken>().is_none());
}

/// Authentication still lives here: a token that fails on its own claims is refused
/// without reaching any handler, and without saying why.
#[test]
fn a_token_failing_on_its_own_claims_is_refused() {
    let expired = encode_token(&json!({
        "iss": "the issuer",
        "sub": "the u",
        "aud": "Lore",
        "iat": 1,
        // Well past `Validation`'s default 60-second leeway.
        "exp": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600,
    }));

    let status = interceptor()
        .call(request_with(&expired, None))
        .expect_err("an expired token is denied");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(status.message(), "Not allowed");
}

#[test]
fn trusting_unsigned_identity_names_the_caller_without_a_verified_token() {
    let token = lore_credential::unsigned_identity_token("alice").expect("encodes");

    let request = JWTInterceptor::new(TokenVerification::TrustUnsigned)
        .call(request_with(&token, None))
        .expect("an unsigned token is accepted");

    let claims = request
        .extensions()
        .get::<AuthorizationToken>()
        .expect("the claims name the caller");
    assert_eq!(claims.identity(), "alice");
    assert!(
        request.extensions().get::<RawToken>().is_none(),
        "without a raw token, authorization sees no verified token"
    );
}

#[test]
fn trusting_unsigned_identity_takes_the_identity_claim_over_sub() {
    let token = encode_token(&json!({
        "iss": "anyone",
        "sub": "service-account-1",
        "identity": "bob",
        "aud": "anything",
        "iat": 1,
        "exp": 1,
    }));

    let request = JWTInterceptor::new(TokenVerification::TrustUnsigned)
        .call(request_with(&token, None))
        .expect("signature, audience and expiry are not checked");

    let claims = request.extensions().get::<AuthorizationToken>().unwrap();
    assert_eq!(claims.identity(), "bob");
}

#[test]
fn trusting_unsigned_identity_passes_a_request_without_a_token() {
    let request = JWTInterceptor::new(TokenVerification::TrustUnsigned)
        .call(tonic::Request::new(()))
        .expect("a token is not required");
    assert!(request.extensions().get::<AuthorizationToken>().is_none());
}

#[test]
fn trusting_unsigned_identity_refuses_a_malformed_token() {
    let nameless = encode_token(&json!({
        "iss": "anyone",
        "sub": "",
        "aud": "anything",
        "iat": 1,
        "exp": 1,
    }));
    for token in ["not.a.jwt", nameless.as_str()] {
        let status = JWTInterceptor::new(TokenVerification::TrustUnsigned)
            .call(request_with(token, None))
            .expect_err("a token that names no one is refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }
}

#[test]
fn a_verifier_refuses_an_unsigned_identity_token() {
    let token = lore_credential::unsigned_identity_token("alice").expect("encodes");

    let status = interceptor()
        .call(request_with(&token, None))
        .expect_err("an unsigned token never verifies");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
}

#[test]
fn token_verification_refuses_a_verifier_with_unsigned_identity() {
    let verifier = JwtVerifier {
        jwk_service: Arc::new(CachedJWKService),
        jwt_issuer: None,
        jwt_audience: None,
        jwt_typ: None,
        exchange_issuer: None,
    };
    assert!(TokenVerification::new(Some(verifier), true).is_err());
}
