// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The walk [`super::clone_into`], [`super::measure`] and [`super::find_directories`] share.
//! Each directory is listed by one of the calling thread and the threads the syscall pool lends
//! it, and the directories it holds are queued for the next free thread, the last found first.
//! The queue holds at most 4,096 directories, and a thread lists at most 256 entries of a directory
//! at a time before handing the rest of it on as a continuation. A continuation holds its directory
//! open, through one descriptor on Unix, and the queue holds 64 of them at most: with that many
//! queued, a thread lists the rest of its directory itself. A thread that finds a directory the
//! queue has no room for lists it itself, depth-first. So the walk holds the queue's cap, the
//! continuations' cap with each continuation's open directory and, for each thread, one open
//! directory for each level it has descended and one batch of the directories it found, whatever
//! the number of files or the width of a directory.
//!
//! The walk benchmark (`lore-io/BENCHMARKS.md`, *The walk's bounds*) chose the bounds: with them,
//! the copy is no slower than the unbounded walk on its four trees, and batches of 1,024 or 4,096
//! and queues of 2,048 to 65,536 are no faster. A step is granted room for a whole batch, so the
//! cap leaves room for the batches of the up to nine threads a default pool runs a walk on. A
//! continuation goes back on the queue only once its batch is done: threads that create entries
//! in one directory at once wait for each other on its lock, which measured a third slower over a
//! tree of wide directories.
//!
//! A continuation keeps its directory open. On Unix it holds the `readdir` stream, which owns the
//! directory's only descriptor, so the thread that takes the continuation lists on without
//! opening the directory or checking its identity again. On Windows it holds the directory's
//! handle, which keeps the directory from being renamed or removed, and the open search of its
//! entries.

use std::cell::Cell;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread::Scope;
use std::time::Duration;
use std::time::Instant;

use parking_lot::Condvar;
use parking_lot::Mutex;
use parking_lot::MutexGuard;

use super::Backend;
use super::CloneStats;
#[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
use super::FaithfulCopy;
use super::Kept;
use super::Progress;
use super::TreeSize;
use super::Visit;
use super::apfs;
use super::copy;
use super::entry;
use super::source::OpenDirectory;
use super::source::Source;
use super::source::is_gone_kind;

/// What a listing says an entry is, a symbolic link not followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    File,
    /// A symbolic link, or on Windows a junction or another name-surrogate reparse point.
    Symlink,
    /// A socket, a pipe or a device.
    Other,
}

/// What a walk decides for each entry, given its path, its path relative to the root walked, and
/// its kind.
pub type VisitFn<'a> = dyn Fn(&Path, &Path, Kind) -> Visit + Sync + 'a;

/// What lists one step of a directory of a walk, handing the directories it finds to [`Found`].
type ListFn<'a, J> = dyn Fn(J, &mut Found<'_, J>) -> io::Result<()> + Sync + 'a;

/// How much a walk holds at most: the directories its queue holds, the continuations it holds,
/// each with its open directory, and the entries one of its threads lists of a directory at a
/// time.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Bounds {
    queue: usize,
    continuations: usize,
    batch: usize,
}

/// The bounds of every walk, chosen by the walk benchmark that `lore-io/BENCHMARKS.md` reports.
pub(super) const BOUNDS: Bounds = Bounds {
    queue: 4096,
    continuations: 64,
    batch: 256,
};

#[cfg(feature = "test-util")]
impl Default for Bounds {
    fn default() -> Self {
        BOUNDS
    }
}

/// What a test build counts of a walk, in the [`Progress`] it was given: the most it held at once,
/// its continuations, and the flushes of its file system and of its files.
#[cfg(feature = "test-util")]
#[derive(Debug, Default)]
pub(super) struct Counters {
    queued: std::sync::atomic::AtomicUsize,
    continuations: std::sync::atomic::AtomicUsize,
    kept: std::sync::atomic::AtomicUsize,
    handed: std::sync::atomic::AtomicUsize,
    continued: std::sync::atomic::AtomicUsize,
    pub(super) flushed: std::sync::atomic::AtomicUsize,
    pub(super) flushed_files: std::sync::atomic::AtomicUsize,
    pub(super) value_held: std::sync::atomic::AtomicUsize,
}

/// What [`Counters`] counted.
#[cfg(feature = "test-util")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// The most directories the queue held.
    pub queued: usize,
    /// The most continuations the queue held.
    pub continuations: usize,
    /// The most directories one thread kept for itself.
    pub kept: usize,
    /// The most directories one step handed on: the batch a thread holds.
    pub handed: usize,
    /// The steps that left part of a directory to a later step.
    pub continued: usize,
    /// The flushes of the copy's file system after its last file.
    pub flushed: usize,
    /// The files flushed one by one, each on its copying thread before its handle closes, on
    /// macOS and Windows.
    pub flushed_files: usize,
    /// The most bytes of an extended attribute's value a faithful copy held at once, on Linux and
    /// macOS.
    pub value_held: usize,
}

#[cfg(feature = "test-util")]
impl Counters {
    /// What was counted.
    pub(super) fn stats(&self) -> Counts {
        Counts {
            queued: self.queued.load(Ordering::Relaxed),
            continuations: self.continuations.load(Ordering::Relaxed),
            kept: self.kept.load(Ordering::Relaxed),
            handed: self.handed.load(Ordering::Relaxed),
            continued: self.continued.load(Ordering::Relaxed),
            flushed: self.flushed.load(Ordering::Relaxed),
            flushed_files: self.flushed_files.load(Ordering::Relaxed),
            value_held: self.value_held.load(Ordering::Relaxed),
        }
    }
}

/// The directories a walk has found and not listed yet, how many of its threads list one, how
/// many wait for one, and how the walk ended.
struct Queue<J> {
    waiting: Vec<J>,
    /// The continuations waiting, each with its open directory, listed once `waiting` is empty.
    continuations: Vec<J>,
    /// Threads with a directory to list, taken from the queue or kept for themselves.
    listing: usize,
    idle: usize,
    /// Room in `waiting` that the steps being listed hold for the directories they find.
    granted: usize,
    /// Room in `continuations` that the steps being listed hold, one each at most.
    continuations_granted: usize,
    /// When the walk last asked for another thread.
    grown: Option<Instant>,
    ended: bool,
    failure: Option<io::Error>,
}

