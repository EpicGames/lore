// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Walks of what is no directory held to the model by what they walked: a walk over a link to the
//! children a load of the revision a pin the link held at some `seq` of its call names holds below
//! the node the pin exposes, as a set, kept by the walk's filter and not `Discarded`, each with the
//! record, the name's hash and the name read, and with no head read, as the read loads that State
//! afresh, private to it; a walk of a node that takes no children to none, its node a file or a
//! link. Each makes one claim. A resolution of a path answers what its last hop found, each hop a
//! find of its own, or, crossing a link, what the revision a pin of the link held from its last hop
//! on gives for the rest of its path. A collection naming a dirty link reports the paths a
//! collection of the revision of the pin it read makes below the node the pin exposes, in a run of
//! no gate with a write since its directory's head read those of any pin the link held since; one
//! passing over a dirty link is caught.

use lore_base::types::Hash;
use lore_base::types::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::A;
use super::B;
use super::F;
use super::G;
use super::L;
use super::PINNED;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::collected::LEFT_OUT;
use super::collected::UNNAMED;
use super::collected::collect;
use super::flagged_of;
use super::flagging_of;
use super::gated_providing;
use super::head_of;
use super::moved_out;
use super::name_read;
use super::record;
use super::renamed;
use super::replayed_providing;
use super::replayed_with;
use super::rewritten_of;
use super::section;
use super::snapshot;
use super::walk_of;
use super::word;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Filter;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Over;
use crate::harness::catalogue::call::Repeats;
use crate::harness::catalogue::call::Yielded;
use crate::harness::catalogue::call::spelling;
use crate::harness::model::BlockSnapshot;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;

/// The revision `l` pins in the State made by hand, and the one a repin pins it to.
const FIRST: u64 = 1;
const SECOND: u64 = 2;

/// The node `l` exposes in [`SECOND`].
const OTHER: NodeID = 50;

/// The violation the model reports of a walk over `l` held to no pin the link held.
const PINNED_TO: &str = "yielded other than the children below the node a pin it held at some seq";

/// A revision's State as a load of it holds it: `src`, the directory `dir` below the root, holding
/// a file of each name of `names` in the slots past it, each put at the head of its chain.
fn revision(dir: NodeID, names: &[&str]) -> Snapshot {
    let files: Vec<_> = (names.iter())
        .map(|&name| (name, Kind::File, NodeFlags::empty()))
        .collect();
    revision_with(dir, &files)
}

/// [`revision`] holding each of `entries` below `src` instead: a path below it, each parent before
/// its children, with its kind and the flags it carries.
fn revision_with(dir: NodeID, entries: &[(&str, Kind, NodeFlags)]) -> Snapshot {
    let first = dir + 1;
    let mut block = BlockSnapshot::empty(0, first + entries.len() as NodeID);
    block.records[dir as usize] = record(0, "src", Kind::Directory);
    block.records[0].child = dir;
    for (at, &(path, kind, flags)) in entries.iter().enumerate() {
        let node = first + at as NodeID;
        let (parent, name) = path.rsplit_once('/').map_or((dir, path), |(parent, name)| {
            let parent = entries[..at]
                .iter()
                .position(|&(above, ..)| above == parent);
            (first + parent.expect("a parent before") as NodeID, name)
        });
        let mut record = record(parent, name, kind);
        record.flags |= flags.bits();
        record.sibling = block.records[parent as usize].child;
        block.records[parent as usize].child = node;
        block.records[node as usize] = record;
        block.names[node as usize] = Some(Name::new(name.as_bytes()));
    }
    Snapshot {
        blocks: vec![block],
        ..Snapshot::default()
    }
}

/// [`FIRST`] holding `a` and `b` below [`PINNED`], `b` given `flags`.
fn first_flagging_b(flags: NodeFlags) -> Snapshot {
    let mut first = revision(PINNED, &["a", "b"]);
    first.blocks[0].records[(PINNED + 2) as usize].flags |= flags.bits();
    first
}

/// The snapshots of [`FIRST`], `first`, and of [`SECOND`], holding `c` below [`OTHER`].
fn revisions(first: Snapshot) -> Vec<(Hash, Snapshot)> {
    vec![
        (Hash::from_u64(FIRST), first),
        (Hash::from_u64(SECOND), revision(OTHER, &["c"])),
    ]
}

/// The State made by hand, `l` pinning [`PINNED`] in [`FIRST`].
fn made() -> Snapshot {
    pinning(PINNED)
}

/// The State made by hand, `l` pinning `node` in [`FIRST`].
fn pinning(node: NodeID) -> Snapshot {
    let mut made = snapshot();
    let pin = &mut made.blocks[0].records[L as usize];
    pin.address.hash = Hash::from_u64(FIRST);
    pin.child = node;
    made
}

/// `l`'s words in `made` repinned to [`OTHER`] in [`SECOND`] at `seq`, by another task.
fn repinned(made: &Snapshot, seq: u64) -> Vec<Record> {
    let mut pin = made.blocks[0].records[L as usize];
    pin.address.hash = Hash::from_u64(SECOND);
    pin.child = OTHER;
    rewritten_of(L, seq, pin)
}

