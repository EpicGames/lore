// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The temporary file that a file read resuming ([`ReadOptions::resume_write`]) writes the content
//! of a fragment list to, and the file it keeps beside it. Only a read of a range of at least
//! [`RESUME_WRITE_MIN_LEN`] resumes.
//!
//! Beside the temporary file, a resuming read keeps its *resume record*, the file
//! `<temporary>.rec`, which names the hash and the range of the content the temporary file holds.
//! A read of the same hash and range to the same path resumes from the temporary file, which it
//! sizes, emptying it only where a hole of the content holds data or its holes cannot be checked,
//! as below. A read of other content empties the temporary file before writing to it.
//!
//! A resumed read takes one map of the ranges the file holds data for before it writes to it. It
//! counts a leaf as held where the leaf's whole range is data, its bytes are not all zeros, and
//! they hash to the leaf's address, and fetches every other leaf. A hole of the content, a
//! reference of the zero hash, is never written, in a temporary file of a resuming read as in any
//! other: the temporary file was emptied before any write of the content it holds. A resumed read
//! hands a hole to the write sink as a hole, so it holds no storage where the file system supports
//! holes. Of the hole, it reads back only the parts of the blocks it shares with the leaves beside
//! it that hold data. Where a whole block of a hole holds data, a part read back holds bytes other
//! than zeros, the file cannot be mapped, or a part of a hole cannot be read back, the read empties
//! the file and writes it as a fresh one. Where the file system reports no holes, the whole file is
//! data, and where the temporary file could not be made sparse, so is all of it but, on NTFS, what
//! lies past what was written to it, so content with a hole holding a whole block there is written
//! as fresh. On Apple platforms, where the file system holds holes, a whole block of a hole holding
//! data does not empty the file: APFS allocates the small holes beside data it writes out, and the
//! write sink deallocates the whole blocks of every hole of the range before the rename.
//!
//! A resuming read holds the exclusive lock of its temporary file for as long as any handle to it
//! lives, from before it reads or empties anything there until every task writing to it has ended,
//! so one resuming read at a time writes a temporary file. A read that does not resume takes no
//! lock and waits for none: it writes the temporary file of its path whether or not a resuming read
//! is writing it. Within that, what the resume record names decides only what a read costs, never
//! what it writes: every leaf taken from a temporary file is checked against its address, so a
//! stale or wrong record cannot put other bytes in the file a read renames into place.
//!
//! Where the file system refuses the lock, as some network file systems do, the read writes as
//! one that does not resume. A lock attempt runs on the calling thread, as [`FSLock`]'s do, so on a
//! network file system it may block that thread for as long as the server takes to answer.
//!
//! [`FSLock`]: lore_base::fs::lock::FSLock
//! [`ReadOptions::resume_write`]: crate::options::ReadOptions::resume_write
//! [`RESUME_WRITE_MIN_LEN`]: crate::options::RESUME_WRITE_MIN_LEN

use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;

use bytes::Bytes;
use lore_base::fs::lock::LockWait;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::LockAttempt;
use lore_io::OpenOptions;
use tokio::task::JoinHandle;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::error::StorageError;
use crate::types::Hash;

/// Marks the resume record, and the layout of what follows it: "Lore resume". A later layout takes
/// another magic, and a record with any other magic names other content, so the temporary file is
/// emptied and every leaf fetched.
const RECORD_MAGIC: [u8; 8] = *b"LORERESM";

/// How many times in a row the file a read locked may have moved from its temporary path before
/// the read gives up on it ([`Opened::Moving`]).
const REOPEN_LIMIT: u32 = 64;

/// What an open of the temporary file of a resuming read came to ([`open`]).
pub(crate) enum Opened {
    /// The temporary file, locked and sized to the range, and whether it holds part of the content
    /// from an earlier read.
    File(IoFile, bool),
    /// The file system refuses the lock. Nothing was read or emptied.
    Refused,
    /// The file the read locked moved from the temporary path [`REOPEN_LIMIT`] times in a row,
    /// other reads renaming theirs into place.
    Moving,
}

/// What the resume record holds: the content hash, and the start and the length of the range the
/// temporary file holds. In the byte order of the machine, which is the only one to read it.
#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct ResumeRecord {
    magic: [u8; 8],
    hash: [u8; 32],
    start: u64,
    length: u64,
}

impl ResumeRecord {
    fn of(hash: &Hash, range: &Range<usize>) -> Self {
        Self {
            magic: RECORD_MAGIC,
            hash: *hash.data(),
            start: range.start as u64,
            length: range.len() as u64,
        }
    }
}

/// The resume record of the temporary file `temporary`: `temporary` with `.rec` appended.
pub(crate) fn record_path(temporary: &Path) -> PathBuf {
    let mut record = temporary.as_os_str().to_os_string();
    record.push(".rec");
    PathBuf::from(record)
}

