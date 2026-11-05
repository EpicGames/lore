// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The closure exceptions narrowed to what a dropped or failed call left (C12): the (dirty node,
//! clean directory) pairs its own records formed, a future it spawned's included, kept while they
//! stand; the rest of closure checked on the model; and the tasks dropped outside every call, which
//! excuse closure on the whole State.

use std::collections::BTreeSet;

use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::G;
use super::H;
use super::STATE;
use super::base_call;
use super::by;
use super::clean;
use super::dirty;
use super::grab;
use super::guard;
use super::initialized;
use super::moved;
use super::publish;
use super::record;
use super::replayed_with;
use super::section;
use super::snapshot;
use super::unlink;
use super::word;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Spawn;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;
use crate::harness::model::checkpoint;
use crate::harness::scheduler::dropped_outside_calls;

const DIRTY: u16 = NodeFlags::Dirty.bits();

/// The model of `snapshot` with `records` replayed with `calls`; it finds nothing.
fn replayed(snapshot: &Snapshot, records: &[Record], calls: Vec<Call>) -> Model {
    let model = replayed_with(snapshot, records, calls);
    clean(&model);
    model
}

/// A scan mark's call of `node` by task 0 over `[1, 3)`, answering `outcome`.
fn mark(node: NodeID, outcome: Outcome) -> Call {
    of(&catalogue::SCAN_MARK_MODIFIED, node, (1, 3), outcome)
}

/// A call of `kind` on `node`, id 1, by task 0 over `[begin, end)`, answering `outcome`.
fn of(
    kind: &'static catalogue::Kind,
    node: NodeID,
    (begin, end): (u64, u64),
    outcome: Outcome,
) -> Call {
    Call {
        target: Some(node),
        ..base_call(kind, (begin, end), outcome)
    }
}

/// `node`'s words in `made` with `flags` set and `cleared` cleared.
fn flagged(made: &Snapshot, node: NodeID, flags: u16, cleared: u16) -> lore_revision::node::Node {
    let mut record = made.blocks[0].records[node as usize];
    record.flags = (record.flags | flags) & !cleared;
    record
}

/// A mark's first level at `seq`, by `task`: `node` written dirty.
fn dirtied(made: &Snapshot, node: NodeID, (seq, task): (u64, usize)) -> Vec<Record> {
    let words = flagged(made, node, DIRTY, 0);
    by(
        task,
        section(seq, Section::UnservedMark, vec![word(node, words, false)]),
    )
}

fn excused(model: &Model) -> BTreeSet<(NodeID, NodeID)> {
    model
        .state(STATE)
        .expect("the State's model")
        .excused
        .clone()
}

fn breaches(model: &Model) -> Vec<String> {
    model
        .state(STATE)
        .expect("the State's model")
        .closure_breaches()
}

/// A mark dropped between its levels leaves its node dirty under a clean directory: excused, as
/// the call's record formed it; the same breach by a call that answered done, or by a writer no
/// call holds, is not.
#[test]
fn a_dropped_call_excuses_the_breach_it_left() {
    let made = snapshot();
    let dropped = replayed(
        &made,
        &dirtied(&made, F, (2, 0)),
        vec![mark(F, Outcome::Dropped)],
    );
    assert_eq!(excused(&dropped), BTreeSet::from([(F, A)]));
    assert!(breaches(&dropped).is_empty(), "{:#?}", breaches(&dropped));
    let done = replayed(
        &made,
        &dirtied(&made, F, (2, 0)),
        vec![mark(F, Outcome::Done(Answer::None))],
    );
    assert!(excused(&done).is_empty());
    assert_eq!(
        breaches(&done),
        ["dirty 2 under clean 1, which no dropped or failed call left"]
    );
    // A failed call's too; but another node's breach, which it did not write, stays.
    let failed = replayed(
        &made,
        &[dirtied(&made, F, (2, 0)), dirtied(&made, H, (3, 1))].concat(),
        vec![Call {
            failed_reads: 1,
            ..mark(F, Outcome::Failed(crate::harness::faults::INJECTED.into()))
        }],
    );
    assert_eq!(excused(&failed), BTreeSet::from([(F, A)]));
    assert_eq!(
        breaches(&failed),
        ["dirty 5 under clean 4, which no dropped or failed call left"]
    );
    // A scenario holding drops to closure holds a dropped call's breach, not a failed one's.
    let holding = |outcome, failed_reads| {
        let mut model = Model::new();
        model.start(STATE, &made);
        model.holding_drops_to_closure();
        let call = Call {
            failed_reads,
            ..mark(F, outcome)
        };
        model.replay_called(&dirtied(&made, F, (2, 0)), vec![call], Vec::new());
        excused(&model)
    };
    assert!(holding(Outcome::Dropped, 0).is_empty());
    assert_eq!(
        holding(Outcome::Failed(crate::harness::faults::INJECTED.into()), 1),
        BTreeSet::from([(F, A)])
    );
}