/// The model of `made` with `records` replayed with `calls`, a test's own edits admitted, the
/// snapshots of [`FIRST`], holding `a` and `b` below [`PINNED`], and of [`SECOND`] provided.
fn linked(made: &Snapshot, records: &[Record], calls: Vec<Call>) -> Model {
    let first = revision(PINNED, &["a", "b"]);
    replayed_providing(made, revisions(first), records, calls)
}

/// A listing by task 0 over `span` of `dir`, walking what `over` names from its begin under
/// `filter`, yielding each node of `yielded` by its id and the hash of the name given with it.
fn listing_of(
    (dir, over): (NodeID, Over),
    span: (u64, u64),
    yielded: &[(NodeID, Option<&str>)],
    filter: Filter,
) -> Call {
    let named = yielded.iter().any(|(_, name)| name.is_some());
    let kind = if named {
        &catalogue::LISTED_NAME_HASHES
    } else {
        &catalogue::LISTED_CHILDREN
    };
    let yielded = yielded
        .iter()
        .map(|&(node, name)| Yielded {
            node,
            record: None,
            name: name.map(hash_string),
            spelling: None,
        })
        .collect();
    walk_of(kind, (dir, over), span, yielded, (Repeats::Dropped, filter))
}

/// A listing of `l` over `span` yielding `yielded`.
fn through(span: (u64, u64), yielded: &[(NodeID, Option<&str>)]) -> Call {
    listing_of((L, Over::Link), span, yielded, Filter::EVERY)
}

/// A walk over a link yields the children of the pinned directory in the revision its pin names,
/// in any order, each with the name's hash it read, and reads no head of the link: one left out,
/// one more, another node in place of one, another revision's, or a name's hash the node does not
/// carry is caught.
#[test]
fn a_walk_over_a_link_yields_the_children_of_the_pinned_directory() {
    let made = made();
    let (a, b, c) = (PINNED + 1, PINNED + 2, OTHER + 1);
    clean(&linked(
        &made,
        &[],
        vec![through((1, 3), &[(b, None), (a, None)])],
    ));
    clean(&linked(
        &made,
        &[],
        vec![through((1, 3), &[(a, None), (b, None)])],
    ));
    clean(&linked(
        &made,
        &[],
        vec![through((1, 3), &[(b, Some("b")), (a, Some("a"))])],
    ));
    for yielded in [
        vec![(b, None)],
        vec![(b, None), (a, None), (F, None)],
        vec![(b, None), (F, None)],
        vec![(c, None)],
        vec![(b, Some("b")), (a, Some("b"))],
    ] {
        caught(
            &linked(&made, &[], vec![through((1, 3), &yielded)]),
            3,
            PINNED_TO,
        );
    }
}

/// A walk over a link yields each node with the record and the name the revision holds for it,
/// where it read them: another node's record, or another spelling, is caught.
#[test]
fn a_walk_over_a_link_yields_each_node_with_its_record_and_name() {
    let made = made();
    let pinned = revision(PINNED, &["a", "b"]);
    let (a, b) = (PINNED + 1, PINNED + 2);
    let read = |node: NodeID, (record, name): (NodeID, &str)| Yielded {
        node,
        record: Some(pinned.blocks[0].records[record as usize]),
        name: None,
        spelling: Some(spelling(name.as_bytes())),
    };
    let reading = |yielded: Vec<Yielded>| {
        let mut call = through((1, 3), &[]);
        call.walks[0].yielded = yielded;
        call
    };
    clean(&linked(
        &made,
        &[],
        vec![reading(vec![read(b, (b, "b")), read(a, (a, "a"))])],
    ));
    for yielded in [
        vec![read(b, (a, "b")), read(a, (a, "a"))],
        vec![read(b, (b, "a")), read(a, (a, "a"))],
    ] {
        caught(&linked(&made, &[], vec![reading(yielded)]), 3, PINNED_TO);
    }
}

/// A walk over a link yields the pinned directory's children its filter keeps and no `Discarded`
/// one: one staged for deletion left out where its filter leaves those out, and kept where it
/// keeps them.
#[test]
fn a_walk_over_a_link_keeps_by_its_filter() {
    let made = made();
    let (a, b) = (PINNED + 1, PINNED + 2);
    let discarded = revisions(first_flagging_b(NodeFlags::Discarded));
    let deleted = revisions(first_flagging_b(NodeFlags::StagedDelete));
    for (revisions, filter, yielded, kept) in [
        (&discarded, Filter::EVERY, vec![(a, None)], true),
        (&discarded, Filter::EVERY, vec![(b, None), (a, None)], false),
        (&deleted, Filter::UNDELETED, vec![(a, None)], true),
        (
            &deleted,
            Filter::UNDELETED,
            vec![(b, None), (a, None)],
            false,
        ),
        (&deleted, Filter::EVERY, vec![(b, None), (a, None)], true),
        (&deleted, Filter::EVERY, vec![(a, None)], false),
    ] {
        let walk = listing_of((L, Over::Link), (1, 3), &yielded, filter);
        let model = replayed_providing(&made, revisions.clone(), &[], vec![walk]);
        if kept {
            clean(&model);
        } else {
            caught(&model, 3, PINNED_TO);
        }
    }
}

