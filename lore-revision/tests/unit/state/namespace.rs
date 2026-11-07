// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the in-place namespace primitives: `State::node_attach`,
//! `State::node_detach` and `State::node_reattach`.
#![allow(clippy::disallowed_methods)] // Tests spawn on `runtime()` and wait on a barrier in `block_in_place`.

use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::Rewrite;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenIterator;
use lore_revision::state::StateNodeChildrenWithNameIterator;
use lore_revision::state::drop_repeated_nodes;
use lore_storage::hash::hash_string;
use tokio::task::JoinSet;

use super::directory;
use super::file;
use super::null_repository;
use crate::fs::filesystem_provider::setup_test_execution;

async fn add(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    name: &str,
) -> NodeID {
    state
        .node_add(repository.clone(), parent, directory(name), name)
        .await
        .expect("adding a node must succeed")
}

/// Every live child of `parent_id`, by name, in walk order -- via the same
/// name-carrying iterator `Listing::build` uses, so a name missing here is
/// also missing from a directory listing.
async fn child_names(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_id: NodeID,
) -> Result<Vec<String>, lore_revision::state::StateError> {
    let mut names = Vec::new();
    let mut iter = StateNodeChildrenWithNameIterator::new(state, repository, parent_id).await?;
    while let Some((_id, _node, name)) = iter.next().await? {
        names.push(name.freeze());
    }
    Ok(names)
}

/// Every child the name iterator yields under `parent_id`, with its name, in walk order,
/// and whether the iterator says it may have yielded one twice.
async fn walk(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_id: NodeID,
) -> Result<(Vec<(NodeID, String)>, bool), lore_revision::state::StateError> {
    let mut walked = Vec::new();
    let mut iter = StateNodeChildrenWithNameIterator::new(state, repository, parent_id).await?;
    while let Some((id, _node, name)) = iter.next().await? {
        walked.push((id, name.freeze()));
    }
    Ok((walked, iter.may_repeat()))
}

/// `walked` with each child's position in the walk, as a consumer carries it.
fn indexed(walked: Vec<(NodeID, String)>) -> Vec<(NodeID, String, u32)> {
    walked
        .into_iter()
        .zip(0u32..)
        .map(|((id, name), index)| (id, name, index))
        .collect()
}

fn has_repeat(ids: impl Iterator<Item = NodeID>) -> bool {
    let mut ids: Vec<NodeID> = ids.collect();
    ids.sort_unstable();
    ids.windows(2).any(|pair| pair[0] == pair[1])
}

/// Rewrites `node_id`'s record in place, as no primitive would: the state a
/// test needs a walk to meet.
async fn rewrite_record(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    rewrite: impl FnOnce(&mut Node),
) {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await
        .expect("the node's block must load");
    rewrite(block.write_for_tests().node(Node::index(node_id)));
}

/// A detach is a chain change: the parent's count moves, so a holder of its children
/// sees that it no longer holds all of them as they are.
#[tokio::test]
async fn a_detach_moves_its_parents_chain_change_count() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let parent = add(&state, &repository, ROOT_NODE, "parent").await;
            let child = add(&state, &repository, parent, "child").await;
            let count = |state: Arc<State>, repository: Arc<RepositoryContext>| async move {
                state
                    .block(repository, NodeBlock::index(parent))
                    .await
                    .expect("the parent's block")
                    .chain_changes(Node::index(parent))
            };
            let before = count(state.clone(), repository.clone()).await;
            state
                .node_detach(repository.clone(), child, Detach::Always)
                .await
                .expect("detach must succeed");
            assert_ne!(
                count(state.clone(), repository.clone()).await,
                before,
                "a detach counts as a change to its parent's chain"
            );
        }))
        .await
        .expect("test task");
}

/// A rename in place by `node_move_staged` counts as a reattach in its directory, so a holder of
/// the directory's children sees that the name it holds moved.
#[tokio::test]
async fn a_staged_rename_moves_its_directorys_chain_change_count_as_a_reattach() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            let node = add(&state, &repository, dir, "name").await;
            let block = state
                .block(repository.clone(), NodeBlock::index(dir))
                .await
                .expect("the directory's block");
            let before = block.chain_changes(Node::index(dir));
            state
                .node_move_staged(
                    repository.clone(),
                    node,
                    dir,
                    ("renamed", hash_string("renamed")),
                    &state.moves_permit().await.expect("the moves permit"),
                    false,
                )
                .await
                .expect("the rename");
            let moved = block.chain_changes(Node::index(dir)).wrapping_sub(before);
            assert!(
                moved >= REATTACH_CHANGES,
                "a rename moves its directory's count as a reattach does, moved by {moved}"
            );
        }))
        .await
        .expect("test task");
}

