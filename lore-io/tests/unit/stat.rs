// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use lore_io::Stat;
use lore_io::StatKind;
use lore_io::psync::attribute_stat;
use lore_io::stat::system_time;

/// A file and a directory are answered by the attribute query, with what `std` gives for them.
#[test]
fn an_attribute_query_answers_a_file_and_a_directory() {
    let dir = lore_base::test_util::TempDir::new("lore-io-attribute-");
    let file = dir.child("file");
    std::fs::write(&file, b"held").expect("write the file");

    let answered = attribute_stat(&file, true)
        .expect("the query answers a file")
        .expect("the file is there");
    assert_eq!(
        Stat::from(&std::fs::metadata(&file).expect("stat the file")),
        answered
    );

    let directory = attribute_stat(dir.path(), true)
        .expect("the query answers a directory")
        .expect("the directory is there");
    assert_eq!(StatKind::Directory, directory.kind);
}

/// A missing name is answered by the attribute query, with the OS code `std` reports for it.
#[test]
fn an_attribute_query_answers_a_missing_name() {
    let dir = lore_base::test_util::TempDir::new("lore-io-attribute-");
    let absent = dir.child("absent");

    let error = attribute_stat(&absent, true)
        .expect("the query answers a missing name")
        .expect_err("nothing is there");
    let expected = std::fs::metadata(&absent).expect_err("nothing is there");
    assert_eq!(expected.raw_os_error(), error.raw_os_error());
}

/// A relative path, a path past `MAX_PATH` without the verbatim prefix, and a symbolic link are
/// left to `std`, which resolves the first, prefixes the second and follows the third.
#[test]
fn an_attribute_query_declines_what_std_answers_differently() {
    let dir = lore_base::test_util::TempDir::new("lore-io-attribute-");
    assert!(attribute_stat(Path::new("relative"), true).is_none());
    assert!(attribute_stat(&dir.path().join("long".repeat(80)), true).is_none());

    let target = dir.child("target");
    std::fs::write(&target, b"held").expect("write the target");
    let link = dir.child("link");
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        eprintln!("skipping the link: this machine creates no links");
        return;
    }
    assert!(attribute_stat(&link, true).is_none());
}

/// Not following a reparse point, the attribute query answers a symbolic link as the name it is:
/// a link made to a file as a `File`, whose time is the link's own.
#[test]
fn an_attribute_query_not_following_answers_a_symbolic_link_itself() {
    let dir = lore_base::test_util::TempDir::new("lore-io-attribute-");
    let target = dir.child("target");
    std::fs::write(&target, b"held").expect("write the target");
    let link = dir.child("link");
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        eprintln!("skipping: this machine creates no links");
        return;
    }

    let described = attribute_stat(&link, false)
        .expect("the query answers a symbolic link")
        .expect("the symbolic link is there");
    assert_eq!(StatKind::File, described.kind);
    assert_eq!(
        std::fs::symlink_metadata(&link)
            .expect("read the link")
            .modified()
            .ok(),
        described.modified
    );
}

/// A file time converts to the `SystemTime` `std` gives for it, and one past what `SystemTime`
/// holds converts to nothing.
#[test]
fn a_file_time_converts_as_std_converts_it() {
    const UNIX_EPOCH_INTERVALS: u64 = 116_444_736_000_000_000;

    assert_eq!(
        Some(SystemTime::UNIX_EPOCH),
        system_time(UNIX_EPOCH_INTERVALS)
    );
    assert_eq!(
        Some(SystemTime::UNIX_EPOCH + Duration::new(1, 100)),
        system_time(UNIX_EPOCH_INTERVALS + 10_000_001)
    );
    assert_eq!(
        Some(SystemTime::UNIX_EPOCH - Duration::from_secs(11_644_473_600)),
        system_time(0)
    );
    assert_eq!(None, system_time(1 << 63));
}
