// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The entries of the source tree as a clone reads them. The walk holds each directory it lists
//! open and checks that the handle is the directory its parent listed. On Unix it reaches every
//! entry through that handle by name, so a link swapped in above the directory while it is listed
//! cannot lead a read out of the tree. On Windows it reaches each entry by path, and the handle,
//! which shares no delete access, keeps the directory and every directory above it from being
//! renamed or removed, so the path names the directory until it is listed.

use std::ffi::OsStr;
use std::io;
use std::ops::Deref;
#[cfg(target_family = "unix")]
use std::os::fd::AsRawFd;
#[cfg(target_family = "unix")]
use std::os::fd::BorrowedFd;
use std::path::Path;

use super::entry::Identity;

/// An entry of the source tree: its path, and on Unix the open directory that holds it and its
/// name there, through which it is read. It derefs to its path, which names it in messages and
/// which Windows reads it through.
#[derive(Clone, Copy, Debug)]
pub struct Source<'a> {
    path: &'a Path,
    #[cfg(target_family = "unix")]
    parent: BorrowedFd<'a>,
    #[cfg(target_family = "unix")]
    name: &'a OsStr,
}

impl Deref for Source<'_> {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path
    }
}

impl AsRef<Path> for Source<'_> {
    fn as_ref(&self) -> &Path {
        self.path
    }
}

/// A directory of the source tree as its parent listed it: its identity, and on Unix its
/// permission bits.
pub(super) struct ListedDirectory {
    pub(super) identity: Identity,
    #[cfg(target_family = "unix")]
    pub(super) mode: u32,
}

impl Source<'_> {
    /// The raw descriptor of the directory that holds the entry.
    #[cfg(target_family = "unix")]
    pub(super) fn parent_fd(&self) -> std::os::fd::RawFd {
        self.parent.as_raw_fd()
    }

    /// The entry's name, NUL-terminated, for a call relative to [`Self::parent_fd`].
    #[cfg(target_family = "unix")]
    pub(super) fn name_for_call(&self) -> io::Result<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;

        Ok(std::ffi::CString::new(self.name.as_bytes())?)
    }

    /// What `fstatat` reports for the entry itself, a link not followed.
    #[cfg(target_family = "unix")]
    pub(super) fn stat(&self) -> io::Result<libc::stat> {
        let name = self.name_for_call()?;
        // SAFETY: zeroed is a valid `stat`, which the call fills.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the directory is open for the borrow, the name is NUL-terminated, and `stat`
        // is writable.
        let done = unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                &raw mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if done == 0 {
            Ok(stat)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// The entry as a directory, or `None` where something else is there now.
    #[cfg(target_family = "unix")]
    pub(super) fn directory(&self) -> io::Result<Option<ListedDirectory>> {
        let stat = self.stat()?;
        Ok(
            (mode_bits(stat.st_mode) & mode_bits(libc::S_IFMT) == mode_bits(libc::S_IFDIR)).then(
                || ListedDirectory {
                    identity: (identity_part(stat.st_dev), identity_part(stat.st_ino)),
                    mode: mode_bits(stat.st_mode) & super::entry::PERMISSION_BITS,
                },
            ),
        )
    }

    /// The entry as a directory, or `None` where something else is there now.
    #[cfg(target_family = "windows")]
    pub(super) fn directory(&self) -> io::Result<Option<ListedDirectory>> {
        Ok(super::entry::directory_identity(self.path)?
            .map(|identity| ListedDirectory { identity }))
    }

    /// The length of the file.
    #[cfg(target_family = "unix")]
    pub(super) fn len(&self) -> io::Result<u64> {
        Ok(u64::try_from(self.stat()?.st_size).unwrap_or(0))
    }

    /// Whether a file is there now, a link not followed.
    #[cfg(target_family = "unix")]
    pub(super) fn is_file(&self) -> bool {
        self.stat().is_ok_and(|stat| {
            mode_bits(stat.st_mode) & mode_bits(libc::S_IFMT) == mode_bits(libc::S_IFREG)
        })
    }

    /// Whether a file is there now, a link not followed.
    #[cfg(target_family = "windows")]
    pub(super) fn is_file(&self) -> bool {
        std::fs::symlink_metadata(self.path).is_ok_and(|now| now.is_file())
    }

    /// Whether the entry is gone, or is no longer a directory, a file or a link.
    #[cfg(target_family = "unix")]
    pub(super) fn is_gone(&self) -> bool {
        match self.stat() {
            Ok(stat) => {
                let kind = mode_bits(stat.st_mode) & mode_bits(libc::S_IFMT);
                ![libc::S_IFDIR, libc::S_IFREG, libc::S_IFLNK]
                    .iter()
                    .any(|&known| kind == mode_bits(known))
            }
            Err(err) => is_gone_kind(err.kind()),
        }
    }

    /// Whether the entry is gone, or is no longer a directory, a file or a link.
    #[cfg(target_family = "windows")]
    pub(super) fn is_gone(&self) -> bool {
        match std::fs::symlink_metadata(self.path) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                !(file_type.is_dir() || file_type.is_file() || file_type.is_symlink())
            }
            Err(err) => is_gone_kind(err.kind()),
        }
    }
}

