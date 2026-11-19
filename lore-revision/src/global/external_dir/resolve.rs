// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Resolution of the external root: what the setting, the marker and the move record of a global
//! data directory say, read once per operation.

use std::path::Path;
use std::path::PathBuf;

use lore_error_set::prelude::*;

use super::EXTERNAL;
use super::ExternalDirError;
use super::marker::Marker;
use super::record::MOVE_RECORD;
use super::setting::Setting;
use crate::instance::InstanceId;
use crate::repository::DOT_LORE;

/// What `<global data>/external`, the default location, holds, from one `lstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultLocation {
    /// Nothing.
    Missing,
    /// A directory.
    Directory,
    /// A symbolic link or a junction.
    Redirect,
    /// Something else.
    Other,
    /// What it holds can't be read.
    Unreadable,
}

/// Why an external root isn't available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The setting, the marker or the move record can't be read, or the default location can't
    /// be examined.
    Unreadable,
    /// The setting names a relative path.
    RelativeSetting,
    /// The default location holds a redirect, and no setting names the root.
    RedirectWithoutSetting,
    /// The setting names the default location, which holds a redirect.
    DefaultIsRedirect,
    /// The root the setting names is missing.
    Missing,
    /// What the setting names isn't a directory.
    NotADirectory,
    /// The root holds no marker.
    NoMarker,
    /// The root's marker holds another root ID.
    ForeignMarker,
    /// The root's marker is from another generation.
    OtherGeneration,
    /// The root belongs to another user, or someone else can write it.
    NotPrivate,
}

impl Unavailable {
    /// The reason, in words.
    pub fn describe(self) -> &'static str {
        match self {
            Unavailable::Unreadable => "its setting, marker or move record can't be read",
            Unavailable::RelativeSetting => "external-root names a relative path",
            Unavailable::RedirectWithoutSetting => {
                "the default location holds a link, and external-root is missing"
            }
            Unavailable::DefaultIsRedirect => {
                "external-root names the default location, which holds a link"
            }
            Unavailable::Missing => "it is missing",
            Unavailable::NotADirectory => "it isn't a directory",
            Unavailable::NoMarker => "it holds no marker",
            Unavailable::ForeignMarker => "its marker belongs to another global data directory",
            Unavailable::OtherGeneration => "its marker is from another generation",
            Unavailable::NotPrivate => "it belongs to another user, or others can write it",
        }
    }
}

/// Whether the external root can be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// The root at this path is available.
    Available(PathBuf),
    /// The root isn't available, for this reason.
    Unavailable(Unavailable),
    /// A move record is present.
    Interrupted,
}

/// The external root of a global data directory, as one resolution found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootState {
    /// The global data directory.
    pub global: PathBuf,
    /// What the default location holds.
    pub default: DefaultLocation,
    /// Whether the root can be used.
    pub availability: Availability,
}

/// What a lookup of an instance's external directory found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The instance's external `.lore` directory, at this path.
    Present(PathBuf),
    /// The instance has no external directory.
    Absent,
    /// Whether the instance has an external directory can't be told, for this reason.
    Unknown(Unknown),
}

/// Why a lookup of an instance's external directory is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unknown {
    /// A move of the external root is recorded.
    Interrupted,
    /// The external root isn't available.
    Unavailable(Unavailable),
    /// The instance's external directory can't be examined.
    StatFailed,
    /// The instance's external directory is missing from the configured root, and present at the
    /// default location.
    AtDefaultLocation,
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unknown::Interrupted => f.write_str("a move of the external root is interrupted"),
            Unknown::Unavailable(reason) => {
                write!(f, "the external root is unavailable: {}", reason.describe())
            }
            Unknown::StatFailed => f.write_str("its external directory can't be examined"),
            Unknown::AtDefaultLocation => f.write_str(
                "its external directory is missing from the external root and present at the \
                 default location",
            ),
        }
    }
}

