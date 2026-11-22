// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The node of one tree that is a given node of another, for trees that may not share node IDs.

use std::sync::Arc;

use lore_error_set::prelude::*;

use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;

/// Finds the node of `to`, whose tree has `block_count` blocks, that is node `node_id` of `from`,
/// for trees that may not share node IDs.
///
/// The node with the same ID is the counterpart when both are the same file
/// ([`same_file_counterpart`]), which keeps a file renamed since the two trees diverged. Any other
/// node is found at the path `from` holds it at ([`path_counterpart`]).
pub(crate) async fn find_counterpart_node(
    repository: &Arc<RepositoryContext>,
    from: &State,
    node_id: NodeID,
    to: &State,
    block_count: u32,
) -> Result<Option<NodeID>, StateError> {
    let from_node = from
        .node(repository.clone(), node_id)
        .await
        .forward::<StateError>("Failed reading a node to find its counterpart")?;
    if let Some(same) =
        same_file_counterpart(repository, &from_node, node_id, to, block_count).await?
    {
        return Ok(Some(same));
    }
    path_counterpart(repository, from, node_id, to).await
}

/// `node_id` where node `node_id` of `to`, whose tree has `block_count` blocks, is the same file as
/// `from_node`, a node of another tree: both files with the same file ID. A slot `to` freed holds
/// no file, as its flags are exactly `Discarded`.
pub(crate) async fn same_file_counterpart(
    repository: &Arc<RepositoryContext>,
    from_node: &Node,
    node_id: NodeID,
    to: &State,
    block_count: u32,
) -> Result<Option<NodeID>, StateError> {
    if NodeBlock::index(node_id) >= block_count as usize
        || !from_node.is_file()
        || from_node.address.context.is_zero()
    {
        return Ok(None);
    }
    let to_node = to
        .node(repository.clone(), node_id)
        .await
        .forward::<StateError>("Failed reading the node of the same ID in the other tree")?;
    Ok(
        (to_node.is_file() && from_node.address.context == to_node.address.context)
            .then_some(node_id),
    )
}

/// The node of `to` at the path `from` holds node `node_id` at ([`node_at_path`]).
async fn path_counterpart(
    repository: &Arc<RepositoryContext>,
    from: &State,
    node_id: NodeID,
    to: &State,
) -> Result<Option<NodeID>, StateError> {
    let path = from
        .node_path(repository.clone(), node_id)
        .await
        .forward::<StateError>("Failed reading the path of a node to find its counterpart")?;
    node_at_path(repository, to, ROOT_NODE, path.as_str()).await
}

/// The node of `state` at `path` below its node `root`. `None` where `state` holds nothing there,
/// or holds it in a linked repository, whose history belongs to that repository's revisions.
pub(crate) async fn node_at_path(
    repository: &Arc<RepositoryContext>,
    state: &State,
    root: NodeID,
    path: &str,
) -> Result<Option<NodeID>, StateError> {
    let node_link = match state
        .find_relative_node_link(repository.clone(), root, path)
        .await
    {
        Ok(node_link) => node_link,
        Err(StateError::NodeNotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if !node_link.is_valid() || node_link.crosses_link(repository, state) {
        return Ok(None);
    }
    Ok(Some(node_link.node))
}