/// How often at most a walk with more directories waiting than threads free asks for another
/// thread.
const GROW_INTERVAL: Duration = Duration::from_millis(10);

/// A running walk: its queue and its caps, how it lists a directory and gets its threads.
struct Walk<'a, J> {
    queue: Mutex<Queue<J>>,
    ready: Condvar,
    /// The most directories `queue` holds, the room it has granted included.
    cap: usize,
    /// The most continuations `queue` holds, the room it has granted included.
    continuations_cap: usize,
    /// The room a step is granted where the queue has it: one for each entry of a batch.
    step_room: usize,
    progress: &'a Progress,
    list: &'a ListFn<'a, J>,
    /// Takes up to the threads it is asked for, beyond the calling thread, and says how many it
    /// took.
    more: &'a (dyn Fn(usize) -> usize + Sync),
    /// Gives back a thread taken that the system refused to start.
    refused: &'a (dyn Fn() + Sync),
}

/// The directories one step of a walk finds. It queues them for any thread within the room the
/// walk granted the step, and keeps the rest for the thread listing the step, which lists the one
/// kept last first.
#[lore_macro::test_pub]
pub(super) struct Found<'a, J> {
    queued: &'a mut Vec<J>,
    /// The rest of the step's directory, queued beneath `queued`.
    rest: &'a mut Option<J>,
    kept: &'a mut Vec<J>,
    room: usize,
    /// Whether the step has room to queue the rest of its directory.
    rest_room: bool,
    progress: &'a Progress,
}

impl<J> Found<'_, J> {
    /// Queues `directory` once the step ends, where the step has room, and otherwise hands it
    /// back.
    #[lore_macro::test_pub]
    pub(super) fn offer(&mut self, directory: J) -> Result<(), J> {
        if self.room == 0 {
            return Err(directory);
        }
        if self.queued.capacity() == 0 {
            self.queued.reserve_exact(self.room);
        }
        self.room -= 1;
        self.queued.push(directory);
        Ok(())
    }

    /// Queues `rest`, the rest of the step's directory, once the step ends, where the queue has
    /// room for another continuation, and otherwise hands it back.
    pub(super) fn offer_rest(&mut self, rest: J) -> Result<(), J> {
        if !self.rest_room {
            return Err(rest);
        }
        self.rest_room = false;
        *self.rest = Some(rest);
        Ok(())
    }

    /// Keeps `directory` for this thread, which lists it before the directories kept before it.
    #[lore_macro::test_pub]
    pub(super) fn keep(&mut self, directory: J) {
        self.kept.push(directory);
    }
}

/// Lists `root` with `list`, and every directory `list` hands on, on up to `threads` threads,
/// within [`BOUNDS`]. The first failure ends the walk, once the directories being listed are
/// done, and is the one reported, unless it is only another thread stopping because the walk was
/// asked to stop, which a later failure replaces.
#[cfg(feature = "test-util")]
pub fn run<J: Send>(root: J, threads: usize, list: &ListFn<'_, J>) -> io::Result<()> {
    let left = Mutex::new(threads.saturating_sub(1));
    run_with(
        root,
        (BOUNDS.queue, BOUNDS.continuations),
        &Progress::default(),
        &|wanted| {
            let mut left = left.lock();
            let given = wanted.min(*left);
            *left -= given;
            given
        },
        &|| {},
        list,
    )
}

/// [`run`] on the calling thread alone, with no queue, so that every directory it finds is
/// listed depth-first.
fn run_alone<J: Send>(root: J, progress: &Progress, list: &ListFn<'_, J>) -> io::Result<()> {
    run_with(root, (0, 0), progress, &|_| 0, &|| {}, list)
}

