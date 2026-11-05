// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the C API tier: the delete's marks and its pending queue with the commit's drain,
//! and the add, modify and restore that refuse what a delete took under their locks (H18).

use std::collections::BTreeSet;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Content;
use lore_revision::state::LevelMark;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use super::add;
use super::directory;
use super::file;
use super::fill;
use super::null_repository;
use super::record;
use super::reload;
use super::with_state;
use crate::fs::filesystem_provider::setup_test_execution;

async fn added(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    node: Node,
    name: &str,
) -> NodeID {
    state
        .node_add_staged(repository.clone(), parent, node, name)
        .await
        .expect("a staged add succeeds")
}

/// The slots on block 0's free list.
async fn free_slots(state: &State, repository: &Arc<RepositoryContext>) -> BTreeSet<usize> {
    let block = state.block(repository.clone(), 0).await.expect("block 0");
    let reader = block.read();
    let data = reader.node_block();
    let mut free = BTreeSet::new();
    let mut at = data.node_unused as usize;
    for _ in 0..data.node_unused_count {
        free.insert(at);
        at = data.node[at].sibling as usize;
    }
    free
}

const DELETED: Mark = Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete);

/// A staged add publishes its node added and dirty, and marks the directories above it with the
/// base bits up to the root.
#[tokio::test]
async fn a_staged_add_publishes_its_node_added_and_marks_its_ancestors() {
    with_state(|repository, state| async move {
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let sub = add(&state, &repository, t, directory("sub"), "sub").await;

        let x = added(&state, &repository, sub, file("x"), "x").await;

        let node = record(&state, &repository, x).await;
        assert!(
            node.is_staged_add() && node.is_dirty_add(),
            "{:x}",
            node.flags
        );
        for dir in [sub, t] {
            let node = record(&state, &repository, dir).await;
            assert!(
                node.is_staged() && node.is_dirty(),
                "{dir}: {:x}",
                node.flags
            );
        }
    })
    .await;
}

/// A staged add refuses a parent a delete took: one staged for deletion as a bad argument, one
/// discarded as not found, taking no slot either way.
#[tokio::test]
async fn a_staged_add_refuses_a_parent_staged_for_deletion_or_discarded() {
    with_state(|repository, state| async move {
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
        state
            .node_delete_level(&repository, t, ROOT_NODE)
            .await
            .expect("the delete's mark");
        state
            .node_discard_pending(repository.clone(), n, ROOT_NODE, hash_string("n"))
            .await
            .expect("the delete's detach")
            .expect("n stands where it was found");
        let before = free_slots(&state, &repository).await;

        let staged = state
            .node_add_staged(repository.clone(), t, file("x"), "x")
            .await
            .expect_err("refused under a deleted parent");
        assert!(staged.is_invalid_arguments(), "{staged}");
        let discarded = state
            .node_add_staged(repository.clone(), n, file("x"), "x")
            .await
            .expect_err("refused under a discarded parent");
        assert!(discarded.is_node_not_found(), "{discarded}");
        assert_eq!(free_slots(&state, &repository).await, before);
    })
    .await;
}

/// A delete's mark stages the node, answers its chain's head, and marks the directories above;
/// a node staged already answers so, and an added one is not the mark's.
#[tokio::test]
async fn a_delete_level_mark_answers_the_chain_head_it_read() {
    with_state(|repository, state| async move {
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let a = add(&state, &repository, t, file("a"), "a").await;
        let sub = add(&state, &repository, t, directory("sub"), "sub").await;
        let n = added(&state, &repository, t, directory("n"), "n").await;

        let marked = state
            .node_delete_level(&repository, t, ROOT_NODE)
            .await
            .expect("the mark");
        assert_eq!(
            marked,
            LevelMark::Marked(Some(n)),
            "the newest child heads the chain"
        );
        assert!(record(&state, &repository, t).await.is_staged_delete());
        assert_eq!(
            state.node_delete_level(&repository, a, t).await.expect("a"),
            LevelMark::Marked(None)
        );
        assert_eq!(
            state
                .node_delete_level(&repository, a, t)
                .await
                .expect("a again"),
            LevelMark::Staged
        );
        assert_eq!(
            state.node_delete_level(&repository, n, t).await.expect("n"),
            LevelMark::Other,
            "an added node is the detach's, not the mark's"
        );
        assert_eq!(
            state
                .node_delete_level(&repository, sub, a)
                .await
                .expect("sub"),
            LevelMark::Other,
            "a node not in the parent named is not the mark's"
        );
    })
    .await;
}

