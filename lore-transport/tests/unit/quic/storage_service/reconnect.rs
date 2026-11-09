// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Sessions across a reconnect: a server numbers each connection's sessions from 1, so a session
//! the client started on one connection is issued again on the next before a command runs under
//! it, and an id from the first never reaches the second.

use lore_base::lore_spawn_net;
use lore_base::types::Partition;
use lore_transport::traits::Storage;
use tokio::task::JoinSet;

use super::test_server::SessionStarts;
use super::test_server::TestStorageServer;
use super::test_server::test_address;
use super::test_server::test_partition;
use super::test_server::wait_until;

/// A partition other than [`test_partition`].
fn other_partition() -> Partition {
    Partition::from([0x5du8; 16])
}

/// A command on a session after its connection is lost reconnects, issues the session on the new
/// connection, and runs under it there, as a priority command after it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_after_a_reconnect_runs_under_its_session_issued_again() {
    let server = TestStorageServer::start();
    let storage = server.connect(1).await.remove(0);
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    storage.get(session, &test_address(0)).await.expect("fetch");

    storage.close().await;
    storage
        .get(session, &test_address(1))
        .await
        .expect("fetch after the reconnect");
    storage
        .get_priority(session, &test_address(2))
        .await
        .expect("priority fetch after the reconnect");

    assert_eq!(server.started_sessions_per_connection(), vec![1, 1]);
    assert_eq!(server.gets_per_connection(), vec![1, 2]);
}

/// Sessions issued again after a reconnect each run their commands under their own partition, in
/// whatever order they are issued: the first issued on the new connection takes the id another
/// held on the old one, which that other's command must not run under.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_issued_again_after_a_reconnect_keep_their_own_partitions() {
    let server = TestStorageServer::start();
    let storage = server.connect(1).await.remove(0);
    let first = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    let second = storage
        .session_start(other_partition(), "test")
        .await
        .expect("session start");
    storage.get(first, &test_address(0)).await.expect("fetch");

    storage.close().await;
    storage
        .get(second, &test_address(1))
        .await
        .expect("fetch on the second session");
    storage
        .get(first, &test_address(2))
        .await
        .expect("fetch on the first session");

    assert_eq!(
        server.served_partitions_per_connection(),
        vec![
            vec![test_partition()],
            vec![other_partition(), test_partition()]
        ]
    );
}

/// Commands that find their session stale at once after a reconnect issue it on the new connection
/// once between them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_after_a_reconnect_issue_their_session_once() {
    const COMMANDS: usize = 32;
    let server = TestStorageServer::start();
    let storage = server.connect(1).await.remove(0);
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    storage.get(session, &test_address(0)).await.expect("fetch");

    storage.close().await;
    let mut fetches = JoinSet::new();
    for index in 1..=COMMANDS {
        let storage = storage.clone();
        lore_spawn_net!(fetches, async move {
            storage.get(session, &test_address(index)).await
        });
    }
    while let Some(fetched) = fetches.join_next().await {
        fetched
            .expect("fetch task")
            .expect("fetch after the reconnect");
    }

    assert_eq!(server.started_sessions_per_connection(), vec![1, 1]);
    assert_eq!(server.gets_per_connection(), vec![1, COMMANDS as u64]);
}

/// Stopping a session the client started before a reconnect, and has not issued since, stops
/// nothing on the new connection, where its old id names another session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_stopped_after_a_reconnect_stops_no_session_of_the_new_connection() {
    let server = TestStorageServer::start();
    let storage = server.connect(1).await.remove(0);
    let first = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    let second = storage
        .session_start(other_partition(), "test")
        .await
        .expect("session start");

    storage.close().await;
    storage
        .get(second, &test_address(0))
        .await
        .expect("fetch on the second session");
    storage.session_stop(first).await.expect("session stop");
    storage
        .get(second, &test_address(1))
        .await
        .expect("fetch on the second session after the first stopped");

    assert_eq!(server.live_sessions_per_connection()[1], 1);
}

/// A command on a session the client stopped is refused without reaching the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_on_a_stopped_session_is_not_sent() {
    let server = TestStorageServer::start();
    let storage = server.connect(1).await.remove(0);
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    storage.session_stop(session).await.expect("session stop");

    assert!(storage.get(session, &test_address(0)).await.is_err());
    assert_eq!(server.received_gets_per_connection(), vec![0]);
}

/// A session issued again on the connection a reconnect opens, then stopped, leaves no session
/// open on that connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_issued_again_and_stopped_leaves_no_session_open() {
    let server = TestStorageServer::start();
    let storage = server.connect(1).await.remove(0);
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");

    storage.close().await;
    storage
        .get(session, &test_address(0))
        .await
        .expect("fetch after the reconnect");
    storage.session_stop(session).await.expect("session stop");

    assert_eq!(server.live_sessions_per_connection()[1], 0);
}