/// [`run`] with a queue of at most `cap` directories and the batches of `progress`'s bounds, on
/// the calling thread, which lists `root`, and on the threads `more` takes: as many as it gives
/// when the walk starts, and one more each time it gives one while more directories wait than
/// threads are free, asked at most every [`GROW_INTERVAL`]. `refused` gives back each thread the
/// system refuses to start, whose share of the walk the other threads take.
///
/// Each step of a directory is granted room in the queue for every directory one batch can find,
/// and for the batch's continuation, where the queue has it. A step hands each directory it finds
/// to the queue within that room, and keeps the others for its thread, which lists them
/// depth-first, so no thread waits for room and the queue never holds more than `caps`: so many
/// directories, and so many continuations. The directories found last are listed first, so the
/// queue holds the directories beside the paths being walked rather than a whole level of the
/// tree, and the continuations once none of those waits.
fn run_with<J: Send>(
    root: J,
    (cap, continuations_cap): (usize, usize),
    progress: &Progress,
    more: &(dyn Fn(usize) -> usize + Sync),
    refused: &(dyn Fn() + Sync),
    list: &ListFn<'_, J>,
) -> io::Result<()> {
    let walk = Walk {
        queue: Mutex::new(Queue {
            waiting: Vec::new(),
            continuations: Vec::new(),
            listing: 1,
            idle: 0,
            granted: 0,
            continuations_granted: 0,
            grown: None,
            ended: false,
            failure: None,
        }),
        ready: Condvar::new(),
        cap,
        continuations_cap,
        step_room: progress.bounds().batch,
        progress,
        list,
        more,
        refused,
    };
    std::thread::scope(|scope| {
        for _ in 0..more(usize::MAX) {
            spawn_walker(scope, &walk);
        }
        walk.take_and_list(Some(root), &|| grow(scope, &walk));
    });
    match walk.queue.into_inner().failure {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

/// Starts a thread of `walk` in `scope`, giving the thread back where the system refuses it.
fn spawn_walker<'scope, 'env, J: Send>(
    scope: &'scope Scope<'scope, 'env>,
    walk: &'scope Walk<'env, J>,
) {
    let started = std::thread::Builder::new()
        .name("lore-io-clone".to_string())
        .spawn_scoped(scope, move || {
            walk.take_and_list(None, &|| grow(scope, walk));
        });
    if started.is_err() {
        (walk.refused)();
    }
}

/// Starts one more thread of `walk` where one is given.
fn grow<'scope, 'env, J: Send>(scope: &'scope Scope<'scope, 'env>, walk: &'scope Walk<'env, J>) {
    if (walk.more)(1) == 1 {
        spawn_walker(scope, walk);
    }
}

impl<J> Walk<'_, J> {
    /// One thread of the walk: lists `first`, then the directories it keeps, the last kept
    /// first, and then directories from the queue, until none is waiting and no thread lists one,
    /// or the walk has failed, calling `grow` where more directories wait than threads are free.
    fn take_and_list(&self, mut first: Option<J>, grow: &dyn Fn()) {
        let mut kept = Vec::new();
        let mut queued = Vec::new();
        let mut rest = None;
        let mut state = self.queue.lock();
        loop {
            if state.ended {
                return;
            }
            let directory = if let Some(directory) = first.take().or_else(|| kept.pop()) {
                directory
            } else {
                let Some(directory) = state.waiting.pop().or_else(|| state.continuations.pop())
                else {
                    if state.listing == 0 {
                        end(&mut state, &self.ready, None);
                        return;
                    }
                    state.idle += 1;
                    self.ready.wait(&mut state);
                    state.idle -= 1;
                    continue;
                };
                state.listing += 1;
                directory
            };
            let room = self
                .cap
                .saturating_sub(state.waiting.len() + state.granted)
                .min(self.step_room);
            state.granted += room;
            let rest_room =
                state.continuations.len() + state.continuations_granted < self.continuations_cap;
            state.continuations_granted += usize::from(rest_room);
            // A panic in `list` fails the walk, where it would leave `listing` counting a
            // thread that lists nothing and the other threads waiting for it.
            let listed = MutexGuard::unlocked(&mut state, || {
                let mut found = Found {
                    queued: &mut queued,
                    rest: &mut rest,
                    kept: &mut kept,
                    room,
                    rest_room,
                    progress: self.progress,
                };
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (self.list)(directory, &mut found)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("a walk thread panicked")))
            });
            state.granted -= room;
            state.continuations_granted -= usize::from(rest_room);
            if let Err(failure) = listed {
                end(&mut state, &self.ready, Some(failure));
                return;
            }
            let count = queued.len() + usize::from(rest.is_some());
            #[cfg(feature = "test-util")]
            hold(&self.progress.counters.handed, count);
            state.continuations.extend(rest.take());
            state.waiting.append(&mut queued);
            for _ in 0..count {
                self.ready.notify_one();
            }
            #[cfg(feature = "test-util")]
            {
                hold(&self.progress.counters.queued, state.waiting.len());
                hold(
                    &self.progress.counters.continuations,
                    state.continuations.len(),
                );
                hold(&self.progress.counters.kept, kept.len());
            }
            if kept.is_empty() {
                state.listing -= 1;
            }
            if state.waiting.len() + state.continuations.len() > state.idle
                && state
                    .grown
                    .is_none_or(|grown| grown.elapsed() >= GROW_INTERVAL)
            {
                state.grown = Some(Instant::now());
                MutexGuard::unlocked(&mut state, grow);
            }
        }
    }
}

/// Ends the walk with `failure`, the first one recorded unless that one is only another thread
/// stopping because the walk was asked to stop, which the failure that caused the stop replaces.
fn end<J>(state: &mut Queue<J>, ready: &Condvar, failure: Option<io::Error>) {
    state.ended = true;
    let stopped = state
        .failure
        .as_ref()
        .is_some_and(|recorded| recorded.kind() == io::ErrorKind::Interrupted);
    if failure.is_some() && (state.failure.is_none() || stopped) {
        state.failure = failure;
    }
    ready.notify_all();
}

/// [`run`] on the calling thread and on the threads the syscall pool lends, within `progress`'s
/// bounds, as [`run_lent_on`] describes.
fn run_lent<J: Send>(root: J, progress: &Progress, list: &ListFn<'_, J>) -> io::Result<()> {
    run_lent_on(crate::pool::SyscallPool::global(), root, progress, list)
}

/// [`run`] within `progress`'s bounds, on the calling thread and on threads of `pool`'s cap it
/// lends: as many as it lends when the walk starts, and more as it lends them while the walk has
/// more directories waiting than threads free, so a walk that starts while the pool is busy takes
/// the threads the pool frees later. The pool lends at most half its cap, so the walk never takes
/// the threads every other file operation in the process runs on, and the threads lent and the
/// pool's own stay within the pool's cap; the walk's calling thread is its caller's. A thread the
/// system refuses to start is given back at once.
#[lore_macro::test_pub]
fn run_lent_on<J: Send>(
    pool: &crate::pool::SyscallPool,
    root: J,
    progress: &Progress,
    list: &ListFn<'_, J>,
) -> io::Result<()> {
    let lent = Mutex::new(pool.lend(0));
    run_with(
        root,
        (progress.bounds().queue, progress.bounds().continuations),
        progress,
        &|wanted| {
            let more = pool.lend(wanted);
            let count = more.count();
            lent.lock().absorb(more);
            count
        },
        &|| lent.lock().give_back(1),
        list,
    )
}

/// An entry a listing found.
struct Listed {
    name: OsString,
    kind: Kind,
    #[cfg(target_family = "windows")]
    len: Option<u64>,
}

/// The length of the file `listed` names, which `source` reaches.
#[cfg(target_family = "unix")]
fn len_of(_listed: &Listed, source: Source<'_>) -> io::Result<u64> {
    source.len()
}

