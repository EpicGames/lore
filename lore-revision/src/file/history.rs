// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::change::FileAction;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::lore::Address;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::TypedBytes;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::metadata::Metadata;
use crate::node;
use crate::node::INVALID_NODE;
use crate::node::NodeDelta;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeID;
use crate::node::NodeLink;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

/// Data for the event describing one entry in a file's history.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileHistoryEventData {
    /// Path of the file.
    pub path: LoreString,
    /// Identifier of the repository.
    pub repository: RepositoryId,
    /// Revision this entry belongs to.
    pub revision: Hash,
    /// Sequential number of the revision.
    pub revision_number: u64,
    /// Parent revisions of this revision.
    pub parent: [Hash; 2],
    /// Address of the file content at this revision.
    pub address: Address,
    /// Size of the file in bytes at this revision.
    pub size: u64,
    /// Action applied to the file at this revision.
    pub action: LoreFileAction,
}

impl LoreFileHistoryEventData {
    pub fn new(
        path: String,
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        address: Address,
        size: u64,
        action: u32,
    ) -> Self {
        let mut file_action = FileAction::Keep;
        if action == FileAction::Add as u32 {
            file_action = FileAction::Add;
        } else if action == FileAction::Delete as u32 {
            file_action = FileAction::Delete;
        } else if action == FileAction::Move as u32 {
            file_action = FileAction::Move;
        } else if action == FileAction::Copy as u32 {
            file_action = FileAction::Copy;
        }

        LoreFileHistoryEventData {
            path: path.into(),
            repository: repository.id,
            revision: state.revision(),
            revision_number: state.revision_number(),
            parent: [state.parent_self(), state.parent_other()],
            address,
            size,
            action: LoreFileAction::from(file_action),
        }
    }
}

