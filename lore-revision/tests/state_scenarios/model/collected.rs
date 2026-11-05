// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The dirty paths held to the model (`collect_dirty_paths`): a collection lists each directory it
//! descends into from a head read of its own, then reads each child's words and names each dirty
//! one, so each path it reports is the names it read, one hop at a time, from its root down: a
//! named file's, a named directory's it reported as a dirty add or a dirty delete, and its empty
//! root's where it holds a dirty add. In a scheduler's run nothing lands between a child's words
//! and its name, so each child named carried then what its answer says; and every node in place,
//! named and dirty below dirty directories throughout from the root's head read is among its paths.

use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::node::hooks::log::Section;

use super::A;
use super::B;
use super::F;
use super::G;
use super::H;
use super::L;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::flagging_of;
use super::gated;
use super::head;
use super::head_of;
use super::moved;
use super::moved_out;
use super::name_read;
use super::renamed;
use super::replayed_with;
use super::repositioned;
use super::rewritten_of;
use super::section;
use super::snapshot;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::path_spellings;

/// A collection below `root` by task 0 over `[begin, end)`, reporting `paths`, each relative to
/// `root`.
pub(super) fn collect(root: NodeID, span: (u64, u64), paths: &[&str]) -> Call {
    Call {
        target: Some(root),
        paths: path_spellings(paths),
        ..base_call(
            &catalogue::DIRTY_PATHS,
            span,
            Outcome::Answer(Answer::Collected),
        )
    }
}

const NAMED_FILE: &str = "a file, at a path it did not report";
pub(super) const UNNAMED: &str = "reported a path no child it named spells";
const UNLISTED: &str = "which it never named";
const CLEAN: &str = "the model holding it clean as it read the name";
const ELSEWHERE: &str = "the model holding it elsewhere since before";
pub(super) const LEFT_OUT: &str =
    "left out a node in place, named and dirty below dirty directories";

/// A collection reports each file it named, at the path its name reads spell down from its root,
/// and no path no child it named spells; and descends into no directory it never named.
#[test]
fn a_collection_reports_what_it_named_and_nothing_else() {
    let made = snapshot();
    let dirty_f = flagging_of(&made, 1, F, NodeFlags::Dirty);
    let named_f = [dirty_f.clone(), head(2, 0), name_read(3, F, "f")].concat();
    clean(&replayed_with(
        &made,
        &named_f,
        vec![collect(A, (2, 5), &["f"])],
    ));
    caught(
        &replayed_with(&made, &named_f, vec![collect(A, (2, 5), &[])]),
        5,
        NAMED_FILE,
    );
    caught(
        &replayed_with(&made, &named_f, vec![collect(A, (2, 5), &["f", "g"])]),
        5,
        UNNAMED,
    );
    caught(
        &replayed_with(&made, &named_f, vec![collect(A, (2, 5), &["f", "f"])]),
        5,
        "reported a path more times than the children it named spell it",
    );
    caught(
        &replayed_with(
            &made,
            &[dirty_f.clone(), head(2, 0)].concat(),
            vec![collect(A, (2, 5), &["f"])],
        ),
        5,
        UNNAMED,
    );
    caught(
        &replayed_with(
            &made,
            &[named_f.clone(), head_of(B, 4, 0)].concat(),
            vec![collect(A, (2, 6), &["f"])],
        ),
        6,
        UNLISTED,
    );
    // From the root: `a`, a dirty directory, named and descended into, `f` below it.
    let from_root = [
        dirty_f.clone(),
        flagging_of(&made, 2, A, NodeFlags::Dirty),
        head_of(ROOT_NODE, 3, 0),
        name_read(4, A, "a"),
        head(5, 0),
        name_read(6, F, "f"),
    ]
    .concat();
    clean(&replayed_with(
        &made,
        &from_root,
        vec![collect(ROOT_NODE, (3, 8), &["a/f"])],
    ));
    caught(
        &replayed_with(&made, &from_root, vec![collect(ROOT_NODE, (3, 8), &["f"])]),
        8,
        NAMED_FILE,
    );
}