/// The length of the file `listed` names, which its listing gave.
#[cfg(target_family = "windows")]
fn len_of(listed: &Listed, _source: Source<'_>) -> io::Result<u64> {
    listed
        .len
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
}

/// The entries of an open directory that a walk has not listed yet: on Unix nothing of its own,
/// since the open directory reads its entries itself.
#[cfg(target_family = "unix")]
struct Listing;

#[cfg(target_family = "unix")]
impl Listing {
    /// The listing of `directory`.
    #[allow(clippy::unnecessary_wraps)]
    fn open(_directory: &OpenDirectory, _path: &Path) -> io::Result<Self> {
        Ok(Self)
    }

    /// The next entry of `directory`, the directory listed, `None` at the end.
    #[allow(clippy::unused_self)]
    fn next(&mut self, directory: &mut OpenDirectory) -> Option<io::Result<Listed>> {
        Some(directory.next()?.map(|(name, kind)| Listed { name, kind }))
    }
}

/// The entries of a directory that a walk has not listed yet, from a search that queries the
/// directory with a larger buffer where the search takes its path, and from `std` otherwise.
#[cfg(target_family = "windows")]
enum Listing {
    Search {
        search: crate::find::Search,
        chunk: std::vec::IntoIter<io::Result<crate::DirEntry>>,
        more: bool,
    },
    /// Boxed, as `std`'s listing holds a whole search record, and serves only the paths the
    /// search declines.
    Std(Box<std::fs::ReadDir>),
    Empty,
}

#[cfg(target_family = "windows")]
impl Listing {
    /// Opens the listing of the directory at `path`.
    fn open(_directory: &OpenDirectory, path: &Path) -> io::Result<Self> {
        let mut first = windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW::default();
        Ok(match crate::find::open(path.to_path_buf(), &mut first)? {
            crate::find::Opened::Search(mut search) => {
                let mut chunk = Vec::new();
                let more = search.fill(Some(&first), &mut chunk, SEARCH_CHUNK);
                Self::Search {
                    search,
                    chunk: chunk.into_iter(),
                    more,
                }
            }
            crate::find::Opened::Empty => Self::Empty,
            crate::find::Opened::Declined(path) => Self::Std(Box::new(std::fs::read_dir(path)?)),
        })
    }

    /// The next entry, `None` at the end.
    fn next(&mut self, _directory: &mut OpenDirectory) -> Option<io::Result<Listed>> {
        match self {
            Self::Search {
                search,
                chunk,
                more,
            } => loop {
                if let Some(entry) = chunk.next() {
                    return Some(entry.map(found_entry));
                }
                if !*more {
                    return None;
                }
                let mut next = Vec::new();
                *more = search.fill(None, &mut next, SEARCH_CHUNK);
                *chunk = next.into_iter();
            },
            Self::Std(entries) => Some(entries.next()?.and_then(|entry| {
                let metadata = entry.metadata()?;
                Ok(Listed {
                    name: entry.file_name(),
                    kind: kind_of(metadata.file_type()),
                    len: Some(metadata.len()),
                })
            })),
            Self::Empty => None,
        }
    }
}

/// Entries a search lists into memory at a time.
#[cfg(target_family = "windows")]
const SEARCH_CHUNK: usize = 256;

#[cfg(target_family = "windows")]
fn found_entry(entry: crate::DirEntry) -> Listed {
    let kind = if entry.is_symlink {
        Kind::Symlink
    } else {
        match entry.metadata.as_ref().map(|stat| stat.kind) {
            Some(crate::StatKind::Directory) => Kind::Directory,
            Some(crate::StatKind::File) => Kind::File,
            Some(crate::StatKind::Other) | None => Kind::Other,
        }
    };
    Listed {
        name: entry.file_name,
        kind,
        len: entry.metadata.map(|stat| stat.len),
    }
}

