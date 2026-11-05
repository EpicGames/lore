// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H6: a discard freeing a directory's children before it detaches the directory, so an error in
//! between leaves the directory attached over freed slots. A discard detaches its root first, and
//! frees a directory only after its children, so an error part way leaks a detached subtree and
//! never leaves a live node over a freed slot; a commit's free loads every block below its
//! detached roots before it frees anything. Each test runs a discard on a State loaded afresh,
//! whose store fails every read once the blocks the test names are in memory: the two of
//! `apply_pending_discards` check the hazard, and the commit's pins its free's order. The C API
//! delete's pending queue keeps a detached subtree whole through the same failures until a drain
//! frees it.

use lore_revision::state::WalkDiscards;

use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;

/// The parent's block fails to load: the discard leaves the directory and its children as they
/// were.
#[tokio::test]
async fn a_discard_whose_parent_fails_to_load_frees_nothing() {
    crate::in_context(async {
        let tree = Tree::spread(&["p/", "p/s"], "fill", &["p/d/x", "p/d/y", "p/keep"]).await;
        let (tree, store) = tree.failing().await;
        let d = tree.id("p/d");
        let _block = invariants::block_of(&tree.state, &tree.repository, d).await;
        assert!(
            tree.state.resident_block(0).is_none(),
            "the parent's block is not in memory"
        );

        store.fail(true);
        let discards = WalkDiscards::default();
        discards.queue(tree.state.clone(), tree.repository.clone(), vec![d]);
        let discarded = discards.apply().await;
        store.fail(false);

        assert!(discarded.is_err(), "the parent's block fails to load");
        let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
        for path in ["p/d", "p/d/x", "p/d/y"] {
            assert!(report.at(path).is_some(), "'{path}' is still live");
        }
    })
    .await;
}

/// A block below the directory fails to load: the directory is detached, and whatever its
/// subtree kept hangs below it.
#[tokio::test]
async fn a_discard_whose_subtree_fails_to_load_leaves_it_detached() {
    crate::in_context(async {
        let tree = Tree::spread(&["p/d/x", "p/d/sub/z", "p/keep"], "fill", &["p/d/sub/w"]).await;
        let (tree, store) = tree.failing().await;
        let d = tree.id("p/d");
        let _block = invariants::block_of(&tree.state, &tree.repository, d).await;
        assert!(
            tree.state.resident_block(1).is_none(),
            "the block of 'p/d/sub/w' is not in memory"
        );

        store.fail(true);
        let discards = WalkDiscards::default();
        discards.queue(tree.state.clone(), tree.repository.clone(), vec![d]);
        let discarded = discards.apply().await;
        store.fail(false);

        assert!(discarded.is_err(), "the block of 'p/d/sub/w' fails to load");
        let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
        assert!(report.at("p/d").is_none(), "'p/d' is detached");
        assert!(report.at("p/keep").is_some(), "its sibling stays");
        assert_eq!(
            report.detached, 2,
            "'p/d' and 'p/d/sub', whose walk failed, are left Discarded"
        );
    })
    .await;
}

/// A commit's free whose collected nodes lie in a block that fails to load: its root is
/// detached, and nothing below it is freed.
#[tokio::test]
async fn a_free_whose_collected_block_fails_to_load_frees_nothing_below_its_root() {
    crate::in_context(async {
        let tree = Tree::spread(&["p/d/x", "p/d/sub/z", "p/keep"], "fill", &["p/d/sub/w"]).await;
        let (tree, store) = tree.failing().await;
        let d = tree.id("p/d");
        let below: Vec<_> = ["p/d/x", "p/d/sub", "p/d/sub/z", "p/d/sub/w"]
            .iter()
            .map(|path| tree.id(path))
            .collect();
        let _block = invariants::block_of(&tree.state, &tree.repository, d).await;
        assert!(
            tree.state.resident_block(1).is_none(),
            "the block of 'p/d/sub/w' is not in memory"
        );

        store.fail(true);
        let freed = lore_revision::state::node_discard_free(
            &tree.state,
            &tree.repository,
            &[d],
            below.clone(),
            |_, _| {},
        )
        .await;
        store.fail(false);

        assert!(freed.is_err(), "the block of 'p/d/sub/w' fails to load");
        let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
        assert!(report.at("p/d").is_none(), "'p/d' is detached");
        for node in below {
            let record = tree
                .state
                .node(tree.repository.clone(), node)
                .await
                .expect("a node below reads");
            assert!(
                !record.is_discarded(),
                "node {node} below 'p/d' is not freed"
            );
        }
    })
    .await;
}

