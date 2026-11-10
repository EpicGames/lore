// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Cloning an on-disk instance through the clone backend its pair of volumes allows.
//!
//! The backends know filesystems only. What a clone of an instance leaves out is decided here:
//! the files a clone writes into its own Lore directory, the Lore directory's transient state,
//! which belongs to the running source, and repositories nested in the tree, which are instances
//! of their own.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::error::InvalidPath;
use lore_base::error::NotSupported;
use lore_error_set::prelude::*;
use lore_io::clone::Backend;
use lore_io::clone::CloneStats;
use lore_io::clone::Detection;
use lore_io::clone::Kind;
use lore_io::clone::Progress;
use lore_io::clone::Visit;

use crate::fs::filesystem_provider::CloneAnswer;
use crate::fs::filesystem_provider::CloneRun;
use crate::fs::filesystem_provider::InstanceCloneError;
use crate::fs::filesystem_provider::clone_io_failure;
use crate::fs::filesystem_provider::lore_directory_within;
use crate::instance::CLONE_IN_PROGRESS_MARK;
use crate::records::NO_STORE_RECORDS;
use crate::records::RECORDS_DIR;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::INSTANCE;
use crate::repository::SERVICE;
use crate::repository::TEMP_FILE_EXTENSION;

/// What a clone of an instance whose Lore directory is at `dot_dir` does with the entry at
/// `relative`, both relative to the tree. The Lore directory and the directories in it are
/// visited entry by entry, so these are left out wherever they lie in it:
///
/// - the instance file, the mark of an instance that holds no store records, and the mark of a
///   clone in progress, which the clone writes for itself;
/// - lock files and the service's state;
/// - the temporary files of interrupted writes: `.~loretemp` and `.tmp` files, as a records shard
///   or a configuration is written to, and a clone's probe files.
#[lore_macro::test_pub]
fn visit(dot_dir: &Path, relative: &Path, kind: Kind) -> Visit {
    let Ok(inside) = relative.strip_prefix(dot_dir) else {
        return Visit::Clone;
    };
    let name = inside.file_name().unwrap_or_default().to_string_lossy();
    let written_by_the_clone = inside == Path::new(INSTANCE)
        || inside == Path::new(CLONE_IN_PROGRESS_MARK)
        || inside == Path::new(RECORDS_DIR).join(NO_STORE_RECORDS);
    let transient = inside == Path::new(SERVICE)
        || name == "lock"
        || name.ends_with(".lock")
        || name.ends_with(TEMP_FILE_EXTENSION)
        || name.ends_with(".tmp")
        || name.starts_with(lore_io::clone::PROBE_PREFIX);
    if written_by_the_clone || transient {
        Visit::Skip
    } else if kind == Kind::Directory {
        Visit::EachEntry
    } else {
        Visit::Clone
    }
}

/// Whether the directory at `path` is the root of a repository nested in the tree: it holds a
/// `.lore` directory, as `state::is_nested_repository_root` decides for stage and reset, which
/// take the files beside an older `.urc` directory for the parent's own. A nested repository is
/// an instance of its own, and a copy of it would carry that instance's ID, so two working trees
/// would share its anchors.
fn is_nested_repository(path: &Path) -> bool {
    std::fs::metadata(path.join(DOT_LORE)).is_ok_and(|metadata| metadata.is_dir())
}

/// Whether `relative` names a Lore directory, `.lore` or `.urc`.
fn is_lore_directory(relative: &Path) -> bool {
    relative
        .file_name()
        .is_some_and(|name| name == DOT_LORE || name == DOT_URC)
}

/// What a clone of a tree does with the entry at `path`, which is at `relative` in it: [`visit`]
/// for an instance whose Lore directory is at `dot_dir`, with nested repositories left out. A
/// directory in `holds_nested` is cloned entry by entry, as
/// [`directories_above_nested_repositories`] describes, so a whole-directory clone copies no
/// nested repository or volume mounted in the tree that the search before it found. A Lore
/// directory outside the instance's own, `.lore` or `.urc`, is left out too, as of an older
/// working tree whose files the tree holds, or of a repository created in the tree after its
/// parent was checked, so no copy carries another instance's ID.
#[lore_macro::test_pub]
fn visit_tree(
    dot_dir: &Path,
    holds_nested: &HashSet<PathBuf>,
    path: &Path,
    relative: &Path,
    kind: Kind,
) -> Visit {
    if kind == Kind::Directory && !relative.starts_with(dot_dir) {
        if is_lore_directory(relative) || is_nested_repository(path) {
            return Visit::Skip;
        }
        if holds_nested.contains(relative) {
            return Visit::EachEntry;
        }
    }
    visit(dot_dir, relative, kind)
}

