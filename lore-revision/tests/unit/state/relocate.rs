// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the unserved movers' primitives: `State::node_move`, moving a node to another
//! parent in one critical section with its own flags, `State::node_rename`, renaming it where it
//! stands, `State::node_set_link_target`, and the C API's `State::node_move_staged`.

use std::sync::Arc;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::LinkPin;
use lore_revision::state::Mark;
use lore_revision::state::NameChange;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use super::InMemoryMarker;
use super::add;
use super::directory;
use super::file;
use super::record;
use super::with_repository;
use super::with_state;

/// The children of `parent`, head first, read from the records.
async fn chain(state: &State, repository: &Arc<RepositoryContext>, parent: NodeID) -> Vec<NodeID> {
    let mut children = Vec::new();
    let mut next = record(state, repository, parent).await.child();
    while let Some(child) = next {
        children.push(child);
        next = record(state, repository, child).await.sibling();
    }
    children
}

/// The chain change count of directory `dir`.
async fn changes(state: &State, repository: &Arc<RepositoryContext>, dir: NodeID) -> u32 {
    state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the directory's block")
        .chain_changes(Node::index(dir))
}

async fn name(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) -> String {
    state
        .node_name_clone(repository.clone(), node)
        .await
        .expect("the node's name")
}

/// A move to another parent takes the node out of its chain and prepends it to the new one,
/// renamed, with its own flags, in one critical section: the old parent's count moves by the
/// reattach weight and the new parent's by one, and neither parent is marked.
#[tokio::test]
async fn a_move_relinks_renames_and_flags_the_node_and_marks_no_parent() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let a = add(&state, &repository, source, file("a"), "a").await;
        let b = add(&state, &repository, source, file("b"), "b").await;
        let c = add(&state, &repository, source, file("c"), "c").await;
        let held = add(&state, &repository, destination, file("held"), "held").await;
        let (source_before, destination_before) = (
            changes(&state, &repository, source).await,
            changes(&state, &repository, destination).await,
        );

        state
            .node_move(
                repository.clone(),
                b,
                destination,
                NameChange::Rename("moved"),
                Mark::staged(NodeFlags::StagedMove, true),
            )
            .await
            .expect("the move");

        assert_eq!(chain(&state, &repository, source).await, vec![c, a]);
        assert_eq!(chain(&state, &repository, destination).await, vec![b, held]);
        let moved = record(&state, &repository, b).await;
        assert_eq!(moved.parent, destination);
        assert_eq!(moved.name_hash, hash_string("moved"));
        assert_eq!(name(&state, &repository, b).await, "moved");
        assert!(moved.is_staged_move(), "the node takes its own flags");
        assert_eq!(
            changes(&state, &repository, source).await,
            source_before.wrapping_add(REATTACH_CHANGES)
        );
        assert_eq!(
            changes(&state, &repository, destination).await,
            destination_before.wrapping_add(1)
        );
        for parent in [source, destination] {
            let parent = record(&state, &repository, parent).await;
            assert!(
                !parent.is_staged() && !parent.is_dirty(),
                "a move marks no parent"
            );
        }
    })
    .await;
}

/// A move's own flags replace the node's staged action and keep its merge flags, as a forced mark
/// of no merge state of its own does: a conflict, and a resolved conflict's resolution and side.
#[tokio::test]
async fn a_moves_own_flags_keep_the_merge_flags() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let node = add(&state, &repository, source, file("f"), "f").await;
        state
            .node_mark(
                &repository,
                node,
                Mark::staged(NodeFlags::StagedMergeConflict, true),
            )
            .await
            .expect("the conflict mark");

        state
            .node_move(
                repository.clone(),
                node,
                destination,
                NameChange::Keep,
                Mark::staged(NodeFlags::StagedMove, true),
            )
            .await
            .expect("the move");

        let moved = record(&state, &repository, node).await;
        assert!(moved.is_staged_move());
        assert!(moved.is_staged_merge_conflict(), "the merge flags stand");

        let resolved = add(&state, &repository, source, file("r"), "r").await;
        state
            .node_mark(
                &repository,
                resolved,
                Mark::staged(
                    NodeFlags::StagedMergeResolved | NodeFlags::StagedMergeTheirs,
                    true,
                ),
            )
            .await
            .expect("the resolution mark");
        assert!(
            record(&state, &repository, resolved)
                .await
                .is_staged_merge_resolved()
        );
        state
            .node_move(
                repository.clone(),
                resolved,
                destination,
                NameChange::Keep,
                Mark::staged(NodeFlags::StagedMove, true),
            )
            .await
            .expect("the move");
        let moved = record(&state, &repository, resolved).await;
        assert!(moved.is_staged_move() && moved.is_staged_merge_conflict());
        assert!(
            moved.is_staged_merge_resolved() && moved.is_staged_merge_theirs(),
            "the resolution and its side stand"
        );
    })
    .await;
}

