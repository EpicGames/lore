// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The kind, size, modification time and permission bits of what a path names.

use std::time::SystemTime;

/// What a path names, with links followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatKind {
    File,
    Directory,
    /// Anything else a filesystem holds: a device, a socket, a pipe.
    Other,
}

/// The kind, size, modification time and permission bits of what a path names, with links
/// followed.
///
/// [`IoDriver::stat`](crate::IoDriver::stat) answers it. Each field is what
/// [`std::fs::Metadata`] holds for it, which `From` takes it from, so the two agree on any path
/// both describe.
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

/// What `GetFileAttributesExW` reports for a path that names no reparse point, and so names
/// what it reports on rather than a link to it. `None` where `SystemTime` cannot hold the time
/// of the last write.
#[cfg(target_family = "windows")]
pub(crate) fn from_attribute_data(
    data: &windows_sys::Win32::Storage::FileSystem::WIN32_FILE_ATTRIBUTE_DATA,
) -> Option<Stat> {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

    let written = data.ftLastWriteTime;
    Some(Stat {
        kind: if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            StatKind::Directory
        } else {
            StatKind::File
        },
        len: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
        modified: Some(system_time(
            (u64::from(written.dwHighDateTime) << 32) | u64::from(written.dwLowDateTime),
        )?),
        mode: None,
    })
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
