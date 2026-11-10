// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! READ throughput, latency, cache memory and allocations per fragment-cache strategy, over a
//! real mount of an in-memory store. Ignored; run with `--ignored --nocapture --test-threads=1
//! bench_fragment_cache`, `LORE_BENCH_CONTENT=random|compressible` choosing the content.
//!
//! Knobs: `LORE_BENCH_STRATEGIES` (comma-separated labels), `LORE_BENCH_W4_MB` (MiB per W4
//! file), `LORE_BENCH_FILES` (W5 small files; 0 skips W5), `LORE_BENCH_DISK_STORE=1` (store in a
//! temp directory rather than memory), `LORE_BENCH_PREFETCH=1` (prefetch ahead of sequential
//! READs).

use std::ffi::CString;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::FragmentReference;
use lore_base::types::typed_bytes::TypedBytes as _;
use lore_fuse::abi::Opcode;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::MountOptions;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::device::DeviceTransport;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::fragment_cache::FragmentCacheStrategy;
use lore_revision::fs::fuse::fragment_cache::LeafPolicy;
use lore_revision::fs::fuse::inode::InodeKind;
use lore_revision::fs::fuse::mount::FuseMount;
use lore_revision::instance::InstanceId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_storage::FlushOptions;

use crate::fs::fuse::bench_stats::Summary;
use crate::fs::fuse::bench_stats::c_path;
use crate::fs::fuse::bench_stats::filesystem_name;
use crate::fs::fuse::bench_stats::find_files;
use crate::fs::fuse::bench_stats::gb_per_s;
use crate::fs::fuse::bench_stats::statx_ns;
use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_test_util;
use crate::fs::fuse::fuse_test_util::Aligned;
use crate::fs::fuse::fuse_test_util::PAGE;
use crate::fs::fuse::fuse_test_util::XorShift;
use crate::fs::fuse::fuse_test_util::open_direct;
use crate::fs::fuse::fuse_test_util::pread;

const BASE_TIME: u64 = 1_700_000_000_000;
const MIB: usize = 1 << 20;
const BIG_FILE: &str = "big.bin";
const W4_FILES: usize = 64;
const W4_THREADS: usize = 16;
const W2_READS: usize = 20_000;
const W3_READS: usize = 4_000;
const GETATTR_ITERATIONS: usize = 10_000;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// The strategies `LORE_BENCH_STRATEGIES` names, or all of them.
fn strategies() -> Vec<(&'static str, FragmentCacheStrategy)> {
    let all = all_strategies();
    match std::env::var("LORE_BENCH_STRATEGIES") {
        Ok(wanted) => {
            let wanted: Vec<&str> = wanted.split(',').collect();
            all.into_iter()
                .filter(|(label, _)| wanted.contains(label))
                .collect()
        }
        Err(_) => all,
    }
}

fn all_strategies() -> Vec<(&'static str, FragmentCacheStrategy)> {
    use FragmentCacheStrategy::*;
    vec![
        ("S0", Storage),
        ("S1", PerInode(LeafPolicy::Load)),
        ("S2", PerInode(LeafPolicy::Decoded(1))),
        ("S3", PerInode(LeafPolicy::Decoded(4))),
        ("S5", PerInode(LeafPolicy::Stored(1))),
        ("S5x4", PerInode(LeafPolicy::Stored(4))),
        (
            "S4d16",
            Global {
                budget: 16 * MIB,
                decoded: true,
            },
        ),
        (
            "S4d64",
            Global {
                budget: 64 * MIB,
                decoded: true,
            },
        ),
        (
            "S4d256",
            Global {
                budget: 256 * MIB,
                decoded: true,
            },
        ),
        (
            "S4s64",
            Global {
                budget: 64 * MIB,
                decoded: false,
            },
        ),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Content {
    Random,
    Compressible,
}

fn content() -> Content {
    match std::env::var("LORE_BENCH_CONTENT").as_deref() {
        Ok("compressible") => Content::Compressible,
        _ => Content::Random,
    }
}

/// Deterministic bytes for chunk `index` of file `seed`: incompressible, or four bits of
/// entropy a byte, which zstd roughly halves.
fn fill(content: Content, seed: u64, index: u64, out: &mut [u8]) {
    let mut rng = XorShift(
        0x2545_f491_4f6c_dd1d
            ^ seed.wrapping_mul(0xff51_afd7_ed55_8ccd)
            ^ (index + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15),
    );
    for word in out.chunks_mut(8) {
        let x = rng.next_u64();
        let value = match content {
            Content::Random => x,
            Content::Compressible => (x & 0x0f0f_0f0f_0f0f_0f0f) | 0x6060_6060_6060_6060,
        };
        word.copy_from_slice(&value.to_ne_bytes()[..word.len()]);
    }
}

fn write_file(path: &Path, content: Content, seed: u64, mib: usize) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture directory");
    }
    // Fixture writes, outside any repository write.
    #[allow(clippy::disallowed_methods)]
    let mut file = std::fs::File::create(path).expect("fixture file");
    let mut chunk = vec![0u8; MIB];
    for index in 0..mib as u64 {
        fill(content, seed, index, &mut chunk);
        file.write_all(&chunk).expect("fixture chunk");
    }
}

fn w4_path(i: usize) -> String {
    format!("w4/f{i:02}.bin")
}

/// Fill seed of `big.bin`; W4 file `i` uses `SEED + 99 + i`.
const SEED: u64 = 1;

/// One dataset file: `mib` generated 1 MiB chunks, or literal bytes.
enum Source {
    Fill { seed: u64, mib: usize },
    Bytes(Vec<u8>),
}

