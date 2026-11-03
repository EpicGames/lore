// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use lore_base::test_util::TempDir;
use lore_io::Stat;
use lore_io::StatKind;
use lore_io::find::Opened;
use lore_io::find::entry;
use lore_io::find::is_symbolic_link;
use lore_io::find::open;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ARCHIVE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_DEDUP;
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_SYMLINK;

const UNIX_EPOCH_INTERVALS: u64 = 116_444_736_000_000_000;

/// A record listing `name` with `attributes`, reparse `tag`, a size of `len` and a last write
/// `intervals` after 1601.
fn record(name: &str, attributes: u32, tag: u32, len: u64, intervals: u64) -> WIN32_FIND_DATAW {
    let mut record = WIN32_FIND_DATAW {
        dwFileAttributes: attributes,
        ftLastWriteTime: FILETIME {
            dwLowDateTime: intervals as u32,
            dwHighDateTime: (intervals >> 32) as u32,
        },
        nFileSizeHigh: (len >> 32) as u32,
        nFileSizeLow: len as u32,
        dwReserved0: tag,
        ..WIN32_FIND_DATAW::default()
    };
    for (unit, encoded) in record.cFileName.iter_mut().zip(name.encode_utf16()) {
        *unit = encoded;
    }
    record
}

/// A record names a file or a directory by its attributes, with the size and time it holds.
#[test]
fn a_record_describes_a_file_and_a_directory() {
    let root = Path::new(r"C:\unused");
    let listed = entry(
        root,
        &record(
            "file",
            FILE_ATTRIBUTE_ARCHIVE,
            0,
            (1 << 32) + 2,
            UNIX_EPOCH_INTERVALS + 10_000_001,
        ),
    )
    .expect("a file is listed");
    assert_eq!(OsString::from("file"), listed.file_name);
    assert!(!listed.is_symlink);
    assert_eq!(
        Some(Stat {
            kind: StatKind::File,
            len: (1 << 32) + 2,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::new(1, 100)),
            mode: None,
        }),
        listed.metadata
    );

    let listed = entry(
        root,
        &record(
            "directory",
            FILE_ATTRIBUTE_DIRECTORY,
            0,
            0,
            UNIX_EPOCH_INTERVALS,
        ),
    )
    .expect("a directory is listed");
    assert_eq!(
        Some(StatKind::Directory),
        listed.metadata.map(|stat| stat.kind)
    );
}

/// `.` and `..` are not listed, and names that only start with a dot are.
#[test]
fn dot_names_are_not_listed() {
    let root = Path::new(r"C:\unused");
    for name in [".", ".."] {
        let listed = entry(
            root,
            &record(name, FILE_ATTRIBUTE_DIRECTORY, 0, 0, UNIX_EPOCH_INTERVALS),
        );
        assert!(listed.is_none(), "{name} was listed");
    }
    for name in ["...", ".hidden"] {
        let listed = entry(
            root,
            &record(name, FILE_ATTRIBUTE_ARCHIVE, 0, 0, UNIX_EPOCH_INTERVALS),
        );
        assert!(listed.is_some(), "{name} was not listed");
    }
}

/// A reparse point is a symbolic link, as `std` takes one, when its tag stands for another name,
/// as a symbolic link's and a junction's do, and not when its tag does not, or when the record is
/// no reparse point at all.
#[test]
fn a_symbolic_link_is_a_reparse_point_whose_tag_stands_for_a_name() {
    let symbolic = |attributes, tag| is_symbolic_link(&record("name", attributes, tag, 0, 0));
    assert!(symbolic(
        FILE_ATTRIBUTE_REPARSE_POINT,
        IO_REPARSE_TAG_SYMLINK
    ));
    assert!(symbolic(
        FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
        IO_REPARSE_TAG_MOUNT_POINT
    ));
    assert!(!symbolic(
        FILE_ATTRIBUTE_REPARSE_POINT,
        IO_REPARSE_TAG_DEDUP
    ));
    assert!(!symbolic(FILE_ATTRIBUTE_ARCHIVE, IO_REPARSE_TAG_SYMLINK));
}

