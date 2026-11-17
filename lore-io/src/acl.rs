// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Whether a path's extended access control list, on macOS, lets anyone but its owner change it.
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
