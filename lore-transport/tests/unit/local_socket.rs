// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Sockets at a path, which every platform runs the same.
#[cfg(target_family = "windows")]
mod windows;

use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use lore_base::test_util::TempDir;
use lore_transport::local_socket;
use lore_transport::local_socket::Claim;
use socket2::Domain;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

const PATIENCE: Duration = Duration::from_secs(5);

/// A socket path in `dir`, which removes the lock files claims leave there.
fn socket_in(dir: &TempDir) -> PathBuf {
    dir.path().join("test.sock")
}

fn listen(path: &Path) -> local_socket::Listener {
    Claim::acquire(path)
        .unwrap()
        .expect("the path is unclaimed")
        .listen(0o600)
        .unwrap()
}

/// Whether a file is at `path`. Not [`Path::exists`], which follows the reparse point a socket
/// file is on Windows, and finds nothing there.
fn file_at(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A socket bound at `path` the way a server that takes no claim binds it.
fn bind_unclaimed(path: &Path) -> Socket {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
    socket.bind(&SockAddr::unix(path).unwrap()).unwrap();
    socket.listen(1).unwrap();
    socket
}

#[test]
fn a_blocking_connection_carries_bytes_both_ways() {
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);
    let listener = listen(&path);

    std::thread::scope(|scope| {
        let client = scope.spawn(|| {
            let mut stream = local_socket::Stream::connect(&path).unwrap();
            stream.write_all(b"ping").unwrap();
            let mut answer = [0u8; 4];
            stream.read_exact(&mut answer).unwrap();
            answer
        });
        let mut served = listener.accept().unwrap();
        let mut request = [0u8; 4];
        served.read_exact(&mut request).unwrap();
        served.write_all(b"pong").unwrap();

        assert_eq!(&request, b"ping");
        assert_eq!(&client.join().unwrap(), b"pong");
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_async_connection_carries_bytes_both_ways() {
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);
    let listener = listen(&path).into_async().unwrap();

    // The accept waits first, so the connection has to wake it.
    let (served, client) = timeout(PATIENCE, async {
        tokio::join!(listener.accept(), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            local_socket::connect(&path).await
        })
    })
    .await
    .expect("the connection is accepted");
    let (mut served, mut client) = (served.unwrap(), client.unwrap());
    client.write_all(b"ping").await.unwrap();
    let mut request = [0u8; 4];
    served.read_exact(&mut request).await.unwrap();
    // The halves the storage socket's reader and writer take.
    let (_read, mut write) = served.into_split();
    write.write_all(b"pong").await.unwrap();
    let mut answer = [0u8; 4];
    client.read_exact(&mut answer).await.unwrap();

    assert_eq!(&request, b"ping");
    assert_eq!(&answer, b"pong");
    drop(listener);
    assert!(!file_at(&path), "the listener removes its socket file");
    assert!(
        Claim::acquire(&path).unwrap().is_some(),
        "the listener gives up its claim"
    );
}

/// A second server must learn that it would lose the socket before it does anything else.
#[test]
fn a_claimed_path_refuses_a_second_claim_until_its_listener_drops() {
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);

    let claim = Claim::acquire(&path)
        .unwrap()
        .expect("the first claim succeeds");
    let while_claimed = Claim::acquire(&path).unwrap().is_none();
    let listener = claim.listen(0o600).unwrap();
    let while_listening = Claim::acquire(&path).unwrap().is_none();
    drop(listener);
    let removed = !file_at(&path);
    let freed = Claim::acquire(&path).unwrap().is_some();

    assert!(while_claimed, "a claimed path refuses a second claim");
    assert!(while_listening, "a listening path refuses a claim");
    assert!(removed, "the listener removes its socket file");
    assert!(freed, "the path is claimable once its listener drops");
}

/// A server from a build that takes no claim listens without one: binding would remove the file
/// it listens on, which Windows allows, and leave it running unreachable.
#[test]
fn a_socket_served_without_a_claim_refuses_a_claim() {
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);
    let _served = bind_unclaimed(&path);

    assert!(
        Claim::acquire(&path).unwrap().is_none(),
        "a served socket refuses a claim"
    );
}

#[test]
fn a_socket_file_nothing_answers_on_is_replaced() {
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);
    drop(bind_unclaimed(&path));
    assert!(file_at(&path), "a closed socket leaves its file");

    let listener = listen(&path);
    let _client = local_socket::Stream::connect(&path).unwrap();

    assert!(listener.accept().is_ok(), "the new socket serves");
}

/// A server starting or stopping retries around a connect, so a connect that waited for one
/// would use a whole wait on a single attempt.
#[test]
fn connecting_with_nothing_listening_fails_at_once() {
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);
    let started = Instant::now();

    let result = local_socket::Stream::connect(&path);

    assert!(result.is_err(), "nothing listens on {}", path.display());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the connect took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_async_connect_with_nothing_listening_fails() {
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);

    let result = timeout(PATIENCE, local_socket::connect(&path))
        .await
        .expect("the connect reports");

    assert!(result.is_err(), "nothing listens on {}", path.display());
}

/// A path grows with its directory and with names taken from configuration, so one too long for
/// an address is refused rather than cut short, which would name another socket.
#[test]
fn a_path_longer_than_an_address_holds_is_refused() {
    let dir = TempDir::new("lore-local-socket-");
    let path = dir.path().join("n".repeat(200));

    let connect = local_socket::Stream::connect(&path)
        .err()
        .expect("the connect is refused");
    let listen = Claim::acquire(&path)
        .unwrap()
        .expect("nothing answers on the path")
        .listen(0o600)
        .err()
        .expect("the bind is refused");

    for error in [connect, listen] {
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
    }
}

/// A path too long for an address can never be bound, so a listen there removes nothing at it.
#[test]
fn a_listen_at_a_path_too_long_for_an_address_keeps_the_file_there() {
    let dir = TempDir::new("lore-local-socket-");
    let path = dir.path().join("n".repeat(200));
    std::fs::write(&path, b"kept").unwrap();

    let listen = Claim::acquire(&path)
        .unwrap()
        .expect("nothing answers on the path")
        .listen(0o600);

    assert!(listen.is_err(), "the bind is refused");
    assert_eq!(std::fs::read(&path).unwrap(), b"kept");
}

#[cfg(target_family = "unix")]
#[test]
fn the_socket_file_takes_the_mode_asked_for() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("lore-local-socket-");
    let path = socket_in(&dir);

    let _listener = Claim::acquire(&path)
        .unwrap()
        .expect("the path is unclaimed")
        .listen(0o640)
        .unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
}
