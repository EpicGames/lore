// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Backend conformance suite. Every backend must pass these tests with
//! identical observable semantics; new backends join the [`drivers`] list.

use bytes::Bytes;
use futures::future::join_all;
use lore_base::test_util::TempDir;
use lore_base::test_util::file_identity;
use lore_io::BackendKind;
use lore_io::IoDriver;
use lore_io::OpenOptions;
use lore_io::Stat;
use lore_io::StatKind;

/// Every backend this build has, skipping one the machine cannot run. A kernel without
/// `io_uring`, or a container that blocks `io_uring_setup`, must not fail the suite — but the
/// skip is announced, because a silent skip reads exactly like a pass.
fn drivers() -> Vec<IoDriver> {
    #[allow(unused_mut)]
    let mut drivers =
        vec![IoDriver::new(BackendKind::Psync).expect("psync backend is always available")];
    #[cfg(target_os = "linux")]
    match IoDriver::new(BackendKind::Uring) {
        Ok(driver) => drivers.push(driver),
        Err(error) => eprintln!("skipping the uring backend: {error}"),
    }
    #[cfg(target_family = "windows")]
    match IoDriver::new(BackendKind::Iocp) {
        Ok(driver) => drivers.push(driver),
        Err(error) => eprintln!("skipping the iocp backend: {error}"),
    }
    drivers
}

fn rw_create() -> OpenOptions {
    OpenOptions::new().read(true).write(true).create(true)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test]
async fn write_read_roundtrip() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-roundtrip-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = pattern(192 * 1024, 7);
        file.write_all_at(Bytes::from(data.clone()), 0)
            .await
            .unwrap();
        let read = file.read_exact_at(data.len(), 0).await.unwrap();
        assert_eq!(&read[..], &data[..]);
    }
}

/// A file opened with `std` writes, syncs its data and syncs it all through every backend.
#[tokio::test]
async fn an_open_std_file_writes_and_syncs_through_every_backend() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-from-std-");
        let std_file = lore_io::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .to_std()
            .open(dir.child("f"))
            .expect("std open");
        // Safety: the file moves into the wrap, and nothing else holds a handle to it.
        let file = unsafe { lore_io::IoFile::from_std(&driver, std::sync::Arc::new(std_file)) }
            .expect("from_std");
        let data = pattern(4096, 5);
        file.write_all_at(data.clone(), 0).await.expect("write");
        file.sync_data().await.expect("sync_data");
        file.sync_all().await.expect("sync_all");
        let read = file.read_exact_at(data.len(), 0).await.expect("read");
        assert_eq!(&read[..], &data[..], "{}", driver.backend_name());
    }
}

#[tokio::test]
async fn positional_reads_have_no_cursor() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-positional-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = pattern(1024 * 1024, 3);
        file.write_all_at(Bytes::from(data.clone()), 0)
            .await
            .unwrap();

        for offset in [512 * 1024usize, 0, 960 * 1024, 256 * 1024] {
            let read = file.read_exact_at(64 * 1024, offset as u64).await.unwrap();
            assert_eq!(&read[..], &data[offset..offset + 64 * 1024]);
        }
    }
}

#[tokio::test]
async fn read_at_past_eof_returns_zero() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-past-eof-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        file.write_all_at(Bytes::from_static(&[1, 2, 3]), 0)
            .await
            .unwrap();
        let read = file.read_at(4096, 4096).await.unwrap();
        assert!(read.is_empty(), "a read past the end returned bytes");
    }
}

/// A short read returns the bytes that arrived, and only those. Read buffers are allocated
/// uninitialised and truncated to the byte count the syscall reports, so the returned length being
/// right is also what keeps uninitialised memory from reaching a caller.
#[tokio::test]
async fn read_at_is_partial_at_eof() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-partial-eof-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = pattern(100, 9);
        file.write_all_at(Bytes::from(data.clone()), 0)
            .await
            .unwrap();
        let read = file.read_at(64 * 1024, 0).await.unwrap();
        assert_eq!(read.len(), 100);
        assert_eq!(&read[..], &data[..]);
    }
}

/// The read length is the caller's, and neither the allocation nor the file decides it. Reading
/// 100 bytes of a 64 KiB file must yield 100, so that a caller asking for one fragment cannot be
/// handed the rest of the file with it.
#[tokio::test]
async fn read_at_honours_the_requested_length() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-read-len-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = pattern(64 * 1024, 37);
        file.write_all_at(Bytes::from(data.clone()), 0)
            .await
            .unwrap();

        let read = file.read_at(100, 0).await.unwrap();

        assert_eq!(read.len(), 100, "read past the requested length");
        assert_eq!(&read[..], &data[..100]);
    }
}

#[tokio::test]
async fn read_exact_at_fails_past_eof() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-exact-eof-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        file.write_all_at(Bytes::from_static(&[0u8; 100]), 0)
            .await
            .unwrap();
        let error = file.read_exact_at(200, 0).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}

#[tokio::test]
async fn write_at_extends_file() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-extend-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let offset = 1024 * 1024;
        file.write_all_at(Bytes::from_static(&[7u8; 4096]), offset)
            .await
            .unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), offset + 4096);
    }
}

#[tokio::test]
async fn write_at_accepts_bytes() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-bytes-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = Bytes::from(pattern(8192, 11));
        file.write_all_at(data.clone(), 0).await.unwrap();
        let read = file.read_exact_at(data.len(), 0).await.unwrap();
        assert_eq!(read, data);
    }
}

/// A write takes its length from the buffer it is given, for every buffer type. Reads take theirs
/// as an argument, because they allocate rather than being handed memory.
#[tokio::test]
async fn a_write_takes_its_length_from_the_buffer() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-write-len-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();

        let empty = file.write_at(Bytes::new(), 0).await.unwrap();
        assert_eq!(empty.1, 0, "an empty buffer wrote bytes");
        assert_eq!(file.metadata().await.unwrap().len(), 0);

        let filled = Bytes::from(pattern(100, 43));
        let (_, written) = file.write_at(filled, 0).await.unwrap();
        assert_eq!(written, 100);
        assert_eq!(file.metadata().await.unwrap().len(), 100);

        let (_, written) = file.write_at(vec![9u8; 64], 100).await.unwrap();
        assert_eq!(written, 64, "a Vec writes its whole length");
        assert_eq!(file.metadata().await.unwrap().len(), 164);

        let read = file.read_exact_at(164, 0).await.unwrap();
        assert_eq!(&read[..100], &pattern(100, 43)[..]);
        assert_eq!(&read[100..], &[9u8; 64][..]);
    }
}

#[tokio::test]
async fn write_file_bytes_roundtrip() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-durable-");
        let data = Bytes::from(pattern(16 * 1024, 13));
        let path = dir.child("blob");
        let metadata = driver
            .write_file_bytes(&path, data.clone(), true)
            .await
            .unwrap();
        assert_eq!(metadata.len(), data.len() as u64);
        let file = driver
            .open(&path, &OpenOptions::new().read(true))
            .await
            .unwrap();
        let read = file.read_exact_at(data.len(), 0).await.unwrap();
        assert_eq!(read, data);
    }
}

#[tokio::test]
async fn read_file_bytes_roundtrip() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-read-file-");
        let data = Bytes::from(pattern(48 * 1024, 19));
        let path = dir.child("blob");
        driver
            .write_file_bytes(&path, data.clone(), false)
            .await
            .unwrap();
        let read = driver.read_file_bytes(&path).await.unwrap();
        assert_eq!(read, data);

        let empty = dir.child("empty");
        driver
            .write_file_bytes(&empty, Bytes::new(), false)
            .await
            .unwrap();
        assert!(driver.read_file_bytes(&empty).await.unwrap().is_empty());

        let error = driver
            .read_file_bytes(dir.child("absent"))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

/// A directory is not a file, whatever size the filesystem reports for it: zero on some, so a
/// read sized by it would otherwise answer with an empty file.
#[tokio::test]
async fn read_file_bytes_refuses_a_directory() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-read-directory-");
        let path = dir.child("directory");
        std::fs::create_dir(&path).expect("create directory");
        assert!(driver.read_file_bytes(&path).await.is_err());
    }
}

