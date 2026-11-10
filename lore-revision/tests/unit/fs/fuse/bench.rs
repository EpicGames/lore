// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Benchmarks settling two FUSE design choices: `dashmap` versus one `Mutex<HashMap>` for the
//! inode table, and whether recorded-mtime lookups at inode creation fit the LOOKUP budget.
//!
//! `cargo test -p lore-revision --release --test unit -- --ignored --nocapture --test-threads=1
//! fs::fuse::bench::`

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;
use std::time::Instant;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::text::fold_eq;
use lore_base::types::Address;
use lore_revision::fs::fuse::inode::InodeKind;
use lore_revision::fs::fuse::inode::InodeTable;
use lore_revision::fs::fuse::inode::NewChild;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::instance::InstanceId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util::path::RelativePath;
use lore_storage::FlushOptions;
use lore_storage::hash::hash_string;
use smallvec::SmallVec;

use crate::fs::fuse::bench_stats::Summary;
use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::RepositoryContextCreationArgsExt as _;
use crate::fs::fuse::fuse_test_util;
use crate::fs::fuse::fuse_test_util::XorShift;
use crate::fs::fuse::fuse_test_util::dirents;
use crate::fs::fuse::fuse_test_util::req;

const INODES: usize = 1_000_000;
const THREADS: usize = 16;
const OPS_PER_THREAD: usize = 200_000;
const MTIME_PATHS: usize = 100_000;
const BASE_TIME: u64 = 1_700_000_000_000;

fn name_of(i: usize) -> String {
    format!("file-{i:07}.dat")
}

/// The inode operations the benchmark drives, over either table.
trait Table: Sync {
    fn lookup_child(&self, parent: u64, name_hash: u64, name: &str) -> Option<u64>;
    fn forget(&self, ino: u64, n: u64);
}

impl Table for InodeTable {
    fn lookup_child(&self, parent: u64, name_hash: u64, name: &str) -> Option<u64> {
        InodeTable::lookup_child(self, parent, name_hash, name)
    }

    fn forget(&self, ino: u64, n: u64) {
        InodeTable::forget(self, ino, n);
    }
}

struct MutexEntry {
    parent: u64,
    name: Box<str>,
    nlookup: u64,
}

#[derive(Default)]
struct MutexInner {
    by_ino: HashMap<u64, MutexEntry>,
    children: HashMap<(u64, u64), SmallVec<[u64; 1]>>,
}

/// The alternative under test: both maps behind one lock, with the same lookup and FORGET
/// semantics as `InodeTable`.
#[derive(Default)]
struct MutexTable(parking_lot::Mutex<MutexInner>);

impl MutexTable {
    fn insert(&self, ino: u64, parent: u64, name_hash: u64, name: &str) {
        let mut inner = self.0.lock();
        inner.by_ino.insert(
            ino,
            MutexEntry {
                parent,
                name: Box::from(name),
                nlookup: 1,
            },
        );
        inner
            .children
            .entry((parent, name_hash))
            .or_default()
            .push(ino);
    }
}

impl Table for MutexTable {
    fn lookup_child(&self, parent: u64, name_hash: u64, name: &str) -> Option<u64> {
        let mut guard = self.0.lock();
        let inner = &mut *guard;
        let candidates = inner.children.get(&(parent, name_hash))?;
        for &ino in candidates {
            if let Some(entry) = inner.by_ino.get_mut(&ino)
                && entry.parent == parent
                && fold_eq(&entry.name, name)
            {
                entry.nlookup += 1;
                return Some(ino);
            }
        }
        None
    }

    fn forget(&self, ino: u64, n: u64) {
        let mut guard = self.0.lock();
        let inner = &mut *guard;
        let Some(entry) = inner.by_ino.get_mut(&ino) else {
            return;
        };
        entry.nlookup = entry.nlookup.saturating_sub(n);
        if entry.nlookup == 0 {
            let key = (entry.parent, hash_string(&entry.name));
            inner.by_ino.remove(&ino);
            if let Some(list) = inner.children.get_mut(&key) {
                list.retain(|child| *child != ino);
            }
        }
    }
}

struct Keys {
    names: Vec<String>,
    hashes: Vec<u64>,
}

struct MixResult {
    lookup: Summary,
    forget: Summary,
    wall: Duration,
}