/// A stop while its session is issued again waits for the issue and stops the session it issued,
/// so none is left open on the connection the reconnect opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_while_its_session_is_issued_again_leaves_no_session_open() {
    let (server, release) = TestStorageServer::start_holding_session_starts();
    release
        .send(true)
        .expect("the server answers the first start");
    let storage = server.connect(1).await.remove(0);
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    release
        .send(false)
        .expect("the server holds the next start");

    storage.close().await;
    let fetching = {
        let storage = storage.clone();
        lore_spawn_net!(async move { storage.get(session, &test_address(0)).await })
    };
    wait_until(
        || server.started_sessions_per_connection() == [1, 1],
        "the session was not issued again",
    )
    .await;
    let stopping = {
        let storage = storage.clone();
        lore_spawn_net!(async move { storage.session_stop(session).await })
    };
    release
        .send(true)
        .expect("the server answers the held start");
    let _ = fetching.await.expect("fetch task");
    stopping.await.expect("stop task").expect("session stop");

    assert_eq!(server.live_sessions_per_connection()[1], 0);
}

/// An issue the server refuses after a reconnect reaches the command as the server's `SlowDown`,
/// for it to back off on, and a later command issues the session once the server accepts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_issue_reaches_the_command_and_a_later_one_issues_the_session() {
    let (server, starts) = TestStorageServer::start_controlling_session_starts();
    let storage = server.connect(1).await.remove(0);
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    starts
        .send(SessionStarts::Refuse)
        .expect("the server refuses starts");

    storage.close().await;
    let refused = storage
        .get(session, &test_address(0))
        .await
        .expect_err("the issue is refused");
    assert!(refused.is_slow_down(), "{refused}");
    starts
        .send(SessionStarts::Accept)
        .expect("the server accepts starts");
    storage
        .get(session, &test_address(1))
        .await
        .expect("fetch once the server accepts");

    assert_eq!(server.started_sessions_per_connection(), vec![1, 1]);
    assert_eq!(server.gets_per_connection(), vec![0, 1]);
}

/// Commands that find their session stale while its issue waits on the server share that issue,
/// and each receives the refusal it ends in, the server asked to start the session once. The
/// server refuses once every command awaits the issue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_waiting_for_a_refused_issue_each_receive_its_refusal() {
    const COMMANDS: usize = 32;
    let (server, starts) = TestStorageServer::start_controlling_session_starts();
    let storage = server.connect_client().await;
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    starts
        .send(SessionStarts::Hold)
        .expect("the server holds starts");

    storage.close().await;
    let mut fetches = JoinSet::new();
    for index in 0..COMMANDS {
        let storage = storage.clone();
        lore_spawn_net!(fetches, async move {
            storage.get(session, &test_address(index)).await
        });
    }
    wait_until(
        || storage.awaiting_issue(session) == COMMANDS,
        "the commands did not all await the session's issue",
    )
    .await;
    starts
        .send(SessionStarts::Refuse)
        .expect("the server refuses the held start");
    while let Some(fetched) = fetches.join_next().await {
        let refused = fetched
            .expect("fetch task")
            .expect_err("the issue is refused");
        assert!(refused.is_slow_down(), "{refused}");
    }

    assert_eq!(server.requested_session_starts_per_connection(), vec![1, 1]);
}

/// An issue whose command is dropped while the server holds it runs to its end, so a later
/// command runs under the session it issued and the server starts no other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_issue_whose_command_is_dropped_issues_the_session() {
    let (server, starts) = TestStorageServer::start_controlling_session_starts();
    let storage = server.connect(1).await.remove(0);
    let session = storage
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    starts
        .send(SessionStarts::Hold)
        .expect("the server holds starts");

    storage.close().await;
    let fetching = {
        let storage = storage.clone();
        lore_spawn_net!(async move { storage.get(session, &test_address(0)).await })
    };
    wait_until(
        || server.requested_session_starts_per_connection() == [1, 1],
        "the session was not issued again",
    )
    .await;
    fetching.abort();
    assert!(fetching.await.is_err_and(|join| join.is_cancelled()));
    starts
        .send(SessionStarts::Accept)
        .expect("the server answers the held start");
    storage
        .get(session, &test_address(1))
        .await
        .expect("fetch after the dropped command");

    assert_eq!(server.started_sessions_per_connection(), vec![1, 1]);
    assert_eq!(server.gets_per_connection(), vec![0, 1]);
}
