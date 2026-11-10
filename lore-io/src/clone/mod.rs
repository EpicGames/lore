// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Cloning a directory tree so that the copy shares the source's data blocks (copy-on-write
//! clones), through one backend per filesystem:
//!
//! - [`Backend::Apfs`]: `clonefile(2)`, which clones a file or a whole directory.
//! - [`Backend::Reflink`]: the `FICLONE` ioctl per file (Btrfs, XFS, bcachefs).
//! - [`Backend::Refs`]: `FSCTL_DUPLICATE_EXTENTS_TO_FILE` per file (the Resilient File System,
//!   Dev Drive).
//! - [`Backend::Copy`]: a plain byte copy, for every other filesystem and across volumes.
//!
//! A block-cloning backend fails with [`io::ErrorKind::Unsupported`] where it cannot clone, never
//! copying in its place. Each backend is compiled on every platform and answers unsupported
//! where its call does not exist. [`detect`] probes which backend shares blocks between two
//! directories, and [`clone_into`] copies a tree with the backend it chose. Both are blocking.
//!
//! A file keeps its permissions and its modification time, read from the source's open handle
//! once its data is cloned or copied, so a file rewritten during the clone never carries a time
//! older than its content. Symbolic links are copied as links. A directory replaced after it was
//! listed, by a link or through a link swapped in above it, is left out rather than followed,
//! where the directory is filled entry by entry; one cloned whole is cloned as it is when the
//! clone reaches it, and a link there is cloned as a link. A link swapped in above a directory
//! while that directory is being filled does not lead the clone out of the tree either: on Unix
//! each entry is read through the directory, held open, and on Windows the directory is held open
//! without sharing delete access, so neither it nor a directory above it can be renamed or
//! removed until it is filled. A link at the root of the walk is followed; on Windows the walk
//! reads the tree through the link, so a link retargeted during the clone leads it there. A
//! directory cloned entry by entry leaves out sockets, pipes and devices, and any entry gone by
//! the time the clone reaches it. Directories keep their permissions on Unix; their times are not
//! kept, apart from those of a directory APFS clones as a whole.

mod apfs;
mod copy;
mod detect;
mod entry;
mod reflink;
mod refs;
mod source;
mod volume;
mod walk;

use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

pub use detect::Detection;
pub use detect::PROBE_PREFIX;
#[cfg(feature = "test-util")]
pub use detect::choose;
pub use detect::detect;
#[cfg(feature = "test-util")]
pub use refs::duplicate_ranges;
#[cfg(feature = "test-util")]
pub use source::Source;
pub use volume::Space;
pub use volume::Volume;
pub use volume::space_of;
pub use volume::volume_of;
pub use walk::Kind;
pub use walk::VisitFn;
#[cfg(feature = "test-util")]
pub use walk::clone_entry;
#[cfg(feature = "test-util")]
pub use walk::clone_into_with;
#[cfg(feature = "test-util")]
pub use walk::run;
#[cfg(feature = "test-util")]
pub use walk::run_lent_on;

/// A way of cloning the entries of a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// `clonefile(2)` on APFS.
    Apfs,
    /// The `FICLONE` ioctl on Btrfs, XFS and bcachefs.
    Reflink,
    /// `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on the Resilient File System and Dev Drive.
    Refs,
    /// A plain byte copy.
    Copy,
}

impl Backend {
    /// The backends that share blocks on this platform, in the order a probe tries them.
    #[cfg(target_os = "macos")]
    pub const BLOCK_CLONING: &[Backend] = &[Backend::Apfs];
    /// The backends that share blocks on this platform, in the order a probe tries them.
    #[cfg(target_os = "linux")]
    pub const BLOCK_CLONING: &[Backend] = &[Backend::Reflink];
    /// The backends that share blocks on this platform, in the order a probe tries them.
    #[cfg(target_family = "windows")]
    pub const BLOCK_CLONING: &[Backend] = &[Backend::Refs];
    /// The backends that share blocks on this platform, in the order a probe tries them.
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_family = "windows")))]
    pub const BLOCK_CLONING: &[Backend] = &[];

    /// Whether the backend shares the source's data blocks rather than copying them.
    pub fn shares_blocks(self) -> bool {
        self != Backend::Copy
    }

    /// Clones or copies the file at `source` to `destination`, which must not exist, with its
    /// permissions and modification time. A symbolic link found at `source` is copied as a link.
    pub fn clone_file(self, source: &Path, destination: &Path) -> io::Result<()> {
        source::at_path(source, |source| self.clone_source(source, destination))
    }

    /// [`Self::clone_file`] for an entry the walk reaches through the directory that holds it.
    fn clone_source(self, source: source::Source<'_>, destination: &Path) -> io::Result<()> {
        match self {
            Backend::Apfs => apfs::clone_file(source, destination),
            Backend::Reflink => reflink::clone_file(source, destination),
            Backend::Refs => refs::clone_file(source, destination),
            Backend::Copy => copy::file(source, destination),
        }
    }
}

/// What a clone does with an entry of the source tree, given its path, its path relative to the
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

/// What [`clone_into`] did, or has done so far, counted in the [`Progress`] it was given, so a
/// `Progress` given to more than one clone counts them all.
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
/// it. A clone that fails stops itself too, so its other threads end at their next entry.
#[derive(Debug, Default)]
pub struct Progress {
    cloned_directory_count: AtomicU64,
    cloned_entry_count: AtomicU64,
    copied_file_count: AtomicU64,
    stopped: AtomicBool,
}

impl Progress {
    /// What the clone has done so far.
    pub fn stats(&self) -> CloneStats {
        CloneStats {
            cloned_directory_count: self.cloned_directory_count.load(Ordering::Relaxed),
            cloned_entry_count: self.cloned_entry_count.load(Ordering::Relaxed),
            copied_file_count: self.copied_file_count.load(Ordering::Relaxed),
        }
    }

    /// Asks the clone to stop. It fails with [`io::ErrorKind::Interrupted`] before its next
    /// entry, leaving what it has written. An entry already being cloned, a whole directory on
    /// APFS among them, is finished first.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
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
    walk::measure(source, visit, block, progress)
}

/// The directories below `root` that `matches` accepts, given each directory's path, without
/// looking below a directory it accepts. Links, and directories replaced since they were listed,
/// are not followed, as [`clone_into`] describes. Runs on the calling thread and the threads the
/// syscall pool lends it, as [`clone_into`] does, and fails as interrupted once `progress` is
/// stopped.
pub fn find_directories(
    root: &Path,
    matches: &(dyn Fn(&Path) -> bool + Sync),
    progress: &Progress,
) -> io::Result<Vec<std::path::PathBuf>> {
    walk::find_directories(root, matches, progress)
}

/// Clones the entries of the directory `source` into the directory `destination`, which exists,
/// with `backend`, leaving out the entries `visit` skips, on the calling thread and the threads
/// the syscall pool lends it, at most half its cap, so other file operations keep the rest.
/// `progress` counts what it does and can stop it.
///
/// A file the backend cannot clone fails the clone with [`io::ErrorKind::Unsupported`], unless
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
    walk::clone_into_with(
        backend,
        source,
        destination,
        visit,
        copy_unclonable,
        progress,
        &walk::clone_entry,
    )
}
