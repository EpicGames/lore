// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::task::Context;
use std::task::Poll;

use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Request;
use http::Response;
use http::Uri;
use http::header::USER_AGENT;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::USER_AGENT_NONE;
use lore_telemetry::USER_AGENT_UNKNOWN;
use lore_telemetry::user_agent_filter::NormalizeOutput;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry_semantic_conventions::attribute::USER_AGENT_NAME;
use opentelemetry_semantic_conventions::trace::HTTP_ROUTE;
use pin_project::pin_project;
use tonic::Code;
use tonic::Status;
use tower::Layer;
use tower::Service;
use tracing::debug;

const GRPC_STATUS_HEADER: &str = "grpc-status";

struct MalformedRequestInstrumentProvider;

impl InstrumentProvider for MalformedRequestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.grpc"
    }
}

static MISSING_REQUEST_MESSAGES: LazyLock<Counter<u64>> =
    LazyLock::new(|| MalformedRequestInstrumentProvider.counter("missing_request_message"));

/// The message tonic pairs with [`Code::Internal`] when a unary request body
/// carries no decodable gRPC message, as an HTTP/2 POST without gRPC
/// length-prefix framing does.
///
/// Matching it is what separates that caller-side rejection from a genuine
/// server fault, because tonic maps both to [`Code::Internal`].
///
/// A caller that resets the request stream reaches the same message, so a
/// match does not by itself mean the caller sent something malformed.
const MISSING_REQUEST_MESSAGE: &str = "Missing request message.";

/// Reclassifies tonic's [`MISSING_REQUEST_MESSAGE`] rejection from
/// [`Code::Internal`] to [`Code::InvalidArgument`].
///
/// Mounted innermost, so every layer outside it observes the reclassified
/// status and [`Code::Internal`] keeps meaning a genuine server fault.
///
/// The other caller-side bodies tonic rejects with [`Code::Internal`] —
/// `"Unexpected EOF decoding stream."` for a body shorter than its length
/// prefix, and `"Error decompressing: ..."` for a corrupt compressed body —
/// are out of scope and still answer [`Code::Internal`].
#[derive(Clone)]
pub struct MalformedRequestLayer {
    filter: Arc<UserAgentFilter>,
}

impl MalformedRequestLayer {
    pub fn new(filter: Arc<UserAgentFilter>) -> Self {
        Self { filter }
    }
}

impl<S> Layer<S> for MalformedRequestLayer {
    type Service = MalformedRequestService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MalformedRequestService {
            service: inner,
            filter: self.filter.clone(),
        }
    }
}

#[derive(Clone)]
pub struct MalformedRequestService<S> {
    service: S,
    filter: Arc<UserAgentFilter>,
}

impl<S, B, C> Service<Request<B>> for MalformedRequestService<S>
where
    S: Service<Request<B>, Response = Response<C>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = MalformedRequestFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        // Cloned rather than resolved into labels here: both are refcounted,
        // while the labels allocate and almost no request needs them.
        let uri = request.uri().clone();
        let user_agent = request.headers().get(USER_AGENT).cloned();

        MalformedRequestFuture {
            inner: self.service.call(request),
            uri,
            user_agent,
            filter: self.filter.clone(),
        }
    }
}

#[pin_project]
pub struct MalformedRequestFuture<F> {
    #[pin]
    inner: F,
    uri: Uri,
    user_agent: Option<HeaderValue>,
    filter: Arc<UserAgentFilter>,
}

impl<F, C, E> Future for MalformedRequestFuture<F>
where
    F: Future<Output = Result<Response<C>, E>>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let Poll::Ready(result) = this.inner.poll(cx) else {
            return Poll::Pending;
        };

        Poll::Ready(result.map(|mut response| {
            if is_missing_request_message(response.headers()) {
                set_invalid_argument(&mut response);

                // Without the leading slash, matching the `http.route` the
                // other gRPC metrics carry so queries can join on it.
                let path = this.uri.path();
                let route = path.strip_prefix('/').unwrap_or(path);
                let user_agent = normalized_user_agent(this.filter, this.user_agent.as_ref());

                debug!(
                    route,
                    user_agent = user_agent.as_ref(),
                    "Reclassified a request carrying no gRPC message as InvalidArgument"
                );
                record_missing_request_message(route, user_agent);
            }
            response
        }))
    }
}