impl RootState {
    /// The external `.lore` directory of `instance_id` in this root, with one `stat`, and a
    /// second one of the instance at the default location only where it is absent from a
    /// configured root while a directory is at the default location. A stat that fails other
    /// than with not-found answers unknown.
    pub fn lookup(&self, instance_id: InstanceId) -> Lookup {
        let root = match &self.availability {
            Availability::Available(root) => root,
            Availability::Unavailable(reason) => {
                return Lookup::Unknown(Unknown::Unavailable(*reason));
            }
            Availability::Interrupted => return Lookup::Unknown(Unknown::Interrupted),
        };
        let mut digits = [0u8; 32];
        let name = crate::util::to_hex_str(instance_id.data(), &mut digits);
        let mut dot_lore = PathBuf::with_capacity(root.as_os_str().len() + name.len() + 8);
        dot_lore.push(root);
        dot_lore.push(name);
        dot_lore.push(DOT_LORE);
        match std::fs::metadata(&dot_lore) {
            Ok(_) => Lookup::Present(dot_lore),
            Err(err) if names_nothing(&err) => self.absent_unless_at_default(root, name),
            Err(_) => Lookup::Unknown(Unknown::StatFailed),
        }
    }

    /// Absent, unless `root` is a configured root and the instance named `name` is in a
    /// directory at the default location, or that can't be told.
    fn absent_unless_at_default(&self, root: &Path, name: &str) -> Lookup {
        if root.strip_prefix(&self.global) == Ok(Path::new(EXTERNAL)) {
            return Lookup::Absent;
        }
        match self.default {
            DefaultLocation::Directory => {
                match std::fs::symlink_metadata(self.global.join(EXTERNAL).join(name)) {
                    Ok(_) => Lookup::Unknown(Unknown::AtDefaultLocation),
                    Err(err) if names_nothing(&err) => Lookup::Absent,
                    Err(_) => Lookup::Unknown(Unknown::StatFailed),
                }
            }
            DefaultLocation::Unreadable => Lookup::Unknown(Unknown::StatFailed),
            DefaultLocation::Missing | DefaultLocation::Redirect | DefaultLocation::Other => {
                Lookup::Absent
            }
        }
    }
}

/// Whether a stat failed because nothing is at the path: it is missing, or a file is where a
/// directory on the way to it would be.
fn names_nothing(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Resolves the external root of the caller's global data directory, as [`resolve_in`] does.
/// Blocking.
pub fn resolve() -> Result<RootState, ExternalDirError> {
    let global = crate::global::get_global_data_dir()
        .forward::<ExternalDirError>("finding the global data directory")?;
    Ok(resolve_in(global))
}

/// Resolves the external root of the global data directory `global`. It `lstat`s the default
/// location, checks for the move record, reads the setting and, where the setting names a root,
/// that root's marker, once each, and on Unix checks the owner and mode of the root. Blocking.
pub fn resolve_in(global: PathBuf) -> RootState {
    let default_path = global.join(EXTERNAL);
    let default = default_location(&default_path);
    let availability = availability(&global, default_path, default);
    RootState {
        global,
        default,
        availability,
    }
}

/// The path of the root that a setting or a move record names as `path`, the default location of
/// `global` where `path` is empty.
pub fn root_path(global: &Path, path: &str) -> PathBuf {
    if path.is_empty() {
        global.join(EXTERNAL)
    } else {
        PathBuf::from(path)
    }
}

/// Whether the root that a setting or a move record names as `path` in `global` is available by
/// the rules a resolution applies, for `setting`. With no setting, only the default location is
/// available, while it is missing or a directory. Blocking.
pub fn root_available(global: &Path, path: &str, setting: Option<&Setting>) -> bool {
    let root = root_path(global, path);
    let default_path = global.join(EXTERNAL);
    let at_default = root == default_path;
    let Some(setting) = setting else {
        return at_default
            && matches!(
                default_location(&default_path),
                DefaultLocation::Missing | DefaultLocation::Directory
            );
    };
    if at_default && default_location(&default_path) != DefaultLocation::Directory {
        return false;
    }
    root.is_absolute() && marked_root(global, &root, setting).is_ok()
}

/// What the default location at `path` holds.
fn default_location(path: &Path) -> DefaultLocation {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => DefaultLocation::Redirect,
        Ok(metadata) if metadata.is_dir() => DefaultLocation::Directory,
        Ok(_) => DefaultLocation::Other,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DefaultLocation::Missing,
        Err(_) => DefaultLocation::Unreadable,
    }
}

