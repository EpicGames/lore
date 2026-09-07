// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_base::fs::lock::*;
use lore_base::test_util::TempDir;

/// The lock excludes a second acquirer, which waits rather than failing. Both `flock` and
/// `LockFileEx` contend between separate handles on one file, so one process is enough to
/// observe it: the second acquisition is still pending when the window closes, where a
/// failing implementation would have returned an error and a broken one would have returned
/// a guard.
#[tokio::test]
async fn a_second_acquirer_waits_while_the_lock_is_held() {
    let dir = TempDir::new("lore-base-lock-held");
    let held = FSLock::acquire_directory_lock(dir.path())
        .await
        .expect("first acquisition");

    // Several poll intervals, so the second acquirer has attempted and backed off repeatedly.
    let waited = tokio::time::timeout(
        Duration::from_millis(50),
        FSLock::acquire_directory_lock(dir.path()),
    )
    .await;

    assert!(
        waited.is_err(),
        "a second acquirer must neither take a held lock nor fail on it"
    );
    drop(held);
}

/// **Trying a lock another holder has answers at once, and trying a free one takes it.**
/// Observed from a second open file description, which contends with the first, so an
/// attempt that waited, or one that answered without locking, is seen.
#[tokio::test]
async fn trying_a_held_lock_answers_at_once_and_trying_a_free_one_takes_it() {
    let dir = TempDir::new("lore-base-lock-try");
    let path = dir.path().join("lock");
    let held = FSLock::acquire_exact_path(&path)
        .await
        .expect("first acquisition");

    let refused = tokio::time::timeout(
        Duration::from_secs(5),
        FSLock::try_acquire_exact_path(&path),
    )
    .await
    .expect("a held lock is refused without waiting")
    .expect("contention is not an error");
    assert!(refused.is_none(), "another holder has the lock");

    drop(held);
    let taken = FSLock::try_acquire_exact_path(&path)
        .await
        .expect("attempts")
        .expect("a free lock is taken");
    let waited =
        tokio::time::timeout(Duration::from_millis(50), FSLock::acquire_exact_path(&path)).await;
    assert!(
        waited.is_err(),
        "and what the attempt took excludes another description"
    );
    drop(taken);
}

/// Dropping the guard releases the OS lock, so the next acquisition completes. Bounded by a
/// timeout because the wait is otherwise unbounded: a lock that was not released would hang
/// the test rather than fail it.
#[tokio::test]
async fn the_lock_is_acquirable_once_the_guard_drops() {
    let dir = TempDir::new("lore-base-lock-released");
    let held = FSLock::acquire_directory_lock(dir.path())
        .await
        .expect("first acquisition");
    drop(held);

    tokio::time::timeout(
        Duration::from_secs(5),
        FSLock::acquire_directory_lock(dir.path()),
    )
    .await
    .expect("the lock must be acquirable once the guard drops")
    .expect("second acquisition");
}