/// A walk over a link is held to some pin the link held in its interval, each read loading the
/// revision its pin named: beside a repin within it, either revision's children; after a repin
/// before it, the second's alone; pinned to a revision no load in the run read, or to a node the
/// revision does not hold, nothing.
#[test]
fn a_walk_over_a_link_holds_by_a_pin_it_held_in_its_interval() {
    let made = made();
    let (a, b, c) = (PINNED + 1, PINNED + 2, OTHER + 1);
    let repin = repinned(&made, 2);
    clean(&linked(
        &made,
        &repin,
        vec![through((1, 4), &[(b, None), (a, None)])],
    ));
    clean(&linked(&made, &repin, vec![through((1, 4), &[(c, None)])]));
    clean(&linked(&made, &repin, vec![through((3, 5), &[(c, None)])]));
    caught(
        &linked(
            &made,
            &repin,
            vec![through((3, 5), &[(b, None), (a, None)])],
        ),
        5,
        PINNED_TO,
    );
    caught(
        &linked(&made, &[], vec![through((1, 3), &[(c, None)])]),
        3,
        PINNED_TO,
    );
    let mut unread = made.clone();
    unread.blocks[0].records[L as usize].address.hash = Hash::from_u64(9);
    caught(
        &linked(&unread, &[], vec![through((1, 3), &[])]),
        3,
        PINNED_TO,
    );
    caught(
        &linked(&pinning(600), &[], vec![through((1, 3), &[])]),
        3,
        PINNED_TO,
    );
}

/// A walk over a link exposing a file yields none, as a load of its revision holds no child below
/// a file; one yielding a node is caught. One over a node that is no link is caught.
#[test]
fn a_walk_over_a_link_exposing_a_file_yields_none() {
    let (a, b) = (PINNED + 1, PINNED + 2);
    let exposing = pinning(a);
    clean(&linked(&exposing, &[], vec![through((1, 3), &[])]));
    caught(
        &linked(&exposing, &[], vec![through((1, 3), &[(b, None)])]),
        3,
        PINNED_TO,
    );
    let over_a_file = listing_of((F, Over::Link), (1, 3), &[], Filter::EVERY);
    caught(&linked(&made(), &[], vec![over_a_file]), 3, PINNED_TO);
}

/// A walk of a node that takes no children, a file or a link, yields none and reads no head; one
/// yielding a node is caught, and one of a directory.
#[test]
fn a_walk_of_a_node_that_takes_no_children_yields_none() {
    let made = made();
    let leaf = |node, yielded: &[(NodeID, Option<&str>)]| {
        listing_of((node, Over::Leaf), (1, 3), yielded, Filter::EVERY)
    };
    clean(&linked(&made, &[], vec![leaf(F, &[])]));
    clean(&linked(&made, &[], vec![leaf(L, &[])]));
    caught(
        &linked(&made, &[], vec![leaf(F, &[(G, None)])]),
        3,
        "yielded 1 children of a node that takes none",
    );
    caught(
        &linked(&made, &[], vec![leaf(A, &[])]),
        3,
        "which was neither a file nor a link at any seq of its interval",
    );
}

/// A walk over a link makes one claim, as one of a node that takes no children does, within the
/// claims the model watches.
#[test]
fn walks_claim_by_what_they_walk() {
    let made = made();
    let walks = |call: Call, count| Call {
        walks: vec![call.walks[0].clone(); count],
        ..call
    };
    let pinned = through((1, 3), &[(PINNED + 2, None), (PINNED + 1, None)]);
    clean(&linked(&made, &[], vec![walks(pinned.clone(), 31)]));
    caught(
        &linked(&made, &[], vec![walks(pinned, 32)]),
        3,
        "made 32 claims past its answer's (0 nodes passed over as taken, 0 lookups, 32 by its walks",
    );
    let leaf = listing_of((F, Over::Leaf), (1, 3), &[], Filter::EVERY);
    clean(&linked(&made, &[], vec![walks(leaf.clone(), 31)]));
    caught(
        &linked(&made, &[], vec![walks(leaf, 32)]),
        3,
        "made 32 claims past its answer's (0 nodes passed over as taken, 0 lookups, 32 by its walks",
    );
}

/// A resolution's hop at `seq` by task 0: its find of `name` in `dir`, answering `found`.
fn hop(seq: u64, dir: NodeID, name: &str, found: Option<NodeID>) -> Vec<Record> {
    let name = hash_string(name);
    section(
        seq,
        Section::Unlabelled,
        vec![Effect::Hop { dir, name, found }],
    )
}

/// A resolution of `path` below the root by task 0 over `span`, answering `answer`.
fn resolution(span: (u64, u64), path: &str, answer: Answer) -> Call {
    Call {
        target: Some(ROOT_NODE),
        names: path.split('/').map(hash_string).collect(),
        ..base_call(&catalogue::FIND_LINKED, span, Outcome::Answer(answer))
    }
}

/// [`Answer::Linked`] of `node` in the revision `revision` names, of the repository `l` pins.
fn linked_to(node: NodeID, revision: u64) -> Answer {
    Answer::Linked {
        node,
        repository: RepositoryId::default(),
        revision: Hash::from_u64(revision),
    }
}

/// [`linked_to`], of another repository than `l` pins.
fn linked_elsewhere(node: NodeID, revision: u64) -> Answer {
    let mut repository = RepositoryId::default();
    repository.data_mut()[15] = 9;
    Answer::Linked {
        node,
        repository,
        revision: Hash::from_u64(revision),
    }
}

