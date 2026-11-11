// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the unserved tiers' marks, clears and rewrites: the fused staged and dirty mark with
//! its early-outs, its root rule and its extension flags, the dirty clear with its clean-ancestor
//! stop, and the rewrite of a node's content with its restore; and for the changes in place every
//! tier makes through `NodeContent`.

use std::sync::Arc;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Clear;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_revision::state::NodeContent;
use lore_revision::state::OwnFlags;
use lore_revision::state::Rewrite;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use super::add;
use super::directory;
use super::file;
use super::null_repository;
use super::record;
use super::reload;
use super::with_repository;
use super::with_state;

async fn mark(state: &State, repository: &Arc<RepositoryContext>, node: NodeID, mark: Mark) {
    state
        .node_mark(repository, node, mark)
        .await
        .expect("the mark succeeds");
}

async fn clear(state: &State, repository: &Arc<RepositoryContext>, node: NodeID, clear: Clear) {
    state
        .node_clear(repository, node, clear)
        .await
        .expect("the clear succeeds");
}

/// Runs `edit` on `node`'s record as only a fixture writes one.
async fn edit(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    edit: impl FnOnce(&mut Node),
) {
    let block = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the node's block");
    state.node_edit_for_tests(&block, node, edit);
}

/// A clean tree: `a/b/c` with files `f` and `g` in `c`, and file `h` in `a`.
struct Chain {
    a: NodeID,
    b: NodeID,
    c: NodeID,
    f: NodeID,
    g: NodeID,
    h: NodeID,
}

async fn chain(state: &State, repository: &Arc<RepositoryContext>) -> Chain {
    let a = add(state, repository, ROOT_NODE, directory("a"), "a").await;
    let b = add(state, repository, a, directory("b"), "b").await;
    let c = add(state, repository, b, directory("c"), "c").await;
    let f = add(state, repository, c, file("f"), "f").await;
    let g = add(state, repository, c, file("g"), "g").await;
    let h = add(state, repository, a, file("h"), "h").await;
    Chain { a, b, c, f, g, h }
}

const NO_EXT: Ext = Ext::Set(NodeExtFlags::NoFlags);

/// A dirty mark sets the target's action and each ancestor's base `Dirty` bit, and a later mark
/// stops at the first ancestor already dirty, leaving the ones above it as they are.
#[tokio::test]
async fn an_unserved_dirty_mark_climbs_to_the_first_ancestor_marked() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;

        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        assert!(record(&state, &repository, t.f).await.is_dirty_modify());
        for dir in [t.c, t.b, t.a] {
            let held = record(&state, &repository, dir).await;
            assert!(held.is_dirty(), "directory {dir} is dirty");
            assert_eq!(
                held.flags & NodeFlags::ActionBits.bits(),
                0,
                "an ancestor takes no action"
            );
        }
        assert!(
            !record(&state, &repository, ROOT_NODE).await.is_dirty(),
            "no mark changes the root's flags"
        );

        // With `b` clean again, a mark of `g` stops at `c`, which is dirty.
        edit(&state, &repository, t.b, Node::clear_dirty_flags).await;
        mark(
            &state,
            &repository,
            t.g,
            Mark::dirty(NodeFlags::DirtyAdd, NO_EXT, false),
        )
        .await;
        assert!(record(&state, &repository, t.g).await.is_dirty_add());
        assert!(!record(&state, &repository, t.b).await.is_dirty());
    })
    .await;
}

/// An unforced mark already in place writes nothing; a forced one writes the target again and
/// stops at its parent, already dirty.
#[tokio::test]
async fn an_unserved_mark_in_place_writes_nothing_unless_forced() {
    with_state(|repository, first| async move {
        let t = chain(&first, &repository).await;
        mark(
            &first,
            &repository,
            t.f,
            Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify),
        )
        .await;

        let state = reload(&first, &repository).await;
        let index = NodeBlock::index(t.f);
        let both = Mark {
            staged: Some((NodeFlags::StagedModify, false)),
            dirty: Some((NodeFlags::DirtyModify, NO_EXT, false)),
        };
        mark(&state, &repository, t.f, both).await;
        assert!(!state.block_recorded_changed(index), "an early-out writes");
        assert!(!state.is_dirty());

        // Flags outside a half's bits take no part in its early-out: directory `c`, staged and
        // dirty, is marked in place.
        let outside = Mark {
            staged: Some((NodeFlags::Staged | NodeFlags::File, false)),
            dirty: Some((NodeFlags::Dirty | NodeFlags::File, Ext::Keep, false)),
        };
        mark(&state, &repository, t.c, outside).await;
        assert!(!state.block_recorded_changed(NodeBlock::index(t.c)));
        assert!(!record(&state, &repository, t.c).await.is_file());

        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, true),
        )
        .await;
        assert!(state.block_recorded_changed(index));
    })
    .await;
}

/// A staged mark keeps the target's merge flags, sets `Staged` on each ancestor up to the first
/// staged, and leaves dirty flags alone.
#[tokio::test]
async fn an_unserved_staged_mark_keeps_merge_flags() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;

        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::StagedMergeConflict, true),
        )
        .await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::StagedModify, true),
        )
        .await;
        let held = record(&state, &repository, t.f).await;
        assert!(held.is_staged_modify());
        assert!(held.is_staged_merge_conflict(), "the merge flags stand");
        assert!(!held.is_dirty());
        for dir in [t.c, t.b, t.a] {
            let held = record(&state, &repository, dir).await;
            assert!(held.is_staged() && !held.is_dirty(), "directory {dir}");
        }
    })
    .await;
}

/// A staged mark of no merge state of its own keeps a resolved conflict resolved, with the side
/// it took, whatever action it sets: a modification, a move, a deletion. One that sets a merge
/// state sets it, keeping a merge and a conflict: a conflict mark, or a merge's mark of
/// `StagedMerge` alone, drops the resolution and the side, and a resolve taking the other side
/// replaces the side.
#[tokio::test]
async fn an_unserved_staged_mark_keeps_a_resolution_unless_it_sets_a_merge_state() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let resolved_theirs = NodeFlags::StagedMergeResolved | NodeFlags::StagedMergeTheirs;
        for (node, action) in [
            (t.f, NodeFlags::StagedModify),
            (t.g, NodeFlags::StagedMove),
            (t.h, NodeFlags::StagedDelete),
        ] {
            mark(
                &state,
                &repository,
                node,
                Mark::staged(resolved_theirs, true),
            )
            .await;
            mark(&state, &repository, node, Mark::staged(action, true)).await;
            let held = record(&state, &repository, node).await;
            assert_eq!(
                held.flags & action.bits(),
                action.bits(),
                "{action:?} is set"
            );
            assert!(
                held.is_staged_merge_resolved() && held.is_staged_merge_theirs(),
                "{action:?} keeps the resolution and its side, flags {:#x}",
                held.flags
            );
        }

        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::StagedMergeConflict, true),
        )
        .await;
        let unresolved = record(&state, &repository, t.f).await;
        assert!(unresolved.is_staged_merge_conflict());
        assert!(!unresolved.is_staged_merge_resolved() && !unresolved.is_staged_merge_theirs());

        let ours =
            NodeFlags::StagedMergeResolved | NodeFlags::StagedMergeMine | NodeFlags::StagedModify;
        mark(&state, &repository, t.g, Mark::staged(ours, true)).await;
        let switched = record(&state, &repository, t.g).await;
        assert!(switched.is_staged_merge_resolved() && switched.is_staged_merge_mine());
        assert!(!switched.is_staged_merge_theirs(), "the side is replaced");

        mark(
            &state,
            &repository,
            t.h,
            Mark::staged(resolved_theirs, true),
        )
        .await;
        mark(
            &state,
            &repository,
            t.h,
            Mark::staged(NodeFlags::StagedMerge, true),
        )
        .await;
        let merged = record(&state, &repository, t.h).await;
        assert!(merged.is_staged_merge_conflict(), "the conflict stands");
        assert!(!merged.is_staged_merge_resolved() && !merged.is_staged_merge_theirs());
    })
    .await;
}

