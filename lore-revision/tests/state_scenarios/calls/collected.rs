// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The dirty paths explored beside the changes that decide what they collect: a dirty mark of a
//! file, a clear of a dirty one, a C API delete of one, a C API move of a dirty one out, a respell
//! of a dirty one, a clear of a dirty file below a dirty directory, a mark beside a directory left
//! a dirty delete, a C API add of a directory, a dirty add, and of a file into an empty one, the
//! root, a mark below the State's root; and, at a listing's block crossing, a clear and a C API
//! move out of a dirty file already listed, a served rename repositioning a dirty one, and a C API
//! move of a dirty file between two directories the collection lists. Each run replays through the
//! model, which holds each path to the head and the name reads that spell it, and the collection
//! to the files and directories dirty in place below its root throughout from its head read.

use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::Ext;
use lore_revision::state::Mark;

use super::OpFn;
use super::across_after;
use super::changed;
use super::explored_within;
use super::met_after_head;
use super::tree;
use crate::harness::catalogue;
use crate::harness::catalogue::call::path_spelled;
use crate::harness::catalogue::call::path_spellings;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::directory;
use crate::harness::catalogue::ops::file;
use crate::harness::fixtures::Tree;
use crate::harness::scheduler::Setup;

/// A collection of the dirty paths below `d/`.
pub(super) fn collect_d() -> OpFn {
    |tree| {
        Box::pin(async move {
            let _ = ops::dirty_paths(&tree, tree.id("d"), "d").await;
        })
    }
}

/// The paths a collection below `d/` reported in each run beside `changes` on the tree `setup`
/// builds, within `preemptions` per run where given, and whether another task's record came
/// between its root's head read and its end.
pub(super) fn collected_beside(
    preemptions: Option<usize>,
    setup: Setup<Tree>,
    changes: Vec<OpFn>,
) -> Vec<(Vec<u64>, bool)> {
    collected_by(preemptions, setup, collect_d(), changes)
}

/// [`collected_beside`] for the collection `collect` makes.
pub(super) fn collected_by(
    preemptions: Option<usize>,
    setup: Setup<Tree>,
    collect: OpFn,
    changes: Vec<OpFn>,
) -> Vec<(Vec<u64>, bool)> {
    explored_within(
        preemptions,
        setup,
        (collect, &catalogue::DIRTY_PATHS),
        changes,
        |run, call| (call.paths.clone(), met_after_head(run, call)),
    )
}

/// Whether `runs` collected each of `made` in some run, and nothing else.
pub(super) fn collected(runs: &[(Vec<u64>, bool)], made: &[&[&str]]) -> bool {
    let made: Vec<Vec<u64>> = made.iter().map(|paths| path_spellings(paths)).collect();
    made.iter()
        .all(|paths| runs.iter().any(|(run, _)| run == paths))
        && runs.iter().all(|(run, _)| made.contains(run))
}

/// A collection below `d/` beside a dirty mark of `d/f`: nothing before the mark, `f` after.
/// Every schedule, as a listing of one block has no point within it.
#[test]
fn a_collection_beside_a_dirty_mark_collects_the_file_or_not() {
    let runs = collected_beside(
        None,
        tree(&["d/f", "d/g"]),
        vec![|tree| Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("d/f")).await })],
    );
    assert!(collected(&runs, &[&[], &["f"]]), "{runs:?}");
}

/// A collection below `d/` beside a clear of `d/f`, which a mark left dirty: `f` before the clear,
/// nothing after.
#[test]
fn a_collection_beside_a_clear_collects_the_file_before_it() {
    let runs = collected_beside(
        None,
        changed(&["d/f", "d/g"], |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/f")).await })
        }),
        vec![|tree| Box::pin(async move { ops::scan_clear(&tree, tree.id("d/f")).await })],
    );
    assert!(collected(&runs, &[&["f"], &[]]), "{runs:?}");
}

/// A collection below `d/` beside a C API delete of `d/f`, which leaves it staged and dirty for
/// deletion: `f` after the delete, nothing before.
#[test]
fn a_collection_beside_a_delete_collects_the_file_after_it() {
    let runs = collected_beside(
        None,
        tree(&["d/f", "d/g"]),
        vec![|tree| {
            Box::pin(async move {
                let (f, d) = (tree.id("d/f"), tree.id("d"));
                ops::staged_delete_level(&tree, f, d).await;
            })
        }],
    );
    assert!(collected(&runs, &[&[], &["f"]]), "{runs:?}");
}

