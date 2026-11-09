// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Admission on the machine server's storage sockets, and the settings the sockets run under.
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_server::settings::AuthSettings;
use lore_server::settings::UdsSettings;
use lore_server::uds::Admission;
use lore_server::uds::Blocked;
use lore_server::uds::ConnectionSlot;
use lore_server::uds::Gate;
use lore_server::uds::Limits;
use lore_server::uds::SOCKET_COUNTED;
use lore_server::uds::SocketBudget;
use lore_server::uds::admitted_bytes;
use lore_server::uds::check_settings;
use lore_server::uds::proxy::route;
use lore_server::uds::refuse_socket_route;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use lore_transport::uds::MAX_MESSAGE;
use lore_transport::uds::SOCKET_ENV;
use lore_transport::uds::io::Wakeup;

use crate::util::test_support::setup_test_execution;

const PATIENCE: Duration = Duration::from_secs(5);

fn limits(credits: usize, credit_bytes: usize) -> Limits {
    Limits {
        credits,
        credit_bytes,
        socket_bytes: 1 << 30,
    }
}

fn connection(socket: &Arc<SocketBudget>) -> (Gate, Arc<Admission>) {
    counted_connection(socket, &Arc::new(AtomicUsize::new(1)))
}

/// A connection that `open` counts among its socket's.
fn counted_connection(
    socket: &Arc<SocketBudget>,
    open: &Arc<AtomicUsize>,
) -> (Gate, Arc<Admission>) {
    let room = Arc::new(Wakeup::default());
    let backlog = Arc::new(socket.backlog(room.clone()));
    let admission = Arc::new(Admission::new(
        backlog,
        socket.clone(),
        room,
        ConnectionSlot(open.clone()),
    ));
    (Gate::new(admission.clone()), admission)
}

/// A request in handling keeps its connection counted on the socket, so the socket's connection
/// limit bounds the work its connections started, not only the connections it has open.
#[test]
fn a_connection_counts_until_its_last_request_in_handling_finishes() {
    let socket = Arc::new(SocketBudget::default());
    let open = Arc::new(AtomicUsize::new(1));
    let (gate, admission) = counted_connection(&socket, &open);
    let in_handling = admission.clone();

    drop(gate);
    drop(admission);
    let while_handling = open.load(Ordering::SeqCst);
    drop(in_handling);

    assert_eq!(while_handling, 1, "a request in handling keeps it counted");
    assert_eq!(open.load(Ordering::SeqCst), 0, "the last request frees it");
}

#[test]
fn a_connection_takes_requests_up_to_its_credits() {
    let socket = Arc::new(SocketBudget::default());
    let (mut gate, admission) = connection(&socket);
    let limits = limits(3, 1 << 30);

    for _ in 0..3 {
        gate.try_admit(&limits).unwrap();
        gate.issue(32);
    }
    assert!(matches!(gate.try_admit(&limits), Err(Blocked::Connection)));
    assert_eq!(gate.outstanding(), (3, 96));

    admission.finish(32);
    gate.try_admit(&limits).unwrap();
    assert_eq!(gate.outstanding(), (2, 64));
}

#[test]
fn bodies_in_handling_count_against_the_connection_bytes() {
    let socket = Arc::new(SocketBudget::default());
    let (mut gate, admission) = connection(&socket);
    // Room for two bodies of the largest size, and not for a third.
    let limits = limits(4000, 3 * MAX_MESSAGE - 1);

    gate.try_admit(&limits).unwrap();
    gate.issue(MAX_MESSAGE);
    gate.try_admit(&limits).unwrap();
    gate.issue(MAX_MESSAGE);
    assert!(matches!(gate.try_admit(&limits), Err(Blocked::Connection)));

    admission.finish(MAX_MESSAGE);
    gate.try_admit(&limits).unwrap();
}

#[test]
fn unwritten_responses_count_against_the_connection_bytes() {
    let socket = Arc::new(SocketBudget::default());
    let (gate, admission) = connection(&socket);
    let limits = limits(4000, 2 * MAX_MESSAGE);

    admission
        .backlog
        .queued
        .store(MAX_MESSAGE + 1, Ordering::SeqCst);
    assert!(matches!(gate.try_admit(&limits), Err(Blocked::Connection)));
    admission.backlog.queued.store(0, Ordering::SeqCst);
    gate.try_admit(&limits).unwrap();
}

