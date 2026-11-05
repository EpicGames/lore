// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A listing's `ChainMark` and the reads by it held to the model from the mark's own read, its
//! listing's `Head` (`find_subnode_added_since` and `ChainMark::reattached`): a node a search since
//! the mark found carried the name; a search answering none is a violation where a node that joined
//! the directory, or was repositioned in it, after the mark carried the name throughout, and, under
//! the gate, where the count had moved by a reattach past the mark's as it began, the mark's head
//! in the chain throughout; a stale search and a mark found reattached need the count moved by a
//! reattach past the mark's, less the prepends its holder counted, or a stale search the directory
//! no longer one; a mark found not reattached, under the gate, needs the count not moved so as the
//! read began. A stage's claim reads by the mark of its own listing, each read held so within its
//! own interval, by the prepends the call counted by then, and needing the call's own marked
//! listing of the directory.

use lore_revision::node::NodeFlags;
use lore_revision::state::AddedSince;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::G;
use super::H;
use super::STATE;
use super::attach;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::gated;
use super::head;
use super::head_of;
use super::moved;
use super::moved_out;
use super::published;
use super::replayed_with;
use super::repositioned;
use super::rewritten_of;
use super::section;
use super::snapshot;
use super::walk_of;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Filter;
use crate::harness::catalogue::call::Lookup;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Over;
use crate::harness::catalogue::call::Repeats;
use crate::harness::catalogue::call::Yielded;
use crate::harness::model::Model;

/// The marked listing of `a/` by task 0 over `[begin, end)`, begun at its begin, yielding `g` and
/// `f` by their ids and names' hashes, its repeats dropped.
fn marked((begin, end): (u64, u64)) -> Call {
    let yielded = [(G, "g"), (F, "f")]
        .map(|(node, name)| Yielded {
            node,
            record: None,
            name: Some(hash_string(name)),
            spelling: None,
        })
        .to_vec();
    walk_of(
        &catalogue::LISTED_MARKED,
        (A, Over::Chain),
        (begin, end),
        yielded,
        (Repeats::Dropped, Filter::EVERY),
    )
}

/// A search of `a/` for `name` since the mark of [`marked`]'s call by task 0 over `[begin, end)`,
/// answering `answer`, the mark's holder having counted `prepends` of its own in it.
fn since(span: (u64, u64), name: &str, answer: Answer, prepends: u32) -> Call {
    Call {
        id: 2,
        target: Some(A),
        name: Some(hash_string(name)),
        listing: Some(1),
        prepends,
        ..base_call(&catalogue::ADDED_SINCE, span, Outcome::Answer(answer))
    }
}

/// A read by task 0 over `[begin, end)` of whether the mark of [`marked`]'s call found `a/`
/// reattached, answering `reattached`.
fn reattached(span: (u64, u64), reattached: bool) -> Call {
    Call {
        id: 2,
        target: Some(A),
        listing: Some(1),
        ..base_call(
            &catalogue::REATTACHED_SINCE,
            span,
            Outcome::Answer(Answer::Reattached(reattached)),
        )
    }
}

const NOT_ADDED: &str = "a node that joined it since the mark carrying the name throughout";
const NOT_MOVED: &str = "its count moving by less than a reattach past the mark's";
const NOWHERE: &str = "carrying the name at no seq of its interval";
const REATTACHED_BEFORE: &str =
    "answered not reattached, its count moved by a reattach past the mark's as it began";
const HEAD_KEPT: &str = "the mark's head in its chain throughout";
const OWN_UNMARKED: &str = "read by its own mark of";

/// A search since a mark answering a node holds it to carrying the name in the directory at some
/// `seq` of its interval.
#[test]
fn a_search_since_a_mark_answers_a_node_carrying_the_name() {
    let made = snapshot();
    let read = [head(1, 0), published(3)].concat();
    clean(&replayed_with(
        &made,
        &read,
        vec![
            marked((1, 3)),
            since((6, 8), "n", Answer::Since(Some(FREE)), 0),
        ],
    ));
    caught(
        &replayed_with(
            &made,
            &read,
            vec![
                marked((1, 3)),
                since((6, 8), "n", Answer::Since(Some(G)), 0),
            ],
        ),
        8,
        NOWHERE,
    );
}

