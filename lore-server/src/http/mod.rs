// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod health_check;
pub mod presign_token;
pub mod presigned;
pub mod repositories;
pub mod server;
pub mod tracing;

use ::tracing::debug;
use ::tracing::warn;
use axum::http::StatusCode;
use lore_transport::grpc::CORRELATION_ID_HEADER;
pub use server::LoreHttpServer;

pub(crate) fn log_http_error(error: &impl std::fmt::Debug, status: StatusCode) {
    if status.is_server_error() {
        warn!(?error, "http server error");
    } else {
        debug!(?error, "http user error");
    }
}

/// Extracts correlation IDs from `http::Request` headers
pub fn extract_correlation_id<B>(req: &http::Request<B>) -> Option<String> {
    match req.headers().get(CORRELATION_ID_HEADER) {
        Some(val) => val.to_str().map(|s| s.to_string()).ok(),
        None => None,
    }
}
