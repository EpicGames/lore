// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Inode attributes and STATFS, per the spec's Attributes table.

use std::sync::Arc;
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
use crate::records::InstanceRecords;
use crate::records::Lookup;
use crate::records::Record;
use crate::records::RecordBatch;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state::file_modified_time_record_key;
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

/// Names the marker a swap stores in the mutable store beside the times it records there.
const RECORDED_MARKER: &str = "fuse-recorded-times";
/// Names the keys a swap stores its times under in the mutable store.
const RECORDED_TIME: &str = "fuse-recorded-time";

/// The modification times of the files a mount serves from the store: for each path, the time it
/// took the content it holds.
///
/// They are the instance's modified-time records ([`InstanceRecords`]), the set its commands
/// share: a swap records its time for each path it changes, before it publishes, an operation
/// that serves files from the store again sets its time for each before it serves it and writes
/// them back as it ends, and a commit records the time it read for each file it stores. The
/// mount's scans compare addresses and read no record, so a record only answers what the mount
/// reports. A copy of the Lore directory carries them. A mount over a context without a Lore
/// directory, as a test's, keeps a swap's in the mutable store, keyed by the instance, beside a
/// marker.
///
/// A new instance records none, so until a time is recorded every path reports the base time and
/// no lookup is made. The gate opens at mount where the records hold a shard file, or the store
/// holds the marker, and once a time is recorded.
pub struct RecordedTimes {
    /// The instance's records, `None` for a mount without a Lore directory.
    records: Option<Arc<InstanceRecords>>,
    /// Set where a time was recorded before the mount, and once a swap stores one in the mutable
    /// store; a time set in the records opens the gate through [`InstanceRecords::has_changed`].
    stored: AtomicBool,
}

impl RecordedTimes {
    /// Opens the times of the instance `repository` serves: its records, the set its commands
    /// share, or those in the mutable store where it has no Lore directory.
    pub async fn load(repository: &RepositoryContext) -> Self {
        let Ok(dot_path) = repository.dot_dir_path() else {
            return Self {
                records: None,
                stored: AtomicBool::new(holds_marker(repository).await),
            };
        };
        let records = InstanceRecords::open(dot_path).await;
        let stored = records.has_shard_files().await;
        Self {
            records: Some(records),
            stored: AtomicBool::new(stored),
        }
    }

    /// Whether no path has a recorded time.
    pub fn none(&self) -> bool {
        !self.stored.load(Ordering::Acquire)
            && !self
                .records
                .as_ref()
                .is_some_and(|records| records.has_changed())
    }

    /// Records `mtime`, a swap's, for each of `paths`: in the instance's records, written back to
    /// its Lore directory before it returns, synced under `--sync-data`, or in the mutable store
    /// with the marker for a mount without one. Runs once per swap, before it publishes; where it
    /// records any time, fails without a write token, and on a write that fails. The keys are taken
    /// before the first await, so the future holds no path.
    pub fn store<'a, 'p>(
        &'a self,
        repository: &'a RepositoryContext,
        paths: impl Iterator<Item = &'p RelativePath>,
        mtime: u64,
    ) -> impl Future<Output = Result<(), FsError>> + Send + 'a {
        let mut paths = paths.peekable();
        let keys = match &self.records {
            _ if paths.peek().is_none() => SwapTimeKeys::None,
            Some(records) => {
                let mut batch = RecordBatch::with_capacity(paths.size_hint().0);
                for path in paths {
                    batch.push(Record::new(
                        file_modified_time_record_key(repository.salt(), path),
                        mtime,
                    ));
                }
                SwapTimeKeys::Records(records, batch)
            }
            None => SwapTimeKeys::Store(paths.map(|path| recorded_key(repository, path)).collect()),
        };
        async move {
            match keys {
                SwapTimeKeys::None => Ok(()),
                SwapTimeKeys::Records(records, batch) => {
                    store_in_records(repository, records, batch, mtime).await
                }
                SwapTimeKeys::Store(keys) => {
                    store_in_mutable_store(repository, keys, mtime).await?;
                    self.stored.store(true, Ordering::Release);
                    Ok(())
                }
            }
        }
    }

    /// Sets `mtime` for `path` in the instance's records, where a lookup sees it at once, ahead of
    /// [`Self::write_back`]. A mount without a Lore directory keeps no time for it. Awaited per
    /// file an operation serves from the store again.
    ///
    /// Not an `async fn`: the key is taken before the first await, so the future holds no path.
    #[allow(clippy::manual_async_fn)]
    pub fn set<'a>(
        &'a self,
        repository: &RepositoryContext,
        path: &RelativePath,
        mtime: u64,
    ) -> impl Future<Output = ()> + 'a {
        let key = file_modified_time_record_key(repository.salt(), path);
        async move {
            if let Some(records) = &self.records {
                records.set(key, mtime).await;
            }
        }
    }

    /// Writes the instance's records back to its Lore directory ([`write_back_records`]), with the
    /// times [`Self::set`] set, as an operation that serves files from the store again ends. Does
    /// nothing for a mount without a Lore directory. Fails without a write token, and on a write
    /// that fails.
    pub async fn write_back(&self, repository: &RepositoryContext) -> Result<(), FsError> {
        let Some(records) = &self.records else {
            return Ok(());
        };
        let token = repository
            .try_write_token()
            .ok_or_else(|| FsError::internal("writing recorded times back needs a write token"))?;
        write_back_records(records, token)
            .await
            .internal("writing the recorded times back")?;
        Ok(())
    }

    /// The time recorded for `path`, else `base_time`. Awaited per new inode and per listing
    /// entry.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn mtime_for<'a>(
        &'a self,
        repository: &'a RepositoryContext,
        path: &'a RelativePath,
        base_time: u64,
    ) -> impl Future<Output = u64> + 'a {
        async move {
            if self.none() {
                return base_time;
            }
            let recorded = match &self.records {
                Some(records) => match records
                    .lookup(&file_modified_time_record_key(repository.salt(), path))
                    .await
                {
                    Lookup::Recorded(mtime) => mtime,
                    Lookup::Missing(_) => 0,
                },
                None => stored_modified_time(repository, recorded_key(repository, path)).await,
            };
            recorded_or_base(recorded, base_time)
        }
    }
}

