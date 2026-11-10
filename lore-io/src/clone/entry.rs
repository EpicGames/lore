// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! What the backends share: opening a source file without following a link, giving a copy the
//! permissions and modification time the source's handle reports, copying links, and making the
//! directories a clone fills entry by entry.

use std::fs::File;
use std::io;
use std::path::Path;

use super::source::Source;
#[cfg(target_family = "unix")]
use super::source::is_gone_kind;

/// The permission bits of a mode, without its file type.
#[cfg(target_family = "unix")]
pub(super) const PERMISSION_BITS: u32 = 0o7777;

/// The owner's write and search bits, which adding an entry to a directory takes.
#[cfg(target_family = "unix")]
pub(super) const OWNER_FILLS: u32 = 0o300;

/// The set-group-ID bit of a mode.
#[cfg(target_family = "unix")]
pub(super) const SETGID: u32 = 0o2000;

/// The permission bits `mkdir` gives a directory it creates, less those the file mode creation
/// mask clears.
#[cfg(target_family = "unix")]
pub(super) const CREATED_BITS: u32 = 0o777;

/// The error a block-cloning backend reports where it cannot clone, never copying in its place.
pub(super) fn unsupported(call: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("the filesystem cannot clone with {call}"),
    )
}

/// What an entry held when it was opened.
pub(super) enum OpenedSource {
    /// A file, open for reading.
    File(File),
    /// A symbolic link, or on Windows a junction, as when a link replaced the file after it was
    /// listed. It is copied as a link.
    Link,
}

/// Opens the file `source` names for reading, through the directory that holds it, without
/// following a link and without waiting on a pipe. A pipe, socket or device put in place of the
/// file after it was listed answers [`io::ErrorKind::NotFound`], which the walk takes for the file
/// being gone; a directory put in its place fails the clone with
/// [`io::ErrorKind::IsADirectory`].
#[cfg(target_family = "unix")]
pub(super) fn open_source(source: Source<'_>) -> io::Result<OpenedSource> {
    use std::os::fd::FromRawFd;

    let name = source.name_for_call()?;
    // SAFETY: the directory is open for the borrow and the name is NUL-terminated.
    let opened = unsafe {
        libc::openat(
            source.parent_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if opened < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            return Ok(OpenedSource::Link);
        }
        // A socket fails the open, where a device opens and is told by its type below; either put
        // in place of the file counts as gone.
        return if source.is_gone() && !is_gone_kind(err.kind()) {
            Err(io::Error::new(io::ErrorKind::NotFound, "no longer a file"))
        } else {
            Err(err)
        };
    }
    // SAFETY: `openat` returned a descriptor owned here.
    let file = unsafe { File::from_raw_fd(opened) };
    let file_type = file.metadata()?.file_type();
    if file_type.is_file() {
        Ok(OpenedSource::File(file))
    } else if file_type.is_dir() {
        Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "a directory now",
        ))
    } else {
        Err(io::Error::new(io::ErrorKind::NotFound, "no longer a file"))
    }
}

/// Opens the file `source` names for reading, by its path, without following a symbolic link or
/// a junction. Any other reparse point, such as a deduplicated file's, is opened as the file it
/// presents.
#[cfg(target_family = "windows")]
pub(super) fn open_source(source: Source<'_>) -> io::Result<OpenedSource> {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let path: &Path = &source;
    // Opened with backup semantics too, so a directory or a directory link put in place of the
    // file opens, and is told apart, rather than failing the open.
    let open_entry = || {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    };
    let file = open_entry()?;
    let tag = windows::reparse_tag(&file)?;
    if tag.is_some_and(|tag| tag & windows::NAME_SURROGATE != 0) {
        return Ok(OpenedSource::Link);
    }
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "a directory now",
        ));
    }
    if tag.is_none() {
        return Ok(OpenedSource::File(file));
    }
    // Opened again by path to read the file the reparse point presents, and kept only where that
    // is the entry first opened, not one a link swapped in meanwhile leads to.
    let presented = File::open(path)?;
    if windows::same_file(&presented, &file)? {
        return Ok(OpenedSource::File(presented));
    }
    // Replaced between the two opens: a link put there is copied as a link, as one found by the
    // first open is.
    drop(presented);
    match windows::reparse_tag(&open_entry()?)? {
        Some(tag) if tag & windows::NAME_SURROGATE != 0 => Ok(OpenedSource::Link),
        _ => Err(io::Error::other("replaced while it was opened")),
    }
}

