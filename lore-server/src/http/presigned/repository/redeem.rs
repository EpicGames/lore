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

            let options = read_options_from_repository(&repository).with_isolation();

            let (tx, rx) = channel(CHUNKED_RESPONSE_BUFFER_SIZE);

            let content_length = immutable::read_stream(repository, parsed_address, options, tx)
                .await
                .map_err(RedeemError::ReadStream)?;

            let mut response_headers = HeaderMap::new();
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

            apply_security_headers(&mut response_headers);

            let stream = ReceiverStream::new(rx).map(Ok::<Bytes, RedeemError>);
            Ok((StatusCode::OK, response_headers, Body::from_stream(stream)))
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use axum::http::StatusCode;
    use axum::http::header::CONTENT_TYPE;
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
            content_type_allowlist: crate::http::security_headers::ContentTypeAllowlist::default(),
        }
    }

    fn valid_token(repository_id: &str, address: &str, config: &PresignConfig) -> String {
        token_with_content_type(repository_id, address, config, None)
    }

    fn token_with_content_type(
        repository_id: &str,
        address: &str,
        config: &PresignConfig,
        content_type: Option<&str>,
    ) -> String {
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
            content_type: content_type.map(String::from),
            content_encoding: None,
            content_disposition: None,
        };
        sign(&payload, &config.hmac_key)
    }

    fn build_test_server(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        config: PresignConfig,
    ) -> TestServer {
        let test_health = ServerHealth::new_without_availability(immutable_store.clone());
        let state = ServerState {
            immutable_store,
            mutable_store,
            jwt_verifier: None,
            max_file_size: 100,
            presign_config: Some(config),
        };
        let settings = LoreHttpServerSettings::default();
        TestServer::new(create_router(state, test_health, &settings)).unwrap()
    }

    /// Stores random content and redeems it with a token carrying `content_type`.
    async fn redeem_with_content_type(content_type: Option<&str>) -> axum_test::TestResponse {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let content_type = content_type.map(String::from);
        LORE_CONTEXT
            .scope(execution, async move {
                let repository = random::<RepositoryId>();
                let (fragment_data, address, payload) = fragment::generate_random();
                immutable_store
                    .clone()
                    .put(repository, address, fragment_data, Some(payload), false)
                    .await
                    .expect("Failed to put data in immutable store");

                let config = test_presign_config();
                let repo_hex = format!("{repository}");
                let address_str = format!("{address}");
                let token = token_with_content_type(
                    &repo_hex,
                    &address_str,
                    &config,
                    content_type.as_deref(),
                );
                let server = build_test_server(immutable_store, mutable_store, config);

                server
                    .get(&format!("/v1/presigned/{repo_hex}/{address_str}"))
                    .add_query_param("token", token)
                    .await
            })
            .await
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

                let server = build_test_server(immutable_store, mutable_store, config);

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

                let server = build_test_server(immutable_store, mutable_store, config);

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

                let server = build_test_server(immutable_store, mutable_store, config);

                let response = server
                    .get(&format!("/v1/presigned/{repo_hex}/{address_str}"))
                    .add_query_param("token", token)
                    .await;

                assert_eq!(response.status_code(), StatusCode::OK);
                assert_eq!(response.as_bytes(), &payload[..]);

                let cache_control = response
                    .headers()
                    .get("cache-control")
                    .expect("missing Cache-Control header")
                    .to_str()
                    .unwrap();
                assert!(
                    cache_control.starts_with("public, immutable, max-age="),
                    "unexpected Cache-Control value: {cache_control}"
                );
                let max_age: u64 = cache_control
                    .strip_prefix("public, immutable, max-age=")
                    .unwrap()
                    .parse()
                    .expect("max-age is not a number");
                assert!(
                    max_age > 3590 && max_age <= 3600,
                    "max-age {max_age} not in expected range"
                );
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

                let server = build_test_server(immutable_store, mutable_store, config);

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

    #[tokio::test]
    async fn redeem_serves_allowlisted_content_type_verbatim() {
        let response = redeem_with_content_type(Some("image/png")).await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "image/png");
    }

    #[tokio::test]
    async fn redeem_coerces_disallowed_content_type_to_octet_stream() {
        let response = redeem_with_content_type(Some("text/html")).await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        // The dangerous input must not also strip the protections.
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert_eq!(
            response.headers().get("content-security-policy").unwrap(),
            "default-src 'none'; sandbox"
        );
    }

    #[tokio::test]
    async fn redeem_coerces_missing_content_type_to_octet_stream() {
        let response = redeem_with_content_type(None).await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn redeem_falls_back_to_octet_stream_for_unserializable_allowed_type() {
        // A legacy token whose allowed media type carries a control-char
        // parameter must not 500 — redeem falls back to octet-stream.
        let response = redeem_with_content_type(Some("image/png; x=\u{7}")).await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn redeem_sets_security_headers() {
        let response = redeem_with_content_type(Some("image/png")).await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert_eq!(
            response.headers().get("content-security-policy").unwrap(),
            "default-src 'none'; sandbox"
        );
    }
}
