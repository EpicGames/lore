// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A storage client: the congestion controller its connection runs, and a client one of whose
//! streams stops carrying requests while its connection stays up.

use std::sync::Arc;
use std::time::Duration;

use lore_transport::error::ProtocolError;
use lore_transport::quic::client::CongestionAlgorithm;
use lore_transport::quic::client::ServiceClient;
use lore_transport::quic::storage_service::client::StorageClient;
use lore_transport::traits::Storage;

use super::test_server::StreamFault;
use super::test_server::TestStorageServer;
use super::test_server::test_address;
use super::test_server::test_partition;
use super::test_server::wait_until;

/// A client with a live session whose last request was answered on a stream the server then
/// faulted as `fault` says. The stream is the one the client's next request goes out on: requests
/// that are not high priority share the streams past the priority ones, and with none in flight
/// the lowest of those takes them.
async fn client_after_fault(server: &TestStorageServer, fault: StreamFault) -> Arc<StorageClient> {
    let client = server.connect_client().await;
    let session_id = client
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    client.get(session_id, &test_address(0)).await.expect("get");
    server.arm_fault(fault);
    client
        .get(session_id, &test_address(1))
        .await
        .expect("the last answer on the faulted stream");
    client
}

/// A session start through `client`, bounded so a request nothing will answer fails the test.
async fn start_session_after_fault(client: &StorageClient) -> Result<u32, ProtocolError> {
    tokio::time::timeout(
        Duration::from_secs(10),
        client.session_start(test_partition(), "after"),
    )
    .await
    .expect("a request was never answered")
}

/// A request on a stream the server has stopped reading reconnects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_on_a_stream_the_server_stopped_reading_reconnects() {
    let server = TestStorageServer::start();
    let client = client_after_fault(&server, StreamFault::StopReading).await;

    let started = start_session_after_fault(&client).await;
    assert!(started.is_ok(), "{started:?}");
    assert_eq!(server.started_sessions_per_connection(), [1, 1]);
}

/// A request on a stream whose reader has stopped, on an answer to a command never sent,
/// reconnects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_on_a_stream_whose_reader_stopped_reconnects() {
    let server = TestStorageServer::start();
    let client = client_after_fault(&server, StreamFault::StrayAnswer).await;
    wait_until(
        || client.quic().streams_ended() != 0,
        "the stream's reader never stopped",
    )
    .await;

    let started = start_session_after_fault(&client).await;
    assert!(started.is_ok(), "{started:?}");
    assert_eq!(server.started_sessions_per_connection(), [1, 1]);
}

/// A storage client's connection runs the congestion controller it is opened with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_client_runs_the_congestion_algorithm_it_is_opened_with() {
    let server = TestStorageServer::start();
    for algorithm in [CongestionAlgorithm::Bbr, CongestionAlgorithm::Cubic] {
        let client = server.connect_client_running(algorithm).await;

        assert_eq!(
            client.quic().running_congestion_algorithm().await,
            Some(algorithm)
        );
    }
}

/// Commands sent at once while a client's writers are held, spread over its streams, as
/// `send(quic)` sends each, then written together once the writers are released.
async fn send_held<F, Fut, T>(client: &StorageClient, commands: usize, send: F) -> Vec<T>
where
    F: Fn(Arc<lore_transport::quic::client::QuicConnection>) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let quic = client.quic().clone();
    let writers = quic.hold_writers().await.expect("open every stream");
    let sent: Vec<_> = (0..commands)
        .map(|_| lore_base::lore_spawn!(send(quic.clone())))
        .collect();
    wait_until(
        || quic.stream_inflight().iter().sum::<u64>() == commands as u64,
        "the commands were never all queued",
    )
    .await;
    drop(writers);
    let mut results = Vec::with_capacity(commands);
    for command in sent {
        results.push(
            tokio::time::timeout(Duration::from_secs(10), command)
                .await
                .expect("a command was never answered")
                .expect("the command's task"),
        );
    }
    results
}

/// The write that meets a stream the server has stopped reading fails every frame queued on the
/// stream, each with `Terminated`, while the frames on the other streams are answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_write_fails_every_frame_queued_on_its_stream() {
    use bytes::Bytes;
    use lore_transport::quic::QuicClientError;
    use lore_transport::quic::client::BoundSession;
    use lore_transport::quic::client::send_normal;
    use lore_transport::quic::storage_service::Command;

    let server = TestStorageServer::start();
    let client = client_after_fault(&server, StreamFault::StopReading).await;

    let results = send_held(&client, 24, |quic| async move {
        let mut chunks = [Bytes::new(), Bytes::from_static(b"queued")];
        send_normal(
            quic,
            Command::Put as u8,
            BoundSession::any(0),
            true,
            &mut chunks,
        )
        .await
    })
    .await;
    let failed = results
        .iter()
        .filter(|result| matches!(result, Err(QuicClientError::Terminated)))
        .count();
    assert!(failed > 1, "the failed write failed {failed} frames");
    assert!(
        results
            .iter()
            .all(|result| matches!(result, Ok(_) | Err(QuicClientError::Terminated))),
        "{results:?}"
    );
}

/// Requests whose frames a failed write fails are sent again over a new connection and answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_a_failed_write_fails_are_answered_over_a_new_connection() {
    let server = TestStorageServer::start();
    let client = client_after_fault(&server, StreamFault::StopReading).await;

    let started = send_held(&client, 24, |_| {
        let client = client.clone();
        async move { client.session_start(test_partition(), "queued").await }
    })
    .await;
    assert!(started.iter().all(Result::is_ok), "{started:?}");
    assert_eq!(server.accepted_connections(), 2);
}