/// A search since a mark answering none is a violation where a node published or repositioned in
/// the directory after the mark carried the name throughout its interval; not one there at the
/// mark, which the listing holds, nor one published once the search began.
#[test]
fn a_search_since_a_mark_misses_only_what_did_not_join_since() {
    let made = snapshot();
    let none = |span, name| since(span, name, Answer::Since(None), 0);
    // Published after the mark, before the search.
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), published(3)].concat(),
            vec![marked((1, 3)), none((6, 8), "n")],
        ),
        8,
        NOT_ADDED,
    );
    // Published before the mark, so listed.
    let mut listing = marked((4, 6));
    listing.walks[0].yielded.insert(
        0,
        Yielded {
            node: FREE,
            record: None,
            name: Some(hash_string("n")),
            spelling: None,
        },
    );
    clean(&replayed_with(
        &made,
        &[by(1, attach(1, A, "n")), head(4, 1)].concat(),
        vec![listing, none((6, 8), "n")],
    ));
    // Published once the search began.
    clean(&replayed_with(
        &made,
        &[head(1, 0), published(4)].concat(),
        vec![marked((1, 3)), none((3, 8), "n")],
    ));
    // Repositioned after the mark.
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), repositioned(3, "f")].concat(),
            vec![marked((1, 3)), none((4, 6), "f")],
        ),
        6,
        NOT_ADDED,
    );
    // There at the mark, untouched.
    clean(&replayed_with(
        &made,
        &head(1, 0),
        vec![marked((1, 3)), none((4, 6), "f")],
    ));
}

/// A stale search needs the directory's count moved by a reattach past the mark's, less the
/// prepends its holder counted: a reattach out of it, not a publish.
#[test]
fn a_stale_search_needs_a_reattach_since_the_mark() {
    let made = snapshot();
    let stale = |span, prepends| since(span, "n", Answer::Stale, prepends);
    caught(
        &replayed_with(&made, &head(1, 0), vec![marked((1, 3)), stale((4, 6), 0)]),
        6,
        NOT_MOVED,
    );
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), published(3)].concat(),
            vec![marked((1, 3)), stale((6, 8), 0)],
        ),
        8,
        NOT_MOVED,
    );
    clean(&replayed_with(
        &made,
        &[head(1, 0), moved_out(3)].concat(),
        vec![marked((1, 3)), stale((4, 6), 0)],
    ));
    // Its own prepend counted with the reattach, and counted with none.
    clean(&replayed_with(
        &made,
        &[head(1, 0), by(0, attach(3, A, "o")), moved_out(6)].concat(),
        vec![marked((1, 3)), stale((7, 9), 1)],
    ));
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), moved_out(3)].concat(),
            vec![marked((1, 3)), stale((4, 6), 1)],
        ),
        6,
        NOT_MOVED,
    );
}

/// A mark found reattached needs the directory's count moved by a reattach past the mark's; one
/// found not reattached, under the gate, needs it not moved so as the read began, the reattach
/// before it, not after.
#[test]
fn a_mark_found_reattached_or_not_matches_the_count_since_it() {
    let made = snapshot();
    caught(
        &replayed_with(
            &made,
            &head(1, 0),
            vec![marked((1, 3)), reattached((4, 6), true)],
        ),
        6,
        NOT_MOVED,
    );
    clean(&replayed_with(
        &made,
        &[head(1, 0), moved_out(3)].concat(),
        vec![marked((1, 3)), reattached((4, 6), true)],
    ));
    let moved = [head(1, 0), moved_out(3)].concat();
    caught(
        &gated(
            &made,
            &moved,
            vec![marked((1, 3)), reattached((4, 6), false)],
        ),
        6,
        REATTACHED_BEFORE,
    );
    clean(&replayed_with(
        &made,
        &moved,
        vec![marked((1, 3)), reattached((4, 6), false)],
    ));
    clean(&gated(
        &made,
        &[head(1, 0), moved_out(5)].concat(),
        vec![marked((1, 3)), reattached((4, 7), false)],
    ));
}

