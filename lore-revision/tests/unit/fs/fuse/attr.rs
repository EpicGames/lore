// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;

use lore_base::types::Address;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::attr::RecordedTimes;
use lore_revision::fs::fuse::attr::attr_for;
use lore_revision::fs::fuse::attr::statfs;
use lore_revision::fs::fuse::inode::InodeEntry;
use lore_revision::fs::fuse::inode::InodeKind;
use lore_revision::records::InstanceRecords;
use lore_revision::records::Lookup;
use lore_revision::records::RECORDS_DIR;
use lore_revision::records::Record;
use lore_revision::records::RecordBatch;
use lore_revision::repository::RepositoryContext;
use lore_revision::util::path::RelativePath;

const S_IFREG: u32 = libc::S_IFREG;
const S_IFDIR: u32 = libc::S_IFDIR;

fn ctx() -> AttrContext {
    AttrContext {
        uid: 1234,
        gid: 5678,
        blksize: 1 << 20,
        base_time: 1_700_000_000_123,
    }
}

fn entry(kind: InodeKind, mtime: u64, orphaned: bool) -> InodeEntry {
    InodeEntry {
        ino: 42,
        parent: 1,
        name_hash: 0,
        name: lore_revision::fs::fuse::inode::Name::from_slice(b"x"),
        kind,
        nlookup: 1,
        mtime,
        orphaned,
        read_cursor: AtomicU64::new(0),
        readahead_started: AtomicBool::new(false),
        access_recorded: AtomicBool::new(false),
        write_generation: AtomicU64::new(0),
        incarnation: 0,
        host_epoch: 0,
        renames: AtomicU32::new(0),
        rename_changes: AtomicU64::new(0),
        host_file: 0,
        node: 0,
    }
}

fn file(size: u64, executable: bool) -> InodeKind {
    InodeKind::File {
        address: Address::default(),
        size,
        executable,
        local: false,
        source: 0,
    }
}

#[test]
fn file_attr_is_0444_or_0555_nlink_1_blocks_ceil_512() {
    let ctx = ctx();
    let attr = attr_for(&entry(file(1025, false), 99_000, false), &ctx);
    assert_eq!(attr.ino, 42);
    assert_eq!(attr.mode, S_IFREG | 0o444);
    assert_eq!(attr.nlink, 1);
    assert_eq!(attr.size, 1025);
    assert_eq!(attr.blocks, 3);
    assert_eq!(attr.blksize, 1 << 20);
    assert_eq!((attr.mtime, attr.ctime, attr.atime), (99, 99, 99));

    let exec = attr_for(&entry(file(512, true), 99, false), &ctx);
    assert_eq!(exec.mode, S_IFREG | 0o555);
    assert_eq!(exec.blocks, 1);
    assert_eq!(attr_for(&entry(file(0, false), 99, false), &ctx).blocks, 0);
}

#[test]
fn times_are_milliseconds_split_into_seconds_and_nanoseconds() {
    let attr = attr_for(&entry(file(1, false), 1_790_141_702_758, false), &ctx());
    assert_eq!(
        (attr.mtime, attr.ctime, attr.atime),
        (1_790_141_702, 1_790_141_702, 1_790_141_702)
    );
    assert_eq!(
        (attr.mtimensec, attr.ctimensec, attr.atimensec),
        (758_000_000, 758_000_000, 758_000_000)
    );
}

#[test]
fn dir_attr_is_0555() {
    let attr = attr_for(
        &entry(InodeKind::dir(0, lore_revision::node::ROOT_NODE), 7, false),
        &ctx(),
    );
    assert_eq!(attr.mode, S_IFDIR | 0o555);
    assert_eq!(attr.nlink, 1);
    assert_eq!(attr.size, 0);
    assert_eq!(attr.blocks, 0);
    assert_eq!(attr.blksize, 1 << 20);
}

#[test]
fn orphan_has_nlink_zero() {
    let attr = attr_for(&entry(file(10, false), 7, true), &ctx());
    assert_eq!(attr.nlink, 0);
    assert_eq!(attr.size, 10, "an orphan keeps serving its old content");
}

#[test]
fn uid_gid_are_mounting_user() {
    let attr = attr_for(&entry(file(1, false), 7, false), &ctx());
    assert_eq!((attr.uid, attr.gid), (1234, 5678));

    let mounting = AttrContext::for_mounting_user(4096, 5);
    // SAFETY: `getuid`/`getgid` take no arguments and cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    assert_eq!((mounting.uid, mounting.gid), (uid, gid));
    assert_eq!((mounting.blksize, mounting.base_time), (4096, 5));
}

#[test]
fn mtime_uses_base_time_when_no_recorded_time() {
    let ctx = ctx();
    let attr = attr_for(&entry(file(1, false), 0, false), &ctx);
    assert_eq!(attr.mtime, ctx.base_time / 1000);
    assert_eq!(attr.mtimensec, 123_000_000);
}

