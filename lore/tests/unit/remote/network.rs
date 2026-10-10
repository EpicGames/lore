// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// The transport is exercised here rather than in the per-OS modules so that
// every backend is covered by the same test.
#![cfg(any(target_os = "windows", target_family = "unix"))]

#[cfg(target_os = "windows")]
mod windows;

use std::io::Read;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::time::Duration;
use std::time::Instant;

use lore::remote::network::*;

const TEST_STRING: &str = "ABC";

/// A socket name unique to one test and one run of this binary.
///
/// Kept short because the name joins the temporary directory into an address
/// that holds 108 bytes. The process suffix keeps two concurrent runs off a
/// single path, which the Windows listener unlinks without checking for a
/// live peer.
fn socket_name(test: &str) -> String {
    format!("{test}-{}", std::process::id())
}

fn run_service(name: &str, ready_signal: Sender<()>) -> String {
    let listener = UdsListener::new(name).unwrap();

    ready_signal.send(()).unwrap();

    let mut stream = listener.accept().unwrap();

    let mut buf = Vec::new();
    stream.reader().read_to_end(&mut buf).unwrap();
    let result = str::from_utf8(&buf).unwrap();
    println!("RECEIVED: {result}");

    result.to_string()
}

fn run_client(name: &str) {
    let mut conn = UdsStream::connect(name).unwrap();
    conn.writer().write_all(TEST_STRING.as_bytes()).unwrap();
}

fn run_both(name: &str) -> String {
    let (sender, receiver) = std::sync::mpsc::channel::<()>();
    // Scoped threads, so both borrow `name` rather than requiring it to be
    // `'static` as `std::thread::spawn` would.
    std::thread::scope(|scope| {
        let service = scope.spawn(move || run_service(name, sender));
        // The signal arrives after the listener is bound and listening, so
        // the backlog already holds the connect below. Nothing to wait for.
        receiver.recv().unwrap();
        let client = scope.spawn(move || {
            run_client(name);
        });
        let result = service.join().unwrap();
        client.join().unwrap();
        result
    })
}

/// Removes the lock file a claim on `name` leaves behind.
fn remove_claim_lock(name: &str) {
    #[cfg(target_family = "unix")]
    {
        let mut lock = unix::uds_sock_path(name).into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
    #[cfg(not(target_family = "unix"))]
    let _ = name;
}

#[test]
fn test_both() {
    let name = socket_name("uds-both");
    let result = run_both(&name);
    remove_claim_lock(&name);
    assert_eq!(result, TEST_STRING.to_string());
}

/// A second service must learn that it would lose the socket before it initializes, since
/// initializing mounts every instance.
#[cfg(target_family = "unix")]
#[test]
fn a_claimed_socket_name_refuses_a_second_claim_until_its_listener_drops() {
    let name = socket_name("uds-claim");
    let claim = UdsListener::claim(&name)
        .expect("the first claim is made")
        .expect("the first claim succeeds");
    let while_claimed = UdsListener::claim(&name).expect("a second claim is made");

    let listener = claim.listen().expect("the claim binds");
    let while_listening = UdsListener::claim(&name).expect("a claim is made");

    drop(listener);
    let after = UdsListener::claim(&name).expect("a claim is made");
    let freed = after.is_some();
    drop(after);
    remove_claim_lock(&name);
    assert!(
        while_claimed.is_none(),
        "a claimed name refuses a second claim"
    );
    assert!(
        while_listening.is_none(),
        "a listening name refuses a claim"
    );
    assert!(freed, "the name is claimable once its listener drops");
}

/// A service from a build that takes no claim listens without one, and a new service must
/// not start next to it.
#[cfg(target_family = "unix")]
#[test]
fn a_socket_served_without_a_claim_refuses_a_claim() {
    let name = socket_name("uds-unclaimed");
    let socket = unix::uds_sock_path(&name);
    std::fs::create_dir_all(socket.parent().expect("the socket has a directory"))
        .expect("the socket directory exists");
    let served = std::os::unix::net::UnixListener::bind(&socket).expect("binds");
    let refused = UdsListener::claim(&name).expect("a claim is made");
    let refused = refused.is_none();
    drop(served);
    let _ = std::fs::remove_file(&socket);
    remove_claim_lock(&name);
    assert!(refused, "a served socket refuses a claim");
}

/// A connect with nothing listening reports it rather than waiting.
///
/// The start and stop waits give up after ten seconds and retry around this
/// call. A connect that waits for a service itself would use one of those
/// whole waits on a single attempt.
#[test]
fn connecting_with_nothing_listening_reports_it_rather_than_waiting() {
    let name = socket_name("uds-unlistened");
    let started = Instant::now();

    let result = UdsStream::connect(&name);

    assert!(result.is_err(), "nothing is listening on {name}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the connect took {:?}, so it waited rather than reporting",
        started.elapsed()
    );
}
