// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A reader's finds held to the model from the `seq` each began at to its call's end: a node found
//! carried the hash in the directory, in its chain and not `Discarded`, at some `seq` of that span;
//! a miss is a violation where one node carried it there throughout, the node a replacing reattach
//! put there carrying on for the one it replaced (C7). A path find is judged per hop, and a find's
//! answer is what its last lookup found.

use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::A;
use super::B;
use super::F;
use super::G;
use super::H;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::discarding;
use super::moved;
use super::moved_out;
use super::replayed_with;
use super::section;
use super::served;
use super::snapshot;
use super::unlink;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Lookup;
use crate::harness::catalogue::call::Outcome;

const AT_NO_SEQ: &str = "carrying it in the directory at no seq of its interval";
const MISSED: &str = "missed, a node carrying it in the directory throughout its interval";

/// A find of `name` in `dir` begun at `from`, answering `found`.
fn lookup(dir: NodeID, name: &str, found: Option<NodeID>, from: u64) -> Lookup {
    Lookup::Find {
        dir,
        name: hash_string(name),
        found,
        from,
    }
}

/// A call of `kind` on `target` by task 0 over `[begin, end)`, looking up `lookups` and answering
/// what the last found.
fn looking_up(
    kind: &'static catalogue::Kind,
    target: NodeID,
    (begin, end): (u64, u64),
    lookups: Vec<Lookup>,
) -> Call {
    let found = match lookups.last() {
        Some(&Lookup::Find { found, .. }) => found,
        _ => None,
    };
    Call {
        target: Some(target),
        lookups,
        ..base_call(kind, (begin, end), Outcome::Answer(Answer::Found(found)))
    }
}

/// A find of `name` in `dir` answering `found`, over `interval`, begun at its start.
fn find(dir: NodeID, name: &str, found: Option<NodeID>, interval: (u64, u64)) -> Vec<Call> {
    vec![looking_up(
        &catalogue::FIND,
        dir,
        interval,
        vec![lookup(dir, name, found, interval.0)],
    )]
}

/// A path find from the root over `[1, end)`, looking up `lookups`.
fn path(lookups: Vec<Lookup>, end: u64) -> Vec<Call> {
    vec![looking_up(
        &catalogue::FIND_PATH,
        ROOT_NODE,
        (1, end),
        lookups,
    )]
}

/// A node found carried the name in the directory at some `seq` of the find's span: there
/// throughout, only until it moved out, or only once it moved in; not a node of another name, one
/// that left before the span or came after it, one under another directory, nor a `Discarded` one.
#[test]
fn a_found_node_carried_the_name_in_the_directory_at_some_seq() {
    let made = snapshot();
    clean(&replayed_with(&made, &[], find(A, "f", Some(F), (1, 3))));
    caught(
        &replayed_with(&made, &[], find(A, "f", Some(G), (1, 3))),
        3,
        AT_NO_SEQ,
    );
    clean(&replayed_with(
        &made,
        &moved_out(2),
        find(A, "f", Some(F), (1, 4)),
    ));
    clean(&replayed_with(
        &made,
        &moved_out(2),
        find(B, "f", Some(F), (1, 4)),
    ));
    caught(
        &replayed_with(&made, &moved_out(2), find(A, "f", Some(F), (3, 5))),
        5,
        AT_NO_SEQ,
    );
    caught(
        &replayed_with(&made, &moved_out(5), find(B, "f", Some(F), (1, 4))),
        4,
        AT_NO_SEQ,
    );
    caught(
        &replayed_with(&made, &[], find(B, "f", Some(F), (1, 3))),
        3,
        AT_NO_SEQ,
    );
    caught(
        &replayed_with(&made, &discarding(&made, 1), find(A, "f", Some(F), (2, 4))),
        4,
        AT_NO_SEQ,
    );
}

/// A miss is a violation where one node carried the name in the directory throughout the find's
/// span: not where none carries it, where the one that did moved out or was detached within or
/// before the span, where another took the name only after it left, nor where it was `Discarded`;
/// a move after the span excuses nothing.
#[test]
fn a_miss_is_a_violation_where_a_node_carried_the_name_throughout() {
    let made = snapshot();
    caught(
        &replayed_with(&made, &[], find(A, "f", None, (1, 3))),
        3,
        MISSED,
    );
    clean(&replayed_with(&made, &[], find(A, "x", None, (1, 3))));
    clean(&replayed_with(
        &made,
        &moved_out(2),
        find(A, "f", None, (1, 4)),
    ));
    let detaching = |seq| {
        by(
            1,
            section(seq, Section::Detach, vec![unlink(F, A, How::Always)]),
        )
    };
    clean(&replayed_with(
        &made,
        &detaching(1),
        find(A, "f", None, (2, 4)),
    ));
    clean(&replayed_with(
        &made,
        &detaching(2),
        find(A, "f", None, (1, 4)),
    ));
    caught(
        &replayed_with(&made, &moved_out(5), find(A, "f", None, (1, 4))),
        4,
        MISSED,
    );
    clean(&replayed_with(
        &made,
        &discarding(&made, 1),
        find(A, "f", None, (2, 4)),
    ));
    let renamed = by(
        1,
        section(3, Section::MoveNamed, vec![moved(G, (A, A), "f", None)]),
    );
    clean(&replayed_with(
        &made,
        &[moved_out(2), renamed].concat(),
        find(A, "f", None, (1, 5)),
    ));
}

