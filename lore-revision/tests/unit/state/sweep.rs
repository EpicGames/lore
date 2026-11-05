// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the sweep of detached slots (C15): what it frees and what it never follows, the flags
//! that bring a block to it, what the served sweep keeps, and the reclaim count that makes older
//! marks and absences stale.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::Arc;

use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Absent;
use lore_revision::state::AddedSince;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Detach;
use lore_revision::state::DiscardCounts;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use super::add;
use super::counted_steps;
use super::directory;
use super::file;
use super::record;
use super::reload;
use super::with_state;

/// The slots on block `index`'s free list.
async fn free_slots(
    state: &State,
    repository: &Arc<RepositoryContext>,
    index: usize,
) -> BTreeSet<usize> {
    let block = state
        .block(repository.clone(), index)
        .await
        .expect("the block");
    let reader = block.read();
    let data = reader.node_block();
    let mut free = BTreeSet::new();
    let mut at = data.node_unused as usize;
    for _ in 0..data.node_unused_count {
        assert!(free.insert(at), "slot {at} is on the free list twice");
        at = data.node[at].sibling as usize;
    }
    free
}

/// Whether `node`'s slot holds no node: on its block's free list, or past its node count, as a
/// block whose every slot is freed resets.
async fn is_free(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) -> bool {
    let node_count = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the block")
        .read()
        .node_count();
    Node::index(node) >= node_count
        || free_slots(state, repository, NodeBlock::index(node))
            .await
            .contains(&Node::index(node))
}

/// Whether `node`'s block is flagged for the next sweep.
async fn flagged(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) -> bool {
    state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the block")
        .read()
        .is_unswept()
}

async fn sweep(state: &State, repository: &Arc<RepositoryContext>) -> DiscardCounts {
    state
        .node_sweep_detached(repository)
        .await
        .expect("the sweep runs")
}

/// `dir`'s children in chain order.
async fn children(state: &State, repository: &Arc<RepositoryContext>, dir: NodeID) -> Vec<NodeID> {
    state
        .node_children_with_name_hash(repository.clone(), dir)
        .await
        .expect("the listing")
        .into_iter()
        .map(|(child, _)| child)
        .collect()
}

async fn detach(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) {
    assert!(
        state
            .node_detach(repository.clone(), node, Detach::Always)
            .await
            .expect("the detach runs"),
        "the node detaches"
    );
}

/// Adds files to the root until block 0 is full, so the next add takes a slot in block 1.
async fn fill_block_0(state: &State, repository: &Arc<RepositoryContext>) {
    let mut index = 0;
    while !state
        .block(repository.clone(), 0)
        .await
        .expect("block 0")
        .read()
        .is_full()
    {
        let name = format!("fill{index}");
        add(state, repository, ROOT_NODE, file(&name), &name).await;
        index += 1;
    }
}

/// A sweep frees a detached file, never the live siblings its `sibling` still links, takes its
/// block's flag, and the next add takes the freed slot. A sweep with nothing detached frees no
/// published slot.
#[tokio::test]
async fn a_sweep_frees_a_detached_file_and_not_the_chain_it_left() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let a = add(&state, &repository, d, file("a"), "a").await;
        let b = add(&state, &repository, d, file("b"), "b").await;
        let c = add(&state, &repository, d, file("c"), "c").await;
        assert!(
            flagged(&state, &repository, b).await,
            "a grab flags its block"
        );
        let counts = sweep(&state, &repository).await;
        assert_eq!((counts.file_count, counts.directory_count), (0, 0));
        assert!(
            !flagged(&state, &repository, b).await,
            "the sweep takes the flag"
        );

        detach(&state, &repository, b).await;
        assert!(
            flagged(&state, &repository, b).await,
            "a detach flags its block"
        );
        assert_eq!(
            record(&state, &repository, b).await.sibling,
            a,
            "the detached record still links the chain it left"
        );
        let counts = sweep(&state, &repository).await;

        assert_eq!((counts.file_count, counts.directory_count), (1, 0));
        assert!(is_free(&state, &repository, b).await);
        assert_eq!(children(&state, &repository, d).await, [c, a]);
        assert!(!is_free(&state, &repository, a).await);
        let e = add(&state, &repository, d, file("e"), "e").await;
        assert_eq!(e, b, "the next add takes the freed slot");
    })
    .await;
}

