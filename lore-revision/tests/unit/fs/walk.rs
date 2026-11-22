// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! `clone_into` copies a tree with contents, permissions, modification times and symbolic links
//! kept, through each backend, leaves out what it is told to and what disappears, never takes
//! over an entry that exists, and fails on a file its backend cannot clone unless told to copy
//! it. `measure` sizes a copy, `find_directories` finds directories in a tree, and
//! `copy_faithfully` copies a tree with what cloning leaves, each within the walk's bounds and on
//! the threads the syscall pool lends.

// Test fixtures write to the filesystem outside the repo write-token discipline.
#![allow(clippy::disallowed_methods)]

use std::fs::FileTimes;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;

use lore_base::test_util::TempDir;
use lore_io::clone::Backend;
use lore_io::clone::Kind;
use lore_io::clone::Source;
use lore_io::clone::clone_entry;
use lore_io::clone::detect;
use lore_revision::fs::walk::Found;
use lore_revision::fs::walk::Progress;
use lore_revision::fs::walk::TreeSize;
use lore_revision::fs::walk::Visit;
use lore_revision::fs::walk::clone_into;
use lore_revision::fs::walk::clone_into_with;
use lore_revision::fs::walk::measure;

/// A time with a nanosecond part, so a copy that rounds it is caught.
fn odd_time(offset: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000 + offset, 123_456_789)
}

fn set_modified(path: &Path, time: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open to set the time")
        .set_times(FileTimes::new().set_modified(time))
        .expect("set the time");
}

fn modified(path: &Path) -> SystemTime {
    std::fs::symlink_metadata(path)
        .expect("metadata")
        .modified()
        .expect("modification time")
}

fn every_entry(_path: &Path, _relative: &Path, _kind: Kind) -> Visit {
    Visit::Clone
}

/// Visits every directory entry by entry, so no directory is cloned as a whole.
fn each_entry(_path: &Path, _relative: &Path, kind: Kind) -> Visit {
    if kind == Kind::Directory {
        Visit::EachEntry
    } else {
        Visit::Clone
    }
}

fn source_tree(root: &TempDir) -> PathBuf {
    let source = root.join("source");
    std::fs::create_dir_all(source.join("a/b")).expect("create source tree");
    std::fs::write(source.join("top.txt"), b"top").expect("write file");
    std::fs::write(source.join("a/b/deep.bin"), vec![7u8; 100_000]).expect("write file");
    set_modified(&source.join("top.txt"), odd_time(1));
    set_modified(&source.join("a/b/deep.bin"), odd_time(2));
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink("top.txt", source.join("link")).expect("create link");
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink("/", source.join("a/outside")).expect("create link out");
    source
}

/// The links `source_tree` makes.
const LINKS: u64 = if cfg!(target_family = "unix") { 2 } else { 0 };

/// The backend `detect` picks for the scratch volume, and the copy backend, so each test runs
/// the backend that clones here as well as the one that copies.
fn backends(root: &TempDir) -> Vec<Backend> {
    let found = native(root);
    if found == Backend::Copy {
        vec![Backend::Copy]
    } else {
        vec![found, Backend::Copy]
    }
}

fn native(root: &TempDir) -> Backend {
    let probes = root.join("probes");
    std::fs::create_dir_all(&probes).expect("create the probe directory");
    detect(&probes, &probes)
        .expect("detect the backend")
        .backend
}

fn clone_tree(backend: Backend, source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::create_dir(destination)?;
    clone_into(
        backend,
        source,
        destination,
        &every_entry,
        false,
        &Progress::default(),
    )
    .map(drop)
}

/// Gives the directories it names back their owner's write bit when dropped, so a test that
/// fails with a read-only directory in its tree still has the tree removed.
#[cfg(target_family = "unix")]
struct Writable(Vec<PathBuf>);

#[cfg(target_family = "unix")]
impl Drop for Writable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        for path in &self.0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
    }
}

#[cfg(target_family = "unix")]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn a_cloned_tree_keeps_contents_and_modification_times() {
    let root = TempDir::new("walk-tree-");
    let source = source_tree(&root);

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        clone_tree(backend, &source, &destination).expect("clone the tree");

        for relative in ["top.txt", "a/b/deep.bin"] {
            assert_eq!(
                std::fs::read(source.join(relative)).expect("read source"),
                std::fs::read(destination.join(relative)).expect("read clone"),
                "{relative} has to hold the same bytes ({backend:?})"
            );
            assert_eq!(
                modified(&source.join(relative)),
                modified(&destination.join(relative)),
                "{relative} has to keep its modification time exactly ({backend:?})"
            );
        }

        // The copy is independent of its source.
        std::fs::write(destination.join("top.txt"), b"changed").expect("write clone");
        assert_eq!(
            std::fs::read(source.join("top.txt")).expect("read source"),
            b"top"
        );
    }
}

#[cfg(target_family = "unix")]
#[test]
fn a_cloned_tree_keeps_links_as_links_and_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new("walk-links-");
    let source = source_tree(&root);
    let mut writable = Writable(vec![source.join("a/b")]);
    std::fs::set_permissions(
        source.join("top.txt"),
        std::fs::Permissions::from_mode(0o751),
    )
    .expect("set the mode");
    std::fs::set_permissions(source.join("a"), std::fs::Permissions::from_mode(0o750))
        .expect("set the directory mode");
    std::fs::set_permissions(source.join("a/b"), std::fs::Permissions::from_mode(0o555))
        .expect("set the directory mode");

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        writable.0.push(destination.join("a/b"));
        clone_tree(backend, &source, &destination).expect("clone the tree");

        assert_eq!(
            std::fs::read_link(destination.join("link")).expect("the link is copied as a link"),
            PathBuf::from("top.txt")
        );
        assert_eq!(
            std::fs::read_link(destination.join("a/outside"))
                .expect("the link is copied as a link"),
            PathBuf::from("/"),
            "a link out of the tree is copied, not followed ({backend:?})"
        );
        assert_eq!(mode(&destination.join("top.txt")), 0o751, "{backend:?}");
        assert_eq!(mode(&destination.join("a")), 0o750, "{backend:?}");
        assert_eq!(mode(&destination.join("a/b")), 0o555, "{backend:?}");
    }
}

/// A directory its owner cannot add entries to is filled before it takes its permissions, at
/// every depth.
#[cfg(target_family = "unix")]
#[test]
fn read_only_directories_are_filled_and_keep_their_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new("walk-read-only-");
    let source = root.join("source");
    std::fs::create_dir_all(source.join("ro/inner/deeper")).expect("create the tree");
    std::fs::write(source.join("ro/inner/deeper/file.txt"), b"deep").expect("write");
    std::fs::write(source.join("ro/file.txt"), b"top").expect("write");
    let mut writable = Writable(Vec::new());
    for (relative, mode) in [
        ("ro/inner/deeper", 0o555),
        ("ro/inner", 0o500),
        ("ro", 0o555),
    ] {
        writable.0.push(source.join(relative));
        std::fs::set_permissions(source.join(relative), std::fs::Permissions::from_mode(mode))
            .expect("set the mode");
    }

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir(&destination).expect("create the destination");
        for relative in ["ro", "ro/inner", "ro/inner/deeper"] {
            writable.0.push(destination.join(relative));
        }
        clone_into(
            backend,
            &source,
            &destination,
            &each_entry,
            false,
            &Progress::default(),
        )
        .expect("clone the tree");

        assert_eq!(
            std::fs::read(destination.join("ro/inner/deeper/file.txt")).expect("read"),
            b"deep"
        );
        assert_eq!(mode(&destination.join("ro")), 0o555, "{backend:?}");
        assert_eq!(mode(&destination.join("ro/inner")), 0o500, "{backend:?}");
        assert_eq!(
            mode(&destination.join("ro/inner/deeper")),
            0o555,
            "{backend:?}"
        );
    }
}

#[test]
fn the_copy_backend_counts_every_file_it_copies() {
    let root = TempDir::new("walk-copy-counts-");
    let source = source_tree(&root);
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    let stats = clone_into(
        Backend::Copy,
        &source,
        &destination,
        &every_entry,
        false,
        &Progress::default(),
    )
    .expect("copy the tree");

    assert_eq!(stats.copied_file_count, 2);
    assert_eq!(stats.cloned_entry_count, LINKS);
    assert_eq!(stats.cloned_directory_count, 0);
}

/// On APFS, which macOS volumes use, the APFS backend clones directories as a whole and copies
/// nothing.
#[cfg(target_os = "macos")]
#[test]
fn the_apfs_backend_clones_on_this_volume() {
    let root = TempDir::new("walk-apfs-");
    let source = source_tree(&root);
    let detection = detect(&source, root.path()).expect("detect the backend");
    assert_eq!(detection.backend, Backend::Apfs);
    assert_eq!(detection.source, detection.destination);
    assert_eq!(detection.destination.filesystem, "apfs");

    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let stats = clone_into(
        Backend::Apfs,
        &source,
        &destination,
        &every_entry,
        false,
        &Progress::default(),
    )
    .expect("clone the tree");

    assert_eq!(stats.copied_file_count, 0);
    assert_eq!(stats.cloned_directory_count, 1, "{stats:?}");
    assert_eq!(stats.cloned_entry_count, 2, "top.txt and link: {stats:?}");
    assert_eq!(
        std::fs::read(destination.join("a/b/deep.bin")).expect("read clone"),
        vec![7u8; 100_000]
    );
}

/// Each block-cloning backend the scratch volume cannot use fails cloning as unsupported, and
/// copies every file where a copy is allowed, with its time.
#[test]
fn a_backend_the_volume_cannot_use_fails_or_copies() {
    let root = TempDir::new("walk-unusable-");
    let source = source_tree(&root);
    let usable = native(&root);

    for backend in [Backend::Apfs, Backend::Reflink, Backend::Refs] {
        if backend == usable {
            continue;
        }
        let refused = root.join(format!("refused-{backend:?}"));
        let err = clone_tree(backend, &source, &refused).expect_err("an unusable backend fails");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{backend:?}: {err}");

        let copied = root.join(format!("copied-{backend:?}"));
        std::fs::create_dir(&copied).expect("create the destination");
        let stats = clone_into(
            backend,
            &source,
            &copied,
            &every_entry,
            true,
            &Progress::default(),
        )
        .expect("copy what cannot be cloned");
        assert_eq!(stats.copied_file_count, 2, "{backend:?}");
        assert_eq!(
            modified(&source.join("a/b/deep.bin")),
            modified(&copied.join("a/b/deep.bin")),
            "{backend:?}"
        );
    }
}

/// With `LORE_TEST_CLONE_DIR` naming a directory on a volume that clones files, such as Btrfs,
/// XFS or a Dev Drive, the backend found for it clones a tree there and copies nothing. Ignored,
/// and run with `--ignored` where the variable names such a directory.
#[test]
#[ignore = "needs LORE_TEST_CLONE_DIR naming a directory on a volume that clones files"]
fn a_block_cloning_volume_named_for_the_test_clones_its_files() {
    let volume =
        std::env::var_os("LORE_TEST_CLONE_DIR").expect("LORE_TEST_CLONE_DIR names a directory");
    let root = TempDir::new_in(Path::new(&volume), "walk-volume-");
    let backend = native(&root);
    assert!(backend.shares_blocks(), "{} does not clone", root.display());
    let source = source_tree(&root);
    // Larger than a cluster, and not a whole number of them.
    std::fs::write(source.join("large.bin"), vec![3u8; 3 * 65_536 + 17]).expect("write");
    set_modified(&source.join("large.bin"), odd_time(4));

    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let stats = clone_into(
        backend,
        &source,
        &destination,
        &each_entry,
        false,
        &Progress::default(),
    )
    .expect("clone the tree");

    assert_eq!(stats.copied_file_count, 0);
    for relative in ["top.txt", "a/b/deep.bin", "large.bin"] {
        assert_eq!(
            std::fs::read(source.join(relative)).expect("read source"),
            std::fs::read(destination.join(relative)).expect("read clone"),
            "{relative}"
        );
        assert_eq!(
            modified(&source.join(relative)),
            modified(&destination.join(relative)),
            "{relative}"
        );
    }
}