/// A directory a collection named and did not descend into, a dirty delete, it reports; in a
/// scheduler's run it descends into each directory it named but a dirty delete, reports each it
/// named as a dirty delete or a dirty add staged or heading no chain and no other, and its empty
/// root where it holds a dirty add.
#[test]
fn a_collection_reports_a_directory_by_its_dirty_add_or_delete() {
    let made = snapshot();
    let named_a = |flags| {
        [
            flagging_of(&made, 1, A, flags),
            head_of(ROOT_NODE, 2, 0),
            name_read(3, A, "a"),
        ]
        .concat()
    };
    let deleted = named_a(NodeFlags::DirtyDelete);
    // With no gate, a listing of `a` binds to its naming whatever its words.
    clean(&replayed_with(
        &made,
        &[deleted.clone(), head(4, 0)].concat(),
        vec![collect(ROOT_NODE, (2, 6), &["a"])],
    ));
    clean(&gated(
        &made,
        &deleted,
        vec![collect(ROOT_NODE, (2, 5), &["a"])],
    ));
    caught(
        &replayed_with(&made, &deleted, vec![collect(ROOT_NODE, (2, 5), &[])]),
        5,
        "a directory it did not descend into, at a path it did not report",
    );
    caught(
        &gated(
            &made,
            &[deleted.clone(), head(4, 0)].concat(),
            vec![collect(ROOT_NODE, (2, 6), &["a"])],
        ),
        6,
        "listed 1, which it never named as a directory to descend into",
    );
    let added = [
        named_a(NodeFlags::DirtyAdd | NodeFlags::StagedAdd),
        head(4, 0),
    ]
    .concat();
    clean(&gated(
        &made,
        &added,
        vec![collect(ROOT_NODE, (2, 6), &["a"])],
    ));
    caught(
        &gated(&made, &added, vec![collect(ROOT_NODE, (2, 6), &[])]),
        6,
        "did not report 1, a directory the model held a dirty add or a dirty delete",
    );
    // `a` holding `f` and `g` and only marked: they carry it.
    let carried = [named_a(NodeFlags::DirtyAdd), head(4, 0)].concat();
    clean(&gated(
        &made,
        &carried,
        vec![collect(ROOT_NODE, (2, 6), &[])],
    ));
    caught(
        &gated(&made, &carried, vec![collect(ROOT_NODE, (2, 6), &["a"])]),
        6,
        "reported 1, a dirty-add directory its children carry",
    );
    let plain = [named_a(NodeFlags::Dirty), head(4, 0)].concat();
    clean(&gated(&made, &plain, vec![collect(ROOT_NODE, (2, 6), &[])]));
    caught(
        &gated(&made, &plain, vec![collect(ROOT_NODE, (2, 6), &["a"])]),
        6,
        "reported 1, a directory the model held neither a dirty add nor a dirty delete",
    );
    caught(
        &gated(
            &made,
            &named_a(NodeFlags::Dirty),
            vec![collect(ROOT_NODE, (2, 5), &["a"])],
        ),
        5,
        "did not descend into 1, which the model held no dirty delete",
    );
    // `b` emptied, its `h` moved into `a`: its empty path where it holds a dirty add.
    let emptied = |flags| {
        [
            by(
                1,
                section(1, Section::MoveNamed, vec![moved(H, (B, A), "h", None)]),
            ),
            flagging_of(&made, 2, B, flags),
            head_of(B, 3, REATTACH_CHANGES),
        ]
        .concat()
    };
    clean(&gated(
        &made,
        &emptied(NodeFlags::DirtyAdd),
        vec![collect(B, (3, 5), &[""])],
    ));
    caught(
        &gated(
            &made,
            &emptied(NodeFlags::Dirty),
            vec![collect(B, (3, 5), &[""])],
        ),
        5,
        "reported its empty root 4, which the model held no dirty add",
    );
    caught(
        &gated(
            &made,
            &emptied(NodeFlags::DirtyAdd),
            vec![collect(B, (3, 5), &[])],
        ),
        5,
        "did not report its empty root 4, which the model held a dirty add",
    );
    // Left out by a run of any scheduler, a dirty add empty throughout from its head read.
    caught(
        &replayed_with(
            &made,
            &emptied(NodeFlags::DirtyAdd),
            vec![collect(B, (3, 5), &[])],
        ),
        5,
        LEFT_OUT,
    );
    // `b`'s only child, `h`, marked `Discarded` in place: empty to its walk, which passes it.
    let passed = [
        flagging_of(&made, 1, H, NodeFlags::Discarded),
        flagging_of(&made, 2, B, NodeFlags::DirtyAdd),
        head_of(B, 3, 0),
    ]
    .concat();
    clean(&gated(&made, &passed, vec![collect(B, (3, 5), &[""])]));
    caught(
        &replayed_with(&made, &passed, vec![collect(B, (3, 5), &[])]),
        5,
        LEFT_OUT,
    );
}