#[error_set]
pub enum FileHistoryError {
    InvalidArguments,
    InvalidPath,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    PayloadNotFound,
    WriteRequired,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    SlowDown,
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

impl EventError for FileHistoryError {
    fn translated(&self) -> LoreError {
        match self {
            FileHistoryError::InvalidArguments(_) | FileHistoryError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            FileHistoryError::FileNotFound(_) => LoreError::FileNotFound,
            FileHistoryError::RevisionNotFound(_)
            | FileHistoryError::BranchNotFound(_)
            | FileHistoryError::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct HistoryOptions {
    /// Optional revision specifier
    pub revision: Option<String>,

    /// Show revisions on specific branch
    pub branch: Option<String>,

    /// Number of revisions to list
    pub length: u32,

    /// Number of revisions to search initially
    pub depth: u32,
}

async fn history_state(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    action: u16,
) -> Result<(), FileHistoryError> {
    let path = if node_id != INVALID_NODE {
        state
            .node_path(repository.clone(), node_id)
            .await
            .forward::<FileHistoryError>("Invalid node")?
    } else {
        String::new()
    };

    let node = if node_id != INVALID_NODE {
        Some(
            state
                .node(repository.clone(), node_id)
                .await
                .forward::<FileHistoryError>("Invalid node")?,
        )
    } else {
        None
    };

    // File properties
    let (file_action, file_metadata_hash) = {
        if node_id != INVALID_NODE {
            let file_metadata_node_id = node::node_to_file_metadata(node_id);
            let file_metadata_block_index = NodeFileMetadataBlock::index(file_metadata_node_id);
            let file_metadata_node_index = NodeFileMetadata::index(file_metadata_node_id);

            let file_metadata_block = state
                .block_file_metadata(repository.clone(), file_metadata_block_index)
                .await
                .forward::<FileHistoryError>("Failed to deserialize metadata block")?;

            let (file_action, file_metadata_hash) = {
                let file_metadata_block_reader = file_metadata_block.read();
                let file_metadata_node = file_metadata_block_reader.node(file_metadata_node_index);

                (file_metadata_node.action[0], file_metadata_node.metadata)
            };

            (file_action, file_metadata_hash)
        } else {
            (FileAction::Delete as u16, Hash::default())
        }
    };

    // Action
    let action = if action == u16::MAX {
        file_action
    } else {
        action
    };

    // Revision
    event::LoreEvent::FileHistory(LoreFileHistoryEventData::new(
        path,
        repository.clone(),
        state.clone(),
        node.as_ref().map(|node| node.address).unwrap_or_default(),
        node.as_ref().map(|node| node.size).unwrap_or_default(),
        action as u32,
    ))
    .send();

    // Revision metadata
    let revision_metadata_hash = state.metadata_hash();
    if !revision_metadata_hash.is_zero() {
        let metadata = Metadata::deserialize(repository.clone(), revision_metadata_hash)
            .await
            .forward::<FileHistoryError>("Failed to deserialize metadata")?;

        event::metadata::send(&metadata)
            .forward::<FileHistoryError>("Failed to deserialize metadata")?;
    }

    // File metadata
    if !file_metadata_hash.is_zero() {
        let metadata = Metadata::deserialize(repository.clone(), file_metadata_hash)
            .await
            .forward::<FileHistoryError>("Failed to deserialize metadata")?;

        event::metadata::send(&metadata)
            .forward::<FileHistoryError>("Failed to deserialize metadata")?;
    }

    Ok(())
}

async fn history_parent(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    parent: Hash,
    relative_path: &RelativePath,
    options: HistoryOptions,
) -> Result<NodeLink, FileHistoryError> {
    let mut depth = if options.depth > 0 { options.depth } else { 10 };

    let mut node_link = NodeLink::invalid();
    let mut state = state.clone();
    let mut parent = parent;
    while depth > 0 && !parent.is_zero() {
        let state_parent = state::State::deserialize(repository.clone(), parent)
            .await
            .forward::<FileHistoryError>("Failed to deserialize state")?;

        node_link = state_parent
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if node_link.is_valid() {
            // Node exists in 'state_parent' but not in 'state'.
            // What happened to it?

            let node_delta = state
                .node_delta(repository.clone(), node_link.node)
                .await
                .forward::<FileHistoryError>("Invalid node")?;

            let action = if let Some(node_delta) = node_delta {
                node_delta.action
            } else {
                FileAction::Delete as u16
            };

            let node_id = if let Some(node_delta) = node_delta {
                node_delta.node
            } else {
                INVALID_NODE
            };

            history_state(repository.clone(), state.clone(), node_id, action).await?;

            break;
        }

        state = state_parent;
        depth -= 1;
        parent = state.parent_self();
    }

    Ok(node_link)
}

async fn find_start_revision(
    repository: Arc<RepositoryContext>,
    options: HistoryOptions,
) -> Result<Hash, FileHistoryError> {
    if options.revision.is_some() && options.branch.is_some() {
        return Err(FileHistoryError::internal("Invalid options"));
    }
    if let Some(revision_spec) = options.revision {
        return revision::resolve(
            repository.clone(),
            revision_spec.as_str(),
            if options.depth > 0 {
                Some(options.depth as usize)
            } else {
                None
            },
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|_err| {
            FileHistoryError::from(RevisionNotFound {
                revision: revision_spec,
            })
        });
    }

    let branch = if options.branch.is_none() {
        let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<FileHistoryError>("Failed to deserialize current revision anchor")?;
        if current_branch.is_zero() {
            let metadata = repository::metadata_hash(repository.clone())
                .await
                .internal("Failed to load repository metadata")?;
            let metadata = repository::metadata(repository.clone(), metadata)
                .await
                .internal("Failed to load repository metadata")?;
            metadata.default_branch
        } else {
            current_branch
        }
    } else {
        let branch_name = options.branch.clone().unwrap_or_default();
        let resolved = branch::resolve(repository.clone(), branch_name.as_str())
            .await
            .map_err(|_err| {
                FileHistoryError::from(BranchNotFound {
                    branch: branch_name,
                })
            })?;
        resolved.id
    };

    let local_latest = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();
    if execution_context().globals().local() {
        return Ok(local_latest);
    }

    let remote_latest = if let Ok(remote) = repository.remote().await {
        branch::load_remote_latest(remote.clone(), repository.id, branch)
            .await
            .unwrap_or_default()
    } else {
        Hash::default()
    };
    if execution_context().globals().remote() {
        return Ok(remote_latest);
    }

    // Is there a latest?
    if remote_latest.is_zero() {
        return Ok(local_latest);
    }
    if remote_latest.is_zero() && local_latest.is_zero() {
        return Ok(Hash::default());
    }

    // Is remote latest the same as local latest?
    if remote_latest == local_latest {
        return Ok(remote_latest);
    }

    let mut remote_state = state::State::deserialize(repository.clone(), remote_latest)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;
    let local_state = state::State::deserialize(repository.clone(), local_latest)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;

    // Is local ahead of remote?
    if local_state.revision_number() > remote_state.revision_number() {
        return Ok(local_latest);
    }

    loop {
        let next_latest = remote_state.parent_self();

        // If a remote latest is encountered that matches the local latest, then remote
        // is ahead of local but there is no divergence.
        if next_latest == local_latest {
            break Ok(remote_latest);
        }

        remote_state = state::State::deserialize(repository.clone(), next_latest)
            .await
            .forward::<FileHistoryError>("Failed to deserialize state")?;

        // If remote revision number is less than the local revision number but
        // we haven't encountered the local revision number, there is a divergence.
        // In this case, show the local revision list until it's been pushed.
        if remote_state.revision_number() < local_state.revision_number() {
            break Ok(local_latest);
        }
    }
}

async fn history_start(
    repository: Arc<RepositoryContext>,
    path: String,
    options: HistoryOptions,
) -> Result<NodeLink, FileHistoryError> {
    let path_for_error = path.clone();
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, &path)
        .forward::<FileHistoryError>("resolving user path")?;

    // Start from the given revision, or from the branch latest (remote / local) if not given.
    let signature = find_start_revision(repository.clone(), options.clone()).await?;

    let state_start = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;

    // Handle case where history is queried for a file that was deleted in this revision by going to parent
    // revision(s), finding the node and if found, checking the delta block of this revision to see if that node was
    // deleted - if so print the history starting with delete and then going to parent revision(s) and continuing.
    let mut node_link = state_start
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .unwrap_or_default();
    if !node_link.is_valid() {
        let parent = state_start.parent_self();

        node_link = history_parent(
            repository.clone(),
            state_start.clone(),
            parent,
            &relative_path,
            options.clone(),
        )
        .await?;
    }
    if !node_link.is_valid() {
        let parent = state_start.parent_other();

        node_link = history_parent(
            repository.clone(),
            state_start.clone(),
            parent,
            &relative_path,
            options.clone(),
        )
        .await?;
    }
    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: path_for_error,
        }
        .into());
    }