/// A resolution in the call's State answers what its last hop found at its path's end, each hop a
/// find of its own: another node, a hop past one that found nothing, more hops than names, or an
/// answer from another repository is caught.
#[test]
fn a_resolution_answers_what_its_last_hop_found() {
    let made = made();
    let found = [hop(2, ROOT_NODE, "a", Some(A)), hop(3, A, "f", Some(F))].concat();
    let missed = [hop(2, ROOT_NODE, "a", Some(A)), hop(3, A, "x", None)].concat();
    clean(&linked(
        &made,
        &found,
        vec![resolution((1, 5), "a/f", Answer::Found(Some(F)))],
    ));
    clean(&linked(
        &made,
        &missed,
        vec![resolution((1, 5), "a/x", Answer::Found(None))],
    ));
    for (records, path, answer, what) in [
        (
            &found,
            "a/f",
            Answer::Found(Some(G)),
            "not what its last hop found",
        ),
        (
            &found,
            "a/f",
            Answer::Found(None),
            "not what its last hop found",
        ),
        (
            &found,
            "a",
            Answer::Found(Some(F)),
            "made more hops than its path has names: 2 for 1",
        ),
        (&found, "a/f", linked_to(F, FIRST), "crossing no link"),
    ] {
        let model = linked(&made, records, vec![resolution((1, 5), path, answer)]);
        caught(&model, 5, what);
    }
    let past = [hop(2, ROOT_NODE, "x", None), hop(3, A, "f", Some(F))].concat();
    caught(
        &linked(
            &made,
            &past,
            vec![resolution((1, 5), "x/f", Answer::Found(Some(F)))],
        ),
        5,
        "not for its path's name there in None",
    );
}

/// Each hop of a resolution is held from just after the hop before it: a node a hop found, moved
/// away after it, held there when found.
#[test]
fn a_resolutions_hop_is_held_from_the_hop_before_it() {
    let made = made();
    let moving = [
        hop(2, ROOT_NODE, "a", Some(A)),
        hop(3, A, "f", Some(F)),
        moved_out(4),
    ]
    .concat();
    let found = resolution((1, 6), "a/f", Answer::Found(Some(F)));
    clean(&linked(&made, &moving, vec![found]));
}

/// A resolution crossing a link answers what the revision a pin of the link held from its last hop
/// on gives for the rest of its path below the node the pin exposes: that node, in that revision,
/// or none where a name is missing or a file stands where a directory would; another node,
/// another revision, none where the revision holds the path, or the link itself is caught.
#[test]
fn a_resolution_through_a_link_answers_what_the_pinned_revision_gives() {
    let made = made();
    let (a, b) = (PINNED + 1, PINNED + 2);
    let crossing = hop(2, ROOT_NODE, "l", Some(L));
    clean(&linked(
        &made,
        &crossing,
        vec![resolution((1, 5), "l/a", linked_to(a, FIRST))],
    ));
    clean(&linked(
        &made,
        &crossing,
        vec![resolution((1, 5), "l/x", Answer::Found(None))],
    ));
    clean(&linked(
        &made,
        &crossing,
        vec![resolution((1, 5), "l/a/x", Answer::Found(None))],
    ));
    for (path, answer, what) in [
        ("l/a", linked_to(b, FIRST), "through the link"),
        ("l/a", linked_to(a, SECOND), "through the link"),
        ("l/a", linked_elsewhere(a, FIRST), "through the link"),
        ("l/a", Answer::Found(None), "through the link"),
        ("l/x", linked_to(a, FIRST), "through the link"),
        (
            "l/a",
            Answer::Found(Some(L)),
            "with names of its path left past the link",
        ),
    ] {
        let model = linked(&made, &crossing, vec![resolution((1, 5), path, answer)]);
        caught(&model, 5, what);
    }
}

/// A resolution crossing a link repinned within it answers by either pin held from its last hop
/// on; one repinned before it began, by the second pin alone.
#[test]
fn a_resolution_through_a_link_holds_by_a_pin_it_held_from_its_hop() {
    let made = made();
    let (a, c) = (PINNED + 1, OTHER + 1);
    let within = [hop(2, ROOT_NODE, "l", Some(L)), repinned(&made, 3)].concat();
    for answer in [linked_to(a, FIRST), Answer::Found(None)] {
        clean(&linked(
            &made,
            &within,
            vec![resolution((1, 5), "l/a", answer)],
        ));
    }
    clean(&linked(
        &made,
        &within,
        vec![resolution((1, 5), "l/c", linked_to(c, SECOND))],
    ));
    let before = [repinned(&made, 1), hop(3, ROOT_NODE, "l", Some(L))].concat();
    caught(
        &linked(
            &made,
            &before,
            vec![resolution((2, 5), "l/a", linked_to(a, FIRST))],
        ),
        5,
        "through the link",
    );
    // Repinned within it, before the hop that found the link: read after that hop.
    let before_hop = [repinned(&made, 2), hop(3, ROOT_NODE, "l", Some(L))].concat();
    caught(
        &linked(
            &made,
            &before_hop,
            vec![resolution((1, 5), "l/a", linked_to(a, FIRST))],
        ),
        5,
        "through the link",
    );
}

