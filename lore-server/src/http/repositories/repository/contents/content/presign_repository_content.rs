// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::SystemTime;
use std::time::SystemTimeError;
use std::time::UNIX_EPOCH;

use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::extract::State;
use axum::http::Extensions;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use hex::FromHexError;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_revision::lore::RepositoryId;
use lore_storage::StoreMatch;
use lore_storage::immutable_store::query_one;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt_axum_middleware::http_permits;
use crate::authnz::repository_authorizer::Action;
use crate::http::log_http_error;
use crate::http::presign_token::CURRENT_TOKEN_VERSION;
use crate::http::presign_token::PresignTokenPayload;
use crate::http::presign_token::sign;
use crate::http::server::ServerState;
use crate::util::get_user_id_from_token;
use crate::util::setup_execution;

#[derive(Debug, Error)]
pub enum PresignError {
    #[error("Failed to parse repository: {0}")]
    ParseRepository(FromHexError),
    #[error("Failed to parse address: {0}")]
    ParseAddress(FromHexError),
    #[error("Presign feature is not configured")]
    NotConfigured,
    #[error("Presign not permitted on this repository")]
    NotPermitted,
    #[error("content_type is not allowed: {0}")]
    DisallowedContentType(String),
    #[error("header value is not valid: {0}")]
    InvalidHeaderValue(String),
    #[error("Content not found")]
    NotFound,
    #[error("Store error checking content existence")]
    StoreError,
    #[error("System clock error: {0}")]
    SystemTime(SystemTimeError),
}

impl IntoResponse for PresignError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match &self {
            PresignError::ParseRepository(_)
            | PresignError::ParseAddress(_)
            | PresignError::DisallowedContentType(_)
            | PresignError::InvalidHeaderValue(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            PresignError::NotConfigured => (
                StatusCode::NOT_FOUND,
                "presigned URL feature is not enabled".to_string(),
            ),
            PresignError::NotPermitted => (
                StatusCode::FORBIDDEN,
                "presign not permitted on this repository".to_string(),
            ),
            PresignError::StoreError | PresignError::SystemTime(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong. See server log for more info.".to_string(),
            ),
            PresignError::NotFound => (StatusCode::NOT_FOUND, "address not found".to_string()),
        };

        log_http_error(&self, status);

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());
        (status, headers, msg).into_response()
    }
}

#[derive(Deserialize)]
pub struct PresignRequest {
    pub ttl_seconds: Option<u64>,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub content_disposition: Option<String>,
}

#[derive(Serialize)]
pub struct PresignResponse {
    pub url_suffix: String,
    pub expires_at: u64,
}

pub async fn handler(
    State(state): State<Arc<ServerState>>,
    Path((repository_id, address)): Path<(String, String)>,
    Extension(user_info): Extension<Option<AuthorizationToken>>,
    extensions: Extensions,
    headers: HeaderMap,
    Json(body): Json<PresignRequest>,
) -> Result<impl IntoResponse, PresignError> {
    let presign_config = state
        .presign_config
        .as_ref()
        .ok_or(PresignError::NotConfigured)?
        .clone();

    let repository = repository_id
        .parse::<RepositoryId>()
        .map_err(PresignError::ParseRepository)?;
    if !http_permits(
        state.repository_authorizer.as_ref(),
        &extensions,
        repository,
        Action::Presign,
    )
    .await
    {
        return Err(PresignError::NotPermitted);
    }
    let parsed_address = address
        .parse::<Address>()
        .map_err(PresignError::ParseAddress)?;

    // Fast-feedback rejection; redeem also enforces the allowlist for
    // already issued tokens.
    if let Some(content_type) = body.content_type.as_deref()
        && !presign_config
            .content_type_allowlist
            .is_allowed(content_type)
    {
        return Err(PresignError::DisallowedContentType(
            content_type.to_string(),
        ));
    }

    // Reject values redeem could not serialize into a response header, so a
    // token mint accepts is always one redeem can serve.
    for value in [
        &body.content_type,
        &body.content_encoding,
        &body.content_disposition,
    ]
    .into_iter()
    .flatten()
    {
        HeaderValue::from_str(value)
            .map_err(|_err| PresignError::InvalidHeaderValue(value.clone()))?;
    }

    let correlation_id = headers
        .get(CORRELATION_ID_HEADER)
        .and_then(|v| v.to_str().map(str::to_string).ok())
        .unwrap_or_default();
    let execution = setup_execution(
        module_path!(),
        correlation_id,
        get_user_id_from_token(user_info),
    );

    let immutable_store = state.immutable_store.clone();

    LORE_CONTEXT
        .scope(execution, async move {
            // Verify the address exists before issuing a URL for it.
            let match_result = query_one(&immutable_store, repository, parsed_address)
                .await
                .map_err(|e| {
                    warn!(%e, "Presign resolve check failed");
                    PresignError::StoreError
                })?;

            if match_result.match_made != StoreMatch::MatchFull {
                return Err(PresignError::NotFound);
            }

            // Clamp TTL to configured bounds.
            let ttl = body
                .ttl_seconds
                .unwrap_or(presign_config.default_ttl_seconds);
            let ttl = ttl.clamp(
                presign_config.min_ttl_seconds,
                presign_config.max_ttl_seconds,
            );

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(PresignError::SystemTime)?
                .as_secs();
            let expires_at = now + ttl;

            let payload = PresignTokenPayload {
                version: CURRENT_TOKEN_VERSION,
                key_id: presign_config.key_id.clone(),
                repository: repository_id.clone(),
                address: address.clone(),
                expires_at,
                content_type: body.content_type,
                content_encoding: body.content_encoding,
                content_disposition: body.content_disposition,
            };

            let token_str = sign(&payload, &presign_config.hmac_key);

            let url_suffix = format!("/v1/presigned/{repository_id}/{address}?token={token_str}");

            Ok((
                StatusCode::OK,
                Json(PresignResponse {
                    url_suffix,
                    expires_at,
                }),
            ))
        })
        .await
}
