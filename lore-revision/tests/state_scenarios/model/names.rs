// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The name reads and the paths held to the model (the name reads and `node_path`): each parent
//! word and each name a reader read, recorded under its block's read lock at a `seq` of its own, is
//! the model's then; a name read answers its one name record, a comparison by it, and a path read
//! answers its walk up its parents, each read's parent the next read's node, to its ancestor for a
//! path, to a node no tree holds for none, and for a path the names it read top-down along that
//! walk, joined.

use lore_revision::node::INVALID_NODE;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::free;
use super::moved_out;
use super::name_read;
use super::renamed;
use super::replayed_with;
use super::section;
use super::snapshot;
use super::unlink;
use crate::harness::catalogue;
use crate::harness::catalogue::Kind;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::spelling;

/// A reader's read of `node`'s parent word as `parent` at `seq`, by task 0.
fn parent_read(seq: u64, node: NodeID, parent: NodeID) -> Vec<Record> {
    section(
        seq,
        Section::Unlabelled,
        vec![Effect::Parent { node, parent }],
    )
}

/// A read of `node` of `kind` by task 0 over `[begin, end)`, answering `answer`.
fn read(kind: &'static Kind, node: NodeID, span: (u64, u64), answer: Answer) -> Call {
    Call {
        target: Some(node),
        ..base_call(kind, span, Outcome::Answer(answer))
    }
}

/// A name read of `node` over `[begin, end)` answering the name `name`, or a skip.
fn named(node: NodeID, span: (u64, u64), name: Option<&str>) -> Call {
    let spelled = name.map(|name| spelling(name.as_bytes()));
    read(&catalogue::NAME_CLONE, node, span, Answer::Name(spelled))
}

/// A path read of `node` up to `ancestor` over `[begin, end)` answering the path `path`, or none.
fn pathed(node: NodeID, ancestor: NodeID, span: (u64, u64), path: Option<&str>) -> Call {
    let kind = if ancestor == ROOT_NODE {
        &catalogue::NODE_PATH
    } else {
        &catalogue::NODE_PATH_BELOW
    };
    let path = path.map(|path| spelling(path.as_bytes()));
    Call {
        destination: Some(ancestor),
        ..read(kind, node, span, Answer::Path(path))
    }
}

/// Each parent word and name a reader read is the model's at its `seq`: before and after a move
/// of the node, and a rename in place of it.
#[test]
fn a_reader_reads_a_nodes_parent_and_name_as_the_model_holds_them() {
    let made = snapshot();
    let replayed = |records: &[Vec<Record>]| replayed_with(&made, &records.concat(), Vec::new());
    clean(&replayed(&[parent_read(1, F, A), name_read(2, F, "f")]));
    caught(
        &replayed(&[parent_read(1, F, B)]),
        1,
        "a reader read 2's parent as 4, its words holding 1",
    );
    caught(
        &replayed(&[name_read(1, F, "g")]),
        1,
        "a reader read 2's name as",
    );
    clean(&replayed(&[moved_out(1), parent_read(2, F, B)]));
    caught(
        &replayed(&[moved_out(1), parent_read(2, F, A)]),
        2,
        "a reader read 2's parent as 1, its words holding 4",
    );
    clean(&replayed(&[
        by(1, renamed(1, F, A, "F")),
        name_read(2, F, "F"),
    ]));
    // A free slot's words and a name the model never learned are not judged: one free from the
    // start, and `f` once a discard freed it, its parent word and name stale in the State.
    clean(&replayed(&[
        parent_read(1, FREE, B),
        name_read(2, FREE, "z"),
    ]));
    let freed = by(
        1,
        section(
            1,
            Section::DiscardMarks,
            vec![unlink(F, A, How::Discard), free(F)],
        ),
    );
    clean(&replayed(&[
        freed,
        parent_read(2, F, B),
        name_read(3, F, "z"),
    ]));
    caught(
        &replayed(&[by(1, renamed(1, F, A, "F")), name_read(2, F, "f")]),
        2,
        "a reader read 2's name as",
    );
}

/// A name read answers the name its one name record of its target read; a skip, which only a name
/// the read path refuses makes, is a violation, as no fixture holds one.
#[test]
fn a_name_read_answers_its_one_name_record() {
    let made = snapshot();
    let record = name_read(1, F, "f");
    clean(&replayed_with(
        &made,
        &record,
        vec![named(F, (1, 3), Some("f"))],
    ));
    caught(
        &replayed_with(&made, &record, vec![named(F, (1, 3), Some("g"))]),
        3,
        "answered the name spelled",
    );
    caught(
        &replayed_with(&made, &[], vec![named(F, (1, 3), Some("f"))]),
        3,
        "answered the name spelled",
    );
    caught(
        &replayed_with(
            &made,
            &name_read(1, A, "a"),
            vec![named(F, (1, 3), Some("a"))],
        ),
        3,
        "answered the name spelled",
    );
    caught(
        &replayed_with(&made, &[], vec![named(F, (1, 3), None)]),
        3,
        "skipped the name of 2",
    );
    caught(
        &replayed_with(
            &made,
            &[record.clone(), name_read(2, F, "f")].concat(),
            vec![named(F, (1, 3), Some("f"))],
        ),
        3,
        "answered the name spelled",
    );
}

