// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// These tests spawn tokio tasks directly without a LORE_CONTEXT, which is fine for
// state-machine unit tests that don't touch the execution context.
#![allow(clippy::disallowed_methods)]

//! Tests for the `RemoteState` state machine and the `RepositoryContext::remote()`
//! lazy resolution path. These exercise the classification logic and the Pending →
//! terminal-state promotion without requiring a real `Arc<Connection>` (Connection
//! construction is non-trivial and covered by integration tests instead). We cover:
//!
//! - Classification: `RemoteState::from_result` routes each error variant correctly.
//! - Terminal-state passthrough: `remote()` on Offline/Failed returns the expected
//!   result via the read-lock fast path.
//! - Pending resolution: `remote()` awaits the shared future and promotes the state.
//! - Concurrent awaiters: N tasks awaiting the same Pending converge on one result.
//! - Cancellation: cancelling awaiters mid-await does not break subsequent awaiters
//!   or the promotion.
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use lore_base::error::Disconnected;
use lore_base::error::NoRemote;
use lore_revision::protocol::RemoteFuture;
use lore_revision::repository::RemoteState;
use lore_revision::test_util::context_with_state;
use lore_transport::ProtocolError;

fn disconnected() -> ProtocolError {
    ProtocolError::from(Disconnected)
}

fn no_remote() -> ProtocolError {
    ProtocolError::from(NoRemote)
}

/// Build a `RemoteFuture` that resolves to the given result, without going through
/// a real connect or spawning a task.
fn ready_remote(result: Result<Arc<lore_transport::Connection>, ProtocolError>) -> RemoteFuture {
    let fut: BoxFuture<'static, _> = async move { result }.boxed();
    fut.shared()
}

/// Nothing is cached until something resolves it, so the first reader or
/// writer drives the connect and a command that does neither remotely never
/// does.
#[tokio::test]
async fn a_fresh_context_has_no_session_pool_to_reuse() {
    for state in [RemoteState::Offline, RemoteState::Failed(no_remote())] {
        let context = context_with_state(state).await;
        assert!(
            context.cached_session_pool("correlation").is_none(),
            "a context that has resolved nothing reported a pool"
        );
    }
}

/// Asking an offline context for a pool must not invent one, and must not
/// leave anything cached for the next caller to find.
#[tokio::test]
async fn an_offline_context_resolves_no_session_pool() {
    let context = context_with_state(RemoteState::Offline).await;
    assert!(context.session_pool("correlation").await.is_err());
    assert!(context.cached_session_pool("correlation").is_none());
}

/// A pool for a context to hold, started for `correlation_id`. The session in it is never
/// resolved: these tests ask which pool answered, not what it yields.
fn held_pool(correlation_id: &str) -> Arc<lore_transport::SessionPool> {
    let session = Arc::new(lore_transport::StorageSession::pending(|| async {
        Err(ProtocolError::internal("a pooled session no test resolves"))
    }));
    Arc::new(lore_transport::SessionPool::new(
        Arc::from(correlation_id),
        vec![session],
    ))
}

/// A pool once resolved is handed back without consulting the remote. This
/// context's connect has failed, so an answer at all is proof the pool was
/// reused rather than looked up again per call.
#[tokio::test]
async fn a_resolved_pool_is_reused_without_touching_the_remote() {
    let context = context_with_state(RemoteState::Failed(disconnected())).await;
    let pool = held_pool("correlation");
    context.set_session_pool(&pool);

    let reused = context
        .session_pool("correlation")
        .await
        .expect("a context holding a pool answers from it");
    assert!(Arc::ptr_eq(&reused, &pool));
}

/// A pool a session over it has invalidated is stale, so the next caller goes back to the
/// remote rather than being handed sessions the server has forgotten.
#[tokio::test]
async fn a_stale_pool_is_not_reused() {
    let context = context_with_state(RemoteState::Failed(disconnected())).await;
    let pool = held_pool("correlation");
    context.set_session_pool(&pool);
    assert!(context.session_pool("correlation").await.is_ok());

    lore_transport::StorageSession::pooled(pool)
        .invalidate()
        .await;
    assert!(
        context.session_pool("correlation").await.is_err(),
        "a stale pool must send the caller back to the remote"
    );
}

/// A pool started for one correlation id is not handed to a call under another, which goes back
/// to the remote for its own.
#[tokio::test]
async fn a_pool_is_not_reused_under_another_correlation_id() {
    let context = context_with_state(RemoteState::Failed(disconnected())).await;
    context.set_session_pool(&held_pool("first"));

    assert!(context.session_pool("first").await.is_ok());
    assert!(
        context.session_pool("second").await.is_err(),
        "another correlation id must send the caller back to the remote"
    );
}

/// Only the terminal no-remote state answers yes. A failure and a connect
/// still in flight both have an answer a session carries, so a caller has to
/// build one.
#[tokio::test]
async fn only_a_context_without_a_remote_is_offline() {
    assert!(
        context_with_state(RemoteState::Offline).await.is_offline(),
        "a context with no remote configured is offline"
    );
    for state in [
        RemoteState::Failed(disconnected()),
        RemoteState::Pending(ready_remote(Err(no_remote()))),
    ] {
        assert!(
            !context_with_state(state).await.is_offline(),
            "a context that has not settled on `Offline` reported itself offline"
        );
    }
}