/// Under the same parent a rename keeps the node's place in the chain, counting the reattach
/// weight for a changed hash; `Keep` moves nothing and still takes the node's own flags.
#[tokio::test]
async fn a_move_under_the_same_parent_keeps_the_nodes_place() {
    with_state(|repository, state| async move {
        let dir = add(&state, &repository, ROOT_NODE, directory("dir"), "dir").await;
        let a = add(&state, &repository, dir, file("a"), "a").await;
        let b = add(&state, &repository, dir, file("b"), "b").await;
        let c = add(&state, &repository, dir, file("c"), "c").await;
        let before = changes(&state, &repository, dir).await;

        state
            .node_move(
                repository.clone(),
                b,
                dir,
                NameChange::Rename("renamed"),
                Mark::staged(NodeFlags::StagedMove, true),
            )
            .await
            .expect("the rename");
        assert_eq!(chain(&state, &repository, dir).await, vec![c, b, a]);
        assert_eq!(name(&state, &repository, b).await, "renamed");
        assert_eq!(
            changes(&state, &repository, dir).await,
            before.wrapping_add(REATTACH_CHANGES)
        );

        let before = changes(&state, &repository, dir).await;
        state
            .node_move(
                repository.clone(),
                a,
                dir,
                NameChange::Keep,
                Mark::staged(NodeFlags::StagedModify, true),
            )
            .await
            .expect("the move to where the node is");
        assert_eq!(chain(&state, &repository, dir).await, vec![c, b, a]);
        assert_eq!(changes(&state, &repository, dir).await, before);
        assert!(record(&state, &repository, a).await.is_staged_modify());
    })
    .await;
}

/// A rename that keeps the name's hash, a case change, counts nothing and keeps the place; one
/// that changes it counts the reattach weight. `Keep` changes nothing.
#[tokio::test]
async fn a_rename_counts_a_changed_hash_and_not_a_respelling() {
    with_state(|repository, state| async move {
        let dir = add(&state, &repository, ROOT_NODE, directory("dir"), "dir").await;
        let a = add(&state, &repository, dir, file("a.txt"), "a.txt").await;
        let b = add(&state, &repository, dir, file("b.txt"), "b.txt").await;
        let before = changes(&state, &repository, dir).await;

        state
            .node_rename(repository.clone(), a, NameChange::Rename("A.TXT"))
            .await
            .expect("the respelling");
        assert_eq!(name(&state, &repository, a).await, "A.TXT");
        assert_eq!(changes(&state, &repository, dir).await, before);
        assert_eq!(chain(&state, &repository, dir).await, vec![b, a]);

        state
            .node_rename(repository.clone(), a, NameChange::Rename("c.txt"))
            .await
            .expect("the rename");
        assert_eq!(
            record(&state, &repository, a).await.name_hash,
            hash_string("c.txt")
        );
        assert_eq!(
            changes(&state, &repository, dir).await,
            before.wrapping_add(REATTACH_CHANGES)
        );
        assert_eq!(chain(&state, &repository, dir).await, vec![b, a]);

        let flags = record(&state, &repository, a).await.flags;
        state
            .node_rename(repository.clone(), a, NameChange::Keep)
            .await
            .expect("nothing to rename");
        assert_eq!(record(&state, &repository, a).await.flags, flags);
        assert!(
            !record(&state, &repository, dir).await.is_dirty(),
            "a rename marks nothing"
        );
    })
    .await;
}

/// A staged move refuses a node staged for deletion, and a parent it leaves or joins that is
/// staged for deletion; nothing moves. A trusted move takes the caller's word.
#[tokio::test]
async fn a_staged_move_refuses_what_a_trusted_move_takes() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let node = add(&state, &repository, source, directory("d"), "d").await;
        let permit = state.moves_permit().await.expect("the moves permit");
        let staged_delete = Mark::staged(NodeFlags::StagedDelete, true);
        for target in [destination, node, source] {
            state
                .node_mark(&repository, target, staged_delete)
                .await
                .expect("the deletion mark");
            let moved = state
                .node_move_staged(
                    repository.clone(),
                    node,
                    destination,
                    ("d", hash_string("d")),
                    &permit,
                    false,
                )
                .await;
            assert!(
                moved.is_err_and(|error| error.is_invalid_arguments()),
                "staged for deletion: {target}"
            );
            assert_eq!(record(&state, &repository, node).await.parent, source);
            state
                .node_mark(
                    &repository,
                    target,
                    Mark::staged(NodeFlags::StagedModify, true),
                )
                .await
                .expect("the deletion taken back");
        }
        drop(permit);

        state
            .node_mark(&repository, node, staged_delete)
            .await
            .expect("the deletion mark");
        state
            .node_move(
                repository.clone(),
                node,
                destination,
                NameChange::Keep,
                Mark::staged(NodeFlags::StagedMove, true),
            )
            .await
            .expect("a trusted move");
        assert_eq!(record(&state, &repository, node).await.parent, destination);
    })
    .await;
}