/// The dataset as `(relative path, source)`: the 1 GiB file, the W4 files and the W5 small files.
fn dataset(w4_mb: usize, small: usize) -> Vec<(String, Source)> {
    let mut files = vec![(
        BIG_FILE.to_string(),
        Source::Fill {
            seed: SEED,
            mib: 1024,
        },
    )];
    files.extend((0..W4_FILES).map(|i| {
        let seed = SEED + 99 + i as u64;
        (w4_path(i), Source::Fill { seed, mib: w4_mb })
    }));
    files.extend(
        fuse_fixture::generated_tree(small)
            .into_iter()
            .map(|(path, bytes)| (format!("small/{path}"), Source::Bytes(bytes))),
    );
    files
}

fn write_dataset(root: &Path, content: Content, files: &[(String, Source)]) {
    for (path, source) in files {
        let full = root.join(path);
        match source {
            Source::Fill { seed, mib } => write_file(&full, content, *seed, *mib),
            Source::Bytes(bytes) => {
                std::fs::create_dir_all(full.parent().expect("parent")).expect("small directory");
                fuse_fixture::test_file_write(&full, bytes);
            }
        }
    }
}

struct Tree {
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    _instance: fuse_fixture::TestRepository,
    store_dir: Option<TempDir>,
    small_files: Vec<String>,
}

async fn tree(content: Content, files: &[(String, Source)]) -> Tree {
    let store_dir = (env_usize("LORE_BENCH_DISK_STORE", 0) == 1)
        .then(|| TempDir::new("lore-fuse-cache-bench-store-"));
    let immutable = lore_storage::local::immutable_store::create(
        store_dir.as_ref().map(|dir| dir.path()),
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("immutable store");
    let mutable: Arc<dyn lore_storage::MutableStore> = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .expect("mutable store");
    let instance = fuse_fixture::test_repository_create(
        immutable.clone(),
        mutable,
        RepositoryId::from(uuid::Uuid::now_v7()),
    )
    .await;
    write_dataset(&instance.path, content, files);
    let state = fuse_fixture::test_commit_tree(&instance, "fragment cache bench").await;
    immutable
        .flush(FlushOptions::SYNC_DATA)
        .await
        .expect("store flush");
    Tree {
        repository: instance.repository.clone(),
        state,
        _instance: instance,
        store_dir,
        small_files: files
            .iter()
            .filter(|(_, source)| matches!(source, Source::Bytes(_)))
            .map(|(path, _)| path.clone())
            .collect(),
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    lore_base::lore_spawn_blocking!(work)
        .await
        .expect("syscall task must not panic")
}

/// Sequential read of `path` in 1 MiB reads: bytes and per-syscall nanoseconds.
fn sequential(path: &Path) -> (u64, Vec<u64>) {
    let mut file = std::fs::File::open(path).expect("open");
    let mut buf = vec![0u8; MIB];
    let mut total = 0u64;
    let mut samples = Vec::with_capacity(1100);
    loop {
        let started = Instant::now();
        let n = file.read(&mut buf).expect("read");
        samples.push(started.elapsed().as_nanos() as u64);
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    (total, samples)
}

/// `count` random page-aligned `len`-byte `O_DIRECT` reads over `path`: per-read nanoseconds.
fn random_direct(path: &Path, len: usize, count: usize, seed: u64) -> Vec<u64> {
    let size = std::fs::metadata(path).expect("stat").len();
    let file = open_direct(path);
    let buf = Aligned::new(len);
    let pages = (size - len as u64) / PAGE as u64;
    let mut rng = XorShift(seed | 1);
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let offset = (rng.next_u64() % pages) * PAGE as u64;
        let started = Instant::now();
        let n = pread(&file, &buf, len, offset);
        samples.push(started.elapsed().as_nanos() as u64);
        assert_eq!(n, len);
    }
    samples
}

/// Runs `work(i)` for every `i < count` on `W4_THREADS` threads; answers the results in order.
fn parallel<T: Send>(count: usize, work: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, T)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..W4_THREADS)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break done;
                        }
                        done.push((i, work(i)));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("worker"))
            .collect()
    });
    done.sort_unstable_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, result)| result).collect()
}

/// W4: the W4 files under `root`, read sequentially by `W4_THREADS` threads.
fn w4(root: &Path) -> (u64, Vec<u64>) {
    parallel(W4_FILES, |i| sequential(&root.join(w4_path(i))))
        .into_iter()
        .fold((0, Vec::new()), |(bytes, mut samples), (n, mut s)| {
            samples.append(&mut s);
            (bytes + n, samples)
        })
}

/// Size and BLAKE3 hash of every dataset file, computed from the generator.
fn manifest(content: Content, files: &[(String, Source)]) -> Vec<(u64, blake3::Hash)> {
    parallel(files.len(), |i| match &files[i].1 {
        Source::Fill { seed, mib } => {
            let mut hasher = blake3::Hasher::new();
            let mut chunk = vec![0u8; MIB];
            for index in 0..*mib as u64 {
                fill(content, *seed, index, &mut chunk);
                hasher.update(&chunk);
            }
            ((mib * MIB) as u64, hasher.finalize())
        }
        Source::Bytes(bytes) => (bytes.len() as u64, blake3::hash(bytes)),
    })
}

/// One hash over the content type and every file's path, size and hash.
fn manifest_digest(
    content: Content,
    files: &[(String, Source)],
    manifest: &[(u64, blake3::Hash)],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("{content:?}\n").as_bytes());
    for ((path, _), (size, hash)) in files.iter().zip(manifest) {
        hasher.update(format!("{path} {size} {hash}\n").as_bytes());
    }
    hasher.finalize().to_string()
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let metadata = entry.metadata().expect("metadata");
        if metadata.is_dir() {
            walk(root, &entry.path(), out);
        } else {
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("under root");
            out.push((relative.to_string_lossy().into_owned(), metadata.len()));
        }
    }
}