/// The answer is taken without waiting, so a state lock held elsewhere reads
/// as not offline rather than blocking a caller that only wanted to skip work.
#[tokio::test]
async fn a_held_state_lock_does_not_answer_offline() {
    let context = context_with_state(RemoteState::Offline).await;
    let guard = context.remote.write().await;
    assert!(!context.is_offline());
    drop(guard);
    assert!(context.is_offline());
}

#[tokio::test]
async fn from_result_classifies_no_remote_as_offline() {
    let state = RemoteState::from_result(Err(no_remote()));
    assert!(matches!(state, RemoteState::Offline));
}

#[tokio::test]
async fn from_result_classifies_other_errors_as_failed() {
    let state = RemoteState::from_result(Err(disconnected()));
    assert!(matches!(state, RemoteState::Failed(_)));
}

#[tokio::test]
async fn remote_returns_no_remote_for_offline_state() {
    let ctx = context_with_state(RemoteState::Offline).await;
    let result = ctx.remote().await;
    assert!(matches!(result, Err(ProtocolError::NoRemote(_))));
}

#[tokio::test]
async fn remote_returns_original_error_for_failed_state() {
    let ctx = context_with_state(RemoteState::Failed(disconnected())).await;
    let result = ctx.remote().await;
    assert!(matches!(result, Err(ProtocolError::Disconnected(_))));
}

#[tokio::test]
async fn pending_err_transitions_to_failed() {
    let ctx = context_with_state(RemoteState::Pending(ready_remote(Err(disconnected())))).await;

    // First call drives resolution.
    let result = ctx.remote().await;
    assert!(matches!(result, Err(ProtocolError::Disconnected(_))));

    // State should now be promoted to terminal Failed — subsequent calls take the
    // fast path (no Pending match in the read-lock branch).
    let state = ctx.remote.read().await;
    assert!(
        matches!(*state, RemoteState::Failed(_)),
        "state should be promoted to Failed after Pending resolution"
    );
}

#[tokio::test]
async fn pending_no_remote_transitions_to_offline() {
    let ctx = context_with_state(RemoteState::Pending(ready_remote(Err(no_remote())))).await;

    let result = ctx.remote().await;
    assert!(matches!(result, Err(ProtocolError::NoRemote(_))));

    let state = ctx.remote.read().await;
    assert!(
        matches!(*state, RemoteState::Offline),
        "NoRemote resolution should promote to Offline rather than Failed"
    );
}

#[tokio::test]
async fn concurrent_awaiters_converge_on_single_resolution() {
    // Use a future that resolves after a tick, so all spawned tasks have a chance
    // to race on the Pending state before resolution completes.
    let slow: BoxFuture<'static, _> = async {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        Err::<Arc<lore_transport::Connection>, _>(disconnected())
    }
    .boxed();
    let shared = slow.shared();
    let ctx = context_with_state(RemoteState::Pending(shared)).await;

    // Spawn many concurrent callers. All should get the same error result.
    let mut handles = Vec::new();
    for _ in 0..16 {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move { ctx.remote().await }));
    }
    for h in handles {
        let result = h.await.expect("task should not panic");
        assert!(matches!(result, Err(ProtocolError::Disconnected(_))));
    }

    // State should be promoted exactly once to the terminal result.
    let state = ctx.remote.read().await;
    assert!(matches!(*state, RemoteState::Failed(_)));
}

#[tokio::test]
async fn remote_status_reports_offline() {
    let ctx = context_with_state(RemoteState::Offline).await;
    assert!(matches!(
        ctx.remote_status().await,
        lore_revision::repository::RemoteStatus::Offline
    ));
}

#[tokio::test]
async fn remote_status_reports_failed() {
    let ctx = context_with_state(RemoteState::Failed(disconnected())).await;
    assert!(matches!(
        ctx.remote_status().await,
        lore_revision::repository::RemoteStatus::Failed(ProtocolError::Disconnected(_))
    ));
}

#[tokio::test]
async fn remote_status_reports_pending_without_driving_connect() {
    // Use a future that would panic if polled, to prove remote_status never polls
    // the shared future.
    let never_poll: BoxFuture<'static, _> = async {
        panic!("remote_status must not poll the pending future");
    }
    .boxed();
    let ctx = context_with_state(RemoteState::Pending(never_poll.shared())).await;

    assert!(matches!(
        ctx.remote_status().await,
        lore_revision::repository::RemoteStatus::Pending
    ));
    // State must still be Pending — remote_status must not promote.
    assert!(matches!(*ctx.remote.read().await, RemoteState::Pending(_)));
}

#[tokio::test]
async fn cancelled_awaiter_does_not_break_subsequent_callers() {
    // Resolution waits for a signal so we can reliably cancel awaiters before it fires.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let gated: BoxFuture<'static, _> = async move {
        let _ = rx.await;
        Err::<Arc<lore_transport::Connection>, _>(disconnected())
    }
    .boxed();
    let shared = gated.shared();
    let ctx = context_with_state(RemoteState::Pending(shared)).await;

    // First caller registers a waker on the Shared future, then is cancelled.
    let ctx_for_cancel = ctx.clone();
    let cancelled = tokio::spawn(async move { ctx_for_cancel.remote().await });
    tokio::task::yield_now().await;
    cancelled.abort();
    let _ = cancelled.await;

    // Drive resolution.
    tx.send(())
        .expect("receiver should still be alive via the Shared future");

    // A fresh caller should still get the resolved error and see the state promoted.
    let result = ctx.remote().await;
    assert!(matches!(result, Err(ProtocolError::Disconnected(_))));
    let state = ctx.remote.read().await;
    assert!(matches!(*state, RemoteState::Failed(_)));
}