/// H5: a move of a node its parent's chain does not reach is a broken hierarchy, on every
/// attempt: the node is linked nowhere else and nothing is written.
#[tokio::test]
async fn a_move_of_a_node_its_parents_chain_misses_writes_nothing() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let kept = add(&state, &repository, source, file("kept"), "kept").await;
        let lost = add(&state, &repository, source, file("lost"), "lost").await;
        let block = state
            .block(repository.clone(), NodeBlock::index(source))
            .await
            .expect("the source's block");
        // The source's chain skips `lost`, which still names it as its parent.
        state.node_edit_for_tests(&block, source, |record| record.child = kept);

        let moved = state
            .node_move(
                repository.clone(),
                lost,
                destination,
                NameChange::Keep,
                Mark::staged(NodeFlags::StagedMove, true),
            )
            .await;

        let error = moved.expect_err("the move fails");
        assert!(error.is_internal(), "a broken hierarchy: {error}");
        assert!(chain(&state, &repository, destination).await.is_empty());
        let lost = record(&state, &repository, lost).await;
        assert_eq!(lost.parent, source);
        assert!(!lost.is_staged(), "nothing is written");
    })
    .await;
}

/// A link's pin is set whole, or its revision alone, keeping the address's context and the node
/// it exposes; a node that is not a link is refused, and a detached one is not found.
#[tokio::test]
async fn a_link_target_sets_the_pin_whole_or_its_revision_alone() {
    with_state(|repository, state| async move {
        let context = lore_base::types::Context::from([5u8; 16]);
        let link = add(
            &state,
            &repository,
            ROOT_NODE,
            Node {
                flags: NodeFlags::Link.bits(),
                name_hash: hash_string("lnk"),
                address: Address {
                    hash: Hash::from([1u8; 32]),
                    context,
                },
                child: 7,
                ..Default::default()
            },
            "lnk",
        )
        .await;

        state
            .node_set_link_target(&repository, link, LinkPin::Revision(Hash::from([2u8; 32])))
            .await
            .expect("the revision");
        let pinned = record(&state, &repository, link).await;
        assert_eq!(pinned.address.hash, Hash::from([2u8; 32]));
        assert_eq!((pinned.address.context, pinned.child), (context, 7));

        let whole = Address {
            hash: Hash::from([3u8; 32]),
            ..Address::default()
        };
        state
            .node_set_link_target(
                &repository,
                link,
                LinkPin::Whole {
                    address: whole,
                    child: 9,
                },
            )
            .await
            .expect("the whole pin");
        let pinned = record(&state, &repository, link).await;
        assert_eq!((pinned.address, pinned.child), (whole, 9));

        let plain = add(&state, &repository, ROOT_NODE, file("plain"), "plain").await;
        assert!(
            state
                .node_set_link_target(&repository, plain, LinkPin::Revision(Hash::from([4u8; 32])))
                .await
                .is_err_and(|error| error.is_invalid_arguments())
        );
        state
            .node_detach(
                repository.clone(),
                link,
                lore_revision::state::Detach::Always,
            )
            .await
            .expect("the detach");
        assert!(
            state
                .node_set_link_target(&repository, link, LinkPin::Revision(Hash::from([4u8; 32])))
                .await
                .is_err_and(|error| error.is_node_not_found())
        );
    })
    .await;
}

/// A repin between two serializes is written by the second: the first's release leaves the link's
/// block clean, so the repin registers it again.
#[tokio::test]
async fn a_link_repinned_between_two_serializes_is_written_by_the_second() {
    with_repository(|repository| async move {
        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let state = State::new();
        let link = add(
            &state,
            &repository,
            ROOT_NODE,
            Node {
                flags: NodeFlags::Link.bits(),
                name_hash: hash_string("lnk"),
                ..Default::default()
            },
            "lnk",
        )
        .await;
        let pin = async |revision: u8| {
            state
                .node_set_link_target(
                    &repository,
                    link,
                    LinkPin::Revision(Hash::from([revision; 32])),
                )
                .await
                .expect("the repin");
            state
                .serialize(repository.clone(), &token)
                .await
                .expect("the serialize")
        };

        let first = pin(1).await;
        let second = pin(2).await;

        assert_ne!(first, second, "the second serialize wrote the second pin");
    })
    .await;
}

