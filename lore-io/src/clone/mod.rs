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
//!
//! [`copy_faithfully`] is the walk's faithful mode, which keeps what the clone mode leaves: holes,
//! owners, extended attributes, ACLs, and the times of directories and links on Linux and macOS,
//! and sparse ranges, alternate data streams, security descriptors and attributes on Windows. It
//! shares the clone mode's walk and entry code through a constant parameter, so each mode's code
//! is compiled on its own. It runs on the threads the syscall pool lends, at most half its cap, as
//! the clone mode does: on Linux the walk benchmark measured the whole cap slower on every tree it
//! copies but the one of large files, where they tie (`lore-io/BENCHMARKS.md`, *The walk's
//! bounds*).

mod apfs;
mod copy;
mod detect;
mod entry;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod faithful;
#[cfg(target_family = "windows")]
#[path = "faithful_windows.rs"]
mod faithful;
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
#[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
pub use faithful::read_sized;
#[cfg(all(feature = "test-util", target_family = "windows"))]
pub use faithful::refusal;
#[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
pub use faithful::refuses_flags;
#[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
pub use faithful::target_keeps;
#[cfg(feature = "test-util")]
pub use refs::duplicate_ranges;
#[cfg(feature = "test-util")]
pub use source::Source;
pub use volume::Locality;
pub use volume::Space;
pub use volume::Volume;
#[cfg(all(feature = "test-util", target_family = "windows"))]
pub use volume::drive_type_is_local;
pub use volume::locality_of;
#[cfg(all(
    feature = "test-util",
    target_family = "unix",
    not(target_os = "macos")
))]
pub use volume::magic_is_local;
#[cfg(all(feature = "test-util", target_os = "macos"))]
pub use volume::name_is_local;
pub use volume::space_of;
pub use volume::volume_of;
#[cfg(feature = "test-util")]
pub use walk::Counts;
#[cfg(feature = "test-util")]
pub use walk::Found;
pub use walk::Kind;
pub use walk::VisitFn;
#[cfg(feature = "test-util")]
pub use walk::clone_entry;
#[cfg(feature = "test-util")]
pub use walk::clone_into_with;
#[cfg(all(
    feature = "test-util",
    any(target_os = "linux", target_os = "macos", target_family = "windows")
))]
pub use walk::copy_faithfully_on;
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
    #[cfg(feature = "test-util")]
    bounds: walk::Bounds,
    #[cfg(feature = "test-util")]
    counters: walk::Counters,
    /// The attribute whose every set a faithful copy given this progress fails, and the OS error
    /// it fails with, standing in for a set the system refuses.
    #[cfg(feature = "test-util")]
    refused_attribute: Option<(Vec<u8>, i32)>,
    /// Whether the target reads as holding the refused attribute, as one the system gives each
    /// new entry.
    #[cfg(feature = "test-util")]
    refused_held: bool,
    /// The name of the entries a faithful copy given this progress reads `flags` for, besides
    /// their own, standing in for flags a test can't set.
    #[cfg(feature = "test-util")]
    flagged: Option<(std::ffi::OsString, u64)>,
    /// Whether the walk, once it has cloned or copied an entry, waits before its next until it is
    /// stopped, standing in for a walk too long to finish before a stop, until it has waited.
    #[cfg(feature = "test-util")]
    held: AtomicBool,
    /// The name of the files whose faithful copy fails once its data is written, on Windows,
    /// standing in for a copy that fails partway.
    #[cfg(feature = "test-util")]
    failing: Option<std::ffi::OsString>,
    /// Which sets of an owner and group a faithful copy refuses with `EPERM`, standing in for a
    /// caller that isn't a member of the group, or may not give the owner either.
    #[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
    owners_refused: Option<Refused>,
}

/// The sets of an owner and group a test build's faithful copy refuses.
#[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Each set that gives a group.
    Group,
    /// Each set.
    Owner,
}

impl Progress {
    /// A progress whose walk queues at most `queue` directories and `continuations`
    /// continuations, and lists `batch` entries of a directory at a time.
    #[cfg(feature = "test-util")]
    pub fn bounded(queue: usize, continuations: usize, batch: usize) -> Self {
        Self {
            bounds: walk::Bounds {
                queue,
                continuations,
                batch,
            },
            ..Self::default()
        }
    }

    /// A progress whose faithful copy fails every set of the extended attribute `name` with the
    /// OS error `code`, in place of setting it.
    #[cfg(feature = "test-util")]
    pub fn refusing_attribute(name: &str, code: i32) -> Self {
        Self {
            refused_attribute: Some((name.as_bytes().to_vec(), code)),
            ..Self::default()
        }
    }

    /// A progress whose faithful copy fails every set of the extended attribute `name` with the
    /// OS error `code`, and reads the target as holding it, as one the system gives each new entry.
    #[cfg(feature = "test-util")]
    pub fn refusing_attribute_the_target_holds(name: &str, code: i32) -> Self {
        Self {
            refused_held: true,
            ..Self::refusing_attribute(name, code)
        }
    }

    /// Whether this progress's faithful copy reads the target as holding the refused attribute
    /// `name`.
    #[cfg(feature = "test-util")]
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn target_holds_refused(&self, name: &[u8]) -> bool {
        self.refused_held
            && self
                .refused_attribute
                .as_ref()
                .is_some_and(|(refused, _)| refused.as_slice() == name)
    }

