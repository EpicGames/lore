// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The test store's evictions and failures: `State::evict_blocks_for_tests` drops what the State
//! keeps in memory for reuse and nothing it needs, and `FailingStore` fails reads as told, so a
//! walk can meet a block it must load between two of its steps.

use std::sync::Arc;

use lore_base::types::Address;
use lore_base::types::Partition;
use lore_revision::node;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_storage::ImmutableStore;

use crate::harness::catalogue::ops;
use crate::harness::faults::FailingStore;
use crate::harness::fixtures;
use crate::harness::fixtures::Tree;
use crate::harness::log::Collector;
use crate::harness::log::recorded;

/// A clean block the State alone keeps leaves memory, and reads back the same from the store.
#[tokio::test]
async fn eviction_drops_a_clean_block_nothing_else_holds() {
    crate::in_context(async {
        let tree = Tree::spread(&["a/x"], "fill", &["b/y"])
            .await
            .reloaded()
            .await;
        let y = tree.id("b/y");
        let index = NodeBlock::index(y);
        let before = tree
            .state
            .node(tree.repository.clone(), y)
            .await
            .expect("'b/y' reads");
        assert!(
            tree.state.resident_block(index).is_some(),
            "the read keeps the block"
        );

        tree.state.evict_blocks_for_tests();

        assert!(
            tree.state.resident_block(index).is_none(),
            "nothing else holds the clean block"
        );
        let after = tree
            .state
            .node(tree.repository.clone(), y)
            .await
            .expect("'b/y' reads again");
        assert_eq!(
            (
                after.parent,
                after.name_hash,
                after.address.hash,
                after.flags
            ),
            (
                before.parent,
                before.name_hash,
                before.address.hash,
                before.flags
            ),
            "the block reads back as it was"
        );
    })
    .await;
}

/// A block whose `Arc` a caller holds stays in memory through an eviction, the same block.
#[tokio::test]
async fn eviction_keeps_a_block_a_caller_holds() {
    crate::in_context(async {
        let tree = Tree::spread(&["a/x"], "fill", &["b/y"])
            .await
            .reloaded()
            .await;
        let index = NodeBlock::index(tree.id("b/y"));
        let held = tree
            .state
            .block(tree.repository.clone(), index)
            .await
            .expect("the block loads");

        tree.state.evict_blocks_for_tests();

        let resident = tree
            .state
            .resident_block(index)
            .expect("the held block stays");
        assert!(Arc::ptr_eq(&held, &resident), "the same block");
    })
    .await;
}

/// A dirty block stays in memory, registered for the next save, though nothing else holds it.
#[tokio::test]
async fn eviction_keeps_a_dirty_block() {
    crate::in_context(async {
        let tree = Tree::spread(&["a/x"], "fill", &["b/y"])
            .await
            .reloaded()
            .await;
        let y = tree.id("b/y");
        tree.state
            .node_mark(
                &tree.repository,
                y,
                Mark::dirty(
                    NodeFlags::DirtyModify,
                    Ext::Set(NodeExtFlags::NoFlags),
                    false,
                ),
            )
            .await
            .expect("the mark");

        tree.state.evict_blocks_for_tests();

        let block = tree
            .state
            .resident_block(NodeBlock::index(y))
            .expect("the dirty block stays");
        assert!(block.is_registered(), "registered for the next save");
        assert!(block.node(Node::index(y)).is_dirty(), "with its mark");
    })
    .await;
}

/// The reads one load of a block not in memory takes, on a State loaded afresh through a failing
/// store.
async fn reads_of_one_load() -> u64 {
    let tree = Tree::spread(&["a/x"], "fill", &["b/y"]).await;
    let (tree, store) = tree.failing().await;
    let _root = tree
        .state
        .node(tree.repository.clone(), ROOT_NODE)
        .await
        .expect("the root reads");
    let before = store.reads();
    let _y = tree
        .state
        .node(tree.repository.clone(), tree.id("b/y"))
        .await
        .expect("'b/y' reads");
    store.reads() - before
}

