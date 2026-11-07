// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The spec's performance targets, measured on a real mount against the same tree written as a
//! plain directory on the local file system, and printed as a met/missed table.
//!
//! `cargo test -p lore-revision --release --test unit -- --ignored --nocapture --test-threads=1
//! fs::fuse::vs_ext4_bench::`
//!
//! `LORE_BENCH_FILES` (default 100000) sizes the tree and `LORE_BENCH_FILE_MB` (default 1024)
//! the large file. Page caches cannot be dropped without root, so the baseline reads are warm;
//! the FUSE first read is cold for the mount and served from a warm in-memory store.

use std::ffi::CString;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_fuse::abi::Opcode;
use lore_fuse::skip_without_fuse;
use lore_revision::change::NodeChange;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::mount::FuseMount;
use lore_revision::fs::fuse::operation::FuseOperation;
use lore_revision::fs::fuse::swap::swap_mounted;
use lore_revision::instance::InstanceId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::State;

use crate::fs::fuse::bench_stats;
use crate::fs::fuse::bench_stats::Row;
use crate::fs::fuse::bench_stats::Summary;
use crate::fs::fuse::bench_stats::c_path;
use crate::fs::fuse::bench_stats::filesystem_name;
use crate::fs::fuse::bench_stats::find_files;
use crate::fs::fuse::bench_stats::statx_ns;
use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::GENERATED_FILES_PER_DIR;
use crate::fs::fuse::fuse_fixture::generated_tree;
use crate::fs::fuse::fuse_test_util;

const BASE_TIME: u64 = 1_700_000_000_000;
const READ_CHUNK: usize = 1 << 20;
const BIG_FILE: &str = "big.bin";
const GETATTR_ITERATIONS: usize = 10_000;
const PUBLISH_ITERATIONS: usize = 101;
const PUBLISH_SMALL_FILES: usize = 10_000;
const SWAP_MANY: usize = 1000;

#[test]
fn generated_tree_is_deterministic_distinct_and_spread() {
    let tree = generated_tree(2500);
    assert_eq!(tree.len(), 2500);
    assert_eq!(tree, generated_tree(2500), "same input, same tree");

    let mut paths: Vec<&str> = tree.iter().map(|(path, _)| path.as_str()).collect();
    assert!(paths.is_sorted(), "path order");
    paths.dedup();
    assert_eq!(paths.len(), 2500, "paths are distinct");

    let mut contents: Vec<&[u8]> = tree.iter().map(|(_, c)| c.as_slice()).collect();
    contents.sort();
    contents.dedup();
    assert_eq!(contents.len(), 2500, "contents are distinct");

    let mut dirs: Vec<&str> = tree
        .iter()
        .map(|(path, _)| path.rsplit_once('/').expect("files sit in a directory").0)
        .collect();
    dirs.dedup();
    assert_eq!(dirs.len(), 3, "2500 files over directories of 1000");
    for dir in &dirs {
        let count = tree.iter().filter(|(p, _)| p.starts_with(dir)).count();
        assert!(count <= GENERATED_FILES_PER_DIR);
    }
}

