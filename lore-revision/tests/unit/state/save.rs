// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for a save's bookkeeping: its threading of the tree's unused chain, where a block that
//! freed a slot while full, which no grab reaches, joins the chain behind its head at the next
//! save, once, so a full head never ends up behind another block; the name table's repack; and
//! the State's dirty mark, set with every registration and clear once a save wrote everything
//! registered.

use std::collections::BTreeSet;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::NameChange;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use super::InMemoryMarker;
use super::add;
use super::directory;
use super::file;
use super::fill;
use super::with_repository;
use super::with_state;

/// The blocks the tree's unused chain reaches, in chain order, asserting none twice.
async fn unused_chain(state: &State, repository: &Arc<RepositoryContext>) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut at = state.unused_head();
    while (at as usize) < state.block_count() {
        assert!(
            seen.insert(at),
            "block {at} is on the unused chain twice, after {chain:?}"
        );
        chain.push(at as usize);
        let block = state
            .block(repository.clone(), at as usize)
            .await
            .expect("the block");
        at = block.read().node_block().block_unused_next;
    }
    chain
}

/// Saves `state`, checking that the unused chain it stored is the one it holds: a block threaded
/// after its image would be on the live chain alone. Answers the signature.
async fn save(state: &Arc<State>, repository: &Arc<RepositoryContext>) -> Hash {
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let signature = state
        .serialize(repository.clone(), &token)
        .await
        .expect("the State saves");
    let stored = State::deserialize(repository.clone(), signature)
        .await
        .expect("the State loads");
    stored
        .tree(repository.clone())
        .await
        .expect("the stored tree loads");
    assert_eq!(
        unused_chain(&stored, repository).await,
        unused_chain(state, repository).await,
        "the save stored the unused chain the State holds"
    );
    signature
}

/// The first file of `files` in block `block`.
fn in_block(files: &[NodeID], block: usize) -> NodeID {
    *files
        .iter()
        .find(|id| NodeBlock::index(**id) == block)
        .expect("a file in the block")
}

/// Holds blocks `0..count` in memory, as the block cache or a listing holds one: a block only its
/// registration held is loaded again after a save, its runtime state clear.
async fn hold(
    state: &State,
    repository: &Arc<RepositoryContext>,
    count: usize,
) -> Vec<Arc<NodeBlock>> {
    let mut held = Vec::new();
    for index in 0..count {
        held.push(
            state
                .block(repository.clone(), index)
                .await
                .expect("the block"),
        );
    }
    held
}

async fn discard(state: &Arc<State>, repository: &Arc<RepositoryContext>, id: NodeID) {
    lore_revision::state::node_discard_fused(state.clone(), repository.clone(), id)
        .await
        .expect("the discard");
}

/// Blocks that free a slot once off the chain join it right behind its head at the next save,
/// each once: a later save, the block changed again, threads it no second time, which would close
/// a cycle. The head, full, keeps its place: the next grab retires it onto them.
#[tokio::test]
async fn a_block_freeing_a_slot_off_the_unused_chain_joins_it_once() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let files = fill(&state, &repository, d, 2).await;
        let _held = hold(&state, &repository, 2).await;
        assert_eq!(unused_chain(&state, &repository).await, vec![2]);

        discard(&state, &repository, in_block(&files, 0)).await;
        save(&state, &repository).await;
        assert_eq!(
            unused_chain(&state, &repository).await,
            vec![2, 0],
            "block 0 joined the chain behind its head"
        );
        discard(&state, &repository, in_block(&files, 1)).await;
        save(&state, &repository).await;
        assert_eq!(
            unused_chain(&state, &repository).await,
            vec![2, 1, 0],
            "block 1 joined the chain behind its head"
        );
        discard(&state, &repository, files[4]).await;
        save(&state, &repository).await;

        assert_eq!(
            unused_chain(&state, &repository).await,
            vec![2, 1, 0],
            "block 0 is on the chain once"
        );
    })
    .await;
}

/// Blocks that free slots once off the chain all join it in one save, behind its head and in
/// front of what followed the head.
#[tokio::test]
async fn blocks_freeing_slots_off_the_unused_chain_join_it_in_one_save() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let files = fill(&state, &repository, d, 3).await;
        let _held = hold(&state, &repository, 3).await;
        discard(&state, &repository, in_block(&files, 2)).await;
        save(&state, &repository).await;
        assert_eq!(unused_chain(&state, &repository).await, vec![3, 2]);

        discard(&state, &repository, in_block(&files, 0)).await;
        discard(&state, &repository, in_block(&files, 1)).await;
        save(&state, &repository).await;

        assert_eq!(unused_chain(&state, &repository).await, vec![3, 1, 0, 2]);
    })
    .await;
}