/// A read failed in a call's own steps answers the call failed, and what the call left its records
/// tell: no fault outside every call. One failed outside every call is one.
#[tokio::test]
async fn a_read_failed_in_a_calls_own_steps_is_no_fault_outside_calls() {
    crate::in_context(async {
        let (tree, store) = Tree::new(&["d/f"]).await.failing().await;
        let f = tree.id("d/f");
        store.fail(true);
        // Calls are kept where the thread's collector records, as a run's thread's does.
        let log = Collector::new();
        log.set_recording(true);
        let entered = log.enter();
        recorded(ops::scan_clear(&tree, f)).await;
        assert!(store.failed() > 0, "the clear's load failed");
        assert_eq!(store.failed_outside_calls(), 0);
        let (read, _) = recorded(tree.state.node(tree.repository.clone(), f)).await;
        drop(entered);
        assert!(read.is_err(), "the bare read failed");
        assert!(store.failed_outside_calls() > 0);
    })
    .await;
}

/// A countdown lets as many reads through as it is given, then fails every read until the store
/// is told to fail none.
#[tokio::test]
async fn a_countdown_lets_its_reads_through_then_fails() {
    crate::in_context(async {
        let reads = reads_of_one_load().await;
        assert!(reads > 0, "a load not in memory reads the store");

        for (allowed, loads) in [(reads, true), (reads - 1, false)] {
            let tree = Tree::spread(&["a/x"], "fill", &["b/y"]).await;
            let (tree, store) = tree.failing().await;
            let _root = tree
                .state
                .node(tree.repository.clone(), ROOT_NODE)
                .await
                .expect("the root reads");
            store.fail_after(allowed);
            let read = tree
                .state
                .node(tree.repository.clone(), tree.id("b/y"))
                .await;
            assert_eq!(
                read.is_ok(),
                loads,
                "a load given {allowed} of its {reads} reads"
            );
            if !loads {
                assert!(store.failed() > 0, "the store counts the failure");
                assert!(
                    tree.state
                        .node(tree.repository.clone(), tree.id("b/y"))
                        .await
                        .is_err(),
                    "every later read fails too"
                );
                store.fail(false);
                assert!(
                    tree.state
                        .node(tree.repository.clone(), tree.id("b/y"))
                        .await
                        .is_ok(),
                    "until the store is told to fail none"
                );
            }
        }
    })
    .await;
}

/// Which of `count` reads a window fails, read by read.
async fn window(one_in: u64, seed: u64, count: u64) -> Vec<bool> {
    let store = FailingStore::new(fixtures::repository().await.immutable_store());
    store.fail_one_in(one_in, seed);
    reads(&store, count).await
}

/// Which of the next `count` reads of `store` it fails, read by read.
async fn reads(store: &Arc<FailingStore>, count: u64) -> Vec<bool> {
    let mut failed = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let before = store.failed();
        // An address the store does not hold: the read fails either way, counted only when the
        // store fails it.
        let _ = store
            .clone()
            .get(Partition::default(), Address::default())
            .await;
        failed.push(store.failed() > before);
    }
    failed
}

/// A window fails about one read in its number, the same reads for the same seed, others for
/// another, and none for 0.
#[tokio::test]
async fn a_window_fails_the_same_reads_for_a_seed() {
    crate::in_context(async {
        let first = window(10, 7, 1000).await;
        let again = window(10, 7, 1000).await;
        let other = window(10, 8, 1000).await;
        let failed = first.iter().filter(|failed| **failed).count();
        assert!(
            (50..=150).contains(&failed),
            "{failed} of 1000 reads failed, about one in ten"
        );
        assert_eq!(first, again, "the same seed fails the same reads");
        assert_ne!(first, other, "another seed fails others");
        assert!(
            window(0, 7, 100).await.iter().all(|failed| !failed),
            "0 fails none"
        );
    })
    .await;
}