/// Two instances on one store, each with a Lore directory and a write token, run under the
/// fixture's execution context.
async fn two_instances(
    work: impl AsyncFnOnce(&super::fuse_fixture::TestRepository, &super::fuse_fixture::TestRepository),
) {
    let (immutable_store, mutable_store, execution) = super::fuse_fixture::test_store_create()
        .await
        .expect("test store");
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let repository_id = lore_revision::lore::RepositoryId::from(uuid::Uuid::now_v7());
            let source = super::fuse_fixture::test_repository_create(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            )
            .await;
            let clone = super::fuse_fixture::test_repository_create(
                immutable_store,
                mutable_store,
                repository_id,
            )
            .await;
            work(&source, &clone).await;
        })
        .await;
}

/// The directory holding the modified-time records of `repository`'s instance.
fn records_of(repository: &RepositoryContext) -> std::path::PathBuf {
    repository
        .dot_dir_path()
        .expect("a Lore directory")
        .join(RECORDS_DIR)
}

/// A swap's times land in the instance's records, and a copy of the records gives another
/// instance the same times.
#[tokio::test]
async fn swap_times_are_kept_in_the_instance_records_and_a_copy_carries_them() {
    two_instances(async |source, clone| {
        let repository = &source.repository;
        let path = RelativePath::new_from_initial_path("dir/swapped.txt").expect("path");
        let times = RecordedTimes::load(repository.dot_dir_path().expect("a Lore directory")).await;
        assert!(times.none());
        times
            .store(repository, std::iter::once(&path), 999)
            .await
            .expect("store the swap's time");
        assert_eq!(times.mtime_for(repository, &path, 777).await, 999);
        drop(times);
        let reloaded =
            RecordedTimes::load(repository.dot_dir_path().expect("a Lore directory")).await;
        assert!(!reloaded.none(), "a later mount finds the shard");
        assert_eq!(reloaded.mtime_for(repository, &path, 777).await, 999);

        let copied = records_of(&clone.repository);
        std::fs::create_dir_all(&copied).expect("the copy's records");
        for entry in std::fs::read_dir(records_of(repository)).expect("list") {
            let entry = entry.expect("entry");
            if entry.file_name().len() == 2 {
                std::fs::copy(entry.path(), copied.join(entry.file_name())).expect("copy");
            }
        }
        let carried =
            RecordedTimes::load(clone.repository.dot_dir_path().expect("a Lore directory")).await;
        assert!(!carried.none());
        assert_eq!(
            carried.mtime_for(&clone.repository, &path, 777).await,
            999,
            "the copy reports the source's time"
        );
        let other = RelativePath::new_from_initial_path("dir/other.txt").expect("path");
        assert_eq!(carried.mtime_for(&clone.repository, &other, 777).await, 777);
    })
    .await;
}

/// The mount reads and writes the records the instance's commands hold: a time a command records
/// opens the gate and is reported at once, and neither the mount's write-back nor the command's
/// writes a shard over a time the other recorded in it.
#[tokio::test]
async fn the_mount_shares_the_records_of_the_instances_commands() {
    lore_base::runtime::LORE_CONTEXT
        .scope(super::fuse_fixture::setup_test_execution(), async {
            let (repository, _directory) = crate::helper::committed_repository("", &[]).await;
            let salt = repository.salt();
            let record_key = |path: &RelativePath| {
                lore_revision::state::file_modified_time_record_key(salt, path)
            };
            let committed = RelativePath::new_from_initial_path("committed.txt").expect("path");
            let swapped = (0..)
                .map(|index| {
                    RelativePath::new_from_initial_path(format!("swapped-{index}.txt"))
                        .expect("path")
                })
                .find(|path| record_key(path)[0] == record_key(&committed)[0])
                .expect("a path in the same shard");
            let command = repository
                .instance_records()
                .expect("the instance keeps its records")
                .clone();
            let mut batch = RecordBatch::with_capacity(1);
            batch.push(Record::new(record_key(&committed), 500));
            command
                .set_all(batch)
                .await
                .expect("record as a commit does");

            let times =
                RecordedTimes::load(repository.dot_dir_path().expect("a Lore directory")).await;
            assert!(!times.none(), "the command's record opens the gate");
            assert_eq!(times.mtime_for(&repository, &committed, 7).await, 500);
            times
                .store(&repository, std::iter::once(&swapped), 900)
                .await
                .expect("store the swap's time");
            command
                .write_back(
                    repository.try_write_token().expect("a write token"),
                    false,
                    (),
                )
                .await
                .expect("write the command's records back");
            let dot_path = repository
                .dot_dir_path()
                .expect("a Lore directory")
                .to_path_buf();
            drop((times, command, repository));

            let reopened = InstanceRecords::open(&dot_path).await;
            assert_eq!(
                reopened.lookup(&record_key(&committed)).await,
                Lookup::Recorded(500)
            );
            assert_eq!(
                reopened.lookup(&record_key(&swapped)).await,
                Lookup::Recorded(900)
            );
        })
        .await;
}

#[test]
fn statfs_namemax_255_bsize_is_max_read() {
    let st = statfs(&ctx(), (3 << 20) + 1, 1024);
    assert_eq!((st.blocks, st.files), (4, 1024));
    assert_eq!(st.namelen, 255);
    assert_eq!(st.bsize, 1 << 20);
    assert_eq!(st.frsize, 1 << 20);
    assert_eq!((st.bfree, st.bavail, st.ffree), (0, 0, 0));
}