/// A file rewritten after it is listed and before it is copied keeps the time of the content
/// the copy holds, not the time it had when listed.
#[test]
fn a_file_rewritten_before_its_copy_keeps_the_later_time() {
    let root = TempDir::new("walk-rewritten-");
    let source = source_tree(&root);

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir(&destination).expect("create the destination");
        std::fs::write(source.join("top.txt"), b"top").expect("restore the source");
        set_modified(&source.join("top.txt"), odd_time(1));

        clone_into_with(
            backend,
            &source,
            &destination,
            &each_entry,
            false,
            &Progress::default(),
            &|backend, kind, from, to| {
                if from.ends_with("top.txt") {
                    std::fs::write(from, b"rewritten").expect("rewrite the source");
                    set_modified(&from, odd_time(50));
                }
                clone_entry(backend, kind, from, to)
            },
        )
        .expect("clone the tree");

        assert_eq!(
            std::fs::read(destination.join("top.txt")).expect("read the copy"),
            b"rewritten"
        );
        assert_eq!(
            modified(&destination.join("top.txt")),
            odd_time(50),
            "{backend:?}"
        );
    }
}

/// An entry removed after it is listed and before it is copied is left out, and the rest of the
/// tree is cloned.
#[test]
fn an_entry_gone_before_its_copy_is_left_out() {
    let root = TempDir::new("walk-gone-");
    let source = source_tree(&root);

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir(&destination).expect("create the destination");
        std::fs::write(source.join("a/b/gone.txt"), b"gone").expect("write");

        clone_into_with(
            backend,
            &source,
            &destination,
            &each_entry,
            false,
            &Progress::default(),
            &|backend, kind, from, to| {
                if from.ends_with("gone.txt") {
                    std::fs::remove_file(from).expect("remove the source");
                }
                clone_entry(backend, kind, from, to)
            },
        )
        .expect("clone the tree");

        assert!(!destination.join("a/b/gone.txt").exists(), "{backend:?}");
        assert!(destination.join("a/b/deep.bin").is_file(), "{backend:?}");
    }
}

#[test]
fn a_stopped_clone_fails_as_interrupted() {
    let root = TempDir::new("walk-stopped-");
    let source = source_tree(&root);
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let progress = Progress::default();
    progress.stop();

    let err = clone_into(
        Backend::Copy,
        &source,
        &destination,
        &every_entry,
        false,
        &progress,
    )
    .expect_err("stopped cloning fails");

    assert_eq!(err.kind(), io::ErrorKind::Interrupted);
}

/// A FIFO in the tree is left out of the copy.
#[cfg(target_family = "unix")]
#[test]
fn a_fifo_in_the_tree_is_left_out() {
    let root = TempDir::new("walk-fifo-");
    let source = source_tree(&root);
    let fifo = std::ffi::CString::new(source.join("a/fifo").into_os_string().into_encoded_bytes())
        .expect("a path");
    // SAFETY: `fifo` is a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir(&destination).expect("create the destination");

        clone_into(
            backend,
            &source,
            &destination,
            &each_entry,
            false,
            &Progress::default(),
        )
        .expect("clone the tree");

        assert!(
            std::fs::symlink_metadata(destination.join("a/fifo")).is_err(),
            "{backend:?}"
        );
        assert!(destination.join("a/b/deep.bin").is_file(), "{backend:?}");
    }
}

/// Clones every file by copying it, except the one at `refused`, which the filesystem cannot
/// clone.
fn clone_all_but(
    refused: &'static str,
) -> impl Fn(Backend, Kind, Source<'_>, &Path) -> io::Result<()> + Sync {
    move |_backend, kind, from, to| {
        if from.ends_with(refused) {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        } else {
            clone_entry(Backend::Copy, kind, from, to)
        }
    }
}

#[test]
fn a_file_that_fails_to_clone_fails_the_clone() {
    let root = TempDir::new("walk-refused-");
    let source = source_tree(&root);
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    let err = clone_into_with(
        Backend::Reflink,
        &source,
        &destination,
        &every_entry,
        false,
        &Progress::default(),
        &clone_all_but("deep.bin"),
    )
    .expect_err("a file that cannot clone fails cloning");

    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    assert!(!destination.join("a/b/deep.bin").exists());
}

#[test]
fn a_file_that_fails_to_clone_is_copied_when_allowed() {
    let root = TempDir::new("walk-copied-");
    let source = source_tree(&root);
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    let stats = clone_into_with(
        Backend::Reflink,
        &source,
        &destination,
        &every_entry,
        true,
        &Progress::default(),
        &clone_all_but("deep.bin"),
    )
    .expect("the file is copied");

    assert_eq!(stats.copied_file_count, 1);
    assert_eq!(stats.cloned_entry_count, 1 + LINKS);
    assert_eq!(
        std::fs::read(destination.join("a/b/deep.bin")).expect("read the copy"),
        vec![7u8; 100_000]
    );
    assert_eq!(
        modified(&source.join("a/b/deep.bin")),
        modified(&destination.join("a/b/deep.bin"))
    );
}

/// A directory that cannot be cloned as a whole is filled entry by entry, and no directory below
/// it is tried as a whole again.
#[test]
fn a_directory_that_cannot_be_cloned_whole_is_tried_once() {
    let root = TempDir::new("walk-whole-once-");
    let source = source_tree(&root);
    std::fs::create_dir_all(source.join("a/b/c")).expect("create a deeper directory");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let tried = Mutex::new(Vec::new());

    clone_into_with(
        Backend::Apfs,
        &source,
        &destination,
        &every_entry,
        false,
        &Progress::default(),
        &|_backend, kind, from, to| {
            if kind == Kind::Directory {
                tried.lock().expect("lock").push(from.to_path_buf());
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            clone_entry(Backend::Copy, kind, from, to)
        },
    )
    .expect("clone the tree");

    assert_eq!(*tried.lock().expect("lock"), vec![source.join("a")]);
    assert!(destination.join("a/b/c").is_dir());
    assert!(destination.join("a/b/deep.bin").is_file());
}

/// Leaves out `a/skipped.txt`, and visits `a` entry by entry so it can.
fn skipping(_path: &Path, relative: &Path, _kind: Kind) -> Visit {
    if relative == Path::new("a/skipped.txt") {
        Visit::Skip
    } else if relative == Path::new("a") {
        Visit::EachEntry
    } else {
        Visit::Clone
    }
}

#[test]
fn clone_into_leaves_out_skipped_entries_and_fills_present_directories() {
    let root = TempDir::new("walk-into-");
    let source = source_tree(&root);
    std::fs::write(source.join("a/skipped.txt"), b"skipped").expect("write file");

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir_all(destination.join("a")).expect("create the destination");
        std::fs::write(destination.join("a/own.txt"), b"own").expect("write own file");

        clone_into(
            backend,
            &source,
            &destination,
            &skipping,
            false,
            &Progress::default(),
        )
        .expect("clone into");

        assert!(!destination.join("a/skipped.txt").exists(), "{backend:?}");
        assert_eq!(
            std::fs::read(destination.join("a/own.txt")).expect("read own file"),
            b"own"
        );
        assert_eq!(
            std::fs::read(destination.join("a/b/deep.bin")).expect("read clone"),
            vec![7u8; 100_000]
        );
        assert_eq!(
            modified(&source.join("a/b/deep.bin")),
            modified(&destination.join("a/b/deep.bin"))
        );
    }
}

/// A directory cloning makes itself, rather than one already present, is still visited entry
/// by entry when told to, so an entry inside it can be left out.
#[test]
fn a_directory_visited_entry_by_entry_leaves_out_skipped_entries_inside_it() {
    let root = TempDir::new("walk-each-");
    let source = source_tree(&root);
    std::fs::write(source.join("a/skipped.txt"), b"skipped").expect("write file");

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir(&destination).expect("create the destination");

        clone_into(
            backend,
            &source,
            &destination,
            &skipping,
            false,
            &Progress::default(),
        )
        .expect("clone into");

        assert!(!destination.join("a/skipped.txt").exists(), "{backend:?}");
        assert!(destination.join("a/b/deep.bin").is_file(), "{backend:?}");
    }
}

#[test]
fn clone_into_refuses_a_file_that_is_present() {
    let root = TempDir::new("walk-into-present-");
    let source = source_tree(&root);

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir(&destination).expect("create the destination");
        std::fs::write(destination.join("top.txt"), b"mine").expect("write own file");

        let err = clone_into(
            backend,
            &source,
            &destination,
            &every_entry,
            false,
            &Progress::default(),
        )
        .expect_err("a present file is refused");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{backend:?}");
        assert_eq!(
            std::fs::read(destination.join("top.txt")).expect("read own file"),
            b"mine"
        );
    }
}

/// Each file counts its blocks, at least one, and each directory and link one block.
#[test]
fn measure_counts_the_blocks_a_copy_takes() {
    let root = TempDir::new("walk-measure-");
    let source = source_tree(&root);
    std::fs::write(source.join("a/skipped.txt"), b"skipped").expect("write file");

    let size = measure(&source, &skipping, 4096, &Progress::default()).expect("measure");

    // top.txt one block, deep.bin 25, the directories a and a/b one each, and each link one.
    assert_eq!(
        size,
        TreeSize {
            size: (1 + 25 + 2 + LINKS) * 4096,
            file_count: 2,
            entry_count: 4 + LINKS,
        }
    );
}

/// A directory present in the destination is filled, not replaced, by every backend, where the
/// backend would otherwise clone that directory as a whole.
#[test]
fn a_present_directory_is_filled_by_every_backend() {
    let root = TempDir::new("walk-present-dir-");
    let source = source_tree(&root);

    for backend in [
        Backend::Apfs,
        Backend::Reflink,
        Backend::Refs,
        Backend::Copy,
    ] {
        let destination = root.join(format!("{backend:?}"));
        std::fs::create_dir_all(destination.join("a")).expect("create the destination");
        std::fs::write(destination.join("a/own.txt"), b"own").expect("write own file");

        clone_into(
            backend,
            &source,
            &destination,
            &every_entry,
            true,
            &Progress::default(),
        )
        .expect("clone into");

        assert_eq!(
            std::fs::read(destination.join("a/own.txt")).expect("read own file"),
            b"own",
            "{backend:?}"
        );
        assert!(destination.join("a/b/deep.bin").is_file(), "{backend:?}");
    }
}

/// A junction is copied as a junction to the same target, and a file symbolic link, where this
/// account may create one, as a link to the same target.
#[cfg(target_family = "windows")]
#[test]
fn junctions_and_links_are_copied_as_they_are() {
    let root = TempDir::new("walk-junction-");
    let source = source_tree(&root);
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(source.join("junction"))
        .arg(source.join("a"))
        .output()
        .expect("run mklink");
    assert!(made.status.success(), "{made:?}");
    let has_link = std::os::windows::fs::symlink_file("top.txt", source.join("link")).is_ok();

    for backend in backends(&root) {
        let destination = root.join(format!("{backend:?}"));
        clone_tree(backend, &source, &destination).expect("clone the tree");

        assert!(
            std::fs::symlink_metadata(destination.join("junction"))
                .expect("the junction is copied")
                .file_type()
                .is_symlink(),
            "{backend:?}"
        );
        assert_eq!(
            std::fs::read_link(destination.join("junction")).expect("read the junction"),
            std::fs::read_link(source.join("junction")).expect("read the junction"),
            "{backend:?}"
        );
        if has_link {
            assert_eq!(
                std::fs::read_link(destination.join("link")).expect("read the link"),
                PathBuf::from("top.txt"),
                "{backend:?}"
            );
        }
    }
}

/// Hands each of `directories` on to the walk's queue, or keeps it for this thread where the queue
/// has no room.
fn hand_on<J>(found: &mut Found<'_, J>, directories: impl IntoIterator<Item = J>) {
    for directory in directories {
        if let Err(directory) = found.offer(directory) {
            found.keep(directory);
        }
    }
}

/// Every directory of a wide and deep tree is listed once, on many threads, and the walk ends.
#[test]
fn a_walk_lists_every_directory_once_on_many_threads() {
    let listed = std::sync::atomic::AtomicUsize::new(0);

    lore_revision::fs::walk::run(0u32, 8, &|depth, found| {
        listed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if depth < 4 {
            hand_on(found, std::iter::repeat_n(depth + 1, 6));
        }
        Ok(())
    })
    .expect("the walk ends");

    // 1 + 6 + 36 + 216 + 1296 directories.
    assert_eq!(listed.into_inner(), 1555);
}

