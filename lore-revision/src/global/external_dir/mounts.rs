// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The live mounts of an external root's virtual instances, from the mount table.

use std::path::Path;
use std::path::PathBuf;

use super::ExternalDirError;

/// The mount points of the live `fuse.lore` mounts whose instance has an external directory in
/// `root`, as [`crate::fs::fuse::discover::live_lore_mounts_kept`] lists them, probing no other.
/// Fails where the mount table can't be read, or an external directory can't be examined.
/// Blocking.
#[cfg(target_os = "linux")]
pub fn live_mounts_in(root: &Path) -> Result<Vec<PathBuf>, ExternalDirError> {
    let mounts = crate::fs::fuse::discover::live_lore_mounts_kept(root, |instance_id| {
        let dir = root.join(instance_id.text_encoding().as_str());
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(ExternalDirError::internal(format!(
                "examining the external directory {}: {err}",
                dir.display()
            ))),
        }
    })?;
    Ok(mounts
        .into_iter()
        .map(|(mount_point, _)| mount_point)
        .collect())
}

/// The mount points of the live mounts whose instance has an external directory in `root`: none,
/// since the mount table of this platform lists no virtual instance.
#[cfg(not(target_os = "linux"))]
pub fn live_mounts_in(_root: &Path) -> Result<Vec<PathBuf>, ExternalDirError> {
    Ok(Vec::new())
}
