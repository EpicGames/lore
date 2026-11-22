// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The checks on a move's target that need nothing stopped: its path, its parent, its file
//! system and the executable that serves after the move, whether the move renames or copies the
//! root, and the free space a copy needs.
// The capability probe writes and removes a directory of its own, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;
#[cfg(target_family = "unix")]
use std::sync::atomic::AtomicU64;
#[cfg(target_family = "unix")]
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_error_set::error_set;
use lore_error_set::prelude::*;
use lore_io::clone::Locality;
use lore_io::clone::Space;
use lore_io::clone::Volume;

use super::EXTERNAL;
use super::record::MoveKind;
use super::redirect::made_by_command;
use super::refused;
use super::resolve::Availability;
use super::resolve::RootState;
use super::setting::Setting;
use crate::errors::InvalidArguments;
use crate::fs::walk::Progress;
use crate::fs::walk::TreeSize;
use crate::fs::walk::Visit;

#[error_set]
pub enum CheckError {
    InvalidArguments,
}

/// The name every capability probe's directory starts with.
#[cfg(target_family = "unix")]
const PROBE_PREFIX: &str = ".lore-external-probe-";
/// The name of the file a capability probe writes, in lower case.
#[cfg(target_family = "unix")]
const PROBE_FILE: &str = "probe";
/// The permission bits a capability probe sets and reads back.
#[cfg(target_family = "unix")]
const PROBE_MODE: u32 = 0o604;
/// The modification time a capability probe sets and reads back, with nanoseconds.
#[cfg(target_family = "unix")]
const PROBE_TIME: Duration = Duration::new(1_700_000_000, 123_456_789);

