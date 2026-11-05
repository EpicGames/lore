// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model's rules that need a record's call, on records and calls made by hand over the State
//! made by hand: the served chain rule by entry point, climb first (H8), the pre-test (H2) per
//! attempt, the moves permit (C10), the served reattach's `added` answer, and each kind's sections.
//! Each as the code keeps it is clean; each broken is caught, and a record no call holds is judged
//! by the rules that need none.

use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use super::A;
use super::B;
use super::F;
use super::G;
use super::H;
use super::INSTANCE;
use super::L;
use super::NEXT;
use super::PINNED;
use super::STATE;
use super::attach;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::count;
use super::guard;
use super::moved;
use super::record;
use super::section;
use super::snapshot;
use super::unlink;
use super::with;
use super::word;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Spawn;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;

const DIRTY: u16 = NodeFlags::Dirty.bits();
const DIRTY_ADD: u16 = NodeFlags::DirtyAdd.bits();
const BASE: u16 = NodeFlags::Staged.bits() | NodeFlags::Dirty.bits();
const STAGED_DELETE: u16 = NodeFlags::StagedDelete.bits();

/// The model of `snapshot` with `records` replayed with `calls` and `spawns`, a `Word` of no
/// labelled section admitted as a test's own edit.
fn judged(snapshot: &Snapshot, records: &[Record], calls: Vec<Call>, spawns: Vec<Spawn>) -> Model {
    let mut model = Model::new();
    model.start(STATE, snapshot);
    model.admit_unlabelled();
    model.replay_called(records, calls, spawns);
    model
}

/// A call of `kind` by task 0 over `[begin, end)`, answering `outcome`, its id `id`.
fn call(
    kind: &'static catalogue::Kind,
    id: u64,
    (begin, end): (u64, u64),
    outcome: Outcome,
) -> Call {
    Call {
        id,
        ..base_call(kind, (begin, end), outcome)
    }
}

fn done(kind: &'static catalogue::Kind, (begin, end): (u64, u64)) -> Call {
    call(kind, 1, (begin, end), Outcome::Done(Answer::None))
}

/// `call` made on `target`.
fn on(target: NodeID, call: Call) -> Call {
    Call {
        target: Some(target),
        ..call
    }
}

/// `node`'s record in [`snapshot`] with `flags` added.
fn marked(node: NodeID, flags: u16) -> Node {
    let mut record = snapshot().blocks[0].records[node as usize];
    record.flags |= flags;
    record
}

/// `made` with the directory `c` below the root in the slot past the others.
fn with_c(mut made: Snapshot) -> Snapshot {
    let block = &mut made.blocks[0];
    let mut c = record(0, "c", Kind::Directory);
    c.sibling = block.records[0].child;
    block.records[0].child = NEXT;
    block.records[NEXT as usize] = c;
    block.names[NEXT as usize] = Some(Name::new(b"c"));
    block.node_count = NEXT + 1;
    made
}

/// [`snapshot`] with the directory `a/c` in the slot past the others.
fn with_nested() -> Snapshot {
    let mut made = snapshot();
    let block = &mut made.blocks[0];
    let mut c = record(A, "c", Kind::Directory);
    c.sibling = block.records[A as usize].child;
    block.records[A as usize].child = NEXT;
    block.records[NEXT as usize] = c;
    block.names[NEXT as usize] = Some(Name::new(b"c"));
    block.node_count = NEXT + 1;
    made
}

fn mark(seq: u64, node: NodeID) -> Vec<Record> {
    section(
        seq,
        Section::UnservedMark,
        vec![word(node, marked(node, BASE), false)],
    )
}