/// A panic while listing fails the walk, rather than leaving the other threads waiting for it.
#[test]
fn a_panic_while_listing_fails_the_walk() {
    let err = lore_revision::fs::walk::run(0u32, 4, &|depth, found| {
        if depth == 2 {
            panic!("listing failed");
        }
        hand_on(found, [depth + 1, depth + 1]);
        Ok(())
    })
    .expect_err("the panic fails the walk");

    assert_eq!(err.kind(), io::ErrorKind::Other);
}

/// A failure that stops the walk is the one the walk reports, not another thread's stop it
/// caused.
#[test]
fn a_walk_reports_the_failure_that_stopped_it() {
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let interrupted = std::sync::atomic::AtomicBool::new(false);

    let err = lore_revision::fs::walk::run(0u32, 4, &|depth, found| match depth {
        0 => {
            hand_on(found, [1, 2]);
            Ok(())
        }
        1 => {
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::yield_now();
            }
            interrupted.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(io::Error::from(io::ErrorKind::Interrupted))
        }
        _ => {
            stopped.store(true, std::sync::atomic::Ordering::Relaxed);
            while !interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::yield_now();
            }
            // Long enough for the other thread's failure to end the walk first, so the walk
            // reports this one only by the rule under test.
            std::thread::sleep(Duration::from_millis(20));
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    })
    .expect_err("the walk fails");

    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

/// A file operation on the syscall pool completes while every thread cloning a tree is busy, since
/// cloning runs only on the threads the pool lends it. The pool's budget is the pool tests'
/// subject; this one checks that cloning takes no thread the operation needs.
#[test]
fn a_file_operation_completes_while_a_clone_runs() {
    let root = TempDir::new("walk-concurrent-");
    let source = root.join("source");
    for directory in 0..64 {
        std::fs::create_dir_all(source.join(format!("d{directory}"))).expect("create");
        std::fs::write(source.join(format!("d{directory}/f")), b"f").expect("write");
    }
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let released = std::sync::atomic::AtomicBool::new(false);
    let entered = std::sync::atomic::AtomicUsize::new(0);
    let probe = root.join("probe");
    std::fs::write(&probe, b"probe").expect("write the probe");

    std::thread::scope(|scope| {
        let clone = scope.spawn(|| {
            clone_into_with(
                Backend::Copy,
                &source,
                &destination,
                &each_entry,
                false,
                &Progress::default(),
                &|backend, kind, from, to| {
                    entered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    while !released.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::yield_now();
                    }
                    clone_entry(backend, kind, from, to)
                },
            )
        });
        // Released however the check below ends, so a failing assertion fails rather than hangs.
        struct Release<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let release = Release(&released);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime");
        let stat = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                // The operation is issued once cloning is busy.
                while entered.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    tokio::task::yield_now().await;
                }
                lore_io::IoDriver::global().metadata(&probe).await
            })
            .await
        });
        drop(release);
        assert!(stat.is_ok(), "the file operation waits on cloning");
        clone.join().expect("join").expect("clone the tree");
    });
    assert!(destination.join("d63/f").is_file());
}

/// Makes a link at `link` to the directory `target`: a symbolic link on Unix, and on Windows a
/// junction, which any account may create.
fn link_directory(target: &Path, link: &Path) {
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(target, link).expect("link the directory");
    #[cfg(target_family = "windows")]
    {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("run mklink");
        assert!(made.status.success(), "{made:?}");
    }
}

/// A directory replaced after its parent listed it, by a link to a directory outside the tree or
/// by another directory, is left out rather than listed. The replacement is made as the parent's
/// second directory is listed: after the first was found, and before it is filled, since the
/// directories a listing finds are queued only once it ends.
#[test]
fn a_directory_replaced_after_it_was_listed_is_left_out() {
    for by_link in [true, false] {
        let root = TempDir::new("walk-replaced-");
        let source = root.join("source");
        for directory in ["a/b1", "a/b2"] {
            std::fs::create_dir_all(source.join(directory)).expect("create the tree");
            std::fs::write(source.join(directory).join("inside"), b"inside").expect("write");
        }
        let outside = root.join("outside");
        std::fs::create_dir(&outside).expect("create outside");
        std::fs::write(outside.join("secret"), b"secret").expect("write");
        let destination = root.join("destination");
        std::fs::create_dir(&destination).expect("create the destination");
        let first = Mutex::new(None::<PathBuf>);

        clone_into_with(
            Backend::Copy,
            &source,
            &destination,
            &|path, relative, kind| {
                let parent = relative.parent().unwrap_or(Path::new(""));
                if kind == Kind::Directory && parent == Path::new("a") {
                    let mut first = first.lock().expect("lock");
                    match first.as_ref() {
                        None => *first = Some(relative.to_path_buf()),
                        Some(found) => {
                            let path = source.join(found);
                            std::fs::rename(&path, root.join("moved")).expect("move it away");
                            if by_link {
                                link_directory(&outside, &path);
                            } else {
                                std::fs::create_dir(&path).expect("create another");
                                std::fs::write(path.join("secret"), b"secret").expect("write");
                            }
                        }
                    }
                }
                each_entry(path, relative, kind)
            },
            false,
            &Progress::default(),
            &clone_entry,
        )
        .expect("clone the tree");

        let first = first
            .into_inner()
            .expect("lock")
            .expect("a directory was found");
        assert!(
            std::fs::symlink_metadata(destination.join(&first)).is_err(),
            "the directory {} replaced {} is left out",
            first.display(),
            if by_link {
                "by a link"
            } else {
                "by another directory"
            }
        );
    }
}

/// A root that is not there fails cloning, a measurement and a search as not found, rather than
/// counting as an empty tree.
#[test]
fn a_root_that_is_not_there_is_not_found() {
    let root = TempDir::new("walk-no-root-");
    let missing = root.join("missing");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    let cloned = clone_into(
        Backend::Copy,
        &missing,
        &destination,
        &each_entry,
        false,
        &Progress::default(),
    );
    let measured = measure(&missing, &each_entry, 4096, &Progress::default());
    let found =
        lore_revision::fs::walk::find_directories(&missing, &|_| true, &Progress::default());

    assert_eq!(
        cloned.map(drop).map_err(|err| err.kind()),
        Err(io::ErrorKind::NotFound)
    );
    assert_eq!(
        measured.map(drop).map_err(|err| err.kind()),
        Err(io::ErrorKind::NotFound)
    );
    assert_eq!(
        found.map(drop).map_err(|err| err.kind()),
        Err(io::ErrorKind::NotFound)
    );
}

/// A search asked to stop fails as interrupted.
#[test]
fn a_stopped_search_fails_as_interrupted() {
    let root = TempDir::new("lore-io-find-stopped-");
    std::fs::create_dir_all(root.join("tree/a")).expect("create the tree");
    let progress = Progress::default();
    progress.stop();

    let found =
        lore_revision::fs::walk::find_directories(&root.join("tree"), &|_| false, &progress);

    assert_eq!(
        found.map(drop).map_err(|err| err.kind()),
        Err(io::ErrorKind::Interrupted)
    );
}

/// The directories a search accepts are found at any depth, nothing below one it accepts is
/// looked at, and a link to an accepted directory is not followed.
#[cfg(target_family = "unix")]
#[test]
fn find_directories_stops_at_a_match_and_follows_no_link() {
    let root = TempDir::new("lore-io-find-directories-");
    let tree = root.join("tree");
    for directory in ["a/b/match/inner/match", "c/match", "d"] {
        std::fs::create_dir_all(tree.join(directory)).expect("create a directory");
    }
    std::fs::write(tree.join("d/match"), "a file").expect("write a file named alike");
    std::fs::create_dir_all(root.join("outside/match")).expect("create a directory outside");
    std::os::unix::fs::symlink(root.join("outside"), tree.join("link")).expect("link outside");

    let mut found = lore_revision::fs::walk::find_directories(
        &tree,
        &|path| path.ends_with("match"),
        &Progress::default(),
    )
    .expect("search the tree");
    found.sort();

    assert_eq!(found, vec![tree.join("a/b/match"), tree.join("c/match")]);
}

/// A directory replaced after its parent listed it, by a link to a directory outside the tree or
/// by another directory, is not searched. The replacement is made as the parent's second
/// directory is offered to the search: after the first was found, and before it is listed.
#[test]
fn a_search_leaves_out_a_directory_replaced_after_it_was_listed() {
    for by_link in [true, false] {
        let root = TempDir::new("lore-io-find-replaced-");
        let tree = root.join("tree");
        for directory in ["a/b1", "a/b2"] {
            std::fs::create_dir_all(tree.join(directory)).expect("create the tree");
        }
        std::fs::create_dir_all(root.join("outside/match")).expect("create outside");
        let first = Mutex::new(None::<PathBuf>);

        let found = lore_revision::fs::walk::find_directories(
            &tree,
            &|path| {
                if path.parent() == Some(tree.join("a").as_path()) {
                    let mut first = first.lock().expect("lock");
                    match first.as_ref() {
                        None => *first = Some(path.to_path_buf()),
                        Some(found) => {
                            std::fs::rename(found, root.join("moved")).expect("move it away");
                            if by_link {
                                link_directory(&root.join("outside"), found);
                            } else {
                                std::fs::create_dir_all(found.join("match"))
                                    .expect("create another");
                            }
                        }
                    }
                }
                path.ends_with("match")
            },
            &Progress::default(),
        )
        .expect("search the tree");

        let first = first
            .into_inner()
            .expect("lock")
            .expect("a directory was found");
        assert!(
            found.iter().all(|path| !path.starts_with(&first)),
            "{found:?} holds nothing below {}, replaced {}",
            first.display(),
            if by_link {
                "by a link"
            } else {
                "by another directory"
            }
        );
    }
}

/// A directory whose parent is replaced by a file after the directory was found is left out
/// rather than failing cloning, as an entry gone by the time the walk reaches it is.
#[cfg(target_family = "unix")]
#[test]
fn a_directory_whose_parent_became_a_file_is_left_out() {
    let root = TempDir::new("walk-parent-file-");
    let source = root.join("source");
    for directory in ["a/b/d1", "a/b/d2", "a/c"] {
        std::fs::create_dir_all(source.join(directory)).expect("create the tree");
    }
    std::fs::write(source.join("a/c/kept"), b"kept").expect("write");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let seen = std::sync::atomic::AtomicUsize::new(0);

    clone_into_with(
        Backend::Copy,
        &source,
        &destination,
        &|path, relative, kind| {
            if kind == Kind::Directory
                && relative.parent() == Some(Path::new("a/b"))
                && seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 1
            {
                std::fs::remove_dir_all(source.join("a/b")).expect("remove the parent");
                std::fs::write(source.join("a/b"), b"a file").expect("put a file there");
            }
            each_entry(path, relative, kind)
        },
        false,
        &Progress::default(),
        &clone_entry,
    )
    .expect("cloning leaves the directories out");

    assert!(destination.join("a/c/kept").is_file());
}

/// A file replaced by a junction after it was listed is copied as the junction, not followed.
#[cfg(target_family = "windows")]
#[test]
fn a_file_replaced_by_a_junction_is_copied_as_one() {
    let root = TempDir::new("walk-file-junction-");
    let source = root.join("source");
    std::fs::create_dir_all(&source).expect("create the tree");
    std::fs::write(source.join("f"), b"f").expect("write");
    let outside = root.join("outside");
    std::fs::create_dir(&outside).expect("create outside");
    std::fs::write(outside.join("secret"), b"secret").expect("write");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    clone_into_with(
        Backend::Copy,
        &source,
        &destination,
        &each_entry,
        false,
        &Progress::default(),
        &|backend, kind, from, to| {
            if from.ends_with("f") && from.is_file() {
                std::fs::remove_file(from).expect("remove the file");
                link_directory(&outside, &from);
            }
            clone_entry(backend, kind, from, to)
        },
    )
    .expect("clone the tree");

    assert!(
        std::fs::symlink_metadata(destination.join("f"))
            .expect("the junction is copied")
            .file_type()
            .is_symlink()
    );
}

