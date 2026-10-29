// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures hold files open through std to stand in for another reader of the instance.
#![allow(clippy::disallowed_methods)]

use std::io;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_revision::instance::IN_USE_RETRY_LIMIT;
use lore_revision::instance::IN_USE_RETRY_MAXIMUM_MILLIS;
use lore_revision::instance::IN_USE_RETRY_START_MILLIS;
use lore_revision::instance::delete_is_retryable;
use lore_revision::instance::is_in_use;
use lore_revision::instance::rename_is_retryable;
use lore_revision::instance::retry_while_in_use;

#[test]
fn a_busy_resource_is_in_use() {
    assert!(is_in_use(&io::Error::from(io::ErrorKind::ResourceBusy)));
}

#[test]
fn a_missing_path_or_a_permission_problem_is_not_in_use() {
    assert!(!is_in_use(&io::Error::from(io::ErrorKind::NotFound)));
    assert!(!is_in_use(&io::Error::from(
        io::ErrorKind::PermissionDenied
    )));
}

#[test]
fn a_delete_is_retried_in_use_or_when_it_leaves_the_directory_non_empty() {
    assert!(delete_is_retryable(&io::Error::from(
        io::ErrorKind::ResourceBusy
    )));
    assert!(delete_is_retryable(&io::Error::from(
        io::ErrorKind::DirectoryNotEmpty
    )));
    assert!(!delete_is_retryable(&io::Error::from(
        io::ErrorKind::NotFound
    )));
}

#[test]
fn a_rename_is_retried_in_use_only_while_the_target_is_free() {
    let parent = lore_base::test_util::TempDir::new("lore-instance-in-use-");
    let target = parent.path().join("target");
    let busy = io::Error::from(io::ErrorKind::ResourceBusy);
    assert!(rename_is_retryable(&busy, &target));
    // A rename onto a non-empty directory fails this way, and no wait empties it.
    let not_empty = io::Error::from(io::ErrorKind::DirectoryNotEmpty);
    assert!(!rename_is_retryable(&not_empty, &target));
    std::fs::create_dir(&target).expect("occupy the target");
    assert!(!rename_is_retryable(&busy, &target));
}

#[cfg(target_family = "windows")]
#[test]
fn access_denied_and_sharing_and_lock_violations_are_in_use_on_windows() {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    for code in [
        ERROR_ACCESS_DENIED,
        ERROR_SHARING_VIOLATION,
        ERROR_LOCK_VIOLATION,
    ] {
        assert!(
            is_in_use(&io::Error::from_raw_os_error(code as i32)),
            "{code}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_operation_failing_in_use_is_reissued_until_it_succeeds() {
    let attempts = AtomicUsize::new(0);
    let result = retry_while_in_use("test", is_in_use, || async {
        if attempts.fetch_add(1, Ordering::Relaxed) < 3 {
            Err(io::Error::from(io::ErrorKind::ResourceBusy))
        } else {
            Ok(7)
        }
    })
    .await;
    assert_eq!(result.expect("succeeds once released"), 7);
    assert_eq!(attempts.load(Ordering::Relaxed), 4);
}

#[tokio::test(start_paused = true)]
async fn any_other_failure_is_reported_at_once() {
    let attempts = AtomicUsize::new(0);
    let result: io::Result<()> = retry_while_in_use("test", is_in_use, || async {
        attempts.fetch_add(1, Ordering::Relaxed);
        Err(io::Error::from(io::ErrorKind::NotFound))
    })
    .await;
    assert_eq!(result.expect_err("fails").kind(), io::ErrorKind::NotFound);
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn an_operation_that_stays_in_use_is_reported_after_the_limit() {
    let attempts = AtomicUsize::new(0);
    let started = tokio::time::Instant::now();
    let result: io::Result<()> = retry_while_in_use("test", is_in_use, || async {
        attempts.fetch_add(1, Ordering::Relaxed);
        Err(io::Error::from(io::ErrorKind::ResourceBusy))
    })
    .await;
    let waited = started.elapsed();

    assert_eq!(
        result.expect_err("fails").kind(),
        io::ErrorKind::ResourceBusy
    );
    assert_eq!(attempts.load(Ordering::Relaxed), IN_USE_RETRY_LIMIT + 1);
    // The doubling schedule, each wait jittered by at most 100 ms either way.
    let schedule: u64 = (0..IN_USE_RETRY_LIMIT as u32)
        .map(|n| (IN_USE_RETRY_START_MILLIS << n).min(IN_USE_RETRY_MAXIMUM_MILLIS))
        .sum();
    let jitter = 100 * IN_USE_RETRY_LIMIT as u64;
    assert!(
        waited >= Duration::from_millis(schedule - jitter)
            && waited <= Duration::from_millis(schedule + jitter),
        "waited {waited:?} for a schedule of {schedule} ms"
    );
    // Long enough for a finished command in the same process to close the stores it keeps open
    // for the default ten seconds.
    assert!(waited > Duration::from_secs(10), "waited {waited:?}");
}

/// A file in `directory` held open with no sharing, as a reader that keeps it open would.
#[cfg(target_family = "windows")]
fn hold_open(directory: &std::path::Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    let path = directory.join("held");
    std::fs::write(&path, b"held").expect("write the held file");
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .expect("hold the file open")
}

#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_rename_blocked_by_an_open_file_succeeds_once_the_file_is_closed() {
    let parent = lore_base::test_util::TempDir::new("lore-instance-in-use-");
    let from = parent.path().join("from");
    let to = parent.path().join("to");
    std::fs::create_dir(&from).expect("create the instance directory");
    let mut held = Some(hold_open(&from));
    let attempts = AtomicUsize::new(0);

    retry_while_in_use(
        "test",
        |e| rename_is_retryable(e, &to),
        || {
            attempts.fetch_add(1, Ordering::Relaxed);
            let result = std::fs::rename(&from, &to);
            // The reader finishes after the first attempt it blocked.
            if result.is_err() {
                drop(held.take());
            }
            async move { result }
        },
    )
    .await
    .expect("moves once the file is closed");

    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert!(to.join("held").exists() && !from.exists());
}

#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_delete_blocked_by_an_open_file_succeeds_once_the_file_is_closed() {
    let parent = lore_base::test_util::TempDir::new("lore-instance-in-use-");
    let directory = parent.path().join("instance");
    std::fs::create_dir(&directory).expect("create the instance directory");
    let mut held = Some(hold_open(&directory));
    let attempts = AtomicUsize::new(0);

    retry_while_in_use("test", delete_is_retryable, || {
        attempts.fetch_add(1, Ordering::Relaxed);
        let result = std::fs::remove_dir_all(&directory);
        if result.is_err() {
            drop(held.take());
        }
        async move { result }
    })
    .await
    .expect("deletes once the file is closed");

    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert!(!directory.exists());
}