/// A comparison by name answers whether its one name record of its target read the name compared.
#[test]
fn a_name_comparison_answers_by_its_name_record() {
    let made = snapshot();
    let compared = |name: &str, is: bool| Call {
        spelling: Some(spelling(name.as_bytes())),
        ..read(&catalogue::NAME_IS, F, (1, 3), Answer::NameIs(is))
    };
    let record = name_read(1, F, "f");
    clean(&replayed_with(&made, &record, vec![compared("f", true)]));
    clean(&replayed_with(&made, &record, vec![compared("F", false)]));
    caught(
        &replayed_with(&made, &record, vec![compared("f", false)]),
        3,
        "answered false for 2",
    );
    caught(
        &replayed_with(&made, &record, vec![compared("F", true)]),
        3,
        "answered true for 2",
    );
    caught(
        &replayed_with(&made, &name_read(1, A, "a"), vec![compared("a", true)]),
        3,
        "answered true for 2",
    );
    caught(
        &replayed_with(&made, &[], vec![compared("f", false)]),
        3,
        "answered false for 2",
    );
}

/// A path read answers its walk up its parents from its target, each read's parent the next
/// read's node, ending at its ancestor for a path and at a node no tree holds for none, and for a
/// path the names it read of the walk's nodes, top-down, joined; the ancestor itself the empty
/// path.
#[test]
fn a_path_read_answers_its_walk_up_its_parents_and_its_names() {
    let made = snapshot();
    let up = [parent_read(1, F, A), parent_read(2, A, ROOT_NODE)].concat();
    let names = [name_read(3, A, "a"), name_read(4, F, "f")].concat();
    let whole = [up.clone(), names.clone()].concat();
    clean(&replayed_with(
        &made,
        &whole,
        vec![pathed(F, ROOT_NODE, (1, 6), Some("a/f"))],
    ));
    caught(
        &replayed_with(
            &made,
            &whole,
            vec![pathed(F, ROOT_NODE, (1, 6), Some("f/a"))],
        ),
        6,
        "whose names its name reads",
    );
    caught(
        &replayed_with(
            &made,
            &[
                parent_read(1, F, A),
                parent_read(2, B, ROOT_NODE),
                names.clone(),
            ]
            .concat(),
            vec![pathed(F, ROOT_NODE, (1, 6), Some("a/f"))],
        ),
        6,
        "read 4's parent where its walk up from 2 stood at 1",
    );
    caught(
        &replayed_with(
            &made,
            &[parent_read(1, F, A), name_read(3, F, "f")].concat(),
            vec![pathed(F, ROOT_NODE, (1, 6), Some("f"))],
        ),
        6,
        "its walk up from 2 ending at 1",
    );
    caught(
        &replayed_with(
            &made,
            &[up.clone(), name_read(3, F, "f"), name_read(4, A, "a")].concat(),
            vec![pathed(F, ROOT_NODE, (1, 6), Some("a/f"))],
        ),
        6,
        "whose names its name reads",
    );
    // Its names read bottom-up and joined so: each read the model's, the join its answer, the
    // order not the walk's.
    caught(
        &replayed_with(
            &made,
            &[up.clone(), name_read(3, F, "f"), name_read(4, A, "a")].concat(),
            vec![pathed(F, ROOT_NODE, (1, 6), Some("f/a"))],
        ),
        6,
        "whose names its name reads",
    );
    // Below `b`, which does not hold `f`: none, its walk ending at the root, which no tree holds
    // as a node.
    clean(&replayed_with(&made, &up, vec![pathed(F, B, (1, 4), None)]));
    caught(
        &replayed_with(&made, &up, vec![pathed(F, B, (1, 4), Some("a/f"))]),
        4,
        "its walk up from 2 ending at 0",
    );
    caught(
        &replayed_with(
            &made,
            &parent_read(1, F, A),
            vec![pathed(F, B, (1, 4), None)],
        ),
        4,
        "its walk up from 2 ending at 1",
    );
    // Below `a`, which holds `f`: the walk stops at `a`, and reads no parent of it.
    caught(
        &replayed_with(&made, &up, vec![pathed(F, A, (1, 4), None)]),
        4,
        "read 1's parent past where its walk up from 2 stops, at 1",
    );
    caught(
        &replayed_with(&made, &whole, vec![pathed(F, B, (1, 6), None)]),
        6,
        "answered no path, reading names",
    );
    clean(&replayed_with(
        &made,
        &[],
        vec![pathed(A, A, (1, 3), Some(""))],
    ));
    // An empty name, which a path leaves out, read of a free slot whose name the model never
    // learned.
    let empty = [
        parent_read(1, FREE, A),
        parent_read(2, A, ROOT_NODE),
        name_read(3, A, "a"),
        name_read(4, FREE, ""),
    ]
    .concat();
    clean(&replayed_with(
        &made,
        &empty,
        vec![pathed(FREE, ROOT_NODE, (1, 6), Some("a"))],
    ));
    caught(
        &replayed_with(
            &made,
            &empty,
            vec![pathed(FREE, ROOT_NODE, (1, 6), Some("a/"))],
        ),
        6,
        "whose names its name reads",
    );
    // A walk that left the tree, from a free slot whose parent word no tree holds: the empty path
    // from the root, none below an ancestor.
    let left = parent_read(1, FREE, INVALID_NODE);
    clean(&replayed_with(
        &made,
        &left,
        vec![pathed(FREE, ROOT_NODE, (1, 3), Some(""))],
    ));
    clean(&replayed_with(
        &made,
        &left,
        vec![pathed(FREE, A, (1, 3), None)],
    ));
    caught(
        &replayed_with(&made, &left, vec![pathed(FREE, A, (1, 3), Some(""))]),
        3,
        "ending at",
    );
}
