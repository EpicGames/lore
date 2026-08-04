// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tower layer that moves request handling from the net runtime to the core one.
//!
//! Inbound gRPC and HTTP are served on the net runtime, so without this layer
//! every handler body — and everything it spawns, since `lore_spawn!` follows the
//! current runtime — would run on net alongside the transport it is supposed to
//! be isolated from. Applying [`CoreHopLayer`] as the outermost layer of a stack
//! makes the handoff structural: one place per stack rather than a hop at the top
//! of every handler, which is the only version of this that cannot be silently
//! regressed by a new endpoint.
use std::future::Future;
use std::panic;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use lore_base::lore_spawn_core;
use tokio::task::JoinHandle;
use tower::Layer;
use tower::Service;

/// Applies [`CoreHop`] to a service. See the module docs for why.
#[derive(Clone, Copy, Default, Debug)]
pub struct CoreHopLayer;

impl<S> Layer<S> for CoreHopLayer {
    type Service = CoreHop<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CoreHop { inner }
    }
}

/// Runs the inner service's response future as a task on the core runtime.
#[derive(Clone, Debug)]
pub struct CoreHop<S> {
    inner: S,
}

impl<S, Request> Service<Request> for CoreHop<S>
where
    S: Service<Request> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    S::Error: Send + 'static,
    Request: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = CoreHopFuture<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // The readiness just polled belongs to `self.inner`, so hand that instance
        // to the task and keep the clone for the next call.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        CoreHopFuture {
            handle: lore_spawn_core!(inner.call(request)),
        }
    }
}

/// Resolves to the inner service's output once the core-runtime task completes.
pub struct CoreHopFuture<T, E> {
    handle: JoinHandle<Result<T, E>>,
}

impl<T, E> Future for CoreHopFuture<T, E> {
    type Output = Result<T, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(output)) => Poll::Ready(output),
            // A handler panic must stay a panic rather than becoming a response:
            // `S::Error` has no variant to carry one, and hyper already treats an
            // unwinding handler as a dropped connection.
            Poll::Ready(Err(join_error)) => match join_error.try_into_panic() {
                Ok(payload) => panic::resume_unwind(payload),
                // Only reachable if someone aborts the handle, which nothing does.
                Err(_) => Poll::Pending,
            },
        }
    }
}
