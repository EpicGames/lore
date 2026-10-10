// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_transport::error::ProtocolError;
use lore_transport::session::*;

/// A lazy session whose resolver counts its calls and always fails with `error`, so how
/// often it is asked is what the test reads and what it resolves to is out of the way.
fn counting_session(calls: Arc<AtomicUsize>, error: ProtocolError) -> StorageSession {
    StorageSession::pending(move || {
        let calls = calls.clone();
        let error = error.clone();
        async move {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(error)
        }
    })
}

/// One resolution serves every operation, a failure the caller cannot retry past being
/// held like a success.
#[tokio::test]
async fn a_lazy_session_resolves_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = counting_session(calls.clone(), ProtocolError::internal("nothing to resolve"));

    assert!(session.is_lazy());
    assert!(session.partition().await.is_err());
    assert!(session.partition().await.is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

/// The read path recovers a rotated server session map by invalidating the
/// session and retrying that same session, which only gets a `session_id` the
/// server knows about where the session resolves again.
#[tokio::test]
async fn an_invalidated_lazy_session_resolves_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = counting_session(calls.clone(), ProtocolError::internal("nothing to resolve"));

    assert!(session.partition().await.is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    session.invalidate().await;

    assert!(session.partition().await.is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

/// The read and write paths back off on `SlowDown` and retry the same session without
/// invalidating it, so a throttled `session_start` has to be asked again for the retry to
/// reach the server at all.
#[tokio::test]
async fn a_throttled_lazy_session_resolves_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = counting_session(
        calls.clone(),
        ProtocolError::from(lore_base::error::SlowDown),
    );

    let first = session.partition().await;
    let second = session.partition().await;

    assert!(first.is_err_and(|err| err.is_slow_down()));
    assert!(second.is_err_and(|err| err.is_slow_down()));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

mod pooled {
    use std::time::Duration;

    use lore_base::lore_spawn_net;
    use lore_transport::connection::Connection;
    use tokio::task::JoinSet;

    use super::*;
    use crate::quic::storage_service::test_server::PAYLOAD_SIZE;
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_address as address;
    use crate::quic::storage_service::test_server::test_partition as partition;

    /// A pool of one resolved session per storage connection to `server`.
    async fn pool_over(server: &TestStorageServer, connections: usize) -> Arc<SessionPool> {
        let connection = Connection::detached(Vec::new());
        let correlation: Arc<str> = Arc::from("test");
        let sessions = server
            .connect_with_sessions(connections)
            .await
            .into_iter()
            .map(|(storage, session_id)| {
                Arc::new(StorageSession::resolved(
                    storage,
                    connection.clone(),
                    session_id,
                    partition(),
                    correlation.clone(),
                ))
            })
            .collect();
        Arc::new(SessionPool::new(sessions))
    }

    /// A lazy session resolving to a session over `pool`, counting its resolutions in `calls`.
    fn lazy_over(pool: Arc<SessionPool>, calls: Arc<AtomicUsize>) -> StorageSession {
        StorageSession::pending(move || {
            calls.fetch_add(1, Ordering::Relaxed);
            let pool = pool.clone();
            async move { Ok(Arc::new(StorageSession::pooled(pool))) }
        })
    }

    /// Fetches `count` distinct addresses through `session`, all of them in flight at once.
    async fn fetch_concurrently(session: &Arc<StorageSession>, count: usize) {
        let mut fetches = JoinSet::new();
        for index in 0..count {
            let session = session.clone();
            lore_spawn_net!(fetches, async move { session.get(&address(index)).await });
        }
        while let Some(fetched) = fetches.join_next().await {
            let (_, payload) = fetched.expect("fetch task").expect("fetch");
            assert_eq!(payload.len(), PAYLOAD_SIZE);
        }
    }

    /// The read paths hold one lazy session per repository for its lifetime. Resolving it to
    /// a pooled session is what spreads those reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lazy_session_resolved_to_a_pool_spreads_requests() {
        let server = TestStorageServer::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = lazy_over(pool_over(&server, 4).await, calls.clone());

        for index in 0..400 {
            session.get(&address(index)).await.expect("fetch");
        }

        assert_eq!(server.gets_per_connection(), vec![100, 100, 100, 100]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    /// Every operation of a resolved lazy session reads what it resolved to without the lock its
    /// resolutions take.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_resolved_lazy_session_serves_while_its_resolution_lock_is_held() {
        let server = TestStorageServer::start();
        let session = lazy_over(pool_over(&server, 2).await, Arc::default());
        session.get(&address(0)).await.expect("fetch");

        let _held = session.hold_resolution().await.expect("a lazy session");
        tokio::time::timeout(Duration::from_secs(10), session.get(&address(1)))
            .await
            .expect("an operation waited for the resolution lock")
            .expect("fetch");

        assert_eq!(server.gets_per_connection(), vec![1, 1]);
    }

    /// Invalidating a resolved lazy session drops what it resolved to, so the next operation
    /// resolves again.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalidated_resolved_lazy_session_resolves_again() {
        let server = TestStorageServer::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = lazy_over(pool_over(&server, 2).await, calls.clone());
        session.get(&address(0)).await.expect("fetch");

        session.invalidate().await;
        session
            .get(&address(1))
            .await
            .expect("fetch after the invalidation");

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// Requests in flight at once are served by every connection at once: the server holds each
    /// until every connection has one, so the fetches finish only when the pool spreads them.
    #[tokio::test(flavor = "multi_thread")]
    async fn requests_in_flight_are_served_by_every_connection_at_once() {
        const CONNECTIONS: usize = 4;
        let server = TestStorageServer::start_gathering(CONNECTIONS);
        let session = Arc::new(StorageSession::pooled(
            pool_over(&server, CONNECTIONS).await,
        ));

        tokio::time::timeout(
            Duration::from_secs(30),
            fetch_concurrently(&session, 16 * CONNECTIONS),
        )
        .await
        .expect("a connection served no request while the others waited for it");

        assert_eq!(server.gets_per_connection(), vec![16; CONNECTIONS]);
    }
}