/// A link replaced by a file after it was listed is copied as the file.
#[cfg(target_family = "unix")]
#[test]
fn a_link_replaced_by_a_file_is_copied_as_the_file() {
    let root = TempDir::new("walk-link-file-");
    let source = root.join("source");
    std::fs::create_dir_all(&source).expect("create the tree");
    std::os::unix::fs::symlink("elsewhere", source.join("link")).expect("link");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    clone_into_with(
        Backend::Copy,
        &source,
        &destination,
        &each_entry,
        false,
        &Progress::default(),
        &|backend, kind, from, to| {
            if kind == Kind::Symlink {
                std::fs::remove_file(from).expect("remove the link");
                std::fs::write(from, b"a file").expect("put a file there");
            }
            clone_entry(backend, kind, from, to)
        },
    )
    .expect("clone the tree");

    assert_eq!(
        std::fs::read(destination.join("link")).expect("read the copy"),
        b"a file"
    );
}

/// A file replaced by a socket after it was listed is left out, as a gone entry is.
#[cfg(target_family = "unix")]
#[test]
fn a_file_replaced_by_a_socket_is_left_out() {
    let root = TempDir::new("walk-socket-");
    let source = root.join("source");
    std::fs::create_dir_all(&source).expect("create the tree");
    std::fs::write(source.join("f"), b"f").expect("write");
    std::fs::write(source.join("kept"), b"kept").expect("write");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let listener = Mutex::new(None);

    clone_into_with(
        Backend::Copy,
        &source,
        &destination,
        &each_entry,
        true,
        &Progress::default(),
        &|backend, kind, from, to| {
            if from.ends_with("f") {
                std::fs::remove_file(from).expect("remove the file");
                *listener.lock().expect("lock") =
                    Some(std::os::unix::net::UnixListener::bind(from).expect("put a socket there"));
            }
            clone_entry(backend, kind, from, to)
        },
    )
    .expect("cloning leaves the socket out");

    assert!(std::fs::symlink_metadata(destination.join("f")).is_err());
    assert_eq!(
        std::fs::read(destination.join("kept")).expect("read"),
        b"kept"
    );
}

/// A link swapped in, while a directory is being filled, for that directory or for a directory
/// above it, does not lead the walk out of the tree: every entry is read from the directory being
/// filled, and nothing from where the link leads. Unix reads each entry through the directory,
/// held open, so the swap goes ahead and changes nothing; Windows refuses the swap, as the walk
/// holds the directory open without sharing delete access, and refuses to rename a directory
/// with an entry open below it.
#[test]
fn a_link_swapped_in_while_a_directory_is_filled_is_not_followed() {
    for swapped in ["top/a", "top"] {
        let root = TempDir::new("walk-swapped-mid-fill-");
        let source = root.join("source");
        let outside = root.join("outside");
        for name in 0..8 {
            for (tree, content) in [(&source, "inside"), (&outside, "secret")] {
                let directory = tree.join("top").join("a");
                std::fs::create_dir_all(&directory).expect("create the tree");
                std::fs::write(directory.join(format!("f{name}")), content).expect("write");
            }
        }
        let destination = root.join("destination");
        std::fs::create_dir(&destination).expect("create the destination");
        let swap = std::sync::Once::new();
        let refused = Mutex::new(None);
        let target = if swapped == "top" {
            outside.join("top")
        } else {
            outside.join("top").join("a")
        };

        clone_into_with(
            Backend::Copy,
            &source,
            &destination,
            &each_entry,
            false,
            &Progress::default(),
            &|backend, kind, from, to| {
                if from.parent() == Some(source.join("top").join("a").as_path()) {
                    swap.call_once(|| {
                        let path = swapped
                            .split('/')
                            .fold(source.clone(), |path, part| path.join(part));
                        let moved = std::fs::rename(&path, root.join("moved"));
                        *refused.lock().expect("lock") = Some(moved.is_err());
                        if moved.is_ok() {
                            link_directory(&target, &path);
                        }
                    });
                }
                clone_entry(backend, kind, from, to)
            },
        )
        .expect("clone the tree");

        let copied: Vec<String> = std::fs::read_dir(destination.join("top").join("a"))
            .expect("list the copy")
            .map(|entry| {
                std::fs::read_to_string(entry.expect("entry").path()).expect("read the copy")
            })
            .collect();
        assert_eq!(
            *refused.lock().expect("lock"),
            Some(cfg!(target_family = "windows")),
            "{swapped}: the swap is refused on Windows alone"
        );
        assert_eq!(
            copied,
            vec!["inside"; 8],
            "{swapped}: every entry is read from the directory being filled"
        );
    }
}

/// A walk's root reached through a link is walked as the directory the link leads to: cloning, a
/// measurement and a search all read the tree there.
#[test]
fn a_root_reached_through_a_link_is_walked() {
    let root = TempDir::new("walk-linked-root-");
    let tree = root.join("tree");
    std::fs::create_dir_all(tree.join("a/match")).expect("create the tree");
    std::fs::write(tree.join("a/file"), b"file").expect("write");
    let linked = root.join("linked");
    link_directory(&tree, &linked);
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    clone_into(
        Backend::Copy,
        &linked,
        &destination,
        &each_entry,
        false,
        &Progress::default(),
    )
    .expect("clone through the link");
    let measured = measure(&linked, &each_entry, 4096, &Progress::default())
        .expect("measure through the link");
    let found = lore_revision::fs::walk::find_directories(
        &linked,
        &|path| path.ends_with("match"),
        &Progress::default(),
    )
    .expect("search through the link");

    assert_eq!(
        std::fs::read(destination.join("a/file")).expect("read"),
        b"file"
    );
    assert_eq!(measured.file_count, 1);
    assert_eq!(found, vec![linked.join("a/match")]);
}

/// Directories created with their permissions keep them where the file mode creation mask would
/// clear bits, and keep the set-group-ID and sticky bits, which `mkdir` does not give, as far as
/// the system let the source keep them: macOS clears a set-group-ID bit its owner cannot set.
#[cfg(target_family = "unix")]
#[test]
fn directories_keep_permissions_the_creation_mask_clears() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new("walk-umask-");
    let source = root.join("source");
    let modes = [("open", 0o777), ("setgid", 0o2775), ("sticky", 0o1777)];
    for (name, bits) in modes {
        std::fs::create_dir_all(source.join(name).join("inner")).expect("create the tree");
        std::fs::set_permissions(source.join(name), std::fs::Permissions::from_mode(bits))
            .expect("set the mode");
    }
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    clone_into(
        Backend::Copy,
        &source,
        &destination,
        &each_entry,
        false,
        &Progress::default(),
    )
    .expect("clone the tree");

    for (name, _) in modes {
        assert_eq!(
            mode(&destination.join(name)),
            mode(&source.join(name)),
            "{name}"
        );
        assert_eq!(
            mode(&destination.join(name).join("inner")),
            mode(&source.join(name).join("inner")),
            "{name}: a directory created in it keeps its own permissions"
        );
    }
}

/// A measurement stopped before it lists a directory fails as interrupted.
#[test]
fn a_stopped_measurement_fails_as_interrupted() {
    let root = TempDir::new("walk-measure-stopped-");
    let source = source_tree(&root);
    let progress = Progress::default();
    progress.stop();

    let measured = measure(&source, &each_entry, 4096, &progress);

    assert_eq!(
        measured.map_err(|err| err.kind()),
        Err(io::ErrorKind::Interrupted)
    );
}

/// On APFS a directory put in place of a file after its listing is never cloned whole: the file
/// is opened before it is cloned, and the directory found there fails cloning.
#[cfg(target_os = "macos")]
#[test]
fn apfs_clones_no_directory_put_in_place_of_a_listed_file() {
    let root = TempDir::new("walk-apfs-swapped-");
    let source = root.join("source");
    std::fs::create_dir_all(&source).expect("create the tree");
    std::fs::write(source.join("f"), b"f").expect("write");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");

    let cloned = clone_into_with(
        Backend::Apfs,
        &source,
        &destination,
        &each_entry,
        false,
        &Progress::default(),
        &|backend, kind, from, to| {
            if from.ends_with("f") {
                std::fs::remove_file(from).expect("remove the file");
                std::fs::create_dir_all(from.join(".lore")).expect("put a repository there");
            }
            clone_entry(backend, kind, from, to)
        },
    );

    assert_eq!(
        cloned.map_err(|err| err.kind()),
        Err(io::ErrorKind::IsADirectory)
    );
    assert!(!destination.join("f").exists(), "nothing is cloned there");
}

/// Waits until `count` threads have called it at once, at most ten seconds: true where they did.
fn meet(
    seen: &(
        std::sync::Mutex<std::collections::HashSet<std::thread::ThreadId>>,
        std::sync::Condvar,
    ),
    count: usize,
) -> bool {
    let (threads, met) = seen;
    let mut threads = threads.lock().expect("lock");
    threads.insert(std::thread::current().id());
    met.notify_all();
    let (threads, _) = met
        .wait_timeout_while(threads, Duration::from_secs(10), |threads| {
            threads.len() < count
        })
        .expect("lock");
    threads.len() >= count
}

/// A walk runs on threads its pool lends, beside the calling thread.
#[test]
fn a_walk_runs_on_threads_its_pool_lends() {
    let pool = lore_io::pool::SyscallPool::new(4);
    let seen = Default::default();

    lore_revision::fs::walk::run_lent_on(&pool, 0usize, &Progress::default(), &|item, found| {
        if item == 0 {
            hand_on(found, 1..=4);
        } else {
            assert!(meet(&seen, 2), "a second thread lists beside the first");
        }
        Ok(())
    })
    .expect("walk");
}

/// A walk that starts while its pool lends nothing takes the threads the pool frees later.
#[test]
fn a_walk_takes_threads_its_pool_frees_after_it_starts() {
    let pool = lore_io::pool::SyscallPool::new(4);
    let held = std::sync::Mutex::new(Some(pool.lend(usize::MAX)));
    let started = std::sync::atomic::AtomicBool::new(false);
    let seen = Default::default();

    lore_revision::fs::walk::run_lent_on(&pool, 0usize, &Progress::default(), &|item, found| {
        match item {
            0 => hand_on(found, 1..=8),
            // Listed first, by the calling thread alone: frees the pool's threads, and lists for
            // longer than the walk waits between asks for another thread.
            8 => {
                held.lock().expect("lock").take();
                started.store(true, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(15));
            }
            _ => assert!(meet(&seen, 2), "a thread lent later lists beside the first"),
        }
        Ok(())
    })
    .expect("walk");
    assert!(started.load(std::sync::atomic::Ordering::Relaxed));
}

/// A directory another process holds open with delete access, as one renaming or removing it
/// does, is listed once the handle is closed, rather than failing cloning.
#[cfg(target_family = "windows")]
#[test]
fn a_directory_held_with_delete_access_is_listed_once_released() {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

    let root = TempDir::new("walk-held-delete-");
    let source = root.join("source");
    std::fs::create_dir_all(source.join("held")).expect("create the tree");
    std::fs::write(source.join("held/file"), b"file").expect("write");
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create the destination");
    let held = std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(source.join("held"))
        .expect("hold the directory with delete access");

    std::thread::scope(|scope| {
        scope.spawn(move || {
            // Released 15 ms in, which a walk that reaches the directory first waits out.
            std::thread::sleep(Duration::from_millis(15));
            drop(held);
        });
        clone_into(
            Backend::Copy,
            &source,
            &destination,
            &each_entry,
            false,
            &Progress::default(),
        )
        .expect("clone the tree");
    });

    assert_eq!(
        std::fs::read(destination.join("held/file")).expect("read"),
        b"file"
    );
}

/// Makes `width` directories in `parent`, each holding a file and `width` directories below it
/// to `depth` levels.
fn make_tree(parent: &Path, width: usize, depth: usize) {
    std::fs::write(parent.join("f"), parent.as_os_str().as_encoded_bytes()).expect("write");
    if depth == 0 {
        return;
    }
    for directory in 0..width {
        let below = parent.join(format!("d{directory}"));
        std::fs::create_dir(&below).expect("create");
        make_tree(&below, width, depth - 1);
    }
}