/// One mark carrying both halves leaves every node as the staged mark and then the dirty mark
/// would, through every early-out combination: the halves climb and stop on their own.
#[tokio::test]
async fn an_unserved_fused_mark_matches_its_halves_in_turn() {
    with_repository(|repository| async move {
        let marks = [
            (NodeFlags::StagedModify, NodeFlags::DirtyModify, true),
            (NodeFlags::StagedAdd, NodeFlags::DirtyModify, false),
            (NodeFlags::StagedMove, NodeFlags::DirtyMove, false),
            (NodeFlags::Staged, NodeFlags::Dirty, false),
        ];
        // Each start: nothing marked, the dirty half already climbed to `a`, the staged half to `c`.
        for start in 0..3 {
            for &(staged, dirty, force) in &marks {
                let (fused, halves) = (State::new(), State::new());
                let mut trees = Vec::new();
                for state in [&fused, &halves] {
                    let t = chain(state, &repository).await;
                    match start {
                        1 => {
                            mark(
                                state,
                                &repository,
                                t.h,
                                Mark::dirty(NodeFlags::DirtyAdd, NO_EXT, false),
                            )
                            .await;
                        }
                        2 => {
                            mark(
                                state,
                                &repository,
                                t.g,
                                Mark::staged(NodeFlags::StagedAdd, false),
                            )
                            .await;
                        }
                        _ => {}
                    }
                    trees.push(t);
                }
                let ext = Ext::Set(NodeExtFlags::NoFlags);
                mark(
                    &fused,
                    &repository,
                    trees[0].f,
                    Mark {
                        staged: Some((staged, force)),
                        dirty: Some((dirty, ext, force)),
                    },
                )
                .await;
                mark(
                    &halves,
                    &repository,
                    trees[1].f,
                    Mark::staged(staged, force),
                )
                .await;
                mark(
                    &halves,
                    &repository,
                    trees[1].f,
                    Mark::dirty(dirty, ext, force),
                )
                .await;
                let t = &trees[0];
                for node in [ROOT_NODE, t.a, t.b, t.c, t.f, t.g, t.h] {
                    assert_eq!(
                        record(&fused, &repository, node).await.flags,
                        record(&halves, &repository, node).await.flags,
                        "node {node}, start {start}, mark {staged:?} {dirty:?} {force}"
                    );
                }
            }
        }
    })
    .await;
}

/// The target alone takes `Ext::Set`; `Ext::Keep` leaves its extension flags; an extension
/// flag the target lacks defeats an unforced early-out.
#[tokio::test]
async fn an_unserved_mark_sets_extension_flags_on_its_target_only() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let local = Ext::Set(NodeExtFlags::LocalContent);

        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyAdd, NO_EXT, false),
        )
        .await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyAdd, local, false),
        )
        .await;
        assert_eq!(
            record(&state, &repository, t.f).await.ext_flags(),
            NodeExtFlags::LocalContent
        );
        assert_eq!(
            record(&state, &repository, t.c).await.ext_flags(),
            NodeExtFlags::NoFlags
        );

        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, true),
        )
        .await;
        let held = record(&state, &repository, t.f).await;
        assert!(held.is_dirty_modify());
        assert_eq!(held.ext_flags(), NodeExtFlags::LocalContent);

        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, true),
        )
        .await;
        assert_eq!(
            record(&state, &repository, t.f).await.ext_flags(),
            NodeExtFlags::NoFlags
        );
    })
    .await;
}

/// The dirty half stops at a Discarded node and the mark answers that it was not found, after
/// the staged half, which accepts Discarded nodes, has climbed past it to the root.
#[tokio::test]
async fn an_unserved_mark_through_a_detached_directory_is_not_found() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        state
            .node_detach(repository.clone(), t.b, Detach::Always)
            .await
            .expect("the detach succeeds");
        assert!(record(&state, &repository, t.b).await.is_discarded());

        let error = state
            .node_mark(
                &repository,
                t.f,
                Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify),
            )
            .await
            .expect_err("a mark through a detached directory fails");
        assert!(error.is_node_not_found(), "{error}");
        let held = record(&state, &repository, t.f).await;
        assert!(held.is_staged_modify() && held.is_dirty_modify());
        assert!(record(&state, &repository, t.c).await.is_dirty());
        let detached = record(&state, &repository, t.b).await;
        assert!(detached.is_staged() && !detached.is_dirty());
        assert!(
            record(&state, &repository, t.a).await.is_staged(),
            "the staged half climbs past the detached directory"
        );

        let error = state
            .node_mark(
                &repository,
                t.b,
                Mark::dirty(NodeFlags::Dirty, Ext::Keep, true),
            )
            .await
            .expect_err("a dirty mark of a detached directory fails");
        assert!(error.is_node_not_found(), "{error}");
        mark(
            &state,
            &repository,
            t.b,
            Mark::staged(NodeFlags::Staged, true),
        )
        .await;
    })
    .await;
}

/// A mark of the root, or of no node, dirties block 0 alone; a walk reaching the root from
/// another block dirties block 0 too.
#[tokio::test]
async fn an_unserved_mark_of_the_root_dirties_block_zero_alone() {
    with_state(|repository, first| async move {
        let fill = add(&first, &repository, ROOT_NODE, directory("fill"), "fill").await;
        for index in 0..BLOCK_NODE_COUNT {
            let name = format!("filler{index}");
            add(&first, &repository, fill, file(&name), &name).await;
        }
        let z = add(&first, &repository, ROOT_NODE, directory("z"), "z").await;
        assert_ne!(NodeBlock::index(z), 0);

        for target in [ROOT_NODE, INVALID_NODE] {
            let state = reload(&first, &repository).await;
            let root = record(&state, &repository, ROOT_NODE).await;
            mark(
                &state,
                &repository,
                target,
                Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify),
            )
            .await;
            assert!(state.block_recorded_changed(0), "target {target}");
            assert!(state.is_dirty());
            assert_eq!(
                record(&state, &repository, ROOT_NODE).await.flags,
                root.flags
            );
        }

        let state = reload(&first, &repository).await;
        mark(
            &state,
            &repository,
            ROOT_NODE,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
        .await;
        assert!(
            !state.block_recorded_changed(NodeBlock::index(z)),
            "a mark of the root marks nothing else"
        );

        let state = reload(&first, &repository).await;
        mark(
            &state,
            &repository,
            z,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
        .await;
        assert!(state.block_recorded_changed(NodeBlock::index(z)));
        assert!(state.block_recorded_changed(0), "the root's block");
    })
    .await;
}

/// A dirty clear of a file clears each directory above it that nothing else keeps dirty, up to
/// the root, whose flags it leaves.
#[tokio::test]
async fn an_unserved_clear_clears_each_ancestor_nothing_keeps_dirty() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        // The root dirty, as no mark leaves it, so a clear of it would show.
        edit(&state, &repository, ROOT_NODE, |node| {
            node.flags |= NodeFlags::Dirty.bits();
        })
        .await;
        let root = record(&state, &repository, ROOT_NODE).await.flags;
        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;

        clear(&state, &repository, t.f, Clear::Dirty).await;
        for node in [t.f, t.c, t.b, t.a] {
            assert!(
                !record(&state, &repository, node).await.is_dirty(),
                "node {node}"
            );
        }
        assert_eq!(record(&state, &repository, ROOT_NODE).await.flags, root);
    })
    .await;
}

/// A dirty clear stops at a directory a dirty child keeps dirty, and leaves it and every
/// directory above it dirty.
#[tokio::test]
async fn an_unserved_clear_stops_below_a_dirty_sibling() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        for node in [t.f, t.g] {
            mark(
                &state,
                &repository,
                node,
                Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
            )
            .await;
        }

        clear(&state, &repository, t.f, Clear::Dirty).await;
        assert!(!record(&state, &repository, t.f).await.is_dirty());
        for node in [t.g, t.c, t.b, t.a] {
            assert!(
                record(&state, &repository, node).await.is_dirty(),
                "node {node}"
            );
        }

        clear(&state, &repository, t.g, Clear::Dirty).await;
        for node in [t.g, t.c, t.b, t.a] {
            assert!(
                !record(&state, &repository, node).await.is_dirty(),
                "node {node}"
            );
        }
    })
    .await;
}

/// A dirty clear ends at a directory already clean: dirty directories above one cleaned some
/// other way stay dirty, the safe direction.
#[tokio::test]
async fn an_unserved_clear_ends_at_a_directory_already_clean() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        // `c` cleaned in place, as an unstage or a reset writes a directory's record.
        edit(&state, &repository, t.c, Node::clear_dirty_flags).await;

        clear(&state, &repository, t.f, Clear::Dirty).await;
        assert!(!record(&state, &repository, t.f).await.is_dirty());
        assert!(record(&state, &repository, t.b).await.is_dirty());
        assert!(record(&state, &repository, t.a).await.is_dirty());
    })
    .await;
}

/// A dirty clear keeps a staged target's staged flags and action; a staged clear drops the
/// target's staged flags alone, leaving its dirty flags and every ancestor's marks.
#[tokio::test]
async fn an_unserved_clear_drops_one_half_of_the_target() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let both = Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify);
        mark(&state, &repository, t.f, both).await;

        clear(&state, &repository, t.f, Clear::Dirty).await;
        let held = record(&state, &repository, t.f).await;
        assert!(!held.is_dirty() && held.is_staged_modify());

        mark(&state, &repository, t.f, both).await;
        clear(&state, &repository, t.f, Clear::Staged).await;
        let held = record(&state, &repository, t.f).await;
        assert!(!held.is_staged() && held.is_dirty_modify());
        let parent = record(&state, &repository, t.c).await;
        assert!(parent.is_staged() && parent.is_dirty());
    })
    .await;
}

