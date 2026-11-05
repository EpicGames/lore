// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model's rules on a call's outcome, on records and calls made by hand over the State made by
//! hand: an error only where a read failed in its call, the self-reported outcomes only where a run
//! admits them, each counted, a `NeedLoad` only where its call met a block out of memory, each
//! call's guards closed within it, a refusal changing nothing and its condition holding at some
//! `seq` of its interval, each node a call passed over as taken gone at some `seq`, and each answer
//! held to the call's own records and to its State at some `seq` of its interval. Each as the code
//! keeps it is clean; each broken is caught at the call's end.

use std::collections::BTreeMap;

use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::Content;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::G;
use super::H;
use super::INSTANCE;
use super::L;
use super::PINNED;
use super::STATE;
use super::attach;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::dirty;
use super::free;
use super::grab;
use super::guard;
use super::moved;
use super::renamed;
use super::section;
use super::served;
use super::snapshot;
use super::unlink;
use super::word;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Leaves;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Refusal;
use crate::harness::catalogue::call::SelfReported;
use crate::harness::catalogue::call::spelling;
use crate::harness::faults::INJECTED;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;

const DIRTY: u16 = NodeFlags::Dirty.bits();

/// The self-reported outcomes a run admits, and whether it drops calls of kinds their production
/// callers never drop.
#[derive(Clone, Copy)]
struct Admits {
    self_reports: &'static [SelfReported],
    uncancellable_drops: bool,
}

const PLAIN: Admits = Admits {
    self_reports: &[],
    uncancellable_drops: false,
};

/// The model of `snapshot` with `records` replayed with `calls`, in a run admitting what `admits`
/// says; a `Word` of no labelled section admitted as a test's own edit.
fn judged(snapshot: &Snapshot, records: &[Record], calls: Vec<Call>, admits: Admits) -> Model {
    let mut model = Model::new();
    model.start(STATE, snapshot);
    model.admit_unlabelled();
    model.admitting_self_reports(admits.self_reports);
    if admits.uncancellable_drops {
        model.admitting_uncancellable_drops();
    }
    model.replay_called(records, calls, Vec::new());
    model
}

/// A call of `kind` on `target` by task 0 over `[begin, end)`, answering `outcome`.
fn call(
    kind: &'static catalogue::Kind,
    (begin, end): (u64, u64),
    target: Option<NodeID>,
    outcome: Outcome,
) -> Call {
    Call {
        id: begin,
        target,
        ..base_call(kind, (begin, end), outcome)
    }
}

/// `call` with a read failing during its interval.
fn failing(call: Call) -> Call {
    Call {
        failed_reads: 1,
        ..call
    }
}

/// `node`'s record in `made` with `flags` added.
fn marked(made: &Snapshot, node: NodeID, flags: u16) -> Node {
    let mut record = made.blocks[0].records[node as usize];
    record.flags |= flags;
    record
}

#[test]
fn a_call_fails_only_where_a_read_failed_with_the_error_it_gives() {
    let failed = |error: String| {
        call(
            &catalogue::CHECKED_CREATE,
            (1, 2),
            Some(A),
            Outcome::Failed(error),
        )
    };
    let injected = || failed(format!("a block load: {INJECTED}"));
    caught(
        &judged(&snapshot(), &[], vec![injected()], PLAIN),
        2,
        "failed with no read failing in its interval: a block load: a read the test fails",
    );
    clean(&judged(&snapshot(), &[], vec![failing(injected())], PLAIN));
    // A block whose fetch the failed read failed.
    let fetch = failing(failed("Address not found: 142a-0".into()));
    clean(&judged(&snapshot(), &[], vec![fetch], PLAIN));
    caught(
        &judged(
            &snapshot(),
            &[],
            vec![failing(failed("hierarchy broken".into()))],
            PLAIN,
        ),
        2,
        "failed with an error no failed read gives: hierarchy broken",
    );
}

#[test]
fn self_reported_outcomes_are_counted_by_what_they_reported() {
    let reported = |begin, what| {
        call(
            &catalogue::SERVED_CREATE,
            (begin, begin + 1),
            Some(A),
            Outcome::SelfReported(what),
        )
    };
    let admitting = Admits {
        self_reports: &[SelfReported::SlowDown, SelfReported::RetriesSpent],
        ..PLAIN
    };
    let calls = || {
        vec![
            reported(1, SelfReported::SlowDown),
            reported(2, SelfReported::RetriesSpent),
            reported(3, SelfReported::SlowDown),
        ]
    };
    let model = judged(&served(), &[], calls(), admitting);
    clean(&model);
    assert_eq!(
        model.self_reported(),
        &BTreeMap::from([(SelfReported::SlowDown, 2), (SelfReported::RetriesSpent, 1)]),
        "the counts by what each reported"
    );
    // A run admitting none, or not that one.
    caught(
        &judged(&served(), &[], calls(), PLAIN),
        2,
        "a call of 'served create' reported SlowDown in a run admitting no such outcome",
    );
    caught(
        &judged(
            &served(),
            &[],
            vec![reported(1, SelfReported::NameTableFull)],
            admitting,
        ),
        2,
        "reported NameTableFull in a run admitting no such outcome",
    );
}

/// A resident find's `NeedLoad` holds only in a call that met a block out of memory, by its own
/// record of it, counted: not with none, nor with another task's, nor with its own task's after
/// the call, though the run admits the outcome; nor with its own in a run admitting none.
#[test]
fn a_need_load_holds_only_where_its_call_met_a_block_out_of_memory() {
    let admitting = Admits {
        self_reports: &[SelfReported::NeedLoad],
        ..PLAIN
    };
    let need_load = || {
        vec![call(
            &catalogue::FIND_RESIDENT,
            (1, 3),
            Some(A),
            Outcome::SelfReported(SelfReported::NeedLoad),
        )]
    };
    let met = |seq| section(seq, Section::Unlabelled, vec![Effect::Arm(Arm::NeedLoad)]);
    let model = judged(&served(), &met(2), need_load(), admitting);
    clean(&model);
    assert_eq!(
        model.self_reported(),
        &BTreeMap::from([(SelfReported::NeedLoad, 1)]),
        "counted"
    );
    const MET_NONE: &str = "a call of 'resident find' self-reported NeedLoad, meeting no block \
                            out of memory";
    caught(&judged(&served(), &[], need_load(), admitting), 3, MET_NONE);
    caught(
        &judged(&served(), &by(1, met(2)), need_load(), admitting),
        3,
        MET_NONE,
    );
    caught(
        &judged(&served(), &met(4), need_load(), admitting),
        3,
        MET_NONE,
    );
    caught(
        &judged(&served(), &met(2), need_load(), PLAIN),
        3,
        "reported NeedLoad in a run admitting no such outcome",
    );
}