/// Panics unless `root` holds exactly the dataset's files, with the manifest's sizes and hashes.
fn verify(root: &Path, files: &[(String, Source)], manifest: &[(u64, blake3::Hash)]) {
    let mut found = Vec::new();
    walk(root, root, &mut found);
    found.sort_unstable();
    let mut expected: Vec<(String, u64)> = files
        .iter()
        .zip(manifest)
        .map(|((path, _), (size, _))| (path.clone(), *size))
        .collect();
    expected.sort_unstable();
    assert!(
        found == expected,
        "{}: paths or sizes differ from the manifest",
        root.display()
    );
    let hashes = parallel(files.len(), |i| {
        let file = std::fs::File::open(root.join(&files[i].0)).expect("open");
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(file).expect("hash");
        hasher.finalize()
    });
    for (((path, _), (_, want)), got) in files.iter().zip(manifest).zip(hashes) {
        assert_eq!(
            got,
            *want,
            "{}: {path} differs from the manifest",
            root.display()
        );
    }
}

fn command_output(program: &str, args: &[&str]) -> String {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .expect("command runs");
    assert!(output.status.success(), "{program} {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The roots `LORE_BENCH_NATIVE` names as `ext4=<dir>:zfs=<dir>`, each labelled after its file
/// system once that is confirmed; an ext4 root on a loop device names the image's file system.
fn native_targets() -> Vec<(String, PathBuf)> {
    let roots = std::env::var("LORE_BENCH_NATIVE").unwrap_or_default();
    roots
        .split(':')
        .filter(|spec| !spec.is_empty())
        .map(|spec| {
            let (kind, root) = spec.split_once('=').expect("<ext4|zfs>=<dir>");
            let label = match kind {
                "ext4" => {
                    let mount =
                        command_output("findmnt", &["-n", "-o", "FSTYPE,SOURCE", "-T", root]);
                    let mut fields = mount.split_whitespace();
                    let (fstype, source) =
                        (fields.next().unwrap_or(""), fields.next().unwrap_or(""));
                    assert_eq!(fstype, "ext4", "{root}: findmnt reports {fstype}, not ext4");
                    if source.starts_with("/dev/loop") {
                        let image = command_output("losetup", &["-n", "-O", "BACK-FILE", source]);
                        let image_fs = filesystem_name(Path::new(&image));
                        format!("ext4 (loop, image {image} on {image_fs})")
                    } else {
                        "ext4".to_string()
                    }
                }
                "zfs" => {
                    let fs = filesystem_name(Path::new(root));
                    assert_eq!(fs, "zfs", "{root}: statfs reports {fs}, not zfs");
                    "zfs".to_string()
                }
                _ => panic!("{spec}: the kind must be ext4 or zfs"),
            };
            (label, PathBuf::from(root))
        })
        .collect()
}

/// The dataset under `root`, rewritten, replacing any other, unless its marker names `digest`.
fn native_dataset(
    root: &Path,
    content: Content,
    files: &[(String, Source)],
    digest: &str,
) -> PathBuf {
    let dir = root.join("lore-read-bench");
    let marker = root.join("lore-read-bench.digest");
    if std::fs::read_to_string(&marker).ok().as_deref() != Some(digest) {
        // Replaces the benchmark's own dataset, outside any repository.
        #[allow(clippy::disallowed_methods)]
        let _ = std::fs::remove_dir_all(&dir);
        write_dataset(&dir, content, files);
        fuse_fixture::test_file_write(&marker, digest.as_bytes());
    }
    dir
}

/// Writes back dirty data, then drops the page, dentry and inode caches; ZFS shrinks its ARC too.
fn drop_caches() {
    let status = std::process::Command::new("sh")
        .args(["-c", "sync && sudo -n /usr/sbin/sysctl -q vm.drop_caches=3"])
        .status()
        .expect("sh runs");
    assert!(
        status.success(),
        "cache drop needs passwordless sudo sysctl"
    );
}

/// ARC demand+prefetch data hits, iohits and misses, summed from the numeric arcstats lines.
fn arc_data() -> [u64; 3] {
    let stats = std::fs::read_to_string("/proc/spl/kstat/zfs/arcstats").unwrap_or_default();
    let mut counts = [0; 3];
    for line in stats.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(Ok(value))) = (fields.next(), fields.nth(1).map(str::parse::<u64>))
        else {
            continue;
        };
        let slot = match name {
            "demand_data_hits" | "prefetch_data_hits" => 0,
            "demand_data_iohits" | "prefetch_data_iohits" => 1,
            "demand_data_misses" | "prefetch_data_misses" => 2,
            _ => continue,
        };
        counts[slot] += value;
    }
    counts
}

/// Bytes read from the block device `LORE_BENCH_DISK` names (as in /proc/diskstats), or 0.
fn disk_read_bytes() -> u64 {
    let Ok(device) = std::env::var("LORE_BENCH_DISK") else {
        return 0;
    };
    let stats = std::fs::read_to_string("/proc/diskstats").expect("diskstats");
    stats
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.get(2) == Some(&device.as_str()))
        .and_then(|fields| fields.get(5)?.parse::<u64>().ok())
        .expect("device in diskstats")
        * 512
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Workload {
    W1,
    W2,
    W3,
    W4,
    W5,
    Lookup,
    Getattr,
    Find,
}

impl Workload {
    const ALL: [Self; 8] = [
        Self::W1,
        Self::W2,
        Self::W3,
        Self::W4,
        Self::W5,
        Self::Lookup,
        Self::Getattr,
        Self::Find,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::W1 => "W1 seq 1GiB 1MiB buffered",
            Self::W2 => "W2 rand 4KiB O_DIRECT",
            Self::W3 => "W3 rand 128KiB O_DIRECT",
            Self::W4 => "W4 64x64MiB 16 threads buffered",
            Self::W5 => "W5 small open+read",
            Self::Lookup => "statx lookup",
            Self::Getattr => "statx FORCE_SYNC dentry warm",
            Self::Find => "find -type f",
        }
    }
}