/// In a scheduler's run, each child a collection named was dirty as it read the name, nothing
/// landing between its words and its name, and in the directory its listing read, or had joined
/// where it stood since that listing's head read, as a node the listing held may move after.
#[test]
fn in_a_scheduler_run_each_child_named_was_dirty_in_its_directory() {
    let made = snapshot();
    let clean_f = [head(1, 0), name_read(2, F, "f")].concat();
    caught(
        &gated(&made, &clean_f, vec![collect(A, (1, 4), &["f"])]),
        4,
        CLEAN,
    );
    clean(&replayed_with(
        &made,
        &clean_f,
        vec![collect(A, (1, 4), &["f"])],
    ));
    let dirty_f = flagging_of(&made, 1, F, NodeFlags::Dirty);
    caught(
        &gated(
            &made,
            &[
                dirty_f.clone(),
                moved_out(2),
                head(3, REATTACH_CHANGES),
                name_read(4, F, "f"),
            ]
            .concat(),
            vec![collect(A, (3, 6), &["f"])],
        ),
        6,
        ELSEWHERE,
    );
    clean(&gated(
        &made,
        &[
            dirty_f.clone(),
            head(2, 0),
            moved_out(3),
            name_read(4, F, "f"),
        ]
        .concat(),
        vec![collect(A, (2, 6), &["f"])],
    ));
}

/// A collection leaves out no node in place, named and dirty throughout from its root's head read,
/// below directories dirty throughout and no dirty delete: not one cleared, moved out, renamed in
/// place to a name it reported, or repositioned within its directory since the head read, which
/// the listing may pass over; nor one below a clean directory or a dirty delete, which it does not
/// descend into.
#[test]
fn a_collection_leaves_out_no_node_dirty_in_place_throughout() {
    let made = snapshot();
    let dirty_f = flagging_of(&made, 1, F, NodeFlags::Dirty);
    caught(
        &replayed_with(
            &made,
            &[dirty_f.clone(), head(2, 0)].concat(),
            vec![collect(A, (2, 4), &[])],
        ),
        4,
        LEFT_OUT,
    );
    let cleared = rewritten_of(F, 3, made.blocks[0].records[F as usize]);
    for after in [cleared, moved_out(3), repositioned(3, "f")] {
        clean(&replayed_with(
            &made,
            &[dirty_f.clone(), head(2, 0), after].concat(),
            vec![collect(A, (2, 5), &[])],
        ));
    }
    caught(
        &replayed_with(
            &made,
            &[
                dirty_f.clone(),
                repositioned(2, "f"),
                head(3, REATTACH_CHANGES + 1),
            ]
            .concat(),
            vec![collect(A, (3, 5), &[])],
        ),
        5,
        LEFT_OUT,
    );
    // Repositioned within the call, before the head read: the walk starts after, so meets it.
    caught(
        &replayed_with(
            &made,
            &[
                dirty_f.clone(),
                repositioned(3, "f"),
                head(4, REATTACH_CHANGES + 1),
            ]
            .concat(),
            vec![collect(A, (2, 6), &[])],
        ),
        6,
        LEFT_OUT,
    );
    // Renamed in place within the collection: either name, as it read one.
    let respelled = [
        dirty_f.clone(),
        head(2, 0),
        by(1, renamed(3, F, A, "n")),
        name_read(4, F, "n"),
    ]
    .concat();
    clean(&replayed_with(
        &made,
        &respelled,
        vec![collect(A, (2, 6), &["n"])],
    ));
    // From the root: `f` below `a`, clean, a dirty directory, or a dirty delete.
    let below_a = |flags: Option<NodeFlags>| {
        let a = flags.map(|flags| flagging_of(&made, 2, A, flags));
        [
            dirty_f.clone(),
            a.unwrap_or_default(),
            head_of(ROOT_NODE, 3, 0),
        ]
        .concat()
    };
    clean(&replayed_with(
        &made,
        &below_a(None),
        vec![collect(ROOT_NODE, (3, 5), &[])],
    ));
    caught(
        &replayed_with(
            &made,
            &below_a(Some(NodeFlags::Dirty)),
            vec![collect(ROOT_NODE, (3, 5), &[])],
        ),
        5,
        LEFT_OUT,
    );
    caught(
        &replayed_with(
            &made,
            &below_a(Some(NodeFlags::DirtyDelete)),
            vec![collect(ROOT_NODE, (3, 5), &[])],
        ),
        5,
        LEFT_OUT,
    );
    clean(&replayed_with(
        &made,
        &[below_a(Some(NodeFlags::DirtyDelete)), name_read(4, A, "a")].concat(),
        vec![collect(ROOT_NODE, (3, 6), &["a"])],
    ));
}

