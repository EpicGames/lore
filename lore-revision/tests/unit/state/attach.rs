// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the attach family: the record an add or an attach publishes, the names of the
//! slots it leaves outside every chain, and the swap's private builds.

use std::sync::Arc;

use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Detach;
use lore_revision::state::State;
use lore_revision::state::TreeAttach;
use lore_revision::state::TreeBuild;

use super::InMemoryMarker;
use super::add;
use super::directory;
use super::file;
use super::with_repository;
use super::with_state;

/// Saves `state` to the in-memory store, which repacks the name tables of its changed blocks,
/// and reads the saved State back.
async fn save(state: &Arc<State>, repository: &Arc<RepositoryContext>) -> Arc<State> {
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let signature = state
        .serialize(repository.clone(), &token)
        .await
        .expect("the State serializes");
    State::deserialize(repository.clone(), signature)
        .await
        .expect("the State reads back")
}

/// A detached record keeps its name through a save, which repacks its block's name table after
/// a later add stored a name (C3), in the State saved and in the one read back: only a free
/// slot's name goes.
#[tokio::test]
async fn a_detached_node_keeps_its_name_through_a_save() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let detached = add(&state, &repository, d, file("detached"), "detached").await;
        state
            .node_detach(repository.clone(), detached, Detach::Always)
            .await
            .expect("the detach");
        add(&state, &repository, d, file("later"), "later").await;

        let read_back = save(&state, &repository).await;

        for state in [&state, &read_back] {
            assert_eq!(
                state
                    .node_name_clone(repository.clone(), detached)
                    .await
                    .expect("the detached node's name reads"),
                "detached"
            );
        }
    })
    .await;
}

/// A build's root keeps its name through a save while the build is under way, its slot
/// `Discarded` and in no chain, and the build then publishes it under that name.
#[tokio::test]
async fn a_build_keeps_its_roots_name_through_a_save() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        build
            .add(build.root(), &file("f"), "f")
            .await
            .expect("f adds");
        add(&state, &repository, p, file("later"), "later").await;

        let read_back = save(&state, &repository).await;

        let AttachOutcome::Attached(d) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        assert_eq!(
            read_back
                .node_name_clone(repository.clone(), d)
                .await
                .expect("the root's name reads back"),
            "d",
            "the State read back holds the root's name, as the save wrote it in flight"
        );
        let names: Vec<String> = children(&state, &repository, p)
            .await
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["d", "later"], "d is published under its name");
        assert_eq!(
            children(&state, &repository, d).await[0].0,
            "f",
            "f keeps its name"
        );
    })
    .await;
}

/// An add publishes a record of its own: a directory handed in with a stale `child` and
/// `sibling`, as a record read from another State carries them, holds no children and ends its
/// directory's chain where it was first.
#[tokio::test]
async fn an_add_publishes_no_chain_its_record_carries() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let other = add(&state, &repository, ROOT_NODE, directory("other"), "other").await;
        let held = add(&state, &repository, other, file("held"), "held").await;

        let stale = Node {
            child: held,
            sibling: other,
            ..directory("copied")
        };
        let copied = add(&state, &repository, d, stale, "copied").await;

        assert!(
            state
                .node_children(repository.clone(), copied)
                .await
                .expect("the copy's children")
                .is_empty(),
            "the copy holds no children"
        );
        assert_eq!(
            state
                .node_children(repository.clone(), d)
                .await
                .expect("d's children"),
            vec![copied],
            "d holds the copy alone"
        );
    })
    .await;
}

/// An add of a link keeps the `child` it is handed: a link's `child` is the source node in the
/// linked State, not a chain.
#[tokio::test]
async fn an_add_of_a_link_keeps_its_child() {
    with_state(|repository, state| async move {
        let link = Node {
            flags: NodeFlags::Link.bits(),
            child: 42,
            sibling: 7,
            ..directory("link")
        };
        let added = add(&state, &repository, ROOT_NODE, link, "link").await;
        let record = state
            .node(repository.clone(), added)
            .await
            .expect("the link reads");
        assert_eq!(record.child, 42, "the link's source node");
        assert_ne!(record.sibling, 7, "the sibling is the chain's");
    })
    .await;
}

