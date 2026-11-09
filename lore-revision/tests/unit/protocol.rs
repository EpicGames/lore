// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::poll;
use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_base::retry::JITTER_CLAMP_MS;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::lore::RepositoryId;
use lore_revision::protocol::RemoteConnections;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::connection::RECONNECT_START_DELAY;

use crate::helper::setup_test_execution;

type Connected = BoxFuture<'static, Result<Arc<Connection>, ProtocolError>>;

/// How many callers take a connection at once.
const CALLERS: usize = 8;

/// The connection [`CALLERS`] callers taking one for `repository` at once are handed, the same for
/// every one of them.
async fn handed_to_callers(
    connections: &RemoteConnections,
    repository: RepositoryId,
) -> Arc<Connection> {
    let handed =
        futures::future::join_all((0..CALLERS).map(|_| connections.connection(repository))).await;
    let connection = handed[0].as_ref().expect("a connection").clone();
    assert!(
        handed.iter().all(|other| other
            .as_ref()
            .is_ok_and(|other| Arc::ptr_eq(other, &connection))),
        "the callers were handed different connections"
    );
    connection
}

/// Callers are handed a connection marked stale within its connect's back-off until the back-off
/// passes, and then share one connect in its place. A replacement marked stale as soon has twice
/// the back-off.
#[tokio::test(start_paused = true)]
async fn callers_are_handed_a_connection_marked_stale_until_its_back_off_passes() {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    fn connecting(_: Arc<str>, _: Arc<str>, _: RepositoryId, _: Option<usize>) -> Connected {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        std::future::ready(Ok(Connection::detached(Vec::new()))).boxed()
    }
    let connections = RemoteConnections::with_connect(connecting);
    let repository = RepositoryId::from([7u8; 16]);
    let back_off = Duration::from_millis(RECONNECT_START_DELAY);
    let jitter = Duration::from_millis(JITTER_CLAMP_MS);

    let first = handed_to_callers(&connections, repository).await;
    first.stale.store(true, Ordering::Relaxed);
    assert!(Arc::ptr_eq(
        &first,
        &handed_to_callers(&connections, repository).await
    ));
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 1);

    tokio::time::advance(back_off + jitter).await;
    let second = handed_to_callers(&connections, repository).await;
    assert!(!Arc::ptr_eq(&first, &second));
    second.stale.store(true, Ordering::Relaxed);
    tokio::time::advance(back_off + jitter).await;
    assert!(Arc::ptr_eq(
        &second,
        &handed_to_callers(&connections, repository).await
    ));
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 2);

    tokio::time::advance(back_off).await;
    let third = handed_to_callers(&connections, repository).await;
    assert!(!Arc::ptr_eq(&second, &third));
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 3);
}

/// A connection marked stale once its connect's back-off has passed is replaced at once, and the
/// replacement's back-off starts again from the first interval.
#[tokio::test(start_paused = true)]
async fn a_connection_marked_stale_after_its_back_off_is_replaced_at_once() {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    fn connecting(_: Arc<str>, _: Arc<str>, _: RepositoryId, _: Option<usize>) -> Connected {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        std::future::ready(Ok(Connection::detached(Vec::new()))).boxed()
    }
    let connections = RemoteConnections::with_connect(connecting);
    let repository = RepositoryId::from([7u8; 16]);
    let after_back_off = Duration::from_millis(RECONNECT_START_DELAY + JITTER_CLAMP_MS);

    let first = handed_to_callers(&connections, repository).await;
    tokio::time::advance(after_back_off).await;
    first.stale.store(true, Ordering::Relaxed);
    let second = handed_to_callers(&connections, repository).await;
    assert!(!Arc::ptr_eq(&first, &second));
    second.stale.store(true, Ordering::Relaxed);
    tokio::time::advance(after_back_off).await;
    let third = handed_to_callers(&connections, repository).await;

    assert!(!Arc::ptr_eq(&second, &third));
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 3);
}

/// A connection whose storage was refused its credentials is replaced at once when marked stale,
/// its refusal answered to no later caller.
#[tokio::test(start_paused = true)]
async fn a_connection_refused_its_credentials_is_replaced_at_once() {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    fn refused(_: Arc<str>, _: Arc<str>, _: RepositoryId, _: Option<usize>) -> Connected {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        let refusal = Err(ProtocolError::from(NotAuthorized));
        std::future::ready(Ok(Connection::detached_ready(Vec::new(), refusal))).boxed()
    }
    let connections = RemoteConnections::with_connect(refused);
    let repository = RepositoryId::from([7u8; 16]);

    let first = handed_to_callers(&connections, repository).await;
    first.stale.store(true, Ordering::Relaxed);
    let second = handed_to_callers(&connections, repository).await;

    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 2);
}

