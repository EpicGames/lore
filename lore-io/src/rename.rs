// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A rename that never replaces what is at its target, and a replacing rename that returns once
//! it is on disk.

use std::io;
use std::path::Path;

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] where anything is at
/// `to`, rather than replacing it as a plain rename replaces an empty directory. With `durable`,
/// it returns once the rename is on disk: it flushes both parents on Unix, through
/// [`crate::sync_dir`], and writes through on Windows. Blocking.
pub fn rename_no_replace(from: &Path, to: &Path, durable: bool) -> io::Result<()> {
    #[cfg(target_family = "windows")]
    {
        use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
        move_file(from, to, if durable { MOVEFILE_WRITE_THROUGH } else { 0 })
    }
    #[cfg(not(target_family = "windows"))]
    {
        rename_exclusive(from, to)?;
        if durable {
            sync_parents(from, to)?;
        }
        Ok(())
    }
}

/// Renames `from` over `to`, replacing a file there, or on Unix an empty directory, and returns
/// once the rename is on disk: it flushes both parents on Unix, through [`crate::sync_dir`], and
/// passes `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` to `MoveFileExW` on Windows.
/// Blocking.
pub fn rename_durable(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_family = "windows")]
    {
        use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
        use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
        move_file(from, to, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
    }
    #[cfg(not(target_family = "windows"))]
    {
        std::fs::rename(from, to)?;
        sync_parents(from, to)
    }
}

/// Flushes the directory that holds `to`, and the one that held `from` where it differs.
#[cfg(not(target_family = "windows"))]
fn sync_parents(from: &Path, to: &Path) -> io::Result<()> {
    crate::psync::sync_dir_of(to)?;
    if from.parent() != to.parent() {
        crate::psync::sync_dir_of(from)?;
    }
    Ok(())
}

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] where anything is at
/// `to`.
#[cfg(target_os = "linux")]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
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
/// `to`.
#[cfg(target_os = "macos")]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
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

/// Renames `from` to `to` with `MoveFileExW` and `flags`, which without
/// `MOVEFILE_REPLACE_EXISTING` fail it with [`io::ErrorKind::AlreadyExists`] where anything is at
/// `to`.
#[cfg(target_family = "windows")]
fn move_file(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    use crate::win_path::to_extended_wide;

    let source = to_extended_wide(&std::path::absolute(from)?);
    let target = to_extended_wide(&std::path::absolute(to)?);
    // SAFETY: both arguments are NUL-terminated wide paths for the duration of the call.
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            flags,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Renames `from` to `to` where nothing is at `to`, on a system with no rename that refuses to
/// replace: what appears at `to` between the check and the rename can still be replaced.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_family = "windows")))]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
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