/// The whole-file operations hold a pool thread and the whole file for their duration, so they
/// are for small files and say so. A caller that reaches for the wrong one finds out at the
/// call rather than by occupying one of at most 32 threads for a multi-gigabyte transfer.
#[tokio::test]
async fn whole_file_operations_reject_a_large_file() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-whole-file-limit-");

        let path = dir.child("too-big");
        let oversized = Bytes::from(vec![0u8; lore_io::WHOLE_FILE_LIMIT + 1]);
        let error = driver
            .write_file_bytes(&path, oversized, false)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        std::fs::write(&path, vec![7u8; lore_io::WHOLE_FILE_LIMIT + 1]).expect("seed large file");
        let error = driver.read_file_bytes(&path).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}

/// The size check runs before the file is opened, so a rejected write cannot have truncated
/// what was already there.
#[tokio::test]
async fn a_rejected_whole_file_write_leaves_the_file_intact() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-whole-file-intact-");
        let path = dir.child("existing");
        let data = Bytes::from(pattern(4096, 41));
        driver
            .write_file_bytes(&path, data.clone(), false)
            .await
            .unwrap();

        let oversized = Bytes::from(vec![0u8; lore_io::WHOLE_FILE_LIMIT + 1]);
        driver
            .write_file_bytes(&path, oversized, false)
            .await
            .expect_err("oversized write must be rejected");

        assert_eq!(driver.read_file_bytes(&path).await.unwrap(), data);
    }
}

#[tokio::test]
async fn rename_remove_and_create_dir_all() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-fsops-");
        let nested = dir.child("a/b/c");
        driver.create_dir_all(&nested).await.unwrap();
        let from = nested.join("from");
        let to = nested.join("to");
        driver
            .write_file_bytes(&from, Bytes::from_static(b"move me"), false)
            .await
            .unwrap();
        driver.rename(&from, &to).await.unwrap();
        assert_eq!(driver.metadata(&to).await.unwrap().len(), 7);
        assert!(driver.metadata(&from).await.is_err());
        driver.remove_file(&to).await.unwrap();
        assert!(driver.metadata(&to).await.is_err());
    }
}

/// Recursive removal takes the whole tree, and reports `NotFound` for a path that is already
/// gone. Callers translate that status into success — a delete that finds nothing to delete has
/// done its job — so it is part of the operation's contract rather than an incidental errno.
#[tokio::test]
async fn remove_dir_all_takes_the_tree_and_reports_a_missing_one() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-removetree-");
        let root = dir.child("tree");
        let nested = root.join("a/b/c");
        driver.create_dir_all(&nested).await.unwrap();
        for name in ["one", "two"] {
            driver
                .write_file_bytes(nested.join(name), Bytes::from_static(b"leaf"), false)
                .await
                .unwrap();
        }

        driver.remove_dir_all(&root).await.unwrap();

        assert!(
            driver.metadata(&root).await.is_err(),
            "the tree must be gone"
        );

        let missing = driver
            .remove_dir_all(&root)
            .await
            .expect_err("removing a missing tree must report it");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
    }
}

/// Recursive removal around a boundary keeps the directory holding one, deep in the tree, with
/// all it holds and every directory leading to it, and removes the rest. A reserved entry deep in
/// the tree stays with every directory leading to it, and its siblings go. A tree holding no
/// boundary goes whole, a link is removed rather than followed, and a missing path is `Ok`.
#[tokio::test]
async fn remove_dir_all_keeping_keeps_each_boundary_and_its_ancestors() {
    fn reserved(name: &str) -> bool {
        name.eq_ignore_ascii_case(".lore") || name.eq_ignore_ascii_case(".urc")
    }
    for driver in drivers() {
        let dir = TempDir::new("lore-io-removetree-keeping-");
        let root = dir.child("tree");
        let kept = root.join("a/b/repository");
        driver.create_dir_all(kept.join(".lore")).await.unwrap();
        driver
            .create_dir_all(root.join("a/gone/deeper"))
            .await
            .unwrap();
        driver.create_dir_all(root.join("c")).await.unwrap();
        driver
            .create_dir_all(root.join("a/legacy/.urc"))
            .await
            .unwrap();
        for file in [
            "top.txt",
            "a/one.txt",
            "a/b/two.txt",
            "a/b/repository/work.txt",
            "a/b/repository/.lore/id",
            "a/gone/deeper/three.txt",
            "c/four.txt",
            "a/legacy/.urc/state",
            "a/legacy/beside.txt",
        ] {
            driver
                .write_file_bytes(root.join(file), Bytes::from_static(b"leaf"), false)
                .await
                .unwrap();
        }
        let outside = dir.child("outside");
        driver.create_dir_all(&outside).await.unwrap();
        driver
            .write_file_bytes(outside.join("held.txt"), Bytes::from_static(b"held"), false)
            .await
            .unwrap();
        #[cfg(target_family = "unix")]
        std::os::unix::fs::symlink(&outside, root.join("a/link")).unwrap();

        driver
            .remove_dir_all_keeping(&root, ".lore", reserved)
            .await
            .unwrap();

        for held in [
            "a/b/repository/work.txt",
            "a/b/repository/.lore/id",
            "a/legacy/.urc/state",
        ] {
            assert!(root.join(held).is_file(), "{held} must stay");
        }
        for gone in [
            "top.txt",
            "a/one.txt",
            "a/b/two.txt",
            "a/gone",
            "a/link",
            "a/legacy/beside.txt",
            "c",
        ] {
            assert!(
                std::fs::symlink_metadata(root.join(gone)).is_err(),
                "{gone} must be gone"
            );
        }
        assert!(
            outside.join("held.txt").is_file(),
            "a link must not be followed"
        );

        driver
            .remove_dir_all_keeping(root.join("a/b/repository"), ".lore", reserved)
            .await
            .unwrap();
        assert!(
            kept.join("work.txt").is_file(),
            "a boundary at the root must stay"
        );

        driver
            .remove_dir_all_keeping(&outside, ".lore", reserved)
            .await
            .unwrap();
        assert!(
            driver.metadata(&outside).await.is_err(),
            "a tree without a boundary must be gone"
        );
        driver
            .remove_dir_all_keeping(&outside, ".lore", reserved)
            .await
            .expect("removing a missing tree is Ok");
    }
}

/// A tree is renamed aside, then removed from there, so neither name is left, and a second call
/// finds nothing left to remove.
#[tokio::test]
async fn remove_dir_all_in_background_renames_the_tree_aside_and_removes_it() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-remove-in-background-");
        let path = dir.child("tree");
        let aside = dir.child("tree.aside");
        driver.create_dir_all(path.join("a/b")).await.unwrap();
        driver
            .write_file_bytes(
                path.join("a/b/file.txt"),
                Bytes::from_static(b"content"),
                false,
            )
            .await
            .unwrap();

        let (removed_from, removed) = driver
            .remove_dir_all_in_background(&path, Some(aside.clone()), false)
            .await;

        removed.expect("removes the tree");
        assert_eq!(removed_from, aside);
        assert!(
            driver.symlink_stat(&path).await.is_err(),
            "the path is free"
        );
        assert!(
            driver.symlink_stat(&aside).await.is_err(),
            "the tree is gone"
        );
        driver
            .remove_dir_all_in_background(&path, Some(aside.clone()), false)
            .await
            .1
            .expect("removing a missing tree is Ok");
    }
}