/// A pending discard detaches its root and marks every node below it discarded, keeping their
/// slots, chains and types, and counts them; the commit's drain frees them all once.
#[tokio::test]
async fn a_pending_discard_keeps_its_subtree_until_the_drain_frees_it() {
    with_state(|repository, state| async move {
        let keep = add(&state, &repository, ROOT_NODE, file("keep"), "keep").await;
        let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
        let c = added(&state, &repository, n, file("c"), "c").await;
        let m = added(&state, &repository, n, directory("m"), "m").await;
        let e = added(&state, &repository, m, file("e"), "e").await;

        let count = state
            .node_discard_pending(repository.clone(), n, ROOT_NODE, hash_string("n"))
            .await
            .expect("the discard")
            .expect("n stands where it was found");

        assert_eq!(count, 4);
        assert_eq!(
            state
                .node_children(repository.clone(), ROOT_NODE)
                .await
                .expect("the root's children"),
            vec![keep]
        );
        for node in [n, c, m, e] {
            assert!(
                record(&state, &repository, node).await.is_discarded(),
                "{node}"
            );
        }
        assert!(
            record(&state, &repository, c).await.is_file(),
            "a pending file keeps its type"
        );
        assert_eq!(
            record(&state, &repository, m).await.child,
            e,
            "and a directory its chain"
        );
        assert!(
            free_slots(&state, &repository).await.is_empty(),
            "nothing is freed yet"
        );

        let counts = state
            .node_free_pending(&repository)
            .await
            .expect("the drain");

        assert_eq!((counts.file_count, counts.directory_count), (2, 2));
        let freed: BTreeSet<usize> = [n, c, m, e].into_iter().map(Node::index).collect();
        assert_eq!(free_slots(&state, &repository).await, freed);
        let again = state
            .node_free_pending(&repository)
            .await
            .expect("a second drain");
        assert_eq!((again.file_count, again.directory_count), (0, 0));
        assert_eq!(free_slots(&state, &repository).await, freed);
    })
    .await;
}

/// A pending discard registers each block its marks turn dirty, so a save writes them: here
/// block 1, loaded clean and held in memory (C14).
#[tokio::test]
async fn a_pending_discard_registers_the_clean_blocks_it_marks() {
    with_state(|repository, written| async move {
        let p = add(&written, &repository, ROOT_NODE, directory("p"), "p").await;
        let d = add(&written, &repository, p, directory("d"), "d").await;
        add(&written, &repository, d, file("x"), "x").await;
        add(&written, &repository, p, file("keep"), "keep").await;
        let filled = add(&written, &repository, ROOT_NODE, directory("fill"), "fill").await;
        fill(&written, &repository, filled, 0).await;
        let far = add(&written, &repository, d, directory("far"), "far").await;
        let w = add(&written, &repository, far, file("w"), "w").await;
        assert_eq!(NodeBlock::index(w), 1, "'w' lies in block 1");
        let state = reload(&written, &repository).await;
        let _held = state.block(repository.clone(), 1).await.expect("block 1");
        assert!(
            !state.is_dirty() && !state.block_recorded_changed(1),
            "block 1 is loaded clean"
        );

        let count = state
            .node_discard_pending(repository.clone(), d, p, hash_string("d"))
            .await
            .expect("the discard")
            .expect("'d' stands where it was found");

        assert_eq!(count, 4, "the subtree is pending whole");
        assert!(
            state.is_dirty() && state.block_recorded_changed(1),
            "the block the marks changed is registered"
        );
    })
    .await;
}