/// Checks that the available external root of `state` can move to the absolute path `target`, and
/// answers how: `None` where `target` is the current root, [`MoveKind::Create`] where the default
/// root doesn't exist yet, and [`MoveKind::Rename`] or [`MoveKind::Copy`] otherwise, as
/// [`move_kind`] chooses. For a copy, refuses on Windows a target that can't hold ACLs, alternate
/// data streams and sparse files, as [`require_copy_features`] does; [`require_copy_space`] checks
/// its free space. Runs every other check on the target that needs nothing stopped, from the path
/// to the file system, and the default location is exempt from the file-system checks. It leaves
/// nothing on disk, so it serves a dry run as it is. Blocking.
pub fn check_target(state: &RootState, target: &Path) -> Result<Option<MoveKind>, CheckError> {
    if target.to_str().is_none() {
        return Err(refused(format!(
            "the target {} isn't valid UTF-8",
            target.display()
        )));
    }
    if !target.is_absolute() {
        return Err(refused(format!(
            "the target {} isn't an absolute path",
            target.display()
        )));
    }
    let root = match &state.availability {
        Availability::Available(root) => root,
        Availability::Unavailable(reason) => {
            return Err(CheckError::internal(format!(
                "The external root isn't available: {}",
                reason.describe()
            )));
        }
        Availability::Interrupted => {
            return Err(CheckError::internal(
                "A move of the external root is interrupted".to_string(),
            ));
        }
    };
    if target == root {
        return Ok(None);
    }
    let at_default = target == state.global.join(EXTERNAL);
    let root_exists = std::fs::symlink_metadata(root).is_ok();
    match std::fs::symlink_metadata(target) {
        Ok(_) if at_default && holds_command_redirect(&state.global, target) => {}
        Ok(_) if root_exists && leads_to(target, root) => return Ok(None),
        Ok(_) => {
            return Err(refused(format!("the target {} exists", target.display())));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(read_failure("examine the target", target, &err)),
    }
    let (ancestor, resolved) = through_nearest_existing(target)?;
    let root_resolved = if root_exists {
        canonical(root)?
    } else {
        through_nearest_existing(root)?.1
    };
    if resolved.starts_with(&root_resolved) || root_resolved.starts_with(&resolved) {
        return Err(refused(format!(
            "the target {} lies inside the external root at {} or contains it",
            target.display(),
            root.display()
        )));
    }
    crate::repository::refuse_instance_nested_at(&resolved)?;
    if root_exists {
        require_working_directory_outside(root)?;
    }
    #[cfg(target_family = "unix")]
    require_safe_parent(&ancestor)?;
    let (target_volume, target_locality) = volume_of(&ancestor)?;
    if !at_default {
        require_local(&target_volume, target_locality, &ancestor)?;
        #[cfg(target_family = "unix")]
        probe_capabilities(&ancestor, root_exists.then_some(root_resolved.as_path()))?;
    }
    if !root_exists {
        return Ok(Some(MoveKind::Create));
    }
    let (root_volume, root_locality) = volume_of(&root_resolved)?;
    let kind = move_kind(
        (&root_volume, root_locality.mount_id),
        (&target_volume, target_locality.mount_id),
    );
    #[cfg(feature = "test-util")]
    let kind = if crate::test_util::move_override(&state.global).is_some() {
        MoveKind::Copy
    } else {
        kind
    };
    #[cfg(target_family = "windows")]
    if kind == MoveKind::Copy {
        require_copy_features(&target_volume, target_locality)?;
    }
    Ok(Some(kind))
}

/// Refuses a copy of the available external root of `state` to the absolute path `target` where
/// the volume of the target's nearest existing ancestor has less free space than the copy takes,
/// measured as [`measure_root`] measures it, until `progress` is stopped. Answers what the copy
/// takes. Blocking.
pub fn require_copy_space(
    state: &RootState,
    target: &Path,
    progress: &Progress,
) -> Result<TreeSize, CheckError> {
    let Availability::Available(root) = &state.availability else {
        return Err(CheckError::internal("The external root isn't available"));
    };
    let (ancestor, _) = through_nearest_existing(target)?;
    let space = lore_io::clone::space_of(&ancestor)
        .map_err(|err| read_failure("read the free space of", &ancestor, &err))?;
    #[cfg(feature = "test-util")]
    let space = if crate::test_util::move_override(&state.global)
        == Some(crate::test_util::MoveOverride::CopyToFullVolume)
    {
        Space {
            available: 0,
            ..space
        }
    } else {
        space
    };
    let needed = measure_root(root, space.block, progress)?;
    require_space(needed.size, &space, &ancestor)?;
    Ok(needed)
}

/// Checks that a new, empty root can be made at the absolute path `target`, as `--abandon`
/// makes one: the path checks of [`check_target`] that need no current root, the parent rule on
/// Unix, and outside the default location of `global`, a local file system with the capabilities
/// [`probe_capabilities`] checks. The default location may hold the command's redirect, which the
/// new root takes the place of. Blocking.
pub fn check_new_root(global: &Path, target: &Path) -> Result<(), CheckError> {
    if target.to_str().is_none() || !target.is_absolute() {
        return Err(refused(format!(
            "the target {} isn't an absolute UTF-8 path",
            target.display()
        )));
    }
    let at_default = target == global.join(EXTERNAL);
    if std::fs::symlink_metadata(target).is_ok()
        && !(at_default && holds_command_redirect(global, target))
    {
        return Err(refused(format!("the target {} exists", target.display())));
    }
    let (ancestor, resolved) = through_nearest_existing(target)?;
    crate::repository::refuse_instance_nested_at(&resolved)?;
    #[cfg(target_family = "unix")]
    require_safe_parent(&ancestor)?;
    if !at_default {
        let (volume, locality) = volume_of(&ancestor)?;
        require_local(&volume, locality, &ancestor)?;
        #[cfg(target_family = "unix")]
        probe_capabilities(&ancestor, None)?;
    }
    Ok(())
}

/// Refuses an adopt of `target` where it is a link or a junction, which names a root without being
/// one. Blocking.
pub fn refuse_linked_root(target: &Path) -> Result<(), CheckError> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(refused(format!(
            "{} is a link, not an external root: adopt the directory it names",
            target.display()
        ))),
        _ => Ok(()),
    }
}

/// Whether the default location `default` of the global data directory `global` holds a redirect
/// that [`made_by_command`] answers for, given the setting.
fn holds_command_redirect(global: &Path, default: &Path) -> bool {
    let Ok(link) = std::fs::read_link(default) else {
        return false;
    };
    let setting = Setting::load(global).ok().flatten();
    made_by_command(&link, setting.as_ref(), None)
}

/// Whether the existing `path` leads to `root`, which it is or redirects to.
fn leads_to(path: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path == root,
        _ => false,
    }
}