/// A collection reads its root's head before any other: with none, its root no directory as read,
/// it reports nothing.
#[test]
fn a_collection_reads_its_roots_head_first() {
    let made = snapshot();
    clean(&replayed_with(&made, &[], vec![collect(A, (1, 3), &[])]));
    caught(
        &replayed_with(&made, &[], vec![collect(A, (1, 3), &["f"])]),
        3,
        "reading no head of it",
    );
    caught(
        &replayed_with(
            &made,
            &[head_of(B, 1, 0), head(2, 0)].concat(),
            vec![collect(A, (1, 4), &[])],
        ),
        4,
        "read the head of 4 before its root's, 1",
    );
    caught(
        &replayed_with(&made, &name_read(1, F, "f"), vec![collect(A, (1, 3), &[])]),
        3,
        "reading no head of it",
    );
    caught(
        &replayed_with(
            &made,
            &[name_read(1, F, "f"), head(2, 0)].concat(),
            vec![collect(A, (1, 4), &["f"])],
        ),
        4,
        "named 2 before reading its root's head",
    );
}

/// A link a collection named it follows into the State the link pins, reporting no path of its
/// own, so no path but its linked State's spells below it.
#[test]
fn a_named_link_spells_no_path_of_its_own() {
    let made = snapshot();
    caught(
        &replayed_with(
            &made,
            &[head_of(ROOT_NODE, 1, 0), name_read(2, L, "l")].concat(),
            vec![collect(ROOT_NODE, (1, 4), &["l"])],
        ),
        4,
        UNNAMED,
    );
}

/// A directory a collection named twice, moved within its subtree between the namings, it lists
/// once for each, its stack popping the later naming's push first: each listing's children
/// spelled below the naming it pops, `g` dirtied between the listings named in the second alone.
#[test]
fn a_directory_named_twice_is_listed_below_each_naming() {
    let made = snapshot();
    let records = [
        flagging_of(&made, 1, F, NodeFlags::Dirty),
        flagging_of(&made, 2, A, NodeFlags::Dirty),
        flagging_of(&made, 3, B, NodeFlags::Dirty),
        head_of(ROOT_NODE, 4, 0),
        name_read(5, A, "a"),
        name_read(6, B, "b"),
        by(
            1,
            section(
                7,
                Section::MoveNamed,
                vec![moved(A, (ROOT_NODE, B), "a", None)],
            ),
        ),
        head_of(B, 8, 1),
        name_read(9, A, "a"),
        head(10, 0),
        name_read(11, F, "f"),
        flagging_of(&made, 12, G, NodeFlags::Dirty),
        head(13, 0),
        name_read(14, F, "f"),
        name_read(15, G, "g"),
    ]
    .concat();
    clean(&gated(
        &made,
        &records,
        vec![collect(ROOT_NODE, (4, 17), &["a/f", "a/g", "b/a/f"])],
    ));
    caught(
        &gated(
            &made,
            &records,
            vec![collect(ROOT_NODE, (4, 17), &["a/f", "b/a/f", "b/a/g"])],
        ),
        17,
        NAMED_FILE,
    );
}

/// A collection may miss a node that moved between two directories of its subtree since its root's
/// head read, as it lists each at a head read of its own, and one whose ancestor a rename respelled
/// since, its names read one at a time spelling a path no one read held; not one that moved before.
#[test]
fn a_collection_may_miss_a_node_moved_or_respelled_above_since_its_head_read() {
    let made = snapshot();
    let dirty = [
        flagging_of(&made, 1, F, NodeFlags::Dirty),
        flagging_of(&made, 2, A, NodeFlags::Dirty),
        flagging_of(&made, 3, B, NodeFlags::Dirty),
    ]
    .concat();
    clean(&replayed_with(
        &made,
        &[dirty.clone(), head_of(ROOT_NODE, 4, 0), moved_out(5)].concat(),
        vec![collect(ROOT_NODE, (4, 7), &[])],
    ));
    caught(
        &replayed_with(
            &made,
            &[dirty.clone(), moved_out(4), head_of(ROOT_NODE, 5, 0)].concat(),
            vec![collect(ROOT_NODE, (5, 7), &[])],
        ),
        7,
        LEFT_OUT,
    );
    let respelled = [
        dirty.clone(),
        head_of(ROOT_NODE, 4, 0),
        name_read(5, A, "a"),
        by(1, renamed(6, A, ROOT_NODE, "a2")),
        head(7, 0),
        by(1, renamed(8, F, A, "n")),
        name_read(9, F, "n"),
    ]
    .concat();
    clean(&replayed_with(
        &made,
        &respelled,
        vec![collect(ROOT_NODE, (4, 11), &["a/n"])],
    ));
}
