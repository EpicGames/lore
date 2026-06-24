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
use bytes::Bytes;
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
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tracing::debug;

use crate::auth::jwt::AuthorizationToken;
use crate::http::log_http_error;
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

            let options = immutable::read_options_from_repository(&repository).with_isolation();

            let (tx, rx) = channel(CHUNKED_RESPONSE_BUFFER_SIZE);

            let content_length = immutable::read_stream(repository, parsed_address, options, tx)
                .await
                .map_err(GetContentError::ReadStream)?;

            let stream = ReceiverStream::new(rx).map(Ok::<Bytes, GetContentError>);

            let headers = create_stream_response_headers(query, content_length)
                .map_err(GetContentError::HeaderGeneration)?;

            Ok((StatusCode::OK, headers, Body::from_stream(stream)))
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::ops::Add;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use axum::http::HeaderName;
    use axum::http::HeaderValue;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use jsonwebtoken::EncodingKey;
    use jsonwebtoken::Header;
    use jsonwebtoken::encode;
    use lore_base::types::Context;
    use lore_revision::fragment;
    use rand::random;

    use super::*;
    use crate::http::server::LoreHttpServerSettings;
    use crate::http::server::ServerHealth;
    use crate::http::server::create_router;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn test_server_is_up_and_listening() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Create the server and test the request
                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let test_shared_state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: None,
                };

                let settings = LoreHttpServerSettings::default();
                let app = create_router(test_shared_state, test_health, &settings);
                let test_server = TestServer::new(app).unwrap();

                let response = test_server.get("/does-not-exist").expect_failure().await;

                assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
            })
            .await;
    }

    #[tokio::test]
    async fn test_address_in_wrong_format() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let test_shared_state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: None,
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(test_shared_state, test_health, &settings);
                let test_server = TestServer::new(app).unwrap();

                // Create the server and test the request
                let non_existing_address = "/v1/repository/fffff/content/fffff-ff"; // Wrong lengths
                let response = test_server.get(non_existing_address).expect_failure().await;

                assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
            })
            .await;
    }

    #[tokio::test]
    async fn test_address_not_found() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let test_shared_state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: None,
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(test_shared_state, test_health, &settings);
                let test_server = TestServer::new(app).unwrap();

                // Create the server and test the request
                let non_existing_address = "/v1/repository/ffffffffffffffffffffffffffffffff/content/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff-ffffffffffffffffffffffffffffffff";
                let response = test_server.get(non_existing_address).expect_failure().await;

                assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
                assert_eq!(response.text(), "address not found");
            }).await;
    }

    #[tokio::test]
    async fn test_address_returned_correctly() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Create a fragment on a repository in the store
                let repository = random::<Context>();
                let (fragment, address, payload) = fragment::generate_random();

                immutable_store
                    .clone()
                    .put(
                        repository.into(),
                        address,
                        fragment,
                        Some(payload.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to put data in immutable store");

                // Create the server and test the request
                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let test_shared_state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: None,
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(test_shared_state, test_health, &settings);
                let test_server = TestServer::new(app).unwrap();
                let valid_url = format!("/v1/repository/{repository}/content/{address}");

                let response = test_server.get(valid_url.as_str()).await;

                assert_eq!(response.status_code(), StatusCode::OK);
            })
            .await;
    }

    #[tokio::test]
    async fn test_address_returned_correctly_with_headers() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Create a fragment on a repository in the store
                let repository = random::<Context>();
                let (fragment, address, payload) = fragment::generate_random();

                immutable_store
                    .clone()
                    .put(
                        repository.into(),
                        address,
                        fragment,
                        Some(payload.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to put data in immutable store");

                // Create the server and test the request
                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let test_shared_state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: None,
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(test_shared_state, test_health, &settings);
                let test_server = TestServer::new(app).unwrap();
                let valid_url = format!("/v1/repository/{repository}/content/{address}");

                let response = test_server
                    .get(valid_url.as_str())
                    .add_query_param("content_type", "image/png")
                    .add_query_param("content_encoding", "gzip")
                    .add_query_param("content_disposition", "inline")
                    .await;

                assert_eq!(response.status_code(), StatusCode::OK);
                assert_eq!(
                    response.headers().get("content-type").unwrap(),
                    HeaderValue::from_static("image/png")
                );
                assert_eq!(
                    response.headers().get("content-encoding").unwrap(),
                    HeaderValue::from_static("gzip")
                );
                assert_eq!(
                    response.headers().get("content-disposition").unwrap(),
                    HeaderValue::from_static("inline")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_address_works_with_jwt_verifier_and_good_token() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Create a fragment on a repository in the store
                let repository = random::<Context>();
                let (fragment, address, payload) = fragment::generate_random();

                immutable_store
                    .clone()
                    .put(
                        repository.into(),
                        address,
                        fragment,
                        Some(payload.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to put data in immutable store");

                // Create the server and test the request
                let test_health = ServerHealth::new_without_availability(immutable_store.clone());
                let test_shared_state = ServerState {
                    immutable_store,
                    mutable_store,
                    jwt_verifier: None,
                    max_file_size: 100,
                    presign_config: None,
                };
                let settings = LoreHttpServerSettings::default();
                let app = create_router(test_shared_state, test_health, &settings);
                let test_server = TestServer::new(app).unwrap();
                let valid_url = format!("/v1/repository/{repository}/content/{address}");

                // Create a valid token, with a kid (which is what's checked for now, so it breaks if/when we check further)
                let auth_header = HeaderName::from_static("authorization");
                let jwt_header = Header {
                    kid: Some("a_kid".to_owned()),
                    ..Default::default()
                };

                let expiration = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .add(Duration::from_secs(60))
                    .as_secs();

                let jwt_claims = AuthorizationToken {
                    user_id: "qweasdzxc123".to_string(),
                    issuer: "test".to_string(),
                    issued_at: 123456700,
                    audience: vec!["Lore".to_string()],
                    env: "DEV".to_string(),
                    name: "test".to_string(),
                    preferred_username: "test".to_string(),
                    resources: None,
                    groups: None,
                    is_service_account: Some(false),
                    expires: expiration,
                    idp: "test".to_string(),
                };
                let jwt_key = EncodingKey::from_secret("test-secret".as_ref());
                let bearer = encode(&jwt_header, &jwt_claims, &jwt_key).unwrap();
                let bearer_header_string = format!("Bearer {bearer}");
                let bearer_header = HeaderValue::from_str(bearer_header_string.as_str()).unwrap();
                let response = test_server
                    .get(valid_url.as_str())
                    .add_header(auth_header, bearer_header)
                    .await;

                assert_eq!(response.status_code(), StatusCode::OK);
            })
            .await;
    }
}
