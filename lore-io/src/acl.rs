// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Whether a path's extended access control list, on macOS, lets anyone but its owner change it,
//! the copy of an extended ACL from one entry to another, and its clearing.
//!
//! `libc` binds none of the ACL calls, so they are declared here as `sys/acl.h` and
//! `membership.h` declare them.

use std::ffi::CString;
use std::ffi::c_int;
use std::ffi::c_void;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

type Acl = *mut c_void;
type AclEntry = *mut c_void;
type AclPermset = *mut c_void;

const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_EXTENDED_ALLOW: c_int = 1;
const ID_TYPE_UID: c_int = 0;

/// The permissions that change an object or what a directory holds: `ACL_WRITE_DATA`
/// (`ACL_ADD_FILE`), `ACL_DELETE`, `ACL_APPEND_DATA` (`ACL_ADD_SUBDIRECTORY`),
/// `ACL_DELETE_CHILD`, `ACL_WRITE_SECURITY` and `ACL_CHANGE_OWNER`.
const WRITE_PERMISSIONS: [c_int; 6] = [1 << 2, 1 << 4, 1 << 5, 1 << 6, 1 << 12, 1 << 13];

unsafe extern "C" {
    fn acl_get_file(path: *const libc::c_char, acl_type: c_int) -> Acl;
    fn acl_get_entry(acl: Acl, entry_id: c_int, entry: *mut AclEntry) -> c_int;
    fn acl_get_tag_type(entry: AclEntry, tag: *mut c_int) -> c_int;
    fn acl_get_permset(entry: AclEntry, permset: *mut AclPermset) -> c_int;
    fn acl_get_perm_np(permset: AclPermset, perm: c_int) -> c_int;
    fn acl_get_qualifier(entry: AclEntry) -> *mut c_void;
    fn acl_free(object: *mut c_void) -> c_int;
    fn acl_init(count: c_int) -> Acl;
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> Acl;
    fn acl_set_fd_np(fd: c_int, acl: Acl, acl_type: c_int) -> c_int;
    fn mbr_uuid_to_id(uuid: *const u8, id: *mut libc::id_t, id_type: *mut c_int) -> c_int;
}

/// Frees an object the ACL calls allocated when it goes out of scope.
struct Owned(*mut c_void);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the pointer came from an ACL call that allocates, and is freed once.
        unsafe { acl_free(self.0) };
    }
}

/// An entry whose extended ACL is read or set: an open file or directory, or a link at a path,
/// not followed.
#[derive(Clone, Copy)]
pub(crate) enum Target<'a> {
    Fd(c_int),
    Link(&'a std::ffi::CStr),
}

/// Gives `to` the extended ACL of `from`, where `from` has one, which a link that answers `ENOTSUP`
/// doesn't. A link's ACL is read and set through a descriptor of the link itself, as an open
/// file's is. A failure names the read or the set.
pub(crate) fn copy_extended(from: Target<'_>, to: Target<'_>) -> io::Result<()> {
    let reading = |err| crate::clone::failed_call(err, "reading the extended ACL");
    let (from_fd, _from_link) = descriptor(from).map_err(reading)?;
    // SAFETY: the descriptor is open for the call.
    let acl = unsafe { acl_get_fd_np(from_fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let err = io::Error::last_os_error();
        return match (err.raw_os_error(), from) {
            (Some(libc::ENOENT), _) | (Some(libc::ENOTSUP), Target::Link(_)) => Ok(()),
            _ => Err(reading(err)),
        };
    }
    let acl = Owned(acl);
    let setting = |err| crate::clone::failed_call(err, "setting the extended ACL");
    let (to_fd, _to_link) = descriptor(to).map_err(setting)?;
    // SAFETY: `acl` is a valid ACL, and the descriptor is open for the call.
    if unsafe { acl_set_fd_np(to_fd, acl.0, ACL_TYPE_EXTENDED) } == 0 {
        Ok(())
    } else {
        Err(setting(io::Error::last_os_error()))
    }
}

/// The descriptor the ACL calls take for `target`, with the one opened for a link, which closes
/// as it drops: a link is opened itself, with `O_SYMLINK`.
fn descriptor(target: Target<'_>) -> io::Result<(c_int, Option<std::os::fd::OwnedFd>)> {
    let path = match target {
        Target::Fd(fd) => return Ok((fd, None)),
        Target::Link(path) => path,
    };
    // SAFETY: the path is NUL-terminated for the call.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_SYMLINK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call returned a descriptor no one else owns.
    let owned = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) };
    Ok((fd, Some(owned)))
}