/// A sweep frees a detached directory with its subtree, files and directories alike.
#[tokio::test]
async fn a_sweep_frees_a_detached_directory_with_its_subtree() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let s = add(&state, &repository, d, directory("s"), "s").await;
        let x = add(&state, &repository, s, file("x"), "x").await;
        let y = add(&state, &repository, s, file("y"), "y").await;
        let t = add(&state, &repository, s, directory("t"), "t").await;
        let z = add(&state, &repository, t, file("z"), "z").await;
        let kept = add(&state, &repository, d, file("kept"), "kept").await;
        sweep(&state, &repository).await;

        detach(&state, &repository, s).await;
        let counts = sweep(&state, &repository).await;

        assert_eq!((counts.file_count, counts.directory_count), (3, 2));
        for node in [s, x, y, t, z] {
            assert!(is_free(&state, &repository, node).await, "{node} is freed");
        }
        assert_eq!(children(&state, &repository, d).await, [kept]);
        assert_eq!(
            sweep(&state, &repository).await.file_count,
            0,
            "nothing is left"
        );
    })
    .await;
}

/// A sweep frees a slot taken and never published, as a call dropped or failed between its grab
/// and its publish leaves one.
#[tokio::test]
async fn a_sweep_frees_a_slot_left_in_flight() {
    with_state(|repository, state| async move {
        add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        sweep(&state, &repository).await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        let taken = state.try_grab_in(&block, 0).expect("block 0 has room");
        assert!(record(&state, &repository, taken).await.is_discarded());
        assert!(block.read().is_unswept(), "the grab flags its block");

        let counts = sweep(&state, &repository).await;

        assert_eq!(counts.file_count + counts.directory_count, 1);
        assert!(is_free(&state, &repository, taken).await);
    })
    .await;
}

/// A slot detached and saved is reclaimed in a later session: its block loads flagged, as an
/// earlier session may have left a slot detached there, and the next sweep frees it.
#[tokio::test]
async fn a_sweep_reclaims_a_detached_slot_a_save_stored() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let gone = add(&state, &repository, d, file("gone"), "gone").await;
        let kept = add(&state, &repository, d, file("kept"), "kept").await;
        detach(&state, &repository, gone).await;

        let loaded = reload(&state, &repository).await;
        assert!(
            flagged(&loaded, &repository, gone).await,
            "a loaded block is flagged"
        );
        assert!(record(&loaded, &repository, gone).await.is_discarded());
        assert!(
            !is_free(&loaded, &repository, gone).await,
            "the save stored it detached"
        );
        let counts = sweep(&loaded, &repository).await;

        assert_eq!(counts.file_count, 1);
        assert!(is_free(&loaded, &repository, gone).await);
        assert!(!flagged(&loaded, &repository, gone).await);
        assert_eq!(children(&loaded, &repository, d).await, [kept]);
    })
    .await;
}

