// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service socket a move of the external root holds.
#![cfg(any(target_os = "windows", target_family = "unix"))]

use std::io::Read;
use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use lore::remote::call::Relayed;
use lore::remote::call::run_service_call;
use lore::remote::command::LoreCommand;
use lore::remote::message::MessageToClient;
use lore::remote::message::MessageToServer;
use lore::remote::message::blocking_read_message;
use lore::remote::message::write_payload;
use lore::remote::network::UdsListener;
use lore::remote::network::UdsStream;
use lore::remote::occupy::OccupiedSocket;
use lore::remote::occupy::READ_TIMEOUT;
use lore::remote::occupy::skip_request;
use lore_revision::event::LoreEvent;

use super::network::remove_claim_lock;
use super::network::socket_name;

/// Whether `connection` was answered with the move reply.
fn answered_with_the_move_reply(connection: &mut UdsStream) -> bool {
    matches!(
        blocking_read_message::<MessageToClient<LoreEvent>, _>(connection.reader()),
        Ok(Some((MessageToClient::ExternalRootMove, _)))
    )
}

/// A request whose payload is `size` bytes, as a client writes it in one piece.
fn write_request(connection: &mut UdsStream, size: usize) {
    write_payload(connection.writer(), &vec![0; size]).expect("the request is written");
}

/// A relayed call is answered with the move reply and handed back, and a call in a FUSE instance,
/// which only the service may run, then fails with `ServiceUnavailable`.
#[test]
fn a_relayed_call_gets_service_unavailable() {
    let _socket = crate::SERVICE_SOCKET_TESTS.blocking_lock();
    let occupied = OccupiedSocket::claim(lore::remote::service_socket_name(), READ_TIMEOUT)
        .expect("the claim is made")
        .expect("no service holds the process's socket");
    let root = lore_base::test_util::TempDir::new("lore-occupy-relayed-");
    let dot_dir = root.path().join(lore_revision::repository::DOT_LORE);
    std::fs::create_dir_all(&dot_dir).expect("creating the repository directory");
    std::fs::write(dot_dir.join("config.toml"), "[vfs]\nvfs_type = \"Fuse\"\n")
        .expect("writing the config");

    let globals = lore_revision::interface::LoreGlobalArgs {
        repository_path: root.path().display().to_string().into(),
        ..Default::default()
    };

    let relayed = lore::runtime().block_on(run_service_call(
        None,
        globals.clone(),
        LoreCommand::RevisionTreeClose(Default::default()),
        None,
    ));
    let status = lore::call_delegation::run_command(
        globals,
        LoreCommand::RevisionTreeClose(Default::default()),
        None,
    );

    lore::runtime().block_on(occupied.release());
    assert!(
        matches!(relayed, Relayed::HandedBack(_)),
        "the held socket hands the call back"
    );
    assert_eq!(status, crate::service_unavailable());
}

/// The payload of a request is read through a fixed buffer, so a request far larger than the
/// buffer is skipped with no allocation made while it is read. A small request is skipped first,
/// so that what the thread and the runtime allocate on their first use isn't counted. Each skip is
/// pinned here and run through its pin, which a debug build's runtime doesn't box, as it boxes a
/// future it is handed that is larger than 2 KiB.
#[test]
fn a_large_payload_is_skipped_without_an_allocation_of_its_size() {
    const SIZE: usize = 64 << 20;
    let mut request = Vec::new();
    write_payload(&mut request, &vec![0; SIZE]).expect("the request is encoded");
    let mut small = Vec::new();
    write_payload(&mut small, &[0; 8]).expect("the small request is encoded");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a runtime is built");
    let mut first = small.as_slice();
    runtime
        .block_on(std::pin::pin!(skip_request(&mut first, READ_TIMEOUT)).as_mut())
        .expect("the small request is skipped");
    let mut reader = request.as_slice();

    let (skipped, allocations) = {
        let mut skip = std::pin::pin!(skip_request(&mut reader, READ_TIMEOUT));
        lore_base::allocator::set_allocation_counting(true);
        let before = lore_base::allocator::thread_allocation_count();
        let skipped = runtime.block_on(skip.as_mut());
        let allocations = lore_base::allocator::thread_allocation_count() - before;
        lore_base::allocator::set_allocation_counting(false);
        (skipped, allocations)
    };

    assert!(skipped.is_ok(), "the request is skipped: {skipped:?}");
    assert!(reader.is_empty(), "the whole request is read");
    assert_eq!(
        allocations, 0,
        "skipping a request of {SIZE} bytes allocates"
    );
}

