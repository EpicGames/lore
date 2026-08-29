// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Backend conformance suite. Every backend must pass these tests with
//! identical observable semantics; new backends join the [`drivers`] list.
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::future::join_all;
use lore_io::BackendKind;
use lore_io::IoDriver;
use lore_io::OpenOptions;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "lore-io-test-{}-{}-{}",
            std::process::id(),
            name,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("failed to create temp dir");
        TempDir { path }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

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
        let dir = TempDir::new("roundtrip");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
        let data = pattern(192 * 1024, 7);
        file.write_all_at(Bytes::from(data.clone()), 0)
            .await
            .unwrap();
        let read = file.read_exact_at(data.len(), 0).await.unwrap();
        assert_eq!(&read[..], &data[..]);
    }
}

#[tokio::test]
async fn positional_reads_have_no_cursor() {
    for driver in drivers() {
        let dir = TempDir::new("positional");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("past-eof");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("partial-eof");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("read-len");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("exact-eof");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("extend");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("bytes");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("write-len");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();

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
        let dir = TempDir::new("durable");
        let data = Bytes::from(pattern(16 * 1024, 13));
        let path = dir.file("blob");
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
        let dir = TempDir::new("read-file");
        let data = Bytes::from(pattern(48 * 1024, 19));
        let path = dir.file("blob");
        driver
            .write_file_bytes(&path, data.clone(), false)
            .await
            .unwrap();
        let read = driver.read_file_bytes(&path).await.unwrap();
        assert_eq!(read, data);

        let empty = dir.file("empty");
        driver
            .write_file_bytes(&empty, Bytes::new(), false)
            .await
            .unwrap();
        assert!(driver.read_file_bytes(&empty).await.unwrap().is_empty());

        let error = driver
            .read_file_bytes(dir.file("absent"))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

/// The whole-file operations hold a pool thread and the whole file for their duration, so they
/// are for small files and say so. A caller that reaches for the wrong one finds out at the
/// call rather than by occupying one of at most 32 threads for a multi-gigabyte transfer.
#[tokio::test]
async fn whole_file_operations_reject_a_large_file() {
    for driver in drivers() {
        let dir = TempDir::new("whole-file-limit");

        let path = dir.file("too-big");
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
        let dir = TempDir::new("whole-file-intact");
        let path = dir.file("existing");
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
        let dir = TempDir::new("fsops");
        let nested = dir.file("a/b/c");
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
        let dir = TempDir::new("removetree");
        let root = dir.file("tree");
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

/// A listing yields every child with the metadata the walk resolved for it, and ends. The
/// entries arrive in whatever order the walk finds them, so the test compares them as a set.
#[tokio::test]
async fn read_dir_yields_every_child_with_metadata() {
    for driver in drivers() {
        let dir = TempDir::new("readdir");
        let root = dir.file("tree");
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
                metadata.is_dir(),
                metadata.len(),
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
        let dir = TempDir::new("holdsname");
        driver
            .write_file_bytes(dir.file("Rock.mesh"), Bytes::from_static(b"data"), false)
            .await
            .unwrap();
        driver.create_dir_all(dir.file("Meshes")).await.unwrap();

        assert_eq!(
            driver.holds_name_exactly(dir.file("Rock.mesh")).await,
            verdict(true)
        );
        assert_eq!(
            driver.holds_name_exactly(dir.file("Meshes")).await,
            verdict(true)
        );
        assert_eq!(
            driver.holds_name_exactly(dir.file("absent")).await,
            verdict(false)
        );
        assert_eq!(
            driver
                .holds_name_exactly(dir.file("absent").join("Rock.mesh"))
                .await,
            verdict(false),
            "a missing directory holds no children"
        );
    }
}

/// A name that cannot exist must never be answered for by some other file. Win32 hands the last
/// component to a matcher that reads `*` and `?`, and `<`, `>` and `"` as the legacy DOS
/// wildcards; none of the five is legal in a Windows filename, and `<` and `"` do match
/// `Rock.mesh` when passed through.
#[tokio::test]
async fn holds_name_exactly_refuses_a_name_that_could_be_read_as_a_pattern() {
    for driver in drivers() {
        let dir = TempDir::new("holdsnamewildcard");
        driver
            .write_file_bytes(dir.file("Rock.mesh"), Bytes::from_static(b"data"), false)
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
                driver.holds_name_exactly(dir.file(pattern)).await,
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
        let dir = TempDir::new("holdsnameverbatim");
        driver
            .write_file_bytes(dir.file("Rock.mesh"), Bytes::from_static(b"data"), false)
            .await
            .unwrap();

        let verbatim = PathBuf::from(format!(r"\\?\{}", dir.path.display()));
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
        let dir = TempDir::new("holdsnameparent");
        driver.create_dir_all(dir.file("Meshes")).await.unwrap();
        driver
            .write_file_bytes(
                dir.file("Meshes").join("Rock.mesh"),
                Bytes::from_static(b"data"),
                false,
            )
            .await
            .unwrap();

        assert_eq!(
            driver
                .holds_name_exactly(dir.file("Mesh*s").join("Rock.mesh"))
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
        let dir = TempDir::new("holdsnamelong");
        let mut deep = dir.path.clone();
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

/// Permissions round-trip through the driver: what `metadata` reports is what `set_permissions`
/// accepts back, which is the whole shape callers use since none of them build a mode by hand.
#[tokio::test]
async fn set_permissions_marks_a_file_read_only() {
    for driver in drivers() {
        let dir = TempDir::new("permissions");
        let path = dir.file("guarded");
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
            let dir = TempDir::new("readdir-chunks");
            let root = dir.file(&format!("wide{count}"));
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
        let dir = TempDir::new("readdir-empty");
        let root = dir.file("empty");
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
        let dir = TempDir::new("readdir-missing");
        let error = driver
            .read_dir(dir.file("absent"))
            .await
            .err()
            .expect("listing a missing directory must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

#[tokio::test]
async fn concurrent_disjoint_writes_share_one_handle() {
    for driver in drivers() {
        let dir = TempDir::new("concurrent");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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

#[tokio::test]
async fn set_len_resizes_the_file() {
    for driver in drivers() {
        let dir = TempDir::new("sizing");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();

        file.set_len(4096).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 4096);

        // Extending leaves a hole that reads as zeros rather than a short file.
        file.set_len(2 * 1024 * 1024).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 2 * 1024 * 1024);
        let tail = file
            .read_exact_at(4096, 2 * 1024 * 1024 - 4096)
            .await
            .unwrap();
        assert_eq!(&tail[..], &[0u8; 4096][..]);

        file.set_len(1024).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 1024);
        file.sync_all().await.unwrap();
    }
}

#[tokio::test]
async fn open_missing_file_fails_not_found() {
    for driver in drivers() {
        let dir = TempDir::new("missing");
        let error = driver
            .open(dir.file("absent"), &OpenOptions::new().read(true))
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
            let dir = TempDir::new("foreign-executor");
            let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
            let data = pattern(64 * 1024, 47);

            file.write_all_at(Bytes::from(data.clone()), 0)
                .await
                .unwrap();
            let read = file.read_exact_at(data.len(), 0).await.unwrap();
            assert_eq!(&read[..], &data[..]);
            file.sync_data().await.unwrap();
            assert_eq!(file.metadata().await.unwrap().len(), data.len() as u64);

            let path = dir.file("blob");
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
        let dir = TempDir::new("cancel-write");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("cancel-then-work");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("head-small");
        let data = pattern(100, 11);
        driver
            .write_file_bytes(dir.file("data"), Bytes::copy_from_slice(&data), false)
            .await
            .unwrap();
        let (_file, metadata, head) = driver
            .open_read_head(dir.file("data"), &OpenOptions::new().read(true), 4096)
            .await
            .unwrap();
        assert_eq!(metadata.len(), 100);
        assert_eq!(&head[..], &data[..]);
    }
}

#[tokio::test]
async fn open_read_head_serves_follow_up_reads() {
    for driver in drivers() {
        let dir = TempDir::new("head-large");
        let data = pattern(8192, 13);
        driver
            .write_file_bytes(dir.file("data"), Bytes::copy_from_slice(&data), false)
            .await
            .unwrap();
        let (file, metadata, head) = driver
            .open_read_head(dir.file("data"), &OpenOptions::new().read(true), 4096)
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
        let dir = TempDir::new("head-missing");
        let error = driver
            .open_read_head(dir.file("absent"), &OpenOptions::new().read(true), 4096)
            .await
            .expect_err("opening a missing file must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

#[tokio::test]
async fn write_file_segments_roundtrip() {
    for driver in drivers() {
        let dir = TempDir::new("write-segments");
        let segments: Vec<Vec<u8>> = vec![pattern(16, 21), pattern(4096, 22), pattern(33, 23)];
        let flat: Vec<u8> = segments.concat();
        driver
            .write_file_segments(
                dir.file("data"),
                &OpenOptions::new().write(true).create(true).truncate(true),
                segments,
                true,
            )
            .await
            .unwrap();
        let read = driver.read_file_bytes(dir.file("data")).await.unwrap();
        assert_eq!(&read[..], &flat[..]);
    }
}

#[tokio::test]
async fn write_file_segments_atomic_replaces_target() {
    for driver in drivers() {
        let dir = TempDir::new("write-atomic");
        let previous = pattern(64, 31);
        driver
            .write_file_bytes(dir.file("data"), Bytes::copy_from_slice(&previous), false)
            .await
            .unwrap();

        let segments: Vec<Vec<u8>> = vec![pattern(16, 32), pattern(2048, 33)];
        let flat: Vec<u8> = segments.concat();
        driver
            .write_file_segments_atomic(
                dir.file("data.tmp"),
                dir.file("data"),
                &OpenOptions::new().write(true).create(true).truncate(true),
                segments,
            )
            .await
            .unwrap();
        assert!(
            !dir.file("data.tmp").exists(),
            "temporary file must be renamed away"
        );
        let read = driver.read_file_bytes(dir.file("data")).await.unwrap();
        assert_eq!(&read[..], &flat[..]);
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
        let dir = TempDir::new("segments-released");
        let data = pattern(4096, 41);
        let released = Arc::new(AtomicBool::new(false));

        driver
            .write_file_segments_atomic(
                dir.file("data.tmp"),
                dir.file("data"),
                &OpenOptions::new().write(true).create(true).truncate(true),
                Guarded {
                    data: data.clone(),
                    released: Arc::clone(&released),
                },
            )
            .await
            .unwrap();

        assert!(
            released.load(Ordering::SeqCst),
            "the segments must not outlive the call"
        );
        let read = driver.read_file_bytes(dir.file("data")).await.unwrap();
        assert_eq!(&read[..], &data[..]);
    }
}

#[tokio::test]
async fn vectored_write_read_roundtrip() {
    for driver in drivers() {
        let dir = TempDir::new("vectored");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("vectored-eof");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("vectored-many");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("stress");
        let file = driver.open(dir.file("data"), &rw_create()).await.unwrap();
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
        let dir = TempDir::new("sharemode");
        let path = dir.file("shared");
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
        let dir = TempDir::new("sharemode-narrow");
        let path = dir.file("guarded");
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
        let dir = TempDir::new("sharerename");
        let path = dir.file("replaced");
        let temporary = dir.file("replaced.tmp");
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