/// A future spawned in a dropped mark's steps that dirties its node after the call ended, as a
/// `JoinSet` drops its futures late: the breach it leaves is the call's, excused once the future's
/// record passes.
#[test]
fn a_dropped_calls_straggler_excuses_the_breach_it_left_after_the_call() {
    let made = snapshot();
    let spawns = vec![
        Spawn {
            spawner: None,
            call: None,
        },
        Spawn {
            spawner: Some(0),
            call: Some(1),
        },
    ];
    let mut model = Model::new();
    model.start(STATE, &made);
    model.replay_called(
        &dirtied(&made, F, (4, 1)),
        vec![mark(F, Outcome::Dropped)],
        spawns,
    );
    let found: Vec<String> = model.violations().iter().map(ToString::to_string).collect();
    assert!(found.is_empty(), "the model found: {found:#?}");
    assert_eq!(excused(&model), BTreeSet::from([(F, A)]));
    assert!(breaches(&model).is_empty(), "{:#?}", breaches(&model));
}

/// A clear dropped before its re-mark leaves a clean directory over a dirty child: excused, as the
/// call's record formed it.
#[test]
fn a_dropped_clear_excuses_the_directory_it_cleaned() {
    let made = dirty(dirty(snapshot(), F), A);
    let cleared = section(
        2,
        Section::UnservedClear,
        vec![word(A, flagged(&made, A, 0, DIRTY), false)],
    );
    let clear = of(&catalogue::SCAN_CLEAR, A, (1, 3), Outcome::Dropped);
    let model = replayed(&made, &cleared, vec![clear]);
    assert_eq!(excused(&model), BTreeSet::from([(F, A)]));
    assert!(breaches(&model).is_empty());
}

/// An add dropped between its publish and its parent's mark leaves its node dirty under a clean
/// directory; a move dropped after its move, its node dirty under the clean directory it moved
/// into: each excused, as the call's `Publish` or `Move` formed it. The model excuses a dropped
/// call's `Move` whatever its climb: the C API's staged move, the one cancellable kind that moves,
/// stands for one here, climbing its destination with the staged half alone, as it does for a
/// clean node; the product's climb gives a dirty node's destination the dirty half too, so forms no
/// such breach.
#[test]
fn a_dropped_publish_or_move_excuses_the_breach_it_formed() {
    let made = snapshot();
    let added = record(A, "n", Kind::File);
    let added = lore_revision::node::Node {
        flags: added.flags | DIRTY,
        ..added
    };
    let adding = [
        section(2, Section::Grab, vec![grab(FREE)]),
        section(
            3,
            Section::Grab,
            vec![word(FREE, initialized(added), false)],
        ),
        section(
            4,
            Section::Publish,
            vec![publish(FREE, A, "n", Kind::File), word(FREE, added, false)],
        ),
    ]
    .concat();
    let add = of(&catalogue::SCAN_ADD, A, (1, 5), Outcome::Dropped);
    let model = replayed(&made, &adding, vec![add]);
    assert_eq!(excused(&model), BTreeSet::from([(FREE, A)]));
    assert!(breaches(&model).is_empty(), "{:#?}", breaches(&model));
    // `f` dirty under a dirty `a`, moved into the clean `b`, under the moves permit, after the
    // climbs of both, which leave `b` clean.
    let made = dirty(dirty(snapshot(), F), A);
    let staged = |node: NodeID| flagged(&made, node, NodeFlags::Staged.bits(), 0);
    let moving = [
        section(2, Section::Unlabelled, vec![Effect::Permit]),
        guard(3, GuardEvent::Armed),
        section(4, Section::UnservedMark, vec![word(A, staged(A), false)]),
        section(5, Section::UnservedMark, vec![word(B, staged(B), false)]),
        section(6, Section::MoveNamed, vec![moved(F, (A, B), "f", None)]),
        guard(7, GuardEvent::Finished),
        section(8, Section::Unlabelled, vec![Effect::Unpermit]),
    ]
    .concat();
    let moves = of(&catalogue::STAGED_MOVE, F, (1, 9), Outcome::Dropped);
    let model = replayed(&made, &moving, vec![moves]);
    assert_eq!(excused(&model), BTreeSet::from([(F, B)]));
    assert!(breaches(&model).is_empty(), "{:#?}", breaches(&model));
}

