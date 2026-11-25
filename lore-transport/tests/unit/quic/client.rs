// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_transport::quic::client::*;
use lore_transport::quic::frame_queue::FRAME_CHUNKS;
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

fn inflight_counters() -> [AtomicU64; STREAM_COUNT as usize] {
    std::array::from_fn(|_| AtomicU64::new(0))
}

#[test]
fn inflight_guard_counts_a_request_only_while_it_is_outstanding() {
    let inflight = AtomicU64::new(0);

    {
        let _first = StreamInflightGuard::new(&inflight);
        assert_eq!(inflight.load(Ordering::Relaxed), 1);

        let _second = StreamInflightGuard::new(&inflight);
        assert_eq!(inflight.load(Ordering::Relaxed), 2);
    }

    assert_eq!(inflight.load(Ordering::Relaxed), 0);
}

#[test]
fn high_priority_spreads_concurrent_requests_over_every_stream() {
    let inflight = inflight_counters();

    let mut guards = Vec::new();
    let mut selected = Vec::new();
    for _ in 0..STREAM_COUNT {
        let stream = select_stream(&inflight, STREAM_COUNT, true);
        guards.push(StreamInflightGuard::new(&inflight[stream as usize]));
        selected.push(stream);
    }

    selected.sort_unstable();
    assert_eq!(selected, (0..STREAM_COUNT).collect::<Vec<_>>());
}

#[test]
fn high_priority_reuses_a_stream_once_its_request_completed() {
    let inflight = inflight_counters();

    for _ in 0..STREAM_COUNT * 4 {
        let stream = select_stream(&inflight, STREAM_COUNT, true);
        let _guard = StreamInflightGuard::new(&inflight[stream as usize]);
        assert_eq!(stream, 0);
    }

    assert!(
        inflight
            .iter()
            .all(|count| count.load(Ordering::Relaxed) == 0)
    );
}

#[test]
fn normal_priority_spreads_concurrent_requests_over_the_non_priority_streams() {
    let inflight = inflight_counters();

    let mut guards = Vec::new();
    let mut selected = Vec::new();
    for _ in PRIORITY_STREAM_COUNT..STREAM_COUNT {
        let stream = select_stream(&inflight, STREAM_COUNT, false);
        guards.push(StreamInflightGuard::new(&inflight[stream as usize]));
        selected.push(stream);
    }

    selected.sort_unstable();
    assert_eq!(
        selected,
        (PRIORITY_STREAM_COUNT..STREAM_COUNT).collect::<Vec<_>>()
    );
}

#[test]
fn normal_priority_leaves_the_priority_streams_to_metadata() {
    let inflight = inflight_counters();

    // Every non-priority stream is busy while the priority streams sit idle. Balancing on
    // outstanding requests alone would send bulk traffic to a priority stream; the reserved
    // window is what stops it.
    let _guards: Vec<_> = (PRIORITY_STREAM_COUNT..STREAM_COUNT)
        .map(|stream| StreamInflightGuard::new(&inflight[stream as usize]))
        .collect();

    let stream = select_stream(&inflight, STREAM_COUNT, false);
    assert!(
        stream >= PRIORITY_STREAM_COUNT,
        "bulk traffic must stay off the reserved streams, got {stream}"
    );
}

#[test]
fn normal_priority_shares_what_is_open_before_any_stream_can_be_reserved() {
    let inflight = inflight_counters();

    // With fewer streams open than the reservation needs, there is nothing to reserve.
    assert_eq!(select_stream(&inflight, 1, false), 0);
    assert_eq!(select_stream(&inflight, PRIORITY_STREAM_COUNT, false), 0);
}

#[test]
fn normal_priority_reuses_a_stream_once_its_request_completed() {
    let inflight = inflight_counters();

    for _ in 0..STREAM_COUNT * 4 {
        let stream = select_stream(&inflight, STREAM_COUNT, false);
        let _guard = StreamInflightGuard::new(&inflight[stream as usize]);
        assert_eq!(stream, PRIORITY_STREAM_COUNT);
    }

    assert!(
        inflight
            .iter()
            .all(|count| count.load(Ordering::Relaxed) == 0)
    );
}
fn ipv6_addr() -> SocketAddr {
    "[::1]:41337".parse().unwrap()
}