/// The unserved sweep drains the C API delete's pending queue before it scans, so a pending
/// subtree is freed once, and each of its slots is on the free list once.
#[tokio::test]
async fn a_sweep_frees_a_pending_subtree_once() {
    with_state(|repository, state| async move {
        let p = state
            .node_add_staged(repository.clone(), ROOT_NODE, directory("p"), "p")
            .await
            .expect("a staged add");
        for name in ["f", "g"] {
            state
                .node_add_staged(repository.clone(), p, file(name), name)
                .await
                .expect("a staged add");
        }
        sweep(&state, &repository).await;
        let marked = state
            .node_discard_pending(repository.clone(), p, ROOT_NODE, hash_string("p"))
            .await
            .expect("the pending discard runs");
        assert_eq!(marked, Some(3));

        let counts = sweep(&state, &repository).await;

        assert_eq!((counts.file_count, counts.directory_count), (2, 1));
        // `free_slots` fails on a slot listed twice.
        assert!(is_free(&state, &repository, p).await);
        // A slot taken again stays taken: the sweep drained the queue, so no later drain frees it.
        let reused = add(
            &state,
            &repository,
            ROOT_NODE,
            directory("reused"),
            "reused",
        )
        .await;
        let again = sweep(&state, &repository).await;
        assert_eq!((again.file_count, again.directory_count), (0, 0));
        let drained = state
            .node_free_pending(&repository)
            .await
            .expect("the drain runs");
        assert_eq!((drained.file_count, drained.directory_count), (0, 0));
        assert!(!is_free(&state, &repository, reused).await);
        assert_eq!(children(&state, &repository, ROOT_NODE).await, [reused]);
    })
    .await;
}

/// A sweep's walk ends a chain at a slot holding no node, as a discard that freed part of a
/// chain in place and then failed leaves one: the freed slot's `sibling` links the free list,
/// which the walk never takes, so nothing is freed twice. Nor is a free slot a root, though it
/// reads `Discarded`. Run through the served sweep, whose `retained` is asked about every node a
/// walk reaches: only `d`.
#[tokio::test]
async fn a_sweep_ends_a_chain_at_a_freed_slot() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let f1 = add(&state, &repository, d, file("f1"), "f1").await;
        let f2 = add(&state, &repository, d, file("f2"), "f2").await;
        // Freed first, so the free list `f2` joins below is not empty.
        let spare = add(&state, &repository, ROOT_NODE, file("spare"), "spare").await;
        detach(&state, &repository, spare).await;
        sweep(&state, &repository).await;
        detach(&state, &repository, d).await;
        // As a discard that failed after freeing the chain's head in place.
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        {
            let mut writer = block.write_for_tests();
            writer.discard_node(0, Node::index(f2));
            writer.mark_dirty();
        }
        state.block_modified_for_tests(block.clone(), 0);
        assert_eq!(record(&state, &repository, d).await.child, f2);
        let before = free_slots(&state, &repository, 0).await;
        state.mark_served();
        let asked = RefCell::new(Vec::new());

        let counts = state
            .served()
            .node_sweep_detached(&repository, |node| {
                asked.borrow_mut().push(node);
                false
            })
            .await
            .expect("the served sweep runs");

        assert_eq!(
            counts.file_count + counts.directory_count,
            1,
            "only `d` is freed"
        );
        assert_eq!(
            asked.into_inner(),
            [d],
            "no walk takes the freed slot or starts at a free one"
        );
        let after = free_slots(&state, &repository, 0).await;
        let mut expected = before;
        expected.insert(Node::index(d));
        assert_eq!(after, expected);
        assert!(
            !is_free(&state, &repository, f1).await,
            "past the break, out of reach"
        );
    })
    .await;
}