/// A pending discard of a node that left the place it was found at detaches nothing.
#[tokio::test]
async fn a_pending_discard_of_a_node_moved_away_detaches_nothing() {
    with_state(|repository, state| async move {
        let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
        let q = add(&state, &repository, ROOT_NODE, directory("q"), "q").await;

        let moved = state
            .node_discard_pending(repository.clone(), n, q, hash_string("n"))
            .await
            .expect("the discard");

        assert_eq!(moved, None);
        assert!(!record(&state, &repository, n).await.is_discarded());
        let counts = state
            .node_free_pending(&repository)
            .await
            .expect("the drain");
        assert_eq!((counts.file_count, counts.directory_count), (0, 0));
    })
    .await;
}

/// A C API modify writes the content and the edit's flags in one write, keeping an added file
/// added, and marks the directories above; it refuses a file staged for deletion.
#[tokio::test]
async fn a_staged_modify_records_the_edit_with_the_content() {
    with_state(|repository, state| async move {
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let a = add(&state, &repository, t, file("a"), "a").await;
        let x = added(&state, &repository, t, file("x"), "x").await;
        let content = Content {
            mode: 0o600,
            size: 20,
            ..Content::default()
        };

        for node in [a, x] {
            state
                .node_modify_staged(&repository, node, &content)
                .await
                .expect("the modify");
        }

        let node = record(&state, &repository, a).await;
        assert!(
            node.is_staged_modify() && node.is_dirty_modify(),
            "{:x}",
            node.flags
        );
        assert_eq!((node.mode, node.size), (0o600, 20));
        let node = record(&state, &repository, x).await;
        assert!(
            node.is_staged_add() && node.is_dirty_add(),
            "{:x}",
            node.flags
        );
        let dir = record(&state, &repository, t).await;
        assert!(dir.is_staged() && dir.is_dirty(), "{:x}", dir.flags);

        state
            .node_mark(&repository, a, DELETED)
            .await
            .expect("a delete's mark");
        let refused = state
            .node_modify_staged(&repository, a, &content)
            .await
            .expect_err("refused");
        assert!(refused.is_invalid_arguments(), "{refused}");
        assert!(record(&state, &repository, a).await.is_staged_delete());
    })
    .await;
}

/// A C API add, a modify and a delete each climb from a directory that is dirty but not staged,
/// as a working-tree edit leaves one, and stage it: only a directory holding both base bits
/// stops the climb.
#[tokio::test]
async fn staged_changes_stage_a_directory_that_is_only_dirty() {
    with_state(|repository, state| async move {
        let dirty = Mark::dirty(NodeFlags::Dirty, lore_revision::state::Ext::Keep, false);
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let u = add(&state, &repository, ROOT_NODE, directory("u"), "u").await;
        let v = add(&state, &repository, ROOT_NODE, directory("v"), "v").await;
        let a = add(&state, &repository, u, file("a"), "a").await;
        let b = add(&state, &repository, v, file("b"), "b").await;
        for directory in [t, u, v] {
            state
                .node_mark(&repository, directory, dirty)
                .await
                .expect("the dirty mark");
        }

        added(&state, &repository, t, file("x"), "x").await;
        state
            .node_modify_staged(&repository, a, &Content::default())
            .await
            .expect("the modify");
        let level = state
            .node_delete_level(&repository, b, v)
            .await
            .expect("the delete");

        assert_eq!(level, LevelMark::Marked(None));
        for directory in [t, u, v] {
            let node = record(&state, &repository, directory).await;
            assert!(node.is_staged() && node.is_dirty(), "{:x}", node.flags);
        }
    })
    .await;
}

/// A C API modify refuses a node that is no live file, a directory, before any mark: the
/// directory above it stays unmarked and no unfinished change is noted.
#[tokio::test]
async fn a_staged_modify_refused_marks_nothing() {
    with_state(|repository, state| async move {
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let d = add(&state, &repository, t, directory("d"), "d").await;

        let refused = state
            .node_modify_staged(&repository, d, &Content::default())
            .await
            .expect_err("refused");

        assert!(refused.is_invalid_arguments(), "{refused}");
        let dir = record(&state, &repository, t).await;
        assert!(!dir.is_staged() && !dir.is_dirty(), "{:x}", dir.flags);
        assert!(!state.has_unfinished_change());
    })
    .await;
}

