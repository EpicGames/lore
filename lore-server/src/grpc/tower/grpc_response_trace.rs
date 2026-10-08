// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::time::Instant;

use http::Request;
use http::Response;
use pin_project::pin_project;
use pin_project::pinned_drop;
use tonic::Code;
use tower::Layer;
use tower::Service;

use crate::grpc::is_code_considered_server_error;
use crate::grpc::rpc_code_to_str;

const GRPC_STATUS_HEADER: &str = "grpc-status";

#[derive(Clone)]
pub struct GrpcResponseTraceLayer;

impl<S> Layer<S> for GrpcResponseTraceLayer {
    type Service = GrpcResponseTraceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcResponseTraceService { service: inner }
    }
}

#[derive(Clone, Debug)]
pub struct GrpcResponseTraceService<S> {
    service: S,
}

impl<S> GrpcResponseTraceService<S> {
    pub fn make_future<F>(&self, inner: F) -> GrpcMetricsFuture<F> {
        GrpcMetricsFuture {
            inner,
            started_at: None,
            polled_to_completion: false,
        }
    }
}

impl<S, B, C> Service<Request<B>> for GrpcResponseTraceService<S>
where
    S: Service<Request<B>, Response = Response<C>>,
    S::Error: std::fmt::Debug,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = GrpcMetricsFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let inner = self.service.call(req);
        self.make_future(inner)
    }
}

/// Logs the outcome of each request with the time it took.
///
/// `elapsed_ms` counts from the first poll, when only the request headers have arrived, so it
/// includes the time the client took to transmit the request body as well as the time spent
/// handling it.
///
/// Use `RUST_LOG` to control verbosity of response logging, e.g.
/// `RUST_LOG=info,lore_server::grpc::tower::grpc_response_trace=debug` logs all responses rather
/// than errors alone.
#[pin_project(PinnedDrop)]
pub struct GrpcMetricsFuture<F> {
    #[pin]
    inner: F,
    started_at: Option<Instant>,
    polled_to_completion: bool,
}

impl<F, B, E> Future for GrpcMetricsFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
    E: std::fmt::Debug,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let started_at = this.started_at.get_or_insert_with(Instant::now);

        if let Poll::Ready(layer_result) = this.inner.poll(cx) {
            let elapsed_ms = Instant::now().duration_since(*started_at).as_millis();
            match &layer_result {
                Ok(response) => {
                    let code = response
                        .headers()
                        .get(GRPC_STATUS_HEADER)
                        .map_or(Code::Ok, |s| Code::from_bytes(s.as_bytes()));
                    let rpc_status_code = rpc_code_to_str(&code);

                    match code {
                        Code::Ok => {
                            tracing::debug!(rpc_status_code, elapsed_ms, "Lore success response");
                        }
                        _ if is_code_considered_server_error(&code) => {
                            tracing::warn!(
                                rpc_status_code,
                                elapsed_ms,
                                "Lore server error response"
                            );
                        }
                        // anything else assume user error
                        _ => {
                            tracing::debug!(
                                rpc_status_code,
                                elapsed_ms,
                                "Lore user error response"
                            );
                        }
                    }
                }
                // an error in the tower layer below us - it has not gracefully handled a request
                // and should be corrected
                Err(error) => {
                    tracing::error!(elapsed_ms, ?error, "Lore server error handling request");
                }
            }

            *this.polled_to_completion = true;
            Poll::Ready(layer_result)
        } else {
            Poll::Pending
        }
    }
}

#[pinned_drop]
impl<F> PinnedDrop for GrpcMetricsFuture<F> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();

        if !*this.polled_to_completion {
            let started_at = this.started_at.get_or_insert_with(Instant::now);
            let elapsed_ms = Instant::now().duration_since(*started_at).as_millis();

            tracing::info!(elapsed_ms, "Lore server dropped request");
        }
    }
}
