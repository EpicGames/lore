// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Walking a directory tree to clone it, copy it faithfully, measure it, or find directories in
//! it, with the per-entry primitives of [`lore_io::clone`]. Each walk is blocking.
//!
//! A directory replaced after it was listed, by a link or through a link swapped in above it, is
//! left out rather than followed, where the directory is filled entry by entry; one cloned whole is
//! cloned as it is when the walk reaches it, and a link there is cloned as a link. A link swapped
//! in above a directory while that directory is being filled does not lead the walk out of the
//! tree either: on Unix each entry is read through the directory, held open, and on Windows the
//! directory is held open without sharing delete access, so neither it nor a directory above it
//! can be renamed or removed until it is filled. A link at the root of the walk is followed; on
//! Windows the walk reads the tree through the link, so a link retargeted during the walk leads it
//! there. A directory cloned entry by entry leaves out sockets, pipes and devices, and any entry
//! gone by the time the walk reaches it. Directories keep their permissions on Unix; their times
//! are not kept, apart from those of a directory APFS clones as a whole.
//!
//! [`copy_faithfully`] is the walk's faithful mode, which copies each entry with
//! [`lore_io::clone::faithful`]. It shares the cloning mode's walk and entry code through a
//! constant parameter, so each mode's code is compiled on its own. It runs on the threads the
//! syscall pool lends, at most half its cap, as the cloning mode does: on Linux the walk benchmark
//! measured the whole cap slower on every tree it copies but the one of large files, where they tie
//! (`lore-revision/BENCHMARKS.md`, *The walk's bounds*).

mod walker;

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_io::clone::Backend;
use lore_io::clone::Kind;
#[cfg(feature = "test-util")]
pub use walker::Counts;
#[cfg(feature = "test-util")]
pub use walker::Found;
#[cfg(feature = "test-util")]
pub use walker::clone_into_with;
#[cfg(all(
    feature = "test-util",
    any(target_os = "linux", target_os = "macos", target_family = "windows")
))]
pub use walker::copy_faithfully_on;
#[cfg(feature = "test-util")]
pub use walker::run;
#[cfg(feature = "test-util")]
pub use walker::run_lent_on;

/// What cloning does with an entry of the source tree, given its path, its path relative to the
/// source and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visit {
    /// Leaves the entry out.
    Skip,
    /// Clones the entry: a directory as a whole where the backend can.
    Clone,
    /// Clones a directory entry by entry, visiting each of its entries.
    EachEntry,
}

/// What a walk decides for each entry, given its path, its path relative to the root walked, and
/// its kind.
pub type VisitFn<'a> = dyn Fn(&Path, &Path, Kind) -> Visit + Sync + 'a;

/// What [`clone_into`] did, or has done so far, counted in the [`Progress`] it was given, so a
/// `Progress` given to more than one walk counts them all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CloneStats {
    /// Directories cloned as a whole, each with everything in it.
    pub cloned_directory_count: u64,
    /// Files and symbolic links cloned one by one.
    pub cloned_entry_count: u64,
    /// Files whose data was copied.
    pub copied_file_count: u64,
}

/// What a running [`clone_into`] has done, which another thread can read, and a request to stop
/// it. A walk that fails stops itself too, so its other threads end at their next entry.
#[derive(Debug, Default)]
pub struct Progress {
    cloned_directory_count: AtomicU64,
    cloned_entry_count: AtomicU64,
    copied_file_count: AtomicU64,
    stopped: AtomicBool,
    #[cfg(feature = "test-util")]
    bounds: walker::Bounds,
    #[cfg(feature = "test-util")]
    counters: walker::Counters,
    /// Whether the walk, once it has cloned or copied an entry, waits before its next until it is
    /// stopped, standing in for a walk too long to finish before a stop, until it has waited.
    #[cfg(feature = "test-util")]
    held: AtomicBool,
    /// Where the copy of each entry stands in for what a test can't arrange.
    points: lore_io::clone::TestPoints,
}

