// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use lore_error_set::prelude::*;

use crate::remote::network::UdsListenerError;

/// Directory holding this user's socket, for example `/run/user/1000/lore-1000`.
///
/// `XDG_RUNTIME_DIR` is the right base on Linux: it is per-user, mode `0700`,
/// and cleared on logout. macOS has no such variable, but its `TMPDIR` is
/// already per-user (`/var/folders/...`), which makes it the direct analogue of
/// the `%TEMP%` path the Windows implementation uses. `/tmp` is the last resort
/// and *is* shared between users, which is why the socket always goes in a
/// uid-suffixed subdirectory rather than sitting in the base directly.
fn uds_sock_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("TMPDIR").filter(|value| !value.is_empty()))
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);

    // Safety: getuid() cannot fail and reads no memory through pointers.
    let uid = unsafe { libc::getuid() };
    base.join(format!("lore-{uid}"))
}

/// The socket file for `name`, in this user's socket directory, beside which its claim's lock file
/// lies. Never fails here; the result matches the Windows path, which can.
#[lore_macro::test_pub]
pub(super) fn uds_sock_path(name: &str) -> std::io::Result<PathBuf> {
    Ok(uds_sock_dir().join(name))
}

/// Creates the socket's directory, which holds its claim's lock file too, for this user alone.
pub(super) fn prepare_socket_directory() -> Result<(), UdsListenerError> {
    let dir = uds_sock_dir();
    fs::create_dir_all(&dir)
        .internal_with(|| format!("creating socket directory {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .internal_with(|| format!("restricting socket directory {}", dir.display()))?;
    Ok(())
}
