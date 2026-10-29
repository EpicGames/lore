// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore::service::service_main::CloseOnExit;
use tokio::sync::oneshot;

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
