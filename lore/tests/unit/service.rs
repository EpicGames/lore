// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore::service::service_main::CloseOnExit;
use tokio::sync::oneshot;

use crate::service_unavailable;

/// A close that counts its runs, and reports each on `ran`.
fn counting_close(
    runs: &Arc<AtomicUsize>,
    ran: oneshot::Sender<()>,
) -> impl FnOnce() -> std::future::Ready<()> {
    let runs = Arc::clone(runs);
    move || {
        runs.fetch_add(1, Ordering::SeqCst);
        let _ = ran.send(());
        std::future::ready(())
    }
}

/// Long enough for a close the guard started on a task of its own to have run.
const SETTLE: Duration = Duration::from_millis(200);

/// The service closes its mounts on the way out. Closed that way, the guard does not close them
/// again when it is dropped after.
#[tokio::test]
async fn closing_runs_the_close_once() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (ran, _) = oneshot::channel();
    let guard = CloseOnExit::new(counting_close(&runs, ran));

    guard.close().await;
    tokio::time::sleep(SETTLE).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// A service future dropped before it reaches its close still closes its mounts: the guard runs
/// the close on a task of its own when it is dropped.
#[tokio::test]
async fn dropping_without_closing_runs_the_close_on_a_task_of_its_own() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (ran, ran_reported) = oneshot::channel();
    let guard = CloseOnExit::new(counting_close(&runs, ran));

    drop(guard);

    tokio::time::timeout(Duration::from_secs(10), ran_reported)
        .await
        .expect("the close runs after the guard is dropped")
        .expect("the close reports its run");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// A service future dropped while it closes its mounts does not close them a second time: the
/// close under way has the guard's one run.
#[tokio::test]
async fn a_close_cut_short_is_not_started_again() {
    let runs = Arc::new(AtomicUsize::new(0));
    let guard = CloseOnExit::new({
        let runs = Arc::clone(&runs);
        move || {
            runs.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>()
        }
    });

    let cut_short = tokio::time::timeout(SETTLE, guard.close()).await;
    tokio::time::sleep(SETTLE).await;

    assert!(
        cut_short.is_err(),
        "the close never finishes, so the wait is cut short"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// The `ServiceStatus` events a status asked with the environment of a caller whose
/// `LORE_GLOBAL_PATH` is `global` delivers, with the call's exit code.
async fn status_in(
    global: &std::path::Path,
) -> (i32, Vec<lore_revision::event::LoreServiceStatusEventData>) {
    use lore::interface::LoreEvent;

    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&received);
    let callback: lore::interface::LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::ServiceStatus(data) = event {
            recorder
                .lock()
                .expect("the collector lock is not poisoned")
                .push(data.clone());
        }
    }));
    let environment = lore_base::env::CallEnvironment {
        values: [Some(global.display().to_string()), None, None],
    };
    let code = lore::call::with_relayed_environment(
        environment,
        lore::service::status(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore::service::LoreServiceStatusArgs {},
            callback,
        ),
    )
    .await;
    let received = received
        .lock()
        .expect("the collector lock is not poisoned")
        .clone();
    (code, received)
}

/// With no service running, a status reports the caller's external root: the path its setting
/// names, whether it can be used, and every leftover listed, marked or not. A move record makes
/// the root's state interrupted.
#[tokio::test]
async fn a_status_with_no_service_reports_the_callers_external_root() {
    use std::num::NonZeroU64;

    use lore::interface::LoreExternalRootState;
    use lore_revision::global::external_dir::leftovers::Leftovers;
    use lore_revision::global::external_dir::record::MoveKind;
    use lore_revision::global::external_dir::record::MovePhase;
    use lore_revision::global::external_dir::record::MoveRecord;
    use lore_revision::global::external_dir::setting::RootId;
    use lore_revision::global::external_dir::setting::Setting;

    let _socket = crate::SERVICE_SOCKET_TESTS.lock().await;
    let global = lore_base::test_util::TempDir::new("lore-service-status-root-");
    let data = global.path().join("data");
    std::fs::create_dir(&data).expect("make the global data directory");
    let root = global.path().join("missing-root");
    Setting {
        path: root.display().to_string(),
        id: RootId::generate(),
        generation: NonZeroU64::MIN,
    }
    .save(&data)
    .await
    .expect("write the setting");
    Leftovers {
        paths: vec!["external.old".to_string()],
        marked: vec!["/unreachable/root".to_string()],
    }
    .save(&data)
    .await
    .expect("write the leftover list");

    let (code, events) = status_in(global.path()).await;
    assert_eq!(code, 0);
    assert_eq!(events.len(), 1, "one ServiceStatus event");
    assert_eq!(events[0].running, 0);
    assert_eq!(events[0].external_root.as_str(), root.display().to_string());
    assert_eq!(
        events[0].external_root_state,
        LoreExternalRootState::Unavailable
    );
    assert_eq!(events[0].external_leftovers, 2);

    MoveRecord {
        from: String::new(),
        to: root.display().to_string(),
        kind: MoveKind::Rename,
        phase: MovePhase::Place,
    }
    .save(&data)
    .await
    .expect("write the move record");
    let (_, events) = status_in(global.path()).await;
    assert_eq!(
        events[0].external_root_state,
        LoreExternalRootState::Interrupted
    );
    assert_eq!(events[0].external_root.as_str(), root.display().to_string());
}

/// While a move of the external root holds the service socket, no service runs: a status reports
/// none running, and a stop and a start fail with `ServiceUnavailable`. Each reaches the socket.
#[tokio::test]
async fn during_a_move_a_status_reports_no_service_and_a_stop_and_a_start_fail() {
    let _socket = crate::SERVICE_SOCKET_TESTS.lock().await;
    let replier = crate::test_util::Replier::move_reply(lore::remote::service_socket_name());
    let global = lore_base::test_util::TempDir::new("lore-service-move-reply-");

    let (code, events) = status_in(global.path()).await;
    assert_eq!(code, 0, "the status succeeds");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].running, 0, "no service runs during a move");
    assert_eq!(
        events[0].external_root_state,
        lore::interface::LoreExternalRootState::Interrupted,
        "the move is under way"
    );
    assert_eq!(replier.answered(), 1);

    let globals = lore_revision::interface::LoreGlobalArgs::default;
    let stopped = lore::service::stop(globals(), lore::service::LoreServiceStopArgs {}, None).await;
    assert_eq!(stopped, service_unavailable(), "the stop fails");
    assert_eq!(replier.answered(), 2);

    let started =
        lore::service::start(globals(), lore::service::LoreServiceStartArgs {}, None).await;
    assert_eq!(started, service_unavailable(), "the start fails");
    assert_eq!(replier.answered(), 3);
}

/// A start against a service whose status it can't decode, as from a service of another build,
/// reports a running service.
#[tokio::test]
async fn a_start_against_a_status_that_does_not_decode_reports_a_running_service() {
    let _socket = crate::SERVICE_SOCKET_TESTS.lock().await;
    let replier = crate::test_util::Replier::with(lore::remote::service_socket_name(), vec![7]);

    let started = lore::service::start(
        lore_revision::interface::LoreGlobalArgs::default(),
        lore::service::LoreServiceStartArgs {},
        None,
    )
    .await;

    assert_eq!(started, 0, "a service is running");
    assert_eq!(replier.answered(), 1, "the start asked it for its status");
}
