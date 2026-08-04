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
