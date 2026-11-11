// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// These tests spawn tokio tasks directly without a LORE_CONTEXT, which is fine for
// state-machine unit tests that don't touch the execution context.
#![allow(clippy::disallowed_methods)]

//! Tests for the `RemoteState` state machine and the `RepositoryContext::remote()`
//! lazy resolution path. These exercise the classification logic and the Pending →
//! terminal-state promotion without a real `Arc<Connection>`. We cover:
//!
//! - Classification: `RemoteState::from_result` routes each error variant correctly.
//! - Terminal-state passthrough: `remote()` on Offline/Failed returns the expected
//!   result via the read-lock fast path.
//! - Pending resolution: `remote()` awaits the shared future and promotes the state.
//! - Concurrent awaiters: N tasks awaiting the same Pending converge on one result.
//! - Cancellation: cancelling awaiters mid-await does not break subsequent awaiters
//!   or the promotion.
//! - Session pools: the pool a context holds and reuses, and the endpoint it takes pools from
//!   where one is set, over a detached connection.
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use lore_base::error::Disconnected;
use lore_base::error::NoRemote;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_revision::immutable::load_raw;
use lore_revision::immutable::read_options_from_repository;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::RepositoryId;
use lore_revision::protocol::RemoteFuture;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::test_util::context_with_state;
use lore_storage::options::ReadOptions;
use lore_transport::ProtocolError;

use super::test_helpers::built_context;
use super::test_helpers::counted_endpoint;
use super::test_helpers::store_fragment;
use crate::helper::setup_test_execution;
use crate::helper::test_execution_with;

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

/// A pool to hold, started for `correlation_id`. The session in it is never
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

/// Building a context with an endpoint, and a read the local store serves, ask the endpoint for
/// nothing. The first read that needs a session asks it once, under the correlation id of the call
/// that read, and the context holds the pool the endpoint started and holds.
#[tokio::test]
async fn an_endpoint_is_asked_only_when_a_read_needs_a_session() {
    let globals = LoreGlobalArgs {
        correlation_id: "c1".into(),
        ..Default::default()
    };
    LORE_CONTEXT
        .scope(test_execution_with(globals), async {
            let (endpoint, connection) = counted_endpoint(RepositoryId::default()).await;
            let context =
                built_context(|context| context.with_remote_endpoint(endpoint.clone())).await;
            let stored = store_fragment(&context, b"held", FragmentFlags::empty(), 4).await;
            load_raw(context.clone(), stored, ReadOptions::default())
                .await
                .expect("the local store serves what it holds");
            assert_eq!(connection.storage_session_starts(), 0);

            let absent = Address {
                hash: Hash::from([0xa5u8; 32]),
                context: Context::from([0xa5u8; 16]),
            };
            load_raw(context.clone(), absent, ReadOptions::default())
                .await
                .expect_err("the endpoint's remote holds nothing");
            assert_eq!(connection.storage_session_starts(), 1);
            let held = endpoint
                .pools
                .get(context.id, "c1")
                .expect("the endpoint holds the pool it started");
            assert!(
                context
                    .cached_session_pool("c1")
                    .is_some_and(|pool| Arc::ptr_eq(&pool, &held))
            );
        })
        .await;
}

/// A context answers with the pool the endpoint holds for its correlation id, and a call under
/// another correlation id asks the endpoint again, which starts a pool for that id.
#[tokio::test]
async fn the_endpoint_answer_serves_its_correlation_id() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (endpoint, connection) = counted_endpoint(RepositoryId::default()).await;
            let held = held_pool("c1");
            endpoint.pools.hold(RepositoryId::default(), held.clone());
            let context =
                built_context(|context| context.with_remote_endpoint(endpoint.clone())).await;

            let answered = context
                .session_pool("c1")
                .await
                .expect("the endpoint answers with the pool it holds");
            assert!(Arc::ptr_eq(&answered, &held));
            assert!(context.cached_session_pool("c2").is_none());
            assert_eq!(connection.storage_session_starts(), 0);

            let other = context
                .session_pool("c2")
                .await
                .expect("the endpoint starts a pool for another correlation id");
            assert!(other.serves("c2"));
            assert_eq!(connection.storage_session_starts(), 1);
        })
        .await;
}

/// A pool the endpoint holds that a session over it has invalidated is not handed out again: the
/// endpoint starts a pool for the correlation id and holds it in the stale one's place.
#[tokio::test]
async fn the_endpoint_replaces_a_stale_pool() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let partition = RepositoryId::default();
            let (endpoint, connection) = counted_endpoint(partition).await;
            let stale = held_pool("c1");
            endpoint.pools.hold(partition, stale.clone());
            lore_transport::StorageSession::pooled(stale.clone())
                .invalidate()
                .await;

            let answered = endpoint
                .session_pool(&partition, "c1")
                .await
                .expect("the endpoint starts a pool in place of the stale one");
            assert_eq!(connection.storage_session_starts(), 1);
            assert!(!Arc::ptr_eq(&answered, &stale));
            assert!(
                endpoint
                    .pools
                    .get(partition, "c1")
                    .is_some_and(|held| Arc::ptr_eq(&held, &answered)),
                "the endpoint holds the pool it started in place of the stale one"
            );
        })
        .await;
}

/// A link context keeps the local reads and the endpoint of the context it is derived from, and
/// asks the endpoint for a pool on the link's partition.
#[tokio::test]
async fn a_link_context_keeps_the_local_reads_and_the_endpoint() {
    type Build = fn(RepositoryContext) -> RepositoryContext;
    let builds: [(Build, (bool, bool, bool)); 2] = [
        (RepositoryContext::reading_remote_only, (false, true, false)),
        (RepositoryContext::strict_local_reads, (true, false, true)),
    ];
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for (build, expected) in builds {
                let link_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
                let (endpoint, connection) = counted_endpoint(link_id).await;
                let context =
                    built_context(|context| build(context).with_remote_endpoint(endpoint.clone()))
                        .await;
                let link = context.to_link_context(link_id).await;

                let options = read_options_from_repository(&link);
                assert_eq!(
                    (options.local, options.no_local_write, options.strict_local),
                    expected
                );
                let pool = link
                    .session_pool("c1")
                    .await
                    .expect("the endpoint answers for the link's partition");
                assert_eq!(connection.storage_session_starts(), 1);
                assert!(
                    endpoint
                        .pools
                        .get(link_id, "c1")
                        .is_some_and(|held| Arc::ptr_eq(&held, &pool))
                );
            }
        })
        .await;
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
