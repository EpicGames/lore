// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H21: an unserved clear checks a directory for dirty children, then clears it, holding nothing
//! in between, so another writer's mark that stops at the still-dirty directory, an `os_diff`
//! task's, a VFS provider diff's or `lore dirty`'s, is left under a clean one. The clear checks the
//! children again after it clears the directory, and marks it again where one turned dirty.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_revision::node::NodeBlock;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::AttachOutcome;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;
use crate::harness::scheduler::Check;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// The tree `paths` with `dirty` marked, as an `os_diff` pass finds it.
async fn dirty_tree(paths: &[&str], dirty: &[&str]) -> Tree {
    let tree = Tree::new(paths).await;
    for path in dirty {
        ops::mark_dirty(&tree, tree.id(path)).await;
    }
    tree
}

fn clear(path: &'static str) -> Op<Tree> {
    Box::new(move |tree: Arc<Tree>| {
        Box::pin(async move {
            ops::scan_clear(&tree, tree.id(path)).await;
        })
    })
}

fn vacate(path: &'static str) -> Op<Tree> {
    Box::new(move |tree: Arc<Tree>| {
        Box::pin(async move {
            ops::vacated_clear(&tree, tree.id(path)).await;
        })
    })
}

fn mark_modified(path: &'static str) -> Op<Tree> {
    Box::new(move |tree: Arc<Tree>| {
        Box::pin(async move {
            ops::scan_mark_modified(&tree, tree.id(path)).await;
        })
    })
}

/// [`Rules::CLOSED`] but for typed nodes below a detached root, which an unserved detach leaves.
const DETACHING: Rules = Rules {
    dirty_closure: true,
    unique_names: true,
    no_ext_flags: true,
    no_builds: false,
};

/// Runs of [`a_clear_marking_again_a_directory_detached_meanwhile_succeeds`] whose clear's mark
/// found its directory detached.
static FOUND_GONE: AtomicU64 = AtomicU64::new(0);

/// Whether the clear of the current run of
/// [`a_finished_clear_keeps_closure_and_a_dropped_one_may_not`] finished.
static CLEAR_FINISHED: AtomicBool = AtomicBool::new(false);

/// Runs of [`a_finished_clear_keeps_closure_and_a_dropped_one_may_not`] whose dropped clear left
/// a clean directory over a dirty child.
static DROPPED_OPEN: AtomicU64 = AtomicU64::new(0);

/// [`closure_holds`], counting into `rechecked` the runs whose clears marked a directory again.
fn closure_holds_counting(rechecked: Arc<AtomicU64>) -> Check<Tree> {
    Box::new(move |tree: Arc<Tree>| {
        let rechecked = rechecked.clone();
        Box::pin(async move {
            if tree.state.clears_rechecked() > 0 {
                rechecked.fetch_add(1, Ordering::Relaxed);
            }
            closure_holds()(tree).await;
        })
    })
}

fn closure_holds() -> Check<Tree> {
    Box::new(|tree: Arc<Tree>| {
        Box::pin(async move {
            crate::holding(&tree.state, &tree.repository, DETACHING).await;
        })
    })
}

/// A clear passing `x` beside `add` into `x`, on the tree `x/f1` dirty, explored: closure kept,
/// and some run's clear marked `x` again, so the add reached the window H21 is.
fn clear_beside_add(name: &'static str, add: Op<Tree>) {
    let rechecked = Arc::new(AtomicU64::new(0));
    let scenario = Scenario::<Tree> {
        name,
        setup: Box::new(|| Box::pin(dirty_tree(&["x/f1", "x/q/g"], &["x/f1"]))),
        ops: vec![clear("x/f1"), add],
        cancellable: vec![],
        check: closure_holds_counting(rechecked.clone()),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        rechecked.load(Ordering::Relaxed) > 0,
        "no run's clear marked 'x' again"
    );
}

