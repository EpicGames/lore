// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The deletion list: the repository-relative paths of tracked entries deleted through the
//! mount, one per line in `<external dir>/deleted`. The State built at mount detaches each
//! listed path the mounted revision holds. Appends go through one descriptor kept open with
//! `O_APPEND` and are never synced, so a crash may lose the last ones, whose paths then
//! reappear.

// The provider owns its deletion list and writes it outside any repository write token.
#![allow(clippy::disallowed_methods)]

use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::IoSlice;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::lore_warn;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

/// The deletion list's file name in the instance's external directory.
pub const DELETION_LIST: &str = "deleted";

/// The suffix of the file a rewrite fills before it replaces the list.
#[lore_macro::test_pub]
const REWRITE_SUFFIX: &str = ".new";

/// Spins a rewrite takes waiting for appends under way before it yields its thread instead.
const APPEND_WAIT_SPINS: u32 = 128;

/// One instance's deletion list, open for appending.
#[lore_macro::test_pub]
pub struct DeletionList {
    path: PathBuf,
    /// Where [`Self::rewrite`] writes the next list before renaming it over `path`.
    rewrite_path: PathBuf,
    /// Opened with `O_APPEND`, so each append lands whole at the end of the list.
    file: File,
    /// Which slot of `appending` an append counts itself in: the low bit of this count.
    epoch: AtomicUsize,
    /// Appends under way, by the epoch they began in. A rewrite that has pointed `file` at the
    /// next list moves the epoch on and waits for the previous epoch's slot to drain before it
    /// reads the previous list, so no append that began on it lands after the read, and appends
    /// that begin meanwhile do not hold the rewrite up.
    appending: [AtomicUsize; 2],
    /// Moved once each append or rewrite is written ([`Self::changes`]).
    changes: AtomicU64,
}