/// The entries of the tree at `root`, by path relative to it, with each file's contents.
fn tree_of(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("list") {
            let path = entry.expect("entry").path();
            let relative = path
                .strip_prefix(root)
                .expect("below the root")
                .to_path_buf();
            if path.is_dir() {
                entries.insert(relative, None);
                pending.push(path);
            } else {
                entries.insert(relative, Some(std::fs::read(&path).expect("read")));
            }
        }
    }
    entries
}

/// Copies the tree at `source` into a new directory beside it with `progress`, and checks the
/// copy holds every entry.
fn copy_whole(root: &TempDir, source: &Path, progress: &Progress) {
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create");
    clone_into(
        Backend::Copy,
        source,
        &destination,
        &every_entry,
        false,
        progress,
    )
    .expect("clone");
    assert_eq!(tree_of(&destination), tree_of(source));
}

/// The queue never holds more directories than its cap over a tree wider than the cap, and the
/// directories it has no room for are walked depth-first by the thread that found them.
#[test]
fn a_clone_queues_no_more_directories_than_its_cap() {
    let root = TempDir::new("walk-queue-cap-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    make_tree(&source, 12, 2);
    let progress = Progress::bounded(2, 64, 4);

    copy_whole(&root, &source, &progress);

    let held = progress.counts();
    assert!(held.queued <= 2, "{held:?}");
    assert!(held.kept >= 2, "the walk descends depth-first: {held:?}");
}

/// A step of the walk hands on at most a batch of the directories it finds, and the rest of its
/// directory, over a directory holding more directories than a batch.
#[test]
fn a_clone_thread_holds_at_most_one_batch() {
    let root = TempDir::new("walk-one-batch-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    make_tree(&source, 40, 1);
    let progress = Progress::bounded(4096, 64, 8);

    copy_whole(&root, &source, &progress);

    let held = progress.counts();
    assert!(held.handed <= 9, "{held:?}");
    assert!(held.continued >= 4, "{held:?}");
}

/// Over a tree deeper than the queue's cap, no thread keeps more directories than the tree has
/// levels.
#[test]
fn a_clone_descends_no_deeper_than_the_tree() {
    let root = TempDir::new("walk-depth-");
    let source = root.join("source");
    let mut level = source.clone();
    for _ in 0..10 {
        std::fs::create_dir_all(level.join("a")).expect("create");
        std::fs::create_dir_all(level.join("b")).expect("create");
        std::fs::write(level.join("f"), b"f").expect("write");
        level.push("c");
    }
    std::fs::create_dir(&level).expect("create");
    let progress = Progress::bounded(1, 64, 2);

    copy_whole(&root, &source, &progress);

    let held = progress.counts();
    assert!(held.kept >= 2, "the walk descends depth-first: {held:?}");
    assert!(held.kept <= 11, "{held:?}");
}

/// A directory wider than a batch is listed a batch at a time, the rest of it put back as a
/// continuation, and copied whole.
#[test]
fn a_directory_wider_than_a_batch_is_copied_whole() {
    let root = TempDir::new("walk-wide-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    for file in 0..2500 {
        std::fs::write(source.join(format!("f{file}")), file.to_string()).expect("write");
    }
    for directory in 0..20 {
        std::fs::create_dir(source.join(format!("d{directory}"))).expect("create");
    }
    let progress = Progress::default();

    copy_whole(&root, &source, &progress);

    assert!(progress.counts().continued >= 2, "{:?}", progress.counts());
}

/// The queue never holds more continuations than their cap over a tree of directories wider than
/// a batch, and a thread lists the rest of a directory itself once the cap is reached.
#[test]
fn a_clone_queues_no_more_continuations_than_their_cap() {
    let root = TempDir::new("walk-continuation-cap-");
    let source = root.join("source");
    for directory in 0..16 {
        let at = source.join(format!("d{directory}"));
        std::fs::create_dir_all(&at).expect("create");
        for file in 0..40 {
            std::fs::write(at.join(format!("f{file}")), b"f").expect("write");
        }
    }
    let progress = Progress::bounded(4096, 1, 4);

    copy_whole(&root, &source, &progress);

    let held = progress.counts();
    assert!(held.continuations <= 1, "{held:?}");
    assert!(held.continued >= 1, "{held:?}");
}

/// An extended attribute name the tests' user can set: in the `user` namespace on Linux.
#[cfg(target_os = "linux")]
const ATTRIBUTE: &str = "user.lore";
/// An extended attribute name the tests' user can set.
#[cfg(target_os = "macos")]
const ATTRIBUTE: &str = "com.lore.test";

/// Sets the extended attribute `name` of `path` to `value`, a link at `path` not followed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_attribute(path: &Path, name: &str, value: &[u8]) {
    try_set_attribute(path, name, value)
        .unwrap_or_else(|err| panic!("set {name:?} on {}: {err}", path.display()));
}

/// Sets the extended attribute `name` of `path` to `value`, a link at `path` not followed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn try_set_attribute(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
    let name = std::ffi::CString::new(name).expect("a name");
    // SAFETY: the path and name are NUL-terminated, and the value is readable for its length.
    #[cfg(target_os = "linux")]
    let set = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    // SAFETY: the path and name are NUL-terminated, and the value is readable for its length.
    #[cfg(target_os = "macos")]
    let set = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if set == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The extended attributes of `path`, by name, a link at `path` not followed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn attributes(path: &Path) -> std::collections::BTreeMap<Vec<u8>, Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
    let mut names = vec![0u8; 65536];
    // SAFETY: the path is NUL-terminated, and `names` is writable for its length.
    #[cfg(target_os = "linux")]
    let listed = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    // SAFETY: the path is NUL-terminated, and `names` is writable for its length.
    #[cfg(target_os = "macos")]
    let listed = unsafe {
        libc::listxattr(
            path.as_ptr(),
            names.as_mut_ptr().cast(),
            names.len(),
            libc::XATTR_NOFOLLOW,
        )
    };
    let listed = usize::try_from(listed).expect("list the attributes");
    names
        .get(..listed)
        .expect("listed")
        .split(|&byte| byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let c_name = std::ffi::CString::new(name).expect("a name");
            let mut value = vec![0u8; 65536];
            // SAFETY: the path and name are NUL-terminated, and `value` is writable for its
            // length.
            #[cfg(target_os = "linux")]
            let read = unsafe {
                libc::lgetxattr(
                    path.as_ptr(),
                    c_name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            };
            // SAFETY: the path and name are NUL-terminated, and `value` is writable for its
            // length.
            #[cfg(target_os = "macos")]
            let read = unsafe {
                libc::getxattr(
                    path.as_ptr(),
                    c_name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            };
            value.truncate(usize::try_from(read).expect("read the attribute"));
            (name.to_vec(), value)
        })
        .collect()
}

/// A POSIX ACL as Linux holds it in `system.posix_acl_*`: the owner, `user` with `user_bits`,
/// the group, the mask and others, each with its permission bits.
#[cfg(target_os = "linux")]
fn posix_acl(user: u32, user_bits: u16) -> Vec<u8> {
    const UNDEFINED: u32 = u32::MAX;
    let entries: [(u16, u16, u32); 5] = [
        (0x01, 7, UNDEFINED),
        (0x02, user_bits, user),
        (0x04, 5, UNDEFINED),
        (0x10, 7, UNDEFINED),
        (0x20, 0, UNDEFINED),
    ];
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, bits, id) in entries {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&bits.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    acl
}

/// The access and modification times of `path`, to the nanosecond, a link not followed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn times_of(path: &Path) -> (i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path).expect("stat");
    (
        metadata.atime(),
        metadata.atime_nsec(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}

/// Sets the access and modification times of `path`, a link not followed, to odd nanoseconds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_odd_times(path: &Path, seed: i64) {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
    let times = [
        libc::timespec {
            tv_sec: 1_600_000_000 + seed,
            tv_nsec: 123_456_789,
        },
        libc::timespec {
            tv_sec: 1_700_000_000 + seed,
            tv_nsec: 987_654_321,
        },
    ];
    // SAFETY: the path is NUL-terminated, and `times` holds two entries.
    let set = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(set, 0, "{}", io::Error::last_os_error());
}

/// Copies the tree at `source` faithfully into a new directory beside it, on the threads the
/// syscall pool lends.
#[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
fn copy_faithfully_beside(
    root: &TempDir,
    source: &Path,
    visit: &lore_revision::fs::walk::VisitFn<'_>,
    progress: &Progress,
) -> io::Result<PathBuf> {
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create");
    lore_revision::fs::walk::copy_faithfully(source, &destination, visit, progress)?;
    Ok(destination)
}

/// A faithful copy keeps a file's holes: on APFS, which fills a hole of under 16 MiB beside data
/// when it writes the data out, the 31 MiB and 32 MiB ones.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_faithful_copy_keeps_holes() {
    use std::os::unix::fs::FileExt;
    use std::os::unix::fs::MetadataExt;

    let root = TempDir::new("lore-io-faithful-holes-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    let file = std::fs::File::create(source.join("sparse")).expect("create");
    file.set_len(64 << 20).expect("size");
    file.write_all_at(&[7; 4096], 1 << 20).expect("write");
    file.write_all_at(&[9; 4096], 32 << 20).expect("write");
    file.sync_all().expect("sync");

    let destination =
        copy_faithfully_beside(&root, &source, &every_entry, &Progress::default()).expect("copy");

    let copy = destination.join("sparse");
    assert_eq!(
        std::fs::read(&copy).expect("read"),
        std::fs::read(source.join("sparse")).expect("read")
    );
    let allocated = std::fs::metadata(&copy).expect("stat").blocks() * 512;
    assert!(
        allocated < 4 << 20,
        "{allocated} bytes allocated for 8 KiB of data"
    );
}

/// The lines of `ls -le` that list the extended ACL of `path`, a link not followed.
#[cfg(target_os = "macos")]
fn acl_of(path: &Path) -> Vec<String> {
    let listed = std::process::Command::new("ls")
        .arg("-led")
        .arg(path)
        .output()
        .expect("ls");
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// Adds the extended ACL entry `entry` to `path`, a link not followed.
#[cfg(target_os = "macos")]
fn add_acl(path: &Path, entry: &str) {
    let status = std::process::Command::new("chmod")
        .args(["-h", "+a", entry])
        .arg(path)
        .status()
        .expect("chmod");
    assert!(status.success(), "chmod +a {entry}");
}

/// Gives `file` a POSIX ACL that lets user 4321 read and write, and `directory` a default ACL
/// that lets that user read what is created in it.
#[cfg(target_os = "linux")]
fn set_acls(file: &Path, directory: &Path, _link: &Path) {
    set_attribute(file, "system.posix_acl_access", &posix_acl(4321, 6));
    set_attribute(directory, "system.posix_acl_default", &posix_acl(4321, 4));
}

/// Gives `file` and `link` an extended ACL that keeps everyone from deleting them, and
/// `directory` one that lets everyone read it and what is created in it.
#[cfg(target_os = "macos")]
fn set_acls(file: &Path, directory: &Path, link: &Path) {
    add_acl(file, "everyone deny delete");
    add_acl(
        directory,
        "everyone allow read,file_inherit,directory_inherit",
    );
    add_acl(link, "everyone deny delete");
}

/// Whether `path` has a POSIX ACL.
#[cfg(target_os = "linux")]
fn has_acl(path: &Path) -> bool {
    attributes(path).contains_key(b"system.posix_acl_access".as_slice())
}

/// Whether `path` has an extended ACL.
#[cfg(target_os = "macos")]
fn has_acl(path: &Path) -> bool {
    !acl_of(path).is_empty()
}

/// A faithful copy keeps the permission bits, extended attributes, ACLs, owner and times to the
/// nanosecond of files, directories and links, and gives an entry no ACL from its directory. On
/// Linux it leaves the access times of files and directories as they are.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_faithful_copy_keeps_metadata() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new("lore-io-faithful-metadata-");
    let source = root.join("source");
    let directory = source.join("directory");
    std::fs::create_dir_all(&directory).expect("create");
    std::fs::write(directory.join("plain"), b"plain").expect("write");
    let file = source.join("file");
    std::fs::write(&file, b"data").expect("write");
    std::os::unix::fs::symlink("file", source.join("link")).expect("link");
    // SAFETY: `geteuid` has no preconditions.
    let (uid, gid) = if unsafe { libc::geteuid() } == 0 {
        (1234, 5678)
    } else {
        let metadata = std::fs::metadata(&file).expect("stat");
        (metadata.uid(), metadata.gid())
    };
    for path in [&file, &directory.join("plain"), &directory] {
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).expect("chown");
    }
    std::os::unix::fs::lchown(source.join("link"), Some(uid), Some(gid)).expect("chown");
    set_attribute(&file, ATTRIBUTE, b"file value");
    set_attribute(&directory, ATTRIBUTE, b"directory value");
    set_acls(&file, &directory, &source.join("link"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o4751)).expect("chmod");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o750)).expect("chmod");
    #[cfg(target_os = "macos")]
    assert!(
        std::process::Command::new("chmod")
            .args(["-h", "700"])
            .arg(source.join("link"))
            .status()
            .expect("chmod")
            .success(),
        "set the link's own permission bits"
    );
    for (seed, path) in [
        &file,
        &source.join("link"),
        &directory.join("plain"),
        &directory,
    ]
    .into_iter()
    .enumerate()
    {
        set_odd_times(path, i64::try_from(seed).expect("small"));
    }
    let entries = ["file", "link", "directory", "directory/plain"];
    let before = entries.map(|entry| times_of(&source.join(entry)));

    let destination =
        copy_faithfully_beside(&root, &source, &every_entry, &Progress::default()).expect("copy");

    for (entry, before) in entries.iter().zip(before) {
        #[cfg(target_os = "linux")]
        if *entry != "link" {
            let left = times_of(&source.join(entry));
            assert_eq!(
                left, before,
                "{entry}: the source's access time is left as it is"
            );
        }
        assert_eq!(times_of(&destination.join(entry)), before, "{entry}");
    }
    for entry in entries {
        let (from, to) = (source.join(entry), destination.join(entry));
        let (from_metadata, to_metadata) = (
            std::fs::symlink_metadata(&from).expect("stat"),
            std::fs::symlink_metadata(&to).expect("stat"),
        );
        assert_eq!(to_metadata.mode(), from_metadata.mode(), "{entry}");
        assert_eq!(
            (to_metadata.uid(), to_metadata.gid()),
            (uid, gid),
            "{entry}"
        );
        assert_eq!(attributes(&to), attributes(&from), "{entry}");
        #[cfg(target_os = "macos")]
        assert_eq!(acl_of(&to), acl_of(&from), "{entry}");
    }
    assert_eq!(
        std::fs::read_link(destination.join("link")).expect("read"),
        Path::new("file")
    );
    assert!(has_acl(&destination.join("file")));
    assert!(
        !has_acl(&destination.join("directory/plain")),
        "an entry takes no ACL from its directory"
    );
}