impl Progress {
    /// A progress whose walk queues at most `queue` directories and `continuations`
    /// continuations, and lists `batch` entries of a directory at a time.
    #[cfg(feature = "test-util")]
    pub fn bounded(queue: usize, continuations: usize, batch: usize) -> Self {
        Self {
            bounds: walker::Bounds {
                queue,
                continuations,
                batch,
            },
            ..Self::default()
        }
    }

    /// A progress whose copy of each entry stands in for what `points` names.
    #[cfg(feature = "test-util")]
    fn with_points(points: lore_io::clone::TestPoints) -> Self {
        Self {
            points,
            ..Self::default()
        }
    }

    /// A progress whose faithful copy fails every set of the extended attribute `name` with the
    /// OS error `code`, in place of setting it.
    #[cfg(feature = "test-util")]
    pub fn refusing_attribute(name: &str, code: i32) -> Self {
        Self::with_points(lore_io::clone::TestPoints::refusing_attribute(name, code))
    }

    /// A progress whose faithful copy fails every set of the extended attribute `name` with the
    /// OS error `code`, and reads the target as holding it, as one the system gives each new entry.
    #[cfg(feature = "test-util")]
    pub fn refusing_attribute_the_target_holds(name: &str, code: i32) -> Self {
        Self::with_points(
            lore_io::clone::TestPoints::refusing_attribute_the_target_holds(name, code),
        )
    }

    /// A progress whose faithful copy reads the flags `flags`, `stx_attributes` on Linux and
    /// `st_flags` on macOS, for each entry named `name`, besides its own.
    #[cfg(feature = "test-util")]
    pub fn flagging(name: &str, flags: u64) -> Self {
        Self::with_points(lore_io::clone::TestPoints::flagging(name, flags))
    }

    /// A progress whose walk, once it has cloned or copied an entry, waits before its next until
    /// it is stopped, for at most a minute, and then goes on.
    #[cfg(feature = "test-util")]
    pub fn holding() -> Self {
        Self {
            held: AtomicBool::new(true),
            ..Self::default()
        }
    }

    /// A progress whose faithful copy refuses the sets of an owner and group `refused` names with
    /// `EPERM`.
    #[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
    pub fn refusing_owners(refused: lore_io::clone::Refused) -> Self {
        Self::with_points(lore_io::clone::TestPoints::refusing_owners(refused))
    }

    /// A progress whose faithful copy fails the copy of each file named `name` once its data is
    /// written, on Windows.
    #[cfg(feature = "test-util")]
    pub fn failing_file(name: &str) -> Self {
        Self::with_points(lore_io::clone::TestPoints::failing_file(name))
    }

    /// Waits until this progress is stopped, for at most a minute, where it holds its walk and the
    /// walk has cloned or copied an entry, and holds it no longer.
    #[cfg(feature = "test-util")]
    fn hold(&self) {
        if !self.held.load(Ordering::Relaxed) || self.stats() == CloneStats::default() {
            return;
        }
        let started = std::time::Instant::now();
        while !self.is_stopped() && started.elapsed() < std::time::Duration::from_secs(60) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.held.store(false, Ordering::Relaxed);
    }

    /// What the test build counted of the walk.
    #[cfg(feature = "test-util")]
    pub fn counts(&self) -> Counts {
        self.counters.stats(&self.points)
    }

    /// The bounds of the walk, which only a test build sets.
    #[cfg_attr(not(feature = "test-util"), allow(clippy::unused_self))]
    fn bounds(&self) -> walker::Bounds {
        #[cfg(feature = "test-util")]
        {
            self.bounds
        }
        #[cfg(not(feature = "test-util"))]
        {
            walker::BOUNDS
        }
    }

    /// What the walk has cloned and copied so far.
    pub fn stats(&self) -> CloneStats {
        CloneStats {
            cloned_directory_count: self.cloned_directory_count.load(Ordering::Relaxed),
            cloned_entry_count: self.cloned_entry_count.load(Ordering::Relaxed),
            copied_file_count: self.copied_file_count.load(Ordering::Relaxed),
        }
    }

