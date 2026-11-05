// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A reader's walks of a directory's children held to the model from the walk's head read to its
//! call's end, by the children iterators' rules (`StateNodeChildrenIterator`): no node twice unless
//! the walk reports it may repeat; each node yielded standing in the directory, kept by its
//! caller's filter, with the record, the name's hash and the name read for it at some `seq`; every
//! node in the directory its caller kept throughout yielded, but one a `Move` within it
//! repositioned behind a walk that may repeat, which with no counted change of the directory is
//! every node of the chain at the head read (C6). A listing reports no repeat but drops any, so it
//! yields none twice and may have walked as one that may repeat.

use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::G;
use super::H;
use super::by;
use super::caught;
use super::clean;
use super::discarding;
use super::flagged;
use super::head;
use super::head_of;
use super::moved_out;
use super::published;
use super::replayed_with;
use super::repositioned;
use super::rewritten;
use super::section;
use super::snapshot;
use super::unlink;
use super::walk_of;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Filter;
use crate::harness::catalogue::call::Over;
use crate::harness::catalogue::call::Repeats;
use crate::harness::catalogue::call::Walk;
use crate::harness::catalogue::call::Yielded;
use crate::harness::catalogue::call::spelling;
use crate::harness::model::Snapshot;

/// `node` detached from `a/` at `seq` by another task.
fn detached(node: NodeID, seq: u64) -> Vec<Record> {
    by(
        1,
        section(seq, Section::Detach, vec![unlink(node, A, How::Always)]),
    )
}

const YIELDS: &str = "the name's hash and the name it read at no seq from its head read";
const THROUGHOUT: &str = "kept by its caller, throughout from its head read";

/// A walk of `a/` by task 0 over `[begin, end)`, begun at its begin, yielding `yielded`, each with
/// its record in `made` and no name read, and reporting `may_repeat`.
fn walk(made: &Snapshot, span: (u64, u64), yielded: &[NodeID], may_repeat: bool) -> Call {
    let yielded = yielded
        .iter()
        .map(|&node| Yielded {
            node,
            record: Some(made.blocks[0].records[node as usize]),
            name: None,
            spelling: None,
        })
        .collect();
    let repeats = Repeats::reported(may_repeat);
    walk_of(
        &catalogue::CHILDREN,
        (A, Over::Chain),
        span,
        yielded,
        (repeats, Filter::EVERY),
    )
}

/// The node `n`'s publish in `records` yields: its slot, with the words its publish's section left.
fn n_in(records: &[Record]) -> Yielded {
    let record: Node = records
        .iter()
        .rev()
        .find_map(|record| match &record.effect {
            Effect::Word { node, record, .. } if *node == FREE => Some(record.0),
            _ => None,
        })
        .expect("the publish's word");
    Yielded {
        node: FREE,
        record: Some(record),
        name: None,
        spelling: None,
    }
}

/// A walk yields no node twice, unless it reports it may have repeated.
#[test]
fn a_walk_yields_no_node_twice_unless_it_may_repeat() {
    let made = snapshot();
    caught(
        &replayed_with(
            &made,
            &head(1, 0),
            vec![walk(&made, (1, 3), &[G, F, G], false)],
        ),
        3,
        "yielded 3 twice, reporting no repeat",
    );
    clean(&replayed_with(
        &made,
        &head(1, 0),
        vec![walk(&made, (1, 3), &[G, F, G], true)],
    ));
}

/// Each node a walk yields stood in the directory with the record and the name read for it at some
/// `seq` from its head read: not a node of another directory, one that left before, one
/// `Discarded`, nor one read with other words or by another spelling.
#[test]
fn each_node_a_walk_yields_stood_in_the_directory_with_its_record() {
    let made = snapshot();
    clean(&replayed_with(
        &made,
        &head(1, 0),
        vec![walk(&made, (1, 3), &[G, F], false)],
    ));
    caught(
        &replayed_with(
            &made,
            &head(1, 0),
            vec![walk(&made, (1, 3), &[G, F, H], false)],
        ),
        3,
        YIELDS,
    );
    // Moved out after the head read, and before it.
    clean(&replayed_with(
        &made,
        &[head(1, 0), moved_out(2)].concat(),
        vec![walk(&made, (1, 4), &[G, F], true)],
    ));
    caught(
        &replayed_with(
            &made,
            &[moved_out(1), head(3, REATTACH_CHANGES)].concat(),
            vec![walk(&made, (2, 5), &[G, F], true)],
        ),
        5,
        YIELDS,
    );
    // Read with words it never had.
    let mut dirty = walk(&made, (1, 3), &[G, F], false);
    let record = dirty.walks[0].yielded[1].record.as_mut();
    record.expect("a record read").flags |= NodeFlags::Dirty.bits();
    caught(&replayed_with(&made, &head(1, 0), vec![dirty]), 3, YIELDS);
    // `Discarded` in place before the walk.
    caught(
        &replayed_with(
            &made,
            &[discarding(&made, 1), head(2, 0)].concat(),
            vec![walk(&made, (2, 4), &[G, F], false)],
        ),
        4,
        YIELDS,
    );
    // Read by its own spelling, and by another.
    let spelled = |name: &str| {
        let mut call = walk(&made, (1, 3), &[G, F], false);
        call.walks[0].yielded[1].spelling = Some(spelling(name.as_bytes()));
        call
    };
    clean(&replayed_with(&made, &head(1, 0), vec![spelled("f")]));
    caught(
        &replayed_with(&made, &head(1, 0), vec![spelled("F")]),
        3,
        YIELDS,
    );
}