impl DeletionList {
    /// Opens the list at `path` for appending, creating it empty where absent. A final line
    /// without its line feed, which only a crash leaves, is dropped. Blocking.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = open_append(path, true)?;
        let listed = std::fs::read(path)?;
        let whole = whole_lines(&listed).len();
        if whole < listed.len() {
            file.set_len(whole as u64)?;
        }
        let mut rewrite_path = OsString::from(path.as_os_str());
        rewrite_path.push(REWRITE_SUFFIX);
        Ok(Self {
            path: path.to_path_buf(),
            rewrite_path: rewrite_path.into(),
            file,
            epoch: AtomicUsize::new(0),
            appending: [AtomicUsize::new(0), AtomicUsize::new(0)],
            changes: AtomicU64::new(0),
        })
    }

    /// Appends `path` as one line with a line feed before and after it, in one write that
    /// allocates nothing, so a short write never joins two paths. A path holding a line feed,
    /// or the empty root path, is refused with `InvalidInput`. Blocking.
    pub fn append(&self, path: &RelativePath) -> io::Result<()> {
        let line = listable(path)?;
        let slot = self.enter_epoch();
        let written = self.write_line(line);
        slot.fetch_sub(1, Ordering::SeqCst);
        self.changes.fetch_add(1, Ordering::SeqCst);
        written
    }

    /// Moved once each append or rewrite is written: a read of the list after taking this holds
    /// every path appended before, so the same count later means nothing was appended since.
    pub fn changes(&self) -> u64 {
        self.changes.load(Ordering::SeqCst)
    }

    /// Every path the list holds, in the order appended, each exactly as appended. Lines that
    /// are not UTF-8 are skipped, as is a final line without its line feed, with one warning
    /// counting them. Blocking.
    pub fn read(&self) -> io::Result<Vec<RelativePath>> {
        let listed = std::fs::read(&self.path)?;
        let whole = whole_lines(&listed);
        let mut dropped = usize::from(whole.len() < listed.len());
        let paths = whole
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let line = std::str::from_utf8(line).ok();
                dropped += usize::from(line.is_none());
                line.map(exact_path)
            })
            .collect();
        if dropped > 0 {
            lore_warn!(
                "FUSE deletion list {}: skipping {dropped} lines that name no path",
                self.path.display()
            );
        }
        Ok(paths)
    }

    /// The list's length in bytes. A caller takes it before it reads the list or works out what
    /// the list should hold, and passes it to [`Self::rewrite`], which keeps what is appended
    /// past it. Taken while an append is under way, it may end inside the appended line, as on a
    /// file system that grows a file page by page during one write. Blocking.
    pub fn length(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Replaces the first `covered` bytes of the list ([`Self::length`]) with `paths` and keeps
    /// the lines appended past them. A new file is written beside the list and the kept
    /// descriptor is pointed at it; once the appends that began on the old list are done, its
    /// lines past `covered`, with the line `covered` ends inside whole, are copied to the new
    /// one, whose data is synced before it is renamed over the list. The list on disk is the old
    /// one or the new one, whole, and every append is in the list that stays. One rewrite runs at
    /// a time. Refuses a path [`Self::append`] refuses, before anything changes. Blocking.
    pub fn rewrite(&self, paths: &[RelativePath], covered: u64) -> io::Result<()> {
        let rewritten = self.rewrite_list(paths, covered);
        self.changes.fetch_add(1, Ordering::SeqCst);
        rewritten
    }

    fn rewrite_list(&self, paths: &[RelativePath], covered: u64) -> io::Result<()> {
        let mut content = Vec::with_capacity(paths.iter().map(|path| path.len() + 1).sum());
        for path in paths {
            content.extend_from_slice(listable(path)?);
            content.push(b'\n');
        }
        std::fs::write(&self.rewrite_path, &content)?;
        self.point_at(&self.rewrite_path)?;
        let replaced = self
            .carry_lines(&self.path, covered)
            .and_then(|()| self.file.sync_data())
            .and_then(|()| std::fs::rename(&self.rewrite_path, &self.path));
        let Err(error) = replaced else {
            return Ok(());
        };
        // The list on disk is the old one: appends go back to it, with those the new one took.
        let restored = self
            .point_at(&self.path)
            .and_then(|()| self.carry_lines(&self.rewrite_path, content.len() as u64));
        match restored {
            Ok(()) => Err(error),
            Err(restore) => Err(io::Error::new(
                restore.kind(),
                format!("{error}; appending to the list again failed: {restore}"),
            )),
        }
    }

    /// Writes `line` between two line feeds through the kept descriptor.
    fn write_line(&self, line: &[u8]) -> io::Result<()> {
        let pieces: [&[u8]; 3] = [b"\n", line, b"\n"];
        let mut written = (&self.file).write_vectored(&pieces.map(IoSlice::new))?;
        for piece in pieces {
            let skip = written.min(piece.len());
            written -= skip;
            (&self.file).write_all(&piece[skip..])?;
        }
        Ok(())
    }

    /// Waits for the appends that began on the file the kept descriptor last referred to, then
    /// appends the whole lines the file at `from` holds past `covered` through it, from the
    /// start of the line `covered` ends inside.
    fn carry_lines(&self, from: &Path, covered: u64) -> io::Result<()> {
        self.drain_epoch();
        let old = std::fs::read(from)?;
        (&self.file).write_all(whole_lines(lines_from(&old, covered)))
    }

    /// Counts an append in the slot of the current epoch and answers the slot. The epoch is
    /// read again once counted, so an append counted in a slot a rewrite has stopped taking
    /// counts itself again in the next.
    fn enter_epoch(&self) -> &AtomicUsize {
        loop {
            let epoch = self.epoch.load(Ordering::SeqCst);
            let slot = &self.appending[epoch & 1];
            slot.fetch_add(1, Ordering::SeqCst);
            if self.epoch.load(Ordering::SeqCst) == epoch {
                return slot;
            }
            slot.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Moves the epoch on and waits until no append of the previous one is under way. Every
    /// append that began before the call is counted in that slot; those that begin after count
    /// in the other, so the wait ends however steadily appends arrive.
    fn drain_epoch(&self) {
        let previous = self.epoch.fetch_add(1, Ordering::SeqCst);
        let slot = &self.appending[previous & 1];
        #[cfg(feature = "test-util")]
        if slot.load(Ordering::SeqCst) != 0 {
            test_util::DRAINS_WAITED.with(|drains| drains.set(drains.get() + 1));
        }
        let mut spins = 0;
        while slot.load(Ordering::SeqCst) != 0 {
            if spins < APPEND_WAIT_SPINS {
                spins += 1;
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    /// Makes the kept descriptor refer to the file at `path`, opened for appending, and keeps
    /// it close-on-exec.
    fn point_at(&self, path: &Path) -> io::Result<()> {
        let next = open_append(path, false)?;
        // SAFETY: both descriptors are open and owned by `File`s alive across the call; `dup3`
        // replaces what the kept descriptor number refers to in one step and closes nothing
        // `next` owns.
        let replaced =
            unsafe { libc::dup3(next.as_raw_fd(), self.file.as_raw_fd(), libc::O_CLOEXEC) };
        if replaced < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn open_append(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new().append(true).create(create).open(path)
}

/// `listed` from the start of the line its first `covered` bytes end inside, or from
/// `covered` where they end a line: a tail of a line is never a path of its own.
fn lines_from(listed: &[u8], covered: u64) -> &[u8] {
    let covered =
        usize::try_from(covered).map_or(listed.len(), |covered| covered.min(listed.len()));
    let start = listed[..covered]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |end| end + 1);
    &listed[start..]
}

/// `listed` up to and with its last line feed.
#[lore_macro::test_pub]
fn whole_lines(listed: &[u8]) -> &[u8] {
    let whole = listed
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |end| end + 1);
    &listed[..whole]
}

/// The bytes of `path` as one line of the list.
fn listable(path: &RelativePath) -> io::Result<&[u8]> {
    let line = path.as_str().as_bytes();
    if line.is_empty() || line.contains(&b'\n') {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(line)
}

/// The path holding exactly `line`, as [`DeletionList::append`] wrote it: a constructor that
/// cleans a path would read back another than was appended.
#[lore_macro::test_pub]
fn exact_path(line: &str) -> RelativePath {
    let mut path = RelativePathBuf::with_capacity(line.len());
    path.push(line);
    path.freeze()
}

/// Drains that waited, counted per thread for tests. Not a field of the list: a mount's futures
/// hold the list by value, and a field would grow them in test builds.
#[cfg(feature = "test-util")]
pub mod test_util {
    use std::cell::Cell;

    thread_local! {
        pub(super) static DRAINS_WAITED: Cell<usize> = const { Cell::new(0) };
    }

    /// Rewrites on this thread whose drain found an append of the previous epoch under way.
    pub fn drains_waited() -> usize {
        DRAINS_WAITED.with(Cell::get)
    }
}