/// A device or inode number as [`Identity`] holds it, converted as `MetadataExt::dev` and
/// `MetadataExt::ino` convert theirs, so the two compare.
#[cfg(target_family = "unix")]
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::unnecessary_cast
)]
fn identity_part(value: impl Into<i128>) -> u64 {
    value.into() as u64
}

/// The bits of a mode from `stat`, which is 16 bits wide on some systems and 32 on others.
#[cfg(target_family = "unix")]
#[allow(clippy::useless_conversion)]
fn mode_bits(mode: libc::mode_t) -> u32 {
    u32::from(mode)
}

/// Whether a failure of `kind` on an entry's path can mean the entry is gone: the entry or a
/// directory above it is not there, or a directory above it is no longer a directory.
pub(super) fn is_gone_kind(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// Runs `with` on the entry at `path`, reached through its parent directory opened by path, for a
/// caller that holds a path rather than a listing.
pub(super) fn at_path<R>(
    path: &Path,
    with: impl FnOnce(Source<'_>) -> io::Result<R>,
) -> io::Result<R> {
    #[cfg(target_family = "unix")]
    {
        use std::os::fd::AsFd;

        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "names no entry"))?;
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let directory = std::fs::File::open(parent)?;
        with(Source {
            path,
            parent: directory.as_fd(),
            name,
        })
    }
    #[cfg(target_family = "windows")]
    {
        with(Source { path })
    }
}

/// A directory of the source tree the walk lists, held open once the walk has checked that the
/// directory is the one its parent listed.
pub(super) struct OpenDirectory {
    /// On Windows only held, so the directory keeps its path while it is listed.
    #[cfg_attr(target_family = "windows", allow(dead_code))]
    handle: std::fs::File,
}

impl OpenDirectory {
    /// Opens the directory at `path`, without following a link at its last component, unless
    /// `expected` is `None`, as for the walk's root, which may be reached through a link. `None`
    /// where a directory with an identity other than `expected` is there, as where it was
    /// replaced, or a link swapped in above it leads elsewhere, since its parent listed it.
    #[cfg(target_family = "unix")]
    pub(super) fn open(path: &Path, expected: Option<&Identity>) -> io::Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::OpenOptionsExt;

