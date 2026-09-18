// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::task::Poll;

use http::Extensions;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::USER_AGENT;
use lore_telemetry::tracing::fields::USER_ID;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use tonic::body::Body;
use tonic::metadata::MetadataMap;
use tower::Layer;
use tower::Service;
use tracing::Instrument;
use tracing::instrument::Instrumented;

use crate::auth::jwt::AuthorizationToken;
use crate::grpc::get_repository;

/*
 * A service that inspects the incoming request and extracts Lore specific information
 * from the request into a new span, then simply delegates to the next service.
 */
#[derive(Clone, Debug)]
pub struct LoreTracingService<S> {
    inner: S,
}

impl<S> Service<http::Request<Body>> for LoreTracingService<S>
where
    S: Service<http::Request<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Instrumented<S::Future>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, input_http_request: http::Request<Body>) -> Self::Future {
        // See: https://docs.rs/tower/latest/tower/trait.Service.html#be-careful-when-cloning-inner-services
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let uri = input_http_request.uri().clone();
        let method = input_http_request.method().clone();
        let version = input_http_request.version();
        let (rpc_service, rpc_method) = grpc_path_segments(uri.path()).map_or_else(
            || {
                (
                    UNKNOWN_RPC_SEGMENT.to_string(),
                    UNKNOWN_RPC_SEGMENT.to_string(),
                )
            },
            |(s, m)| (s.to_string(), m.to_string()),
        );
        let grpc_request = tonic::Request::from_http(input_http_request);

        let (metadata, extensions, msg) = grpc_request.into_parts();

        let repository_id = repository_id_field(&metadata);
        let user_id_present = user_id_from_extensions(&extensions);

        let mut output_http_request = http::Request::new(msg);
        *output_http_request.version_mut() = version;
        *output_http_request.method_mut() = method;
        *output_http_request.uri_mut() = uri;
        *output_http_request.headers_mut() = metadata.into_headers();
        *output_http_request.extensions_mut() = extensions;

        let correlation_id = correlation_id_field(output_http_request.headers());
        let user_agent = user_agent_field(output_http_request.headers());

        let span = tracing::info_span!(
            parent: None,
            "lore_tracing",
            "rpc.system" = "grpc",
            "rpc.service" = rpc_service,
            "rpc.method" = rpc_method,
            { CORRELATION_ID } = correlation_id,
            { USER_AGENT } = user_agent,
            { REPOSITORY_ID } = repository_id,
            { USER_ID } = tracing::field::Empty,
        );
        if let Some(user_id) = user_id_present {
            span.record(USER_ID, user_id);
        }

        inner.call(output_http_request).instrument(span)
    }
}

#[derive(Clone)]
pub struct LoreTracingLayer {}

impl<S> Layer<S> for LoreTracingLayer
where
    S: Service<http::Request<Body>>,
{
    type Service = LoreTracingService<S>;

    fn layer(&self, service: S) -> Self::Service {
        LoreTracingService { inner: service }
    }
}

fn repository_id_field(metadata: &MetadataMap) -> String {
    get_repository(metadata)
        .ok()
        .map_or("<no_repo_id>".to_string(), |context| context.to_string())
}

fn correlation_id_field(headers: &http::HeaderMap) -> &str {
    headers
        .get(CORRELATION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<no_correlation_id>")
}

fn user_agent_field(headers: &http::HeaderMap) -> &str {
    headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<no_user_agent>")
}

fn user_id_from_extensions(extensions: &Extensions) -> Option<String> {
    extensions
        .get::<AuthorizationToken>()
        .map(|auth| auth.identity().to_string())
}

const UNKNOWN_RPC_SEGMENT: &str = "<unknown>";

fn grpc_path_segments(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.strip_prefix('/')?;
    let mut parts = trimmed.split('/');
    let service = parts.next()?;
    let method = parts.next()?;
    if service.is_empty() || method.is_empty() {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((service, method))
}

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;

    use super::*;

    #[test]
    fn can_extract_repository_id() {
        let repository = random::<Context>();
        let mut metadata = MetadataMap::new();
        metadata.insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let string_from_helper = repository_id_field(&metadata);
        let expected_string = repository.to_string();
        assert_eq!(string_from_helper, expected_string);
    }

    #[test]
    fn handles_no_repository_id() {
        let metadata = MetadataMap::new();

        let string_from_helper = repository_id_field(&metadata);
        assert_eq!(string_from_helper, "<no_repo_id>".to_string());
    }
}
