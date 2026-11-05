// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for the discard family under an exclusive claim: the fused discard of a subtree, and a
//! commit's walk then free.

use std::collections::BTreeSet;
use std::sync::Arc;

use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Detach;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use super::add;
use super::directory;
use super::file;
use super::fill;
use super::reload;
use super::with_state;

/// A tree of `d/a`, `d/e/f` and `d/e/g` beside `keep`, all in block 0: `d`, then the nodes below
/// it, then `keep`.
struct Subtree {
    d: NodeID,
    below: [NodeID; 4],
    keep: NodeID,
}

async fn subtree(state: &State, repository: &Arc<RepositoryContext>) -> Subtree {
    let d = add(state, repository, ROOT_NODE, directory("d"), "d").await;
    let a = add(state, repository, d, file("a"), "a").await;
    let e = add(state, repository, d, directory("e"), "e").await;
    let f = add(state, repository, e, file("f"), "f").await;
    let g = add(state, repository, e, file("g"), "g").await;
    let keep = add(state, repository, ROOT_NODE, file("keep"), "keep").await;
    Subtree {
        d,
        below: [a, e, f, g],
        keep,
    }
}

/// The slots on block 0's free list, in list order.
async fn free_list(state: &State, repository: &Arc<RepositoryContext>) -> Vec<usize> {
    let block = state.block(repository.clone(), 0).await.expect("block 0");
    let reader = block.read();
    let data = reader.node_block();
    let mut free = Vec::new();
    let mut at = data.node_unused as usize;
    for _ in 0..data.node_unused_count {
        free.push(at);
        at = data.node[at].sibling as usize;
    }
    free
}

/// The slots on block `index`'s free list, in list order, asserting none is on it twice.
async fn free_list_once(
    state: &State,
    repository: &Arc<RepositoryContext>,
    index: usize,
) -> Vec<usize> {
    let block = state
        .block(repository.clone(), index)
        .await
        .expect("the block");
    let reader = block.read();
    let data = reader.node_block();
    let mut free = Vec::new();
    let mut seen = BTreeSet::new();
    let mut at = data.node_unused as usize;
    for _ in 0..data.node_unused_count {
        assert!(
            seen.insert(at),
            "slot {at} of block {index} is on its free list twice"
        );
        free.push(at);
        at = data.node[at].sibling as usize;
    }
    free
}

/// The ids the root's chain reaches.
async fn root_children(state: &Arc<State>, repository: &Arc<RepositoryContext>) -> Vec<NodeID> {
    state
        .node_children(repository.clone(), ROOT_NODE)
        .await
        .expect("the root's children")
}

/// A fused discard unlinks the directory and frees it with everything below it, the root's other
/// child untouched.
#[tokio::test]
async fn a_fused_discard_frees_a_whole_subtree() {
    with_state(|repository, state| async move {
        let tree = subtree(&state, &repository).await;

        let counts =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), tree.d)
                .await
                .expect("the discard");

        assert_eq!((counts.file_count, counts.directory_count), (3, 2));
        let free: BTreeSet<usize> = free_list(&state, &repository).await.into_iter().collect();
        let freed: BTreeSet<usize> = std::iter::once(tree.d)
            .chain(tree.below)
            .map(Node::index)
            .collect();
        assert_eq!(
            free, freed,
            "every node of the subtree is free, and only those"
        );
        assert_eq!(root_children(&state, &repository).await, vec![tree.keep]);
    })
    .await;
}

/// A link is a leaf: its child names a node of the linked State, which a discard of the link
/// never frees, though a node of this State holds the same id.
#[tokio::test]
async fn a_fused_discard_of_a_link_frees_only_the_link() {
    with_state(|repository, state| async move {
        let live = add(&state, &repository, ROOT_NODE, file("live"), "live").await;
        let link_node = Node {
            flags: NodeFlags::Link.bits(),
            name_hash: hash_string("link"),
            ..Default::default()
        };
        let link = add(&state, &repository, ROOT_NODE, link_node, "link").await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        state.node_edit_for_tests(&block, link, |node| node.child = live);

        let counts =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), link)
                .await
                .expect("the discard");

        assert_eq!((counts.file_count, counts.directory_count), (0, 1));
        assert_eq!(
            free_list(&state, &repository).await,
            vec![Node::index(link)]
        );
        let record = state
            .node(repository.clone(), live)
            .await
            .expect("the live file reads");
        assert!(
            !record.is_discarded(),
            "the file the link's child names stays"
        );
    })
    .await;
}