#[tokio::test]
async fn a_detached_node_is_absent_from_its_parent_and_the_parent_is_dirty() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let clean_id = add(&state, &repository, ROOT_NODE, "clean").await;
            let clean_child = add(&state, &repository, clean_id, "child").await;
            let clean = state
                .node(repository.clone(), clean_id)
                .await
                .expect("parent must read back");
            assert!(!clean.is_dirty(), "the parent must start clean");
            state
                .node_detach(repository.clone(), clean_child, Detach::Always)
                .await
                .expect("detach must succeed");
            let clean = state
                .node(repository.clone(), clean_id)
                .await
                .expect("parent must read back");
            assert!(
                clean.is_dirty(),
                "a clean parent must be dirty after the detach"
            );

            let parent_id = add(&state, &repository, ROOT_NODE, "parent").await;
            let child_id = add(&state, &repository, parent_id, "child").await;
            state
                .node_mark(
                    &repository,
                    parent_id,
                    Mark::dirty(
                        NodeFlags::DirtyAdd,
                        Ext::Set(NodeExtFlags::LocalContent),
                        true,
                    ),
                )
                .await
                .expect("marking the parent must succeed");

            state
                .node_detach(repository.clone(), child_id, Detach::Always)
                .await
                .expect("detach must succeed");

            let found = state
                .find_subnode(repository.clone(), parent_id, hash_string("child"))
                .await;
            assert!(
                found.is_err(),
                "the detached child must not be found under its parent, got {found:?}"
            );

            let parent = state
                .node(repository.clone(), parent_id)
                .await
                .expect("parent must read back");
            assert!(
                parent.is_dirty(),
                "the parent must be dirty after the detach"
            );
            assert!(
                parent.is_dirty_add(),
                "the parent must keep its dirty action, flags {:#x}",
                parent.flags
            );
            assert_eq!(
                parent.ext_flags(),
                NodeExtFlags::LocalContent,
                "the parent must keep its extension flags"
            );

            let again = state
                .node_detach(repository.clone(), child_id, Detach::Always)
                .await;
            assert!(
                again.as_ref().is_err_and(|error| error.is_node_not_found()),
                "detaching a detached node must report NodeNotFound, got {again:?}"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn a_reader_holding_a_detached_node_walks_on_to_its_siblings() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            // The name iterator stands on c, the node ahead of b, when b is detached.
            let named = add(&state, &repository, ROOT_NODE, "named").await;
            let a_id = add(&state, &repository, named, "a").await;
            let b_id = add(&state, &repository, named, "b").await;
            let c_id = add(&state, &repository, named, "c").await;
            let mut iter =
                StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), named)
                    .await
                    .expect("iterator must build");
            let (first, _, name) = iter.next().await.expect("next").expect("c is live");
            assert_eq!((first, &*name), (c_id, "c"));
            drop(name);
            state
                .node_detach(repository.clone(), b_id, Detach::Always)
                .await
                .expect("detach must succeed");
            let (second, _, name) = iter.next().await.expect("next").expect("a is live");
            assert_eq!(
                (second, &*name),
                (a_id, "a"),
                "the name iterator must skip the detached node and reach its sibling"
            );
            drop(name);
            assert!(iter.next().await.expect("next").is_none());

            // The plain iterator, in the same position.
            let plain = add(&state, &repository, ROOT_NODE, "plain").await;
            let a_id = add(&state, &repository, plain, "a").await;
            let b_id = add(&state, &repository, plain, "b").await;
            let c_id = add(&state, &repository, plain, "c").await;
            let mut iter = StateNodeChildrenIterator::new(state.clone(), repository.clone(), plain)
                .await
                .expect("iterator must build");
            assert_eq!(
                iter.next().await.expect("next").map(|(id, _)| id),
                Some(c_id)
            );
            state
                .node_detach(repository.clone(), b_id, Detach::Always)
                .await
                .expect("detach must succeed");
            assert_eq!(
                iter.next().await.expect("next").map(|(id, _)| id),
                Some(a_id),
                "the plain iterator must skip the detached node and reach its sibling"
            );
            assert!(iter.next().await.expect("next").is_none());

            // A lookup once the directory's head was detached.
            let lookup = add(&state, &repository, ROOT_NODE, "lookup").await;
            let a_id = add(&state, &repository, lookup, "a").await;
            let head_id = add(&state, &repository, lookup, "head").await;
            state
                .node_detach(repository.clone(), head_id, Detach::Always)
                .await
                .expect("detach must succeed");
            let gone = state
                .find_subnode(repository.clone(), lookup, hash_string("head"))
                .await;
            assert!(
                gone.as_ref().is_err_and(|error| error.is_node_not_found()),
                "a lookup must not find the detached node it walks over, got {gone:?}"
            );
            let found = state
                .find_subnode(repository.clone(), lookup, hash_string("a"))
                .await
                .expect("the lookup must walk on to the sibling");
            assert_eq!(found, a_id);
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn a_detached_slot_is_never_handed_out_again() {
    const FRESH_ADDS: usize = 32;

    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let parent_id = add(&state, &repository, ROOT_NODE, "parent").await;
            let gone_id = add(&state, &repository, parent_id, "gone").await;

            state
                .node_detach(repository.clone(), gone_id, Detach::Always)
                .await
                .expect("detach must succeed");

            let mut fresh_ids = Vec::with_capacity(FRESH_ADDS);
            for i in 0..FRESH_ADDS {
                let name = format!("fresh-{i}");
                fresh_ids.push(add(&state, &repository, parent_id, &name).await);
            }

            assert!(
                !fresh_ids.contains(&gone_id),
                "the detached slot must never be handed back out by a later add"
            );

            let gone = state
                .node(repository.clone(), gone_id)
                .await
                .expect("a detached slot still reads back");
            assert!(
                gone.is_discarded(),
                "the detached node's own slot must still read as discarded"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn attach_of_a_name_already_held_reports_exists() {
    const REPEATS: usize = 64;

    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let parent_id = add(&state, &repository, ROOT_NODE, "parent").await;
            let first = state
                .node_attach(repository.clone(), parent_id, Node::default(), "taken")
                .await
                .expect("the first attach must succeed");
            let AttachOutcome::Attached(first_id) = first else {
                panic!("the first attach must attach, got {first:?}");
            };
            let attached = state
                .node(repository.clone(), first_id)
                .await
                .expect("the attached node must read back");
            assert_eq!(
                attached.name_hash,
                hash_string("taken"),
                "an attached node must carry the hash of the name it is attached by"
            );

            let block = state
                .block(repository.clone(), NodeBlock::index(first_id))
                .await
                .expect("the block must load");
            let (names_before, nodes_before) = {
                let reader = block.read();
                (reader.clone_name_table().len(), reader.node_count())
            };

            for spelling in ["taken", "TAKEN"].into_iter().cycle().take(REPEATS) {
                let result = state
                    .node_attach(repository.clone(), parent_id, directory(spelling), spelling)
                    .await
                    .expect("an attach of a held name must report Exists, not error");
                assert_eq!(
                    result,
                    AttachOutcome::Exists(first_id),
                    "attaching a name a live sibling holds must report that sibling"
                );
            }

            let reader = block.read();
            assert_eq!(
                reader.clone_name_table().len(),
                names_before,
                "an attach that finds the name taken must store no name"
            );
            assert_eq!(
                reader.node_count(),
                nodes_before,
                "an attach that finds the name taken must take no slot"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn attach_whose_name_scan_fails_returns_the_error() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let parent_id = add(&state, &repository, ROOT_NODE, "parent").await;
            let other_id = add(&state, &repository, ROOT_NODE, "other").await;
            add(&state, &repository, parent_id, "a").await;
            let broken_id = add(&state, &repository, parent_id, "broken").await;
            rewrite_record(&state, &repository, broken_id, |node| {
                node.parent = other_id;
            })
            .await;

            let result = state
                .node_attach(repository.clone(), parent_id, directory("new"), "new")
                .await;
            assert!(
                result.is_err(),
                "an attach whose name scan fails must return the error, got {result:?}"
            );
            let parent = state
                .node(repository.clone(), parent_id)
                .await
                .expect("parent must read back");
            assert_eq!(
                parent.child(),
                Some(broken_id),
                "a failed attach must attach nothing"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn attach_under_a_file_or_a_detached_directory_is_refused_and_takes_no_slot() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let parent_id = add(&state, &repository, ROOT_NODE, "parent").await;
            let file_id = state
                .node_add(repository.clone(), parent_id, file("file"), "file")
                .await
                .expect("adding a file must succeed");
            let detached = add(&state, &repository, ROOT_NODE, "detached").await;
            state
                .node_detach(repository.clone(), detached, Detach::Always)
                .await
                .expect("detach must succeed");
            let block = state
                .block(repository.clone(), NodeBlock::index(file_id))
                .await
                .expect("the block must load");
            let nodes_before = block.read().node_count();

            let under_file = state
                .node_attach(repository.clone(), file_id, directory("x"), "x")
                .await;
            assert!(
                under_file
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "an attach under a file must be refused, got {under_file:?}"
            );
            let under_detached = state
                .node_attach(repository.clone(), detached, directory("x"), "x")
                .await;
            assert!(
                under_detached
                    .as_ref()
                    .is_err_and(|error| error.is_node_not_found()),
                "an attach under a detached directory must report NodeNotFound, got \
                     {under_detached:?}"
            );
            assert_eq!(
                block.read().node_count(),
                nodes_before,
                "a refused attach must take no slot"
            );
        }))
        .await
        .expect("Test task failed");
}

/// Runs on a multi-thread runtime: `runtime()` hands back the test's own. Every attach is of
/// one name into a directory nothing else changes, so where attaches race, one's lookup misses
/// the name and its publish finds another's published since.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_attaches_of_one_name_attach_it_once_and_name_the_winner() {
    const ATTACHES: usize = 4;
    const ROUNDS: usize = 1000;

    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let mut failure = None;
            for round in 0..ROUNDS {
                let state = State::new();
                let dir = add(&state, &repository, ROOT_NODE, "dir").await;
                let start = Arc::new(std::sync::Barrier::new(ATTACHES));
                let mut attaches = JoinSet::new();
                for _ in 0..ATTACHES {
                    let (state, repository, start) =
                        (state.clone(), repository.clone(), start.clone());
                    let _ = lore_spawn!(attaches, async move {
                        tokio::task::block_in_place(|| start.wait());
                        state
                            .node_attach(repository, dir, directory("dup"), "dup")
                            .await
                    });
                }
                let outcomes: Vec<AttachOutcome> = attaches
                    .join_all()
                    .await
                    .into_iter()
                    .map(|outcome| outcome.expect("an attach of a held name reports Exists"))
                    .collect();
                let attached: Vec<NodeID> = outcomes
                    .iter()
                    .filter_map(|outcome| match outcome {
                        AttachOutcome::Attached(id) => Some(*id),
                        AttachOutcome::Exists(_) => None,
                    })
                    .collect();
                let listed = state
                    .node_children(repository.clone(), dir)
                    .await
                    .expect("the directory lists");
                let one_winner = attached.len() == 1
                    && outcomes.iter().all(|outcome| match outcome {
                        AttachOutcome::Attached(id) | AttachOutcome::Exists(id) => {
                            *id == attached[0]
                        }
                    })
                    && listed == attached;
                if !one_winner {
                    failure.get_or_insert_with(|| {
                        format!(
                            "round {round}: one attach must attach the name and the others \
                                 name it, got {outcomes:?}, listed {listed:?}"
                        )
                    });
                }
            }
            if let Some(failure) = failure {
                panic!("{failure}");
            }
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn reattach_moves_a_node_between_directories_and_dirties_both() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let old_parent = add(&state, &repository, ROOT_NODE, "old").await;
            let new_parent = add(&state, &repository, ROOT_NODE, "new").await;
            let node_id = add(&state, &repository, old_parent, "moved").await;
            let holder_id = add(&state, &repository, new_parent, "held").await;

            let outcome = state
                .node_reattach(repository.clone(), node_id, new_parent, "renamed")
                .await
                .expect("reattach must succeed");
            assert_eq!(outcome, AttachOutcome::Attached(node_id));

            let gone = state
                .find_subnode(repository.clone(), old_parent, hash_string("moved"))
                .await;
            assert!(
                gone.is_err(),
                "the node must no longer be found under its old parent, got {gone:?}"
            );
            let found = state
                .find_subnode(repository.clone(), new_parent, hash_string("renamed"))
                .await
                .expect("the node must be found under its new parent by its new name");
            assert_eq!(found, node_id);

            let moved = state
                .node(repository.clone(), node_id)
                .await
                .expect("the moved node must read back");
            assert_eq!(moved.parent, new_parent);
            assert_eq!(moved.name_hash, hash_string("renamed"));
            assert!(
                !moved.is_discarded(),
                "a reattach must not discard the node"
            );
            for parent in [old_parent, new_parent] {
                let parent = state
                    .node(repository.clone(), parent)
                    .await
                    .expect("parent must read back");
                assert!(parent.is_dirty(), "both parents must be dirty");
            }

            let taken = state
                .node_reattach(repository.clone(), node_id, new_parent, "held")
                .await
                .expect("a reattach onto a held name must report Exists, not error");
            assert_eq!(taken, AttachOutcome::Exists(holder_id));
            let unchanged = state
                .node(repository.clone(), node_id)
                .await
                .expect("the node must read back");
            assert_eq!(
                unchanged.name_hash,
                hash_string("renamed"),
                "a refused reattach must leave the node's name"
            );

            let file_id = state
                .node_add(repository.clone(), new_parent, file("file"), "file")
                .await
                .expect("adding a file must succeed");
            let under_file = state
                .node_reattach(repository.clone(), node_id, file_id, "x")
                .await;
            assert!(
                under_file
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "a reattach under a file must be refused, got {under_file:?}"
            );
            let detached_dir = add(&state, &repository, ROOT_NODE, "detached").await;
            state
                .node_detach(repository.clone(), detached_dir, Detach::Always)
                .await
                .expect("detach must succeed");
            let under_detached = state
                .node_reattach(repository.clone(), node_id, detached_dir, "x")
                .await;
            assert!(
                under_detached
                    .as_ref()
                    .is_err_and(|error| error.is_node_not_found()),
                "a reattach under a detached directory must report NodeNotFound, got \
                     {under_detached:?}"
            );
            let still = state
                .find_subnode(repository.clone(), new_parent, hash_string("renamed"))
                .await
                .expect("a refused reattach must leave the node where it was");
            assert_eq!(still, node_id);
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn detach_refuses_an_id_that_names_no_detachable_node() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;

            for id in [ROOT_NODE, INVALID_NODE] {
                let detached = state
                    .node_detach(repository.clone(), id, Detach::Always)
                    .await;
                assert!(
                    detached
                        .as_ref()
                        .is_err_and(|error| error.is_invalid_arguments()),
                    "detach of {id:#x} must be refused, got {detached:?}"
                );
                let at = state
                    .node_detach(
                        repository.clone(),
                        id,
                        Detach::At(ROOT_NODE, hash_string("dir")),
                    )
                    .await;
                let if_clean = state
                    .node_detach(repository.clone(), id, Detach::IfClean)
                    .await;
                let if_empty = state
                    .node_detach(repository.clone(), id, Detach::IfEmpty)
                    .await;
                for (what, result) in [("at", at), ("if clean", if_clean), ("if empty", if_empty)] {
                    assert!(
                        result
                            .as_ref()
                            .is_err_and(|error| error.is_invalid_arguments()),
                        "detach {what} of {id:#x} must be refused, got {result:?}"
                    );
                }
            }
            state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("dir"))
                .await
                .expect("a refused detach must leave the tree");
            assert!(
                !state
                    .node(repository.clone(), dir)
                    .await
                    .expect("dir")
                    .is_discarded()
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn reattach_refuses_an_id_a_parent_or_a_name_it_cannot_take() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            let node = add(&state, &repository, dir, "node").await;

            let refused = [
                ("the root", ROOT_NODE, dir, "x"),
                ("an invalid node", INVALID_NODE, dir, "x"),
                ("an invalid parent", node, INVALID_NODE, "x"),
                ("a parent reference", node, dir, ".."),
                ("a name with a separator", node, dir, "a/b"),
            ];
            for (what, id, parent, name) in refused {
                let moved = state
                    .node_reattach(repository.clone(), id, parent, name)
                    .await;
                assert!(
                    moved
                        .as_ref()
                        .is_err_and(|error| error.is_invalid_arguments()),
                    "a reattach of {what} must be refused, got {moved:?}"
                );
            }
            let still = state
                .find_subnode(repository.clone(), dir, hash_string("node"))
                .await
                .expect("a refused reattach must leave the node where it was");
            assert_eq!(still, node);
            assert!(
                !state
                    .node(repository.clone(), dir)
                    .await
                    .expect("dir")
                    .is_dirty(),
                "a refused reattach must not mark its parents dirty"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn a_reader_on_a_reattached_node_restarts_and_finds_every_sibling() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let elsewhere = add(&state, &repository, ROOT_NODE, "elsewhere").await;

            // The name iterator has yielded d and c when b moves away.
            let named = add(&state, &repository, ROOT_NODE, "named").await;
            let a_id = add(&state, &repository, named, "a").await;
            let b_id = add(&state, &repository, named, "b").await;
            let c_id = add(&state, &repository, named, "c").await;
            let d_id = add(&state, &repository, named, "d").await;
            let mut iter =
                StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), named)
                    .await
                    .expect("iterator must build");
            let mut yielded = Vec::new();
            for _ in 0..2 {
                let (id, _, name) = iter.next().await.expect("next").expect("a live child");
                drop(name);
                yielded.push(id);
            }
            state
                .node_reattach(repository.clone(), b_id, elsewhere, "b-named")
                .await
                .expect("reattach must succeed");
            while let Some((id, _, name)) = iter
                .next()
                .await
                .expect("the name iterator must restart, not fail")
            {
                drop(name);
                yielded.push(id);
            }
            assert_eq!(
                yielded,
                vec![d_id, c_id, a_id],
                "the name iterator must yield every remaining sibling once"
            );

            // The plain iterator, in the same position.
            let plain = add(&state, &repository, ROOT_NODE, "plain").await;
            let a_id = add(&state, &repository, plain, "a").await;
            let b_id = add(&state, &repository, plain, "b").await;
            let c_id = add(&state, &repository, plain, "c").await;
            let d_id = add(&state, &repository, plain, "d").await;
            let mut iter = StateNodeChildrenIterator::new(state.clone(), repository.clone(), plain)
                .await
                .expect("iterator must build");
            let mut yielded = Vec::new();
            for _ in 0..2 {
                let (id, _) = iter.next().await.expect("next").expect("a live child");
                yielded.push(id);
            }
            state
                .node_reattach(repository.clone(), b_id, elsewhere, "b-plain")
                .await
                .expect("reattach must succeed");
            while let Some((id, _)) = iter
                .next()
                .await
                .expect("the plain iterator must restart, not fail")
            {
                yielded.push(id);
            }
            assert_eq!(
                yielded,
                vec![d_id, c_id, a_id],
                "the plain iterator must yield every remaining sibling once"
            );

            // A lookup once the directory's head moved away.
            let lookup = add(&state, &repository, ROOT_NODE, "lookup").await;
            let a_id = add(&state, &repository, lookup, "a").await;
            let head_id = add(&state, &repository, lookup, "head").await;
            state
                .node_reattach(repository.clone(), head_id, elsewhere, "head-moved")
                .await
                .expect("reattach must succeed");
            let found = state
                .find_subnode(repository.clone(), lookup, hash_string("a"))
                .await
                .expect("the lookup must find the sibling");
            assert_eq!(found, a_id);
        }))
        .await
        .expect("Test task failed");
}

