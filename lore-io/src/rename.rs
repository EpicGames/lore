// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A rename that never replaces what is at its target.

use std::io;
use std::path::Path;

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] where anything is at
/// `to`, rather than replacing it as a plain rename replaces an empty directory. Blocking.
#[cfg(target_os = "linux")]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let (source, target) = (
        std::ffi::CString::new(from.as_os_str().as_bytes())?,
        std::ffi::CString::new(to.as_os_str().as_bytes())?,
    );
    // SAFETY: both arguments are valid NUL-terminated paths for the duration of the call.
    let renamed = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if renamed == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // A filesystem that cannot refuse to replace is checked first instead.
        Some(libc::EINVAL | libc::ENOSYS) => rename_unless_present(from, to),
        _ => Err(err),
    }
}

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] where anything is at
/// `to`, rather than replacing it as a plain rename replaces an empty directory. Blocking.
#[cfg(target_os = "macos")]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let (source, target) = (
        std::ffi::CString::new(from.as_os_str().as_bytes())?,
        std::ffi::CString::new(to.as_os_str().as_bytes())?,
    );
    // SAFETY: both arguments are valid NUL-terminated paths for the duration of the call.
    if unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // A volume that cannot refuse to replace is checked first instead.
        Some(libc::ENOTSUP | libc::EINVAL) => rename_unless_present(from, to),
        _ => Err(err),
    }
}

/// Renames `from` to `to` with `MoveFileExW` and no flag to replace, so anything at `to` fails it
/// with [`io::ErrorKind::AlreadyExists`]. Blocking.
#[cfg(target_family = "windows")]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use crate::win_path::to_extended_wide;

    let source = to_extended_wide(&std::path::absolute(from)?);
    let target = to_extended_wide(&std::path::absolute(to)?);
    // SAFETY: both arguments are NUL-terminated wide paths for the duration of the call.
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(source.as_ptr(), target.as_ptr(), 0)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Renames `from` to `to` where nothing is at `to`, on a system with no rename that refuses to
/// replace: what appears at `to` between the check and the rename can still be replaced.
/// Blocking.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_family = "windows")))]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    rename_unless_present(from, to)
}

/// Renames `from` to `to` where nothing is at `to`, for a filesystem that cannot refuse to
/// replace: what appears at `to` between the check and the rename can still be replaced.
#[cfg(not(target_family = "windows"))]
fn rename_unless_present(from: &Path, to: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} appeared before the rename", to.display()),
        ));
    }
    std::fs::rename(from, to)
}