/// Gives `destination`, open for writing, the permissions and the access and modification
/// times `source`'s handle reports, read after the data is in place so that a source rewritten
/// meanwhile gives a time no older than the data.
#[cfg(target_family = "unix")]
pub(super) fn finish_file(source: &File, destination: &File) -> io::Result<()> {
    let metadata = source.metadata()?;
    destination.set_permissions(metadata.permissions())?;
    destination.set_times(
        std::fs::FileTimes::new()
            .set_accessed(metadata.accessed()?)
            .set_modified(metadata.modified()?),
    )
}

/// Gives `destination`, open to write its attributes, the times and the read-only attribute
/// `source`'s handle reports, read after the data is in place so that a source rewritten
/// meanwhile gives a time no older than the data. The times go first, since a read-only file
/// takes no further change.
#[cfg(target_family = "windows")]
pub(super) fn finish_file(source: &File, destination: &File) -> io::Result<()> {
    let metadata = source.metadata()?;
    destination.set_times(
        std::fs::FileTimes::new()
            .set_accessed(metadata.accessed()?)
            .set_modified(metadata.modified()?),
    )?;
    destination.set_permissions(metadata.permissions())
}

/// Copies the symbolic link `source` names to `destination` as a link, reading it through the
/// directory that holds it.
#[cfg(target_family = "unix")]
pub(super) fn link(source: Source<'_>, destination: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let name = source.name_for_call()?;
    let mut target = vec![0u8; 256];
    loop {
        // SAFETY: the directory is open for the borrow, the name is NUL-terminated, and
        // `target` is writable for its length.
        let read = unsafe {
            libc::readlinkat(
                source.parent_fd(),
                name.as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        let Ok(read) = usize::try_from(read) else {
            return Err(io::Error::last_os_error());
        };
        if read < target.len() {
            target.truncate(read);
            break;
        }
        target.resize(target.len() * 2, 0);
    }
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), destination)
}

/// Copies the symbolic link or junction at `source` to `destination` as it is. A symbolic link is
/// made with `CreateSymbolicLinkW`, which Developer Mode lets an unelevated user call, and keeps
/// its target and its file or directory kind. Any other name-surrogate reparse point, a junction
/// among them, has its data read and set on a new entry of the same kind, so a junction stays a
/// junction, which takes no privilege to create.
#[cfg(target_family = "windows")]
pub(super) fn link(source: Source<'_>, destination: &Path) -> io::Result<()> {
    windows::copy_reparse_point(&source, destination)
}

/// Whether `path` still names the file `file` was opened on.
#[cfg(target_family = "windows")]
pub(super) fn names_file(path: &Path, file: &File) -> io::Result<bool> {
    windows::names_file(path, file)
}

/// What tells one directory from another on its volume, so a directory replaced after it was
/// found is told from the one found: the device and inode numbers.
#[cfg(target_family = "unix")]
pub(super) type Identity = (u64, u64);

/// What tells one directory from another on its volume, so a directory replaced after it was
/// found is told from the one found: the volume serial number and the file ID.
#[cfg(target_family = "windows")]
pub(super) type Identity = (u64, [u8; 16]);

/// The identity of the directory at `path`, or `None` where something else is there, such as a
/// link or a junction. Every component of `path` but the last is resolved, so a directory reached
/// through a link swapped in above it answers with the identity of the directory the link leads
/// to.
#[cfg(target_family = "windows")]
pub(super) fn directory_identity(path: &Path) -> io::Result<Option<Identity>> {
    windows::directory_identity(path)
}

/// Opens the directory at `path` to list it, held open without sharing delete access, so that
/// while the handle is open no process renames or removes the directory, and Windows renames no
/// directory above it either, as it refuses to for a directory with an entry open below it. A link
/// or junction at `path` is followed where `follow` is set, as for the walk's root. The handle and
/// the directory's identity, or `None` where something other than a directory is there, a link
/// or junction not followed among them.
#[cfg(target_family = "windows")]
pub(super) fn open_directory_held(
    path: &Path,
    follow: bool,
) -> io::Result<Option<(File, Identity)>> {
    windows::open_directory_held(path, follow)
}

/// The volume serial number and file ID of the entry `file` was opened on, as [`windows::file_id`]
/// reads them.
#[cfg(target_family = "windows")]
pub(super) fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    windows::file_id(file)
}

/// Creates the directory `path` with the permission bits `mode`, less those the file mode creation
/// mask clears: true where it was created, false where a directory is already there, and
/// [`io::ErrorKind::AlreadyExists`] where something else is.
#[cfg(target_family = "unix")]
pub(super) fn create_directory(path: &Path, mode: u32) -> io::Result<bool> {
    use std::os::unix::fs::DirBuilderExt;

    forgive_present_directory(std::fs::DirBuilder::new().mode(mode).create(path), path)
}

