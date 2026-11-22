// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Cloning or copying the entries of a directory tree so that the copy shares the source's data
//! blocks (copy-on-write cloning), through one backend per filesystem:
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
//! directories, and [`clone_entry`] clones an entry with the backend it chose. All are blocking.
//!
//! A file keeps its permissions and its modification time, read from the source's open handle
//! once its data is cloned or copied, so a file rewritten during the clone never carries a time
//! older than its content. Symbolic links are copied as links. An [`OpenDirectory`] lists a
//! directory of the source, and its entries are reached through it, as `source.rs` describes.
//!
//! [`faithful`] copies an entry keeping what cloning leaves: holes, owners, extended attributes,
//! ACLs, and the times of directories and links on Linux and macOS, and sparse ranges, alternate
//! data streams, security descriptors and attributes on Windows.

mod apfs;
mod copy;
mod detect;
pub mod entry;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod faithful;
#[cfg(target_family = "windows")]
#[path = "faithful_windows.rs"]
pub mod faithful;
mod reflink;
mod refs;
mod source;
mod volume;

use std::io;
use std::path::Path;
use std::sync::atomic::AtomicU64;
#[cfg(feature = "test-util")]
use std::sync::atomic::AtomicUsize;
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
pub use source::Listed;
pub use source::ListedDirectory;
pub use source::OpenDirectory;
pub use source::Source;
pub use source::is_gone_kind;
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

    /// [`Self::clone_file`] for an entry a walk reaches through the directory that holds it.
    pub fn clone_source(self, source: source::Source<'_>, destination: &Path) -> io::Result<()> {
        match self {
            Backend::Apfs => apfs::clone_file(source, destination),
            Backend::Reflink => reflink::clone_file(source, destination),
            Backend::Refs => refs::clone_file(source, destination),
            Backend::Copy => copy::file(source, destination),
        }
    }
}

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

