// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H1: a C API move leaving its node in no chain between its detach and its prepend, so a move of
//! the same node from another call, reading it under its old parent, misses it there and fails.
//! A C API move holds the State's moves permit for its whole batch (C10), and `node_move` moves the
//! node in one critical section. The scenarios of a find and of a move against a reattach check the
//! hazard, and that of two moves, each under the permit, checks the permit (C10); those of a move
//! against an add, a detach, a children walk and a directory reattach pin what the section checks
//! under its locks, and the permit checked moves share with directory reattaches.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachOutcome;
use lore_revision::state::NameChange;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenIterator;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

struct Context {
    tree: Tree,
    /// `p/x`, the node both calls move.
    moved: NodeID,
    /// Moves that failed.
    failures: AtomicUsize,
}

impl Modelled for Context {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// One call moving `p/x` into `destination`, as the C API's batch does: under the moves
/// permit, taken first.
fn move_into(destination: &'static str) -> Op<Context> {
    move_to(destination, "x")
}

/// One call moving `p/x` into `destination` under `name`, as [`move_into`] does: the model fails a
/// run whose move failed.
fn move_to(destination: &'static str, name: &'static str) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let tree = &context.tree;
            ops::staged_move(tree, context.moved, tree.id(destination), name).await;
        })
    })
}

/// Two calls moving one directory into two others, each under the moves permit: both succeed, one
/// after the other (C10), and the directory ends in one of them.
#[test]
fn two_moves_of_one_node_from_two_calls_both_succeed() {
    let scenario = Scenario::<Context> {
        name: "h1: two C API moves of one node",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["p/x/f", "p/y", "q/", "r/"]).await;
                let moved = tree.id("p/x");
                Context {
                    tree,
                    moved,
                    failures: AtomicUsize::new(0),
                }
            })
        }),
        ops: vec![move_into("q"), move_into("r")],
        cancellable: vec![],
        check: Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                let report = crate::holding(&tree.state, &tree.repository, Rules::CLOSED).await;
                let at = report.paths.get(&context.moved).map(String::as_str);
                assert!(
                    matches!(at, Some("q/x" | "r/x")),
                    "x ends in one destination, at {at:?}"
                );
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// Finds of `p/x` that met it in neither `p` nor `q`.
static MISSED: AtomicUsize = AtomicUsize::new(0);

/// A find of `x` in `p`, then, where it is gone from there, in `q`: one move of `x` from `p` to
/// `q` running alongside leaves it in one of the two at every moment, so the second find meets
/// it wherever the first missed it.
fn find_in_either() -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let tree = &context.tree;
            let name_hash = lore_storage::hash::hash_string("x");
            let found = |parent| {
                tree.state
                    .find_subnode(tree.repository.clone(), parent, name_hash)
            };
            if found(tree.id("p")).await.is_ok() {
                return;
            }
            if found(tree.id("q")).await.is_err() {
                MISSED.fetch_add(1, Ordering::Relaxed);
            }
        })
    })
}

/// A move of a directory to another and finds of its name in both: no find misses it, since the
/// move takes it out of one chain and into the other in one critical section.
#[test]
fn a_find_during_a_move_meets_the_node_in_one_of_its_directories() {
    MISSED.store(0, Ordering::Relaxed);
    let scenario = Scenario::<Context> {
        name: "h1: a find during a C API move",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["p/x/f", "p/y", "q/z"]).await;
                let moved = tree.id("p/x");
                Context {
                    tree,
                    moved,
                    failures: AtomicUsize::new(0),
                }
            })
        }),
        ops: vec![move_into("q"), find_in_either()],
        cancellable: vec![],
        check: Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                assert_eq!(
                    MISSED.load(Ordering::Relaxed),
                    0,
                    "a find met x in neither directory"
                );
                let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
                assert_eq!(report.at("q/x").map(|(id, _)| id), Some(context.moved));
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A move of `p/x` into `q` alongside `other` on the tree `paths`, explored as [`explore`]
/// does.
fn explore_alongside(
    name: &'static str,
    paths: &'static [&'static str],
    other: Op<Context>,
    ends: &'static [&'static str],
    check: fn(&invariants::Report),
) {
    explore(Alongside {
        name,
        paths,
        dirty: &[],
        mover: move_into("q"),
        other,
        ends,
        check,
    });
}