/// Walks `parent_id` with the name iterator, running `change` once `before` children have
/// been yielded, and returns what the walk yielded and whether it may repeat.
async fn walk_around<F, Fut>(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    parent_id: NodeID,
    before: usize,
    change: F,
) -> (Vec<(NodeID, String)>, bool)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut iter =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), parent_id)
            .await
            .expect("iterator must build");
    let mut walked = Vec::new();
    for _ in 0..before {
        let (id, _, name) = iter.next().await.expect("next").expect("a live child");
        walked.push((id, name.freeze()));
    }
    change().await;
    while let Some((id, _, name)) = iter.next().await.expect("the walk must go on") {
        walked.push((id, name.freeze()));
    }
    (walked, iter.may_repeat())
}

#[tokio::test]
async fn a_walk_past_a_rename_in_its_directory_says_it_may_repeat() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            for name in ["a", "b", "c", "d"] {
                add(&state, &repository, dir, name).await;
            }
            let c_id = state
                .find_subnode(repository.clone(), dir, hash_string("c"))
                .await
                .expect("c is a child");

            let (walked, may_repeat) = walk_around(&state, &repository, dir, 1, || async {
                state
                    .node_reattach(repository.clone(), c_id, dir, "c2")
                    .await
                    .expect("reattach must succeed");
            })
            .await;
            assert!(
                may_repeat || !has_repeat(walked.iter().map(|(id, _)| *id)),
                "a walk that yields a node twice must say it may repeat, got {walked:?}"
            );
            assert!(
                may_repeat,
                "a rename in the walked directory must be reported"
            );
            let mut kept = indexed(walked);
            drop_repeated_nodes(&mut kept, |(id, _, index)| (*id, *index));
            assert!(!has_repeat(kept.iter().map(|(id, _, _)| *id)));
            assert_eq!(kept.len(), 4, "every child must be kept once, got {kept:?}");
            assert!(
                kept.iter().any(|(id, name, _)| *id == c_id && name == "c2"),
                "the renamed child must keep its last name, got {kept:?}"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn a_plain_walk_past_a_rename_in_its_directory_says_it_may_repeat() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            let ids = add_all(&state, &repository, dir, &["a", "b", "c", "d"]).await;

            let mut iter = StateNodeChildrenIterator::new(state.clone(), repository.clone(), dir)
                .await
                .expect("iterator must build");
            let mut walked = vec![iter.next().await.expect("next").expect("a live child").0];
            state
                .node_reattach(repository.clone(), ids[2], dir, "c2")
                .await
                .expect("reattach must succeed");
            while let Some((id, _)) = iter.next().await.expect("the walk must go on") {
                walked.push(id);
            }
            assert!(
                iter.may_repeat(),
                "a rename in the walked directory must be reported, walked {walked:?}"
            );
        }))
        .await
        .expect("Test task failed");
}