/// A collection below `d/` beside a C API move of `d/f`, which a mark left dirty, into `x`: `f`
/// before the move, nothing after, `x` outside the root.
#[test]
fn a_collection_beside_a_move_out_collects_the_file_before_it() {
    let runs = collected_beside(
        None,
        changed(&["d/f", "d/g", "x/"], |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/f")).await })
        }),
        vec![|tree| {
            Box::pin(async move {
                let (f, x) = (tree.id("d/f"), tree.id("x"));
                ops::staged_move(&tree, f, x, "f").await;
            })
        }],
    );
    assert!(collected(&runs, &[&["f"], &[]]), "{runs:?}");
}

/// A collection below `d/` beside a respell of `d/f`, which a mark left dirty, to `F`: either
/// spelling, as it read one.
#[test]
fn a_collection_beside_a_respell_collects_either_spelling() {
    let runs = collected_beside(
        None,
        changed(&["d/f", "d/g"], |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/f")).await })
        }),
        vec![|tree| {
            Box::pin(async move { ops::rename_then_mark(&tree, tree.id("d/f"), "F").await })
        }],
    );
    assert!(collected(&runs, &[&["f"], &["F"]]), "{runs:?}");
}

/// A collection below `d/` beside a clear of `d/e/f`, which a mark left dirty with `e`, the clear
/// taking `e` with it: `e/f` before the clear, descending into `e`, nothing after.
#[test]
fn a_collection_descends_into_a_dirty_directory_beside_a_clear_below_it() {
    let runs = collected_beside(
        None,
        changed(&["d/e/f", "d/g"], |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/e/f")).await })
        }),
        vec![|tree| Box::pin(async move { ops::scan_clear(&tree, tree.id("d/e/f")).await })],
    );
    assert!(collected(&runs, &[&["e/f"], &[]]), "{runs:?}");
}

/// A collection below `d/` whose `e`, holding a dirty `h`, a setup left a dirty delete, beside a
/// dirty mark of `d/f`: `e` and not below it, as the stage deletes the whole subtree, `f` after the
/// mark.
#[test]
fn a_collection_reports_a_dirty_delete_and_not_below_it() {
    let runs = collected_beside(
        None,
        changed(&["d/e/h", "d/f"], |t| {
            Box::pin(async move {
                ops::mark_dirty(t, t.id("d/e/h")).await;
                let deleted = Mark::dirty(
                    NodeFlags::DirtyDelete,
                    Ext::Set(NodeExtFlags::NoFlags),
                    true,
                );
                (t.state.node_mark(&t.repository, t.id("d/e"), deleted).await)
                    .expect("a dirty delete marks");
            })
        }),
        vec![|tree| Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("d/f")).await })],
    );
    assert!(collected(&runs, &[&["e"], &["e", "f"]]), "{runs:?}");
}

/// A collection below a `d/` whose chain crosses blocks, its `x`, in the later block, left dirty by
/// a mark, beside a clear of `x`: where the clear lands at the listing's crossing, the walk has
/// listed `x` and reads its words clean, collecting nothing, which the model admits of a file that
/// stopped being dirty. Every schedule: three preemptions a run reach them all.
#[test]
fn a_collection_beside_a_clear_of_a_file_it_listed_collects_nothing() {
    let runs = collected_beside(
        Some(3),
        across_after(false, |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/x")).await })
        }),
        vec![|tree| Box::pin(async move { ops::scan_clear(&tree, tree.id("d/x")).await })],
    );
    assert!(
        runs.contains(&(path_spellings(&[]), true))
            && runs.iter().any(|(run, _)| *run == path_spellings(&["x"])),
        "{runs:?}"
    );
}

/// A collection below a `d/` whose chain crosses blocks, its `f`, the last of the chain, left dirty
/// by a mark, beside a served rename of `f` within `d`, which puts it at the chain's head: where
/// the rename lands at the listing's crossing, the walk has passed the head and never meets `f`,
/// collecting nothing, which the model admits of a repositioned file. Every schedule: three
/// preemptions a run reach them all.
#[test]
fn a_collection_beside_a_reposition_may_pass_the_file_over() {
    let runs = collected_beside(
        Some(3),
        across_after(true, |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/f")).await })
        }),
        vec![|tree| {
            Box::pin(async move {
                let (f, d) = (tree.id("d/f"), tree.id("d"));
                assert!(ops::served_rename(&tree, f, d, "F").await);
            })
        }],
    );
    assert!(
        runs.contains(&(Vec::new(), true))
            && runs.iter().any(|(run, _)| *run == path_spellings(&["f"])),
        "{runs:?}"
    );
}