/// A staged move refuses a destination detached, as a C API delete leaves one, and, under the
/// same parent, a rename of a node staged for deletion or under a parent staged for deletion; out
/// of a source detached it fails at its mark of the source, which finds it gone. Nothing moves and
/// no name changes.
#[tokio::test]
async fn a_staged_move_refuses_a_detached_parent_and_a_guarded_rename() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let node = add(&state, &repository, source, file("f"), "f").await;
        let permit = state.moves_permit().await.expect("the moves permit");
        let staged = async |parent, name: &str| {
            state
                .node_move_staged(
                    repository.clone(),
                    node,
                    parent,
                    (name, hash_string(name)),
                    &permit,
                    false,
                )
                .await
        };
        let detach = async |node| {
            state
                .node_detach(
                    repository.clone(),
                    node,
                    lore_revision::state::Detach::Always,
                )
                .await
                .expect("the detach")
        };
        let refused = |moved: Result<(), lore_revision::state::StateError>| {
            moved.is_err_and(|error| error.is_invalid_arguments())
        };

        for (target, guarded) in [
            (node, Mark::staged(NodeFlags::StagedDelete, true)),
            (source, Mark::staged(NodeFlags::StagedDelete, true)),
        ] {
            state
                .node_mark(&repository, target, guarded)
                .await
                .expect("the deletion mark");
            assert!(
                refused(staged(source, "g").await),
                "a same-parent rename, staged for deletion: {target}"
            );
            assert_eq!(name(&state, &repository, node).await, "f");
            state
                .node_mark(
                    &repository,
                    target,
                    Mark::staged(NodeFlags::StagedModify, true),
                )
                .await
                .expect("the deletion taken back");
        }

        assert!(detach(destination).await);
        assert!(refused(staged(destination, "f").await));
        assert_eq!(record(&state, &repository, node).await.parent, source);

        let elsewhere = add(&state, &repository, ROOT_NODE, directory("other"), "other").await;
        assert!(detach(source).await);
        assert!(
            staged(elsewhere, "f")
                .await
                .is_err_and(|error| error.is_node_not_found()),
            "the mark of the detached source finds it gone"
        );
        assert_eq!(record(&state, &repository, node).await.parent, source);
        assert!(chain(&state, &repository, elsewhere).await.is_empty());
    })
    .await;
}

/// The destination of a move takes the base bit of each half the move marked the node with and
/// nothing of the other: a staged move stages it, a dirty move dirties it, keeping its action
/// and extension flags; the root takes nothing.
#[tokio::test]
async fn a_moves_destination_takes_the_base_bits_of_the_moves_halves() {
    with_state(|repository, state| async move {
        let staged = add(&state, &repository, ROOT_NODE, directory("s"), "s").await;
        let dirtied = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let added = add(&state, &repository, ROOT_NODE, directory("a"), "a").await;
        state
            .node_mark(
                &repository,
                added,
                Mark::dirty(
                    NodeFlags::DirtyAdd,
                    lore_revision::state::Ext::Set(NodeExtFlags::LocalContent),
                    true,
                ),
            )
            .await
            .expect("the added directory's mark");

        assert_eq!(
            Mark::staged(NodeFlags::StagedMove, true).for_ancestors(),
            Mark::staged(NodeFlags::Staged, false)
        );
        assert_eq!(
            Mark::staged_and_dirty(NodeFlags::StagedMove, NodeFlags::DirtyMove).for_ancestors(),
            Mark {
                staged: Some((NodeFlags::Staged, false)),
                dirty: Some((NodeFlags::Dirty, lore_revision::state::Ext::Keep, false)),
            }
        );
        let dirty_move = Mark::dirty(
            NodeFlags::DirtyMove,
            lore_revision::state::Ext::Set(NodeExtFlags::NoFlags),
            true,
        );
        for (to, own) in [
            (staged, Mark::staged(NodeFlags::StagedMove, true)),
            (dirtied, dirty_move),
            (added, dirty_move),
            (ROOT_NODE, dirty_move),
        ] {
            state
                .node_mark_destination(&repository, to, own)
                .await
                .expect("the destination's mark");
        }

        let staged = record(&state, &repository, staged).await;
        assert!(
            staged.is_staged() && !staged.is_staged_move(),
            "staged, no action"
        );
        assert!(!staged.is_dirty(), "a staged move dirties nothing");
        let dirtied = record(&state, &repository, dirtied).await;
        assert!(
            dirtied.is_dirty() && !dirtied.is_dirty_move(),
            "dirty, no action"
        );
        assert!(!dirtied.is_staged(), "a dirty move stages nothing");
        let added = record(&state, &repository, added).await;
        assert!(added.is_dirty_add(), "an added destination stays added");
        assert_eq!(added.ext_flags(), NodeExtFlags::LocalContent);
        let root = record(&state, &repository, ROOT_NODE).await;
        assert!(!root.is_dirty() && !root.is_staged());
    })
    .await;
}