/// One run of `workload` over the dataset at `root`: its rate, the rate's unit and per-call
/// latency. `find` reports seconds and no latency.
fn measure(workload: Workload, root: &Path, small: &[String]) -> (f64, &'static str, Summary) {
    let dir_files = || -> Vec<CString> {
        let last = small
            .last()
            .and_then(|p| p.rsplit_once('/'))
            .expect("small files")
            .0;
        small
            .iter()
            .filter(|p| p.rsplit_once('/').is_some_and(|(dir, _)| dir == last))
            .map(|p| c_path(&root.join(p)))
            .collect()
    };
    let t = Instant::now();
    let (count, unit, mut samples) = match workload {
        Workload::W1 => {
            let (bytes, samples) = sequential(&root.join(BIG_FILE));
            assert_eq!(bytes, 1 << 30);
            (bytes as f64 / 1e9, "GB/s", samples)
        }
        Workload::W2 => {
            let samples = random_direct(&root.join(BIG_FILE), PAGE, W2_READS, 7);
            (W2_READS as f64, "IOPS", samples)
        }
        Workload::W3 => {
            let samples = random_direct(&root.join(BIG_FILE), 128 << 10, W3_READS, 7);
            (W3_READS as f64, "IOPS", samples)
        }
        Workload::W4 => {
            let (bytes, samples) = w4(root);
            (bytes as f64 / 1e9, "GB/s", samples)
        }
        Workload::W5 => {
            let samples: Vec<u64> = small
                .iter()
                .map(|path| {
                    let started = Instant::now();
                    let read = std::fs::read(root.join(path)).expect("small read");
                    assert!(!read.is_empty());
                    started.elapsed().as_nanos() as u64
                })
                .collect();
            (samples.len() as f64, "files/s", samples)
        }
        Workload::Lookup => {
            let samples: Vec<u64> = dir_files()
                .iter()
                .map(|p| statx_ns(p, libc::AT_SYMLINK_NOFOLLOW))
                .collect();
            (samples.len() as f64, "stat/s", samples)
        }
        Workload::Getattr => {
            let paths = dir_files();
            for path in &paths {
                statx_ns(path, libc::AT_SYMLINK_NOFOLLOW);
            }
            let t = Instant::now();
            let mut samples: Vec<u64> = (0..GETATTR_ITERATIONS)
                .map(|i| {
                    statx_ns(
                        &paths[i % paths.len()],
                        libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_FORCE_SYNC,
                    )
                })
                .collect();
            let rate = samples.len() as f64 / t.elapsed().as_secs_f64();
            return (rate, "stat/s", Summary::of(&mut samples));
        }
        Workload::Find => {
            let (found, elapsed) = find_files(root);
            assert_eq!(found, small.len() + 1 + W4_FILES);
            return (elapsed.as_secs_f64(), "s", Summary::of(&mut []));
        }
    };
    let rate = count / t.elapsed().as_secs_f64();
    (rate, unit, Summary::of(&mut samples))
}

/// Runs `workload` once and prints it as a tab-separated `ROW`: rate, p50 and p99 in us, READs
/// that reached the service (mounts only), the ARC data hits, iohits and misses it caused, and MiB
/// read from the pool's disk.
async fn row(
    target: &str,
    workload: Workload,
    mode: &str,
    root: &Path,
    small: &Arc<Vec<String>>,
    mount: Option<&FuseMount>,
) {
    let reads = || mount.map(|mount| mount.metrics().snapshot(Opcode::Read).count);
    let (reads_before, arc_before, disk_before) = (reads(), arc_data(), disk_read_bytes());
    let (root, small) = (root.to_path_buf(), small.clone());
    let (rate, unit, summary) = blocking(move || measure(workload, &root, &small)).await;
    let (arc, disk) = (arc_data(), disk_read_bytes() - disk_before);
    let reads = reads()
        .zip(reads_before)
        .map_or("-".to_string(), |(after, before)| {
            (after - before).to_string()
        });
    println!(
        "ROW\t{:?}\t{target}\t{}\t{mode}\t{rate:.4}\t{unit}\t{}\t{}\t{reads}\t{}\t{}\t{}\t{}",
        content(),
        workload.label(),
        us(summary.p50),
        us(summary.p99),
        arc[0] - arc_before[0],
        arc[1] - arc_before[1],
        arc[2] - arc_before[2],
        disk >> 20,
    );
}

fn us(ns: u64) -> String {
    format!("{:.1}", ns as f64 / 1000.0)
}

/// A mount with `strategy`, or the default one, prefetching ahead of sequential READs when
/// `LORE_BENCH_PREFETCH=1`: a fresh page cache and fresh session metrics. The guard unmounts
/// before the directory goes, however the bench ends.
async fn mount_with(
    tree: &Tree,
    strategy: Option<FragmentCacheStrategy>,
) -> (Arc<FuseMount>, TempDir, fuse_test_util::MountGuard, PathBuf) {
    let dir = TempDir::new("lore-fuse-cache-bench-mount-");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    let mount = FuseMount::mount(
        tree.repository.clone(),
        tree.state.clone(),
        &root,
        InstanceId::generate(),
        BASE_TIME,
    )
    .await
    .expect("mount");
    if let Some(strategy) = strategy {
        mount.filesystem.fragment_cache().set_strategy(strategy);
    }
    mount
        .filesystem
        .set_sequential_prefetch(env_usize("LORE_BENCH_PREFETCH", 0) == 1);
    let guard = fuse_test_util::MountGuard(root.clone());
    (mount, dir, guard, root)
}