/// The canonical form of the existing `path`.
fn canonical(path: &Path) -> Result<PathBuf, CheckError> {
    std::fs::canonicalize(path)
        .map_err(|err| CheckError::internal(format!("Unable to resolve {}: {err}", path.display())))
}

/// The nearest existing ancestor of `path`, which must be a directory, in canonical form, and
/// `path` resolved through it.
fn through_nearest_existing(path: &Path) -> Result<(PathBuf, PathBuf), CheckError> {
    for ancestor in path.ancestors().skip(1) {
        match std::fs::canonicalize(ancestor) {
            Ok(canonical) if canonical.is_dir() => {
                let below = path.strip_prefix(ancestor).unwrap_or(path);
                let resolved = canonical.join(below);
                return Ok((canonical, resolved));
            }
            Ok(_) => {
                return Err(refused(format!(
                    "{} isn't a directory, so {} can't be created",
                    ancestor.display(),
                    path.display()
                )));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(read_failure("resolve", ancestor, &err)),
        }
    }
    Err(refused(format!(
        "no directory above {} exists",
        path.display()
    )))
}

/// On Windows, refuses while the caller's working directory is inside the current `root`, which
/// Windows then refuses to rename.
fn require_working_directory_outside(root: &Path) -> Result<(), CheckError> {
    crate::instance::require_working_directory_outside(
        &root.to_string_lossy(),
        "move the external root",
        "Run the command from outside the external root",
    )
    .forward_any::<CheckError>("checking the working directory")
}

/// The volume that holds the existing `path`, and its locality.
fn volume_of(path: &Path) -> Result<(Volume, Locality), CheckError> {
    let read = |path| {
        Ok::<_, std::io::Error>((
            lore_io::clone::volume_of(path)?,
            lore_io::clone::locality_of(path)?,
        ))
    };
    read(path).map_err(|err| read_failure("read the volume of", path, &err))
}

/// The failure `err` to `what` `path`: a refusal where it says the drive holds no media, as a
/// Windows drive with no disc or card in it answers, and internal otherwise.
#[lore_macro::test_pub]
fn read_failure(what: &str, path: &Path, err: &std::io::Error) -> CheckError {
    #[cfg(target_family = "windows")]
    if err.raw_os_error() == i32::try_from(windows_sys::Win32::Foundation::ERROR_NOT_READY).ok() {
        return refused(format!("the drive of {} holds no media", path.display()));
    }
    CheckError::internal(format!("Unable to {what} {}: {err}", path.display()))
}

/// Refuses a target whose nearest existing ancestor `ancestor` is on `volume`, unless its
/// `locality` says its file system is local.
fn require_local(volume: &Volume, locality: Locality, ancestor: &Path) -> Result<(), CheckError> {
    if locality.local {
        return Ok(());
    }
    Err(refused(format!(
        "{} is on the {} volume at {}, which isn't a file system Lore has verified as local and \
         as durable across a power loss",
        ancestor.display(),
        volume.filesystem,
        volume.mount_point.display()
    )))
}

/// Refuses a target whose nearest existing ancestor `ancestor` fails [`is_safe_parent`].
#[cfg(target_family = "unix")]
fn require_safe_parent(ancestor: &Path) -> Result<(), CheckError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(ancestor).map_err(|err| {
        CheckError::internal(format!("Unable to examine {}: {err}", ancestor.display()))
    })?;
    let acl_lets_others_write = super::resolve::acl_lets_others_write(ancestor, metadata.uid())
        .map_err(|_unreadable| {
            CheckError::internal(format!(
                "Unable to read the access control list of {}",
                ancestor.display()
            ))
        })?;
    // SAFETY: geteuid has no preconditions.
    let user = unsafe { libc::geteuid() };
    if is_safe_parent(metadata.uid(), metadata.mode(), acl_lets_others_write, user) {
        return Ok(());
    }
    Err(refused(format!(
        "{} belongs to another user, or others can write it",
        ancestor.display()
    )))
}

/// Whether a directory owned by `owner`, with the permission bits of `mode`, can hold the user
/// `user`'s external root: it belongs to `user` or to root, and no one else can write it, as
/// the owner rule of a root reads that.
#[cfg(target_family = "unix")]
#[lore_macro::test_pub]
fn is_safe_parent(owner: u32, mode: u32, acl_lets_others_write: bool, user: u32) -> bool {
    (owner == user || owner == 0)
        && super::resolve::is_private(owner, mode, acl_lets_others_write, owner)
}