/// A move or a rename of a node detached meanwhile is not found, and a name the name table would
/// refuse is refused before anything is read; nothing changes.
#[tokio::test]
async fn a_move_or_rename_refuses_a_detached_node_and_an_unstorable_name() {
    with_state(|repository, state| async move {
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let node = add(&state, &repository, ROOT_NODE, file("f"), "f").await;
        let own = Mark::staged(NodeFlags::StagedMove, true);

        let moved = state
            .node_move(
                repository.clone(),
                node,
                destination,
                NameChange::Rename("a/b"),
                own,
            )
            .await;
        assert!(moved.is_err_and(|error| error.is_invalid_arguments()));
        let renamed = state
            .node_rename(repository.clone(), node, NameChange::Rename(".."))
            .await;
        assert!(renamed.is_err_and(|error| error.is_invalid_arguments()));
        assert_eq!(name(&state, &repository, node).await, "f");
        assert_eq!(record(&state, &repository, node).await.parent, ROOT_NODE);

        assert!(
            state
                .node_detach(
                    repository.clone(),
                    node,
                    lore_revision::state::Detach::Always
                )
                .await
                .expect("the detach")
        );
        let moved = state
            .node_move(repository.clone(), node, destination, NameChange::Keep, own)
            .await;
        assert!(moved.is_err_and(|error| error.is_node_not_found()));
        let renamed = state
            .node_rename(repository.clone(), node, NameChange::Rename("g"))
            .await;
        assert!(renamed.is_err_and(|error| error.is_node_not_found()));
        assert!(chain(&state, &repository, destination).await.is_empty());
    })
    .await;
}

/// Adds files named `{prefix}{n}` to `dir` until the block the last one landed in is full, so
/// the next node added lands in the next block.
async fn fill_block(state: &State, repository: &Arc<RepositoryContext>, dir: NodeID, prefix: &str) {
    for index in 0.. {
        let name = format!("{prefix}{index}");
        let id = add(state, repository, dir, file(&name), &name).await;
        if Node::index(id) == BLOCK_NODE_COUNT - 1 {
            break;
        }
    }
}

/// A move whose old parent, predecessor, node and new parent lie in four blocks, and a rename of
/// a node in another block than its parent, change every block they must: the node leaves the
/// old chain behind its predecessor, heads the new one, and takes its name.
#[tokio::test]
async fn a_move_and_a_rename_across_blocks_change_each_block_they_touch() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let filler = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        fill_block(&state, &repository, filler, "a").await;
        let node = add(&state, &repository, source, file("x"), "x").await;
        fill_block(&state, &repository, filler, "b").await;
        // Added after `x`, so it precedes `x` in the source's chain.
        let before = add(&state, &repository, source, file("y"), "y").await;
        fill_block(&state, &repository, filler, "c").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        assert_eq!(
            [source, node, before, destination].map(NodeBlock::index),
            [0, 1, 2, 3],
            "each in a block of its own"
        );
        let source_changes = changes(&state, &repository, source).await;
        let destination_changes = changes(&state, &repository, destination).await;

        state
            .node_move(
                repository.clone(),
                node,
                destination,
                NameChange::Rename("z"),
                Mark::staged(NodeFlags::StagedMove, true),
            )
            .await
            .expect("the move");

        assert_eq!(chain(&state, &repository, source).await, vec![before]);
        assert_eq!(chain(&state, &repository, destination).await, vec![node]);
        let moved = record(&state, &repository, node).await;
        assert_eq!(moved.parent, destination);
        assert!(moved.is_staged_move());
        assert_eq!(name(&state, &repository, node).await, "z");
        assert_eq!(
            changes(&state, &repository, source).await,
            source_changes.wrapping_add(REATTACH_CHANGES)
        );
        assert_eq!(
            changes(&state, &repository, destination).await,
            destination_changes.wrapping_add(1)
        );

        let destination_changes = changes(&state, &repository, destination).await;
        state
            .node_rename(repository.clone(), node, NameChange::Rename("w"))
            .await
            .expect("the rename");
        assert_eq!(name(&state, &repository, node).await, "w");
        assert_eq!(chain(&state, &repository, destination).await, vec![node]);
        assert_eq!(
            changes(&state, &repository, destination).await,
            destination_changes.wrapping_add(REATTACH_CHANGES)
        );
    })
    .await;
}