/// A root already detached is skipped, whatever hangs below it.
#[tokio::test]
async fn a_fused_discard_skips_a_root_already_discarded() {
    with_state(|repository, state| async move {
        let tree = subtree(&state, &repository).await;
        state
            .node_detach(repository.clone(), tree.d, Detach::Always)
            .await
            .expect("the detach");

        let counts =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), tree.d)
                .await
                .expect("the discard");

        assert_eq!((counts.file_count, counts.directory_count), (0, 0));
        assert!(free_list(&state, &repository).await.is_empty());
    })
    .await;
}

/// A walk reports each node below its root once, with its flags, and not the root.
#[tokio::test]
async fn a_walk_reports_each_node_below_its_root_once() {
    with_state(|repository, state| async move {
        let tree = subtree(&state, &repository).await;
        let walked = Arc::new(std::sync::Mutex::new(Vec::new()));

        let sink = walked.clone();
        lore_revision::state::node_discard_walk(
            state.clone(),
            repository.clone(),
            tree.d,
            move |id, flags| sink.lock().expect("the walked nodes").push((id, flags)),
        )
        .await
        .expect("the walk");

        let mut walked = walked.lock().expect("the walked nodes").clone();
        walked.sort_unstable();
        let mut expected: Vec<(NodeID, u16)> = Vec::new();
        for id in tree.below {
            let flags = state
                .node(repository.clone(), id)
                .await
                .expect("a node below reads")
                .flags;
            expected.push((id, flags));
        }
        expected.sort_unstable();
        assert_eq!(walked, expected);
        assert!(
            free_list(&state, &repository).await.is_empty(),
            "a walk frees nothing"
        );
    })
    .await;
}

/// A commit's walk then free leaves what a fused discard of the same subtree does, with its
/// roots told to the caller.
#[tokio::test]
async fn a_walk_then_free_matches_a_fused_discard() {
    with_state(|repository, fused| async move {
        let tree = subtree(&fused, &repository).await;
        let fused_counts =
            lore_revision::state::node_discard_fused(fused.clone(), repository.clone(), tree.d)
                .await
                .expect("the fused discard");

        let state = State::new();
        let tree = subtree(&state, &repository).await;
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = collected.clone();
        lore_revision::state::node_discard_walk(
            state.clone(),
            repository.clone(),
            tree.d,
            move |id, _| {
                sink.lock().expect("the collected nodes").push(id);
            },
        )
        .await
        .expect("the walk");
        let collected = std::mem::take(&mut *collected.lock().expect("the collected nodes"));
        let mut told = Vec::new();
        let counts = lore_revision::state::node_discard_free(
            &state,
            &repository,
            &[tree.d],
            collected,
            |root, flags| {
                told.push((root, flags));
            },
        )
        .await
        .expect("the free");

        assert_eq!(
            (counts.file_count, counts.directory_count),
            (fused_counts.file_count, fused_counts.directory_count)
        );
        assert_eq!(told, vec![(tree.d, NodeFlags::NoFlags.bits())]);
        let free: BTreeSet<usize> = free_list(&state, &repository).await.into_iter().collect();
        let fused_free: BTreeSet<usize> =
            free_list(&fused, &repository).await.into_iter().collect();
        assert_eq!(free, fused_free);
        assert_eq!(root_children(&state, &repository).await, vec![tree.keep]);
    })
    .await;
}