/// A resolution's hops run from its target down its path, each from the node the hop before it
/// found and for its path's name there, whatever it answered: a hop from another directory, or
/// for another name, is caught, in a dropped resolution too; a hop another task made, or one past
/// the call's end, is no hop of its.
#[test]
fn a_resolutions_hops_walk_its_path() {
    let made = made();
    let lost = [hop(2, ROOT_NODE, "a", Some(A)), hop(3, B, "f", Some(F))].concat();
    let other = [hop(2, ROOT_NODE, "a", Some(A)), hop(3, A, "g", Some(G))].concat();
    for (records, answer) in [
        (&lost, Answer::Found(Some(F))),
        (&hop(2, A, "a", Some(A)), Answer::Found(None)),
        (&other, Answer::Found(Some(G))),
    ] {
        let model = linked(&made, records, vec![resolution((1, 5), "a/f", answer)]);
        caught(&model, 5, "not for its path's name there");
    }
    let dropped = Call {
        outcome: Outcome::Dropped,
        ..resolution((1, 5), "a/f", Answer::Found(None))
    };
    caught(
        &linked(&made, &lost, vec![dropped]),
        5,
        "not for its path's name there",
    );
    let beside = [
        hop(2, ROOT_NODE, "a", Some(A)),
        by(1, hop(3, A, "x", None)),
        hop(4, A, "f", Some(F)),
        hop(7, F, "x", None),
    ]
    .concat();
    let found = resolution((1, 6), "a/f", Answer::Found(Some(F)));
    clean(&linked(&made, &beside, vec![found]));
}

/// Each hop is a find of its own: one answering a node that does not carry the name, or one
/// answering a name another node carried only before the hop before it, is caught.
#[test]
fn a_resolutions_hops_are_finds() {
    let made = made();
    let wrong = [hop(2, ROOT_NODE, "a", Some(A)), hop(3, A, "f", Some(G))].concat();
    caught(
        &linked(
            &made,
            &wrong,
            vec![resolution((1, 5), "a/f", Answer::Found(Some(G)))],
        ),
        5,
        "carrying it in the directory at no seq",
    );
    let respelled = [
        by(1, renamed(2, F, A, "x")),
        hop(3, ROOT_NODE, "a", Some(A)),
        hop(4, A, "f", Some(F)),
    ]
    .concat();
    caught(
        &linked(
            &made,
            &respelled,
            vec![resolution((1, 6), "a/f", Answer::Found(Some(F)))],
        ),
        6,
        "carrying it in the directory at no seq",
    );
}

/// A resolution hopping on from a link with names of its path left, as from a directory, did not
/// enter it: caught, the node held no link from just after the hop that found it, not from the
/// call's begin.
#[test]
fn a_resolution_hopping_on_from_a_link_is_caught() {
    let made = made();
    let past_link = [hop(2, ROOT_NODE, "l", Some(L)), hop(3, L, "a", None)].concat();
    caught(
        &linked(
            &made,
            &past_link,
            vec![resolution((1, 5), "l/a", Answer::Found(None))],
        ),
        5,
        "not entering it",
    );
    // `f` a link from before the hop that found it: as a slot freed and used again for a link.
    let mut link = made.blocks[0].records[F as usize];
    link.flags = NodeFlags::Link.bits();
    let relinked = [
        hop(2, ROOT_NODE, "a", Some(A)),
        rewritten_of(F, 3, link),
        hop(4, A, "f", Some(F)),
        hop(5, F, "x", None),
    ]
    .concat();
    caught(
        &linked(
            &made,
            &relinked,
            vec![resolution((1, 7), "a/f/x", Answer::Found(None))],
        ),
        7,
        "not entering it",
    );
}

/// A resolution through a link whose pin exposes a link answers nothing, as a find below a link
/// misses; one meeting a link in the pinned revision at its path's end answers it, and one meeting
/// it with names left, which enters a third State the model does not hold, is caught whatever it
/// answers.
#[test]
fn a_resolution_through_a_link_meeting_another_link() {
    let made = made();
    let crossing = hop(2, ROOT_NODE, "l", Some(L));
    let a = PINNED + 1;
    let resolving = |link: NodeID, path: &str, answer| {
        let mut first = revision(PINNED, &["a", "b"]);
        first.blocks[0].records[link as usize].flags = NodeFlags::Link.bits();
        let calls = vec![resolution((1, 5), path, answer)];
        replayed_providing(&made, revisions(first), &crossing, calls)
    };
    clean(&resolving(PINNED, "l/a", Answer::Found(None)));
    caught(
        &resolving(PINNED, "l/a", linked_to(a, FIRST)),
        5,
        "through the link",
    );
    clean(&resolving(a, "l/a", linked_to(a, FIRST)));
    for answer in [Answer::Found(None), linked_to(a, FIRST)] {
        caught(&resolving(a, "l/a/x", answer), 5, "through the link");
    }
}

/// A State of `depth` directories below the root, each named `d` and the one child of the one
/// above it.
fn deep(depth: NodeID) -> Snapshot {
    let mut block = BlockSnapshot::empty(0, depth + 1);
    for node in 1..=depth {
        block.records[node as usize] = record(node - 1, "d", Kind::Directory);
        block.records[(node - 1) as usize].child = node;
        block.names[node as usize] = Some(Name::new(b"d"));
    }
    Snapshot {
        blocks: vec![block],
        ..Snapshot::default()
    }
}