/// A rename under the root of a node outside block 0 leaves block 0 unchanged: neither the
/// rename nor the climb after it, which skips the root, records it for the next save.
#[tokio::test]
async fn a_rename_under_the_root_records_no_change_to_block_zero() {
    with_repository(|repository| async move {
        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let state = State::new();
        let filler = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        fill_block(&state, &repository, filler, "a").await;
        let node = add(&state, &repository, ROOT_NODE, file("x"), "x").await;
        assert_eq!(NodeBlock::index(node), 1);
        state
            .serialize(repository.clone(), &token)
            .await
            .expect("the serialize");
        assert!(!state.block_recorded_changed(0));

        let own = Mark::staged(NodeFlags::StagedMove, true);
        state
            .node_move(
                repository.clone(),
                node,
                ROOT_NODE,
                NameChange::Rename("y"),
                own,
            )
            .await
            .expect("the rename");
        state
            .node_mark_destination(&repository, ROOT_NODE, own)
            .await
            .expect("the destination's mark");

        assert_eq!(name(&state, &repository, node).await, "y");
        assert!(state.block_recorded_changed(1), "the node's block is saved");
        assert!(!state.block_recorded_changed(0), "block 0 is not");
    })
    .await;
}

/// A C API move of a child of the root, out of the middle of the root's chain and into a
/// directory already marked, leaves block 0 unchanged: the section writes the node's
/// predecessor, not the root, and the move marks no root, as no mark changes its flags.
#[tokio::test]
async fn a_move_out_of_the_root_records_no_change_to_block_zero() {
    with_repository(|repository| async move {
        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let state = State::new();
        let filler = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        fill_block(&state, &repository, filler, "a").await;
        let node = add(&state, &repository, ROOT_NODE, file("x"), "x").await;
        let after = add(&state, &repository, ROOT_NODE, file("y"), "y").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        assert_eq!(
            [node, after, destination].map(NodeBlock::index),
            [1, 1, 1],
            "past block 0"
        );
        state
            .node_mark(
                &repository,
                destination,
                Mark {
                    staged: Some((NodeFlags::Staged, false)),
                    dirty: Some((NodeFlags::Dirty, lore_revision::state::Ext::Keep, false)),
                },
            )
            .await
            .expect("the destination's mark");
        state
            .serialize(repository.clone(), &token)
            .await
            .expect("the serialize");
        assert!(!state.block_recorded_changed(0));

        let permit = state.moves_permit().await.expect("the moves permit");
        state
            .node_move_staged(
                repository.clone(),
                node,
                destination,
                ("x", hash_string("x")),
                &permit,
                false,
            )
            .await
            .expect("the move");

        assert_eq!(chain(&state, &repository, destination).await, vec![node]);
        assert!(
            state.block_recorded_changed(1),
            "the moved node's block is saved"
        );
        assert!(!state.block_recorded_changed(0), "block 0 is not");
    })
    .await;
}

/// A C API modify and a delete of a file under the root, past block 0, record block 0 changed: the
/// climb from the root marks nothing and dirties block 0 alone, before the modify's rewrite and
/// after the delete's mark.
#[tokio::test]
async fn a_modify_and_a_delete_under_the_root_record_block_zero_changed() {
    with_repository(|repository| async move {
        let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
        let state = State::new();
        let filler = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        fill_block(&state, &repository, filler, "a").await;
        let x = add(&state, &repository, ROOT_NODE, file("x"), "x").await;
        let y = add(&state, &repository, ROOT_NODE, file("y"), "y").await;
        assert_eq!([x, y].map(NodeBlock::index), [1, 1], "past block 0");
        let saved = async || {
            state
                .serialize(repository.clone(), &token)
                .await
                .expect("the serialize");
            assert!(!state.block_recorded_changed(0));
        };

        saved().await;
        state
            .node_modify_staged(&repository, x, &lore_revision::state::Content::default())
            .await
            .expect("the modify");
        assert!(state.block_recorded_changed(0), "the modify's climb");

        saved().await;
        let level = state
            .node_delete_level(&repository, y, ROOT_NODE)
            .await
            .expect("the delete");
        assert_eq!(level, lore_revision::state::LevelMark::Marked(None));
        assert!(state.block_recorded_changed(0), "the delete's climb");
    })
    .await;
}