/// A move of `p/x` alongside another op.
struct Alongside {
    name: &'static str,
    /// The tree the scenario starts from.
    paths: &'static [&'static str],
    /// Nodes of the tree marked dirty before the ops run.
    dirty: &'static [&'static str],
    /// The move.
    mover: Op<Context>,
    other: Op<Context>,
    /// Where `x` may end.
    ends: &'static [&'static str],
    /// A check of the report beyond the invariants and `ends`.
    check: fn(&invariants::Report),
}

/// Explores `alongside` to completion: no op fails, the invariants hold, `x` ends at one of
/// its `ends`, and its `check` holds on the report.
fn explore(alongside: Alongside) {
    let Alongside {
        name,
        paths,
        dirty,
        mover,
        other,
        ends,
        check,
    } = alongside;
    let scenario = Scenario::<Context> {
        name,
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::new(paths).await;
                for path in dirty {
                    ops::mark_dirty(&tree, tree.id(path)).await;
                }
                let moved = tree.id("p/x");
                Context {
                    tree,
                    moved,
                    failures: AtomicUsize::new(0),
                }
            })
        }),
        ops: vec![mover, other],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                assert_eq!(context.failures.load(Ordering::Relaxed), 0, "an op failed");
                let report = crate::holding(&tree.state, &tree.repository, Rules::CLOSED).await;
                let at = report.paths.get(&context.moved).map(String::as_str);
                assert!(
                    at.is_some_and(|at| ends.contains(&at)),
                    "x ends at {at:?}, not one of {ends:?}"
                );
                check(&report);
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A move of `x`, the head of `p`'s chain, while a file is added to `p`, which prepends it
/// before `x`: the move finds `x` behind the added file where the add landed first, and the
/// add keeps its file where the move took `x` first.
#[test]
fn a_move_of_a_chains_head_and_an_add_into_its_directory_keep_both() {
    explore_alongside(
        "h1: a move of a chain's head against an add",
        // `x`, made last, heads `p`'s chain.
        &["p/y", "p/x/f", "q/"],
        Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                if ops::staged_add(tree, tree.id("p"), ops::file("w"), "w")
                    .await
                    .is_none()
                {
                    context.failures.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
        &["q/x"],
        |report| {
            assert!(report.at("p/w").is_some(), "the added file is in p");
            assert!(report.at("p/y").is_some(), "y stays in p");
            assert!(report.at("q/x/f").is_some(), "x's child travels with it");
        },
    );
}

/// A move of `x` into `q` while a file is added to `q`: both land, in either order.
#[test]
fn a_move_and_an_add_into_its_destination_keep_both() {
    explore_alongside(
        "h1: a move against an add into its destination",
        &["p/x", "q/z"],
        Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                if ops::staged_add(tree, tree.id("q"), ops::file("w"), "w")
                    .await
                    .is_none()
                {
                    context.failures.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
        &["q/x"],
        |report| {
            assert!(report.at("q/w").is_some(), "the added file is in q");
            assert!(report.at("q/z").is_some(), "z stays in q");
        },
    );
}

/// A move of `x` while the node before it in `p`'s chain is detached, which relinks the chain
/// around it: `x` lands in `q`, and `p` holds neither.
#[test]
fn a_move_and_a_detach_of_the_node_before_it_keep_the_chain() {
    explore_alongside(
        "h1: a move against a detach of its predecessor",
        // Each node made is prepended: `p`'s chain is `z`, `y`, `x`.
        &["p/x/f", "p/y", "p/z", "q/"],
        Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                if !ops::unserved_detach(tree, tree.id("p/y")).await {
                    context.failures.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
        &["q/x"],
        |report| {
            assert!(report.at("p/y").is_none(), "y is detached");
            assert!(report.at("p/z").is_some(), "z stays in p");
        },
    );
}

/// A move of file `x` into `q` while an unserved reattach, under no permit, moves it into `r`:
/// a move finding `x` gone from the parent it read follows it to where it went, so both
/// succeed and `x` ends in one of the two.
#[test]
fn a_move_and_a_reattach_of_one_file_both_succeed() {
    explore_alongside(
        "h1: a move against an unserved reattach of its file",
        &["p/x", "p/y", "q/", "r/"],
        Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                let moved = tree
                    .state
                    .node_reattach(tree.repository.clone(), context.moved, tree.id("r"), "x")
                    .await;
                if !matches!(moved, Ok(AttachOutcome::Attached(_))) {
                    eprintln!("the reattach into r answered {moved:?}");
                    context.failures.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
        &["q/x", "r/x"],
        |report| {
            assert!(report.at("p/y").is_some(), "y stays in p");
            // Ending in `q`, `x` was moved there from `r`, which the move then marks.
            if report.at("q/x").is_some() {
                let r = report.at("r").expect("r").1;
                assert!(r.is_staged(), "the move marks the directory x left");
            }
        },
    );
}

/// A walk of `dir`'s children, parking between children: each child yielded stands under `dir`
/// as the walk reads it, no child is yielded twice unless the walk says it may repeat, and each
/// of `kept`, children that never move, is yielded.
fn walk_children(dir: &'static str, kept: &'static [&'static str]) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let tree = &context.tree;
            let parent = tree.id(dir);
            let mut children =
                StateNodeChildrenIterator::new(tree.state.clone(), tree.repository.clone(), parent)
                    .await
                    .expect("the walk begins");
            let mut seen = Vec::new();
            while let Some((child, node)) = children.next().await.expect("the walk steps") {
                assert_eq!(
                    node.parent, parent,
                    "child {child} of {dir} as the walk read it"
                );
                seen.push(child);
                hooks::reached(Point::Named("walk")).await;
            }
            let mut unique = seen.clone();
            unique.sort_unstable();
            unique.dedup();
            assert!(
                unique.len() == seen.len() || children.may_repeat(),
                "a walk of {dir} yielded {seen:?} and says it does not repeat"
            );
            for path in kept {
                assert!(
                    seen.contains(&tree.id(path)),
                    "the walk of {dir} missed {path}"
                );
            }
        })
    })
}

/// A move of `x` out of the middle of `p`'s chain while `p`'s children are walked: the walk
/// meets `x` in `p` or not at all, and yields the children that stay.
#[test]
fn a_walk_of_the_directory_a_move_leaves_yields_the_children_that_stay() {
    explore_alongside(
        "h1: a move against a walk of its old directory",
        // Each node made is prepended: `p`'s chain is `y`, `x`, `w`.
        &["p/w", "p/x/f", "p/y", "q/z"],
        walk_children("p", &["p/w", "p/y"]),
        &["q/x"],
        |_| {},
    );
}

/// A move of `x` into `q` while `q`'s children are walked: the walk meets `x` in `q` or not at
/// all, and yields the child that was there.
#[test]
fn a_walk_of_the_directory_a_move_joins_yields_the_child_there() {
    explore_alongside(
        "h1: a move against a walk of its new directory",
        &["p/x/f", "p/y", "q/z"],
        walk_children("q", &["q/z"]),
        &["q/x"],
        |_| {},
    );
}

/// Refusals of a move of a directory into the other's subtree.
static REFUSED: AtomicUsize = AtomicUsize::new(0);

/// A checked move of `p` into `q` and an unserved reattach of `q` into `p`: each checks the
/// other is not in its subtree under the moves permit, so one goes first and the second is
/// refused, and the two never form a cycle cut off from the root.
#[test]
fn a_checked_move_and_a_directory_reattach_never_form_a_cycle() {
    REFUSED.store(0, Ordering::Relaxed);
    let refused = |moved: Result<(), lore_revision::state::StateError>| match moved {
        Ok(()) => {}
        Err(error) if error.is_invalid_arguments() => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => panic!("a directory move fails: {error}"),
    };
    let scenario = Scenario::<Context> {
        name: "h1: a checked move and a directory reattach into each other",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["p/f", "q/g"]).await;
                let moved = tree.id("p");
                Context {
                    tree,
                    moved,
                    failures: AtomicUsize::new(0),
                }
            })
        }),
        ops: vec![
            Box::new(move |context: Arc<Context>| {
                Box::pin(async move {
                    let tree = &context.tree;
                    let permit = tree.state.moves_permit().await.expect("the moves permit");
                    let moved = tree
                        .state
                        .node_move_staged(
                            tree.repository.clone(),
                            tree.id("p"),
                            tree.id("q"),
                            ("p", lore_storage::hash::hash_string("p")),
                            &permit,
                            false,
                        )
                        .await;
                    refused(moved);
                })
            }),
            Box::new(move |context: Arc<Context>| {
                Box::pin(async move {
                    let tree = &context.tree;
                    let moved = tree
                        .state
                        .node_reattach(tree.repository.clone(), tree.id("q"), tree.id("p"), "q")
                        .await;
                    refused(moved.map(|_| ()));
                })
            }),
        ],
        cancellable: vec![],
        check: Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                assert_eq!(REFUSED.swap(0, Ordering::Relaxed), 1, "one move is refused");
                let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
                assert!(
                    report.at("q/p/f").is_some() || report.at("p/q/g").is_some(),
                    "one directory ends in the other, under the root"
                );
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A rename of `x` where it stands, in `p`, while an unserved reattach moves it into `r`: a
/// rename finding `x` gone from the parent it read follows it, moving it back under its new
/// name, so both succeed.
#[test]
fn a_rename_and_a_reattach_of_one_file_both_succeed() {
    explore(Alongside {
        name: "h1: a rename in place against an unserved reattach of its file",
        paths: &["p/x", "p/y", "r/"],
        dirty: &[],
        mover: move_to("p", "w"),
        other: Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                let moved = tree
                    .state
                    .node_reattach(tree.repository.clone(), context.moved, tree.id("r"), "x")
                    .await;
                if !matches!(moved, Ok(AttachOutcome::Attached(_))) {
                    eprintln!("the reattach into r answered {moved:?}");
                    context.failures.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
        ends: &["p/w", "r/x"],
        check: |report| {
            assert!(report.at("p/y").is_some(), "y stays in p");
        },
    });
}

/// A move of `p/x` into `destination` as `lore dirty move` makes one, on a State no other mover
/// changes ([`ops::dirty_move`]).
fn dirty_move_into(destination: &'static str) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let tree = &context.tree;
            let to = tree.id(destination);
            let moved = ops::dirty_move(tree, context.moved, to, NameChange::Keep).await;
            if let Err(error) = moved {
                eprintln!("the move into {destination} failed: {error}");
                context.failures.fetch_add(1, Ordering::Relaxed);
            }
        })
    })
}

/// A move of `x` into `q` while a clear of `q`'s only dirty child climbs into `q`: the move puts
/// `x` in `q`'s chain before it marks `q`, so the clear's second walk meets `x` dirty and keeps `q`
/// dirty, or the mark finds `q` clean and dirties it again. The mover marks after it moves, as the
/// dirty and stage movers do; the C API's, which marks first, never runs beside a clear (C9).
/// Beyond the contract, which runs a trusted move beside readers alone: it covers H1's order of the
/// move and its mark against the clear's walks.
#[test]
fn a_move_and_a_clear_of_its_destination_keep_dirty_closure() {
    explore(Alongside {
        name: "h1: a move against a clear of its destination",
        paths: &["p/x", "q/z"],
        dirty: &["q/z"],
        mover: dirty_move_into("q"),
        other: Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let tree = &context.tree;
                ops::scan_clear(tree, tree.id("q/z")).await;
            })
        }),
        ends: &["q/x"],
        check: |report| {
            let q = report.at("q").expect("q").1;
            assert!(q.is_dirty(), "q holds the moved x, dirty");
        },
    });
}