/// Adds `names` under `dir` in order, so the chain lists them in reverse, and returns
/// their ids in the order given.
async fn add_all(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
    names: &[&str],
) -> Vec<NodeID> {
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        ids.push(add(state, repository, dir, name).await);
    }
    ids
}

#[tokio::test]
async fn a_walk_sent_back_over_its_nodes_by_two_renames_restarts() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            let ids = add_all(&state, &repository, dir, &["a", "b", "c", "d"]).await;
            let (b_id, c_id) = (ids[1], ids[2]);

            let mut iter =
                StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), dir)
                    .await
                    .expect("iterator must build");
            let mut walked = Vec::new();
            for _ in 0..2 {
                let (id, _, name) = iter.next().await.expect("next").expect("a live child");
                walked.push((id, name.freeze()));
            }
            state
                .node_reattach(repository.clone(), c_id, dir, "c2")
                .await
                .expect("reattach must succeed");
            state
                .node_reattach(repository.clone(), b_id, dir, "b2")
                .await
                .expect("reattach must succeed");
            loop {
                match iter.next().await {
                    Ok(Some((id, _, name))) => walked.push((id, name.freeze())),
                    Ok(None) => break,
                    Err(error) => panic!("the walk must restart, not fail: {error:?}"),
                }
            }
            assert!(
                iter.may_repeat(),
                "the walk must say it may repeat, got {walked:?}"
            );
            let mut seen: Vec<NodeID> = walked.iter().map(|(id, _)| *id).collect();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), 4, "every child must be yielded, got {walked:?}");
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn a_walk_sent_back_over_its_nodes_by_detaches_and_a_rename_restarts() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            let ids = add_all(&state, &repository, dir, &["a", "b", "c", "d", "e"]).await;
            let (a_id, c_id, e_id) = (ids[0], ids[2], ids[4]);

            let mut iter =
                StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), dir)
                    .await
                    .expect("iterator must build");
            let mut walked = Vec::new();
            for _ in 0..2 {
                let (id, _, name) = iter.next().await.expect("next").expect("a live child");
                walked.push((id, name.freeze()));
            }
            for detached in [e_id, a_id] {
                state
                    .node_detach(repository.clone(), detached, Detach::Always)
                    .await
                    .expect("detach must succeed");
            }
            state
                .node_reattach(repository.clone(), c_id, dir, "c2")
                .await
                .expect("reattach must succeed");
            loop {
                match iter.next().await {
                    Ok(Some((id, _, name))) => walked.push((id, name.freeze())),
                    Ok(None) => break,
                    Err(error) => panic!("the walk must restart, not fail: {error:?}"),
                }
            }
            assert!(
                iter.may_repeat(),
                "the walk must say it may repeat, got {walked:?}"
            );
            assert!(
                walked.iter().any(|(id, name)| *id == c_id && name == "c2"),
                "the renamed child must be yielded by its new name, got {walked:?}"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn a_walk_past_a_rename_and_a_move_away_says_it_may_repeat() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let elsewhere = add(&state, &repository, ROOT_NODE, "elsewhere").await;
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            for name in ["a", "b", "c", "d", "e"] {
                add(&state, &repository, dir, name).await;
            }
            let id_of = async |name: &str| {
                state
                    .find_subnode(repository.clone(), dir, hash_string(name))
                    .await
                    .expect("a child")
            };
            let (c_id, d_id) = (id_of("c").await, id_of("d").await);

            let (walked, may_repeat) = walk_around(&state, &repository, dir, 2, || async {
                state
                    .node_reattach(repository.clone(), d_id, dir, "d2")
                    .await
                    .expect("reattach must succeed");
                state
                    .node_reattach(repository.clone(), c_id, elsewhere, "c")
                    .await
                    .expect("reattach must succeed");
            })
            .await;
            assert!(
                may_repeat || !has_repeat(walked.iter().map(|(id, _)| *id)),
                "a walk that yields a node twice must say it may repeat, got {walked:?}"
            );
            assert!(may_repeat, "a restarted walk must be reported");
            let mut kept = indexed(walked);
            drop_repeated_nodes(&mut kept, |(id, _, index)| (*id, *index));
            assert!(!has_repeat(kept.iter().map(|(id, _, _)| *id)));
            assert!(
                !kept.iter().any(|(id, _, _)| *id == c_id),
                "the child moved away must not be kept, got {kept:?}"
            );
        }))
        .await
        .expect("Test task failed");
}