#[test]
fn the_served_chain_rule_admits_by_the_parents_word_through_served_entry_points() {
    let admitting = with(&[(A, DIRTY)], true);
    let created = |parent| attach(1, parent, "n");
    let create = || vec![done(&catalogue::SERVED_CREATE, (1, 4))];
    let model = judged(&admitting, &created(A), create(), Vec::new());
    clean(&model);
    // `b` is not dirty; nor is `a` with its clear pending.
    let refused = "the served chain rule refuses it";
    caught(
        &judged(&admitting, &created(B), create(), Vec::new()),
        3,
        refused,
    );
    let mut pending = admitting.clone();
    pending.blocks[0].clear_pending[A as usize] = true;
    caught(
        &judged(&pending, &created(A), create(), Vec::new()),
        3,
        refused,
    );
    // The root admits; a publish no call holds, or one of an unserved entry point, is not the
    // rule's.
    clean(&judged(&admitting, &created(0), create(), Vec::new()));
    clean(&judged(&admitting, &created(B), Vec::new(), Vec::new()));
    clean(&judged(
        &admitting,
        &created(B),
        vec![done(&catalogue::CHECKED_CREATE, (1, 4))],
        Vec::new(),
    ));
    // A future spawned in the call's own steps is the call's; a record of its window is not.
    let spawned = [
        Spawn::default(),
        Spawn {
            spawner: Some(0),
            call: Some(1),
        },
    ];
    caught(
        &judged(&admitting, &by(1, created(B)), create(), spawned.to_vec()),
        3,
        refused,
    );
    let windowed = Call {
        windows: vec![(3, 4)],
        ..done(&catalogue::SERVED_CREATE, (1, 4))
    };
    clean(&judged(&admitting, &created(B), vec![windowed], Vec::new()));
    // A record of a call after one made in its window ended is the outer call's.
    let outer = Call {
        windows: vec![(1, 3)],
        ..done(&catalogue::SERVED_CREATE, (1, 7))
    };
    let inner = call(
        &catalogue::SERVED_MARK,
        2,
        (2, 3),
        Outcome::Done(Answer::None),
    );
    caught(
        &judged(
            &admitting,
            &attach(4, B, "n"),
            vec![outer, inner],
            Vec::new(),
        ),
        6,
        refused,
    );
    // A Discarded parent, dirty still.
    let gone = [
        section(1, Section::Detach, vec![unlink(A, 0, How::Always)]),
        attach(2, A, "n"),
    ]
    .concat();
    caught(
        &judged(
            &admitting,
            &gone,
            vec![done(&catalogue::SERVED_CREATE, (2, 5))],
            Vec::new(),
        ),
        4,
        "Discarded: the served chain rule",
    );
    // A move needs both parents; an unlink, a rewrite and a mark level their node's.
    let rename = || vec![done(&catalogue::SERVED_RENAME, (1, 2))];
    let reattach = section(1, Section::Reattach, vec![moved(G, (A, B), "g", None)]);
    caught(
        &judged(&admitting, &reattach, rename(), Vec::new()),
        1,
        refused,
    );
    let both = with(&[(A, DIRTY), (B, DIRTY)], true);
    clean(&judged(&both, &reattach, rename(), Vec::new()));
    let detached = section(1, Section::Detach, vec![unlink(H, B, How::Always)]);
    clean(&judged(
        &admitting,
        &section(1, Section::Detach, vec![unlink(G, A, How::Always)]),
        vec![on(G, done(&catalogue::SERVED_DETACH, (1, 2)))],
        Vec::new(),
    ));
    caught(
        &judged(
            &admitting,
            &detached,
            vec![on(H, done(&catalogue::SERVED_DETACH, (1, 2)))],
            Vec::new(),
        ),
        1,
        refused,
    );
    for (kind, section_of) in [
        (&catalogue::SERVED_MARK, Section::ServedMark),
        (&catalogue::SERVED_COPY_UP, Section::Rewrite),
    ] {
        let written = |node| section(1, section_of, vec![word(node, marked(node, DIRTY), false)]);
        caught(
            &judged(
                &admitting,
                &written(H),
                vec![on(H, done(kind, (1, 2)))],
                Vec::new(),
            ),
            1,
            refused,
        );
        clean(&judged(
            &admitting,
            &written(G),
            vec![on(G, done(kind, (1, 2)))],
            Vec::new(),
        ));
    }
}

