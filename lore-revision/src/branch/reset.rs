// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::error::LoreResultExt;
use crate::errors::*;
use crate::event::EventError;
use crate::event::LoreEvent;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::revision::sync;
use crate::state;

#[error_set]
pub enum ResetError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    NoRemote,
    LocalModifications,
    NotAuthenticated,
    NotAuthorized,
    SlowDown,
    Maintenance,
    InvalidArguments,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NotConnected,
    NothingStaged,
    NotSupported,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for ResetError {
    fn translated(&self) -> LoreError {
        match self {
            ResetError::Disconnected(_) => LoreError::Connection,
            ResetError::SlowDown(_) => LoreError::SlowDown,
            ResetError::Oversized(_) => LoreError::Oversized,
            ResetError::FileNotFound(_) => LoreError::FileNotFound,
            ResetError::NotFound(_)
            | ResetError::BranchNotFound(_)
            | ResetError::RevisionNotFound(_) => LoreError::NotFound,
            ResetError::AddressNotFound(_) => LoreError::AddressNotFound,
            ResetError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            ResetError::InvalidPath(_) | ResetError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Event data reported when a branch is reset to a revision.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchResetEventData {
    /// Branch identifier.
    pub id: Context,
    /// Branch name.
    pub name: LoreString,
    /// Revision the branch was reset to.
    pub revision: Hash,
}

pub async fn reset(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: String,
    revision: String,
) -> Result<(), ResetError> {
    let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<ResetError>("loading current anchor")?;

    // Reject if any node is actually staged; dirty-only tracking is
    // tolerated and forwarded by `sync::sync` → `rebase_staged_anchor`
    // when the same-branch reset path runs below.
    if let Some(staged_revision) = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        && !staged_revision.is_zero()
    {
        let state_staged = state::State::deserialize(repository.clone(), staged_revision)
            .await
            .forward::<ResetError>("deserializing staged state")?;
        let has_staged = state_staged
            .node_has_staged_children(repository.clone(), crate::node::ROOT_NODE)
            .await
            .forward::<ResetError>("checking staged nodes")?;
        if has_staged {
            return Err(ResetError::internal(
                "Unable to reset branch when there is a staged state",
            ));
        }
    }

    let branch = if branch.is_empty() {
        current_branch.to_string()
    } else {
        branch
    };

    let branch = branch::resolve(repository.clone(), branch.as_str())
        .await
        .emit_map_err(ResetError::from(BranchNotFound {
            branch: branch.clone(),
        }))?;

    let branch_metadata = branch::metadata(repository.clone(), branch.id)
        .await
        .forward::<ResetError>("loading branch metadata")?;
    let branch_name = branch::name(&branch_metadata).unwrap_or_default();

    let branch_stack = branch::stack(&branch_metadata);
    let branch_point = if branch_stack.is_empty() {
        Hash::default()
    } else {
        branch_stack[0].revision
    };

    let revision = revision::resolve(
        repository.clone(),
        revision.as_str(),
        execution_context().globals().search_limit(),
        execution_context().globals().search_location(),
    )
    .await
    .forward::<ResetError>("resolving revision")?;

    let state = state::State::deserialize(repository.clone(), revision)
        .await
        .forward::<ResetError>("deserializing revision state")?;
    if state.branch(repository.clone()).await != branch.id && revision != branch_point {
        return Err(ResetError::internal(
            "Given revision is not on the same branch",
        ));
    }

    let mapped = branch::load_name_to_id_local(repository.clone(), branch_name)
        .await
        .unwrap_or_default();
    if mapped != Context::default() && mapped != branch.id {
        return Err(ResetError::internal(
            "Given branch's name is already used by another branch",
        ));
    }
    if mapped == Context::default() {
        // Restore the name-to-id mapping for the branch to ensure it shows up in the local branch list.
        branch::store_name_to_id(repository.clone(), branch.id, &branch_name)
            .await
            .forward::<ResetError>("restoring name-to-id mapping")?;
    }

    let latest = branch::load_latest(repository.clone(), branch.id)
        .await
        .unwrap_or_default();
    if latest == revision {
        LoreEvent::BranchReset(LoreBranchResetEventData {
            id: branch.id,
            name: branch_name.into(),
            revision,
        })
        .send();
        return Ok(());
    }

    if current_branch == branch.id {
        let options = sync::SyncOptions {
            revision: Some(revision.to_string()),
            ..Default::default()
        };
        Box::pin(sync::sync(repository.clone(), token, options))
            .await
            .forward::<ResetError>("syncing to given revision")?;

        crate::instance::store_current_anchor(&repository, revision)
            .await
            .forward::<ResetError>("storing current anchor")?;
    }

    // We don't know if the revision is on the history line of the remote branch, set local flag
    // to force the next sync to do divergence check. Also remove the last sync cache.
    branch::store_latest(
        repository.clone(),
        branch.id,
        revision,
        BranchLatestStatus::Divergent,
    )
    .await
    .forward::<ResetError>("storing new latest revision for branch")?;

    branch::store_last_sync(repository.clone(), branch.id, Hash::default()).await;

    LoreEvent::BranchReset(LoreBranchResetEventData {
        id: branch.id,
        name: branch_name.into(),
        revision,
    })
    .send();
    Ok(())
}
