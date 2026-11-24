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

use http_body::Body;
use http_body::Frame;
use http_body::SizeHint;
use lore_base::lore_spawn_core;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::interface::ExecutionContext;
use lore_revision::runtime::CancelOnDrop;
use lore_revision::runtime::Cancellation;
use pin_project::pin_project;
use tokio::task::JoinHandle;
use tower::Layer;
use tower::Service;

tokio::task_local! {
    /// Cancelled once nobody is waiting for the response of the request this task serves.
    static REQUEST_CANCELLATION: Cancellation;
}

/// The cancellation of the request being served: the one this task serves, through [`CoreHop`]
/// or [`with_request_cancellation`], else the one the current execution context carries, which a
/// task spawned for the request inherits.
pub fn request_cancellation() -> Option<Cancellation> {
    REQUEST_CANCELLATION
        .try_with(Cancellation::clone)
        .ok()
        .or_else(lore_revision::runtime::current_cancellation)
}

/// Whether the request being served has been cancelled: the one this task serves, else the one
/// the current execution context carries.
pub fn request_is_cancelled() -> bool {
    REQUEST_CANCELLATION
        .try_with(Cancellation::is_cancelled)
        .unwrap_or_else(|_| lore_revision::runtime::check_cancelled().is_err())
}

/// Runs `future`, work a request starts that must run to its end whether or not anyone still
/// waits for the request, under the request's context without its cancellation.
pub async fn outliving_request<F: Future>(future: F) -> F::Output {
    let current = LORE_CONTEXT.try_with(std::sync::Arc::clone).ok();
    let outliving = match current
        .as_deref()
        .and_then(|context| context.downcast_ref::<ExecutionContext>())
    {
        Some(context) if context.cancellation().is_some() => {
            Some(std::sync::Arc::new(context.outliving().await))
        }
        _ => None,
    };
    // A cancellation nothing cancels stands in for the request's, which a check reads first.
    let future = with_request_cancellation(Cancellation::new(), future);
    match outliving {
        Some(context) => LORE_CONTEXT.scope(context, future).await,
        None => future.await,
    }
}

/// Runs `future` as the handling of a request whose cancellation is `cancellation`.
pub fn with_request_cancellation<F: Future>(
    cancellation: Cancellation,
    future: F,
) -> impl Future<Output = F::Output> {
    REQUEST_CANCELLATION.scope(cancellation, future)
}

/// Runs one poll of a request handler, `poll`, as the handling of a request whose cancellation
/// is `cancellation`.
pub fn poll_with_request_cancellation<R>(
    cancellation: Cancellation,
    poll: impl FnOnce() -> R,
) -> R {
    REQUEST_CANCELLATION.sync_scope(cancellation, poll)
}

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
///
/// The task is never aborted: a client that goes away cancels the request instead, which the
/// handler observes at the points where stopping is safe. The request is cancelled when the
/// response future is dropped before the handler returns, or once the response body is
/// dropped, whether the body was sent in full or the client stopped reading it.
#[derive(Clone, Debug)]
pub struct CoreHop<S> {
    inner: S,
}

impl<S, Request, ResponseBody> Service<Request> for CoreHop<S>
where
    S: Service<Request, Response = http::Response<ResponseBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResponseBody: Send + 'static,
    Request: Send + 'static,
{
    type Response = http::Response<CancellingBody<ResponseBody>>;
    type Error = S::Error;
    type Future = CoreHopFuture<ResponseBody, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // The readiness just polled belongs to `self.inner`, so hand that instance
        // to the task and keep the clone for the next call.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let cancellation = Cancellation::new();
        CoreHopFuture {
            handle: lore_spawn_core!(with_request_cancellation(
                cancellation.clone(),
                inner.call(request)
            )),
            guard: Some(cancellation.cancel_on_drop()),
        }
    }
}

/// Resolves to the inner service's output once the core-runtime task completes.
pub struct CoreHopFuture<B, E> {
    handle: JoinHandle<Result<http::Response<B>, E>>,
    /// Moves into the response body once the handler returns one.
    guard: Option<CancelOnDrop>,
}

impl<B, E> Future for CoreHopFuture<B, E> {
    type Output = Result<http::Response<CancellingBody<B>>, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(output)) => {
                let guard = self.guard.take();
                Poll::Ready(output.map(|response| {
                    response.map(|inner| CancellingBody {
                        inner,
                        _guard: guard,
                    })
                }))
            }
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

/// A response body that cancels its request when dropped.
#[pin_project]
pub struct CancellingBody<B> {
    #[pin]
    inner: B,
    _guard: Option<CancelOnDrop>,
}

impl<B: Body> Body for CancellingBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