    /// A progress whose faithful copy reads the flags `flags`, `stx_attributes` on Linux and
    /// `st_flags` on macOS, for each entry named `name`, besides its own.
    #[cfg(feature = "test-util")]
    pub fn flagging(name: &str, flags: u64) -> Self {
        Self {
            flagged: Some((name.into(), flags)),
            ..Self::default()
        }
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
    pub fn refusing_owners(refused: Refused) -> Self {
        Self {
            owners_refused: Some(refused),
            ..Self::default()
        }
    }

    /// The failure this progress's faithful copy stands in for a set of an owner, with a group
    /// where `gives_group` is set, where it refuses one.
    #[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
    fn owner_refusal(&self, gives_group: bool) -> Option<io::Error> {
        match self.owners_refused {
            Some(Refused::Owner) => Some(io::Error::from_raw_os_error(libc::EPERM)),
            Some(Refused::Group) if gives_group => Some(io::Error::from_raw_os_error(libc::EPERM)),
            _ => None,
        }
    }

    /// A progress whose faithful copy fails the copy of each file named `name` once its data is
    /// written, on Windows.
    #[cfg(feature = "test-util")]
    pub fn failing_file(name: &str) -> Self {
        Self {
            failing: Some(name.into()),
            ..Self::default()
        }
    }

    /// The failure this progress's faithful copy stands in for once the data of the file at
    /// `path` is written, where it names the file.
    #[cfg(feature = "test-util")]
    #[cfg_attr(not(target_family = "windows"), allow(dead_code))]
    fn failure_after_data(&self, path: &Path) -> io::Result<()> {
        match &self.failing {
            Some(name) if path.file_name() == Some(name.as_os_str()) => {
                Err(io::Error::other("a copy that fails partway"))
            }
            _ => Ok(()),
        }
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

    /// The flags this progress's faithful copy stands in for the entry at `path`.
    #[cfg(feature = "test-util")]
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn flags_standing_in(&self, path: &Path) -> u64 {
        self.flagged
            .as_ref()
            .filter(|(name, _)| path.file_name() == Some(name.as_os_str()))
            .map_or(0, |&(_, flags)| flags)
    }

    /// The failure this progress's faithful copy stands in for the set of the attribute `name`,
    /// where it names one.
    #[cfg(feature = "test-util")]
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn attribute_refusal(&self, name: &[u8]) -> Option<io::Error> {
        self.refused_attribute
            .as_ref()
            .filter(|(refused, _)| refused.as_slice() == name)
            .map(|&(_, code)| io::Error::from_raw_os_error(code))
    }

    /// What the test build counted of the walk.
    #[cfg(feature = "test-util")]
    pub fn counts(&self) -> Counts {
        self.counters.stats()
    }

    /// The bounds of the walk, which only a test build sets.
    #[cfg_attr(not(feature = "test-util"), allow(clippy::unused_self))]
    fn bounds(&self) -> walk::Bounds {
        #[cfg(feature = "test-util")]
        {
            self.bounds
        }
        #[cfg(not(feature = "test-util"))]
        {
            walk::BOUNDS
        }
    }

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

    /// Whether the clone has been asked to stop.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}

/// What a running [`copy_faithfully`] left as its target set it: the `security.*` extended
/// attributes, each with the entries it did so for, and the entries that kept the target's group.
#[derive(Debug, Default)]
struct Kept {
    attributes: parking_lot::Mutex<Vec<(String, u64)>>,
    groups: AtomicU64,
}

impl Kept {
    /// Counts one more entry for the attribute `name`.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn count(&self, name: &[u8]) {
        let mut counts = self.attributes.lock();
        match counts
            .iter_mut()
            .find(|(counted, _)| counted.as_bytes() == name)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((String::from_utf8_lossy(name).into_owned(), 1)),
        }
    }

    /// Counts one more entry that kept the target's group.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn count_group(&self) {
        self.groups.fetch_add(1, Ordering::Relaxed);
    }

    /// The attributes counted, each with its count, and the entries that kept the target's group.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", target_family = "windows")),
        allow(dead_code)
    )]
    fn into_counts(self) -> (Vec<(String, u64)>, u64) {
        (self.attributes.into_inner(), self.groups.into_inner())
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

/// Copies the entries of the directory `source` into the empty directory `destination`, leaving
/// out those `visit` skips, so that the copy holds what the source does and is on its device, as
/// `faithful.rs` and `faithful_windows.rs` describe. Fails with [`io::ErrorKind::Unsupported`] on
/// other platforms.
pub fn copy_faithfully(
    source: &Path,
    destination: &Path,
    visit: &VisitFn<'_>,
    progress: &Progress,
) -> io::Result<FaithfulCopy> {
    #[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
    {
        walk::copy_faithfully_on(
            Some(crate::pool::SyscallPool::global()),
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

/// The bytes a thread of a faithful copy copies through at a time, where `copy_file_range` can't
/// copy on Linux, and on macOS and Windows.
#[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
const COPY_BUFFER: usize = 256 << 10;

/// `err`, which `call` failed with, naming the call, as a faithful copy's error does.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn failed_call(err: io::Error, call: &str) -> io::Error {
    io::Error::new(err.kind(), format!("{call}: {err}"))
}

/// The failure of an entry a faithful copy doesn't keep, named by `what` it is.
fn refused(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{what}, which a faithful copy can't keep"),
    )
}

/// Clones the entries of the directory `source` into the directory `destination`, which exists,
/// with `backend`, leaving out the entries `visit` skips, on the calling thread and the threads
/// the syscall pool lends it, at most half its cap, so other file operations keep the rest. Its
/// memory does not grow with the number of files or the width of a directory, as the walk's
/// bounds hold it. `progress` counts what it does and can stop it.
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