/// Probes, in a directory of its own inside `ancestor`, that the file system there compares
/// names as the current root at `root` does, and holds symbolic links, permission bits and
/// modification times to the nanosecond. Names can't clash with no `root`, or with a root that
/// holds no external directory. The probe's directory is removed before it returns. Blocking.
#[cfg(target_family = "unix")]
pub fn probe_capabilities(ancestor: &Path, root: Option<&Path>) -> Result<(), CheckError> {
    let probe = ProbeDir::create(ancestor)?;
    let failed = |what: &str, err: std::io::Error| {
        CheckError::internal(format!("Unable to {what} in {}: {err}", probe.0.display()))
    };
    let file = probe.0.join(PROBE_FILE);
    let written = std::fs::File::create(&file).map_err(|err| failed("write a file", err))?;
    if let Some(root) = root
        && let Some(root_ignores_case) = names_ignore_case(root)?
    {
        let ignores_case = probe.0.join(PROBE_FILE.to_ascii_uppercase()).exists();
        if ignores_case != root_ignores_case {
            return Err(refused(format!(
                "{} compares names {}, and the external root at {} {}",
                ancestor.display(),
                case_rule(ignores_case),
                root.display(),
                case_rule(root_ignores_case)
            )));
        }
    }
    let lacks = |what: &str| {
        refused(format!(
            "the file system at {} can't hold {what}",
            ancestor.display()
        ))
    };
    if std::os::unix::fs::symlink(PROBE_FILE, probe.0.join("link")).is_err() {
        return Err(lacks("symbolic links"));
    }
    std::fs::set_permissions(
        &file,
        std::os::unix::fs::PermissionsExt::from_mode(PROBE_MODE),
    )
    .map_err(|err| failed("set permission bits", err))?;
    let time = std::time::SystemTime::UNIX_EPOCH + PROBE_TIME;
    written
        .set_modified(time)
        .map_err(|err| failed("set a modification time", err))?;
    let metadata = std::fs::metadata(&file).map_err(|err| failed("read a file", err))?;
    if std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o777 != PROBE_MODE {
        return Err(lacks("permission bits"));
    }
    if metadata.modified().ok() != Some(time) {
        return Err(lacks("modification times to the nanosecond"));
    }
    Ok(())
}

/// The words for whether names compare regardless of case.
#[cfg(target_family = "unix")]
fn case_rule(ignores_case: bool) -> &'static str {
    if ignores_case {
        "regardless of case"
    } else {
        "by case"
    }
}

/// Whether names in the external root `root` compare regardless of case: whether the upper-case
/// form of an external directory's lower-case hex name finds an entry. `None` where the root
/// holds no external directory with a letter in its name.
#[cfg(target_family = "unix")]
fn names_ignore_case(root: &Path) -> Result<Option<bool>, CheckError> {
    let entries = std::fs::read_dir(root)
        .map_err(|err| CheckError::internal(format!("Unable to list {}: {err}", root.display())))?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            CheckError::internal(format!("Unable to list {}: {err}", root.display()))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if crate::instance::InstanceId::from_hex(name).is_some()
            && name.bytes().any(|byte| byte.is_ascii_lowercase())
        {
            return Ok(Some(
                std::fs::symlink_metadata(root.join(name.to_ascii_uppercase())).is_ok(),
            ));
        }
    }
    Ok(None)
}

/// A capability probe's directory, removed with what it holds when dropped.
#[cfg(target_family = "unix")]
struct ProbeDir(PathBuf);