#[cfg(target_family = "windows")]
fn kind_of(file_type: std::fs::FileType) -> Kind {
    if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_dir() {
        Kind::Directory
    } else if file_type.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

/// `err` with `path` in its message, of the same kind.
fn with_path(err: io::Error, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}

thread_local! {
    /// The path buffers a thread lists directories with, kept from one directory to the next so
    /// that the paths of a directory cost no allocation. A directory listed inside another on the
    /// same thread takes new buffers.
    static PATHS: Cell<Option<(PathBuf, PathBuf)>> = const { Cell::new(None) };
}

/// Runs `with` on this thread's path buffers, and keeps them for the thread's next directory.
fn with_paths<R>(with: impl FnOnce(&mut PathBuf, &mut PathBuf) -> R) -> R {
    let (mut from, mut to) = PATHS.take().unwrap_or_default();
    let done = with(&mut from, &mut to);
    PATHS.set(Some((from, to)));
    done
}

/// Sets `buffer` to `relative` below `root`, keeping its allocation.
fn set_below(buffer: &mut PathBuf, root: &Path, relative: &Path) {
    buffer.as_mut_os_string().clear();
    buffer.push(root);
    if !relative.as_os_str().is_empty() {
        buffer.push(relative);
    }
}

/// A directory of a walk: its path relative to the root walked, the identity it had when its
/// parent listed it, which the root has none of, what the walk knows of it beside, `of`, and,
/// once a step has listed part of it, the rest.
struct Directory<D> {
    relative: PathBuf,
    identity: Option<entry::Identity>,
    rest: Option<Rest>,
    of: D,
}

/// A directory a step has opened and listed part of: the directory, held open, and the rest of
/// its listing, so a later step on any thread lists it on without opening it again.
struct Rest {
    directory: OpenDirectory,
    listing: Listing,
}

impl<D: Copy> Directory<D> {
    /// The root of a walk.
    fn root(of: D) -> Self {
        Self {
            relative: PathBuf::new(),
            identity: None,
            rest: None,
            of,
        }
    }

    /// The directory `source` names, at `relative`, unless it is gone or no longer a directory.
    fn below(source: Source<'_>, relative: &Path, of: D) -> io::Result<Option<Self>> {
        match source.directory() {
            Ok(Some(directory)) => Ok(Some(Self {
                relative: relative.to_path_buf(),
                identity: Some(directory.identity),
                rest: None,
                of,
            })),
            Ok(None) => Ok(None),
            Err(err) if is_gone_kind(err.kind()) => Ok(None),
            Err(err) => Err(with_path(err, &source)),
        }
    }

    /// The rest of this directory, at `relative`, for a later step.
    fn continued(relative: PathBuf, rest: Rest, of: D, progress: &Progress) -> Self {
        #[cfg(feature = "test-util")]
        progress.counters.continued.fetch_add(1, Ordering::Relaxed);
        #[cfg(not(feature = "test-util"))]
        let _ = progress;
        Self {
            relative,
            identity: None,
            rest: Some(rest),
            of,
        }
    }
}

/// What a step does with an entry of the directory it lists, given the entry, its source, its
/// path relative to the walk's root and the visit: a directory it answers is listed below it.
type EachFn<'a, D> =
    dyn FnMut(&Listed, Source<'_>, &Path, Visit) -> io::Result<Option<Directory<D>>> + 'a;

/// What one step of a directory found of it.
enum Step {
    /// The directory is gone, or was replaced, by a link or another directory, since its parent
    /// listed it.
    Gone,
    /// The step left part of the directory to a later step.
    Continued,
    /// The step listed the last of the directory, which it hands back, still open.
    Done(
        #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
        OpenDirectory,
    ),
}

/// Lists one step of `directory`, whose path is `path`, calling `each` with every entry that
/// `visit` does not skip, and handing each directory `each` answers to `found`. A directory that
/// is gone or was replaced fails the walk where it is the walk's root. Where `keeps_atime` is set,
/// listing leaves the directory's access time as [`OpenDirectory::open`] says.
///
/// A directory is opened once, checked against the identity its parent listed it with, and held
/// open while it is listed; each entry is reached through it. An entry gone by the time it is
/// listed is left out. A step lists at most a batch of entries, and hands the rest of the
/// directory on as a continuation once its batch is done, so no thread lists a directory whole
/// and the threads take turns at a wide one, never listing it at once. At the first directory
/// found that the queue has no room for, it keeps the rest of its directory for its thread,
/// beneath that one, so a thread holds one open directory for each level it has descended.
fn each_entry<D: Copy>(
    directory: Directory<D>,
    path: &mut PathBuf,
    visit: &VisitFn<'_>,
    found: &mut Found<'_, Directory<D>>,
    keeps_atime: bool,
    each: &mut EachFn<'_, D>,
) -> io::Result<Step> {
    let Directory {
        mut relative,
        identity,
        rest,
        of,
    } = directory;
    let progress = found.progress;
    let rest = if let Some(rest) = rest {
        rest
    } else {
        let root = relative.as_os_str().is_empty();
        match open_listing(path, identity.as_ref(), root, keeps_atime)? {
            Some(rest) => rest,
            None => return Ok(Step::Gone),
        }
    };
    let Rest {
        directory: mut open,
        mut listing,
    } = rest;
    for _ in 0..progress.bounds().batch {
        let listed = match listing.next(&mut open) {
            None => return Ok(Step::Done(open)),
            Some(Ok(listed)) => listed,
            Some(Err(err)) if is_gone_kind(err.kind()) => continue,
            Some(Err(err)) => return Err(with_path(err, path)),
        };
        let Some(below) = visit_entry(&listed, &open, path, &mut relative, visit, each)? else {
            continue;
        };
        if let Err(below) = found.offer(below) {
            let rest = Rest {
                directory: open,
                listing,
            };
            found.keep(Directory::continued(relative, rest, of, progress));
            found.keep(below);
            return Ok(Step::Continued);
        }
    }
    let rest = Rest {
        directory: open,
        listing,
    };
    if let Err(rest) = found.offer_rest(Directory::continued(relative, rest, of, progress)) {
        found.keep(rest);
    }
    Ok(Step::Continued)
}

/// Opens the directory at `path` and its listing, as [`each_entry`] describes, checked against
/// `identity`, and leaving its access time where `keeps_atime` says. `None` where it is gone or
/// was replaced, unless it is the walk's `root`.
fn open_listing(
    path: &Path,
    identity: Option<&entry::Identity>,
    root: bool,
    keeps_atime: bool,
) -> io::Result<Option<Rest>> {
    let directory = match OpenDirectory::open(path, identity, keeps_atime) {
        Ok(Some(directory)) => directory,
        Ok(None) if !root => return Ok(None),
        Ok(None) => {
            return Err(with_path(
                io::Error::new(io::ErrorKind::NotFound, "no longer a directory"),
                path,
            ));
        }
        // Gone, or, on Unix, a link at the path, which fails the open as not a directory.
        Err(err) if !root && is_gone_kind(err.kind()) => return Ok(None),
        Err(err) => return Err(with_path(err, path)),
    };
    match Listing::open(&directory, path) {
        Ok(listing) => Ok(Some(Rest { directory, listing })),
        Err(err) if !root && is_gone_kind(err.kind()) => Ok(None),
        Err(err) => Err(with_path(err, path)),
    }
}

/// Calls `each` with `listed`, an entry of `open`, with `path` and `relative` extended by its
/// name, unless `visit` skips it.
fn visit_entry<D>(
    listed: &Listed,
    open: &OpenDirectory,
    path: &mut PathBuf,
    relative: &mut PathBuf,
    visit: &VisitFn<'_>,
    each: &mut EachFn<'_, D>,
) -> io::Result<Option<Directory<D>>> {
    path.push(&listed.name);
    relative.push(&listed.name);
    let visit = visit(path, relative, listed.kind);
    let below = if visit == Visit::Skip {
        Ok(None)
    } else {
        each(listed, open.entry(path, &listed.name), relative, visit)
    };
    relative.pop();
    path.pop();
    below
}

/// Raises `most` to `now` where `now` is more, reading it first so a count already reached
/// writes nothing.
#[cfg(feature = "test-util")]
fn hold(most: &std::sync::atomic::AtomicUsize, now: usize) {
    if now > most.load(Ordering::Relaxed) {
        most.fetch_max(now, Ordering::Relaxed);
    }
}

/// A directory [`super::measure`] or [`super::find_directories`] has found.
type Walked = Directory<()>;

/// [`super::measure`].
pub(super) fn measure(
    source: &Path,
    visit: &VisitFn<'_>,
    block: u64,
    progress: &Progress,
) -> io::Result<TreeSize> {
    let block = block.max(1);
    let size = AtomicU64::new(0);
    let file_count = AtomicU64::new(0);
    let entry_count = AtomicU64::new(0);
    run_lent(Walked::root(()), progress, &|directory: Walked, found| {
        if progress.is_stopped() {
            return Err(stopped());
        }
        with_paths(|path, _| {
            set_below(path, source, &directory.relative);
            each_entry(
                directory,
                path,
                visit,
                found,
                false,
                &mut |listed, source, relative, _visit| {
                    let mut below = None;
                    let taken = match listed.kind {
                        Kind::File => match len_of(listed, source) {
                            Ok(len) => {
                                file_count.fetch_add(1, Ordering::Relaxed);
                                len.div_ceil(block).max(1) * block
                            }
                            Err(err) if is_gone_kind(err.kind()) => return Ok(None),
                            Err(err) => return Err(with_path(err, &source)),
                        },
                        Kind::Directory => {
                            below = Walked::below(source, relative, ())?;
                            if below.is_none() {
                                return Ok(None);
                            }
                            block
                        }
                        Kind::Symlink => block,
                        Kind::Other => return Ok(None),
                    };
                    entry_count.fetch_add(1, Ordering::Relaxed);
                    size.fetch_add(taken, Ordering::Relaxed);
                    Ok(below)
                },
            )
            .map(drop)
        })
    })?;
    Ok(TreeSize {
        size: size.into_inner(),
        file_count: file_count.into_inner(),
        entry_count: entry_count.into_inner(),
    })
}

/// [`super::find_directories`].
pub(super) fn find_directories(
    root: &Path,
    matches: &(dyn Fn(&Path) -> bool + Sync),
    progress: &Progress,
) -> io::Result<Vec<PathBuf>> {
    let matched = Mutex::new(Vec::new());
    let only_directories = |_: &Path, _: &Path, kind: Kind| match kind {
        Kind::Directory => Visit::EachEntry,
        _ => Visit::Skip,
    };
    run_lent(Walked::root(()), progress, &|directory: Walked, found| {
        if progress.is_stopped() {
            return Err(stopped());
        }
        with_paths(|path, _| {
            set_below(path, root, &directory.relative);
            each_entry(
                directory,
                path,
                &only_directories,
                found,
                false,
                &mut |_listed, source, relative, _visit| {
                    if matches(&source) {
                        matched.lock().push(source.to_path_buf());
                        Ok(None)
                    } else {
                        Walked::below(source, relative, ())
                    }
                },
            )
            .map(drop)
        })
    })?;
    Ok(matched.into_inner())
}

/// The failure of a walk asked to stop.
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the walk was stopped")
}

/// What a clone knows of a directory it fills: whether a directory in it can be cloned as a
/// whole, whether the clone created the copy, which it removes again only then, and whether the
/// copy gives a directory created in it its set-group-ID bit, as Linux does.
#[derive(Clone, Copy)]
struct Fill {
    whole: bool,
    created: bool,
    #[cfg(target_family = "unix")]
    gives_setgid: bool,
}

/// A directory a clone fills.
type Filled = Directory<Fill>;

/// A directory being cloned: its source, its copy, the source's path relative to the root, and
/// what the directory that holds it allows: a directory in it cloned as a whole, and, as Linux
/// does, a directory created in it given its set-group-ID bit.
struct Place<'a> {
    from: Source<'a>,
    to: &'a Path,
    relative: &'a Path,
    whole: bool,
    #[cfg(target_family = "unix")]
    gives_setgid: bool,
}

