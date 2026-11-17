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

#[test]
fn try_acquire_reports_a_held_lock_rather_than_waiting() {
    let dir = TempDir::new("lore-base-lock-try");
    let path = dir.path().join("name");
    let held = FSLock::try_acquire_file_lock(&path)
        .expect("first attempt")
        .expect("a free lock is taken");

    assert!(
        FSLock::try_acquire_file_lock(&path)
            .expect("second attempt")
            .is_none(),
        "a held lock is reported as held"
    );

    drop(held);
    assert!(
        FSLock::try_acquire_file_lock(&path)
            .expect("third attempt")
            .is_some(),
        "a released lock is taken again"
    );
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

/// A mode a test takes a lock in.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Shared,
    Exclusive,
}

/// Tries the lock guarding `path` in `mode`, without waiting.
fn try_in(path: &std::path::Path, mode: Mode) -> Option<FSLock> {
    match mode {
        Mode::Shared => FSLock::try_acquire_shared_file_lock(path),
        Mode::Exclusive => FSLock::try_acquire_file_lock(path),
    }
    .expect("attempt")
}

/// The pairs of a held mode and a tried one, with whether the try takes the lock.
const MODE_PAIRS: [(Mode, Mode, bool); 4] = [
    (Mode::Shared, Mode::Shared, true),
    (Mode::Shared, Mode::Exclusive, false),
    (Mode::Exclusive, Mode::Shared, false),
    (Mode::Exclusive, Mode::Exclusive, false),
];

/// **A shared lock stands beside another shared one, and excludes an exclusive one both ways.**
/// Observed from a second open file description in this process, which contends with the first.
#[test]
fn a_shared_lock_admits_shared_holders_and_excludes_an_exclusive_one_in_one_process() {
    let dir = TempDir::new("lore-base-lock-shared");
    let path = dir.path().join("name");
    for (held, tried, taken) in MODE_PAIRS {
        let holder = try_in(&path, held).expect("a free lock is taken");
        assert_eq!(
            try_in(&path, tried).is_some(),
            taken,
            "{tried:?} beside {held:?}"
        );
        drop(holder);
    }
    assert!(
        try_in(&path, Mode::Exclusive).is_some(),
        "the released shared locks leave the lock free"
    );
}

/// **The shared mode excludes and admits across processes as within one.** The child is this
/// test binary run again, which tries the given mode on the parent's lock and passes only if the
/// try answers as expected.
#[test]
fn a_shared_lock_admits_shared_holders_and_excludes_an_exclusive_one_across_processes() {
    const LOCK_VAR: &str = "LORE_LOCK_TEST_CHILD_PATH";
    const MODE_VAR: &str = "LORE_LOCK_TEST_CHILD_MODE";
    const TAKEN_VAR: &str = "LORE_LOCK_TEST_CHILD_TAKES";
    const TEST_NAME: &str = "fs::lock::a_shared_lock_admits_shared_holders_and_excludes_an_exclusive_one_across_processes";

    if let Some(path) = std::env::var_os(LOCK_VAR) {
        let mode = match std::env::var(MODE_VAR).as_deref() {
            Ok("shared") => Mode::Shared,
            _ => Mode::Exclusive,
        };
        let taken = try_in(std::path::Path::new(&path), mode).is_some();
        assert_eq!(taken, std::env::var_os(TAKEN_VAR).is_some(), "{mode:?}");
        return;
    }

    let dir = TempDir::new("lore-base-lock-shared-child");
    let path = dir.path().join("name");
    for (held, tried, taken) in MODE_PAIRS {
        let holder = try_in(&path, held).expect("a free lock is taken");
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        child
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(LOCK_VAR, &path)
            .env(
                MODE_VAR,
                match tried {
                    Mode::Shared => "shared",
                    Mode::Exclusive => "exclusive",
                },
            );
        if taken {
            child.env(TAKEN_VAR, "1");
        }
        let output = child.output().expect("run the child");
        assert!(
            output.status.success(),
            "{tried:?} in another process beside {held:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child ran the test"
        );
        drop(holder);
    }
}