/// A swap's build of `name` below `parent`, begun; panics where the name is held.
async fn begin<'s>(
    state: &'s State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    root: Node,
    name: &str,
) -> TreeBuild<'s> {
    match state
        .swap()
        .node_attach_tree(repository.clone(), parent, &root, name)
        .await
        .expect("the build begins")
    {
        TreeAttach::Building(build) => build,
        TreeAttach::Exists(holder) => panic!("'{name}' is held by {holder}"),
    }
}

/// The Discarded slots of block `index` of `state` that are not free.
fn detached_in(state: &State, index: usize) -> usize {
    state.resident_block(index).map_or(0, |block| {
        let reader = block.read();
        let data = reader.node_block();
        let discarded = data.node[..data.node_count as usize]
            .iter()
            .filter(|node| node.is_discarded())
            .count();
        discarded - data.node_unused_count as usize
    })
}

/// The free slots of every block of `state` in memory.
fn free_slots(state: &State) -> u32 {
    (0..state.block_count())
        .filter_map(|index| state.resident_block(index))
        .map(|block| block.read().node_block().node_unused_count)
        .sum()
}

/// The ids of the live children of `dir`, by name.
async fn children(
    state: &State,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
) -> Vec<(String, NodeID)> {
    let mut named = Vec::new();
    for child in state
        .node_children(repository.clone(), dir)
        .await
        .expect("the children")
    {
        let name = state
            .node_name_clone(repository.clone(), child)
            .await
            .expect("the child's name");
        named.push((name, child));
    }
    named.sort();
    named
}

/// A build's root stays out of its parent, and `Discarded`, until its publish, which publishes
/// it with every node the build added below it, each in the chain of the directory it went into.
#[tokio::test]
async fn a_build_publishes_its_tree_whole() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let detached = detached_in(&state, 0);
        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        let root = build.root();
        let sub = build
            .add(root, &directory("sub"), "sub")
            .await
            .expect("sub adds");
        build
            .add(sub, &file("deep"), "deep")
            .await
            .expect("deep adds");
        build.add(root, &file("f"), "f").await.expect("f adds");
        assert!(
            children(&state, &repository, p).await.is_empty(),
            "nothing stands in p before the publish"
        );
        assert_eq!(
            detached_in(&state, 0),
            detached + 1,
            "the root alone is Discarded, out of every chain, before the publish"
        );

        let AttachOutcome::Attached(d) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        assert_eq!(
            children(&state, &repository, p).await,
            vec![("d".to_string(), d)]
        );
        let below = children(&state, &repository, d).await;
        assert_eq!(
            below
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["f", "sub"]
        );
        let deep = children(&state, &repository, below[1].1).await;
        assert_eq!(deep.len(), 1, "sub holds deep");
        for node in [d, below[0].1, below[1].1, deep[0].1] {
            let record = state
                .node(repository.clone(), node)
                .await
                .expect("the copy's node");
            assert!(!record.is_discarded(), "node {node} is published");
            assert!(!record.is_dirty(), "node {node} is a clean copy");
        }
    })
    .await;
}

/// An abandoned build frees every node it took, its root included, and touches no chain.
#[tokio::test]
async fn an_abandoned_build_frees_every_node_it_took() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let kept = add(&state, &repository, p, file("kept"), "kept").await;
        let free = free_slots(&state);
        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        let root = build.root();
        let sub = build
            .add(root, &directory("sub"), "sub")
            .await
            .expect("sub adds");
        build
            .add(sub, &file("deep"), "deep")
            .await
            .expect("deep adds");
        build.add(root, &file("f"), "f").await.expect("f adds");

        build.abandon().await;
        assert_eq!(
            free_slots(&state),
            free + 4,
            "the root and its three nodes are free"
        );
        assert_eq!(
            children(&state, &repository, p).await,
            vec![("kept".to_string(), kept)]
        );
    })
    .await;
}