/// A free skips a slot already free, and one named twice: each slot goes on the free list once.
#[tokio::test]
async fn a_free_skips_a_slot_already_free() {
    with_state(|repository, state| async move {
        let tree = subtree(&state, &repository).await;
        let gone = add(&state, &repository, ROOT_NODE, file("gone"), "gone").await;
        lore_revision::state::node_discard_fused(state.clone(), repository.clone(), gone)
            .await
            .expect("the earlier discard");
        let [a, e, f, g] = tree.below;

        let counts = lore_revision::state::node_discard_free(
            &state,
            &repository,
            &[tree.d],
            vec![g, a, f, gone, e, a],
            |_, _| {},
        )
        .await
        .expect("the free");

        assert_eq!((counts.file_count, counts.directory_count), (3, 2));
        let free = free_list(&state, &repository).await;
        let distinct: BTreeSet<usize> = free.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            free.len(),
            "no slot is on the free list twice"
        );
        assert_eq!(
            free.len(),
            6,
            "the subtree's five slots and the earlier one"
        );
    })
    .await;
}

/// Files in each wide directory of [`wide`], enough to fill block 0 and reach into block 1.
const WIDE: usize = BLOCK_NODE_COUNT + 40;

/// Three roots: `big`, a directory of [`WIDE`] files reaching from block 0 into block 1, so its
/// slots run past every word of a block's free bitmap; `small/s`; and the file `lone`; beside
/// `keep`, added last. Answers the roots and every node below them.
async fn wide(
    state: &State,
    repository: &Arc<RepositoryContext>,
) -> ([NodeID; 3], Vec<NodeID>, NodeID) {
    let big = add(state, repository, ROOT_NODE, directory("big"), "big").await;
    let mut below = Vec::with_capacity(WIDE + 1);
    for index in 0..WIDE {
        let name = format!("f{index}");
        below.push(add(state, repository, big, file(&name), &name).await);
    }
    let small = add(state, repository, ROOT_NODE, directory("small"), "small").await;
    below.push(add(state, repository, small, file("s"), "s").await);
    let lone = add(state, repository, ROOT_NODE, file("lone"), "lone").await;
    let keep = add(state, repository, ROOT_NODE, file("keep"), "keep").await;
    ([big, small, lone], below, keep)
}

/// Every slot of every block that holds no node: on its block's free list, or at or past its
/// node count, as a block whose last node is freed starts over.
async fn all_free(state: &State, repository: &Arc<RepositoryContext>) -> BTreeSet<NodeID> {
    let id = |block_index: usize, index: usize| {
        NodeID::try_from(block_index * BLOCK_NODE_COUNT + index).expect("a node id")
    };
    let mut free = BTreeSet::new();
    for block_index in 0..state.block_count() {
        let block = state
            .block(repository.clone(), block_index)
            .await
            .expect("the block");
        let reader = block.read();
        let data = reader.node_block();
        let mut at = data.node_unused as usize;
        for _ in 0..data.node_unused_count {
            free.insert(id(block_index, at));
            at = data.node[at].sibling as usize;
        }
        free.extend(
            (data.node_count as usize..BLOCK_NODE_COUNT).map(|index| id(block_index, index)),
        );
    }
    free
}