#[test]
fn a_refusal_changes_nothing_of_its_own() {
    // Named as `f`, so a refusal of the name held holds its condition.
    let create = |outcome| {
        vec![Call {
            name: Some(hash_string("f")),
            ..call(&catalogue::CHECKED_CREATE, (1, 5), Some(A), outcome)
        }]
    };
    let refused = || create(Outcome::Refused(Refusal::Exists));
    clean(&judged(&snapshot(), &[], refused(), PLAIN));
    caught(
        &judged(&snapshot(), &attach(1, A, "n"), refused(), PLAIN),
        5,
        "a call of 'checked create' answered Refused(Exists), yet published 1, unlinked 0, \
         moved 0 and renamed 0",
    );
    // Another task's publish is not the call's.
    clean(&judged(
        &snapshot(),
        &by(1, attach(1, A, "n")),
        refused(),
        PLAIN,
    ));
    // An unlink, a move, and a self-reported answer's publish.
    caught(
        &judged(
            &snapshot(),
            &section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
            vec![call(
                &catalogue::UNSERVED_DETACH,
                (1, 2),
                Some(F),
                Outcome::Refused(Refusal::NotFound),
            )],
            PLAIN,
        ),
        2,
        "answered Refused(NotFound), yet published 0, unlinked 1, moved 0 and renamed 0",
    );
    caught(
        &judged(
            &dirty(dirty(served(), A), B),
            &section(1, Section::Reattach, vec![moved(G, (A, B), "g", None)]),
            vec![call(
                &catalogue::SERVED_RENAME,
                (1, 2),
                Some(G),
                Outcome::Refused(Refusal::Exists),
            )],
            PLAIN,
        ),
        2,
        "answered Refused(Exists), yet published 0, unlinked 0, moved 1 and renamed 0",
    );
    caught(
        &judged(
            &snapshot(),
            &attach(1, A, "n"),
            create(Outcome::SelfReported(SelfReported::SlowDown)),
            Admits {
                self_reports: &[SelfReported::SlowDown],
                ..PLAIN
            },
        ),
        5,
        "answered SelfReported(SlowDown), yet published 1, unlinked 0, moved 0 and renamed 0",
    );
    caught(
        &judged(
            &snapshot(),
            &renamed(1, G, A, "x"),
            vec![call(
                &catalogue::RENAME,
                (1, 2),
                Some(G),
                Outcome::Refused(Refusal::NotFound),
            )],
            PLAIN,
        ),
        2,
        "moved 0 and renamed 1",
    );
}

#[test]
fn a_calls_guards_close_within_it() {
    let create = |outcome| {
        vec![Call {
            name: Some(hash_string("f")),
            ..call(&catalogue::CHECKED_CREATE, (1, 4), Some(A), outcome)
        }]
    };
    let done = || create(Outcome::Done(Answer::None));
    let closed = |event| [guard(1, GuardEvent::Armed), guard(2, event)].concat();
    clean(&judged(
        &snapshot(),
        &closed(GuardEvent::Finished),
        done(),
        PLAIN,
    ));
    caught(
        &judged(&snapshot(), &guard(1, GuardEvent::Armed), done(), PLAIN),
        4,
        "a call of 'checked create' ended with 1 guard(s) armed",
    );
    // Dropped armed only where it did not answer done.
    caught(
        &judged(
            &snapshot(),
            &closed(GuardEvent::DroppedArmed),
            done(),
            PLAIN,
        ),
        2,
        "a guard dropped armed in a call of 'checked create' that answered done",
    );
    let failed = || vec![failing(create(Outcome::Failed(INJECTED.into())).remove(0))];
    for calls in [
        create(Outcome::Dropped),
        create(Outcome::Refused(Refusal::Exists)),
        failed(),
    ] {
        clean(&judged(
            &snapshot(),
            &closed(GuardEvent::DroppedArmed),
            calls,
            PLAIN,
        ));
    }
    // A guard its task armed before the call, finished in it: the task's guards balance, the
    // call's do not.
    let before = [guard(0, GuardEvent::Armed), guard(2, GuardEvent::Finished)].concat();
    caught(
        &judged(&snapshot(), &before, done(), PLAIN),
        2,
        "a guard Finished with none armed in its call of 'checked create'",
    );
}

#[test]
fn an_attach_answers_the_node_it_published_or_the_live_holder_it_found() {
    let named = |name: &str, answer| {
        vec![Call {
            name: Some(hash_string(name)),
            ..call(
                &catalogue::CHECKED_CREATE,
                (1, 5),
                Some(A),
                Outcome::Done(answer),
            )
        }]
    };
    let create = |answer| match answer {
        Answer::Held(F) => named("f", answer),
        _ => named("n", answer),
    };
    let attached = attach(1, A, "n");
    clean(&judged(
        &snapshot(),
        &attached,
        create(Answer::Node(FREE)),
        PLAIN,
    ));
    caught(
        &judged(&snapshot(), &attached, create(Answer::Node(F)), PLAIN),
        5,
        "answered node 2, which it did not publish",
    );
    // Another task's publish is not the call's.
    caught(
        &judged(
            &snapshot(),
            &by(1, attached.clone()),
            create(Answer::Node(FREE)),
            PLAIN,
        ),
        5,
        "answered node 7, which it did not publish",
    );
    // A name held: no publish of its own, its holder live at some `seq` of the call; as it began,
    // detached by another task after, or published by another task while it ran.
    clean(&judged(&snapshot(), &[], create(Answer::Held(F)), PLAIN));
    caught(
        &judged(&snapshot(), &attached, create(Answer::Held(F)), PLAIN),
        5,
        "answered the name held by 2, publishing 1",
    );
    let detached = |seq| {
        by(
            1,
            section(seq, Section::Detach, vec![unlink(F, A, How::Always)]),
        )
    };
    for seq in [1, 2] {
        clean(&judged(
            &snapshot(),
            &detached(seq),
            create(Answer::Held(F)),
            PLAIN,
        ));
    }
    // Moved to another directory by another task after it began; `Discarded` where it stands.
    let moved_away = by(
        1,
        section(2, Section::Reattach, vec![moved(F, (A, B), "f", None)]),
    );
    clean(&judged(
        &snapshot(),
        &moved_away,
        create(Answer::Held(F)),
        PLAIN,
    ));
    let discarded = by(
        1,
        section(
            0,
            Section::Unlabelled,
            vec![word(
                F,
                marked(&snapshot(), F, NodeFlags::Discarded.bits()),
                false,
            )],
        ),
    );
    caught(
        &judged(&snapshot(), &discarded, create(Answer::Held(F)), PLAIN),
        5,
        "answered the name held by 2, a live child of Some(1) named",
    );
    caught(
        &judged(&snapshot(), &detached(0), create(Answer::Held(F)), PLAIN),
        5,
        "answered the name held by 2, a live child of Some(1) named",
    );
    // A live node, but not the directory's; the directory's, but not of the name.
    caught(
        &judged(&snapshot(), &[], named("h", Answer::Held(H)), PLAIN),
        5,
        "answered the name held by 5, a live child of Some(1) named",
    );
    caught(
        &judged(&snapshot(), &[], named("f", Answer::Held(G)), PLAIN),
        5,
        "answered the name held by 3, a live child of Some(1) named",
    );
    // Renamed away by another task after it began.
    let rename = Effect::Rename {
        node: F,
        parent: A,
        name_hash: hash_string("x"),
        name: Name::new(b"x"),
        instance: INSTANCE,
    };
    let renamed = by(1, section(2, Section::RenameInPlace, vec![rename]));
    clean(&judged(
        &snapshot(),
        &renamed,
        create(Answer::Held(F)),
        PLAIN,
    ));
    clean(&judged(
        &snapshot(),
        &by(1, attach(2, A, "n")),
        create(Answer::Held(FREE)),
        PLAIN,
    ));
    // A holder published only after the call ended.
    caught(
        &judged(
            &snapshot(),
            &by(1, attach(4, A, "n")),
            create(Answer::Held(FREE)),
            PLAIN,
        ),
        5,
        "answered the name held by 7, a live child of Some(1) named",
    );
}