#[test]
fn generated_tree_of_zero_is_empty() {
    assert!(generated_tree(0).is_empty());
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Deterministic incompressible bytes for chunk `index` of the large file.
fn big_chunk(index: u64, out: &mut [u8]) {
    let mut rng = fuse_test_util::XorShift(
        0x2545_f491_4f6c_dd1d ^ (index + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15),
    );
    for word in out.chunks_mut(8) {
        word.copy_from_slice(&rng.next_u64().to_ne_bytes()[..word.len()]);
    }
}

fn write_tree(root: &Path, tree: &[(String, Vec<u8>)]) {
    let mut last_dir = String::new();
    for (path, contents) in tree {
        if let Some((dir, _)) = path.rsplit_once('/')
            && dir != last_dir
        {
            std::fs::create_dir_all(root.join(dir)).expect("tree directory");
            last_dir = dir.to_string();
        }
        fuse_fixture::test_file_write(&root.join(path), contents);
    }
}

/// Writes the large file identically under every root.
fn write_big_file(roots: &[&Path], megabytes: usize) {
    // Fixture writes, outside any repository write.
    #[allow(clippy::disallowed_methods)]
    let mut files: Vec<std::fs::File> = roots
        .iter()
        .map(|root| std::fs::File::create(root.join(BIG_FILE)).expect("large file"))
        .collect();
    let mut chunk = vec![0u8; READ_CHUNK];
    for index in 0..megabytes as u64 {
        big_chunk(index, &mut chunk);
        for file in &mut files {
            file.write_all(&chunk).expect("large file chunk");
        }
    }
}

/// The baseline tree's directory: on the same file system as /tmp, or under
/// `$HOME/.cache/lore-fuse-bench` when /tmp is tmpfs. Removed on drop.
struct BaselineDir {
    path: PathBuf,
    _temp: Option<TempDir>,
}

impl BaselineDir {
    fn new() -> Self {
        let temp = TempDir::new("lore-fuse-bench-baseline-");
        if filesystem_name(temp.path()) != "tmpfs" {
            return Self {
                path: temp.path().to_path_buf(),
                _temp: Some(temp),
            };
        }
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let path = home
            .join(".cache/lore-fuse-bench")
            .join(uuid::Uuid::now_v7().simple().to_string());
        std::fs::create_dir_all(&path).expect("baseline directory");
        Self { path, _temp: None }
    }
}

impl Drop for BaselineDir {
    fn drop(&mut self) {
        if self._temp.is_none() {
            // Removes the benchmark's own tree, outside any repository.
            #[allow(clippy::disallowed_methods)]
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    lore_base::lore_spawn_blocking!(work)
        .await
        .expect("syscall task must not panic")
}

/// Reads `path` start to end in `READ_CHUNK` reads; answers bytes read and wall time.
fn sequential_read(path: &Path) -> (u64, Duration) {
    let mut file = std::fs::File::open(path).expect("open large file");
    let mut buf = vec![0u8; READ_CHUNK];
    let mut total = 0u64;
    let started = Instant::now();
    loop {
        let n = file.read(&mut buf).expect("read large file");
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    (total, started.elapsed())
}

fn us(ns: u64) -> String {
    format!("{:.1} us", ns as f64 / 1000.0)
}

fn below_us(value_ns: u64, target_us: u64) -> bool {
    value_ns < target_us * 1000
}

struct Live {
    instance: fuse_fixture::TestRepository,
    state: Arc<State>,
}

impl Live {
    fn repository(&self) -> Arc<RepositoryContext> {
        self.instance.repository.clone()
    }

    /// Rewrites `paths` with new contents and commits the tree.
    async fn commit_changes(&mut self, paths: &[&str], generation: u8) -> Arc<State> {
        for path in paths {
            let mut contents = format!("changed {path}").into_bytes();
            contents.push(generation);
            fuse_fixture::test_file_write(&self.instance.path.join(path), &contents);
        }
        self.state = fuse_fixture::test_commit_tree(&self.instance, "bench change").await;
        self.state.clone()
    }

    async fn diff(&self, from: &Arc<State>, to: &Arc<State>) -> Arc<Vec<NodeChange>> {
        Arc::new(
            state::diff_collect(
                self.repository(),
                from.clone(),
                self.repository(),
                to.clone(),
                None,
                FilterMode::View,
            )
            .await
            .expect("diff"),
        )
    }
}

async fn live_tree(
    immutable: Arc<dyn lore_storage::ImmutableStore>,
    mutable: Arc<dyn lore_storage::MutableStore>,
    tree: &[(String, Vec<u8>)],
    big_file_mb: usize,
    baseline: Option<&Path>,
) -> Live {
    let instance = fuse_fixture::test_repository_create(
        immutable,
        mutable,
        RepositoryId::from(uuid::Uuid::now_v7()),
    )
    .await;
    write_tree(&instance.path, tree);
    let mut roots = vec![instance.path.as_path()];
    if let Some(baseline) = baseline {
        write_tree(baseline, tree);
        roots.push(baseline);
    }
    if big_file_mb > 0 {
        write_big_file(&roots, big_file_mb);
    }
    let state = fuse_fixture::test_commit_tree(&instance, "bench tree").await;
    Live { instance, state }
}

/// Median wall time of republishing `state` on an unmounted provider.
async fn publish_median(repository: Arc<RepositoryContext>, state: Arc<State>) -> Duration {
    let fs = FuseFilesystem::new(
        repository.clone(),
        state.clone(),
        AttrContext::for_mounting_user(READ_CHUNK as u32, BASE_TIME),
    )
    .await
    .expect("provider construction");
    let mut samples = Vec::with_capacity(PUBLISH_ITERATIONS);
    for _ in 0..PUBLISH_ITERATIONS {
        let started = Instant::now();
        let tree = state.tree(repository.clone()).await.expect("tree");
        fs.publish(state.clone(), repository.clone(), None, Some(&tree));
        samples.push(started.elapsed().as_nanos() as u64);
    }
    Duration::from_nanos(Summary::of(&mut samples).p50)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_fuse_vs_ext4_target_table() {
    skip_without_fuse!();
    let files = env_usize("LORE_BENCH_FILES", 100_000);
    let big_file_mb = env_usize("LORE_BENCH_FILE_MB", 1024);
    let (immutable, mutable, execution) =
        fuse_fixture::test_store_create().await.expect("test store");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
            let tree = generated_tree(files);
            let baseline = BaselineDir::new();
            let baseline_root = baseline.path.clone();
            let started = Instant::now();
            let mut live = live_tree(
                immutable.clone(),
                mutable.clone(),
                &tree,
                big_file_mb,
                Some(&baseline_root),
            )
            .await;
            println!(
                "fixture: {files} files + {big_file_mb} MiB file committed in {:?}; baseline on {} \
                 at {}",
                started.elapsed(),
                filesystem_name(&baseline_root),
                baseline_root.display()
            );

            let mount_dir = TempDir::new("lore-fuse-bench-mount-");
            let mount_root = std::fs::canonicalize(mount_dir.path()).expect("canonical");
            let mount = FuseMount::mount(
                live.repository(),
                live.state.clone(),
                &mount_root,
                InstanceId::generate(),
                BASE_TIME,
            )
            .await
            .expect("mount");
            let metrics = || mount.metrics();
            let count = |op: Opcode| metrics().snapshot(op).count;

            // LOOKUP: kernel caches cold; the first stat builds the directory's listing, every
            // later one finds it warm, so the first sample is dropped.
            let last_dir = tree
                .last()
                .map(|(p, _)| p.rsplit_once('/').expect("dir").0.to_string())
                .expect("a non-empty tree");
            let dir_paths: Vec<CString> = tree
                .iter()
                .filter(|(p, _)| p.starts_with(&format!("{last_dir}/")))
                .map(|(p, _)| c_path(&mount_root.join(p)))
                .collect();
            let lookups_before = count(Opcode::Lookup);
            let lookup_paths = dir_paths.clone();
            let mut lookup_samples: Vec<u64> = blocking(move || {
                lookup_paths
                    .iter()
                    .map(|p| statx_ns(p, libc::AT_SYMLINK_NOFOLLOW))
                    .collect()
            })
            .await;
            let lookups = count(Opcode::Lookup) - lookups_before;
            lookup_samples.remove(0);
            let lookup = Summary::of(&mut lookup_samples);

            // GETATTR: dentries warm, the attribute cache bypassed so each call round-trips.
            let getattrs_before = count(Opcode::Getattr);
            let getattr_paths = dir_paths.clone();
            let mut getattr_samples: Vec<u64> = blocking(move || {
                (0..GETATTR_ITERATIONS)
                    .map(|i| {
                        statx_ns(
                            &getattr_paths[i % getattr_paths.len()],
                            libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_FORCE_SYNC,
                        )
                    })
                    .collect()
            })
            .await;
            let getattrs = count(Opcode::Getattr) - getattrs_before;
            let getattr = Summary::of(&mut getattr_samples);
            let daemon_lookup = metrics().snapshot(Opcode::Lookup);
            let daemon_getattr = metrics().snapshot(Opcode::Getattr);

            // find: first run on the mount, then again, then the baseline (already warm).
            let root = mount_root.clone();
            let (fuse_found, fuse_find) = blocking(move || find_files(&root)).await;
            let root = mount_root.clone();
            let (_, fuse_find_again) = blocking(move || find_files(&root)).await;
            let root = baseline_root.clone();
            let (_, _) = blocking(move || find_files(&root)).await;
            let root = baseline_root.clone();
            let (base_found, base_find) = blocking(move || find_files(&root)).await;
            assert_eq!(fuse_found, base_found, "both trees list the same files");

            // Sequential read.
            let big = mount_root.join(BIG_FILE);
            let reads_before = count(Opcode::Read);
            let path = big.clone();
            let (fuse_bytes, fuse_first) = blocking(move || sequential_read(&path)).await;
            let first_reads = count(Opcode::Read) - reads_before;
            let reads_before = count(Opcode::Read);
            let path = big.clone();
            let (_, fuse_second) = blocking(move || sequential_read(&path)).await;
            let second_reads = count(Opcode::Read) - reads_before;
            let path = baseline_root.join(BIG_FILE);
            let _ = blocking(move || sequential_read(&path)).await;
            let path = baseline_root.join(BIG_FILE);
            let (base_bytes, base_read) = blocking(move || sequential_read(&path)).await;
            assert_eq!(fuse_bytes, base_bytes, "same large file");
            let fuse_first_gbps = bench_stats::gb_per_s(fuse_bytes, fuse_first);
            let fuse_second_gbps = bench_stats::gb_per_s(fuse_bytes, fuse_second);
            let base_gbps = bench_stats::gb_per_s(base_bytes, base_read);

            // Publish: a 10k-file tree against the full one, same store.
            let small = live_tree(
                immutable.clone(),
                mutable.clone(),
                &generated_tree(PUBLISH_SMALL_FILES),
                0,
                None,
            )
            .await;
            let publish_small = publish_median(small.repository(), small.state.clone()).await;
            let publish_large = publish_median(live.repository(), live.state.clone()).await;

            // Swap on the mount, whose kernel knows every inode after `find`.
            let fs = mount.filesystem.clone();
            let operation = InstanceOperationImpl::new(StaticDispatchInstanceOperation::Fuse(
                FuseOperation::new(fs.clone()),
            ), true);
            let one: Vec<&str> = vec![tree[0].0.as_str()];
            let second = live.commit_changes(&one, 1).await;
            let started = Instant::now();
            swap_mounted(
                &fs,
                &operation,
                live.repository(),
                second.clone(),)
            .await
            .expect("swap one");
            let swap_one = started.elapsed();
            let many: Vec<&str> = tree
                .iter()
                .skip(1)
                .step_by((tree.len() / SWAP_MANY).max(1))
                .take(SWAP_MANY)
                .map(|(p, _)| p.as_str())
                .collect();
            let third = live.commit_changes(&many, 2).await;
            let changes = live.diff(&second, &third).await;
            let changed = changes.len();
            let started = Instant::now();
            swap_mounted(
                &fs,
                &operation,
                live.repository(),
                third,)
            .await
            .expect("swap many");
            let swap_many = started.elapsed();

            mount.unmount(false).await.expect("unmount");

            println!();
            println!("LOOKUP ({} samples, {lookups} LOOKUPs reached the service): client p50 {} p99 {} | daemon-side p50 {} p99 {}",
                lookup.count, us(lookup.p50), us(lookup.p99), us(daemon_lookup.p50_ns), us(daemon_lookup.p99_ns));
            println!("GETATTR ({} samples, {getattrs} GETATTRs reached the service): client p50 {} p99 {} | daemon-side p50 {} p99 {}",
                getattr.count, us(getattr.p50), us(getattr.p99), us(daemon_getattr.p50_ns), us(daemon_getattr.p99_ns));
            println!("find -type f ({fuse_found} files): FUSE first {fuse_find:?}, FUSE second {fuse_find_again:?}, baseline (warm) {base_find:?}");
            println!("sequential read {} MiB, 1 MiB reads: FUSE first (cold mount, warm store) {fuse_first:?} = {fuse_first_gbps:.2} GB/s with {first_reads} READs; FUSE second (page cache) {fuse_second:?} = {fuse_second_gbps:.2} GB/s with {second_reads} READs; baseline warm {base_read:?} = {base_gbps:.2} GB/s",
                fuse_bytes >> 20);
            println!("publish median: {PUBLISH_SMALL_FILES} files {publish_small:?}, {files} files {publish_large:?}");
            println!("swap: 1 changed {swap_one:?}; {changed} changed {swap_many:?} ({:.1} us per change)",
                swap_many.as_nanos() as f64 / changed.max(1) as f64 / 1000.0);
            println!();

            let find_ratio = fuse_find.as_secs_f64() / base_find.as_secs_f64();
            let rows = vec![
                Row { measure: "LOOKUP p50 (statx, dentry cold, listing warm)".into(), value: us(lookup.p50), target: "< 20 us".into(), met: below_us(lookup.p50, 20) },
                Row { measure: "LOOKUP p99".into(), value: us(lookup.p99), target: "< 100 us".into(), met: below_us(lookup.p99, 100) },
                Row { measure: "GETATTR p50 (statx FORCE_SYNC, dentry warm)".into(), value: us(getattr.p50), target: "< 20 us".into(), met: below_us(getattr.p50, 20) },
                Row { measure: "GETATTR p99".into(), value: us(getattr.p99), target: "< 100 us".into(), met: below_us(getattr.p99, 100) },
                Row { measure: "seq read, FUSE first vs baseline warm".into(), value: format!("{fuse_first_gbps:.2} GB/s ({:.0}%)", fuse_first_gbps / base_gbps * 100.0), target: ">= 80% and >= 2 GB/s".into(), met: fuse_first_gbps >= 0.8 * base_gbps && fuse_first_gbps >= 2.0 },
                Row { measure: "re-read READs reaching the service".into(), value: second_reads.to_string(), target: "0".into(), met: second_reads == 0 },
                Row { measure: format!("find -type f, FUSE first / baseline ({files} files)"), value: format!("{find_ratio:.2}x"), target: "<= 2x (at 1M)".into(), met: find_ratio <= 2.0 },
                Row { measure: format!("publish, {files} vs {PUBLISH_SMALL_FILES} files"), value: format!("{publish_large:?} vs {publish_small:?}"), target: "flat (<= 2x)".into(), met: bench_stats::within_factor(publish_small, publish_large, 2.0) },
                Row { measure: format!("swap, {changed} vs 1 changed"), value: format!("{swap_many:?} vs {swap_one:?}"), target: "linear in changed (<= 1000x)".into(), met: bench_stats::within_factor(swap_one, swap_many, SWAP_MANY as f64) },
            ];
            print!("{}", bench_stats::render(&rows));
            drop(small);
            drop(baseline);
        }))
        .await;
}

/// `ls -l` of `dir`: list it, then stat every entry.
fn ls_long(dir: &Path) -> (usize, Duration) {
    let started = Instant::now();
    let mut entries = 0;
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        std::hint::black_box(statx_ns(&c_path(&entry.path()), libc::AT_SYMLINK_NOFOLLOW));
        entries += 1;
    }
    (entries, started.elapsed())
}

/// Cold `find -type f` over the whole tree and cold `ls -l` of ten directories, each on a fresh
/// mount, under the READDIRPLUS policy and under plain-first READDIRPLUS.
/// `LORE_BENCH_FILES` sizes the tree (default 100000); `LORE_BENCH_REPS` repeats (default 3).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_readdir_policy_find_and_ls_long() {
    skip_without_fuse!();
    let files = env_usize("LORE_BENCH_FILES", 100_000);
    let reps = env_usize("LORE_BENCH_REPS", 3);
    let (immutable, mutable, execution) =
        fuse_fixture::test_store_create().await.expect("test store");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let tree = generated_tree(files);
        let live = live_tree(immutable.clone(), mutable.clone(), &tree, 0, None).await;
        let mut dirs: Vec<String> = tree
            .iter()
            .map(|(p, _)| p.rsplit_once('/').expect("dir").0.to_string())
            .collect();
        dirs.dedup();
        dirs.truncate(10);
        for rep in 0..reps {
            for plain_first in [false, true] {
                let policy = if plain_first { "plain-first" } else { "plus" };
                let mount_dir = TempDir::new("lore-fuse-policy-mount-");
                let root = std::fs::canonicalize(mount_dir.path()).expect("canonical");
                let mount = FuseMount::mount(
                    live.repository(),
                    live.state.clone(),
                    &root,
                    InstanceId::generate(),
                    BASE_TIME,
                )
                .await
                .expect("mount");
                mount.filesystem.set_readdirplus_plain_first(plain_first);
                let count = |op: Opcode| mount.metrics().snapshot(op).count;
                let before = [Opcode::Readdir, Opcode::Readdirplus, Opcode::Lookup].map(count);
                let find_root = root.clone();
                let (found, find) = blocking(move || find_files(&find_root)).await;
                let after = [Opcode::Readdir, Opcode::Readdirplus, Opcode::Lookup].map(count);
                println!(
                    "rep {rep} {policy:<11} find: {found} files in {:>8.3} ms; READDIR {} \
                     READDIRPLUS {} LOOKUP {}",
                    find.as_secs_f64() * 1e3,
                    after[0] - before[0],
                    after[1] - before[1],
                    after[2] - before[2]
                );
                mount.unmount(false).await.expect("unmount");

                let mount_dir = TempDir::new("lore-fuse-policy-mount-");
                let root = std::fs::canonicalize(mount_dir.path()).expect("canonical");
                let mount = FuseMount::mount(
                    live.repository(),
                    live.state.clone(),
                    &root,
                    InstanceId::generate(),
                    BASE_TIME,
                )
                .await
                .expect("mount");
                mount.filesystem.set_readdirplus_plain_first(plain_first);
                let count = |op: Opcode| mount.metrics().snapshot(op).count;
                let before = [Opcode::Readdir, Opcode::Readdirplus, Opcode::Lookup].map(count);
                let paths: Vec<PathBuf> = dirs.iter().map(|dir| root.join(dir)).collect();
                let (listed, ls) = blocking(move || {
                    paths.iter().fold((0, Duration::ZERO), |(n, t), dir| {
                        let (entries, elapsed) = ls_long(dir);
                        (n + entries, t + elapsed)
                    })
                })
                .await;
                let after = [Opcode::Readdir, Opcode::Readdirplus, Opcode::Lookup].map(count);
                println!(
                    "rep {rep} {policy:<11} ls -l: {listed} entries in {:>8.3} ms; READDIR {} \
                     READDIRPLUS {} LOOKUP {}",
                    ls.as_secs_f64() * 1e3,
                    after[0] - before[0],
                    after[1] - before[1],
                    after[2] - before[2]
                );
                mount.unmount(false).await.expect("unmount");
            }
        }
    }))
    .await;
}