/// A listing yields every child with the metadata the walk resolved for it, and ends. The
/// entries arrive in whatever order the walk finds them, so the test compares them as a set.
#[tokio::test]
async fn read_dir_yields_every_child_with_metadata() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdir-");
        let root = dir.child("tree");
        driver.create_dir_all(root.join("child_dir")).await.unwrap();
        driver
            .write_file_bytes(root.join("child_file"), Bytes::from_static(b"four"), false)
            .await
            .unwrap();

        let mut listing = driver.read_dir(&root).await.unwrap();
        let mut found = Vec::new();
        while let Some(entry) = listing.next().await {
            let entry = entry.expect("an entry must be readable");
            let metadata = entry.metadata.expect("an entry must resolve its metadata");
            found.push((
                entry.file_name.to_string_lossy().to_string(),
                metadata.kind == StatKind::Directory,
                metadata.len,
            ));
        }
        found.sort();

        assert_eq!(found.len(), 2, "unexpected listing: {found:?}");
        assert_eq!(found[0].0, "child_dir");
        assert!(found[0].1, "the directory must report itself as one");
        assert_eq!(found[1].0, "child_file");
        assert!(
            !found[1].1,
            "the file must not report itself as a directory"
        );
        assert_eq!(found[1].2, 4);
    }
}

/// A verdict where the platform has a lookup to reach one with, and `None` where it has none:
/// macOS cannot say anything about a spelling short of the directory read the caller would
/// otherwise do, so every expectation below collapses to "cannot say" there.
fn verdict(held: bool) -> Option<bool> {
    cfg!(any(target_os = "linux", target_family = "windows")).then_some(held)
}

/// The verdict is about the spelling, not about the file: a name the filesystem holds answers
/// yes, and a name it does not hold answers no rather than going unanswered. A directory answers
/// as readily as a file, which matters because most of what a caller resolves a path through is
/// directories.
#[tokio::test]
async fn holds_name_exactly_answers_for_a_name_the_filesystem_holds() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-holdsname-");
        driver
            .write_file_bytes(dir.child("Rock.mesh"), Bytes::from_static(b"data"), false)
            .await
            .unwrap();
        driver.create_dir_all(dir.child("Meshes")).await.unwrap();

        assert_eq!(
            driver.holds_name_exactly(dir.child("Rock.mesh")).await,
            verdict(true)
        );
        assert_eq!(
            driver.holds_name_exactly(dir.child("Meshes")).await,
            verdict(true)
        );
        assert_eq!(
            driver.holds_name_exactly(dir.child("absent")).await,
            verdict(false)
        );
        assert_eq!(
            driver
                .holds_name_exactly(dir.child("absent").join("Rock.mesh"))
                .await,
            verdict(false),
            "a missing directory holds no children"
        );
    }
}

/// Two paths name one entry where they reach one file, symbolic links followed: the same path, a
/// link to it, and another spelling where the file system folds case. Another file, a path naming
/// nothing and a path through a file never do, and a first path naming nothing is answered as
/// naming nothing.
#[tokio::test]
async fn names_one_entry_answers_for_paths_reaching_one_file() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-namesoneentry-");
        for name in ["Rock.mesh", "Other.mesh"] {
            driver
                .write_file_bytes(dir.child(name), Bytes::from_static(b"data"), false)
                .await
                .unwrap();
        }
        driver.create_dir_all(dir.child("Meshes")).await.unwrap();
        let folds_case = driver.stat(dir.child("ROCK.MESH")).await.is_ok();
        let linked = file_link(&dir.child("Rock.mesh"), &dir.child("link"));

        let mut cases = vec![
            ("Rock.mesh", "Rock.mesh", Some(true)),
            ("Rock.mesh", "ROCK.MESH", Some(folds_case)),
            ("Meshes", "MESHES", Some(folds_case)),
            ("Rock.mesh", "Other.mesh", Some(false)),
            ("Rock.mesh", "absent", Some(false)),
            ("Rock.mesh", "Other.mesh/child", Some(false)),
            ("absent", "Rock.mesh", None),
            ("Other.mesh/child", "Rock.mesh", None),
        ];
        if linked {
            cases.push(("Rock.mesh", "link", Some(true)));
        }
        for (from, to, one) in cases {
            assert_eq!(
                driver
                    .names_one_entry(dir.child(from), dir.child(to))
                    .await
                    .expect("an answer"),
                one,
                "{from} and {to}, {}",
                driver.backend_name()
            );
        }
    }
}

/// A name that cannot exist must never be answered for by some other file. Win32 hands the last
/// component to a matcher that reads `*` and `?`, and `<`, `>` and `"` as the legacy DOS
/// wildcards; none of the five is legal in a Windows filename, and `<` and `"` do match
/// `Rock.mesh` when passed through.
#[tokio::test]
async fn holds_name_exactly_refuses_a_name_that_could_be_read_as_a_pattern() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-holdsnamewildcard-");
        driver
            .write_file_bytes(dir.child("Rock.mesh"), Bytes::from_static(b"data"), false)
            .await
            .unwrap();

        for pattern in [
            "*.mesh",
            "Rock.mes?",
            "Rock<mesh",
            "Rock>mesh",
            "Rock.mesh\"",
        ] {
            assert_eq!(
                driver.holds_name_exactly(dir.child(pattern)).await,
                verdict(false),
                "{pattern} names no file"
            );
        }
    }
}

/// A path that arrives already verbatim carries a `?` in its prefix, and is the shape a caller
/// that has been anywhere near `MAX_PATH` hands over - `std::fs::canonicalize` returns one, and
/// the test fixtures in this workspace are built from those. Refusing it would answer no for
/// every path under such a root.

#[tokio::test]
#[cfg(target_family = "windows")]
async fn holds_name_exactly_answers_for_a_verbatim_path() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-holdsnameverbatim-");
        driver
            .write_file_bytes(dir.child("Rock.mesh"), Bytes::from_static(b"data"), false)
            .await
            .unwrap();

        let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", dir.path().display()));
        assert_eq!(
            driver.holds_name_exactly(verbatim.join("Rock.mesh")).await,
            verdict(true)
        );
        assert_eq!(
            driver.holds_name_exactly(verbatim.join("rock.mesh")).await,
            verdict(false)
        );
    }
}

/// Only the last component is matched as a pattern; the rest of the path is resolved literally,
/// so a pattern character in it names a directory that is not there rather than standing in for
/// one that is.
#[tokio::test]
async fn holds_name_exactly_does_not_match_a_pattern_in_a_parent() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-holdsnameparent-");
        driver.create_dir_all(dir.child("Meshes")).await.unwrap();
        driver
            .write_file_bytes(
                dir.child("Meshes").join("Rock.mesh"),
                Bytes::from_static(b"data"),
                false,
            )
            .await
            .unwrap();

        assert_eq!(
            driver
                .holds_name_exactly(dir.child("Mesh*s").join("Rock.mesh"))
                .await,
            verdict(false)
        );
    }
}

/// Past `MAX_PATH` the Win32 lookup wants the path rewritten with a verbatim prefix, which this
/// does not do, so it declines rather than answering wrongly - and the caller reads the directory,
/// which `std` does prefix. The file is there either way; declining says only that the lookup
/// could not be the one to say so, which is what `None` is for.
#[tokio::test]
#[cfg(target_family = "windows")]
async fn holds_name_exactly_declines_a_path_beyond_max_path() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-holdsnamelong-");
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 260 {
            deep = deep.join("directory-with-a-name-long-enough-to-get-there");
        }
        driver.create_dir_all(&deep).await.unwrap();
        let path = deep.join("Rock.mesh");
        driver
            .write_file_bytes(&path, Bytes::from_static(b"data"), false)
            .await
            .unwrap();

        assert!(
            path.exists(),
            "the file itself must be there to be declined"
        );
        assert_eq!(driver.holds_name_exactly(path).await, None);
    }
}

/// What `stat` answers for `path`, held against what `metadata` answers for it.
async fn stat_and_metadata(driver: &IoDriver, path: &std::path::Path) -> (Stat, Stat) {
    let stat = driver.stat(path).await.expect("stat");
    let metadata = driver.metadata(path).await.expect("metadata");
    (stat, Stat::from(&metadata))
}

/// A file and a directory stat as `metadata` describes them: the kind, the length, the
/// modification time and the permission bits.
#[tokio::test]
async fn stat_answers_as_metadata_does() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-stat-");
        driver
            .write_file_bytes(dir.child("file"), Bytes::from(pattern(5000, 1)), false)
            .await
            .unwrap();
        driver.create_dir_all(dir.child("directory")).await.unwrap();

        let (file, expected) = stat_and_metadata(&driver, &dir.child("file")).await;
        assert_eq!(file, expected, "{}", driver.backend_name());
        assert_eq!(file.kind, StatKind::File);
        assert_eq!(file.len, 5000);

        let (directory, expected) = stat_and_metadata(&driver, &dir.child("directory")).await;
        assert_eq!(directory, expected, "{}", driver.backend_name());
        assert_eq!(directory.kind, StatKind::Directory);
    }
}

