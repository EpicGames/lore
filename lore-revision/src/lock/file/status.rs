// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::types::LockData;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

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
use crate::lore_debug;
use crate::lore_error;
use crate::lore_trace;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::runtime::execution_context;
use crate::state;
use crate::util::path::RelativePath;

#[derive(Clone, Debug)]
pub struct StatusOptions {
    pub paths: LoreArray<LoreString>,
    pub branch: String,
}

#[error_set]
pub enum StatusError {
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
    NotConnected,
    Oversized,
    PayloadNotFound,
    RevisionNotFound,
    WriteRequired,
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

impl EventError for StatusError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for an event that marks the start of a lock status report.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileStatusBeginEventData {
    /// Number of status entries that follow.
    pub count: u64,
}

/// Data for an event reporting the lock status of a single path.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileStatusEventData {
    /// Path the status applies to.
    pub path: LoreString,
    /// Identifier of the user that holds the lock.
    pub owner: LoreString,
    /// Timestamp recorded when the lock was acquired.
    pub locked_at: u64,
}

pub async fn status(
    repository: Arc<RepositoryContext>,
    options: StatusOptions,
) -> Result<(), StatusError> {
    let remote = repository
        .remote()
        .await
        .forward::<StatusError>("Unable to check lock status while offline")?;

    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<StatusError>("Failed to deserialize current revision anchor")?;
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
            .forward::<StatusError>("Invalid branch")?;
        resolved.id
    };

    let mut resources = HashSet::<lock::LockResource>::with_capacity(options.paths.len());
    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<StatusError>("Failed to deserialize state")?;

    lore_debug!("Inspecting {} path(s)", options.paths.len());
    let force = execution_context().globals().force();
    for path in options.paths.as_slice().iter() {
        let relative_path =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
                .forward_with::<StatusError, _>(|| format!("Invalid path: {}", path.as_str()))?;

        if !force
            && repository
                .filter
                .emit_excludes(&relative_path, true, FilterMode::Full)
        {
            lore_trace!("Path excluded by filter: {}", relative_path.as_str());
            continue;
        }

        if let Ok(node_link) = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
        {
            if !node_link.is_valid() {
                emit_path_ignore(path.as_str()).await;
                lore_trace!("Ignoring invalid path: {path}");
                continue;
            }
        } else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path, not found in repository: {path}");
            continue;
        };

        let resource = assemble_resource_for_path(relative_path.as_str(), branch);
        resources.insert(resource);
    }

    let resources_count = resources.len();
    let resources_values = Vec::from_iter(resources);
    let batch_iterator = resources_values.chunks(LOCK_BATCH_SIZE);
    let num_batches = batch_iterator.len();

    let mut batches: JoinSet<Result<Vec<LockData>, StatusError>> = JoinSet::new();
    let mut batches_results = Vec::with_capacity(num_batches);
    for batch_resources in batch_iterator {
        let batch_resources = batch_resources.to_vec();
        let remote = remote.clone();
        let repository_id = repository.id;
        lore_spawn!(batches, async move {
            let response = remote
                .lock(repository_id)
                .await
                .forward_with::<StatusError, _>(|| {
                    format!("Failed to connect to remote {}", remote.remote_url())
                })?
                .status(&Vec::from_iter(batch_resources))
                .await
                .forward::<StatusError>("Failed to fetch lock status")?;

            Ok(response)
        });
    }

    let mut task_error: Result<(), StatusError> = Ok(());
    while let Some(task_result) = batches.join_next().await {
        if let Ok(result) = task_result {
            batches_results.push(result);
        } else {
            task_error = Err(StatusError::internal("Failed executing batch task"));
        }
    }
    task_error?;

    let mut locks = Vec::with_capacity(resources_count);

    let mut num_batch_success = 0;
    let mut num_batch_failed = 0;
    for batch_result in batches_results {
        if let Ok(mut results) = batch_result {
            locks.append(&mut results);
            num_batch_success += 1;
        } else {
            num_batch_failed += 1;
        }
    }

    if num_batch_failed > 0 {
        lore_error!("Failed to status {num_batch_failed} batch(es) out of {num_batches}");
    }

    if num_batch_success != num_batches {
        return Err(StatusError::internal("Failed to fetch lock status"));
    }

    locks.sort_by(|lock_a, lock_b| {
        lock_a
            .resource
            .description
            .cmp(&lock_b.resource.description)
    });

    event::LoreEvent::LockFileStatusBegin(LoreLockFileStatusBeginEventData {
        count: locks.len() as u64,
    })
    .send();

    lore_debug!("Received {} path(s)", locks.len());
    for lock in locks {
        let path = &lock.resource.description;

        event::LoreEvent::LockFileStatus(LoreLockFileStatusEventData {
            path: path.into(),
            owner: LoreString::from(&lock.owner),
            locked_at: lock.locked_at,
        })
        .send();
    }

    Ok(())
}
