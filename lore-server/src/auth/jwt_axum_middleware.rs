// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use axum::body::Body;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::Extensions;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use lore_base::types::Context;
use lore_revision::lore::RepositoryId;
use lore_telemetry::tracing::fields::USER_ID;
use serde::Deserialize;
use tracing::Span;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::TokenVerification;
use crate::auth::jwt::decode_unsigned_identity;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::PartitionGrants;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::authnz::repository_authorizer::permits_with;
use crate::http::server::ServerState;

#[derive(Deserialize)]
pub struct Params {
    repository_id: String,
}

pub async fn jwt_axum_verify_authorization(
    State(state): State<ServerState>,
    Path(params): Path<Params>,
    mut request: Request,
    next: Next,
) -> Response {
    let jwt_verifier = match state.token_verification {
        TokenVerification::Verify(jwt_verifier) => jwt_verifier,
        TokenVerification::TrustUnsigned => {
            let claims = match extract_bearer_token(&request)
                .as_deref()
                .map(decode_unsigned_identity)
            {
                None => None,
                Some(Ok(claims)) => Some(claims),
                Some(Err(_)) => return unauthorized(),
            };
            if let Some(claims) = &claims {
                Span::current().record(USER_ID, claims.identity());
            }
            request.extensions_mut().insert(claims);
            return next.run(request).await;
        }
        TokenVerification::Ignore => {
            let no_user_info: Option<AuthorizationToken> = None;
            request.extensions_mut().insert(no_user_info);
            return next.run(request).await;
        }
    };
    if let Some(accesstoken) = extract_bearer_token(&request) {
        if let Ok(user_info) = jwt_verifier.verify_token(&accesstoken).await {
            let repository: lore_revision::lore::RepositoryId =
                Context::from_str(params.repository_id.as_str())
                    .unwrap_or_default()
                    .into();
            let token = VerifiedToken {
                raw: &accesstoken,
                claims: &user_info,
            };
            if let Ok(grants) = state
                .repository_authorizer
                .granted_access(Some(&token), repository)
                .await
            {
                Span::current().record(USER_ID, user_info.identity());
                if let Some(grants) = grants {
                    request.extensions_mut().insert(PartitionGrants {
                        repository_id: repository,
                        grants,
                    });
                }
                // Set `user_info` as a request extension so it can be used down the stack
                request.extensions_mut().insert(RawToken(accesstoken));
                request.extensions_mut().insert(Some(user_info));

                return next.run(request).await;
            }
        }
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .unwrap()
    } else {
        unauthorized()
    }
}

fn unauthorized() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(Body::empty())
        .unwrap()
}

fn extract_bearer_token(request: &Request) -> Option<String> {
    request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| {
            if header.starts_with("Bearer ") {
                Some(header.trim_start_matches("Bearer ").to_string())
            } else {
                None
            }
        })
}

/// [`permits_with`] over what [`jwt_axum_verify_authorization`] inserts into
/// the request extensions, where the claims sit as `Option<AuthorizationToken>`.
pub async fn http_permits(
    authorizer: &dyn RepositoryAuthorizer,
    extensions: &Extensions,
    repository: RepositoryId,
    action: Action,
) -> bool {
    let grants = extensions
        .get::<PartitionGrants>()
        .filter(|grants| grants.repository_id == repository);
    let claims = extensions
        .get::<Option<AuthorizationToken>>()
        .and_then(Option::as_ref);
    let token = match (extensions.get::<RawToken>(), claims) {
        (Some(raw), Some(claims)) => Some(VerifiedToken {
            raw: &raw.0,
            claims,
        }),
        _ => None,
    };
    permits_with(
        authorizer,
        grants.map(|grants| &grants.grants),
        token.as_ref(),
        repository,
        action,
    )
    .await
}