/// A restore refuses a node whose parent is staged for deletion, under the parent's lock, and
/// takes one back below a live parent, marked in the same write.
#[tokio::test]
async fn a_restore_refuses_a_node_whose_parent_is_staged_for_deletion() {
    with_state(|repository, state| async move {
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let sub = add(&state, &repository, t, directory("sub"), "sub").await;
        let b = add(&state, &repository, sub, file("b"), "b").await;
        state
            .node_mark(&repository, b, DELETED)
            .await
            .expect("b's deletion");
        state
            .node_mark(&repository, sub, DELETED)
            .await
            .expect("sub's deletion");
        let content = Content {
            mode: 0o644,
            size: 3,
            ..Content::default()
        };

        let refused = state
            .node_restore(&repository, b, &content)
            .await
            .expect_err("refused below a deleted parent");
        assert!(refused.is_invalid_arguments(), "{refused}");
        assert!(record(&state, &repository, b).await.is_staged_delete());

        state
            .node_restore(&repository, sub, &content)
            .await
            .expect("sub restores below a live parent");
        state
            .node_restore(&repository, b, &content)
            .await
            .expect("then b");
        let node = record(&state, &repository, b).await;
        assert!(
            node.is_staged_modify() && node.is_dirty_modify(),
            "{:x}",
            node.flags
        );
    })
    .await;
}

/// A pending discard whose subtree holds a sibling cycle ends the marks of that chain at the
/// node it meets again, and answers the broken chain once the other chains are marked.
#[tokio::test]
async fn a_pending_discard_ends_a_chain_that_loops() {
    with_state(|repository, state| async move {
        let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
        let c = added(&state, &repository, n, file("c"), "c").await;
        let m = added(&state, &repository, n, directory("m"), "m").await;
        let e = added(&state, &repository, m, file("e"), "e").await;
        // `n`'s chain runs `m`, `c`; `c` now leads back to `m`.
        let block = state
            .block(repository.clone(), NodeBlock::index(c))
            .await
            .expect("c's block");
        state.node_edit_for_tests(&block, c, |node| node.sibling = m);

        let looped = state
            .node_discard_pending(repository.clone(), n, ROOT_NODE, hash_string("n"))
            .await;

        assert!(looped.is_err(), "the loop is a broken chain");
        for node in [n, m, c, e] {
            assert!(
                record(&state, &repository, node).await.is_discarded(),
                "{node} is marked"
            );
        }
    })
    .await;
}

/// A restore of an invalid id is a bad argument, before any block is read.
#[tokio::test]
async fn a_restore_of_an_invalid_id_is_a_bad_argument() {
    with_state(|repository, state| async move {
        let refused = state
            .node_restore(&repository, INVALID_NODE, &Content::default())
            .await
            .expect_err("refused");

        assert!(refused.is_invalid_arguments(), "{refused}");
    })
    .await;
}

/// A pending discard meeting a child whose parent link names another directory marks the other
/// chains and answers the broken chain; the drain then meets it too, frees nothing and keeps the
/// root queued, and once the link is mended a drain frees the whole subtree.
#[tokio::test]
async fn a_pending_discard_and_its_drain_fail_at_a_misparented_child() {
    with_state(|repository, state| async move {
        let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
        let c = added(&state, &repository, n, file("c"), "c").await;
        let m = added(&state, &repository, n, directory("m"), "m").await;
        let e = added(&state, &repository, m, file("e"), "e").await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        state.node_edit_for_tests(&block, e, |node| node.parent = ROOT_NODE);
        let before = free_slots(&state, &repository).await;

        let marked = state
            .node_discard_pending(repository.clone(), n, ROOT_NODE, hash_string("n"))
            .await;

        assert!(marked.is_err(), "the misparented child breaks its chain");
        for node in [n, c, m] {
            assert!(
                record(&state, &repository, node).await.is_discarded(),
                "{node} is marked"
            );
        }
        let drained = state.node_free_pending(&repository).await;
        assert!(drained.is_err(), "the drain meets the broken chain too");
        assert_eq!(
            free_slots(&state, &repository).await,
            before,
            "and frees nothing"
        );

        state.node_edit_for_tests(&block, e, |node| node.parent = m);
        let counts = state
            .node_free_pending(&repository)
            .await
            .expect("the drain, the link mended");
        assert_eq!(
            (counts.file_count, counts.directory_count),
            (2, 2),
            "the queue kept the root, and the drain frees its whole subtree"
        );
    })
    .await;
}