/// Explores `add` into the clean `x` alone, dropped at each point it parks at: a run that drops it
/// between its publish and its mark leaves `left` dirty under a clean `above`, which the model
/// excuses as the dropped call's; some run must leave one.
fn dropped_before_its_mark(
    name: &'static str,
    add: Op<Tree>,
    (left, above): (&'static str, &'static str),
) {
    let counted = Arc::new(AtomicU64::new(0));
    let counts = counted.clone();
    let scenario = Scenario::<Tree> {
        name,
        setup: Box::new(|| Box::pin(Tree::new(&["x/f1"]))),
        ops: vec![add],
        cancellable: vec![0],
        check: Box::new(move |tree: Arc<Tree>| {
            let counts = counts.clone();
            Box::pin(async move {
                let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
                let dirty = |path| report.at(path).is_some_and(|(_, node)| node.is_dirty());
                if dirty(left) && !dirty(above) {
                    counts.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    assert!(
        counted.load(Ordering::Relaxed) > 0,
        "no dropped add left '{left}' dirty under a clean '{above}'"
    );
}

/// A clear of a file climbing through its directories, and a mark of a file in a clean sibling
/// directory: the marked file's directories end dirty.
#[test]
fn a_clear_and_a_siblings_mark_keep_dirty_closure() {
    let rechecked = Arc::new(AtomicU64::new(0));
    let scenario = Scenario::<Tree> {
        name: "h21: an os_diff clear against a sibling's mark",
        setup: Box::new(|| Box::pin(dirty_tree(&["x/q1/f1", "x/q3/g"], &["x/q1/f1"]))),
        ops: vec![clear("x/q1/f1"), mark_modified("x/q3/g")],
        cancellable: vec![],
        check: closure_holds_counting(rechecked.clone()),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        rechecked.load(Ordering::Relaxed) > 0,
        "no run marked a directory again"
    );
}

/// Two clears through a shared parent, the second reaching it after the first cleaned it, and a
/// mark of a file in a clean sibling directory: the marked file's directories end dirty.
#[test]
fn two_clears_through_a_shared_parent_and_a_mark_keep_dirty_closure() {
    let rechecked = Arc::new(AtomicU64::new(0));
    let scenario = Scenario::<Tree> {
        name: "h21: two os_diff clears through a shared parent and a sibling's mark",
        setup: Box::new(|| Box::pin(dirty_tree(&["x/f1", "x/f2", "x/q3/g"], &["x/f1", "x/f2"]))),
        ops: vec![clear("x/f1"), clear("x/f2"), mark_modified("x/q3/g")],
        cancellable: vec![],
        check: closure_holds_counting(rechecked.clone()),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        rechecked.load(Ordering::Relaxed) > 0,
        "no run marked a directory again"
    );
}

/// Two clears of the files of one directory, each refusing where the other's file is still dirty
/// or passing where it is not, in every order: once both files are clean, so is every directory
/// above them.
#[test]
fn two_clears_at_a_shared_parent_leave_it_clean_once_every_child_is() {
    let scenario = Scenario::<Tree> {
        name: "h21: two os_diff clears refusing and passing at a shared parent",
        setup: Box::new(|| {
            Box::pin(dirty_tree(
                &["x/p/a", "x/p/b", "x/q/g"],
                &["x/p/a", "x/p/b"],
            ))
        }),
        ops: vec![clear("x/p/a"), clear("x/p/b")],
        cancellable: vec![],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                closure_holds()(tree.clone()).await;
                for path in ["x", "x/p", "x/p/a", "x/p/b"] {
                    let node = tree
                        .state
                        .node(tree.repository.clone(), tree.id(path))
                        .await
                        .expect("the node reads");
                    assert!(!node.is_dirty(), "{path} ends dirty");
                }
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A clear passing a directory while a mark of a file in it stops there, still dirty, and a
/// second clear passing the directory above: the directory the first clear cleaned is marked
/// again by its re-check, or by the mark, and the directory above with it.
#[test]
fn a_clears_recheck_marks_its_directory_again_against_a_clear_above() {
    let rechecked = Arc::new(AtomicU64::new(0));
    let scenario = Scenario::<Tree> {
        name: "h21: a clear's re-check against a mark below and a clear above",
        setup: Box::new(|| Box::pin(dirty_tree(&["x/p/a", "x/p/c", "x/h"], &["x/p/a", "x/h"]))),
        ops: vec![clear("x/p/a"), clear("x/h"), mark_modified("x/p/c")],
        cancellable: vec![],
        check: closure_holds_counting(rechecked.clone()),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        rechecked.load(Ordering::Relaxed) > 0,
        "no run marked a directory again"
    );
}

/// A vacated clear of a directory a dirty child left, as a reset leaves one after discarding a
/// dirty add, while a mark of a file in it stops there, still dirty: the vacated clear's own
/// re-check marks the directory again, the only clear running, or the mark finds it clean.
#[test]
fn a_vacated_clears_recheck_marks_its_directory_again_against_a_mark_below() {
    let rechecked = Arc::new(AtomicU64::new(0));
    let scenario = Scenario::<Tree> {
        name: "h21: a vacated clear's re-check against a mark below",
        setup: Box::new(|| Box::pin(dirty_tree(&["x/p/a", "x/p/c"], &["x/p"]))),
        ops: vec![vacate("x/p"), mark_modified("x/p/c")],
        cancellable: vec![],
        check: closure_holds_counting(rechecked.clone()),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        rechecked.load(Ordering::Relaxed) > 0,
        "no run marked a directory again"
    );
}

/// The same beside a clear passing the directory above: every directory ends dirty above the
/// marked file.
#[test]
fn a_vacated_clear_keeps_dirty_closure_against_a_mark_below_and_a_clear_above() {
    let rechecked = Arc::new(AtomicU64::new(0));
    let scenario = Scenario::<Tree> {
        name: "h21: a vacated clear against a mark below and a clear above",
        setup: Box::new(|| Box::pin(dirty_tree(&["x/p/a", "x/p/c", "x/h"], &["x/p", "x/h"]))),
        ops: vec![vacate("x/p"), clear("x/h"), mark_modified("x/p/c")],
        cancellable: vec![],
        check: closure_holds_counting(rechecked.clone()),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        rechecked.load(Ordering::Relaxed) > 0,
        "no run marked a directory again"
    );
}

/// A clear passing a directory while a VFS provider diff's checked create attaches a file into it
/// dirty-added and marks the directory: the added file is in the chain the clear's re-check walks,
/// or the mark finds the directory clean and marks it. The checked create's own exploration beside
/// a clear of its directory; its writes are the diff add's, its name walk a read outside the
/// window.
#[test]
fn a_clear_and_a_checked_create_into_its_directory_keep_dirty_closure() {
    clear_beside_add(
        "h21: an os_diff clear against a checked create into its directory",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let created = ops::checked_create(&tree, tree.id("x"), "n").await;
                assert!(
                    matches!(created, Some(AttachOutcome::Attached(_))),
                    "'x/n' is attached: {created:?}"
                );
            })
        }),
    );
}

/// A clear passing a directory while the scan adds a directory into it, published dirty-added
/// and its parent marked after, as `os_diff` adds one.
#[test]
fn a_clear_and_a_scan_add_into_its_directory_keep_dirty_closure() {
    clear_beside_add(
        "h21: an os_diff clear against an os_diff add into its directory",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::scan_add(&tree, tree.id("x"), "n").await;
            })
        }),
    );
}

