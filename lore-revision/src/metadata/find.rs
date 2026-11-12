// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::MetadataErrors;
use crate::errors::FileNotFound;
use crate::errors::MatchedStateErrors;
use crate::link::LinkedState;
use crate::lore::Hash;
use crate::metadata::Metadata;
use crate::node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state;
use crate::util::path::RelativePath;

pub(super) async fn revision(
    repository: Arc<RepositoryContext>,
    signature: Hash,
) -> Result<Option<Metadata>, MetadataErrors> {
    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<MetadataErrors>("deserializing state")?;

    let metadata_hash = state.metadata_hash();
    if !metadata_hash.is_zero() {
        let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .forward::<MetadataErrors>("deserializing metadata")?;

        return Ok(Some(metadata));
    }

    Ok(None)
}

/// The node `path` names in `state`, with the repository and state that number it: those of the
/// link `path` is inside, if any. `None` where `state` does not hold `path`.
async fn linked_file_node(
    repository: &Arc<RepositoryContext>,
    state: Arc<state::State>,
    path: &RelativePath,
) -> Result<Option<(NodeID, LinkedState)>, MetadataErrors> {
    let found = state
        .find_relative_held(repository.clone(), ROOT_NODE, path.as_str())
        .await
        .try_match("finding node")?;
    match found {
        Ok((node_link, linked)) => Ok(node_link.is_valid().then_some((node_link.node, linked))),
        Err(MatchedStateErrors::NodeNotFound(_)) => Ok(None),
        Err(other) => Err(other.forward("finding node")),
    }
}

pub(super) async fn file(
    repository: Arc<RepositoryContext>,
    signature: Hash,
    path: &RelativePath,
) -> Result<Option<Metadata>, MetadataErrors> {
    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<MetadataErrors>("deserializing state")?;

    // A deleted path is found in a parent.
    let mut found = linked_file_node(&repository, state.clone(), path).await?;
    for parent in [state.parent_self(), state.parent_other()] {
        if found.is_some() {
            break;
        }
        if let Ok(state_parent) = state::State::deserialize(repository.clone(), parent).await {
            found = linked_file_node(&repository, state_parent, path).await?;
        }
    }

    let Some((node, (repository, state))) = found else {
        return Err(FileNotFound {
            resource: path.to_string(),
        }
        .into());
    };

    let metadata_node = node::node_to_file_metadata(node);
    let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
    let metadata_node_index = NodeFileMetadata::index(metadata_node);

    let metadata_block = state
        .block_file_metadata(repository.clone(), metadata_block_index)
        .await
        .forward::<MetadataErrors>("deserializing metadata block")?;

    let metadata_hash = {
        let metadata_block_reader = metadata_block.read();
        let node = metadata_block_reader.node(metadata_node_index);

        node.metadata
    };

    if metadata_hash.is_zero() {
        return Ok(None);
    }

    let metadata = Metadata::deserialize(repository, metadata_hash)
        .await
        .forward::<MetadataErrors>("deserializing metadata")?;

    Ok(Some(metadata))
}