/// A resolution claims each hop's find, and each hop after its first made from no link, within
/// the claims the model watches: one of 16 names, 31 claims past its answer's, is judged, and one
/// of 17 caught.
#[test]
fn a_resolution_claims_within_the_claims_the_model_watches() {
    let deep = deep(17);
    let resolving = |depth: NodeID| {
        let hops: Vec<Record> = (1..=depth)
            .flat_map(|node| hop(u64::from(node) + 1, node - 1, "d", Some(node)))
            .collect();
        let path = vec!["d"; depth as usize].join("/");
        let calls = vec![resolution((1, 20), &path, Answer::Found(Some(depth)))];
        linked(&deep, &hops, calls)
    };
    clean(&resolving(16));
    caught(
        &resolving(17),
        20,
        "made 33 claims past its answer's (0 nodes passed over as taken, 17 lookups, 0 by its \
         walks, 0 marks' heads kept, 16 hops from no link)",
    );
}

/// The State made by hand with `l` moved into `a/`, which holds `l`, `g` and `f`, `l` pinning
/// [`PINNED`] in [`FIRST`].
fn nested() -> Snapshot {
    let mut made = made();
    let records = &mut made.blocks[0].records;
    // `l` off the root's chain, which it heads, to the head of `a/`'s.
    records[0].child = records[L as usize].sibling;
    records[L as usize].sibling = records[A as usize].child;
    records[A as usize].child = L;
    records[L as usize].parent = A;
    made
}

/// A resolution crossing a link at its second hop reads the link's pin after that hop: a repin
/// between its hops leaves it the second pin alone.
#[test]
fn a_resolution_crossing_at_a_later_hop_reads_the_pin_after_it() {
    let nested = nested();
    let (a, c) = (PINNED + 1, OTHER + 1);
    let between = [
        hop(2, ROOT_NODE, "a", Some(A)),
        repinned(&nested, 3),
        hop(4, A, "l", Some(L)),
    ]
    .concat();
    let resolving = |path, answer| vec![resolution((1, 6), path, answer)];
    clean(&linked(
        &nested,
        &between,
        resolving("a/l/a", Answer::Found(None)),
    ));
    clean(&linked(
        &nested,
        &between,
        resolving("a/l/c", linked_to(c, SECOND)),
    ));
    caught(
        &linked(&nested, &between, resolving("a/l/a", linked_to(a, FIRST))),
        6,
        "through the link",
    );
}

/// A resolution stopping at a directory with names of its path left crossed no link: caught as an
/// answer through one.
#[test]
fn a_resolution_stopping_at_a_directory_with_names_left_is_caught() {
    let made = made();
    let stopped = hop(2, ROOT_NODE, "a", Some(A));
    caught(
        &linked(
            &made,
            &stopped,
            vec![resolution((1, 5), "a/x/y", Answer::Found(None))],
        ),
        5,
        "through the link",
    );
}

/// [`FIRST`] as a dirty linked State saves it, below [`PINNED`]: `a` a dirty file, `b` a clean one,
/// `n` a dirty-add directory holding the dirty-add file `m`, and `o` a dirty-delete directory
/// holding `p`.
fn dirtied() -> Snapshot {
    revision_with(
        PINNED,
        &[
            ("a", Kind::File, NodeFlags::DirtyModify),
            ("b", Kind::File, NodeFlags::empty()),
            ("n", Kind::Directory, NodeFlags::DirtyAdd),
            ("n/m", Kind::File, NodeFlags::DirtyAdd),
            ("o", Kind::Directory, NodeFlags::DirtyDelete),
            ("o/p", Kind::File, NodeFlags::empty()),
        ],
    )
}

/// The paths a collection from the root reports through `l` pinning [`PINNED`] in [`dirtied`].
const THROUGH: [&str; 3] = ["l/a", "l/n/m", "l/o"];

/// The violation the model reports of a collection leaving out a path below a dirty link it named.
const BELOW: &str = "leaving out a path its pinned revision holds below it";

/// The violation the model reports of a collection leaving out a path below a dirty link it named,
/// written since its directory's head read, of each pin the link held since.
const EACH_PIN: &str = "leaving out a path each pin it held since its directory's head read holds";

/// The violation the model reports of a path reported more times than what it named spells it.
const TWICE: &str = "more times than the children it named spell it";

/// A revision no load read, which a repin pins.
const UNREAD: u64 = 99;

/// A third revision, holding what [`dirtied`] holds.
const THIRD: u64 = 3;

/// A revision holding a dirty link below the node a pin exposes, which a repin pins.
const NESTED: u64 = 4;

/// `l`'s words in `made`, dirty, pinning `child` in the revision `revision` names.
fn pinning_dirty(made: &Snapshot, (revision, child): (u64, NodeID)) -> Node {
    let mut words = flagged_of(made, L, NodeFlags::Dirty);
    words.address.hash = Hash::from_u64(revision);
    words.child = child;
    words
}

/// `l` in `made` marked dirty at seq 1 by another task, then a collection from the root: its head
/// read at 2 and its naming of `l` at 3.
fn naming_l(made: &Snapshot) -> Vec<Record> {
    [
        flagging_of(made, 1, L, NodeFlags::Dirty),
        head_of(ROOT_NODE, 2, 0),
        name_read(3, L, "l"),
    ]
    .concat()
}

/// `l` in `made` marked dirty at seq 1 by another task, then a collection from the root: its head
/// read at 2, `written` landing at 3, and its naming of `l` at 4.
fn written_between(made: &Snapshot, written: Vec<Record>) -> Vec<Record> {
    [
        flagging_of(made, 1, L, NodeFlags::Dirty),
        head_of(ROOT_NODE, 2, 0),
        written,
        name_read(4, L, "l"),
    ]
    .concat()
}