/// A commit's free of several roots, a directory, its nodes over two blocks, a small directory
/// and a file, frees every slot below each, past every word of each block's free bitmap, and
/// tells the caller each root once; a slot freed already, or named twice, past the bitmap's
/// first word, is freed once.
#[tokio::test]
async fn a_free_of_several_roots_over_two_blocks_frees_every_slot() {
    with_state(|repository, state| async move {
        let (roots, mut below, keep) = wide(&state, &repository).await;
        assert!(state.block_count() >= 2, "the tree reaches into block 1");
        let gone = below.remove(200);
        assert!(Node::index(gone) >= 64, "past the bitmap's first word");
        lore_revision::state::node_discard_fused(state.clone(), repository.clone(), gone)
            .await
            .expect("the earlier discard");
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        for root in roots {
            let sink = collected.clone();
            lore_revision::state::node_discard_walk(
                state.clone(),
                repository.clone(),
                root,
                move |id, _| {
                    sink.lock().expect("the collected nodes").push(id);
                },
            )
            .await
            .expect("the walk");
        }
        let mut collected = std::mem::take(&mut *collected.lock().expect("the collected nodes"));
        assert_eq!(
            collected.len(),
            below.len(),
            "the walks reach every node below the roots"
        );
        collected.extend([gone, below[300]]);
        let before = all_free(&state, &repository).await;
        let mut told = Vec::new();

        let counts = lore_revision::state::node_discard_free(
            &state,
            &repository,
            &roots,
            collected,
            |root, _| told.push(root),
        )
        .await
        .expect("the free");

        assert_eq!(told, roots.to_vec());
        assert_eq!(
            (counts.file_count, counts.directory_count),
            (WIDE as u64 + 1, 2),
            "each freed once, the one freed already not at all"
        );
        let freed: BTreeSet<NodeID> = roots.into_iter().chain(below).collect();
        let after = all_free(&state, &repository).await;
        assert!(before.is_disjoint(&freed), "every node freed was live");
        assert_eq!(
            &after - &before,
            freed,
            "every slot below the roots is freed, and only those"
        );
        assert_eq!(root_children(&state, &repository).await, vec![keep]);
    })
    .await;
}

/// A fused discard of a State loaded from the store, none of its blocks in memory, loads them
/// as it goes and frees the whole subtree over both blocks.
#[tokio::test]
async fn a_cold_fused_discard_frees_a_subtree_over_two_blocks() {
    with_state(|repository, written| async move {
        let ([big, small, lone], below, _) = wide(&written, &repository).await;
        let state = reload(&written, &repository).await;
        assert!(
            state.resident_block(1).is_none(),
            "block 1 is not in memory"
        );
        assert!(!state.is_dirty(), "a State read back has nothing to save");
        let before = all_free(&written, &repository).await;

        let counts =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), big)
                .await
                .expect("the discard");

        assert_eq!(
            (counts.file_count, counts.directory_count),
            (WIDE as u64, 1)
        );
        let freed: BTreeSet<NodeID> = std::iter::once(big)
            .chain(below[..WIDE].iter().copied())
            .collect();
        let after = all_free(&state, &repository).await;
        assert!(before.is_disjoint(&freed), "every node freed was live");
        assert_eq!(
            &after - &before,
            freed,
            "every slot of the subtree is freed, and only those"
        );
        let mut left = root_children(&state, &repository).await;
        left.sort_unstable();
        assert!(left.contains(&small) && left.contains(&lone) && !left.contains(&big));
        assert!(
            state.is_dirty() && state.block_recorded_changed(0) && state.block_recorded_changed(1),
            "the discard leaves the State and both blocks to save"
        );
    })
    .await;
}

/// A fused discard registers each block its frees turn dirty, so a save writes them: here block
/// 1, loaded clean and held in memory, holding only a file deep in the subtree (C14).
#[tokio::test]
async fn a_fused_discard_registers_the_clean_blocks_it_frees() {
    with_state(|repository, written| async move {
        let p = add(&written, &repository, ROOT_NODE, directory("p"), "p").await;
        let d = add(&written, &repository, p, directory("d"), "d").await;
        let sub = add(&written, &repository, d, directory("sub"), "sub").await;
        add(&written, &repository, sub, file("z"), "z").await;
        add(&written, &repository, p, file("keep"), "keep").await;
        let filled = add(&written, &repository, ROOT_NODE, directory("fill"), "fill").await;
        fill(&written, &repository, filled, 0).await;
        let w = add(&written, &repository, sub, file("w"), "w").await;
        assert_eq!(NodeBlock::index(w), 1, "'w' lies in block 1");
        let state = reload(&written, &repository).await;
        let _held = state.block(repository.clone(), 1).await.expect("block 1");
        assert!(
            !state.is_dirty() && !state.block_recorded_changed(1),
            "block 1 is loaded clean"
        );

        lore_revision::state::node_discard_fused(state.clone(), repository.clone(), d)
            .await
            .expect("the discard");

        assert!(
            state.is_dirty() && state.block_recorded_changed(1),
            "the block the frees changed is registered"
        );
    })
    .await;
}