/// A clear against a detach of the directory it climbs through, or of a sibling of its file: the
/// clear neither fails nor leaves a live dirty node under a clean directory.
#[test]
fn a_clear_against_a_detach_of_its_directory_or_a_sibling_keeps_dirty_closure() {
    let detaches = [
        (
            "h21: an os_diff clear against an unserved detach of its directory",
            "x/p",
        ),
        (
            "h21: an os_diff clear against an unserved detach of a sibling",
            "x/p/c",
        ),
    ];
    for (name, detached) in detaches {
        let scenario = Scenario::<Tree> {
            name,
            setup: Box::new(|| Box::pin(dirty_tree(&["x/p/a", "x/p/c", "x/q/g"], &["x/p/a"]))),
            ops: vec![
                clear("x/p/a"),
                Box::new(move |tree: Arc<Tree>| {
                    Box::pin(async move {
                        ops::unserved_detach(&tree, tree.id(detached)).await;
                    })
                }),
            ],
            cancellable: vec![],
            check: closure_holds(),
        };
        crate::explored(scenario, Modelling::default(), Exploration::default());
    }
}

/// A clear whose directory's chain changes under its second walk, its head detached: the clear
/// marks the directory again, which leaves it stale-dirty at worst, the safe direction.
#[test]
fn a_clear_against_a_detach_of_a_sibling_keeps_dirty_closure() {
    let rechecked = Arc::new(AtomicU64::new(0));
    let scenario = Scenario::<Tree> {
        name: "h21: an os_diff clear against a sibling's detach and a mark",
        // `c`, added last, heads the chain.
        setup: Box::new(|| Box::pin(dirty_tree(&["x/a", "x/g", "x/c"], &["x/a"]))),
        ops: vec![
            clear("x/a"),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::unserved_detach(&tree, tree.id("x/c")).await;
                })
            }),
        ],
        cancellable: vec![],
        check: Box::new({
            let rechecked = rechecked.clone();
            move |tree: Arc<Tree>| {
                let counts = closure_holds_counting(rechecked.clone());
                Box::pin(async move {
                    if tree.state.clears_rechecked() > 0 {
                        let x = tree
                            .state
                            .node(tree.repository.clone(), tree.id("x"))
                            .await
                            .expect("x reads");
                        assert!(x.is_dirty(), "the clear's re-mark left x clean");
                    }
                    counts(tree).await;
                })
            }
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        rechecked.load(Ordering::Relaxed) > 0,
        "no run found the chain changed under the second walk"
    );
}