fn ipv4_addr() -> SocketAddr {
    "127.0.0.1:41337".parse().unwrap()
}

#[test]
fn happy_eyeballs_interleaves_ipv6_first_addresses() {
    let ipv6_second = "[::2]:41337".parse().unwrap();
    let ipv6_third = "[::3]:41337".parse().unwrap();
    let ipv4_second = "127.0.0.2:41337".parse().unwrap();

    assert_eq!(
        interleave_socket_addrs(vec![
            ipv6_addr(),
            ipv6_second,
            ipv6_third,
            ipv4_addr(),
            ipv4_second,
        ]),
        vec![
            ipv6_addr(),
            ipv4_addr(),
            ipv6_second,
            ipv4_second,
            ipv6_third,
        ]
    );
}

#[test]
fn happy_eyeballs_interleaves_ipv4_first_addresses() {
    let ipv4_second = "127.0.0.2:41337".parse().unwrap();
    let ipv6_second = "[::2]:41337".parse().unwrap();

    assert_eq!(
        interleave_socket_addrs(vec![ipv4_addr(), ipv4_second, ipv6_addr(), ipv6_second,]),
        vec![ipv4_addr(), ipv6_addr(), ipv4_second, ipv6_second]
    );
}

#[tokio::test]
async fn happy_eyeballs_starts_fallback_while_first_attempt_is_stalled() {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let attempt_log = attempts.clone();

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        connect_happy_eyeballs(
            vec![ipv6_addr(), ipv4_addr()],
            Duration::from_millis(10),
            move |addr| {
                let attempt_log = attempt_log.clone();
                async move {
                    attempt_log.lock().push(addr);
                    if addr.is_ipv6() {
                        std::future::pending().await
                    } else {
                        Some(addr)
                    }
                }
            },
        ),
    )
    .await
    .expect("fallback should not wait for the stalled first attempt");

    assert_eq!(result, Some(ipv4_addr()));
    assert_eq!(*attempts.lock(), vec![ipv6_addr(), ipv4_addr()]);
}

#[tokio::test]
async fn happy_eyeballs_advances_immediately_after_failure() {
    let started = std::time::Instant::now();

    let result = connect_happy_eyeballs(
        vec![ipv6_addr(), ipv4_addr()],
        Duration::from_secs(1),
        |addr| async move { if addr.is_ipv6() { None } else { Some(addr) } },
    )
    .await;

    assert_eq!(result, Some(ipv4_addr()));
    assert!(started.elapsed() < Duration::from_millis(750));
}

#[tokio::test]
async fn happy_eyeballs_does_not_start_fallback_after_first_success() {
    let attempts = Arc::new(Mutex::new(HashMap::new()));
    let attempt_counts = attempts.clone();

    let result = connect_happy_eyeballs(
        vec![ipv6_addr(), ipv4_addr()],
        Duration::from_millis(10),
        move |addr| {
            let attempt_counts = attempt_counts.clone();
            async move {
                *attempt_counts.lock().entry(addr).or_insert(0) += 1;
                Some(addr)
            }
        },
    )
    .await;

    assert_eq!(result, Some(ipv6_addr()));
    assert_eq!(attempts.lock().get(&ipv6_addr()), Some(&1));
    assert_eq!(attempts.lock().get(&ipv4_addr()), None);
}

#[tokio::test]
async fn happy_eyeballs_returns_none_when_all_attempts_fail() {
    let result = connect_happy_eyeballs(
        vec![ipv6_addr(), ipv4_addr()],
        Duration::from_millis(10),
        |_| async { None::<SocketAddr> },
    )
    .await;

    assert_eq!(result, None);
}