/// A walk yields every node in the directory throughout from its head read: not one that left
/// after it, nor one marked `Discarded` in place; one published after it it may yield too, its
/// record standing from its publish; one published or repositioned before it too; one a `Move`
/// within the directory repositioned after it only where the walk reports it may have repeated.
#[test]
fn a_walk_yields_every_node_in_the_directory_from_its_head_read() {
    let made = snapshot();
    caught(
        &replayed_with(&made, &head(1, 0), vec![walk(&made, (1, 3), &[G], false)]),
        3,
        THROUGHOUT,
    );
    clean(&replayed_with(
        &made,
        &[head(1, 0), detached(F, 2)].concat(),
        vec![walk(&made, (1, 4), &[G], true)],
    ));
    let after = [head(1, 0), published(2)].concat();
    clean(&replayed_with(
        &made,
        &after,
        vec![walk(&made, (1, 6), &[G, F], true)],
    ));
    let mut with_n = walk(&made, (1, 6), &[G, F], true);
    with_n.walks[0].yielded.insert(0, n_in(&after));
    clean(&replayed_with(&made, &after, vec![with_n]));
    clean(&replayed_with(
        &made,
        &[head(1, 0), discarding(&made, 2)].concat(),
        vec![walk(&made, (1, 4), &[G], false)],
    ));
    clean(&replayed_with(
        &made,
        &[discarding(&made, 1), head(2, 0)].concat(),
        vec![walk(&made, (2, 4), &[G], false)],
    ));
    // Repositioned after the head read: passed over where the walk may repeat.
    clean(&replayed_with(
        &made,
        &[head(1, 0), repositioned(2, "f")].concat(),
        vec![walk(&made, (1, 4), &[G], true)],
    ));
    caught(
        &replayed_with(
            &made,
            &[head(1, 0), repositioned(2, "f")].concat(),
            vec![walk(&made, (1, 4), &[G], false)],
        ),
        4,
        THROUGHOUT,
    );
    // Repositioned before it, so at the head the walk began at.
    let before = [repositioned(2, "f"), head(3, REATTACH_CHANGES + 1)].concat();
    clean(&replayed_with(
        &made,
        &before,
        vec![walk(&made, (1, 5), &[F, G], false)],
    ));
    caught(
        &replayed_with(&made, &before, vec![walk(&made, (1, 5), &[G], true)]),
        5,
        THROUGHOUT,
    );
    // Published after the walk's call began and before its head read; with no counted change
    // after, the chain at the head read (C6).
    let read = [published(2), head(5, 1)].concat();
    caught(
        &replayed_with(&made, &read, vec![walk(&made, (1, 7), &[G, F], false)]),
        7,
        THROUGHOUT,
    );
    let mut all = walk(&made, (1, 7), &[G, F], false);
    all.walks[0].yielded.insert(0, n_in(&read));
    clean(&replayed_with(&made, &read, vec![all]));
}

/// A walk is held from its own read of its directory's head: not that of another directory its call
/// walks, nor one its call made before the walk began.
#[test]
fn a_walk_is_held_from_its_own_head_read() {
    let made = snapshot();
    let read = [head_of(B, 1, 0), published(2), head(5, 1)].concat();
    let mut both = walk(&made, (1, 7), &[G, F], false);
    let of_b = Walk {
        dir: B,
        from: 1,
        yielded: vec![Yielded {
            node: H,
            record: Some(made.blocks[0].records[H as usize]),
            name: None,
            spelling: None,
        }],
        repeats: Repeats::No,
        skipped: 0,
        filter: Filter::EVERY,
        over: Over::Chain,
    };
    both.walks.insert(0, of_b);
    caught(&replayed_with(&made, &read, vec![both]), 7, THROUGHOUT);
    let earlier = [head(1, 0), published(2), head(6, 1)].concat();
    let mut later = walk(&made, (1, 8), &[G, F], false);
    later.walks[0].from = 5;
    caught(&replayed_with(&made, &earlier, vec![later]), 8, THROUGHOUT);
}

