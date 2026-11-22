// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The entries of the source tree as a clone reads them. The walk holds each directory it lists
//! open and checks that the handle is the directory its parent listed. On Unix it reaches every
//! entry through that handle by name, so a link swapped in above the directory while it is listed
//! cannot lead a read out of the tree. On Windows it reaches each entry by path, and the handle,
//! which shares no delete access, keeps the directory and every directory above it from being
//! renamed or removed, so the path names the directory until it is listed.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::ops::Deref;
#[cfg(target_family = "unix")]
use std::os::fd::AsRawFd;
#[cfg(target_family = "unix")]
use std::os::fd::BorrowedFd;
use std::path::Path;

use super::Kind;
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
/// permission bits, on Windows its attributes.
pub struct ListedDirectory {
    pub identity: Identity,
    #[cfg(target_family = "unix")]
    pub mode: u32,
    #[cfg(target_family = "windows")]
    pub attributes: u32,
}

impl Source<'_> {
    /// The raw descriptor of the directory that holds the entry.
    #[cfg(target_family = "unix")]
    pub(super) fn parent_fd(&self) -> std::os::fd::RawFd {
        self.parent.as_raw_fd()
    }

    /// The entry's name, NUL-terminated, for a call relative to [`Self::parent_fd`].
    #[cfg(target_family = "unix")]
    #[inline]
    pub(super) fn name_for_call(&self) -> io::Result<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;

        Ok(std::ffi::CString::new(self.name.as_bytes())?)
    }

    /// What `fstatat` reports for the entry itself, a link not followed. Always inlined, since
    /// [`Self::directory`] runs it for every directory a walk lists.
    #[cfg(target_family = "unix")]
    #[inline(always)]
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

    /// The entry as a directory, or `None` where something else is there now. Always inlined,
    /// since the walk in `lore-revision` asks it of every directory it lists.
    #[cfg(target_family = "unix")]
    #[inline(always)]
    pub fn directory(&self) -> io::Result<Option<ListedDirectory>> {
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
    pub fn directory(&self) -> io::Result<Option<ListedDirectory>> {
        Ok(
            super::entry::directory_identity(self.path)?.map(|(identity, attributes)| {
                ListedDirectory {
                    identity,
                    attributes,
                }
            }),
        )
    }

    /// The length of the file.
    #[cfg(target_family = "unix")]
    fn len(&self) -> io::Result<u64> {
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
    pub fn is_gone(&self) -> bool {
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
    pub fn is_gone(&self) -> bool {
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
#[inline]
fn mode_bits(mode: libc::mode_t) -> u32 {
    u32::from(mode)
}

/// Whether a failure of `kind` on an entry's path can mean the entry is gone: the entry or a
/// directory above it is not there, or a directory above it is no longer a directory.
pub fn is_gone_kind(kind: io::ErrorKind) -> bool {
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

/// An entry an open directory listed: its name and its kind, and on Windows the length its
/// listing gave.
pub struct Listed {
    pub name: OsString,
    pub kind: Kind,
    #[cfg(target_family = "windows")]
    len: Option<u64>,
}

impl Listed {
    /// The length of the file this entry names, which `source` reaches.
    #[cfg(target_family = "unix")]
    pub fn len(&self, source: Source<'_>) -> io::Result<u64> {
        source.len()
    }

    /// The length of the file this entry names, which its listing gave.
    #[cfg(target_family = "windows")]
    pub fn len(&self, _source: Source<'_>) -> io::Result<u64> {
        self.len
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

/// A directory of the source tree a walk lists, held open once it is checked to be the one its
/// parent listed, with the entries it has not listed yet. On Unix its one descriptor is the one its
/// `readdir` stream reads, which the walk reaches its entries through. On Windows it holds the
/// directory's handle, which keeps the directory from being renamed or removed, and the open
/// search of its entries.
pub struct OpenDirectory {
    #[cfg(target_family = "unix")]
    stream: Stream,
    /// The access time the directory had as it was opened, where the listing keeps it, in seconds
    /// and nanoseconds: on macOS, which has no `O_NOATIME`, the listing sets it.
    #[cfg(target_family = "unix")]
    atime: Option<(i64, i64)>,
    /// Only held, so the directory keeps its path while it is listed.
    #[cfg(target_family = "windows")]
    #[allow(dead_code)]
    handle: std::fs::File,
    #[cfg(target_family = "windows")]
    listing: Listing,
}

impl OpenDirectory {
    /// Opens the directory at `path`, without following a link at its last component, unless
    /// `expected` is `None`, as for the walk's root, which may be reached through a link. `None`
    /// where a directory with an identity other than `expected` is there, as where it was
    /// replaced, or a link swapped in above it leads elsewhere, since its parent listed it. Where
    /// `keeps_atime` is set, listing it leaves its access time as it is, on Linux where the caller
    /// owns it.
    #[cfg(target_family = "unix")]
    pub fn open(
        path: &Path,
        expected: Option<&Identity>,
        keeps_atime: bool,
    ) -> io::Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::OpenOptionsExt;

        let no_follow = if expected.is_some() {
            libc::O_NOFOLLOW
        } else {
            0
        };
        let open = |flags| {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | no_follow | flags)
                .open(path)
        };
        #[cfg(target_os = "linux")]
        let handle = if keeps_atime {
            match open(libc::O_NOATIME) {
                Err(err) if err.raw_os_error() == Some(libc::EPERM) => open(0),
                opened => opened,
            }
        } else {
            open(0)
        }?;
        #[cfg(not(target_os = "linux"))]
        let handle = {
            let _ = keeps_atime;
            open(0)?
        };
        let metadata = handle.metadata()?;
        if expected.is_some_and(|expected| *expected != (metadata.dev(), metadata.ino())) {
            return Ok(None);
        }
        Ok(Some(Self {
            stream: Stream::of(handle)?,
            atime: keeps_atime.then(|| (metadata.atime(), metadata.atime_nsec())),
        }))
    }

    /// Opens the directory at `path`, held so that neither it nor a directory above it is renamed
    /// or removed while it is open, without following a link or junction at its last component,
    /// unless `expected` is `None`, as for the walk's root, which may be reached through one.
    /// `None` where a directory with an identity other than `expected` is there, or something
    /// other than a directory is. Its listing opens once it is checked.
    #[cfg(target_family = "windows")]
    pub fn open(
        path: &Path,
        expected: Option<&Identity>,
        _keeps_atime: bool,
    ) -> io::Result<Option<Self>> {
        let Some((handle, identity)) = super::entry::open_directory_held(path, expected.is_none())?
        else {
            return Ok(None);
        };
        if expected.is_some_and(|expected| *expected != identity) {
            return Ok(None);
        }
        Ok(Some(Self {
            handle,
            listing: Listing::open(path)?,
        }))
    }

    /// The entry `name` of this directory, whose path is `path`.
    #[cfg(target_family = "unix")]
    #[inline]
    pub fn entry<'a>(&'a self, path: &'a Path, name: &'a OsStr) -> Source<'a> {
        Source {
            path,
            parent: self.stream.fd(),
            name,
        }
    }

    /// The entry of this directory at `path`.
    #[cfg(target_family = "windows")]
    #[allow(clippy::unused_self)]
    #[inline]
    pub fn entry<'a>(&'a self, path: &'a Path, _name: &'a OsStr) -> Source<'a> {
        Source { path }
    }

    /// The directory's descriptor.
    #[cfg(target_family = "unix")]
    #[inline]
    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.stream.fd()
    }

    /// The access time the directory had as it was opened, where the listing keeps it.
    #[cfg(target_family = "unix")]
    pub(super) fn atime(&self) -> Option<(i64, i64)> {
        self.atime
    }
}

