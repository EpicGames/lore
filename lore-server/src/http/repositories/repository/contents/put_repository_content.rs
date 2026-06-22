// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::immutable;
use lore_revision::repository::RepositoryContext;
use lore_storage::options::WriteOptions;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use serde::Serialize;
use tracing::debug;
use tracing::info;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::verify_repository_write_authorization;
use crate::http::server::ServerState;
use crate::util::get_user_id_from_token;
use crate::util::setup_execution;

#[derive(Serialize)]
struct ResponseData {
    address: String,
}

#[derive(Serialize)]
struct ResponseSuccess {
    data: ResponseData,
}

pub async fn handler(
    State(state): State<Arc<ServerState>>,
    Path(repository_id): Path<String>,
    Extension(user_info): Extension<Option<AuthorizationToken>>,
    headers: HeaderMap,
    data: Bytes,
) -> Response {
    info!("Put repository {} data {} bytes", repository_id, data.len());
    info!("User info: {:?}", user_info);

    let mut header_error = HeaderMap::new();
    header_error.insert("content-type", HeaderValue::from_str("text/plain").unwrap());

    let correlation_id = headers
        .get(CORRELATION_ID_HEADER)
        .and_then(|header_value| header_value.to_str().map(str::to_string).ok())
        .unwrap_or_default();

    let user_id = get_user_id_from_token(user_info.clone());
    let execution = setup_execution(module_path!(), correlation_id, user_id);
    LORE_CONTEXT
        .scope(execution, async move {
            let repository_id = match repository_id.parse::<Context>() {
                Ok(repository_id) => repository_id,
                Err(error) => {
                    debug!("Error parsing the repository ID {}", error);
                    return (
                        StatusCode::BAD_REQUEST,
                        header_error,
                        Body::from("Wrong repository"),
                    )
                        .into_response();
                }
            };

            if let Some(token) = user_info.as_ref()
                && verify_repository_write_authorization(token, repository_id.into()).is_err()
            {
                return (
                    StatusCode::FORBIDDEN,
                    header_error,
                    Body::from("Write permission required"),
                )
                    .into_response();
            }

            let repository = Arc::new(RepositoryContext::new_server_context(
                state.immutable_store.clone(),
                state.mutable_store.clone(),
                repository_id.into(),
            ));

            let context = uuid::Uuid::now_v7().into();
            let (address, _fragment) = match immutable::write(
                repository.clone(),
                context,
                data,
                WriteOptions::default().with_remote_write(),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    debug!("Failed to write into immutable store. {}", error);
                    return (
                        StatusCode::BAD_REQUEST,
                        header_error,
                        Body::from("Malformed data"),
                    )
                        .into_response();
                }
            };

            (
                StatusCode::OK,
                Json(ResponseSuccess {
                    data: ResponseData {
                        address: format!("{address}"),
                    },
                }),
            )
                .into_response()
        })
        .await
}
