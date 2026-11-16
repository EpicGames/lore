// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What an open handle tells about the file it has open: whether a path names it, and its
//! exclusive lock.

use lore_base::test_util::TempDir;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::LockAttempt;
use lore_io::OpenOptions;
use lore_io::lock_refused;

async fn open(path: &std::path::Path) -> IoFile {
    IoDriver::global()
        .open(
            path,
            &OpenOptions::new().read(true).write(true).create(true),
        )
        .await
        .expect("open the file")
}

/// A handle is at the path it was opened by until the file is renamed away, and then at the path
/// it was renamed to, not at a new file that takes its old name.
#[tokio::test]
async fn a_handle_is_at_the_path_of_the_file_it_has_open() {
    let dir = TempDir::new("lore-io-file-is-at-");
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    let file = open(&first).await;

    assert!(file.is_at(&first).await.expect("compare"));
    assert!(
        !file.is_at(&second).await.expect("compare"),
        "a path naming nothing"
    );

    IoDriver::global()
        .rename(&first, &second)
        .await
        .expect("rename the file");
    std::fs::write(&first, b"another file").expect("create a file at the old name");

    assert!(file.is_at(&second).await.expect("compare"));
    assert!(
        !file.is_at(&first).await.expect("compare"),
        "a path naming another file"
    );
}

/// The exclusive lock goes to one handle of a file at a time, and is free again once every clone
/// of the handle holding it is dropped.
#[tokio::test]
async fn the_lock_of_a_file_is_free_once_every_clone_holding_it_is_gone() {
    let dir = TempDir::new("lore-io-file-lock-");
    let path = dir.path().join("locked");
    let holder = open(&path).await;
    let waiter = open(&path).await;

    assert_eq!(
        holder.try_lock().expect("lock"),
        LockAttempt::Acquired,
        "an unlocked file"
    );
    assert_eq!(
        waiter.try_lock().expect("lock"),
        LockAttempt::Busy,
        "a file another handle holds"
    );

    let clone = holder.clone();
    drop(holder);
    assert_eq!(
        waiter.try_lock().expect("lock"),
        LockAttempt::Busy,
        "a clone still holds it"
    );
    drop(clone);
    assert_eq!(
        waiter.try_lock().expect("lock"),
        LockAttempt::Acquired,
        "every clone is gone"
    );
}

/// A lock attempt is refused for the codes a file system without advisory locks answers with, and
/// not for the one another holder of the lock makes it fail with.
#[test]
fn a_lock_is_refused_for_the_codes_of_a_file_system_without_locks() {
    #[cfg(target_family = "windows")]
    let (refusals, busy) = (
        [
            windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED as i32,
            windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION as i32,
        ],
        windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32,
    );
    #[cfg(not(target_family = "windows"))]
    let (refusals, busy) = (
        [libc::ENOLCK, libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS],
        libc::EWOULDBLOCK,
    );
    for code in refusals {
        assert!(
            lock_refused(&std::io::Error::from_raw_os_error(code)),
            "code {code}"
        );
    }
    assert!(lock_refused(&std::io::Error::from(
        std::io::ErrorKind::Unsupported
    )));
    assert!(
        !lock_refused(&std::io::Error::from_raw_os_error(busy)),
        "another holder"
    );
}