#[tokio::test]
async fn happy_eyeballs_bounds_in_flight_attempts() {
    let remote_addrs: Vec<_> = (1..=HAPPY_EYEBALLS_MAX_IN_FLIGHT + 1)
        .map(|port| SocketAddr::new(ipv6_addr().ip(), port as u16))
        .collect();
    let release = Arc::new(Semaphore::new(0));
    let attempt_release = release.clone();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let task = lore_base::lore_spawn!(connect_happy_eyeballs(
        remote_addrs.clone(),
        Duration::from_millis(1),
        move |addr| {
            started_tx.send(addr).unwrap();
            let attempt_release = attempt_release.clone();
            async move {
                attempt_release.acquire().await.unwrap().forget();
                None::<SocketAddr>
            }
        },
    ));

    for expected in remote_addrs.iter().take(HAPPY_EYEBALLS_MAX_IN_FLIGHT) {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .expect("attempt should start"),
            Some(*expected)
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), started_rx.recv())
            .await
            .is_err(),
        "attempts above the in-flight limit should remain queued"
    );

    release.add_permits(1);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .expect("queued attempt should start when a slot opens"),
        Some(remote_addrs[HAPPY_EYEBALLS_MAX_IN_FLIGHT])
    );

    task.abort();
}

fn receive_buffer_size(socket: &std::net::UdpSocket) -> usize {
    socket2::SockRef::from(socket)
        .recv_buffer_size()
        .expect("socket receive buffer size should be readable")
}

fn bound_socket_with_receive_buffer(bytes: usize) -> std::net::UdpSocket {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
    socket2::SockRef::from(&socket)
        .set_recv_buffer_size(bytes)
        .expect("receive buffer size should be settable");
    socket
}

#[test]
fn raise_receive_buffer_grows_a_smaller_buffer() {
    let socket = bound_socket_with_receive_buffer(64 * 1024);
    let before = receive_buffer_size(&socket);

    raise_receive_buffer(&socket, RECEIVE_BUFFER_BYTES);

    assert!(receive_buffer_size(&socket) > before);
}

#[test]
fn raise_receive_buffer_keeps_a_larger_buffer() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind should succeed");
    let before = receive_buffer_size(&socket);

    // Asking for less than the buffer holds would shrink it, so an unchanged size shows the
    // request was not made.
    raise_receive_buffer(&socket, before / 4);

    assert_eq!(receive_buffer_size(&socket), before);
}

/// Held by the tests that drop a command part way through its frame, as one of them shortens the
/// time every dropped frame's remainder has to be written.
static FINISHING_FRAMES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A test server, and a client of it with a session started and the client's connection.
async fn served_connection() -> (
    crate::quic::storage_service::test_server::TestStorageServer,
    Arc<lore_transport::quic::storage_service::client::StorageClient>,
    Arc<QuicConnection>,
) {
    use lore_transport::traits::Storage;

    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_partition;

    let server = TestStorageServer::start();
    let client = server.connect_client().await;
    let quic = ServiceClient::quic(client.as_ref()).clone();
    client
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    (server, client, quic)
}

/// Drops, under a context of its own, a command whose frame the stream does not take whole while
/// the server reads nothing, once its write is held. Answers the index of the stream the frame is
/// finished on, the dropped command's context, and its payload, which it lends as a caller of the C
/// API does.
async fn drop_a_held_command(
    quic: &Arc<QuicConnection>,
) -> (
    usize,
    std::sync::Weak<dyn std::any::Any + Send + Sync>,
    std::sync::Weak<[u8]>,
) {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    let lent: Arc<[u8]> = Arc::from(vec![0xa5u8; 128 * 1024]);
    let dropped_payload = Arc::downgrade(&lent);
    let mut large = [Bytes::new(), Bytes::from_owner(lent)];
    let context: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    let dropped_context = Arc::downgrade(&context);
    let dropped = tokio::time::timeout(
        Duration::from_millis(200),
        lore_base::runtime::LORE_CONTEXT.scope(
            context,
            send_normal(
                quic.clone(),
                Command::Put as u8,
                BoundSession::any(0),
                true,
                &mut large,
            ),
        ),
    )
    .await;
    assert!(dropped.is_err(), "the write was not held: {dropped:?}");
    let inflight = quic.stream_inflight();
    assert_eq!(
        inflight.iter().sum::<u64>(),
        1,
        "the finish is not the one request outstanding: {inflight:?}"
    );
    let finishing = inflight
        .iter()
        .position(|&count| count == 1)
        .expect("the finish is counted on its stream");
    (finishing, dropped_context, dropped_payload)
}