/// A clear of a node in a detached directory clears the node and ends there; a clear of the
/// root or of no node does nothing; a clear changing nothing writes nothing.
#[tokio::test]
async fn an_unserved_clear_ends_at_a_detached_directory_and_writes_only_changes() {
    with_state(|repository, first| async move {
        let t = chain(&first, &repository).await;
        mark(
            &first,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        first
            .node_detach(repository.clone(), t.c, Detach::Always)
            .await
            .expect("the detach succeeds");
        clear(&first, &repository, t.f, Clear::Dirty).await;
        assert!(!record(&first, &repository, t.f).await.is_dirty());
        assert!(
            record(&first, &repository, t.b).await.is_dirty(),
            "b keeps its mark"
        );

        let state = reload(&first, &repository).await;
        for target in [ROOT_NODE, INVALID_NODE, t.h] {
            clear(&state, &repository, target, Clear::Dirty).await;
            clear(&state, &repository, target, Clear::Staged).await;
        }
        assert!(!state.block_recorded_changed(0));
        assert!(!state.is_dirty());
    })
    .await;
}

/// A change in place sets a file's content, keeping its file ID where the new address carries
/// none, and records the block changed; a node no longer at the place is not handed to it.
#[tokio::test]
async fn a_change_in_place_sets_content_keeping_the_file_id() {
    with_state(|repository, first| async move {
        let t = chain(&first, &repository).await;
        let file_id = Hash::from_u64(7).to_context();
        edit(&first, &repository, t.f, |node| {
            node.address.context = file_id;
        })
        .await;

        let state = reload(&first, &repository).await;
        let index = NodeBlock::index(t.f);
        let block = state
            .block(repository.clone(), index)
            .await
            .expect("the file's block");
        let place = (t.c, hash_string("f"));
        assert!(
            !state.node_change_in_place(&block, t.f, (t.c, hash_string("g")), |_| {
                unreachable!("a node at another place is not handed")
            })
        );
        assert!(!state.block_recorded_changed(index));
        let second = Address::zero_context_hash(Hash::from_u64(2));
        assert!(state.node_change_in_place(&block, t.f, place, |node| {
            node.set_content(second, 4, 0o644);
            true
        }));
        let held = block.node(Node::index(t.f));
        assert_eq!(held.address.hash, second.hash);
        assert_eq!(held.address.context, file_id, "the file ID is kept");
        assert_eq!((held.size, held.mode), (4, 0o644));
        assert!(state.block_recorded_changed(index));
        assert!(state.is_dirty());
    })
    .await;
}

/// A change in place marks a staged merge conflict resolved and unresolved, and refuses to on a
/// node in no conflict; replaces the action of a dirty node but refuses to make a clean one
/// dirty; and clears dirty and extension flags, keeping staged ones.
#[tokio::test]
async fn a_change_in_place_edits_marks_only_where_consistent() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let block = state
            .block(repository.clone(), NodeBlock::index(t.f))
            .await
            .expect("the file's block");
        let place = (t.c, hash_string("f"));
        let change = |edit: &dyn Fn(&mut NodeContent<'_>) -> bool| {
            state.node_change_in_place(&block, t.f, place, |node| edit(node))
        };
        let flags = || NodeFlags::from_bits_retain(block.node(Node::index(t.f)).flags);

        assert!(!change(
            &|node| node.replace_dirty_action(NodeFlags::DirtyAdd)
        ));
        assert!(!change(&|node| node.set_merge_resolved(true)));
        assert_eq!(flags(), NodeFlags::File, "a refusal changed the record");

        state.node_edit_for_tests(&block, t.f, |node| {
            node.flags |= (NodeFlags::StagedMergeConflict | NodeFlags::DirtyAdd).bits();
            node.ext_flags = NodeExtFlags::LocalContent.bits();
        });
        assert!(change(&|node| node.set_merge_resolved(true)));
        assert!(flags().contains(NodeFlags::StagedMergeResolved));
        // Resolved taking their side, then unresolved: the side goes with the resolution.
        state.node_edit_for_tests(&block, t.f, |node| {
            node.flags |= NodeFlags::StagedMergeTheirs.bits();
        });
        assert!(change(&|node| node.set_merge_resolved(false)));
        assert!(flags().contains(NodeFlags::StagedMergeConflict));
        assert!(!flags().contains(NodeFlags::StagedMergeResolved));
        assert!(
            !flags().contains(NodeFlags::StagedMergeTheirs),
            "no side stays"
        );

        assert!(change(&|node| node.replace_dirty_action(
            NodeFlags::DirtyModify | NodeFlags::Discarded
        )));
        assert!(
            !flags().contains(NodeFlags::Discarded),
            "only action bits change"
        );
        assert!(flags().contains(NodeFlags::DirtyModify));
        assert!(!flags().contains(NodeFlags::DirtyAdd));
        assert!(change(&|node| node.replace_dirty_action(NodeFlags::NoFlags)));
        assert_eq!(
            flags() & NodeFlags::DirtyBits,
            NodeFlags::Dirty,
            "the action dropped, the node still dirty"
        );

        assert!(change(&|node| {
            node.clear_dirty_flags();
            true
        }));
        let held = block.node(Node::index(t.f));
        assert!(!held.is_dirty());
        assert_eq!(held.ext_flags(), NodeExtFlags::NoFlags);
        assert!(held.is_staged_merge_conflict(), "staged flags stay");

        // A directory's clear is the clears' alone: `c` stays dirty.
        let dir_block = state
            .block(repository.clone(), NodeBlock::index(t.c))
            .await
            .expect("the directory's block");
        mark(
            &state,
            &repository,
            t.c,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
        )
        .await;
        assert!(
            !state.node_change_in_place(&dir_block, t.c, (t.b, hash_string("c")), |node| {
                node.clear_dirty_flags()
            })
        );
        assert!(record(&state, &repository, t.c).await.is_dirty());
    })
    .await;
}

/// An owned change, an exclusive tier's, writes a live node with no place given and refuses a
/// Discarded one; its commit setters keep to a node's kind: a file's legacy modification time and
/// a file's every change flag, never a directory's, and a directory's hash and size with its
/// staged and action bits dropped, its dirty flag kept.
#[tokio::test]
async fn an_owned_change_writes_a_live_node_wherever_it_stands() {
    with_state(|repository, first| async move {
        let t = chain(&first, &repository).await;
        let both = Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify);
        for node in [t.f, t.g] {
            mark(&first, &repository, node, both).await;
        }
        edit(&first, &repository, t.f, |node| node.child = 7).await;
        let state = reload(&first, &repository).await;
        let index = NodeBlock::index(t.f);
        assert_eq!(index, NodeBlock::index(t.c), "the chain shares a block");
        let block = state
            .block(repository.clone(), index)
            .await
            .expect("the block");
        let held = |node: NodeID| block.node(Node::index(node));

        assert!(state.node_change_owned(&block, t.f, |node| {
            assert!(node.clear_legacy_mtime());
            node.clear_change_flags()
        }));
        assert_eq!(held(t.f).child, 0);
        assert!(!held(t.f).is_staged() && !held(t.f).is_dirty());
        assert!(state.block_recorded_changed(index));

        assert!(!state.node_change_owned(&block, t.c, |node| node.clear_legacy_mtime()));
        assert!(!state.node_change_owned(&block, t.c, |node| node.clear_change_flags()));
        assert!(
            held(t.c).is_staged() && held(t.c).is_dirty(),
            "the refusals changed nothing"
        );

        assert!(state.node_change_owned(&block, t.c, |node| {
            node.set_hash_and_size(Hash::from_u64(9), 12);
            node.clear_staged_and_action_bits();
            true
        }));
        assert_eq!(
            (held(t.c).address.hash, held(t.c).size),
            (Hash::from_u64(9), 12)
        );
        assert!(!held(t.c).is_staged() && held(t.c).is_dirty());

        // The staged and action bits alone, where clearing the staged flags keeps a dirty node's
        // action.
        assert!(state.node_change_owned(&block, t.g, |node| {
            node.clear_staged_and_action_bits();
            true
        }));
        assert_eq!(
            NodeFlags::from_bits_retain(held(t.g).flags)
                & (NodeFlags::StagedBits | NodeFlags::DirtyBits),
            NodeFlags::Dirty
        );

        state.node_edit_for_tests(&block, t.g, |node| {
            node.flags |= NodeFlags::Discarded.bits();
        });
        assert!(!state.node_change_owned(&block, t.g, |_| {
            unreachable!("a Discarded node is not handed")
        }));
    })
    .await;
}