const UNMARKED: &str = "read by a mark no marked listing of its directory made";

/// A read by a mark needs the marked listing of its directory it names, on its State, whichever
/// task made it: one ended at the `seq` the read began at, as one task's calls with no record
/// between them meet, counts.
#[test]
fn a_read_by_a_mark_needs_the_marked_listing_it_names() {
    let made = snapshot();
    let unmarked = |read: Call, listing: Call| {
        caught(
            &replayed_with(&made, &head(1, 0), vec![listing, read]),
            6,
            UNMARKED,
        );
    };
    let mut naming_none = reattached((4, 6), false);
    naming_none.listing = None;
    unmarked(naming_none, marked((1, 3)));
    let mut naming_another = reattached((4, 6), false);
    naming_another.listing = Some(7);
    unmarked(naming_another, marked((1, 3)));
    let mut of_another = reattached((4, 6), false);
    of_another.target = Some(B);
    unmarked(of_another, marked((1, 3)));
    let mut unmarking = marked((1, 3));
    unmarking.kind = &catalogue::LISTED_NAME_HASHES;
    unmarked(reattached((4, 6), false), unmarking);
    // A listing of another State, its records replayed before the read's State's.
    let other = STATE - 1;
    let mut on_another = Model::new();
    on_another.start(STATE, &made);
    on_another.start(other, &made);
    let mut listing = marked((1, 3));
    listing.state = other;
    let records: Vec<Record> = (head(1, 0).into_iter())
        .map(|record| Record {
            state: other,
            ..record
        })
        .collect();
    on_another.replay_called(
        &records,
        vec![listing, reattached((4, 6), false)],
        Vec::new(),
    );
    caught(&on_another, 6, UNMARKED);
    let mut by_another = marked((1, 3));
    by_another.task = Some(1);
    clean(&replayed_with(
        &made,
        &by(1, head(1, 0)),
        vec![by_another, reattached((4, 6), false)],
    ));
    clean(&replayed_with(
        &made,
        &head(1, 0),
        vec![marked((1, 3)), reattached((3, 5), false)],
    ));
}