/// The directories of the tree at `source`, relative to it, that a backend that clones whole
/// directories clones entry by entry instead: those above a nested repository or another Lore
/// directory, and those above a volume mounted in the tree along with its mount point. Empty for
/// any other backend, as each directory it reaches is checked as it is listed. A whole-directory
/// clone copies none of a mounted volume's files, so the volume's entries are cloned one by one,
/// which copies them where the clone allows a copy and fails it otherwise. The instance's own Lore
/// directory, at `dot_dir`, is not searched. A repository created, or a volume mounted, after the
/// search inside a directory the clone then copies whole is missed.
///
/// The search lists every directory of the tree once, as the clone is planned and before it takes
/// any lock: on APFS, over 100,000 files in 11,000 directories, 0.14 to 0.19 s, against 1.24 to
/// 1.25 s for the whole-directory clone itself. It fails as interrupted once `progress` is
/// stopped.
#[lore_macro::test_pub]
fn directories_above_nested_repositories(
    backend: Backend,
    source: &Path,
    dot_dir: &Path,
    progress: &Progress,
) -> std::io::Result<HashSet<PathBuf>> {
    let mut above = HashSet::new();
    if backend != Backend::Apfs {
        return Ok(above);
    }
    let own = source.join(dot_dir);
    let device = device_of(source)?;
    let found = lore_io::clone::find_directories(
        source,
        &|path| {
            path == own
                || is_lore_directory(path)
                || is_nested_repository(path)
                || device_of(path).is_ok_and(|other| other != device)
        },
        progress,
    )?;
    for nested in found.iter().filter(|path| **path != own) {
        let mut relative = nested.strip_prefix(source).unwrap_or(nested);
        if device_of(nested).is_ok_and(|other| other != device) {
            above.insert(relative.to_path_buf());
        }
        while let Some(parent) = relative.parent() {
            if parent.as_os_str().is_empty() || !above.insert(parent.to_path_buf()) {
                break;
            }
            relative = parent;
        }
    }
    Ok(above)
}

/// The device the directory at `path` lies on, which differs from its parent's where a volume is
/// mounted there.
#[cfg(target_family = "unix")]
fn device_of(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    Ok(std::fs::symlink_metadata(path)?.dev())
}

/// The device the directory at `path` lies on: one for every path, as only APFS, which Windows
/// does not have, clones whole directories.
#[cfg(target_family = "windows")]
#[allow(clippy::unnecessary_wraps)]
fn device_of(_path: &Path) -> std::io::Result<u64> {
    Ok(0)
}

/// Clones the tree at `source`, whose Lore directory is at `dot_dir` relative to it, into
/// `destination` with `backend`, as [`lore_io::clone::clone_into`] does, leaving out what
/// [`visit_tree`] skips, with the directories in `holds_nested` cloned entry by entry.
#[lore_macro::test_pub]
fn clone_tree(
    backend: Backend,
    source: &Path,
    dot_dir: &Path,
    destination: &Path,
    holds_nested: &HashSet<PathBuf>,
    copy_unclonable: bool,
    progress: &Progress,
) -> std::io::Result<CloneStats> {
    lore_io::clone::clone_into(
        backend,
        source,
        destination,
        &|path, relative, kind| visit_tree(dot_dir, holds_nested, path, relative, kind),
        copy_unclonable,
        progress,
    )
}

/// Why `detection` found no backend that shares blocks, naming both volumes.
fn reason_to_copy(detection: &Detection) -> String {
    let (source, destination) = (&detection.source, &detection.destination);
    if source.id == destination.id && source.mount_point == destination.mount_point {
        format!(
            "the {} volume at {} does not clone files",
            destination.filesystem,
            destination.mount_point.display()
        )
    } else {
        format!(
            "the source is on the {} volume at {} and the destination on the {} volume at {}, \
             and no filesystem shares blocks between them",
            source.filesystem,
            source.mount_point.display(),
            destination.filesystem,
            destination.mount_point.display()
        )
    }
}

