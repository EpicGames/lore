// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::TypedBytes;
use crate::change::FileAction;
use crate::change::Flags;
use crate::errors::*;
use crate::event;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::metadata::Metadata;
use crate::node;
use crate::node::NodeDelta;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::runtime::execution_context;
use crate::state;
use crate::state::State;
use crate::util::serde::u8_as_bool;

/// Summary information about a single revision.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionInfoEventData {
    /// Repository identifier the revision belongs to.
    pub repository: RepositoryId,
    /// Revision hash signature.
    pub revision: Hash,
    /// Revision number.
    pub revision_number: u64,
    /// Parent revision hashes; the first is the direct parent and the second
    /// is the other parent of a merge, or zero when there is none.
    pub parent: [Hash; 2],
}

impl LoreRevisionInfoEventData {
    pub fn new(repository: RepositoryId, state: Arc<State>) -> Self {
        Self {
            repository,
            revision: state.revision(),
            revision_number: state.revision_number(),
            parent: [state.parent_self(), state.parent_other()],
        }
    }
}

/// Per-file change information between a revision and its parent.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionInfoDeltaEventData {
    /// Path of the file relative to the repository root.
    pub path: LoreString,
    /// Size of the file in bytes.
    pub size: u64,
    /// Action applied to the file.
    pub action: LoreFileAction,
    /// Flag indicating the file content was modified.
    #[serde(with = "u8_as_bool")]
    pub flag_modify: u8,
    /// Flag indicating the change came from a merge.
    #[serde(with = "u8_as_bool")]
    pub flag_merged: u8,
    /// Flag indicating the entry is a file rather than a directory.
    #[serde(with = "u8_as_bool")]
    pub flag_file: u8,
}

impl LoreRevisionInfoDeltaEventData {
    pub fn new(path: &str, delta: &NodeDelta, file: bool, size: u64) -> Self {
        Self {
            path: path.into(),
            size,
            action: LoreFileAction::from(delta.action),
            flag_modify: ((delta.flags & Flags::Modify) != 0).into(),
            flag_merged: ((delta.flags & Flags::Merge) != 0).into(),
            flag_file: file.into(),
        }
    }

    pub fn action_as_string_short(&self) -> &'static str {
        self.action.as_string_short()
    }

    pub fn merged_as_string_short(&self) -> &'static str {
        if self.flag_merged != 0 {
            return "(M)";
        }
        ""
    }
}

#[error_set]
pub enum InfoError {
    AddressNotFound,
    Disconnected,
    FileNotFound,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    PayloadNotFound,
    RevisionNotFound,
    WriteRequired,
    InvalidArguments,
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
    MissingIdentity,
}

impl crate::event::EventError for InfoError {}

#[derive(Clone, Debug)]
pub struct InfoOptions {
    /// Optional revision signature
    pub signature: Option<String>,
    /// Optional include of delta information
    pub delta: bool,
    /// Optional include of file metadata information, implies --delta
    pub metadata: bool,
}

pub async fn info(
    repository: Arc<RepositoryContext>,
    options: InfoOptions,
) -> Result<(), InfoError> {
    let signature = if let Some(signature) = options.signature {
        revision::resolve(
            repository.clone(),
            signature,
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .forward::<InfoError>("invalid revision")?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<InfoError>("deserializing current anchor")?;
        current_revision
    };

    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<InfoError>("deserializing state")?;

    event::LoreEvent::RevisionInfo(LoreRevisionInfoEventData::new(repository.id, state.clone()))
        .send();

    let metadata_hash = state.metadata_hash();
    if !metadata_hash.is_zero()
        && let Ok(metadata) = Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .forward::<InfoError>("deserializing revision metadata")
    {
        let _ = event::metadata::send(&metadata);
    }

    if options.delta || options.metadata {
        let mut parent_state: Option<Arc<State>> = None;

        if let Ok(delta_buffer) = state
            .delta_block(repository.clone())
            .await
            .forward::<InfoError>("deserializing delta block")
        {
            let delta_buffer = delta_buffer.to_aligned::<NodeDelta>();
            for delta in delta_buffer.as_type_slice::<NodeDelta>().iter() {
                let is_delete = delta.action == FileAction::Delete as u16;
                let is_parent_loaded = parent_state.is_some();
                if is_delete && !is_parent_loaded {
                    parent_state = Some(
                        state::State::deserialize(repository.clone(), state.parent_self())
                            .await
                            .forward::<InfoError>("deserializing state")?,
                    );
                }

                let node_id = delta.node;

                // TODO: Take merges into account when looking up information in the parent.

                let path;
                let file;
                let size;

                if is_delete {
                    let Some(parent_state) = parent_state.clone() else {
                        return Err(InfoError::internal("deserializing state"));
                    };

                    path = parent_state
                        .node_path(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node path")?;
                    file = parent_state
                        .node(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node block")?
                        .is_file();
                    size = 0;
                } else {
                    path = state
                        .node_path(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node path")?;

                    let node = state
                        .node(repository.clone(), node_id)
                        .await
                        .forward::<InfoError>("accessing node block")?;

                    file = node.is_file();
                    size = node.size;
                }

                event::LoreEvent::RevisionInfoDelta(LoreRevisionInfoDeltaEventData::new(
                    &path, delta, file, size,
                ))
                .send();

                if options.metadata {
                    let metadata_node_id = node::node_to_file_metadata(node_id);
                    let metadata_block_index = NodeFileMetadataBlock::index(metadata_node_id);
                    let metadata_node_index = NodeFileMetadata::index(metadata_node_id);

                    let metadata_hash = if is_delete {
                        let Some(parent_state) = parent_state.clone() else {
                            return Err(InfoError::internal("deserializing state"));
                        };

                        let metadata_block = parent_state
                            .block_file_metadata(repository.clone(), metadata_block_index)
                            .await
                            .forward::<InfoError>("deserializing metadata block")?;

                        let metadata_block_reader = metadata_block.read();
                        let metadata_node = metadata_block_reader.node(metadata_node_index);

                        metadata_node.metadata
                    } else {
                        let metadata_block = state
                            .block_file_metadata(repository.clone(), metadata_block_index)
                            .await
                            .forward::<InfoError>("deserializing metadata block")?;

                        let metadata_block_reader = metadata_block.read();
                        let metadata_node = metadata_block_reader.node(metadata_node_index);

                        metadata_node.metadata
                    };

                    if !metadata_hash.is_zero()
                        && let Ok(metadata) =
                            Metadata::deserialize(repository.clone(), metadata_hash)
                                .await
                                .forward::<InfoError>("deserializing file metadata")
                    {
                        let _ = event::metadata::send(&metadata);
                    }
                }
            }
        }
    }

    Ok(())
}
