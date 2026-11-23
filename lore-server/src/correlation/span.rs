// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_telemetry::tracing::fields::CORRELATION_ID;
use tower_http::trace::MakeSpan;
use tracing::Span;
use tracing::info_span;

use crate::http::extract_correlation_id;

/// Span names have to be literals, and since this is meant
/// to be used in a Tower service layer, it will most likely
/// be the parent.
pub const SPAN_NAME: &str = "urc";

/// Implements `MakeSpan` to add a correlation ID if it exists
/// in the request
#[derive(Debug, Clone)]
pub struct MakeCorrelationIdSpan;

impl<B> MakeSpan<B> for MakeCorrelationIdSpan {
    fn make_span(&mut self, request: &http::Request<B>) -> Span {
        let correlation_id = extract_correlation_id(request);
        info_span!(SPAN_NAME, {CORRELATION_ID} = correlation_id, method = %request.method(), uri = %request.uri().path())
    }
}