/// A save threading blocks behind a head not in memory loads the head and writes it with them:
/// left to the next save, its link would be in memory alone and keep the State dirty. The head,
/// full, keeps its place, never behind another block, where no grab retires it and the slot it
/// later freed would thread it a second time, a cycle. Blocks 0 and 1 hold directories of their
/// own, so a discard in block 0 walks no chain through block 1.
#[tokio::test]
async fn a_save_threading_behind_a_head_not_in_memory_loads_and_writes_it() {
    with_state(|repository, state| async move {
        let a = add(&state, &repository, ROOT_NODE, directory("a"), "a").await;
        let in_zero = fill(&state, &repository, a, 0).await;
        let b = add(&state, &repository, ROOT_NODE, directory("b"), "b").await;
        let in_one = fill(&state, &repository, b, 1).await;
        let _held = hold(&state, &repository, 1).await;
        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        state
            .serialize(repository.clone(), &token)
            .await
            .expect("the State saves");
        discard(&state, &repository, in_zero[3]).await;
        assert!(
            state.resident_block(1).is_none(),
            "block 1, the head, clean, left memory"
        );

        let signature = save(&state, &repository).await;

        assert_eq!(unused_chain(&state, &repository).await, vec![1, 0]);
        assert!(!state.is_dirty(), "the save left the State clean");
        assert_eq!(
            save(&state, &repository).await,
            signature,
            "a second save wrote nothing"
        );
        discard(&state, &repository, in_one[3]).await;
        save(&state, &repository).await;
        assert_eq!(
            unused_chain(&state, &repository).await,
            vec![1, 0],
            "block 1, freeing a slot as the head, keeps its place"
        );
    })
    .await;
}

/// The chain's head, full, frees a slot and fills again, as a grab takes it back: a save finds it
/// the head still and spends its mark, which would otherwise keep it registered. Each discard's
/// predecessor in `d`'s chain lies in its own block, so block 1, freeing first, registers first.
#[tokio::test]
async fn a_head_marked_and_full_again_stays_the_head() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let files = fill(&state, &repository, d, 1).await;
        let _held = hold(&state, &repository, 2).await;
        save(&state, &repository).await;
        assert_eq!(unused_chain(&state, &repository).await, vec![1]);
        let (in_one, in_one_later) = (files[files.len() - 2], files[files.len() - 3]);
        assert_eq!(
            (NodeBlock::index(in_one), NodeBlock::index(in_one_later)),
            (1, 1)
        );
        discard(&state, &repository, in_one).await;
        discard(&state, &repository, in_block(&files, 0)).await;
        add(&state, &repository, d, file("refill"), "refill").await;
        save(&state, &repository).await;
        assert_eq!(unused_chain(&state, &repository).await, vec![1, 0]);
        assert!(
            !state.is_dirty(),
            "the save spent block 1's mark: kept, it would keep the block registered"
        );

        discard(&state, &repository, in_one_later).await;
        save(&state, &repository).await;

        assert_eq!(unused_chain(&state, &repository).await, vec![1, 0]);
    })
    .await;
}

/// A marked block full again, its slot taken back while it was the head, then retired by a grab,
/// is not threaded at the next save, its mark spent: threaded, it would sit full below the head,
/// where no grab retires it, and the slot it later frees would thread it a second time, a cycle.
/// Threaded once it frees a slot again.
#[tokio::test]
async fn a_marked_block_full_again_off_the_head_is_not_threaded() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let files = fill(&state, &repository, d, 1).await;
        let held = hold(&state, &repository, 2).await;
        save(&state, &repository).await;
        let (in_one, in_one_later) = (files[files.len() - 2], files[files.len() - 3]);
        discard(&state, &repository, in_one).await;
        discard(&state, &repository, in_block(&files, 0)).await;
        add(&state, &repository, d, file("refill"), "refill").await;
        // Block 1 full again: the next grab retires it, and pushes a fresh block 2.
        add(&state, &repository, d, file("fresh"), "fresh").await;
        assert_eq!(unused_chain(&state, &repository).await, vec![2]);
        assert!(
            held[1].read().has_first_unused_node() && held[1].read().is_full(),
            "block 1 is full again, still marked"
        );

        save(&state, &repository).await;

        assert_eq!(
            unused_chain(&state, &repository).await,
            vec![2, 0],
            "block 1, full, stays off the chain"
        );
        assert!(!state.is_dirty(), "the save spent block 1's mark");
        discard(&state, &repository, in_one_later).await;
        save(&state, &repository).await;
        assert_eq!(unused_chain(&state, &repository).await, vec![2, 1, 0]);
    })
    .await;
}