/// Gives `directory` an ACL that what is created in it inherits: a default ACL on Linux, and an
/// inheritable entry of its extended ACL on macOS.
#[cfg(target_os = "linux")]
fn set_inherited_acl(directory: &Path) {
    set_attribute(directory, "system.posix_acl_default", &posix_acl(4321, 4));
}

/// Gives `directory` an ACL that what is created in it inherits: a default ACL on Linux, and an
/// inheritable entry of its extended ACL on macOS.
#[cfg(target_os = "macos")]
fn set_inherited_acl(directory: &Path) {
    add_acl(
        directory,
        "everyone allow read,file_inherit,directory_inherit",
    );
}

/// A faithful copy into a directory that inherited an ACL from its parent gives no entry that
/// ACL, and gives the directory the source's metadata once filled, as it does each directory.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_faithful_copy_takes_no_acl_from_the_destinations_parent() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new("lore-io-faithful-inherited-");
    let source = root.join("source");
    std::fs::create_dir_all(source.join("directory")).expect("create");
    std::fs::write(source.join("file"), b"f").expect("write");
    std::fs::write(source.join("directory/file"), b"f").expect("write");
    set_attribute(&source, ATTRIBUTE, b"root value");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o750)).expect("chmod");
    set_odd_times(&source, 0);
    let before = times_of(&source);
    let parent = root.join("parent");
    std::fs::create_dir(&parent).expect("create");
    set_inherited_acl(&parent);
    let destination = parent.join("destination");
    std::fs::create_dir(&destination).expect("create");
    assert!(has_acl(&destination), "the destination inherits an ACL");

    lore_revision::fs::walk::copy_faithfully(
        &source,
        &destination,
        &every_entry,
        &Progress::default(),
    )
    .expect("copy");

    for entry in ["", "file", "directory", "directory/file"] {
        let (from, to) = (source.join(entry), destination.join(entry));
        assert_eq!(attributes(&to), attributes(&from), "{entry}");
        #[cfg(target_os = "macos")]
        assert_eq!(acl_of(&to), acl_of(&from), "{entry}");
        assert_eq!(
            std::fs::metadata(&to).expect("stat").mode(),
            std::fs::metadata(&from).expect("stat").mode(),
            "{entry}"
        );
    }
    assert_eq!(times_of(&destination), before);
}

/// A faithful copy copies a resource fork larger than its buffer a buffer's length at a time, and
/// holds no more of it at once.
#[cfg(target_os = "macos")]
#[test]
fn a_faithful_copy_copies_a_resource_fork_by_chunks() {
    let root = TempDir::new("lore-io-faithful-fork-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    let file = source.join("file");
    std::fs::write(&file, b"data").expect("write");
    let fork: Vec<u8> = (0..1u32 << 20)
        .map(|byte| u8::try_from(byte % 251).expect("small"))
        .collect();
    set_attribute(&file, "com.apple.ResourceFork", &fork);
    let progress = Progress::default();

    let destination =
        copy_faithfully_beside(&root, &source, &every_entry, &progress).expect("copy");

    assert_eq!(attributes(&destination.join("file")), attributes(&file));
    let held = progress.counts().value_held;
    assert!(held > 0 && held <= 256 << 10, "{held} bytes held at once");
}

/// A faithful copy refuses a file with a second link, a FIFO and a socket.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_faithful_copy_refuses_links_fifos_and_sockets() {
    type Make = fn(&Path);
    let makes: [(&str, Make); 3] = [
        ("hard link", |source| {
            std::fs::write(source.join("file"), b"f").expect("write");
            std::fs::hard_link(source.join("file"), source.join("second")).expect("link");
        }),
        ("fifo", |source| {
            let status = std::process::Command::new("mkfifo")
                .arg(source.join("fifo"))
                .status()
                .expect("mkfifo");
            assert!(status.success());
        }),
        ("socket", |source| {
            std::os::unix::net::UnixListener::bind(source.join("socket")).expect("bind");
        }),
    ];
    for (what, make) in makes {
        let root = TempDir::new("lore-io-faithful-refused-");
        let source = root.join("source");
        std::fs::create_dir(&source).expect("create");
        make(&source);

        let err = copy_faithfully_beside(&root, &source, &every_entry, &Progress::default())
            .expect_err(what);

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{what}: {err}");
    }
}

/// A faithful copy leaves out what its visit skips: the contents of each external directory's
/// `write-staging/`, which it keeps, and the root's marker, as a move of the external root asks.
#[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
#[test]
fn a_faithful_copy_leaves_out_write_staging_and_the_marker() {
    let root = TempDir::new("lore-io-faithful-staging-");
    let source = root.join("source");
    std::fs::create_dir_all(source.join("instance/write-staging/nested")).expect("create");
    std::fs::write(source.join("instance/write-staging/staged"), b"s").expect("write");
    std::fs::write(source.join("instance/kept"), b"k").expect("write");
    std::fs::write(source.join(".lore-external-root"), b"marker").expect("write");
    let visit = |_: &Path, relative: &Path, kind: Kind| {
        let mut components = relative.components().map(|part| part.as_os_str());
        let first = components.next();
        if first == Some(std::ffi::OsStr::new(".lore-external-root")) {
            return Visit::Skip;
        }
        if components.next() == Some(std::ffi::OsStr::new("write-staging"))
            && components.next().is_some()
        {
            return Visit::Skip;
        }
        if kind == Kind::Directory {
            Visit::EachEntry
        } else {
            Visit::Clone
        }
    };

    let destination =
        copy_faithfully_beside(&root, &source, &visit, &Progress::default()).expect("copy");

    let copied: Vec<_> = tree_of(&destination).into_keys().collect();
    assert_eq!(
        copied,
        [
            PathBuf::from("instance"),
            PathBuf::from("instance/kept"),
            PathBuf::from("instance/write-staging"),
        ]
    );
}

/// One flush of the copy's file system follows its last file: on macOS one `F_FULLFSYNC`, on the
/// calling thread, after an `fsync` of each file on its copying thread.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_faithful_copy_flushes_its_file_system_once() {
    let root = TempDir::new("lore-io-faithful-flush-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    make_tree(&source, 3, 2);
    #[cfg(target_os = "macos")]
    let files = tree_of(&source)
        .values()
        .filter(|file| file.is_some())
        .count();
    let progress = Progress::default();
    #[cfg(target_os = "macos")]
    let full_syncs = lore_io::psync::test_util::full_syncs_on_this_thread();

    copy_faithfully_beside(&root, &source, &every_entry, &progress).expect("copy");

    let counts = progress.counts();
    assert_eq!(counts.flushed, 1);
    #[cfg(target_os = "linux")]
    assert_eq!(counts.flushed_files, 0);
    #[cfg(target_os = "macos")]
    {
        assert_eq!(counts.flushed_files, files);
        assert_eq!(
            lore_io::psync::test_util::full_syncs_on_this_thread() - full_syncs,
            1
        );
    }
}

/// The walk's bounds hold in the faithful mode: the queue's cap over a tree wider than it, and
/// a directory wider than a batch copied whole through continuations.
#[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
#[test]
fn a_faithful_copy_keeps_the_walks_bounds() {
    let root = TempDir::new("lore-io-faithful-bounds-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    make_tree(&source, 12, 2);
    for file in 0..20 {
        std::fs::write(source.join(format!("w{file}")), b"w").expect("write");
    }
    let progress = Progress::bounded(2, 64, 4);

    let destination =
        copy_faithfully_beside(&root, &source, &every_entry, &progress).expect("copy");

    assert_eq!(tree_of(&destination), tree_of(&source));
    let counts = progress.counts();
    assert!(counts.queued <= 2, "{counts:?}");
    assert!(counts.handed <= 5, "{counts:?}");
    assert!(counts.kept >= 2, "{counts:?}");
    assert!(counts.continued >= 2, "{counts:?}");
}

/// A file capability `security.capability` with no capabilities, which only a caller with
/// `CAP_SETFCAP` can set.
#[cfg(target_os = "linux")]
const EMPTY_CAPABILITY: [u8; 20] = [
    0, 0, 0, 2, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Writes two files with a file capability and a `user.*` attribute under a new `source`, or
/// `None` where the caller can't set a file capability.
#[cfg(target_os = "linux")]
fn source_with_capabilities(root: &TempDir) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    for file in ["one", "two"] {
        let path = source.join(file);
        std::fs::write(&path, b"f").expect("write");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
        // SAFETY: the path and name are NUL-terminated, and the value is readable for its
        // length.
        let set = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                c"security.capability".as_ptr(),
                EMPTY_CAPABILITY.as_ptr().cast(),
                EMPTY_CAPABILITY.len(),
                0,
            )
        };
        if set != 0 {
            return None;
        }
        set_attribute(&path, "user.lore", b"kept");
    }
    Some(source)
}

/// A `security.*` attribute whose set the system refuses with `EPERM` or `EACCES` keeps the
/// value the target gives it, and the copy reports it with the entries it affected. Returns early
/// where the test can't set a file capability, which takes `CAP_SETFCAP`.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_security_attribute_is_reported_with_its_entries() {
    for code in [libc::EPERM, libc::EACCES] {
        let root = TempDir::new("lore-io-faithful-relabeled-");
        let Some(source) = source_with_capabilities(&root) else {
            return;
        };
        let destination = root.join("destination");
        std::fs::create_dir(&destination).expect("create");
        let progress = Progress::refusing_attribute("security.capability", code);

        let copied = lore_revision::fs::walk::copy_faithfully_on(
            None,
            &source,
            &destination,
            &every_entry,
            &progress,
        )
        .expect("copy");

        assert_eq!(
            copied.relabeled,
            [("security.capability".to_string(), 2)],
            "{code}"
        );
        for file in ["one", "two"] {
            let kept = attributes(&destination.join(file));
            assert!(
                !kept.contains_key(b"security.capability".as_slice()),
                "{code}"
            );
            assert_eq!(
                kept.get(b"user.lore".as_slice()).map(Vec::as_slice),
                Some(&b"kept"[..])
            );
        }
    }
}

