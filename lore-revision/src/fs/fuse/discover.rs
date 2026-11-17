// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Discovers a FUSE instance's external `.lore` directory by reading
//! `/proc/self/mountinfo` rather than statting anything inside the mount itself
//! (statting inside the mount would recurse into FUSE request handling).

use std::path::Path;
use std::path::PathBuf;

use crate::global::external_dir::ExternalDirError;
use crate::global::external_dir::resolve::Lookup;
use crate::global::external_dir::resolve::RootState;
use crate::global::external_dir::unknown_external_dir;
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
    let mountinfo = mountinfo_for(path).ok()?;
    lore_mount_for_path(&mountinfo, path)
}

/// The text of `/proc/self/mountinfo`, for a lookup of `path`. Built with `test-util`, it is the
/// table a test planted for a mount point at or above `path`, where there is one.
fn mountinfo_for(path: &Path) -> std::io::Result<String> {
    #[cfg(feature = "test-util")]
    if let Some(planted) = test_util::planted_for(path) {
        return planted;
    }
    #[cfg(not(feature = "test-util"))]
    let _ = path;
    std::fs::read_to_string(MOUNTINFO_PATH)
}

/// Discovers `path`'s external `.lore` directory via `/proc/self/mountinfo`, without statting
/// anything inside the mount, in the external root `state` or, where it is `None`, one resolved
/// once a mount matches. Returns `Ok(None)` when `path` isn't under a `fuse.lore` mount, or the
/// mount table can't be read, and fails where the mount's external directory is unknown.
pub fn discover_external_dot_lore(
    path: &Path,
    state: Option<&RootState>,
) -> Result<Option<PathBuf>, ExternalDirError> {
    let Some((_, instance_id)) = lore_mount_containing(path) else {
        return Ok(None);
    };
    let lookup = match state {
        Some(state) => state.lookup(instance_id),
        None => crate::global::external_dir::resolve::resolve()?.lookup(instance_id),
    };
    match lookup {
        Lookup::Present(dot_lore) => Ok(Some(dot_lore)),
        Lookup::Absent => Ok(None),
        Lookup::Unknown(unknown) => Err(unknown_external_dir(instance_id, unknown)),
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

/// Mount tables a test plants for the paths below a mount point, so a test of a lookup inside a
/// virtual instance needs no mount, and sees no other test's.
#[cfg(feature = "test-util")]
pub mod test_util {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::LazyLock;

    use parking_lot::Mutex;

    use super::FUSE_LORE_FSTYPE;
    use super::LORE_SOURCE_PREFIX;
    use crate::instance::InstanceId;

    /// Each planted mount point, with its table's text, or `None` for a table that fails to read.
    type PlantedTables = Vec<(PathBuf, Option<String>)>;

    static PLANTED: LazyLock<Mutex<PlantedTables>> = LazyLock::new(Default::default);

    /// A planted table, which lookups read until it drops.
    #[must_use]
    pub struct PlantedTable(PathBuf);

    impl Drop for PlantedTable {
        fn drop(&mut self) {
            PLANTED
                .lock()
                .retain(|(mount_point, _)| *mount_point != self.0);
        }
    }

    /// Plants `table` as the mount table every lookup of a path at or below `mount_point` reads,
    /// on any thread.
    pub fn plant_table(mount_point: &Path, table: String) -> PlantedTable {
        plant(mount_point, Some(table))
    }

    /// Plants a mount table that fails to read, for every lookup of a path at or below
    /// `mount_point`.
    pub fn plant_unreadable_table(mount_point: &Path) -> PlantedTable {
        plant(mount_point, None)
    }

    fn plant(mount_point: &Path, table: Option<String>) -> PlantedTable {
        PLANTED.lock().push((mount_point.to_path_buf(), table));
        PlantedTable(mount_point.to_path_buf())
    }

    /// A mount table line for a `fuse.lore` mount of `instance_id` at `mount_point`, with its
    /// spaces escaped as the kernel escapes them.
    pub fn fuse_lore_entry(mount_point: &Path, instance_id: InstanceId) -> String {
        let escaped = mount_point.display().to_string().replace(' ', "\\040");
        format!(
            "40 30 0:50 / {escaped} rw,nosuid,nodev,relatime shared:1 - {FUSE_LORE_FSTYPE} \
             {LORE_SOURCE_PREFIX}{} rw,user_id=0,group_id=0\n",
            instance_id.text_encoding()
        )
    }

    /// The deepest table planted at or above `path`, read as the file would be.
    pub(super) fn planted_for(path: &Path) -> Option<std::io::Result<String>> {
        let planted = PLANTED.lock();
        let (_, table) = planted
            .iter()
            .filter(|(mount_point, _)| path.starts_with(mount_point))
            .max_by_key(|(mount_point, _)| mount_point.components().count())?;
        Some(table.clone().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the planted mount table fails to read",
            )
        }))
    }
}
