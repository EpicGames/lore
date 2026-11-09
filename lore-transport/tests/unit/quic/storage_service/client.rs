// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A storage client one of whose streams stops carrying requests while its connection stays up.

use std::sync::Arc;
use std::time::Duration;

use lore_transport::error::ProtocolError;
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