/// The chain's head, full but not yet left behind by a grab, frees a slot: a save finds it on
/// the chain already and threads it no second time.
#[tokio::test]
async fn a_full_head_freeing_a_slot_stays_on_the_unused_chain_once() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let files = fill(&state, &repository, d, 0).await;
        assert_eq!(
            unused_chain(&state, &repository).await,
            vec![0],
            "block 0, full, is still the chain's head"
        );

        discard(&state, &repository, files[3]).await;
        save(&state, &repository).await;

        assert_eq!(
            unused_chain(&state, &repository).await,
            vec![0],
            "block 0 is on the chain once"
        );
        assert!(
            !state.is_dirty(),
            "the save spent the head's mark: kept, it would keep the block registered"
        );
    })
    .await;
}

/// A save repacks a name table a store grew, dropping the names of the slots freed: once
/// reloaded, a block whose freed slot a new name took stores only its live names.
#[tokio::test]
async fn a_save_repacks_a_grown_name_table() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        add(&state, &repository, d, file("a"), "a").await;
        let freed = add(&state, &repository, d, file("bb"), "bb").await;
        add(&state, &repository, d, file("ccc"), "ccc").await;
        save(&state, &repository).await;
        discard(&state, &repository, freed).await;
        let signature = save(&state, &repository).await;
        // A load drops the deferral a discard sets, which keeps a freed name for history.
        let loaded = State::deserialize(repository.clone(), signature)
            .await
            .expect("the State loads");
        let d = loaded
            .find_subnode(repository.clone(), ROOT_NODE, hash_string("d"))
            .await
            .expect("d");
        let taken = add(&loaded, &repository, d, file("dddd"), "dddd").await;
        assert_eq!(taken, freed, "the new file took the freed slot");

        let signature = save(&loaded, &repository).await;

        let stored = State::deserialize(repository.clone(), signature)
            .await
            .expect("the State loads");
        let block = stored.block(repository.clone(), 0).await.expect("block 0");
        block
            .deserialize_nametable_for_tests(repository.clone())
            .await
            .expect("the name table loads");
        assert_eq!(
            block.read().clone_name_table().len(),
            ["d", "a", "ccc", "dddd"]
                .iter()
                .map(|name| name.len())
                .sum::<usize>(),
            "the stored table holds the live names alone"
        );
    })
    .await;
}

/// A block registered for the next save marks the State dirty with it, whatever registers it: a
/// save of a clean State answers its last signature and writes nothing, so a block registered
/// under it would go into no save.
#[tokio::test]
async fn a_block_registered_under_a_clean_state_marks_it_dirty() {
    with_state(|repository, state| async move {
        add(&state, &repository, ROOT_NODE, file("f"), "f").await;
        save(&state, &repository).await;
        assert!(!state.is_dirty(), "the save left the State clean");
        let block = state.block(repository.clone(), 0).await.expect("block 0");

        state.block_modified_for_tests(block, 0);

        assert!(state.is_dirty(), "the registration marked the State dirty");
    })
    .await;
}