/// Creates the directory `path`: true where it was created, false where a directory is already
/// there, and [`io::ErrorKind::AlreadyExists`] where something else is.
#[cfg(target_family = "windows")]
pub(super) fn create_directory(path: &Path) -> io::Result<bool> {
    forgive_present_directory(std::fs::create_dir(path), path)
}

/// What creating the directory `path` answered, `created`, as [`create_directory`] answers it.
fn forgive_present_directory(created: io::Result<()>, path: &Path) -> io::Result<bool> {
    match created {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(path)?.is_dir() {
                Ok(false)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// Removes what a whole-directory clone that failed left at `path`.
pub(super) fn remove_partial(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(left) if left.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// The permission bits the file mode creation mask clears from a directory created in the
/// directory `parent`, read from a directory created there with every permission bit and removed
/// again, as the mask cannot be read without changing it for every thread of the process.
#[cfg(target_family = "unix")]
pub(super) fn umask_in(parent: &Path) -> io::Result<u32> {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static PROBES: AtomicU64 = AtomicU64::new(0);
    let probe = parent.join(format!(
        "{}umask-{}-{}",
        super::PROBE_PREFIX,
        std::process::id(),
        PROBES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new()
        .mode(CREATED_BITS)
        .create(&probe)?;
    let mode = mode_of(&probe);
    let _ = std::fs::remove_dir(&probe);
    Ok(!mode? & CREATED_BITS)
}

/// The permission bits of the directory at `path`.
#[cfg(target_family = "unix")]
pub(super) fn mode_of(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;

    Ok(std::fs::metadata(path)?.permissions().mode() & PERMISSION_BITS)
}

/// Sets the permission bits of `path` to `mode`.
#[cfg(target_family = "unix")]
pub(super) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(target_family = "windows")]
mod windows {
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION;
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
    use windows_sys::Win32::Storage::FileSystem::MAXIMUM_REPARSE_DATA_BUFFER_SIZE;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_SYMLINK;

    /// The tag bit that marks a reparse point standing for another name, as a symbolic link and a
    /// junction do. windows-sys has no constant for it.
    pub(super) const NAME_SURROGATE: u32 = 0x2000_0000;

    /// The reparse tag of the entry `file` was opened on, `None` where it is no reparse point.
    pub(super) fn reparse_tag(file: &File) -> io::Result<Option<u32>> {
        let mut info = FILE_ATTRIBUTE_TAG_INFO {
            FileAttributes: 0,
            ReparseTag: 0,
        };
        // SAFETY: the handle is open, and `info` is writable for the size given.
        let done = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileAttributeTagInfo,
                (&raw mut info).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        };
        if done == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag))
    }

    /// Whether `path` still names the file `file` was opened on, by volume and file ID.
    pub(super) fn names_file(path: &Path, file: &File) -> io::Result<bool> {
        let named = open_entry(path, FILE_READ_ATTRIBUTES)?;
        Ok(file_id(&named)? == file_id(file)?)
    }

    /// Whether `one` and `other` are open on the same file.
    pub(super) fn same_file(one: &File, other: &File) -> io::Result<bool> {
        Ok(file_id(one)? == file_id(other)?)
    }

    /// The volume serial number and file ID of the entry `file` was opened on, from
    /// `FILE_ID_INFO`, or where the volume does not answer for it, as FAT and exFAT volumes may
    /// not, from the 32-bit serial number and 64-bit file index the handle's basic information
    /// holds.
    pub(super) fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
        // SAFETY: zeroed is a valid FILE_ID_INFO, which the call fills.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open, and `info` is writable for the size given.
        let done = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                (&raw mut info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if done != 0 {
            return Ok((info.VolumeSerialNumber, info.FileId.Identifier));
        }
        let failure = io::Error::last_os_error();
        let unanswered = [
            ERROR_INVALID_PARAMETER,
            ERROR_INVALID_FUNCTION,
            ERROR_NOT_SUPPORTED,
        ]
        .iter()
        .any(|&code| failure.raw_os_error() == Some(code as i32));
        if !unanswered {
            return Err(failure);
        }
        // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION, which the call fills.
        let mut basic: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open, and `basic` is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &raw mut basic) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut id = [0u8; 16];
        id[..4].copy_from_slice(&basic.nFileIndexLow.to_le_bytes());
        id[4..8].copy_from_slice(&basic.nFileIndexHigh.to_le_bytes());
        Ok((u64::from(basic.dwVolumeSerialNumber), id))
    }

    /// [`super::directory_identity`].
    pub(super) fn directory_identity(path: &Path) -> io::Result<Option<super::Identity>> {
        let entry = open_entry(path, FILE_READ_ATTRIBUTES)?;
        let is_directory = entry.metadata()?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0;
        if !is_directory || reparse_tag(&entry)?.is_some_and(|tag| tag & NAME_SURROGATE != 0) {
            return Ok(None);
        }
        file_id(&entry).map(Some)
    }

    /// The first pause before a held open refused for sharing is tried again, doubled before each
    /// later try up to [`HELD_OPEN_LAST_PAUSE`]: about 2.5 s in all, for another process that holds
    /// the directory with delete access, as one renaming or removing it briefly does.
    const HELD_OPEN_FIRST_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

    /// The longest pause before a held open is tried again.
    const HELD_OPEN_LAST_PAUSE: std::time::Duration = std::time::Duration::from_millis(1280);

    /// [`super::open_directory_held`], trying again for about 2.5 s where another process holds the
    /// directory open with delete access, which refuses the open for sharing.
    pub(super) fn open_directory_held(
        path: &Path,
        follow: bool,
    ) -> io::Result<Option<(File, super::Identity)>> {
        let reparse = if follow {
            0
        } else {
            FILE_FLAG_OPEN_REPARSE_POINT
        };
        // Read access, which a handle needs for its share mode to keep other opens out.
        let open = || {
            std::fs::OpenOptions::new()
                .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | reparse)
                .open(path)
        };
        let mut pause = HELD_OPEN_FIRST_PAUSE;
        let directory = loop {
            match open() {
                Err(err)
                    if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
                        && pause <= HELD_OPEN_LAST_PAUSE =>
                {
                    std::thread::sleep(pause);
                    pause *= 2;
                }
                opened => break opened?,
            }
        };
        let is_directory = directory.metadata()?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0;
        if !is_directory
            || (!follow && reparse_tag(&directory)?.is_some_and(|tag| tag & NAME_SURROGATE != 0))
        {
            return Ok(None);
        }
        let identity = file_id(&directory)?;
        Ok(Some((directory, identity)))
    }

    /// Opens the entry at `path` itself, a directory or a reparse point included, with `access`.
    fn open_entry(path: &Path, access: u32) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .access_mode(access)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    /// Copies the name-surrogate reparse point at `source`, a link or a junction, to a new file or
    /// directory at `destination`. Anything else at `source`, as where a file replaced the link
    /// after it was listed, fails with [`io::ErrorKind::InvalidInput`].
    pub(super) fn copy_reparse_point(source: &Path, destination: &Path) -> io::Result<()> {
        let from = open_entry(source, FILE_READ_ATTRIBUTES)?;
        let is_directory = from.metadata()?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0;
        let tag = reparse_tag(&from)?;
        if !tag.is_some_and(|tag| tag & NAME_SURROGATE != 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no longer a link",
            ));
        }
        if tag == Some(IO_REPARSE_TAG_SYMLINK) {
            drop(from);
            let target = std::fs::read_link(source)?;
            return if is_directory {
                std::os::windows::fs::symlink_dir(target, destination)
            } else {
                std::os::windows::fs::symlink_file(target, destination)
            };
        }
        let mut data = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
        let mut length = 0u32;
        // SAFETY: the handle is open, and `data` is writable for the size given.
        let read = unsafe {
            DeviceIoControl(
                from.as_raw_handle() as HANDLE,
                FSCTL_GET_REPARSE_POINT,
                std::ptr::null(),
                0,
                data.as_mut_ptr().cast(),
                MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
                &mut length,
                std::ptr::null_mut(),
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        drop(from);
        if is_directory {
            std::fs::create_dir(destination)?;
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
        }
        let set = open_entry(destination, GENERIC_WRITE).and_then(|to| {
            let mut returned = 0u32;
            // SAFETY: the handle is open, and `data` holds `length` bytes of reparse data.
            let done = unsafe {
                DeviceIoControl(
                    to.as_raw_handle() as HANDLE,
                    FSCTL_SET_REPARSE_POINT,
                    data.as_ptr().cast(),
                    length,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if done == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
        if set.is_err() {
            let _ = if is_directory {
                std::fs::remove_dir(destination)
            } else {
                std::fs::remove_file(destination)
            };
        }
        set
    }
}