/// The availability of the root of `global`, whose default location at `default_path` holds
/// `default`.
fn availability(global: &Path, default_path: PathBuf, default: DefaultLocation) -> Availability {
    #[cfg(feature = "test-util")]
    test_util::count(global, test_util::RootFile::Record);
    match std::fs::symlink_metadata(global.join(MOVE_RECORD)) {
        Ok(_) => return Availability::Interrupted,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Availability::Unavailable(Unavailable::Unreadable),
    }
    #[cfg(feature = "test-util")]
    test_util::count(global, test_util::RootFile::Setting);
    let setting = match Setting::load(global) {
        Ok(setting) => setting,
        Err(_) => return Availability::Unavailable(Unavailable::Unreadable),
    };
    let Some(setting) = setting else {
        return match default {
            DefaultLocation::Missing | DefaultLocation::Directory => {
                Availability::Available(default_path)
            }
            DefaultLocation::Redirect => {
                Availability::Unavailable(Unavailable::RedirectWithoutSetting)
            }
            DefaultLocation::Other => Availability::Unavailable(Unavailable::NotADirectory),
            DefaultLocation::Unreadable => Availability::Unavailable(Unavailable::Unreadable),
        };
    };
    let root = if setting.path.is_empty() || Path::new(&setting.path) == default_path {
        match default {
            DefaultLocation::Directory => default_path,
            DefaultLocation::Missing => return Availability::Unavailable(Unavailable::Missing),
            DefaultLocation::Redirect => {
                return Availability::Unavailable(Unavailable::DefaultIsRedirect);
            }
            DefaultLocation::Other => {
                return Availability::Unavailable(Unavailable::NotADirectory);
            }
            DefaultLocation::Unreadable => {
                return Availability::Unavailable(Unavailable::Unreadable);
            }
        }
    } else {
        let root = PathBuf::from(&setting.path);
        if !root.is_absolute() {
            return Availability::Unavailable(Unavailable::RelativeSetting);
        }
        root
    };
    match marked_root(global, &root, &setting) {
        Ok(()) => Availability::Available(root),
        Err(reason) => Availability::Unavailable(reason),
    }
}

/// Checks that `root` is a directory that holds the marker of `setting` and, on Unix, that the
/// root is the user's alone. Its marker gets no such check: no one else can change an entry of a
/// root that passes it.
fn marked_root(global: &Path, root: &Path, setting: &Setting) -> Result<(), Unavailable> {
    checked_marker(global, root, Some(setting)).map(drop)
}

/// The marker of `root`, checked as [`marked_root`] checks it, against `setting` where there is
/// one, and accepting any marker where there is none. Blocking.
pub fn checked_marker(
    global: &Path,
    root: &Path,
    setting: Option<&Setting>,
) -> Result<Marker, Unavailable> {
    let metadata =
        std::fs::metadata(root).map_err(|err| failed_read(&err, Unavailable::Missing))?;
    if !metadata.is_dir() {
        return Err(Unavailable::NotADirectory);
    }
    #[cfg(target_family = "unix")]
    private_to_user(root, &metadata)?;
    #[cfg(feature = "test-util")]
    test_util::count(global, test_util::RootFile::Marker);
    #[cfg(not(feature = "test-util"))]
    let _ = global;
    let marker = match Marker::load(root) {
        Ok(Some(marker)) => marker,
        Ok(None) => return Err(Unavailable::NoMarker),
        Err(_) => return Err(Unavailable::Unreadable),
    };
    let Some(setting) = setting else {
        return Ok(marker);
    };
    if marker.id != setting.id {
        return Err(Unavailable::ForeignMarker);
    }
    if marker.generation != setting.generation {
        return Err(Unavailable::OtherGeneration);
    }
    Ok(marker)
}

