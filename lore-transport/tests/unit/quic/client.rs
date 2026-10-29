// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_transport::quic::client::*;
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
#[tokio::test(flavor = "multi_thread")]
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
/// waiting on the held writer. The server reads nothing meanwhile, so it is written only on a
/// stream other than the dropped frame's.
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
