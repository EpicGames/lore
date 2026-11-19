// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Inode attributes and STATFS, per the spec's Attributes table.

use std::path::Path;
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_fuse::abi::Attr;
use lore_fuse::abi::Kstatfs;

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

/// The permission bits of a file served from the store, before a writable mount adds the
/// owner's write bit.
fn file_permissions(executable: bool) -> u32 {
    if executable { 0o555 } else { 0o444 }
}

/// The permission bits a writable mount reports for a file it serves from the store, with the
/// owner's write bit it adds (`FuseFilesystem::entry_attr`), and those a copy-up gives the file's
/// host copy.
pub fn served_file_permissions(executable: bool) -> u32 {
    file_permissions(executable) | libc::S_IWUSR
}

/// Attributes of an entry from what the inode holds. A local file or symlink reports its host
/// file's instead ([`host_attr`]); this answers for one only while no host file is at hand.
pub fn attr_for(entry: &InodeEntry, ctx: &AttrContext) -> Attr {
    let (mode, size, nlink) = match entry.kind {
        InodeKind::Dir { .. } => (libc::S_IFDIR | 0o555, 0, 1),
        InodeKind::File {
            size, executable, ..
        } => (
            libc::S_IFREG | file_permissions(executable),
            size,
            u32::from(!entry.orphaned),
        ),
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

/// The modification times of the files a mount serves from the store: for each path, the time it
/// took the content it holds.
///
/// They are the instance's modified-time records ([`InstanceRecords`]), the set its commands over
/// stores on disk share: a swap records its time for each path it changes, before it publishes,
/// an operation that serves files from the store again sets its time for each before it serves it
/// and writes them back as it ends, and a commit records the time it read for each file it
/// stores. A record answers what the mount reports for a file it serves from the store. A local
/// file reports its host file's time, and a diff compares the file by its record before reading
/// it, and records the time it shows where it finds it holding the current revision's content
/// ([`Self::replace`]). A copy of the Lore directory carries them.
///
/// A new instance records none, so until a time is recorded every path reports the base time and
/// no lookup is made. The gate opens at mount where the records hold a shard file, and once a time
/// is recorded.
pub struct RecordedTimes {
    records: Arc<InstanceRecords>,
    /// Whether the records held a shard file at mount; a time set since opens the gate through
    /// [`InstanceRecords::has_changed`].
    stored: bool,
}

impl RecordedTimes {
    /// Opens the times of the instance whose Lore directory is `dot_path`: its records, the set its
    /// commands over stores on disk share.
    pub async fn load(dot_path: &Path) -> Self {
        let records = InstanceRecords::open(dot_path).await;
        let stored = records.has_shard_files().await;
        Self { records, stored }
    }

    /// Whether no path has a recorded time.
    pub fn none(&self) -> bool {
        !self.stored && !self.records.has_changed()
    }

    /// Records `mtime`, a swap's, for each of `paths` in the instance's records, written back to
    /// its Lore directory before it returns, synced under `--sync-data`. Runs once per swap, before
    /// it publishes; where it records any time, fails without a write token, and on a write that
    /// fails. The keys are taken before the first await, so the future holds no path.
    pub fn store<'a, 'p>(
        &'a self,
        repository: &'a RepositoryContext,
        paths: impl Iterator<Item = &'p RelativePath>,
        mtime: u64,
    ) -> impl Future<Output = Result<(), FsError>> + Send + 'a {
        let mut paths = paths.peekable();
        let batch = paths.peek().is_some().then(|| {
            let mut batch = RecordBatch::with_capacity(paths.size_hint().0);
            for path in paths {
                batch.push(Record::new(
                    file_modified_time_record_key(repository.salt(), path),
                    mtime,
                ));
            }
            batch
        });
        async move {
            match batch {
                Some(batch) => store_in_records(repository, &self.records, batch, mtime).await,
                None => Ok(()),
            }
        }
    }

    /// Sets `mtime` for `path` in the instance's records, where a lookup sees it at once, ahead of
    /// [`Self::write_back`]. Awaited per file an operation serves from the store again, and per
    /// local file a swap or a commit leaves local, cleared to 0.
    ///
    /// Answers the records' own future, with the key taken, so it holds no path.
    pub fn set<'a>(
        &'a self,
        repository: &RepositoryContext,
        path: &RelativePath,
        mtime: u64,
    ) -> impl Future<Output = ()> + 'a {
        self.records.set(
            file_modified_time_record_key(repository.salt(), path),
            mtime,
        )
    }

    /// Sets `mtime` for `path` in the instance's records, as [`Self::set`] does, where the records
    /// still hold `expected` for it, the time read before a diff compared the file, 0 for none:
    /// a time a swap, a commit or a restore set since is the one the path is served with.
    pub fn replace<'a>(
        &'a self,
        repository: &RepositoryContext,
        path: &RelativePath,
        expected: u64,
        mtime: u64,
    ) -> impl Future<Output = ()> + 'a {
        self.records.replace(
            file_modified_time_record_key(repository.salt(), path),
            expected,
            mtime,
        )
    }

    /// Writes the instance's records back to its Lore directory ([`write_back_records`]), with the
    /// times [`Self::set`] set, as an operation that serves files from the store again ends. Fails
    /// without a write token, and on a write that fails.
    pub async fn write_back(&self, repository: &RepositoryContext) -> Result<(), FsError> {
        let token = repository
            .try_write_token()
            .ok_or_else(|| FsError::internal("writing recorded times back needs a write token"))?;
        write_back_records(&self.records, token)
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
            match self
                .records
                .lookup(&file_modified_time_record_key(repository.salt(), path))
                .await
            {
                Lookup::Recorded(mtime) => mtime,
                Lookup::Missing(_) => base_time,
            }
        }
    }
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

/// A recorded time, or `base_time` where none was recorded (0).
pub fn recorded_or_base(recorded: u64, base_time: u64) -> u64 {
    if recorded == 0 { base_time } else { recorded }
}
