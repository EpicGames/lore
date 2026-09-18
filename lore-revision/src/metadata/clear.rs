// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::MetadataErrors;
use crate::errors::FileNotFound;
use crate::event;
use crate::interface::LoreString;
use crate::lore::Hash;
use crate::node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state;
use crate::util::path::RelativePath;

/// Data for an event reporting that revision metadata was cleared.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreMetadataClearRevisionEventData {
    /// Hash of the revision whose metadata was cleared.
    pub revision: Hash,
}

/// Data for an event reporting that a file's metadata was cleared.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreMetadataClearFileEventData {
    /// Path of the file whose metadata was cleared.
    pub path: LoreString,
}

pub(crate) async fn clear_revision(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
) -> Result<(), MetadataErrors> {
    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<MetadataErrors>("deserializing current anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<MetadataErrors>("deserializing state")?;

    state.set_metadata_hash(Hash::default());

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
            .forward::<MetadataErrors>("serializing state")?;

        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<MetadataErrors>("serializing staged anchor")?;
    }

    event::LoreEvent::MetadataClearRevision(LoreMetadataClearRevisionEventData {
        revision: state.revision(),
    })
    .send();

    Ok(())
}

/// Boxed version of [`clear_revision`] for cross-crate use.
pub fn clear_revision_boxed(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
) -> crate::BoxFuture<'_, Result<(), MetadataErrors>> {
    Box::pin(clear_revision(repository, token))
}

pub async fn clear_file(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    path: String,
) -> Result<(), MetadataErrors> {
    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<MetadataErrors>("deserializing current anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<MetadataErrors>("deserializing state")?;

    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        .forward::<MetadataErrors>("resolving user path")?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .forward::<MetadataErrors>("finding node")?;
    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: path.clone(),
        }
        .into());
    }

    let metadata_node = node::node_to_file_metadata(node_link.node);
    let block_index = NodeFileMetadataBlock::index(metadata_node);
    let node_index = NodeFileMetadata::index(metadata_node);

    let metadata_block = state
        .block_file_metadata(repository.clone(), block_index)
        .await
        .forward::<MetadataErrors>("deserializing metadata block")?;

    let dirtied = {
        let mut block_writer = metadata_block.write();
        let node = block_writer.node(node_index);

        node.metadata = Hash::default();

        block_writer.mark_dirty()
    };

    if dirtied {
        state.block_file_metadata_modified(metadata_block, block_index);
        state.mark_dirty();
    }

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
            .forward::<MetadataErrors>("serializing state")?;

        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<MetadataErrors>("serializing staged anchor")?;
    }

    event::LoreEvent::MetadataClearFile(LoreMetadataClearFileEventData { path: path.into() })
        .send();

    Ok(())
}
