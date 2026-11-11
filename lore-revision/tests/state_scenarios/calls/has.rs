// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The has-children reads explored beside the changes that decide what they answer: a dirty mark
//! of a child, a clear of a dirty one, a C API delete staging one, a C API move of a dirty one out
//! of the directory, each trusted move of the directory itself, and, at the listing's block
//! crossing, a served rename repositioning a dirty one, a C API move of one already listed out,
//! and a clear of a dirty one already listed. Each run replays through the model, which holds each
//! answer to the directory from the listing's head read as its walk and its reads of the children
//! found it.

use super::OpFn;
use super::across;
use super::across_after;
use super::answers_beside;
use super::changed;
use super::explored_within;
use super::met_after_head;
use super::moves_of_d;
use super::tree;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::Flagged;
use crate::harness::fixtures::Tree;
use crate::harness::scheduler::Setup;

/// The has-children read of `d/` asking for `flagged`.
fn read_of_d(flagged: Flagged) -> OpFn {
    match flagged {
        Flagged::Staged => |tree| {
            Box::pin(async move {
                let _ = ops::has_children(&tree, tree.id("d"), Flagged::Staged).await;
            })
        },
        Flagged::Dirty => |tree| {
            Box::pin(async move {
                let _ = ops::has_children(&tree, tree.id("d"), Flagged::Dirty).await;
            })
        },
        Flagged::StagedOrDirty => |tree| {
            Box::pin(async move {
                let _ = ops::has_children(&tree, tree.id("d"), Flagged::StagedOrDirty).await;
            })
        },
    }
}

/// Each has-children read of `d/`, with what it asks for.
fn reads_of_d() -> [(OpFn, Flagged); 3] {
    Flagged::EVERY.map(|flagged| (read_of_d(flagged), flagged))
}

/// A has-children read's answer.
fn has(has: bool) -> Outcome {
    Outcome::Answer(Answer::Has(has))
}

/// Whether `runs` made each of the answers `made` in some run, and no other.
fn answered(runs: &[Outcome], made: &[bool]) -> bool {
    made.iter().all(|&answer| runs.contains(&has(answer)))
        && runs
            .iter()
            .all(|run| made.iter().any(|&answer| *run == has(answer)))
}

/// The answers a read asking for `flagged` makes beside a dirty child coming or going: both, but a
/// staged read's none.
fn beside_a_dirty_child(flagged: Flagged) -> &'static [bool] {
    match flagged {
        Flagged::Staged => &[false],
        Flagged::Dirty | Flagged::StagedOrDirty => &[false, true],
    }
}

/// Each has-children read of `d/` beside a dirty mark of `d/f`: a dirty or a staged-or-dirty read
/// finds it in some runs and not in others; a staged read never finds it.
#[test]
fn each_has_children_read_beside_a_dirty_mark_finds_the_child_or_not() {
    for (read, flagged) in reads_of_d() {
        let runs = answers_beside(
            None,
            tree(&["d/f", "d/g"]),
            (read, flagged.kind()),
            vec![|tree| {
                Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("d/f")).await })
            }],
        );
        let made = beside_a_dirty_child(flagged);
        assert!(answered(&runs, made), "{}: {runs:?}", flagged.kind().name);
    }
}

/// Each has-children read of `d/` beside a clear of `d/f`, which a mark left dirty: a dirty or a
/// staged-or-dirty read finds it before the clear and not after; a staged read never finds it.
#[test]
fn each_has_children_read_beside_a_clear_of_its_dirty_child_finds_it_before() {
    for (read, flagged) in reads_of_d() {
        let runs = answers_beside(
            None,
            changed(&["d/f", "d/g"], |t| {
                Box::pin(async move { ops::mark_dirty(t, t.id("d/f")).await })
            }),
            (read, flagged.kind()),
            vec![|tree| Box::pin(async move { ops::scan_clear(&tree, tree.id("d/f")).await })],
        );
        let made = beside_a_dirty_child(flagged);
        assert!(answered(&runs, made), "{}: {runs:?}", flagged.kind().name);
    }
}

/// Each has-children read of `d/` beside a C API delete of `d/f`, which leaves it staged and dirty
/// for deletion: each finds it after the delete and not before.
#[test]
fn each_has_children_read_beside_a_delete_of_its_child_finds_it_after() {
    for (read, flagged) in reads_of_d() {
        let runs = answers_beside(
            None,
            tree(&["d/f", "d/g"]),
            (read, flagged.kind()),
            vec![|tree| {
                Box::pin(async move {
                    let (f, d) = (tree.id("d/f"), tree.id("d"));
                    ops::staged_delete_level(&tree, f, d).await;
                })
            }],
        );
        assert!(
            answered(&runs, &[false, true]),
            "{}: {runs:?}",
            flagged.kind().name
        );
    }
}

