// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The walk [`super::clone_into`], [`super::measure`] and [`super::find_directories`] share.
//! Each directory is listed by one of the calling thread and the threads the syscall pool lends
//! it, and the directories it holds are queued for the next free thread, the last found first.

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

/// What lists one directory of a walk, pushing the directories to list next onto the vector it
/// is given.
type ListFn<'a, J> = dyn Fn(J, &mut Vec<J>) -> io::Result<()> + Sync + 'a;

/// The directories a walk has found and not listed yet, how many are being listed, how many of
/// its threads wait for one, and how the walk ended.
struct Queue<J> {
    waiting: Vec<J>,
    listing: usize,
    idle: usize,
    /// When the walk last asked for another thread.
    grown: Option<Instant>,
    ended: bool,
    failure: Option<io::Error>,
}

/// How often at most a walk with more directories waiting than threads free asks for another
/// thread.
const GROW_INTERVAL: Duration = Duration::from_millis(10);

/// A running walk: its queue, and how it lists a directory and gets its threads.
struct Walk<'a, J> {
    queue: Mutex<Queue<J>>,
    ready: Condvar,
    list: &'a ListFn<'a, J>,
    /// Takes up to the threads it is asked for, beyond the calling thread, and says how many it
    /// took.
    more: &'a (dyn Fn(usize) -> usize + Sync),
    /// Gives back a thread taken that the system refused to start.
    refused: &'a (dyn Fn() + Sync),
}