/// `g`, the head of `a/`'s chain as the snapshot makes it, moved into `b` at `seq` by another task.
fn head_moved_out(seq: u64) -> Vec<Record> {
    by(
        1,
        section(seq, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
    )
}

/// Under the gate, a search since a mark answering none is a violation where the count had moved
/// by a reattach past the mark's as it began and the mark's head stood in the chain throughout, as
/// its walk then meets the head and finds the count moved by its end: not where the head left the
/// chain, which turns the walk to the whole chain, nor where the count moved once it began, nor
/// without the gate.
#[test]
fn a_search_since_a_mark_answering_none_needs_no_reattach_before_its_head() {
    let made = snapshot();
    let none = |span| since(span, "n", Answer::Since(None), 0);
    let moved = [head(1, 0), moved_out(3)].concat();
    caught(
        &gated(&made, &moved, vec![marked((1, 3)), none((4, 6))]),
        6,
        HEAD_KEPT,
    );
    clean(&replayed_with(
        &made,
        &moved,
        vec![marked((1, 3)), none((4, 6))],
    ));
    clean(&gated(
        &made,
        &[head(1, 0), head_moved_out(3)].concat(),
        vec![marked((1, 3)), none((4, 6))],
    ));
    clean(&gated(
        &made,
        &[head(1, 0), moved_out(5)].concat(),
        vec![marked((1, 3)), none((4, 7))],
    ));
}

/// Under the gate, a search since the mark of an empty directory answering none holds however the
/// count moved: with no head to stop at, the search walks the whole chain against the count it
/// read as it began.
#[test]
fn a_search_since_a_mark_of_an_empty_directory_answering_none_holds() {
    let made = snapshot();
    let moving = |seq, (node, name), (from, to)| {
        by(
            1,
            section(
                seq,
                Section::MoveNamed,
                vec![moved(node, (from, to), name, None)],
            ),
        )
    };
    let records = [
        moving(1, (H, "h"), (B, A)),
        head_of(B, 2, REATTACH_CHANGES),
        moving(4, (F, "f"), (A, B)),
        moving(5, (F, "f"), (B, A)),
    ]
    .concat();
    let listing = walk_of(
        &catalogue::LISTED_MARKED,
        (B, Over::Chain),
        (2, 3),
        Vec::new(),
        (Repeats::Dropped, Filter::EVERY),
    );
    let mut search = since((6, 8), "n", Answer::Since(None), 0);
    search.target = Some(B);
    clean(&gated(&made, &records, vec![listing, search]));
}

/// A stale search needs the count moved by a reattach at some `seq` of its interval, one landing
/// while it walks included; or the directory no longer one, not merely `Discarded`, which the
/// search walks as any directory.
#[test]
fn a_stale_search_holds_by_a_reattach_within_it_or_a_directory_gone() {
    let made = snapshot();
    let stale = |span| since(span, "n", Answer::Stale, 0);
    clean(&replayed_with(
        &made,
        &[head(1, 0), moved_out(5)].concat(),
        vec![marked((1, 3)), stale((4, 7))],
    ));
    let rewritten = |flags: NodeFlags| {
        let mut record = made.blocks[0].records[A as usize];
        record.flags |= flags.bits();
        rewritten_of(A, 3, record)
    };
    clean(&replayed_with(
        &made,
        &[head(1, 0), rewritten(NodeFlags::File)].concat(),
        vec![marked((1, 3)), stale((4, 6))],
    ));
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), rewritten(NodeFlags::Discarded)].concat(),
            vec![marked((1, 3)), stale((4, 6))],
        ),
        6,
        NOT_MOVED,
    );
}

/// A stage's claim of entries of `a/` by task 0 over `[begin, end)`: its marked listing, begun at
/// its begin as [`marked`]'s, then `lookups`, its reads by the listing's own mark, answering done.
fn claim(span: (u64, u64), lookups: Vec<Lookup>) -> Call {
    Call {
        kind: &catalogue::STAGE_CLAIM,
        lookups,
        outcome: Outcome::Done(Answer::None),
        ..marked(span)
    }
}

/// A search of `a/` for `name` since the claim's own mark over `[from, to)`, answering `searched`,
/// with `prepends` of the claim's own counted in the mark by then.
fn own_since(name: &str, searched: AddedSince, prepends: u32, (from, to): (u64, u64)) -> Lookup {
    Lookup::Since {
        dir: A,
        name: hash_string(name),
        searched,
        prepends,
        from,
        to,
    }
}

/// A read over `[from, to)` of whether the claim's own mark found `a/` reattached, answering
/// `reattached`, with `prepends` of the claim's own counted in the mark by then.
fn own_reattached(reattached: bool, prepends: u32, (from, to): (u64, u64)) -> Lookup {
    Lookup::Reattached {
        dir: A,
        reattached,
        prepends,
        from,
        to,
    }
}

/// A search since the call's own mark answering a node holds it to carrying the name in the
/// directory at some `seq` of the search's interval.
#[test]
fn a_search_since_its_own_mark_answers_a_node_carrying_the_name() {
    let made = snapshot();
    let read = [head(1, 0), published(3)].concat();
    let found = |node| {
        claim(
            (1, 9),
            vec![own_since("n", AddedSince::Found(node), 0, (6, 8))],
        )
    };
    clean(&replayed_with(&made, &read, vec![found(FREE)]));
    caught(&replayed_with(&made, &read, vec![found(G)]), 9, NOWHERE);
}

