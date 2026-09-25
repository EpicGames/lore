// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use anyhow::Result;
use lore_base::runtime::runtime;
use lore_telemetry::tracing::fields::USER_ID;
use tokio::task;
use tonic::service::Interceptor;
use tracing::Span;
use tracing::debug;

use super::jwt::JwtVerifier;
use crate::auth::jwt::AuthorizationToken;
use crate::authnz::repository_authorizer::RawToken;

fn add_auth_fields_to_current_span(auth: &AuthorizationToken) {
    let span = Span::current();
    span.record(USER_ID, auth.identity().to_string());
}

/// Resolve the bearer token to an [`AuthorizationToken`]. The cached signing key serves the
/// hot path synchronously; the blocking fallback runs only when the cache cannot answer —
/// no key for the id, or a key that rejected the signature and may therefore have been
/// rotated out. A token that fails on its own claims is refused without blocking. That is
/// what lets this run inside tonic's synchronous [`Interceptor::call`].
fn authorize(verifier: &JwtVerifier, token: &str) -> Result<AuthorizationToken, tonic::Status> {
    match verifier.try_verify_token_cached(token) {
        Ok(Some(authorization)) => Ok(authorization),
        // Reached only when the cache cannot answer, so the core handed off here is one the
        // hot path never gives up.
        #[allow(clippy::disallowed_methods)]
        Ok(None) => task::block_in_place(|| runtime().block_on(verifier.verify_token(token))),
        Err(e) => Err(e),
    }
    .map_err(|e| {
        // The reason stays in the log. Told apart, "the signature is wrong", "the token
        // expired", "no such key id" and "the JWKS endpoint is unwell" are an oracle for a
        // caller who has not authenticated — and not one of them is something that caller
        // could act on.
        debug!(error = ?e, "Rejecting request: token verification failed");
        tonic::Status::permission_denied("Not allowed")
    })
}

/// Authentication only: verifies the signature, parses the bearer token claims, and hands
/// the verified halves to the handler as extensions ([`RawToken`] for the raw token
/// string, and [`AuthorizationToken`] for the parsed claims).
///
/// With no verifier (no `[server.auth]`), every request passes through untouched: no
/// bearer token is required, one that is present is not read, and no extension is
/// inserted.
#[derive(Clone)]
pub struct JWTInterceptor {
    jwt_verifier: Option<JwtVerifier>,
}

impl JWTInterceptor {
    pub fn new(jwt_verifier: Option<&JwtVerifier>) -> Self {
        Self {
            jwt_verifier: jwt_verifier.cloned(),
        }
    }
}

impl Interceptor for JWTInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        let Some(jwt_verifier) = &self.jwt_verifier else {
            return Ok(request);
        };

        let token = extract_bearer_token(request.metadata()).ok_or(
            tonic::Status::unauthenticated("authorization header required"),
        )?;

        let authorization = authorize(jwt_verifier, &token)?;
        add_auth_fields_to_current_span(&authorization);

        request.extensions_mut().insert(RawToken(token));
        request.extensions_mut().insert(authorization);

        Ok(request)
    }
}

pub(crate) fn extract_bearer_token(metadata: &tonic::metadata::MetadataMap) -> Option<String> {
    metadata
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|header| {
            if header.starts_with("Bearer ") {
                Some(header.trim_start_matches("Bearer ").to_string())
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
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
    use serde_json::json;
    use tonic::metadata::MetadataValue;

    use super::*;
    use crate::auth::jwk::JWKService;
    use crate::auth::jwk::JWKServiceError;
    use crate::auth::jwt::DEFAULT_IDENTITY_CLAIM;

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
        interceptor_with_identity_claim(DEFAULT_IDENTITY_CLAIM)
    }

    fn interceptor_with_identity_claim(identity_claim: &str) -> JWTInterceptor {
        JWTInterceptor::new(Some(&JwtVerifier {
            jwk_service: Arc::new(CachedJWKService),
            jwt_issuer: None,
            jwt_audience: Some(vec!["Lore".to_string()]),
            jwt_typ: None,
            identity_claim: identity_claim.to_string(),
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
    /// interceptor inserts, so this is where `identity_claim` takes effect end to end.
    #[test]
    fn the_identity_claim_decides_what_handlers_read_as_the_user_id() {
        let token = encode_token(&json!({
            "iss": "the issuer",
            "sub": "f7d3a1c2-0000-0000-0000-000000000000",
            "preferred_username": "alice",
            "aud": "Lore",
            "iat": 1,
            "exp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .checked_add(Duration::from_secs(60))
                .unwrap()
                .as_secs(),
        }));

        let request = interceptor()
            .call(request_with(&token, None))
            .expect("verifies");
        assert_eq!(
            crate::grpc::get_user_id(request.extensions()),
            "f7d3a1c2-0000-0000-0000-000000000000"
        );

        let request = interceptor_with_identity_claim("preferred_username")
            .call(request_with(&token, None))
            .expect("verifies");
        assert_eq!(crate::grpc::get_user_id(request.extensions()), "alice");

        let status = interceptor_with_identity_claim("oid")
            .call(request_with(&token, None))
            .expect_err("no `oid` claim to attribute the caller by");
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
        let mut interceptor = JWTInterceptor::new(None);

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
}