/// Every node under `ROOT_NODE`, found by walking down from it, or an error on a cycle.
async fn reachable(state: &Arc<State>, repository: &Arc<RepositoryContext>) -> Vec<NodeID> {
    let mut found = Vec::new();
    let mut pending = vec![ROOT_NODE];
    while let Some(parent) = pending.pop() {
        for child in state
            .node_children(repository.clone(), parent)
            .await
            .expect("the children must list")
        {
            assert!(found.len() < 1024, "the tree must stay acyclic");
            found.push(child);
            pending.push(child);
        }
    }
    found
}

/// Runs on a multi-thread runtime: `runtime()` hands back the test's own. The two
/// destinations sit in different node blocks, so neither move's chain check sees the
/// other's prepend.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reattaches_of_directories_into_each_other_keep_the_tree_acyclic() {
    const ROUNDS: usize = 1000;

    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let mut failure = None;
            for round in 0..ROUNDS {
                let state = State::new();
                let a = add(&state, &repository, ROOT_NODE, "a").await;
                let b = add(&state, &repository, ROOT_NODE, "b").await;
                let under_a = add(&state, &repository, a, "under-a").await;
                let filler = add(&state, &repository, ROOT_NODE, "filler").await;
                for i in 0..BLOCK_NODE_COUNT {
                    add(&state, &repository, filler, &format!("f{i}")).await;
                }
                let under_b = add(&state, &repository, b, "under-b").await;
                assert_ne!(NodeBlock::index(under_a), NodeBlock::index(under_b));

                // Both moves start on threads of their own at once.
                let start = Arc::new(std::sync::Barrier::new(2));
                let mut moves = JoinSet::new();
                for (node, into) in [(a, under_b), (b, under_a)] {
                    let (state, repository, start) =
                        (state.clone(), repository.clone(), start.clone());
                    let _ = lore_spawn!(moves, async move {
                        tokio::task::block_in_place(|| start.wait());
                        state.node_reattach(repository, node, into, "moved").await
                    });
                }
                let mut attached = 0;
                for result in moves.join_all().await {
                    match result {
                        Ok(AttachOutcome::Attached(_)) => attached += 1,
                        Err(error) if error.is_invalid_arguments() => {}
                        other => {
                            failure.get_or_insert_with(|| {
                                format!(
                                    "round {round}: a move attaches or is refused, got {other:?}"
                                )
                            });
                        }
                    }
                }
                if attached != 1 {
                    failure.get_or_insert_with(|| {
                        format!("round {round}: one move of the two attaches, {attached} did")
                    });
                }

                let mut found = reachable(&state, &repository).await;
                found.sort_unstable();
                for node in [a, b, under_a, under_b, filler] {
                    if found.binary_search(&node).is_err() {
                        failure.get_or_insert_with(|| {
                            format!("round {round}: node {node} must stay reachable from the root")
                        });
                    }
                }
            }
            if let Some(failure) = failure {
                panic!("{failure}");
            }
        }))
        .await
        .expect("Test task failed");
}

/// Walks `dir`, whose chain lists `ids` in reverse, one child at a time, moving the
/// child after the one just yielded to `other` before each step, `rounds` times, and
/// detaching the child just yielded before the step `detach_on` names. Returns the
/// walk's error, if any.
async fn walk_while_moving_out(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
    other: NodeID,
    ids: &[NodeID],
    rounds: usize,
    detach_on: Option<usize>,
) -> Option<lore_revision::state::StateError> {
    let mut iter = StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), dir)
        .await
        .expect("iterator must build");
    let (mut last, _, name) = iter.next().await.expect("next").expect("a live child");
    drop(name);
    for round in 0..rounds {
        let mut next = ids.iter().position(|&id| id == last).expect("a child") - 1;
        loop {
            let node = state
                .node(repository.clone(), ids[next])
                .await
                .expect("node");
            if node.parent == dir && !node.is_discarded() {
                break;
            }
            next -= 1;
        }
        if detach_on == Some(round) {
            state
                .node_detach(repository.clone(), last, Detach::Always)
                .await
                .expect("detach must succeed");
        }
        state
            .node_reattach(repository.clone(), ids[next], other, &format!("m{next}"))
            .await
            .expect("reattach must succeed");
        match iter.next().await {
            Ok(Some((id, _, name))) => {
                drop(name);
                last = id;
            }
            Ok(None) => return None,
            Err(error) => return Some(error),
        }
    }
    None
}

/// A directory of 160 children, and a directory in another node block.
async fn directory_and_a_far_one(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
) -> (NodeID, NodeID, Vec<NodeID>) {
    let dir = add(state, repository, ROOT_NODE, "dir").await;
    let mut ids = Vec::with_capacity(160);
    for i in 0..160 {
        ids.push(add(state, repository, dir, &format!("a{i}")).await);
    }
    let filler = add(state, repository, ROOT_NODE, "filler").await;
    for i in 0..BLOCK_NODE_COUNT {
        add(state, repository, filler, &format!("f{i}")).await;
    }
    let far = add(state, repository, ROOT_NODE, "far").await;
    assert_ne!(NodeBlock::index(dir), NodeBlock::index(far));
    (dir, far, ids)
}

#[tokio::test]
async fn a_walk_whose_next_child_keeps_moving_to_another_block_slows_down() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let (dir, far, ids) = directory_and_a_far_one(&state, &repository).await;
            let error = walk_while_moving_out(&state, &repository, dir, far, &ids, 65, None)
                .await
                .expect("65 moves under one walk must run out of restarts");
            assert!(
                error.is_slow_down(),
                "a walk out of restarts must report SlowDown, got {error:?}"
            );
        }))
        .await
        .expect("Test task failed");
}

#[tokio::test]
async fn a_walk_whose_resume_child_is_detached_after_many_restarts_slows_down() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let (dir, far, ids) = directory_and_a_far_one(&state, &repository).await;
            let error = walk_while_moving_out(&state, &repository, dir, far, &ids, 65, Some(63))
                .await
                .expect("the walk must run out of restarts");
            assert!(
                error.is_slow_down(),
                "a walk out of restarts must report SlowDown, got {error:?}"
            );
        }))
        .await
        .expect("Test task failed");
}

/// Two directories in one node block: a large one and a busy one.
async fn a_directory_beside_a_busy_one(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    children: usize,
) -> (NodeID, NodeID) {
    let dir = add(state, repository, ROOT_NODE, "dir").await;
    let busy = add(state, repository, ROOT_NODE, "busy").await;
    assert_eq!(NodeBlock::index(dir), NodeBlock::index(busy));
    for i in 0..children {
        add(state, repository, dir, &format!("c{i}")).await;
    }
    (dir, busy)
}

/// Adds into `busy` from `tasks` tasks until `stop` is set.
fn keep_adding(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    busy: NodeID,
    tasks: usize,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> JoinSet<()> {
    let mut adders = JoinSet::new();
    for task in 0..tasks {
        let (state, repository, stop) = (state.clone(), repository.clone(), stop.clone());
        let _ = lore_spawn!(adders, async move {
            let mut i = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                add(&state, &repository, busy, &format!("b{task}-{i}")).await;
                i += 1;
                if i.is_multiple_of(16) {
                    tokio::task::yield_now().await;
                }
            }
        });
    }
    adders
}

#[tokio::test]
async fn a_walk_of_an_unchanged_directory_does_not_repeat_for_adds_beside_it() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let (dir, busy) = a_directory_beside_a_busy_one(&state, &repository, 300).await;
            let (walked, may_repeat) = walk_around(&state, &repository, dir, 10, || async {
                add(&state, &repository, busy, "beside").await;
            })
            .await;
            assert_eq!(walked.len(), 300);
            assert!(
                !may_repeat,
                "an add to another directory must not make a walk report a repeat"
            );
        }))
        .await
        .expect("Test task failed");
}