/// A C API move of a directory marks each node below it moved, an unforced mark of no merge state
/// of its own, which keeps a resolved conflict below resolved with its side.
#[tokio::test]
async fn a_moved_directorys_resolved_child_stays_resolved() {
    with_state(|repository, state| async move {
        let directory_id = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let child = add(&state, &repository, directory_id, file("r"), "r").await;
        state
            .node_mark(
                &repository,
                child,
                Mark::staged(
                    NodeFlags::StagedMergeResolved | NodeFlags::StagedMergeTheirs,
                    true,
                ),
            )
            .await
            .expect("the resolution");

        let permit = state.moves_permit().await.expect("the moves permit");
        state
            .node_move_staged(
                repository.clone(),
                directory_id,
                destination,
                ("d", hash_string("d")),
                &permit,
                false,
            )
            .await
            .expect("the move");

        let held = record(&state, &repository, child).await;
        assert!(held.is_staged_move(), "flags {:#x}", held.flags);
        assert!(
            held.is_staged_merge_resolved() && held.is_staged_merge_theirs(),
            "the resolution and its side stand, flags {:#x}",
            held.flags
        );
    })
    .await;
}

/// Whether `dir` carries neither a staged nor a dirty mark.
async fn unmarked(state: &State, repository: &Arc<RepositoryContext>, dir: NodeID) -> bool {
    let held = record(state, repository, dir).await;
    !held.is_staged() && !held.is_dirty()
}

/// A staged move marks the directory the node leaves and the one it joins, each with its
/// ancestors, staged and dirty, and the node staged moved.
#[tokio::test]
async fn a_staged_move_marks_both_directories_and_their_ancestors() {
    with_state(|repository, state| async move {
        let top = add(&state, &repository, ROOT_NODE, directory("top"), "top").await;
        let source = add(&state, &repository, top, directory("src"), "src").await;
        let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
        let node = add(&state, &repository, source, file("x"), "x").await;

        let permit = state.moves_permit().await.expect("the moves permit");
        state
            .node_move_staged(
                repository.clone(),
                node,
                destination,
                ("x", hash_string("x")),
                &permit,
                false,
            )
            .await
            .expect("the move");

        for dir in [top, source, destination] {
            let held = record(&state, &repository, dir).await;
            assert!(
                held.is_staged() && held.is_dirty(),
                "directory {dir} is marked, flags {:#x}",
                held.flags
            );
        }
        assert!(record(&state, &repository, node).await.is_staged_move());
        assert!(
            !state.has_unfinished_change(),
            "a finished move notes nothing"
        );
    })
    .await;
}