/// Lists `root` with `list`, and every directory `list` pushes onto the vector it is given, on
/// up to `threads` threads. The first failure ends the walk, once the directories being listed
/// are done, and is the one reported, unless it is only another thread stopping because the walk
/// was asked to stop, which a later failure replaces.
///
/// The directories found last are listed first, so the queue holds the directories beside the
/// paths being walked rather than a whole level of the tree.
#[lore_macro::test_pub]
fn run<J: Send>(root: J, threads: usize, list: &ListFn<'_, J>) -> io::Result<()> {
    let left = Mutex::new(threads.saturating_sub(1));
    run_with(
        root,
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

/// [`run`] on the calling thread and on the threads `more` takes: as many as it gives when the
/// walk starts, and one more each time it gives one while more directories wait than threads are
/// free, asked at most every [`GROW_INTERVAL`]. `refused` gives back each thread the system
/// refuses to start, whose share of the walk the other threads take.
fn run_with<J: Send>(
    root: J,
    more: &(dyn Fn(usize) -> usize + Sync),
    refused: &(dyn Fn() + Sync),
    list: &ListFn<'_, J>,
) -> io::Result<()> {
    let walk = Walk {
        queue: Mutex::new(Queue {
            waiting: vec![root],
            listing: 0,
            idle: 0,
            grown: None,
            ended: false,
            failure: None,
        }),
        ready: Condvar::new(),
        list,
        more,
        refused,
    };
    std::thread::scope(|scope| {
        for _ in 0..more(usize::MAX) {
            spawn_walker(scope, &walk);
        }
        walk.take_and_list(&|| grow(scope, &walk));
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
        .spawn_scoped(scope, move || walk.take_and_list(&|| grow(scope, walk)));
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
    /// One thread of the walk: lists directories from the queue until none is waiting and none
    /// is being listed, or the walk has failed, calling `grow` where more directories wait than
    /// threads are free.
    fn take_and_list(&self, grow: &dyn Fn()) {
        let mut found = Vec::new();
        let mut state = self.queue.lock();
        loop {
            if state.ended {
                return;
            }
            let Some(directory) = state.waiting.pop() else {
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
            // A panic in `list` fails the walk, where it would leave `listing` counting a
            // directory no thread lists and the other threads waiting for it.
            let listed = MutexGuard::unlocked(&mut state, || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (self.list)(directory, &mut found)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("a walk thread panicked")))
            });
            state.listing -= 1;
            if let Err(failure) = listed {
                end(&mut state, &self.ready, Some(failure));
                return;
            }
            let count = found.len();
            state.waiting.append(&mut found);
            for _ in 0..count {
                self.ready.notify_one();
            }
            if state.waiting.len() > state.idle
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

/// [`run`] on the calling thread and on the threads the syscall pool lends, as [`run_lent_on`]
/// describes.
fn run_lent<J: Send>(root: J, list: &ListFn<'_, J>) -> io::Result<()> {
    run_lent_on(crate::pool::SyscallPool::global(), root, list)
}

/// [`run`] on the calling thread and on threads of `pool`'s cap it lends: as many as it lends when
/// the walk starts, and more as it lends them while the walk has more directories waiting than
/// threads free, so a walk that starts while the pool is busy takes the threads the pool frees
/// later. The pool lends at most half its cap, so the walk never takes the threads every other
/// file operation in the process runs on, and the threads lent and the pool's own stay within the
/// pool's cap; the walk's calling thread is its caller's. A thread the system refuses to start is
/// given back at once.
#[lore_macro::test_pub]
fn run_lent_on<J: Send>(
    pool: &crate::pool::SyscallPool,
    root: J,
    list: &ListFn<'_, J>,
) -> io::Result<()> {
    let lent = Mutex::new(pool.lend(0));
    run_with(
        root,
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

/// Calls `each` with every entry of the directory at `path` that is a directory, a file or a
/// link and that `visit` does not skip. `path` and `relative`, the directory's path relative to
/// the walk's root, are extended by the entry's name for `visit` and `each`, and the entry is
/// reached through the directory, opened once, checked against `identity`, the identity its
/// parent listed it with, and held open while it is listed. An entry gone by the time it is listed
/// is left out. False where the directory itself is gone, or was replaced, by a link or another
/// directory, since its parent listed it, unless it is the walk's root, which fails the walk.
fn each_entry(
    path: &mut PathBuf,
    relative: &mut PathBuf,
    identity: Option<&entry::Identity>,
    visit: &VisitFn<'_>,
    each: &mut dyn FnMut(&Listed, Source<'_>, &Path, Visit) -> io::Result<()>,
) -> io::Result<bool> {
    let root = relative.as_os_str().is_empty();
    let directory = match OpenDirectory::open(path, identity) {
        Ok(Some(directory)) => directory,
        Ok(None) if !root => return Ok(false),
        Ok(None) => {
            return Err(with_path(
                io::Error::new(io::ErrorKind::NotFound, "no longer a directory"),
                path,
            ));
        }
        // Gone, or, on Unix, a link at the path, which fails the open as not a directory.
        Err(err) if !root && is_gone_kind(err.kind()) => return Ok(false),
        Err(err) => return Err(with_path(err, path)),
    };
    let mut listing = match listing(&directory, path) {
        Ok(listing) => listing,
        Err(err) if !root && is_gone_kind(err.kind()) => return Ok(false),
        Err(err) => return Err(with_path(err, path)),
    };
    loop {
        let listed = match listing.next() {
            None => return Ok(true),
            Some(Ok(listed)) => listed,
            Some(Err(err)) if is_gone_kind(err.kind()) => continue,
            Some(Err(err)) => return Err(with_path(err, path)),
        };
        if listed.kind == Kind::Other {
            continue;
        }
        path.push(&listed.name);
        relative.push(&listed.name);
        let visit = visit(path, relative, listed.kind);
        let done = if visit == Visit::Skip {
            Ok(())
        } else {
            each(
                &listed,
                directory.entry(path, &listed.name),
                relative,
                visit,
            )
        };
        relative.pop();
        path.pop();
        done?;
    }
}

/// The entries of `directory`, read through its handle.
#[cfg(target_family = "unix")]
fn listing<'a>(
    directory: &'a OpenDirectory,
    _path: &Path,
) -> io::Result<impl Iterator<Item = io::Result<Listed>> + 'a> {
    Ok(directory
        .names()?
        .map(|found| found.map(|(name, kind)| Listed { name, kind })))
}

/// The entries of the directory at `path`, listed by a search that queries the directory with a
/// larger buffer where the search takes the path.
#[cfg(target_family = "windows")]
fn listing(
    _directory: &OpenDirectory,
    path: &Path,
) -> io::Result<Box<dyn Iterator<Item = io::Result<Listed>>>> {
    let mut first = windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW::default();
    match crate::find::open(path.to_path_buf(), &mut first)? {
        crate::find::Opened::Search(mut search) => {
            let mut chunk = Vec::new();
            let mut more = search.fill(Some(&first), &mut chunk, SEARCH_CHUNK);
            let mut found = chunk.into_iter();
            Ok(Box::new(std::iter::from_fn(move || {
                loop {
                    if let Some(entry) = found.next() {
                        return Some(entry.map(found_entry));
                    }
                    if !more {
                        return None;
                    }
                    let mut chunk = Vec::new();
                    more = search.fill(None, &mut chunk, SEARCH_CHUNK);
                    found = chunk.into_iter();
                }
            })))
        }
        crate::find::Opened::Empty => Ok(Box::new(std::iter::empty())),
        crate::find::Opened::Declined(path) => {
            Ok(Box::new(std::fs::read_dir(path)?.map(|entry| {
                let entry = entry?;
                let metadata = entry.metadata()?;
                Ok(Listed {
                    name: entry.file_name(),
                    kind: kind_of(metadata.file_type()),
                    len: Some(metadata.len()),
                })
            })))
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

/// A directory [`super::measure`] or [`super::find_directories`] has found: its path relative to
/// the root walked, and the identity it had when its parent listed it, which the root has none of.
struct Walked {
    relative: PathBuf,
    identity: Option<entry::Identity>,
}

impl Walked {
    /// The root of a walk.
    fn root() -> Self {
        Self {
            relative: PathBuf::new(),
            identity: None,
        }
    }

    /// Pushes the directory `source` onto `found`, unless it is gone or no longer a directory:
    /// true where it was pushed.
    fn push(found: &mut Vec<Self>, source: Source<'_>, relative: &Path) -> io::Result<bool> {
        match source.directory() {
            Ok(Some(directory)) => {
                found.push(Self {
                    relative: relative.to_path_buf(),
                    identity: Some(directory.identity),
                });
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(err) if is_gone_kind(err.kind()) => Ok(false),
            Err(err) => Err(with_path(err, &source)),
        }
    }

    /// Lists this directory of the tree at `root`, as [`each_entry`] does.
    fn each_entry(
        mut self,
        root: &Path,
        visit: &VisitFn<'_>,
        each: &mut dyn FnMut(&Listed, Source<'_>, &Path, Visit) -> io::Result<()>,
    ) -> io::Result<()> {
        with_paths(|path, _| {
            set_below(path, root, &self.relative);
            each_entry(
                path,
                &mut self.relative,
                self.identity.as_ref(),
                visit,
                each,
            )
            .map(drop)
        })
    }
}

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
    run_lent(Walked::root(), &|directory: Walked, found| {
        if progress.is_stopped() {
            return Err(stopped());
        }
        directory.each_entry(source, visit, &mut |listed, source, relative, _visit| {
            let taken = match listed.kind {
                Kind::File => match len_of(listed, source) {
                    Ok(len) => {
                        file_count.fetch_add(1, Ordering::Relaxed);
                        len.div_ceil(block).max(1) * block
                    }
                    Err(err) if is_gone_kind(err.kind()) => return Ok(()),
                    Err(err) => return Err(with_path(err, &source)),
                },
                Kind::Directory => {
                    if !Walked::push(found, source, relative)? {
                        return Ok(());
                    }
                    block
                }
                Kind::Symlink | Kind::Other => block,
            };
            entry_count.fetch_add(1, Ordering::Relaxed);
            size.fetch_add(taken, Ordering::Relaxed);
            Ok(())
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
    run_lent(Walked::root(), &|directory: Walked, found| {
        if progress.is_stopped() {
            return Err(stopped());
        }
        directory.each_entry(
            root,
            &only_directories,
            &mut |_listed, source, relative, _visit| {
                if matches(&source) {
                    matched.lock().push(source.to_path_buf());
                    Ok(())
                } else {
                    Walked::push(found, source, relative).map(drop)
                }
            },
        )
    })?;
    Ok(matched.into_inner())
}

/// The failure of a walk asked to stop.
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the walk was stopped")
}

/// A directory a clone fills: the source's path relative to the clone's root, whether a directory
/// in it can be cloned as a whole, and the identity the source had when its parent listed it,
/// which the root has none of.
struct Filled {
    relative: PathBuf,
    whole: bool,
    identity: Option<entry::Identity>,
    /// Whether the clone created the copy, which it removes again only then.
    created: bool,
    /// Whether the copy gives a directory created in it its set-group-ID bit, as Linux does.
    #[cfg(target_family = "unix")]
    gives_setgid: bool,
}

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

/// What one clone works with.
struct Cloning<'a> {
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
    let cloning = Cloning {
        backend,
        source,
        destination,
        visit,
        copy_unclonable,
        progress,
        clone_entry,
        #[cfg(target_family = "unix")]
        umask: entry::umask_in(destination).map_err(|err| with_path(err, destination))?,
    };
    let root = Filled {
        relative: PathBuf::new(),
        whole: backend == Backend::Apfs,
        identity: None,
        created: false,
        #[cfg(target_family = "unix")]
        gives_setgid: entry::mode_of(destination)? & entry::SETGID != 0,
    };
    cloning.fill(root, true)?;
    #[cfg(target_family = "unix")]
    entry::set_mode(destination, entry::mode_of(source)?)?;
    Ok(progress.stats())
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

impl Cloning<'_> {
    /// Fills `root` and every directory below it, on the threads the syscall pool lends where
    /// `lent` is set and on the calling thread alone otherwise.
    fn fill(&self, root: Filled, lent: bool) -> io::Result<()> {
        let list = |directory, found: &mut Vec<Filled>| {
            self.fill_directory(directory, found)
                .inspect_err(|_| self.progress.stop())
        };
        if lent {
            run_lent(root, &list)
        } else {
            run(root, 1, &list)
        }
    }

    /// Clones the entries of `directory`, pushing the directories it creates onto `found`. Where
    /// the source directory is gone, or was replaced since its parent listed it, its copy is
    /// removed again.
    fn fill_directory(&self, directory: Filled, found: &mut Vec<Filled>) -> io::Result<()> {
        let Filled {
            mut relative,
            whole,
            identity,
            created,
            #[cfg(target_family = "unix")]
            gives_setgid,
        } = directory;
        with_paths(|from, to| {
            set_below(from, self.source, &relative);
            set_below(to, self.destination, &relative);
            let listed = each_entry(
                from,
                &mut relative,
                identity.as_ref(),
                self.visit,
                &mut |listed, from, relative, visit| {
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
                            found,
                        ),
                        kind => self
                            .file_or_link(kind, from, to)
                            .map_err(|err| with_path(err, &from)),
                    };
                    to.pop();
                    done
                },
            )?;
            if !listed && created {
                let _ = std::fs::remove_dir(to);
            }
            Ok(())
        })
    }

    /// Clones the file or link `from` names to `to`, copying a file that cannot be cloned where
    /// the clone allows it. One gone from `from` is left out.
    fn file_or_link(&self, kind: Kind, from: Source<'_>, to: &Path) -> io::Result<()> {
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
    /// `from` and pushing it onto `found` to be filled. A directory whose permissions keep its
    /// owner from adding entries is filled here, owner-writable, and takes its permissions once it
    /// is full, so no more directories wait for their permissions than the tree is deep. Everything
    /// below such a directory is filled on this thread alone.
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
        found: &mut Vec<Filled>,
    ) -> io::Result<()> {
        let mut whole_below = whole;
        if whole && visit == Visit::Clone && std::fs::symlink_metadata(to).is_err() {
            match (self.clone_entry)(self.backend, Kind::Directory, from, to) {
                Ok(()) => {
                    self.progress
                        .cloned_directory_count
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(err) if is_gone_kind(err.kind()) && from.is_gone() => return Ok(()),
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
            Ok(None) => return Ok(()),
            Err(err) if is_gone_kind(err.kind()) && from.is_gone() => return Ok(()),
            Err(err) => return Err(with_path(err, &from)),
        };
        #[cfg(target_family = "unix")]
        {
            let mode = directory.mode;
            let filled = |created| Filled {
                relative: relative.to_path_buf(),
                whole: whole_below,
                identity: Some(directory.identity),
                created,
                gives_setgid: mode & entry::SETGID != 0,
            };
            if mode & entry::OWNER_FILLS != entry::OWNER_FILLS {
                let created = self
                    .create(to, mode | entry::OWNER_FILLS, gives_setgid)
                    .map_err(|err| with_path(err, to))?;
                self.fill(filled(created), false)?;
                return match entry::set_mode(to, mode) {
                    Err(err) if is_gone_kind(err.kind()) => Ok(()),
                    set => set.map_err(|err| with_path(err, to)),
                };
            }
            let created = self
                .create(to, mode, gives_setgid)
                .map_err(|err| with_path(err, to))?;
            found.push(filled(created));
        }
        #[cfg(target_family = "windows")]
        {
            let created = entry::create_directory(to).map_err(|err| with_path(err, to))?;
            found.push(Filled {
                relative: relative.to_path_buf(),
                whole: whole_below,
                identity: Some(directory.identity),
                created,
            });
        }
        Ok(())
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