/// A path holding nothing fails as `NotFound` with the OS code `metadata` reports, whether its
/// own name or a directory above it is what is missing.
#[tokio::test]
async fn stat_reports_a_missing_path_as_not_found() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-statmissing-");
        for path in [dir.child("absent"), dir.child("absent").join("child")] {
            let error = driver.stat(&path).await.expect_err("nothing is there");
            let expected = driver.metadata(&path).await.expect_err("nothing is there");
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::NotFound,
                "{} through {}",
                path.display(),
                driver.backend_name()
            );
            assert_eq!(
                error.raw_os_error(),
                expected.raw_os_error(),
                "{}",
                path.display()
            );
        }
    }
}

/// A file last written before the Unix epoch stats with the time `metadata` gives it. A
/// filesystem that refuses to hold such a time skips the case.
#[tokio::test]
async fn stat_answers_a_time_before_the_unix_epoch_as_metadata_does() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-statpreepoch-");
        let path = dir.child("old");
        driver
            .write_file_bytes(&path, Bytes::from_static(b"old"), false)
            .await
            .unwrap();
        let before_epoch =
            std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(400 * 86_400);
        let stamped = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(before_epoch));
        if let Err(error) = stamped {
            eprintln!("skipping: the filesystem holds no time before 1970: {error}");
            return;
        }

        let (stat, expected) = stat_and_metadata(&driver, &path).await;
        assert_eq!(stat, expected, "{}", driver.backend_name());
    }
}

/// A path holding a null names nothing a filesystem holds, and fails as `metadata` fails it.
#[tokio::test]
async fn stat_refuses_a_path_holding_a_null_as_metadata_does() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-statnull-");
        let path = dir.child("before\0after");
        let stat = driver.stat(&path).await.expect_err("a null names nothing");
        let metadata = driver
            .metadata(&path)
            .await
            .expect_err("a null names nothing");
        assert_eq!(stat.kind(), metadata.kind(), "{}", driver.backend_name());
    }
}

/// Creates a link at `link` to the file at `target`, answering whether the platform let it:
/// Windows creates one only with the privilege or developer mode to.
fn file_link(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(target_family = "unix")]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(target_family = "windows")]
    let created = std::os::windows::fs::symlink_file(target, link);
    created.is_ok()
}

/// A link stats as the file it names, and a link naming nothing fails as `NotFound`, as the
/// name does when nothing is there.
#[tokio::test]
async fn stat_follows_a_link() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-statlink-");
        driver
            .write_file_bytes(dir.child("target"), Bytes::from_static(b"held"), false)
            .await
            .unwrap();
        if !file_link(&dir.child("target"), &dir.child("link"))
            || !file_link(&dir.child("gone"), &dir.child("dangling"))
        {
            eprintln!("skipping: this machine creates no links");
            return;
        }

        let (link, expected) = stat_and_metadata(&driver, &dir.child("link")).await;
        assert_eq!(link, expected, "{}", driver.backend_name());
        assert_eq!(link.kind, StatKind::File);
        assert_eq!(link.len, 4);

        let error = driver
            .stat(dir.child("dangling"))
            .await
            .expect_err("a dangling link names nothing");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

/// A file and a directory are described by `symlink_stat` as `symlink_metadata` describes them,
/// and as `stat` describes them.
#[tokio::test]
async fn symlink_stat_answers_a_file_and_a_directory_as_stat_does() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-symlinkstat-");
        driver
            .write_file_bytes(dir.child("file"), Bytes::from(pattern(5000, 1)), false)
            .await
            .unwrap();
        driver.create_dir_all(dir.child("directory")).await.unwrap();

        for name in ["file", "directory"] {
            let path = dir.child(name);
            let described = driver.symlink_stat(&path).await.expect("symlink_stat");
            let expected = Stat::from(&std::fs::symlink_metadata(&path).expect("symlink_metadata"));
            assert_eq!(
                expected,
                described,
                "{name} through {}",
                driver.backend_name()
            );
            assert_eq!(
                driver.stat(&path).await.expect("stat"),
                described,
                "{name} through {}",
                driver.backend_name()
            );
        }
    }
}

/// A path holding nothing fails `symlink_stat` as `NotFound`, with the OS code
/// `symlink_metadata` reports.
#[tokio::test]
async fn symlink_stat_reports_a_missing_path_as_not_found() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-symlinkstatmissing-");
        let path = dir.child("absent");
        let error = driver
            .symlink_stat(&path)
            .await
            .expect_err("nothing is there");
        let expected = std::fs::symlink_metadata(&path).expect_err("nothing is there");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            error.raw_os_error(),
            expected.raw_os_error(),
            "{}",
            driver.backend_name()
        );
    }
}

/// A symbolic link is described as the name it is, not as what it names, and one naming nothing
/// is described too. On Unix a symbolic link is `Other`; on Windows one made to a directory is a
/// `Directory` and one made to a file a `File`, as their attributes give.
#[tokio::test]
async fn symlink_stat_describes_a_symbolic_link_itself() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-symlinkstatlink-");
        driver
            .write_file_bytes(dir.child("target"), Bytes::from_static(b"held"), false)
            .await
            .unwrap();
        driver.create_dir_all(dir.child("directory")).await.unwrap();
        if !file_link(&dir.child("target"), &dir.child("file-link"))
            || !directory_link(&dir.child("directory"), &dir.child("directory-link"))
            || !file_link(&dir.child("gone"), &dir.child("dangling"))
        {
            eprintln!("skipping: this machine creates no links");
            return;
        }

        let expected = if cfg!(target_family = "windows") {
            [
                ("file-link", StatKind::File),
                ("directory-link", StatKind::Directory),
                ("dangling", StatKind::File),
            ]
        } else {
            [
                ("file-link", StatKind::Other),
                ("directory-link", StatKind::Other),
                ("dangling", StatKind::Other),
            ]
        };
        for (name, kind) in expected {
            let described = driver
                .symlink_stat(dir.child(name))
                .await
                .expect("the symbolic link is there");
            assert_eq!(
                kind,
                described.kind,
                "{name} through {}",
                driver.backend_name()
            );
        }
    }
}

/// A junction, which Windows creates without a privilege, is described as the directory its
/// attributes say it is made as.
#[tokio::test]
#[cfg(target_family = "windows")]
async fn symlink_stat_describes_a_junction_as_a_directory() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-symlinkstatjunction-");
        driver.create_dir_all(dir.child("directory")).await.unwrap();
        junction(&dir.child("junction"), &dir.child("directory"));

        let described = driver
            .symlink_stat(dir.child("junction"))
            .await
            .expect("the junction is there");
        assert_eq!(
            StatKind::Directory,
            described.kind,
            "{}",
            driver.backend_name()
        );
    }
}

/// A directory is made writable by its owner, its group and other bits left as they are, and a
/// symbolic link to a directory is refused, the directory it stands for left as it is.
#[tokio::test]
#[cfg(target_family = "unix")]
async fn make_owner_writable_adds_the_owners_write_and_refuses_a_symbolic_link() {
    use std::os::unix::fs::PermissionsExt;

    for driver in drivers() {
        let dir = TempDir::new("lore-io-ownerwritable-");
        let directory = dir.child("directory");
        let target = dir.child("target");
        let link = dir.child("link");
        driver.create_dir_all(&directory).await.unwrap();
        driver.create_dir_all(&target).await.unwrap();
        std::os::unix::fs::symlink(&target, &link).expect("create the link");
        for path in [&directory, &target] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555))
                .expect("set the directory's mode");
        }
        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("stat the directory")
                .permissions()
                .mode()
                & 0o777
        };

        driver
            .make_owner_writable(&directory)
            .await
            .expect("make the directory writable by its owner");
        driver
            .make_owner_writable(&link)
            .await
            .expect_err("a symbolic link is refused");

        assert_eq!(0o755, mode(&directory), "{}", driver.backend_name());
        assert_eq!(0o555, mode(&target), "{}", driver.backend_name());
    }
}