/// A build whose name a live child took after the lookup answers that child at its publish,
/// and frees every node it took.
#[tokio::test]
async fn a_build_finding_its_name_taken_at_its_publish_answers_the_holder_and_frees_itself() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let free = free_slots(&state);
        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        build
            .add(build.root(), &file("f"), "f")
            .await
            .expect("f adds");
        let holder = add(&state, &repository, p, directory("d"), "d").await;

        assert_eq!(
            build.publish().await.expect("the publish"),
            AttachOutcome::Exists(holder)
        );
        assert_eq!(
            children(&state, &repository, p).await,
            vec![("d".to_string(), holder)]
        );
        assert_eq!(
            free_slots(&state),
            free + 2,
            "the build's two nodes are free"
        );
    })
    .await;
}

/// A build whose parent's chain changed after the lookup, by a sibling of another name, looks
/// the name up again and publishes.
#[tokio::test]
async fn a_build_publishes_after_its_parents_chain_moved() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let build = begin(&state, &repository, p, directory("d"), "d").await;
        let other = add(&state, &repository, p, file("other"), "other").await;
        let retries = state.attach_retries();

        let AttachOutcome::Attached(d) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        assert_eq!(state.attach_retries(), retries + 1, "one more lookup");
        assert_eq!(
            children(&state, &repository, p).await,
            vec![("d".to_string(), d), ("other".to_string(), other)]
        );
    })
    .await;
}

/// A build whose parent was detached after the lookup answers `NodeNotFound` at its publish and
/// frees every node it took; one begun below a detached parent takes none.
#[tokio::test]
async fn a_build_below_a_detached_parent_answers_node_not_found_and_frees_itself() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let free = free_slots(&state);
        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        build
            .add(build.root(), &file("f"), "f")
            .await
            .expect("f adds");
        state
            .node_detach(repository.clone(), p, Detach::Always)
            .await
            .expect("the detach");

        let published = build.publish().await;
        assert!(
            published
                .as_ref()
                .is_err_and(|error| error.is_node_not_found()),
            "the publish finds its parent gone: {published:?}"
        );
        assert_eq!(
            free_slots(&state),
            free + 2,
            "the build's two nodes are free"
        );

        let begun = state
            .swap()
            .node_attach_tree(repository.clone(), p, &directory("e"), "e")
            .await;
        assert!(
            begun.as_ref().is_err_and(|error| error.is_node_not_found()),
            "a build below a detached parent does not begin"
        );
        assert_eq!(free_slots(&state), free + 2, "and takes no slot");
    })
    .await;
}

/// A build of a name a live child holds builds nothing and answers that child.
#[tokio::test]
async fn a_build_of_a_held_name_takes_no_slot() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let holder = add(&state, &repository, p, file("d"), "d").await;
        let count = state
            .block(repository.clone(), NodeBlock::index(holder))
            .await
            .expect("the block")
            .read()
            .node_count();

        let begun = state
            .swap()
            .node_attach_tree(repository.clone(), p, &directory("D"), "D")
            .await
            .expect("the lookup");
        assert!(matches!(begun, TreeAttach::Exists(found) if found == holder));
        assert_eq!(
            state
                .block(repository.clone(), NodeBlock::index(holder))
                .await
                .expect("the block")
                .read()
                .node_count(),
            count,
            "no slot taken"
        );
    })
    .await;
}

/// A build of a file takes nothing below it: an add into it is refused and gives its slot back.
#[tokio::test]
async fn a_build_adds_only_into_its_directories() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let mut build = begin(&state, &repository, p, file("f"), "f").await;
        let free = free_slots(&state);

        let added = build.add(build.root(), &file("g"), "g").await;
        assert!(
            added
                .as_ref()
                .is_err_and(|error| error.is_invalid_arguments()),
            "an add into a file is refused: {added:?}"
        );
        assert_eq!(
            free_slots(&state),
            free + 1,
            "the refused add's slot is free"
        );
        let AttachOutcome::Attached(f) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        assert_eq!(
            children(&state, &repository, p).await,
            vec![("f".to_string(), f)]
        );
    })
    .await;
}

