// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The external root: where every virtual instance's external directory lives, and the files of
//! the global data directory that name it.

pub mod access;
pub mod check;
pub mod leftovers;
pub mod locks;
pub mod marker;
pub mod mounts;
pub mod moving;
pub mod record;
pub mod recovery;
pub mod redirect;
pub mod resolve;
pub mod setting;
pub mod takeover;

use std::path::Path;
use std::path::PathBuf;

use lore_error_set::prelude::*;

use self::resolve::Availability;
use self::resolve::DefaultLocation;
use self::resolve::RootState;
use self::setting::Setting;
use crate::global::get_global_data_dir;
use crate::instance::InstanceId;

#[error_set]
pub enum ExternalDirError {}

/// The default location of the external root, in the global data directory.
pub const EXTERNAL: &str = "external";
/// The directory in an instance's external directory holding what its mount wrote.
pub const WRITE_DIR: &str = "write";

/// The external root `state` found available, or an error naming why it can't be used.
pub fn external_dirs_root(state: &RootState) -> Result<&Path, ExternalDirError> {
    match &state.availability {
        Availability::Available(root) => Ok(root),
        Availability::Unavailable(reason) => Err(ExternalDirError::internal(format!(
            "the external root isn't available: {}",
            reason.describe()
        ))),
        Availability::Interrupted => Err(ExternalDirError::internal(
            "the external root isn't available: a move of it was interrupted",
        )),
    }
}

/// The external directory of `instance_id` in the root `state` found available.
pub fn external_dir_for_instance(
    state: &RootState,
    instance_id: InstanceId,
) -> Result<PathBuf, ExternalDirError> {
    Ok(external_dirs_root(state)?.join(Path::new(instance_id.text_encoding().as_str())))
}

/// Creates the external directory of `instance_id` in the root that `state` found available, and
/// answers its path. Only a missing default location with no setting is made with it. A missing
/// configured root fails, so a root on a volume that isn't mounted is never made on the volume
/// underneath. Blocking.
pub fn create_external_dir(
    state: &RootState,
    instance_id: InstanceId,
) -> Result<PathBuf, ExternalDirError> {
    let dir = external_dir_for_instance(state, instance_id)?;
    let created = if state.default == DefaultLocation::Missing
        && dir.parent() == Some(state.global.join(EXTERNAL).as_path())
    {
        std::fs::create_dir_all(&dir)
    } else {
        lore_io::create_dir_in_existing_parent(&dir)
    };
    created.internal_with(|| format!("creating the external directory {}", dir.display()))?;
    Ok(dir)
}

/// The path a status reports for the root `state` resolved: the available root, else the setting's
/// path, else the default location, and empty where the setting can't be read. Blocking.
pub fn reported_root(state: &RootState) -> PathBuf {
    if let Availability::Available(root) = &state.availability {
        return root.clone();
    }
    match Setting::load(&state.global) {
        Ok(Some(setting)) if !setting.path.is_empty() => PathBuf::from(setting.path),
        Ok(_) => state.global.join(EXTERNAL),
        Err(_) => PathBuf::new(),
    }
}

/// The refusal of a target or a form of the command for `reason`, as invalid arguments.
pub fn refused<E: From<crate::errors::InvalidArguments>>(reason: String) -> E {
    E::from(crate::errors::InvalidArguments { reason })
}

/// The canonical path of the global data directory `global`, as a mount records it.
pub fn canonical_global(global: &Path) -> Result<std::sync::Arc<Path>, ExternalDirError> {
    let canonical =
        std::fs::canonicalize(global).internal("canonicalizing the global data directory")?;
    Ok(std::sync::Arc::from(canonical))
}

/// [`canonical_global`] of the caller's global data directory.
pub fn canonical_global_data_dir() -> Result<std::sync::Arc<Path>, ExternalDirError> {
    let global =
        get_global_data_dir().forward::<ExternalDirError>("Missing global data directory")?;
    canonical_global(&global)
}

/// The error of a command that needs the external directory of `instance_id`, whose lookup is
/// `unknown`.
pub fn unknown_external_dir(
    instance_id: InstanceId,
    unknown: resolve::Unknown,
) -> ExternalDirError {
    ExternalDirError::internal(format!("instance {instance_id} can't be used: {unknown}"))
}
