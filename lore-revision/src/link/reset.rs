// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use tokio::fs;

use super::LinkError;
use crate::error::LoreResultExt;
use crate::link;
use crate::link::LinkFlags;
use crate::lore::Hash;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;

pub(crate) async fn reset_staged_add_link(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    link_node_id: NodeID,
    staged_link_node: Node,
    link_path: RelativePath,
) -> Result<(), LinkError> {
    let link_id = staged_link_node.linked_node().repository;
    let absolute_path = link_path.to_absolute_path(repository.require_path()?);

    // If the link replaced a committed directory, `link::add` staged that
    // directory node for delete. Capture it so we can restore it.
    let committed_directory_node = state_current
        .find_node_link(repository.clone(), link_path.as_str())
        .await
        .ok()
        .filter(|node_link| node_link.is_valid())
        .map(|node_link| node_link.node);

    state_staged
        .link_remove(repository.clone(), link_id, link_node_id)
        .await
        .forward::<LinkError>("Failed to remove link registry entry")?;

    util::fs::unlink_recursive(absolute_path.as_path())
        .await
        .emit_map_err(LinkError::internal(
            "Failed to remove realized link directory",
        ))?;

    if let Some(committed_node_id) = committed_directory_node {
        // Restore the committed directory: recreate the empty placeholder on
        // disk and clear the staged-delete on its node so it returns to its
        // clean committed state.
        fs::create_dir_all(absolute_path.as_path())
            .await
            .emit_map_err(LinkError::internal(
                "Failed to recreate placeholder directory",
            ))?;

        let block_index = NodeBlock::index(committed_node_id);
        let node_index = Node::index(committed_node_id);
        let block = state_staged
            .block(repository.clone(), block_index)
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;
        let dirtied = {
            let mut block_writer = block.write();
            block_writer.node(node_index).clear_all_change_flags();
            block_writer.mark_dirty()
        };
        if dirtied {
            state_staged.block_modified(block, block_index);
            state_staged.mark_dirty();
        }

        return Ok(());
    }

    let mut current_buf = link_path.into_buf();
    loop {
        current_buf.pop();
        if current_buf.as_str().is_empty() {
            break;
        }
        let parent_abs = repository.require_path()?.join(current_buf.as_str());
        if fs::remove_dir(parent_abs.as_path()).await.is_err() {
            break;
        }
    }

    Ok(())
}

pub(crate) async fn reset_staged_remove_link(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    link_node_id: NodeID,
    current_link_node: Node,
    link_path: RelativePath,
) -> Result<(), LinkError> {
    let link_id = current_link_node.linked_node().repository;

    let current_link_ref = state_current
        .link_find(repository.clone(), link_id, link_node_id)
        .await
        .forward::<LinkError>("Failed to find link registry entry")?;

    state_staged
        .link_add(
            repository.clone(),
            current_link_ref.repository,
            current_link_ref.branch,
            current_link_ref.signature,
            current_link_ref.local_node,
            LinkFlags::from_bits_truncate(current_link_ref.flags),
        )
        .await
        .forward::<LinkError>("Failed to restore link registry entry")?;

    let absolute_path = link_path.to_absolute_path(repository.require_path()?);
    fs::create_dir_all(absolute_path.as_path())
        .await
        .emit_map_err(LinkError::internal("Failed to recreate link directory"))?;

    let linked_repository = Arc::new(repository.to_link_context(link_id).await);
    link::realize_link_pin_change(
        repository.clone(),
        linked_repository,
        link_path,
        Hash::default(),
        current_link_node.address.hash,
        current_link_node.child,
    )
    .await?;

    Ok(())
}

pub(crate) async fn reset_staged_update_link(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    link_node_id: NodeID,
    staged_link_node: Node,
    current_link_node: Node,
    link_path: RelativePath,
) -> Result<(), LinkError> {
    let link_id = current_link_node.linked_node().repository;
    let staged_pin = staged_link_node.address.hash;
    let current_pin = current_link_node.address.hash;

    let prev_ref = state_current
        .link_find(repository.clone(), link_id, link_node_id)
        .await
        .forward::<LinkError>("Failed to find link registry entry")?;

    let linked_repository = Arc::new(repository.to_link_context(link_id).await);
    link::realize_link_pin_change(
        repository.clone(),
        linked_repository,
        link_path,
        staged_pin,
        current_pin,
        staged_link_node.child,
    )
    .await?;

    state_staged
        .link_update(
            repository,
            link_id,
            prev_ref.branch,
            current_pin,
            link_node_id,
        )
        .await
        .forward::<LinkError>("Failed to update link registry entry")?;

    Ok(())
}
