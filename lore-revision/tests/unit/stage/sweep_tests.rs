// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The stage's call of the sweep of detached slots (C15): a stage frees, before it saves, a slot
//! a dropped add left in flight, taken from its block's free list and never published, in the
//! staged State an earlier session saved. Without the call the slot stays taken. The fixtures
//! serve the commits' calls too.

use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;

use crate::helper::stage_paths;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::helper::test_store_create;

/// Whether `node`'s slot holds no node in `state`: on its block's free list or past its count.
pub(crate) async fn freed(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
) -> bool {
    let block = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the slot's block loads");
    let reader = block.read();
    let data = reader.node_block();
    let index = Node::index(node);
    if index >= data.node_count as usize {
        return true;
    }
    let mut at = data.node_unused as usize;
    for _ in 0..data.node_unused_count {
        if at == index {
            return true;
        }
        at = data.node[at].sibling as usize;
    }
    false
}

/// Takes a slot from the block of `parent` in `state` and never publishes it, as an add
/// dropped right after its grab leaves one: `Discarded`, on no free list and in no chain.
pub(crate) async fn leak_a_slot(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
) -> NodeID {
    let index = NodeBlock::index(parent);
    let block = state
        .block(repository.clone(), index)
        .await
        .expect("the block loads");
    let leaked = state
        .try_grab_in(&block, index)
        .expect("the block has a slot to take");
    assert!(
        !freed(state, repository, leaked).await,
        "the slot taken holds a node"
    );
    leaked
}

/// The staged State, its slot leaked by a dropped add saved into the staged anchor as an
/// earlier session leaves one. The slot.
pub(crate) async fn staged_with_a_leaked_slot(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
) -> NodeID {
    let staged = lore_revision::instance::load_staged_revision(repository)
        .await
        .expect("the staged anchor reads")
        .expect("something is staged");
    let state = State::deserialize(repository.clone(), staged)
        .await
        .expect("the staged State loads");
    let leaked = leak_a_slot(&state, repository, ROOT_NODE).await;
    let saved = state
        .serialize(repository.clone(), token)
        .await
        .expect("the staged State saves");
    lore_revision::instance::store_staged_anchor(repository, saved)
        .await
        .expect("the staged anchor stores");
    leaked
}

/// A stage of a change, over a staged State an earlier session saved with a slot in flight:
/// the stage saves its State with the slot freed.
#[tokio::test]
async fn a_stage_frees_a_slot_left_in_flight_before_it_saves() {
    let (immutable, mutable, execution) =
        test_store_create().await.expect("Failed to create stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let instance =
            test_repository_create(immutable, mutable, RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
        let (repository, token) = (&instance.repository, &instance.write_token);
        test_file_write(&instance.path.join("a.file"), &[0, 1, 2]);
        stage_paths(repository, &instance.path, &["."], true, false).await;
        let leaked = staged_with_a_leaked_slot(repository, token).await;

        test_file_write(&instance.path.join("b.file"), &[3, 4, 5]);
        stage_paths(repository, &instance.path, &["."], true, false).await;

        let staged = lore_revision::instance::load_staged_revision(repository)
            .await
            .expect("the staged anchor reads")
            .expect("something is staged");
        let state = State::deserialize(repository.clone(), staged)
            .await
            .expect("the staged State loads");
        assert!(
            freed(&state, repository, leaked).await,
            "the stage freed slot {leaked} before it saved"
        );
    }))
    .await
    .expect("Task failed");
}