/// Kind and payload size of the first leaf of `path`'s content, to show what compression did.
async fn first_leaf(
    repository: &Arc<RepositoryContext>,
    fs: &FuseFilesystem,
    path: &Path,
) -> String {
    let ino = std::fs::metadata(path).expect("stat").ino();
    let address = match fs.inodes().get(ino).as_deref().map(|entry| &entry.kind) {
        Some(InodeKind::File { address, .. }) => *address,
        _ => return "unknown".to_string(),
    };
    let mut address = address;
    let options = lore_storage::ReadOptions::default().no_decompress();
    for _ in 0..8 {
        let (fragment, payload) = lore_storage::load_fragment(
            repository.immutable_store(),
            repository.id,
            address,
            options,
            None,
        )
        .await
        .expect("load");
        if fragment.flags & lore_storage::FragmentFlags::PayloadFragmented == 0 {
            return format!(
                "leaf flags {:#x}, {} stored of {} content",
                fragment.flags, fragment.size_payload, fragment.size_content
            );
        }
        let list = payload.to_aligned::<FragmentReference>();
        address.hash = list.as_type_slice::<FragmentReference>()[0].hash;
    }
    "too deep".to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_fragment_cache_matrix() {
    skip_without_fuse!();
    let content = content();
    let w4_mb = env_usize("LORE_BENCH_W4_MB", 64);
    let small = env_usize("LORE_BENCH_FILES", 100_000);
    let (_immutable, _mutable, execution) =
        fuse_fixture::test_store_create().await.expect("test store");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let started = Instant::now();
        let tree = tree(content, &dataset(w4_mb, small)).await;
        println!(
            "fixture {content:?}: 1 GiB + {W4_FILES}x{w4_mb} MiB + {small} small files in {:?}",
            started.elapsed()
        );
        let mut first = true;
        println!(
            "| strategy | content | workload | throughput | syscall p50 us | syscall p99 us | READ p50 us | READ p99 us | READs | peak cache bytes |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|");
        for (label, strategy) in strategies() {
            let row = |workload: &str, throughput: String, summary: &Summary, read: lore_fuse::metrics::OpcodeSnapshot, peak: usize| {
                println!(
                    "| {label} | {content:?} | {workload} | {throughput} | {} | {} | {} | {} | {} | {peak} |",
                    us(summary.p50),
                    us(summary.p99),
                    us(read.p50_ns),
                    us(read.p99_ns),
                    read.count
                );
            };

            let (mount, _dir, _guard, root) = mount_with(&tree, Some(strategy)).await;
            if first {
                first = false;
                println!(
                    "compression: {}",
                    first_leaf(&tree.repository, &mount.filesystem, &root.join(BIG_FILE)).await
                );
            }
            let cache = || mount.filesystem.fragment_cache();
            let path = root.join(BIG_FILE);
            let t = Instant::now();
            let (bytes, mut samples) = blocking(move || sequential(&path)).await;
            let elapsed = t.elapsed();
            assert_eq!(bytes, 1 << 30);
            row(
                "W1 seq 1MiB",
                format!("{:.2} GB/s", gb_per_s(bytes, elapsed)),
                &Summary::of(&mut samples),
                mount.metrics().snapshot(Opcode::Read),
                cache().peak_bytes(),
            );
            mount.unmount(false).await.expect("unmount");

            for (workload, len, count) in [("W2 rand 4KiB", PAGE, W2_READS), ("W3 rand 128KiB", 128 << 10, W3_READS)] {
                let (mount, _dir, _guard, root) = mount_with(&tree, Some(strategy)).await;
                let path = root.join(BIG_FILE);
                let t = Instant::now();
                let mut samples = blocking(move || random_direct(&path, len, count, 7)).await;
                let elapsed = t.elapsed();
                row(
                    workload,
                    format!("{:.0} IOPS", count as f64 / elapsed.as_secs_f64()),
                    &Summary::of(&mut samples),
                    mount.metrics().snapshot(Opcode::Read),
                    mount.filesystem.fragment_cache().peak_bytes(),
                );
                mount.unmount(false).await.expect("unmount");
            }

            let (mount, _dir, _guard, root) = mount_with(&tree, Some(strategy)).await;
            let t = Instant::now();
            let (bytes, mut samples) = blocking(move || w4(&root)).await;
            let elapsed = t.elapsed();
            assert_eq!(bytes, (W4_FILES * w4_mb * MIB) as u64);
            row(
                "W4 64 files x16",
                format!("{:.2} GB/s", gb_per_s(bytes, elapsed)),
                &Summary::of(&mut samples),
                mount.metrics().snapshot(Opcode::Read),
                mount.filesystem.fragment_cache().peak_bytes(),
            );
            mount.unmount(false).await.expect("unmount");

            let single_leaf_variant = matches!(
                strategy,
                FragmentCacheStrategy::Storage
                    | FragmentCacheStrategy::PerInode(LeafPolicy::Load)
            );
            if small > 0 && single_leaf_variant {
                let (mount, _dir, _guard, root) = mount_with(&tree, Some(strategy)).await;
                let files: Arc<Vec<PathBuf>> =
                    Arc::new(tree.small_files.iter().map(|path| root.join(path)).collect());
                // Lookups warm first, so the pass times READs rather than listing builds.
                let warm = files.clone();
                blocking(move || {
                    for path in warm.iter() {
                        std::fs::metadata(path).expect("stat");
                    }
                })
                .await;
                let reads_before = mount.metrics().snapshot(Opcode::Read).count;
                let t = Instant::now();
                let mut samples = blocking(move || {
                    let mut samples = Vec::with_capacity(files.len());
                    for path in files.iter() {
                        let started = Instant::now();
                        let read = std::fs::read(path).expect("small read");
                        samples.push(started.elapsed().as_nanos() as u64);
                        assert!(!read.is_empty());
                    }
                    samples
                })
                .await;
                let elapsed = t.elapsed();
                let read = mount.metrics().snapshot(Opcode::Read);
                assert!(read.count > reads_before);
                row(
                    &format!("W5 {} small", tree.small_files.len()),
                    format!("{:.0} files/s", samples.len() as f64 / elapsed.as_secs_f64()),
                    &Summary::of(&mut samples),
                    read,
                    mount.filesystem.fragment_cache().peak_bytes(),
                );
                mount.unmount(false).await.expect("unmount");
            }
        }
    }))
    .await;
}

struct DisableCountingOnDrop;

impl Drop for DisableCountingOnDrop {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}