/// A block registered twice is registered once, and a save's release drops the registration of
/// a block it wrote clean, its registered bit with it: an edit after the save registers it again,
/// and the next save writes it. The same for a file metadata block.
#[tokio::test]
async fn a_block_is_registered_once_and_again_after_its_save() {
    with_state(|repository, state| async move {
        let f = add(&state, &repository, ROOT_NODE, file("f"), "f").await;
        save(&state, &repository).await;
        let index = NodeBlock::index(f);
        assert_eq!(state.block_registrations(index), 0, "the save released it");
        let block = state
            .block(repository.clone(), index)
            .await
            .expect("the block");
        let slot = lore_revision::node::node_to_file_metadata(f);
        let metadata_index = NodeFileMetadataBlock::index(slot);
        let metadata = state
            .block_file_metadata(repository.clone(), metadata_index)
            .await
            .expect("the file metadata block");
        let edit = |size: u64, digest: u8| {
            state.node_change_owned(&block, f, |node| {
                node.set_size(size);
                true
            });
            {
                let mut writer = metadata.write();
                writer.node(NodeFileMetadata::index(slot)).metadata = [digest; 32].into();
                writer.mark_dirty();
            }
            state.block_file_metadata_modified(metadata.clone(), metadata_index);
        };

        let bits = || (block.is_registered(), metadata.is_registered());
        assert_eq!(
            bits(),
            (false, false),
            "the bits clear with the registrations"
        );

        edit(5, 1);
        state.block_modified_for_tests(block.clone(), index);
        state.block_file_metadata_modified(metadata.clone(), metadata_index);
        assert_eq!(state.block_registrations(index), 1);
        assert_eq!(state.block_file_metadata_registrations(metadata_index), 1);
        assert_eq!(bits(), (true, true));
        assert_eq!(state.registration_faults(), Vec::<String>::new());

        save(&state, &repository).await;
        assert_eq!(state.block_registrations(index), 0);
        assert_eq!(state.block_file_metadata_registrations(metadata_index), 0);
        assert_eq!(bits(), (false, false), "the release dropped the bits");
        edit(9, 2);
        assert_eq!(state.block_registrations(index), 1, "registered again");
        assert_eq!(
            state.block_file_metadata_registrations(metadata_index),
            1,
            "registered again"
        );
        assert_eq!(bits(), (true, true));
        assert_eq!(state.registration_faults(), Vec::<String>::new());

        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let signature = state
            .serialize(repository.clone(), &token)
            .await
            .expect("the State saves");
        let loaded = State::deserialize(repository.clone(), signature)
            .await
            .expect("the State loads");
        let node = loaded
            .node(repository.clone(), f)
            .await
            .expect("the file reads");
        assert_eq!(node.size, 9, "the edit after the first save was written");
        let loaded_metadata = loaded
            .block_file_metadata(repository.clone(), metadata_index)
            .await
            .expect("the file metadata block");
        assert_eq!(
            loaded_metadata
                .read()
                .node(NodeFileMetadata::index(slot))
                .metadata,
            Hash::from([2u8; 32]),
            "the metadata edit after the first save was written"
        );
    })
    .await;
}

/// A State saved with block 0 full and directory `d` holding file `g` in block 1, loaded again
/// to rehash its names, and block 1 loaded: registered by the load, its bit claimed, under a
/// State left clean. Answers the loaded State, `g`, and block 1.
async fn rehash_loaded(
    repository: &Arc<RepositoryContext>,
) -> (Arc<State>, NodeID, Arc<NodeBlock>) {
    let state = State::new();
    fill(&state, repository, ROOT_NODE, 0).await;
    let d = add(&state, repository, ROOT_NODE, directory("d"), "d").await;
    let g = add(&state, repository, d, file("g"), "g").await;
    assert_eq!(
        (NodeBlock::index(d), NodeBlock::index(g)),
        (1, 1),
        "both lie past the full block 0"
    );
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let signature = state
        .serialize(repository.clone(), &token)
        .await
        .expect("the State saves");
    let loaded = State::deserialize(repository.clone(), signature)
        .await
        .expect("the State loads");
    loaded.force_rehash_names();
    assert!(loaded.resident_block(1).is_none(), "not loaded yet");
    let block = loaded
        .block(repository.clone(), 1)
        .await
        .expect("the block");
    assert_eq!(loaded.block_registrations(1), 1, "the load registered it");
    assert!(block.is_registered(), "the load claimed its bit");
    assert_eq!(loaded.registration_faults(), Vec::<String>::new());
    assert!(!loaded.is_dirty(), "the load left the State clean");
    (loaded, g, block)
}