/// `THREADS` threads each doing `OPS_PER_THREAD` lookup-then-forget pairs on random names,
/// timing every call.
fn run_mix(table: &dyn Table, keys: &Keys) -> MixResult {
    let barrier = Barrier::new(THREADS + 1);
    let (mut lookups, mut forgets, wall) = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let barrier = &barrier;
                scope.spawn(move || {
                    // Seeded, so both variants see the same access sequence.
                    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15 ^ (t as u64 + 1));
                    let mut lookups = Vec::with_capacity(OPS_PER_THREAD);
                    let mut forgets = Vec::with_capacity(OPS_PER_THREAD);
                    barrier.wait();
                    for _ in 0..OPS_PER_THREAD {
                        let i = (rng.next_u64() % INODES as u64) as usize;
                        let started = Instant::now();
                        let ino = table
                            .lookup_child(ROOT_INO, keys.hashes[i], &keys.names[i])
                            .expect("every name is live");
                        let looked_up = Instant::now();
                        table.forget(ino, 1);
                        let forgotten = Instant::now();
                        lookups.push((looked_up - started).as_nanos() as u64);
                        forgets.push((forgotten - looked_up).as_nanos() as u64);
                    }
                    (lookups, forgets)
                })
            })
            .collect();
        barrier.wait();
        let started = Instant::now();
        let mut lookups = Vec::with_capacity(THREADS * OPS_PER_THREAD);
        let mut forgets = Vec::with_capacity(THREADS * OPS_PER_THREAD);
        for worker in workers {
            let (l, f) = worker.join().expect("worker completes");
            lookups.extend(l);
            forgets.extend(f);
        }
        (lookups, forgets, started.elapsed())
    });
    MixResult {
        lookup: Summary::of(&mut lookups),
        forget: Summary::of(&mut forgets),
        wall,
    }
}

fn print_mix(label: &str, result: &MixResult) {
    let ops = (THREADS * OPS_PER_THREAD * 2) as f64;
    println!(
        "{label:<16} lookup p50={:>5}ns p99={:>6}ns mean={:>5}ns | forget p50={:>5}ns p99={:>6}ns \
         mean={:>5}ns | wall {:?} = {:.1} ns/op aggregate",
        result.lookup.p50,
        result.lookup.p99,
        result.lookup.mean,
        result.forget.p50,
        result.forget.p99,
        result.forget.mean,
        result.wall,
        result.wall.as_nanos() as f64 / ops,
    );
}

#[test]
#[ignore = "benchmark"]
fn bench_inode_table_dashmap_vs_mutex_hashmap_16_threads() {
    let names: Vec<String> = (0..INODES).map(name_of).collect();
    let hashes: Vec<u64> = names.iter().map(|name| hash_string(name)).collect();
    let keys = Keys { names, hashes };

    let dashmap = InodeTable::new();
    let mutex = MutexTable::default();
    for (i, name) in keys.names.iter().enumerate() {
        let (ino, ()) = dashmap.insert_child(
            NewChild {
                parent: ROOT_INO,
                name,
                name_hash: keys.hashes[i],
                kind: InodeKind::File {
                    address: Address::default(),
                    size: i as u64,
                    executable: false,
                    local: false,
                },
                mtime: 0,
                node: i as u32 + 1,
            },
            |_| (),
        );
        mutex.insert(ino, ROOT_INO, keys.hashes[i], name);
    }
    assert_eq!(dashmap.len(), INODES + 1);

    println!(
        "inode table: {INODES} inodes, {THREADS} threads x {OPS_PER_THREAD} lookup+forget pairs"
    );
    let dash = run_mix(&dashmap, &keys);
    print_mix("DashMap", &dash);
    let locked = run_mix(&mutex, &keys);
    print_mix("Mutex<HashMap>", &locked);

    let p99 = |r: &MixResult| r.lookup.p99.max(r.forget.p99) as f64;
    let advantage = (p99(&dash) - p99(&locked)) / p99(&dash) * 100.0;
    println!(
        "worst-op p99: DashMap {}ns, Mutex<HashMap> {}ns; Mutex<HashMap> faster by {advantage:.1}% \
         -> {}",
        p99(&dash),
        p99(&locked),
        if advantage > 20.0 {
            "exceeds 20%: swap inode.rs"
        } else {
            "keep DashMap"
        }
    );
}