/// A breach another writer forms within a dropped call's interval, about a node the call wrote, is
/// not the call's: a mark dirties `f` under a dirty `a`, then a writer no call holds cleans `a`;
/// nor one standing before the call's record: `f` dirty under a clean `a` as the run starts.
#[test]
fn a_breach_another_writer_forms_is_not_the_dropped_calls() {
    let made = dirty(snapshot(), A);
    let records = [
        dirtied(&made, F, (2, 0)),
        by(
            1,
            section(
                3,
                Section::Unlabelled,
                vec![word(A, flagged(&made, A, 0, DIRTY), false)],
            ),
        ),
    ]
    .concat();
    let calls = vec![of(
        &catalogue::SCAN_MARK_MODIFIED,
        F,
        (1, 5),
        Outcome::Dropped,
    )];
    let model = replayed(&made, &records, calls);
    assert!(excused(&model).is_empty());
    assert_eq!(
        breaches(&model),
        ["dirty 2 under clean 1, which no dropped or failed call left"]
    );
    let made = dirty(snapshot(), F);
    let model = replayed(
        &made,
        &dirtied(&made, F, (2, 0)),
        vec![mark(F, Outcome::Dropped)],
    );
    assert!(
        excused(&model).is_empty(),
        "a breach standing before the call's record excused: {:?}",
        excused(&model)
    );
    assert_eq!(
        breaches(&model),
        ["dirty 2 under clean 1, which no dropped or failed call left"]
    );
}

/// An excused pair lasts while it stands, until its directory turns dirty, its node clean, moved or
/// out of the tree: a breach of the same pair after is a new one.
#[test]
fn an_excused_breach_lasts_while_it_stands() {
    let made = snapshot();
    let left = || dirtied(&made, F, (2, 0));
    let calls = || vec![mark(F, Outcome::Dropped)];
    let edit = |seq, node, flags, cleared| {
        let words = flagged(&dirty(made.clone(), F), node, flags, cleared);
        by(
            1,
            section(seq, Section::Unlabelled, vec![word(node, words, false)]),
        )
    };
    // The directory turned dirty: no longer excused, nor breached.
    let marked = replayed(&made, &[left(), edit(4, A, DIRTY, 0)].concat(), calls());
    assert!(excused(&marked).is_empty());
    // The directory turned dirty then clean again: breached anew.
    let again = replayed(
        &made,
        &[left(), edit(4, A, DIRTY, 0), edit(5, A, 0, DIRTY)].concat(),
        calls(),
    );
    assert_eq!(
        breaches(&again),
        ["dirty 2 under clean 1, which no dropped or failed call left"]
    );
    // The node turned clean, moved out, or taken out of its chain.
    let cleaned = replayed(&made, &[left(), edit(4, F, 0, DIRTY)].concat(), calls());
    assert!(excused(&cleaned).is_empty());
    let moving = by(
        1,
        section(4, Section::MoveNamed, vec![moved(F, (A, B), "f", None)]),
    );
    let moved_out = replayed(&made, &[left(), moving].concat(), calls());
    assert!(excused(&moved_out).is_empty());
    let detaching = by(
        1,
        section(4, Section::Detach, vec![unlink(F, A, How::Always)]),
    );
    let detached = replayed(&made, &[left(), detaching].concat(), calls());
    assert!(excused(&detached).is_empty());
    // The directory taken out of the tree, its subtree with it.
    let above = by(
        1,
        section(4, Section::Detach, vec![unlink(A, ROOT_NODE, How::Always)]),
    );
    let gone = replayed(&made, &[left(), above].concat(), calls());
    assert!(excused(&gone).is_empty());
}