#[cfg(target_family = "unix")]
impl ProbeDir {
    /// Creates a probe directory in `ancestor`, under a name no other probe uses.
    fn create(ancestor: &Path) -> Result<ProbeDir, CheckError> {
        static PROBES: AtomicU64 = AtomicU64::new(0);
        let path = ancestor.join(format!(
            "{PROBE_PREFIX}{}-{}",
            std::process::id(),
            PROBES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).map_err(|err| {
            refused::<CheckError>(format!("unable to write in {}: {err}", ancestor.display()))
        })?;
        Ok(ProbeDir(path))
    }
}

#[cfg(target_family = "unix")]
impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How a move from a root on the volume `current` to a target on the volume `target`, each with
/// the mount ID [`Locality::mount_id`] reads, goes: a rename where both have one volume ID and
/// either one mount ID, which `statx` names on Linux, or, without mount IDs, one mount point, and
/// a copy otherwise.
#[lore_macro::test_pub]
fn move_kind(
    (current, current_mount): (&Volume, Option<u64>),
    (target, target_mount): (&Volume, Option<u64>),
) -> MoveKind {
    let same_mount = match (current_mount, target_mount) {
        (Some(current), Some(target)) => current == target,
        _ => current.mount_point == target.mount_point,
    };
    if current.id == target.id && same_mount {
        MoveKind::Rename
    } else {
        MoveKind::Copy
    }
}

/// Measures what a copy of the external root at `root` takes on a volume whose blocks are
/// `block` bytes, leaving out what each external directory's `write-staging/` holds, which FUSE
/// empties at mount. Fails as internal once `progress` is stopped. Blocking.
pub fn measure_root(root: &Path, block: u64, progress: &Progress) -> Result<TreeSize, CheckError> {
    crate::fs::walk::measure(
        root,
        &|_path, relative, _kind| {
            if in_write_staging(relative) {
                Visit::Skip
            } else {
                Visit::EachEntry
            }
        },
        block,
        progress,
    )
    .map_err(|err| CheckError::internal(format!("Unable to measure {}: {err}", root.display())))
}

/// Whether the entry at `relative` in an external root lies inside an external directory's
/// `write-staging/`.
#[cfg(target_os = "linux")]
pub(super) fn in_write_staging(relative: &Path) -> bool {
    let mut components = relative.components();
    components.next();
    components
        .next()
        .is_some_and(|name| name.as_os_str() == crate::fs::fuse::write::STAGING_DIRECTORY)
        && components.next().is_some()
}

/// Only FUSE instances, which run on Linux, stage writes.
#[cfg(not(target_os = "linux"))]
pub(super) fn in_write_staging(_relative: &Path) -> bool {
    false
}

/// Refuses a copy of `needed` bytes to the volume at `at`, which has `space`.
pub fn require_space(needed: u64, space: &Space, at: &Path) -> Result<(), CheckError> {
    if space.available >= needed {
        return Ok(());
    }
    Err(refused(format!(
        "the volume of {} has {} bytes free, and the external directories take {needed}",
        at.display(),
        space.available
    )))
}

/// Refuses a copy to `volume` on Windows unless its file system holds access control lists,
/// alternate data streams and sparse files, as its `locality` says.
#[cfg(target_family = "windows")]
pub fn require_copy_features(volume: &Volume, locality: Locality) -> Result<(), CheckError> {
    if locality.holds_acls_streams_and_sparse_files() {
        return Ok(());
    }
    Err(refused(format!(
        "the {} volume at {} can't hold access control lists, alternate data streams and \
         sparse files",
        volume.filesystem,
        volume.mount_point.display()
    )))
}

/// Refuses unless the executable that serves after the move runs `service
/// set-external-directory --help` within `timeout`: the `configured` executable, unless it is
/// `restarts`, the binary the service starts again from. With none configured, it probes
/// nothing. A Windows build from before the setting can't read a write directory through the
/// redirect. Blocking.
pub fn probe_service_executable(
    configured: Option<&Path>,
    restarts: Option<&Path>,
    timeout: Duration,
) -> Result<(), CheckError> {
    let Some(executable) = configured else {
        return Ok(());
    };
    if Some(executable) == restarts {
        return Ok(());
    }
    let refuse = |why: String| {
        CheckError::internal(format!(
            "The configured service executable {} {why}, so it can't serve the moved external \
             root",
            executable.display()
        ))
    };
    let mut child = std::process::Command::new(executable)
        .args(["service", "set-external-directory", "--help"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| refuse(format!("can't run: {err}")))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(refuse(format!(
                    "doesn't know `service set-external-directory` ({status})"
                )));
            }
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(refuse(format!("didn't answer within {timeout:?}")));
            }
            Err(err) => return Err(refuse(format!("can't be waited for: {err}"))),
        }
    }
}
