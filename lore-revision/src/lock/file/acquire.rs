// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
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
use crate::lock::file::release::ReleaseOptions;
use crate::lock::file::release::release;
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
pub struct AcquireOptions {
    pub paths: LoreArray<LoreString>,
    pub branch: String,
    pub owner: String,
}

#[error_set]
pub enum AcquireError {
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

impl EventError for AcquireError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for an event that marks the start of a lock acquire report.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileAcquireBeginEventData {
    /// Number of acquire entries that follow.
    pub count: u64,
    /// Whether the entries that follow were already owned.
    pub ignored: u8,
}

/// Data for an event reporting a path whose lock is being acquired.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileAcquireEventData {
    /// The path whose lock is being acquired.
    pub path: LoreString,
}

pub async fn acquire(
    repository: Arc<RepositoryContext>,
    options: AcquireOptions,
) -> Result<(), AcquireError> {
    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<AcquireError>("Failed to deserialize current revision anchor")?;
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

    let owner = if options.owner.is_empty() {
        None
    } else {
        Some(options.owner)
    };

    let mut resources = HashMap::<String, lock::LockResource>::with_capacity(options.paths.len());
    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<AcquireError>("Failed to deserialize state")?;

    lore_debug!("Inspecting {} path(s)", options.paths.len());
    let force = execution_context().globals().force();
    for path in options.paths.as_slice().iter() {
        let relative_path =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
                .forward_with::<AcquireError, _>(|| format!("Invalid path: {}", path.as_str()))?;

        if !force
            && repository
                .filter
                .emit_excludes(&relative_path, true, FilterMode::Full)
        {
            lore_trace!("Path excluded by filter: {}", relative_path.as_str());
            continue;
        }

        let node_link = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .forward_with::<AcquireError, _>(|| format!("Invalid path: {}", path.as_str()))?;
        if !node_link.is_valid() {
            return Err(AcquireError::internal(format!(
                "Invalid path: {}",
                path.as_str()
            )));
        }

        let resource = assemble_resource_for_path(relative_path.as_str(), branch);
        resources.insert(relative_path.to_string(), resource);
    }

    if resources.is_empty() {
        lore_debug!("No paths to acquire lock on");
        return Ok(());
    }

    // We cannot know which locks are going to be acquired or which ones are owned without contacting the server, so every path is reported as a would-be acquisition.
    if execution_context().globals().dry_run() {
        let mut paths = resources.keys().cloned().collect::<Vec<_>>();
        paths.sort();

        event::LoreEvent::LockFileAcquireBegin(LoreLockFileAcquireBeginEventData {
            count: paths.len() as u64,
            ignored: 0,
        })
        .send();

        for path in paths {
            event::LoreEvent::LockFileAcquire(LoreLockFileAcquireEventData { path: path.into() })
                .send();
        }

        return Ok(());
    }

    let remote = repository
        .remote()
        .await
        .forward::<AcquireError>("Unable to acquire lock while offline")?;

    let resources_count = resources.len();
    let resources_values = resources.values().cloned().collect::<Vec<_>>();
    let batch_iterator = resources_values.chunks(LOCK_BATCH_SIZE);
    let num_batches = batch_iterator.len();

    let mut batches: JoinSet<Result<Vec<LockData>, AcquireError>> = JoinSet::new();
    let mut batches_results = Vec::with_capacity(num_batches);
    for batch_resources in batch_iterator {
        let batch_resources = batch_resources.to_vec();
        let owner = owner.clone();
        let remote = remote.clone();
        let repository_id = repository.id;
        lore_spawn!(batches, async move {
            let response = remote
                .lock(repository_id)
                .await
                .forward_with::<AcquireError, _>(|| {
                    format!("Failed to connect to remote {}", remote.remote_url())
                })?
                .lock(&batch_resources, owner.as_deref())
                .await
                .forward::<AcquireError>("Failed to acquire the lock")?;

            Ok(response)
        });
    }

    let mut task_error: Result<(), AcquireError> = Ok(());
    while let Some(task_result) = batches.join_next().await {
        if let Ok(result) = task_result {
            batches_results.push(result);
        } else {
            task_error = Err(AcquireError::internal("Failed executing batch task"));
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
        lore_error!("Failed to lock-acquire {num_batch_failed} batch(es) out of {num_batches}");
    }

    if num_batch_success == 0 {
        return Err(AcquireError::internal("Failed to acquire the lock"));
    }

    if num_batch_success > 0 && num_batch_success < num_batches {
        lore_debug!("Attempting releasing partial acquired locks.");

        let options = ReleaseOptions {
            paths: options.paths,
            branch: options.branch,
            owner: String::default(),
            owner_id: String::default(),
        };

        release(repository.clone(), options)
            .await
            .forward::<AcquireError>("Failed to acquire the lock")?;

        return Err(AcquireError::internal("Failed to acquire the lock"));
    }

    locks.sort_by(|lock_a, lock_b| {
        lock_a
            .resource
            .description
            .cmp(&lock_b.resource.description)
    });

    // Generate structured output for locks successfully acquired
    lore_debug!("Locked {} path(s)", locks.len());
    if !locks.is_empty() {
        event::LoreEvent::LockFileAcquireBegin(LoreLockFileAcquireBeginEventData {
            count: locks.len() as u64,
            ignored: 0,
        })
        .send();
    }
    for lock in locks {
        let path = lock.resource.description;

        // From the requested paths, remove those successfully locked
        resources.remove(&path);

        event::LoreEvent::LockFileAcquire(LoreLockFileAcquireEventData { path: path.into() })
            .send();
    }

    // Generate structured output for locks already owned by the user.
    if !resources.is_empty() {
        event::LoreEvent::LockFileAcquireBegin(LoreLockFileAcquireBeginEventData {
            count: resources.len() as u64,
            ignored: 1,
        })
        .send();
    }
    for (key, _) in resources {
        event::LoreEvent::LockFileAcquire(LoreLockFileAcquireEventData { path: key.into() }).send();
    }

    Ok(())
}