/// Resolves the `User-Agent` to the value recorded, without sampling the
/// unrecognised ones into the log: the metrics layer already samples the same
/// request, and this path answers callers that can repeat it at will.
fn normalized_user_agent(filter: &UserAgentFilter, value: Option<&HeaderValue>) -> Arc<str> {
    let Some(value) = value.and_then(|value| value.to_str().ok()) else {
        return USER_AGENT_NONE.clone();
    };

    match filter.normalize(value) {
        NormalizeOutput::KnownAgent(label) => label,
        NormalizeOutput::Unknown => USER_AGENT_UNKNOWN.clone(),
    }
}

fn record_missing_request_message(route: &str, user_agent: Arc<str>) {
    MISSING_REQUEST_MESSAGES.add(
        1,
        &[
            KeyValue::new(HTTP_ROUTE, Arc::from(route)),
            KeyValue::new(USER_AGENT_NAME, user_agent),
        ],
    );
}

fn is_missing_request_message(headers: &HeaderMap) -> bool {
    // `Status::from_header_map` clones the header map to build a `MetadataMap`,
    // so the code is compared first to keep that off every other response.
    let Some(code) = headers.get(GRPC_STATUS_HEADER) else {
        return false;
    };
    if Code::from_bytes(code.as_bytes()) != Code::Internal {
        return false;
    }

    Status::from_header_map(headers)
        .is_some_and(|status| status.message() == MISSING_REQUEST_MESSAGE)
}