/// Clones the entry of `kind` `source` names to `destination` with `backend`: a file, a link, or
/// a directory as a whole, which only APFS clones. A link replaced by a file after it was listed
/// is cloned as the file. Fails with [`io::ErrorKind::Unsupported`] where the backend cannot.
pub fn clone_entry(
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

/// Where a test build's copy of an entry stands in for what a test can't arrange, and what it
/// counts: in any other build, nothing.
#[derive(Debug, Default)]
pub struct TestPoints {
    /// The attribute whose every set a faithful copy fails, and the OS error it fails with,
    /// standing in for a set the system refuses.
    #[cfg(feature = "test-util")]
    refused_attribute: Option<(Vec<u8>, i32)>,
    /// Whether the target reads as holding the refused attribute, as one the system gives each
    /// new entry.
    #[cfg(feature = "test-util")]
    refused_held: bool,
    /// The name of the entries a faithful copy reads `flags` for, besides their own, standing in
    /// for flags a test can't set.
    #[cfg(feature = "test-util")]
    flagged: Option<(std::ffi::OsString, u64)>,
    /// The name of the files whose faithful copy fails once its data is written, on Windows,
    /// standing in for a copy that fails partway.
    #[cfg(feature = "test-util")]
    failing: Option<std::ffi::OsString>,
    /// Which sets of an owner and group a faithful copy refuses with `EPERM`, standing in for a
    /// caller that isn't a member of the group, or may not give the owner either.
    #[cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]
    owners_refused: Option<Refused>,
    /// The flushes of a faithful copy's file system after its last file.
    #[cfg(feature = "test-util")]
    flushed: AtomicUsize,
    /// The files a faithful copy flushed one by one, each on its copying thread before its handle
    /// closes, on macOS and Windows.
    #[cfg(feature = "test-util")]
    flushed_files: AtomicUsize,
    /// The most bytes of an extended attribute's value a faithful copy held at once, on Linux and
    /// macOS.
    #[cfg(feature = "test-util")]
    value_held: AtomicUsize,
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

#[cfg(feature = "test-util")]
impl TestPoints {
    /// Points whose faithful copy fails every set of the extended attribute `name` with the OS
    /// error `code`, in place of setting it.
    pub fn refusing_attribute(name: &str, code: i32) -> Self {
        Self {
            refused_attribute: Some((name.as_bytes().to_vec(), code)),
            ..Self::default()
        }
    }

    /// Points whose faithful copy fails every set of the extended attribute `name` with the OS
    /// error `code`, and reads the target as holding it, as one the system gives each new entry.
    pub fn refusing_attribute_the_target_holds(name: &str, code: i32) -> Self {
        Self {
            refused_held: true,
            ..Self::refusing_attribute(name, code)
        }
    }

    /// Whether a faithful copy reads the target as holding the refused attribute `name`.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn target_holds_refused(&self, name: &[u8]) -> bool {
        self.refused_held
            && self
                .refused_attribute
                .as_ref()
                .is_some_and(|(refused, _)| refused.as_slice() == name)
    }

    /// Points whose faithful copy reads the flags `flags`, `stx_attributes` on Linux and
    /// `st_flags` on macOS, for each entry named `name`, besides its own.
    pub fn flagging(name: &str, flags: u64) -> Self {
        Self {
            flagged: Some((name.into(), flags)),
            ..Self::default()
        }
    }

    /// Points whose faithful copy refuses the sets of an owner and group `refused` names with
    /// `EPERM`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn refusing_owners(refused: Refused) -> Self {
        Self {
            owners_refused: Some(refused),
            ..Self::default()
        }
    }

    /// The failure a faithful copy stands in for a set of an owner, with a group where
    /// `gives_group` is set, where it refuses one.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn owner_refusal(&self, gives_group: bool) -> Option<io::Error> {
        match self.owners_refused {
            Some(Refused::Owner) => Some(io::Error::from_raw_os_error(libc::EPERM)),
            Some(Refused::Group) if gives_group => Some(io::Error::from_raw_os_error(libc::EPERM)),
            _ => None,
        }
    }

    /// Points whose faithful copy fails the copy of each file named `name` once its data is
    /// written, on Windows.
    pub fn failing_file(name: &str) -> Self {
        Self {
            failing: Some(name.into()),
            ..Self::default()
        }
    }

    /// The failure a faithful copy stands in for once the data of the file at `path` is written,
    /// where it names the file.
    #[cfg_attr(not(target_family = "windows"), allow(dead_code))]
    fn failure_after_data(&self, path: &Path) -> io::Result<()> {
        match &self.failing {
            Some(name) if path.file_name() == Some(name.as_os_str()) => {
                Err(io::Error::other("a copy that fails partway"))
            }
            _ => Ok(()),
        }
    }

    /// The flags a faithful copy stands in for the entry at `path`.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn flags_standing_in(&self, path: &Path) -> u64 {
        self.flagged
            .as_ref()
            .filter(|(name, _)| path.file_name() == Some(name.as_os_str()))
            .map_or(0, |&(_, flags)| flags)
    }

    /// The failure a faithful copy stands in for the set of the attribute `name`, where it names
    /// one.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn attribute_refusal(&self, name: &[u8]) -> Option<io::Error> {
        self.refused_attribute
            .as_ref()
            .filter(|(refused, _)| refused.as_slice() == name)
            .map(|&(_, code)| io::Error::from_raw_os_error(code))
    }

    /// The flushes of a faithful copy's file system after its last file.
    pub fn flushed(&self) -> usize {
        self.flushed.load(Ordering::Relaxed)
    }

    /// The files a faithful copy flushed one by one, on macOS and Windows.
    pub fn flushed_files(&self) -> usize {
        self.flushed_files.load(Ordering::Relaxed)
    }

    /// The most bytes of an extended attribute's value a faithful copy held at once, on Linux and
    /// macOS.
    pub fn value_held(&self) -> usize {
        self.value_held.load(Ordering::Relaxed)
    }
}

/// What a running faithful copy left as its target set it: the `security.*` extended attributes,
/// each with the entries it did so for, and the entries that kept the target's group.
#[derive(Debug, Default)]
pub struct Kept {
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
    pub fn into_counts(self) -> (Vec<(String, u64)>, u64) {
        (self.attributes.into_inner(), self.groups.into_inner())
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
pub fn refused(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{what}, which a faithful copy can't keep"),
    )
}