/// A pending discard whose subtree reaches a block that fails to load marks every chain it can
/// read, answers the error, and keeps its detached root queued; a drain that then fails to load
/// the block frees nothing and keeps the queue; once the store reads again, a drain frees the
/// whole subtree.
#[tokio::test]
async fn a_pending_discard_and_its_drain_whose_blocks_fail_to_load() {
    crate::in_context(async {
        let tree = Tree::spread(&["p/d/x", "p/d/sub/z", "p/keep"], "fill", &["p/d/sub/w"]).await;
        let (tree, store) = tree.failing().await;
        let (p, d) = (tree.id("p"), tree.id("p/d"));
        let _block = invariants::block_of(&tree.state, &tree.repository, d).await;
        assert!(
            tree.state.resident_block(1).is_none(),
            "the block of 'p/d/sub/w' is not in memory"
        );
        let discarded = async |path: &str| {
            tree.state
                .node(tree.repository.clone(), tree.id(path))
                .await
                .expect("a node in memory reads")
                .is_discarded()
        };

        store.fail(true);
        let pending = tree
            .state
            .node_discard_pending(
                tree.repository.clone(),
                d,
                p,
                lore_storage::hash::hash_string("d"),
            )
            .await;
        assert!(
            pending.is_err(),
            "the block of 'p/d/sub/w', heading 'p/d/sub', fails to load"
        );
        for path in ["p/d", "p/d/x", "p/d/sub"] {
            assert!(discarded(path).await, "'{path}' is marked");
        }
        assert!(
            !discarded("p/d/sub/z").await,
            "'p/d/sub/z', behind the node that failed in its chain, is not"
        );
        let drained = tree.state.node_free_pending(&tree.repository).await;
        assert!(drained.is_err(), "the drain fails to load the block too");
        store.fail(false);
        let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
        assert!(report.at("p/d").is_none(), "'p/d' is detached");
        assert_eq!(
            report.detached, 3,
            "the marked nodes in memory are not freed"
        );

        let counts = tree
            .state
            .node_free_pending(&tree.repository)
            .await
            .expect("the drain, with the store reading again");

        assert_eq!(
            (counts.file_count, counts.directory_count),
            (3, 2),
            "the queue kept the root, and the drain frees its whole subtree"
        );
        let report = crate::holding(
            &tree.state,
            &tree.repository,
            Rules {
                no_builds: true,
                ..Rules::default()
            },
        )
        .await;
        assert_eq!(report.detached, 0, "nothing is left pending");
        assert!(report.at("p/keep").is_some(), "the sibling stays");
    })
    .await;
}

/// A pending discard that meets a block it cannot load in the first chain it marks still marks
/// the chains after it: here `p/d/bad`, whose chain heads at the node past the fill, is marked
/// before `p/d/good`, as the chains go on a stack.
#[tokio::test]
async fn a_pending_discard_marks_the_chains_after_one_that_fails() {
    crate::in_context(async {
        let tree = Tree::spread(
            &["p/d/bad/z", "p/d/good/y", "p/keep"],
            "fill",
            &["p/d/bad/w"],
        )
        .await;
        let (tree, store) = tree.failing().await;
        let (p, d) = (tree.id("p"), tree.id("p/d"));
        let _block = invariants::block_of(&tree.state, &tree.repository, d).await;
        assert!(
            tree.state.resident_block(1).is_none(),
            "the block of 'p/d/bad/w' is not in memory"
        );

        store.fail(true);
        let pending = tree
            .state
            .node_discard_pending(
                tree.repository.clone(),
                d,
                p,
                lore_storage::hash::hash_string("d"),
            )
            .await;
        store.fail(false);

        assert!(pending.is_err(), "the block of 'p/d/bad/w' fails to load");
        for path in ["p/d/good", "p/d/good/y"] {
            let record = tree
                .state
                .node(tree.repository.clone(), tree.id(path))
                .await
                .expect("a node in memory reads");
            assert!(
                record.is_discarded(),
                "'{path}' is marked after the failed chain"
            );
        }
    })
    .await;
}