/// A staged add whose climb meets an ancestor a detach took answers success, the add having
/// come first: the node is published, staged and dirty added.
#[tokio::test]
async fn a_staged_add_whose_climb_meets_a_detached_ancestor_succeeds() {
    with_state(|repository, state| async move {
        let t = add(&state, &repository, ROOT_NODE, directory("t"), "t").await;
        let sub = add(&state, &repository, t, directory("sub"), "sub").await;
        assert!(
            state
                .node_detach(repository.clone(), t, lore_revision::state::Detach::Always)
                .await
                .expect("the detach"),
            "t is detached"
        );

        let x = state
            .node_add_staged(repository.clone(), sub, file("x"), "x")
            .await
            .expect("the add succeeds, its climb ended at the detached ancestor");

        let node = record(&state, &repository, x).await;
        assert!(
            node.is_staged_add() && node.is_dirty_add(),
            "{:x}",
            node.flags
        );
    })
    .await;
}

/// A drain whose roots lie in a later block before an earlier one reads each node from its own
/// block: the blocks it holds are kept in index order.
#[tokio::test]
async fn a_drain_meeting_a_later_block_first_frees_each_root() {
    with_state(|repository, state| async move {
        let early = added(&state, &repository, ROOT_NODE, directory("early"), "early").await;
        let early_child = added(&state, &repository, early, file("a"), "a").await;
        let fill = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        for index in 0.. {
            let name = format!("f{index}");
            let id = add(&state, &repository, fill, file(&name), &name).await;
            if Node::index(id) == BLOCK_NODE_COUNT - 1 {
                break;
            }
        }
        let late = added(&state, &repository, ROOT_NODE, directory("late"), "late").await;
        let late_child = added(&state, &repository, late, file("b"), "b").await;
        assert_eq!(
            NodeBlock::index(late_child),
            1,
            "the late root lies in block 1"
        );
        for (root, name) in [(late, "late"), (early, "early")] {
            state
                .node_discard_pending(repository.clone(), root, ROOT_NODE, hash_string(name))
                .await
                .expect("the discard")
                .expect("the root stands where it was found");
        }

        let counts = state
            .node_free_pending(&repository)
            .await
            .expect("the drain");

        assert_eq!((counts.file_count, counts.directory_count), (2, 2));
        let free = free_slots(&state, &repository).await;
        for node in [early, early_child] {
            assert!(
                free.contains(&Node::index(node)),
                "{node} is freed in block 0"
            );
        }
        let block = state.block(repository.clone(), 1).await.expect("block 1");
        assert_eq!(
            block.read().node_block().node_count,
            0,
            "block 1, its two nodes freed, started over"
        );
    })
    .await;
}

/// An unserved restore, whose callers run where no C API delete does, takes back a node below
/// an added directory without the moves permit.
#[tokio::test]
async fn an_unserved_restore_below_an_added_directory_takes_the_node_back() {
    with_state(|repository, state| async move {
        let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
        let x = add(&state, &repository, n, file("x"), "x").await;
        state
            .node_mark(&repository, x, DELETED)
            .await
            .expect("x's deletion");

        let restored = state
            .node_rewrite(
                &repository,
                x,
                &Content::default(),
                lore_revision::state::Rewrite::Restore,
            )
            .await
            .expect("the restore");

        assert!(restored, "the restore wrote");
        assert!(!record(&state, &repository, x).await.is_staged_delete());
    })
    .await;
}

