// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CONTENT_DISPOSITION;
use axum::http::header::CONTENT_ENCODING;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::InvalidHeaderValue;
use axum::response::IntoResponse;
use hex::FromHexError;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_revision::immutable;
use lore_revision::immutable::ImmutableError;
use lore_revision::immutable::read_options_from_repository;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use reqwest::header::CONTENT_LENGTH;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc::channel;
use tokio_stream::wrappers::ReceiverStream;

use crate::http::log_http_error;
use crate::http::presign_token::PresignTokenError;
use crate::http::presign_token::verify;
use crate::http::range::RangeRequest;
use crate::http::range::advertise_ranges;
use crate::http::range::content_range;
use crate::http::range::range_request;
use crate::http::range::set_content_range;
use crate::http::range::unsatisfied_range;
use crate::http::security_headers::apply_security_headers;
use crate::http::server::ServerState;
use crate::util::setup_execution;

const CHUNKED_RESPONSE_BUFFER_SIZE: usize = 16;

#[derive(Debug, Error)]
pub enum RedeemError {
    #[error("Presign feature is not configured")]
    NotConfigured,
    #[error("Failed to parse repository: {0}")]
    ParseRepository(FromHexError),
    #[error("Failed to parse address: {0}")]
    ParseAddress(FromHexError),
    #[error("Invalid presign token: {0}")]
    InvalidToken(PresignTokenError),
    #[error("Token is not valid for the requested resource")]
    TokenMismatch,
    #[error("Failed to read content: {0}")]
    ReadStream(ImmutableError),
    #[error("Failed to generate response headers: {0}")]
    HeaderGeneration(InvalidHeaderValue),
}

impl IntoResponse for RedeemError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match &self {
            RedeemError::ParseRepository(_) | RedeemError::ParseAddress(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            RedeemError::InvalidToken(_) | RedeemError::TokenMismatch => (
                StatusCode::UNAUTHORIZED,
                "invalid or expired token".to_string(),
            ),
            RedeemError::ReadStream(e) if e.is_address_not_found() || e.is_payload_not_found() => {
                (StatusCode::NOT_FOUND, "address not found".to_string())
            }
            RedeemError::NotConfigured => (
                StatusCode::NOT_FOUND,
                "presigned URL feature is not enabled".to_string(),
            ),
            RedeemError::ReadStream(_) | RedeemError::HeaderGeneration(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong. See server log for more info.".to_string(),
            ),
        };

        log_http_error(&self, status);

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());
        (status, headers, msg).into_response()
    }
}

#[derive(Deserialize)]
pub struct RedeemQuery {
    pub token: String,
}

pub async fn handler(
    State(state): State<Arc<ServerState>>,
    Path((repository_id, address)): Path<(String, String)>,
    Query(query): Query<RedeemQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, RedeemError> {
    let presign_config = state
        .presign_config
        .as_ref()
        .ok_or(RedeemError::NotConfigured)?;

    let parsed_repository = repository_id
        .parse::<RepositoryId>()
        .map_err(RedeemError::ParseRepository)?;
    let parsed_address = address
        .parse::<Address>()
        .map_err(RedeemError::ParseAddress)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let payload = verify(
        &query.token,
        &presign_config.hmac_key,
        &presign_config.key_id,
        now,
    )
    .map_err(RedeemError::InvalidToken)?;

    if payload.repository != repository_id || payload.address != address {
        return Err(RedeemError::TokenMismatch);
    }

    let content_type = presign_config
        .content_type_allowlist
        .coerce(payload.content_type);
    let content_encoding = payload.content_encoding;
    let content_disposition = payload.content_disposition;

    let immutable_store = state.immutable_store.clone();
    let mutable_store = state.mutable_store.clone();

    let correlation_id = headers
        .get(CORRELATION_ID_HEADER)
        .and_then(|v| v.to_str().map(str::to_string).ok())
        .unwrap_or_default();

    let execution = setup_execution(module_path!(), correlation_id, "<presigned>".to_string());
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                parsed_repository,
            ));

            let options = read_options_from_repository(&repository);

            let (fragment, _) = immutable::load_raw(
                repository.clone(),
                parsed_address,
                options.with_decompress(),
            )
            .await
            .map_err(RedeemError::ReadStream)?;
            let total = fragment.size_content;
            let requested = range_request(&headers, total);

            let mut response_headers = HeaderMap::new();
            advertise_ranges(&mut response_headers);
            apply_security_headers(&mut response_headers);
            let range = match requested {
                RangeRequest::Unsatisfiable => {
                    set_content_range(
                        &mut response_headers,
                        unsatisfied_range(total).map_err(RedeemError::HeaderGeneration)?,
                    );
                    return Ok((
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        response_headers,
                        Body::empty(),
                    ));
                }
                RangeRequest::Partial(range) => Some(range),
                RangeRequest::Full => None,
            };
            let status = match &range {
                Some(range) => {
                    set_content_range(
                        &mut response_headers,
                        content_range(range, total).map_err(RedeemError::HeaderGeneration)?,
                    );
                    StatusCode::PARTIAL_CONTENT
                }
                None => StatusCode::OK,
            };
            let read_range = range.map(|r| r.start as usize..r.end as usize);

            let (tx, rx) = channel(CHUNKED_RESPONSE_BUFFER_SIZE);

            let content_length =
                immutable::read_stream(repository, parsed_address, read_range, options, tx)
                    .await
                    .map_err(RedeemError::ReadStream)?;

            response_headers.insert(CONTENT_TYPE, content_type);
            if let Some(ce) = content_encoding {
                response_headers.insert(
                    CONTENT_ENCODING,
                    HeaderValue::from_str(&ce).map_err(RedeemError::HeaderGeneration)?,
                );
            }
            if let Some(cd) = content_disposition {
                response_headers.insert(
                    CONTENT_DISPOSITION,
                    HeaderValue::from_str(&cd).map_err(RedeemError::HeaderGeneration)?,
                );
            }

            response_headers.insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&format!("{content_length}"))
                    .map_err(RedeemError::HeaderGeneration)?,
            );

            let remaining_ttl = payload.expires_at.saturating_sub(now);
            response_headers.insert(
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_str(&format!("public, immutable, max-age={remaining_ttl}"))
                    .map_err(RedeemError::HeaderGeneration)?,
            );

            let stream = ReceiverStream::new(rx);
            Ok((status, response_headers, Body::from_stream(stream)))
        })
        .await
}
