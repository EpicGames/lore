// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The APFS backend: `clonefile(2)`, which clones a file or a whole directory with its metadata,
//! sharing every data block, and fails rather than copies when it cannot. The kernel clones a
//! file's data and its times as one, so a file rewritten during the clone keeps a time that
//! matches its content.

use std::io;
use std::path::Path;

use super::source::Source;

/// Clones the directory `source` names, as a whole, to `destination`, which must not exist,
/// reading it through the directory that holds it. A link found at `source` is cloned as the link,
/// and a file as the file. [`io::ErrorKind::Unsupported`] where the volume cannot clone or
/// `destination` is on another volume.
#[cfg(target_os = "macos")]
pub(super) fn clone_directory(source: Source<'_>, destination: &Path) -> io::Result<()> {
    /// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`, which libc does not define: a link at `source`
    /// is cloned as the link.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    let name = source.name_for_call()?;
    let destination = destination_for_call(destination)?;
    // SAFETY: the source's directory is open for the borrow, and the name and the destination
    // are NUL-terminated for the duration of the call.
    let cloned = unsafe {
        libc::clonefileat(
            source.parent_fd(),
            name.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            CLONE_NOFOLLOW,
        )
    };
    answer(cloned)
}

/// Clones the file `source` names to `destination`, which must not exist, from the file opened
/// through the directory that holds it, so a directory put in its place is never cloned whole. A
/// link found at `source` is copied as a link. [`io::ErrorKind::Unsupported`] where the volume
/// cannot clone or `destination` is on another volume.
#[cfg(target_os = "macos")]
pub(super) fn clone_file(source: Source<'_>, destination: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    use super::entry::OpenedSource;

    let file = match super::entry::open_source(source)? {
        OpenedSource::File(file) => file,
        OpenedSource::Link => return super::entry::link(source, destination),
    };
    let destination = destination_for_call(destination)?;
    // SAFETY: the file is open for the call, and the destination is NUL-terminated.
    let cloned =
        unsafe { libc::fclonefileat(file.as_raw_fd(), libc::AT_FDCWD, destination.as_ptr(), 0) };
    answer(cloned)
}

/// `destination` as a NUL-terminated path for a call.
#[cfg(target_os = "macos")]
fn destination_for_call(destination: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    Ok(std::ffi::CString::new(destination.as_os_str().as_bytes())?)
}

/// What a clone call that returned `cloned` answers: unsupported where the volume cannot clone or
/// the destination is on another volume.
#[cfg(target_os = "macos")]
fn answer(cloned: libc::c_int) -> io::Result<()> {
    if cloned == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => Err(super::entry::unsupported("clonefile")),
        _ => Err(err),
    }
}

/// `clonefile(2)` exists on macOS only.
#[cfg(not(target_os = "macos"))]
pub(super) fn clone_directory(_source: Source<'_>, _destination: &Path) -> io::Result<()> {
    Err(super::entry::unsupported("clonefile"))
}

/// `clonefile(2)` exists on macOS only.
#[cfg(not(target_os = "macos"))]
pub(super) fn clone_file(_source: Source<'_>, _destination: &Path) -> io::Result<()> {
    Err(super::entry::unsupported("clonefile"))
}