/// Making a junction writable clears the junction's own read-only attribute and leaves the
/// read-only directory it stands for as it is.
#[tokio::test]
#[cfg(target_family = "windows")]
async fn make_owner_writable_clears_a_junctions_own_attribute_and_leaves_its_target() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-clearreadonly-");
        let target = dir.child("directory");
        let link = dir.child("junction");
        driver.create_dir_all(&target).await.unwrap();
        junction(&link, &target);
        mark_read_only_itself(&link);
        let mut permissions = std::fs::metadata(&target)
            .expect("stat the directory")
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&target, permissions).expect("make the directory read-only");
        let readonly_itself = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .expect("read the name itself")
                .permissions()
                .readonly()
        };
        assert!(readonly_itself(&link), "the junction is read-only itself");

        driver
            .make_owner_writable(&link)
            .await
            .expect("clear the junction's attribute");

        assert!(
            !readonly_itself(&link),
            "the junction's own attribute is clear through {}",
            driver.backend_name()
        );
        assert!(
            std::fs::metadata(&target)
                .expect("stat the directory")
                .permissions()
                .readonly(),
            "the directory stays read-only through {}",
            driver.backend_name()
        );
    }
}

/// Creates a junction at `link` to the directory `target`, which Windows does without a privilege.
#[cfg(target_family = "windows")]
fn junction(link: &std::path::Path, target: &std::path::Path) {
    let created = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("run mklink");
    assert!(created.status.success(), "mklink /J failed: {created:?}");
}

/// Sets the read-only attribute of the name `path` itself, a junction's on the junction.
#[cfg(target_family = "windows")]
fn mark_read_only_itself(path: &std::path::Path) {
    let marked = std::process::Command::new("cmd")
        .args(["/C", "attrib", "+r"])
        .arg(path)
        .arg("/L")
        .output()
        .expect("run attrib");
    assert!(marked.status.success(), "attrib +r /L failed: {marked:?}");
}

/// Past `MAX_PATH` a path answers whether or not it carries the verbatim prefix: a plain one is
/// left to `metadata`, which prefixes it, and a verbatim one is queried as it stands.
#[tokio::test]
#[cfg(target_family = "windows")]
async fn stat_answers_for_a_path_beyond_max_path() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-statlong-");
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 260 {
            deep = deep.join("directory-with-a-name-long-enough-to-get-there");
        }
        driver.create_dir_all(&deep).await.unwrap();
        let path = deep.join("Rock.mesh");
        driver
            .write_file_bytes(&path, Bytes::from_static(b"data"), false)
            .await
            .unwrap();
        let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", path.display()));

        for path in [path, verbatim] {
            let (stat, expected) = stat_and_metadata(&driver, &path).await;
            assert_eq!(stat, expected, "{}", path.display());
            assert_eq!(stat.kind, StatKind::File);
        }
    }
}

/// Permissions round-trip through the driver: what `metadata` reports is what `set_permissions`
/// accepts back, which is the whole shape callers use since none of them build a mode by hand.
#[tokio::test]
async fn set_permissions_marks_a_file_read_only() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-permissions-");
        let path = dir.child("guarded");
        driver
            .write_file_bytes(&path, Bytes::from_static(b"contents"), false)
            .await
            .unwrap();

        let mut permissions = driver.metadata(&path).await.unwrap().permissions();
        permissions.set_readonly(true);
        driver.set_permissions(&path, permissions).await.unwrap();
        assert!(
            driver
                .metadata(&path)
                .await
                .unwrap()
                .permissions()
                .readonly()
        );

        let mut permissions = driver.metadata(&path).await.unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        driver.set_permissions(&path, permissions).await.unwrap();
        assert!(
            !driver
                .metadata(&path)
                .await
                .unwrap()
                .permissions()
                .readonly(),
            "the file must be writable again for the directory to clean up"
        );
    }
}

/// A directory wider than one chunk yields every child exactly once. The stream refills as the
/// consumer drains it, so this is what says a name is neither dropped nor repeated at the seam
/// between chunks — including the case where the walk ends exactly on one.
#[tokio::test]
async fn read_dir_spans_chunk_boundaries() {
    for driver in drivers() {
        // Straddles the chunk the listing resolves per dispatch: one short of it, exactly it,
        // one past, and a count needing several refills with the last one partial.
        for count in [1usize, 255, 256, 257, 600] {
            let dir = TempDir::new("lore-io-readdir-chunks-");
            let root = dir.child(&format!("wide{count}"));
            driver.create_dir_all(&root).await.unwrap();
            for index in 0..count {
                driver
                    .write_file_bytes(root.join(format!("child{index:04}")), Bytes::new(), false)
                    .await
                    .unwrap();
            }

            let mut listing = driver.read_dir(&root).await.unwrap();
            let mut names = Vec::new();
            while let Some(entry) = listing.next().await {
                names.push(entry.unwrap().file_name.to_string_lossy().to_string());
            }
            names.sort();
            names.dedup();

            assert_eq!(names.len(), count, "{count} children yielded {names:?}");
            assert_eq!(names[0], "child0000");
        }
    }
}

/// An empty directory ends immediately rather than hanging: the walk marks the stream finished
/// even when it produced nothing, which a consumer parked on the first entry depends on.
#[tokio::test]
async fn read_dir_on_an_empty_directory_ends() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdir-empty-");
        let root = dir.child("empty");
        driver.create_dir_all(&root).await.unwrap();

        let mut listing = driver.read_dir(&root).await.unwrap();
        assert!(listing.next().await.is_none(), "an empty listing yielded");
    }
}

/// A directory that cannot be read fails at the call rather than arriving as an empty listing,
/// which is the distinction a caller deciding whether children exist depends on.
#[tokio::test]
async fn read_dir_reports_a_missing_directory() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdir-missing-");
        let error = driver
            .read_dir(dir.child("absent"))
            .await
            .err()
            .expect("listing a missing directory must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

/// Listing a file fails with the OS error `std` reports for it.
#[tokio::test]
async fn read_dir_of_a_file_fails_as_std_fails() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdir-file-");
        let file = dir.child("file");
        std::fs::write(&file, b"held").expect("write the file");

        let error = driver
            .read_dir(&file)
            .await
            .err()
            .expect("listing a file must fail");
        let expected = std::fs::read_dir(&file).expect_err("listing a file must fail");
        assert_eq!(
            expected.raw_os_error(),
            error.raw_os_error(),
            "{}",
            driver.backend_name()
        );
    }
}

/// Every name that is no symbolic link carries what `std`'s own listing describes it with.
#[tokio::test]
async fn read_dir_describes_each_entry_as_std_lists_it() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdir-std-");
        let root = dir.child("tree");
        std::fs::create_dir_all(root.join("child_dir")).expect("create the directory");
        std::fs::write(root.join("child_file"), b"four").expect("write the file");
        std::fs::write(root.join("empty"), b"").expect("write the empty file");
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("child_file"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("mark the file executable");
        }

        let mut expected = std::fs::read_dir(&root)
            .expect("std lists the directory")
            .map(|entry| {
                let entry = entry.expect("std reads the entry");
                let metadata = entry.metadata().expect("std describes the entry");
                (entry.file_name(), Some(Stat::from(&metadata)), false)
            })
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.0.cmp(&right.0));

        let mut listing = driver.read_dir(&root).await.unwrap();
        let mut found = Vec::new();
        while let Some(entry) = listing.next().await {
            let entry = entry.expect("an entry must be readable");
            found.push((entry.file_name, entry.metadata, entry.is_symlink));
        }
        found.sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(expected, found, "{}", driver.backend_name());
    }
}

/// Creates a symbolic link at `link` to the directory at `target`, answering whether the platform
/// let it.
fn directory_link(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(target_family = "unix")]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(target_family = "windows")]
    let created = std::os::windows::fs::symlink_dir(target, link);
    created.is_ok()
}