/// A free whose root another discard freed already skips it: nothing is unlinked or freed again,
/// its caller is not told, and the counts hold the rest.
#[tokio::test]
async fn a_free_skips_a_root_freed_already() {
    with_state(|repository, state| async move {
        let tree = subtree(&state, &repository).await;
        let gone = add(&state, &repository, ROOT_NODE, file("gone"), "gone").await;
        lore_revision::state::node_discard_fused(state.clone(), repository.clone(), gone)
            .await
            .expect("the earlier discard");
        let before = free_list(&state, &repository).await;
        let mut told = Vec::new();

        let counts = lore_revision::state::node_discard_free(
            &state,
            &repository,
            &[gone, tree.keep],
            Vec::new(),
            |root, _| told.push(root),
        )
        .await
        .expect("the free");

        assert_eq!(told, vec![tree.keep], "only the live root is told");
        assert_eq!((counts.file_count, counts.directory_count), (1, 0));
        let free = free_list(&state, &repository).await;
        assert_eq!(free.len(), before.len() + 1, "only the live root is freed");
        assert_eq!(root_children(&state, &repository).await, vec![tree.d]);
    })
    .await;
}

/// A free naming a slot past its block's node count, as a block whose last node was freed starts
/// over, skips it: the block stays as it was.
#[tokio::test]
async fn a_free_skips_a_slot_past_its_blocks_node_count() {
    with_state(|repository, state| async move {
        add(&state, &repository, ROOT_NODE, file("keep"), "keep").await;
        let big = add(&state, &repository, ROOT_NODE, directory("big"), "big").await;
        let mut far = None;
        for index in 0..BLOCK_NODE_COUNT + 10 {
            let name = format!("f{index}");
            let id = add(&state, &repository, big, file(&name), &name).await;
            if NodeBlock::index(id) == 1 {
                far.get_or_insert(id);
            }
        }
        let far = far.expect("the files reach into block 1");
        lore_revision::state::node_discard_fused(state.clone(), repository.clone(), big)
            .await
            .expect("the discard");
        let shape = async || {
            let block = state.block(repository.clone(), 1).await.expect("block 1");
            let reader = block.read();
            let data = reader.node_block();
            (data.node_count, data.node_unused, data.node_unused_count)
        };
        let before = shape().await;
        assert_eq!(
            before.0, 0,
            "block 1 started over once its last node was freed"
        );

        let counts =
            lore_revision::state::node_discard_free(&state, &repository, &[], vec![far], |_, _| {})
                .await
                .expect("the free");

        assert_eq!((counts.file_count, counts.directory_count), (0, 0));
        assert_eq!(shape().await, before, "block 1 is as it was");
    })
    .await;
}

/// A fused discard whose chain breaks part way, here at a file whose parent link names another
/// directory, answers the broken chain once the directory tasks it spawned are joined: the run's
/// directory met before the break is still spawned, and freed with its file, and the misparented
/// file is not.
#[tokio::test]
async fn a_fused_discard_whose_chain_breaks_joins_its_tasks_and_fails() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let f = add(&state, &repository, d, file("f"), "f").await;
        let sub = add(&state, &repository, d, directory("sub"), "sub").await;
        let g = add(&state, &repository, sub, file("g"), "g").await;
        add(&state, &repository, ROOT_NODE, file("keep"), "keep").await;
        // `d`'s chain runs `sub`, then `f`, which now names the root as its parent.
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        state.node_edit_for_tests(&block, f, |node| node.parent = ROOT_NODE);

        let broken =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), d).await;

        assert!(broken.is_err(), "the misparented file breaks the chain");
        let free: BTreeSet<usize> = free_list(&state, &repository).await.into_iter().collect();
        for node in [sub, g] {
            assert!(
                free.contains(&Node::index(node)),
                "{node} was freed by its joined task"
            );
        }
        assert!(
            !free.contains(&Node::index(f)),
            "the misparented file is not freed"
        );
    })
    .await;
}

