// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;

use crate::pool::SyscallPool;

/// One child of a directory listing.
#[derive(Debug)]
pub struct DirEntry {
    /// The child's name within its directory, not a path.
    pub file_name: OsString,
    /// What the name refers to, with symlinks followed, or `None` where that could not be
    /// resolved: a link whose target is gone, or an entry unlinked between the listing and the
    /// stat. A listing describes what it can rather than failing over one entry.
    pub metadata: Option<std::fs::Metadata>,
}

/// Entries one dispatch resolves.
///
/// This is both what a dispatch buys and what the stream holds. Each entry carries a stat, so a
/// chunk is that many stats' worth of work against one round trip to the pool — enough at this
/// size that the round trip stops showing against it, while a chunk of entries remains a bounded
/// amount to hold for a directory of any width.
const CHUNK: usize = 256;

/// Directory entries, resolved a chunk at a time.
///
/// A listing is one `getdents` plus a stat per child, and the stats are most of it. Both run on
/// the syscall pool, pulled as the consumer asks: the stream holds at most one chunk, so a
/// directory of any width costs the same memory, and a consumer that stops early stops the walk
/// with it.
pub struct DirStream {
    /// The walk between pulls, `None` once it has ended.
    reader: Option<std::fs::ReadDir>,
    buffered: std::vec::IntoIter<std::io::Result<DirEntry>>,
}

impl DirStream {
    /// Reads the first chunk and returns the stream holding it.
    ///
    /// Called on the pool thread that opened the directory, so opening and the first chunk are
    /// one dispatch rather than two.
    pub(crate) fn start(reader: std::fs::ReadDir) -> DirStream {
        let (reader, chunk) = read_chunk(reader);
        DirStream {
            reader,
            buffered: chunk.into_iter(),
        }
    }

    /// The next entry, or `None` once the directory is exhausted.
    ///
    /// An entry the walk could not read is yielded as an error rather than skipped, leaving a
    /// consumer to decide: a listing may reasonably ignore one, and a scan deciding what changed
    /// may not, since skipping is indistinguishable from the name not being there.
    pub async fn next(&mut self) -> Option<std::io::Result<DirEntry>> {
        loop {
            if let Some(entry) = self.buffered.next() {
                return Some(entry);
            }
            let reader = self.reader.take()?;
            let (reader, chunk) = SyscallPool::global()
                .submit(move || read_chunk(reader))
                .await;
            if chunk.is_empty() {
                return None;
            }
            self.reader = reader;
            self.buffered = chunk.into_iter();
        }
    }
}

/// Resolves up to [`CHUNK`] entries, handing the walk back when it has more to give.
fn read_chunk(
    mut reader: std::fs::ReadDir,
) -> (Option<std::fs::ReadDir>, Vec<std::io::Result<DirEntry>>) {
    let mut chunk = Vec::with_capacity(CHUNK);
    for _ in 0..CHUNK {
        let Some(entry) = reader.next() else {
            return (None, chunk);
        };
        chunk.push(entry.map(resolve));
    }
    (Some(reader), chunk)
}

/// Describes what a name refers to, following a link to its target.
fn resolve(entry: std::fs::DirEntry) -> DirEntry {
    let metadata = match entry.metadata() {
        // `DirEntry::metadata` does not follow links, so a link is stat'd again by path to
        // describe the target the name stands for.
        Ok(metadata) if metadata.is_symlink() => std::fs::metadata(entry.path()).ok(),
        Ok(metadata) => Some(metadata),
        Err(_) => None,
    };
    DirEntry {
        file_name: entry.file_name(),
        metadata,
    }
}