    let state_node = state::State::deserialize(repository.clone(), node_link.revision)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;

    let delta_block = state_node
        .delta_block(repository.clone())
        .await
        .forward::<FileHistoryError>("Deserialize delta block failed")?
        .to_aligned::<NodeDelta>();

    for node_delta in delta_block.as_type_slice::<NodeDelta>().iter() {
        if node_delta.node == node_link.node {
            history_state(
                repository.clone(),
                state_node.clone(),
                node_link.node,
                node_delta.action,
            )
            .await?;

            if node_delta.action == FileAction::Add as u16 {
                // Should not iterate history, added in this revision
                node_link = NodeLink::invalid();
            }

            break;
        }
    }

    Ok(node_link)
}

pub async fn history(
    repository: Arc<RepositoryContext>,
    path: String,
    options: HistoryOptions,
) -> Result<(), FileHistoryError> {
    let mut limit = if options.length > 0 {
        options.length
    } else {
        100
    };

    let node_link = history_start(repository.clone(), path, options).await?;

    if !node_link.is_valid() {
        return Ok(());
    }

    let mut node_id = node_link.node;
    let mut state = state::State::deserialize(repository.clone(), node_link.revision)
        .await
        .forward::<FileHistoryError>("Failed to deserialize state")?;

    while limit > 0 {
        // File metadata
        let metadata_node_id = node::node_to_file_metadata(node_id);
        let metadata_block_index = NodeFileMetadataBlock::index(metadata_node_id);
        let metadata_node_index = NodeFileMetadata::index(metadata_node_id);

        let metadata_block = state
            .block_file_metadata(repository.clone(), metadata_block_index)
            .await
            .forward::<FileHistoryError>("Failed to deserialize metadata block")?;

        let (action, signature_next, node_id_next) = {
            let metadata_block_reader = metadata_block.read();
            let metadata_node = metadata_block_reader.node(metadata_node_index);

            lore_debug!(
                "Revision {} -> {} node {} file metadata {:?}",
                state.revision(),
                state.revision_number(),
                node_id,
                metadata_node
            );
            (
                metadata_node.action[0],
                metadata_node.revision[0],
                metadata_node.node[0],
            )
        };

        if signature_next.is_zero() {
            lore_debug!("Reached end of file history without ADD");
            break;
        }

        state = state::State::deserialize(repository.clone(), signature_next)
            .await
            .forward::<FileHistoryError>("Failed to deserialize state")?;

        history_state(repository.clone(), state.clone(), node_id, action).await?;

        // Last?
        if action == FileAction::Add as u16 {
            break;
        }

        // Next.
        node_id = node_id_next;
        limit -= 1;
    }

    Ok(())
}
