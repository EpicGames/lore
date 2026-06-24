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
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use hex::FromHexError;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_revision::lore::RepositoryId;
use lore_storage::StoreMatch;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
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
            PresignError::ParseRepository(_) | PresignError::ParseAddress(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            PresignError::NotConfigured => (
                StatusCode::NOT_FOUND,
                "presigned URL feature is not enabled".to_string(),
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
    let parsed_address = address
        .parse::<Address>()
        .map_err(PresignError::ParseAddress)?;

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
            let match_result = immutable_store
                .clone()
                .exist(repository, parsed_address, StoreMatch::MatchFull)
                .await
                .map_err(|e| {
                    warn!(%e, "Presign exist check failed");
                    PresignError::StoreError
                })?;

            if match_result == StoreMatch::MatchNone {
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use lore_base::runtime::LORE_CONTEXT;
    use rand::random;
    use serde_json::json;

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

    #[tokio::test]
    async fn returns_404_when_address_not_found() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = random::<lore_revision::lore::RepositoryId>();
                let address = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";

                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: Some(test_presign_config()),
                };
                let repo_hex = format!("{repository}");
                let settings = LoreHttpServerSettings::default();
                let app = create_router(state, test_health, &settings);
                let server = TestServer::new(app).unwrap();

                let response = server
                    .post(&format!("/v1/repository/{repo_hex}/content/{address}/presign"))
                    .json(&json!({"ttl_seconds": 3600}))
                    .await;

                assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            })
            .await;
    }
}