/// A symbolic link is listed as one, described by what it names, and a symbolic link naming
/// nothing carries no metadata.
#[tokio::test]
async fn read_dir_follows_a_symbolic_link_and_marks_it() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdir-link-");
        let root = dir.child("tree");
        std::fs::create_dir_all(root.join("directory")).expect("create the directory");
        std::fs::write(root.join("file"), b"held").expect("write the file");
        if !file_link(&root.join("file"), &root.join("file-link"))
            || !directory_link(&root.join("directory"), &root.join("directory-link"))
            || !file_link(&root.join("gone"), &root.join("dangling"))
        {
            eprintln!("skipping: this machine creates no links");
            return;
        }

        let mut listing = driver.read_dir(&root).await.unwrap();
        let mut links = Vec::new();
        while let Some(entry) = listing.next().await {
            let entry = entry.expect("an entry must be readable");
            if entry.is_symlink {
                links.push((
                    entry.file_name.to_string_lossy().to_string(),
                    entry.metadata,
                ));
            }
        }
        links.sort_by(|left, right| left.0.cmp(&right.0));

        let file = driver.stat(root.join("file")).await.unwrap();
        let directory = driver.stat(root.join("directory")).await.unwrap();
        assert_eq!(
            vec![
                ("dangling".to_string(), None),
                ("directory-link".to_string(), Some(directory)),
                ("file-link".to_string(), Some(file)),
            ],
            links,
            "{}",
            driver.backend_name()
        );
    }
}

/// A junction, which Windows creates without a privilege, is marked as a symbolic link as `std`
/// marks it, and is described by the directory it stands for, as `IoDriver::stat` describes it.
#[tokio::test]
#[cfg(target_family = "windows")]
async fn read_dir_follows_a_junction_and_marks_it_as_std_does() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdir-junction-");
        let root = dir.child("tree");
        std::fs::create_dir_all(root.join("directory")).expect("create the directory");
        std::fs::write(root.join("directory").join("file"), b"held").expect("write the file");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("junction"))
            .arg(root.join("directory"))
            .output()
            .expect("run mklink");
        assert!(created.status.success(), "mklink /J failed: {created:?}");

        let mut expected = std::fs::read_dir(&root)
            .expect("std lists the directory")
            .map(|entry| {
                let entry = entry.expect("std reads the entry");
                let file_type = entry.file_type().expect("std types the entry");
                (entry.file_name(), file_type.is_symlink())
            })
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.0.cmp(&right.0));

        let mut listing = driver.read_dir(&root).await.unwrap();
        let mut marks = Vec::new();
        let mut junction = None;
        while let Some(entry) = listing.next().await {
            let entry = entry.expect("an entry must be readable");
            if entry.file_name == "junction" {
                junction = Some(entry.metadata);
            }
            marks.push((entry.file_name, entry.is_symlink));
        }
        marks.sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(
            vec![
                (std::ffi::OsString::from("directory"), false),
                (std::ffi::OsString::from("junction"), true),
            ],
            expected,
            "std marks the junction as a symbolic link"
        );
        assert_eq!(expected, marks, "{}", driver.backend_name());
        let stat = driver.stat(root.join("junction")).await.unwrap();
        assert_eq!(StatKind::Directory, stat.kind);
        assert_eq!(Some(Some(stat)), junction, "{}", driver.backend_name());
    }
}

/// Past `MAX_PATH` a directory lists whether or not its path carries the verbatim prefix.
#[tokio::test]
#[cfg(target_family = "windows")]
async fn read_dir_lists_a_directory_beyond_max_path() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-readdirlong-");
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 260 {
            deep = deep.join("directory-with-a-name-long-enough-to-get-there");
        }
        driver.create_dir_all(&deep).await.unwrap();
        std::fs::write(deep.join("Rock.mesh"), b"data").expect("write the file");
        let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", deep.display()));

        for path in [deep, verbatim] {
            let mut listing = driver.read_dir(&path).await.unwrap();
            let mut found = Vec::new();
            while let Some(entry) = listing.next().await {
                let entry = entry.expect("an entry must be readable");
                let metadata = entry.metadata.expect("an entry must resolve its metadata");
                found.push((entry.file_name, metadata.kind, metadata.len));
            }
            assert_eq!(
                vec![(std::ffi::OsString::from("Rock.mesh"), StatKind::File, 4)],
                found,
                "{}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn concurrent_disjoint_writes_share_one_handle() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-concurrent-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let region = 64 * 1024usize;
        let regions = 16;

        let writes = (0..regions).map(|i| {
            let file = file.clone();
            async move {
                let data = pattern(region, i as u8);
                file.write_all_at(Bytes::from(data.clone()), (i * region) as u64)
                    .await
                    .unwrap();
            }
        });
        join_all(writes).await;
        file.sync_data().await.unwrap();

        for i in 0..regions {
            let read = file
                .read_exact_at(region, (i * region) as u64)
                .await
                .unwrap();
            assert_eq!(&read[..], &pattern(region, i as u8)[..]);
        }
    }
}

/// The completion of a resize is dropped once the length is set, whether or not the call is still
/// awaited, as an atomic write's is. Observed through a completion that reads the file's length as
/// it is dropped.
#[tokio::test]
async fn the_completion_of_a_resize_is_dropped_once_the_length_is_set() {
    struct Sized {
        path: std::path::PathBuf,
        len: std::sync::mpsc::Sender<Option<u64>>,
    }

    impl Drop for Sized {
        fn drop(&mut self) {
            let _ = self
                .len
                .send(std::fs::metadata(&self.path).ok().map(|meta| meta.len()));
        }
    }

    for driver in drivers() {
        for awaited in [true, false] {
            let dir = TempDir::new("lore-io-resize-completion-");
            let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
            let (len, sized) = std::sync::mpsc::channel();
            let resize = file.set_len(
                4096,
                Sized {
                    path: dir.child("data"),
                    len,
                },
            );
            if awaited {
                resize.await.unwrap();
            } else {
                // Polled once, which dispatches the resize, and dropped.
                let _ = tokio::time::timeout(std::time::Duration::ZERO, resize).await;
            }
            let len = sized
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the completion is dropped");
            assert_eq!(
                len,
                Some(4096),
                "{} awaited {awaited}: the completion was dropped before the length was set",
                driver.backend_name()
            );
        }
    }
}

#[tokio::test]
async fn set_len_resizes_the_file() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-sizing-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();

        file.set_len(4096, ()).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 4096);

        // Extending leaves a hole that reads as zeros rather than a short file.
        file.set_len(2 * 1024 * 1024, ()).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 2 * 1024 * 1024);
        let tail = file
            .read_exact_at(4096, 2 * 1024 * 1024 - 4096)
            .await
            .unwrap();
        assert_eq!(&tail[..], &[0u8; 4096][..]);

        file.set_len(1024, ()).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 1024);
        file.sync_all().await.unwrap();
    }
}

#[tokio::test]
async fn open_missing_file_fails_not_found() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-missing-");
        let error = driver
            .open(dir.child("absent"), &OpenOptions::new().read(true))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

/// The crate's futures suspend on wakers alone, so they run under any executor. Every other test
/// here is driven by tokio, which cannot show that; this one uses `futures::executor::block_on`
/// with no tokio runtime present at all.
///
/// This is the property the completion backends rest on: a reaper thread parked in the kernel
/// wakes a plain waker, with no runtime to be affiliated with. Cancellation is included because
/// abandoning a future must free the job and its buffer under any executor, not only where a
/// scheduler drives the drop.
#[test]
fn operations_complete_under_a_foreign_executor() {
    for driver in drivers() {
        futures::executor::block_on(async {
            let dir = TempDir::new("lore-io-foreign-executor-");
            let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
            let data = pattern(64 * 1024, 47);

            file.write_all_at(Bytes::from(data.clone()), 0)
                .await
                .unwrap();
            let read = file.read_exact_at(data.len(), 0).await.unwrap();
            assert_eq!(&read[..], &data[..]);
            file.sync_data().await.unwrap();
            assert_eq!(file.metadata().await.unwrap().len(), data.len() as u64);

            let path = dir.child("blob");
            driver
                .write_file_bytes(&path, Bytes::from(data.clone()), false)
                .await
                .unwrap();
            assert_eq!(
                driver.read_file_bytes(&path).await.unwrap(),
                Bytes::from(data.clone())
            );

            let mut cancelled = Box::pin(file.read_exact_at(data.len(), 0));
            let _ = futures::poll!(&mut cancelled);
            drop(cancelled);

            let after = file.read_exact_at(data.len(), 0).await.unwrap();
            assert_eq!(
                &after[..],
                &data[..],
                "the pool stopped serving after a cancellation"
            );
        });
    }
}

