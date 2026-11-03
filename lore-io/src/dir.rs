// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;
use std::path::PathBuf;

use crate::Stat;
use crate::pool::SyscallPool;

/// One child of a directory listing.
#[derive(Debug)]
pub struct DirEntry {
    /// The child's name within its directory, not a path.
    pub file_name: OsString,
    /// What the name refers to, with symlinks followed, or `None` where that could not be
    /// resolved: a link whose target is gone, or an entry unlinked between the listing and the
    /// stat. A listing describes what it can rather than failing over one entry.
    pub metadata: Option<Stat>,
    /// Whether the name is itself a link, which `metadata` describes the target of rather than.
    /// A caller that tracks what a name holds rather than what it points at reads this first.
    pub is_symlink: bool,
}

/// Entries one dispatch resolves.
///
/// This is both what a dispatch buys and what the stream holds. On Unix each entry is a stat, so
/// a chunk is that many stats' worth of work against one round trip to the pool — enough at this
/// size that the round trip stops showing against it. On Windows each entry is decoded from a
/// record of the directory search, and only a symbolic link, or a name whose last-write time
/// `SystemTime` can't hold, is stat'd. On either, a chunk of entries is a bounded amount to hold
/// for a directory of any width.
const CHUNK: usize = 256;

/// Directory entries, resolved a chunk at a time.
///
/// A listing reads the directory and describes each child: on Unix a stat per child, which is
/// most of the work, and on Windows from the records the directory search returns, with a stat
/// only for a symbolic link, or a name whose last-write time `SystemTime` can't hold. Both run on
/// the syscall pool, pulled as the consumer asks: the stream holds
/// at most one chunk, so a directory of any width costs the same memory, and a consumer that
/// stops early stops the walk with it.
pub struct DirStream {
    /// The walk between pulls, `None` once it has ended.
    reader: Option<Reader>,
    buffered: std::vec::IntoIter<std::io::Result<DirEntry>>,
}

/// The walk a stream pulls its entries from.
enum Reader {
    /// `std`'s walk: every listing on Unix, and on Windows the paths the search declines.
    Std(StdReadDir),
    /// A search that queries the directory with a larger buffer.
    #[cfg(target_family = "windows")]
    Search(crate::find::Search),
}

#[cfg(not(target_family = "windows"))]
type StdReadDir = std::fs::ReadDir;

/// Boxed on Windows, where `std`'s walk holds a whole find record and lists only the paths the
/// search declines, a cold path that would otherwise make every stream as large.
#[cfg(target_family = "windows")]
type StdReadDir = Box<std::fs::ReadDir>;

/// Opens the directory at `path` and reads the first chunk of its entries.
///
/// Called on a pool thread, so opening and the first chunk are one dispatch rather than two. A
/// directory that cannot be read fails here, with the error `std::fs::read_dir` reports for it.
pub(crate) fn open(path: PathBuf) -> std::io::Result<DirStream> {
    #[cfg(target_family = "windows")]
    let path = {
        let mut first = windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW::default();
        match crate::find::open(path, &mut first)? {
            crate::find::Opened::Search(search) => {
                return Ok(DirStream::holding(read_search_chunk(search, Some(&first))));
            }
            crate::find::Opened::Empty => return Ok(DirStream::holding((None, Vec::new()))),
            crate::find::Opened::Declined(path) => path,
        }
    };
    let reader = std::fs::read_dir(path)?;
    #[cfg(target_family = "windows")]
    let reader = Box::new(reader);
    Ok(DirStream::holding(read_std_chunk(reader)))
}

impl DirStream {
    /// The stream holding `chunk`, with the walk that continues it.
    fn holding((reader, chunk): (Option<Reader>, Vec<std::io::Result<DirEntry>>)) -> DirStream {
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
fn read_chunk(reader: Reader) -> (Option<Reader>, Vec<std::io::Result<DirEntry>>) {
    match reader {
        Reader::Std(reader) => read_std_chunk(reader),
        #[cfg(target_family = "windows")]
        Reader::Search(search) => read_search_chunk(search, None),
    }
}

/// Resolves up to [`CHUNK`] entries from `std`'s walk.
fn read_std_chunk(mut reader: StdReadDir) -> (Option<Reader>, Vec<std::io::Result<DirEntry>>) {
    let mut chunk = Vec::with_capacity(CHUNK);
    for _ in 0..CHUNK {
        let Some(entry) = reader.next() else {
            return (None, chunk);
        };
        chunk.push(entry.map(resolve));
    }
    (Some(Reader::Std(reader)), chunk)
}

/// Resolves up to [`CHUNK`] entries from a search, starting with `first` where it has just
/// opened.
#[cfg(target_family = "windows")]
fn read_search_chunk(
    mut search: crate::find::Search,
    first: Option<&windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW>,
) -> (Option<Reader>, Vec<std::io::Result<DirEntry>>) {
    let mut chunk = Vec::with_capacity(CHUNK);
    let more = search.fill(first, &mut chunk, CHUNK);
    (more.then_some(Reader::Search(search)), chunk)
}

/// Describes what a name refers to, following a link to its target.
fn resolve(entry: std::fs::DirEntry) -> DirEntry {
    let (metadata, is_symlink) = match entry.metadata() {
        // `DirEntry::metadata` does not follow links, so a link is stat'd again by path to
        // describe the target the name stands for.
        Ok(metadata) if metadata.is_symlink() => (crate::psync::std_stat(&entry.path()).ok(), true),
        Ok(metadata) => (Some(Stat::from(&metadata)), false),
        Err(_) => (None, false),
    };
    DirEntry {
        file_name: entry.file_name(),
        metadata,
        is_symlink,
    }
}