/// A walk whose call holds no read of its directory's head since the walk began began nowhere the
/// model can hold it from; one that passed live children over for their names left out what the
/// model cannot excuse.
#[test]
fn a_walk_with_no_head_read_or_names_passed_over_is_caught() {
    let made = snapshot();
    const NO_HEAD: &str = "read no head of its directory";
    caught(
        &replayed_with(&made, &[], vec![walk(&made, (1, 3), &[G, F], false)]),
        3,
        NO_HEAD,
    );
    caught(
        &replayed_with(
            &made,
            &head_of(B, 1, 0),
            vec![walk(&made, (1, 3), &[G, F], false)],
        ),
        3,
        NO_HEAD,
    );
    let mut skipping = walk(&made, (1, 3), &[G], false);
    skipping.walks[0].skipped = 1;
    caught(
        &replayed_with(&made, &head(1, 0), vec![skipping]),
        3,
        "passed over 1 live children for a name it could not read",
    );
}

/// A call's walks of a chain make two claims each, within the claims the model watches.
#[test]
fn a_calls_walks_make_two_claims_each() {
    let made = snapshot();
    let walks = |count| {
        let mut call = walk(&made, (1, 3), &[G, F], false);
        call.walks = vec![call.walks[0].clone(); count];
        call
    };
    clean(&replayed_with(&made, &head(1, 0), vec![walks(15)]));
    caught(
        &replayed_with(&made, &head(1, 0), vec![walks(16)]),
        3,
        "made 32 claims past its answer's (0 nodes passed over as taken, 0 lookups, 32 by its walks",
    );
}

/// A listing of `a/` of `kind`'s by task 0 over `[begin, end)`, begun at its begin, under `filter`,
/// yielding each node of `yielded` by its id and the hash of the name given with it, dropping its
/// repeats.
fn listing(
    kind: &'static catalogue::Kind,
    span: (u64, u64),
    yielded: &[(NodeID, Option<&str>)],
    filter: Filter,
) -> Call {
    let yielded = yielded
        .iter()
        .map(|&(node, name)| Yielded {
            node,
            record: None,
            name: name.map(hash_string),
            spelling: None,
        })
        .collect();
    walk_of(
        kind,
        (A, Over::Chain),
        span,
        yielded,
        (Repeats::Dropped, filter),
    )
}

/// A listing by id of `a/` over `[begin, end)` yielding `yielded`.
fn listed(span: (u64, u64), yielded: &[NodeID]) -> Call {
    listing(
        &catalogue::LISTED_CHILDREN,
        span,
        &ids(yielded),
        Filter::EVERY,
    )
}

