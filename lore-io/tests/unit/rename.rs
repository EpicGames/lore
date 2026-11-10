// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_base::test_util::TempDir;
use lore_io::rename::rename_no_replace;

/// The rename refuses an empty directory at its target rather than replacing it, as a plain
/// rename would on Unix, and leaves both where they were.
#[test]
fn a_rename_refuses_an_empty_directory_at_its_target() {
    let root = TempDir::new("lore-io-rename-no-replace-");
    let from = root.join("from");
    let to = root.join("to");
    std::fs::create_dir(&from).expect("create the source");
    std::fs::write(from.join("file"), b"file").expect("write a file in it");
    std::fs::create_dir(&to).expect("create an empty target");

    let renamed = rename_no_replace(&from, &to);

    assert_eq!(
        renamed.map_err(|err| err.kind()),
        Err(std::io::ErrorKind::AlreadyExists)
    );
    assert_eq!(std::fs::read_dir(&to).expect("list the target").count(), 0);
    assert!(from.join("file").is_file(), "the source stays where it was");
}

/// The rename moves a directory to a target where nothing is.
#[test]
fn a_rename_moves_a_directory_where_nothing_is() {
    let root = TempDir::new("lore-io-rename-free-");
    let from = root.join("from");
    let to = root.join("to");
    std::fs::create_dir(&from).expect("create the source");
    std::fs::write(from.join("file"), b"file").expect("write a file in it");

    rename_no_replace(&from, &to).expect("rename");

    assert!(to.join("file").is_file() && !from.exists());
}