/// A command dropped part way through writing its frame has the frame finished in the background,
/// so the command after it on the same stream reaches the server whole and is answered over the
/// same connection. Neither the background write nor a stream's reader, which serve every later
/// command, holds the dropped command's context, and the background write holds none of its
/// payload, which a caller may lend only until the command is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_dropped_mid_write_leaves_its_stream_framed() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    use crate::quic::storage_service::test_server::wait_until;

    let _finishing = FINISHING_FRAMES.lock().await;
    let (server, _client, quic) = served_connection().await;
    server.pause_reading();
    let (finishing, dropped_context, dropped_payload) = drop_a_held_command(&quic).await;
    assert!(
        dropped_context.upgrade().is_none(),
        "a task the dropped command started holds its context"
    );
    wait_until(
        || dropped_payload.upgrade().is_none(),
        "the finish holds the dropped command's payload",
    )
    .await;
    server.resume_reading();
    wait_until(
        || quic.stream_inflight()[finishing] == 0,
        "the finish never released its stream",
    )
    .await;

    server.pause_reading();
    let after = {
        let quic = quic.clone();
        lore_base::lore_spawn!(async move {
            let mut small = [Bytes::new(), Bytes::from_static(b"after")];
            send_normal(
                quic,
                Command::Put as u8,
                BoundSession::any(0),
                true,
                &mut small,
            )
            .await
        })
    };
    wait_until(
        || quic.stream_inflight()[finishing] == 1,
        "the command after the dropped one went out on another stream",
    )
    .await;
    server.resume_reading();
    let answered = tokio::time::timeout(Duration::from_secs(10), after)
        .await
        .expect("the command after the dropped one was never answered")
        .expect("the command's task");
    assert!(answered.is_ok(), "{answered:?}");
    assert_eq!(server.accepted_connections(), 1);
}

/// A command sent while a stream finishes a dropped frame goes out on another stream rather than
/// queueing behind the dropped frame's rest. The server reads nothing meanwhile, so it is written
/// only on a stream other than the dropped frame's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_finishing_a_dropped_frame_is_passed_over() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    use crate::quic::storage_service::test_server::wait_until;

    let _finishing = FINISHING_FRAMES.lock().await;
    let (server, _client, quic) = served_connection().await;
    server.pause_reading();
    let (finishing, ..) = drop_a_held_command(&quic).await;

    let mut small = [Bytes::new(), Bytes::from_static(b"meanwhile")];
    let written = tokio::time::timeout(
        Duration::from_secs(5),
        send_without_response(quic.clone(), Command::Put as u8, 0, true, &mut small),
    )
    .await
    .expect("the command waited on the stream finishing a dropped frame");
    assert!(written.is_ok(), "{written:?}");

    server.resume_reading();
    wait_until(
        || quic.stream_inflight()[finishing] == 0,
        "the finish never released its stream",
    )
    .await;
}

/// A dropped frame whose remainder the server does not take in time has its stream reset. The
/// command after it on that stream fails as a lost connection does, so it is sent again over a new
/// connection and answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_after_a_reset_frame_is_answered_over_a_new_connection() {
    use lore_transport::traits::Storage;

    use crate::quic::storage_service::test_server::test_partition;
    use crate::quic::storage_service::test_server::wait_until;

    let _finishing = FINISHING_FRAMES.lock().await;
    let (server, client, quic) = served_connection().await;
    set_finish_frame_timeout(Some(Duration::from_millis(300)));
    server.pause_reading();
    let (finishing, ..) = drop_a_held_command(&quic).await;
    wait_until(
        || quic.stream_inflight()[finishing] == 0,
        "the finish never reset its stream",
    )
    .await;
    set_finish_frame_timeout(None);
    server.resume_reading();

    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        client.session_start(test_partition(), "test"),
    )
    .await
    .expect("the command after the reset stream was never answered");
    assert!(answered.is_ok(), "{answered:?}");
    assert_eq!(server.accepted_connections(), 2);
}

