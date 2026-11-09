// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows_sys::Win32::Storage::FileSystem::GetTempPathW;

use crate::remote::network::UdsListenerError;

/// The socket file for `socket_name`, in the temporary directory, beside which its claim's lock
/// file lies. Fails if the temporary directory cannot be read.
#[lore_macro::test_pub]
pub(super) fn uds_sock_path(socket_name: &str) -> io::Result<PathBuf> {
    // Not `GetTempPath2W`: the path must be the same whether or not this runs as a system process.
    // Safety: asks for the length only, writing nothing.
    let len = unsafe { GetTempPathW(0, std::ptr::null_mut()) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut path = vec![0; len as usize];
    // Safety: writes at most `len` units, into a buffer of that length.
    let written = unsafe { GetTempPathW(len, path.as_mut_ptr()) };
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    // A count the buffer cannot hold is the length a longer path needs: it changed between calls.
    if written >= len {
        return Err(io::Error::other(
            "the temporary directory changed while it was read",
        ));
    }
    path.truncate(written as usize);
    Ok(PathBuf::from(OsString::from_wide(&path)).join(socket_name))
}

/// Nothing to prepare: the temporary directory exists, and is the user's own.
pub(super) fn prepare_socket_directory() -> Result<(), UdsListenerError> {
    Ok(())
}