/// The served sweep keeps whole a detached subtree any node of which the provider retains, its
/// block flagged for a later sweep, and frees it once the provider released it.
#[tokio::test]
async fn a_served_sweep_keeps_a_retained_subtree_until_it_is_released() {
    with_state(|repository, state| async move {
        let s = add(&state, &repository, ROOT_NODE, directory("s"), "s").await;
        let x = add(&state, &repository, s, file("x"), "x").await;
        let gone = add(&state, &repository, ROOT_NODE, file("gone"), "gone").await;
        state.mark_served();
        for node in [s, gone] {
            assert!(
                state
                    .served()
                    .node_detach(repository.clone(), node, Detach::Always)
                    .await
                    .expect("the served detach runs")
            );
        }

        let counts = state
            .served()
            .node_sweep_detached(&repository, |node| node == x)
            .await
            .expect("the served sweep runs");

        assert_eq!(
            (counts.file_count, counts.directory_count),
            (1, 0),
            "`gone` alone"
        );
        assert!(is_free(&state, &repository, gone).await);
        assert!(!is_free(&state, &repository, s).await && !is_free(&state, &repository, x).await);
        assert!(
            flagged(&state, &repository, s).await,
            "the kept root's block is flagged again"
        );
        let counts = state
            .served()
            .node_sweep_detached(&repository, |_| false)
            .await
            .expect("the served sweep runs");
        assert_eq!((counts.file_count, counts.directory_count), (1, 1));
        assert!(is_free(&state, &repository, s).await && is_free(&state, &repository, x).await);
    })
    .await;
}

/// A sweep that frees a slot makes a chain mark and an absence taken before it stale: the search
/// since the mark answers stale, and an attach handed the absence walks the chain. A sweep that
/// frees nothing leaves both standing.
#[tokio::test]
async fn a_sweep_that_frees_makes_older_marks_and_absences_stale() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let a = add(&state, &repository, d, file("a"), "a").await;
        add(&state, &repository, d, file("b"), "b").await;
        sweep(&state, &repository).await;
        let block = state
            .block(repository.clone(), NodeBlock::index(d))
            .await
            .expect("the directory's block");
        let (_, mark) = state
            .node_children_with_name_hash_marked(repository.clone(), d)
            .await
            .expect("the listing");
        let mark = mark.expect("a directory's listing is marked");
        let absence = |name: &str| {
            let (at, _) = block.watch_chain(Node::index(d), None);
            Absent::new(&state, at.seq, block.clone(), d, hash_string(name))
        };
        let attach = async |name: &str, absent: &Absent| {
            let before = counted_steps();
            let outcome = state
                .node_attach_absent(repository.clone(), d, file(name), name, Some(absent))
                .await
                .expect("the attach runs");
            assert!(matches!(outcome, AttachOutcome::Attached(_)), "{outcome:?}");
            counted_steps() - before
        };

        let standing = absence("n");
        assert_eq!(sweep(&state, &repository).await.file_count, 0);
        assert_eq!(
            state
                .find_subnode_added_since(repository.clone(), &mark, hash_string("none"))
                .await
                .expect("the search runs"),
            AddedSince::Absent
        );
        assert_eq!(
            attach("n", &standing).await,
            0,
            "the absence stands in for the walk"
        );

        let stale = absence("m");
        detach(&state, &repository, a).await;
        assert_eq!(sweep(&state, &repository).await.file_count, 1);
        assert_eq!(
            state
                .find_subnode_added_since(repository.clone(), &mark, hash_string("none"))
                .await
                .expect("the search runs"),
            AddedSince::Stale
        );
        assert!(
            attach("m", &stale).await > 0,
            "a stale absence walks the chain"
        );

        // Taken after that sweep, a mark and an absence stand again.
        let (_, fresh_mark) = state
            .node_children_with_name_hash_marked(repository.clone(), d)
            .await
            .expect("the listing");
        let fresh_mark = fresh_mark.expect("a directory's listing is marked");
        assert_eq!(
            state
                .find_subnode_added_since(repository.clone(), &fresh_mark, hash_string("none"))
                .await
                .expect("the search runs"),
            AddedSince::Absent
        );
        let fresh = absence("k");
        assert_eq!(attach("k", &fresh).await, 0, "a fresh absence stands");
    })
    .await;
}