/// A command dropped while its frame waits in its stream's queue leaves no command waiting on the
/// stream, and its frame is never written: of the two `get`s sent, the server receives only the one
/// after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_dropped_while_queued_leaves_nothing_pending() {
    use lore_transport::traits::Storage;

    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_address;
    use crate::quic::storage_service::test_server::test_partition;

    let server = TestStorageServer::start();
    let client = server.connect_client().await;
    let session_id = client
        .session_start(test_partition(), "test")
        .await
        .expect("session start");
    let quic = ServiceClient::quic(client.as_ref()).clone();
    let writers = quic.hold_writers().await.expect("open every stream");
    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        client.get(session_id, &test_address(0)),
    )
    .await;
    assert!(dropped.is_err(), "the command was not held: {dropped:?}");
    assert_eq!(quic.pending_commands().await, 0);
    drop(writers);

    // Both go out on the stream that a command that is not high priority takes when none is in
    // flight, so the server answers the second only once the stream's writer has taken the first.
    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        client.get(session_id, &test_address(1)),
    )
    .await
    .expect("the command after the dropped one was never answered");
    assert!(answered.is_ok(), "{answered:?}");
    assert_eq!(server.received_gets_per_connection(), [1]);
    assert_eq!(quic.pending_commands().await, 0);
}

/// Many commands queued at once on each stream, each frame larger than the stream takes in one
/// write, are each written whole and one after another: every command is answered, over the one
/// connection, and no stream's reader meets an answer it does not expect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_commands_queued_on_each_stream_are_all_answered() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    use crate::quic::storage_service::test_server::wait_until;

    const COMMANDS: usize = 300;
    let (server, _client, quic) = served_connection().await;
    let writers = quic.hold_writers().await.expect("open every stream");
    let commands: Vec<_> = (0..COMMANDS)
        .map(|index| {
            let quic = quic.clone();
            lore_base::lore_spawn!(async move {
                let mut chunks = [
                    Bytes::new(),
                    Bytes::from(vec![index as u8; 1000 + index]),
                    Bytes::from_static(b"tail"),
                ];
                send_normal(
                    quic,
                    Command::Put as u8,
                    BoundSession::any(0),
                    true,
                    &mut chunks,
                )
                .await
            })
        })
        .collect();
    wait_until(
        || quic.stream_inflight().iter().sum::<u64>() == COMMANDS as u64,
        "the commands were never all queued",
    )
    .await;
    let inflight = quic.stream_inflight();
    assert!(
        inflight[PRIORITY_STREAM_COUNT as usize..]
            .iter()
            .all(|&count| count > 1),
        "a stream holds at most one command: {inflight:?}"
    );
    drop(writers);

    for command in commands {
        let answered = tokio::time::timeout(Duration::from_secs(10), command)
            .await
            .expect("a command was never answered")
            .expect("the command's task");
        assert!(answered.is_ok(), "{answered:?}");
    }
    assert_eq!(quic.pending_commands().await, 0);
    assert_eq!(quic.streams_ended(), 0);
    assert_eq!(server.accepted_connections(), 1);
}

/// A frame of more chunks than a queued frame holds reaches the server whole, its chunks one after
/// another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_of_more_chunks_than_a_queued_frame_holds_arrives_whole() {
    use bytes::Bytes;

    use crate::quic::storage_service::test_server::ECHO;

    let (_server, _client, quic) = served_connection().await;
    let mut chunks: Vec<Bytes> = std::iter::once(Bytes::new())
        .chain((0..FRAME_CHUNKS + 2).map(|index| Bytes::from(vec![index as u8; 100 + index])))
        .collect();
    let payload: Vec<u8> = chunks[1..].iter().flatten().copied().collect();
    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        send_normal(quic, ECHO as u8, BoundSession::any(0), true, &mut chunks),
    )
    .await
    .expect("the frame never arrived whole");
    assert_eq!(answered.expect("the echo"), payload);
}