/// A clear and a mark on a State loaded afresh, across blocks: the clear's file in another block
/// than its directory, so each block turns dirty and is recorded under the scheduler; or the
/// directory's chain starting in a block not in memory, which the clear's walks load. The State
/// saved after the run holds every node's flags as they stand, so each block written was recorded
/// changed.
#[test]
fn a_clear_and_a_mark_across_blocks_on_a_loaded_state_keep_dirty_closure() {
    // (name, before the fill, after it): the nodes after it lie in a later block.
    let layouts: [(
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    ); 2] = [
        (
            "h21: an os_diff clear and a mark across blocks, its file in a later block",
            &["x/q3/g"],
            &["x/f1"],
        ),
        (
            "h21: an os_diff clear and a mark across blocks, its chain's head in a later block",
            &["x/f1", "x/q3/g"],
            &["x/late"],
        ),
    ];
    for (name, before, after) in layouts {
        let scenario = Scenario::<Tree> {
            name,
            setup: Box::new(move || {
                Box::pin(async move {
                    let tree = Tree::spread(before, "fill", after).await;
                    ops::mark_dirty(&tree, tree.id("x/f1")).await;
                    let tree = tree.reloaded().await;
                    for path in after {
                        let block = NodeBlock::index(tree.id(path));
                        assert!(
                            block != NodeBlock::index(tree.id("x"))
                                && tree.state.resident_block(block).is_none(),
                            "{path} lies in a block of its own, not in memory"
                        );
                    }
                    tree
                })
            }),
            ops: vec![clear("x/f1"), mark_modified("x/q3/g")],
            cancellable: vec![],
            check: Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    closure_holds()(tree.clone()).await;
                    let saved = tree.saved().await;
                    for (path, id) in &tree.nodes {
                        let live = tree.state.node(tree.repository.clone(), *id).await;
                        let loaded = saved.node(tree.repository.clone(), *id).await;
                        assert_eq!(
                            live.expect("the node reads").flags,
                            loaded.expect("the saved node reads").flags,
                            "{path} as saved"
                        );
                    }
                })
            }),
        };
        crate::explored(scenario, Modelling::default(), Exploration::default());
    }
}

