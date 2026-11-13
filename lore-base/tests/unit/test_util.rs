// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::ErrorKind;
use std::path::PathBuf;

use lore_base::test_util::*;

#[test]
fn removes_the_directory_on_drop() {
    let path = {
        let temp = TempDir::new("lore-test-util-drop-");
        std::fs::write(temp.child("file.txt"), b"contents").expect("write");
        temp.path().to_path_buf()
    };
    assert_eq!(
        path.exists(),
        keep_test_data(),
        "{path:?} should be gone once the TempDir is dropped, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
    );
}

#[test]
fn removes_the_directory_while_unwinding() {
    // Removal has to happen on the failing path too, not just when a test
    // returns normally.
    let captured = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
    let inner = std::sync::Arc::clone(&captured);
    let result = std::panic::catch_unwind(move || {
        let temp = TempDir::new("lore-test-util-panic-");
        *inner.lock().expect("lock") = temp.path().to_path_buf();
        panic!("as a failing assertion would");
    });
    assert!(result.is_err(), "the closure was supposed to panic");
    let path = captured.lock().expect("lock").clone();
    assert_eq!(
        path.exists(),
        keep_test_data(),
        "{path:?} should be gone after unwinding, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
    );
}

#[test]
fn temp_file_holds_what_was_written() {
    let file = TempFile::with_contents("lore-test-util-file-", b"scratch content");
    assert_eq!(
        std::fs::read(file.path()).expect("read back"),
        b"scratch content",
        "the file must hold exactly what it was created with"
    );
}

#[test]
fn temp_file_goes_on_drop() {
    let path = {
        let file = TempFile::with_contents("lore-test-util-file-drop-", b"x");
        file.path().to_path_buf()
    };
    assert_eq!(
        path.exists(),
        keep_test_data(),
        "{path:?} should be gone once the TempFile is dropped, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
    );
}

#[test]
fn temp_file_goes_while_unwinding() {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
    let inner = std::sync::Arc::clone(&captured);
    let result = std::panic::catch_unwind(move || {
        let file = TempFile::with_contents("lore-test-util-file-panic-", b"x");
        *inner.lock().expect("lock") = file.path().to_path_buf();
        panic!("as a failing assertion would");
    });
    assert!(result.is_err(), "the closure was supposed to panic");
    let path = captured.lock().expect("lock").clone();
    assert_eq!(
        path.exists(),
        keep_test_data(),
        "{path:?} should be gone after unwinding, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
    );
}

/// The one test that covers the `keep` branch of both `Drop` impls.
///
/// It needs a child process: `keep_test_data` caches its answer in a
/// `OnceLock`, so the variable cannot be flipped part-way through a run. The
/// child creates a guard and records where it put it; the parent checks that
/// the path outlived the child, and removes it.
#[test]
fn keep_test_data_leaves_them_on_disk() {
    const RECORD_VAR: &str = "LORE_TEST_UTIL_RECORD_TO";
    const TEST_NAME: &str = "test_util::keep_test_data_leaves_them_on_disk";

    if let Some(record_to) = std::env::var_os(RECORD_VAR) {
        let dir = TempDir::new("lore-test-util-kept-dir-");
        let file = TempFile::with_contents("lore-test-util-kept-file-", b"kept");
        let record = format!("{}\n{}", dir.path().display(), file.path().display());
        std::fs::write(record_to, record).expect("record the paths");
        return;
    }

    let record = TempDir::new("lore-test-util-record-");
    let record_to = record.child("paths");
    let status = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(KEEP_TEST_DATA_VAR, "1")
        .env(RECORD_VAR, &record_to)
        .status()
        .expect("run the child");
    assert!(status.success(), "the child run should have passed");

    let recorded = std::fs::read_to_string(&record_to).expect("read the recorded paths");
    let mut lines = recorded.lines();
    let kept_dir = PathBuf::from(lines.next().expect("a directory path"));
    let kept_file = PathBuf::from(lines.next().expect("a file path"));

    assert!(
        kept_dir.is_dir(),
        "{kept_dir:?} should have survived the child, which had {KEEP_TEST_DATA_VAR} set"
    );
    assert!(
        kept_file.is_file(),
        "{kept_file:?} should have survived the child, which had {KEEP_TEST_DATA_VAR} set"
    );

    // Nothing owns these now: the child deliberately let them go.
    let _ = std::fs::remove_dir_all(&kept_dir);
    let _ = std::fs::remove_file(&kept_file);
}

#[test]
fn names_do_not_collide() {
    let first = TempDir::new("lore-test-util-unique-");
    let second = TempDir::new("lore-test-util-unique-");
    assert_ne!(
        first.path(),
        second.path(),
        "two directories asked for with the same prefix must not collide"
    );
}

/// A connection to the closed port is refused, on every platform the tests run on.
#[test]
fn a_closed_port_refuses_connections() {
    let connected = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], CLOSED_PORT)),
        std::time::Duration::from_secs(5),
    );

    assert_eq!(
        connected.err().map(|err| err.kind()),
        Some(ErrorKind::ConnectionRefused)
    );
}
