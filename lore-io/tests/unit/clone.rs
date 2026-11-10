// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! `clone_into` copies a tree with contents, permissions, modification times and symbolic links
//! kept, through each backend, leaves out what it is told to and what disappears, never takes
//! over an entry that exists, and fails on a file its backend cannot clone unless told to copy
//! it. `detect` and `choose` pick the backend for a pair of directories, and `measure` sizes a
//! copy.

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
use lore_io::clone::Progress;
use lore_io::clone::Source;
use lore_io::clone::TreeSize;
use lore_io::clone::Visit;
use lore_io::clone::choose;
use lore_io::clone::clone_entry;
use lore_io::clone::clone_into;
use lore_io::clone::clone_into_with;
use lore_io::clone::detect;
use lore_io::clone::duplicate_ranges;
use lore_io::clone::measure;

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
    let root = TempDir::new("lore-io-clone-tree-");
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

        // The clone is independent of its source.
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

    let root = TempDir::new("lore-io-clone-links-");
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

    let root = TempDir::new("lore-io-clone-read-only-");
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
    let root = TempDir::new("lore-io-clone-copy-counts-");
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
    let root = TempDir::new("lore-io-clone-apfs-");
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

/// Each block-cloning backend the scratch volume cannot use fails a clone as unsupported, and
/// copies every file where a copy is allowed, with its time.
#[test]
fn a_backend_the_volume_cannot_use_fails_or_copies() {
    let root = TempDir::new("lore-io-clone-unusable-");
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
    let root = TempDir::new_in(Path::new(&volume), "lore-io-clone-volume-");
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
    let root = TempDir::new("lore-io-clone-rewritten-");
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
    let root = TempDir::new("lore-io-clone-gone-");
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
    let root = TempDir::new("lore-io-clone-stopped-");
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
    .expect_err("a stopped clone fails");

    assert_eq!(err.kind(), io::ErrorKind::Interrupted);
}

