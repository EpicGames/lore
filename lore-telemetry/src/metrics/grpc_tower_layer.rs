// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use std::time::Instant;

use http::Request;
use http::Response;
use http::header::USER_AGENT;
use http_body::Frame;
use http_body::SizeHint;
use pin_project::pin_project;
use pin_project::pinned_drop;
use tonic::Code;
use tonic::body::Body;
use tower::Layer;
use tower::Service;

use super::USER_AGENT_NONE;
use super::USER_AGENT_UNKNOWN;
use super::grpc_metrics::GrpcRequestMetrics;
use super::user_agent_filter::NormalizeOutput;
use super::user_agent_filter::UserAgentFilter;

const GRPC_STATUS_HEADER: &str = "grpc-status";

// Code based on <https://github.com/blkmlk/tonic-prometheus-layer> which has a similar
// requirement but implements specifically for Prometheus. Reworking using the `opentelementry` crate

/// A `tower::Layer` that wraps the `GrpcMetricsService` used to integrate with your Tonic server
///
/// Example
/// ```
/// use std::sync::Arc;
/// use lore_telemetry::user_agent_filter::UserAgentFilter;
/// let filter = Arc::new(UserAgentFilter::default());
/// let metrics_layer = lore_telemetry::grpc_tower_layer::GrpcMetricsLayer::new(filter);
/// let tower_layer = tower::ServiceBuilder::new().layer(metrics_layer);
/// let mut server = tonic::transport::Server::builder().layer(tower_layer);
/// ```
#[derive(Clone)]
pub struct GrpcMetricsLayer {
    filter: Arc<UserAgentFilter>,
}

impl GrpcMetricsLayer {
    pub fn new(filter: Arc<UserAgentFilter>) -> Self {
        Self { filter }
    }
}

impl<S> Layer<S> for GrpcMetricsLayer {
    type Service = GrpcMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcMetricsService {
            service: inner,
            filter: self.filter.clone(),
        }
    }
}

/// A `tower::Service` implementation that records standard metrics for http/gRPC calls.
///
/// The request body is handed on re-wrapped, so that the service can tell when the inner
/// service has finished reading it.
#[derive(Clone)]
pub struct GrpcMetricsService<S> {
    service: S,
    filter: Arc<UserAgentFilter>,
}

impl<S, C> Service<Request<Body>> for GrpcMetricsService<S>
where
    S: Service<Request<Body>, Response = Response<C>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = GrpcMetricsFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let method = req.method().to_string();
        let path = req.uri().path().to_owned();

        let user_agent = match req.headers().get(USER_AGENT).and_then(|v| v.to_str().ok()) {
            Some(v) => match self.filter.normalize(v) {
                NormalizeOutput::KnownAgent(label) => label,
                NormalizeOutput::Unknown => {
                    self.filter.sample_unknown_agent(v);
                    USER_AGENT_UNKNOWN.clone()
                }
            },
            None => USER_AGENT_NONE.clone(),
        };

        let (parts, body) = req.into_parts();
        let request_received_at = Arc::new(OnceLock::new());
        let body = Body::new(RequestReadBody {
            inner: body,
            read_at: request_received_at.clone(),
        });
        let f = self.service.call(Request::from_parts(parts, body));

        GrpcMetricsFuture::new(&method, &path, user_agent, request_received_at, f)
    }
}

/// A request body that notes the instant its reader reaches the end of it.
///
/// The instant is published through the shared cell once the wrapped body reports end of
/// stream, and never before, so a reader finding the cell empty knows the body is still being
/// read.
#[pin_project]
struct RequestReadBody<B> {
    #[pin]
    inner: B,
    read_at: Arc<OnceLock<Instant>>,
}

impl<B> http_body::Body for RequestReadBody<B>
where
    B: http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        let frame = ready!(this.inner.as_mut().poll_frame(cx));
        if frame.is_none() || this.inner.is_end_stream() {
            let _ = this.read_at.set(Instant::now());
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A `Future` that handles the lifetime of the http/gRPC request and tracks the metrics for it
#[pin_project(PinnedDrop)]
pub struct GrpcMetricsFuture<F> {
    metrics: GrpcRequestMetrics,
    started_at: Option<Instant>,
    request_received_at: Arc<OnceLock<Instant>>,
    #[pin]
    inner: F,
}

impl<F> GrpcMetricsFuture<F> {
    fn new(
        method: &str,
        path: &str,
        user_agent: Arc<str>,
        request_received_at: Arc<OnceLock<Instant>>,
        inner: F,
    ) -> Self {
        Self {
            started_at: None,
            request_received_at,
            inner,
            metrics: GrpcRequestMetrics::new(method, path, user_agent),
        }
    }
}

impl<F, B, E> Future for GrpcMetricsFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let started_at = this.started_at.get_or_insert_with(|| {
            this.metrics.request_started();
            Instant::now()
        });

        if let Poll::Ready(response) = this.inner.poll(cx) {
            let status_code = response.as_ref().ok().map(Response::status);
            let rpc_code = response.as_ref().map_or(Code::Unknown, |resp| {
                resp.headers()
                    .get(GRPC_STATUS_HEADER)
                    .map_or(Code::Ok, |s| Code::from_bytes(s.as_bytes()))
            });
            // If the rpc returned `Unimplemented` do not emit metrics, it's likely a request from a
            // security scan.
            if !matches!(rpc_code, Code::Unimplemented) {
                let now = Instant::now();
                let elapsed = now.duration_since(*started_at);
                // A response ready before the request has been read to its end, as a
                // bidirectional stream's is, has no later point to count from than the
                // request's start.
                let handler_elapsed = this
                    .request_received_at
                    .get()
                    .map_or(elapsed, |received_at| now.duration_since(*received_at));
                this.metrics.request_complete(
                    elapsed.as_secs_f64(),
                    handler_elapsed.as_secs_f64(),
                    rpc_code,
                    status_code,
                );
            }
            Poll::Ready(response)
        } else {
            Poll::Pending
        }
    }
}

#[pinned_drop]
impl<F> PinnedDrop for GrpcMetricsFuture<F> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();

        if this.started_at.is_some() {
            this.metrics.request_finished();
        }
    }
}