/// Most allocations one READ made, per strategy: a single worker on a `current_thread` runtime,
/// so the dispatcher's thread-local count covers the whole request. `seq` reads 56 MiB in
/// order after 8 MiB of warm-up; `hot` re-reads one 128 KiB range the READ before read.
#[tokio::test(flavor = "current_thread")]
#[ignore = "benchmark"]
async fn bench_fragment_cache_allocations() {
    skip_without_fuse!();
    let content = content();
    let mut file = vec![0u8; 64 * MIB];
    for (index, chunk) in file.chunks_mut(MIB).enumerate() {
        fill(content, 9, index as u64, chunk);
    }
    let fixture = fuse_fixture::committed(&[("f.bin", &file)]).await;
    println!("| strategy | content | pass | READs | max allocations per READ |");
    println!("|---|---|---|---|---|");
    for (label, strategy) in strategies() {
        for pass in ["seq", "hot"] {
            let fs = LORE_CONTEXT
                .scope(
                    fuse_fixture::setup_test_execution(),
                    FuseFilesystem::new(
                        fixture.repository.clone(),
                        fixture.state.clone(),
                        AttrContext::for_mounting_user(1 << 17, BASE_TIME),
                    ),
                )
                .await
                .expect("provider");
            fs.fragment_cache().set_strategy(strategy);
            let dir = TempDir::new("lore-fuse-cache-alloc-");
            let options = MountOptions {
                fsname: "lore:cache-alloc".to_string(),
                max_read: 1 << 17,
                fusermount: None,
            };
            let mount = Arc::new(lore_fuse::mount::mount(dir.path(), &options).expect("mount"));
            let guard = fuse_test_util::MountGuard(dir.path().to_path_buf());
            let session = Session::start(
                fs.clone(),
                Arc::new(DeviceTransport::new(mount)),
                SessionConfig {
                    workers: 1,
                    init: InitConfig {
                        zero_message_open: true,
                        ..InitConfig::default()
                    },
                },
            )
            .await
            .expect("session");
            let path = dir.path().join("f.bin");
            let len = 128 << 10;
            let warm_path = path.clone();
            // Warm-up outside counting: the lookup, the per-inode state, the first lists, and
            // any per-thread or pooled codec state.
            blocking(move || {
                let file = open_direct(&warm_path);
                let buf = Aligned::new(len);
                for i in 0..64u64 {
                    pread(&file, &buf, len, i * len as u64);
                }
            })
            .await;
            lore_base::allocator::set_allocation_counting(true);
            let restore = DisableCountingOnDrop;
            let reads_before = session.metrics().snapshot(Opcode::Read).count;
            blocking(move || {
                let file = open_direct(&path);
                let buf = Aligned::new(len);
                match pass {
                    "seq" => {
                        for i in 64..512u64 {
                            pread(&file, &buf, len, i * len as u64);
                        }
                    }
                    _ => {
                        for _ in 0..256 {
                            pread(&file, &buf, len, 63 * len as u64);
                        }
                    }
                }
            })
            .await;
            drop(restore);
            let read = session.metrics().snapshot(Opcode::Read);
            println!(
                "| {label} | {content:?} | {pass} | {} | {} |",
                read.count - reads_before,
                read.max_allocations
            );
            session.shutdown().await;
            drop(guard);
        }
    }
    drop(fixture);
}

