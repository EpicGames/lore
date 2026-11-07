// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The read-write mount's allocation assertion, in a process of its own: allocation counting is
//! a process-wide switch, and a test in a shared process switching it off during this test's
//! measurement would let a WRITE that allocates pass.
#![cfg(target_os = "linux")]

use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_fuse::abi::Opcode;
use lore_fuse::mount::unmount_path;
use lore_fuse::skip_without_fuse;

#[path = "support/memory_fs.rs"]
mod memory_fs;

use memory_fs::MemoryFs;
use memory_fs::mount_read_write;

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

/// Restores allocation counting to disabled on drop, including on test panic/unwind.
struct DisableCountingOnDrop;

impl Drop for DisableCountingOnDrop {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}

/// The single-worker, `current_thread` harness of `protocol_bench.rs`: the session's only worker
/// runs every request on this test's thread, whose allocation counter `dispatch` samples.
/// System calls run on the blocking pool of another runtime.
#[tokio::test(flavor = "current_thread")]
async fn write_dispatch_allocates_nothing_after_warmup() {
    skip_without_fuse!();

    const CHUNK: usize = 64 << 10;
    const CHUNKS: u64 = 8;

    let dir = TempDir::new("lore-fuse-read-write-alloc-test-");
    let guard = PathUnmountGuard(dir.path().to_path_buf());
    let fs = MemoryFs::new();
    let (_mount, session) = mount_read_write(dir.path(), Arc::clone(&fs), 1).await;

    let path = dir.path().join("w.bin");
    let file = lore_base::lore_spawn_blocking!(move || -> std::io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let chunk = vec![1u8; CHUNK];
        for index in 0..CHUNKS {
            file.write_all_at(&chunk, index * CHUNK as u64)?;
        }
        Ok(file)
    })
    .await
    .expect("blocking task must not panic")
    .expect("warmup writes");
    let writes_before = session.metrics().snapshot(Opcode::Write).count;

    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    // Closes the file on the blocking pool as well: a `close(2)` on this thread would wait on a
    // FLUSH only this thread can answer.
    let measured = lore_base::lore_spawn_blocking!(move || -> std::io::Result<()> {
        let chunk = vec![2u8; CHUNK];
        for index in 0..CHUNKS {
            file.write_all_at(&chunk, index * CHUNK as u64)?;
        }
        Ok(())
    })
    .await
    .expect("blocking task must not panic");
    let write = session.metrics().snapshot(Opcode::Write);
    drop(restore);

    measured.expect("measured writes");
    assert!(
        write.count >= writes_before + CHUNKS,
        "every measured write must reach the daemon: {write:?}"
    );
    assert_eq!(
        write.max_allocations, 0,
        "WRITE must not allocate: {write:?}"
    );
    assert_eq!(
        fs.content(&[b"w.bin"]),
        Some(vec![2u8; CHUNK * CHUNKS as usize])
    );

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown must complete promptly");
    drop(guard);
}
