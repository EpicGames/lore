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
use crate::auth::jwt::TokenVerification;
use crate::auth::jwt::decode_unsigned_identity;
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
/// Under [`TokenVerification::TrustUnsigned`] it inserts the claims of an unsigned token
/// and no [`RawToken`]. The claims then name the caller, while authorization still sees a
/// request without a verified token.
#[derive(Clone)]
pub struct JWTInterceptor {
    token_verification: TokenVerification,
}

impl JWTInterceptor {
    pub fn new(token_verification: TokenVerification) -> Self {
        Self { token_verification }
    }
}

impl Interceptor for JWTInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        match &self.token_verification {
            TokenVerification::Ignore => Ok(request),
            TokenVerification::TrustUnsigned => {
                let Some(token) = extract_bearer_token(request.metadata()) else {
                    return Ok(request);
                };
                let claims = decode_unsigned_identity(&token).map_err(|e| {
                    debug!(error = ?e, "Rejecting request: unsigned token is malformed");
                    tonic::Status::unauthenticated("Malformed bearer token")
                })?;
                add_auth_fields_to_current_span(&claims);
                request.extensions_mut().insert(claims);
                Ok(request)
            }
            TokenVerification::Verify(jwt_verifier) => {
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