/// Each read by the call's own mark is held within its own interval, not the call's: a node found
/// published only at or after its search's end, or a reattach found landing only at or after its
/// read's, is a violation, though the call goes on past both. A read taking no `seq`, as the
/// mark's reattach read does, is held to the model as it stood at its `seq`.
#[test]
fn a_read_by_its_own_mark_is_held_within_its_own_interval() {
    let made = snapshot();
    let read = [head(1, 0), published(3)].concat();
    let found = |span| {
        claim(
            (1, 9),
            vec![own_since("n", AddedSince::Found(FREE), 0, span)],
        )
    };
    caught(
        &replayed_with(&made, &read, vec![found((2, 4))]),
        9,
        NOWHERE,
    );
    caught(
        &replayed_with(&made, &read, vec![found((2, 5))]),
        9,
        NOWHERE,
    );
    clean(&replayed_with(&made, &read, vec![found((2, 6))]));
    let moved = [head(1, 0), moved_out(5)].concat();
    let reattached = |span| claim((1, 9), vec![own_reattached(true, 0, span)]);
    for ended in [(2, 4), (2, 5), (5, 5)] {
        caught(
            &replayed_with(&made, &moved, vec![reattached(ended)]),
            9,
            NOT_MOVED,
        );
    }
    for held in [(2, 6), (6, 6)] {
        clean(&replayed_with(&made, &moved, vec![reattached(held)]));
    }
}

/// A search since the call's own mark answering none is a violation where a node published or
/// repositioned in the directory after the mark carried the name throughout its interval; not one
/// published once it began, nor one published before the mark, which its listing holds.
#[test]
fn a_search_since_its_own_mark_misses_only_what_did_not_join_since() {
    let made = snapshot();
    let none = |name, prepends, span| {
        claim(
            (1, 9),
            vec![own_since(name, AddedSince::Absent, prepends, span)],
        )
    };
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), published(3)].concat(),
            vec![none("n", 0, (6, 8))],
        ),
        9,
        NOT_ADDED,
    );
    clean(&replayed_with(
        &made,
        &[head(1, 0), published(4)].concat(),
        vec![none("n", 0, (3, 8))],
    ));
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), repositioned(3, "f")].concat(),
            vec![none("f", 0, (4, 6))],
        ),
        9,
        NOT_ADDED,
    );
    let mut listed = none("n", 0, (6, 8));
    listed.begin = 4;
    listed.walks[0].from = 4;
    listed.walks[0].yielded.insert(
        0,
        Yielded {
            node: FREE,
            record: None,
            name: Some(hash_string("n")),
            spelling: None,
        },
    );
    clean(&replayed_with(
        &made,
        &[by(1, attach(1, A, "n")), head(4, 1)].concat(),
        vec![listed],
    ));
}

/// A stale search since the call's own mark needs the directory's count moved by a reattach past
/// the mark's with the prepends the call counted by then: its own add counted with a reattach
/// holds, a reattach less its add counted with none does not.
#[test]
fn a_stale_search_since_its_own_mark_counts_its_own_prepends() {
    let made = snapshot();
    let stale = |prepends, span| {
        claim(
            (1, 12),
            vec![own_since("n", AddedSince::Stale, prepends, span)],
        )
    };
    caught(
        &replayed_with(&made, &head(1, 0), vec![stale(0, (4, 6))]),
        12,
        NOT_MOVED,
    );
    clean(&replayed_with(
        &made,
        &[head(1, 0), attach(3, A, "o"), moved_out(6)].concat(),
        vec![stale(1, (7, 9))],
    ));
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), moved_out(3)].concat(),
            vec![stale(1, (4, 6))],
        ),
        12,
        NOT_MOVED,
    );
}

