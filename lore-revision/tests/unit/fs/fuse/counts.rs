// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Deterministic per-entry counts for READDIRPLUS and `read_directory`: allocations and
//! mutable-store loads, on the thread answering, with listings already built and no swap-recorded
//! times.
//!
//! `cargo test -p lore-revision --release --test unit -- --nocapture fs::fuse::counts::`

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::Filesystem;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::operation::FuseOperation;
use lore_revision::repository::RepositoryContext;
use lore_revision::util::path::RelativePath;
use lore_storage::FlushOptions;
use lore_storage::KeyType;
use lore_storage::KeyValueStream;
use lore_storage::MutableStore;
use lore_storage::StoreError;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::RepositoryContextCreationArgsExt as _;
use crate::fs::fuse::fuse_test_util::dirents;
use crate::fs::fuse::fuse_test_util::req;

const DIRS: usize = 20;
const FILES: usize = DIRS * fuse_fixture::GENERATED_FILES_PER_DIR;
const BASE_TIME: u64 = 1_700_000_000_000;
const SWAPPED_TIME: u64 = 1_800_000_000_000;

/// A mutable store counting its loads.
struct CountingStore {
    inner: Arc<dyn MutableStore>,
    loads: AtomicU64,
}

#[async_trait]
impl MutableStore for CountingStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.inner.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}

/// Names and kinds from one READDIR reply, past `.` and `..`, and the last offset.
fn names(buf: &[u8]) -> (Vec<(Vec<u8>, u32)>, u64) {
    let last = dirents(buf, false).last().map_or(0, |dirent| dirent.offset);
    let named = dirents(buf, false)
        .filter(|dirent| dirent.name != b"." && dirent.name != b"..")
        .map(|dirent| (dirent.name.to_vec(), dirent.kind))
        .collect();
    (named, last)
}

/// Allocations on this thread while `work` runs.
async fn allocations<T>(work: impl std::future::Future<Output = T>) -> (T, u64) {
    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    let result = work.await;
    let after = lore_base::allocator::thread_allocation_count();
    lore_base::allocator::set_allocation_counting(false);
    (result, after - before)
}

/// Every READDIRPLUS reply of `dir` into `buffer`; returns the entries past `.` and `..`.
async fn readdirplus_all(fs: &FuseFilesystem, dir: u64, buffer: &mut [u8]) -> usize {
    let mut offset = 0;
    let mut entries = 0;
    loop {
        let mut writer = DirentWriter::new(buffer);
        let emitted = fs
            .readdirplus(&req(), dir, 0, offset, &mut writer)
            .await
            .expect("readdirplus");
        let used = writer.finish();
        if used == 0 {
            return entries;
        }
        entries += emitted;
        offset = next_offset_plus(&buffer[..used]);
    }
}

fn next_offset_plus(buf: &[u8]) -> u64 {
    dirents(buf, true).last().map_or(0, |dirent| dirent.offset)
}

fn per(value: u64, entries: usize) -> f64 {
    value as f64 / entries.max(1) as f64
}