/// The model of `made` with `records` replayed with a collection from the root over `span`
/// reporting `paths`, [`FIRST`] as `first` and [`SECOND`] provided, under the gate or not.
fn collected_through(
    (made, first): (&Snapshot, Snapshot),
    records: &[Record],
    (span, paths): ((u64, u64), &[&str]),
    gate: bool,
) -> Model {
    let calls = vec![collect(ROOT_NODE, span, paths)];
    if gate {
        gated_providing(made, revisions(first), records, calls)
    } else {
        replayed_providing(made, revisions(first), records, calls)
    }
}

/// A collection naming a dirty link reports the paths a collection of its pinned revision makes
/// from the node the pin exposes, spelled below the link's path: dirty files, dirty deletes and
/// dirty adds staged or heading no chain, nothing below a dirty delete; the link's own path where
/// the node is a dirty add with no child but a `Discarded` one, none where it is a file. One left
/// out, one more, one twice, a clean file's, one below a dirty delete, a dirty add its children
/// carry or the link's own is caught.
#[test]
fn a_collection_through_a_dirty_link_reports_what_its_pinned_revision_holds() {
    let made = made();
    let named = naming_l(&made);
    let span = (2, 5);
    for gate in [false, true] {
        let collecting =
            |paths: &[&str]| collected_through((&made, dirtied()), &named, (span, paths), gate);
        clean(&collecting(&THROUGH));
        caught(&collecting(&["l/a", "l/o"]), 5, BELOW);
        for more in ["l/b", "l/o/p", "l/n", "l"] {
            let paths = [&THROUGH[..], &[more]].concat();
            caught(&collecting(&paths), 5, UNNAMED);
        }
        caught(&collecting(&[&THROUGH[..], &["l/a"]].concat()), 5, TWICE);
    }
    // `src` a dirty add with no child, or none but a `Discarded` one, which the walk passes.
    for children in [&[][..], &[("x", Kind::File, NodeFlags::Discarded)]] {
        let mut empty_add = revision_with(PINNED, children);
        empty_add.blocks[0].records[PINNED as usize].flags |= NodeFlags::DirtyAdd.bits();
        let collecting = |paths: &[&str]| {
            collected_through((&made, empty_add.clone()), &named, (span, paths), true)
        };
        clean(&collecting(&["l"]));
        caught(&collecting(&[]), 5, BELOW);
    }
    // `l` exposing `a`, a dirty file: none.
    let exposing_a = pinning(PINNED + 1);
    let named = naming_l(&exposing_a);
    let collecting =
        |paths: &[&str]| collected_through((&exposing_a, dirtied()), &named, (span, paths), true);
    clean(&collecting(&[]));
    caught(&collecting(&["l"]), 5, UNNAMED);
}

