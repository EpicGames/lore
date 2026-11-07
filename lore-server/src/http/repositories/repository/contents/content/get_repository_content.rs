// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use axum::Extension;
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
use lore_base::types::Context;
use lore_revision::immutable;
use lore_revision::immutable::ImmutableError;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use reqwest::header::CONTENT_LENGTH;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc::channel;
use tokio_stream::wrappers::ReceiverStream;
use tracing::debug;

use crate::auth::jwt::AuthorizationToken;
use crate::http::log_http_error;
use crate::http::range::RangeRequest;
use crate::http::range::advertise_ranges;
use crate::http::range::content_range;
use crate::http::range::range_request;
use crate::http::range::set_content_range;
use crate::http::range::unsatisfied_range;
use crate::http::server::ServerState;
use crate::util::get_user_id_from_token;
use crate::util::setup_execution;

// The maximum number of chunks waiting in the send queue
const CHUNKED_RESPONSE_BUFFER_SIZE: usize = 16;

#[derive(Error, Debug)]
pub enum GetContentError {
    #[error("Failed to parse context: {0}")]
    ParseContext(FromHexError),
    #[error("Failed to parse address: {0}")]
    ParseAddress(FromHexError),
    #[error("Failed to create read steam from immutable store: {0}")]
    ReadStream(ImmutableError),
    #[error("Failed to generate chunked response headers: {0}")]
    HeaderGeneration(InvalidHeaderValue),
}

impl IntoResponse for GetContentError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match &self {
            GetContentError::ParseContext(_) | GetContentError::ParseAddress(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            GetContentError::ReadStream(e)
                if e.is_address_not_found() || e.is_payload_not_found() =>
            {
                (StatusCode::NOT_FOUND, "address not found".to_string())
            }
            GetContentError::ReadStream(_) | GetContentError::HeaderGeneration(_) => (
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
pub struct GetRepositoryContentQuery {
    content_type: Option<String>,
    content_encoding: Option<String>,
    content_disposition: Option<String>,
}

fn create_stream_response_headers(
    query: GetRepositoryContentQuery,
    content_length: u64,
) -> Result<HeaderMap, InvalidHeaderValue> {
    let mut headers = HeaderMap::new();

    if let Some(content_type) = query.content_type {
        headers.insert(CONTENT_TYPE, HeaderValue::from_str(&content_type)?);
    }
    if let Some(content_encoding) = query.content_encoding {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_str(&content_encoding)?);
    }
    if let Some(content_disposition) = query.content_disposition {
        headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_str(&content_disposition)?,
        );
    }

    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&format!("{content_length}"))?,
    );
    Ok(headers)
}

pub async fn handler(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<GetRepositoryContentQuery>,
    Path((repository_id, address)): Path<(String, String)>,
    Extension(user_info): Extension<Option<AuthorizationToken>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, GetContentError> {
    debug!({ADDRESS} = %address, user_info = ?user_info, "Get repository content");

    let immutable_store = state.immutable_store.clone();
    let mutable_store = state.mutable_store.clone();

    // Parse and validate parameters
    let parsed_repository = repository_id
        .parse::<Context>()
        .map_err(GetContentError::ParseContext)?;
    let parsed_address = address
        .parse::<Address>()
        .map_err(GetContentError::ParseAddress)?;

    let user_id = get_user_id_from_token(user_info);

    let correlation_id = headers
        .get(CORRELATION_ID_HEADER)
        .and_then(|header_value| header_value.to_str().map(str::to_string).ok())
        .unwrap_or_default();

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                parsed_repository.into(),
            ));

            let options = immutable::read_options_from_repository(&repository);

            let (fragment, _) = immutable::load_raw(
                repository.clone(),
                parsed_address,
                options.with_decompress(),
            )
            .await
            .map_err(GetContentError::ReadStream)?;
            let total = fragment.size_content;
            let range = match range_request(&headers, total) {
                RangeRequest::Unsatisfiable => {
                    let mut out = HeaderMap::new();
                    advertise_ranges(&mut out);
                    set_content_range(
                        &mut out,
                        unsatisfied_range(total).map_err(GetContentError::HeaderGeneration)?,
                    );
                    return Ok((StatusCode::RANGE_NOT_SATISFIABLE, out, Body::empty()));
                }
                RangeRequest::Partial(range) => Some(range),
                RangeRequest::Full => None,
            };
            let read_range = range.as_ref().map(|r| r.start as usize..r.end as usize);

            let (tx, rx) = channel(CHUNKED_RESPONSE_BUFFER_SIZE);

            let content_length =
                immutable::read_stream(repository, parsed_address, read_range, options, tx)
                    .await
                    .map_err(GetContentError::ReadStream)?;

            let stream = ReceiverStream::new(rx);

            let mut headers = create_stream_response_headers(query, content_length)
                .map_err(GetContentError::HeaderGeneration)?;
            advertise_ranges(&mut headers);
            let status = match &range {
                Some(range) => {
                    set_content_range(
                        &mut headers,
                        content_range(range, total).map_err(GetContentError::HeaderGeneration)?,
                    );
                    StatusCode::PARTIAL_CONTENT
                }
                None => StatusCode::OK,
            };

            Ok((status, headers, Body::from_stream(stream)))
        })
        .await
}
