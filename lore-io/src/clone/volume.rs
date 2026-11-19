// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The volume a path lies on and its free space, which describe a clone between two paths and
//! decide whether a copy between them fits, and the locality of its file system.

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

/// What [`locality_of`] reads of the volume that holds a path, beside its [`Volume`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Locality {
    /// Whether the file system is one Lore has verified as local and as durable across a power
    /// loss: btrfs, xfs, bcachefs, the ext family or zfs by its `statfs` magic number on Linux,
    /// apfs by its `f_fstypename` on macOS, and on Windows a fixed or removable drive.
    pub local: bool,
    /// The mount the path lies on, as `statx` names it (`STATX_MNT_ID`) on Linux, which tells
    /// apart two mounts of one file system. `None` elsewhere, and where the kernel names none.
    pub mount_id: Option<u64>,
    /// The file system flags `GetVolumeInformationW` reports on Windows, and 0 elsewhere.
    pub flags: u32,
}

impl Locality {
    /// Whether the file system holds access control lists, alternate data streams and sparse
    /// files, by its flags.
    #[cfg(target_family = "windows")]
    pub fn holds_acls_streams_and_sparse_files(&self) -> bool {
        use windows_sys::Win32::System::SystemServices::FILE_NAMED_STREAMS;
        use windows_sys::Win32::System::SystemServices::FILE_PERSISTENT_ACLS;
        use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_SPARSE_FILES;

        let needed = FILE_PERSISTENT_ACLS | FILE_NAMED_STREAMS | FILE_SUPPORTS_SPARSE_FILES;
        self.flags & needed == needed
    }
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
    Ok(Volume {
        id,
        filesystem: text(&stat.f_fstypename),
        mount_point: PathBuf::from(text(&stat.f_mntonname)),
    })
}

/// The locality of the volume that holds `path`, which exists: whether its file system is apfs.
#[cfg(target_os = "macos")]
pub fn locality_of(path: &Path) -> io::Result<Locality> {
    Ok(Locality {
        local: name_is_local(&text(&statfs(path)?.f_fstypename)),
        mount_id: None,
        flags: 0,
    })
}

/// The text of a NUL-terminated field of a macOS `statfs`.
#[cfg(target_os = "macos")]
fn text(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c.cast_unsigned())
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Whether the macOS file system named `name`, as `f_fstypename` names it, is local: apfs. HFS+
/// keeps modification times to the second only.
#[cfg(target_os = "macos")]
#[lore_macro::test_pub]
fn name_is_local(name: &str) -> bool {
    name == "apfs"
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
    let filesystem = match statfs_magic(&path)? {
        BTRFS_MAGIC => "btrfs".to_string(),
        XFS_MAGIC => "xfs".to_string(),
        BCACHEFS_MAGIC => "bcachefs".to_string(),
        EXT_MAGIC => "ext2/ext3/ext4".to_string(),
        ZFS_MAGIC => "zfs".to_string(),
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

/// The locality of the volume that holds `path`, which exists: whether its file system is local
/// by its `statfs` magic number, and the mount it lies on.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
pub fn locality_of(path: &Path) -> io::Result<Locality> {
    Ok(Locality {
        local: magic_is_local(statfs_magic(path)?),
        mount_id: mount_id(path),
        flags: 0,
    })
}

/// The `statfs` magic number of the file system that holds `path`.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
fn statfs_magic(path: &Path) -> io::Result<u64> {
    #[allow(
        clippy::unnecessary_cast,
        reason = "the field's type differs between targets"
    )]
    Ok((statfs(path)?.f_type as u64) & 0xFFFF_FFFF)
}

/// The `statfs` magic number of btrfs.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
const BTRFS_MAGIC: u64 = 0x9123_683E;
/// The `statfs` magic number of xfs.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
const XFS_MAGIC: u64 = 0x5846_5342;
/// The `statfs` magic number of bcachefs.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
const BCACHEFS_MAGIC: u64 = 0xCA45_1A4E;
/// The `statfs` magic number of ext2, ext3 and ext4.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
const EXT_MAGIC: u64 = 0xEF53;
/// The `statfs` magic number of zfs.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
const ZFS_MAGIC: u64 = 0x2FC1_2FC1;

/// Whether the file system of the `statfs` magic number `magic` is local: btrfs, xfs,
/// bcachefs, the ext family or zfs. tmpfs is not, since what it holds is lost at a power loss.
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
#[lore_macro::test_pub]
fn magic_is_local(magic: u64) -> bool {
    matches!(
        magic,
        BTRFS_MAGIC | XFS_MAGIC | BCACHEFS_MAGIC | EXT_MAGIC | ZFS_MAGIC
    )
}

/// The ID of the mount that holds `path`, as `statx` names it, or `None` where the kernel names
/// none.
#[cfg(target_os = "linux")]
fn mount_id(path: &Path) -> Option<u64> {
    let path =
        std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).ok()?;
    // SAFETY: zeroed is a valid statx, which the call fills.
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a valid statx to fill.
    let done = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            0,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    };
    (done == 0 && stat.stx_mask & libc::STATX_MNT_ID != 0).then_some(stat.stx_mnt_id)
}

/// Other Unix systems name no mount.
#[cfg(all(
    target_family = "unix",
    not(any(target_os = "macos", target_os = "linux"))
))]
fn mount_id(_path: &Path) -> Option<u64> {
    None
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

/// The locality of the volume that holds `path`, which exists: whether its drive is local by
/// `GetDriveTypeW`, and the file system flags `GetVolumeInformationW` reports.
#[cfg(target_family = "windows")]
pub fn locality_of(path: &Path) -> io::Result<Locality> {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    let root = windows::volume_root(path)?;
    let mut flags = 0u32;
    // SAFETY: `root` is NUL-terminated, and the null buffers ask for nothing but the flags.
    let done = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `root` is NUL-terminated.
    let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
    Ok(Locality {
        local: drive_type_is_local(drive_type),
        mount_id: None,
        flags,
    })
}

/// Whether a drive of the type `GetDriveTypeW` answers `drive_type` with is local: a fixed or
/// a removable drive. A network drive, a mapped network share and a RAM disk are not.
#[cfg(target_family = "windows")]
#[lore_macro::test_pub]
fn drive_type_is_local(drive_type: u32) -> bool {
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;

    matches!(drive_type, DRIVE_FIXED | DRIVE_REMOVABLE)
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