/// As a discard that failed after freeing the chain's head in place and an add that then took the
/// slot under another parent: the sweep's walk from the detached directory meets a live node of
/// another parent and stops, freeing nothing of the chain that node is in.
#[tokio::test]
async fn a_sweep_stops_at_a_slot_taken_again_under_another_parent() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        add(&state, &repository, d, file("f1"), "f1").await;
        let f2 = add(&state, &repository, d, file("f2"), "f2").await;
        let live = add(&state, &repository, ROOT_NODE, file("live"), "live").await;
        sweep(&state, &repository).await;
        detach(&state, &repository, d).await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        {
            let mut writer = block.write_for_tests();
            writer.discard_node(0, Node::index(f2));
            writer.mark_dirty();
        }
        state.block_modified_for_tests(block.clone(), 0);
        let taken = add(&state, &repository, ROOT_NODE, file("taken"), "taken").await;
        assert_eq!(taken, f2, "the add takes the freed head's slot");
        assert_eq!(record(&state, &repository, d).await.child, taken);

        let counts = sweep(&state, &repository).await;

        assert_eq!(
            (counts.file_count, counts.directory_count),
            (0, 1),
            "only `d` is freed"
        );
        assert_eq!(
            children(&state, &repository, ROOT_NODE).await,
            [taken, live]
        );
        assert!(
            !is_free(&state, &repository, taken).await && !is_free(&state, &repository, live).await
        );
    })
    .await;
}

/// A sweep's walk ends a chain that loops back on itself, freeing each node once.
#[tokio::test]
async fn a_sweep_ends_a_chain_at_a_cycle() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let f1 = add(&state, &repository, d, file("f1"), "f1").await;
        let f2 = add(&state, &repository, d, file("f2"), "f2").await;
        sweep(&state, &repository).await;
        detach(&state, &repository, d).await;
        // A corrupt chain: `f1`, the tail, links back to `f2`, the head.
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        {
            let mut writer = block.write_for_tests();
            writer.node(Node::index(f1)).sibling = f2;
            writer.mark_dirty();
        }
        state.block_modified_for_tests(block.clone(), 0);

        let counts = sweep(&state, &repository).await;

        assert_eq!((counts.file_count, counts.directory_count), (2, 1));
        for node in [d, f1, f2] {
            assert!(is_free(&state, &repository, node).await, "{node} is freed");
        }
    })
    .await;
}

/// The served sweep keeps a root nested in a kept subtree with it, as a discard that failed part
/// way leaves a marked directory in a detached chain: freeing it would free part of a subtree the
/// provider retains.
#[tokio::test]
async fn a_served_sweep_keeps_a_root_nested_in_a_retained_subtree() {
    with_state(|repository, state| async move {
        let s = add(&state, &repository, ROOT_NODE, directory("s"), "s").await;
        let r = add(&state, &repository, s, file("r"), "r").await;
        let x = add(&state, &repository, s, directory("x"), "x").await;
        let y = add(&state, &repository, x, file("y"), "y").await;
        state.mark_served();
        assert!(
            state
                .served()
                .node_detach(repository.clone(), s, Detach::Always)
                .await
                .expect("the served detach runs")
        );
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        {
            let mut writer = block.write_for_tests();
            writer.node(Node::index(x)).flags |= NodeFlags::Discarded.bits();
            writer.mark_unswept();
            writer.mark_dirty();
        }
        state.block_modified_for_tests(block.clone(), 0);

        let counts = state
            .served()
            .node_sweep_detached(&repository, |node| node == r)
            .await
            .expect("the served sweep runs");

        assert_eq!((counts.file_count, counts.directory_count), (0, 0));
        for node in [s, r, x, y] {
            assert!(!is_free(&state, &repository, node).await, "{node} is kept");
        }
        let counts = state
            .served()
            .node_sweep_detached(&repository, |_| false)
            .await
            .expect("the served sweep runs");
        assert_eq!((counts.file_count, counts.directory_count), (2, 2));
    })
    .await;
}