/// [`Tree::with_a_free_slot`] with the file metadata of `d/g` set, so the file
/// metadata block it shares with the freed slot is stored, and clean once the freed slot takes a
/// node. Answers the freed slot and that block's index.
async fn tree_with_stored_metadata() -> (Tree, Arc<FailingStore>, NodeID, usize) {
    let (tree, store, freed) = Tree::with_a_free_slot(Some("d/g")).await;
    let index = NodeFileMetadataBlock::index(node::node_to_file_metadata(freed));
    assert_eq!(
        index,
        NodeFileMetadataBlock::index(node::node_to_file_metadata(tree.id("d/g"))),
        "both slots share one file metadata block"
    );
    (tree, store, freed, index)
}

/// A file metadata block the State keeps once a check read it leaves memory with an eviction: the
/// check answers from memory with the store failing before it, and reads the store after.
#[tokio::test]
async fn eviction_drops_the_checked_file_metadata() {
    crate::in_context(async {
        let (tree, store, _, index) = tree_with_stored_metadata().await;
        let repository = &tree.repository;
        let checked = tree
            .state
            .try_block_file_metadata_existing(repository.clone(), index)
            .await
            .expect("the check reads the block");
        assert!(checked.is_some(), "the block is stored");
        drop(checked);

        store.fail(true);
        let kept = tree
            .state
            .try_block_file_metadata_existing(repository.clone(), index)
            .await;
        assert!(
            matches!(kept, Ok(Some(_))),
            "the State keeps the checked block"
        );
        drop(kept);
        tree.state.evict_blocks_for_tests();
        let evicted = tree
            .state
            .try_block_file_metadata_existing(repository.clone(), index)
            .await;
        store.fail(false);

        assert!(
            evicted.is_err(),
            "the eviction dropped it: the check reads the store"
        );
    })
    .await;
}

/// The file metadata block an attach holds, for the next attach into the same block, leaves
/// memory with an eviction.
#[tokio::test]
async fn eviction_drops_the_attach_file_metadata() {
    crate::in_context(async {
        let (tree, store, freed, index) = tree_with_stored_metadata().await;
        let repository = &tree.repository;
        // Loaded, not checked, so only the attach keeps it once this is dropped.
        let metadata = tree
            .state
            .block_file_metadata(repository.clone(), index)
            .await
            .expect("the file metadata block loads");
        let _names = tree
            .state
            .block_with_nametable(repository.clone(), NodeBlock::index(freed))
            .await
            .expect("the slot's names load");
        let node = Node {
            flags: NodeFlags::File.bits(),
            ..Node::default()
        };
        let attached = tree
            .state
            .node_attach_resident(repository.clone(), tree.id("d"), node, "h", None)
            .await
            .expect("the attach");
        assert_eq!(
            attached,
            Some(AttachOutcome::Attached(freed)),
            "the attach takes the freed slot from memory"
        );
        drop(metadata);

        store.fail(true);
        let held = tree
            .state
            .try_block_file_metadata_existing(repository.clone(), index)
            .await;
        assert!(matches!(held, Ok(Some(_))), "the attach holds the block");
        drop(held);
        tree.state.evict_blocks_for_tests();
        let evicted = tree
            .state
            .try_block_file_metadata_existing(repository.clone(), index)
            .await;
        store.fail(false);

        assert!(
            evicted.is_err(),
            "the eviction dropped it: the check reads the store"
        );
    })
    .await;
}

/// The modes combine: a countdown armed beside a window fails the reads the window fails while it
/// runs, and every read once it has run out; failing none drops both.
#[tokio::test]
async fn the_modes_combine_and_failing_none_drops_them() {
    crate::in_context(async {
        let store = FailingStore::new(fixtures::repository().await.immutable_store());
        store.fail_one_in(4, 7);
        store.fail_after(50);
        let first = reads(&store, 50).await;
        let failed = first.iter().filter(|failed| **failed).count();
        assert!(
            (1..50).contains(&failed),
            "the window fails {failed} of the countdown's 50 reads"
        );
        assert!(
            reads(&store, 20).await.iter().all(|failed| *failed),
            "every read fails once the countdown has run out"
        );

        store.fail(false);
        assert!(
            reads(&store, 100).await.iter().all(|failed| !failed),
            "failing none drops the countdown and the window"
        );
    })
    .await;
}