#[test]
fn a_reattach_answers_its_one_move() {
    let both = dirty(dirty(served(), A), B);
    let rename = |records: &[Record]| {
        judged(
            &both,
            records,
            vec![call(
                &catalogue::SERVED_RENAME,
                (1, 3),
                Some(G),
                Outcome::Done(Answer::Moved { added: false }),
            )],
            PLAIN,
        )
    };
    let reattach = |seq, from, to| {
        section(
            seq,
            Section::Reattach,
            vec![moved(G, (from, to), "g", None)],
        )
    };
    clean(&rename(&reattach(1, A, B)));
    caught(&rename(&[]), 3, "answered moved, with 0 moves");
    caught(
        &rename(&[reattach(1, A, B), reattach(2, B, A)].concat()),
        3,
        "answered moved, with 2 moves",
    );
}

#[test]
fn a_detach_answers_done_unlinking_its_target() {
    let detach = |target| {
        vec![call(
            &catalogue::UNSERVED_DETACH,
            (1, 2),
            Some(target),
            Outcome::Done(Answer::None),
        )]
    };
    // Another task's unlink of its target is not the call's.
    caught(
        &judged(
            &snapshot(),
            &by(
                1,
                section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
            ),
            detach(F),
            PLAIN,
        ),
        2,
        "answered detached, its target Some(2) not among its 0 unlinks",
    );
    // The C API delete's pend answers done by its unlink too.
    caught(
        &judged(
            &snapshot(),
            &[],
            vec![call(
                &catalogue::PEND,
                (1, 2),
                Some(F),
                Outcome::Done(Answer::None),
            )],
            PLAIN,
        ),
        2,
        "a call of 'pend' answered detached, its target Some(2) not among its 0 unlinks",
    );
    let unlinked = section(1, Section::Detach, vec![unlink(F, A, How::Always)]);
    clean(&judged(&snapshot(), &unlinked, detach(F), PLAIN));
    caught(
        &judged(&snapshot(), &unlinked, detach(G), PLAIN),
        2,
        "answered detached, its target Some(3) not among its 1 unlinks",
    );
    caught(
        &judged(&snapshot(), &[], detach(F), PLAIN),
        2,
        "answered detached, its target Some(2) not among its 0 unlinks",
    );
}

#[test]
fn a_discard_answers_the_frees_it_made() {
    let discard = |files, directories| {
        vec![call(
            &catalogue::ADD_DISCARD_TREE,
            (1, 4),
            Some(B),
            Outcome::Done(Answer::Freed { files, directories }),
        )]
    };
    // `b` unlinked and marked, its file `h` freed as its directory's run marks it, `b` freed
    // after; then `f`, and the link `l`, a directory to the counts, each unlinked and freed in one
    // section, as a node with nothing below it is.
    let freed = [
        section(
            1,
            Section::DiscardMarks,
            vec![unlink(B, ROOT_NODE, How::Discard)],
        ),
        section(2, Section::DiscardMarks, vec![free(H)]),
        section(3, Section::DiscardFree, vec![free(B)]),
    ]
    .concat();
    clean(&judged(&snapshot(), &freed, discard(1, 1), PLAIN));
    for (files, directories) in [(2, 0), (1, 0)] {
        caught(
            &judged(&snapshot(), &freed, discard(files, directories), PLAIN),
            4,
            &format!(
                "answered freeing {files} files and {directories} directories, where its frees \
                 were (1, 1)"
            ),
        );
    }
    let file = section(
        1,
        Section::DiscardMarks,
        vec![unlink(F, A, How::Discard), free(F)],
    );
    let discard_file = |files| {
        vec![call(
            &catalogue::ADD_DISCARD,
            (1, 2),
            Some(F),
            Outcome::Done(Answer::Freed {
                files,
                directories: 0,
            }),
        )]
    };
    clean(&judged(&snapshot(), &file, discard_file(1), PLAIN));
    caught(
        &judged(&snapshot(), &file, discard_file(0), PLAIN),
        2,
        "answered freeing 0 files and 0 directories, where its frees were (1, 0)",
    );
    let link = section(
        1,
        Section::DiscardMarks,
        vec![unlink(L, ROOT_NODE, How::Discard), free(L)],
    );
    let discard_link = vec![call(
        &catalogue::ADD_DISCARD,
        (1, 2),
        Some(L),
        Outcome::Done(Answer::Freed {
            files: 0,
            directories: 1,
        }),
    )];
    clean(&judged(&snapshot(), &link, discard_link, PLAIN));
    // A slot given back is no discard's free.
    let released = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(2, Section::Release, vec![free(FREE)]),
    ]
    .concat();
    caught(
        &judged(
            &snapshot(),
            &released,
            vec![call(
                &catalogue::ADD_DISCARD,
                (1, 3),
                Some(A),
                Outcome::Done(Answer::Freed {
                    files: 1,
                    directories: 0,
                }),
            )],
            PLAIN,
        ),
        3,
        "answered freeing 1 files and 0 directories, where its frees were (0, 0)",
    );
}

#[test]
fn a_served_mark_answers_done_where_its_target_admits_a_served_change_in_its_interval() {
    let mark = |target, end| {
        vec![call(
            &catalogue::SERVED_MARK,
            (1, end),
            Some(target),
            Outcome::Done(Answer::None),
        )]
    };
    let missed = "answered done, its target Some(1) admitting no served change at any seq";
    // Dirty as it begins, or by its own mark.
    clean(&judged(&dirty(served(), A), &[], mark(A, 2), PLAIN));
    let own = section(
        1,
        Section::ServedMark,
        vec![word(A, marked(&served(), A, DIRTY), false)],
    );
    clean(&judged(&served(), &own, mark(A, 2), PLAIN));
    caught(&judged(&served(), &[], mark(A, 2), PLAIN), 2, missed);
    // Dirty with its clear pending admits nothing; the root always admits.
    let mut pending = dirty(served(), A);
    pending.blocks[0].clear_pending[A as usize] = true;
    caught(&judged(&pending, &[], mark(A, 2), PLAIN), 2, missed);
    clean(&judged(&served(), &[], mark(ROOT_NODE, 2), PLAIN));
    // Dirty still, but detached before it began.
    let detached = by(
        1,
        section(0, Section::Detach, vec![unlink(A, ROOT_NODE, How::Always)]),
    );
    caught(
        &judged(&dirty(served(), A), &detached, mark(A, 2), PLAIN),
        2,
        missed,
    );
    // Another task's mark while it runs, cleared again before it ends; or made only after it ends.
    let others = |seq| {
        by(
            1,
            [
                section(
                    seq,
                    Section::Unlabelled,
                    vec![word(A, marked(&served(), A, DIRTY), false)],
                ),
                section(
                    seq + 1,
                    Section::Unlabelled,
                    vec![word(A, marked(&served(), A, 0), false)],
                ),
            ]
            .concat(),
        )
    };
    clean(&judged(&served(), &others(2), mark(A, 5), PLAIN));
    caught(&judged(&served(), &others(5), mark(A, 5), PLAIN), 5, missed);
    // Dirty only part way through another task's section, which no other task sees.
    let within = by(
        1,
        section(
            2,
            Section::Unlabelled,
            vec![
                word(A, marked(&served(), A, DIRTY), false),
                word(A, marked(&served(), A, 0), false),
            ],
        ),
    );
    caught(&judged(&served(), &within, mark(A, 5), PLAIN), 5, missed);
}