/// The reason a read that failed with `err` gives: `missing` where nothing is at the path, else
/// [`Unavailable::Unreadable`].
fn failed_read(err: &std::io::Error, missing: Unavailable) -> Unavailable {
    if err.kind() == std::io::ErrorKind::NotFound {
        missing
    } else {
        Unavailable::Unreadable
    }
}

/// Checks that the object at `path`, with `metadata`, passes [`is_private`] for the user this
/// process runs as.
#[cfg(target_family = "unix")]
fn private_to_user(path: &Path, metadata: &std::fs::Metadata) -> Result<(), Unavailable> {
    use std::os::unix::fs::MetadataExt;

    let acl_lets_others_write = acl_lets_others_write(path, metadata.uid())?;
    // SAFETY: geteuid has no preconditions.
    let user = unsafe { libc::geteuid() };
    if is_private(metadata.uid(), metadata.mode(), acl_lets_others_write, user) {
        Ok(())
    } else {
        Err(Unavailable::NotPrivate)
    }
}

/// Whether the extended ACL of `path` lets anyone but its owner `owner` write it.
#[cfg(target_os = "macos")]
pub(super) fn acl_lets_others_write(path: &Path, owner: u32) -> Result<bool, Unavailable> {
    lore_io::acl::extended_acl_lets_others_write(path, owner)
        .map_err(|err| failed_read(&err, Unavailable::Missing))
}

/// Linux has no ACL beyond what the mode's group bits bound, as [`is_private`] describes.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
pub(super) fn acl_lets_others_write(_path: &Path, _owner: u32) -> Result<bool, Unavailable> {
    Ok(false)
}

/// Whether an object owned by `owner`, with the permission bits of `mode`, is the user `user`'s
/// alone: `user` owns it, no write bit is set for group or others, and, on macOS, no extended ACL
/// lets anyone else write it. On Linux the group bits of an object with an access ACL are its
/// ACL's mask, which bounds every entry but the owner's and others', so `mode` covers the ACL.
#[cfg(target_family = "unix")]
#[lore_macro::test_pub]
pub(super) fn is_private(owner: u32, mode: u32, acl_lets_others_write: bool, user: u32) -> bool {
    owner == user && mode & 0o022 == 0 && !acl_lets_others_write
}

/// The count of the reads one resolution makes, per global data directory.
#[cfg(feature = "test-util")]
pub mod test_util {
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::LazyLock;

    use parking_lot::Mutex;

    /// A file a resolution reads.
    #[derive(Clone, Copy, Debug)]
    pub(super) enum RootFile {
        Setting,
        Marker,
        Record,
    }

    /// The reads of each file of a global data directory's external root so far.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Reads {
        /// Reads of `external-root`.
        pub setting: usize,
        /// Reads of a root's marker.
        pub marker: usize,
        /// Checks for `external.move`.
        pub record: usize,
    }

    static READS: LazyLock<Mutex<HashMap<PathBuf, Reads>>> = LazyLock::new(Default::default);

    pub(super) fn count(global: &Path, file: RootFile) {
        let mut reads = READS.lock();
        let reads = reads.entry(global.to_path_buf()).or_default();
        match file {
            RootFile::Setting => reads.setting += 1,
            RootFile::Marker => reads.marker += 1,
            RootFile::Record => reads.record += 1,
        }
    }

    /// The reads resolutions of the global data directory `global` have made.
    pub fn reads_in(global: &Path) -> Reads {
        READS.lock().get(global).copied().unwrap_or_default()
    }
}