/// A command too big to send is refused before anything is written, so no answer will come for it,
/// and it leaves no command waiting on the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_too_big_to_send_leaves_nothing_pending() {
    use bytes::Bytes;
    use lore_transport::quic::QuicClientError;
    use lore_transport::quic::storage_service::Command;
    use lore_transport::quic::storage_service::MAX_CHUNK_SIZE;

    let (_server, _client, quic) = served_connection().await;
    let mut too_big = [Bytes::new(), Bytes::from(vec![0u8; MAX_CHUNK_SIZE + 1])];
    let refused = send_normal(
        quic.clone(),
        Command::Put as u8,
        BoundSession::any(0),
        true,
        &mut too_big,
    )
    .await;
    assert!(
        matches!(refused, Err(QuicClientError::ClientMessageTooBig)),
        "{refused:?}"
    );
    assert_eq!(quic.pending_commands().await, 0);
}

/// Fills, with a frame written whole, the window of the stream that a command that is not high
/// priority takes when none is in flight, while the server reads nothing, so that the stream takes
/// none of the next frame. The stream is one nothing has used, so that its whole window is free.
async fn fill_a_stream_window(
    server: &crate::quic::storage_service::test_server::TestStorageServer,
    quic: &Arc<QuicConnection>,
) {
    use bytes::Bytes;
    use lore_transport::quic::command_header::COMMAND_HEADER_SIZE_V4;
    use lore_transport::quic::storage_service::Command;

    use crate::quic::storage_service::test_server::STREAM_WINDOW;

    quic.open_streams().await.expect("open every stream");
    server.pause_reading();
    let mut filling = [
        Bytes::new(),
        Bytes::from(vec![0u8; STREAM_WINDOW as usize - COMMAND_HEADER_SIZE_V4]),
    ];
    tokio::time::timeout(
        Duration::from_secs(5),
        send_without_response(quic.clone(), Command::Put as u8, 0, true, &mut filling),
    )
    .await
    .expect("the frame filling the window was not written whole")
    .expect("the frame filling the window");
    assert_eq!(quic.pending_commands().await, 1);
}

/// A command dropped once its stream's writer has taken its frame, but before the stream takes any
/// of it, has written nothing either, and leaves no command waiting on the stream. A frame that
/// fills the stream's window, which the server does not read meanwhile, leaves it no room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_dropped_before_the_stream_takes_any_of_its_frame_leaves_nothing_pending() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    let (server, _client, quic) = served_connection().await;
    fill_a_stream_window(&server, &quic).await;

    let mut small = [Bytes::new(), Bytes::from_static(b"dropped")];
    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        send_normal(
            quic.clone(),
            Command::Put as u8,
            BoundSession::any(0),
            true,
            &mut small,
        ),
    )
    .await;
    assert!(dropped.is_err(), "the command was not held: {dropped:?}");
    assert_eq!(quic.pending_commands().await, 1);
}

/// Commands dropped while the frame ahead of theirs on the stream waits for flow control leave
/// nothing queued behind it, however many there are.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_dropped_behind_a_blocked_frame_leave_nothing_queued() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    let (server, _client, quic) = served_connection().await;
    fill_a_stream_window(&server, &quic).await;
    for _ in 0..20 {
        let mut small = [Bytes::new(), Bytes::from_static(b"dropped")];
        let dropped = tokio::time::timeout(
            Duration::from_millis(20),
            send_normal(
                quic.clone(),
                Command::Put as u8,
                BoundSession::any(0),
                true,
                &mut small,
            ),
        )
        .await;
        assert!(dropped.is_err(), "the command was not held: {dropped:?}");
    }
    assert_eq!(quic.queued_frames().await, 0);
    assert_eq!(quic.pending_commands().await, 1);
}