/// Any other refused set fails the copy: a `security.*` attribute the target can't hold, and a
/// `user.*` attribute the system refuses. Returns early where the test can't set a file
/// capability.
#[cfg(target_os = "linux")]
#[test]
fn any_other_refused_attribute_fails_the_copy() {
    for (name, code) in [
        ("security.capability", libc::ENOTSUP),
        ("security.capability", libc::EIO),
        ("user.lore", libc::EPERM),
        ("user.lore", libc::ENOTSUP),
    ] {
        let root = TempDir::new("lore-io-faithful-refused-attribute-");
        let Some(source) = source_with_capabilities(&root) else {
            return;
        };
        let destination = root.join("destination");
        std::fs::create_dir(&destination).expect("create");

        let failed = lore_revision::fs::walk::copy_faithfully_on(
            None,
            &source,
            &destination,
            &every_entry,
            &Progress::refusing_attribute(name, code),
        )
        .expect_err(name);

        assert_eq!(
            failed.kind(),
            io::Error::from_raw_os_error(code).kind(),
            "{name}"
        );
        assert!(
            failed
                .to_string()
                .contains(&format!("setting the extended attribute {name}: ")),
            "{failed}"
        );
    }
}

/// Writes a file, a directory and a link under a new `source`, each given the group 5678 where
/// the caller is root, and answers it with the owner and group they have.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn source_with_a_group(root: &TempDir) -> (PathBuf, u32, u32) {
    use std::os::unix::fs::MetadataExt;

    let source = root.join("source");
    std::fs::create_dir_all(source.join("directory")).expect("create");
    std::fs::write(source.join("file"), b"f").expect("write");
    std::os::unix::fs::symlink("file", source.join("link")).expect("link");
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        for entry in ["", "file", "directory"] {
            std::os::unix::fs::chown(source.join(entry), None, Some(5678)).expect("chown");
        }
        std::os::unix::fs::lchown(source.join("link"), None, Some(5678)).expect("chown");
    }
    let metadata = std::fs::metadata(source.join("file")).expect("stat");
    (source, metadata.uid(), metadata.gid())
}

/// An entry whose group the caller may not give it, as one it isn't a member of, keeps the group
/// the target gives it, and the copy counts it: the root, a file, a directory and a link, through
/// the test point that refuses each set that gives a group.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_group_the_caller_may_not_give_keeps_the_targets() {
    use std::os::unix::fs::MetadataExt;

    let root = TempDir::new("lore-io-faithful-group-");
    let (source, uid, _) = source_with_a_group(&root);
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create");
    let given = std::fs::metadata(&destination).expect("stat").gid();

    let copied = lore_revision::fs::walk::copy_faithfully_on(
        None,
        &source,
        &destination,
        &every_entry,
        &Progress::refusing_owners(lore_io::clone::Refused::Group),
    )
    .expect("copy");

    assert_eq!(copied.kept_groups, 4);
    for entry in ["", "file", "directory", "link"] {
        let metadata = std::fs::symlink_metadata(destination.join(entry)).expect("stat");
        assert_eq!((metadata.uid(), metadata.gid()), (uid, given), "{entry}");
    }
}

/// An owner the caller may not give fails the copy, even once the group is left as the target
/// gives it, and the failure names the call and the owner and group asked for.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn an_owner_the_caller_may_not_give_fails_the_copy() {
    let root = TempDir::new("lore-io-faithful-owner-");
    let (source, uid, gid) = source_with_a_group(&root);
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create");

    let failed = lore_revision::fs::walk::copy_faithfully_on(
        None,
        &source,
        &destination,
        &every_entry,
        &Progress::refusing_owners(lore_io::clone::Refused::Owner),
    )
    .expect_err("an owner the caller may not give");

    assert_eq!(failed.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        failed
            .to_string()
            .contains(&format!("setting the owner {uid}:{gid}: ")),
        "{failed}"
    );
}

/// An attribute of a link that the target refuses to set as unsupported fails the copy, and the
/// failure names the call and the attribute. Returns early where a link can't hold an attribute
/// the tests' user can set, as on Linux.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_link_attribute_the_target_refuses_fails_the_copy() {
    let root = TempDir::new("lore-io-faithful-link-attribute-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    std::fs::write(source.join("file"), b"f").expect("write");
    let link = source.join("link");
    std::os::unix::fs::symlink("file", &link).expect("link");
    if try_set_attribute(&link, ATTRIBUTE, b"link value").is_err() {
        return;
    }
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create");

    let failed = lore_revision::fs::walk::copy_faithfully_on(
        None,
        &source,
        &destination,
        &every_entry,
        &Progress::refusing_attribute(ATTRIBUTE, libc::ENOTSUP),
    )
    .expect_err("an attribute the target link can't hold");

    assert_eq!(failed.kind(), io::ErrorKind::Unsupported);
    assert!(
        failed
            .to_string()
            .contains(&format!("setting the extended attribute {ATTRIBUTE}: ")),
        "{failed}"
    );
}

/// A `security.*` attribute that the target refuses to set with `ENOTSUP` but holds, as one the
/// system gives each new entry, keeps the target's value, and the copy reports it with the entries
/// it affected. Returns early where the test can't set a file capability.
#[cfg(target_os = "linux")]
#[test]
fn a_security_attribute_the_system_gives_the_target_keeps_its_value() {
    let root = TempDir::new("lore-io-faithful-assigned-");
    let Some(source) = source_with_capabilities(&root) else {
        return;
    };
    let destination = root.join("destination");
    std::fs::create_dir(&destination).expect("create");
    let progress =
        Progress::refusing_attribute_the_target_holds("security.capability", libc::ENOTSUP);

    let copied = lore_revision::fs::walk::copy_faithfully_on(
        None,
        &source,
        &destination,
        &every_entry,
        &progress,
    )
    .expect("copy");

    assert_eq!(copied.relabeled, [("security.capability".to_string(), 2)]);
    for file in ["one", "two"] {
        assert_eq!(
            attributes(&destination.join(file))
                .get(b"user.lore".as_slice())
                .map(Vec::as_slice),
            Some(&b"kept"[..])
        );
    }
}

/// Sets the flag `flag` of `path` on, or off where `on` is clear: `false` where the caller can't.
/// `flag` as the faithful copy reads flags.
#[cfg(target_os = "linux")]
fn flag_bits(flag: libc::c_int) -> u64 {
    u64::try_from(flag).expect("flags")
}

/// `flag` as the faithful copy reads flags.
#[cfg(target_os = "macos")]
fn flag_bits(flag: libc::c_uint) -> u64 {
    u64::from(flag)
}

#[cfg(target_os = "linux")]
fn set_flag(path: &Path, flag: i32, on: bool) -> bool {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    else {
        return false;
    };
    let mut flags: libc::c_int = 0;
    // SAFETY: the descriptor is open, and `flags` is writable.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &raw mut flags) } != 0 {
        return false;
    }
    flags = if on { flags | flag } else { flags & !flag };
    // SAFETY: the descriptor is open, and `flags` is readable.
    unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &raw const flags) == 0 }
}

/// Sets the flag `flag` of `path` on, or off where `on` is clear: `false` where the caller can't.
#[cfg(target_os = "macos")]
fn set_flag(path: &Path, flag: libc::c_uint, on: bool) -> bool {
    use std::os::macos::fs::MetadataExt;
    use std::os::unix::ffi::OsStrExt;

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let flags = if on {
        metadata.st_flags() | flag
    } else {
        metadata.st_flags() & !flag
    };
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
    // SAFETY: the path is NUL-terminated.
    unsafe { libc::chflags(c_path.as_ptr(), flags) == 0 }
}

/// A faithful copy refuses a file, a link, a directory or the root it copies whose status it reads
/// as immutable or append-only, through flags the test stands in for, which it couldn't set
/// itself.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_faithful_copy_refuses_entries_it_reads_as_immutable() {
    #[cfg(target_os = "linux")]
    let flags = [libc::STATX_ATTR_IMMUTABLE, libc::STATX_ATTR_APPEND];
    #[cfg(target_os = "macos")]
    let flags = [libc::UF_IMMUTABLE, libc::SF_APPEND];
    for flag in flags {
        for entry in ["file", "link", "directory", "source"] {
            let root = TempDir::new("lore-io-faithful-read-flags-");
            let source = root.join("source");
            std::fs::create_dir_all(source.join("directory")).expect("create");
            std::fs::write(source.join("file"), b"f").expect("write");
            std::os::unix::fs::symlink("file", source.join("link")).expect("link");
            let progress = Progress::flagging(entry, flag_bits(flag));

            let err =
                copy_faithfully_beside(&root, &source, &every_entry, &progress).expect_err(entry);

            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidInput,
                "{entry} {flag:#x}: {err}"
            );
        }
    }
    let root = TempDir::new("lore-io-faithful-read-flags-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    std::fs::write(source.join("file"), b"f").expect("write");
    #[cfg(target_os = "linux")]
    let shown = libc::STATX_ATTR_NODUMP;
    #[cfg(target_os = "macos")]
    let shown = libc::UF_HIDDEN;
    let progress = Progress::flagging("file", flag_bits(shown));
    copy_faithfully_beside(&root, &source, &every_entry, &progress).expect("a flag left out");
}

/// A faithful copy refuses a file or a directory marked immutable or append-only. Returns early
/// where the caller can't set the flag, which takes `CAP_LINUX_IMMUTABLE` on Linux.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_faithful_copy_refuses_immutable_and_append_only_entries() {
    // `FS_IMMUTABLE_FL` and `FS_APPEND_FL`, which `libc` doesn't bind.
    #[cfg(target_os = "linux")]
    let flags = [0x10, 0x20];
    #[cfg(target_os = "macos")]
    let flags = [libc::UF_IMMUTABLE, libc::UF_APPEND];
    for flag in flags {
        for entry in ["file", "directory"] {
            let root = TempDir::new("lore-io-faithful-flags-");
            let source = root.join("source");
            std::fs::create_dir_all(source.join("directory")).expect("create");
            std::fs::write(source.join("file"), b"f").expect("write");
            let flagged = source.join(entry);
            if !set_flag(&flagged, flag, true) {
                return;
            }

            let copied = copy_faithfully_beside(&root, &source, &every_entry, &Progress::default());

            assert!(set_flag(&flagged, flag, false), "clear the flag");
            let err = copied.expect_err(entry);
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidInput,
                "{entry} {flag:#x}: {err}"
            );
        }
    }
}

/// `path` with its alternate data stream `name`.
#[cfg(target_family = "windows")]
fn stream_of(path: &Path, name: &str) -> PathBuf {
    let mut named = path.as_os_str().to_os_string();
    named.push(format!(":{name}"));
    PathBuf::from(named)
}

/// The named data streams of `path`, each with its contents.
#[cfg(target_family = "windows")]
fn streams_of(path: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::FindClose;
    use windows_sys::Win32::Storage::FileSystem::FindFirstStreamW;
    use windows_sys::Win32::Storage::FileSystem::FindNextStreamW;
    use windows_sys::Win32::Storage::FileSystem::FindStreamInfoStandard;
    use windows_sys::Win32::Storage::FileSystem::WIN32_FIND_STREAM_DATA;

    let wide = wide(path);
    let mut found = WIN32_FIND_STREAM_DATA::default();
    // SAFETY: the path is NUL-terminated, and `found` is writable.
    let search = unsafe {
        FindFirstStreamW(
            wide.as_ptr(),
            FindStreamInfoStandard,
            (&raw mut found).cast(),
            0,
        )
    };
    let mut streams = std::collections::BTreeMap::new();
    if search == INVALID_HANDLE_VALUE {
        return streams;
    }
    loop {
        let length = found
            .cStreamName
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(found.cStreamName.len());
        let name = String::from_utf16(&found.cStreamName[..length]).expect("a stream name");
        if name != "::$DATA" {
            let stream = name.trim_start_matches(':').trim_end_matches(":$DATA");
            streams.insert(
                name.clone(),
                std::fs::read(stream_of(path, stream)).expect("read the stream"),
            );
        }
        // SAFETY: the search is open, and `found` is writable.
        if unsafe { FindNextStreamW(search, (&raw mut found).cast()) } == 0 {
            break;
        }
    }
    // SAFETY: the search is open, and closed only here.
    unsafe { FindClose(search) };
    streams
}

