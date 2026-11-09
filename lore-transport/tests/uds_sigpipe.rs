// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Writes to a socket whose peer has gone, in a process that leaves `SIGPIPE` at its default
//! action, as a host of the library may. They must fail with `EPIPE` rather than raise the signal,
//! which would end the process. A binary of its own, as it changes how the process takes the
//! signal.
#![cfg(unix)]

use std::io::IoSlice;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_transport::local_socket;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::uds::SocketConnector;
use lore_transport::uds::io::Message;
use lore_transport::uds::io::SocketWriter;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use tower::Service;

const PATIENCE: Duration = Duration::from_secs(5);

/// Restores the default action of `SIGPIPE`, which the Rust runtime ignores, so that a write that
/// raises it ends this process.
fn default_sigpipe() {
    // SAFETY: the default disposition installs no handler.
    let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    assert_ne!(previous, libc::SIG_ERR);
}

/// A listener that blocks, at a socket in `dir`, and its path.
fn listen_blocking_in(dir: &TempDir) -> (local_socket::Listener, PathBuf) {
    let path = dir.path().join("sigpipe.sock");
    let listener = local_socket::Claim::acquire(&path)
        .unwrap()
        .expect("the path is unclaimed")
        .listen(0o600)
        .unwrap();
    (listener, path)
}

/// A listener on the runtime, at a socket in `dir`, and its path.
fn listen_in(dir: &TempDir) -> (local_socket::AsyncListener, PathBuf) {
    let (listener, path) = listen_blocking_in(dir);
    (listener.into_async().unwrap(), path)
}

/// A connection that blocks, as the service and its clients write on, fails a write to a peer that
/// has gone rather than raise the signal.
#[test]
fn a_blocking_stream_fails_its_writes_on_a_peer_that_has_gone() {
    default_sigpipe();
    let dir = TempDir::new("lore-uds-sigpipe-");
    let (listener, path) = listen_blocking_in(&dir);
    let mut connected = local_socket::Stream::connect(&path).unwrap();
    drop(listener.accept().unwrap());

    let failed = loop {
        if let Err(err) = connected.write_all(b"PRI * HTTP/2.0\r\n") {
            break err;
        }
    };

    assert_eq!(failed.kind(), std::io::ErrorKind::BrokenPipe, "{failed}");
}

/// Whether a storage writer on `stream`, whose peer has gone, closes within [`PATIENCE`].
async fn the_writer_closes(stream: local_socket::AsyncStream) -> bool {
    let (_read, write) = stream.into_split();
    let writer = SocketWriter::new(write, None, None);
    timeout(PATIENCE, async {
        while writer.send(Message::new(CommandHeader::new_with_session(0, 1, 0, 0))) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn the_storage_writer_closes_on_a_peer_that_has_gone() {
    default_sigpipe();
    let dir = TempDir::new("lore-uds-sigpipe-");
    let (listener, path) = listen_in(&dir);
    let (accepted, connected) = tokio::join!(listener.accept(), local_socket::connect(&path));
    drop(accepted.unwrap());

    assert!(
        the_writer_closes(connected.unwrap()).await,
        "the writer closes once its peer has gone"
    );
}

/// An accepted connection, as a server writes on, takes its socket options from its listener.
#[tokio::test]
async fn the_storage_writer_on_an_accepted_connection_closes_on_a_peer_that_has_gone() {
    default_sigpipe();
    let dir = TempDir::new("lore-uds-sigpipe-");
    let (listener, path) = listen_in(&dir);
    let (accepted, connected) = tokio::join!(listener.accept(), local_socket::connect(&path));
    drop(connected.unwrap());

    assert!(
        the_writer_closes(accepted.unwrap()).await,
        "the writer closes once its peer has gone"
    );
}

#[tokio::test]
async fn a_channel_stream_fails_its_vectored_writes_on_a_peer_that_has_gone() {
    default_sigpipe();
    let dir = TempDir::new("lore-uds-sigpipe-");
    let (listener, path) = listen_in(&dir);
    let path: &'static Path = Box::leak(path.into_boxed_path());
    let mut connector = SocketConnector::new(path);
    let (accepted, connected) = tokio::join!(
        listener.accept(),
        connector.call(http::Uri::from_static("http://localhost"))
    );
    drop(accepted.unwrap());
    let mut stream = connected.unwrap().into_inner();

    let failed = timeout(PATIENCE, async {
        loop {
            let slices = [IoSlice::new(b"PRI * HTTP/2.0"), IoSlice::new(b"\r\n")];
            if let Err(err) = stream.write_vectored(&slices).await {
                return err;
            }
        }
    })
    .await
    .expect("a write fails once its peer has gone");

    assert_eq!(failed.kind(), std::io::ErrorKind::BrokenPipe, "{failed}");
}
