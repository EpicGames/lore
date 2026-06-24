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
use bytes::Bytes;
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
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::http::log_http_error;
use crate::http::presign_token::PresignTokenError;
use crate::http::presign_token::verify;
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

            let options = read_options_from_repository(&repository).with_isolation();

            let (tx, rx) = channel(CHUNKED_RESPONSE_BUFFER_SIZE);

            let content_length = immutable::read_stream(repository, parsed_address, options, tx)
                .await
                .map_err(RedeemError::ReadStream)?;

            let mut response_headers = HeaderMap::new();
            if let Some(ct) = payload.content_type {
                response_headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_str(&ct).map_err(RedeemError::HeaderGeneration)?,
                );
            }
            if let Some(ce) = payload.content_encoding {
                response_headers.insert(
                    CONTENT_ENCODING,
                    HeaderValue::from_str(&ce).map_err(RedeemError::HeaderGeneration)?,
                );
            }
            if let Some(cd) = payload.content_disposition {
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

            let stream = ReceiverStream::new(rx).map(Ok::<Bytes, RedeemError>);
            Ok((StatusCode::OK, response_headers, Body::from_stream(stream)))
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use axum::http::StatusCode;
    use axum_test::TestServer;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::fragment;
    use lore_revision::lore::RepositoryId;
    use rand::random;

    use crate::http::presign_token::CURRENT_TOKEN_VERSION;
    use crate::http::presign_token::PresignTokenPayload;
    use crate::http::presign_token::sign;
    use crate::http::server::LoreHttpServerSettings;
    use crate::http::server::PresignConfig;
    use crate::http::server::ServerHealth;
    use crate::http::server::ServerState;
    use crate::http::server::create_router;
    use crate::store::test_store_create;

    fn test_presign_config() -> PresignConfig {
        let key_bytes = [0u8; 32];
        PresignConfig {
            hmac_key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes),
            key_id: "test_key_id_1234".to_string(),
            min_ttl_seconds: 1,
            default_ttl_seconds: 3600,
            max_ttl_seconds: 86400,
        }
    }

    fn valid_token(repository_id: &str, address: &str, config: &PresignConfig) -> String {
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let payload = PresignTokenPayload {
            version: CURRENT_TOKEN_VERSION,
            key_id: config.key_id.clone(),
            repository: repository_id.to_string(),
            address: address.to_string(),
            expires_at,
            content_type: None,
            content_encoding: None,
            content_disposition: None,
        };
        sign(&payload, &config.hmac_key)
    }

    #[tokio::test]
    async fn returns_404_when_address_not_found() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = random::<RepositoryId>();
                let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

                let config = test_presign_config();
                let repo_hex = format!("{repository}");
                let token = valid_token(&repo_hex, address, &config);

                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: Some(config),
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(state, test_health, &settings);
                let server = TestServer::new(app).unwrap();

                let response = server
                    .get(&format!("/v1/presigned/{repo_hex}/{address}"))
                    .add_query_param("token", token)
                    .await;

                assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            })
            .await;
    }

    #[tokio::test]
    async fn returns_401_for_expired_token() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = random::<RepositoryId>();
                let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

                let config = test_presign_config();
                let repo_hex = format!("{repository}");

                let payload = PresignTokenPayload {
                    version: CURRENT_TOKEN_VERSION,
                    key_id: config.key_id.clone(),
                    repository: repo_hex.clone(),
                    address: address.to_string(),
                    expires_at: 1,
                    content_type: None,
                    content_encoding: None,
                    content_disposition: None,
                };
                let token = sign(&payload, &config.hmac_key);

                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: Some(config),
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(state, test_health, &settings);
                let server = TestServer::new(app).unwrap();

                let response = server
                    .get(&format!("/v1/presigned/{repo_hex}/{address}"))
                    .add_query_param("token", token)
                    .await;

                assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED);
            })
            .await;
    }

    #[tokio::test]
    async fn returns_200_with_content_for_valid_token() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = random::<RepositoryId>();
                let (fragment_data, address, payload) = fragment::generate_random();

                immutable_store
                    .clone()
                    .put(
                        repository,
                        address,
                        fragment_data,
                        Some(payload.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to put data in immutable store");

                let config = test_presign_config();
                let repo_hex = format!("{repository}");
                let address_str = format!("{address}");
                let token = valid_token(&repo_hex, &address_str, &config);

                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: Some(config),
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(state, test_health, &settings);
                let server = TestServer::new(app).unwrap();

                let response = server
                    .get(&format!("/v1/presigned/{repo_hex}/{address_str}"))
                    .add_query_param("token", token)
                    .await;

                assert_eq!(response.status_code(), StatusCode::OK);
                assert_eq!(response.as_bytes(), &payload[..]);
            })
            .await;
    }

    #[tokio::test]
    async fn returns_400_when_no_token_query_param() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = random::<RepositoryId>();
                let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

                let config = test_presign_config();
                let repo_hex = format!("{repository}");

                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: Some(config),
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(state, test_health, &settings);
                let server = TestServer::new(app).unwrap();

                let response = server
                    .get(&format!("/v1/presigned/{repo_hex}/{address}"))
                    .await;

                assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
                assert_eq!(
                    response.text(),
                    "Failed to deserialize query string: missing field `token`"
                );
            })
            .await;
    }
}