#[test]
fn a_staged_change_climbs_first_under_its_guard() {
    let plain = snapshot();
    let add = || vec![done(&catalogue::STAGED_ADD, (1, 7))];
    let climbed = [
        guard(1, GuardEvent::Armed),
        mark(2, A),
        attach(3, A, "n"),
        guard(6, GuardEvent::Finished),
    ]
    .concat();
    clean(&judged(&plain, &climbed, add(), Vec::new()));
    let unclimbed = [
        guard(1, GuardEvent::Armed),
        attach(3, A, "n"),
        guard(6, GuardEvent::Finished),
    ]
    .concat();
    let skipped = "skipped its climb at 1";
    caught(&judged(&plain, &unclimbed, add(), Vec::new()), 5, skipped);
    // A parent holding its base marks needs no climb, nor one holding them at some point since
    // the call began; one that lost them before is climbed.
    clean(&judged(
        &with(&[(A, BASE)], false),
        &unclimbed,
        add(),
        Vec::new(),
    ));
    let lost = |seq| {
        by(
            1,
            section(seq, Section::Unlabelled, vec![word(A, marked(A, 0), false)]),
        )
    };
    let mut lost_since = [unclimbed.clone(), lost(2)].concat();
    lost_since.sort_by_key(|record| (record.seq, record.sub));
    clean(&judged(
        &with(&[(A, BASE)], false),
        &lost_since,
        add(),
        Vec::new(),
    ));
    let lost_before = [lost(0), unclimbed.clone()].concat();
    caught(
        &judged(&with(&[(A, BASE)], false), &lost_before, add(), Vec::new()),
        5,
        skipped,
    );
    // Its marks under its guard, and before its change, but a later climb its kind makes.
    let unguarded = [mark(2, A), attach(3, A, "n")].concat();
    caught(
        &judged(&plain, &unguarded, add(), Vec::new()),
        2,
        "with no guard armed",
    );
    let late = [climbed.clone(), mark(7, A)].concat();
    caught(
        &judged(
            &plain,
            &late,
            vec![done(&catalogue::STAGED_ADD, (1, 8))],
            Vec::new(),
        ),
        7,
        "after its change",
    );
    let arm = section(
        6,
        Section::Unlabelled,
        vec![Effect::Arm(Arm::ModifyMovedClimb)],
    );
    let later = [
        guard(1, GuardEvent::Armed),
        mark(2, A),
        attach(3, A, "n"),
        arm,
        mark(7, A),
        guard(8, GuardEvent::Finished),
    ]
    .concat();
    clean(&judged(
        &plain,
        &later,
        vec![done(&catalogue::STAGED_ADD, (1, 9))],
        Vec::new(),
    ));
    // A modify's change is its `Word`, a staged move's its move, below both parents.
    let modified = |climb: bool| {
        let mut records = guard(1, GuardEvent::Armed);
        if climb {
            records.extend(mark(2, A));
        }
        records.extend(section(
            3,
            Section::StagedModify,
            vec![word(F, marked(F, NodeFlags::DirtyModify.bits()), false)],
        ));
        records.extend(guard(4, GuardEvent::Finished));
        records
    };
    let modify = || vec![done(&catalogue::STAGED_MODIFY, (1, 5))];
    clean(&judged(&plain, &modified(true), modify(), Vec::new()));
    caught(
        &judged(&plain, &modified(false), modify(), Vec::new()),
        3,
        skipped,
    );
    // A node another task moved since the call began read where the call climbed, or where it
    // went: `g` moved from `a` to `c` between a staged move's marks and its move.
    let raced = [
        section(1, Section::Unlabelled, vec![Effect::Permit]),
        guard(2, GuardEvent::Armed),
        mark(3, A),
        by(
            1,
            section(4, Section::Reattach, vec![moved(G, (A, NEXT), "g", None)]),
        ),
        mark(5, B),
        section(6, Section::MoveNamed, vec![moved(G, (NEXT, B), "g", None)]),
        guard(7, GuardEvent::Finished),
        section(8, Section::Unlabelled, vec![Effect::Unpermit]),
    ]
    .concat();
    clean(&judged(
        &with_c(plain.clone()),
        &raced,
        vec![done(&catalogue::STAGED_MOVE, (1, 9))],
        Vec::new(),
    ));
    // After its move, a staged move beaten by a reattach marks the directory the node left, and
    // nothing else of the chains it climbed.
    let beaten = |remarked: NodeID| {
        let mut records = raced[..raced.len() - 2].to_vec();
        records.extend(section(
            7,
            Section::Unlabelled,
            vec![Effect::Arm(Arm::MoverBeaten)],
        ));
        let mut word_of = with_c(snapshot()).blocks[0].records[remarked as usize];
        word_of.flags |= BASE;
        records.extend(section(
            8,
            Section::UnservedMark,
            vec![word(remarked, word_of, false)],
        ));
        records.extend(guard(9, GuardEvent::Finished));
        records.extend(section(10, Section::Unlabelled, vec![Effect::Unpermit]));
        records
    };
    let beaten_call = || vec![done(&catalogue::STAGED_MOVE, (1, 11))];
    clean(&judged(
        &with_c(plain.clone()),
        &beaten(NEXT),
        beaten_call(),
        Vec::new(),
    ));
    caught(
        &judged(
            &with_c(plain.clone()),
            &beaten(A),
            beaten_call(),
            Vec::new(),
        ),
        8,
        "after its change",
    );
    let staged_move = |destination: bool| {
        let mut records = section(1, Section::Unlabelled, vec![Effect::Permit]);
        records.extend(guard(2, GuardEvent::Armed));
        records.extend(mark(3, A));
        if destination {
            records.extend(mark(4, B));
        }
        records.extend(section(
            5,
            Section::MoveNamed,
            vec![moved(G, (A, B), "g", None)],
        ));
        records.extend(guard(6, GuardEvent::Finished));
        records.extend(section(7, Section::Unlabelled, vec![Effect::Unpermit]));
        records
    };
    let move_call = || vec![done(&catalogue::STAGED_MOVE, (1, 8))];
    clean(&judged(&plain, &staged_move(true), move_call(), Vec::new()));
    caught(
        &judged(&plain, &staged_move(false), move_call(), Vec::new()),
        5,
        "skipped its climb at 4",
    );
}

