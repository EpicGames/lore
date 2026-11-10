// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The volume a path lies on and its free space, which describe a clone between two paths and
//! decide whether a copy between them fits.

use std::io;
use std::path::Path;
use std::path::PathBuf;

/// A mounted volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// The identifier the system gives the volume: the device number on Unix, the volume serial
    /// number on Windows, 64 bits or, on a volume such as FAT, 32. It tells volumes apart for
    /// display only: a Btrfs subvolume has one of its own, and two copies of one virtual disk can
    /// share one.
    pub id: u64,
    /// The filesystem's name, such as `apfs`, `btrfs` or `NTFS`.
    pub filesystem: String,
    /// Where the volume is mounted.
    pub mount_point: PathBuf,
}

/// The free space on a volume, and the size of its blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    /// The bytes free for this user.
    pub available: u64,
    /// The bytes in each block a file takes: the fragment size on Unix, the cluster size on
    /// Windows.
    pub block: u64,
}

/// The volume that holds `path`, which exists.
#[cfg(target_os = "macos")]
pub fn volume_of(path: &Path) -> io::Result<Volume> {
    use std::os::unix::fs::MetadataExt;

    let id = std::fs::metadata(path)?.dev();
    let stat = statfs(path)?;
    let text = |chars: &[libc::c_char]| {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c.cast_unsigned())
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(Volume {
        id,
        filesystem: text(&stat.f_fstypename),
        mount_point: PathBuf::from(text(&stat.f_mntonname)),
    })
}

/// The volume that holds `path`, which exists. Its mount point is the outermost directory above
/// `path` on the same device.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
pub fn volume_of(path: &Path) -> io::Result<Volume> {
    use std::os::unix::fs::MetadataExt;

    let path = path.canonicalize()?;
    let id = std::fs::metadata(&path)?.dev();
    let mut mount_point = path.clone();
    for ancestor in path.ancestors().skip(1) {
        if std::fs::metadata(ancestor)?.dev() != id {
            break;
        }
        mount_point = ancestor.to_path_buf();
    }
    #[allow(
        clippy::unnecessary_cast,
        reason = "the field's type differs between targets"
    )]
    let magic = (statfs(&path)?.f_type as u64) & 0xFFFF_FFFF;
    let filesystem = match magic {
        0x9123_683E => "btrfs".to_string(),
        0x5846_5342 => "xfs".to_string(),
        0xCA45_1A4E => "bcachefs".to_string(),
        0xEF53 => "ext2/ext3/ext4".to_string(),
        0x2FC1_2FC1 => "zfs".to_string(),
        0x0102_1994 => "tmpfs".to_string(),
        0x794C_7630 => "overlayfs".to_string(),
        0x6969 => "nfs".to_string(),
        0xFF53_4D42 | 0xFE53_4D42 => "smb".to_string(),
        other => format!("filesystem {other:#x}"),
    };
    Ok(Volume {
        id,
        filesystem,
        mount_point,
    })
}

/// The volume that holds `path`, which exists.
#[cfg(target_family = "windows")]
pub fn volume_of(path: &Path) -> io::Result<Volume> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    let root = windows::volume_root(path)?;
    let mut filesystem = vec![0u16; 64];
    // SAFETY: `root` is NUL-terminated and every buffer holds the length given.
    let done = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Volume {
        id: windows::volume_serial(path)?,
        filesystem: windows::text(&filesystem),
        mount_point: PathBuf::from(windows::text(&root)),
    })
}

/// The free space on the volume that holds `path`, which exists.
#[cfg(target_family = "unix")]
pub fn space_of(path: &Path) -> io::Result<Space> {
    let path = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()))?;
    // SAFETY: zeroed is a valid statvfs, which the call fills.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a valid statvfs to fill.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(
        clippy::unnecessary_cast,
        reason = "the fields' types differ between targets"
    )]
    let (available, block) = (stat.f_bavail as u64, stat.f_frsize as u64);
    Ok(Space {
        available: available.saturating_mul(block),
        block,
    })
}

/// The free space on the volume that holds `path`, which exists.
#[cfg(target_family = "windows")]
pub fn space_of(path: &Path) -> io::Result<Space> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;

    let wide = windows::wide(path);
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated; the totals the call can skip are null.
    let done = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    let root = windows::volume_root(path)?;
    let (mut sectors, mut bytes) = (0u32, 0u32);
    // SAFETY: `root` is NUL-terminated; the cluster counts the call can skip are null.
    let done = unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &mut sectors,
            &mut bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Space {
        available,
        block: u64::from(sectors) * u64::from(bytes),
    })
}

#[cfg(target_family = "unix")]
fn statfs(path: &Path) -> io::Result<libc::statfs> {
    let path = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()))?;
    // SAFETY: zeroed is a valid statfs, which the call fills.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a valid statfs to fill.
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

#[cfg(target_family = "windows")]
mod windows {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

    pub(super) fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    pub(super) fn text(wide: &[u16]) -> String {
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        String::from_utf16_lossy(&wide[..end])
    }

    /// The root of the volume that holds `path`, NUL-terminated.
    pub(super) fn volume_root(path: &Path) -> io::Result<Vec<u16>> {
        let wide = wide(path);
        let mut root = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `root` holds the length given.
        if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(root)
    }

    /// The serial number of the volume that holds `path`, as `entry::file_id` reads it: 64 bits
    /// from `FILE_ID_INFO`, or 32 where the volume answers only for its basic information.
    pub(super) fn volume_serial(path: &Path) -> io::Result<u64> {
        let entry = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        super::super::entry::file_id(&entry).map(|(serial, _)| serial)
    }
}
