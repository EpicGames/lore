// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::lore::Address;
use crate::repository::RepositoryContext;
use crate::state;
use crate::util::path::RelativePath;

/// Data for the event reporting the stored representation of file content.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDumpEventData {
    /// Address of the content.
    pub address: Address,
    /// Flags describing the stored content.
    pub flags: u32,
    /// Size of the stored payload in bytes.
    pub size_payload: u32,
    /// Size of the content in bytes.
    pub size_content: u64,
    /// Set when a matching stored object was found.
    pub match_made: u8,
}

#[error_set]
pub enum DumpError {
    AddressNotFound,
    Disconnected,
    InvalidArguments,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    Maintenance,
    NodeNotFound,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    RevisionNotFound,
    SlowDown,
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
    FileNotFound,
    MissingIdentity,
}

impl crate::event::EventError for DumpError {}

pub async fn dump_file(repository: Arc<RepositoryContext>, path: String) -> Result<(), DumpError> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        .forward::<DumpError>("invalid path")?;

    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<DumpError>("deserializing current anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<DumpError>("deserializing state")?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .forward::<DumpError>("invalid node")?;

    if !node_link.is_valid() {
        return Err(DumpError::internal("invalid node"));
    }

    let node = state
        .node(repository.clone(), node_link.node)
        .await
        .forward::<DumpError>("invalid node")?;

    dump_address(repository, node.address).await?;

    Ok(())
}

pub async fn dump_address(
    repository: Arc<RepositoryContext>,
    address: Address,
) -> Result<(), DumpError> {
    let result = repository
        .immutable_store()
        .get_metadata(repository.id, address)
        .await
        .forward::<DumpError>("querying fragment from immutable store")?;

    event::LoreEvent::FileDump(LoreFileDumpEventData {
        address,
        flags: result.fragment.flags,
        size_payload: result.fragment.size_payload,
        size_content: result.fragment.size_content,
        match_made: result.match_made as u8,
    })
    .send();

    Ok(())
}