/// A call of `kind` by task 0 over `[1, 3)` on `target`, moving it into `destination` under
/// `name` where they are given, refused for `reason`.
fn refusing(
    kind: &'static catalogue::Kind,
    (target, destination, name): (NodeID, Option<NodeID>, Option<&str>),
    reason: Refusal,
) -> Vec<Call> {
    vec![Call {
        destination,
        name: name.map(hash_string),
        ..call(kind, (1, 3), Some(target), Outcome::Refused(reason))
    }]
}

/// `made` with `node`'s flags as `flags` adds them.
fn flagged(mut made: Snapshot, node: NodeID, flags: u16) -> Snapshot {
    made.blocks[0].records[node as usize].flags |= flags;
    made
}

const STAGED_DELETE: u16 = NodeFlags::StagedDelete.bits();
const DISCARDED: u16 = NodeFlags::Discarded.bits();

/// `made` with `f` a directory, below `a`.
fn deep(mut made: Snapshot) -> Snapshot {
    made.blocks[0].records[F as usize].flags = 0;
    made
}

#[test]
fn a_refusal_holds_its_condition_at_some_seq_of_its_call() {
    let missed = "its condition holding at no seq of its interval";
    let detached = |seq, node, parent| {
        by(
            1,
            section(
                seq,
                Section::Detach,
                vec![unlink(node, parent, How::Always)],
            ),
        )
    };
    // `NotFound`: the node, an ancestor, or the destination gone at some `seq`.
    let detach = || {
        refusing(
            &catalogue::UNSERVED_DETACH,
            (F, None, None),
            Refusal::NotFound,
        )
    };
    caught(&judged(&snapshot(), &[], detach(), PLAIN), 3, missed);
    clean(&judged(
        &flagged(snapshot(), F, DISCARDED),
        &[],
        detach(),
        PLAIN,
    ));
    clean(&judged(&snapshot(), &detached(2, F, A), detach(), PLAIN));
    clean(&judged(
        &snapshot(),
        &detached(2, A, ROOT_NODE),
        detach(),
        PLAIN,
    ));
    caught(
        &judged(&snapshot(), &detached(3, F, A), detach(), PLAIN),
        3,
        missed,
    );
    let moving = || {
        refusing(
            &catalogue::DIRTY_MOVE,
            (G, Some(B), None),
            Refusal::NotFound,
        )
    };
    clean(&judged(
        &snapshot(),
        &detached(1, B, ROOT_NODE),
        moving(),
        PLAIN,
    ));
    caught(&judged(&snapshot(), &[], moving(), PLAIN), 3, missed);
    // A walk up from the node reads one level at a time: `a` taken after `g` moved out of it, at
    // no one `seq` above `g`, was above it when the walk read `g`'s parent.
    let gone_from = || {
        refusing(
            &catalogue::UNSERVED_DETACH,
            (G, None, None),
            Refusal::NotFound,
        )
    };
    let moved_then_taken = [
        by(
            1,
            section(1, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
        ),
        detached(2, A, ROOT_NODE),
    ]
    .concat();
    clean(&judged(&snapshot(), &moved_then_taken, gone_from(), PLAIN));
    caught(
        &judged(
            &snapshot(),
            &by(
                1,
                section(1, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
            ),
            gone_from(),
            PLAIN,
        ),
        3,
        missed,
    );
    // A pend's: the node away from the place it was found at, or gone.
    let pend = |parent, name| {
        vec![Call {
            place: Some((parent, hash_string(name))),
            ..call(
                &catalogue::PEND,
                (1, 3),
                Some(F),
                Outcome::Refused(Refusal::NotFound),
            )
        }]
    };
    caught(&judged(&snapshot(), &[], pend(A, "f"), PLAIN), 3, missed);
    clean(&judged(&snapshot(), &[], pend(B, "f"), PLAIN));
    clean(&judged(&snapshot(), &[], pend(A, "g"), PLAIN));
    clean(&judged(
        &snapshot(),
        &detached(2, F, A),
        pend(A, "f"),
        PLAIN,
    ));
    // `Exists`: a live child of the directory carrying the name, but the mover.
    let create = |name| {
        refusing(
            &catalogue::SERVED_CREATE,
            (A, None, Some(name)),
            Refusal::Exists,
        )
    };
    clean(&judged(&served(), &[], create("f"), PLAIN));
    caught(&judged(&served(), &[], create("x"), PLAIN), 3, missed);
    caught(
        &judged(&flagged(served(), F, DISCARDED), &[], create("f"), PLAIN),
        3,
        missed,
    );
    // Held as the call began, though taken before it ends.
    clean(&judged(&snapshot(), &detached(2, F, A), create("f"), PLAIN));
    let rename = |name| {
        refusing(
            &catalogue::SERVED_RENAME,
            (G, Some(A), Some(name)),
            Refusal::Exists,
        )
    };
    clean(&judged(&served(), &[], rename("f"), PLAIN));
    caught(&judged(&served(), &[], rename("g"), PLAIN), 3, missed);
    // Nor the holder a replacing rename replaces.
    let replacing = |holder| {
        let mut calls = refusing(
            &catalogue::SERVED_RENAME_REPLACING,
            (G, Some(A), Some("f")),
            Refusal::Exists,
        );
        calls[0].holder = holder;
        calls
    };
    clean(&judged(&served(), &[], replacing(None), PLAIN));
    caught(
        &judged(&served(), &[], replacing(Some(F)), PLAIN),
        3,
        missed,
    );
    // `IfClean`: the node dirty.
    let rewrite = || refusing(&catalogue::SWAP_REWRITE, (F, None, None), Refusal::IfClean);
    clean(&judged(&dirty(served(), F), &[], rewrite(), PLAIN));
    caught(&judged(&served(), &[], rewrite(), PLAIN), 3, missed);
    let cleaned = by(
        1,
        section(
            2,
            Section::Unlabelled,
            vec![word(F, marked(&snapshot(), F, 0), false)],
        ),
    );
    clean(&judged(&dirty(snapshot(), F), &cleaned, rewrite(), PLAIN));
    // `NotEmpty`: a live child not staged for deletion.
    let rmdir = |dir| {
        refusing(
            &catalogue::SERVED_RMDIR,
            (dir, None, None),
            Refusal::NotEmpty,
        )
    };
    clean(&judged(&served(), &[], rmdir(B), PLAIN));
    caught(
        &judged(&flagged(served(), H, STAGED_DELETE), &[], rmdir(B), PLAIN),
        3,
        missed,
    );
    caught(
        &judged(&flagged(served(), H, DISCARDED), &[], rmdir(B), PLAIN),
        3,
        missed,
    );
    clean(&judged(&snapshot(), &detached(2, H, B), rmdir(B), PLAIN));
    // `NotLocal`: a directory without local content.
    let local = || {
        refusing(
            &catalogue::SERVED_RENAME,
            (A, Some(B), Some("a")),
            Refusal::NotLocal,
        )
    };
    clean(&judged(&served(), &[], local(), PLAIN));
    let mut held = served();
    held.blocks[0].records[A as usize].ext_flags |= NodeExtFlags::LocalContent.bits();
    caught(&judged(&held, &[], local(), PLAIN), 3, missed);
    let local_at = |node, to, name| {
        refusing(
            &catalogue::SERVED_RENAME,
            (node, Some(to), Some(name)),
            Refusal::NotLocal,
        )
    };
    // A respell moves any directory; a file is no directory.
    caught(
        &judged(&served(), &[], local_at(A, ROOT_NODE, "a"), PLAIN),
        3,
        missed,
    );
    caught(
        &judged(&served(), &[], local_at(F, B, "f"), PLAIN),
        3,
        missed,
    );
    // `Modify` and `Restore`: the node not one to change.
    let modify = |node| {
        refusing(
            &catalogue::STAGED_MODIFY,
            (node, None, None),
            Refusal::Modify,
        )
    };
    clean(&judged(
        &flagged(snapshot(), F, STAGED_DELETE),
        &[],
        modify(F),
        PLAIN,
    ));
    clean(&judged(&snapshot(), &[], modify(A), PLAIN));
    caught(&judged(&snapshot(), &[], modify(F), PLAIN), 3, missed);
    clean(&judged(
        &flagged(snapshot(), F, DISCARDED),
        &[],
        modify(F),
        PLAIN,
    ));
    // Its own words decide, not a directory above it detached.
    caught(
        &judged(&snapshot(), &detached(2, A, ROOT_NODE), modify(F), PLAIN),
        3,
        missed,
    );
    let restore = || refusing(&catalogue::RESTORE, (F, None, None), Refusal::Restore);
    clean(&judged(&snapshot(), &[], restore(), PLAIN));
    clean(&judged(
        &flagged(flagged(snapshot(), F, STAGED_DELETE), A, STAGED_DELETE),
        &[],
        restore(),
        PLAIN,
    ));
    clean(&judged(
        &flagged(flagged(snapshot(), F, STAGED_DELETE), A, DISCARDED),
        &[],
        restore(),
        PLAIN,
    ));
    clean(&judged(
        &flagged(snapshot(), F, STAGED_DELETE | DISCARDED),
        &[],
        restore(),
        PLAIN,
    ));
    caught(
        &judged(
            &flagged(snapshot(), F, STAGED_DELETE),
            &[],
            restore(),
            PLAIN,
        ),
        3,
        missed,
    );
    // `Add`: the directory staged for deletion.
    let add = || refusing(&catalogue::STAGED_ADD, (A, None, Some("n")), Refusal::Add);
    clean(&judged(
        &flagged(snapshot(), A, STAGED_DELETE),
        &[],
        add(),
        PLAIN,
    ));
    caught(&judged(&snapshot(), &[], add(), PLAIN), 3, missed);
    let add_to = |dir| refusing(&catalogue::STAGED_ADD, (dir, None, Some("n")), Refusal::Add);
    clean(&judged(&snapshot(), &[], add_to(FREE), PLAIN));
    // A `Discarded` directory refuses as not found.
    caught(
        &judged(&flagged(snapshot(), A, DISCARDED), &[], add(), PLAIN),
        3,
        missed,
    );
    caught(
        &judged(
            &deep(snapshot()),
            &detached(2, A, ROOT_NODE),
            add_to(F),
            PLAIN,
        ),
        3,
        missed,
    );
    // `Move`: the node, a parent, or the destination not one to move with; `Subtree`: the
    // destination the node or below it.
    let staged_move = |node, to, name| {
        refusing(
            &catalogue::STAGED_MOVE,
            (node, Some(to), Some(name)),
            Refusal::Move,
        )
    };
    clean(&judged(
        &flagged(snapshot(), B, STAGED_DELETE),
        &[],
        staged_move(G, B, "x"),
        PLAIN,
    ));
    clean(&judged(
        &flagged(snapshot(), A, STAGED_DELETE),
        &[],
        staged_move(G, B, "x"),
        PLAIN,
    ));
    for flags in [STAGED_DELETE, DISCARDED] {
        clean(&judged(
            &flagged(snapshot(), G, flags),
            &[],
            staged_move(G, B, "x"),
            PLAIN,
        ));
    }
    clean(&judged(&snapshot(), &[], staged_move(G, F, "x"), PLAIN));
    clean(&judged(&snapshot(), &[], staged_move(G, L, "x"), PLAIN));
    clean(&judged(&snapshot(), &[], staged_move(A, A, "x"), PLAIN));
    clean(&judged(
        &deep(snapshot()),
        &[],
        staged_move(A, F, "x"),
        PLAIN,
    ));
    // Already under the destination by that name.
    clean(&judged(&snapshot(), &[], staged_move(G, A, "g"), PLAIN));
    caught(
        &judged(&snapshot(), &[], staged_move(G, A, "x"), PLAIN),
        3,
        missed,
    );
    caught(
        &judged(&snapshot(), &[], staged_move(G, B, "x"), PLAIN),
        3,
        missed,
    );
    // The destination's own words decide, not a directory above it detached.
    caught(
        &judged(
            &deep(snapshot()),
            &detached(2, A, ROOT_NODE),
            staged_move(H, F, "x"),
            PLAIN,
        ),
        3,
        missed,
    );
    let subtree = |to| {
        refusing(
            &catalogue::SERVED_RENAME,
            (A, Some(to), Some("x")),
            Refusal::Subtree,
        )
    };
    clean(&judged(&served(), &[], subtree(A), PLAIN));
    clean(&judged(&served(), &[], subtree(F), PLAIN));
    clean(&judged(&deep(served()), &[], subtree(F), PLAIN));
    caught(&judged(&served(), &[], subtree(B), PLAIN), 3, missed);
    // Below the node as the call began, though moved out before it ends.
    let moved_out = by(
        1,
        section(2, Section::MoveNamed, vec![moved(F, (A, B), "f", None)]),
    );
    clean(&judged(&deep(snapshot()), &moved_out, subtree(F), PLAIN));
    caught(
        &judged(&deep(snapshot()), &moved_out, subtree(B), PLAIN),
        3,
        missed,
    );
}

/// A refusal that claims a condition carries what it is read by: its target, the destination a
/// move names, the name it attaches or moves under.
#[test]
fn a_refusal_carries_what_its_condition_is_read_by() {
    let missing = "carrying no target, destination or name to read its condition by";
    let cases = [
        refusing(&catalogue::STAGED_MOVE, (G, None, Some("x")), Refusal::Move),
        refusing(&catalogue::STAGED_MOVE, (G, Some(B), None), Refusal::Move),
        refusing(&catalogue::SERVED_CREATE, (A, None, None), Refusal::Exists),
        refusing(
            &catalogue::SERVED_RENAME,
            (A, Some(B), None),
            Refusal::NotLocal,
        ),
        refusing(
            &catalogue::SERVED_RENAME,
            (A, None, Some("x")),
            Refusal::Subtree,
        ),
        vec![call(
            &catalogue::UNSERVED_DETACH,
            (1, 3),
            None,
            Outcome::Refused(Refusal::NotFound),
        )],
    ];
    for refused in cases {
        caught(&judged(&snapshot(), &[], refused, PLAIN), 3, missing);
    }
}

#[test]
fn a_node_passed_over_as_taken_is_gone_at_some_seq_of_its_call() {
    let create = |taken: Vec<(NodeID, u64)>| {
        vec![Call {
            taken,
            ..call(
                &catalogue::CHECKED_CREATE,
                (1, 3),
                Some(A),
                Outcome::Done(Answer::None),
            )
        }]
    };
    let detached = by(
        1,
        section(2, Section::Detach, vec![unlink(A, ROOT_NODE, How::Always)]),
    );
    clean(&judged(&snapshot(), &detached, create(vec![(A, 1)]), PLAIN));
    clean(&judged(&snapshot(), &detached, create(vec![(F, 2)]), PLAIN));
    caught(
        &judged(&snapshot(), &[], create(vec![(A, 1)]), PLAIN),
        3,
        "a call of 'checked create' passed over 1 as taken at 1, gone at no seq of its interval \
         from there",
    );
    caught(
        &judged(&snapshot(), &detached, create(vec![(A, 1), (H, 2)]), PLAIN),
        3,
        "passed over 5 as taken at 2, gone at no seq",
    );
    let most = (0..31).map(|seq| (A, seq)).collect();
    clean(&judged(&snapshot(), &detached, create(most), PLAIN));
    let many = (0..32).map(|seq| (A, seq)).collect();
    caught(
        &judged(&snapshot(), &detached, create(many), PLAIN),
        3,
        "made 32 claims past its answer's (32 nodes passed over as taken, 0 lookups, 0 by its \
         walks, 0 marks' heads kept, 0 hops from no link), more than the 31 the model watches",
    );
}

/// A node passed over as taken is read from the `seq` the call found it at on, not before: a slot
/// free as the call began and published before the call found it taken was not gone there.
#[test]
fn a_node_passed_over_as_taken_is_read_from_where_it_was_found() {
    let create = |taken: Vec<(NodeID, u64)>| {
        vec![Call {
            taken,
            ..call(
                &catalogue::CHECKED_CREATE,
                (1, 6),
                Some(A),
                Outcome::Done(Answer::None),
            )
        }]
    };
    let attached = by(1, attach(2, B, "n"));
    clean(&judged(
        &snapshot(),
        &attached,
        create(vec![(FREE, 1)]),
        PLAIN,
    ));
    caught(
        &judged(&snapshot(), &attached, create(vec![(FREE, 5)]), PLAIN),
        6,
        "passed over 7 as taken at 5",
    );
}

/// The nodes a call passed over as taken are held to the model whatever it answered: a held name,
/// a self-report.
#[test]
fn a_node_passed_over_as_taken_is_judged_whatever_the_call_answered() {
    let taken = "passed over 3 as taken at 1";
    let held = |taken| {
        vec![Call {
            name: Some(hash_string("f")),
            taken,
            ..call(
                &catalogue::CHECKED_CREATE,
                (1, 3),
                Some(A),
                Outcome::Done(Answer::Held(F)),
            )
        }]
    };
    let detached = by(
        1,
        section(2, Section::Detach, vec![unlink(F, A, How::Always)]),
    );
    clean(&judged(&snapshot(), &detached, held(vec![(F, 1)]), PLAIN));
    caught(
        &judged(&snapshot(), &detached, held(vec![(G, 1)]), PLAIN),
        3,
        taken,
    );
    let slowed = vec![Call {
        taken: vec![(G, 1)],
        ..call(
            &catalogue::CHECKED_CREATE,
            (1, 3),
            Some(A),
            Outcome::SelfReported(SelfReported::SlowDown),
        )
    }];
    let admitting = Admits {
        self_reports: &[SelfReported::SlowDown],
        ..PLAIN
    };
    caught(&judged(&snapshot(), &[], slowed, admitting), 3, taken);
}

/// A call of `kind` by task 0 over `[1, 4)` on `target`, moving it into `destination` under `name`
/// where they are given, answering done and leaving `leaves`.
fn done_leaving(
    kind: &'static catalogue::Kind,
    (target, destination, name): (NodeID, Option<NodeID>, Option<&str>),
    leaves: Leaves,
) -> Vec<Call> {
    vec![Call {
        destination,
        name: name.map(hash_string),
        spelling: name.map(|name| spelling(name.as_bytes())),
        leaves: Some(leaves),
        ..call(kind, (1, 4), Some(target), Outcome::Done(Answer::None))
    }]
}

/// `node`'s words in `made` with its size `size`, written at `seq` by `task`, a test's own edit
/// where task 1's, the call's own where task 0's.
fn sized(made: &Snapshot, (node, task): (NodeID, usize), seq: u64, size: u64) -> Vec<Record> {
    let record = Node {
        size,
        ..marked(made, node, 0)
    };
    by(
        task,
        section(seq, Section::Unlabelled, vec![word(node, record, false)]),
    )
}

const STAGED_MODIFY: u16 = NodeFlags::StagedModify.bits();
const DIRTY_MODIFY: u16 = NodeFlags::DirtyModify.bits();
const DIRTY_DELETE: u16 = NodeFlags::DirtyDelete.bits();

/// A done answer leaves on its target what it claims at some `seq` of its interval where it wrote
/// nothing there: the words in force before it, or another task's, though another task changes
/// them before it ends.
#[test]
fn a_done_answer_leaves_what_it_claims_at_some_seq_of_its_call() {
    let missed = "at no seq of its interval";
    let at = |node| (node, None, None);
    // Content: held before the call, written in it, or written and overwritten in it; by mode,
    // size and the address's hash, a file keeping its own context.
    let three = Content {
        size: 3,
        ..Content::default()
    };
    let modify = |content| done_leaving(&catalogue::STAGED_MODIFY, at(F), Leaves::Content(content));
    clean(&judged(&snapshot(), &[], modify(Content::default()), PLAIN));
    caught(&judged(&snapshot(), &[], modify(three), PLAIN), 4, missed);
    let written = sized(&snapshot(), (F, 1), 2, 3);
    clean(&judged(&snapshot(), &written, modify(three), PLAIN));
    let overwritten = [written, sized(&snapshot(), (F, 1), 3, 5)].concat();
    clean(&judged(&snapshot(), &overwritten, modify(three), PLAIN));
    let moded = Content {
        mode: 0o644,
        ..Content::default()
    };
    caught(&judged(&snapshot(), &[], modify(moded), PLAIN), 4, missed);
    let hashed = Content {
        address: Address {
            hash: Hash::from([1; 32]),
            ..Address::default()
        },
        ..Content::default()
    };
    caught(&judged(&snapshot(), &[], modify(hashed), PLAIN), 4, missed);
    let contexted = Content {
        address: Address {
            context: Context::from([2; 16]),
            ..Address::default()
        },
        ..Content::default()
    };
    clean(&judged(&snapshot(), &[], modify(contexted), PLAIN));
    // A flip's content, clean.
    let flip = |content| done_leaving(&catalogue::FLIP, at(F), Leaves::Flipped(content));
    clean(&judged(&snapshot(), &[], flip(Content::default()), PLAIN));
    caught(
        &judged(&dirty(snapshot(), F), &[], flip(Content::default()), PLAIN),
        4,
        missed,
    );
    caught(&judged(&snapshot(), &[], flip(three), PLAIN), 4, missed);
    // An in-place change's size.
    let resize = || done_leaving(&catalogue::IN_PLACE, at(F), Leaves::Size(3));
    caught(&judged(&snapshot(), &[], resize(), PLAIN), 4, missed);
    clean(&judged(
        &snapshot(),
        &sized(&snapshot(), (F, 1), 2, 3),
        resize(),
        PLAIN,
    ));
    // A forced modify mark's dirt, a clear's, a staged clear, a delete level, a restore, a
    // copy-up, each by the flags it leaves.
    let flags = |kind, node, leaves| done_leaving(kind, at(node), leaves);
    let mark = || flags(&catalogue::SCAN_MARK_MODIFIED, F, Leaves::Dirty);
    clean(&judged(
        &flagged(snapshot(), F, DIRTY_MODIFY),
        &[],
        mark(),
        PLAIN,
    ));
    caught(&judged(&snapshot(), &[], mark(), PLAIN), 4, missed);
    caught(
        &judged(&dirty(snapshot(), F), &[], mark(), PLAIN),
        4,
        missed,
    );
    let clear = || flags(&catalogue::SCAN_CLEAR, F, Leaves::Clean);
    clean(&judged(&snapshot(), &[], clear(), PLAIN));
    clean(&judged(
        &flagged(snapshot(), F, DISCARDED),
        &[],
        clear(),
        PLAIN,
    ));
    caught(
        &judged(&dirty(snapshot(), F), &[], clear(), PLAIN),
        4,
        missed,
    );
    let modified = || flagged(snapshot(), F, STAGED_MODIFY | DIRTY_MODIFY);
    let deleted = || flagged(snapshot(), F, STAGED_DELETE | DIRTY_DELETE);
    let unstage = || flags(&catalogue::STAGED_CLEAR, F, Leaves::Unstaged);
    clean(&judged(&snapshot(), &[], unstage(), PLAIN));
    caught(&judged(&deleted(), &[], unstage(), PLAIN), 4, missed);
    caught(&judged(&modified(), &[], unstage(), PLAIN), 4, missed);
    let delete = || flags(&catalogue::STAGED_DELETE, F, Leaves::StagedDelete);
    clean(&judged(&deleted(), &[], delete(), PLAIN));
    caught(&judged(&snapshot(), &[], delete(), PLAIN), 4, missed);
    caught(&judged(&modified(), &[], delete(), PLAIN), 4, missed);
    caught(
        &judged(&flagged(snapshot(), F, STAGED_DELETE), &[], delete(), PLAIN),
        4,
        missed,
    );
    let restore = || flags(&catalogue::RESTORE, F, Leaves::Restored);
    clean(&judged(&modified(), &[], restore(), PLAIN));
    caught(&judged(&snapshot(), &[], restore(), PLAIN), 4, missed);
    caught(&judged(&deleted(), &[], restore(), PLAIN), 4, missed);
    caught(
        &judged(
            &flagged(snapshot(), F, STAGED_MODIFY),
            &[],
            restore(),
            PLAIN,
        ),
        4,
        missed,
    );
    caught(
        &judged(&flagged(snapshot(), F, DIRTY_MODIFY), &[], restore(), PLAIN),
        4,
        missed,
    );
    let copy_up = || flags(&catalogue::SERVED_COPY_UP, F, Leaves::LocalCopy);
    let held_local = |made: Snapshot| {
        let mut local = made;
        local.blocks[0].records[F as usize].ext_flags |= NodeExtFlags::LocalContent.bits();
        local
    };
    clean(&judged(
        &held_local(dirty(served(), F)),
        &[],
        copy_up(),
        PLAIN,
    ));
    caught(
        &judged(&dirty(served(), F), &[], copy_up(), PLAIN),
        4,
        missed,
    );
    caught(
        &judged(&held_local(served()), &[], copy_up(), PLAIN),
        4,
        missed,
    );
    // A repin's pin: its revision, and the node it exposes where the pin names one.
    let repin = |hash, child| flags(&catalogue::REPIN, L, Leaves::Pinned { hash, child });
    clean(&judged(
        &snapshot(),
        &[],
        repin(Hash::default(), Some(PINNED)),
        PLAIN,
    ));
    clean(&judged(
        &snapshot(),
        &[],
        repin(Hash::default(), None),
        PLAIN,
    ));
    caught(
        &judged(
            &snapshot(),
            &[],
            repin(Hash::default(), Some(PINNED + 1)),
            PLAIN,
        ),
        4,
        missed,
    );
    caught(
        &judged(&snapshot(), &[], repin(Hash::from([1; 32]), None), PLAIN),
        4,
        missed,
    );
    // A move's place and name, a rename's name, each by its spelling too.
    let place =
        |to, name| done_leaving(&catalogue::DIRTY_MOVE, (G, Some(to), name), Leaves::Placed);
    clean(&judged(&snapshot(), &[], place(A, None), PLAIN));
    clean(&judged(&snapshot(), &[], place(A, Some("g")), PLAIN));
    caught(
        &judged(&snapshot(), &[], place(A, Some("x")), PLAIN),
        4,
        missed,
    );
    caught(
        &judged(&snapshot(), &[], place(A, Some("G")), PLAIN),
        4,
        missed,
    );
    caught(&judged(&snapshot(), &[], place(B, None), PLAIN), 4, missed);
    // A name the model has not read is held by its hash alone.
    let mut unnamed = snapshot();
    unnamed.blocks[0].names[G as usize] = None;
    clean(&judged(&unnamed, &[], place(A, Some("G")), PLAIN));
    caught(
        &judged(&unnamed, &[], place(A, Some("x")), PLAIN),
        4,
        missed,
    );
    let moved_in = by(
        1,
        section(2, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
    );
    clean(&judged(&snapshot(), &moved_in, place(B, None), PLAIN));
    let rename = |name| done_leaving(&catalogue::RENAME, (G, None, Some(name)), Leaves::Named);
    clean(&judged(&snapshot(), &[], rename("g"), PLAIN));
    caught(&judged(&snapshot(), &[], rename("x"), PLAIN), 4, missed);
    // A respell keeps the hash: its spelling tells it.
    caught(&judged(&snapshot(), &[], rename("G"), PLAIN), 4, missed);
}

/// A done answer whose call wrote its target is held to what it leaves once its own last write
/// there applied, not by the words in force before it nor at another `seq`.
#[test]
fn a_done_answer_is_held_after_its_own_last_write() {
    let after = "not holding once its own last write on its target applied";
    // A clear writing its file dirty, though the file was clean as it began; one writing it clean.
    let clear = || done_leaving(&catalogue::SCAN_CLEAR, (F, None, None), Leaves::Clean);
    let writing = |made: &Snapshot, flags| {
        section(
            2,
            Section::UnservedClear,
            vec![word(F, marked(made, F, flags), false)],
        )
    };
    caught(
        &judged(&snapshot(), &writing(&snapshot(), DIRTY), clear(), PLAIN),
        4,
        after,
    );
    let mut cleaned = marked(&dirty(snapshot(), F), F, 0);
    cleaned.flags &= !DIRTY;
    clean(&judged(
        &dirty(snapshot(), F),
        &section(2, Section::UnservedClear, vec![word(F, cleaned, false)]),
        clear(),
        PLAIN,
    ));
    // An owned change writing another size than it claims, though the size held as it began.
    let owned = |size| done_leaving(&catalogue::OWNED, (F, None, None), Leaves::Size(size));
    caught(
        &judged(
            &snapshot(),
            &sized(&snapshot(), (F, 0), 2, 5),
            owned(0),
            PLAIN,
        ),
        4,
        after,
    );
    // Overwritten by another task after its own write: held where its own write left it.
    let overwritten = [
        sized(&snapshot(), (F, 0), 1, 3),
        sized(&snapshot(), (F, 1), 2, 5),
    ]
    .concat();
    clean(&judged(&snapshot(), &overwritten, owned(3), PLAIN));
    let rename = |name| done_leaving(&catalogue::RENAME, (G, None, Some(name)), Leaves::Named);
    // A rename's own mark after its `Rename`, another task renaming between: held where its
    // `Rename` left the node, as the mark writes words its answer leaves nothing in.
    let marked_as_y = Node {
        name_hash: hash_string("y"),
        ..marked(&snapshot(), G, DIRTY)
    };
    let renamed_then_marked = [
        renamed(1, G, A, "x"),
        by(1, renamed(2, G, A, "y")),
        section(3, Section::UnservedMark, vec![word(G, marked_as_y, false)]),
    ]
    .concat();
    clean(&judged(
        &snapshot(),
        &renamed_then_marked,
        rename("x"),
        PLAIN,
    ));
    // A respell with its own `Rename`, as called; one renaming as another name.
    clean(&judged(
        &snapshot(),
        &renamed(2, G, A, "G"),
        rename("G"),
        PLAIN,
    ));
    caught(
        &judged(&snapshot(), &renamed(2, G, A, "y"), rename("x"), PLAIN),
        4,
        "not as called",
    );
    caught(
        &judged(&snapshot(), &renamed(2, G, A, "G"), rename("g"), PLAIN),
        4,
        "not as called",
    );
}

/// A served rename answering moved leaves its node where it named, as its one `Move` put it, of
/// its own node.
#[test]
fn a_served_rename_leaves_its_node_where_it_named() {
    let misplaced = "not as called";
    let both = dirty(dirty(served(), A), B);
    let moving = |node, name| section(2, Section::Reattach, vec![moved(node, (A, B), name, None)]);
    let rename = |to, name| {
        vec![Call {
            destination: Some(to),
            name: Some(hash_string(name)),
            spelling: Some(spelling(name.as_bytes())),
            ..call(
                &catalogue::SERVED_RENAME,
                (1, 3),
                Some(G),
                Outcome::Done(Answer::Moved { added: false }),
            )
        }]
    };
    clean(&judged(&both, &moving(G, "g"), rename(B, "g"), PLAIN));
    caught(
        &judged(&both, &moving(G, "g"), rename(A, "g"), PLAIN),
        3,
        misplaced,
    );
    caught(
        &judged(&both, &moving(G, "g"), rename(B, "x"), PLAIN),
        3,
        misplaced,
    );
    caught(
        &judged(&both, &moving(G, "g"), rename(B, "G"), PLAIN),
        3,
        misplaced,
    );
    caught(
        &judged(&both, &moving(F, "f"), rename(B, "f"), PLAIN),
        3,
        misplaced,
    );
}

/// A moving kind's done answer holds one `Move` or `Rename` at most, as called: a `Keep` within
/// one directory none.
#[test]
fn a_move_answers_one_move_at_most() {
    let place = |to| done_leaving(&catalogue::DIRTY_MOVE, (G, Some(to), None), Leaves::Placed);
    clean(&judged(&snapshot(), &[], place(A), PLAIN));
    let there_and_back = [
        section(1, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
        section(2, Section::MoveNamed, vec![moved(G, (B, A), "g", None)]),
    ]
    .concat();
    caught(
        &judged(&snapshot(), &there_and_back, place(A), PLAIN),
        4,
        "a call of 'dirty move' answered done with 2 moves and 0 renames",
    );
    let moved_and_renamed = [
        section(1, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
        renamed(2, G, B, "x"),
    ]
    .concat();
    caught(
        &judged(&snapshot(), &moved_and_renamed, place(B), PLAIN),
        4,
        "answered done with 1 moves and 1 renames",
    );
    // Its one `Move` to another directory than it was called for.
    let elsewhere = section(1, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]);
    caught(
        &judged(&snapshot(), &elsewhere, place(ROOT_NODE), PLAIN),
        4,
        "not as called",
    );
}

/// A dropped or failed call made no more than its done answer makes: a move's one `Move` or
/// `Rename`, as called, a detach's one unlink, of its target.
#[test]
fn a_call_left_part_way_makes_no_more_than_its_done_answer() {
    let more = "more than its done answer makes";
    let there_and_back = [
        section(1, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
        section(2, Section::MoveNamed, vec![moved(G, (B, A), "g", None)]),
    ]
    .concat();
    // The dirty move stands for any kind here, though its callers drop none, in runs admitting its
    // drop: the model judges a call left part way whatever its kind.
    const DROPPING: Admits = Admits {
        uncancellable_drops: true,
        ..PLAIN
    };
    let move_left = |outcome| {
        vec![Call {
            destination: Some(B),
            leaves: Some(Leaves::Placed),
            failed_reads: 1,
            ..call(&catalogue::DIRTY_MOVE, (1, 4), Some(G), outcome)
        }]
    };
    caught(
        &judged(
            &snapshot(),
            &there_and_back,
            move_left(Outcome::Dropped),
            DROPPING,
        ),
        4,
        more,
    );
    caught(
        &judged(
            &snapshot(),
            &there_and_back,
            move_left(Outcome::Failed(INJECTED.into())),
            DROPPING,
        ),
        4,
        more,
    );
    let there = section(1, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]);
    clean(&judged(
        &snapshot(),
        &there,
        move_left(Outcome::Dropped),
        DROPPING,
    ));
    // Its one move elsewhere than called.
    for outcome in [Outcome::Dropped, Outcome::Failed(INJECTED.into())] {
        let elsewhere = vec![Call {
            destination: Some(A),
            ..move_left(outcome).remove(0)
        }];
        caught(
            &judged(&snapshot(), &there, elsewhere, DROPPING),
            4,
            "answered part way, moving 3 under 4",
        );
    }
    let detach_left = |target, outcome| {
        vec![Call {
            failed_reads: 1,
            ..call(&catalogue::UNSERVED_DETACH, (1, 4), Some(target), outcome)
        }]
    };
    let failed = || Outcome::Failed(INJECTED.into());
    let detached = section(2, Section::Detach, vec![unlink(F, A, How::Always)]);
    clean(&judged(
        &snapshot(),
        &detached,
        detach_left(F, Outcome::Dropped),
        DROPPING,
    ));
    for outcome in [Outcome::Dropped, failed()] {
        caught(
            &judged(&snapshot(), &detached, detach_left(G, outcome), DROPPING),
            4,
            "with 1 unlinks, its target Some(3) not among them",
        );
    }
    // Two unlinks, its target's among them.
    let both = [
        detached,
        section(3, Section::Detach, vec![unlink(G, A, How::Always)]),
    ]
    .concat();
    caught(
        &judged(&snapshot(), &both, detach_left(F, failed()), DROPPING),
        4,
        "with 2 unlinks, its target Some(2) among them",
    );
}
