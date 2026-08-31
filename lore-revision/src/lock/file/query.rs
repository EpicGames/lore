// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth;
use crate::branch;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_trace;
use crate::repository::RepositoryContext;
use crate::util::path::RelativePath;

#[derive(Clone, Debug)]
pub struct QueryOptions {
    pub branch: String,
    pub owner: String,
    pub path: String,
}

#[error_set]
pub enum QueryError {
    Disconnected,
    InvalidArguments,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    NoRemote,
    NotSupported,
    InvalidPath,
    Oversized,
}

impl EventError for QueryError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for an event that marks the start of a lock query result.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileQueryBeginEventData {
    /// Number of query entries that follow.
    pub count: u64,
}

/// Data for an event reporting a single lock matched by a query.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockFileQueryEventData {
    /// Identifier of the branch the lock belongs to.
    pub branch: BranchId,
    /// Path the lock applies to.
    pub path: LoreString,
    /// Identifier of the user that holds the lock.
    pub owner: LoreString,
    /// Timestamp recorded when the lock was acquired.
    pub locked_at: u64,
}

pub async fn query(
    repository: Arc<RepositoryContext>,
    options: QueryOptions,
) -> Result<(), QueryError> {
    let remote = repository
        .remote()
        .await
        .forward::<QueryError>("Unable to check lock status while offline")?;

    let branch = if options.branch.is_empty() {
        None
    } else {
        let resolved = branch::resolve(repository.clone(), options.branch.as_str())
            .await
            .forward_any::<QueryError>("Invalid branch")?;
        Some(resolved.id)
    };

    let owner = if options.owner.is_empty() {
        None
    } else {
        let owner_id = auth::userinfo::user_id(repository.clone(), &options.owner)
            .await
            .forward::<QueryError>("Failed to resolve user id from user name")?;

        Some(owner_id)
    };

    let relative_path = if options.path.is_empty() {
        None
    } else {
        let relative_path =
            RelativePath::new_from_user_path(repository.require_path()?, options.path.as_str())
                .forward_with::<QueryError, _>(|| format!("Invalid path: {}", options.path))?;

        if !execution_context().globals().force()
            && repository
                .filter
                .emit_excludes(&relative_path, true, FilterMode::Full)
        {
            lore_trace!("Path excluded by filter: {}", relative_path.as_str());
            return Ok(());
        }
        Some(relative_path.to_string())
    };

    let mut response = remote
        .lock(repository.id)
        .await
        .forward_with::<QueryError, _>(|| {
            format!("Failed to connect to remote {}", remote.remote_url())
        })?
        .query(branch, owner.as_deref(), relative_path.as_deref())
        .await
        .forward::<QueryError>("Failed to query lock status")?;

    response.sort_by(|lock_a, lock_b| {
        lock_a
            .resource
            .description
            .cmp(&lock_b.resource.description)
    });

    event::LoreEvent::LockFileQueryBegin(LoreLockFileQueryBeginEventData {
        count: response.len() as u64,
    })
    .send();

    lore_debug!("Received {} path(s)", response.len());
    for lock in response.iter() {
        let path = &lock.resource.description;
        let branch = lock.resource.branch;

        event::LoreEvent::LockFileQuery(LoreLockFileQueryEventData {
            path: path.into(),
            owner: LoreString::from(&lock.owner),
            branch,
            locked_at: lock.locked_at,
        })
        .send();
    }

    Ok(())
}