/// A fused discard on a runtime of several workers frees sibling directories lying in one block,
/// each by a task of its own, spawned once the parent's run is freed, the tasks freeing in one
/// block side by side: every slot of the subtree is freed once, and the counts hold each node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fused_discard_of_sibling_directories_on_several_workers_frees_each_once() {
    with_state(|repository, written| async move {
        add(&written, &repository, ROOT_NODE, file("keep"), "keep").await;
        let d = add(&written, &repository, ROOT_NODE, directory("d"), "d").await;
        let mut below = Vec::new();
        for index in 0..8 {
            let name = format!("s{index}");
            let sub = add(&written, &repository, d, directory(&name), &name).await;
            below.push(sub);
            for leaf in 0..16 {
                let name = format!("f{leaf}");
                below.push(add(&written, &repository, sub, file(&name), &name).await);
            }
            let name = format!("x{index}");
            below.push(add(&written, &repository, d, file(&name), &name).await);
        }
        let state = reload(&written, &repository).await;
        // From the State written: the one loaded counts no block until it reads one.
        let before = all_free(&written, &repository).await;

        let counts = lore_revision::state::node_discard_fused(state.clone(), repository.clone(), d)
            .await
            .expect("the discard");

        assert_eq!((counts.file_count, counts.directory_count), (8 * 16 + 8, 9));
        let freed: BTreeSet<NodeID> = std::iter::once(d).chain(below).collect();
        let after = all_free(&state, &repository).await;
        let gained = &after - &before;
        assert_eq!(
            gained,
            freed,
            "every slot of the subtree is freed, and only those: extra {:?}, missing {:?}",
            gained.difference(&freed).collect::<Vec<_>>(),
            freed.difference(&gained).collect::<Vec<_>>()
        );
    })
    .await;
}

/// A fused discard one of whose directory tasks fails joins the others before it answers: below
/// `d`, a directory `m` holds `a`, whose file names another parent, and `b`, holding a directory
/// with a file. `m`'s task, joining `a`'s failure first, still joins `b`'s, so `b` is freed whole.
#[tokio::test]
async fn a_fused_discard_whose_directory_task_fails_joins_the_others() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let m = add(&state, &repository, d, directory("m"), "m").await;
        let a = add(&state, &repository, m, directory("a"), "a").await;
        let x = add(&state, &repository, a, file("x"), "x").await;
        let b = add(&state, &repository, m, directory("b"), "b").await;
        let b1 = add(&state, &repository, b, directory("b1"), "b1").await;
        let y = add(&state, &repository, b1, file("y"), "y").await;
        add(&state, &repository, ROOT_NODE, file("keep"), "keep").await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        // `m`'s chain runs `a`, then `b`, so `a`'s task runs, and fails, first.
        state.node_edit_for_tests(&block, m, |node| node.child = a);
        state.node_edit_for_tests(&block, a, |node| node.sibling = b);
        state.node_edit_for_tests(&block, b, |node| node.sibling = INVALID_NODE);
        state.node_edit_for_tests(&block, x, |node| node.parent = ROOT_NODE);

        let broken =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), d).await;

        assert!(
            broken.is_err(),
            "the misparented file fails its directory's task"
        );
        let free: BTreeSet<usize> = free_list_once(&state, &repository, 0)
            .await
            .into_iter()
            .collect();
        for node in [b, b1, y] {
            assert!(
                free.contains(&Node::index(node)),
                "{node} was freed by its joined task"
            );
        }
    })
    .await;
}