/// A directory `p` below the root, filled until block 0 has one slot left, which the next slot
/// taken takes.
async fn nearly_full(state: &State, repository: &Arc<RepositoryContext>) -> NodeID {
    let p = add(state, repository, ROOT_NODE, directory("p"), "p").await;
    let mut index = 0;
    while Node::index(index) < BLOCK_NODE_COUNT - 2 {
        let name = format!("fill{index}");
        index = add(state, repository, p, file(&name), &name).await;
    }
    p
}

/// A build whose root takes the last slot of a block and whose nodes take the next block's
/// publishes them across both blocks: the root's adds lock two blocks, the nodes' below a
/// directory of the second block one, and an add into the root after one into that directory
/// locks the root's block again.
#[tokio::test]
async fn a_build_across_two_blocks_publishes_whole() {
    with_state(|repository, state| async move {
        let p = nearly_full(&state, &repository).await;

        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        let root = build.root();
        let sub = build
            .add(root, &directory("sub"), "sub")
            .await
            .expect("sub adds");
        build
            .add(sub, &file("deep"), "deep")
            .await
            .expect("deep adds");
        build.add(root, &file("g"), "g").await.expect("g adds");
        let AttachOutcome::Attached(d) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        assert_eq!(NodeBlock::index(d), 0, "the root takes block 0's last slot");
        let below = children(&state, &repository, d).await;
        let names: Vec<&str> = below.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["g", "sub"], "d holds g and sub");
        assert!(
            below.iter().all(|&(_, node)| NodeBlock::index(node) == 1),
            "g and sub lie in the next block"
        );
        let deeper = children(&state, &repository, below[1].1).await;
        assert_eq!(deeper.len(), 1, "sub holds deep");
        assert_eq!(
            NodeBlock::index(deeper[0].1),
            1,
            "deep lies in the next block"
        );
    })
    .await;
}

/// An abandoned build whose root lies in one block and its nodes in the next frees them across
/// both.
#[tokio::test]
async fn a_build_across_two_blocks_frees_whole() {
    with_state(|repository, state| async move {
        let p = nearly_full(&state, &repository).await;

        let mut build = begin(&state, &repository, p, directory("e"), "e").await;
        let sub = build
            .add(build.root(), &directory("sub"), "sub")
            .await
            .expect("sub adds");
        build
            .add(sub, &file("deep"), "deep")
            .await
            .expect("deep adds");
        let used = [used_in(&state, 0), used_in(&state, 1)];
        build.abandon().await;
        assert_eq!(
            [used_in(&state, 0), used_in(&state, 1)],
            [used[0] - 1, used[1] - 2],
            "the root is freed in block 0, sub and deep in block 1"
        );
    })
    .await;
}

/// The slots of block `index` of `state` in use: taken and not free.
fn used_in(state: &State, index: usize) -> u32 {
    state.resident_block(index).map_or(0, |block| {
        let reader = block.read();
        let data = reader.node_block();
        data.node_count - data.node_unused_count
    })
}