/// Commands dropped while their frames wait behind a frame the stream has taken only part of, with
/// frames before and after theirs, take theirs back from the middle of the queue: every other
/// command is answered, over the one connection, and nothing is left queued or pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_dropped_from_the_middle_of_a_queue_leave_the_rest_answered() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    use crate::quic::storage_service::test_server::STREAM_WINDOW;
    use crate::quic::storage_service::test_server::wait_until;

    const COMMANDS: usize = 48;
    let (server, _client, quic) = served_connection().await;
    quic.open_streams().await.expect("open every stream");
    server.pause_reading();
    let commands: Vec<_> = (0..COMMANDS)
        .map(|index| {
            let quic = quic.clone();
            lore_base::lore_spawn!(async move {
                let mut chunks = [
                    Bytes::new(),
                    Bytes::from(vec![index as u8; STREAM_WINDOW as usize + index]),
                ];
                send_normal(
                    quic,
                    Command::Put as u8,
                    BoundSession::any(0),
                    true,
                    &mut chunks,
                )
                .await
            })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(10), async {
        while quic.queued_frames().await != COMMANDS {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the writers never took every frame");

    let mut kept = Vec::new();
    for (index, command) in commands.into_iter().enumerate() {
        if index % 2 == 1 {
            command.abort();
            assert!(command.await.is_err_and(|err| err.is_cancelled()));
        } else {
            kept.push(command);
        }
    }
    server.resume_reading();

    for command in kept {
        let answered = tokio::time::timeout(Duration::from_secs(10), command)
            .await
            .expect("a command was never answered")
            .expect("the command's task");
        assert!(answered.is_ok(), "{answered:?}");
    }
    wait_until(
        || quic.stream_inflight().iter().all(|&count| count == 0),
        "a dropped frame's rest was never written",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while quic.pending_commands().await != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a dropped frame's answer was never taken");
    assert_eq!(quic.queued_frames().await, 0);
    assert_eq!(quic.streams_ended(), 0);
    assert_eq!(server.accepted_connections(), 1);
}

/// A command whose frame the stream does not take whole has the rest written whether or not the
/// command is polled again: polled once, its answer is taken from the stream before it is polled
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_is_written_in_full_while_its_command_is_not_polled() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    use crate::quic::storage_service::test_server::STREAM_WINDOW;

    let (server, _client, quic) = served_connection().await;
    server.pause_reading();
    let mut large = [
        Bytes::new(),
        Bytes::from(vec![0u8; 4 * STREAM_WINDOW as usize]),
    ];
    let mut command = std::pin::pin!(send_normal(
        quic.clone(),
        Command::Put as u8,
        BoundSession::any(0),
        true,
        &mut large,
    ));
    assert!(futures::poll!(command.as_mut()).is_pending());
    server.resume_reading();

    tokio::time::timeout(Duration::from_secs(10), async {
        while quic.pending_commands().await != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the frame was not written while its command was not polled");
    let answered = tokio::time::timeout(Duration::from_secs(10), command)
        .await
        .expect("the answer taken never reached the command");
    assert!(answered.is_ok(), "{answered:?}");
}

/// A command dropped once its frame is written still waits on its stream, so the reader takes its
/// answer when it comes rather than ending the stream on an answer it does not expect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_dropped_after_its_write_is_answered_and_its_stream_kept() {
    use bytes::Bytes;
    use lore_transport::quic::storage_service::Command;

    let (server, _client, quic) = served_connection().await;
    server.pause_reading();
    let mut small = [Bytes::new(), Bytes::from_static(b"dropped")];
    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        send_normal(
            quic.clone(),
            Command::Put as u8,
            BoundSession::any(0),
            true,
            &mut small,
        ),
    )
    .await;
    assert!(dropped.is_err(), "the command was answered: {dropped:?}");
    assert_eq!(quic.pending_commands().await, 1);

    server.resume_reading();
    tokio::time::timeout(Duration::from_secs(10), async {
        while quic.pending_commands().await != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the dropped command's answer never came");
    assert_eq!(quic.streams_ended(), 0);
}

mod controller_factory {
    use std::time::Instant;

    use quinn::congestion;

    use super::*;

    const MTU: u16 = 1200;
    const SEEDED_WINDOW: u64 = 64 * 1024 * 1024;
    const UNLIMITED_SEND_WINDOW: u64 = u64::MAX;
    const SMALL_SEND_WINDOW: u64 = 4 * 1024 * 1024;

    fn build(
        algorithm: CongestionAlgorithm,
        initial_window: Option<u64>,
        send_window: u64,
    ) -> Box<dyn congestion::Controller> {
        algorithm
            .controller_factory(initial_window, send_window)
            .build(Instant::now(), MTU)
    }

    #[test]
    fn cubic_builds_a_cubic_controller_seeded_with_the_initial_window() {
        let controller = build(
            CongestionAlgorithm::Cubic,
            Some(SEEDED_WINDOW),
            UNLIMITED_SEND_WINDOW,
        );

        assert_eq!(controller.initial_window(), SEEDED_WINDOW);
        controller
            .into_any()
            .downcast::<congestion::Cubic>()
            .expect("controller should be Cubic");
    }

    #[test]
    fn bbr_builds_a_bbr_controller_seeded_with_the_initial_window() {
        let controller = build(
            CongestionAlgorithm::Bbr,
            Some(SEEDED_WINDOW),
            UNLIMITED_SEND_WINDOW,
        );

        assert_eq!(controller.initial_window(), SEEDED_WINDOW);
        controller
            .into_any()
            .downcast::<congestion::Bbr>()
            .expect("controller should be BBR");
    }

    #[test]
    fn no_initial_window_keeps_the_algorithm_default() {
        for algorithm in [CongestionAlgorithm::Cubic, CongestionAlgorithm::Bbr] {
            let controller = build(algorithm, None, UNLIMITED_SEND_WINDOW);

            assert!(
                controller.initial_window() < SEEDED_WINDOW,
                "{algorithm:?} initial window ({}) should be its small default",
                controller.initial_window(),
            );
        }
    }

    #[test]
    fn bbr_window_is_held_to_the_send_window() {
        let controller = build(
            CongestionAlgorithm::Bbr,
            Some(SEEDED_WINDOW),
            SMALL_SEND_WINDOW,
        );

        assert_eq!(controller.window(), SMALL_SEND_WINDOW);
    }
}

mod observe_responses {
    use bytes::Bytes;
    use lore_transport::quic::response_observer::ObservedResponse;
    use lore_transport::quic::response_observer::ResponseObserver;
    use lore_transport::quic::storage_service::Command;

    use super::*;

    #[derive(Default)]
    struct Recorded(Mutex<Vec<ObservedResponse>>);

    impl ResponseObserver for Recorded {
        fn observe(&self, response: &ObservedResponse, _connection: &quinn::Connection) {
            self.0.lock().push(*response);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_observer_sees_each_answered_command() {
        let (_server, _client, quic) = served_connection().await;
        let recorded = Arc::new(Recorded::default());
        assert!(quic.observe_responses(recorded.clone()));

        let mut chunks = [Bytes::new(), Bytes::from_static(b"observed")];
        send_normal(
            quic.clone(),
            Command::Put as u8,
            BoundSession::any(0),
            true,
            &mut chunks,
        )
        .await
        .expect("the put is answered");

        // The reader observes a response once it has handed it over, so the observation can trail
        // the answer.
        let observed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let observed = recorded.0.lock().clone();
                if !observed.is_empty() {
                    return observed;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the response was never observed");
        let [response] = observed.as_slice() else {
            panic!("expected one observed response, got {observed:?}");
        };
        assert_eq!(response.opcode, Command::Put as u8);
        assert_eq!(response.error_status, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_observer_is_refused() {
        let (_server, _client, quic) = served_connection().await;

        assert!(quic.observe_responses(Arc::new(Recorded::default())));
        assert!(!quic.observe_responses(Arc::new(Recorded::default())));
    }
}