/// A block a name rehash registered under a clean State takes an edit turning it dirty: the edit
/// finds it registered, registers nothing more and marks the State, so the next save writes it.
#[tokio::test]
async fn a_block_a_name_rehash_registered_takes_an_edit_into_the_next_save() {
    with_repository(|repository| async move {
        let (loaded, g, block) = rehash_loaded(&repository).await;

        assert!(loaded.node_change_owned(&block, g, |node| {
            node.set_size(7);
            true
        }));

        assert_eq!(loaded.block_registrations(1), 1, "registered once");
        assert!(loaded.is_dirty(), "the edit marked the State");
        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let saved = loaded
            .serialize(repository.clone(), &token)
            .await
            .expect("the State saves again");
        let reloaded = State::deserialize(repository.clone(), saved)
            .await
            .expect("the State loads again");
        let node = reloaded
            .node(repository.clone(), g)
            .await
            .expect("the file reads");
        assert_eq!(node.size, 7, "the save wrote the edit");
    })
    .await;
}

/// A rename turning a block a name rehash registered dirty, through the locks a relocation
/// releases, registers it no second time and marks the State.
#[tokio::test]
async fn a_rename_in_a_block_a_name_rehash_registered_registers_it_once() {
    with_repository(|repository| async move {
        let (loaded, g, _block) = rehash_loaded(&repository).await;

        loaded
            .node_rename(repository.clone(), g, NameChange::Rename("renamed"))
            .await
            .expect("the rename");

        assert_eq!(loaded.block_registrations(1), 1, "registered once");
        assert_eq!(loaded.registration_faults(), Vec::<String>::new());
        assert!(loaded.is_dirty(), "the rename marked the State");
    })
    .await;
}

/// A file metadata block registered for the next save marks the State dirty with it, as a node
/// block does.
#[tokio::test]
async fn a_file_metadata_block_registered_under_a_clean_state_marks_it_dirty() {
    with_state(|repository, state| async move {
        add(&state, &repository, ROOT_NODE, file("f"), "f").await;
        save(&state, &repository).await;
        assert!(!state.is_dirty(), "the save left the State clean");
        let block = state
            .block_file_metadata(repository.clone(), 0)
            .await
            .expect("file metadata block 0");

        state.block_file_metadata_modified(block, 0);

        assert!(state.is_dirty(), "the registration marked the State dirty");
    })
    .await;
}

/// A save of a dirty file metadata block leaves it clean, and the State with it: a second save
/// writes nothing and answers the first one's signature.
#[tokio::test]
async fn a_saved_file_metadata_block_is_clean() {
    with_state(|repository, state| async move {
        let f = add(&state, &repository, ROOT_NODE, file("f"), "f").await;
        save(&state, &repository).await;
        let slot = lore_revision::node::node_to_file_metadata(f);
        let block = state
            .block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(slot))
            .await
            .expect("the file metadata block");
        {
            let mut writer = block.write();
            writer.node(NodeFileMetadata::index(slot)).metadata = [7u8; 32].into();
            assert!(writer.mark_dirty(), "the write turned the block dirty");
        }
        state.block_file_metadata_modified(block.clone(), NodeFileMetadataBlock::index(slot));

        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let signature = state
            .serialize(repository.clone(), &token)
            .await
            .expect("the State saves");

        assert!(!block.read().is_dirty(), "the save left the block clean");
        assert!(!state.is_dirty(), "the save left the State clean");
        assert_eq!(
            state
                .serialize(repository.clone(), &token)
                .await
                .expect("the State saves"),
            signature,
            "a second save wrote nothing"
        );
    })
    .await;
}

/// A save repacks a table a name was appended to, dropping the name of a node a discard freed,
/// though the block stayed in memory from the discard to the save.
#[tokio::test]
async fn a_save_drops_a_name_a_discard_freed() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        add(&state, &repository, d, file("a"), "a").await;
        let freed = add(&state, &repository, d, file("bb"), "bb").await;
        add(&state, &repository, d, file("ccc"), "ccc").await;
        save(&state, &repository).await;
        discard(&state, &repository, freed).await;
        let taken = add(&state, &repository, d, file("dddd"), "dddd").await;
        assert_eq!(taken, freed, "the new file took the freed slot");

        let signature = save(&state, &repository).await;

        let stored = State::deserialize(repository.clone(), signature)
            .await
            .expect("the State loads");
        let block = stored.block(repository.clone(), 0).await.expect("block 0");
        block
            .deserialize_nametable_for_tests(repository.clone())
            .await
            .expect("the name table loads");
        assert_eq!(
            block.read().clone_name_table().len(),
            ["d", "a", "ccc", "dddd"]
                .iter()
                .map(|name| name.len())
                .sum::<usize>(),
            "the stored table drops the freed name"
        );
    })
    .await;
}