        let no_follow = if expected.is_some() {
            libc::O_NOFOLLOW
        } else {
            0
        };
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | no_follow)
            .open(path)?;
        let metadata = handle.metadata()?;
        if expected.is_some_and(|expected| *expected != (metadata.dev(), metadata.ino())) {
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }

    /// Opens the directory at `path`, held so that neither it nor a directory above it is renamed
    /// or removed while it is open, without following a link or junction at its last component,
    /// unless `expected` is `None`, as for the walk's root, which may be reached through one.
    /// `None` where a directory with an identity other than `expected` is there, or something
    /// other than a directory is.
    #[cfg(target_family = "windows")]
    pub(super) fn open(path: &Path, expected: Option<&Identity>) -> io::Result<Option<Self>> {
        let Some((handle, identity)) = super::entry::open_directory_held(path, expected.is_none())?
        else {
            return Ok(None);
        };
        if expected.is_some_and(|expected| *expected != identity) {
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }

    /// The entry `name` of this directory, whose path is `path`.
    #[cfg(target_family = "unix")]
    pub(super) fn entry<'a>(&'a self, path: &'a Path, name: &'a OsStr) -> Source<'a> {
        use std::os::fd::AsFd;

        Source {
            path,
            parent: self.handle.as_fd(),
            name,
        }
    }

    /// The entry of this directory at `path`.
    #[cfg(target_family = "windows")]
    #[allow(clippy::unused_self)]
    pub(super) fn entry<'a>(&'a self, path: &'a Path, _name: &'a OsStr) -> Source<'a> {
        Source { path }
    }

    /// The names and kinds the directory lists, read through the open handle: `.` and `..`
    /// are left out, and a kind the listing does not give is read with `fstatat`.
    #[cfg(target_family = "unix")]
    pub(super) fn names(
        &self,
    ) -> io::Result<impl Iterator<Item = io::Result<(std::ffi::OsString, super::Kind)>> + '_> {
        use std::os::fd::IntoRawFd;

        // `fdopendir` takes the descriptor it is given, so it gets a duplicate of the handle.
        let duplicate = self.handle.try_clone()?.into_raw_fd();
        // SAFETY: the descriptor is open and owned here; `fdopendir` takes it over on success.
        let stream = unsafe { libc::fdopendir(duplicate) };
        let Some(stream) = std::ptr::NonNull::new(stream) else {
            let err = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed, so the descriptor is still owned here.
            unsafe { libc::close(duplicate) };
            return Err(err);
        };
        let stream = Stream(stream);
        // A stream that failed is read no further, as `readdir` may fail the same way again.
        let mut failed = false;
        Ok(std::iter::from_fn(move || {
            loop {
                if failed {
                    return None;
                }
                set_errno_zero();
                // SAFETY: the stream is open until `Stream` is dropped with the iterator.
                let found = unsafe { libc::readdir(stream.as_ptr()) };
                if found.is_null() {
                    let err = io::Error::last_os_error();
                    failed = true;
                    return (err.raw_os_error() != Some(0)).then_some(Err(err));
                }
                // SAFETY: `readdir` returned an entry valid until the next call on the stream.
                let entry = unsafe { &*found };
                // SAFETY: `d_name` is NUL-terminated.
                let name = unsafe { std::ffi::CStr::from_ptr(entry.d_name.as_ptr()) };
                let bytes = name.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                let name: &OsStr = std::os::unix::ffi::OsStrExt::from_bytes(bytes);
                let kind = match entry.d_type {
                    libc::DT_DIR => super::Kind::Directory,
                    libc::DT_REG => super::Kind::File,
                    libc::DT_LNK => super::Kind::Symlink,
                    libc::DT_UNKNOWN => match self.entry(Path::new(""), name).stat() {
                        Ok(stat) => kind_of_mode(mode_bits(stat.st_mode)),
                        Err(err) => return Some(Err(err)),
                    },
                    _ => super::Kind::Other,
                };
                return Some(Ok((name.to_os_string(), kind)));
            }
        }))
    }
}

/// The kind of entry a mode from `stat` names.
#[cfg(target_family = "unix")]
fn kind_of_mode(mode: u32) -> super::Kind {
    let kind = mode & mode_bits(libc::S_IFMT);
    if kind == mode_bits(libc::S_IFDIR) {
        super::Kind::Directory
    } else if kind == mode_bits(libc::S_IFREG) {
        super::Kind::File
    } else if kind == mode_bits(libc::S_IFLNK) {
        super::Kind::Symlink
    } else {
        super::Kind::Other
    }
}

/// A directory stream from `fdopendir`, closed when dropped.
#[cfg(target_family = "unix")]
struct Stream(std::ptr::NonNull<libc::DIR>);

#[cfg(target_family = "unix")]
impl Stream {
    /// The stream, borrowed from `self` so a closure that reads it owns the whole `Stream`.
    fn as_ptr(&self) -> *mut libc::DIR {
        self.0.as_ptr()
    }
}

#[cfg(target_family = "unix")]
impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream is open and closed only here.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

/// Clears `errno`, so a null from `readdir` tells the end of the stream from a failure.
#[cfg(target_family = "unix")]
fn set_errno_zero() {
    // SAFETY: the thread's `errno` is writable.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            *libc::__errno_location() = 0;
        }
        #[cfg(target_os = "android")]
        {
            *libc::__errno() = 0;
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            *libc::__error() = 0;
        }
    }
}