/// Clears the extended ACL of `fd`, an open file or directory. A file system that holds no ACLs
/// has none to clear.
pub(crate) fn clear_extended(fd: c_int) -> io::Result<()> {
    // SAFETY: `acl_init` has no preconditions.
    let acl = unsafe { acl_init(0) };
    if acl.is_null() {
        return Err(io::Error::last_os_error());
    }
    let acl = Owned(acl);
    // SAFETY: `acl` is a valid ACL, and the descriptor is open for the call.
    if unsafe { acl_set_fd_np(fd, acl.0, ACL_TYPE_EXTENDED) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTSUP) => Ok(()),
        _ => Err(err),
    }
}

/// Whether the extended ACL of `path`, which exists, has an entry that allows anyone other than
/// the user `owner` to change it: to write it, add or delete entries in it, delete it, or change
/// its permissions or owner. A path with no extended ACL has none. Follows a link at `path`.
pub fn extended_acl_lets_others_write(path: &Path, owner: u32) -> io::Result<bool> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: the path is NUL-terminated for the duration of the call.
    let acl = unsafe { acl_get_file(c_path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err(err),
        };
    }
    let acl = Owned(acl);
    let mut entry: AclEntry = std::ptr::null_mut();
    let mut entry_id = ACL_FIRST_ENTRY;
    // SAFETY: `acl` is a valid ACL, and `entry` receives an entry within it.
    while unsafe { acl_get_entry(acl.0, entry_id, &mut entry) } == 0 {
        entry_id = ACL_NEXT_ENTRY;
        // SAFETY: `entry` is an entry of `acl`, which outlives this iteration.
        if unsafe { entry_lets_another_write(entry, owner) }? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `entry` allows a write permission to anyone other than the user `owner`.
///
/// # Safety
///
/// `entry` must be an entry of an ACL that stays allocated for the call.
unsafe fn entry_lets_another_write(entry: AclEntry, owner: u32) -> io::Result<bool> {
    let mut tag = 0;
    // SAFETY: the caller guarantees `entry`, and `tag` is writable.
    if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if tag != ACL_EXTENDED_ALLOW {
        return Ok(false);
    }
    let mut permset: AclPermset = std::ptr::null_mut();
    // SAFETY: the caller guarantees `entry`, and `permset` is writable.
    if unsafe { acl_get_permset(entry, &mut permset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `permset` belongs to `entry`.
    let writes = WRITE_PERMISSIONS
        .iter()
        .any(|&perm| unsafe { acl_get_perm_np(permset, perm) } == 1);
    if !writes {
        return Ok(false);
    }
    // SAFETY: the caller guarantees `entry`.
    let qualifier = unsafe { acl_get_qualifier(entry) };
    if qualifier.is_null() {
        return Err(io::Error::last_os_error());
    }
    let qualifier = Owned(qualifier);
    let mut id: libc::id_t = 0;
    let mut id_type: c_int = 0;
    // SAFETY: an extended entry's qualifier is a 16-byte `guid_t`, and both outputs are writable.
    if unsafe { mbr_uuid_to_id(qualifier.0.cast(), &mut id, &mut id_type) } != 0 {
        return Ok(true);
    }
    Ok(id_type != ID_TYPE_UID || id != owner)
}