/// Runs on a multi-thread runtime: `runtime()` hands back the test's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_order_holds_while_a_directory_in_the_same_block_takes_adds() {
    const OVERLAPPED: usize = 64;
    const MAX_LISTINGS: usize = 100_000;
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let (dir, busy) = a_directory_beside_a_busy_one(&state, &repository, 600).await;
            let quiet = state
                .node_children_with_name_hash(repository.clone(), dir)
                .await
                .expect("the children must list");
            let block = state
                .block(repository.clone(), NodeBlock::index(busy))
                .await
                .expect("the block must load");
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let adders = keep_adding(&state, &repository, busy, 1, &stop);
            let (mut listings, mut overlapped) = (0usize, 0usize);
            let (mut reordered, mut repeated) = (None, None);
            while overlapped < OVERLAPPED && listings < MAX_LISTINGS {
                let adds_before = block.chain_changes(Node::index(busy));
                let listed = state
                    .node_children_with_name_hash(repository.clone(), dir)
                    .await
                    .expect("the children must list");
                let (_, may_repeat) = walk(state.clone(), repository.clone(), dir)
                    .await
                    .expect("the children must walk");
                if block.chain_changes(Node::index(busy)) != adds_before {
                    overlapped += 1;
                }
                if listed != quiet {
                    reordered.get_or_insert(listings);
                }
                if may_repeat {
                    repeated.get_or_insert(listings);
                }
                listings += 1;
                if listings.is_multiple_of(64) {
                    tokio::task::yield_now().await;
                }
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            adders.join_all().await;
            assert_eq!(
                overlapped, OVERLAPPED,
                "adds into the other directory landed during only {overlapped} of {listings} \
                     listings"
            );
            assert_eq!(
                reordered, None,
                "a listing must keep sibling order while another directory takes adds"
            );
            assert_eq!(
                repeated, None,
                "a walk must not report a repeat while another directory in its block takes \
                     adds"
            );
        }))
        .await
        .expect("Test task failed");
}

/// Runs on a multi-thread runtime: `runtime()` hands back the test's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attaches_succeed_while_a_directory_in_the_same_block_takes_adds() {
    const OVERLAPPED: usize = 100;
    const MAX_ATTACHES: usize = 10_000;
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let (dir, busy) = a_directory_beside_a_busy_one(&state, &repository, 3000).await;
            let block = state
                .block(repository.clone(), NodeBlock::index(busy))
                .await
                .expect("the block must load");
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let adders = keep_adding(&state, &repository, busy, 2, &stop);
            tokio::task::yield_now().await;
            let mut failed = Vec::new();
            let (mut attaches, mut overlapped) = (0usize, 0usize);
            while overlapped < OVERLAPPED && attaches < MAX_ATTACHES {
                let name = format!("n{attaches}");
                let adds_before = block.chain_changes(Node::index(busy));
                let attached = state
                    .node_attach(repository.clone(), dir, directory(&name), &name)
                    .await;
                if block.chain_changes(Node::index(busy)) != adds_before {
                    overlapped += 1;
                }
                if let Err(error) = attached {
                    failed.push(error);
                }
                attaches += 1;
                tokio::task::yield_now().await;
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            adders.join_all().await;
            assert_eq!(
                overlapped, OVERLAPPED,
                "adds into the other directory landed during only {overlapped} of {attaches} \
                     attaches"
            );
            assert!(
                failed.is_empty(),
                "every attach must succeed beside a busy directory, {} failed: {:?}",
                failed.len(),
                failed.first()
            );
        }))
        .await
        .expect("Test task failed");
}

/// Asserts that `result` is an `InvalidNodeHierarchy`.
fn assert_broken<T: std::fmt::Debug>(
    what: &str,
    result: Result<T, lore_revision::state::StateError>,
) {
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.is_invalid_node_hierarchy()),
        "{what} must report the broken hierarchy, got {result:?}"
    );
}

/// Every reader and the unlink over `dir` report `InvalidNodeHierarchy`; `unreached`
/// is a child of `dir` its chain does not reach.
async fn assert_every_walk_reports_broken(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
    unreached: NodeID,
) {
    let named = async {
        let mut iter =
            StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), dir).await?;
        while iter.next().await?.is_some() {}
        Ok(())
    };
    assert_broken("the name iterator", named.await);
    let plain = async {
        let mut iter =
            StateNodeChildrenIterator::new(state.clone(), repository.clone(), dir).await?;
        while iter.next().await?.is_some() {}
        Ok(())
    };
    assert_broken("the plain iterator", plain.await);
    assert_broken(
        "find_subnode",
        state
            .find_subnode(repository.clone(), dir, hash_string("absent"))
            .await,
    );
    assert_broken(
        "node_children",
        state.node_children(repository.clone(), dir).await,
    );
    assert_broken(
        "node_detach",
        state
            .node_detach(repository.clone(), unreached, Detach::Always)
            .await,
    );
    let target = add(state, repository, ROOT_NODE, &format!("target-{dir}")).await;
    assert_broken(
        "node_reattach",
        state
            .node_reattach(repository.clone(), unreached, target, "moved")
            .await,
    );
}

#[tokio::test]
async fn a_static_cycle_or_wrong_parent_is_reported_by_every_walk() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let cyclic = add(&state, &repository, ROOT_NODE, "cyclic").await;
            let ids = add_all(&state, &repository, cyclic, &["x", "a", "b", "c"]).await;
            let (x, a, c) = (ids[0], ids[1], ids[3]);
            rewrite_record(&state, &repository, a, |node| node.sibling = c).await;
            assert_every_walk_reports_broken(&state, &repository, cyclic, x).await;

            let other = add(&state, &repository, ROOT_NODE, "other").await;
            let misparented = add(&state, &repository, ROOT_NODE, "misparented").await;
            let ids = add_all(&state, &repository, misparented, &["a", "b", "c"]).await;
            rewrite_record(&state, &repository, ids[1], |node| node.parent = other).await;
            assert_every_walk_reports_broken(&state, &repository, misparented, ids[0]).await;
        }))
        .await
        .expect("Test task failed");
}

