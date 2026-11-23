// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::Extension;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header::CONTENT_DISPOSITION;
use axum::http::header::CONTENT_ENCODING;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::InvalidHeaderValue;
use axum::response::IntoResponse;
use axum::response::Response;
use hex::FromHexError;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_revision::immutable;
use lore_revision::immutable::ImmutableError;
use lore_revision::immutable::read_options_from_repository;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::no_link_traversal;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use reqwest::header::CONTENT_LENGTH;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc::channel;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;

use crate::http::log_http_error;
use crate::http::presign_token::PresignTokenError;
use crate::http::presign_token::token_hash;
use crate::http::presign_token::verify;
use crate::http::range::Selection;
use crate::http::range::range_headers;
use crate::http::range::read_range;
use crate::http::range::requested_range;
use crate::http::range::select;
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

/// One redemption's log line, written when it drops, so a request abandoned before its response,
/// as by a timeout, is logged too.
struct Redemption {
    token_hash: String,
    client_ip: Option<IpAddr>,
    /// Set once the token verifies or fails only as expired.
    key_id: Option<String>,
    /// Set once the token verifies or fails only as expired.
    expires_at: Option<u64>,
    /// Set once the response is built; absent for a request abandoned before then.
    status: Option<StatusCode>,
    /// The response's `Content-Length`; zero for a response with no content.
    bytes_served: u64,
}

impl Drop for Redemption {
    fn drop(&mut self) {
        info!(
            key_id = self.key_id,
            expires_at = self.expires_at,
            token_hash = self.token_hash,
            client_ip = self.client_ip.map(tracing::field::display),
            status = self.status.map(|status| status.as_u16()),
            bytes_served = self.bytes_served,
            "Presigned URL redemption"
        );
    }
}

/// Serves the content a presigned token names, or the byte range its `Range` header asks for,
/// and logs one line per request that carries a token, whatever its outcome.
///
/// Answers `GET` only: the router sends every other method to `reject_non_get`, so it reads
/// `Range` as a `GET` request's. The client IP is the TCP peer's, present only when the server
/// is served with connect info.
pub async fn handler(
    State(state): State<Arc<ServerState>>,
    Path((repository_id, address)): Path<(String, String)>,
    Query(query): Query<RedeemQuery>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
) -> Response {
    let mut redemption = Redemption {
        token_hash: token_hash(&query.token),
        client_ip: connect_info.map(|Extension(ConnectInfo(peer))| peer.ip().to_canonical()),
        key_id: None,
        expires_at: None,
        status: None,
        bytes_served: 0,
    };
    let response = redeem(
        state,
        repository_id,
        address,
        &query.token,
        headers,
        &mut redemption,
    )
    .await
    .into_response();
    redemption.status = Some(response.status());
    response
}

async fn redeem(
    state: Arc<ServerState>,
    repository_id: String,
    address: String,
    token: &str,
    headers: HeaderMap,
    redemption: &mut Redemption,
) -> Result<(StatusCode, HeaderMap, Body), RedeemError> {
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

    let verified = verify(token, &presign_config.hmac_key, &presign_config.key_id, now);
    // An expired token's signature has verified, so its claims are trustworthy enough to log.
    let claims = match &verified {
        Ok(payload) => Some(payload),
        Err(PresignTokenError::Expired(payload)) => Some(&**payload),
        Err(_) => None,
    };
    if let Some(payload) = claims {
        redemption.key_id = Some(payload.key_id.clone());
        redemption.expires_at = Some(payload.expires_at);
    }
    let payload = verified.map_err(RedeemError::InvalidToken)?;

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

    let range = requested_range(&Method::GET, &headers);
    let execution = setup_execution(module_path!(), correlation_id, "<presigned>".to_string());
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                parsed_repository,
                no_link_traversal(),
            ));

            let options = read_options_from_repository(&repository);

            let (fragment, buffer) =
                immutable::load_root(repository.clone(), parsed_address, options)
                    .await
                    .map_err(RedeemError::ReadStream)?;
            let total = fragment.size_content;
            let selection = select(range, total);
            if selection == Selection::Unsatisfiable {
                let mut response_headers = HeaderMap::new();
                let status = range_headers(&selection, total, &mut response_headers)
                    .map_err(RedeemError::HeaderGeneration)?;
                apply_security_headers(&mut response_headers);
                return Ok((status, response_headers, Body::empty()));
            }

            let (tx, rx) = channel(CHUNKED_RESPONSE_BUFFER_SIZE);

            let content_length = immutable::stream_root(
                repository,
                parsed_address,
                fragment,
                buffer,
                read_range(&selection),
                options,
                tx,
            )
            .await
            .map_err(RedeemError::ReadStream)?;

            let mut response_headers = HeaderMap::new();
            let status = range_headers(&selection, total, &mut response_headers)
                .map_err(RedeemError::HeaderGeneration)?;
            apply_security_headers(&mut response_headers);
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

            redemption.bytes_served = content_length;
            let stream = ReceiverStream::new(rx);
            Ok((status, response_headers, Body::from_stream(stream)))
        })
        .await
}