/// A connect refused its credentials is answered to the callers waiting on it and to no later one.
#[tokio::test]
async fn a_refused_connect_is_answered_to_no_later_caller() {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    fn refusing(_: Arc<str>, _: Arc<str>, _: RepositoryId, _: Option<usize>) -> Connected {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Err(ProtocolError::from(NotAuthorized))
        }
        .boxed()
    }
    let connections = RemoteConnections::with_connect(refusing);
    let repository = RepositoryId::from([7u8; 16]);

    let answers =
        futures::future::join_all((0..CALLERS).map(|_| connections.connection(repository))).await;
    assert!(answers.iter().all(Result::is_err));
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 1);
    assert!(connections.connection(repository).await.is_err());

    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 2);
}

/// Callers waiting on a repository's connect share it: the connect's failure answers all of them,
/// and none of them starts another.
#[tokio::test]
async fn callers_waiting_on_a_connect_share_it() {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    fn failing_late(_: Arc<str>, _: Arc<str>, _: RepositoryId, _: Option<usize>) -> Connected {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Err(ProtocolError::from(Disconnected))
        }
        .boxed()
    }
    let connections = RemoteConnections::with_connect(failing_late);
    let repository = RepositoryId::from([7u8; 16]);

    let answers =
        futures::future::join_all((0..3).map(|_| connections.connection(repository))).await;

    assert!(answers.iter().all(Result::is_err));
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 1);
}

/// A connect whose caller settles it after a newer connect for the repository has started leaves
/// the newer one in place, so the newer one's connection is what later callers are handed.
#[tokio::test(start_paused = true)]
async fn a_superseded_connect_leaves_its_successor_in_place() {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    fn failing_then_connecting(
        _: Arc<str>,
        _: Arc<str>,
        _: RepositoryId,
        _: Option<usize>,
    ) -> Connected {
        let attempt = ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if attempt == 0 {
                Err(ProtocolError::from(Disconnected))
            } else {
                Ok(Connection::detached(Vec::new()))
            }
        }
        .boxed()
    }
    let connections = RemoteConnections::with_connect(failing_then_connecting);
    let repository = RepositoryId::from([7u8; 16]);
    let back_off = Duration::from_millis(RECONNECT_START_DELAY + JITTER_CLAMP_MS);

    let mut superseded = pin!(connections.connection(repository));
    assert!(poll!(superseded.as_mut()).is_pending());
    assert!(connections.connection(repository).await.is_err());
    tokio::time::advance(back_off).await;
    let mut successor = pin!(connections.connection(repository));
    assert!(poll!(successor.as_mut()).is_pending());

    assert!(superseded.await.is_err());
    assert!(successor.await.is_ok());
    assert!(connections.connection(repository).await.is_ok());
    assert_eq!(ATTEMPTS.load(Ordering::Relaxed), 2);
}

/// A failed connect is answered to every caller until its back-off passes, and the failure of the
/// connect retrying it doubles the back-off.
#[tokio::test(start_paused = true)]
async fn a_failed_connect_is_retried_once_its_back_off_passes() {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    fn failing(_: Arc<str>, _: Arc<str>, _: RepositoryId, _: Option<usize>) -> Connected {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        std::future::ready(Err(ProtocolError::from(Disconnected))).boxed()
    }
    let connections = RemoteConnections::with_connect(failing);
    let repository = RepositoryId::from([7u8; 16]);
    let back_off = Duration::from_millis(RECONNECT_START_DELAY);
    let jitter = Duration::from_millis(JITTER_CLAMP_MS);

    for (wait, attempts) in [
        (Duration::ZERO, 1),
        (Duration::ZERO, 1),
        (back_off + jitter, 2),
        (back_off + jitter, 2),
        (back_off, 3),
    ] {
        tokio::time::advance(wait).await;
        assert!(connections.connection(repository).await.is_err());
        assert_eq!(
            ATTEMPTS.load(Ordering::Relaxed),
            attempts,
            "attempts after waiting {wait:?}"
        );
    }
}

/// A connect asks for the storage connections its connections were given, and for the execution's
/// count where they were given none.
#[tokio::test]
async fn a_connect_asks_for_the_storage_connections_it_was_given() {
    static OPENED: std::sync::Mutex<Vec<Option<usize>>> = std::sync::Mutex::new(Vec::new());
    fn connecting(_: Arc<str>, _: Arc<str>, _: RepositoryId, opened: Option<usize>) -> Connected {
        OPENED.lock().expect("the opened counts").push(opened);
        std::future::ready(Ok(Connection::detached(Vec::new()))).boxed()
    }
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = RepositoryId::from([7u8; 16]);

            for connections in [
                RemoteConnections::with_connect(connecting),
                RemoteConnections::with_connect(connecting).connecting_over(4),
            ] {
                connections
                    .connection(repository)
                    .await
                    .expect("a connection");
            }

            assert_eq!(*OPENED.lock().expect("the opened counts"), [None, Some(4)]);
        })
        .await;
}