/// A connection that stops part way through its request is closed once a read waits past the
/// timeout, and holds up no other connection meanwhile.
#[test]
fn a_stalled_connection_times_out_while_another_gets_its_answer() {
    let read_timeout = Duration::from_secs(2);
    let name = socket_name("occupy-stalled");
    let occupied = OccupiedSocket::claim(&name, read_timeout)
        .expect("the claim is made")
        .expect("no service holds the socket");

    let mut stalled = UdsStream::connect(&name).expect("connect the stalled client");
    stalled
        .writer()
        .write_all(&[1, 0])
        .expect("write part of a header");
    let stalled_at = Instant::now();
    let mut prompt = UdsStream::connect(&name).expect("connect the prompt client");
    write_request(&mut prompt, 16);
    let prompt_answered = answered_with_the_move_reply(&mut prompt);
    let prompt_waited = stalled_at.elapsed();

    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = [0; 1];
        let _ = closed_tx.send(
            stalled
                .reader()
                .read(&mut rest)
                .map_err(|error| error.kind()),
        );
    });
    let closed = closed_rx.recv_timeout(read_timeout * 5);
    let stalled_waited = stalled_at.elapsed();

    lore::runtime().block_on(occupied.release());
    remove_claim_lock(&name);
    assert!(prompt_answered, "the prompt client gets the move reply");
    assert!(
        prompt_waited < read_timeout,
        "the prompt client waited {prompt_waited:?}, behind the stalled one"
    );
    assert!(
        matches!(closed, Ok(Ok(0))),
        "the stalled connection is closed unanswered: {closed:?}"
    );
    assert!(
        stalled_waited >= read_timeout,
        "the stalled connection was closed after {stalled_waited:?}, before its timeout"
    );
}

/// The socket can't be held while a service holds it, and no service can claim it while it is
/// held.
#[test]
fn a_claim_is_refused_while_a_service_holds_the_socket() {
    let name = socket_name("occupy-claim");
    let service = UdsListener::new(&name).expect("the service listens");
    let while_served = OccupiedSocket::claim(&name, READ_TIMEOUT)
        .expect("a claim is made")
        .is_none();
    drop(service);

    let occupied = OccupiedSocket::claim(&name, READ_TIMEOUT)
        .expect("a claim is made")
        .expect("the claim succeeds once the service has gone");
    let while_held = UdsListener::claim(&name)
        .expect("a service's claim is made")
        .is_none();
    let mut client = UdsStream::connect(&name).expect("connect to the held socket");
    let request = MessageToServer {
        globals: Default::default(),
        command: LoreCommand::ServiceStatus(Default::default()),
        environment: lore_base::env::CallEnvironment::capture(),
    };
    lore::remote::message::write_message(client.writer(), &request).expect("send a request");
    let answered = answered_with_the_move_reply(&mut client);

    lore::runtime().block_on(occupied.release());
    let after = UdsListener::claim(&name).expect("a claim is made");
    let freed = after.is_some();
    drop(after);
    remove_claim_lock(&name);
    assert!(while_served, "a served socket refuses the claim");
    assert!(while_held, "a held socket refuses a service's claim");
    assert!(answered, "a request on the held socket gets the move reply");
    assert!(freed, "a released socket can be claimed");
}