/// Grows the name table of `node`'s block until it holds `room` bytes fewer than
/// [`NODE_NAME_MAX_SIZE`].
async fn fill_name_table(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    room: usize,
) {
    let block = state
        .block_with_nametable(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the node's block");
    let mut writer = block.write_for_tests();
    let filler = "f".repeat(1000);
    let (offset, stored) = writer.node_name_store("f", 0, 0).expect("a name stores");
    let mut length = (offset + stored) as usize;
    while length + filler.len() + room <= NODE_NAME_MAX_SIZE {
        let (offset, stored) = writer
            .node_name_store(&filler, 0, 0)
            .expect("a name stores");
        length = (offset + stored) as usize;
    }
    let last = NODE_NAME_MAX_SIZE - room - length;
    if last > 0 {
        writer
            .node_name_store(&"f".repeat(last), 0, 0)
            .expect("the last name stores");
    }
}

/// Stores `node`'s name again as the last of its block's name table, and points the node at it.
async fn name_last(state: &State, repository: &Arc<RepositoryContext>, node: NodeID, name: &str) {
    let block = state
        .block_with_nametable(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the node's block");
    let mut writer = block.write_for_tests();
    let (offset, length) = writer.node_name_store(name, 0, 0).expect("the name stores");
    let record = writer.node(Node::index(node));
    (record.name_offset, record.name_length) = (offset, length);
}

/// A staged move with no room for its name in the node's name table, one byte short, is refused
/// before any mark, so neither directory is marked and no unfinished change is noted: a name one
/// byte longer than the one the node holds, in a table with one byte of room; a name one byte
/// longer than the table's room; and a name as long as the one the node holds where that one is
/// the last of a full table, whose slot the store does not reuse.
#[tokio::test]
async fn a_staged_move_with_no_room_for_its_name_marks_nothing() {
    with_repository(|repository| async move {
        for (new_name, room, last) in [
            ("yy".to_string(), 1, false),
            ("y".repeat(1000), 999, false),
            ("y".to_string(), 1, true),
        ] {
            let state = State::new();
            let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
            let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
            let node = add(&state, &repository, source, file("x"), "x").await;
            fill_name_table(&state, &repository, node, room).await;
            if last {
                name_last(&state, &repository, node, "x").await;
            }

            let permit = state.moves_permit().await.expect("the moves permit");
            let refused = state
                .node_move_staged(
                    repository.clone(),
                    node,
                    destination,
                    (&new_name, hash_string(&new_name)),
                    &permit,
                    false,
                )
                .await;

            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "a {}-byte name is refused: {refused:?}",
                new_name.len()
            );
            for dir in [source, destination] {
                assert!(
                    unmarked(&state, &repository, dir).await,
                    "directory {dir} is unmarked"
                );
            }
            assert!(
                !state.has_unfinished_change(),
                "a refused move notes nothing"
            );
            assert_eq!(chain(&state, &repository, source).await, vec![node]);
        }
    })
    .await;
}

/// A staged move goes through wherever the store takes its name: a name filling the table's room
/// exactly, a rename into the slot of the name the node holds and a move keeping its name, each
/// in a table with no room left, the last a name that is the table's last. The room check refuses
/// no name the store takes, and a name that does not change stores nothing.
#[tokio::test]
async fn a_staged_move_takes_the_last_room_of_its_name_table() {
    with_repository(|repository| async move {
        for (new_name, room, last) in [
            ("y".repeat(1000), 1000, false),
            ("y".to_string(), 0, false),
            ("x".to_string(), 1, true),
        ] {
            let state = State::new();
            let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
            let destination = add(&state, &repository, ROOT_NODE, directory("dst"), "dst").await;
            let node = add(&state, &repository, source, file("x"), "x").await;
            fill_name_table(&state, &repository, node, room).await;
            if last {
                name_last(&state, &repository, node, "x").await;
            }

            let permit = state.moves_permit().await.expect("the moves permit");
            state
                .node_move_staged(
                    repository.clone(),
                    node,
                    destination,
                    (&new_name, hash_string(&new_name)),
                    &permit,
                    false,
                )
                .await
                .unwrap_or_else(|error| panic!("'{}' moves: {error}", &new_name[..1]));
            assert_eq!(chain(&state, &repository, destination).await, vec![node]);
            assert_eq!(name(&state, &repository, node).await, new_name);
        }
    })
    .await;
}

/// A staged move out of a directory into the root marks the directory it leaves and changes no
/// flag of the root; a node staged for addition keeps its addition, and its move marks the
/// directory it leaves as well.
#[tokio::test]
async fn a_staged_move_into_the_root_marks_the_directory_it_leaves() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let tracked = add(&state, &repository, source, file("x"), "x").await;
        let added = add(&state, &repository, source, file("y"), "y").await;
        state
            .node_mark(&repository, added, Mark::staged(NodeFlags::StagedAdd, true))
            .await
            .expect("the add's mark");
        let unmark = async || {
            state
                .block(repository.clone(), NodeBlock::index(source))
                .await
                .expect("the source's block")
                .write_for_tests()
                .node(Node::index(source))
                .clear_all_change_flags();
        };
        let root_flags = record(&state, &repository, ROOT_NODE).await.flags;

        let permit = state.moves_permit().await.expect("the moves permit");
        for node in [tracked, added] {
            unmark().await;
            let node_name = name(&state, &repository, node).await;
            state
                .node_move_staged(
                    repository.clone(),
                    node,
                    ROOT_NODE,
                    (&node_name, hash_string(&node_name)),
                    &permit,
                    false,
                )
                .await
                .expect("the move");
            let held = record(&state, &repository, source).await;
            assert!(
                held.is_staged() && held.is_dirty(),
                "the directory {node_name} left is marked, flags {:#x}",
                held.flags
            );
        }

        assert!(record(&state, &repository, tracked).await.is_staged_move());
        assert!(
            record(&state, &repository, added).await.is_staged_add(),
            "a node staged for addition stays one"
        );
        assert_eq!(
            record(&state, &repository, ROOT_NODE).await.flags,
            root_flags,
            "no mark changes the root's flags"
        );
        assert!(!state.has_unfinished_change());
    })
    .await;
}

/// A staged move of a directory into its own subtree is refused before any mark.
#[tokio::test]
async fn a_staged_move_into_its_own_subtree_marks_nothing() {
    with_state(|repository, state| async move {
        let source = add(&state, &repository, ROOT_NODE, directory("src"), "src").await;
        let moved = add(&state, &repository, source, directory("d"), "d").await;
        let inner = add(&state, &repository, moved, directory("e"), "e").await;

        let permit = state.moves_permit().await.expect("the moves permit");
        let refused = state
            .node_move_staged(
                repository.clone(),
                moved,
                inner,
                ("d", hash_string("d")),
                &permit,
                false,
            )
            .await;

        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_invalid_arguments()),
            "the move is refused: {refused:?}"
        );
        for dir in [source, moved, inner] {
            assert!(
                unmarked(&state, &repository, dir).await,
                "directory {dir} is unmarked"
            );
        }
        assert!(
            !state.has_unfinished_change(),
            "a refused move notes nothing"
        );
    })
    .await;
}
