// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The redirect: `external` in the global data directory, a symbolic link on Unix and a junction
//! on Windows, which leads Lore versions that don't read the setting to the external root.

use std::path::Path;
use std::path::PathBuf;

use lore_error_set::prelude::*;

use super::EXTERNAL;
use super::ExternalDirError;
use super::record::MoveRecord;
use super::setting::Setting;
use crate::util::fs::unlink;

/// The name in the global data directory where a redirect is made before it is placed at the
/// default location.
pub const TEMPORARY_REDIRECT: &str = "external.redirect";

/// What the default location holds once [`redirect_to`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Redirect {
    /// A redirect to the root, or nothing where no root is wanted there.
    Matched,
    /// What the command didn't make, which it keeps.
    Kept(Occupant),
}

/// What the default location holds that the redirect never replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupant {
    /// A directory with entries.
    Entries,
    /// A redirect to this path, which neither the setting nor the move record names.
    OtherRedirect(PathBuf),
    /// Something other than a directory or a redirect.
    Other,
}

/// What the default location holds, as one examination found it.
enum Found {
    /// Nothing.
    Free,
    /// An empty directory.
    EmptyDirectory,
    /// A redirect to this path.
    Redirect(PathBuf),
    /// What is never replaced.
    Kept(Occupant),
}

/// Whether a redirect to `target` is one the command made: one to a root that `setting` or
/// `record` names. The default location, which both name with an empty path, is no target.
pub fn made_by_command(
    target: &Path,
    setting: Option<&Setting>,
    record: Option<&MoveRecord>,
) -> bool {
    let named = setting.map(|setting| setting.path.as_str());
    let moved = record
        .into_iter()
        .flat_map(|record| [record.from.as_str(), record.to.as_str()]);
    named
        .into_iter()
        .chain(moved)
        .any(|root| !root.is_empty() && Path::new(root) == target)
}

/// Makes the default location of the global data directory `global` match `root`: a redirect to
/// `root`, or with `None`, nothing, so that the root can take the default location. It replaces
/// only nothing, an empty directory, or a redirect that [`made_by_command`] answers for, given
/// `setting` and `record`, and reports anything else, which it keeps. A redirect at
/// [`TEMPORARY_REDIRECT`] is removed first.
///
/// On Unix the redirect is a symbolic link made at the temporary name and renamed over the
/// default location, whose directory is flushed. On Windows it is a junction made at the
/// temporary name, placed with a write-through rename once the old one is removed, since a rename
/// can't replace a junction.
pub async fn redirect_to(
    global: &Path,
    root: Option<&Path>,
    setting: Option<&Setting>,
    record: Option<&MoveRecord>,
) -> Result<Redirect, ExternalDirError> {
    let default = global.join(EXTERNAL);
    let temporary = global.join(TEMPORARY_REDIRECT);
    unlink(&temporary)
        .await
        .internal_with(|| format!("removing the temporary redirect {}", temporary.display()))?;
    let examined = default.clone();
    let found = lore_base::lore_spawn_blocking!(move || examine(&examined))
        .await
        .internal("examining the default location of the external root")?
        .internal_with(|| format!("examining {}", default.display()))?;
    let empty_directory = match found {
        Found::Free => false,
        Found::EmptyDirectory => true,
        Found::Redirect(target) => {
            if !made_by_command(&target, setting, record) {
                return Ok(Redirect::Kept(Occupant::OtherRedirect(target)));
            }
            if Some(target.as_path()) == root {
                return Ok(Redirect::Matched);
            }
            false
        }
        Found::Kept(occupant) => return Ok(Redirect::Kept(occupant)),
    };
    let Some(root) = root else {
        remove_durably(global, &default).await?;
        return Ok(Redirect::Matched);
    };
    if empty_directory {
        unlink(&default)
            .await
            .internal_with(|| format!("removing the empty directory {}", default.display()))?;
    }
    place(root, &temporary, &default).await?;
    Ok(Redirect::Matched)
}

/// What the default location at `default` holds, read without following a redirect. Blocking.
fn examine(default: &Path) -> std::io::Result<Found> {
    let metadata = match std::fs::symlink_metadata(default) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Found::Free),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() {
        return std::fs::read_link(default).map(Found::Redirect);
    }
    if !metadata.is_dir() {
        return Ok(Found::Kept(Occupant::Other));
    }
    Ok(match std::fs::read_dir(default)?.next() {
        None => Found::EmptyDirectory,
        Some(_) => Found::Kept(Occupant::Entries),
    })
}

/// Removes the redirect or empty directory at `default`, if any, and flushes the global data
/// directory `global` that holds it.
async fn remove_durably(global: &Path, default: &Path) -> Result<(), ExternalDirError> {
    unlink(default)
        .await
        .internal_with(|| format!("removing {}", default.display()))?;
    let flushed = global.to_path_buf();
    lore_base::lore_spawn_blocking!(move || lore_io::sync_dir(&flushed))
        .await
        .internal("flushing the global data directory")?
        .internal("flushing the global data directory")?;
    Ok(())
}

/// Makes a symbolic link to `root` at `temporary`, and renames it over `default`, replacing a
/// link there, and flushes their directory.
#[cfg(target_family = "unix")]
async fn place(root: &Path, temporary: &Path, default: &Path) -> Result<(), ExternalDirError> {
    let (root, made, placed) = (
        root.to_path_buf(),
        temporary.to_path_buf(),
        default.to_path_buf(),
    );
    lore_base::lore_spawn_blocking!(move || {
        std::os::unix::fs::symlink(&root, &made)?;
        lore_io::rename::rename_durable(&made, &placed)
    })
    .await
    .internal("making the redirect")?
    .internal_with(|| format!("making the redirect at {}", default.display()))?;
    Ok(())
}

/// Makes a junction to `root` at `temporary`, removes the junction or empty directory at
/// `default`, if any, and renames the new one into its place with a write-through rename.
#[cfg(target_family = "windows")]
async fn place(root: &Path, temporary: &Path, default: &Path) -> Result<(), ExternalDirError> {
    let (target, made) = (root.to_path_buf(), temporary.to_path_buf());
    lore_base::lore_spawn_blocking!(move || lore_io::junction::create_junction(&target, &made))
        .await
        .internal("making the redirect")?
        .internal_with(|| format!("making a junction at {}", temporary.display()))?;
    unlink(default)
        .await
        .internal_with(|| format!("removing the junction at {}", default.display()))?;
    let (made, placed) = (temporary.to_path_buf(), default.to_path_buf());
    lore_base::lore_spawn_blocking!(move || lore_io::rename::rename_no_replace(
        &made, &placed, true
    ))
    .await
    .internal("placing the redirect")?
    .internal_with(|| format!("placing the junction at {}", default.display()))?;
    Ok(())
}
