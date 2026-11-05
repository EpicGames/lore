// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::commit;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::revision_state;
use crate::helper::test_store_create;

/// The node `revision` holds at `Dir`.
async fn directory_at(repository: &Arc<RepositoryContext>, revision: Hash) -> Node {
    let state = revision_state(repository, revision).await;
    let node = state
        .find_node_link(repository.clone(), "Dir")
        .await
        .expect("Failed to find the directory")
        .node;
    state
        .node(repository.clone(), node)
        .await
        .expect("Failed to read the directory")
}

/// `base` with `Dir/legacy` renamed in place to `.URC`, a name the read paths refuse, holding
/// `content`, and `Dir/a.txt` edited to 20 bytes, committed.
async fn commit_with_refused_name(
    repository: &Arc<RepositoryContext>,
    base: Hash,
    content: u64,
) -> Hash {
    let state = revision_state(repository, base).await;
    let directory = state
        .find_node_link(repository.clone(), "Dir")
        .await
        .expect("Failed to find the directory")
        .node;
    let legacy = state
        .find_subnode(repository.clone(), directory, hash_string("legacy"))
        .await
        .expect("Failed to find the legacy file");
    let block_index = NodeBlock::index(legacy);
    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await
        .expect("Failed to read the block");
    {
        let mut writer = block.write_for_tests();
        let node = writer.node(Node::index(legacy));
        let (offset, length) = (node.name_offset, node.name_length);
        node.name_hash = hash_string(".URC");
        node.address.hash = Hash::from_u64(content);
        let (offset, length) = writer
            .node_name_store_unchecked(".URC", offset, length)
            .expect("Failed to store the name");
        let node = writer.node(Node::index(legacy));
        node.name_offset = offset;
        node.name_length = length;
        writer.mark_dirty();
    }
    state.block_modified_for_tests(block, block_index);

    let edited = state
        .find_subnode(repository.clone(), directory, hash_string("a.txt"))
        .await
        .expect("Failed to find the edited file");
    let context = state
        .node(repository.clone(), edited)
        .await
        .expect("Failed to read the edited file")
        .address
        .context;
    state
        .node_modify_staged(
            repository,
            edited,
            &lore_revision::state::Content {
                mode: 0o644,
                size: 20,
                address: Address {
                    hash: Hash::from_u64(content + 100),
                    context,
                },
            },
        )
        .await
        .expect("Failed to edit and stage the file");
    commit(repository, &state, base, branch_id()).await
}

/// A child whose stored name the read paths refuse, as a revision written before a name rule can
/// hold, counts in its directory's address and size like any other child.
#[tokio::test]
async fn a_child_with_a_refused_name_counts_in_its_directory() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable_store, mutable_store);
        let state = State::new();
        let directory = add(
            &state,
            repository.clone(),
            ROOT_NODE,
            directory("Dir"),
            "Dir",
        )
        .await;
        add(
            &state,
            repository.clone(),
            directory,
            file("a.txt"),
            "a.txt",
        )
        .await;
        let legacy = Node {
            size: 5,
            ..file("legacy")
        };
        add(&state, repository.clone(), directory, legacy, "legacy").await;
        let base = commit(&repository, &state, Hash::default(), branch_id()).await;

        let first = directory_at(
            &repository,
            commit_with_refused_name(&repository, base, 1).await,
        )
        .await;
        let second = directory_at(
            &repository,
            commit_with_refused_name(&repository, base, 2).await,
        )
        .await;
        assert_eq!(first.size, 25);
        assert_ne!(first.address, second.address);
    }))
    .await
    .expect("Test task failed");
}