/// `path` as a NUL-terminated wide string.
#[cfg(target_family = "windows")]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().chain([0]).collect()
}

/// A faithful copy of a file that fails once its data is written removes what it wrote, through
/// the test point that fails the copy of a file then.
#[cfg(target_family = "windows")]
#[test]
fn a_faithful_copy_that_fails_partway_removes_the_file() {
    let root = TempDir::new("lore-io-faithful-partway-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    std::fs::write(source.join("partway"), vec![7; 1 << 20]).expect("write");

    let failed = copy_faithfully_beside(
        &root,
        &source,
        &every_entry,
        &Progress::failing_file("partway"),
    );

    assert!(failed.is_err());
    assert!(
        !root.join("destination").join("partway").exists(),
        "the file whose copy failed is removed"
    );
}

/// The owner, group, DACL and mandatory label of `path`, as SDDL.
#[cfg(target_family = "windows")]
fn security_of(path: &Path) -> String {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetFileSecurityW;
    use windows_sys::Win32::Security::LABEL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

    let what = OWNER_SECURITY_INFORMATION
        | GROUP_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | LABEL_SECURITY_INFORMATION;
    let wide = wide(path);
    let mut needed = 0u32;
    // SAFETY: the path is NUL-terminated, and a call with no buffer only sizes it.
    unsafe { GetFileSecurityW(wide.as_ptr(), what, std::ptr::null_mut(), 0, &mut needed) };
    let mut descriptor = vec![0u8; needed as usize];
    // SAFETY: the path is NUL-terminated, and `descriptor` is writable for `needed` bytes.
    let read = unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            what,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    assert_ne!(read, 0, "read the security: {}", io::Error::last_os_error());
    let mut text = std::ptr::null_mut();
    let mut length = 0u32;
    // SAFETY: `descriptor` holds a self-relative security descriptor, and the outputs are
    // writable.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.as_mut_ptr().cast(),
            SDDL_REVISION_1,
            what,
            &mut text,
            &mut length,
        )
    };
    assert_ne!(converted, 0, "{}", io::Error::last_os_error());
    // SAFETY: the call wrote `length` characters at `text`.
    let sddl =
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length as usize) });
    // SAFETY: the call allocated `text` with `LocalAlloc`.
    unsafe { LocalFree(text.cast()) };
    sddl.trim_end_matches('\0').to_owned()
}

/// Sets the DACL and the mandatory label of `path` to those `sddl` names.
#[cfg(target_family = "windows")]
fn set_security(path: &Path, sddl: &str) {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::LABEL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::SetFileSecurityW;

    let labelled = sddl.contains("S:");
    let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the text is NUL-terminated, and `descriptor` is writable.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(converted, 0, "{}", io::Error::last_os_error());
    let what = DACL_SECURITY_INFORMATION
        | if labelled {
            LABEL_SECURITY_INFORMATION
        } else {
            0
        };
    // SAFETY: the path is NUL-terminated, and `descriptor` is a security descriptor.
    let set = unsafe { SetFileSecurityW(wide(path).as_ptr(), what, descriptor) };
    let failure = io::Error::last_os_error();
    // SAFETY: the call allocated `descriptor` with `LocalAlloc`.
    unsafe { LocalFree(descriptor.cast()) };
    assert_ne!(set, 0, "set the security of {}: {failure}", path.display());
}

/// Sets the creation, access and write times of the file or directory `path` to odd times.
#[cfg(target_family = "windows")]
fn set_odd_times(path: &Path, seed: u64) {
    use std::os::windows::fs::FileTimesExt;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;

    std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .expect("open to set the times")
        .set_times(
            FileTimes::new()
                .set_created(odd_time(seed))
                .set_accessed(odd_time(seed + 100))
                .set_modified(odd_time(seed + 200)),
        )
        .expect("set the times");
}

/// Sets the attributes of `path` to `attributes`.
#[cfg(target_family = "windows")]
fn set_attributes(path: &Path, attributes: u32) {
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;

    // SAFETY: the path is NUL-terminated.
    let set = unsafe { SetFileAttributesW(wide(path).as_ptr(), attributes) };
    assert_ne!(set, 0, "{}", io::Error::last_os_error());
}

/// The bytes the volume holds for the data of the file `path`.
#[cfg(target_family = "windows")]
fn allocated(path: &Path) -> u64 {
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    let mut high = 0u32;
    // SAFETY: the path is NUL-terminated, and `high` is writable.
    let low = unsafe { GetCompressedFileSizeW(wide(path).as_ptr(), &mut high) };
    (u64::from(high) << 32) | u64::from(low)
}

/// What a faithful copy keeps of an entry on Windows: its attributes, its creation, access and
/// write times, its security and its named data streams.
#[cfg(target_family = "windows")]
type Kept = (
    u32,
    (u64, u64, u64),
    String,
    std::collections::BTreeMap<String, Vec<u8>>,
);

/// What a faithful copy keeps of the entry `path`.
#[cfg(target_family = "windows")]
fn kept_of(path: &Path) -> Kept {
    use std::os::windows::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path).expect("stat");
    (
        metadata.file_attributes(),
        (
            metadata.creation_time(),
            metadata.last_access_time(),
            metadata.last_write_time(),
        ),
        security_of(path),
        streams_of(path),
    )
}

/// A faithful copy keeps the data, the alternate data streams, the security, the attributes and
/// the times of files and directories, and the sparse ranges of a file: of a file whose DACL grants
/// no write too, and of data and streams longer than the copy's buffer.
#[cfg(target_family = "windows")]
#[test]
fn a_faithful_copy_keeps_streams_security_attributes_and_sparse_ranges() {
    use std::os::windows::fs::FileExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ARCHIVE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    let root = TempDir::new("lore-io-faithful-windows-");
    let source = root.join("source");
    let directory = source.join("directory");
    std::fs::create_dir_all(&directory).expect("create");
    std::fs::write(directory.join("plain"), b"plain").expect("write");
    let sparse = source.join("sparse");
    {
        let file = std::fs::File::create(&sparse).expect("create");
        let mut returned = 0u32;
        // SAFETY: the handle is open, and the control takes no buffers.
        let made = unsafe {
            DeviceIoControl(
                std::os::windows::io::AsRawHandle::as_raw_handle(&file),
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(made, 0, "{}", io::Error::last_os_error());
        file.set_len(64 << 20).expect("size");
        file.seek_write(&[7; 4096], 1 << 20).expect("write");
        file.seek_write(&[9; 4096], 32 << 20).expect("write");
    }
    std::fs::write(stream_of(&sparse, "lore"), b"file stream").expect("write");
    let long: Vec<u8> = (0..(1 << 20) + 12_345)
        .map(|byte: u32| (byte % 251) as u8)
        .collect();
    let read_only = source.join("read-only");
    std::fs::write(&read_only, &long).expect("write");
    std::fs::write(stream_of(&read_only, "long"), &long[..300_000]).expect("write");
    std::fs::write(stream_of(&directory, "lore"), &long[..300_000]).expect("write");
    set_security(
        &sparse,
        "D:P(A;;FA;;;WD)(A;;FR;;;S-1-5-32-545)S:(ML;;NW;;;LW)",
    );
    set_security(
        &directory,
        "D:P(A;OICI;FA;;;WD)(A;;0x1200a9;;;S-1-5-32-545)",
    );
    for (seed, path) in [&sparse, &directory.join("plain"), &directory, &read_only]
        .into_iter()
        .enumerate()
    {
        set_odd_times(path, 1000 * seed as u64);
    }
    set_attributes(
        &sparse,
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED | FILE_ATTRIBUTE_READONLY,
    );
    set_attributes(&directory.join("plain"), FILE_ATTRIBUTE_NORMAL);
    set_attributes(&directory, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
    set_security(&read_only, "D:P(A;;FR;;;WD)");

    let destination =
        copy_faithfully_beside(&root, &source, &every_entry, &Progress::default()).expect("copy");

    for entry in ["sparse", "directory", "directory/plain", "read-only"] {
        let (from, to) = (source.join(entry), destination.join(entry));
        assert_eq!(kept_of(&to), kept_of(&from), "{entry}");
    }
    assert_eq!(
        std::fs::read(destination.join("read-only")).expect("read"),
        long
    );
    let (attributes, ..) = kept_of(&destination.join("directory/plain"));
    assert_eq!(
        attributes & FILE_ATTRIBUTE_ARCHIVE,
        0,
        "the archive bit is cleared"
    );
    let copy = destination.join("sparse");
    assert_eq!(
        std::fs::read(&copy).expect("read"),
        std::fs::read(&sparse).expect("read")
    );
    assert_eq!(
        std::fs::read(destination.join("directory/plain")).expect("read"),
        b"plain"
    );
    let (attributes, ..) = kept_of(&copy);
    assert_ne!(attributes & FILE_ATTRIBUTE_SPARSE_FILE, 0);
    let allocated = allocated(&copy);
    assert!(
        allocated < 4 << 20,
        "{allocated} bytes allocated for 8 KiB of data"
    );
}

/// A faithful copy refuses a file with a second link, a junction, a compressed file and directory,
/// and, where this account may create one, a symbolic link.
#[cfg(target_family = "windows")]
#[test]
fn a_faithful_copy_refuses_reparse_points_and_second_links() {
    type Make = fn(&Path) -> bool;
    let makes: [(&str, Make); 5] = [
        ("hard link", |source| {
            std::fs::write(source.join("file"), b"f").expect("write");
            std::fs::hard_link(source.join("file"), source.join("second")).expect("link");
            true
        }),
        ("junction", |source| {
            std::fs::create_dir(source.join("target")).expect("create");
            lore_io::junction::create_junction(&source.join("target"), &source.join("junction"))
                .expect("junction");
            true
        }),
        ("symbolic link", |source| {
            std::fs::write(source.join("file"), b"f").expect("write");
            std::os::windows::fs::symlink_file("file", source.join("link")).is_ok()
        }),
        ("compressed file", |source| {
            std::fs::write(source.join("file"), b"f").expect("write");
            compress(&source.join("file"));
            true
        }),
        ("compressed directory", |source| {
            std::fs::create_dir(source.join("directory")).expect("create");
            compress(&source.join("directory"));
            true
        }),
    ];
    for (what, make) in makes {
        let root = TempDir::new("lore-io-faithful-refused-");
        let source = root.join("source");
        std::fs::create_dir(&source).expect("create");
        if !make(&source) {
            continue;
        }

        let err = copy_faithfully_beside(&root, &source, &every_entry, &Progress::default())
            .expect_err(what);

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{what}: {err}");
    }
}

/// Compresses the file or directory `path` with NTFS compression.
#[cfg(target_family = "windows")]
fn compress(path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_COMPRESSION;

    let entry = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .expect("open to compress");
    let format: u16 = 1;
    let mut returned = 0u32;
    // SAFETY: the handle is open, and `format` is readable for its size.
    let compressed = unsafe {
        DeviceIoControl(
            std::os::windows::io::AsRawHandle::as_raw_handle(&entry),
            FSCTL_SET_COMPRESSION,
            (&raw const format).cast(),
            size_of::<u16>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(compressed, 0, "{}", io::Error::last_os_error());
}

/// Each file of a faithful copy is flushed once, by the call that holds its handle, before the
/// handle closes, as the test-util counter beside the flush counts, and the copy flushes no file
/// system as a whole.
#[cfg(target_family = "windows")]
#[test]
fn a_faithful_copy_flushes_each_file_before_it_closes() {
    let root = TempDir::new("lore-io-faithful-flush-");
    let source = root.join("source");
    std::fs::create_dir(&source).expect("create");
    make_tree(&source, 3, 2);
    let progress = Progress::default();

    copy_faithfully_beside(&root, &source, &every_entry, &progress).expect("copy");

    let files = tree_of(&source)
        .values()
        .filter(|data| data.is_some())
        .count();
    assert_eq!(files, 13);
    assert_eq!(progress.counts().flushed_files, files);
    assert_eq!(progress.counts().flushed, 0);
}