/// A dirty read of a `d/` whose chain crosses blocks, its `f`, the last of the chain, left dirty by
/// a mark, beside a served rename of `f` within `d`, which puts it at the chain's head: where the
/// rename lands at the listing's crossing, the walk has passed the head and never meets `f`, so the
/// read finds no dirty child, which the model admits of a repositioned child. Every schedule: three
/// preemptions a run reach them all.
#[test]
fn a_has_dirty_children_read_beside_a_reposition_of_its_dirty_child_may_pass_it_over() {
    let runs = answers_beside(
        Some(3),
        across_after(true, |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/f")).await })
        }),
        (read_of_d(Flagged::Dirty), Flagged::Dirty.kind()),
        vec![|tree| {
            Box::pin(async move {
                let (f, d) = (tree.id("d/f"), tree.id("d"));
                assert!(ops::served_rename(&tree, f, d, "F").await);
            })
        }],
    );
    assert!(answered(&runs, &[false, true]), "{runs:?}");
}

/// A dirty read of `d/` beside a C API move of `d/f`, which a mark left dirty, into `e`: it finds
/// `f` before the move and not after, the move leaving `d`'s other child clean. Every schedule, as
/// a listing of one block has no point within it.
#[test]
fn a_has_dirty_children_read_beside_a_move_of_its_dirty_child_out_finds_it_before() {
    let runs = answers_beside(
        None,
        changed(&["d/f", "d/g", "e/"], |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/f")).await })
        }),
        (read_of_d(Flagged::Dirty), Flagged::Dirty.kind()),
        vec![|tree| {
            Box::pin(async move {
                let (f, e) = (tree.id("d/f"), tree.id("e"));
                ops::staged_move(&tree, f, e, "f").await;
            })
        }],
    );
    assert!(answered(&runs, &[false, true]), "{runs:?}");
}

/// Each has-children read of `d/` beside each trusted move of `d` itself into `e`: beside the
/// dirty and the stage move, which mark each node below `d` moved as they go, dirty or staged, a
/// read asking for that flag finds a child marked where the marks landed first and none where
/// after; a read asking for the other flag, or any read beside the realize move, which marks no
/// subtree, finds none.
#[test]
fn each_has_children_read_beside_a_move_of_its_directory_finds_the_marks_landed() {
    for (moved, by) in moves_of_d() {
        let marks = if std::ptr::eq(by, &catalogue::DIRTY_MOVE) {
            Some(Flagged::Dirty)
        } else if std::ptr::eq(by, &catalogue::STAGE_MOVE) {
            Some(Flagged::Staged)
        } else {
            None
        };
        for (read, flagged) in reads_of_d() {
            let runs = answers_beside(
                None,
                tree(&["d/f", "d/g", "e/"]),
                (read, flagged.kind()),
                vec![moved],
            );
            let finds = matches!(
                (flagged, marks),
                (Flagged::Staged, Some(Flagged::Staged))
                    | (Flagged::Dirty, Some(Flagged::Dirty))
                    | (Flagged::StagedOrDirty, Some(_))
            );
            let made: &[bool] = if finds { &[false, true] } else { &[false] };
            assert!(
                answered(&runs, made),
                "{} beside a {}: {runs:?}",
                flagged.kind().name,
                by.name
            );
        }
    }
}

/// Each run's answer of `read`, asking for `flagged`, beside `change` on the tree `setup` builds,
/// within three preemptions, which reach every schedule, and whether another task's record came
/// between its listing's head read and its end.
fn answers_met(
    setup: Setup<Tree>,
    (read, flagged): (OpFn, Flagged),
    change: OpFn,
) -> Vec<(Outcome, bool)> {
    explored_within(
        Some(3),
        setup,
        (read, flagged.kind()),
        vec![change],
        |run, call| (call.outcome.clone(), met_after_head(run, call)),
    )
}

/// A staged or a staged-or-dirty read of a `d/` whose chain crosses blocks beside a C API move of
/// `d/x`, in the later block, into `e`, which stages it: the walk reads each child's record as it
/// reaches it, `x` before its move, unstaged, or not at all once it left the chain, so no schedule
/// finds a staged child, which the model admits. Every schedule: three preemptions a run reach them
/// all.
#[test]
fn a_staged_read_beside_a_move_out_of_a_child_it_listed_finds_none() {
    for flagged in [Flagged::Staged, Flagged::StagedOrDirty] {
        let runs = answers_met(across(false), (read_of_d(flagged), flagged), |tree| {
            Box::pin(async move {
                let (x, e) = (tree.id("d/x"), tree.id("e"));
                ops::staged_move(&tree, x, e, "x").await;
            })
        });
        assert!(
            !runs.is_empty() && runs.iter().all(|(answer, _)| *answer == has(false)),
            "{}: {runs:?}",
            flagged.kind().name
        );
    }
}

/// A dirty read of a `d/` whose chain crosses blocks, its `x`, in the later block, left dirty by a
/// mark, beside a clear of `x`: where the clear lands at the listing's crossing, the walk has
/// listed `x` and reads its words clean, finding no dirty child, which the model admits of a child
/// that stopped carrying the bit. Every schedule: three preemptions a run reach them all.
#[test]
fn a_dirty_read_beside_a_clear_of_a_child_it_listed_finds_none() {
    let runs = answers_met(
        across_after(false, |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/x")).await })
        }),
        (read_of_d(Flagged::Dirty), Flagged::Dirty),
        |tree| Box::pin(async move { ops::scan_clear(&tree, tree.id("d/x")).await }),
    );
    assert!(
        runs.contains(&(has(false), true)) && runs.iter().any(|(answer, _)| *answer == has(true)),
        "{runs:?}"
    );
}
