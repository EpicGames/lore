// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures hold files open through std to stand in for another reader of the instance.
#![allow(clippy::disallowed_methods)]

use std::future::Ready;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_revision::instance::FailedRemoval;
use lore_revision::instance::IN_USE_RETRY_LIMIT;
use lore_revision::instance::IN_USE_RETRY_MAXIMUM_MILLIS;
use lore_revision::instance::IN_USE_RETRY_START_MILLIS;
use lore_revision::instance::delete_is_retryable;
use lore_revision::instance::is_in_use;
use lore_revision::instance::removal_is_retryable;
use lore_revision::instance::remove_instance_directory_with;
use lore_revision::instance::rename_is_retryable;
use lore_revision::instance::retry_while_in_use;

/// `predicate` as the asynchronous predicate `retry_while_in_use` takes.
fn ready(predicate: fn(&io::Error) -> bool) -> impl Fn(&io::Error) -> Ready<bool> {
    move |error| std::future::ready(predicate(error))
}

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

#[tokio::test]
async fn a_rename_is_retried_in_use_only_while_the_target_is_free() {
    let parent = lore_base::test_util::TempDir::new("lore-instance-in-use-");
    let target = parent.path().join("target");
    let busy = io::Error::from(io::ErrorKind::ResourceBusy);
    assert!(rename_is_retryable(&busy, &target).await);
    // A rename onto a non-empty directory fails this way, and no wait empties it.
    let not_empty = io::Error::from(io::ErrorKind::DirectoryNotEmpty);
    assert!(!rename_is_retryable(&not_empty, &target).await);
    std::fs::create_dir(&target).expect("occupy the target");
    assert!(!rename_is_retryable(&busy, &target).await);
}

/// A link at the target takes it, even one that leads nowhere.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_rename_onto_a_dangling_link_is_not_retried() {
    let parent = lore_base::test_util::TempDir::new("lore-instance-in-use-");
    let target = parent.path().join("target");
    std::os::unix::fs::symlink(parent.path().join("missing"), &target).expect("link the target");
    let busy = io::Error::from(io::ErrorKind::ResourceBusy);

    assert!(!rename_is_retryable(&busy, &target).await);
}

/// A delete that failed in use is retried once the directory is renamed aside, which holds the
/// aside name, and before that only while the aside name is free.
#[tokio::test]
async fn a_delete_is_retried_in_use_and_a_rename_aside_only_while_the_aside_name_is_free() {
    let parent = lore_base::test_util::TempDir::new("lore-instance-in-use-");
    let path = parent.path().join("instance");
    let aside = parent.path().join("aside");
    let failure = |directory: &std::path::Path, kind: io::ErrorKind| FailedRemoval {
        directory: directory.to_path_buf(),
        error: io::Error::from(kind),
    };
    let busy = io::ErrorKind::ResourceBusy;

    assert!(removal_is_retryable(&failure(&path, busy), Some(&aside)).await);
    std::fs::create_dir(&aside).expect("take the aside name");
    assert!(!removal_is_retryable(&failure(&path, busy), Some(&aside)).await);
    assert!(removal_is_retryable(&failure(&aside, busy), Some(&aside)).await);
    assert!(
        !removal_is_retryable(
            &failure(&aside, io::ErrorKind::PermissionDenied),
            Some(&aside)
        )
        .await
    );
}

/// Once an attempt has renamed the instance aside, a retry removes the aside alone: a new
/// directory made at the freed path is neither renamed nor removed, and the error names the
/// aside, which holds what is left.
#[tokio::test]
async fn a_retry_after_the_rename_aside_removes_only_the_aside() {
    let parent = lore_base::test_util::TempDir::new("lore-instance-in-use-");
    let path = parent.path().join("instance");
    let aside = parent.path().join("aside");
    std::fs::create_dir(&path).expect("create the instance directory");
    std::fs::write(path.join("old.txt"), b"old").expect("write a file of the instance");
    let attempts = Mutex::new(Vec::new());

    let result = remove_instance_directory_with(&path, Some(&aside), |from, to| {
        let first = {
            let mut attempts = attempts.lock().expect("the attempts");
            attempts.push((from.clone(), to.clone()));
            attempts.len() == 1
        };
        let (path, aside) = (path.clone(), aside.clone());
        async move {
            if first {
                // The rename lands, a new clone takes the freed path, and the removal fails in
                // use.
                std::fs::rename(&path, &aside).expect("rename the instance aside");
                std::fs::create_dir(&path).expect("make a new directory at the freed path");
                std::fs::write(path.join("new.txt"), b"new").expect("write a file in it");
                (aside, Err(io::Error::from(io::ErrorKind::ResourceBusy)))
            } else {
                (from, Err(io::Error::from(io::ErrorKind::PermissionDenied)))
            }
        }
    })
    .await;

    let failure = result.expect_err("the second attempt fails");
    assert_eq!(failure.directory, aside);
    assert_eq!(
        attempts.into_inner().expect("the attempts"),
        [(path.clone(), Some(aside.clone())), (aside.clone(), None)]
    );
    assert!(path.join("new.txt").exists(), "the new directory is kept");
    assert!(aside.join("old.txt").exists());
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
    let result = retry_while_in_use("test", ready(is_in_use), || async {
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
    let result: io::Result<()> = retry_while_in_use("test", ready(is_in_use), || async {
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
    let result: io::Result<()> = retry_while_in_use("test", ready(is_in_use), || async {
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

    retry_while_in_use("test", ready(delete_is_retryable), || {
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
