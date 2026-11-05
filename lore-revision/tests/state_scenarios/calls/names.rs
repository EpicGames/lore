// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The name reads and the paths explored beside the changes that decide what they answer: a
//! respell of the node, a rename changing its hash, a move of an ancestor and a respell of one.
//! Each run replays through the model, which holds each parent word and name read, recorded at a
//! `seq` of its own, to the model then, and each answer to its call's records.

use lore_revision::node::INVALID_NODE;

use super::OpFn;
use super::answers_beside;
use super::renamed_within;
use super::tree;
use crate::harness::catalogue;
use crate::harness::catalogue::Kind;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::spelling;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::NameRead;
use crate::harness::fixtures::Tree;

/// Each name read of `d/f`, with its kind.
fn name_reads_of_f() -> [(OpFn, &'static Kind); 4] {
    [
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::name_read(&tree, tree.id("d/f"), NameRead::Clone).await;
                })
            },
            NameRead::Clone.kind(),
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::name_read(&tree, tree.id("d/f"), NameRead::Ref).await;
                })
            },
            NameRead::Ref.kind(),
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::name_read(&tree, tree.id("d/f"), NameRead::RefOrSkip).await;
                })
            },
            NameRead::RefOrSkip.kind(),
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::name_read(&tree, tree.id("d/f"), NameRead::CloneOrSkip).await;
                })
            },
            NameRead::CloneOrSkip.kind(),
        ),
    ]
}

/// A name read's answer of `name`.
fn name(name: &str) -> Outcome {
    Outcome::Answer(Answer::Name(Some(spelling(name.as_bytes()))))
}

/// A path read's answer of `path`, or of none.
fn path(path: Option<&str>) -> Outcome {
    Outcome::Answer(Answer::Path(path.map(|path| spelling(path.as_bytes()))))
}

/// The pre-serving respell of the node at `path` to `name`, its name in another case.
async fn respelled(tree: &Tree, path: &str, name: &str) {
    ops::rename_then_mark(tree, tree.id(path), name).await;
}

/// Each name read of `d/f` beside a respell of it to `F` and beside a dirty move renaming it to `n`
/// within `d`: the name before the change in some runs and after it in others, each read the
/// model's then.
#[test]
fn each_name_read_beside_a_rename_reads_the_name_before_or_after_it() {
    let changes: [(OpFn, &str); 2] = [
        (
            |tree| Box::pin(async move { respelled(&tree, "d/f", "F").await }),
            "F",
        ),
        (
            |tree| Box::pin(async move { renamed_within(&tree, "d/f", "n").await }),
            "n",
        ),
    ];
    for (read, kind) in name_reads_of_f() {
        for (change, after) in changes {
            let runs = answers_beside(None, tree(&["d/f"]), (read, kind), vec![change]);
            assert!(
                runs.contains(&name("f")) && runs.contains(&name(after)),
                "{}: no run read 'f', or none read '{after}': {runs:?}",
                kind.name
            );
        }
    }
}

/// A comparison of `d/f`'s name with `f` beside a respell of it to `F`: true before the respell,
/// false after it, as it compares the stored spelling.
#[test]
fn a_name_comparison_beside_a_respell_answers_by_the_spelling_then() {
    let runs = answers_beside(
        None,
        tree(&["d/f"]),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::name_is(&tree, tree.id("d/f"), "f").await;
                })
            },
            &catalogue::NAME_IS,
        ),
        vec![|tree| Box::pin(async move { respelled(&tree, "d/f", "F").await })],
    );
    let compared = |is| Outcome::Answer(Answer::NameIs(is));
    assert!(
        runs.contains(&compared(true)) && runs.contains(&compared(false)),
        "no comparison before the respell, or none after it: {runs:?}"
    );
}

/// A path read from the root of an id no tree holds: the walk leaves the tree at once, which
/// `node_path` spells as the empty path, the model holding it to its records.
#[test]
fn a_path_read_of_an_id_no_tree_holds_is_the_empty_path() {
    let runs = answers_beside(
        None,
        tree(&["d/f"]),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::node_path(&tree, INVALID_NODE, None).await;
                })
            },
            &catalogue::NODE_PATH,
        ),
        vec![],
    );
    assert!(
        runs.iter().all(|run| *run == path(Some(""))),
        "a path read of an id no tree holds answered otherwise: {runs:?}"
    );
}

/// A C API move of `d/e`, holding `f`, into `x`.
fn move_e_into_x() -> OpFn {
    |tree| {
        Box::pin(async move {
            let (e, x) = (tree.id("d/e"), tree.id("x"));
            ops::staged_move(&tree, e, x, "e").await;
        })
    }
}

/// A path read of `d/e/f` from the root beside a move of `e` into `x`: the path before or after it,
/// the walk up its parents one hop at a time, each parent and name read the model's then; and one
/// below `d`, which holds `f` before the move and not after: its path or none. Every schedule:
/// thirteen preemptions a run reach them all, the move landing at each hop in some.
#[test]
fn a_path_read_beside_a_move_of_an_ancestor_reads_the_path_before_or_after_it() {
    let from_root = answers_beside(
        Some(13),
        tree(&["d/e/f", "x/"]),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::node_path(&tree, tree.id("d/e/f"), None).await;
                })
            },
            &catalogue::NODE_PATH,
        ),
        vec![move_e_into_x()],
    );
    assert!(
        from_root.contains(&path(Some("d/e/f"))) && from_root.contains(&path(Some("x/e/f"))),
        "no path read before the move, or none after it: {from_root:?}"
    );
    let below = answers_beside(
        Some(13),
        tree(&["d/e/f", "x/"]),
        (
            |tree| {
                Box::pin(async move {
                    let (f, d) = (tree.id("d/e/f"), tree.id("d"));
                    let _ = ops::node_path(&tree, f, Some(d)).await;
                })
            },
            &catalogue::NODE_PATH_BELOW,
        ),
        vec![move_e_into_x()],
    );
    assert!(
        below.contains(&path(Some("e/f"))) && below.contains(&path(None)),
        "no path read below 'd' before the move, or none after it: {below:?}"
    );
}

/// A path read of `d/e/f` beside a respell of `d` to `D`: either spelling of the path, each name
/// read the model's then. Every schedule.
#[test]
fn a_path_read_beside_a_respell_of_an_ancestor_reads_either_spelling() {
    let runs = answers_beside(
        None,
        tree(&["d/e/f"]),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::node_path(&tree, tree.id("d/e/f"), None).await;
                })
            },
            &catalogue::NODE_PATH,
        ),
        vec![|tree| Box::pin(async move { respelled(&tree, "d", "D").await })],
    );
    assert!(
        runs.contains(&path(Some("d/e/f"))) && runs.contains(&path(Some("D/e/f"))),
        "no path read before the respell, or none after it: {runs:?}"
    );
}