#[test]
fn a_delete_level_climbs_before_its_mark() {
    let plain = snapshot();
    let level = |climb: bool| {
        let mut records = section(1, Section::Unlabelled, vec![Effect::Permit]);
        records.extend(guard(2, GuardEvent::Armed));
        if climb {
            records.extend(mark(3, A));
        }
        records.extend(section(
            4,
            Section::DeleteLevel,
            vec![
                Effect::Admitted {
                    node: F,
                    expected: Expected::Deletable(A),
                },
                word(F, marked(F, STAGED_DELETE | DIRTY), false),
            ],
        ));
        records.extend(guard(5, GuardEvent::Finished));
        records.extend(section(6, Section::Unlabelled, vec![Effect::Unpermit]));
        records
    };
    let delete = || vec![done(&catalogue::STAGED_DELETE, (1, 7))];
    clean(&judged(&plain, &level(true), delete(), Vec::new()));
    caught(
        &judged(&plain, &level(false), delete(), Vec::new()),
        4,
        "skipped its climb at 1",
    );
    // The level decides on its parent's word under its locks: marks held since the call began and
    // lost before the level are not what it read.
    let lost = by(
        1,
        section(3, Section::Unlabelled, vec![word(A, marked(A, 0), false)]),
    );
    let mut records = level(false);
    records.extend(lost);
    records.sort_by_key(|record| (record.seq, record.sub));
    caught(
        &judged(&with(&[(A, BASE)], false), &records, delete(), Vec::new()),
        4,
        "skipped its climb at 1",
    );
    clean(&judged(
        &with(&[(A, BASE)], false),
        &level(false),
        delete(),
        Vec::new(),
    ));
}

/// A pre-test of `node` at `seq`, refused or not, on `expected`.
fn pre_test(seq: u64, node: NodeID, refused: bool, expected: Expected) -> Vec<Record> {
    section(
        seq,
        Section::Unlabelled,
        vec![Effect::PreTest {
            node,
            attempt: 0,
            refused,
            expected,
        }],
    )
}

