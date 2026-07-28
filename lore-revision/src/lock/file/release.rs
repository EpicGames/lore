// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::auth;
use crate::branch;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lock;
use crate::lock::util::LOCK_BATCH_SIZE;
use crate::lock::util::assemble_resource_for_path;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_trace;
use crate::repository::RepositoryContext;
use crate::state;
use crate::util::path::RelativePath;

#[derive(Clone, Debug)]
pub struct ReleaseOptions {
    pub paths: LoreArray<LoreString>,
    pub branch: String,
    pub owner: String,
    pub owner_id: String,
}

#[error_set]
pub enum ReleaseError {
    Disconnected,
    InvalidArguments,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    NoRemote,
    NotSupported,
    AddressNotFound,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    NodeNotFound,
    Oversized,
    RevisionNotFound,
    WriteRequired,
    NotConnected,
    PayloadNotFound,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for ReleaseError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for an event that marks the start of a lock release report.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileReleaseBeginEventData {
    /// Number of release entries that follow.
    pub count: u64,
    /// Whether no matching lock was found to release.
    pub not_found: u8,
}

/// Data for an event reporting a path whose lock is being released.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileReleaseEventData {
    /// The path whose lock is being released.
    pub path: LoreString,
}

pub async fn release(
    repository: Arc<RepositoryContext>,
    options: ReleaseOptions,
) -> Result<(), ReleaseError> {
    let remote = repository
        .remote()
        .await
        .forward::<ReleaseError>("Unable to release lock while offline")?;

    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<ReleaseError>("Failed to deserialize current revision anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let branch = if options.branch.is_empty() {
        current_branch
    } else {
        let resolved = branch::resolve(repository.clone(), options.branch.as_str())
            .await
            .internal("Invalid branch")?;
        resolved.id
    };

    let owner = if !options.owner_id.is_empty() {
        Some(options.owner_id)
    } else if !options.owner.is_empty() {
        let owner_id = auth::userinfo::user_id(repository.clone(), &options.owner)
            .await
            .internal("Failed to resolve user id from user name")?;

        Some(owner_id)
    } else {
        None
    };

    let mut resources = HashSet::<lock::LockResource>::with_capacity(options.paths.len());
    let force = execution_context().globals().force();
    if !options.paths.is_empty() {
        // When --force flag IS enabled we attempt to release a lock on all paths passed
        // When --force flag ISN'T enabled we attempt to release a lock considering the following
        // a)   If the path is excluded by the filter, discard it from operation
        //      This happens when file was excluded by --view or .urcignore
        // b)   Otherwise we verify the path is a valid node in the repository
        // REMARK: since locks are treated as an atomic operation if anything here fails we abort

        let state = state::State::deserialize(repository.clone(), staged_revision)
            .await
            .forward::<ReleaseError>("Failed to deserialize state")?;

        lore_debug!("Inspecting {} path(s)", options.paths.len());
        for path in options.paths.as_slice().iter() {
            let relative_path = RelativePath::new_from_user_path(
                repository.require_path()?,
                path.as_str(),
            )
            .forward_with::<ReleaseError, _>(|| format!("Invalid path: {}", path.as_str()))?;
            if !force {
                if repository
                    .filter
                    .emit_excludes(&relative_path, true, FilterMode::Full)
                {
                    lore_trace!("Path excluded by filter: {}", relative_path.as_str());
                    continue;
                }

                let node_link = state
                    .find_node_link(repository.clone(), relative_path.as_str())
                    .await
                    .unwrap_or_default();

                if !node_link.is_valid() {
                    lore_error!(
                        "Path not found in repository. Use --force if file was deleted while being locked."
                    );
                    return Err(ReleaseError::internal(format!(
                        "Invalid path: {}",
                        path.as_str()
                    )));
                }
            }

            let resource = assemble_resource_for_path(relative_path.as_str(), branch);
            resources.insert(resource);
        }
    } else if force {
        // If there are no paths and --force flag IS enabled we attempt to release all locks for
        // i) the current branch or the branch passed in by the --branch option
        // ii) the current user or the user passed in by the --owner option

        let response = remote
            .lock(repository.id)
            .await
            .forward_with::<ReleaseError, _>(|| {
                format!("Failed to connect to remote {}", remote.remote_url())
            })?
            .query(Some(branch), owner.as_deref(), None)
            .await
            .forward::<ReleaseError>("Failed to query the locks")?;

        for lock in response.iter() {
            let relative_path = &lock.resource.description;
            let resource = assemble_resource_for_path(relative_path.as_str(), branch);
            resources.insert(resource);
        }
    }

    if resources.is_empty() {
        lore_debug!("No paths to release lock on");
        return Ok(());
    }

    // We cannot know which locks are going to be released without contacting the server, so every path is reported as a would-be release.
    if execution_context().globals().dry_run() {
        let mut paths = resources
            .iter()
            .map(|resource| resource.description.clone())
            .collect::<Vec<_>>();
        paths.sort();

        event::LoreEvent::LockFileReleaseBegin(LoreLockFileReleaseBeginEventData {
            count: paths.len() as u64,
            not_found: 0,
        })
        .send();

        for path in paths {
            event::LoreEvent::LockFileRelease(LoreLockFileReleaseEventData {
                path: LoreString::from(&path),
            })
            .send();
        }

        return Ok(());
    }

    lore_debug!("Unlocking {} path(s)", resources.len());

    let resources_count = resources.len();
    let resources_values = Vec::from_iter(resources);
    let batch_iterator = resources_values.chunks(LOCK_BATCH_SIZE);
    let num_batches = batch_iterator.len();

    let mut batches: JoinSet<Result<Vec<lock::LockResource>, ReleaseError>> = JoinSet::new();
    let mut batches_results = Vec::with_capacity(num_batches);
    for batch_resources in batch_iterator {
        let batch_resources = batch_resources.to_vec();
        let remote = remote.clone();
        let repository_id = repository.id;
        lore_spawn!(batches, async move {
            let response = remote
                .lock(repository_id)
                .await
                .forward_with::<ReleaseError, _>(|| {
                    format!("Failed to connect to remote {}", remote.remote_url())
                })?
                .unlock(&batch_resources)
                .await
                .forward::<ReleaseError>("Failed to release the lock")?;

            Ok(response)
        });
    }

    let mut task_error: Result<(), ReleaseError> = Ok(());
    while let Some(task_result) = batches.join_next().await {
        if let Ok(result) = task_result {
            batches_results.push(result);
        } else {
            task_error = Err(ReleaseError::internal("Failed executing batch task"));
        }
    }
    task_error?;

    let mut unlocks = Vec::with_capacity(resources_count);

    let mut num_batch_success = 0;
    let mut num_batch_failed = 0;
    for batch_result in batches_results {
        if let Ok(mut results) = batch_result {
            unlocks.append(&mut results);
            num_batch_success += 1;
        } else {
            num_batch_failed += 1;
        }
    }

    if num_batch_failed > 0 {
        lore_error!("Failed to lock-release {num_batch_failed} batch(es) out of {num_batches}");
    }

    if num_batch_success == 0 {
        return Err(ReleaseError::internal("Failed to release the lock"));
    }

    if unlocks.is_empty() {
        event::LoreEvent::LockFileReleaseBegin(LoreLockFileReleaseBeginEventData {
            count: 0,
            not_found: 1,
        })
        .send();
    } else {
        unlocks
            .sort_by(|resource_a, resource_b| resource_a.description.cmp(&resource_b.description));

        // Generate structured output for locks successfully released
        lore_debug!("Unlocked {} path(s)", unlocks.len());
        event::LoreEvent::LockFileReleaseBegin(LoreLockFileReleaseBeginEventData {
            count: unlocks.len() as u64,
            not_found: 0,
        })
        .send();
        for unlock in unlocks.iter() {
            event::LoreEvent::LockFileRelease(LoreLockFileReleaseEventData {
                path: LoreString::from(&unlock.description),
            })
            .send();
        }
    }

    Ok(())
}
