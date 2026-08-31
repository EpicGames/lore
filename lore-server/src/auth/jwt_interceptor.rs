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
use super::jwt::verify_authorization;
use crate::auth::jwt::AuthorizationToken;
use crate::grpc::get_repository;

fn add_auth_fields_to_current_span(auth: &AuthorizationToken) {
    let span = Span::current();
    span.record(USER_ID, auth.user_id.clone());
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

#[derive(Clone)]
pub struct JWTInterceptor {
    jwt_verifier: JwtVerifier,
}

impl JWTInterceptor {
    pub fn new(jwt_verifier: &JwtVerifier) -> Self {
        Self {
            jwt_verifier: jwt_verifier.clone(),
        }
    }
}

impl Interceptor for JWTInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        let token = extract_bearer_token(request.metadata()).ok_or(
            tonic::Status::unauthenticated("authorization header required"),
        )?;

        let authorization = authorize(&self.jwt_verifier, &token)?;
        add_auth_fields_to_current_span(&authorization);

        let repository = get_repository(request.metadata()).unwrap_or_default();
        verify_authorization(&authorization, repository)
            .map_err(|_err| crate::grpc::no_repository_access_status())?;

        request.extensions_mut().insert(authorization);

        Ok(request)
    }
}

#[derive(Clone)]
pub struct JWTAuthnInterceptor {
    jwt_verifier: JwtVerifier,
}

impl JWTAuthnInterceptor {
    pub fn new(jwt_verifier: &JwtVerifier) -> Self {
        Self {
            jwt_verifier: jwt_verifier.clone(),
        }
    }
}

impl Interceptor for JWTAuthnInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        let token = extract_bearer_token(request.metadata()).ok_or(
            tonic::Status::unauthenticated("authorization header required"),
        )?;

        // TODO(UCS-13506): Placeholder authn verifier until separate authz flow for repository service is in place
        let authorization = authorize(&self.jwt_verifier, &token)?;
        add_auth_fields_to_current_span(&authorization);

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