/// The store `LORE_BENCH_DISK_STORE=1` asks for, in a temporary directory, or in memory.
async fn bench_stores() -> (
    Arc<dyn lore_storage::ImmutableStore>,
    Arc<dyn lore_storage::MutableStore>,
    Option<TempDir>,
) {
    let store_dir = (env_usize("LORE_BENCH_DISK_STORE", 0) == 1)
        .then(|| TempDir::new("lore-fuse-cache-bench-store-"));
    let immutable = lore_storage::local::immutable_store::create(
        store_dir.as_ref().map(|dir| dir.path()),
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("immutable store");
    let mutable: Arc<dyn lore_storage::MutableStore> = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .expect("mutable store");
    (immutable, mutable, store_dir)
}

/// Content bytes of the breakdown's file.
const CONTENT_SIZE: u64 = 1 << 30;

/// The checks `FragmentCache` makes of a list as it loads, `lore_storage::defragment`'s: every
/// entry's window strictly increasing and non-empty, and no zero hash.
fn list_checks(refs: &[FragmentReference], level_end: u64) -> bool {
    lore_storage::defragment::wanted_entries(refs, level_end, &(0..level_end)).is_ok()
}

/// Offsets of `count` READs of `len` bytes over `size`: in order, or with
/// `LORE_BENCH_PATTERN=random` scattered and `len`-aligned.
fn read_offsets(size: u64, len: usize, count: usize) -> Vec<u64> {
    let slots = size / len as u64;
    if std::env::var("LORE_BENCH_PATTERN").as_deref() != Ok("random") {
        return (0..count as u64)
            .map(|i| (i % slots) * len as u64)
            .collect();
    }
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    (0..count)
        .map(|_| (rng.next_u64() % slots) * len as u64)
        .collect()
}

/// Per-READ microseconds in process, no mount: 128 KiB READs over a 1 GiB file through
/// `FragmentCache::read`, one at a time, so each number is one READ's own cost. Also times the
/// step S1 removes: reloading the sub-list a READ's leaves hang off. `LORE_BENCH_DISK_STORE=1`
/// and `LORE_BENCH_PATTERN=random` as elsewhere; `LORE_BENCH_READ_KIB` sizes the READs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_fragment_cache_read_breakdown() {
    let content = content();
    let (immutable, mutable, store_dir) = bench_stores().await;
    let execution = fuse_fixture::setup_test_execution();
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository = Arc::new(RepositoryContext::new_null_context(immutable, mutable));
        let mut bytes = bytes::BytesMut::with_capacity(CONTENT_SIZE as usize);
        let mut chunk = vec![0u8; MIB];
        for index in 0..CONTENT_SIZE / MIB as u64 {
            fill(content, 1, index, &mut chunk);
            bytes.extend_from_slice(&chunk);
        }
        let address = lore_revision::immutable::write(
            repository.clone(),
            lore_base::types::Context::default(),
            bytes.freeze(),
            lore_storage::WriteOptions::default(),
        )
        .await
        .expect("writes");
        repository
            .immutable_store()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("store flush");
        println!(
            "store: {:?}",
            store_dir.as_ref().map(|dir| dir.path().to_path_buf())
        );
        let size = CONTENT_SIZE;
        let read_len = env_usize("LORE_BENCH_READ_KIB", 128) << 10;
        let offsets = read_offsets(size, read_len, 4160);

        // The list step: root plus one sub-list, loaded as S0 loads them for every READ.
        let options = lore_storage::ReadOptions::default();
        let mut list_samples = Vec::new();
        for _ in 0..200 {
            let started = Instant::now();
            let (_, root) = lore_storage::load_fragment(
                repository.immutable_store(),
                repository.id,
                address,
                options,
                None,
            )
            .await
            .expect("root");
            let root = root.to_aligned::<FragmentReference>();
            let sub = lore_base::types::Address {
                hash: root.as_type_slice::<FragmentReference>()[0].hash,
                context: address.context,
            };
            let (_, list) = lore_storage::load_fragment(
                repository.immutable_store(),
                repository.id,
                sub,
                options,
                None,
            )
            .await
            .expect("sub-list");
            let _ = list.to_aligned::<FragmentReference>();
            list_samples.push(started.elapsed().as_nanos() as u64);
        }
        let list = Summary::of(&mut list_samples);
        println!(
            "list step ({content:?}): root + sub-list load p50 {} us p99 {} us",
            us(list.p50),
            us(list.p99)
        );
        let (_, root) = lore_storage::load_fragment(
            repository.immutable_store(),
            repository.id,
            address,
            options,
            None,
        )
        .await
        .expect("root");
        let root = root.to_aligned::<FragmentReference>();
        let root_refs = root.as_type_slice::<FragmentReference>();
        let (_, sub) = lore_storage::load_fragment(
            repository.immutable_store(),
            repository.id,
            lore_base::types::Address {
                hash: root_refs[0].hash,
                context: address.context,
            },
            options,
            None,
        )
        .await
        .expect("sub-list");
        let sub = sub.to_aligned::<FragmentReference>();
        let sub_refs = sub.as_type_slice::<FragmentReference>();
        let sub_end = root_refs
            .get(1)
            .map_or(CONTENT_SIZE, |next| next.offset_content);
        let mut check_samples = Vec::new();
        for _ in 0..2000 {
            let started = Instant::now();
            let ok = list_checks(root_refs, CONTENT_SIZE)
                && list_checks(sub_refs, sub_end)
                && sub_refs[0].offset_content == root_refs[0].offset_content;
            check_samples.push(started.elapsed().as_nanos() as u64);
            assert!(std::hint::black_box(ok));
        }
        let checks = Summary::of(&mut check_samples);
        println!(
            "list checks ({content:?}): defragment.rs checks over root ({} refs) + sub-list ({} \
             refs), as loading both pays: p50 {} us p99 {} us",
            root_refs.len(),
            sub_refs.len(),
            us(checks.p50),
            us(checks.p99)
        );
        println!("| strategy | content | READ p50 us | READ p99 us | READ mean us |");
        println!("|---|---|---|---|---|");
        for (label, strategy) in strategies() {
            let cache = lore_revision::fs::fuse::fragment_cache::FragmentCache::new(strategy);
            let mut out = vec![0u8; read_len];
            let mut samples = Vec::with_capacity(8192);
            for (index, &offset) in offsets.iter().enumerate() {
                let started = Instant::now();
                cache
                    .read(&repository, 7, address, size, offset, &mut out, false)
                    .await
                    .expect("read");
                if index >= 64 {
                    samples.push(started.elapsed().as_nanos() as u64);
                }
            }
            let summary = Summary::of(&mut samples);
            println!(
                "| {label} | {content:?} | {} | {} | {} |",
                us(summary.p50),
                us(summary.p99),
                us(summary.mean)
            );
        }
    }))
    .await;
}

/// READs per second of many tasks reading one hot inode through `FragmentCache::read`, in
/// process: 4 KiB READs at scattered offsets of a 64 MiB file whose lists are cached after
/// warm-up. `LORE_BENCH_HOT_TASKS` tasks (default 12) of `LORE_BENCH_HOT_READS` READs each
/// (default 200000), under the default strategy or the first `LORE_BENCH_STRATEGIES` names;
/// `LORE_BENCH_HOT_MIB` sizes the file.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_hot_inode_reads() {
    let content = content();
    let tasks = env_usize("LORE_BENCH_HOT_TASKS", 12);
    let reads = env_usize("LORE_BENCH_HOT_READS", 200_000);
    let (immutable, mutable, _store_dir) = bench_stores().await;
    let execution = fuse_fixture::setup_test_execution();
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository = Arc::new(RepositoryContext::new_null_context(immutable, mutable));
        let size = env_usize("LORE_BENCH_HOT_MIB", 64) * MIB;
        let mut bytes = bytes::BytesMut::with_capacity(size);
        let mut chunk = vec![0u8; MIB];
        for index in 0..(size / MIB) as u64 {
            fill(content, 3, index, &mut chunk);
            bytes.extend_from_slice(&chunk);
        }
        let address = lore_revision::immutable::write(
            repository.clone(),
            lore_base::types::Context::default(),
            bytes.freeze(),
            lore_storage::WriteOptions::default(),
        )
        .await
        .expect("writes");
        repository
            .immutable_store()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("store flush");
        let (label, strategy) = std::env::var("LORE_BENCH_STRATEGIES")
            .ok()
            .and_then(|_| strategies().into_iter().next())
            .unwrap_or((
                "default",
                lore_revision::fs::fuse::fragment_cache::DEFAULT_FRAGMENT_CACHE,
            ));
        let cache = Arc::new(lore_revision::fs::fuse::fragment_cache::FragmentCache::new(
            strategy,
        ));
        let mut out = vec![0u8; PAGE];
        for offset in (0..size as u64).step_by(128 << 10) {
            cache
                .read(&repository, 7, address, size as u64, offset, &mut out, false)
                .await
                .expect("warm-up read");
        }
        let started = Instant::now();
        let mut joins = tokio::task::JoinSet::new();
        for task in 0..tasks as u64 {
            let cache = cache.clone();
            let repository = repository.clone();
            lore_base::lore_spawn!(joins, async move {
                let mut out = vec![0u8; PAGE];
                let mut rng =
                    XorShift(0x2545_f491_4f6c_dd1d ^ (task + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
                for _ in 0..reads {
                    let offset = (rng.next_u64() % (size / PAGE) as u64) * PAGE as u64;
                    cache
                        .read(&repository, 7, address, size as u64, offset, &mut out, false)
                        .await
                        .expect("read");
                }
            });
        }
        while let Some(joined) = joins.join_next().await {
            joined.expect("reader task");
        }
        let elapsed = started.elapsed().as_secs_f64();
        let total = (tasks * reads) as f64;
        println!(
            "hot inode ({label}, {content:?}): {tasks} tasks, {:.0} READs/s, {:.2} us per READ per task",
            total / elapsed,
            elapsed * 1e6 * tasks as f64 / total
        );
    }))
    .await;
}

