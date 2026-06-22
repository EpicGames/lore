// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::lore::Address;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::stage::StageStats;
use crate::stage::stage_delete;
use crate::state;
use crate::store::StoreObliterateStats;
use crate::util;
use crate::util::path::RelativePath;

/// Data for the event emitted when file content is obliterated.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileObliterateEventData {
    /// Address of the obliterated content.
    pub address: Address,
    /// Number of fragments removed.
    pub num_fragments: usize,
    /// Number of payloads removed.
    pub num_payloads: usize,
}

#[error_set]
pub enum ObliterateError {
    InvalidArguments,
    InvalidPath,
    InvalidAddress,
    FileNotFound,
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
    IdenticalMetadata,
    InvalidNodeHierarchy,
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
    NotFound,
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

impl EventError for ObliterateError {
    fn translated(&self) -> LoreError {
        match self {
            Self::InvalidArguments(_) | Self::InvalidPath(_) | Self::InvalidAddress(_) => {
                LoreError::InvalidArguments
            }
            Self::FileNotFound(_) => LoreError::FileNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

pub async fn obliterate_file(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    path: String,
) -> Result<(), ObliterateError> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        .forward::<ObliterateError>("resolving user path")?;

    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<ObliterateError>("Failed to deserialize current revision anchor")?;

    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<ObliterateError>("Failed to deserialize state")?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_err(|_err| {
            ObliterateError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;

    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: relative_path.to_string(),
        }
        .into());
    }

    let node = state
        .node(repository.clone(), node_link.node)
        .await
        .map_err(|_err| {
            ObliterateError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;

    if !node.is_file() {
        return Err(InvalidPath {
            path: relative_path.to_string(),
        }
        .into());
    }

    obliterate_address(repository.clone(), node.address).await?;

    util::fs::unlink_recursive(relative_path.to_absolute_path(repository.require_path()?))
        .await
        .internal("Failed to unlink filesystem entry")?;

    let stage_stats = Arc::new(StageStats::default());

    stage_delete(
        repository.clone(),
        state.clone(),
        node_link.node,
        NodeFlags::NoFlags,
        stage_stats,
        None, // TODO(vri): UCS-18010 - Implement obliterate for links
    )
    .await
    .forward::<ObliterateError>("Failed to stage deleted path")?;

    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<ObliterateError>("Failed to serialize staged obliterate state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<ObliterateError>("Failed to serialize revision anchor")?;

    Ok(())
}

pub async fn obliterate_address(
    repository: Arc<RepositoryContext>,
    address: Address,
) -> Result<(), ObliterateError> {
    let stats = Arc::new(StoreObliterateStats::default());

    repository
        .immutable_store()
        .obliterate(repository.id, address, stats.clone())
        .await
        .forward::<ObliterateError>(&format!("Failed to obliterate an address: {address}"))?;

    if let Ok(remote) = repository.remote().await
        && let Ok(admin) = remote.admin(repository.id).await
    {
        admin
            .obliterate(address)
            .await
            .forward::<ObliterateError>(&format!(
                "Failed to obliterate a remote address: {address}"
            ))?;
    }

    event::LoreEvent::FileObliterate(LoreFileObliterateEventData {
        address,
        num_fragments: stats.num_fragments.load(Ordering::Relaxed),
        num_payloads: stats.num_payloads.load(Ordering::Relaxed),
    })
    .send();

    Ok(())
}