#[test]
fn a_conditional_detach_pre_tests_before_it_marks_and_stops_where_refused() {
    let served = with(&[], true);
    let mark_a = || {
        section(
            2,
            Section::ServedMark,
            vec![word(A, marked(A, DIRTY), false)],
        )
    };
    let rmdir = || vec![on(F, done(&catalogue::SERVED_RMDIR, (1, 4)))];
    let detached = |seq| {
        section(
            seq,
            Section::Detach,
            vec![Effect::Detach {
                node: F,
                parent: A,
                how: How::IfUnchanged,
                expected: count(0),
                instance: INSTANCE,
            }],
        )
    };
    let tested = [pre_test(1, F, false, count(0)), mark_a(), detached(3)].concat();
    clean(&judged(&served, &tested, rmdir(), Vec::new()));
    // Its parent marked before the pre-test.
    let early = [
        section(
            0,
            Section::ServedMark,
            vec![word(A, marked(A, DIRTY), false)],
        ),
        pre_test(1, F, false, count(0)),
        detached(3),
    ]
    .concat();
    caught(
        &judged(
            &served,
            &early,
            vec![done(&catalogue::SERVED_RMDIR, (0, 4))],
            Vec::new(),
        ),
        1,
        "before its pre-test",
    );
    // Its parent marked after a pre-test that refused.
    let refused = [pre_test(1, F, true, count(5)), mark_a()].concat();
    caught(
        &judged(&served, &refused, rmdir(), Vec::new()),
        2,
        "after its pre-test refused",
    );
    // Per attempt: an attempt's own mark after its pre-test stands; a mark after the detach went
    // round again, before the next attempt's pre-test, does not.
    let attempts = |marked_at: u64| {
        let mut records = pre_test(1, F, false, count(0));
        let round = section(
            3,
            Section::Unlabelled,
            vec![Effect::Arm(Arm::DetachChanged)],
        );
        let mark = section(
            marked_at,
            Section::ServedMark,
            vec![word(A, marked(A, DIRTY), false)],
        );
        records.extend(if marked_at < 3 {
            [mark, round].concat()
        } else {
            [round, mark].concat()
        });
        records.extend(pre_test(5, F, false, count(0)));
        records.extend(detached(6));
        records
    };
    let twice = || vec![on(F, done(&catalogue::SERVED_RMDIR, (1, 7)))];
    // Words on another node than the parent, before a pre-test or after a refused one.
    let elsewhere = |seq| {
        section(
            seq,
            Section::ServedMark,
            vec![word(B, marked(B, DIRTY), false)],
        )
    };
    // Dropped after its refused pre-test, it detached nothing.
    let others = [elsewhere(0), pre_test(1, F, true, count(5)), elsewhere(2)].concat();
    clean(&judged(
        &served,
        &others,
        vec![on(
            F,
            call(&catalogue::SERVED_RMDIR, 1, (0, 3), Outcome::Dropped),
        )],
        Vec::new(),
    ));
    clean(&judged(&served, &attempts(2), twice(), Vec::new()));
    caught(
        &judged(&served, &attempts(4), twice(), Vec::new()),
        5,
        "before its pre-test",
    );
}

