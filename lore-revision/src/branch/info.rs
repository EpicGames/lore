// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::errors::*;
use crate::event::EventError;
use crate::event::LoreEvent;
use crate::interface::LoreArray;
use crate::interface::LoreBranchPoint;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::runtime::execution_context;
use crate::util::serde::u8_as_bool;

#[error_set]
pub enum InfoError {
    NotFound,
    AddressNotFound,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Disconnected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    InvalidArguments,
    InvalidNodeHierarchy,
    InvalidPath,
    LayerNotFound,
    LinkNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    Maintenance,
    MaxHistorySearchDepth,
    NodeNotFound,
    NoRemote,
    NotALayer,
    NotALink,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NothingStaged,
    NotSupported,
    Oversized,
    PayloadNotFound,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    RevisionNotFound,
    SharedStoreNotFound,
    SlowDown,
    TokenNotFound,
    WriteRequired,
    MissingIdentity,
}

impl EventError for InfoError {
    fn translated(&self) -> LoreError {
        match self {
            Self::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Event data reported with information about a single branch.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchInfoEventData {
    /// Branch identifier.
    pub id: BranchId,
    /// Branch name.
    pub name: LoreString,
    /// Branch category.
    pub category: LoreString,
    /// Latest revision known locally for the branch.
    pub latest: Hash,
    /// Latest revision known on the remote for the branch.
    pub latest_remote: Hash,
    /// Identifier of the parent branch.
    pub parent: BranchId,
    /// Revision on the parent branch where this branch was created.
    pub branch_point: Hash,
    /// Identifier of the user who created the branch.
    pub creator: LoreString,
    /// Creation time of the branch as a timestamp.
    pub created: u64,
    /// Stack of branch points this branch was created from.
    pub stack: LoreArray<LoreBranchPoint>,
    /// Set when the branch has been archived.
    #[serde(with = "u8_as_bool")]
    pub archived: u8,
}

pub async fn info(repository: Arc<RepositoryContext>, branch: String) -> Result<(), InfoError> {
    let branch_name = if branch.is_empty() {
        let (_revision, branch) =
            crate::instance::load_current_anchor(&repository)
                .await
                .forward::<InfoError>("Failed to deserialize current revision anchor")?;
        branch.to_string()
    } else {
        branch
    };

    let branch_resolved = branch::resolve(repository.clone(), branch_name.as_str())
        .await
        .map_matched_err("Nothing known about branch", |m| match m {
            branch::MatchedBranchError::BranchNotFound(_) => InfoError::from(NotFound),
            other => other.forward::<InfoError>("resolving branch info"),
        })?;

    let branch_local = if branch_resolved.local {
        Some(branch_resolved.clone())
    } else {
        None
    };

    let branch_remote = if !branch_resolved.local {
        Some(branch_resolved.clone())
    } else if execution_context().globals().local() {
        None
    } else if let Ok(remote) = repository.remote().await {
        branch::load_remote(remote.clone(), repository.id, branch_resolved.id)
            .await
            .ok()
    } else {
        None
    };

    let metadata = match (branch_local.as_ref(), branch_remote.as_ref()) {
        (Some(branch_local), Some(branch_remote)) => {
            let metadata_local =
                branch::load_metadata(repository.clone(), branch_local.metadata).await;
            let metadata_remote = branch::load_metadata(repository.clone(), branch_remote.metadata)
                .await
                .forward::<InfoError>("Failed to load branch metadata")?;
            if let Ok(metadata_local) = metadata_local
                && branch::created(&metadata_local) >= branch::created(&metadata_remote)
            {
                lore_debug!("Using local branch metadata for info");
                metadata_local
            } else {
                lore_debug!("Using remote branch metadata for info");
                metadata_remote
            }
        }
        (Some(branch), None) => {
            lore_debug!("Using local branch metadata for info");
            branch::load_metadata(repository.clone(), branch.metadata)
                .await
                .forward::<InfoError>("Failed to load branch metadata")?
        }
        (None, Some(branch)) => {
            lore_debug!("Using remote branch metadata for info");
            branch::load_metadata(repository.clone(), branch.metadata)
                .await
                .forward::<InfoError>("Failed to load branch metadata")?
        }
        (None, None) => {
            return Err(NotFound.into());
        }
    };

    let stack = branch::stack(&metadata);
    let branch_point = stack.first().cloned().unwrap_or_default();

    // Resolve creator name
    let creator = branch::creator(&metadata).unwrap_or_default();

    let metadata_name = branch::name(&metadata).ok();

    let category = branch::category(&metadata)
        .map_or(branch::default_category().to_string(), |name| {
            name.to_string()
        });

    let archived = branch_local
        .as_ref()
        .or(branch_remote.as_ref())
        .is_some_and(|b| b.deleted);

    let name = metadata_name.map_or_else(|| branch_resolved.id.to_string(), str::to_string);

    LoreEvent::BranchInfo(LoreBranchInfoEventData {
        id: branch_resolved.id,
        name: name.into(),
        category: category.into(),
        latest: branch_local.map(|branch| branch.latest).unwrap_or_default(),
        latest_remote: branch_remote
            .map(|branch| branch.latest)
            .unwrap_or_default(),
        parent: branch_point.branch,
        branch_point: branch_point.revision,
        creator: creator.into(),
        created: branch::created(&metadata),
        stack: LoreArray::from_vec(stack.iter().map(LoreBranchPoint::from).collect()),
        archived: archived as u8,
    })
    .send();

    Ok(())
}
