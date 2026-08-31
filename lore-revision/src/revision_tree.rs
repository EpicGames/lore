// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Working-tree-free revision tree edits shared by the SDK handle verbs and
//! server-side changeset handlers.

use std::pin::Pin;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::types::Address;
use lore_error_set::prelude::*;
use lore_storage::hash::hash_string;

use crate::errors::StateErrors;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::interface::LoreNodeType;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeDelta;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::repository::RepositoryContext;
use crate::state;
use crate::state::State;

#[error_set]
pub enum RevisionTreeEditError {
    InvalidArguments,
}

impl EventError for RevisionTreeEditError {
    fn translated(&self) -> LoreError {
        match self {
            RevisionTreeEditError::InvalidArguments(_) => LoreError::InvalidArguments,
            RevisionTreeEditError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn invalid(reason: impl Into<String>) -> RevisionTreeEditError {
    RevisionTreeEditError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// Add a file, empty directory, or link beneath `parent_node_id` and mark it
/// staged. `kind` uses the public [`LoreNodeType`] numeric encoding.
pub async fn add_node(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node_id: NodeID,
    name: &[u8],
    kind: u32,
    mode: u16,
    size: u64,
    address: Address,
) -> Result<NodeID, RevisionTreeEditError> {
    if !parent_node_id.is_valid_or_root_node_id() {
        return Err(invalid("parent node id is invalid"));
    }

    let name = std::str::from_utf8(name).map_err(|_| invalid("name is not valid UTF-8"))?;
    if name.is_empty() {
        return Err(invalid("name is empty"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(invalid("name must not contain path separators"));
    }

    let flags = if kind == LoreNodeType::File as u32 {
        NodeFlags::File
    } else if kind == LoreNodeType::Directory as u32 {
        NodeFlags::NoFlags
    } else if kind == LoreNodeType::Link as u32 {
        NodeFlags::Link
    } else {
        return Err(invalid("kind is not a valid node type"));
    };

    if kind == LoreNodeType::Directory as u32
        && (size != 0 || !address.hash.is_zero() || !address.context.is_zero())
    {
        return Err(invalid(
            "a directory takes no size or address; both are computed at commit",
        ));
    }
    if kind == LoreNodeType::Link as u32 {
        if size != 0 {
            return Err(invalid("a link takes no size"));
        }
        if address.hash.is_zero() {
            return Err(invalid("a link requires a target revision in address.hash"));
        }
    }

    let parent = state
        .node(repository.clone(), parent_node_id)
        .await
        .map_err(|_| invalid("parent node id is unknown"))?;
    if parent.is_discarded() {
        return Err(invalid("parent node id resolves to a deleted node"));
    }
    if !parent.is_directory() {
        return Err(invalid(
            "parent node is not a directory in the handle's own tree",
        ));
    }
    if parent_node_id != ROOT_NODE {
        let parent_name = state
            .node_name_clone(repository.clone(), parent_node_id)
            .await
            .map_err(|error| {
                RevisionTreeEditError::internal_with_context(error, "State::node_name_clone")
            })?;
        if parent_name.is_empty() {
            return Err(invalid("parent node id does not resolve to a named node"));
        }
    }

    let name_hash = hash_string(name);
    match state
        .find_subnode(repository.clone(), parent_node_id, name_hash)
        .await
    {
        Ok(_) => return Err(invalid("name already exists under the parent")),
        Err(error) if error.is_node_not_found() => {}
        Err(error) => {
            return Err(RevisionTreeEditError::internal_with_context(
                error,
                "State::find_subnode",
            ));
        }
    }

    let node = Node {
        flags: flags.bits(),
        name_hash,
        mode,
        size,
        address,
        ..Default::default()
    };
    let node_id = state
        .node_add(repository.clone(), parent_node_id, node, name)
        .await
        .map_err(|error| RevisionTreeEditError::internal_with_context(error, "State::node_add"))?;
    state
        .node_mark(repository, node_id, NodeFlags::StagedAdd, true)
        .await
        .map_err(|error| RevisionTreeEditError::internal_with_context(error, "State::node_mark"))?;
    Ok(node_id)
}

/// Create exactly one empty directory. Missing ancestors are not created.
pub async fn create_directory(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node_id: NodeID,
    name: &[u8],
    mode: u16,
) -> Result<NodeID, RevisionTreeEditError> {
    add_node(
        state,
        repository,
        parent_node_id,
        name,
        LoreNodeType::Directory as u32,
        mode,
        0,
        Address::default(),
    )
    .await
}

/// Replace a file node's content hash, mode, and logical size while preserving
/// its existing file identity context.
pub async fn modify_file(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    mode: u16,
    size: u64,
    address: Address,
) -> Result<NodeID, RevisionTreeEditError> {
    if !node_id.is_valid_node_id() {
        return Err(invalid("node id is invalid"));
    }

    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .map_err(|_| invalid("node id is unknown"))?;
    let node = block.node(node_index);
    if node.is_discarded() {
        return Err(invalid("node id resolves to a deleted node"));
    }
    if !node.is_file() {
        return Err(invalid("node is not a leaf (file) node"));
    }
    if node.name_hash == 0 {
        return Err(invalid("node id does not resolve to a named node"));
    }
    if !address.context.is_zero() && address.context != node.address.context {
        return Err(invalid("address context does not match the node's file id"));
    }

    let file_id = node.address.context;
    let mark_flags = if node.is_staged_add() {
        NodeFlags::StagedAdd
    } else {
        NodeFlags::StagedModify
    };
    let dirtied = {
        let mut block_writer = block.write();
        let node = block_writer.node(node_index);
        node.address.hash = address.hash;
        node.address.context = file_id;
        node.mode = mode;
        node.size = size;
        block_writer.mark_dirty()
    };
    if dirtied {
        state.block_modified(block, block_index);
        state.mark_dirty();
    }
    state
        .node_mark(repository, node_id, mark_flags, true)
        .await
        .map_err(|error| RevisionTreeEditError::internal_with_context(error, "State::node_mark"))?;
    Ok(node_id)
}

fn mark_delete_subtree(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
) -> Pin<Box<dyn Future<Output = Result<(), StateErrors>> + Send>> {
    Box::pin(async move {
        let node = state.node(repository.clone(), node_id).await?;
        if node.is_staged_delete() {
            return Ok(());
        }
        state
            .node_mark(repository.clone(), node_id, NodeFlags::StagedDelete, true)
            .await?;
        if node.is_directory() {
            let mut child_node_iter = node.child();
            let mut cycle = SiblingCycleGuard::new(node_id);
            while let Some(child_node_id) = child_node_iter {
                mark_delete_subtree(state.clone(), repository.clone(), child_node_id).await?;
                let child_node = state.node(repository.clone(), child_node_id).await?;
                child_node.walk_step(child_node_id, node_id, &mut cycle)?;
                child_node_iter = child_node.sibling();
            }
        }
        Ok(())
    })
}

/// Delete a node and its in-tree descendants, returning the delta entries that
/// must be carried into `commit_tree` after the nodes are discarded.
pub async fn delete_node(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
) -> Result<Vec<NodeDelta>, RevisionTreeEditError> {
    if !node_id.is_valid_node_id() {
        return Err(invalid("node id is invalid (the root cannot be deleted)"));
    }
    let node = state
        .node(repository.clone(), node_id)
        .await
        .map_err(|_| invalid("node id is unknown"))?;
    if node.name_hash == 0 {
        return Err(invalid("node id does not resolve to a named node"));
    }
    if node.is_discarded() {
        return Err(invalid("node id resolves to a deleted node"));
    }

    mark_delete_subtree(state.clone(), repository.clone(), node_id)
        .await
        .map_err(|error| {
            RevisionTreeEditError::internal_with_context(error, "mark delete subtree")
        })?;

    let deltas = Arc::new(parking_lot::RwLock::new(Vec::new()));
    let recorder = deltas.clone();
    let handler = move |discarded_node_id: NodeID, flags: u16| {
        recorder
            .write()
            .push(NodeDelta::from_node_and_flags(discarded_node_id, flags));
    };

    if node.is_directory() {
        let mut child_node_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_iter {
            let child_node = state
                .node(repository.clone(), child_node_id)
                .await
                .map_err(|error| {
                    RevisionTreeEditError::internal_with_context(error, "read child node")
                })?;
            child_node
                .walk_step(child_node_id, node_id, &mut cycle)
                .map_err(|error| {
                    RevisionTreeEditError::internal_with_context(error, "walk child chain")
                })?;
            let next_sibling = child_node.sibling();
            state::node_discard_nopatch(
                state.clone(),
                repository.clone(),
                child_node_id,
                true,
                true,
                handler.clone(),
            )
            .await
            .map_err(|error| {
                RevisionTreeEditError::internal_with_context(error, "discard subtree node")
            })?;
            child_node_iter = next_sibling;
        }
    }

    state::node_discard_patch(state, repository, node_id, handler)
        .await
        .map_err(|error| {
            RevisionTreeEditError::internal_with_context(error, "discard deleted node")
        })?;
    let result = deltas.read().clone();
    Ok(result)
}

fn mark_children_moved(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    move_flag: NodeFlags,
) -> Pin<Box<dyn Future<Output = Result<(), StateErrors>> + Send>> {
    Box::pin(async move {
        let children = state.node_children(repository.clone(), parent_node).await?;
        for child_id in children {
            let child_node = state.node(repository.clone(), child_id).await?;
            let child_flag = if child_node.is_staged_add() {
                NodeFlags::StagedAdd
            } else {
                move_flag
            };
            state
                .node_mark(repository.clone(), child_id, child_flag, false)
                .await?;
            if child_node.is_directory() {
                mark_children_moved(state.clone(), repository.clone(), child_id, move_flag).await?;
            }
        }
        Ok(())
    })
}

/// Reparent and/or rename a node while preserving its file identity.
pub async fn move_node(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    destination_parent_id: NodeID,
    dst_name: &[u8],
) -> Result<NodeID, RevisionTreeEditError> {
    if !node_id.is_valid_node_id() {
        return Err(invalid("node id is invalid (the root cannot be moved)"));
    }
    if !destination_parent_id.is_valid_or_root_node_id() {
        return Err(invalid("destination parent node id is invalid"));
    }
    let dst_name = std::str::from_utf8(dst_name)
        .map_err(|_| invalid("destination name is not valid UTF-8"))?;
    if dst_name.is_empty() {
        return Err(invalid("destination name is empty"));
    }
    if dst_name.contains('/') || dst_name.contains('\\') {
        return Err(invalid("destination name must not contain path separators"));
    }

    let node = state
        .node(repository.clone(), node_id)
        .await
        .map_err(|_| invalid("node id is unknown"))?;
    if node.name_hash == 0 {
        return Err(invalid("node id does not resolve to a named node"));
    }
    if node.is_discarded() {
        return Err(invalid("node id resolves to a deleted node"));
    }

    let destination = state
        .node(repository.clone(), destination_parent_id)
        .await
        .map_err(|_| invalid("destination parent node id is unknown"))?;
    if destination.is_discarded() {
        return Err(invalid("destination parent id resolves to a deleted node"));
    }
    if !destination.is_directory() {
        return Err(invalid(
            "destination parent is not a directory in the handle's own tree",
        ));
    }
    if destination_parent_id != ROOT_NODE {
        let name = state
            .node_name_clone(repository.clone(), destination_parent_id)
            .await
            .map_err(|error| {
                RevisionTreeEditError::internal_with_context(error, "State::node_name_clone")
            })?;
        if name.is_empty() {
            return Err(invalid(
                "destination parent id does not resolve to a named node",
            ));
        }
    }

    let mut ancestor = destination_parent_id;
    while ancestor.is_valid_node_id() {
        if ancestor == node_id {
            return Err(invalid("destination lives inside the moved subtree"));
        }
        let ancestor_node = state
            .node(repository.clone(), ancestor)
            .await
            .map_err(|_| invalid("destination parent chain is unreadable"))?;
        ancestor = ancestor_node.parent;
    }

    let dst_name_hash = hash_string(dst_name);
    if node.parent == destination_parent_id && node.name_hash == dst_name_hash {
        return Err(invalid("source and destination are the same"));
    }
    match state
        .find_subnode(repository.clone(), destination_parent_id, dst_name_hash)
        .await
    {
        Ok(existing) if existing != node_id => {
            return Err(invalid("destination name already exists under the parent"));
        }
        Ok(_) => {}
        Err(error) if error.is_node_not_found() => {}
        Err(error) => {
            return Err(RevisionTreeEditError::internal_with_context(
                error,
                "State::find_subnode",
            ));
        }
    }

    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .map_err(|error| RevisionTreeEditError::internal_with_context(error, "read node block"))?;
    let mut node = block.node(node_index);

    if node.parent != destination_parent_id {
        let old_parent_id = node.parent;
        let old_parent_block_index = NodeBlock::index(old_parent_id);
        let old_parent_node_index = Node::index(old_parent_id);
        let old_parent_block = state
            .block(repository.clone(), old_parent_block_index)
            .await
            .map_err(|error| {
                RevisionTreeEditError::internal_with_context(error, "read old parent block")
            })?;
        let old_parent = old_parent_block.node(old_parent_node_index);
        if old_parent.child == node_id {
            let dirtied = {
                let mut block_writer = old_parent_block.write();
                block_writer.node(old_parent_node_index).child = node.sibling;
                block_writer.mark_dirty()
            };
            if dirtied {
                state.block_modified(old_parent_block, old_parent_block_index);
                state.mark_dirty();
            }
        } else {
            let mut found = false;
            let mut child_id = old_parent.child().unwrap_or_default();
            let mut cycle = SiblingCycleGuard::new(old_parent_id);
            while child_id.is_valid_node_id() {
                let child = state
                    .node(repository.clone(), child_id)
                    .await
                    .map_err(|error| {
                        RevisionTreeEditError::internal_with_context(
                            error,
                            "walk old sibling chain",
                        )
                    })?;
                child
                    .walk_step(child_id, old_parent_id, &mut cycle)
                    .map_err(|error| {
                        RevisionTreeEditError::internal_with_context(
                            error,
                            "walk old sibling chain",
                        )
                    })?;
                let Some(sibling) = child.sibling() else {
                    break;
                };
                if sibling == node_id {
                    let child_block_index = NodeBlock::index(child_id);
                    let child_node_index = Node::index(child_id);
                    let child_block = state
                        .block(repository.clone(), child_block_index)
                        .await
                        .map_err(|error| {
                            RevisionTreeEditError::internal_with_context(
                                error,
                                "read sibling block",
                            )
                        })?;
                    let dirtied = {
                        let mut block_writer = child_block.write();
                        block_writer.node(child_node_index).sibling = node.sibling;
                        block_writer.mark_dirty()
                    };
                    if dirtied {
                        state.block_modified(child_block, child_block_index);
                        state.mark_dirty();
                    }
                    found = true;
                    break;
                }
                child_id = sibling;
            }
            if !found {
                return Err(RevisionTreeEditError::internal(
                    "node not found in its parent's child chain",
                ));
            }
        }

        let dst_block_index = NodeBlock::index(destination_parent_id);
        let dst_node_index = Node::index(destination_parent_id);
        let dst_block = state
            .block(repository.clone(), dst_block_index)
            .await
            .map_err(|error| {
                RevisionTreeEditError::internal_with_context(error, "read destination block")
            })?;
        let sibling_node_id = dst_block.node(dst_node_index).child;
        let dirtied = {
            let mut block_writer = dst_block.write();
            block_writer.node(dst_node_index).child = node_id;
            block_writer.mark_dirty()
        };
        if dirtied {
            state.block_modified(dst_block, dst_block_index);
            state.mark_dirty();
        }
        node.sibling = sibling_node_id;
    }

    node.parent = destination_parent_id;
    if node.name_hash != dst_name_hash {
        block
            .deserialize_nametable(repository.clone())
            .await
            .map_err(|error| {
                RevisionTreeEditError::internal_with_context(error, "deserialize name table")
            })?;
        node.name_hash = dst_name_hash;
        let (name_offset, name_length) = {
            let mut block_writer = block.write();
            block_writer.node_name_store(dst_name, node.name_offset, node.name_length)
        }
        .map_err(|error| {
            RevisionTreeEditError::internal_with_context(error, "store renamed node name")
        })?;
        node.name_offset = name_offset;
        node.name_length = name_length;
    }

    let dirtied = {
        let mut block_writer = block.write();
        *block_writer.node(node_index) = node;
        block_writer.mark_dirty()
    };
    if dirtied {
        state.block_modified(block, block_index);
        state.mark_dirty();
    }

    let move_flag = if node.is_staged_add() {
        NodeFlags::StagedAdd
    } else {
        NodeFlags::StagedMove
    };
    state
        .node_mark(repository.clone(), node_id, move_flag, true)
        .await
        .map_err(|error| RevisionTreeEditError::internal_with_context(error, "State::node_mark"))?;
    if node.is_directory() {
        mark_children_moved(state, repository, node_id, move_flag)
            .await
            .map_err(|error| {
                RevisionTreeEditError::internal_with_context(error, "mark children moved")
            })?;
    }
    Ok(node_id)
}

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_storage::immutable_store::ImmutableStore;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::immutable_store::create as create_immutable;
    use lore_storage::local::mutable_store::LocalMutableStore;
    use lore_storage::local::mutable_store::MutableStoreSettings;
    use lore_storage::mutable_store::MutableStore;

    use super::*;
    use crate::node::INVALID_NODE;

    async fn fixture() -> (Arc<State>, Arc<RepositoryContext>) {
        let immutable = create_immutable(
            Option::<std::path::PathBuf>::None,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("in-memory immutable store");
        let mutable: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Option::<&std::path::Path>::None,
                MutableStoreSettings::default(),
                immutable.clone(),
            )
            .await
            .expect("in-memory mutable store"),
        );
        let immutable: Arc<dyn ImmutableStore> = immutable;
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable,
            mutable,
            Partition::from(Context::from([0x44; 16])),
        ));
        let state = State::deserialize(repository.clone(), Default::default())
            .await
            .expect("empty state");
        (state, repository)
    }

    #[tokio::test]
    async fn create_directory_adds_a_staged_empty_node() {
        let (state, repository) = fixture().await;
        let node_id = create_directory(
            state.clone(),
            repository.clone(),
            ROOT_NODE,
            b"empty",
            0o755,
        )
        .await
        .expect("create directory");
        let node = state
            .node(repository, node_id)
            .await
            .expect("directory node");
        assert!(node.is_directory());
        assert!(node.is_staged_add());
        assert!(!node.child.is_valid_node_id());
        assert_eq!(node.size, 0);
        assert_eq!(node.address, Address::default());
    }

    #[test]
    fn invalid_node_constant_remains_reserved() {
        assert!(!INVALID_NODE.is_valid_or_root_node_id());
    }
}
