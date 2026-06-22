// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;
use zerocopy::FromZeros;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::immutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::metadata::Metadata;
use crate::node;
use crate::node::Node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::state;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

#[error_set]
pub enum InfoError {
    InvalidArguments,
    InvalidPath,
    RevisionNotFound,
    FileNotFound,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
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
    MissingIdentity,
}

impl EventError for InfoError {
    fn translated(&self) -> LoreError {
        match self {
            Self::InvalidArguments(_) | Self::InvalidPath(_) => LoreError::InvalidArguments,
            Self::RevisionNotFound(_) | Self::NotFound(_) => LoreError::NotFound,
            Self::FileNotFound(_) => LoreError::FileNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for the event reporting information about a single file or directory.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileInfoEventData {
    /// Path of the file or directory.
    pub path: LoreString,
    /// Context identifying the file or directory.
    pub context: Context,
    /// Content hash of the file or directory.
    pub hash: Hash,
    /// Set when the entry is a file.
    #[serde(with = "u8_as_bool")]
    pub is_file: u8,
    /// Set when the entry is a directory.
    #[serde(with = "u8_as_bool")]
    pub is_dir: u8,
    /// Set when the entry has been modified.
    #[serde(with = "u8_as_bool")]
    pub flag_modified: u8,
    /// Set when the entry has been deleted.
    #[serde(with = "u8_as_bool")]
    pub flag_deleted: u8,
    /// Set when the entry has been added.
    #[serde(with = "u8_as_bool")]
    pub flag_added: u8,
    /// Set when the entry is in conflict.
    #[serde(with = "u8_as_bool")]
    pub flag_conflict: u8,
    /// File mode bits.
    pub mode: u16,
    /// Size of the entry in the repository, in bytes.
    pub size: u64,
    /// Size of the entry on the local filesystem, in bytes.
    pub local_size: u64,
    /// Content hash of the entry on the local filesystem.
    pub local_hash: Hash,
    /// Size of the entry after filters are applied, in bytes.
    pub filter_size: u64,
}

#[derive(Clone, Debug)]
pub struct InfoOptions {
    /// Optional revision specifier
    pub revision: Option<String>,
    /// Calculate the filtered local filesystem hash and size
    pub local: bool,
    /// Calculate the filtered repository size
    pub filtered: bool,
}

pub async fn info(
    repository: Arc<RepositoryContext>,
    paths: Vec<RelativePath>,
    options: InfoOptions,
) -> Result<(), InfoError> {
    let signature = if let Some(revision_spec) = options.revision {
        revision::resolve(
            repository.clone(),
            revision_spec.as_str(),
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|_err| {
            InfoError::from(RevisionNotFound {
                revision: revision_spec,
            })
        })?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<InfoError>("Failed deserializing revision state")?;
        current_revision
    };

    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<InfoError>("Failed deserializing revision state")?;

    let mut tasks = JoinSet::new();
    for path in paths.iter() {
        lore_debug!("Info path: {path}");

        let repository = repository.clone();
        let state = state.clone();
        let path = path.clone();

        lore_spawn!(tasks, async move {
            info_path(repository, state, path, options.local, options.filtered).await
        });
    }

    let mut failure: Option<InfoError> = None;
    while let Some(result) = tasks.join_next().await {
        let inner = result
            .internal("Internal task failure")
            .map_err(InfoError::from)
            .flatten();
        failure = failure.or(inner.err());
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(())
}

async fn info_path(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    path: RelativePath,
    local: bool,
    filtered: bool,
) -> Result<(), InfoError> {
    if let Ok(node_link) = state_current
        .find_node_link(repository.clone(), path.as_str())
        .await
    {
        // TODO(vri): UCS-19229 - Links: Handle link nodes in file info lookup
        let node_path = state_current
            .node_path(repository.clone(), node_link.node)
            .await
            .map_err(|_err| {
                InfoError::from(FileNotFound {
                    resource: path.to_string(),
                })
            })?;

        let node = state_current
            .node(repository.clone(), node_link.node)
            .await
            .map_err(|_err| {
                InfoError::from(FileNotFound {
                    resource: path.to_string(),
                })
            })?;

        let address_context = if node.is_file() {
            node.address.context
        } else {
            Context::new_zeroed()
        };

        if filtered {
            // We will be iterating the tree, cache the state fragments
            let _ = state_current.cache_fragments(repository.clone()).await;
        }

        lore_debug!("Info local {local} and filtered {filtered}");
        let mut local_filtered = if local || filtered {
            // Calculate the local size and hash
            calculate_local_filtered_size_hash(
                repository.clone(),
                path.clone(),
                state_current.clone(),
                node,
                node_link.node,
                local,
                filtered,
            )
            .await?
        } else {
            LocalFiltered::default()
        };

        let mut is_modified = false;
        let mut is_deleted = false;
        if local_filtered.local_size == 0 {
            let absolute_path = path.to_absolute_path(repository.require_path()?);
            let file_metadata = tokio::fs::metadata(absolute_path.as_path()).await;
            if let Ok(file_metadata) = file_metadata {
                if file_metadata.is_file() {
                    local_filtered.local_size = util::fs::file_size(&file_metadata);
                    if local_filtered.local_size != node.size {
                        is_modified = true;
                    } else {
                        local_filtered.local_hash = immutable::hash_file(
                            repository.clone(),
                            absolute_path.as_path(),
                            Some(node.address),
                            Some(node.size as usize),
                        )
                        .await
                        .forward::<InfoError>(&format!("Failed to hash local file: {path}"))?;
                    }
                }
            } else {
                is_deleted = true;
            }
        }

        if !local_filtered.local_hash.is_zero() {
            is_modified = local_filtered.local_hash != node.address.hash;
        }

        let node_size = if node_link.node == ROOT_NODE {
            let tree = state_current
                .tree(repository.clone())
                .await
                .forward::<InfoError>("Failed deserializing revision state")?;
            tree.size
        } else {
            node.size
        };
        event::LoreEvent::FileInfo(LoreFileInfoEventData {
            path: node_path.into(),
            context: address_context,
            hash: node.address.hash,
            is_file: node.is_file().into(),
            is_dir: (node.is_directory() || node.is_link()).into(),
            flag_modified: is_modified.into(),
            flag_deleted: is_deleted.into(),
            flag_added: 0,
            flag_conflict: 0,
            size: node_size,
            mode: node.mode,
            local_size: local_filtered.local_size,
            local_hash: local_filtered.local_hash,
            filter_size: local_filtered.filtered_size,
        })
        .send();

        let metadata_node = node::node_to_file_metadata(node_link.node);
        let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
        let metadata_node_index = NodeFileMetadata::index(metadata_node);

        let metadata_block = state_current
            .block_file_metadata(repository.clone(), metadata_block_index)
            .await
            .forward::<InfoError>("Deserialize metadata block failed")?;

        let metadata_hash = {
            let metadata_block_reader = metadata_block.read();
            let node = metadata_block_reader.node(metadata_node_index);

            node.metadata
        };

        if !metadata_hash.is_zero() {
            let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
                .await
                .forward::<InfoError>("Deserialize metadata failed")?;

            event::metadata::send(&metadata).forward::<InfoError>("Deserialize metadata failed")?;
        }
    } else {
        let absolute_path = path.to_absolute_path(repository.require_path()?);
        let file_metadata = tokio::fs::metadata(absolute_path).await;
        if let Ok(file_metadata) = file_metadata
            && (file_metadata.is_file() || file_metadata.is_dir())
        {
            event::LoreEvent::FileInfo(LoreFileInfoEventData {
                path: path.into(),
                context: Context::default(),
                hash: Hash::default(),
                is_file: file_metadata.is_file().into(),
                is_dir: file_metadata.is_dir().into(),
                flag_modified: 0,
                flag_deleted: 0,
                flag_added: 1,
                flag_conflict: 0,
                size: 0,
                mode: 0,
                local_size: util::fs::file_size(&file_metadata),
                local_hash: Hash::default(),
                filter_size: 0,
            })
            .send();
        }
    }

    Ok(())
}

#[derive(Default)]
struct LocalFiltered {
    local_size: u64,
    local_hash: Hash,
    filtered_size: u64,
}

async fn calculate_local_filtered_size_hash(
    repository: Arc<RepositoryContext>,
    relative_path: RelativePath,
    state: Arc<State>,
    node: Node,
    node_id: NodeID,
    local: bool,
    filtered: bool,
) -> Result<LocalFiltered, InfoError> {
    if repository
        .filter
        .emit_excludes(&relative_path, node.is_directory(), FilterMode::Full)
    {
        return Ok(LocalFiltered::default());
    }

    let absolute_path = relative_path.to_absolute_path(repository.require_path()?);
    if node.is_file() {
        if let Ok(metadata) = tokio::fs::metadata(absolute_path.as_path()).await {
            let size = util::fs::file_size(&metadata);
            let hash = if size > 0 {
                immutable::hash_file(
                    repository.clone(),
                    absolute_path.as_path(),
                    Some(node.address),
                    Some(node.size as usize),
                )
                .await
                .forward::<InfoError>(&format!("Failed to hash local file: {relative_path}"))?
            } else {
                Hash::default()
            };
            Ok(LocalFiltered {
                local_size: size,
                local_hash: hash,
                filtered_size: node.size,
            })
        } else {
            Ok(LocalFiltered {
                local_size: 0,
                local_hash: Hash::default(),
                filtered_size: node.size,
            })
        }
    } else if node.is_directory() {
        // Get the local file sizes
        let local_size_repository = repository.clone();
        let local_size_relative_path = relative_path.clone();
        let local_size_task = lore_spawn!(async move {
            if local {
                lore_debug!("Calculating local size");
                calculate_local_size_recurse(local_size_repository, local_size_relative_path).await
            } else {
                Ok(0)
            }
        });

        // Filter local directory and calculate sizes
        let filtered_size_repository = repository.clone();
        let filtered_size_relative_path = relative_path.clone();
        let filtered_state = state.clone();
        let filtered_size_task = lore_spawn!(async move {
            if filtered {
                lore_debug!("Calculating filtered size");
                calculate_filtered_size_recurse(
                    filtered_size_repository,
                    filtered_size_relative_path,
                    filtered_state,
                    node,
                    node_id,
                )
                .await
            } else {
                Ok(0)
            }
        });

        let local_result = local_size_task.await;
        let filtered_result = filtered_size_task.await;

        let local_size = local_result
            .internal("Internal task failure")
            .map_err(InfoError::from)
            .flatten()?;
        let filtered_size = filtered_result
            .internal("Internal task failure")
            .map_err(InfoError::from)
            .flatten()?;

        Ok(LocalFiltered {
            local_size,
            local_hash: Hash::default(),
            filtered_size,
        })
    } else if node.is_link() {
        // TODO(vri): UCS-19229 - Links: Handle link nodes in file info lookup
        Ok(LocalFiltered::default())
    } else {
        Ok(LocalFiltered::default())
    }
}

fn calculate_local_size_recurse(
    repository: Arc<RepositoryContext>,
    relative_path: RelativePath,
) -> Pin<Box<dyn Future<Output = Result<u64, InfoError>> + Send>> {
    Box::pin(async move {
        if relative_path.as_str() == DOT_URC || relative_path.as_str() == DOT_LORE {
            return Ok(0);
        }
        let absolute_path = relative_path.to_absolute_path(repository.require_path()?);

        if let Ok(metadata) = tokio::fs::metadata(absolute_path.as_path()).await {
            if repository
                .filter
                .emit_excludes(&relative_path, metadata.is_dir(), FilterMode::Full)
            {
                return Ok(0);
            }

            if metadata.is_file() {
                return Ok(util::fs::file_size(&metadata));
            } else if metadata.is_dir() {
                let mut local_size = 0;
                let mut local_size_tasks = JoinSet::new();
                let mut list = util::fs::list_directory(absolute_path)
                    .internal(&format!("Failed to list directory: {relative_path}"))?;

                while let Some(item) = list.recv().await {
                    let repository = repository.clone();
                    let relative_path = relative_path.push_into_buf(item.name.as_str()).freeze();
                    lore_spawn!(local_size_tasks, async move {
                        calculate_local_size_recurse(repository, relative_path).await
                    });
                }

                let mut failure: Option<InfoError> = None;
                while let Some(result) = local_size_tasks.join_next().await {
                    let inner = result
                        .internal("Internal task failure")
                        .map_err(InfoError::from)
                        .flatten();
                    match inner {
                        Ok(size) => {
                            local_size += size;
                        }
                        Err(err) => {
                            failure = failure.or(Some(err));
                        }
                    }
                }

                if let Some(err) = failure {
                    return Err(err);
                }

                return Ok(local_size);
            }
        }
        Ok(0)
    })
}

fn calculate_filtered_size_recurse(
    repository: Arc<RepositoryContext>,
    relative_path: RelativePath,
    state: Arc<State>,
    node: Node,
    node_id: NodeID,
) -> Pin<Box<dyn Future<Output = Result<u64, InfoError>> + Send>> {
    Box::pin(async move {
        if repository
            .filter
            .emit_excludes(&relative_path, node.is_directory(), FilterMode::Full)
        {
            return Ok(0);
        }
        if node.is_file() {
            return Ok(node.size);
        }

        let mut filtered_size = 0;
        let mut filtered_size_tasks = JoinSet::new();

        let mut failure: Option<InfoError> = None;
        let mut child_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child) = child_iter {
            let repository = repository.clone();
            let state = state.clone();
            let relative_path = relative_path.clone();
            let Ok(child_node) = state.node(repository.clone(), child).await else {
                failure = Some(InfoError::internal(
                    "Failed to calculate filtered size, encountered an invalid node",
                ));
                break;
            };
            if let Err(err) = child_node
                .walk_step(child, node_id, &mut cycle)
                .forward::<InfoError>("Failed to calculate filtered size, invalid node hierarchy")
            {
                failure = Some(err);
                break;
            }
            let sibling = child_node.sibling();
            lore_spawn!(filtered_size_tasks, async move {
                let name = state
                    .node_name_ref(repository.clone(), child)
                    .await
                    .forward::<InfoError>(
                        "Failed to calculate filtered size, encountered an invalid node",
                    )?;
                let relative_path = relative_path.push_into_buf(name).freeze();
                calculate_filtered_size_recurse(repository, relative_path, state, child_node, child)
                    .await
            });

            child_iter = sibling;
        }

        while let Some(result) = filtered_size_tasks.join_next().await {
            let inner = result
                .internal("Internal task failure")
                .map_err(InfoError::from)
                .flatten();
            match inner {
                Ok(size) => {
                    filtered_size += size;
                }
                Err(err) => {
                    failure = failure.or(Some(err));
                }
            }
        }

        if let Some(err) = failure {
            return Err(err);
        }

        Ok(filtered_size)
    })
}