/// An owned clear drops a node's own flags whatever its kind, a directory's dirty flags whatever
/// its children hold among them, and nothing above it; one changing nothing writes nothing, and a
/// Discarded node is left as it is.
#[tokio::test]
async fn an_owned_clear_drops_a_nodes_own_flags_alone() {
    with_state(|repository, first| async move {
        let t = chain(&first, &repository).await;
        let both = Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify);
        mark(&first, &repository, t.f, both).await;
        let state = reload(&first, &repository).await;
        let index = NodeBlock::index(t.c);
        let block = state
            .block(repository.clone(), index)
            .await
            .expect("the block");
        let held = |node: NodeID| block.node(Node::index(node));

        assert!(state.node_clear_own(&block, t.c, OwnFlags::Dirty));
        assert!(
            state.block_recorded_changed(index) && state.is_dirty(),
            "a clear that changed a flag records the block changed"
        );
        assert!(!held(t.c).is_dirty() && held(t.c).is_staged());
        assert!(held(t.f).is_dirty(), "the child keeps its flags");
        assert!(
            held(t.b).is_dirty() && held(t.b).is_staged(),
            "nothing above changes"
        );
        assert!(state.node_clear_own(&block, t.c, OwnFlags::Staged));
        assert!(!state.node_clear_own(&block, t.c, OwnFlags::Change));
        assert!(state.node_clear_own(&block, t.f, OwnFlags::Change));
        assert!(!held(t.f).is_dirty() && !held(t.f).is_staged());
        assert_eq!(held(t.f).ext_flags(), NodeExtFlags::NoFlags);

        let state = reload(&state, &repository).await;
        let block = state
            .block(repository.clone(), index)
            .await
            .expect("the block");
        assert!(!state.node_clear_own(&block, t.f, OwnFlags::Change));
        assert!(
            !state.block_recorded_changed(index),
            "a clear changing nothing"
        );
        assert!(!state.is_dirty());
        state.node_edit_for_tests(&block, t.g, |node| {
            node.flags |= (NodeFlags::Discarded | NodeFlags::DirtyModify).bits();
        });
        assert!(!state.node_clear_own(&block, t.g, OwnFlags::Change));
        assert!(
            block.node(Node::index(t.g)).is_dirty(),
            "a Discarded node keeps its flags"
        );
    })
    .await;
}

/// A vacated clear climbs from the directory itself, for one a dirty child left: it and each
/// directory above it that nothing else keeps dirty are cleared, up to one a dirty child keeps
/// dirty; a directory a dirty child keeps dirty is left as it is.
#[tokio::test]
async fn a_vacated_clear_climbs_from_the_directory_itself() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        for node in [t.f, t.h] {
            mark(
                &state,
                &repository,
                node,
                Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
            )
            .await;
        }
        state
            .node_detach(repository.clone(), t.f, Detach::Always)
            .await
            .expect("the detach succeeds");

        mark(
            &state,
            &repository,
            t.g,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        clear(&state, &repository, t.c, Clear::Vacated).await;
        assert!(
            record(&state, &repository, t.c).await.is_dirty(),
            "g keeps c dirty"
        );

        edit(&state, &repository, t.g, Node::clear_dirty_flags).await;
        clear(&state, &repository, t.c, Clear::Vacated).await;
        for node in [t.c, t.b] {
            assert!(
                !record(&state, &repository, node).await.is_dirty(),
                "node {node}"
            );
        }
        assert!(
            record(&state, &repository, t.a).await.is_dirty(),
            "h keeps a dirty"
        );
    })
    .await;
}

/// A staged clear drops the target's staged flags and climbs no further: a directory nothing
/// below keeps dirty, here dirtied by a child's detach, keeps its mark.
#[tokio::test]
async fn an_unserved_staged_clear_leaves_every_ancestor() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        state
            .node_detach(repository.clone(), t.g, Detach::Always)
            .await
            .expect("the detach succeeds");
        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::StagedModify, false),
        )
        .await;
        assert!(!record(&state, &repository, t.f).await.is_dirty());

        clear(&state, &repository, t.f, Clear::Staged).await;
        assert!(!record(&state, &repository, t.f).await.is_staged());
        for dir in [t.c, t.b, t.a] {
            let held = record(&state, &repository, dir).await;
            assert!(held.is_dirty() && held.is_staged(), "directory {dir}");
        }
    })
    .await;
}

/// A dirty clear refused at a directory leaves its own action: the refusal comes before any
/// write.
#[tokio::test]
async fn an_unserved_clear_refused_leaves_the_directorys_action() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.c,
            Mark::dirty(NodeFlags::DirtyMove, NO_EXT, false),
        )
        .await;
        for node in [t.f, t.g] {
            mark(
                &state,
                &repository,
                node,
                Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
            )
            .await;
        }

        clear(&state, &repository, t.f, Clear::Dirty).await;
        assert!(record(&state, &repository, t.c).await.is_dirty_move());
    })
    .await;
}

/// A mark and a clear crossing from a block to another and back: a file in block 1 below a
/// directory in block 0.
#[tokio::test]
async fn an_unserved_mark_and_clear_cross_blocks() {
    with_state(|repository, state| async move {
        let fill = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        let mut last = fill;
        for index in 0..BLOCK_NODE_COUNT {
            let name = format!("filler{index}");
            last = add(&state, &repository, fill, file(&name), &name).await;
        }
        assert_eq!(NodeBlock::index(fill), 0);
        assert_ne!(NodeBlock::index(last), 0);

        mark(
            &state,
            &repository,
            last,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        assert!(record(&state, &repository, last).await.is_dirty_modify());
        assert!(record(&state, &repository, fill).await.is_dirty());

        clear(&state, &repository, last, Clear::Dirty).await;
        assert!(!record(&state, &repository, last).await.is_dirty());
        assert!(!record(&state, &repository, fill).await.is_dirty());
    })
    .await;
}

/// A mark leaves an ancestor already marked as it is, its actions and merge flags included,
/// however forced the mark of the target; ancestors it marks take the base bits alone, never
/// the target's action or merge flags.
#[tokio::test]
async fn an_unserved_mark_leaves_marked_ancestors_as_they_are() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.c,
            Mark::staged_and_dirty(NodeFlags::StagedMove, NodeFlags::DirtyMove),
        )
        .await;
        edit(&state, &repository, t.c, |node| {
            node.flags |= NodeFlags::StagedMerge.bits();
        })
        .await;
        // `b` and `a` clean again, as a clear above `c` alone would leave them.
        for dir in [t.b, t.a] {
            edit(&state, &repository, dir, |node| {
                node.flags &= !(NodeFlags::StagedBits | NodeFlags::DirtyBits).bits();
            })
            .await;
        }
        let held = record(&state, &repository, t.c).await.flags;

        mark(
            &state,
            &repository,
            t.f,
            Mark::staged_and_dirty(NodeFlags::StagedMergeConflict, NodeFlags::DirtyModify),
        )
        .await;
        assert_eq!(record(&state, &repository, t.c).await.flags, held);
        assert!(
            record(&state, &repository, t.f)
                .await
                .is_staged_merge_conflict()
        );
        assert_eq!(
            record(&state, &repository, t.b).await.flags,
            0,
            "the walk stops at c"
        );

        // From `h`, directly below `a`: `a` takes the base bits alone.
        mark(
            &state,
            &repository,
            t.h,
            Mark::staged_and_dirty(NodeFlags::StagedMergeConflict, NodeFlags::DirtyModify),
        )
        .await;
        assert_eq!(
            NodeFlags::from_bits_retain(record(&state, &repository, t.a).await.flags),
            NodeFlags::Staged | NodeFlags::Dirty
        );
    })
    .await;
}

/// A dirty mark reaching a clean directory staged with an action keeps the action: the two
/// halves share its bits, and the staged action is not the mark's to drop.
#[tokio::test]
async fn an_unserved_mark_keeps_a_clean_directorys_staged_action() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.c,
            Mark::staged_and_dirty(NodeFlags::StagedMove, NodeFlags::DirtyMove),
        )
        .await;
        // A scan finds every file of `c` unchanged: `c` clean, its staged move kept.
        edit(&state, &repository, t.c, Node::clear_dirty_flags).await;
        let held = record(&state, &repository, t.c).await;
        assert!(held.is_staged_move() && !held.is_dirty());

        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        let held = record(&state, &repository, t.c).await;
        assert!(held.is_staged_move(), "the staged move stands");
        assert!(held.is_dirty());

        // A forced base mark of a dirty node replaces its action, as before.
        mark(
            &state,
            &repository,
            t.c,
            Mark::dirty(NodeFlags::Dirty, Ext::Keep, true),
        )
        .await;
        assert_eq!(
            record(&state, &repository, t.c).await.flags & NodeFlags::ActionBits.bits(),
            0
        );
    })
    .await;
}

/// The scan's directory chain for a path below a clean directory marks that directory, and its
/// ancestors, above the directories it adds.
#[tokio::test]
async fn a_scan_chain_marks_the_directory_above_its_adds() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;

        let x = lore_revision::state::os_diff::ensure_scan_dir_chain(
            repository.clone(),
            state.clone(),
            ROOT_NODE,
            "a/b/new/x",
        )
        .await
        .expect("the chain is made");
        let new = record(&state, &repository, x).await.parent;
        for added in [x, new] {
            assert!(record(&state, &repository, added).await.is_dirty_add());
        }
        assert_eq!(record(&state, &repository, new).await.parent, t.b);
        for dir in [t.b, t.a] {
            assert!(
                record(&state, &repository, dir).await.is_dirty(),
                "directory {dir}"
            );
        }
        assert!(!record(&state, &repository, t.c).await.is_dirty());
    })
    .await;
}