/// Where a dropped call left a breach, the invariants leave closure to the model, which checks it
/// all but the excused; a drop outside every call excuses it on the whole State.
#[test]
fn closure_is_checked_where_no_dropped_call_left_a_breach() {
    let made = snapshot();
    let clean = replayed(&made, &[], Vec::new());
    let state = clean.state(STATE).expect("the State's model");
    assert!(checkpoint::rules(state, false).dirty_closure);
    assert!(!checkpoint::rules(state, true).dirty_closure);
    let dropped = replayed(
        &made,
        &dirtied(&made, G, (2, 0)),
        vec![mark(G, Outcome::Dropped)],
    );
    let state = dropped.state(STATE).expect("the State's model");
    assert!(!checkpoint::rules(state, false).dirty_closure);
    assert_eq!(state.excused, BTreeSet::from([(G, A)]));
    // The model checks the rest, but where closure is excused whole.
    assert!(checkpoint::closure_left_to_model(state, false));
    assert!(!checkpoint::closure_left_to_model(state, true));
    assert!(!checkpoint::closure_left_to_model(
        clean.state(STATE).expect("the State's model"),
        false
    ));
    // The root's children breach nothing.
    let rooted = replayed(
        &made,
        &dirtied(&made, A, (2, 0)),
        vec![mark(A, Outcome::Dropped)],
    );
    assert!(excused(&rooted).is_empty());
}

/// A task dropped outside every call, with an operation the run cancelled, excuses closure on the
/// whole State, as no call's records tell what it left: the operation's own task with no call
/// dropped with it, or a future spawned outside every call. A future spawned in a call is that
/// call's, whatever it answered, a future that future spawned included; and a task dropped with no
/// operation cancelled, by a call's early return, is the dropping call's.
#[test]
fn a_task_dropped_outside_every_call_excuses_closure_on_the_whole_state() {
    let operation = Spawn {
        spawner: None,
        call: None,
    };
    let spawned = |spawner, call| Spawn {
        spawner: Some(spawner),
        call,
    };
    let answering = |outcome| vec![mark(F, outcome)];
    let failed = || Outcome::Failed(crate::harness::faults::INJECTED.into());
    let done = || Outcome::Done(Answer::None);
    // The operation itself: in its call dropped with it alone.
    let spawns = [operation, operation];
    let outside =
        |dropped: &[usize], calls: &[Call]| dropped_outside_calls(dropped, &[0], calls, &spawns);
    assert!(!outside(&[0], &answering(Outcome::Dropped)));
    assert!(outside(&[0], &answering(failed())));
    assert!(outside(&[0], &answering(done())));
    assert!(outside(&[0], &[]));
    let another = Call {
        task: Some(1),
        ..mark(F, Outcome::Dropped)
    };
    assert!(outside(&[0], &[another]));
    // A future spawned in call 1, and one that future spawned: call 1's, whatever it answered.
    let spawns = [operation, spawned(0, Some(1)), spawned(1, None)];
    for task in [1, 2] {
        for outcome in [failed(), Outcome::Dropped, done()] {
            assert!(!dropped_outside_calls(
                &[task],
                &[0],
                &answering(outcome),
                &spawns
            ));
        }
    }
    // A future spawned outside every call: outside, where its operation was cancelled.
    let spawns = [operation, spawned(0, None)];
    assert!(dropped_outside_calls(
        &[1],
        &[0],
        &answering(Outcome::Dropped),
        &spawns
    ));
    assert!(!dropped_outside_calls(&[1], &[], &[], &spawns));
    assert!(!dropped_outside_calls(&[], &[0], &[], &spawns));
}