/// Cancelling abandons the result, not the operation. A read's buffer is allocated inside the job
/// and freed there, so a dropped future cannot free memory the kernel is still writing into —
/// there is no caller-held buffer to free early. A write's buffer travels into the job the same
/// way, and the file holding the bytes afterwards is what shows the operation ran to completion
/// with the future already gone.
#[tokio::test]
async fn a_cancelled_write_still_completes() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-cancel-write-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = pattern(256 * 1024, 29);

        let mut write = Box::pin(file.write_all_at(Bytes::from(data.clone()), 0));
        let _ = futures::poll!(&mut write);
        drop(write);

        // The write owns its buffer until the job ends, so the bytes land whether or not anyone
        // is still waiting for the result.
        for _ in 0..200 {
            if file.metadata().await.unwrap().len() == data.len() as u64 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let written = file.read_exact_at(data.len(), 0).await.unwrap();
        assert_eq!(&written[..], &data[..]);
    }
}

/// A cancelled operation must not take the pool with it: the next operation on the same handle
/// completes normally.
#[tokio::test]
async fn the_pool_still_serves_work_after_a_cancellation() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-cancel-then-work-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = pattern(64 * 1024, 31);
        file.write_all_at(Bytes::from(data.clone()), 0)
            .await
            .unwrap();

        for _ in 0..64 {
            let mut read = Box::pin(file.read_exact_at(64 * 1024, 0));
            let _ = futures::poll!(&mut read);
            drop(read);
        }

        let read = file.read_exact_at(64 * 1024, 0).await.unwrap();
        assert_eq!(&read[..], &data[..]);
    }
}

#[tokio::test]
async fn open_read_head_covers_small_files() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-head-small-");
        let data = pattern(100, 11);
        driver
            .write_file_bytes(dir.child("data"), Bytes::copy_from_slice(&data), false)
            .await
            .unwrap();
        let (_file, metadata, head) = driver
            .open_read_head(dir.child("data"), &OpenOptions::new().read(true), 4096)
            .await
            .unwrap();
        assert_eq!(metadata.len(), 100);
        assert_eq!(&head[..], &data[..]);
    }
}

#[tokio::test]
async fn open_read_head_serves_follow_up_reads() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-head-large-");
        let data = pattern(8192, 13);
        driver
            .write_file_bytes(dir.child("data"), Bytes::copy_from_slice(&data), false)
            .await
            .unwrap();
        let (file, metadata, head) = driver
            .open_read_head(dir.child("data"), &OpenOptions::new().read(true), 4096)
            .await
            .unwrap();
        assert_eq!(metadata.len(), 8192);
        assert_eq!(&head[..], &data[..4096]);
        let rest = file.read_exact_at(4096, 4096).await.unwrap();
        assert_eq!(&rest[..], &data[4096..]);
    }
}

#[tokio::test]
async fn open_read_head_missing_file_fails_not_found() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-head-missing-");
        let error = driver
            .open_read_head(dir.child("absent"), &OpenOptions::new().read(true), 4096)
            .await
            .expect_err("opening a missing file must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

#[tokio::test]
async fn write_file_segments_roundtrip() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-write-segments-");
        let segments: Vec<Vec<u8>> = vec![pattern(16, 21), pattern(4096, 22), pattern(33, 23)];
        let flat: Vec<u8> = segments.concat();
        driver
            .write_file_segments(
                dir.child("data"),
                &OpenOptions::new().write(true).create(true).truncate(true),
                segments,
                true,
                (),
            )
            .await
            .unwrap();
        let read = driver.read_file_bytes(dir.child("data")).await.unwrap();
        assert_eq!(&read[..], &flat[..]);
    }
}

#[tokio::test]
async fn write_file_segments_atomic_replaces_target() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-write-atomic-");
        let previous = pattern(64, 31);
        driver
            .write_file_bytes(dir.child("data"), Bytes::copy_from_slice(&previous), false)
            .await
            .unwrap();

        let segments: Vec<Vec<u8>> = vec![pattern(16, 32), pattern(2048, 33)];
        let flat: Vec<u8> = segments.concat();
        driver
            .write_file_segments_atomic(
                dir.child("data.tmp"),
                dir.child("data"),
                &OpenOptions::new().write(true).create(true).truncate(true),
                segments,
                true,
                (),
            )
            .await
            .unwrap();
        assert!(
            !dir.child("data.tmp").exists(),
            "temporary file must be renamed away"
        );
        let read = driver.read_file_bytes(dir.child("data")).await.unwrap();
        assert_eq!(&read[..], &flat[..]);
    }
}

/// A write over an existing longer file writes into that file, rather than replacing it, and leaves
/// it holding the new contents alone, with and without a sync.
#[tokio::test]
async fn write_file_segments_writes_over_a_file_in_place() {
    for driver in drivers() {
        for durable in [false, true] {
            let dir = TempDir::new("lore-io-write-in-place-");
            let previous = pattern(8192, 71);
            driver
                .write_file_bytes(dir.child("data"), Bytes::copy_from_slice(&previous), false)
                .await
                .unwrap();
            let identity = file_identity(&dir.child("data"));

            let segments: Vec<Vec<u8>> = vec![pattern(16, 72), pattern(2048, 73)];
            let flat: Vec<u8> = segments.concat();
            driver
                .write_file_segments(
                    dir.child("data"),
                    &OpenOptions::new().write(true).create(true).truncate(true),
                    segments,
                    durable,
                    (),
                )
                .await
                .unwrap();
            let read = driver.read_file_bytes(dir.child("data")).await.unwrap();
            assert_eq!(
                &read[..],
                &flat[..],
                "{} durable {durable}",
                driver.backend_name()
            );
            assert_eq!(
                file_identity(&dir.child("data")),
                identity,
                "{} durable {durable}: the write replaced the file",
                driver.backend_name()
            );
        }
    }
}

/// An atomic write that fails removes its temporary file, here one whose rename meets a directory.
#[tokio::test]
async fn a_failed_atomic_write_leaves_no_temporary_file() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-write-atomic-failed-");
        std::fs::create_dir_all(dir.child("data").join("blocker")).unwrap();

        let written = driver
            .write_file_segments_atomic(
                dir.child("data.tmp"),
                dir.child("data"),
                &OpenOptions::new().write(true).create(true).truncate(true),
                vec![pattern(64, 51)],
                false,
                (),
            )
            .await;

        assert!(
            written.is_err(),
            "{}: the rename succeeded",
            driver.backend_name()
        );
        assert!(
            !dir.child("data.tmp").exists(),
            "{}: the temporary file is left behind",
            driver.backend_name()
        );
        assert!(dir.child("data").join("blocker").is_dir());
    }
}