/// A read's response is not there to count when the read is taken, so the read holds the largest
/// response while it is handled, on its connection and on its socket.
#[test]
fn a_read_in_handling_holds_the_largest_response() {
    let socket = Arc::new(SocketBudget::default());
    let (mut gate, admission) = connection(&socket);
    // Room for two reads, and not for a third.
    let limits = limits(4000, 3 * MAX_MESSAGE - 1);
    let read = |command: Command| CommandHeader::new_with_session(command as u8, 0, 48, 0);

    for command in [Command::Get, Command::GetResolved] {
        gate.try_admit(&limits).unwrap();
        gate.issue(admitted_bytes(&read(command), 48));
    }
    let blocked = gate.try_admit(&limits);
    let on_socket = socket.bodies.load(Ordering::SeqCst);
    admission.finish(MAX_MESSAGE);

    assert!(matches!(blocked, Err(Blocked::Connection)), "{blocked:?}");
    assert_eq!(on_socket, 2 * MAX_MESSAGE);
    assert_eq!(admitted_bytes(&read(Command::Query), 48), 48);
    gate.try_admit(&limits).unwrap();
}

#[test]
fn small_bodies_touch_nothing_the_socket_shares() {
    let socket = Arc::new(SocketBudget::default());
    let (mut gate, admission) = connection(&socket);

    for _ in 0..100 {
        gate.issue(SOCKET_COUNTED - 1);
    }
    assert_eq!(socket.bodies.load(Ordering::SeqCst), 0);
    gate.issue(SOCKET_COUNTED);
    assert_eq!(socket.bodies.load(Ordering::SeqCst), SOCKET_COUNTED);
    admission.finish(SOCKET_COUNTED);
    assert_eq!(socket.bodies.load(Ordering::SeqCst), 0);
}

/// Readers blocked on the socket's large bodies proceed once those finish, and not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_waiting_on_the_socket_proceeds_once_large_bodies_finish() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let socket = Arc::new(SocketBudget::default());
            let limits = Limits {
                credits: 4000,
                credit_bytes: 1 << 30,
                socket_bytes: 2 * MAX_MESSAGE,
            };
            let (mut busy, busy_admission) = connection(&socket);
            let (waiting, _) = connection(&socket);
            busy.issue(MAX_MESSAGE);
            busy.issue(MAX_MESSAGE);
            assert!(matches!(waiting.try_admit(&limits), Err(Blocked::Socket)));
            assert!(matches!(busy.try_admit(&limits), Err(Blocked::Socket)));

            let mut other = std::pin::pin!(waiting.admit(&limits, || false));
            let mut own = std::pin::pin!(busy.admit(&limits, || false));
            assert!(futures::poll!(other.as_mut()).is_pending());
            assert!(futures::poll!(own.as_mut()).is_pending());

            busy_admission.finish(MAX_MESSAGE);
            tokio::time::timeout(PATIENCE, other)
                .await
                .expect("another connection's reader wakes");
            tokio::time::timeout(PATIENCE, own)
                .await
                .expect("the connection's own reader wakes");
        })
        .await;
}

/// A reader waiting for credit wakes when a handler finishes, and not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_reader_wakes_when_a_handler_finishes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let socket = Arc::new(SocketBudget::default());
            let (mut gate, admission) = connection(&socket);
            let limits = limits(1, 1 << 30);
            gate.try_admit(&limits).unwrap();
            gate.issue(0);

            {
                let mut waiting = std::pin::pin!(gate.admit(&limits, || false));
                assert!(futures::poll!(waiting.as_mut()).is_pending());

                admission.finish(0);
                tokio::time::timeout(PATIENCE, waiting)
                    .await
                    .expect("the reader wakes");
            }
            assert_eq!(gate.outstanding(), (0, 0));
        })
        .await;
}

/// A reader waiting for credit stops when its connection closes, and not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_reader_stops_when_its_connection_closes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let socket = Arc::new(SocketBudget::default());
            let (mut gate, admission) = connection(&socket);
            let limits = limits(1, 1 << 30);
            gate.issue(0);
            let closed = AtomicBool::new(false);

            let mut waiting = std::pin::pin!(gate.admit(&limits, || closed.load(Ordering::SeqCst)));
            assert!(futures::poll!(waiting.as_mut()).is_pending());

            closed.store(true, Ordering::SeqCst);
            admission.room.wake();
            tokio::time::timeout(PATIENCE, waiting)
                .await
                .expect("the reader stops");
        })
        .await;
}

