// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use http_body_util::Full;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::runtime::Cancellation;
use lore_server::util::core_hop::CoreHopLayer;
use lore_server::util::core_hop::request_cancellation;
use tokio::sync::oneshot;
use tower::Layer;
use tower::Service;
use tower::ServiceExt;

/// A handler that hands its request's token out, then answers once `release` fires.
fn handler(
    token_tx: oneshot::Sender<Cancellation>,
    release: oneshot::Receiver<()>,
) -> impl Service<
    (),
    Response = http::Response<Full<bytes::Bytes>>,
    Error = Infallible,
    Future = impl Send,
> + Clone {
    let state = Arc::new(std::sync::Mutex::new(Some((token_tx, release))));
    tower::service_fn(move |()| {
        let (token_tx, release) = state
            .lock()
            .expect("the lock is not poisoned")
            .take()
            .expect("the handler serves one request");
        async move {
            let token = request_cancellation().expect("the request has a token");
            token_tx.send(token).expect("the test waits for the token");
            let _ = release.await;
            Ok(http::Response::new(Full::new(bytes::Bytes::from_static(
                b"body",
            ))))
        }
    })
}

async fn cancelled_soon(token: &Cancellation) -> bool {
    tokio::time::timeout(Duration::from_secs(5), token.cancelled())
        .await
        .is_ok()
}

#[tokio::test]
async fn dropping_the_response_future_cancels_the_request() {
    let (token_tx, token_rx) = oneshot::channel();
    let (_release_tx, release_rx) = oneshot::channel();
    let mut service = CoreHopLayer.layer(handler(token_tx, release_rx));

    let response = service
        .ready()
        .await
        .expect("the service is ready")
        .call(());
    let token = token_rx.await.expect("the handler runs");
    assert!(!token.is_cancelled());

    drop(response);
    assert!(cancelled_soon(&token).await);
}

#[tokio::test]
async fn the_request_stays_live_until_the_body_is_dropped() {
    let (token_tx, token_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mut service = CoreHopLayer.layer(handler(token_tx, release_rx));

    let response = service
        .ready()
        .await
        .expect("the service is ready")
        .call(());
    let token = token_rx.await.expect("the handler runs");
    release_tx.send(()).expect("the handler waits for release");
    let response = response.await.expect("the handler answers");
    assert!(!token.is_cancelled());

    let body = response
        .into_body()
        .collect()
        .await
        .expect("the body is readable")
        .to_bytes();
    assert_eq!(body.as_ref(), b"body");
    assert!(token.is_cancelled());
}

/// An execution context carrying `cancellation`.
fn context_with(cancellation: &Cancellation) -> Arc<ExecutionContext> {
    Arc::new(
        ExecutionContext::new_server(
            LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            String::default(),
        )
        .with_cancellation(cancellation.clone()),
    )
}

mod request_cancellation {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_server::util::core_hop::with_request_cancellation;

    use super::*;

    #[tokio::test]
    async fn a_task_without_the_request_reads_it_from_its_context() {
        let request = Cancellation::new();
        let found = LORE_CONTEXT
            .scope(context_with(&request), async {
                lore_base::lore_spawn!(async { request_cancellation() })
                    .await
                    .expect("the task runs to its end")
            })
            .await
            .expect("the context carries the request");
        request.cancel();
        assert!(found.is_cancelled());
    }

    #[tokio::test]
    async fn the_task_served_takes_precedence_over_the_context() {
        let served = Cancellation::new();
        let other = Cancellation::new();
        let found = with_request_cancellation(
            served.clone(),
            LORE_CONTEXT.scope(context_with(&other), async { request_cancellation() }),
        )
        .await
        .expect("the task serves a request");
        served.cancel();
        assert!(found.is_cancelled());
        assert!(!other.is_cancelled());
    }
}

mod request_is_cancelled {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_server::util::core_hop::request_is_cancelled;

    use super::*;

    #[tokio::test]
    async fn reads_the_context_where_the_task_serves_no_request() {
        let request = Cancellation::new();
        request.cancel();
        let cancelled = LORE_CONTEXT
            .scope(context_with(&request), async { request_is_cancelled() })
            .await;
        assert!(cancelled);
    }
}

mod outliving_request {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_server::grpc::check_request_cancelled;
    use lore_server::util::core_hop::outliving_request;
    use lore_server::util::core_hop::with_request_cancellation;

    use super::*;

    #[tokio::test]
    async fn work_awaited_in_place_sees_no_cancellation_of_its_request() {
        let request = Cancellation::new();
        request.cancel();
        let checked = with_request_cancellation(
            request.clone(),
            LORE_CONTEXT.scope(
                context_with(&request),
                outliving_request(async {
                    (
                        lore_revision::runtime::check_cancelled(),
                        check_request_cancelled(),
                    )
                }),
            ),
        )
        .await;
        checked.0.expect("the context carries no cancellation");
        checked.1.expect("the task serves no cancelled request");
    }
}
