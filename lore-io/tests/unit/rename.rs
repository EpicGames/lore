// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_base::test_util::TempDir;
use lore_io::rename::rename_durable;
use lore_io::rename::rename_no_replace;

/// The rename refuses an empty directory at its target rather than replacing it, as a plain
/// rename would on Unix, and leaves both where they were, with or without durability.
#[test]
fn a_rename_refuses_an_empty_directory_at_its_target() {
    for durable in [false, true] {
        let root = TempDir::new("lore-io-rename-no-replace-");
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir(&from).expect("create the source");
        std::fs::write(from.join("file"), b"file").expect("write a file in it");
        std::fs::create_dir(&to).expect("create an empty target");

        let renamed = rename_no_replace(&from, &to, durable);

        assert_eq!(
            renamed.map_err(|err| err.kind()),
            Err(std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read_dir(&to).expect("list the target").count(), 0);
        assert!(from.join("file").is_file(), "the source stays where it was");
    }
}

/// The rename moves a directory to a target where nothing is, with or without durability.
#[test]
fn a_rename_moves_a_directory_where_nothing_is() {
    for durable in [false, true] {
        let root = TempDir::new("lore-io-rename-free-");
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir(&from).expect("create the source");
        std::fs::write(from.join("file"), b"file").expect("write a file in it");

        rename_no_replace(&from, &to, durable).expect("rename");

        assert!(to.join("file").is_file() && !from.exists());
    }
}

/// The durable replacing rename replaces a file at its target, and moves one where nothing is.
#[test]
fn a_durable_rename_replaces_a_file_at_its_target() {
    let root = TempDir::new("lore-io-rename-durable-");
    let from = root.join("from");
    let to = root.join("to");
    std::fs::write(&from, b"new").expect("write the source");
    std::fs::write(&to, b"old").expect("write the target");

    rename_durable(&from, &to).expect("rename over the target");

    assert_eq!(std::fs::read(&to).expect("read the target"), b"new");
    assert!(!from.exists());

    let elsewhere = root.join("elsewhere");
    rename_durable(&to, &elsewhere).expect("rename where nothing is");
    assert_eq!(
        std::fs::read(&elsewhere).expect("read the moved file"),
        b"new"
    );
}

/// A durable rename in a directory its caller can write but not read renames, then fails to open
/// the directory to flush it, and reports that failure.
#[cfg(target_family = "unix")]
#[test]
fn a_durable_rename_reports_a_failed_flush() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let root = TempDir::new("lore-io-rename-unflushable-");
    let parent = root.join("parent");
    std::fs::create_dir(&parent).expect("create the parent");
    std::fs::write(parent.join("a"), b"a").expect("write a file");
    std::fs::write(parent.join("c"), b"c").expect("write a file");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o300))
        .expect("make the parent unreadable");

    let replaced = rename_durable(&parent.join("a"), &parent.join("b"));
    let placed = rename_no_replace(&parent.join("c"), &parent.join("d"), true);

    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
        .expect("make the parent readable again");
    assert_eq!(
        replaced.map_err(|err| err.kind()),
        Err(std::io::ErrorKind::PermissionDenied)
    );
    assert_eq!(
        placed.map_err(|err| err.kind()),
        Err(std::io::ErrorKind::PermissionDenied)
    );
    assert!(parent.join("b").is_file() && parent.join("d").is_file());
}

/// On macOS, a durable rename flushes its directory through `F_FULLFSYNC`, which `fsync` there
/// does not reach.
#[cfg(target_os = "macos")]
#[test]
fn a_durable_rename_flushes_with_a_full_sync_on_macos() {
    use lore_io::psync::test_util::full_syncs_on_this_thread;

    let root = TempDir::new("lore-io-rename-full-sync-");
    let from = root.join("from");
    std::fs::write(&from, b"file").expect("write the source");

    let before = full_syncs_on_this_thread();
    rename_durable(&from, &root.join("to")).expect("rename");
    let replaced = full_syncs_on_this_thread();
    rename_no_replace(&root.join("to"), &root.join("placed"), true).expect("rename");

    assert!(replaced > before);
    assert!(full_syncs_on_this_thread() > replaced);
}
