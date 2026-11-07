// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Inode attributes and STATFS, per the spec's Attributes table.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_fuse::abi::Attr;
use lore_fuse::abi::Kstatfs;
use lore_storage::hash::hash_function_args_slice;

use crate::fs::filesystem_provider::FsError;
use crate::fs::fuse::inode::InodeEntry;
use crate::fs::fuse::inode::InodeKind;
use crate::repository::RepositoryContext;
use crate::state::stored_modified_time;
use crate::store::KeyType;
use crate::util::path::RelativePath;

pub struct AttrContext {
    pub uid: u32,
    pub gid: u32,
    /// The negotiated maximum READ size.
    pub blksize: u32,
    /// The instance's clone time in milliseconds, reported until a swap records another.
    pub base_time: u64,
}

impl AttrContext {
    /// Attributes owned by the user running the mount.
    pub fn for_mounting_user(blksize: u32, base_time: u64) -> Self {
        // SAFETY: `getuid`/`getgid` take no arguments and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            uid,
            gid,
            blksize,
            base_time,
        }
    }
}

const BLOCK_SIZE: u64 = 512;
const NAME_MAX: u32 = 255;

/// Attributes of an entry from what the inode holds. A local file or symlink reports its host
/// file's instead ([`host_attr`]); this answers for one only while no host file is at hand.
pub fn attr_for(entry: &InodeEntry, ctx: &AttrContext) -> Attr {
    let (mode, size, nlink) = match entry.kind {
        InodeKind::Dir { .. } => (libc::S_IFDIR | 0o555, 0, 1),
        InodeKind::File {
            size, executable, ..
        } => {
            let permissions = if executable { 0o555 } else { 0o444 };
            (
                libc::S_IFREG | permissions,
                size,
                u32::from(!entry.orphaned),
            )
        }
        InodeKind::Symlink => (libc::S_IFLNK | 0o777, 0, 1),
    };
    let millis = recorded_or_base(entry.mtime, ctx.base_time);
    let (time, nanos) = (millis / 1000, (millis % 1000) as u32 * 1_000_000);
    Attr {
        ino: entry.ino,
        size,
        blocks: size.div_ceil(BLOCK_SIZE),
        atime: time,
        mtime: time,
        ctime: time,
        atimensec: nanos,
        mtimensec: nanos,
        ctimensec: nanos,
        mode,
        nlink,
        uid: ctx.uid,
        gid: ctx.gid,
        blksize: ctx.blksize,
        ..Attr::default()
    }
}

/// Attributes of inode `ino` from its host file's `stat`: the host owns size, mode, link count
/// and times; owner and block size are the mount's.
pub fn host_attr(ino: u64, stat: &libc::stat, ctx: &AttrContext) -> Attr {
    Attr {
        ino,
        size: stat.st_size as u64,
        blocks: stat.st_blocks as u64,
        atime: stat.st_atime as u64,
        mtime: stat.st_mtime as u64,
        ctime: stat.st_ctime as u64,
        atimensec: stat.st_atime_nsec as u32,
        mtimensec: stat.st_mtime_nsec as u32,
        ctimensec: stat.st_ctime_nsec as u32,
        mode: stat.st_mode,
        nlink: stat.st_nlink as u32,
        uid: ctx.uid,
        gid: ctx.gid,
        blksize: ctx.blksize,
        ..Attr::default()
    }
}

/// A read-only tree: no free space and no free inodes. `tree_bytes` and `node_slots` come from
/// the tree header (total content size, and node blocks times nodes per block).
pub fn statfs(ctx: &AttrContext, tree_bytes: u64, node_slots: u64) -> Kstatfs {
    Kstatfs {
        blocks: tree_bytes.div_ceil(u64::from(ctx.blksize.max(1))),
        files: node_slots,
        bsize: ctx.blksize,
        frsize: ctx.blksize,
        namelen: NAME_MAX,
        ..Kstatfs::default()
    }
}

/// Names the marker a swap stores beside the times it records.
const RECORDED_MARKER: &str = "fuse-recorded-times";
/// Names the keys a swap stores its times under.
const RECORDED_TIME: &str = "fuse-recorded-time";

/// The times swaps recorded for paths, kept in the mutable store, which holds a loaded bucket in
/// memory.
///
/// Clone records none, so until a swap records one every path reports the base time and no
/// lookup reaches the store: the marker, loaded at mount and set once this process's swap
/// stores a time, is the gate. Past it, every lookup goes to the store.
///
/// Swap times have keys of their own: scans and commits store the times they observe under the
/// instance's file modification keys, and a mount reports only times a swap recorded.
pub struct RecordedTimes {
    stored: AtomicBool,
}

impl RecordedTimes {
    /// Reads the marker from `repository`'s mutable store.
    pub async fn load(repository: &RepositoryContext) -> Self {
        let stored = repository
            .read_mutable_store()
            .load(
                repository.root_id(),
                marker_key(repository),
                KeyType::Untyped,
            )
            .await
            .is_ok();
        Self {
            stored: AtomicBool::new(stored),
        }
    }

    /// Whether no path has a recorded time.
    pub fn none(&self) -> bool {
        !self.stored.load(Ordering::Acquire)
    }

    /// Stores `mtime` as the time a swap recorded for each of `paths`, then the marker, and opens
    /// the gate. Runs once per swap, before it publishes; fails on the first write the store
    /// refuses. The keys are taken before the first await, so the future holds no path.
    pub fn store<'a, 'p>(
        &'a self,
        repository: &'a RepositoryContext,
        paths: impl Iterator<Item = &'p RelativePath>,
        mtime: u64,
    ) -> impl Future<Output = Result<(), FsError>> + Send + 'a {
        let mut keys: Vec<Hash> = paths.map(|path| recorded_key(repository, path)).collect();
        async move {
            if keys.is_empty() {
                return Ok(());
            }
            keys.push(marker_key(repository));
            let handle = repository.try_write_mutable_store().ok_or_else(|| {
                FsError::internal("storing swap-recorded times needs a write token")
            })?;
            for key in keys {
                handle
                    .store(
                        repository.root_id(),
                        key,
                        Hash::from_u64(mtime),
                        KeyType::Untyped,
                    )
                    .await
                    .forward_any::<FsError>("storing a swap-recorded time")?;
            }
            self.stored.store(true, Ordering::Release);
            Ok(())
        }
    }

    /// The time recorded for `path` at its last swap, else `base_time`.
    pub async fn mtime_for(
        &self,
        repository: &RepositoryContext,
        path: &RelativePath,
        base_time: u64,
    ) -> u64 {
        if self.none() {
            return base_time;
        }
        let recorded = stored_modified_time(repository, recorded_key(repository, path)).await;
        recorded_or_base(recorded, base_time)
    }
}

fn marker_key(repository: &RepositoryContext) -> Hash {
    hash_function_args_slice(
        repository.salt(),
        RECORDED_MARKER,
        repository.instance_id.data(),
        &[],
    )
}

fn recorded_key(repository: &RepositoryContext, path: &RelativePath) -> Hash {
    hash_function_args_slice(
        repository.salt(),
        RECORDED_TIME,
        repository.instance_id.data(),
        path.as_lowercase_str().as_bytes(),
    )
}

/// A recorded time, or `base_time` where none was recorded (0).
pub fn recorded_or_base(recorded: u64, base_time: u64) -> u64 {
    if recorded == 0 { base_time } else { recorded }
}
