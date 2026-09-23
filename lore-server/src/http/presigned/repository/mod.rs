// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod redeem;

use std::sync::Arc;

use axum::Router;
use axum::extract::Path;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::ALLOW;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use serde::Deserialize;
use tracing::Span;

use crate::http::server::ServerState;

#[derive(Deserialize)]
struct TracePath {
    repository_id: String,
}

async fn trace(Path(params): Path<TracePath>, request: Request, next: Next) -> Response {
    Span::current().record(REPOSITORY_ID, &params.repository_id as &str);
    next.run(request).await
}

// Axum otherwise dispatches HEAD to GET, which would start a redemption pipeline.
async fn reject_non_get() -> impl IntoResponse {
    (StatusCode::METHOD_NOT_ALLOWED, [(ALLOW, "GET")])
}

pub fn create_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route(
            "/{address}",
            routing::get(redeem::handler)
                .head(reject_non_get)
                .fallback(reject_non_get),
        )
        .layer(middleware::from_fn(trace))
        .with_state(state)
}