/// The completion is dropped once the write has landed, the rename of an atomic write included,
/// whether or not the call is still awaited, so a caller that drops the call part way learns from
/// it when the write it abandoned is on disk. Observed through a completion that reads the file as
/// it is dropped.
#[tokio::test]
async fn the_completion_is_dropped_once_the_write_has_landed() {
    struct Landed {
        path: std::path::PathBuf,
        read: std::sync::mpsc::Sender<Option<Vec<u8>>>,
    }

    impl Drop for Landed {
        fn drop(&mut self) {
            let _ = self.read.send(std::fs::read(&self.path).ok());
        }
    }

    for driver in drivers() {
        for (awaited, in_place) in [(true, false), (false, false), (true, true), (false, true)] {
            let dir = TempDir::new("lore-io-completion-");
            let data = pattern(4096, 61);
            let (read, landed) = std::sync::mpsc::channel();
            let options = OpenOptions::new().write(true).create(true).truncate(true);
            let completion = Landed {
                path: dir.child("data"),
                read,
            };
            let write: std::pin::Pin<
                Box<dyn std::future::Future<Output = std::io::Result<()>> + '_>,
            > = if in_place {
                Box::pin(driver.write_file_segments(
                    dir.child("data"),
                    &options,
                    vec![data.clone()],
                    true,
                    completion,
                ))
            } else {
                Box::pin(driver.write_file_segments_atomic(
                    dir.child("data.tmp"),
                    dir.child("data"),
                    &options,
                    vec![data.clone()],
                    true,
                    completion,
                ))
            };
            if awaited {
                write.await.unwrap();
            } else {
                // Polled once, which dispatches the write, and dropped.
                let _ = tokio::time::timeout(std::time::Duration::ZERO, write).await;
            }
            let read = landed
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the completion is dropped");
            assert_eq!(
                read.as_deref(),
                Some(&data[..]),
                "{} awaited {awaited} in place {in_place}: the completion was dropped before the \
                 write landed",
                driver.backend_name()
            );
        }
    }
}

/// The segments are released once the kernel has the bytes, so a caller holding a lock to keep
/// them stable is not held across the sync and the rename. Observed through a guard the segment
/// list owns: it has to be dropped by the time the call returns, and the file has to be complete.
#[tokio::test]
async fn segments_are_released_before_the_call_returns() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    struct Guarded {
        data: Vec<u8>,
        released: Arc<AtomicBool>,
    }

    impl Drop for Guarded {
        fn drop(&mut self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    impl lore_io::StableBufList for Guarded {
        fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
            std::iter::once(self.data.as_slice())
        }
    }

    for driver in drivers() {
        let dir = TempDir::new("lore-io-segments-released-");
        let data = pattern(4096, 41);
        let released = Arc::new(AtomicBool::new(false));

        driver
            .write_file_segments_atomic(
                dir.child("data.tmp"),
                dir.child("data"),
                &OpenOptions::new().write(true).create(true).truncate(true),
                Guarded {
                    data: data.clone(),
                    released: Arc::clone(&released),
                },
                true,
                (),
            )
            .await
            .unwrap();

        assert!(
            released.load(Ordering::SeqCst),
            "the segments must not outlive the call"
        );
        let read = driver.read_file_bytes(dir.child("data")).await.unwrap();
        assert_eq!(&read[..], &data[..]);
    }
}

#[tokio::test]
async fn vectored_write_read_roundtrip() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-vectored-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let segments: Vec<Vec<u8>> = vec![
            pattern(7, 1),
            pattern(4096, 2),
            pattern(64 * 1024 + 13, 3),
            pattern(1, 4),
            vec![],
            pattern(257, 5),
        ];
        let flat: Vec<u8> = segments.concat();
        let segments = file.write_all_vectored_at(segments, 5).await.unwrap();

        let read = file.read_exact_at(flat.len(), 5).await.unwrap();
        assert_eq!(&read[..], &flat[..]);

        let targets: Vec<Vec<u8>> = segments
            .iter()
            .map(|segment| vec![0u8; segment.len()])
            .collect();
        let targets = file.read_exact_vectored_at(targets, 5).await.unwrap();
        assert_eq!(targets, segments);
    }
}

#[tokio::test]
async fn vectored_read_past_eof_fails() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-vectored-eof-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        file.write_all_at(pattern(100, 9), 0).await.unwrap();

        let targets: Vec<Vec<u8>> = vec![vec![0u8; 64], vec![0u8; 136]];
        let error = file
            .read_exact_vectored_at(targets, 0)
            .await
            .expect_err("vectored read past EOF must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}

/// 1500 segments exceed the 1024-iovec syscall cap, so the operation
/// completes through the exact-loop continuation with a byte skip.
#[tokio::test]
async fn vectored_io_handles_many_segments() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-vectored-many-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let segments: Vec<Vec<u8>> = (0..1500).map(|i| pattern(9, i as u8)).collect();
        let flat: Vec<u8> = segments.concat();
        let segments = file.write_all_vectored_at(segments, 0).await.unwrap();

        let read = file.read_exact_at(flat.len(), 0).await.unwrap();
        assert_eq!(&read[..], &flat[..]);

        let targets: Vec<Vec<u8>> = segments
            .iter()
            .map(|segment| vec![0u8; segment.len()])
            .collect();
        let targets = file.read_exact_vectored_at(targets, 0).await.unwrap();
        assert_eq!(targets, segments);
    }
}

#[tokio::test]
async fn many_concurrent_reads_complete() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-stress-");
        let file = driver.open(dir.child("data"), &rw_create()).await.unwrap();
        let data = pattern(1024 * 1024, 17);
        file.write_all_at(Bytes::from(data.clone()), 0)
            .await
            .unwrap();

        let reads = (0..256).map(|i| {
            let file = file.clone();
            let expected = data[(i % 16) * 64 * 1024..][..64 * 1024].to_vec();
            async move {
                let read = file
                    .read_exact_at(64 * 1024, ((i % 16) * 64 * 1024) as u64)
                    .await
                    .unwrap();
                assert_eq!(&read[..], &expected[..]);
            }
        });
        join_all(reads).await;
    }
}

/// The default shares as `std` does, so a handle open for reading admits both another reader and
/// a writer. Most files opened here are files Lore does not own, and Windows refuses an open whose
/// share mode omits a right an existing handle holds — a narrower default would fail an ordinary
/// read of a working-tree file whenever an editor or a build tool had it open.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_read_handle_admits_readers_and_writers() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-sharemode-");
        let path = dir.child("shared");
        driver
            .write_file_bytes(&path, Bytes::from_static(b"shared content"), false)
            .await
            .unwrap();

        let _reader = driver
            .open(&path, &OpenOptions::new().read(true))
            .await
            .expect("opening the file for reading");

        driver
            .open(&path, &OpenOptions::new().read(true))
            .await
            .expect("a second reader shares with the first");

        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("a writer shares with a reader");
    }
}

/// Exclusion is the call site's to claim, which is what the pack store's opens do: with the share
/// mode narrowed to reads, a writer is refused for as long as the handle is open.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_narrowed_share_mode_refuses_writers() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-sharemode-narrow-");
        let path = dir.child("guarded");
        driver
            .write_file_bytes(&path, Bytes::from_static(b"guarded content"), false)
            .await
            .unwrap();

        let _owner = driver
            .open(
                &path,
                &OpenOptions::new()
                    .read(true)
                    .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ),
            )
            .await
            .expect("opening the file with reads shared");

        driver
            .open(&path, &OpenOptions::new().read(true))
            .await
            .expect("a reader is still admitted");

        let refused = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect_err("a writer must not open a file claimed for reads only");
        assert_eq!(
            refused.raw_os_error(),
            Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32),
            "expected a sharing violation, got: {refused}"
        );
    }
}

/// Readers share deletion, which is what lets the storage layer replace a file by rename while
/// one is open. The reader goes on reading the bytes it opened, which is the whole reason the
/// replace is safe to allow.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_read_handle_permits_replace_by_rename() {
    for driver in drivers() {
        let dir = TempDir::new("lore-io-sharerename-");
        let path = dir.child("replaced");
        let temporary = dir.child("replaced.tmp");
        let original = Bytes::from_static(b"the bytes the reader opened");
        driver
            .write_file_bytes(&path, original.clone(), false)
            .await
            .unwrap();
        driver
            .write_file_bytes(&temporary, Bytes::from_static(b"the replacement"), false)
            .await
            .unwrap();

        let reader = driver
            .open(&path, &OpenOptions::new().read(true))
            .await
            .expect("opening the file for reading");

        driver
            .rename(&temporary, &path)
            .await
            .expect("replacing a file that is being read");

        let read = reader
            .read_exact_at(original.len(), 0)
            .await
            .expect("reading through the handle after the replace");
        assert_eq!(read, original, "the reader must keep the bytes it opened");
    }
}
