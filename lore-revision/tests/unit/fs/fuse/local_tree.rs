// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// The tests plant entries as a mount would have left them under `write/`.
#![allow(clippy::disallowed_methods)]

use std::ffi::CString;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_revision::fs::fuse::local_tree::LocalTree;

/// The names of the entries a scan of `dir` keeps at its top.
fn scanned(dir: &Path) -> Vec<String> {
    let tree = LocalTree::scan(dir).expect("scan");
    tree.top()
        .iter()
        .map(|entry| tree.name(entry).to_owned())
        .collect()
}

#[test]
fn a_scan_skips_an_entry_no_file_directory_or_symlink() {
    let dir = TempDir::new("lore-fuse-local-tree-");
    std::fs::write(dir.path().join("kept.txt"), b"k").expect("a file");
    let fifo = CString::new(dir.path().join("fifo").as_os_str().as_bytes()).expect("a path");
    // SAFETY: `fifo` is NUL-terminated.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "a FIFO");

    assert_eq!(scanned(dir.path()), ["kept.txt"]);
}

/// A directory removed on drop.
struct Removed(PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A new directory on a file system that takes names that are not UTF-8, as one that
/// enforces an encoding does not: under the temporary directory, else under `/dev/shm`.
fn taking_any_name() -> Option<Removed> {
    let unique = format!(
        "lore-fuse-local-tree-bytes-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    );
    for parent in [std::env::temp_dir(), PathBuf::from("/dev/shm")] {
        let dir = parent.join(&unique);
        if std::fs::create_dir(&dir).is_err() {
            continue;
        }
        let dir = Removed(dir);
        let probe = dir.0.join(OsStr::from_bytes(b"probe-\xff"));
        if std::fs::write(&probe, b"").is_ok() {
            std::fs::remove_file(&probe).expect("remove the probe");
            return Some(dir);
        }
    }
    None
}

#[test]
fn a_scan_skips_a_name_that_is_not_utf8() {
    let Some(dir) = taking_any_name() else {
        eprintln!("skipped: no file system here takes a name that is not UTF-8");
        return;
    };
    std::fs::write(dir.0.join("kept.txt"), b"k").expect("a file");
    std::fs::write(dir.0.join(OsStr::from_bytes(b"not-\xff-utf8")), b"x")
        .expect("a file named by bytes that are not UTF-8");

    assert_eq!(scanned(&dir.0), ["kept.txt"]);
}