/// A forced staged mark of a node holding its flags writes it again, dropping staged bits the
/// flags lack, and climbs on, staging an ancestor that lost its mark; an unforced one stops at
/// the node.
#[tokio::test]
async fn a_forced_staged_mark_writes_and_climbs_past_a_node_holding_it() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::StagedAdd, false),
        )
        .await;
        edit(&state, &repository, t.c, |node| {
            node.flags &= !NodeFlags::StagedBits.bits();
        })
        .await;

        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::Staged, false),
        )
        .await;
        assert!(record(&state, &repository, t.f).await.is_staged_add());
        assert!(!record(&state, &repository, t.c).await.is_staged());

        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::Staged, true),
        )
        .await;
        let held = record(&state, &repository, t.f).await;
        assert!(
            held.is_staged() && !held.is_staged_add(),
            "the forced flags replace"
        );
        assert!(record(&state, &repository, t.c).await.is_staged());
    })
    .await;
}

/// A staged mark keeps a node's staged merge flag where the node holds no conflict.
#[tokio::test]
async fn an_unserved_staged_mark_keeps_a_merge_without_conflict() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::StagedMerge, true),
        )
        .await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::staged(NodeFlags::StagedModify, true),
        )
        .await;
        let held = record(&state, &repository, t.f).await;
        assert!(held.is_staged_merge() && held.is_staged_modify());
        assert!(!held.is_staged_merge_conflict());
    })
    .await;
}

/// The scan's directory chain refuses a reserved name anywhere in its path before it adds
/// anything, so no add stands unmarked under a clean directory.
#[tokio::test]
async fn a_scan_chain_with_a_reserved_name_adds_nothing() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;

        let error = lore_revision::state::os_diff::ensure_scan_dir_chain(
            repository.clone(),
            state.clone(),
            ROOT_NODE,
            "a/b/new/.lore/x",
        )
        .await
        .expect_err("a reserved name is refused");
        assert!(error.is_node_not_found(), "{error}");
        assert!(
            state
                .find_subnode(repository.clone(), t.b, hash_string("new"))
                .await
                .is_err(),
            "nothing is added before the refusal"
        );
        assert!(!record(&state, &repository, t.b).await.is_dirty());
    })
    .await;
}

/// The scan's directory chain refuses to add below a file or a link, or to end at one: a file
/// holds no children, and a link's child is the node it exposes in its linked tree. It leaves the
/// node as it was.
#[tokio::test]
async fn a_scan_chain_below_or_at_a_file_or_a_link_adds_nothing() {
    with_state(|repository, state| async move {
        let link_node = Node {
            flags: NodeFlags::Link.bits(),
            name_hash: hash_string("lnk"),
            child: 7,
            ..Default::default()
        };
        let link = add(&state, &repository, ROOT_NODE, link_node, "lnk").await;
        let held_file = add(&state, &repository, ROOT_NODE, file("f"), "f").await;
        let file_record = record(&state, &repository, held_file).await;

        for (node, path) in [
            (link, "lnk/new"),
            (link, "lnk"),
            (held_file, "f/new"),
            (held_file, "f"),
        ] {
            let error = lore_revision::state::os_diff::ensure_scan_dir_chain(
                repository.clone(),
                state.clone(),
                ROOT_NODE,
                path,
            )
            .await
            .expect_err("a chain below or at a file or a link is refused");
            assert!(error.is_node_not_found(), "{path}: {error}");
            let held = record(&state, &repository, node).await;
            let expected = if node == link { link_node } else { file_record };
            assert_eq!(
                (held.child, held.flags),
                (expected.child, expected.flags),
                "the refused chain {path} changed the node"
            );
        }
    })
    .await;
}

/// The scan's directory chain refuses a name no add would store, anywhere in its path, before it
/// adds anything: a separator in a name, or a name over the length bound.
#[tokio::test]
async fn a_scan_chain_with_an_unstorable_name_adds_nothing() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;

        let long = format!("a/b/new/{}", "n".repeat(MAX_NODE_NAME_LEN + 1));
        for path in ["a/b/new/x\\y", long.as_str()] {
            let error = lore_revision::state::os_diff::ensure_scan_dir_chain(
                repository.clone(),
                state.clone(),
                ROOT_NODE,
                path,
            )
            .await
            .expect_err("an unstorable name is refused");
            assert!(!error.is_node_not_found(), "{error}");
            assert!(
                state
                    .find_subnode(repository.clone(), t.b, hash_string("new"))
                    .await
                    .is_err(),
                "nothing is added before the refusal"
            );
            assert!(!record(&state, &repository, t.b).await.is_dirty());
        }
    })
    .await;
}

/// The scan's directory chain takes a name the tree holds as it holds it: below a directory whose
/// name was stored before a rule refused it, a separator in the name or a length over the bound,
/// the chain reaches the directory and adds below it.
#[tokio::test]
async fn a_scan_chain_below_a_held_unstorable_name_adds_below_it() {
    with_repository(|repository| async move {
        let long = "n".repeat(MAX_NODE_NAME_LEN + 1);
        for name in ["x\\y", long.as_str()] {
            let state = State::new();
            let t = chain(&state, &repository).await;
            let held = add(&state, &repository, t.b, directory("held"), "held").await;
            let block_index = NodeBlock::index(held);
            let block = state
                .block_with_nametable(repository.clone(), block_index)
                .await
                .expect("the directory's block");
            {
                let mut writer = block.write_for_tests();
                let node = *writer.node(Node::index(held));
                let (offset, length) = writer
                    .node_name_store_unchecked(name, node.name_offset, node.name_length)
                    .expect("the unchecked store takes the name");
                let node = writer.node(Node::index(held));
                node.name_hash = hash_string(name);
                node.name_offset = offset;
                node.name_length = length;
                writer.mark_dirty();
            }
            state.block_modified_for_tests(block, block_index);

            let new = lore_revision::state::os_diff::ensure_scan_dir_chain(
                repository.clone(),
                state.clone(),
                ROOT_NODE,
                format!("a/b/{name}/new"),
            )
            .await
            .expect("the chain reaches the held directory");
            let added = record(&state, &repository, new).await;
            assert_eq!(added.parent, held, "the chain adds below {name:?}");
            assert!(added.is_dirty_add());
        }
    })
    .await;
}

/// Both directory chains, the scan's and `lore dirty`'s, read a path as its names alone: a leading,
/// doubled or trailing separator adds no directory of an empty name, so a path spelled so reaches
/// the node its names do.
#[tokio::test]
async fn a_chain_skips_empty_names_in_its_path() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;

        let scanned = lore_revision::state::os_diff::ensure_scan_dir_chain(
            repository.clone(),
            state.clone(),
            ROOT_NODE,
            "/a//b/n/",
        )
        .await
        .expect("the scan's chain");
        let dirtied = lore_revision::file::dirty::ensure_dirty_parent_dirs(
            &repository,
            &state,
            ROOT_NODE,
            "//a/b//m/",
        )
        .await
        .expect("lore dirty's chain");
        let n = state
            .find_subnode(repository.clone(), t.b, hash_string("n"))
            .await
            .expect("the scan adds 'a/b/n'");
        let m = state
            .find_subnode(repository.clone(), t.b, hash_string("m"))
            .await
            .expect("lore dirty adds 'a/b/m'");
        assert_eq!(
            (scanned, dirtied),
            (n, lore_revision::file::dirty::ParentChain::Reached(m)),
            "each chain answers its last name's node"
        );
        for dir in [ROOT_NODE, t.a, t.b, n, m] {
            assert!(
                state
                    .find_subnode(repository.clone(), dir, hash_string(""))
                    .await
                    .is_err(),
                "no directory of an empty name below {dir}"
            );
        }
    })
    .await;
}

/// A State whose directory `fill`, in block 0, holds a file `first` in block 0 and the files
/// added after it in block 1, so `fill`'s chain starts in block 1; `dirty` marked, then saved
/// and loaded again, block 1 not in memory.
async fn spilled(dirty_late: bool) -> (Arc<State>, Arc<RepositoryContext>, NodeID, NodeID) {
    let repository = null_repository().await;
    let first_state = State::new();
    let fill = add(
        &first_state,
        &repository,
        ROOT_NODE,
        directory("fill"),
        "fill",
    )
    .await;
    let first = add(&first_state, &repository, fill, file("first"), "first").await;
    let mut late = first;
    for index in 0..BLOCK_NODE_COUNT {
        let name = format!("filler{index}");
        late = add(&first_state, &repository, fill, file(&name), &name).await;
    }
    assert_eq!(NodeBlock::index(first), 0);
    assert_eq!(NodeBlock::index(late), 1);
    mark(
        &first_state,
        &repository,
        first,
        Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
    )
    .await;
    if dirty_late {
        mark(
            &first_state,
            &repository,
            late,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
    }
    let state = reload(&first_state, &repository).await;
    (state, repository, fill, first)
}

/// A clear's walks load a block of the chain not in memory and go on where they stopped: the
/// directory is cleared where every child is clean, and kept where one in the loaded block is
/// dirty.
#[tokio::test]
async fn an_unserved_clear_loads_a_block_of_the_chain_and_resumes() {
    use lore_base::runtime::LORE_CONTEXT;

    use crate::fs::filesystem_provider::setup_test_execution;

    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for dirty_late in [false, true] {
                let (state, repository, fill, first) = spilled(dirty_late).await;
                assert!(state.resident_block(1).is_none(), "block 1 starts unloaded");

                // Bounded, so a walk that loses the block it loaded fails here rather than spinning.
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    clear(&state, &repository, first, Clear::Dirty),
                )
                .await
                .expect("the clear finishes");
                assert!(!record(&state, &repository, first).await.is_dirty());
                assert_eq!(
                    record(&state, &repository, fill).await.is_dirty(),
                    dirty_late,
                    "a dirty file in block 1: {dirty_late}"
                );
            }
        })
        .await;
}