/// The forward allow-list matches a configured host whatever the case a request names it in, and
/// nothing else.
#[test]
fn a_forwarded_host_matches_whatever_its_case() {
    let routes = HashMap::from([(
        "lore.example.com".to_string(),
        "https://lore.example.com:443".to_string(),
    )]);
    for host in ["lore.example.com", "LORE.Example.COM"] {
        assert_eq!(
            route(&routes, host).map(|(_, target)| target.as_str()),
            Some("https://lore.example.com:443"),
            "{host}"
        );
    }
    assert_eq!(route(&routes, "other.example.com"), None);
}

/// `[server.uds]` with one socket, forwarding one host, and a handler timeout of
/// `handler_timeout_seconds`.
fn uds_settings(handler_timeout_seconds: u64) -> UdsSettings {
    let mut settings: UdsSettings = toml::from_str(
        "enabled = true\nsockets = [\"/run/lore/c1/lore.sock\"]\n\
         forward = { \"lore.example.com\" = \"https://lore.example.com\" }",
    )
    .unwrap();
    settings.handler_timeout_seconds = handler_timeout_seconds;
    settings
}

/// `[server.auth]` with the keys every one needs, and `extra`.
fn auth(extra: &str) -> AuthSettings {
    toml::from_str(&format!(
        "jwt_issuer = \"https://issuer.test\"\njwt_audience = [\"lore\"]\n{extra}"
    ))
    .unwrap()
}

/// The sockets run only under an authorizer that tells partitions apart: the auth service's or
/// the resource claim's, not global grants or the allow-all one of a server without
/// `[server.auth]`.
#[test]
fn the_sockets_run_only_under_an_authorizer_that_grants_per_partition() {
    let settings = uds_settings(50);
    let (global, resources) = (auth(""), auth("resource_claim = \"resources\""));

    assert!(check_settings(&settings, Some(&global), Some("https://auth.test")).is_ok());
    assert!(check_settings(&settings, Some(&resources), None).is_ok());
    assert!(check_settings(&settings, Some(&global), None).is_err());
    assert!(check_settings(&settings, None, None).is_err());
}

/// The sockets need a socket to serve, and forward keys that are host names alone, since the
/// forwarder matches a request's host against them and a key with a port or scheme matches none.
#[test]
fn the_sockets_need_a_socket_and_forward_keys_that_are_hosts() {
    let resources = auth("resource_claim = \"resources\"");
    let mut none = uds_settings(50);
    none.sockets.clear();
    assert!(check_settings(&none, Some(&resources), None).is_err());

    for (key, accepted) in [
        ("lore.example.com", true),
        ("localhost", true),
        ("[::1]", true),
        ("lore.example.com:443", false),
        ("https://lore.example.com", false),
        ("user@lore.example.com", false),
    ] {
        let mut settings = uds_settings(50);
        settings.forward =
            HashMap::from([(key.to_string(), "https://lore.example.com".to_string())]);
        assert_eq!(
            check_settings(&settings, Some(&resources), None).is_ok(),
            accepted,
            "{key}"
        );
    }

    let mut unforwarded = uds_settings(50);
    unforwarded.forward.clear();
    assert!(
        check_settings(&unforwarded, Some(&resources), None).is_ok(),
        "a forward that names no host is warned of, not refused"
    );
}

/// A server whose process names a machine server's socket would send its own gRPC through it,
/// so it refuses to run, naming the variable.
#[test]
fn a_server_that_names_a_socket_refuses_to_run() {
    assert!(refuse_socket_route(None).is_ok());
    let refusal = refuse_socket_route(Some(Path::new("/run/lore/lore.sock")))
        .expect_err("a server that names a socket is refused");
    assert!(refusal.to_string().contains(SOCKET_ENV), "{refusal}");
}

/// The server's hello states the handler timeout in 16 bits of seconds, from one.
#[test]
fn the_handler_timeout_fits_the_hello() {
    let resources = auth("resource_claim = \"resources\"");

    for (seconds, fits) in [(0, false), (1, true), (65_535, true), (65_536, false)] {
        assert_eq!(
            check_settings(&uds_settings(seconds), Some(&resources), None).is_ok(),
            fits,
            "{seconds} seconds"
        );
    }
}
