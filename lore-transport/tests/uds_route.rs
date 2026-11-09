// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests of a process that names its socket in `LORE_UDS_SOCKET`. The variable is read once per
//! process, so they run in a process of their own that sets it before anything reads it.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_base::types::Partition;
use lore_transport::auth::AuthEndpoint;
use lore_transport::connection::SuppliedCredentials;
use lore_transport::local_socket;
use lore_transport::quic::storage_service::Command;
use lore_transport::uds::CLIENT_HELLO_SIZE;
use lore_transport::uds::MAX_MESSAGE;
use lore_transport::uds::SOCKET_ENV;
use lore_transport::uds::ServerHello;
use lore_transport::uds::VERSION;
use lore_transport::uds::io::FrameReader;
use lore_transport::uds::io::Message;
use lore_transport::uds::io::SocketWriter;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

const PATIENCE: Duration = Duration::from_secs(5);

static SOCKET_DIR: Mutex<Option<TempDir>> = Mutex::new(None);

/// Held by each test that listens on the process's socket, which one listener holds at a time.
static LISTENING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A constructor because the environment is process-wide: this runs before the harness starts
/// the threads that would make writing to it a data race.
#[ctor::ctor]
fn name_the_socket() {
    let dir = TempDir::new("lore-uds-route-");
    // Safety: constructors run before `main`, so this is the single-threaded window where writing
    // to the environment has no reader to race.
    unsafe { std::env::set_var(SOCKET_ENV, dir.child("lore.sock")) };
    *SOCKET_DIR.lock().unwrap() = Some(dir);
}

#[ctor::dtor]
fn remove_the_socket_dir() {
    if let Ok(mut held) = SOCKET_DIR.lock() {
        drop(held.take());
    }
}

/// Listens at `path`, on the runtime of the test that calls this.
fn listen(path: &Path) -> local_socket::AsyncListener {
    local_socket::Claim::acquire(path)
        .unwrap()
        .expect("the path is unclaimed")
        .listen(0o600)
        .unwrap()
        .into_async()
        .unwrap()
}

/// Accepts one connection on `path` and answers its first request, a session start, with the
/// server's session id 7. Returns the command the request carried.
#[allow(clippy::disallowed_methods)] // A test-local task, outside any Lore context.
fn answer_one_session_start(path: PathBuf) -> tokio::task::JoinHandle<u8> {
    let listener = listen(&path);
    tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut hello = [0u8; CLIENT_HELLO_SIZE];
        stream.read_exact(&mut hello).await.unwrap();
        let answer = ServerHello {
            version: VERSION,
            handler_timeout_seconds: 100,
            max_message: MAX_MESSAGE as u32,
            credits: 16,
            credit_bytes: 1 << 24,
        };
        stream.write_all(&answer.to_bytes()).await.unwrap();
        let (read, write) = stream.into_split();
        let writer = SocketWriter::new(write, None, None);
        tokio::spawn(writer.clone().drain());
        let mut reader = FrameReader::new(read, MAX_MESSAGE, false);
        let (mut header, _) = reader.next().await.unwrap().expect("a request");
        let session = Bytes::copy_from_slice(&7u32.to_le_bytes());
        header.size_or_status = session.len() as u32;
        let mut message = Message::new(header);
        message.payload(session);
        assert!(writer.send(message));
        // The drain task keeps the write side, and so the connection, open for the client.
        header.cmd
    })
}

/// A `lore-unix` remote is accepted once the process names a socket, and its storage reaches the
/// server through that socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lore_unix_remote_reaches_the_named_socket() {
    let _listening = LISTENING.lock().await;
    let path = lore_transport::uds::socket().expect("the process names a socket");
    assert_eq!(lore_transport::uds::route(), Some(path));
    assert!(lore_transport::connection::parse("lore-unix://example.com/repository").is_ok());
    let serving = answer_one_session_start(path.to_path_buf());

    let storage = lore_transport::uds::storage(
        "lore-unix://example.com/repository",
        &AuthEndpoint::none(),
        "",
        &Arc::new(SuppliedCredentials::default()),
    )
    .await
    .unwrap();
    let session = timeout(
        PATIENCE,
        storage.session_start(Partition::default(), "routed"),
    )
    .await
    .expect("the session start is answered");

    assert!(session.is_ok(), "{session:?}");
    assert_eq!(
        timeout(PATIENCE, serving).await.unwrap().unwrap(),
        Command::Authorize as u8
    );
}

/// Starts a gRPC connection to `remote` on a task of the test's own, whatever its outcome.
#[allow(clippy::disallowed_methods)] // A test-local task, outside any Lore context.
fn connect_grpc(remote: &'static str) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = lore_transport::grpc::connect(Weak::new(), remote, false).await;
    })
}

/// A gRPC channel to a remote that authenticates its server goes over the socket in plaintext
/// HTTP/2, not TLS: the server forwards it and secures the way to the remote itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grpc_channel_to_a_tls_remote_speaks_plaintext_on_the_socket() {
    let _listening = LISTENING.lock().await;
    let path = lore_transport::uds::socket().expect("the process names a socket");
    let listener = listen(path);

    let connecting = connect_grpc("lores://example.com");
    let mut accepted = timeout(PATIENCE, listener.accept())
        .await
        .expect("the channel dials the socket")
        .unwrap();
    let mut preface = [0u8; 24];
    timeout(PATIENCE, accepted.read_exact(&mut preface))
        .await
        .expect("the channel writes its first bytes")
        .unwrap();
    connecting.abort();

    assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
}

/// A process that names a socket opens one storage connection, whatever it asks for: every
/// storage client of its remote shares the socket's one.
#[test]
fn a_process_that_names_a_socket_opens_one_storage_connection() {
    for requested in [0, 1, 4, 100] {
        assert_eq!(
            lore_transport::connection::storage_connection_count(requested),
            1,
            "{requested} requested"
        );
    }
}
