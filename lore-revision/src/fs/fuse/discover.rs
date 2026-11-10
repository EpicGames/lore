// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Discovers a FUSE instance's external `.lore` directory by reading
//! `/proc/self/mountinfo` rather than statting anything inside the mount itself
//! (statting inside the mount would recurse into FUSE request handling).

use std::path::Path;
use std::path::PathBuf;

use lore_error_set::WrapInternal;

use crate::global::external_dir::ExternalDirError;
use crate::global::external_dir::check_for_external_lore_dir;
use crate::instance::InstanceId;

const FUSE_LORE_FSTYPE: &str = "fuse.lore";
const LORE_SOURCE_PREFIX: &str = "lore:";
const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";

/// Un-escapes the octal escapes (e.g. `\040` for space) the kernel uses for whitespace and
/// backslashes in `/proc/self/mountinfo` fields.
fn unescape_octal(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let octal = std::str::from_utf8(&bytes[i + 1..i + 4]).unwrap_or_default();
            if let Ok(value) = u8::from_str_radix(octal, 8) {
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Finds the innermost `fuse.lore` mount above `path` in the given mountinfo text, returning
/// its mount point and the instance ID encoded in its `lore:<hex>` source field.
///
/// Per `proc_pid_mountinfo(5)`: fields are space-separated; field 5 (index 4) is the mount
/// point; zero or more optional fields follow the mount options before a literal `-`
/// separator; the field after that is the filesystem type, then the mount source.
pub fn lore_mount_for_path(mountinfo: &str, path: &Path) -> Option<(PathBuf, InstanceId)> {
    let mut best: Option<(PathBuf, InstanceId)> = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        let Some(separator_index) = fields
            .iter()
            .skip(6)
            .position(|field| *field == "-")
            .map(|i| i + 6)
        else {
            continue;
        };
        let Some(fstype) = fields.get(separator_index + 1) else {
            continue;
        };
        if *fstype != FUSE_LORE_FSTYPE {
            continue;
        }
        let Some(source) = fields.get(separator_index + 2) else {
            continue;
        };
        let Some(hex) = unescape_octal(source)
            .strip_prefix(LORE_SOURCE_PREFIX)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(instance_id) = InstanceId::from_hex(&hex) else {
            continue;
        };
        let mount_point = PathBuf::from(unescape_octal(fields[4]));
        if !path.starts_with(&mount_point) {
            continue;
        }
        let is_deeper = match &best {
            Some((best_point, _)) => {
                mount_point.components().count() > best_point.components().count()
            }
            None => true,
        };
        if is_deeper {
            best = Some((mount_point, instance_id));
        }
    }
    best
}

/// The innermost `fuse.lore` mount at or above `path` in `/proc/self/mountinfo`, as
/// [`lore_mount_for_path`] finds it. `None` where there is none, or where the mount table cannot be
/// read.
pub fn lore_mount_containing(path: &Path) -> Option<(PathBuf, InstanceId)> {
    let mountinfo = std::fs::read_to_string(MOUNTINFO_PATH).ok()?;
    lore_mount_for_path(&mountinfo, path)
}

/// Discovers `path`'s external `.lore` directory via `/proc/self/mountinfo`, without statting
/// anything inside the mount. Returns `Ok(None)` when `path` isn't under a `fuse.lore` mount.
pub fn discover_external_dot_lore(path: &Path) -> Result<Option<PathBuf>, ExternalDirError> {
    let mountinfo =
        std::fs::read_to_string(MOUNTINFO_PATH).internal("Reading /proc/self/mountinfo")?;
    match lore_mount_for_path(&mountinfo, path) {
        Some((_, instance_id)) => check_for_external_lore_dir(instance_id),
        None => Ok(None),
    }
}

/// The mount point of the innermost `fuse.lore` mount at or above `path` when that mount answers,
/// so a service serves it. `None` where there is no such mount, or where the innermost one is
/// stale, left by a service that ended without unmounting it. Probes the mount point alone, with
/// a `statfs` its server answers, so a caller on a runtime runs it on a blocking thread.
pub fn answering_lore_mount(path: &Path) -> Option<PathBuf> {
    let (mount_point, _) = lore_mount_containing(path)?;
    (!lore_fuse::mount::is_stale_mount(&mount_point)).then_some(mount_point)
}
