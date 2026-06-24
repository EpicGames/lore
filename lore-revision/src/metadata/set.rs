// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::task::JoinSet;
use zerocopy::IntoBytes;

use crate::errors::AddressNotFound;
use crate::errors::Disconnected;
use crate::errors::InvalidArguments;
use crate::errors::InvalidPath;
use crate::errors::LinkNotFound;
use crate::errors::NodeNotFound;
use crate::errors::NotFound;
use crate::errors::Oversized;
use crate::errors::PayloadNotFound;
use crate::errors::WriteRequired;
use crate::event;
use crate::immutable;
use crate::lore::Context;
use crate::lore::Hash;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;
use crate::node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum SetError {
    InvalidArguments,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
}

impl event::EventError for SetError {
    fn translated(&self) -> crate::interface::LoreError {
        match self {
            SetError::InvalidArguments(_) => crate::interface::LoreError::InvalidArguments,
            SetError::Disconnected(_) => crate::interface::LoreError::Connection,
            _ => crate::interface::LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

pub async fn set_revision(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    keys: &[&[u8]],
    values: &[&[u8]],
    formats: &[MetadataType],
) -> Result<(), SetError> {
    if keys.len() != values.len() || keys.len() != formats.len() {
        return Err(InvalidArguments {
            reason: format!(
                "metadata requires matching keys, values, and formats (got {} keys, {} values, {} formats)",
                keys.len(),
                values.len(),
                formats.len()
            ),
        }
        .into());
    }

    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .internal("Failed to deserialize current revision anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .internal("Failed to deserialize state")?;

    let metadata_hash = if current_revision == staged_revision {
        Hash::default()
    } else {
        state.metadata_hash()
    };
    let mut metadata = if metadata_hash.is_zero() {
        Metadata::new()
    } else {
        Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .internal("Failed to deserialize metadata")?
    };

    for i in 0..keys.len() {
        let key = keys[i];
        let value = values[i];
        let format = formats[i];

        let is_binary = format == MetadataType::Binary;
        if is_binary {
            // Read metadata from disk
            let payload = {
                let input_path = {
                    let user_path = String::from_utf8_lossy(value).to_string();
                    let given_path = PathBuf::from(&user_path);
                    if given_path.is_absolute() {
                        given_path
                    } else {
                        let repository_path = repository.require_path()?;
                        let relative_path =
                            RelativePath::new_from_user_path(repository_path, &user_path)
                                .internal("Invalid path")?;
                        relative_path.to_absolute_path(repository_path)
                    }
                };

                tokio::fs::read(input_path).await.internal("Invalid path")?
            };

            // When storing binary data, put it in the immutable store
            // Use a zero context to avoid creating extra entries if multiple
            // revisions use the same metadata blob
            let (address, _) = {
                immutable::write(
                    repository.clone(),
                    Context::default(),
                    Bytes::from_owner(payload),
                    immutable::write_options_from_repository(repository.clone()),
                )
                .await
                .internal("Failed to write payload")?
            };

            // When storing binary data, put its address in the metadata
            metadata
                .set(key, address.as_bytes(), MetadataType::Address)
                .internal("Failed to set metadata")?;
        } else {
            metadata
                .set(key, value, format)
                .internal("Failed to set metadata")?;
        }
    }

    state.set_metadata_hash(
        metadata
            .serialize(repository.clone())
            .await
            .internal("Failed to write metadata")?,
    );

    // Serialize the new current state
    if state.is_dirty() {
        state.set_revision_number(0);
        state.set_parent_self(current_revision);

        if staged_revision == current_revision {
            state.set_parent_other(Hash::default());
        }

        let signature = state
            .serialize(repository.clone(), token)
            .await
            .internal("Failed to serialize revision state")?;

        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .internal("Failed to serialize staged revision anchor")?;
    }

    let _ = event::metadata::send(&metadata);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn set_file_task(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path: &str,
    keys: &[Vec<u8>],
    values: &[Vec<u8>],
    formats: &[MetadataType],
    events: bool,
) -> Result<(), SetError> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path)
        .internal("Invalid path")?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .internal("Invalid node")?;
    if !node_link.is_valid() {
        return Err(SetError::internal("Invalid node"));
    }

    let metadata_node = node::node_to_file_metadata(node_link.node);
    let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
    let metadata_node_index = NodeFileMetadata::index(metadata_node);

    let metadata_block = state
        .block_file_metadata(repository.clone(), metadata_block_index)
        .await
        .internal("Failed to deserialize metadata block")?;

    let mut metadata;
    loop {
        let metadata_hash = {
            let block_reader = metadata_block.read();
            let node = block_reader.node(metadata_node_index);

            node.metadata
        };

        metadata = if metadata_hash.is_zero() {
            Metadata::new()
        } else {
            Metadata::deserialize(repository.clone(), metadata_hash)
                .await
                .internal("Failed to deserialize metadata")?
        };

        for index in 0..keys.len() {
            let key = &keys[index];
            let value = &values[index];
            let format = formats[index];

            let is_binary = format == MetadataType::Binary;
            if is_binary {
                // Read metadata from disk
                let payload = {
                    let input_path = {
                        let user_path = String::from_utf8_lossy(value).to_string();
                        let given_path = PathBuf::from(&user_path);
                        if given_path.is_absolute() {
                            given_path
                        } else {
                            let repository_path = repository.require_path()?;
                            let relative_path =
                                RelativePath::new_from_user_path(repository_path, &user_path)
                                    .internal("Invalid path")?;
                            relative_path.to_absolute_path(repository_path)
                        }
                    };

                    tokio::fs::read(input_path).await.internal("Invalid path")?
                };

                // When storing binary data, put it in the immutable store
                // Use a zero context to avoid creating extra entries if multiple
                // files use the same metadata blob
                let (address, _) = {
                    immutable::write(
                        repository.clone(),
                        Context::default(),
                        Bytes::from_owner(payload),
                        immutable::write_options_from_repository(repository.clone()),
                    )
                    .await
                    .internal("Failed to write payload")?
                };

                // When storing binary data, put its address in the metadata
                metadata
                    .set(key, address.as_bytes(), MetadataType::Address)
                    .internal("Failed to set metadata")?;
            } else {
                metadata
                    .set(key, value, format)
                    .internal("Failed to set metadata")?;
            }
        }

        let metadata_hash_updated = metadata
            .serialize(repository.clone())
            .await
            .internal("Failed to write metadata")?;

        let dirtied = {
            let mut block_writer = metadata_block.write();
            let node = block_writer.node(metadata_node_index);

            if node.metadata != metadata_hash {
                // Something else modified the node metadata while we were
                // serializing the data, need to loop and redo the operation
                continue;
            }

            if node.metadata != metadata_hash_updated {
                node.metadata = metadata_hash_updated;

                block_writer.mark_dirty()
            } else {
                false
            }
        };

        if dirtied {
            state.block_file_metadata_modified(metadata_block, metadata_block_index);
            state.mark_dirty();
        }

        break;
    }

    if events {
        let _ = event::metadata::send(&metadata);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn set_file(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: &[&str],
    keys: &[&[u8]],
    values: &[&[u8]],
    formats: &[MetadataType],
    entries: &[u32],
) -> Result<(), SetError> {
    if paths.len() != entries.len() {
        return Err(InvalidArguments {
            reason: format!(
                "metadata requires one entry count per path (got {} paths, {} entry counts)",
                paths.len(),
                entries.len()
            ),
        }
        .into());
    }

    let total: usize = entries.iter().map(|count| *count as usize).sum();
    if total != keys.len() || total != values.len() || total != formats.len() {
        return Err(InvalidArguments {
            reason: format!(
                "metadata requires matching keys, values, and formats (entry counts sum to {total}, got {} keys, {} values, {} formats)",
                keys.len(),
                values.len(),
                formats.len()
            ),
        }
        .into());
    }

    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .internal("Failed to deserialize current revision anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .internal("Failed to deserialize state")?;

    let events = paths.len() == 1; // Only if a single path is given.

    const MAX_TASK_COUNT: usize = 1000;

    let mut offset = 0;
    let mut tasks = JoinSet::new();
    let mut failure = None;
    for (index, path) in paths.iter().enumerate() {
        let count = entries[index] as usize;

        let repository = repository.clone();
        let state = state.clone();
        let path = (*path).to_string();
        let formats = formats[offset..offset + count].to_vec();

        let mut keys_vec = vec![];
        let mut values_vec = vec![];
        for i in 0..count {
            keys_vec.push(keys[offset + i].to_vec());
            values_vec.push(values[offset + i].to_vec());
        }

        lore_spawn!(tasks, {
            async move {
                set_file_task(
                    repository,
                    state,
                    &path,
                    &keys_vec,
                    &values_vec,
                    &formats,
                    events,
                )
                .await
            }
        });

        while tasks.len() > MAX_TASK_COUNT {
            if let Some(result) = tasks.join_next().await {
                failure = failure.or(result.internal("task failed").err());
            }
        }

        if failure.is_some() {
            break;
        }

        offset += count;
    }

    while let Some(result) = tasks.join_next().await {
        failure = failure.or(result.internal("task failed").err());
    }

    if let Some(err) = failure {
        return Err(err.into());
    }

    // Serialize the new current state
    if state.is_dirty() {
        state.set_revision_number(0);
        state.set_parent_self(current_revision);

        if staged_revision == current_revision {
            state.set_parent_other(Hash::default());
            state.set_metadata_hash(Hash::default());
        }

        let signature = state
            .serialize(repository.clone(), token)
            .await
            .internal("Failed to serialize revision state")?;

        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .internal("Failed to serialize staged revision anchor")?;
    }

    Ok(())
}