/// A collection of `a/` of `kind`'s over `[begin, end)` under `filter` yielding `yielded`.
fn collected(
    (kind, filter): (&'static catalogue::Kind, Filter),
    span: (u64, u64),
    yielded: &[NodeID],
) -> Call {
    listing(kind, span, &ids(yielded), filter)
}

/// `nodes`, each with no name read.
fn ids(nodes: &[NodeID]) -> Vec<(NodeID, Option<&'static str>)> {
    nodes.iter().map(|&node| (node, None)).collect()
}

/// A listing, which drops its repeats, yields no node twice, and may pass over a node a `Move`
/// within the directory repositioned after its head read, as a walk reporting it may repeat may.
#[test]
fn a_listing_yields_no_node_twice_and_may_pass_a_repositioned_node_over() {
    let made = snapshot();
    caught(
        &replayed_with(&made, &head(1, 0), vec![listed((1, 3), &[G, F, G])]),
        3,
        "yielded 3 twice, where it drops its repeats",
    );
    clean(&replayed_with(
        &made,
        &head(1, 0),
        vec![listed((1, 3), &[G, F])],
    ));
    clean(&replayed_with(
        &made,
        &[head(1, 0), repositioned(2, "f")].concat(),
        vec![listed((1, 4), &[G])],
    ));
    caught(
        &replayed_with(&made, &head(1, 0), vec![listed((1, 3), &[G])]),
        3,
        THROUGHOUT,
    );
}

/// The hash a listing answers with a node is one the node carried in the directory at some `seq`
/// from its head read: the old name's or the new one's across a rename, never another.
#[test]
fn a_listings_name_hash_is_one_the_node_carried() {
    let made = snapshot();
    let named = |name: &str| {
        let yielded = [(G, Some("g")), (F, Some(name))];
        listing(
            &catalogue::LISTED_NAME_HASHES,
            (1, 4),
            &yielded,
            Filter::EVERY,
        )
    };
    let renamed = [head(1, 0), repositioned(2, "x")].concat();
    clean(&replayed_with(&made, &renamed, vec![named("f")]));
    clean(&replayed_with(&made, &renamed, vec![named("x")]));
    caught(&replayed_with(&made, &renamed, vec![named("y")]), 4, YIELDS);
}

/// A walk whose caller leaves children staged for deletion out yields only a child it keeps at
/// some `seq` from its head read, and every one in the directory and kept throughout: not one
/// staged for deletion then or since, which it may yield where it was kept at its head read.
#[test]
fn a_collection_leaving_staged_deletes_out_keeps_by_its_filter() {
    let made = snapshot();
    let deleted = flagged(&made, NodeFlags::StagedDelete);
    let undeleted = |span, yielded: &[NodeID]| {
        collected(
            (&catalogue::COLLECTED_CHILDREN, Filter::UNDELETED),
            span,
            yielded,
        )
    };
    // Staged for deletion before the head read: never kept.
    let before = [rewritten(1, deleted), head(2, 0)].concat();
    clean(&replayed_with(
        &made,
        &before,
        vec![undeleted((1, 4), &[G])],
    ));
    caught(
        &replayed_with(&made, &before, vec![undeleted((1, 4), &[G, F])]),
        4,
        YIELDS,
    );
    // Staged for deletion after it: kept at the head read, not throughout.
    let after = [head(1, 0), rewritten(2, deleted)].concat();
    for yielded in [&[G][..], &[G, F]] {
        clean(&replayed_with(
            &made,
            &after,
            vec![undeleted((1, 4), yielded)],
        ));
    }
    // Kept throughout.
    caught(
        &replayed_with(&made, &head(1, 0), vec![undeleted((1, 3), &[G])]),
        3,
        THROUGHOUT,
    );
    // Restored after the head read: kept from there, not throughout.
    let mut staged = made.clone();
    staged.blocks[0].records[F as usize] = deleted;
    let restored = [head(1, 0), rewritten(2, made.blocks[0].records[F as usize])].concat();
    for yielded in [&[G][..], &[G, F]] {
        clean(&replayed_with(
            &staged,
            &restored,
            vec![undeleted((1, 4), yielded)],
        ));
    }
    // A walk keeping every child yields one staged for deletion too.
    caught(
        &replayed_with(
            &made,
            &before,
            vec![collected(
                (&catalogue::COLLECTED_CHILDREN, Filter::EVERY),
                (1, 4),
                &[G],
            )],
        ),
        4,
        THROUGHOUT,
    );
}

/// A walk whose caller keeps children by their records, as the diff against a working tree leaves
/// symbolic links out, yields only a child its `keep` kept at some `seq` from its head read, and
/// every one in the directory it kept throughout.
#[test]
fn a_collection_keeping_by_record_keeps_by_its_filter() {
    let made = snapshot();
    let mut linked = made.blocks[0].records[F as usize];
    linked.ext_flags |= NodeExtFlags::Symlink.bits();
    let versioned = |span, yielded: &[NodeID]| {
        let kind = &catalogue::COLLECTED_VERSIONED_CHILDREN;
        collected((kind, Filter::VERSIONED), span, yielded)
    };
    let before = [rewritten(1, linked), head(2, 0)].concat();
    clean(&replayed_with(
        &made,
        &before,
        vec![versioned((1, 4), &[G])],
    ));
    caught(
        &replayed_with(&made, &before, vec![versioned((1, 4), &[G, F])]),
        4,
        YIELDS,
    );
    // Made a symbolic link after the head read: kept there, not throughout.
    let after = [head(1, 0), rewritten(2, linked)].concat();
    for yielded in [&[G][..], &[G, F]] {
        clean(&replayed_with(
            &made,
            &after,
            vec![versioned((1, 4), yielded)],
        ));
    }
    caught(
        &replayed_with(&made, &head(1, 0), vec![versioned((1, 3), &[G])]),
        3,
        THROUGHOUT,
    );
}