/// Runs on a multi-thread runtime: `runtime()` hands back the test's own, and on the
/// default current-thread one the changes would never overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_attaches_and_detaches_in_one_directory_leave_a_chain_that_lists_every_survivor()
{
    const ROUNDS: usize = 1000;
    const BASE: usize = 12;
    const DETACHED: std::ops::Range<usize> = 0..6;
    const MOVED: std::ops::Range<usize> = 6..8;
    const RENAMED: usize = 8;
    const UNTOUCHED: std::ops::Range<usize> = 9..12;
    const ATTACHED: usize = 4;
    const DUPLICATES: usize = 3;
    const READERS: usize = 2;

    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;

            let mut walk_failure = None;
            for round in 0..ROUNDS {
                let state = State::new();
                let dir_a = add(&state, &repository, ROOT_NODE, "a").await;
                let dir_b = add(&state, &repository, ROOT_NODE, "b").await;
                let mut base = Vec::with_capacity(BASE);
                for i in 0..BASE {
                    base.push(add(&state, &repository, dir_a, &format!("base-{i}")).await);
                }

                let mut changes = JoinSet::new();
                for &id in &base[DETACHED] {
                    let (state, repository) = (state.clone(), repository.clone());
                    let _ = lore_spawn!(changes, async move {
                        state
                            .node_detach(repository, id, Detach::Always)
                            .await
                            .map(|_| None)
                    });
                }
                for i in MOVED {
                    let (state, repository, id) = (state.clone(), repository.clone(), base[i]);
                    let _ = lore_spawn!(changes, async move {
                        let name = format!("moved-{i}");
                        state
                            .node_reattach(repository, id, dir_b, &name)
                            .await
                            .map(|_| None)
                    });
                }
                {
                    let (state, repository, id) =
                        (state.clone(), repository.clone(), base[RENAMED]);
                    let _ = lore_spawn!(changes, async move {
                        let name = format!("renamed-{RENAMED}");
                        state
                            .node_reattach(repository, id, dir_a, &name)
                            .await
                            .map(|_| None)
                    });
                }
                for i in 0..ATTACHED {
                    let (state, repository) = (state.clone(), repository.clone());
                    let _ = lore_spawn!(changes, async move {
                        let name = format!("new-{i}");
                        state
                            .node_attach(repository, dir_a, directory(&name), &name)
                            .await
                            .map(|_| None)
                    });
                }
                for _ in 0..DUPLICATES {
                    let (state, repository) = (state.clone(), repository.clone());
                    let _ = lore_spawn!(changes, async move {
                        state
                            .node_attach(repository, dir_a, directory("dup"), "dup")
                            .await
                            .map(Some)
                    });
                }
                let mut readers = JoinSet::new();
                for _ in 0..READERS {
                    let (state, repository) = (state.clone(), repository.clone());
                    let _ = lore_spawn!(readers, async move {
                        let failed = |error: lore_revision::state::StateError| {
                            format!("a concurrent walk failed: {error:?}")
                        };
                        for _ in 0..4 {
                            let (walked, may_repeat) =
                                walk(state.clone(), repository.clone(), dir_a)
                                    .await
                                    .map_err(failed)?;
                            if !may_repeat && has_repeat(walked.iter().map(|(id, _)| *id)) {
                                return Err(format!(
                                    "a walk that yields a node twice must say it may repeat, \
                                         got {walked:?}"
                                ));
                            }
                            let collected = state
                                .collect_named_children_unsorted(
                                    repository.clone(),
                                    dir_a,
                                    false,
                                    false,
                                )
                                .await
                                .map_err(failed)?;
                            if has_repeat(collected.children.iter().map(|child| child.node)) {
                                return Err(format!(
                                    "a collection of children must hold each node once, got \
                                         {:?}",
                                    collected.children
                                ));
                            }
                            let names: Vec<String> =
                                walked.into_iter().map(|(_, name)| name).collect();
                            for i in UNTOUCHED {
                                let name = format!("base-{i}");
                                if !names.contains(&name) {
                                    return Err(format!(
                                        "a walk concurrent with changes must list {name}, \
                                             got {names:?}"
                                    ));
                                }
                                state
                                    .find_subnode(repository.clone(), dir_a, hash_string(&name))
                                    .await
                                    .map_err(failed)?;
                            }
                            child_names(state.clone(), repository.clone(), dir_b)
                                .await
                                .map_err(failed)?;
                        }
                        Ok::<(), String>(())
                    });
                }

                let mut dup_outcomes = Vec::new();
                for result in changes.join_all().await {
                    match result {
                        Ok(Some(outcome)) => dup_outcomes.push(outcome),
                        Ok(None) => {}
                        Err(error) => panic!("round {round}: a change failed: {error:?}"),
                    }
                }
                for result in readers.join_all().await {
                    if let Err(failure) = result {
                        walk_failure.get_or_insert_with(|| format!("round {round}: {failure}"));
                    }
                }

                let attached: Vec<NodeID> = dup_outcomes
                    .iter()
                    .filter_map(|outcome| match outcome {
                        AttachOutcome::Attached(id) => Some(*id),
                        AttachOutcome::Exists(_) => None,
                    })
                    .collect();
                assert_eq!(
                    attached.len(),
                    1,
                    "round {round}: exactly one attach of a name must attach, got \
                         {dup_outcomes:?}"
                );
                assert!(
                    dup_outcomes.iter().all(|outcome| match outcome {
                        AttachOutcome::Attached(id) | AttachOutcome::Exists(id) =>
                            *id == attached[0],
                    }),
                    "round {round}: every refused attach must name the winner, got \
                         {dup_outcomes:?}"
                );

                let mut names_a = child_names(state.clone(), repository.clone(), dir_a)
                    .await
                    .expect("the directory must list");
                names_a.sort();
                let mut expected_a: Vec<String> = UNTOUCHED.map(|i| format!("base-{i}")).collect();
                expected_a.push(format!("renamed-{RENAMED}"));
                expected_a.extend((0..ATTACHED).map(|i| format!("new-{i}")));
                expected_a.push("dup".to_string());
                expected_a.sort();
                assert_eq!(
                    names_a, expected_a,
                    "round {round}: the chain must list every survivor once"
                );

                let mut names_b = child_names(state.clone(), repository.clone(), dir_b)
                    .await
                    .expect("the directory must list");
                names_b.sort();
                let expected_b: Vec<String> = MOVED.map(|i| format!("moved-{i}")).collect();
                assert_eq!(
                    names_b, expected_b,
                    "round {round}: the other chain must list every moved node once"
                );

                for &id in &base[DETACHED] {
                    let node = state
                        .node(repository.clone(), id)
                        .await
                        .expect("a detached node reads back");
                    assert!(node.is_discarded(), "round {round}: {id} must be discarded");
                }
            }
            if let Some(failure) = walk_failure {
                panic!("{failure}");
            }
        }))
        .await
        .expect("Test task failed");
}

/// A mark that dirties the ancestors of a node below a directory, and a clean-only detach
/// of that directory, meet under one block write lock: a detach that lands first makes the
/// mark fail, and a mark that lands first makes the detach refuse.
#[tokio::test]
async fn marking_below_a_detached_directory_fails_and_a_mark_first_keeps_it() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();

            let gone = add(&state, &repository, ROOT_NODE, "gone").await;
            let below = add(&state, &repository, gone, "sub").await;
            assert!(
                state
                    .node_detach(repository.clone(), gone, Detach::IfClean)
                    .await
                    .expect("detach")
            );
            let marked = state
                .node_mark(
                    &repository,
                    below,
                    Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
                )
                .await;
            assert!(
                marked.is_err_and(|error| error.is_node_not_found()),
                "a mark below a detached directory fails"
            );

            let kept = add(&state, &repository, ROOT_NODE, "kept").await;
            let below = add(&state, &repository, kept, "sub").await;
            state
                .node_mark(
                    &repository,
                    below,
                    Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
                )
                .await
                .expect("mark");
            assert!(
                !state
                    .node_detach(repository.clone(), kept, Detach::IfClean)
                    .await
                    .expect("detach"),
                "a directory a mark reached first stays"
            );
            assert!(
                state
                    .find_subnode(repository.clone(), ROOT_NODE, hash_string("kept"))
                    .await
                    .is_ok()
            );
        }))
        .await
        .expect("test task");
}

#[tokio::test]
async fn detach_if_clean_and_detach_if_empty_refuse_a_dirty_or_populated_node() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let held = async |name: &str| {
                state
                    .find_subnode(repository.clone(), ROOT_NODE, hash_string(name))
                    .await
                    .is_ok()
            };

            let clean = add(&state, &repository, ROOT_NODE, "clean").await;
            assert!(
                state
                    .node_detach(repository.clone(), clean, Detach::IfClean)
                    .await
                    .expect("detach")
            );
            assert!(!held("clean").await);

            let dirty = add(&state, &repository, ROOT_NODE, "dirty").await;
            state
                .node_mark(
                    &repository,
                    dirty,
                    Mark::dirty(
                        NodeFlags::DirtyModify,
                        Ext::Set(NodeExtFlags::NoFlags),
                        false,
                    ),
                )
                .await
                .expect("mark");
            assert!(
                !state
                    .node_detach(repository.clone(), dirty, Detach::IfClean)
                    .await
                    .expect("detach")
            );
            assert!(held("dirty").await, "a dirty node stays");

            let full = add(&state, &repository, ROOT_NODE, "full").await;
            add(&state, &repository, full, "child").await;
            assert!(
                !state
                    .node_detach(repository.clone(), full, Detach::IfEmpty)
                    .await
                    .expect("detach")
            );
            assert!(held("full").await, "a directory with a child stays");

            let empty = add(&state, &repository, ROOT_NODE, "empty").await;
            assert!(
                state
                    .node_detach(repository.clone(), empty, Detach::IfEmpty)
                    .await
                    .expect("detach")
            );
            assert!(!held("empty").await);
        }))
        .await
        .expect("test task");
}