impl Iterator for OpenDirectory {
    type Item = io::Result<Listed>;

    /// The next entry the directory lists: `.` and `..` are left out, and a kind the listing does
    /// not give is read with `fstatat`. `None` at the end, and after a failure.
    #[cfg(target_family = "unix")]
    fn next(&mut self) -> Option<io::Result<Listed>> {
        Some(
            self.stream
                .next()?
                .map(|(name, kind)| Listed { name, kind }),
        )
    }

    /// The next entry the directory lists, `None` at the end.
    #[cfg(target_family = "windows")]
    fn next(&mut self) -> Option<io::Result<Listed>> {
        self.listing.next()
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
    fn open(path: &Path) -> io::Result<Self> {
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
    fn next(&mut self) -> Option<io::Result<Listed>> {
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

/// A stream from `fdopendir` of the names a directory lists, which owns the directory's
/// descriptor and closes it when dropped. It moves to another thread with the position it has
/// read to, without the directory being opened again.
#[cfg(target_family = "unix")]
struct Stream {
    stream: std::ptr::NonNull<libc::DIR>,
    /// Set once `readdir` failed, as it may fail the same way again.
    failed: bool,
}

// SAFETY: a directory stream is not tied to the thread that opened it, and `Stream` reads it only
// through `&mut self`, so one thread reads it at a time.
#[cfg(target_family = "unix")]
unsafe impl Send for Stream {}

#[cfg(target_family = "unix")]
impl Stream {
    /// The stream of the directory `handle` is open on, which takes the handle's descriptor.
    fn of(handle: std::fs::File) -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        use std::os::fd::IntoRawFd;

        let descriptor = handle.into_raw_fd();
        // SAFETY: the descriptor is open and owned here; `fdopendir` takes it over on success.
        let stream = unsafe { libc::fdopendir(descriptor) };
        let Some(stream) = std::ptr::NonNull::new(stream) else {
            let err = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed, so the descriptor is still owned here, and is closed once.
            drop(unsafe { std::fs::File::from_raw_fd(descriptor) });
            return Err(err);
        };
        Ok(Stream {
            stream,
            failed: false,
        })
    }

    /// The directory's descriptor, which the stream owns.
    #[inline]
    fn fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the stream is open, and its descriptor stays open for as long as it is.
        unsafe { BorrowedFd::borrow_raw(libc::dirfd(self.stream.as_ptr())) }
    }

    /// [`OpenDirectory::next`].
    fn next(&mut self) -> Option<io::Result<(OsString, Kind)>> {
        loop {
            if self.failed {
                return None;
            }
            set_errno_zero();
            // SAFETY: the stream is open until `Stream` is dropped.
            let found = unsafe { libc::readdir(self.stream.as_ptr()) };
            if found.is_null() {
                let err = io::Error::last_os_error();
                self.failed = true;
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
                libc::DT_DIR => Kind::Directory,
                libc::DT_REG => Kind::File,
                libc::DT_LNK => Kind::Symlink,
                libc::DT_UNKNOWN => match (Source {
                    path: Path::new(""),
                    parent: self.fd(),
                    name,
                })
                .stat()
                {
                    Ok(stat) => kind_of_mode(mode_bits(stat.st_mode)),
                    Err(err) => return Some(Err(err)),
                },
                _ => Kind::Other,
            };
            return Some(Ok((name.to_os_string(), kind)));
        }
    }
}

/// The kind of entry a mode from `stat` names.
#[cfg(target_family = "unix")]
fn kind_of_mode(mode: u32) -> Kind {
    let kind = mode & mode_bits(libc::S_IFMT);
    if kind == mode_bits(libc::S_IFDIR) {
        Kind::Directory
    } else if kind == mode_bits(libc::S_IFREG) {
        Kind::File
    } else if kind == mode_bits(libc::S_IFLNK) {
        Kind::Symlink
    } else {
        Kind::Other
    }
}

#[cfg(target_family = "unix")]
impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream is open and closed only here.
        unsafe { libc::closedir(self.stream.as_ptr()) };
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