/// A fused discard whose chain loops back to a file it freed already fails rather than freeing
/// the slot a second time: the cycle guard passes a node more than once before it trips, and a
/// freed slot's flags read as a directory's.
#[tokio::test]
async fn a_fused_discard_meeting_a_freed_file_again_frees_it_once() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let f1 = add(&state, &repository, d, file("f1"), "f1").await;
        let f2 = add(&state, &repository, d, file("f2"), "f2").await;
        let f3 = add(&state, &repository, d, file("f3"), "f3").await;
        add(&state, &repository, ROOT_NODE, file("keep"), "keep").await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        // `d`'s chain runs `f1`, `f2`, `f3`, then back to `f1`.
        state.node_edit_for_tests(&block, d, |node| node.child = f1);
        state.node_edit_for_tests(&block, f1, |node| node.sibling = f2);
        state.node_edit_for_tests(&block, f2, |node| node.sibling = f3);
        state.node_edit_for_tests(&block, f3, |node| node.sibling = f1);

        let looped =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), d).await;

        assert!(looped.is_err(), "the chain meets a freed file");
        let free = free_list_once(&state, &repository, 0).await;
        for node in [f1, f2, f3] {
            assert!(free.contains(&Node::index(node)), "{node} is freed");
        }
    })
    .await;
}

/// A fused discard whose chain loops back to a directory it met already, across a block, fails
/// rather than freeing the directory's subtree twice, as two tasks side by side would.
#[tokio::test]
async fn a_fused_discard_meeting_a_directory_again_frees_its_subtree_once() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let d1 = add(&state, &repository, d, directory("d1"), "d1").await;
        let g = add(&state, &repository, d1, file("g"), "g").await;
        let fill = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        for index in 0.. {
            let name = format!("f{index}");
            let id = add(&state, &repository, fill, file(&name), &name).await;
            if Node::index(id) == BLOCK_NODE_COUNT - 1 {
                break;
            }
        }
        let e = add(&state, &repository, d, file("e"), "e").await;
        assert_eq!(NodeBlock::index(e), 1, "'e' lies in block 1");
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        let later = state.block(repository.clone(), 1).await.expect("block 1");
        // `d`'s chain runs `d1`, in block 0, then `e`, in block 1, then back to `d1`.
        state.node_edit_for_tests(&block, d, |node| node.child = d1);
        state.node_edit_for_tests(&block, d1, |node| node.sibling = e);
        state.node_edit_for_tests(&later, e, |node| node.sibling = d1);

        let looped =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), d).await;

        assert!(looped.is_err(), "the chain meets 'd1' again");
        let free = free_list_once(&state, &repository, 0).await;
        for node in [d1, g] {
            assert!(free.contains(&Node::index(node)), "{node} is freed");
        }
        assert_eq!(
            later.read().node_block().node_count,
            0,
            "block 1, 'e' freed, started over"
        );
    })
    .await;
}

/// A fused discard whose chain holds an empty directory looping to itself fails rather than
/// freeing it twice: the directory is marked as its run collects it, so its second visit, which
/// the cycle guard passes, fails the walk, where two tasks would each free it.
#[tokio::test]
async fn a_fused_discard_meeting_an_empty_directory_again_frees_it_once() {
    with_state(|repository, state| async move {
        let d = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let d0 = add(&state, &repository, d, directory("d0"), "d0").await;
        add(&state, &repository, ROOT_NODE, file("keep"), "keep").await;
        let block = state.block(repository.clone(), 0).await.expect("block 0");
        state.node_edit_for_tests(&block, d0, |node| node.sibling = d0);

        let looped =
            lore_revision::state::node_discard_fused(state.clone(), repository.clone(), d).await;

        assert!(looped.is_err(), "the chain meets 'd0' again");
        let free = free_list_once(&state, &repository, 0).await;
        assert!(free.contains(&Node::index(d0)), "'d0' is freed, once");
    })
    .await;
}