/// A pending discard counts neither a node staged for deletion nor anything below it: here a live
/// file left below a directory a delete staged, as a delete that failed part way leaves one.
/// Every node is marked all the same.
#[tokio::test]
async fn a_pending_discard_counts_nothing_below_a_node_staged_for_deletion() {
    with_state(|repository, state| async move {
        let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
        let t = add(&state, &repository, n, directory("t"), "t").await;
        let c = add(&state, &repository, t, file("c"), "c").await;
        state
            .node_mark(&repository, t, DELETED)
            .await
            .expect("t's deletion");
        assert!(
            !record(&state, &repository, c).await.is_staged_delete(),
            "c is live below t"
        );

        let marked = state
            .node_discard_pending(repository.clone(), n, ROOT_NODE, hash_string("n"))
            .await
            .expect("the discard")
            .expect("n stands where it was found");

        assert_eq!(marked, 1, "only n is counted");
        for node in [n, t, c] {
            assert!(
                record(&state, &repository, node).await.is_discarded(),
                "{node} is marked"
            );
        }
    })
    .await;
}

/// A drain meeting its root again inside the root's subtree, here as a child's sibling, fails
/// that step's parent check, frees nothing and keeps the root queued, where walking the root
/// again would never end; once the link is mended a drain frees the whole subtree. The drain
/// runs on a thread of its own, so a walk that never ends fails the test rather than hanging it.
#[test]
fn a_drain_whose_chain_leads_back_to_its_root_fails_and_keeps_it() {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    };
    let (state, repository, c, sibling, before) =
        runtime().block_on(LORE_CONTEXT.scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();
            let n = added(&state, &repository, ROOT_NODE, directory("n"), "n").await;
            let c = added(&state, &repository, n, file("c"), "c").await;
            let m = added(&state, &repository, n, directory("m"), "m").await;
            added(&state, &repository, m, file("e"), "e").await;
            state
                .node_discard_pending(repository.clone(), n, ROOT_NODE, hash_string("n"))
                .await
                .expect("the discard")
                .expect("n stands where it was found");
            let before = free_slots(&state, &repository).await;
            let sibling = record(&state, &repository, c).await.sibling;
            let block = state.block(repository.clone(), 0).await.expect("block 0");
            state.node_edit_for_tests(&block, c, |node| node.sibling = n);
            (state, repository, c, sibling, before)
        }));

    let (sender, receiver) = std::sync::mpsc::channel();
    {
        let (state, repository) = (state.clone(), repository.clone());
        std::thread::spawn(move || {
            let drained = runtime().block_on(
                LORE_CONTEXT.scope(setup_test_execution(), state.node_free_pending(&repository)),
            );
            let _ = sender.send(drained.map(|_| ()));
        });
    }
    let drained = match receiver.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(drained) => drained,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the drain's thread panicked")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // A walk that never ends grows without bound: end the process rather than let it, the
            // reason written past the harness's capture, which the abort would discard.
            const REASON: &[u8] =
                b"a_drain_whose_chain_leads_back_to_its_root_fails_and_keeps_it: \
                the drain walks its root again without end\n";
            let _ = std::io::Write::write_all(&mut std::io::stderr(), REASON);
            std::process::abort();
        }
    };

    assert!(
        drained.is_err(),
        "the root met again fails its parent check"
    );
    runtime().block_on(LORE_CONTEXT.scope(setup_test_execution(), async {
        assert_eq!(
            free_slots(&state, &repository).await,
            before,
            "the drain frees nothing"
        );
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        state.node_edit_for_tests(&block, c, |node| node.sibling = sibling);
        let counts = state
            .node_free_pending(&repository)
            .await
            .expect("the drain, the link mended");
        assert_eq!(
            (counts.file_count, counts.directory_count),
            (2, 2),
            "the queue kept the root, and the drain frees its whole subtree"
        );
    }));
}