/// Every workload on the FUSE mount (default strategy, then S0) and on the `LORE_BENCH_NATIVE`
/// roots, over one byte-identical dataset verified by hash on every target first. Each runs
/// `cold` (caches dropped, fresh mount), then `warm` and `warm2` (again, caches kept), since the
/// ARC regrows only gradually after a drop; FUSE also runs `remount` (fresh mount, caches kept). Needs `LORE_BENCH_DISK_STORE=1` and passwordless
/// `sudo -n /usr/sbin/sysctl -q vm.drop_caches=3`; `LORE_BENCH_TARGETS` and
/// `LORE_BENCH_WORKLOADS` (comma-separated label prefixes) narrow the rows.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_read_targets() {
    skip_without_fuse!();
    assert_eq!(
        env_usize("LORE_BENCH_DISK_STORE", 0),
        1,
        "cold FUSE rows need the store on disk"
    );
    let selected = |name: &str, label: &str| {
        std::env::var(name).map_or(true, |wanted| {
            wanted.split(',').any(|prefix| label.starts_with(prefix))
        })
    };
    let content = content();
    let files = Arc::new(dataset(
        env_usize("LORE_BENCH_W4_MB", 64),
        env_usize("LORE_BENCH_FILES", 100_000),
    ));
    let manifest = Arc::new(manifest(content, &files));
    let digest = manifest_digest(content, &files, &manifest);
    println!(
        "dataset {content:?}: seed {SEED}, {} files, manifest digest {digest}",
        files.len()
    );
    let (_immutable, _mutable, execution) =
        fuse_fixture::test_store_create().await.expect("test store");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let mut natives = Vec::new();
        for (label, root) in native_targets() {
            let (files, manifest, digest) = (files.clone(), manifest.clone(), digest.clone());
            let dir = blocking(move || {
                let dir = native_dataset(&root, content, &files, &digest);
                verify(&dir, &files, &manifest);
                dir
            })
            .await;
            println!("target {label}: {} verified", dir.display());
            natives.push((label, dir));
        }
        let started = Instant::now();
        let tree = tree(content, &files).await;
        let store = tree.store_dir.as_ref().expect("disk store").path();
        println!(
            "FUSE store on {} at {}, committed in {:?}",
            filesystem_name(store),
            store.display(),
            started.elapsed()
        );
        let (mount, _dir, _guard, root) = mount_with(&tree, None).await;
        println!(
            "default strategy {:?}, sequential prefetch {}",
            mount.filesystem.fragment_cache().strategy(),
            env_usize("LORE_BENCH_PREFETCH", 0) == 1
        );
        let (files, manifest) = (files.clone(), manifest.clone());
        blocking(move || verify(&root, &files, &manifest)).await;
        mount.unmount(false).await.expect("unmount");
        println!("target FUSE: mount verified");

        let small = Arc::new(tree.small_files.clone());
        let workloads: Vec<Workload> = Workload::ALL
            .into_iter()
            .filter(|w| selected("LORE_BENCH_WORKLOADS", w.label()))
            .collect();
        let fuse = [
            ("FUSE default", None),
            ("FUSE S0", Some(FragmentCacheStrategy::Storage)),
        ];
        for (target, strategy) in fuse
            .into_iter()
            .filter(|(t, _)| selected("LORE_BENCH_TARGETS", t))
        {
            for &workload in &workloads {
                blocking(drop_caches).await;
                let (mount, _dir, _guard, root) = mount_with(&tree, strategy).await;
                for mode in ["cold", "warm", "warm2"] {
                    row(target, workload, mode, &root, &small, Some(&mount)).await;
                }
                mount.unmount(false).await.expect("unmount");
                let (mount, _dir, _guard, root) = mount_with(&tree, strategy).await;
                row(target, workload, "remount", &root, &small, Some(&mount)).await;
                mount.unmount(false).await.expect("unmount");
            }
        }
        for (target, root) in natives
            .iter()
            .filter(|(t, _)| selected("LORE_BENCH_TARGETS", t))
        {
            for &workload in &workloads {
                blocking(drop_caches).await;
                for mode in ["cold", "warm", "warm2"] {
                    row(target, workload, mode, root, &small, None).await;
                }
            }
        }
    }))
    .await;
}

#[test]
fn every_label_names_one_strategy() {
    let labels: Vec<&str> = all_strategies().iter().map(|(label, _)| *label).collect();
    let mut unique = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), labels.len());
}