/// A root kept in a block the State loaded clean, so no registration notes the block, is noted
/// again by the sweep that kept it, and the next sweep frees it once released.
#[tokio::test]
async fn a_kept_root_in_a_clean_block_is_swept_again() {
    with_state(|repository, state| async move {
        let s = add(&state, &repository, ROOT_NODE, directory("s"), "s").await;
        let x = add(&state, &repository, s, file("x"), "x").await;
        detach(&state, &repository, s).await;
        let loaded = reload(&state, &repository).await;
        loaded.mark_served();
        assert!(flagged(&loaded, &repository, s).await);

        let counts = loaded
            .served()
            .node_sweep_detached(&repository, |node| node == x)
            .await
            .expect("the served sweep runs");
        assert_eq!((counts.file_count, counts.directory_count), (0, 0));
        assert!(!loaded.is_dirty(), "the kept root left the block clean");

        let counts = loaded
            .served()
            .node_sweep_detached(&repository, |_| false)
            .await
            .expect("the served sweep runs");
        assert_eq!((counts.file_count, counts.directory_count), (1, 1));
    })
    .await;
}

/// A block noted for the sweep and evicted before it, which a kept root's walk loads, flagged,
/// stays noted, so the next sweep scans it and frees the detached slot it holds.
#[tokio::test]
async fn a_block_a_walk_loads_stays_noted_for_the_next_sweep() {
    with_state(|repository, state| async move {
        let r = add(&state, &repository, ROOT_NODE, directory("r"), "r").await;
        fill_block_0(&state, &repository).await;
        let c = add(&state, &repository, r, file("c"), "c").await;
        let x = add(&state, &repository, ROOT_NODE, file("x"), "x").await;
        assert_eq!((NodeBlock::index(c), NodeBlock::index(x)), (1, 1));
        detach(&state, &repository, r).await;
        detach(&state, &repository, x).await;
        let loaded = reload(&state, &repository).await;
        loaded.mark_served();
        let held = loaded.block(repository.clone(), 0).await.expect("block 0");
        drop(loaded.block(repository.clone(), 1).await.expect("block 1"));
        loaded.evict_blocks_for_tests();
        assert!(loaded.resident_block(1).is_none(), "block 1 left memory");
        assert!(
            loaded.block_noted_unswept(1),
            "block 1 is noted from its load"
        );

        let counts = loaded
            .served()
            .node_sweep_detached(&repository, |node| node == c)
            .await
            .expect("the served sweep runs");

        assert_eq!((counts.file_count, counts.directory_count), (0, 0));
        assert!(
            loaded
                .resident_block(1)
                .expect("the walk loaded block 1")
                .read()
                .is_unswept(),
            "block 1 loaded flagged"
        );
        assert!(loaded.block_noted_unswept(1), "block 1 stays noted");
        let counts = loaded
            .served()
            .node_sweep_detached(&repository, |node| node == c)
            .await
            .expect("the served sweep runs");
        assert_eq!(
            (counts.file_count, counts.directory_count),
            (1, 0),
            "the next sweep frees `x`"
        );
        assert!(is_free(&loaded, &repository, x).await);
        drop(held);
    })
    .await;
}

/// A sweep whose only frees are the pending drain's makes a chain mark taken before it stale, as
/// a sweep's scan that frees does: the slot it freed, taken again, never reads as the node the
/// listing saw.
#[tokio::test]
async fn a_sweep_freeing_only_a_pending_subtree_makes_older_marks_stale() {
    with_state(|repository, state| async move {
        add(&state, &repository, ROOT_NODE, file("a"), "a").await;
        let p = state
            .node_add_staged(repository.clone(), ROOT_NODE, directory("p"), "p")
            .await
            .expect("a staged add");
        sweep(&state, &repository).await;
        let (_, mark) = state
            .node_children_with_name_hash_marked(repository.clone(), ROOT_NODE)
            .await
            .expect("the listing");
        let mark = mark.expect("a directory's listing is marked");
        let before = state.reclaims();
        assert_eq!(
            state
                .node_discard_pending(repository.clone(), p, ROOT_NODE, hash_string("p"))
                .await
                .expect("the pending discard runs"),
            Some(1)
        );

        let counts = sweep(&state, &repository).await;

        assert_eq!(counts.directory_count, 1, "the drain freed `p`");
        assert_ne!(
            state.reclaims(),
            before,
            "the drain moved the reclaim count"
        );
        let q = add(&state, &repository, ROOT_NODE, directory("q"), "q").await;
        assert_eq!(q, p, "`q` takes the slot of `p`");
        assert_eq!(
            state
                .find_subnode_added_since(repository.clone(), &mark, hash_string("q"))
                .await
                .expect("the search runs"),
            AddedSince::Stale
        );
    })
    .await;
}

