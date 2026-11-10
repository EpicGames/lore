// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The copy backend: a plain byte copy of each file, for every filesystem that cannot clone and
//! for a destination on another volume.

use std::io;
use std::path::Path;

use super::entry;
use super::entry::OpenedSource;
use super::source::Source;

/// Copies the data of the file `source` names to `destination`, which must not exist, from one open
/// handle to the other, and then gives the copy the permissions and times that handle reports. A
/// link found at `source` is copied as a link.
#[cfg(target_family = "unix")]
pub(super) fn file(source: Source<'_>, destination: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let input = match entry::open_source(source)? {
        OpenedSource::File(input) => input,
        OpenedSource::Link => return entry::link(source, destination),
    };
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    let copied = copy_data(&input, &mut output).and_then(|()| entry::finish_file(&input, &output));
    if copied.is_err() {
        drop(output);
        let _ = std::fs::remove_file(destination);
    }
    copied
}

/// `fcopyfile(3)`, which copies in the kernel's chunks rather than through a buffer here.
#[cfg(target_os = "macos")]
fn copy_data(input: &std::fs::File, output: &mut std::fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // SAFETY: both descriptors are open for the call, and a null state asks for none.
    let copied = unsafe {
        libc::fcopyfile(
            input.as_raw_fd(),
            output.as_raw_fd(),
            std::ptr::null_mut(),
            libc::COPYFILE_DATA,
        )
    };
    if copied == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `io::copy`, which on Linux copies in the kernel with `copy_file_range`.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
fn copy_data(mut input: &std::fs::File, output: &mut std::fs::File) -> io::Result<()> {
    io::copy(&mut input, output).map(drop)
}

/// Copies the file `source` names to `destination`, which must not exist, with `CopyFileExW`, and
/// then gives the copy the times and read-only attribute of a handle on the source held across the
/// copy. A link at `source` is copied as a link.
#[cfg(target_family = "windows")]
pub(super) fn file(source: Source<'_>, destination: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::COPY_FILE_COPY_SYMLINK;
    use windows_sys::Win32::Storage::FileSystem::COPY_FILE_FAIL_IF_EXISTS;
    use windows_sys::Win32::Storage::FileSystem::CopyFileExW;
    use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;

    // Held across the copy, and compared with what the path names after it, so the copy never
    // takes this file's times for a file renamed over the path meanwhile.
    let input = match entry::open_source(source)? {
        OpenedSource::File(input) => input,
        OpenedSource::Link => return entry::link(source, destination),
    };
    let from = crate::win_path::to_extended_wide(&std::path::absolute(&*source)?);
    let to = crate::win_path::to_extended_wide(&std::path::absolute(destination)?);
    // SAFETY: both paths are NUL-terminated, and the call takes no progress routine.
    let copied = unsafe {
        CopyFileExW(
            from.as_ptr(),
            to.as_ptr(),
            None,
            std::ptr::null(),
            std::ptr::null_mut(),
            COPY_FILE_FAIL_IF_EXISTS | COPY_FILE_COPY_SYMLINK,
        )
    };
    if copied == 0 {
        return Err(io::Error::last_os_error());
    }
    if !entry::names_file(&source, &input).unwrap_or(true) {
        remove_copy(destination);
        return copy_from_handle(&input, destination);
    }
    let finished = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .open(destination)
        .and_then(|output| entry::finish_file(&input, &output));
    if finished.is_err() {
        remove_copy(destination);
    }
    finished
}

/// Removes the copy at `path`, clearing the read-only attribute `CopyFileExW` may have copied to
/// it, which would keep it from being removed.
#[cfg(target_family = "windows")]
fn remove_copy(path: &Path) {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            // On Windows this clears the read-only attribute and grants nothing else.
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Copies the data of the open file `input` to `destination`, which must not exist, and gives the
/// copy the times and read-only attribute `input` reports.
#[cfg(target_family = "windows")]
fn copy_from_handle(input: &std::fs::File, destination: &Path) -> io::Result<()> {
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let copied = io::copy(&mut &*input, &mut output)
        .map(drop)
        .and_then(|()| entry::finish_file(input, &output));
    if copied.is_err() {
        drop(output);
        let _ = std::fs::remove_file(destination);
    }
    copied
}