/// A repository over an on-disk mutable store at `store_dir`, keyed like the instance whose
/// times were recorded.
async fn repository_over(
    store_dir: &Path,
    path: &Path,
    id: RepositoryId,
    instance_id: InstanceId,
    token: Option<&RepositoryWriteToken>,
) -> (Arc<RepositoryContext>, Arc<dyn lore_storage::MutableStore>) {
    let (immutable, _, _) = fuse_fixture::test_store_create()
        .await
        .expect("in-memory immutable store");
    let mutable: Arc<dyn lore_storage::MutableStore> = lore_storage::local::mutable_store::create(
        Some(store_dir),
        lore_storage::MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .expect("on-disk mutable store");
    let repository = RepositoryContext::new(
        fuse_fixture::default_repository_creation_args(immutable, mutable.clone())
            .with_path(path)
            .with_id(id)
            .with_instance_id(instance_id),
    );
    let repository = match token {
        Some(token) => repository.with_write_token(token.share()),
        None => repository,
    };
    (Arc::new(repository), mutable)
}

/// A store lookup of the kind a mount makes per inode creation once a swap has recorded times.
async fn mtime_for(repository: Arc<RepositoryContext>, path: &RelativePath, base_time: u64) -> u64 {
    let recorded = lore_revision::state::file_modified_time(repository, path).await;
    lore_revision::fs::fuse::attr::recorded_or_base(recorded, base_time)
}

async fn time_pass(repository: &Arc<RepositoryContext>, paths: &[RelativePath]) -> Summary {
    let mut samples = Vec::with_capacity(paths.len());
    for path in paths {
        let started = Instant::now();
        let mtime = mtime_for(repository.clone(), path, BASE_TIME).await;
        samples.push(started.elapsed().as_nanos() as u64);
        std::hint::black_box(mtime);
    }
    Summary::of(&mut samples)
}

fn print_pass(label: &str, summary: &Summary) {
    println!(
        "{label:<34} p50={:>6}ns p99={:>7}ns mean={:>6}ns max={:>8}ns",
        summary.p50, summary.p99, summary.mean, summary.max
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_recorded_mtime_lookup_per_inode_creation() {
    let execution = fuse_fixture::setup_test_execution();
    LORE_CONTEXT
        .scope(execution, async move {
            let store_dir = fuse_fixture::generate_tempdir();
            let work_dir = fuse_fixture::generate_tempdir();
            let path = work_dir.to_path_buf();
            let id = RepositoryId::from(uuid::Uuid::now_v7());
            let token = RepositoryWriteToken::acquire(&path).await;
            let created = lore_revision::repository::create_local(
                &path,
                &token,
                id,
                lore_base::types::Context::from(uuid::Uuid::now_v7()),
                lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                lore_revision::repository::RepositoryConfig::default(),
                false,
            )
            .await
            .expect("instance created");
            let instance_id = created.instance_id;

            let paths: Vec<RelativePath> = (0..MTIME_PATHS)
                .map(|i| {
                    RelativePath::new_from_initial_path(format!("d{:03}/{}", i / 1000, name_of(i)))
                        .expect("relative path")
                })
                .collect();
            let unrecorded: Vec<RelativePath> = (0..MTIME_PATHS)
                .map(|i| {
                    RelativePath::new_from_initial_path(format!("u{:03}/{}", i / 1000, name_of(i)))
                        .expect("relative path")
                })
                .collect();

            {
                let (repository, mutable) =
                    repository_over(store_dir.path(), &path, id, instance_id, Some(&token)).await;
                let started = Instant::now();
                for (i, path) in paths.iter().enumerate() {
                    lore_revision::state::file_modified_time_store(
                        repository.clone(),
                        path,
                        BASE_TIME + 1 + i as u64,
                    )
                    .await;
                }
                println!("recorded {MTIME_PATHS} mtimes in {:?}", started.elapsed());
                assert_eq!(
                    mtime_for(repository.clone(), &paths[7], BASE_TIME).await,
                    BASE_TIME + 8
                );
                mutable.flush(FlushOptions::SYNC_DATA).await.expect("flush");
            }
            // Let the first store's background work release its files.
            tokio::time::sleep(Duration::from_millis(200)).await;

            let (repository, _mutable) =
                repository_over(store_dir.path(), &path, id, instance_id, None).await;
            println!("mtime_for over {MTIME_PATHS} paths, store reopened from disk:");
            print_pass(
                "recorded, cold (first pass)",
                &time_pass(&repository, &paths).await,
            );
            print_pass(
                "recorded, warm (second pass)",
                &time_pass(&repository, &paths).await,
            );
            print_pass(
                "unrecorded, first pass",
                &time_pass(&repository, &unrecorded).await,
            );
            print_pass(
                "unrecorded, second pass",
                &time_pass(&repository, &unrecorded).await,
            );
            assert_eq!(
                mtime_for(repository.clone(), &paths[7], BASE_TIME).await,
                BASE_TIME + 8,
                "the reopened store holds the recorded time"
            );
            println!("LOOKUP budget: p50 < 20000ns, p99 < 100000ns round trip");
        })
        .await;
}

const DT_DIR: u32 = 4;

/// Walks the whole tree as `find` would through the kernel: READDIRPLUS in 128 KiB replies when
/// `plus`, else READDIR plus a LOOKUP per directory. Returns the entries seen past `.` and `..`.
async fn walk_tree(fs: &lore_revision::fs::fuse::filesystem::FuseFilesystem, plus: bool) -> usize {
    use lore_fuse::filesystem::Filesystem as _;
    let mut buffer = vec![0u8; 128 << 10];
    let mut pending = vec![ROOT_INO];
    let mut entries = 0;
    while let Some(dir) = pending.pop() {
        let mut offset = 0;
        loop {
            let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut buffer);
            if plus {
                fs.readdirplus(&req(), dir, 0, offset, &mut writer)
                    .await
                    .expect("readdirplus");
            } else {
                fs.readdir(&req(), dir, 0, offset, &mut writer)
                    .await
                    .expect("readdir");
            }
            let used = writer.finish();
            if used == 0 {
                break;
            }
            for fuse_test_util::Dirent {
                nodeid,
                kind,
                name,
                offset: off,
            } in dirents(&buffer[..used], plus)
            {
                offset = off;
                if name == b"." || name == b".." {
                    continue;
                }
                entries += 1;
                if kind == DT_DIR {
                    let ino = if plus {
                        nodeid
                    } else {
                        fs.lookup(&req(), dir, name).await.expect("lookup").ino
                    };
                    pending.push(ino);
                }
            }
        }
    }
    entries
}

async fn breakdown_provider(
    fixture: &fuse_fixture::FuseFixture,
) -> Arc<lore_revision::fs::fuse::filesystem::FuseFilesystem> {
    lore_revision::fs::fuse::filesystem::FuseFilesystem::new(
        fixture.repository.clone(),
        fixture.state.clone(),
        lore_revision::fs::fuse::attr::AttrContext {
            uid: 1000,
            gid: 1000,
            blksize: 1 << 17,
            base_time: BASE_TIME,
        },
    )
    .await
    .expect("provider")
}

fn per_entry(label: &str, elapsed: Duration, entries: usize) {
    println!(
        "{label:<52} {:>9.3} ms  {:>7.3} us/entry",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e6 / entries.max(1) as f64
    );
}

/// Per-entry daemon cost of a cold `find` over READDIRPLUS, split into listing builds, inode
/// creation, and the warm repeat; and the READDIR-first alternative for comparison.
/// `LORE_BENCH_FILES` sizes the tree (default 100000); `LORE_BENCH_REPS` repeats (default 5).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_readdirplus_per_entry_breakdown() {
    let files = std::env::var("LORE_BENCH_FILES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000usize);
    let reps = std::env::var("LORE_BENCH_REPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5usize);
    let tree = fuse_fixture::generated_tree(files);
    let borrowed: Vec<(&str, &[u8])> = tree
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_slice()))
        .collect();
    let started = Instant::now();
    let fixture = Box::pin(fuse_fixture::committed(&borrowed)).await;
    println!("committed {files} files in {:?}", started.elapsed());
    let execution = fuse_fixture::setup_test_execution();
    LORE_CONTEXT
        .scope(execution, async move {
            for rep in 0..reps {
                println!("rep {rep}:");
                let fs = breakdown_provider(&fixture).await;
                let started = Instant::now();
                let entries = walk_tree(&fs, true).await;
                per_entry(
                    "READDIRPLUS, cold listings and inodes",
                    started.elapsed(),
                    entries,
                );

                let fs = breakdown_provider(&fixture).await;
                let started = Instant::now();
                let entries = walk_tree(&fs, false).await;
                per_entry(
                    "READDIR + dir LOOKUP, cold listings",
                    started.elapsed(),
                    entries,
                );
                let started = Instant::now();
                let entries = walk_tree(&fs, true).await;
                per_entry(
                    "READDIRPLUS, warm listings, cold file inodes",
                    started.elapsed(),
                    entries,
                );
                let started = Instant::now();
                let entries = walk_tree(&fs, true).await;
                per_entry(
                    "READDIRPLUS, warm listings and inodes",
                    started.elapsed(),
                    entries,
                );
                let started = Instant::now();
                let entries = walk_tree(&fs, false).await;
                per_entry("READDIR + dir LOOKUP, warm", started.elapsed(), entries);

                let started = Instant::now();
                for (path, _) in &tree {
                    let path = RelativePath::new_from_initial_path(path.as_str()).expect("path");
                    std::hint::black_box(
                        mtime_for(fixture.repository.clone(), &path, BASE_TIME).await,
                    );
                }
                per_entry(
                    "mtime_for per file (path parse included)",
                    started.elapsed(),
                    files,
                );
            }
        })
        .await;
}