/// A FIFO in the tree is left out of the copy.
#[cfg(target_family = "unix")]
#[test]
fn a_fifo_in_the_tree_is_left_out() {
    let root = TempDir::new("lore-io-clone-fifo-");
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

#[test]
fn detection_leaves_no_probe_behind() {
    let root = TempDir::new("lore-io-clone-probe-");
    let source = root.join("source");
    let destination = root.join("parent");
    std::fs::create_dir(&source).expect("create the source");
    std::fs::create_dir(&destination).expect("create the parent");

    let detection = detect(&source, &destination).expect("detect the backend");

    assert_eq!(detection.source, detection.destination);
    for directory in [&source, &destination] {
        assert_eq!(
            std::fs::read_dir(directory)
                .expect("read the directory")
                .count(),
            0
        );
    }
}

#[test]
fn different_filesystems_copy_without_a_probe() {
    let backend = choose(false, |backend| panic!("{backend:?} was probed"));

    assert_eq!(backend.expect("a choice"), Backend::Copy);
}

#[test]
fn a_volume_whose_probes_all_fail_copies() {
    let mut probed = Vec::new();

    let backend = choose(true, |backend| {
        probed.push(backend);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    });

    assert_eq!(backend.expect("a choice"), Backend::Copy);
    assert_eq!(probed, Backend::BLOCK_CLONING);
}

#[test]
fn a_backend_whose_probe_clones_is_chosen() {
    let Some(&cloning) = Backend::BLOCK_CLONING.last() else {
        return;
    };

    let backend = choose(true, |backend| {
        if backend == cloning {
            Ok(())
        } else {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    });

    assert_eq!(backend.expect("a choice"), cloning);
}

#[test]
fn a_probe_that_cannot_write_fails_the_choice() {
    if Backend::BLOCK_CLONING.is_empty() {
        return;
    }

    let backend = choose(true, |_| {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    });

    assert_eq!(
        backend.expect_err("no choice").kind(),
        io::ErrorKind::PermissionDenied
    );
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
    let root = TempDir::new("lore-io-clone-refused-");
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
    .expect_err("a file that cannot clone fails the clone");

    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    assert!(!destination.join("a/b/deep.bin").exists());
}

#[test]
fn a_file_that_fails_to_clone_is_copied_when_allowed() {
    let root = TempDir::new("lore-io-clone-copied-");
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
    let root = TempDir::new("lore-io-clone-whole-once-");
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
    let root = TempDir::new("lore-io-clone-into-");
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

/// A directory the clone makes itself, rather than one already present, is still visited entry
/// by entry when told to, so an entry inside it can be left out.
#[test]
fn a_directory_visited_entry_by_entry_leaves_out_skipped_entries_inside_it() {
    let root = TempDir::new("lore-io-clone-each-");
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
    let root = TempDir::new("lore-io-clone-into-present-");
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
    let root = TempDir::new("lore-io-clone-measure-");
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

#[test]
fn duplicate_ranges_cover_whole_clusters_in_calls_under_four_gibibytes() {
    let ranges = |length, cluster| duplicate_ranges(length, cluster).collect::<Vec<_>>();
    let most = (1u64 << 32) - 4096;

    assert_eq!(ranges(0, 4096), vec![]);
    assert_eq!(ranges(100, 4096), vec![(0, 4096)]);
    assert_eq!(ranges(8192, 4096), vec![(0, 8192)]);
    assert_eq!(
        ranges(5 << 30, 4096),
        vec![(0, most), (most, (5 << 30) - most)]
    );
    assert_eq!(
        ranges(1 << 32, 65_536),
        vec![(0, (1 << 32) - 65_536), ((1 << 32) - 65_536, 65_536)]
    );
}

/// A directory present in the destination is filled, not replaced, by every backend, where the
/// clone would otherwise clone that directory as a whole.
#[test]
fn a_present_directory_is_filled_by_every_backend() {
    let root = TempDir::new("lore-io-clone-present-dir-");
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
    let root = TempDir::new("lore-io-clone-junction-");
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

/// Every directory of a wide and deep tree is listed once, on many threads, and the walk ends.
#[test]
fn a_walk_lists_every_directory_once_on_many_threads() {
    let listed = std::sync::atomic::AtomicUsize::new(0);

    lore_io::clone::run(0u32, 8, &|depth, found| {
        listed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if depth < 4 {
            found.extend(std::iter::repeat_n(depth + 1, 6));
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
    let err = lore_io::clone::run(0u32, 4, &|depth, found| {
        if depth == 2 {
            panic!("listing failed");
        }
        found.extend([depth + 1, depth + 1]);
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

    let err = lore_io::clone::run(0u32, 4, &|depth, found| match depth {
        0 => {
            found.extend([1, 2]);
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

/// A file operation on the syscall pool completes while every thread of a clone is busy, since
/// the clone runs only on the threads the pool lends it. The pool's budget is the pool tests'
/// subject; this one checks the clone takes no thread the operation needs.
#[test]
fn a_file_operation_completes_while_a_clone_runs() {
    let root = TempDir::new("lore-io-clone-concurrent-");
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
                // The operation is issued once the clone is busy.
                while entered.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    tokio::task::yield_now().await;
                }
                lore_io::IoDriver::global().metadata(&probe).await
            })
            .await
        });
        drop(release);
        assert!(stat.is_ok(), "the file operation waits on the clone");
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
        let root = TempDir::new("lore-io-clone-replaced-");
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

/// A root that is not there fails a clone, a measurement and a search as not found, rather than
/// counting as an empty tree.
#[test]
fn a_root_that_is_not_there_is_not_found() {
    let root = TempDir::new("lore-io-clone-no-root-");
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
    let found = lore_io::clone::find_directories(&missing, &|_| true, &Progress::default());

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

    let found = lore_io::clone::find_directories(&root.join("tree"), &|_| false, &progress);

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

    let mut found = lore_io::clone::find_directories(
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

        let found = lore_io::clone::find_directories(
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
/// rather than failing the clone, as an entry gone by the time the clone reaches it is.
#[cfg(target_family = "unix")]
#[test]
fn a_directory_whose_parent_became_a_file_is_left_out() {
    let root = TempDir::new("lore-io-clone-parent-file-");
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
    .expect("the clone leaves the directories out");

    assert!(destination.join("a/c/kept").is_file());
}

/// A file replaced by a junction after it was listed is copied as the junction, not followed.
#[cfg(target_family = "windows")]
#[test]
fn a_file_replaced_by_a_junction_is_copied_as_one() {
    let root = TempDir::new("lore-io-clone-file-junction-");
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
    let root = TempDir::new("lore-io-clone-link-file-");
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
    let root = TempDir::new("lore-io-clone-socket-");
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
    .expect("the clone leaves the socket out");

    assert!(std::fs::symlink_metadata(destination.join("f")).is_err());
    assert_eq!(
        std::fs::read(destination.join("kept")).expect("read"),
        b"kept"
    );
}

/// A link swapped in, while a directory is being filled, for that directory or for a directory
/// above it, does not lead the clone out of the tree: every entry is read from the directory being
/// filled, and nothing from where the link leads. Unix reads each entry through the directory,
/// held open, so the swap goes ahead and changes nothing; Windows refuses the swap, as the clone
/// holds the directory open without sharing delete access, and refuses to rename a directory
/// with an entry open below it.
#[test]
fn a_link_swapped_in_while_a_directory_is_filled_is_not_followed() {
    for swapped in ["top/a", "top"] {
        let root = TempDir::new("lore-io-clone-swapped-mid-fill-");
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

/// A walk's root reached through a link is walked as the directory the link leads to: a clone, a
/// measurement and a search all read the tree there.
#[test]
fn a_root_reached_through_a_link_is_walked() {
    let root = TempDir::new("lore-io-clone-linked-root-");
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
    let found = lore_io::clone::find_directories(
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

    let root = TempDir::new("lore-io-clone-umask-");
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
    let root = TempDir::new("lore-io-clone-measure-stopped-");
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
/// is opened before it is cloned, and the directory found there fails the clone.
#[cfg(target_os = "macos")]
#[test]
fn apfs_clones_no_directory_put_in_place_of_a_listed_file() {
    let root = TempDir::new("lore-io-clone-apfs-swapped-");
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

    lore_io::clone::run_lent_on(&pool, 0usize, &|item, found| {
        if item == 0 {
            found.extend(1..=4);
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

    lore_io::clone::run_lent_on(&pool, 0usize, &|item, found| {
        match item {
            0 => found.extend(1..=8),
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
/// does, is listed once the handle is closed, rather than failing the clone.
#[cfg(target_family = "windows")]
#[test]
fn a_directory_held_with_delete_access_is_listed_once_released() {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

    let root = TempDir::new("lore-io-clone-held-delete-");
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
            // Released 15 ms in, which a clone that reaches the directory first waits out.
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