#[tokio::test]
async fn modify_dirty_marks_in_the_rewrite_and_modify_if_clean_leaves_a_dirty_node() {
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let address = |marker: u64| lore_base::types::Address {
                hash: lore_base::types::Hash::from_u64(marker),
                context: lore_base::types::Context::default(),
            };
            let local = state
                .node_add(repository.clone(), ROOT_NODE, file("local"), "local")
                .await
                .expect("add");
            state
                .node_rewrite(
                    &repository,
                    local,
                    &Content {
                        mode: 0,
                        size: 5,
                        address: lore_base::types::Address::default(),
                    },
                    Rewrite::Dirty(NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
                )
                .await
                .expect("modify");
            let node = state.node(repository.clone(), local).await.expect("node");
            assert_eq!(node.size, 5);
            assert!(node.is_dirty_modify());
            assert!(node.ext_flags().contains(NodeExtFlags::LocalContent));
            assert!(
                !state
                    .node_rewrite(
                        &repository,
                        local,
                        &Content {
                            mode: 0,
                            size: 9,
                            address: address(3)
                        },
                        Rewrite::IfClean
                    )
                    .await
                    .expect("modify"),
                "a dirty node keeps its content"
            );
            assert_eq!(
                state
                    .node(repository.clone(), local)
                    .await
                    .expect("node")
                    .size,
                5
            );

            let tracked = state
                .node_add(repository.clone(), ROOT_NODE, file("tracked"), "tracked")
                .await
                .expect("add");
            assert!(
                state
                    .node_rewrite(
                        &repository,
                        tracked,
                        &Content {
                            mode: 0,
                            size: 7,
                            address: address(4)
                        },
                        Rewrite::IfClean
                    )
                    .await
                    .expect("modify")
            );
            let node = state.node(repository.clone(), tracked).await.expect("node");
            assert_eq!((node.size, node.address.hash), (7, address(4).hash));
        }))
        .await
        .expect("test task");
}

/// A reattach replacing the node that holds its new name moves the node and discards the
/// holder in one critical section, so every find of the name while it runs finds one of the
/// two, wherever in the chain the moved node and the holder stood: a find reading blocks, and
/// one through blocks in memory only.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_find_during_a_replacing_reattach_always_finds_the_name() {
    const ROUNDS: usize = 1000;
    const READERS: usize = 4;
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let finds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let missed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for round in 0..ROUNDS {
                let state = State::new();
                let dir = add(&state, &repository, ROOT_NODE, "dir").await;
                for i in 0..4 {
                    add(&state, &repository, dir, &format!("before-{i}")).await;
                }
                let holder = state
                    .node_add(repository.clone(), dir, file("b.txt"), "b.txt")
                    .await
                    .expect("add");
                for i in 0..(round % 3) * 8 {
                    add(&state, &repository, dir, &format!("mid-{i}")).await;
                }
                let moved = state
                    .node_add(repository.clone(), dir, file("a.txt"), "a.txt")
                    .await
                    .expect("add");
                for i in 0..(round % 2) * 4 {
                    add(&state, &repository, dir, &format!("after-{i}")).await;
                }
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let started = Arc::new(tokio::sync::Barrier::new(READERS + 1));
                let mut readers = JoinSet::new();
                for reader in 0..READERS {
                    let resident = reader % 2 == 1;
                    let (state, repository) = (state.clone(), repository.clone());
                    let (stop, started) = (stop.clone(), started.clone());
                    let (finds, missed) = (finds.clone(), missed.clone());
                    let _ = lore_spawn!(readers, async move {
                        // Held, so the resident find finds the directory's block in memory.
                        let _block = state
                            .block(repository.clone(), NodeBlock::index(dir))
                            .await
                            .expect("the directory's block");
                        started.wait().await;
                        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                            finds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let found = if resident {
                                state
                                    .find_subnode_resident(dir, hash_string("b.txt"))
                                    .map(|found| found.expect("the directory's block is in memory"))
                            } else {
                                state
                                    .find_subnode(repository.clone(), dir, hash_string("b.txt"))
                                    .await
                            };
                            match found {
                                Ok(_) => {}
                                Err(error) if error.is_node_not_found() => {
                                    missed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                }
                                Err(error) => panic!("find: {error}"),
                            }
                            tokio::task::yield_now().await;
                        }
                    });
                }
                started.wait().await;
                assert!(matches!(
                    state
                        .node_reattach_replacing(repository.clone(), moved, dir, "b.txt", holder)
                        .await
                        .expect("replace"),
                    AttachOutcome::Attached(_)
                ));
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                while readers.join_next().await.is_some() {}
                let names = child_names(state.clone(), repository.clone(), dir)
                    .await
                    .expect("walk");
                assert_eq!(
                    names.iter().filter(|name| *name == "b.txt").count(),
                    1,
                    "round {round}: one b.txt"
                );
                assert!(!names.iter().any(|name| name == "a.txt"), "round {round}");
                assert!(
                    state
                        .node(repository.clone(), holder)
                        .await
                        .expect("holder")
                        .is_discarded()
                );
            }
            let finds = finds.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                missed.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "finds missed the name during a replace, of {finds}"
            );
        }))
        .await
        .expect("test task");
}

/// Attaches only prepend to a chain, so a find that misses a name with only attaches since
/// it read the head answers at once, and one that finds a name held throughout never walks
/// again: a find walks again only after a reattach.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_find_during_steady_creates_never_walks_again() {
    const HELD: usize = 64;
    const CREATES: usize = 4000;
    const READERS: usize = 3;
    runtime()
        .spawn(LORE_CONTEXT.scope(setup_test_execution(), async move {
            let repository = null_repository().await;
            let state = State::new();
            let dir = add(&state, &repository, ROOT_NODE, "dir").await;
            for i in 0..HELD {
                add(&state, &repository, dir, &format!("held-{i}")).await;
            }
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let started = Arc::new(tokio::sync::Barrier::new(READERS + 1));
            let mut readers = JoinSet::new();
            for _ in 0..READERS {
                let (state, repository) = (state.clone(), repository.clone());
                let (stop, started) = (stop.clone(), started.clone());
                let _ = lore_spawn!(readers, async move {
                    let mut failure = None;
                    started.wait().await;
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        let held = state
                            .find_subnode(repository.clone(), dir, hash_string("held-0"))
                            .await;
                        let absent = state
                            .find_subnode(repository.clone(), dir, hash_string("absent"))
                            .await;
                        if let Err(error) = held {
                            failure.get_or_insert_with(|| {
                                format!("a name held throughout is not found: {error:?}")
                            });
                        }
                        if !absent
                            .as_ref()
                            .is_err_and(|error| error.is_node_not_found())
                        {
                            failure.get_or_insert_with(|| {
                                format!("an absent name is not found, got {absent:?}")
                            });
                        }
                        tokio::task::yield_now().await;
                    }
                    failure
                });
            }
            started.wait().await;
            for created in 0..CREATES {
                add(&state, &repository, dir, &format!("created-{created}")).await;
                if created % 8 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let mut failure = None;
            while let Some(joined) = readers.join_next().await {
                if let Some(failed) = joined.expect("reader") {
                    failure.get_or_insert(failed);
                }
            }
            if let Some(failure) = failure {
                panic!("{failure}");
            }
            assert_eq!(
                state.find_restarts(),
                0,
                "a find walked again with only creates since it read the head"
            );
        }))
        .await
        .expect("test task");
}