/// A clear whose second walk finds its directory's chain changed, its head detached, while the
/// directory itself is detached before the clear marks it again: the mark finds it gone, which
/// the clear answers as success.
#[test]
fn a_clear_marking_again_a_directory_detached_meanwhile_succeeds() {
    let scenario = Scenario::<Tree> {
        name: "h21: an os_diff clear's re-mark of a directory detached meanwhile",
        // `c`, added after `a`, heads `p`'s chain.
        setup: Box::new(|| Box::pin(dirty_tree(&["x/p/a", "x/p/c", "x/q/g"], &["x/p/a"]))),
        ops: vec![
            clear("x/p/a"),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::unserved_detach(&tree, tree.id("x/p/c")).await;
                    ops::unserved_detach(&tree, tree.id("x/p")).await;
                })
            }),
        ],
        cancellable: vec![],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                if tree.state.clears_found_gone() > 0 {
                    FOUND_GONE.fetch_add(1, Ordering::Relaxed);
                }
                closure_holds()(tree).await;
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
    assert!(
        FOUND_GONE.load(Ordering::Relaxed) > 0,
        "no run's clear found its directory detached when it marked it again"
    );
}

/// A clear passing a directory while the scan builds a chain of new directories in it, the first
/// published under it and marked, the rest under the first.
#[test]
fn a_clear_and_a_scan_chain_into_its_directory_keep_dirty_closure() {
    clear_beside_add(
        "h21: an os_diff clear against a scan's directory chain into its directory",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let chained = ops::scan_chain(&tree, tree.id("x"), "n1/n2").await;
                assert!(chained.is_some(), "the chain is made");
            })
        }),
    );
}

/// The same with the product's chain, `ensure_scan_dir_chain` itself, its records judged by the
/// rules that need no call.
#[test]
fn a_clear_and_the_products_scan_chain_into_its_directory_keep_dirty_closure() {
    clear_beside_add(
        "h21: an os_diff clear against ensure_scan_dir_chain into its directory",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                lore_revision::state::os_diff::ensure_scan_dir_chain(
                    tree.repository.clone(),
                    tree.state.clone(),
                    tree.id("x"),
                    "n1/n2",
                )
                .await
                .expect("the chain is made");
            })
        }),
    );
}

/// A clear dropped at each point it parks at, against a mark of a sibling of its file: a clear
/// that finishes keeps closure, and one dropped between clearing a directory and its re-mark's
/// write may leave that directory clean over the marked file (C12).
#[test]
fn a_finished_clear_keeps_closure_and_a_dropped_one_may_not() {
    let scenario = Scenario::<Tree> {
        name: "h21: a cancelled os_diff clear against a sibling's mark",
        setup: Box::new(|| {
            Box::pin(async {
                CLEAR_FINISHED.store(false, Ordering::Relaxed);
                dirty_tree(&["x/p/a", "x/p/c", "x/q/g"], &["x/p/a"]).await
            })
        }),
        ops: vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::scan_clear(&tree, tree.id("x/p/a")).await;
                    CLEAR_FINISHED.store(true, Ordering::Relaxed);
                })
            }),
            mark_modified("x/p/c"),
        ],
        cancellable: vec![0],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let Err(violations) =
                    invariants::check(&tree.state, &tree.repository, DETACHING).await
                else {
                    return;
                };
                assert!(
                    !CLEAR_FINISHED.load(Ordering::Relaxed),
                    "a finished clear left {violations:#?}"
                );
                DROPPED_OPEN.fetch_add(1, Ordering::Relaxed);
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    assert!(
        DROPPED_OPEN.load(Ordering::Relaxed) > 0,
        "no dropped clear left a clean directory over a dirty child"
    );
}

