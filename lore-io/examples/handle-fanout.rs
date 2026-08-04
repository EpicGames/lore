// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Diagnostic: does spreading cold reads across several handles to one file beat sharing one?
//!
//! The cold pool-cap sweep on Windows found `lore-io` needing 64 pool threads to reach a device
//! throughput that 16 plain threads with private handles already reach. Two candidates explain the
//! deficit at smaller caps: contention on the single shared handle, or per-operation latency in the
//! dispatch path. This holds the engine, the pool and the workload fixed and varies only the number
//! of handles the same reads are spread over, which separates them.
//!
//!   bench-fanout <file> <handles>...     e.g. handle-fanout data4k-loreio 1 8 64
//!
//! `LORE_IO_POOL_THREADS` sets the pool cap, as everywhere else.
#[allow(unused_extern_crates)]
extern crate lore_base;

use std::path::Path;
use std::time::Instant;

use futures::StreamExt;
use futures::stream;
use lore_io::BackendKind;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;

const READ_SIZE: usize = 4 * 1024;
const STRIDE: u64 = 128 * 1024;
const OPS: usize = 4096;
const CONCURRENCY: usize = 128;

/// Purges the file's cached pages so the reads reach the device, matching what the benchmark
/// harness does on Windows.
fn evict(path: &Path) {
    #[cfg(target_family = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        let _ = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path);
    }
    #[cfg(all(target_family = "unix", not(target_vendor = "apple")))]
    {
        use std::os::fd::AsRawFd;
        if let Ok(file) = std::fs::File::open(path) {
            let _ = file.sync_all();
            // Safety: Calling OS functions
            unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        }
    }
    // Apple platforms have no `posix_fadvise`; `msync(MS_INVALIDATE)` over a mapping of the file
    // is what invalidates its pages in the unified buffer cache. See `bench.rs`.
    #[cfg(all(target_family = "unix", target_vendor = "apple"))]
    {
        use std::os::fd::AsRawFd;
        if let Ok(file) = std::fs::File::open(path) {
            let _ = file.sync_all();
            let length = file
                .metadata()
                .map_or(0, |metadata| metadata.len() as usize);
            if length > 0 {
                // Safety: Calling OS functions
                unsafe {
                    let mapping = libc::mmap(
                        std::ptr::null_mut(),
                        length,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        file.as_raw_fd(),
                        0,
                    );
                    if mapping != libc::MAP_FAILED {
                        libc::msync(mapping, length, libc::MS_INVALIDATE);
                        libc::munmap(mapping, length);
                    }
                }
            }
        }
    }
}

fn offsets(slots: u64) -> Vec<u64> {
    let mut state: u64 = 0xc01d_cafe ^ slots;
    let mut indices: Vec<u64> = (0..slots).collect();
    for i in (1..indices.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        indices.swap(i, (state % (i as u64 + 1)) as usize);
    }
    indices.truncate(OPS);
    indices.into_iter().map(|index| index * STRIDE).collect()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = std::path::PathBuf::from(args.next().expect("usage: handle-fanout <file> <n>..."));
    let counts: Vec<usize> = args
        .map(|value| value.parse().expect("handle count"))
        .collect();
    let counts = if counts.is_empty() {
        vec![1, 8, 64]
    } else {
        counts
    };

    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");
    let size = std::fs::metadata(&path).expect("stat").len();
    let all = offsets(size / STRIDE);

    for handle_count in counts {
        evict(&path);
        let mut handles: Vec<IoFile> = Vec::with_capacity(handle_count);
        for _ in 0..handle_count {
            handles.push(
                driver
                    .open(&path, &OpenOptions::new().read(true))
                    .await
                    .expect("open"),
            );
        }

        let started = Instant::now();
        stream::iter(all.iter().copied().enumerate())
            .map(|(index, offset)| {
                let file = handles[index % handle_count].clone();
                async move {
                    file.read_exact_at(READ_SIZE, offset).await.expect("read");
                }
            })
            .buffer_unordered(CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let seconds = started.elapsed().as_secs_f64();
        let stats = lore_io::pool_stats();
        println!(
            "cold 4KiB strided  handles {handle_count:>4}  {:>10.0} ops/s   pool {}/{} queue peak {}",
            OPS as f64 / seconds,
            stats.threads,
            stats.max_threads,
            stats.queue_high_water
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