/// [`crate::fs::filesystem_provider::FilesystemProvider::clone_answer`] for a tree on disk:
/// probes the backends from the source's Lore directory into the destination's parent, and finds
/// the directories a whole-directory clone clones entry by entry where one shares blocks, outside a
/// dry run, or measures the tree where none does. Dropping the future stops the search or the measurement.
pub(super) async fn answer(
    source: &Path,
    dot_path: &Path,
    destination: &Path,
) -> Result<CloneAnswer, InstanceCloneError> {
    let dot_dir = lore_directory_within(source, dot_path)?.to_path_buf();
    let source = source.to_path_buf();
    let dot_path = dot_path.to_path_buf();
    let parent = destination
        .parent()
        .ok_or_else(|| InvalidPath {
            path: format!("{}: names no directory to create", destination.display()),
        })?
        .to_path_buf();
    // A dry run makes no clone, which the search serves.
    let search = !crate::lore::execution_context().globals().dry_run();
    let progress = Arc::new(Progress::default());
    let mut stop = StopOnDrop(Some(Arc::clone(&progress)));
    let answered = lore_base::lore_spawn_blocking!(move || {
        let detection = lore_io::clone::detect(&dot_path, &parent)
            .map_err(|err| clone_io_failure(err, "probing how the instance can be cloned"))?;
        if detection.backend.shares_blocks() {
            let entry_by_entry = if search {
                directories_above_nested_repositories(
                    detection.backend,
                    &source,
                    &dot_dir,
                    &progress,
                )
            } else {
                Ok(HashSet::new())
            }
            .map_err(|err| {
                clone_io_failure(err, "finding the repositories nested in the instance")
            })?;
            return Ok(CloneAnswer::SharesBlocks {
                backend: detection.backend,
                volume: detection.destination,
                entry_by_entry: Arc::new(entry_by_entry),
            });
        }
        let space = lore_io::clone::space_of(&parent)
            .map_err(|err| clone_io_failure(err, "reading the destination's free space"))?;
        let size = lore_io::clone::measure(
            &source,
            &|path, relative, kind| visit_tree(&dot_dir, &HashSet::new(), path, relative, kind),
            space.block,
            &progress,
        )
        .map_err(|err| clone_io_failure(err, "measuring the instance"))?;
        Ok(CloneAnswer::CopiesBytes {
            reason: reason_to_copy(&detection),
            size,
            volume: detection.destination,
            available: space.available,
        })
    })
    .await
    .internal("probing how the instance can be cloned")?;
    stop.0 = None;
    answered
}

/// Asks the walk it is given to stop when dropped, as it is when the future awaiting the walk is
/// dropped.
struct StopOnDrop(Option<Arc<Progress>>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(progress) = &self.0 {
            progress.stop();
        }
    }
}

/// [`crate::fs::filesystem_provider::FilesystemProvider::clone_instance`] for a tree on disk. A
/// clone promised to share blocks that fails to clone an entry reports
/// [`InstanceCloneError::NotSupported`].
pub(super) async fn clone(
    source: &Path,
    dot_path: &Path,
    destination: &Path,
    answer: &CloneAnswer,
    copy_unclonable: bool,
    run: &CloneRun,
) -> Result<CloneStats, InstanceCloneError> {
    let (backend, holds_nested) = match answer {
        CloneAnswer::SharesBlocks {
            backend,
            entry_by_entry,
            ..
        } => (*backend, Arc::clone(entry_by_entry)),
        CloneAnswer::CopiesBytes { .. } => (Backend::Copy, Arc::default()),
        CloneAnswer::Unsupported { reason } => {
            return Err(NotSupported {
                operation: reason.clone(),
            }
            .into());
        }
    };
    let dot_dir = lore_directory_within(source, dot_path)?.to_path_buf();
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let mut stop = StopOnDrop(Some(Arc::clone(&run.progress)));
    let progress = Arc::clone(&run.progress);
    let hold = Arc::clone(&run.hold);
    let cloned = lore_base::lore_spawn_blocking!(move || {
        let _hold = hold;
        clone_tree(
            backend,
            &source,
            &dot_dir,
            &destination,
            &holds_nested,
            copy_unclonable,
            &progress,
        )
    })
    .await
    .internal("cloning the instance's directory")?;
    stop.0 = None;
    cloned.map_err(|err| clone_io_failure(err, "cloning the instance's directory"))
}
