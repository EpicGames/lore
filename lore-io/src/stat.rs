// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The kind, size, modification time and permission bits of what a path names.

use std::time::SystemTime;

/// What a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatKind {
    File,
    Directory,
    /// Anything else a filesystem holds: a device, a socket, a pipe, and a symbolic link that
    /// [`IoDriver::symlink_stat`](crate::IoDriver::symlink_stat) describes on Unix.
    Other,
}

/// The kind, size, modification time and permission bits of what a path names.
///
/// [`IoDriver::stat`](crate::IoDriver::stat) answers it with symbolic links followed, and
/// [`IoDriver::symlink_stat`](crate::IoDriver::symlink_stat) for the name itself. Each field is
/// what [`std::fs::Metadata`] holds for it, which `From` takes it from, so the two agree on any
/// path both describe, but for the kind `symlink_stat` gives a symbolic link on Windows, which it
/// takes from the link's file attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: StatKind,
    /// The length in bytes.
    pub len: u64,
    /// The time of the last modification, `None` where the platform keeps none.
    pub modified: Option<SystemTime>,
    /// The permission bits with the file type bits, as `st_mode` holds them, `None` on a
    /// platform whose files carry none.
    pub mode: Option<u32>,
}

impl From<&std::fs::Metadata> for Stat {
    fn from(metadata: &std::fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            StatKind::Directory
        } else if file_type.is_file() {
            StatKind::File
        } else {
            StatKind::Other
        };
        Stat {
            kind,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            mode: mode(metadata),
        }
    }
}

#[cfg(target_family = "unix")]
fn mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode())
}

#[cfg(not(target_family = "unix"))]
fn mode(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

/// What `GetFileAttributesExW` reports for a path: the name itself, a reparse point described
/// rather than followed. `None` where `SystemTime` cannot hold the time of the last write.
#[cfg(target_family = "windows")]
pub(crate) fn from_attribute_data(
    data: &windows_sys::Win32::Storage::FileSystem::WIN32_FILE_ATTRIBUTE_DATA,
) -> Option<Stat> {
    from_attributes(
        data.dwFileAttributes,
        data.nFileSizeHigh,
        data.nFileSizeLow,
        data.ftLastWriteTime,
    )
}

/// The `Stat` for Win32 file `attributes`, a size split into its high and low halves, and the
/// time of the last write, or `None` where `SystemTime` cannot hold that time. A directory
/// search's record carries these for the name it lists, as `GetFileAttributesExW` does for a
/// path.
#[cfg(target_family = "windows")]
pub(crate) fn from_attributes(
    attributes: u32,
    size_high: u32,
    size_low: u32,
    written: windows_sys::Win32::Foundation::FILETIME,
) -> Option<Stat> {
    Some(Stat {
        kind: attribute_kind(attributes),
        len: (u64::from(size_high) << 32) | u64::from(size_low),
        modified: Some(system_time(
            (u64::from(written.dwHighDateTime) << 32) | u64::from(written.dwLowDateTime),
        )?),
        mode: None,
    })
}

/// The kind Win32 file `attributes` give: a directory where they carry the directory bit, which a
/// symbolic link or junction made to a directory carries too, and a file otherwise.
#[cfg(target_family = "windows")]
pub(crate) fn attribute_kind(attributes: u32) -> StatKind {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        StatKind::Directory
    } else {
        StatKind::File
    }
}

/// The `SystemTime` `std` gives for a file time of `intervals`, which counts 100-nanosecond
/// intervals since 1601, or `None` where `SystemTime` cannot hold it.
#[lore_macro::test_pub]
#[cfg(target_family = "windows")]
fn system_time(intervals: u64) -> Option<SystemTime> {
    const INTERVALS_BEFORE_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
    const INTERVALS_PER_SECOND: u64 = 10_000_000;

    let span = |intervals: u64| {
        std::time::Duration::new(
            intervals / INTERVALS_PER_SECOND,
            (intervals % INTERVALS_PER_SECOND) as u32 * 100,
        )
    };
    if intervals >= INTERVALS_BEFORE_UNIX_EPOCH {
        SystemTime::UNIX_EPOCH.checked_add(span(intervals - INTERVALS_BEFORE_UNIX_EPOCH))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(span(INTERVALS_BEFORE_UNIX_EPOCH - intervals))
    }
}