/// A collection below `d/` beside a C API add of directory `d/t`, a dirty add: `t` after the add,
/// nothing before.
#[test]
fn a_collection_beside_an_add_of_a_directory_reports_it_as_a_dirty_add() {
    let runs = collected_beside(
        None,
        tree(&["d/f"]),
        vec![|tree| {
            Box::pin(async move {
                let added = ops::staged_add(&tree, tree.id("d"), directory("t"), "t").await;
                assert!(added.is_some(), "'d/t' adds");
            })
        }],
    );
    assert!(collected(&runs, &[&[], &["t"]]), "{runs:?}");
}

/// A collection below `d/t`, an empty directory a C API add made, a dirty add, beside a C API add
/// of a file `u` into it: its own empty path before the add, `u` after.
#[test]
fn a_collection_below_an_empty_added_directory_reports_its_root() {
    let runs = collected_by(
        None,
        Box::new(|| {
            Box::pin(async {
                let mut tree = Tree::new(&["d/f"]).await;
                let t = ops::staged_add(&tree, tree.id("d"), directory("t"), "t")
                    .await
                    .expect("'d/t' adds");
                tree.nodes.push(("d/t".to_string(), t));
                tree
            })
        }),
        |tree| {
            Box::pin(async move {
                let _ = ops::dirty_paths(&tree, tree.id("d/t"), "d/t").await;
            })
        },
        vec![|tree| {
            Box::pin(async move {
                let added = ops::staged_add(&tree, tree.id("d/t"), file("u"), "u").await;
                assert!(added.is_some(), "'d/t/u' adds");
            })
        }],
    );
    assert!(collected(&runs, &[&[""], &["u"]]), "{runs:?}");
}

/// A collection below a `d/` whose chain crosses blocks, its `x`, in the later block, left dirty by
/// a mark, beside a C API move of `x` into `e`: where the move lands at the listing's crossing, the
/// walk has listed `x` and names it dirty in `e`, reporting it below `d`, which the model admits of
/// a child that joined where it stands since `d`'s head read. Every schedule: three preemptions a
/// run reach them all.
#[test]
fn a_collection_beside_a_move_out_of_a_file_it_listed_reports_it() {
    let runs = collected_beside(
        Some(3),
        across_after(false, |t| {
            Box::pin(async move { ops::mark_dirty(t, t.id("d/x")).await })
        }),
        vec![|tree| {
            Box::pin(async move {
                let (x, e) = (tree.id("d/x"), tree.id("e"));
                ops::staged_move(&tree, x, e, "x").await;
            })
        }],
    );
    assert!(
        runs.contains(&(path_spellings(&["x"]), true))
            && runs.iter().any(|(run, _)| run.is_empty()),
        "{runs:?}"
    );
}

/// A collection below a `d/` holding `p1` and `p2`, each holding a dirty file, `p2`'s chain
/// crossing blocks and `p2` listed first, beside a C API move of `p1`'s `f` into `p2`: where the
/// move lands at `p2`'s crossing, its listing has passed the head the move puts `f` at, and `p1`,
/// listed after, holds it no more, so the collection misses `f`, which the model admits of a node
/// that moved between two directories of the subtree since its root's head read. Every schedule:
/// three preemptions a run reach them all.
#[test]
fn a_collection_beside_a_move_between_its_directories_may_miss_the_file() {
    let runs = collected_beside(
        Some(3),
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["d/p2/a", "d/p1/f"], "fill", &["d/p2/b"]).await;
                for path in ["d/p1/f", "d/p2/a"] {
                    ops::mark_dirty(&tree, tree.id(path)).await;
                }
                tree
            })
        }),
        vec![|tree| {
            Box::pin(async move {
                let (f, p2) = (tree.id("d/p1/f"), tree.id("d/p2"));
                ops::staged_move(&tree, f, p2, "f").await;
            })
        }],
    );
    assert!(
        runs.iter()
            .any(|(run, _)| *run == path_spellings(&["p2/a"]))
            && runs
                .iter()
                .any(|(run, _)| run.contains(&path_spelled("p1/f"))),
        "no run missed the moved file, or none collected it: {runs:?}"
    );
}

/// A collection from the State's root, its base the empty path, beside a dirty mark of `d/f`,
/// which marks `d` with it: `d/f` after the mark, descending into `d`, nothing before.
#[test]
fn a_collection_from_the_root_spells_whole_paths() {
    let runs = collected_by(
        None,
        tree(&["d/f", "d/g"]),
        |tree| {
            Box::pin(async move {
                let _ = ops::dirty_paths(&tree, ROOT_NODE, "").await;
            })
        },
        vec![|tree| Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("d/f")).await })],
    );
    assert!(collected(&runs, &[&[], &["d/f"]]), "{runs:?}");
}