/// A sweep never follows a link's child, which names a node of the linked State: a node of this
/// State holding that id stays, though its parent names the link, whether the link is a detached
/// root or lies below one.
#[tokio::test]
async fn a_sweep_never_follows_a_links_child() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        // Directories, each given a child, then made links: the child's parent names the link.
        let inner = add(&state, &repository, d, directory("inner"), "inner").await;
        let x = add(&state, &repository, inner, file("x"), "x").await;
        let outer = add(&state, &repository, ROOT_NODE, directory("outer"), "outer").await;
        let y = add(&state, &repository, outer, file("y"), "y").await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        for link in [inner, outer] {
            state.node_edit_for_tests(&block, link, |node| {
                node.flags |= NodeFlags::Link.bits();
            });
        }
        sweep(&state, &repository).await;
        detach(&state, &repository, d).await;
        detach(&state, &repository, outer).await;

        let counts = sweep(&state, &repository).await;

        assert_eq!(
            (counts.file_count, counts.directory_count),
            (0, 3),
            "`d` and both links, links counted as directories"
        );
        for node in [d, inner, outer] {
            assert!(is_free(&state, &repository, node).await, "{node} is freed");
        }
        for node in [x, y] {
            assert!(
                !is_free(&state, &repository, node).await,
                "{node}, which a link's child names, stays"
            );
        }
    })
    .await;
}

/// A discard that fails part way leaves the root it marked, and each directory a run below it
/// marked, to the next sweep, which finds them by the flags the discard set, here on blocks no
/// flag was left on before: the root's, and the one a run of its chain marked a directory in.
#[tokio::test]
async fn a_sweep_frees_what_a_discard_failed_part_way_left() {
    with_state(|repository, state| async move {
        let r = add(&state, &repository, ROOT_NODE, directory("r"), "r").await;
        fill_block_0(&state, &repository).await;
        let s = add(&state, &repository, r, directory("s"), "s").await;
        let f = add(&state, &repository, r, file("f"), "f").await;
        let x = add(&state, &repository, s, file("x"), "x").await;
        assert_eq!([r, s, f, x].map(NodeBlock::index), [0, 1, 1, 1]);
        // Misparented, so the discard's walk below `s` fails on it.
        let block = state.block(repository.clone(), 1).await.expect("block 1");
        state.node_edit_for_tests(&block, x, |node| node.parent = ROOT_NODE);
        sweep(&state, &repository).await;
        assert!(!flagged(&state, &repository, r).await && !flagged(&state, &repository, s).await);

        let discarded =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), r).await;

        assert!(discarded.is_err(), "the walk below `s` fails");
        assert!(is_free(&state, &repository, f).await, "the run freed `f`");
        for node in [r, s] {
            assert!(
                record(&state, &repository, node).await.is_discarded(),
                "{node} is marked"
            );
        }
        let counts = sweep(&state, &repository).await;
        assert_eq!(
            (counts.file_count, counts.directory_count),
            (0, 2),
            "`r` and `s`"
        );
        for node in [r, s] {
            assert!(is_free(&state, &repository, node).await, "{node} is freed");
        }
        assert!(
            !is_free(&state, &repository, x).await,
            "misparented, out of reach"
        );
    })
    .await;
}