/// A clear walking a chain that loops, within a block or back into one, refuses: the directory
/// stays dirty and the clear answers success.
#[tokio::test]
async fn an_unserved_clear_refuses_a_chain_that_loops() {
    with_state(|repository, state| async move {
        // Within a block: `g`, the head, and `f` point at each other.
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        edit(&state, &repository, t.f, |node| node.sibling = t.g).await;
        clear(&state, &repository, t.f, Clear::Dirty).await;
        assert!(record(&state, &repository, t.c).await.is_dirty());

        // Back into a block: the last file of `fill`'s chain, in block 0, points at its head, in
        // block 1.
        let (state, repository, fill, first) = spilled(false).await;
        let head = record(&state, &repository, fill)
            .await
            .child()
            .expect("a head");
        assert_eq!(NodeBlock::index(head), 1);
        edit(&state, &repository, first, |node| node.sibling = head).await;
        clear(&state, &repository, first, Clear::Dirty).await;
        assert!(record(&state, &repository, fill).await.is_dirty());
    })
    .await;
}

/// The dirty half finds a Discarded node before testing whether it already holds the mark: a
/// detached directory still dirty answers not found.
#[tokio::test]
async fn an_unserved_mark_finds_a_detached_dirty_directory() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let k = add(&state, &repository, t.b, file("k"), "k").await;
        mark(
            &state,
            &repository,
            k,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;
        state
            .node_detach(repository.clone(), t.b, Detach::Always)
            .await
            .expect("the detach succeeds");
        assert!(record(&state, &repository, t.b).await.is_dirty());

        let error = state
            .node_mark(
                &repository,
                t.f,
                Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
            )
            .await
            .expect_err("a mark through a detached directory fails");
        assert!(error.is_node_not_found(), "{error}");
    })
    .await;
}

/// A change in place refuses a node no longer under the parent named, and a detached one, and
/// records nothing.
#[tokio::test]
async fn a_change_in_place_refuses_a_moved_or_detached_node() {
    with_state(|repository, first| async move {
        let t = chain(&first, &repository).await;
        first
            .node_detach(repository.clone(), t.g, Detach::Always)
            .await
            .expect("the detach succeeds");

        let state = reload(&first, &repository).await;
        let block = state
            .block(repository.clone(), NodeBlock::index(t.f))
            .await
            .expect("the file's block");
        for (node, place) in [
            (t.f, (t.b, hash_string("f"))),
            (t.g, (t.c, hash_string("g"))),
        ] {
            assert!(!state.node_change_in_place(&block, node, place, |_| {
                unreachable!("a node not at its place is not handed")
            }));
        }
        assert!(!state.block_recorded_changed(NodeBlock::index(t.f)));
    })
    .await;
}

/// A stage records the staged action with the stage's own flags and the matching dirty action,
/// except for a merge, which takes no dirty action.
#[test]
fn a_stage_records_a_dirty_action_beside_its_staged_one_unless_merging() {
    use lore_revision::stage::staged_change;

    let plain = staged_change(
        NodeFlags::StagedModify,
        NodeFlags::DirtyModify,
        NodeFlags::NoFlags,
    );
    assert_eq!(plain.staged, Some((NodeFlags::StagedModify, true)));
    assert_eq!(plain.dirty, Some((NodeFlags::DirtyModify, NO_EXT, true)));

    let merge = staged_change(
        NodeFlags::StagedModify,
        NodeFlags::DirtyModify,
        NodeFlags::StagedMerge,
    );
    assert_eq!(
        merge.staged,
        Some((NodeFlags::StagedModify | NodeFlags::StagedMerge, true))
    );
    assert_eq!(merge.dirty, None);
}

/// A staged-moved directory a scan cleaned, then dirtied by a new unstaged file below it, is
/// walked for the commit's dirty-only paths: staged, it records no path of its own, so the file
/// below it is collected and survives the commit.
#[tokio::test]
async fn a_clean_staged_move_dirtied_again_is_walked_for_dirty_only_paths() {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::util::path::RelativePathBuf;

    use crate::fs::filesystem_provider::setup_test_execution;

    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();
            let t = chain(&state, &repository).await;
            mark(
                &state,
                &repository,
                t.c,
                Mark::staged_and_dirty(NodeFlags::StagedMove, NodeFlags::DirtyMove),
            )
            .await;
            // A scan finds `f` unchanged: `c` clean, its staged move kept.
            mark(
                &state,
                &repository,
                t.f,
                Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
            )
            .await;
            clear(&state, &repository, t.f, Clear::Dirty).await;
            assert!(!record(&state, &repository, t.c).await.is_dirty());
            // The scan adds `h`, then marks its directory.
            let added = Node {
                flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
                name_hash: hash_string("h"),
                ..Default::default()
            };
            add(&state, &repository, t.c, added, "h").await;
            mark(
                &state,
                &repository,
                t.c,
                Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
            )
            .await;
            assert!(record(&state, &repository, t.c).await.is_dirty_move());

            let mut paths = Vec::new();
            lore_revision::state::collect_dirty_only_paths(
                state.clone(),
                repository.clone(),
                ROOT_NODE,
                RelativePathBuf::new(),
                &mut paths,
            )
            .await
            .expect("the walk runs");
            let paths: Vec<&str> = paths.iter().map(|path| path.as_str()).collect();
            assert_eq!(paths, ["a/b/c/h"]);
        })
        .await;
}

/// A staged-deleted directory holding a new unstaged file is walked for the commit's dirty-only
/// paths: staged, it records no path of its own, so the file below it is collected.
#[tokio::test]
async fn a_staged_delete_holding_a_new_file_is_walked_for_dirty_only_paths() {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::util::path::RelativePathBuf;

    use crate::fs::filesystem_provider::setup_test_execution;

    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();
            let t = chain(&state, &repository).await;
            for node in [t.c, t.f, t.g] {
                mark(
                    &state,
                    &repository,
                    node,
                    Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
                )
                .await;
            }
            // The directory made again on disk with a new file, which a scan adds.
            let added = Node {
                flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
                name_hash: hash_string("h"),
                ..Default::default()
            };
            add(&state, &repository, t.c, added, "h").await;
            mark(
                &state,
                &repository,
                t.c,
                Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
            )
            .await;
            assert!(record(&state, &repository, t.c).await.is_dirty_delete());

            let mut paths = Vec::new();
            lore_revision::state::collect_dirty_only_paths(
                state.clone(),
                repository.clone(),
                ROOT_NODE,
                RelativePathBuf::new(),
                &mut paths,
            )
            .await
            .expect("the walk runs");
            let paths: Vec<&str> = paths.iter().map(|path| path.as_str()).collect();
            assert_eq!(paths, ["a/b/c/h"]);
        })
        .await;
}

/// A mark that only keeps a directory dirty above a change below it keeps the directory's own
/// action and extension flags: the source directory of a move, and the directory above a scan's
/// chain of adds. The directory is a staged add, as a move's staged half drops the action of a
/// directory it newly stages, which the two halves share.
#[tokio::test]
async fn a_directory_marked_for_a_change_below_keeps_its_action_and_extension_flags() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.c,
            Mark {
                staged: Some((NodeFlags::StagedAdd, true)),
                dirty: Some((
                    NodeFlags::DirtyAdd,
                    Ext::Set(NodeExtFlags::LocalContent),
                    true,
                )),
            },
        )
        .await;
        let kept = |node: Node| {
            assert!(node.is_dirty_add(), "the action stands");
            assert_eq!(
                node.ext_flags(),
                NodeExtFlags::LocalContent,
                "the extension flags stand"
            );
        };

        state
            .node_move_staged(
                repository.clone(),
                t.f,
                t.b,
                ("f2", hash_string("f2")),
                &state.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("the move succeeds");
        kept(record(&state, &repository, t.c).await);

        lore_revision::state::os_diff::ensure_scan_dir_chain(
            repository.clone(),
            state.clone(),
            ROOT_NODE,
            "a/b/c/new",
        )
        .await
        .expect("the chain is made");
        kept(record(&state, &repository, t.c).await);
    })
    .await;
}