/// What one clone works with, faithful where `FAITHFUL` is set, each mode's code compiled on its
/// own.
struct Cloning<'a, const FAITHFUL: bool> {
    backend: Backend,
    source: &'a Path,
    destination: &'a Path,
    visit: &'a VisitFn<'a>,
    copy_unclonable: bool,
    progress: &'a Progress,
    clone_entry: &'a (dyn Fn(Backend, Kind, Source<'_>, &Path) -> io::Result<()> + Sync),
    /// The permission bits the process's file mode creation mask clears from a directory the
    /// clone creates.
    #[cfg(target_family = "unix")]
    umask: u32,
    /// What a faithful copy left as its target set it.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    kept: &'a Kept,
}

/// The threads a walk runs on.
#[derive(Clone, Copy)]
enum Threads<'a> {
    /// The calling thread alone.
    Alone,
    /// The calling thread and the threads the pool lends.
    Lent(&'a crate::pool::SyscallPool),
}

/// [`super::clone_into`], with each file, link and whole directory cloned by `clone_entry`, which
/// answers as [`clone_entry`] does.
#[lore_macro::test_pub]
pub(super) fn clone_into_with(
    backend: Backend,
    source: &Path,
    destination: &Path,
    visit: &VisitFn<'_>,
    copy_unclonable: bool,
    progress: &Progress,
    clone_entry: &(dyn Fn(Backend, Kind, Source<'_>, &Path) -> io::Result<()> + Sync),
) -> io::Result<CloneStats> {
    let cloning = Cloning::<false> {
        backend,
        source,
        destination,
        visit,
        copy_unclonable,
        progress,
        clone_entry,
        #[cfg(target_family = "unix")]
        umask: entry::umask_in(destination).map_err(|err| with_path(err, destination))?,
        kept: &Kept::default(),
    };
    cloning.fill_root(Threads::Lent(crate::pool::SyscallPool::global()))?;
    Ok(progress.stats())
}

/// [`super::copy_faithfully`], on the calling thread and the threads `pool` lends, or on the
/// calling thread alone where `pool` is `None`.
#[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
#[lore_macro::test_pub]
pub(super) fn copy_faithfully_on(
    pool: Option<&crate::pool::SyscallPool>,
    source: &Path,
    destination: &Path,
    visit: &VisitFn<'_>,
    progress: &Progress,
) -> io::Result<FaithfulCopy> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    super::faithful::refuse_root(source, progress).map_err(|err| with_path(err, source))?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    super::faithful::clear_inherited_acl(destination).map_err(|err| with_path(err, destination))?;
    let kept = Kept::default();
    let cloning = Cloning::<true> {
        backend: Backend::Copy,
        source,
        destination,
        visit,
        copy_unclonable: false,
        progress,
        clone_entry: &clone_entry,
        #[cfg(target_family = "unix")]
        umask: entry::umask_in(destination).map_err(|err| with_path(err, destination))?,
        kept: &kept,
    };
    cloning.fill_root(pool.map_or(Threads::Alone, Threads::Lent))?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    super::faithful::flush_file_system(destination, progress)
        .map_err(|err| with_path(err, destination))?;
    let (relabeled, kept_groups) = kept.into_counts();
    Ok(FaithfulCopy {
        stats: progress.stats(),
        relabeled,
        kept_groups,
    })
}

/// Clones the entry of `kind` `source` names to `destination` with `backend`: a file, a link, or
/// a directory as a whole, which only APFS clones. A link replaced by a file after it was listed
/// is cloned as the file. Fails with [`io::ErrorKind::Unsupported`] where the backend cannot.
#[lore_macro::test_pub]
pub(super) fn clone_entry(
    backend: Backend,
    kind: Kind,
    source: Source<'_>,
    destination: &Path,
) -> io::Result<()> {
    match (backend, kind) {
        (Backend::Apfs, Kind::Directory) => apfs::clone_directory(source, destination),
        (_, Kind::Directory) => Err(entry::unsupported("whole directories")),
        (_, Kind::Symlink) => match entry::link(source, destination) {
            Err(_) if source.is_file() => backend.clone_source(source, destination),
            linked => linked,
        },
        (backend, _) => backend.clone_source(source, destination),
    }
}

impl<const FAITHFUL: bool> Cloning<'_, FAITHFUL> {
    /// What the entries of a faithful copy share.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn copying(&self) -> super::faithful::Copying<'_> {
        super::faithful::Copying {
            kept: self.kept,
            progress: self.progress,
        }
    }

    /// Fills the destination from the source on `threads`, and gives it the source's permission
    /// bits, which a faithful copy gives it with the rest of the source's metadata as it finishes
    /// it like any other directory.
    fn fill_root(&self, threads: Threads<'_>) -> io::Result<()> {
        let root = Filled::root(Fill {
            whole: self.backend == Backend::Apfs,
            created: false,
            #[cfg(target_family = "unix")]
            gives_setgid: entry::mode_of(self.destination)? & entry::SETGID != 0,
        });
        self.fill(root, threads)?;
        #[cfg(target_family = "unix")]
        if !FAITHFUL {
            entry::set_mode(self.destination, entry::mode_of(self.source)?)?;
        }
        Ok(())
    }

    /// Fills `root` and every directory below it on `threads`, depth-first on the calling thread
    /// alone.
    fn fill(&self, root: Filled, threads: Threads<'_>) -> io::Result<()> {
        let list = |directory, found: &mut Found<'_, Filled>| {
            self.fill_directory(directory, found)
                .inspect_err(|_| self.progress.stop())
        };
        match threads {
            Threads::Lent(pool) => run_lent_on(pool, root, self.progress, &list),
            Threads::Alone => run_alone(root, self.progress, &list),
        }
    }

    /// Clones the entries of one step of `directory`, handing the directories it creates to
    /// `found`. Where the source directory is gone, or was replaced since its parent listed it,
    /// its copy is removed again. A faithful copy refuses a socket, a pipe or a device, where the
    /// clone mode leaves it out, and on Windows a link or a junction, and finishes a directory
    /// once its last entry is in place.
    fn fill_directory(&self, directory: Filled, found: &mut Found<'_, Filled>) -> io::Result<()> {
        let Fill {
            whole,
            created,
            #[cfg(target_family = "unix")]
            gives_setgid,
        } = directory.of;
        with_paths(|from, to| {
            set_below(from, self.source, &directory.relative);
            set_below(to, self.destination, &directory.relative);
            let listed = each_entry(
                directory,
                from,
                self.visit,
                found,
                FAITHFUL,
                &mut |listed, from, relative, visit| {
                    #[cfg(feature = "test-util")]
                    self.progress.hold();
                    if self.progress.is_stopped() {
                        return Err(stopped());
                    }
                    to.push(&listed.name);
                    let done = match listed.kind {
                        Kind::Directory => self.directory(
                            Place {
                                from,
                                to,
                                relative,
                                whole,
                                #[cfg(target_family = "unix")]
                                gives_setgid,
                            },
                            visit,
                        ),
                        Kind::Other if FAITHFUL => Err(with_path(
                            super::refused("a socket, a pipe or a device"),
                            &from,
                        )),
                        Kind::Other => Ok(None),
                        kind => self
                            .file_or_link(kind, from, to)
                            .map(|()| None)
                            .map_err(|err| with_path(err, &from)),
                    };
                    to.pop();
                    done
                },
            )?;
            match listed {
                Step::Gone if created => {
                    let _ = std::fs::remove_dir(to);
                }
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                Step::Done(source) if FAITHFUL => {
                    super::faithful::finish_directory(&source, to, self.copying())
                        .map_err(|err| with_path(err, from))?;
                }
                #[cfg(target_family = "windows")]
                Step::Done(_) if FAITHFUL => {
                    super::faithful::finish_directory(from, to)
                        .map_err(|err| with_path(err, from))?;
                }
                _ => {}
            }
            Ok(())
        })
    }

    /// Clones the file or link `from` names to `to`, copying a file that cannot be cloned where
    /// the clone allows it. One gone from `from` is left out.
    fn file_or_link(&self, kind: Kind, from: Source<'_>, to: &Path) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
        if FAITHFUL {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let (copied, counter) = match kind {
                Kind::Symlink => (
                    super::faithful::link(from, to, self.copying()),
                    &self.progress.cloned_entry_count,
                ),
                _ => (
                    super::faithful::file(from, to, self.copying()),
                    &self.progress.copied_file_count,
                ),
            };
            #[cfg(target_family = "windows")]
            let (copied, counter) = match kind {
                Kind::Symlink => return Err(super::refused("a reparse point")),
                _ => (
                    super::faithful::file(from, to, self.progress),
                    &self.progress.copied_file_count,
                ),
            };
            return match copied {
                Ok(()) => {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Err(err) if is_gone_kind(err.kind()) && from.is_gone() => Ok(()),
                Err(err) => Err(err),
            };
        }
        let counter = match (self.clone_entry)(self.backend, kind, from, to) {
            Ok(()) if kind == Kind::File && self.backend == Backend::Copy => {
                &self.progress.copied_file_count
            }
            Ok(()) => &self.progress.cloned_entry_count,
            Err(err) if is_gone_kind(err.kind()) && from.is_gone() => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::Unsupported && self.copy_unclonable => {
                match copy::file(from, to) {
                    Err(err) if is_gone_kind(err.kind()) && from.is_gone() => {
                        return Ok(());
                    }
                    copied => copied?,
                }
                &self.progress.copied_file_count
            }
            Err(err) => return Err(err),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Clones the directory `from` names to `to`: as a whole where `whole`, the backend and
    /// `visit` allow and nothing is at `to` yet, so nothing a caller wrote there is removed with a
    /// whole-directory clone that fails; and otherwise by creating `to` with the permissions of
    /// `from` and answering it, to be filled. A directory whose permissions keep its
    /// owner from adding entries is filled here, owner-writable, and takes its permissions once it
    /// is full, so no more directories wait for their permissions than the tree is deep. Everything
    /// below such a directory is filled on this thread alone. A faithful copy on Windows refuses a
    /// directory whose attributes it doesn't keep before it creates the copy.
    fn directory(
        &self,
        Place {
            from,
            to,
            relative,
            whole,
            #[cfg(target_family = "unix")]
            gives_setgid,
        }: Place<'_>,
        visit: Visit,
    ) -> io::Result<Option<Filled>> {
        let mut whole_below = whole;
        if whole && visit == Visit::Clone && std::fs::symlink_metadata(to).is_err() {
            match (self.clone_entry)(self.backend, Kind::Directory, from, to) {
                Ok(()) => {
                    self.progress
                        .cloned_directory_count
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(None);
                }
                Err(err) if is_gone_kind(err.kind()) && from.is_gone() => return Ok(None),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) if err.kind() == io::ErrorKind::Unsupported => {
                    entry::remove_partial(to).map_err(|err| with_path(err, to))?;
                    whole_below = false;
                }
                Err(err) => return Err(with_path(err, &from)),
            }
        }
        let directory = match from.directory() {
            Ok(Some(directory)) => directory,
            Ok(None) => return Ok(None),
            Err(err) if is_gone_kind(err.kind()) && from.is_gone() => return Ok(None),
            Err(err) => return Err(with_path(err, &from)),
        };
        #[cfg(target_family = "unix")]
        {
            let mode = directory.mode;
            let filled = |created| Filled {
                relative: relative.to_path_buf(),
                identity: Some(directory.identity),
                rest: None,
                of: Fill {
                    whole: whole_below,
                    created,
                    gives_setgid: mode & entry::SETGID != 0,
                },
            };
            if !FAITHFUL && mode & entry::OWNER_FILLS != entry::OWNER_FILLS {
                let created = self
                    .create(to, mode | entry::OWNER_FILLS, gives_setgid)
                    .map_err(|err| with_path(err, to))?;
                self.fill(filled(created), Threads::Alone)?;
                return match entry::set_mode(to, mode) {
                    Err(err) if is_gone_kind(err.kind()) => Ok(None),
                    set => set.map(|()| None).map_err(|err| with_path(err, to)),
                };
            }
            let made = if FAITHFUL {
                mode | entry::OWNER_FILLS
            } else {
                mode
            };
            let created = self
                .create(to, made, gives_setgid)
                .map_err(|err| with_path(err, to))?;
            Ok(Some(filled(created)))
        }
        #[cfg(target_family = "windows")]
        {
            if FAITHFUL {
                super::faithful::refuse(directory.attributes, 1)
                    .map_err(|err| with_path(err, &from))?;
            }
            let created = entry::create_directory(to).map_err(|err| with_path(err, to))?;
            Ok(Some(Filled {
                relative: relative.to_path_buf(),
                identity: Some(directory.identity),
                rest: None,
                of: Fill {
                    whole: whole_below,
                    created,
                },
            }))
        }
    }

    /// Creates the directory `to` with the permission bits `mode`, in a directory of the copy that
    /// gives a directory created in it its set-group-ID bit where `gives_setgid`, as Linux does:
    /// true where it was created, false where a directory is already there. The bits are set
    /// again only where `mkdir` leaves them other than `mode`: for a directory already there, bits
    /// the file mode creation mask clears, the set-user-ID, set-group-ID and sticky bits, and a
    /// set-group-ID bit taken from the parent.
    #[cfg(target_family = "unix")]
    fn create(&self, to: &Path, mode: u32, gives_setgid: bool) -> io::Result<bool> {
        let created = entry::create_directory(to, mode)?;
        let takes_setgid = cfg!(target_os = "linux") && gives_setgid && mode & entry::SETGID == 0;
        if !created || takes_setgid || mode & (self.umask | !entry::CREATED_BITS) != 0 {
            entry::set_mode(to, mode)?;
        }
        Ok(created)
    }
}