/// The keys [`RecordedTimes::store`] writes, taken before its first await.
enum SwapTimeKeys<'a> {
    /// None: the swap changed no path.
    None,
    /// For the instance's records, and the records.
    Records(&'a Arc<InstanceRecords>, RecordBatch),
    /// For the mutable store, with the marker left to add.
    Store(Vec<Hash>),
}

/// Sets `batch` in `records` and writes them back ([`write_back_records`]).
async fn store_in_records(
    repository: &RepositoryContext,
    records: &Arc<InstanceRecords>,
    batch: RecordBatch,
    mtime: u64,
) -> Result<(), FsError> {
    let token = repository
        .try_write_token()
        .ok_or_else(|| FsError::internal("storing swap-recorded times needs a write token"))?;
    records
        .set_all(batch)
        .await
        .internal_with(|| format!("recording the swap's time {mtime}"))?;
    write_back_records(records, token)
        .await
        .internal("writing the swap-recorded times back")?;
    Ok(())
}

/// Writes `records` back to the Lore directory, synced under `--sync-data`, holding the records
/// until the write has landed. Answers the write-back's own future, which each caller awaits, so
/// it adds nothing to the caller's future beside it.
fn write_back_records<'a>(
    records: &'a Arc<InstanceRecords>,
    token: &'a RepositoryWriteToken,
) -> impl Future<Output = std::io::Result<()>> + Send + 'a {
    let durable = crate::runtime::try_execution_context()
        .is_some_and(|context| context.globals().flush_options().sync_data);
    records.write_back(token, durable, Arc::clone(records))
}

/// Stores `mtime` under each of `keys` in the mutable store, then the marker. Fails on the first
/// write the store refuses.
async fn store_in_mutable_store(
    repository: &RepositoryContext,
    mut keys: Vec<Hash>,
    mtime: u64,
) -> Result<(), FsError> {
    keys.push(marker_key(repository));
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| FsError::internal("storing swap-recorded times needs a write token"))?;
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
    Ok(())
}

/// Whether the mutable store holds the marker of the instance `repository` serves.
async fn holds_marker(repository: &RepositoryContext) -> bool {
    repository
        .read_mutable_store()
        .load(
            repository.root_id(),
            marker_key(repository),
            KeyType::Untyped,
        )
        .await
        .is_ok()
}

/// The key of the marker a swap stores in the mutable store for the instance `repository` serves.
#[lore_macro::test_pub]
fn marker_key(repository: &RepositoryContext) -> Hash {
    hash_function_args_slice(
        repository.salt(),
        RECORDED_MARKER,
        repository.instance_id.data(),
        &[],
    )
}

/// The key the mutable store holds the time a swap recorded for `path` under, for the instance
/// `repository` serves.
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