/// A build adding nodes of a third block into two directories of two blocks, neither the root's,
/// one after the other and back, then into the root, adds each node into the directory it names.
#[tokio::test]
async fn a_build_adding_into_directories_of_two_other_blocks_adds_into_each() {
    with_state(|repository, state| async move {
        let p = nearly_full(&state, &repository).await;

        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        let root = build.root();
        let one = build
            .add(root, &directory("one"), "one")
            .await
            .expect("one adds");
        // The rest of block 1, so the next directory takes block 2.
        for index in 0..BLOCK_NODE_COUNT - 1 {
            let name = format!("f{index}");
            build
                .add(root, &file(&name), &name)
                .await
                .expect("a filler adds");
        }
        let two = build
            .add(root, &directory("two"), "two")
            .await
            .expect("two adds");
        // The rest of block 2, so the adds below take block 3, apart from both directories'.
        for index in 0..BLOCK_NODE_COUNT - 1 {
            let name = format!("g{index}");
            build
                .add(root, &file(&name), &name)
                .await
                .expect("a filler adds");
        }
        for name in ["a", "b", "c"] {
            build
                .add(one, &file(name), name)
                .await
                .expect("an add into one");
            let name = name.to_uppercase();
            build
                .add(two, &file(&name), &name)
                .await
                .expect("an add into two");
        }
        build
            .add(root, &file("last"), "last")
            .await
            .expect("an add into the root after them");
        let AttachOutcome::Attached(d) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        let below = children(&state, &repository, d).await;
        let find = |name: &str| {
            below
                .iter()
                .find(|(held, _)| held == name)
                .map_or_else(|| panic!("d holds {name}"), |&(_, node)| node)
        };
        let (one, two) = (find("one"), find("two"));
        assert_eq!(
            (NodeBlock::index(one), NodeBlock::index(two)),
            (1, 2),
            "the directories lie in blocks 1 and 2"
        );
        let names = |held: Vec<(String, NodeID)>| -> Vec<String> {
            held.into_iter().map(|(name, _)| name).collect()
        };
        for (dir, held) in [(one, ["a", "b", "c"]), (two, ["A", "B", "C"])] {
            let below = children(&state, &repository, dir).await;
            assert!(
                below.iter().all(|&(_, node)| NodeBlock::index(node) == 3),
                "the adds lie in block 3"
            );
            assert_eq!(names(below), held);
        }
        assert!(
            below.iter().any(|(name, _)| name == "last"),
            "d holds the last add"
        );
    })
    .await;
}

/// A node of one build is refused by another, which adds nothing.
#[tokio::test]
async fn a_build_refuses_a_node_of_another_build() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let first = begin(&state, &repository, p, directory("a"), "a").await;
        let mut second = begin(&state, &repository, p, directory("b"), "b").await;
        let added = second.add(first.root(), &file("f"), "f").await;
        assert!(
            added
                .as_ref()
                .is_err_and(|error| error.is_invalid_arguments()),
            "a node of another build is refused: {added:?}"
        );
        first.abandon().await;
        second.abandon().await;
    })
    .await;
}

/// `dir`'s chain log installed, as a listing of it installs one.
async fn watch(state: &State, repository: &Arc<RepositoryContext>, dir: NodeID) {
    state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the directory's block")
        .watch_chain(Node::index(dir), None);
}

/// A build below a listed directory, whose chain changed under another name after the lookup,
/// publishes against the directory's log since the lookup, without looking the name up again.
#[tokio::test]
async fn a_build_below_a_listed_directory_publishes_past_other_names_without_a_lookup() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        watch(&state, &repository, p).await;
        let build = begin(&state, &repository, p, directory("d"), "d").await;
        let other = add(&state, &repository, p, file("other"), "other").await;
        let retries = state.attach_retries();

        let AttachOutcome::Attached(d) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        assert_eq!(state.attach_retries(), retries, "no lookup again");
        assert_eq!(
            children(&state, &repository, p).await,
            vec![("d".to_string(), d), ("other".to_string(), other)]
        );
    })
    .await;
}

/// A build below a listed directory whose name a sibling took after the lookup finds it held in
/// the directory's log, answers it without looking it up again, and frees what it built.
#[tokio::test]
async fn a_build_below_a_listed_directory_finds_its_name_held_in_the_log() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        watch(&state, &repository, p).await;
        let free = free_slots(&state);
        let mut build = begin(&state, &repository, p, directory("d"), "d").await;
        build
            .add(build.root(), &file("f"), "f")
            .await
            .expect("f adds");
        let holder = add(&state, &repository, p, directory("d"), "d").await;
        let retries = state.attach_retries();

        assert_eq!(
            build.publish().await.expect("the publish"),
            AttachOutcome::Exists(holder)
        );
        assert_eq!(state.attach_retries(), retries, "no lookup again");
        assert_eq!(
            free_slots(&state),
            free + 2,
            "the build's two nodes are free"
        );
    })
    .await;
}