/// A collection follows the pin its link's words held as it read them, between its directory's
/// head read and its naming of the link: under the gate the words the model held at the naming; in
/// a run of no gate those where no write landed on the link since the head read, else a pin the
/// link held since whose paths it reported every one of, each path no more times than such a pin
/// gives it, none such a violation, a pin of a revision no load read none it read, and one the
/// model does not follow leaving the paths no naming spells unjudged, the collection's
/// completeness holding it to a pin the link held throughout.
#[test]
fn a_collection_through_a_dirty_link_follows_the_pin_it_read() {
    let made = made();
    let second = pinning_dirty(&made, (SECOND, OTHER));
    let repin = |seq| rewritten_of(L, seq, second);
    let collecting = |records: &[Record], paths: &[&str], gate| {
        collected_through((&made, dirtied()), records, ((2, 7), paths), gate)
    };
    let more = |more: &'static str| [&THROUGH[..], &[more]].concat();
    // Repinned after the naming: the first pin's paths.
    let after = [naming_l(&made), repin(4)].concat();
    for gate in [false, true] {
        clean(&collecting(&after, &THROUGH, gate));
        caught(&collecting(&after, &[], gate), 7, BELOW);
    }
    // Repinned between the head read and the naming: the second pin's, none, under the gate; either
    // pin's in a run of no gate, not a part of the first's, and no other path.
    let between = written_between(&made, repin(3));
    clean(&collecting(&between, &[], true));
    caught(&collecting(&between, &THROUGH, true), 7, UNNAMED);
    clean(&collecting(&between, &[], false));
    clean(&collecting(&between, &THROUGH, false));
    caught(&collecting(&between, &["l/a"], false), 7, UNNAMED);
    for stray in ["l/b", "x"] {
        caught(&collecting(&between, &more(stray), false), 7, UNNAMED);
    }
    // Repinned from the second revision to the first between them: the pin it holds now among
    // those it held.
    let mut from_second = made.clone();
    let link = &mut from_second.blocks[0].records[L as usize];
    (link.address.hash, link.child) = (Hash::from_u64(SECOND), OTHER);
    let to_first = written_between(
        &from_second,
        rewritten_of(L, 3, pinning_dirty(&from_second, (FIRST, PINNED))),
    );
    for paths in [&[][..], &THROUGH] {
        let model = collected_through((&from_second, dirtied()), &to_first, ((2, 7), paths), false);
        clean(&model);
    }
    // A repin's own section, its `Repin` before its `Word`: the pin before it among those held,
    // not the first revision with the new pin's node, `n`, whose `m` it would report.
    let to_n = pinning_dirty(&made, (SECOND, PINNED + 3));
    let repin_section = section(
        3,
        Section::Repin,
        vec![
            Effect::Repin {
                node: L,
                child: PINNED + 3,
                whole: true,
            },
            word(L, to_n, false),
        ],
    );
    let sectioned = written_between(&made, by(1, repin_section));
    clean(&collecting(&sectioned, &THROUGH, false));
    caught(&collecting(&sectioned, &more("l/m"), false), 7, UNNAMED);
    // Repinned to a revision no load read between them: no pin the collection read.
    let unread = written_between(
        &made,
        rewritten_of(L, 3, pinning_dirty(&made, (UNREAD, OTHER))),
    );
    clean(&collecting(&unread, &THROUGH, false));
    caught(&collecting(&unread, &more("l/b"), false), 7, UNNAMED);
    // Repinned between them to a revision holding a dirty link below the pinned node, which the
    // model does not follow: the paths no naming spells unjudged, but no path more times than a
    // pin it reported gives it, and none left out at every read.
    let unfollowed = written_between(
        &made,
        rewritten_of(L, 3, pinning_dirty(&made, (NESTED, PINNED))),
    );
    let collecting_unfollowed = |paths: &[&str]| {
        let nested = revision_with(PINNED, &[("a", Kind::Link, NodeFlags::Dirty)]);
        let provided = [revisions(dirtied()), vec![(Hash::from_u64(NESTED), nested)]].concat();
        let calls = vec![collect(ROOT_NODE, (2, 7), paths)];
        replayed_providing(&made, provided, &unfollowed, calls)
    };
    clean(&collecting_unfollowed(&more("l/z")));
    caught(&collecting_unfollowed(&more("l/a")), 7, TWICE);
    caught(&collecting_unfollowed(&["l/a"]), 7, LEFT_OUT);
    // Repinned between them to another revision holding the same paths: each no more times than
    // one pin gives it, and a part of them caught.
    let third = Hash::from_u64(THIRD);
    let alike = written_between(
        &made,
        rewritten_of(L, 3, pinning_dirty(&made, (THIRD, PINNED))),
    );
    let collecting_alike = |paths: &[&str]| {
        let revisions = [revisions(dirtied()), vec![(third, dirtied())]].concat();
        let calls = vec![collect(ROOT_NODE, (2, 7), paths)];
        replayed_providing(&made, revisions, &alike, calls)
    };
    clean(&collecting_alike(&THROUGH));
    caught(&collecting_alike(&more("l/a")), 7, TWICE);
    caught(&collecting_alike(&["l/a"]), 7, EACH_PIN);
    // Written between them keeping the pin: its pin's paths alone, every one of them.
    let kept = written_between(
        &made,
        rewritten_of(L, 3, flagged_of(&made, L, NodeFlags::Dirty)),
    );
    clean(&collecting(&kept, &THROUGH, false));
    caught(&collecting(&kept, &["l/a"], false), 7, EACH_PIN);
    caught(&collecting(&kept, &more("l/b"), false), 7, UNNAMED);
    // Repinned before the head read: the second pin's alone.
    let before = [
        flagging_of(&made, 1, L, NodeFlags::Dirty),
        repin(2),
        head_of(ROOT_NODE, 3, 0),
        name_read(4, L, "l"),
    ]
    .concat();
    for gate in [false, true] {
        clean(&collecting(&before, &[], gate));
        caught(&collecting(&before, &THROUGH, gate), 7, UNNAMED);
    }
}

/// A collection passing over a dirty link in place below dirty directories throughout leaves out
/// the paths its pinned revision holds below it: caught where the revision holds any, or where no
/// load read it, which following the link makes; not where the link is clean, or once repinned to
/// a revision holding none.
#[test]
fn a_collection_passing_over_a_dirty_link_is_caught() {
    let made = made();
    let dirty = flagging_of(&made, 1, L, NodeFlags::Dirty);
    let through =
        |records: &[Record]| collected_through((&made, dirtied()), records, ((2, 6), &[]), false);
    let passed = [dirty.clone(), head_of(ROOT_NODE, 2, 0)].concat();
    caught(&through(&passed), 6, LEFT_OUT);
    caught(
        &replayed_with(&made, &passed, vec![collect(ROOT_NODE, (2, 6), &[])]),
        6,
        LEFT_OUT,
    );
    clean(&through(&head_of(ROOT_NODE, 2, 0)));
    let second = pinning_dirty(&made, (SECOND, OTHER));
    clean(&through(
        &[dirty, head_of(ROOT_NODE, 2, 0), rewritten_of(L, 3, second)].concat(),
    ));
}

/// A dirty link in a dirty link's pinned revision, which the collection follows into a third
/// State, is not modelled, as no fixture holds one: caught.
#[test]
fn a_collection_meeting_a_dirty_link_below_a_dirty_link_is_caught() {
    let made = made();
    let nested = revision_with(PINNED, &[("a", Kind::Link, NodeFlags::Dirty)]);
    caught(
        &collected_through((&made, nested), &naming_l(&made), ((2, 5), &[]), true),
        5,
        "pinning a revision the model does not follow below it",
    );
}
