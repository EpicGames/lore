// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LoreRepositoryStateDumpEventData;
use super::LoreRepositoryStateDumpNodeEventData;
use super::Node;
use super::NodeBlock;
use super::NodeID;
use super::ROOT_NODE;
use super::SiblingCycleGuard;
use super::State;
use super::StateError;
use crate::event;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

pub async fn dump(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    path: Option<RelativePath>,
    max_depth: usize,
) -> Result<(), StateError> {
    // Dump out the merkle tree
    let tree_hash = state.data.read().hash_tree;
    let tree = state.tree(repository.clone()).await?;
    event::LoreEvent::RepositoryStateDump(LoreRepositoryStateDumpEventData {
        revision_number: state.revision_number(),
        revision: state.revision(),
        tree_hash,
        tree_size: tree.size,
    })
    .send();

    if let Some(path) = path {
        let node_link = state
            .find_node_link(repository.clone(), path.as_str())
            .await?;
        let link_repository = Arc::new(repository.to_link_context(node_link.repository).await);
        let entry_node = state.node(link_repository.clone(), node_link.node).await?;
        let mut cycle = SiblingCycleGuard::new(entry_node.parent);
        dump_node(
            state.clone(),
            link_repository,
            node_link.node,
            entry_node.parent,
            RelativePathBuf::new(),
            0,
            max_depth,
            &mut cycle,
        )
        .await?;
    } else {
        let block = state.block(repository.clone(), 0).await?;
        let mut node_id_ref = block.node(0).child();
        let mut cycle = SiblingCycleGuard::new(ROOT_NODE);
        while let Some(node_id) = node_id_ref {
            node_id_ref = dump_node(
                state.clone(),
                repository.clone(),
                node_id,
                ROOT_NODE,
                RelativePathBuf::new(),
                0,
                max_depth,
                &mut cycle,
            )
            .await?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn dump_node(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    expected_parent: NodeID,
    mut subpath: RelativePathBuf,
    depth: usize,
    max_depth: usize,
    cycle: &mut SiblingCycleGuard,
) -> Result<Option<NodeID>, StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await?;
    let node = block.node(node_index);
    if node.is_discarded() {
        lore_debug!("Ignoring discarded node {node_id}");
        return Ok(None);
    }
    node.walk_step(node_id, expected_parent, cycle)?;
    {
        let node_name = state
            .node_name_ref(repository.clone(), node_id)
            .await
            .forward::<StateError>("Failed to get node name")?;
        subpath.push(node_name);

        let type_data = if node.is_directory() {
            format!("child {}", node.child)
        } else if node.is_link() {
            let link_node = node.linked_node();
            format!(
                "link {} rev {} node {}",
                link_node.repository, link_node.revision, link_node.node
            )
        } else {
            format!("addr {}", node.address)
        };

        let name = format!(
            "{}{}",
            subpath.as_str(),
            if node.is_directory() { "/" } else { "" }
        );
        event::LoreEvent::RepositoryStateDumpNode(LoreRepositoryStateDumpNodeEventData {
            name: name.into(),
            id: node_id,
            parent: node.parent,
            sibling: node.sibling,
            mode: node.mode,
            size: node.size,
            flags: node.flags,
            type_data: type_data.into(),
        })
        .send();
    }
    if node.is_link() && ((max_depth == 0) || (depth + 1 < max_depth)) {
        let link_node = node.linked_node();
        let linked_repository = Arc::new(repository.to_link_context(link_node.repository).await);
        let link_state = State::deserialize(linked_repository.clone(), link_node.revision).await?;
        let link_entry_id = link_node.node as NodeID;
        let link_entry = link_state
            .node(linked_repository.clone(), link_entry_id)
            .await?;
        let link_expected_parent = link_entry.parent;
        let mut link_child_node_ref = Some(link_entry_id);
        let mut link_cycle = SiblingCycleGuard::new(link_expected_parent);
        while let Some(child_node_id) = link_child_node_ref {
            let fut = dump_node_recurse(
                link_state.clone(),
                linked_repository.clone(),
                child_node_id,
                link_expected_parent,
                subpath.clone(),
                depth + 1,
                max_depth,
                &mut link_cycle,
            );
            link_child_node_ref = fut.await?;
        }
    }
    if node.is_directory() && ((max_depth == 0) || (depth + 1 < max_depth)) {
        let mut child_node_ref = node.child();
        let mut child_cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_ref {
            let fut = dump_node_recurse(
                state.clone(),
                repository.clone(),
                child_node_id,
                node_id,
                subpath.clone(),
                depth + 1,
                max_depth,
                &mut child_cycle,
            );
            child_node_ref = fut.await?;
        }
    }
    subpath.pop();

    Ok(node.sibling())
}

#[allow(clippy::too_many_arguments)]
pub fn dump_node_recurse<'a>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    expected_parent: NodeID,
    subpath: RelativePathBuf,
    depth: usize,
    max_depth: usize,
    cycle: &'a mut SiblingCycleGuard,
) -> Pin<Box<dyn Future<Output = Result<Option<NodeID>, StateError>> + Send + 'a>> {
    Box::pin(dump_node(
        state,
        repository,
        node_id,
        expected_parent,
        subpath,
        depth,
        max_depth,
        cycle,
    ))
}