/// Marks and clears of files in one block, on threads, keep every dirty node's directory dirty
/// once they are done: the marks climb several levels of the block under one guard, which no
/// scheduled scenario runs, as the scheduler takes one guard per level.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn marks_and_clears_on_threads_keep_dirty_closure() {
    const DIRECTORIES: usize = 4;
    const FILES: usize = 8;
    with_state(|repository, state| async move {
        // `a/b/c/d{n}/f{m}`, every node in block 0.
        let a = add(&state, &repository, ROOT_NODE, directory("a"), "a").await;
        let b = add(&state, &repository, a, directory("b"), "b").await;
        let c = add(&state, &repository, b, directory("c"), "c").await;
        let mut parents = vec![(a, ROOT_NODE), (b, a), (c, b)];
        let mut files = Vec::new();
        for d in 0..DIRECTORIES {
            let name = format!("d{d}");
            let dir = add(&state, &repository, c, directory(&name), &name).await;
            parents.push((dir, c));
            for f in 0..FILES {
                let name = format!("f{f}");
                let id = add(&state, &repository, dir, file(&name), &name).await;
                parents.push((id, dir));
                files.push(id);
            }
        }
        assert!(parents.iter().all(|(node, _)| NodeBlock::index(*node) == 0));
        let files = Arc::new(files);

        for round in 0..200u64 {
            let mut tasks = tokio::task::JoinSet::new();
            for task in 0..8u64 {
                let (state, repository, files) = (state.clone(), repository.clone(), files.clone());
                let _ = lore_base::lore_spawn!(tasks, async move {
                    let mut seed = round * 8 + task + 1;
                    for _ in 0..16 {
                        seed = seed
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        let file = files[(seed >> 33) as usize % files.len()];
                        if seed >> 63 == 0 {
                            mark(
                                &state,
                                &repository,
                                file,
                                Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
                            )
                            .await;
                        } else {
                            clear(&state, &repository, file, Clear::Dirty).await;
                        }
                    }
                });
            }
            tasks.join_all().await;
            for (node, parent) in &parents {
                if *parent == ROOT_NODE || !record(&state, &repository, *node).await.is_dirty() {
                    continue;
                }
                assert!(
                    record(&state, &repository, *parent).await.is_dirty(),
                    "round {round}: dirty {node} under clean {parent}"
                );
            }
        }
    })
    .await;
}

/// A rewrite's content: `size` and an address of hash byte `hash` and context byte `context`,
/// none for 0.
fn content(size: u64, hash: u64, context: u64) -> Content {
    let mut address = Address::zero_context_hash(Hash::from_u64(hash));
    if context != 0 {
        address.context = Hash::from_u64(context).to_context();
    }
    Content {
        mode: 0o644,
        size,
        address,
    }
}

async fn rewrite(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    content: &Content,
    rewrite: Rewrite,
) -> Result<bool, lore_revision::state::StateError> {
    state.node_rewrite(repository, node, content, rewrite).await
}

/// A plain rewrite gives a file the content, keeping its file id where the address carries
/// none and taking the address's where it does; its flags and its ancestors stay as they are.
#[tokio::test]
async fn an_unserved_rewrite_gives_a_file_its_content_and_keeps_its_file_id() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        edit(&state, &repository, t.f, |node| {
            node.address = content(1, 1, 7).address;
        })
        .await;

        let fresh = content(10, 2, 0);
        assert!(
            rewrite(&state, &repository, t.f, &fresh, Rewrite::Plain)
                .await
                .expect("rewrite")
        );
        let held = record(&state, &repository, t.f).await;
        assert_eq!(
            (held.mode, held.size, held.address.hash),
            (0o644, 10, fresh.address.hash)
        );
        assert_eq!(
            held.address.context,
            content(1, 1, 7).address.context,
            "the file id stands"
        );
        assert!(!held.is_dirty() && !record(&state, &repository, t.c).await.is_dirty());

        let moved = content(11, 3, 9);
        rewrite(&state, &repository, t.f, &moved, Rewrite::Plain)
            .await
            .expect("rewrite");
        assert_eq!(
            record(&state, &repository, t.f).await.address,
            moved.address
        );
    })
    .await;
}

/// A rewrite if clean leaves a dirty file as it is, answering false, and rewrites a clean one.
#[tokio::test]
async fn an_unserved_rewrite_if_clean_leaves_a_dirty_file() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.f,
            Mark::dirty(NodeFlags::DirtyModify, NO_EXT, false),
        )
        .await;

        let fresh = content(10, 2, 0);
        assert!(
            !rewrite(&state, &repository, t.f, &fresh, Rewrite::IfClean)
                .await
                .expect("rewrite")
        );
        assert_eq!(record(&state, &repository, t.f).await.size, 0);
        assert!(
            rewrite(&state, &repository, t.g, &fresh, Rewrite::IfClean)
                .await
                .expect("rewrite")
        );
        assert_eq!(record(&state, &repository, t.g).await.size, 10);

        // Any dirty action keeps the content, not a modify alone.
        for action in [NodeFlags::DirtyAdd, NodeFlags::DirtyMove] {
            mark(&state, &repository, t.g, Mark::dirty(action, NO_EXT, true)).await;
            assert!(
                !rewrite(
                    &state,
                    &repository,
                    t.g,
                    &content(20, 3, 0),
                    Rewrite::IfClean
                )
                .await
                .expect("rewrite"),
                "{action:?}"
            );
            assert_eq!(record(&state, &repository, t.g).await.size, 10);
        }
    })
    .await;
}

/// A dirty rewrite writes the content, the dirty action and the extension flags together,
/// replacing the action and flags the file held, and marks no ancestor.
#[tokio::test]
async fn an_unserved_dirty_rewrite_writes_the_action_and_extension_flags_with_the_content() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        edit(&state, &repository, t.f, |node| {
            node.flags |= NodeFlags::DirtyAdd.bits();
            node.ext_flags = NodeExtFlags::Symlink.bits();
        })
        .await;

        rewrite(
            &state,
            &repository,
            t.f,
            &content(5, 2, 0),
            Rewrite::Dirty(NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
        )
        .await
        .expect("rewrite");
        let held = record(&state, &repository, t.f).await;
        assert_eq!(held.size, 5);
        assert!(held.is_dirty_modify() && !held.is_dirty_add());
        assert_eq!(held.ext_flags(), NodeExtFlags::LocalContent);
        assert!(
            !record(&state, &repository, t.c).await.is_dirty(),
            "no ancestor marked"
        );

        // No action drops the dirty base bit too, and the extension flags; staged bits in the flags
        // are not the dirty rewrite's to write.
        rewrite(
            &state,
            &repository,
            t.f,
            &content(6, 2, 0),
            Rewrite::Dirty(NodeFlags::NoFlags, NodeExtFlags::NoFlags),
        )
        .await
        .expect("rewrite");
        let held = record(&state, &repository, t.f).await;
        assert!(!held.is_dirty());
        assert_eq!(held.ext_flags, 0);
        rewrite(
            &state,
            &repository,
            t.f,
            &content(7, 2, 0),
            Rewrite::Dirty(
                NodeFlags::DirtyModify | NodeFlags::StagedModify,
                NodeExtFlags::NoFlags,
            ),
        )
        .await
        .expect("rewrite");
        let held = record(&state, &repository, t.f).await;
        assert!(held.is_dirty_modify() && !held.is_staged());
    })
    .await;
}

/// A rewrite refuses an invalid id, a detached node, a directory and a file staged for deletion,
/// each as invalid arguments, writing nothing; a restore refuses a node not staged for deletion
/// and a detached one.
#[tokio::test]
async fn an_unserved_rewrite_refuses_what_holds_no_content_to_rewrite() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        mark(
            &state,
            &repository,
            t.g,
            Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
        )
        .await;
        let link = add(
            &state,
            &repository,
            t.c,
            Node {
                flags: NodeFlags::Link.bits(),
                name_hash: hash_string("l"),
                ..Default::default()
            },
            "l",
        )
        .await;
        let fresh = content(10, 2, 0);

        for (node, kind) in [
            (INVALID_NODE, Rewrite::Plain),
            (INVALID_NODE, Rewrite::Restore),
            (t.c, Rewrite::Plain),
            (link, Rewrite::Plain),
            (t.g, Rewrite::Plain),
            (t.g, Rewrite::IfClean),
            (
                t.g,
                Rewrite::Dirty(NodeFlags::DirtyModify, NodeExtFlags::NoFlags),
            ),
            (t.f, Rewrite::Restore),
        ] {
            let error = rewrite(&state, &repository, node, &fresh, kind)
                .await
                .expect_err("refused");
            assert!(error.is_invalid_arguments(), "{node} {kind:?}: {error}");
        }
        assert_eq!(record(&state, &repository, t.g).await.size, 0);
        assert_eq!(record(&state, &repository, t.f).await.size, 0);

        // A file deleted only locally, or staged for another action, is no file staged for deletion:
        // the two halves share the action bits, and only both together refuse.
        let a = add(
            &state,
            &repository,
            t.c,
            file("dirty-deleted"),
            "dirty-deleted",
        )
        .await;
        mark(
            &state,
            &repository,
            a,
            Mark::dirty(NodeFlags::DirtyDelete, NO_EXT, false),
        )
        .await;
        let b = add(
            &state,
            &repository,
            t.c,
            file("staged-modified"),
            "staged-modified",
        )
        .await;
        mark(
            &state,
            &repository,
            b,
            Mark::staged(NodeFlags::StagedModify, true),
        )
        .await;
        for node in [a, b] {
            assert!(
                rewrite(&state, &repository, node, &fresh, Rewrite::Plain)
                    .await
                    .expect("a live file is rewritten")
            );
        }

        state
            .node_detach(repository.clone(), t.h, Detach::Always)
            .await
            .expect("the detach");
        for kind in [Rewrite::Plain, Rewrite::Restore] {
            let error = rewrite(&state, &repository, t.h, &fresh, kind)
                .await
                .expect_err("refused");
            assert!(error.is_invalid_arguments(), "{kind:?}: {error}");
        }

        // A restore takes only a node staged for deletion and live: not one staged for deletion and
        // detached since, nor one staged for another action, nor one deleted only locally.
        state
            .node_detach(repository.clone(), t.g, Detach::Always)
            .await
            .expect("the detach");
        mark(
            &state,
            &repository,
            t.b,
            Mark::staged(NodeFlags::StagedModify, true),
        )
        .await;
        let local = add(&state, &repository, t.a, file("local"), "local").await;
        mark(
            &state,
            &repository,
            local,
            Mark::dirty(NodeFlags::DirtyDelete, NO_EXT, false),
        )
        .await;
        for node in [t.g, t.b, local] {
            let before = record(&state, &repository, node).await;
            let error = rewrite(&state, &repository, node, &fresh, Rewrite::Restore)
                .await
                .expect_err("refused");
            assert!(error.is_invalid_arguments(), "{node}: {error}");
            assert_eq!(
                record(&state, &repository, node).await.flags,
                before.flags,
                "{node}"
            );
        }
        assert!(record(&state, &repository, t.g).await.is_discarded());
    })
    .await;
}