    /// Asks the walk to stop. It fails with [`io::ErrorKind::Interrupted`] before its next
    /// entry, leaving what it has written. An entry already being cloned, a whole directory on
    /// APFS among them, is finished first.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Whether the walk has been asked to stop.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}

/// What [`copy_faithfully`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FaithfulCopy {
    /// What the copy counted.
    pub stats: CloneStats,
    /// The `security.*` extended attributes the system refused to set on the copy,
    /// which keep the values the target gave them, each with the entries it affected. None on
    /// Windows.
    pub relabeled: Vec<(String, u64)>,
    /// The entries that kept the group the target gave them, a group this process may not give,
    /// as it isn't a member of it. None on Windows.
    pub kept_groups: u64,
}

/// The size of a tree, as [`measure`] finds it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TreeSize {
    /// Bytes a copy of the tree takes: each file rounded up to whole blocks, at least one, and
    /// one block for each directory and link.
    pub size: u64,
    /// Files.
    pub file_count: u64,
    /// Files, directories and links.
    pub entry_count: u64,
}

/// Measures what a copy of the tree at `source` takes on a volume whose blocks are `block`
/// bytes, leaving out the entries `visit` skips, on the calling thread and the threads the
/// syscall pool lends it, as [`clone_into`] does. Fails as interrupted once `progress` is stopped.
pub fn measure(
    source: &Path,
    visit: &VisitFn<'_>,
    block: u64,
    progress: &Progress,
) -> io::Result<TreeSize> {
    walker::measure(source, visit, block, progress)
}

/// The directories below `root` that `matches` accepts, given each directory's path, without
/// looking below a directory it accepts. Links, and directories replaced since they were listed,
/// are not followed, as the module describes. Runs on the calling thread and the threads the
/// syscall pool lends it, as [`clone_into`] does, and fails as interrupted once `progress` is
/// stopped.
pub fn find_directories(
    root: &Path,
    matches: &(dyn Fn(&Path) -> bool + Sync),
    progress: &Progress,
) -> io::Result<Vec<PathBuf>> {
    walker::find_directories(root, matches, progress)
}

/// Copies the entries of the directory `source` into the empty directory `destination`, leaving
/// out those `visit` skips, so that the copy holds what the source does and is on its device, as
/// [`lore_io::clone::faithful`] describes. Fails with [`io::ErrorKind::Unsupported`] on other
/// platforms.
pub fn copy_faithfully(
    source: &Path,
    destination: &Path,
    visit: &VisitFn<'_>,
    progress: &Progress,
) -> io::Result<FaithfulCopy> {
    #[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
    {
        walker::copy_faithfully_with(
            walker::Threads::Lent(&lore_io::lend_threads),
            source,
            destination,
            visit,
            progress,
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_family = "windows")))]
    {
        let _ = (source, destination, visit, progress);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a faithful copy runs on Linux, macOS and Windows only",
        ))
    }
}

/// Clones the entries of the directory `source` into the directory `destination`, which exists,
/// with `backend`, leaving out the entries `visit` skips, on the calling thread and the threads
/// the syscall pool lends it, at most half its cap, so other file operations keep the rest. Its
/// memory does not grow with the number of files or the width of a directory, as the walk's
/// bounds hold it. `progress` counts what it does and can stop it.
///
/// A file the backend cannot clone fails cloning with [`io::ErrorKind::Unsupported`], unless
/// `copy_unclonable` is set, in which case its data is copied. A directory already present in
/// `destination` is filled entry by entry, so a caller can write files of its own into the copy
/// before it is made. Any other entry already present is an [`io::ErrorKind::AlreadyExists`]
/// error. A failure names the entry it failed on.
pub fn clone_into(
    backend: Backend,
    source: &Path,
    destination: &Path,
    visit: &VisitFn<'_>,
    copy_unclonable: bool,
    progress: &Progress,
) -> io::Result<CloneStats> {
    walker::clone_into_with(
        backend,
        source,
        destination,
        visit,
        copy_unclonable,
        progress,
        &lore_io::clone::clone_entry,
    )
}