/// A build below a listed directory whose log no longer holds every change since the lookup, more
/// of them landing than it keeps, looks its name up again once and publishes.
#[tokio::test]
async fn a_build_below_a_listed_directory_whose_log_overflowed_looks_its_name_up_again() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        watch(&state, &repository, p).await;
        let build = begin(&state, &repository, p, directory("d"), "d").await;
        for index in 0..=CHAIN_LOG_INITIAL {
            let name = format!("other{index}");
            add(&state, &repository, p, file(&name), &name).await;
        }
        let retries = state.attach_retries();

        let AttachOutcome::Attached(d) = build.publish().await.expect("the publish") else {
            panic!("the name is free");
        };
        assert_eq!(state.attach_retries(), retries + 1, "one lookup again");
        assert!(
            children(&state, &repository, p)
                .await
                .contains(&("d".to_string(), d))
        );
    })
    .await;
}

/// A directory `d` holding `old` then `head`, and what a lookup of `x` that missed there saw:
/// `d`'s block, its head and its chain change count.
async fn missed_x(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
) -> (NodeID, NodeID, Arc<NodeBlock>, (Option<NodeID>, u32)) {
    let d = add(state, repository, ROOT_NODE, directory("d"), "d").await;
    let old = add(state, repository, d, file("old"), "old").await;
    let head = add(state, repository, d, file("head"), "head").await;
    let block = state
        .resident_block(NodeBlock::index(d))
        .expect("d's block is in memory");
    let (parent, changes) = block.node_with_chain_changes(Node::index(d));
    assert_eq!(parent.child(), Some(head));
    (d, old, block, (parent.child(), changes))
}

/// A retry's lookup after a miss walks what was prepended since: it walks no node where nothing
/// was, finds a node of the name prepended there in one step, and answers the miss again where
/// nothing of the name was. Where a move's weight landed, a rename in place behind the head it
/// saw, it walks the whole chain and finds the renamed node; where that head was detached, it
/// walks the whole chain too, finding a node of the name behind it.
#[tokio::test]
async fn a_lookup_after_a_miss_walks_what_was_prepended_since() {
    use lore_revision::state::step_counting;
    step_counting::set_chain_step_counting(true);
    with_repository(|repository| async move {
        let x = lore_storage::hash::hash_string("x");

        let state = State::new();
        let (d, _, block, since) = missed_x(&state, &repository).await;
        let walked = step_counting::thread_walk_steps();
        let missed = state
            .find_subnode_since(repository.clone(), d, &block, since, x)
            .await
            .expect_err("nothing of the name");
        assert!(missed.is_node_not_found(), "{missed}");
        assert_eq!(
            step_counting::thread_walk_steps(),
            walked,
            "nothing prepended"
        );
        let prepended = add(&state, &repository, d, file("x"), "x").await;
        let found = state
            .find_subnode_since(repository.clone(), d, &block, since, x)
            .await
            .expect("the node prepended since");
        assert_eq!(found, prepended);
        assert_eq!(
            step_counting::thread_walk_steps(),
            walked + 1,
            "the one node prepended since"
        );

        let state = State::new();
        let (d, old, block, since) = missed_x(&state, &repository).await;
        state
            .node_rename(
                repository.clone(),
                old,
                lore_revision::state::NameChange::Rename("x"),
            )
            .await
            .expect("renaming old to x in place");
        let found = state
            .find_subnode_since(repository.clone(), d, &block, since, x)
            .await
            .expect("the node renamed behind the head");
        assert_eq!(found, old);

        let state = State::new();
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let behind = add(&state, &repository, d, file("x"), "x").await;
        let head = add(&state, &repository, d, file("head"), "head").await;
        let block = state
            .resident_block(NodeBlock::index(d))
            .expect("d's block is in memory");
        let (parent, changes) = block.node_with_chain_changes(Node::index(d));
        assert!(
            state
                .node_detach(repository.clone(), head, Detach::Always)
                .await
                .expect("detaching the head")
        );
        let found = state
            .find_subnode_since(repository.clone(), d, &block, (parent.child(), changes), x)
            .await
            .expect("the whole chain walked");
        assert_eq!(found, behind);
    })
    .await;
}
