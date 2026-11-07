// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Protocol benchmarks and the zero-message-open measurement (spec: "Protocol layer",
//! "Performance targets"). The per-opcode zero-allocation assertion is `tests/protocol_bench.rs`.

use std::io::Read as _;
use std::path::PathBuf;
use std::time::Instant;

use lore_base::test_util::TempDir;
use lore_fuse::abi::Opcode;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::unmount_path;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;

use crate::synthetic::SynthNode;
use crate::synthetic::SyntheticFs;
use crate::synthetic::mount_synthetic;

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

fn session_config() -> SessionConfig {
    SessionConfig {
        workers: 1,
        init: InitConfig::default(),
    }
}

/// `#[ignore]`d benchmark (spec: "Performance targets"): 100k `stat`/`fstat` round trips over a
/// 10k-name directory, all served with `Timeouts::ZERO` (`SyntheticFs` never caches an entry or
/// its attributes, so every call is a fresh LOOKUP/GETATTR round trip, not a cache hit). Prints
/// LOOKUP and GETATTR p50/p99.
#[ignore = "benchmark: run explicitly, see the crate's docs/developing notes"]
#[tokio::test(flavor = "multi_thread")]
async fn bench_lookup_getattr_roundtrip_p50_p99() {
    skip_without_fuse!();

    const NAMES: usize = 10_000;
    const ITERATIONS: usize = 100_000;

    let dir = TempDir::new("lore-fuse-protocol-bench-lookup-test-");
    let entries: Vec<(String, SynthNode)> = (0..NAMES)
        .map(|i| {
            (
                format!("file-{i:05}"),
                SynthNode::File {
                    content: Vec::new(),
                    executable: false,
                },
            )
        })
        .chain(std::iter::once((
            "fstat-target".to_string(),
            SynthNode::File {
                content: vec![0u8; 4096],
                executable: false,
            },
        )))
        .collect();
    let fs = SyntheticFs::from_tree(entries);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let root = dir.path().to_path_buf();
    std::thread::spawn(move || {
        let fstat_target =
            std::fs::File::open(root.join("fstat-target")).expect("fstat target must open");
        for i in 0..ITERATIONS {
            let name = format!("file-{:05}", i % NAMES);
            std::fs::symlink_metadata(root.join(name)).expect("lookup must succeed");
            fstat_target.metadata().expect("fstat must succeed");
        }
    })
    .join()
    .expect("benchmark thread must not panic");

    let lookup = session.metrics().snapshot(Opcode::Lookup);
    let getattr = session.metrics().snapshot(Opcode::Getattr);
    println!(
        "LOOKUP: count={} p50={}ns p99={}ns max={}ns",
        lookup.count, lookup.p50_ns, lookup.p99_ns, lookup.max_ns
    );
    println!(
        "GETATTR: count={} p50={}ns p99={}ns max={}ns",
        getattr.count, getattr.p50_ns, getattr.p99_ns, getattr.max_ns
    );

    session.shutdown().await;
    drop(guard);
}

/// `#[ignore]`d benchmark (spec: "Performance targets"): sequential 1 MiB reads over a 1 GiB
/// zero file. Prints the achieved GB/s.
#[ignore = "benchmark: run explicitly, see the crate's docs/developing notes"]
#[tokio::test(flavor = "multi_thread")]
async fn bench_sequential_read_gbps() {
    skip_without_fuse!();

    const FILE_LEN: u64 = 1024 * 1024 * 1024;
    const CHUNK: usize = 1024 * 1024;

    let dir = TempDir::new("lore-fuse-protocol-bench-read-test-");
    let fs = SyntheticFs::from_tree(vec![("big".to_string(), SyntheticFs::zero_file(FILE_LEN))]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let root = dir.path().to_path_buf();
    let (elapsed, total_read) = std::thread::spawn(move || {
        let mut file = std::fs::File::open(root.join("big")).expect("open must succeed");
        let mut buf = vec![0u8; CHUNK];
        let mut total = 0u64;
        let start = Instant::now();
        loop {
            let n = file.read(&mut buf).expect("read must succeed");
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        (start.elapsed(), total)
    })
    .join()
    .expect("benchmark thread must not panic");

    assert_eq!(total_read, FILE_LEN, "must read the whole file");
    let gbps = (total_read as f64) / elapsed.as_secs_f64() / 1e9;
    println!(
        "sequential read: {total_read} bytes in {:.3}s = {gbps:.3} GB/s",
        elapsed.as_secs_f64()
    );

    session.shutdown().await;
    drop(guard);
}

/// Reads `path` twice (fresh open each time) on a plain `std::thread`, returning `(open_count,
/// read_count_on_second_read)`.
async fn page_cache_survival(zero_message_open: bool) -> (u64, u64) {
    let dir = TempDir::new("lore-fuse-protocol-bench-zmo-test-");
    let fs = SyntheticFs::from_tree(vec![(
        "f".to_string(),
        SynthNode::File {
            content: vec![0u8; 4096],
            executable: false,
        },
    )]);
    fs.set_zero_message_open(zero_message_open);
    let config = SessionConfig {
        workers: 1,
        init: InitConfig {
            zero_message_open,
            ..InitConfig::default()
        },
    };
    let (_mount, session) = mount_synthetic(dir.path(), fs, config)
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let path = dir.path().join("f");
    let first = path.clone();
    std::thread::spawn(move || {
        std::fs::read(&first).expect("first read must succeed");
    })
    .join()
    .expect("first read thread must not panic");
    let after_first_read = session.metrics().snapshot(Opcode::Read).count;

    let second = path.clone();
    std::thread::spawn(move || {
        std::fs::read(&second).expect("second read must succeed");
    })
    .join()
    .expect("second read thread must not panic");
    let after_second_read = session.metrics().snapshot(Opcode::Read).count;

    let open_count = session.metrics().snapshot(Opcode::Open).count;
    let second_read_count = after_second_read - after_first_read;

    session.shutdown().await;
    drop(guard);
    (open_count, second_read_count)
}

/// `#[ignore]`d benchmark (spec: "Performance targets", decision 3): reads the same file twice
/// under `zero_message_open` false and true, printing OPEN count and the READ count the *second*
/// read alone causes -- the coordinator records the page-cache survival/saving from these.
#[ignore = "benchmark: run explicitly, see the crate's docs/developing notes"]
#[tokio::test(flavor = "multi_thread")]
async fn bench_zero_message_open_page_cache_survival() {
    skip_without_fuse!();

    let (open_disabled, read_disabled) = page_cache_survival(false).await;
    let (open_enabled, read_enabled) = page_cache_survival(true).await;

    println!(
        "zero_message_open=false: OPEN count={open_disabled}, READ count on second read={read_disabled}"
    );
    println!(
        "zero_message_open=true:  OPEN count={open_enabled}, READ count on second read={read_enabled}"
    );
}