/// The message tonic set still describes the failure, so only `grpc-status`
/// changes.
fn set_invalid_argument<C>(response: &mut Response<C>) {
    response.headers_mut().insert(
        HeaderName::from_static(GRPC_STATUS_HEADER),
        HeaderValue::from(i32::from(Code::InvalidArgument)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses tonic's own encoding so these tests cannot drift from the wire
    /// format the server actually produces.
    fn response_for(code: Code, message: &str) -> Response<()> {
        Status::new(code, message.to_owned()).into_http()
    }

    fn status_of(response: &Response<()>) -> Status {
        Status::from_header_map(response.headers()).expect("a gRPC status in the headers")
    }

    mod is_missing_request_message {
        use super::*;

        #[test]
        fn detects_the_undecodable_unary_body_rejection() {
            let response = response_for(Code::Internal, MISSING_REQUEST_MESSAGE);

            assert!(is_missing_request_message(response.headers()));
        }

        /// The message is percent-encoded on the wire, so the comparison has to
        /// decode it rather than match the header bytes.
        #[test]
        fn detects_the_rejection_through_the_wire_encoding() {
            let response = response_for(Code::Internal, MISSING_REQUEST_MESSAGE);
            let encoded = response
                .headers()
                .get("grpc-message")
                .expect("a grpc-message header")
                .to_str()
                .expect("a valid header value");

            assert_eq!(encoded, "Missing%20request%20message.");
            assert!(is_missing_request_message(response.headers()));
        }

        /// A server fault carrying its own message stays `Internal`, so real
        /// faults keep reaching alerting.
        #[test]
        fn ignores_an_internal_status_with_another_message() {
            let response = response_for(Code::Internal, "store unavailable");

            assert!(!is_missing_request_message(response.headers()));
        }

        #[test]
        fn ignores_the_same_message_under_another_code() {
            let response = response_for(Code::InvalidArgument, MISSING_REQUEST_MESSAGE);

            assert!(!is_missing_request_message(response.headers()));
        }

        /// A status delivered in trailers leaves none in the headers.
        #[test]
        fn ignores_headers_carrying_no_grpc_status() {
            assert!(!is_missing_request_message(&HeaderMap::new()));
        }
    }

    mod set_invalid_argument {
        use super::*;

        #[test]
        fn replaces_the_code_and_keeps_the_message() {
            let mut response = response_for(Code::Internal, MISSING_REQUEST_MESSAGE);

            set_invalid_argument(&mut response);

            let status = status_of(&response);
            assert_eq!(status.code(), Code::InvalidArgument);
            assert_eq!(status.message(), MISSING_REQUEST_MESSAGE);
        }
    }

    mod service {
        use std::convert::Infallible;
        use std::future::Ready;

        use super::*;

        struct Inner(Option<(Code, &'static str)>);

        impl Service<Request<()>> for Inner {
            type Response = Response<()>;
            type Error = Infallible;
            type Future = Ready<Result<Response<()>, Infallible>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _request: Request<()>) -> Self::Future {
                let response = match self.0 {
                    Some((code, message)) => response_for(code, message),
                    None => Response::new(()),
                };
                std::future::ready(Ok(response))
            }
        }

        async fn call_through(inner: Inner, user_agent: &str) -> Response<()> {
            let mut service =
                MalformedRequestLayer::new(Arc::new(UserAgentFilter::default())).layer(inner);
            let request = Request::builder()
                .uri("/urc.rpc.AdminService/ServerInfo")
                .header(USER_AGENT, user_agent)
                .body(())
                .expect("a valid request");

            service
                .call(request)
                .await
                .expect("the inner service cannot fail")
        }

        #[tokio::test]
        async fn the_undecodable_body_rejection_becomes_invalid_argument() {
            let response = call_through(
                Inner(Some((Code::Internal, MISSING_REQUEST_MESSAGE))),
                "curl/8.5.0",
            )
            .await;

            let status = status_of(&response);
            assert_eq!(status.code(), Code::InvalidArgument);
            assert_eq!(status.message(), MISSING_REQUEST_MESSAGE);
        }

        #[tokio::test]
        async fn a_genuine_internal_status_passes_through_untouched() {
            let response = call_through(
                Inner(Some((Code::Internal, "store unavailable"))),
                "lore/1.0",
            )
            .await;

            let status = status_of(&response);
            assert_eq!(status.code(), Code::Internal);
            assert_eq!(status.message(), "store unavailable");
        }

        /// Drives a real generated service so the constant stays tied to the
        /// status tonic actually produces. Every other test builds its input
        /// from [`MISSING_REQUEST_MESSAGE`], so a tonic release that reworded
        /// the message would leave them green while the reclassification
        /// silently stopped.
        #[tokio::test]
        async fn tonic_still_rejects_an_unframed_body_with_the_matched_message() {
            use http_body_util::Empty;
            use lore_revision::environment::EnvironmentConfig;

            use crate::grpc::environment_service::LoreEnvironmentService;
            use crate::legacy::rpc::environment_service_server::EnvironmentServiceServer;

            let inner = EnvironmentServiceServer::new(LoreEnvironmentService::maintenance(
                EnvironmentConfig::default(),
            ));
            let mut service =
                MalformedRequestLayer::new(Arc::new(UserAgentFilter::default())).layer(inner);
            let request = Request::builder()
                .method("POST")
                .uri("/urc.rpc.EnvironmentService/Get")
                .header("content-type", "application/grpc")
                .body(Empty::<bytes::Bytes>::new())
                .expect("a valid request");

            let response = service
                .call(request)
                .await
                .expect("the generated service answers with a status");
            let status =
                Status::from_header_map(response.headers()).expect("a gRPC status in the headers");

            assert_eq!(status.code(), Code::InvalidArgument);
            assert_eq!(status.message(), MISSING_REQUEST_MESSAGE);
        }

        #[tokio::test]
        async fn a_response_carrying_no_grpc_status_passes_through_untouched() {
            let response = call_through(Inner(None), "lore/1.0").await;

            assert!(Status::from_header_map(response.headers()).is_none());
        }
    }
}