/// A clear passing a directory while a VFS provider diff adds a file into it, unchecked, and marks
/// the directory after.
#[test]
fn a_clear_and_a_diff_add_into_its_directory_keep_dirty_closure() {
    clear_beside_add(
        "h21: an os_diff clear against a diff add into its directory",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let added = ops::diff_add(&tree, tree.id("x"), "n", true).await;
                assert!(added.is_some(), "'x/n' is added");
            })
        }),
    );
}

/// A clear passing a directory while `lore dirty` adds a file below it, under a directory it adds
/// and marks first, each mark forced, then climbing to the first dirty ancestor.
#[test]
fn a_clear_and_a_dirty_add_below_its_directory_keep_dirty_closure() {
    clear_beside_add(
        "h21: an os_diff clear against a dirty add below its directory",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let added = ops::dirty_add(&tree, tree.id("x"), "m", "n", true).await;
                assert!(added.is_some(), "'x/m/n' is added");
            })
        }),
    );
}

/// A clear of a file in a directory while a VFS provider diff settles an add under it whose name a
/// staged delete holds: the chain finds the directory, and the checked create takes the delete
/// back, marks the holder modified and marks the directory. The directory stays dirty over the
/// held file, so the clear never clears it: closure alone is checked, H21's window being the other
/// chained create's.
#[test]
fn a_clear_and_a_chained_checked_create_of_a_held_name_keep_dirty_closure() {
    let scenario = Scenario::<Tree> {
        name: "h21: an os_diff clear against a chained checked create of a held name",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = dirty_tree(&["x/f1", "x/f", "x/q/g"], &["x/f1"]).await;
                ops::staged_delete_level(&tree, tree.id("x/f"), tree.id("x")).await;
                tree
            })
        }),
        ops: vec![
            clear("x/f1"),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let created = ops::chain_checked_create(&tree, ROOT_NODE, "x", "f").await;
                    assert_eq!(created, Some(AttachOutcome::Exists(tree.id("x/f"))));
                })
            }),
        ],
        cancellable: vec![],
        check: closure_holds(),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A clear passing a directory while a VFS provider diff settles an add under a directory missing
/// from it: the chain adds the directory into it and marks it, and the checked create attaches
/// the file in the directory the chain added.
#[test]
fn a_clear_and_a_chained_checked_create_into_a_directory_it_adds_keep_dirty_closure() {
    clear_beside_add(
        "h21: an os_diff clear against a chained checked create into a directory it adds",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let created = ops::chain_checked_create(&tree, ROOT_NODE, "x/m", "n").await;
                assert!(
                    matches!(created, Some(AttachOutcome::Attached(_))),
                    "'x/m/n' is attached: {created:?}"
                );
            })
        }),
    );
}

/// A VFS provider diff's add dropped after its publish and before it marks its directory.
#[test]
fn a_diff_add_dropped_before_its_mark_is_excused() {
    dropped_before_its_mark(
        "h21: a diff add dropped before its mark",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let _ = ops::diff_add(&tree, tree.id("x"), "n", true).await;
            })
        }),
        ("x/n", "x"),
    );
}

/// A scan's chain dropped after its first add and before it marks the directory above it.
#[test]
fn a_scan_chain_dropped_before_its_mark_is_excused() {
    dropped_before_its_mark(
        "h21: a scan chain dropped before its mark",
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let _ = ops::scan_chain(&tree, tree.id("x"), "n1/n2").await;
            })
        }),
        ("x/n1", "x"),
    );
}