/// A restore takes a node staged for deletion back with the content its kind carries: a file
/// all of it, its file id kept; a link its address and no size; a directory neither. Its staged
/// flags and its delete action go, a dirty node staying dirty; `node_restore` then marks it,
/// with its ancestors, staged and dirty modified.
#[tokio::test]
async fn an_unserved_restore_takes_each_kind_back_with_its_content() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let link = add(
            &state,
            &repository,
            t.c,
            Node {
                flags: NodeFlags::Link.bits(),
                name_hash: hash_string("l"),
                ..Default::default()
            },
            "l",
        )
        .await;
        edit(&state, &repository, t.f, |node| {
            node.address = content(1, 1, 7).address;
        })
        .await;
        // Sizes a link and a directory do not carry, which the restore drops, and addresses: the
        // directory's, which it does not carry either, and the link's, which the restore replaces.
        for node in [link, t.b] {
            edit(&state, &repository, node, |node| {
                node.size = 99;
                node.address = content(1, 9, 9).address;
            })
            .await;
        }
        for node in [t.f, link, t.b] {
            mark(
                &state,
                &repository,
                node,
                Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
            )
            .await;
        }
        let fresh = content(10, 2, 0);

        for node in [t.f, link, t.b] {
            assert!(
                rewrite(&state, &repository, node, &fresh, Rewrite::Restore)
                    .await
                    .expect("restore")
            );
            let held = record(&state, &repository, node).await;
            assert!(!held.is_staged(), "{node}: staged flags dropped");
            assert!(
                held.is_dirty() && !held.is_dirty_delete(),
                "{node}: the delete action dropped, the node dirty still"
            );
            assert_eq!(held.mode, 0o644);
        }
        let file = record(&state, &repository, t.f).await;
        assert_eq!((file.size, file.address.hash), (10, fresh.address.hash));
        assert_eq!(file.address.context, content(1, 1, 7).address.context);
        let held_link = record(&state, &repository, link).await;
        assert_eq!((held_link.size, held_link.address), (0, fresh.address));
        let dir = record(&state, &repository, t.b).await;
        assert_eq!((dir.size, dir.address), (0, Address::default()));

        // `node_restore` on a tree whose only staged node is the one it restores: its ancestors
        // are marked by the restore itself.
        let state = State::new();
        let t = chain(&state, &repository).await;
        edit(&state, &repository, t.g, |node| {
            node.flags |= NodeFlags::StagedDelete.bits();
        })
        .await;
        state
            .node_restore(&repository, t.g, &fresh)
            .await
            .expect("restore");
        for node in [t.g, t.c, t.b, t.a] {
            let held = record(&state, &repository, node).await;
            assert!(held.is_staged() && held.is_dirty(), "{node} marked");
        }
        let held = record(&state, &repository, t.g).await;
        assert!(held.is_staged_modify() && held.is_dirty_modify());
    })
    .await;
}

/// A rewrite of a node whose block is not in memory loads it, and records the block changed.
#[tokio::test]
async fn an_unserved_rewrite_loads_a_block_not_in_memory_and_records_it_changed() {
    use lore_base::runtime::LORE_CONTEXT;

    use crate::fs::filesystem_provider::setup_test_execution;

    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (state, repository, fill, _first) = spilled(false).await;
            // The chain's head, the last file added, lies in block 1.
            let late = record(&state, &repository, fill)
                .await
                .child()
                .expect("fill holds files");
            assert_eq!(NodeBlock::index(late), 1);
            assert!(state.resident_block(1).is_none(), "block 1 starts unloaded");
            assert!(!state.block_recorded_changed(1));

            // A refusal, and a rewrite if clean of a dirty file, write nothing.
            edit(&state, &repository, late, |node| {
                node.flags |= NodeFlags::DirtyModify.bits();
            })
            .await;
            let state = reload(&state, &repository).await;
            assert!(!state.is_dirty());
            rewrite(
                &state,
                &repository,
                late,
                &content(3, 2, 0),
                Rewrite::Restore,
            )
            .await
            .expect_err("not staged for deletion");
            assert!(
                !rewrite(
                    &state,
                    &repository,
                    late,
                    &content(3, 2, 0),
                    Rewrite::IfClean
                )
                .await
                .expect("rewrite")
            );
            assert!(!state.block_recorded_changed(1) && !state.is_dirty());

            rewrite(&state, &repository, late, &content(3, 2, 0), Rewrite::Plain)
                .await
                .expect("rewrite");
            assert_eq!(record(&state, &repository, late).await.size, 3);
            assert!(state.block_recorded_changed(1));
            assert!(state.is_dirty());
        })
        .await;
}

/// Rewrites of the same files on threads, plain and if clean, each leave one writer's whole
/// content: a file's mode, size and address all come from the rewrite that landed last.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rewrites_of_one_file_on_threads_leave_one_writers_whole_content() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let files = [t.f, t.g, t.h];
        let mut tasks = tokio::task::JoinSet::new();
        for task in 1..=8u64 {
            let (state, repository) = (state.clone(), repository.clone());
            let _ = lore_base::lore_spawn!(tasks, async move {
                let content = Content {
                    mode: u16::try_from(task).expect("a small task number"),
                    size: task,
                    address: Address::zero_context_hash(Hash::from_u64(task)),
                };
                for step in 0..200usize {
                    let kind = if step % 2 == 0 {
                        Rewrite::Plain
                    } else {
                        Rewrite::IfClean
                    };
                    state
                        .node_rewrite(&repository, files[step % files.len()], &content, kind)
                        .await
                        .expect("a rewrite of a live clean file");
                }
            });
        }
        tasks.join_all().await;
        for file in files {
            let held = record(&state, &repository, file).await;
            assert!(held.size >= 1, "{file} rewritten");
            assert_eq!(
                u64::from(held.mode),
                held.size,
                "{file}: mode and size of one writer"
            );
            assert_eq!(
                held.address.hash,
                Hash::from_u64(held.size),
                "{file}: address and size of one writer"
            );
        }
    })
    .await;
}

/// H2: a detach its condition refuses marks no parent. In a clean tree, an `IfEmpty` detach of a
/// directory holding children and an `At` detach of a file named in another directory or under
/// another name each answer false and leave every directory above clean.
#[tokio::test]
async fn an_unserved_detach_its_condition_refuses_leaves_the_directories_above_clean() {
    with_state(|repository, state| async move {
        let t = chain(&state, &repository).await;
        let full = state
            .node_detach(repository.clone(), t.c, Detach::IfEmpty)
            .await
            .expect("the detach runs");
        assert!(!full, "a directory holding children is not detached");
        let elsewhere = state
            .node_detach(repository.clone(), t.f, Detach::At(t.b, hash_string("f")))
            .await
            .expect("the detach runs");
        assert!(!elsewhere, "a file not at the place named is not detached");
        let renamed = state
            .node_detach(
                repository.clone(),
                t.f,
                Detach::At(t.c, hash_string("other")),
            )
            .await
            .expect("the detach runs");
        assert!(!renamed, "a file not under the name named is not detached");
        for (name, node) in [("a", t.a), ("b", t.b), ("c", t.c), ("f", t.f)] {
            let held = record(&state, &repository, node).await;
            assert!(!held.is_discarded(), "{name} stands");
            assert!(!held.is_dirty(), "{name} stays clean");
        }
    })
    .await;
}
