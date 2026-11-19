// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Directory junctions: directories whose mount-point reparse point leads to another directory on
//! a local volume, which Windows lets a user make without a privilege.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Component;
use std::path::Path;
use std::path::Prefix;

use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::MAXIMUM_REPARSE_DATA_BUFFER_SIZE;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

/// The bytes of a mount-point reparse buffer before its path buffer: the tag, the data length and
/// a reserved word, then the offset and length of the substitute name and of the print name.
const HEADER_LEN: usize = 16;

/// The bytes of the header that the data length leaves out: the tag, the length and the reserved
/// word.
const TAG_HEADER_LEN: usize = 8;

/// Makes a junction at `junction` that leads to the directory `target`, an absolute path on a
/// drive. It fails with [`io::ErrorKind::AlreadyExists`] where anything is at `junction`, and with
/// [`io::ErrorKind::InvalidInput`] for any other `target`, and removes the directory it made at
/// `junction` when it fails. `std::fs::read_link` reads `target` back. Blocking.
pub fn create_junction(target: &Path, junction: &Path) -> io::Result<()> {
    let buffer = mount_point_buffer(target)?;
    std::fs::create_dir(junction)?;
    let set = set_reparse_point(junction, &buffer);
    if set.is_err() {
        let _ = std::fs::remove_dir(junction);
    }
    set
}

/// The mount-point reparse buffer that leads to `target`, whose substitute name is `target` in the
/// NT namespace, `\??\C:\...`, and whose print name is the same path, `C:\...`.
fn mount_point_buffer(target: &Path) -> io::Result<Vec<u8>> {
    let print = drive_path(target)?;
    let substitute: Vec<u16> = "\\??\\"
        .encode_utf16()
        .chain(print.iter().copied())
        .collect();
    let substitute_bytes = substitute.len() * 2;
    let print_bytes = print.len() * 2;
    let path_bytes = substitute_bytes + 2 + print_bytes + 2;
    let total = HEADER_LEN + path_bytes;
    if total > MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is too long for a junction", target.display()),
        ));
    }
    let mut buffer = Vec::with_capacity(total);
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&((total - TAG_HEADER_LEN) as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&(substitute_bytes as u16).to_le_bytes());
    buffer.extend_from_slice(&((substitute_bytes + 2) as u16).to_le_bytes());
    buffer.extend_from_slice(&(print_bytes as u16).to_le_bytes());
    for unit in substitute.iter().chain([&0]).chain(&print).chain([&0]) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(buffer)
}

/// `target` as wide characters, `C:\...`, where it is an absolute path on a drive, in plain or
/// verbatim form, with no `..` in it. A junction leads only to a local volume, which a drive
/// names, and its substitute name is never normalized.
fn drive_path(target: &Path) -> io::Result<Vec<u16>> {
    let mut components = target.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(not_on_a_drive(target)),
        },
        _ => return Err(not_on_a_drive(target)),
    };
    if components.next() != Some(Component::RootDir) {
        return Err(not_on_a_drive(target));
    }
    let mut path: Vec<u16> = vec![u16::from(drive), u16::from(b':')];
    for component in components {
        let Component::Normal(name) = component else {
            return Err(not_on_a_drive(target));
        };
        path.push(u16::from(b'\\'));
        path.extend(name.encode_wide());
    }
    if path.len() == 2 {
        path.push(u16::from(b'\\'));
    }
    Ok(path)
}

/// The refusal of a junction to `target`, which is no absolute path on a drive, or holds `..`.
fn not_on_a_drive(target: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("a junction can't lead to {}", target.display()),
    )
}

/// Sets the reparse point in `buffer` on the empty file or directory at `path`.
pub(crate) fn set_reparse_point(path: &Path, buffer: &[u8]) -> io::Result<()> {
    let entry = std::fs::OpenOptions::new()
        .access_mode(GENERIC_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut returned = 0u32;
    // SAFETY: the handle is open, and `buffer` holds `buffer.len()` bytes of reparse data.
    let done = unsafe {
        DeviceIoControl(
            entry.as_raw_handle() as HANDLE,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