/// A free of roots whose parents lie in different blocks finds each parent in its own block: the
/// block it keeps from the last root's parent serves only a parent in that block. Here `r1` lies
/// in block 1 below a directory in block 0, and `r2` in block 2 below one in block 1.
#[tokio::test]
async fn a_free_of_roots_whose_parents_lie_in_different_blocks_unlinks_each() {
    with_state(|repository, state| async move {
        let p0 = add(&state, &repository, ROOT_NODE, directory("p0"), "p0").await;
        let fill = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        let mut index = 0;
        let mut fill_to = async |block: usize| loop {
            let name = format!("f{index}");
            index += 1;
            let id = add(&state, &repository, fill, file(&name), &name).await;
            if NodeBlock::index(id) == block && Node::index(id) == BLOCK_NODE_COUNT - 1 {
                break;
            }
        };
        fill_to(0).await;
        let r1 = add(&state, &repository, p0, file("r1"), "r1").await;
        let p1 = add(&state, &repository, ROOT_NODE, directory("p1"), "p1").await;
        fill_to(1).await;
        let r2 = add(&state, &repository, p1, file("r2"), "r2").await;
        assert_eq!(
            [
                NodeBlock::index(p0),
                NodeBlock::index(r1),
                NodeBlock::index(p1),
                NodeBlock::index(r2)
            ],
            [0, 1, 1, 2],
            "the roots and their parents lie in the blocks named"
        );
        let mut told = Vec::new();

        let counts = lore_revision::state::node_discard_free(
            &state,
            &repository,
            &[r1, r2],
            Vec::new(),
            |root, _| told.push(root),
        )
        .await
        .expect("the free");

        assert_eq!(told, vec![r1, r2], "each root is told");
        assert_eq!((counts.file_count, counts.directory_count), (2, 0));
        for parent in [p0, p1] {
            let children = state
                .node_children(repository.clone(), parent)
                .await
                .expect("the parent's children");
            assert!(children.is_empty(), "{parent} holds nothing: {children:?}");
        }
        let free = free_list_once(&state, &repository, 1).await;
        assert!(free.contains(&Node::index(r1)), "'r1' is freed in block 1");
        let last = state.block(repository.clone(), 2).await.expect("block 2");
        assert_eq!(
            last.read().node_block().node_count,
            0,
            "block 2, 'r2' freed, started over"
        );
        assert!(
            free_list_once(&state, &repository, 0).await.is_empty(),
            "block 0, holding the parents, frees nothing"
        );
    })
    .await;
}

/// A discard whose predecessor walk leaves the parent's block and comes back to it reads each
/// step from that step's block: `p`'s chain runs `a`, in block 1, then `b` and `t`, in block 0,
/// and unlinking `t` rewrites `b`'s sibling, not a record of block 1.
#[tokio::test]
async fn a_discard_whose_predecessor_walk_returns_to_the_parents_block_unlinks_it() {
    with_state(|repository, state| async move {
        let p = add(&state, &repository, ROOT_NODE, directory("p"), "p").await;
        let t = add(&state, &repository, p, file("t"), "t").await;
        let b = add(&state, &repository, p, file("b"), "b").await;
        let fill = add(&state, &repository, ROOT_NODE, directory("fill"), "fill").await;
        for index in 0.. {
            let name = format!("f{index}");
            let id = add(&state, &repository, fill, file(&name), &name).await;
            if Node::index(id) == BLOCK_NODE_COUNT - 1 {
                break;
            }
        }
        let a = add(&state, &repository, p, file("a"), "a").await;
        assert_eq!(
            [
                NodeBlock::index(t),
                NodeBlock::index(b),
                NodeBlock::index(a)
            ],
            [0, 0, 1],
            "'t' and 'b' lie in block 0, 'a' in block 1"
        );
        let chain = state
            .node_children(repository.clone(), p)
            .await
            .expect("p's children");
        assert_eq!(chain, vec![a, b, t], "p's chain runs 'a', 'b', then 't'");

        let counts = lore_revision::state::node_discard_fused(state.clone(), repository.clone(), t)
            .await
            .expect("the discard");

        assert_eq!((counts.file_count, counts.directory_count), (1, 0));
        let chain = state
            .node_children(repository.clone(), p)
            .await
            .expect("p's children");
        assert_eq!(
            chain,
            vec![a, b],
            "'t' is unlinked, the rest of the chain kept"
        );
        let free = free_list_once(&state, &repository, 0).await;
        assert!(free.contains(&Node::index(t)), "'t' is freed in block 0");
    })
    .await;
}