#[tokio::test]
async fn readdirplus_and_read_directory_per_entry_counts() {
    let tree = fuse_fixture::generated_tree(FILES);
    let borrowed: Vec<(&str, &[u8])> = tree
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_slice()))
        .collect();
    let fixture = Box::pin(fuse_fixture::committed(&borrowed)).await;
    let counting = Arc::new(CountingStore {
        inner: fixture
            .repository
            .try_mutable_store_arc()
            .expect("fixture holds a write token"),
        loads: AtomicU64::new(0),
    });
    let repository = Arc::new(RepositoryContext::new(
        fuse_fixture::default_repository_creation_args(
            fixture.repository.immutable_store(),
            counting.clone(),
        )
        .with_path(&fixture.path)
        .with_id(fixture.repository.id)
        .with_instance_id(fixture.repository.instance_id),
    ));
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            Box::pin(async move {
                let fs = FuseFilesystem::new(
                    repository,
                    fixture.state.clone(),
                    AttrContext {
                        uid: 1000,
                        gid: 1000,
                        blksize: 1 << 17,
                        base_time: BASE_TIME,
                    },
                )
                .await
                .expect("provider");
                let mut buffer = vec![0u8; 128 << 10];

                let mut writer = DirentWriter::new(&mut buffer);
                fs.readdir(&req(), ROOT_INO, 0, 0, &mut writer)
                    .await
                    .expect("root readdir");
                let used = writer.finish();
                let (root_names, _) = names(&buffer[..used]);
                let dir_names: Vec<Vec<u8>> = root_names
                    .into_iter()
                    .filter(|(_, kind)| *kind == 4)
                    .map(|(name, _)| name)
                    .collect();
                assert_eq!(dir_names.len(), DIRS);
                let mut dirs = Vec::with_capacity(DIRS);
                for name in &dir_names {
                    let dir = fs.lookup(&req(), ROOT_INO, name).await.expect("lookup").ino;
                    let mut offset = 0;
                    loop {
                        let mut writer = DirentWriter::new(&mut buffer);
                        fs.readdir(&req(), dir, 0, offset, &mut writer)
                            .await
                            .expect("readdir warms the listing");
                        let used = writer.finish();
                        if used == 0 {
                            break;
                        }
                        offset = names(&buffer[..used]).1;
                    }
                    dirs.push(dir);
                }

                counting.loads.store(0, Ordering::Relaxed);
                let (entries, cold) = allocations(Box::pin(async {
                    let mut entries = 0;
                    for &dir in &dirs {
                        entries += readdirplus_all(&fs, dir, &mut buffer).await;
                    }
                    entries
                }))
                .await;
                let cold_loads = counting.loads.swap(0, Ordering::Relaxed);
                let (warm_entries, warm) = allocations(Box::pin(async {
                    let mut entries = 0;
                    for &dir in &dirs {
                        entries += readdirplus_all(&fs, dir, &mut buffer).await;
                    }
                    entries
                }))
                .await;
                let warm_loads = counting.loads.swap(0, Ordering::Relaxed);
                assert_eq!(entries, warm_entries);
                let dir_name = &dir_names[0];

                let operation = InstanceOperationImpl::new(
                    StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                    true,
                );
                let path = RelativePath::new_from_initial_path(
                    std::str::from_utf8(dir_name).expect("utf-8"),
                )
                .expect("path");
                let (listed, listing_allocations) = allocations(async {
                    let mut listing = operation.read_directory(&path).await.expect("listing");
                    let mut listed = 0usize;
                    while let Some(entry) = listing.next().await {
                        std::hint::black_box(entry.expect("entry"));
                        listed += 1;
                    }
                    listed
                })
                .await;
                let listing_loads = counting.loads.swap(0, Ordering::Relaxed);

                // Per new inode, set beside the listing walk: a parent's path rebuilt from a depth-1
                // inode chain, and a store lookup that misses.
                let (_, path_build) = allocations(async {
                    let mut built = RelativePath::new();
                    built = built.join(std::str::from_utf8(dir_name).expect("utf-8"));
                    std::hint::black_box(built.join("file-0000001.dat"))
                })
                .await;
                let absent = path.join("never-recorded.dat");
                let (_, store_miss) = allocations(lore_revision::state::file_modified_time(
                    fixture.repository.clone(),
                    &absent,
                ))
                .await;

                println!("READDIRPLUS over {entries} entries, listing built:");
                println!(
                    "  cold inodes: {:.3} allocations/entry, {:.3} store loads/entry",
                    per(cold, entries),
                    per(cold_loads, entries)
                );
                println!(
                    "  warm inodes: {:.3} allocations/entry, {:.3} store loads/entry",
                    per(warm, entries),
                    per(warm_loads, entries)
                );
                println!(
                    "read_directory over {listed} entries: {:.3} allocations/entry, {:.3} store \
                 loads/entry",
                    per(listing_allocations, listed),
                    per(listing_loads, listed)
                );
                println!(
                    "per new inode: path build {path_build} allocations (depth 1), store miss \
                 {store_miss} allocations"
                );

                assert_eq!(
                    cold_loads, 0,
                    "no recorded times: READDIRPLUS asks the store nothing"
                );
                assert_eq!(
                    warm_loads, 0,
                    "no recorded times: READDIRPLUS asks the store nothing"
                );
                assert_eq!(
                    listing_loads, 0,
                    "no recorded times: listing asks the store nothing"
                );
                assert_eq!(warm, 0, "READDIRPLUS over known inodes allocates nothing");

                // Once a swap stored a time, a listing asks the store once per file and never
                // for a directory, which keeps the base time.
                let swapped = path.join("f00000000.txt");
                fs.recorded_times()
                    .store(&fixture.repository, std::iter::once(&swapped), SWAPPED_TIME)
                    .await
                    .expect("store a swap time");
                counting.loads.store(0, Ordering::Relaxed);
                let mut root = operation
                    .read_directory(&RelativePath::new())
                    .await
                    .expect("root listing");
                let mut root_entries = 0;
                while let Some(entry) = root.next().await {
                    assert!(entry.expect("entry").info.is_dir());
                    root_entries += 1;
                }
                assert_eq!(root_entries, DIRS);
                assert_eq!(counting.loads.swap(0, Ordering::Relaxed), 0);
                let mut files = operation.read_directory(&path).await.expect("listing");
                let mut listed = 0u64;
                while let Some(entry) = files.next().await {
                    let entry = entry.expect("entry");
                    let expected = if entry.name == "f00000000.txt" {
                        SWAPPED_TIME
                    } else {
                        BASE_TIME
                    };
                    assert_eq!(entry.info.mtime(), expected, "{}", entry.name);
                    listed += 1;
                }
                assert_eq!(counting.loads.swap(0, Ordering::Relaxed), listed);
            }),
        )
        .await;
}