#[test]
fn a_pre_tests_answer_is_its_condition_on_the_model() {
    let served = with(&[(F, DIRTY)], true);
    let answered = "answered refused";
    let cases = [
        // `IfUnchanged`: the count and the block's instance.
        (F, false, count(0), None),
        (F, false, count(5), Some(answered)),
        (F, true, count(5), None),
        (
            F,
            false,
            Expected::Count {
                changes: 0,
                instance: INSTANCE + 1,
            },
            Some(answered),
        ),
        // `IfClean`, its condition carrying nothing: `f` is dirty.
        (F, true, Expected::Nothing, None),
        (F, false, Expected::Nothing, Some(answered)),
        // `At`: the parent and the name's hash.
        (
            G,
            false,
            Expected::At {
                parent: A,
                name_hash: lore_storage::hash::hash_string("g"),
            },
            None,
        ),
        (
            G,
            false,
            Expected::At {
                parent: B,
                name_hash: lore_storage::hash::hash_string("g"),
            },
            Some(answered),
        ),
        // `IfEmpty`: the head read, the model's.
        (B, true, Expected::Head(H), None),
        (B, false, Expected::Head(H), Some(answered)),
        (B, true, Expected::Head(G), Some("head")),
        // A link's `child` is its pin, which no chain holds.
        (L, false, Expected::Head(PINNED), None),
    ];
    for (node, refused, expected, what) in cases {
        // Judged with no call: the answer needs none.
        let model = judged(
            &served,
            &pre_test(1, node, refused, expected),
            Vec::new(),
            Vec::new(),
        );
        match what {
            None => clean(&model),
            Some(what) => caught(&model, 1, what),
        }
    }
    let gone = [
        section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
        pre_test(2, F, false, count(1)),
    ]
    .concat();
    caught(
        &judged(&served, &gone, Vec::new(), Vec::new()),
        2,
        "a pre-test of Discarded",
    );
}

#[test]
fn the_moves_permit_has_one_holder_and_covers_its_holders_records() {
    let plain = snapshot();
    let permit = |seq| section(seq, Section::Unlabelled, vec![Effect::Permit]);
    let unpermit = |seq| section(seq, Section::Unlabelled, vec![Effect::Unpermit]);
    let delete = || vec![done(&catalogue::STAGED_DELETE, (1, 4))];
    let level = |seq| {
        section(
            seq,
            Section::DeleteLevel,
            vec![
                Effect::Admitted {
                    node: F,
                    expected: Expected::Deletable(A),
                },
                word(F, marked(F, STAGED_DELETE | DIRTY), false),
            ],
        )
    };
    let base = with(&[(A, BASE)], false);
    clean(&judged(
        &base,
        &[permit(1), level(2), unpermit(3)].concat(),
        delete(),
        Vec::new(),
    ));
    caught(
        &judged(
            &base,
            &[level(1), permit(2), unpermit(3)].concat(),
            delete(),
            Vec::new(),
        ),
        1,
        "outside its own moves permit",
    );
    // Two holders at once; a release with none taken.
    caught(
        &judged(
            &plain,
            &[permit(1), by(1, permit(2))].concat(),
            Vec::new(),
            Vec::new(),
        ),
        2,
        "taken while",
    );
    caught(
        &judged(&plain, &unpermit(1), Vec::new(), Vec::new()),
        1,
        "released by",
    );
    // A directory's reattach across parents: inside a permit, its call's own where one holds it.
    let reattach = by(
        1,
        section(2, Section::Reattach, vec![moved(A, (0, B), "a", None)]),
    );
    let outside = "outside its moves permit";
    caught(
        &judged(&plain, &reattach, Vec::new(), Vec::new()),
        2,
        outside,
    );
    let taken = [by(1, permit(1)), reattach.clone(), by(1, unpermit(3))].concat();
    clean(&judged(&plain, &taken, Vec::new(), Vec::new()));
    let others = [permit(1), reattach.clone(), unpermit(3)].concat();
    let rename = Call {
        task: Some(1),
        ..call(
            &catalogue::SERVED_RENAME,
            2,
            (2, 3),
            Outcome::Done(Answer::Moved { added: false }),
        )
    };
    caught(
        &judged(
            &with(&[(B, DIRTY)], true),
            &others,
            vec![done(&catalogue::STAGED_MOVE, (1, 4)), rename],
            Vec::new(),
        ),
        2,
        outside,
    );
    // A served directory move holding its own permit; a permit released by another call than took
    // it.
    // Moved clean, it takes `Dirty` (H20).
    let mut dirtied = marked(A, DIRTY);
    dirtied.parent = B;
    let moved_dirty = section(
        2,
        Section::Reattach,
        vec![moved(A, (0, B), "a", None), word(A, dirtied, false)],
    );
    let own = by(1, [permit(1), moved_dirty, unpermit(3)].concat());
    let renaming = Call {
        task: Some(1),
        ..call(
            &catalogue::SERVED_RENAME,
            2,
            (1, 4),
            Outcome::Done(Answer::Moved { added: false }),
        )
    };
    clean(&judged(
        &with(&[(B, DIRTY)], true),
        &own,
        vec![renaming],
        Vec::new(),
    ));
    let pend = Call {
        task: Some(1),
        ..call(&catalogue::PEND, 2, (2, 3), Outcome::Done(Answer::None))
    };
    caught(
        &judged(
            &plain,
            &[permit(1), by(1, unpermit(2))].concat(),
            vec![done(&catalogue::STAGED_MOVE, (1, 2)), pend],
            Vec::new(),
        ),
        2,
        "released by",
    );
    // A file's needs none.
    clean(&judged(
        &plain,
        &section(1, Section::Reattach, vec![moved(G, (A, B), "g", None)]),
        Vec::new(),
        Vec::new(),
    ));
}

