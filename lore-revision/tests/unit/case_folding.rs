// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Revisions with every directory hashed under the rule folding the case of child names, which a
//! revision may hold.

use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;
use zerocopy::IntoBytes;

/// The address the rule folding the case of child names gives a directory holding `children`:
/// BLAKE3 over each child's name hash, mode, address, mount source and size, in the order of their
/// name hashes.
pub(crate) fn case_folding_address(mut children: Vec<Node>) -> Hash {
    children.sort_unstable_by_key(|child| child.name_hash);
    let mut hasher = blake3::Hasher::new();
    for child in children {
        hasher.update(&child.name_hash.to_le_bytes());
        hasher.update(&u32::from(child.mode).to_le_bytes());
        hasher.update(child.address.as_bytes());
        hasher.update(&child.mount_source().unwrap_or(ROOT_NODE).to_le_bytes());
        hasher.update(&child.size.to_le_bytes());
    }
    Hash::from(*hasher.finalize().as_bytes())
}

/// The children of `directory` in `state`.
pub(crate) async fn children(
    repository: &Arc<RepositoryContext>,
    state: &State,
    directory: NodeID,
) -> Vec<(NodeID, Node)> {
    let mut children = Vec::new();
    for child in state
        .node_children(repository.clone(), directory)
        .await
        .expect("Failed to list the children")
    {
        let node = state
            .node(repository.clone(), child)
            .await
            .expect("Failed to read a child");
        children.push((child, node));
    }
    children
}

/// The address and size `directory` takes under the rule folding case, with every directory
/// below it given the same, written into `state`.
async fn fold_directory(
    repository: &Arc<RepositoryContext>,
    state: &State,
    directory: NodeID,
) -> (Hash, u64) {
    let mut folded = Vec::new();
    let mut size = 0;
    for (child, mut node) in children(repository, state, directory).await {
        if node.is_directory() {
            (node.address.hash, node.size) =
                Box::pin(fold_directory(repository, state, child)).await;
            write_node(repository, state, child, node).await;
        }
        size += node.size;
        folded.push(node);
    }
    (case_folding_address(folded), size)
}

async fn write_node(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node_id: NodeID,
    node: Node,
) {
    let block_index = NodeBlock::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .expect("Failed to read a block");
    let dirtied = {
        let mut writer = block.write_for_tests();
        *writer.node(Node::index(node_id)) = node;
        writer.mark_dirty()
    };
    if dirtied {
        state.block_modified_for_tests(block, block_index);
    }
    state.mark_dirty();
}

/// `revision` with every directory hashed under the rule folding case, stored through
/// `write_token` as a revision of its own.
pub(crate) async fn case_folding(
    repository: &Arc<RepositoryContext>,
    write_token: &RepositoryWriteToken,
    revision: Hash,
) -> Hash {
    let state = State::deserialize(repository.clone(), revision)
        .await
        .expect("Failed to read the revision");
    let mut root = state
        .node(repository.clone(), ROOT_NODE)
        .await
        .expect("Failed to read the root");
    (root.address.hash, root.size) = fold_directory(repository, &state, ROOT_NODE).await;
    write_node(repository, &state, ROOT_NODE, root).await;
    state
        .update_tree_root_hash(repository.clone())
        .await
        .expect("Failed to update the root hash");
    state
        .serialize(repository.clone(), write_token)
        .await
        .expect("Failed to store the revision")
}