/// A read of the call's own mark found reattached needs the count moved by a reattach past the
/// mark's; found not reattached, under the gate, the count not moved so as the read began.
#[test]
fn a_read_of_its_own_mark_found_reattached_or_not_matches_the_count_since_it() {
    let made = snapshot();
    let read = |reattached, span| claim((1, 9), vec![own_reattached(reattached, 0, span)]);
    caught(
        &replayed_with(&made, &head(1, 0), vec![read(true, (4, 6))]),
        9,
        NOT_MOVED,
    );
    let moved = [head(1, 0), moved_out(3)].concat();
    clean(&replayed_with(&made, &moved, vec![read(true, (4, 6))]));
    caught(
        &gated(&made, &moved, vec![read(false, (4, 6))]),
        9,
        REATTACHED_BEFORE,
    );
    clean(&replayed_with(&made, &moved, vec![read(false, (4, 6))]));
    clean(&gated(
        &made,
        &[head(1, 0), moved_out(5)].concat(),
        vec![read(false, (4, 7))],
    ));
}

/// Under the gate, a search since the call's own mark answering none is a violation where the
/// count had moved by a reattach past the mark's as it began and the mark's head stood in the
/// chain throughout its interval, though the head left it at or after its end: not where the head
/// left the chain within it, nor where the count moved once it began, nor without the gate.
#[test]
fn a_search_since_its_own_mark_answering_none_needs_no_reattach_before_its_head() {
    let made = snapshot();
    let none = |span| claim((1, 9), vec![own_since("n", AddedSince::Absent, 0, span)]);
    let moved = [head(1, 0), moved_out(3)].concat();
    caught(&gated(&made, &moved, vec![none((4, 6))]), 9, HEAD_KEPT);
    let head_left = |seq| [head(1, 0), moved_out(3), head_moved_out(seq)].concat();
    caught(
        &gated(&made, &head_left(6), vec![none((4, 6))]),
        9,
        HEAD_KEPT,
    );
    clean(&replayed_with(&made, &moved, vec![none((4, 6))]));
    clean(&gated(&made, &head_left(5), vec![none((4, 6))]));
    clean(&gated(
        &made,
        &[head(1, 0), moved_out(5)].concat(),
        vec![none((4, 7))],
    ));
}

/// A read by the call's own mark needs the call's own marked listing of the directory, which read
/// its head: a violation where the call listed another directory or none, or is of a kind whose
/// listing carries no mark.
#[test]
fn a_read_by_its_own_mark_needs_its_own_marked_listing() {
    let made = snapshot();
    let read = || claim((1, 9), vec![own_reattached(false, 0, (4, 6))]);
    let mut of_another = read();
    of_another.walks[0].dir = B;
    caught(
        &replayed_with(&made, &head_of(B, 1, 0), vec![of_another]),
        9,
        OWN_UNMARKED,
    );
    let mut unlisted = read();
    unlisted.walks.clear();
    caught(
        &replayed_with(&made, &head(1, 0), vec![unlisted]),
        9,
        OWN_UNMARKED,
    );
    let mut unmarking = read();
    unmarking.kind = &catalogue::LISTED_NAME_HASHES;
    caught(
        &replayed_with(&made, &head(1, 0), vec![unmarking]),
        9,
        OWN_UNMARKED,
    );
    clean(&replayed_with(&made, &head(1, 0), vec![read()]));
}

/// The reads by the call's own mark count among the claims the model watches, a search answering
/// none twice, its mark's head kept the second: past the most, a violation.
#[test]
fn reads_by_its_own_mark_count_among_the_claims_watched() {
    let made = snapshot();
    let searches = |count| {
        let search = own_since("n", AddedSince::Absent, 0, (4, 6));
        claim((1, 9), vec![search; count])
    };
    clean(&replayed_with(&made, &head(1, 0), vec![searches(14)]));
    caught(
        &replayed_with(&made, &head(1, 0), vec![searches(15)]),
        9,
        "made 32 claims past its answer's",
    );
}