#[test]
fn a_reattachs_added_answer_is_its_movers_dirty_add() {
    let served = with(&[(A, DIRTY), (B, DIRTY), (G, DIRTY_ADD)], true);
    let reattach = section(1, Section::Reattach, vec![moved(G, (A, B), "g", None)]);
    let answering = |added| {
        vec![call(
            &catalogue::SERVED_RENAME,
            1,
            (1, 2),
            Outcome::Done(Answer::Moved { added }),
        )]
    };
    clean(&judged(&served, &reattach, answering(true), Vec::new()));
    caught(
        &judged(&served, &reattach, answering(false), Vec::new()),
        1,
        "answered added false",
    );
}

#[test]
fn a_staged_changes_climb_carries_staged_up_to_a_staged_ancestor() {
    let nested = with_nested();
    let mark_in = |made: &Snapshot, seq: u64, node: NodeID| {
        let mut word_of = made.blocks[0].records[node as usize];
        word_of.flags |= BASE;
        section(seq, Section::UnservedMark, vec![word(node, word_of, false)])
    };
    let climbed = |up: bool| {
        let mut records = guard(1, GuardEvent::Armed);
        records.extend(mark_in(&nested, 2, NEXT));
        if up {
            records.extend(mark_in(&nested, 3, A));
        }
        records.extend(attach(4, NEXT, "n"));
        records.extend(guard(7, GuardEvent::Finished));
        records
    };
    let add = || vec![done(&catalogue::STAGED_ADD, (1, 9))];
    clean(&judged(&nested, &climbed(true), add(), Vec::new()));
    caught(
        &judged(&nested, &climbed(false), add(), Vec::new()),
        6,
        "stopped at 8, below 1 lacking Staged",
    );
    // An ancestor holding `Staged` stops the staged half; a later mark of a node outside the
    // chains it climbed is not its.
    let mut staged = nested.clone();
    staged.blocks[0].records[A as usize].flags |= NodeFlags::Staged.bits();
    clean(&judged(&staged, &climbed(false), add(), Vec::new()));
    let elsewhere = [climbed(true), mark_in(&nested, 8, B)].concat();
    clean(&judged(&nested, &elsewhere, add(), Vec::new()));
}

#[test]
fn a_calls_labelled_records_come_in_its_kinds_sections() {
    let admitting = with(&[(A, DIRTY)], true);
    let mark = |section_of, task| {
        judged(
            &admitting,
            &by(
                task,
                section(1, section_of, vec![word(G, marked(G, DIRTY), false)]),
            ),
            vec![on(G, done(&catalogue::SERVED_MARK, (1, 2)))],
            Vec::new(),
        )
    };
    clean(&mark(Section::ServedMark, 0));
    caught(
        &mark(Section::Rewrite, 0),
        1,
        "a record in Rewrite in a call of 'served mark', whose kind writes in [ServedMark]",
    );
    // An unlabelled record is any call's; another task's record is not the call's.
    clean(&mark(Section::Unlabelled, 0));
    clean(&mark(Section::Rewrite, 1));
}