/// A drain dropped while it loads a block, here one a failed pending discard left unmarked and
/// not in memory, frees nothing and leaves the queue as it was: the next drain frees the whole
/// subtree.
#[test]
fn a_drain_dropped_while_it_loads_leaves_the_queue() {
    use std::sync::Arc;

    use lore_revision::state::node::hooks::Point;

    use crate::harness::catalogue::modelled::Modelling;
    use crate::harness::scheduler::Exploration;
    use crate::harness::scheduler::Op;
    use crate::harness::scheduler::Scenario;

    let drain: Op<Tree> = Box::new(|tree: Arc<Tree>| {
        Box::pin(async move {
            let _ = tree.state.node_free_pending(&tree.repository).await;
        })
    });
    let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = dropped.clone();
    let scenario = Scenario::<Tree> {
        name: "h6: a drain dropped while it loads",
        setup: Box::new(|| {
            Box::pin(async {
                let tree =
                    Tree::spread(&["p/d/x", "p/d/sub/z", "p/keep"], "fill", &["p/d/sub/w"]).await;
                let (tree, store) = tree.failing().await;
                let (p, d) = (tree.id("p"), tree.id("p/d"));
                let _block = invariants::block_of(&tree.state, &tree.repository, d).await;
                store.fail(true);
                let pending = tree
                    .state
                    .node_discard_pending(
                        tree.repository.clone(),
                        d,
                        p,
                        lore_storage::hash::hash_string("d"),
                    )
                    .await;
                store.fail(false);
                assert!(
                    pending.is_err(),
                    "the marks stop at the block not in memory"
                );
                tree
            })
        }),
        ops: vec![drain],
        cancellable: vec![0],
        check: Box::new(move |tree: Arc<Tree>| {
            let seen = seen.clone();
            Box::pin(async move {
                let again = tree
                    .state
                    .node_free_pending(&tree.repository)
                    .await
                    .expect("the next drain");
                if (again.file_count, again.directory_count) == (3, 2) {
                    seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let report = crate::holding(
                    &tree.state,
                    &tree.repository,
                    Rules {
                        no_builds: true,
                        ..Rules::default()
                    },
                )
                .await;
                assert_eq!(report.detached, 0, "nothing is left pending");
                assert!(report.at("p/keep").is_some(), "the sibling stays");
            })
        }),
    };
    // A drain frees detached nodes alone, so a run dropping it leaves closure as it was.
    let keeping_closure = Modelling {
        drops_excuse_closure: false,
        ..Modelling::default()
    };
    crate::explored(
        scenario,
        keeping_closure,
        Exploration {
            cancellation: true,
            sleep_sets: false,
            ..Exploration::default()
        },
    );
    assert!(
        dropped.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "some run dropped the drain at {:?} and the next drain freed it all",
        Point::BlockLoad
    );
}

/// A fused discard whose chain reaches a block that fails to load, after a run that met a
/// directory, joins that directory's task before it answers: the directory, spawned before the
/// load, is freed with its file. The task runs only once the discard awaits it: the failing load
/// answers in the poll that spawned it, as the store refuses without yielding, so a discard that
/// returned without joining would drop the task unpolled.
#[tokio::test]
async fn a_fused_discard_whose_later_block_fails_to_load_joins_its_tasks() {
    crate::in_context(async {
        let tree = Tree::spread(&["p/d/s/z", "p/keep"], "fill", &["p/d/w"]).await;
        let (tree, store) = tree.failing().await;
        let (d, s, z, w) = (
            tree.id("p/d"),
            tree.id("p/d/s"),
            tree.id("p/d/s/z"),
            tree.id("p/d/w"),
        );
        let block = invariants::block_of(&tree.state, &tree.repository, d).await;
        // `d`'s chain runs `s`, in block 0, then `w`, in block 1, which is not in memory; `w`'s own
        // sibling still names `s`, which the walk never reads, as `w`'s block fails to load.
        tree.state
            .node_edit_for_tests(&block, d, |node| node.child = s);
        tree.state
            .node_edit_for_tests(&block, s, |node| node.sibling = w);
        assert!(
            tree.state.resident_block(1).is_none(),
            "the block of 'p/d/w' is not in memory"
        );

        store.fail(true);
        let discarded = lore_revision::state::node_discard_fused(
            tree.state.clone(),
            tree.repository.clone(),
            d,
        )
        .await;
        store.fail(false);

        assert!(discarded.is_err(), "the block of 'p/d/w' fails to load");
        let free = {
            let reader = block.read();
            let data = reader.node_block();
            let mut free = Vec::new();
            let mut at = data.node_unused as usize;
            for _ in 0..data.node_unused_count {
                free.push(at);
                at = data.node[at].sibling as usize;
            }
            free
        };
        for node in [s, z] {
            assert!(
                free.contains(&lore_revision::node::Node::index(node)),
                "{node}, met before the failed load, is freed by its joined task"
            );
        }
    })
    .await;
}