/// A symbolic link is described by a stat of its path rather than by its record, and carries no
/// metadata where that path names nothing.
#[test]
fn a_symbolic_link_is_described_by_its_path() {
    let dir = TempDir::new("lore-io-find-link-");
    std::fs::write(dir.child("target"), b"held").expect("write the file");

    let listed = entry(
        dir.path(),
        &record(
            "target",
            FILE_ATTRIBUTE_REPARSE_POINT,
            IO_REPARSE_TAG_SYMLINK,
            99,
            UNIX_EPOCH_INTERVALS,
        ),
    )
    .expect("a symbolic link is listed");
    assert!(listed.is_symlink);
    let expected = Stat::from(&std::fs::metadata(dir.child("target")).expect("stat the file"));
    assert_eq!(Some(expected), listed.metadata);

    let listed = entry(
        dir.path(),
        &record(
            "gone",
            FILE_ATTRIBUTE_REPARSE_POINT,
            IO_REPARSE_TAG_SYMLINK,
            0,
            UNIX_EPOCH_INTERVALS,
        ),
    )
    .expect("a symbolic link is listed");
    assert!(listed.is_symlink);
    assert_eq!(None, listed.metadata);
}

/// A reparse point that stands for no name is described by its record, as `std` lists it.
#[test]
fn a_reparse_point_that_is_no_symbolic_link_is_described_by_its_record() {
    let listed = entry(
        Path::new(r"C:\unused"),
        &record(
            "deduplicated",
            FILE_ATTRIBUTE_REPARSE_POINT,
            IO_REPARSE_TAG_DEDUP,
            7,
            UNIX_EPOCH_INTERVALS,
        ),
    )
    .expect("the file is listed");
    assert!(!listed.is_symlink);
    assert_eq!(
        Some(Stat {
            kind: StatKind::File,
            len: 7,
            modified: Some(SystemTime::UNIX_EPOCH),
            mode: None,
        }),
        listed.metadata
    );
}

/// A time `SystemTime` cannot hold has the name described by a stat of its path.
#[test]
fn a_time_system_time_cannot_hold_is_read_by_path() {
    let dir = TempDir::new("lore-io-find-time-");
    std::fs::write(dir.child("file"), b"held").expect("write the file");

    let listed = entry(
        dir.path(),
        &record("file", FILE_ATTRIBUTE_ARCHIVE, 0, 99, 1 << 63),
    )
    .expect("the file is listed");
    let expected = Stat::from(&std::fs::metadata(dir.child("file")).expect("stat the file"));
    assert_eq!(Some(expected), listed.metadata);
}

/// A relative path and a path past `MAX_PATH` without the verbatim prefix are handed back, a
/// missing directory fails with the code `std` reports, and a directory opens a search.
#[test]
fn a_search_declines_what_std_answers_differently() {
    let dir = TempDir::new("lore-io-find-open-");
    let mut first = WIN32_FIND_DATAW::default();

    let relative = Path::new("relative").to_path_buf();
    assert!(matches!(
        open(relative.clone(), &mut first),
        Ok(Opened::Declined(path)) if path == relative
    ));
    let long = dir.path().join("long".repeat(80));
    assert!(matches!(open(long, &mut first), Ok(Opened::Declined(_))));

    let absent = dir.child("absent");
    let Err(error) = open(absent.clone(), &mut first) else {
        panic!("a missing directory opens no search");
    };
    let expected = std::fs::read_dir(&absent).expect_err("nothing is there");
    assert_eq!(expected.raw_os_error(), error.raw_os_error());

    assert!(matches!(
        open(dir.path().to_path_buf(), &mut first),
        Ok(Opened::Search(_))
    ));
}

/// A stream holds less than the `std` walk it lists a declined path with, which it keeps boxed,
/// so a stream held across a walk carries no whole find record.
#[test]
fn a_stream_holds_less_than_the_std_walk_it_falls_back_to() {
    assert!(
        size_of::<lore_io::DirStream>() < size_of::<std::fs::ReadDir>(),
        "a stream holds {} bytes for a walk of {}",
        size_of::<lore_io::DirStream>(),
        size_of::<std::fs::ReadDir>()
    );
}