/// Opens the temporary file `temporary` for the `range` of the content `hash` names, locked, sparse
/// and sized to the range ([`IoFile::set_len_sparse`]), and answers whether it holds part of that
/// content from an earlier read: it is not empty, and its resume record names the same hash and
/// range. Any other temporary file is emptied ([`IoFile::empty`]), and its resume record written
/// for this content, before the read writes anything to it.
///
/// # Errors
///
/// Any error of the open, the lock or the file operations.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub(crate) fn open<'a>(
    temporary: &'a Path,
    hash: &Hash,
    range: &Range<usize>,
) -> impl Future<Output = std::io::Result<Opened>> + 'a {
    let record = ResumeRecord::of(hash, range);
    let len = range.len() as u64;
    async move {
        let driver = IoDriver::global();
        let file = match open_locked(temporary).await? {
            Ok(file) => file,
            Err(opened) => return Ok(opened),
        };
        let record_path = record_path(temporary);
        let held = file.metadata().await?.len();
        let resumes = held > 0
            && driver
                .read_file_bytes(&record_path)
                .await
                .is_ok_and(|read| read.as_ref() == record.as_bytes());
        if !resumes {
            if held > 0 {
                file.empty(0).await?;
            }
            driver
                .write_file_bytes(
                    &record_path,
                    Bytes::copy_from_slice(record.as_bytes()),
                    false,
                )
                .await?;
        }
        file.set_len_sparse(len).await?;
        Ok(Opened::File(file, resumes))
    }
}

/// Opens the temporary file `temporary`, creating it where it is missing, and takes its lock: the
/// file, or [`Opened::Refused`] or [`Opened::Moving`] where the read gets no locked file there.
///
/// The lock is taken on the file a handle has open, while the read holding it may rename that file
/// into place before letting go. So whenever the one locked is no longer at `temporary`, the
/// handle is dropped, and with it the lock, and the file opened again as [`LockWait`] paces it, at
/// most [`REOPEN_LIMIT`] times in a row.
async fn open_locked(temporary: &Path) -> std::io::Result<Result<IoFile, Opened>> {
    let mut wait = LockWait::new();
    for _ in 0..REOPEN_LIMIT {
        let file = IoDriver::global()
            .open(
                temporary,
                &OpenOptions::new().read(true).write(true).create(true),
            )
            .await?;
        if !lock(&file, temporary).await? {
            lore_base::lore_debug!(
                "{} refuses the lock, writing it without resuming",
                temporary.display()
            );
            return Ok(Err(Opened::Refused));
        }
        if file.is_at(temporary).await? && !moved() {
            return Ok(Ok(file));
        }
        drop(file);
        wait.next(temporary).await;
    }
    lore_base::lore_warn!(
        "{} moved {REOPEN_LIMIT} times while waiting for its lock, other reads renaming theirs \
         into place",
        temporary.display()
    );
    Ok(Err(Opened::Moving))
}

/// Waits until `file` holds its exclusive lock, which another read of the same temporary file
/// holds for as long as it writes there, retrying as [`LockWait`] paces it: `false` where the file
/// system refuses the lock.
async fn lock(file: &IoFile, temporary: &Path) -> std::io::Result<bool> {
    let mut wait = LockWait::new();
    loop {
        match try_lock(file)? {
            LockAttempt::Acquired => return Ok(true),
            LockAttempt::Busy => {
                #[cfg(feature = "test-util")]
                crate::test_util::note_lock_wait();
                wait.next(temporary).await;
            }
            LockAttempt::Unsupported => return Ok(false),
        }
    }
}

/// Whether a test build runs the read inside
/// [`moving_temporary_files`](crate::test_util::moving_temporary_files).
fn moved() -> bool {
    #[cfg(feature = "test-util")]
    let moved = crate::test_util::temporary_files_moved();
    #[cfg(not(feature = "test-util"))]
    let moved = false;
    moved
}

/// One attempt to take the exclusive lock of `file`, as [`IoFile::try_lock`] makes it, or in a
/// test build refused inside [`refusing_locks`](crate::test_util::refusing_locks).
fn try_lock(file: &IoFile) -> std::io::Result<LockAttempt> {
    #[cfg(feature = "test-util")]
    if crate::test_util::locks_refused() {
        return Ok(LockAttempt::Unsupported);
    }
    file.try_lock()
}

/// Finishes the temporary file `temporary` of a resuming read, which `file` has open and locked
/// and holds the whole range: syncs it where `sync_data` asks, removes its resume record, and
/// renames it into place at `target`. Runs as a task of its own that holds `file`, and with it the
/// lock, to the end, so a read dropped while it runs leaves the task to finish, and no other read
/// empties the file before the rename moves it.
///
/// The resume record goes before the rename, while the lock guards the file, so it never names a
/// file no longer at `temporary`. A rename that fails leaves the file without it, so the next read
/// empties it rather than resuming.
pub(crate) fn finish(
    file: IoFile,
    temporary: PathBuf,
    target: &Path,
    sync_data: bool,
) -> JoinHandle<Result<(), StorageError>> {
    let target = target.to_path_buf();
    lore_base::lore_spawn!(async move {
        if sync_data {
            file.sync_data()
                .await
                .map_err(|e| StorageError::internal_with_context(e, "flush file"))?;
        }
        remove_record(&temporary).await;
        #[cfg(feature = "test-util")]
        crate::test_util::rename_point(&temporary).await;
        let renamed = IoDriver::global().rename(&temporary, &target).await;
        drop(file);
        renamed.map_err(|e| {
            StorageError::internal_with_context(
                e,
                &format!("rename {} -> {}", temporary.display(), target.display()),
            )
        })
    })
}

/// Removes the resume record of `temporary`. A failure is logged and fails nothing: the next
/// resuming read to the same path writes it again.
async fn remove_record(temporary: &Path) {
    let record_path = record_path(temporary);
    report_removal(
        &record_path,
        IoDriver::global().remove_file(&record_path).await,
    );
}

/// Logs the removal of `path`, a file a read leaves, where it failed for a reason other than there
/// being nothing to remove.
pub(crate) fn report_removal(path: &Path, removed: std::io::Result<()>) {
    if let Err(err) = removed
        && !matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
    {
        lore_base::lore_warn!("failed to remove {}: {err}", path.display());
    }
}
