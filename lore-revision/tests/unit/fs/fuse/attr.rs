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

#[tokio::test]
async fn mtime_uses_base_time_when_no_recorded_time() {
    let ctx = ctx();
    let attr = attr_for(&entry(file(1, false), 0, false), &ctx);
    assert_eq!(attr.mtime, ctx.base_time / 1000);
    assert_eq!(attr.mtimensec, 123_000_000);

    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");
    let repository = RepositoryContext::new_null_context(immutable_store, mutable_store);
    let path = RelativePath::new_from_initial_path("never/recorded.txt").expect("path");
    let times = RecordedTimes::load(&repository).await;
    assert!(times.none());
    assert_eq!(times.mtime_for(&repository, &path, 777).await, 777);

    times
        .store(&repository, std::iter::once(&path), 999)
        .await
        .expect("store the swap's time");
    assert!(!times.none());
    assert_eq!(times.mtime_for(&repository, &path, 777).await, 999);
    let other = RelativePath::new_from_initial_path("never/other.txt").expect("path");
    assert_eq!(times.mtime_for(&repository, &other, 777).await, 777);
    assert!(
        !RecordedTimes::load(&repository).await.none(),
        "a later mount finds the marker"
    );
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