/// A replacing reattach hands the name from its holder to the node it moves in one record (C7):
/// a find across it answers either node, and its miss is a violation, the name carried throughout.
#[test]
fn a_find_across_a_replacing_reattach_answers_either_node() {
    let made = served();
    let replacing = by(
        1,
        section(2, Section::Reattach, vec![moved(G, (A, A), "f", Some(F))]),
    );
    clean(&replayed_with(
        &made,
        &replacing,
        find(A, "f", Some(F), (1, 4)),
    ));
    clean(&replayed_with(
        &made,
        &replacing,
        find(A, "f", Some(G), (1, 4)),
    ));
    caught(
        &replayed_with(&made, &replacing, find(A, "f", None, (1, 4))),
        4,
        MISSED,
    );
    // The node it put there carries on only while it stays.
    let leaving = by(
        1,
        section(3, Section::Detach, vec![unlink(G, A, How::Always)]),
    );
    clean(&replayed_with(
        &made,
        &[replacing, leaving].concat(),
        find(A, "f", None, (1, 5)),
    ));
    // A replacing reattach in another directory hands over nothing of this one's.
    let elsewhere = by(
        1,
        section(2, Section::Reattach, vec![moved(G, (A, B), "h", Some(H))]),
    );
    caught(
        &replayed_with(&made, &elsewhere, find(A, "f", None, (1, 4))),
        4,
        MISSED,
    );
}

/// A path find is judged per hop, each from the `seq` it began at: a later hop answering a node
/// that had left before the hop began is caught, though it stood there as the call began.
#[test]
fn a_path_find_is_judged_per_hop_from_where_each_began() {
    let made = snapshot();
    clean(&replayed_with(
        &made,
        &[],
        path(
            vec![
                lookup(ROOT_NODE, "a", Some(A), 1),
                lookup(A, "f", Some(F), 1),
            ],
            3,
        ),
    ));
    caught(
        &replayed_with(
            &made,
            &[],
            path(
                vec![lookup(ROOT_NODE, "a", Some(B), 1), lookup(B, "x", None, 1)],
                3,
            ),
        ),
        3,
        AT_NO_SEQ,
    );
    caught(
        &replayed_with(
            &made,
            &[],
            path(
                vec![lookup(ROOT_NODE, "a", Some(A), 1), lookup(A, "g", None, 1)],
                3,
            ),
        ),
        3,
        MISSED,
    );
    let hops = |from| {
        path(
            vec![
                lookup(ROOT_NODE, "a", Some(A), 1),
                lookup(A, "f", Some(F), from),
            ],
            5,
        )
    };
    clean(&replayed_with(&made, &moved_out(2), hops(1)));
    caught(&replayed_with(&made, &moved_out(2), hops(3)), 5, AT_NO_SEQ);
}

/// A find answers what its last lookup found; a call makes no more claims than the model watches.
#[test]
fn a_find_answers_its_last_lookup_within_the_claims_watched() {
    let made = snapshot();
    let mut answered = find(A, "f", Some(F), (1, 3));
    answered[0].outcome = Outcome::Answer(Answer::Found(Some(G)));
    caught(
        &replayed_with(&made, &[], answered),
        3,
        "answered Some(3), not what its last lookup found",
    );
    let mut silent = find(A, "f", Some(F), (1, 3));
    silent[0].lookups.clear();
    caught(
        &replayed_with(&made, &[], silent),
        3,
        "not what its last lookup found",
    );
    let many = vec![lookup(ROOT_NODE, "a", Some(A), 1); 32];
    caught(
        &replayed_with(&made, &[], path(many, 3)),
        3,
        "made 32 claims past its answer's (0 nodes passed over as taken, 32 lookups, 0 by its walks",
    );
}

/// The lookups' claims follow the nodes passed over as taken, each judged by its own bit: a taken
/// node not gone is caught though the lookup holds, and a lookup not holding though the taken node
/// went.
#[test]
fn lookups_follow_the_nodes_passed_over_as_taken() {
    let made = snapshot();
    let taking = |taken: NodeID, found: NodeID| {
        let mut calls = find(A, "f", Some(found), (1, 4));
        calls[0].taken = vec![(taken, 1)];
        calls
    };
    let detached = by(
        1,
        section(2, Section::Detach, vec![unlink(G, A, How::Always)]),
    );
    clean(&replayed_with(&made, &detached, taking(G, F)));
    caught(
        &replayed_with(&made, &detached, taking(F, F)),
        4,
        "passed over 2 as taken at 1",
    );
    caught(&replayed_with(&made, &detached, taking(G, G)), 4, AT_NO_SEQ);
}
