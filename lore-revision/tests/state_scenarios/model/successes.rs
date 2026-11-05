// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model's conditional successes and the clear's last phase, on records made by hand over the
//! State made by hand: a change decided on a condition is a violation where the condition of its
//! refusal held, by the model, at its record. Each success as the code makes it is clean; one made
//! where its condition failed is caught.

use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Check;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
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
use super::NEXT;
use super::caught;
use super::clean;
use super::count;
use super::detached;
use super::grab;
use super::initialized;
use super::record;
use super::replayed;
use super::section;
use super::served;
use super::snapshot;
use super::with;
use super::word;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;

/// The model of `snapshot` with `records` replayed, and the `seq` of the last.
fn judged(snapshot: &Snapshot, records: &[Record]) -> (Model, u64) {
    let seq = records.last().map_or(0, |record| record.seq);
    (replayed(snapshot, records), seq)
}

/// Asserts `records` are clean.
fn holds(snapshot: &Snapshot, records: &[Record]) {
    clean(&judged(snapshot, records).0);
}

/// Asserts `records` are caught at their last, saying `what`.
fn wrong(snapshot: &Snapshot, records: &[Record], what: &str) {
    let (model, seq) = judged(snapshot, records);
    caught(&model, seq, what);
}

const DIRTY: u16 = NodeFlags::Dirty.bits();
const DIRTY_ADD: u16 = NodeFlags::DirtyAdd.bits();
const DIRTY_MODIFY: u16 = NodeFlags::DirtyModify.bits();
const ACTIONS: u16 = NodeFlags::ActionBits.bits();
const STAGED_DELETE: u16 = NodeFlags::StagedDelete.bits();
const STAGED_ADD: u16 = NodeFlags::StagedAdd.bits();

/// An attach's records from `seq` on: the free slot initialized as file `name` below `parent`
/// and published there, checked as `check` says against `expected`.
fn checked_attach(
    seq: u64,
    parent: NodeID,
    name: &str,
    (check, expected): (Check, Expected),
) -> Vec<Record> {
    let made = record(parent, name, Kind::File);
    [
        section(seq, Section::Grab, vec![grab(FREE)]),
        section(
            seq + 1,
            Section::Grab,
            vec![word(FREE, initialized(made), false)],
        ),
        section(
            seq + 2,
            Section::Publish,
            vec![
                Effect::Publish {
                    node: FREE,
                    parent,
                    name_hash: hash_string(name),
                    name: Name::new(name.as_bytes()),
                    kind: Kind::File,
                    check,
                    expected,
                    instance: INSTANCE,
                },
                word(FREE, made, false),
            ],
        ),
    ]
    .concat()
}

/// `records` after `before`.
fn then(before: Vec<Record>, records: Vec<Record>) -> Vec<Record> {
    [before, records].concat()
}

#[test]
fn a_checked_publish_finds_its_parent_live_and_its_name_free() {
    let plain = snapshot();
    let log = (Check::Absent, Expected::Log { seq: 0 });
    holds(&plain, &checked_attach(1, A, "n", log));
    // `f` holds the name: a lookup's unchanged count found it free, and on a served State any
    // publish did.
    wrong(
        &plain,
        &checked_attach(1, A, "f", (Check::Unchanged, count(0))),
        "holds its name",
    );
    let others = [
        log,
        (Check::Logged, Expected::Log { seq: 0 }),
        (Check::Live, Expected::Nothing),
    ];
    for check in others {
        wrong(
            &served(),
            &checked_attach(1, A, "f", check),
            "holds its name",
        );
    }
    // An unserved State's adds may duplicate a name, which its logs do not see.
    for check in others {
        holds(&plain, &checked_attach(1, A, "f", check));
    }
    // `Unchanged`: the count the lookup read.
    holds(
        &plain,
        &checked_attach(1, A, "n", (Check::Unchanged, count(0))),
    );
    wrong(
        &plain,
        &checked_attach(1, A, "n", (Check::Unchanged, count(3))),
        "a publish checked unchanged",
    );
    // A parent Discarded, one staged for deletion under `Unstaged`, one no directory.
    wrong(
        &plain,
        &then(
            detached(1, A, 0),
            checked_attach(2, A, "n", (Check::Live, Expected::Nothing)),
        ),
        "published below Discarded",
    );
    let staged = with(&[(A, STAGED_DELETE)], false);
    wrong(
        &staged,
        &checked_attach(1, A, "n", (Check::Unstaged, Expected::Nothing)),
        "published by a C API add below",
    );
    holds(
        &staged,
        &checked_attach(1, A, "n", (Check::Live, Expected::Nothing)),
    );
    // The kind where the check tests it, as `Live` and `Unstaged` do not.
    wrong(
        &plain,
        &checked_attach(1, F, "n", (Check::Unchanged, count(0))),
        "no directory",
    );
    holds(
        &plain,
        &checked_attach(1, F, "n", (Check::Live, Expected::Nothing)),
    );
}

fn moved(
    node: NodeID,
    (from, to): (NodeID, NodeID),
    name: &str,
    replaced: Option<NodeID>,
    (check, expected): (Check, Expected),
) -> Effect {
    Effect::Move {
        node,
        from,
        to,
        name_hash: hash_string(name),
        name: Some(Name::new(name.as_bytes())),
        replaced,
        check,
        expected,
        instances: (INSTANCE, INSTANCE),
    }
}

#[test]
fn a_checked_move_finds_its_count_or_its_name_free() {
    let plain = snapshot();
    let walk = |changes: u32| (Check::Walk, count(changes));
    let reattach = |name: &str, check| {
        section(
            1,
            Section::Reattach,
            vec![moved(G, (A, B), name, None, check)],
        )
    };
    holds(&plain, &reattach("n", walk(0)));
    wrong(&plain, &reattach("n", walk(2)), "a move checked unchanged");
    // Against an absence: on a served State no other node holds the name it joins; an unserved
    // State's log does not see a duplicate.
    let log = (Check::Absent, Expected::Log { seq: 0 });
    holds(&plain, &reattach("n", log));
    holds(&plain, &reattach("h", log));
    wrong(&served(), &reattach("h", log), "holds its name");
    // Its own name: a respell holds what it held.
    holds(
        &plain,
        &section(
            1,
            Section::Reattach,
            vec![moved(
                G,
                (A, A),
                "g",
                None,
                (Check::Logged, Expected::Log { seq: 0 }),
            )],
        ),
    );
    // A walk compares the count alone: on an unserved State it takes the mover's own name as
    // free, where an add may have made a duplicate; a served name is unique whatever the check.
    holds(&plain, &reattach("h", walk(0)));
    wrong(&served(), &reattach("h", walk(0)), "holds its name");
    // A checked move joins a live directory; any move moves a live node.
    let into = |to: NodeID, check| {
        section(
            1,
            Section::Reattach,
            vec![moved(G, (A, to), "n", None, check)],
        )
    };
    wrong(&plain, &into(F, walk(0)), "no live directory");
    let gone = with(&[(B, NodeFlags::Discarded.bits())], false);
    wrong(&gone, &into(B, log), "no live directory");
    holds(&plain, &into(F, (Check::Unchecked, Expected::Nothing)));
    let discarded = with(&[(G, NodeFlags::Discarded.bits())], false);
    wrong(&discarded, &reattach("n", walk(0)), "a move of Discarded");
    let renamed = |snapshot: &Snapshot| {
        judged(
            snapshot,
            &section(
                1,
                Section::RenameInPlace,
                vec![Effect::Rename {
                    node: G,
                    parent: A,
                    name_hash: hash_string("n"),
                    name: Name::new(b"n"),
                    instance: INSTANCE,
                }],
            ),
        )
    };
    clean(&renamed(&plain).0);
    caught(&renamed(&discarded).0, 1, "a rename of Discarded");
}

#[test]
fn a_conditional_detach_held_its_condition() {
    let plain = snapshot();
    let detach = |node: NodeID, parent: NodeID, how: How, expected: Expected| {
        section(
            1,
            Section::Detach,
            vec![Effect::Detach {
                node,
                parent,
                how,
                expected,
                instance: INSTANCE,
            }],
        )
    };
    // `IfClean`: a dirty node detached is data lost.
    holds(&plain, &detach(F, A, How::IfClean, Expected::Nothing));
    wrong(
        &with(&[(F, DIRTY)], false),
        &detach(F, A, How::IfClean, Expected::Nothing),
        "data lost",
    );
    // `IfUnchanged`: the node's own count.
    holds(&plain, &detach(A, 0, How::IfUnchanged, count(0)));
    wrong(
        &plain,
        &detach(A, 0, How::IfUnchanged, count(5)),
        "a detach checked unchanged",
    );
    // `At`: where it stood.
    let at = |parent: NodeID, name: &str| Expected::At {
        parent,
        name_hash: hash_string(name),
    };
    holds(&plain, &detach(F, A, How::At, at(A, "f")));
    wrong(
        &plain,
        &detach(F, A, How::At, at(A, "x")),
        "not where it expected",
    );
    // `IfEmpty`: no child.
    wrong(
        &plain,
        &detach(A, 0, How::IfEmpty, Expected::Head(G)),
        "held a child",
    );
    holds(
        &plain,
        &then(
            detached(1, H, B),
            section(
                2,
                Section::Detach,
                vec![Effect::Detach {
                    node: B,
                    parent: 0,
                    how: How::IfEmpty,
                    expected: Expected::Head(0),
                    instance: INSTANCE,
                }],
            ),
        ),
    );
    // Under a Discarded parent: refused, but for a discard's unlink.
    let below_gone = |how: How| {
        then(
            detached(1, A, 0),
            section(
                2,
                Section::Detach,
                vec![Effect::Detach {
                    node: F,
                    parent: A,
                    how,
                    expected: Expected::Nothing,
                    instance: INSTANCE,
                }],
            ),
        )
    };
    wrong(
        &plain,
        &below_gone(How::Always),
        "a detach of 2 below Discarded",
    );
    holds(&plain, &below_gone(How::Discard));
}

/// A `Word` of `node` in `section` at `seq`, its record as `snapshot` holds it changed by `f`.
fn written(
    snapshot: &Snapshot,
    seq: u64,
    section_of: Section,
    node: NodeID,
    f: impl Fn(&mut Node),
) -> Vec<Record> {
    let mut is = snapshot.blocks[0].records[node as usize];
    f(&mut is);
    section(seq, section_of, vec![word(node, is, false)])
}

fn resized(node: &mut Node) {
    node.size += 1;
}

#[test]
fn a_rewrite_or_a_modify_writes_a_live_file() {
    let plain = snapshot();
    for section_of in [Section::Rewrite, Section::StagedModify] {
        holds(&plain, &written(&plain, 1, section_of, F, resized));
        wrong(
            &plain,
            &written(&plain, 1, section_of, A, resized),
            "no live file",
        );
        let staged = with(&[(F, STAGED_DELETE)], false);
        wrong(
            &staged,
            &written(&staged, 1, section_of, F, resized),
            "no live file",
        );
        wrong(
            &plain,
            &then(
                detached(1, F, A),
                written(&plain, 2, section_of, F, |node| {
                    node.flags |= NodeFlags::Discarded.bits();
                    node.size += 1;
                }),
            ),
            "no live file",
        );
    }
    // A served State's unserved rewrite, the swap's, tests no parent: its served rewrite's
    // `Admitted` does.
    let served = with(&[], true);
    holds(
        &served,
        &then(
            detached(1, A, 0),
            written(&served, 2, Section::Rewrite, F, resized),
        ),
    );
}

#[test]
fn a_restore_takes_back_a_node_staged_for_deletion() {
    let staged = with(&[(F, STAGED_DELETE)], false);
    let restored = |node: &mut Node| node.flags &= !NodeFlags::StagedBits.bits();
    holds(&staged, &written(&staged, 1, Section::Restore, F, restored));
    let plain = snapshot();
    wrong(
        &plain,
        &written(&plain, 1, Section::Restore, F, resized),
        "not staged for deletion",
    );
    // Not Discarded either, which no staged flag keeps.
    let gone = with(&[(F, STAGED_DELETE | NodeFlags::Discarded.bits())], false);
    wrong(
        &gone,
        &written(&gone, 1, Section::Restore, F, restored),
        "not staged for deletion",
    );
    let parent_staged = with(&[(F, STAGED_DELETE), (A, STAGED_DELETE)], false);
    wrong(
        &parent_staged,
        &written(&parent_staged, 1, Section::Restore, F, restored),
        "below its parent",
    );
    wrong(
        &staged,
        &then(
            detached(1, A, 0),
            written(&staged, 2, Section::Restore, F, restored),
        ),
        "below its parent",
    );
}

#[test]
fn a_delete_level_marks_a_live_node_not_yet_staged() {
    let plain = snapshot();
    let mut deleted = plain.blocks[0].records[F as usize];
    deleted.flags |= STAGED_DELETE | DIRTY;
    holds(
        &plain,
        &admission(
            1,
            Section::DeleteLevel,
            (F, Expected::Deletable(A)),
            vec![word(F, deleted, false)],
        ),
    );
    // Its admission: live, still below its parent, neither added nor staged for deletion, which
    // a mark leaving the flags as they were writes no `Word` to show.
    for flags in [STAGED_DELETE, STAGED_ADD] {
        let marked = with(&[(F, flags)], false);
        wrong(
            &marked,
            &admission(1, Section::DeleteLevel, (F, Expected::Deletable(A)), vec![]),
            "admitted on Deletable",
        );
    }
    wrong(
        &plain,
        &admission(1, Section::DeleteLevel, (F, Expected::Deletable(B)), vec![]),
        "admitted on Deletable",
    );
    for flags in [STAGED_DELETE, STAGED_ADD] {
        let marked = with(&[(F, flags)], false);
        wrong(
            &marked,
            &written(&marked, 1, Section::DeleteLevel, F, |node| {
                node.flags |= DIRTY;
            }),
            "a delete level's mark",
        );
    }
    wrong(
        &plain,
        &then(
            detached(1, F, A),
            written(&plain, 2, Section::DeleteLevel, F, |node| {
                node.flags |= NodeFlags::Discarded.bits() | DIRTY;
            }),
        ),
        "a delete level's mark",
    );
}

#[test]
fn an_owned_change_writes_a_live_node() {
    let plain = snapshot();
    holds(&plain, &written(&plain, 1, Section::Owned, F, resized));
    wrong(
        &plain,
        &then(
            detached(1, F, A),
            written(&plain, 2, Section::Owned, F, |node| {
                node.flags |= NodeFlags::Discarded.bits();
                node.size += 1;
            }),
        ),
        "an owned change of Discarded",
    );
}

/// `section_of`'s admission of `node` on `expected` at `seq`, before `words`.
fn admission(
    seq: u64,
    section_of: Section,
    (node, expected): (NodeID, Expected),
    words: Vec<Effect>,
) -> Vec<Record> {
    section(
        seq,
        section_of,
        [vec![Effect::Admitted { node, expected }], words].concat(),
    )
}

#[test]
fn an_admitted_change_held_its_condition() {
    let admitted =
        |node: NodeID, expected: Expected| admission(1, Section::InPlace, (node, expected), vec![]);
    let at = |parent: NodeID, name: &str| Expected::At {
        parent,
        name_hash: hash_string(name),
    };
    let plain = snapshot();
    holds(&plain, &admitted(F, at(A, "f")));
    wrong(&plain, &admitted(F, at(B, "f")), "admitted on At");
    wrong(&plain, &admitted(F, at(A, "x")), "admitted on At");
    wrong(
        &plain,
        &then(
            detached(1, F, A),
            admission(2, Section::InPlace, (F, at(A, "f")), vec![]),
        ),
        "admitted on At",
    );
    // An `IfClean` rewrite's.
    let clean_rewrite = |snapshot: &Snapshot| {
        let mut is = snapshot.blocks[0].records[F as usize];
        is.size += 1;
        admission(
            1,
            Section::Rewrite,
            (F, Expected::Clean),
            vec![word(F, is, false)],
        )
    };
    holds(&plain, &clean_rewrite(&plain));
    let dirty = with(&[(F, DIRTY)], false);
    wrong(&dirty, &clean_rewrite(&dirty), "admitted on Clean");
    // Of a live file: one rewritten to the content it holds writes no `Word`.
    let staged = with(&[(F, STAGED_DELETE)], false);
    wrong(
        &staged,
        &admission(1, Section::Rewrite, (F, Expected::Clean), vec![]),
        "admitted on Clean",
    );
    wrong(
        &plain,
        &then(
            detached(1, F, A),
            admission(2, Section::Rewrite, (F, Expected::Clean), vec![]),
        ),
        "admitted on Clean",
    );
    wrong(
        &plain,
        &admission(1, Section::Rewrite, (A, Expected::Clean), vec![]),
        "admitted on Clean",
    );
    // A served rewrite's: a live node below its live parent.
    let serving = served();
    let below =
        |parent: NodeID| admission(2, Section::Rewrite, (F, Expected::Below(parent)), vec![]);
    holds(&serving, &below(A));
    let gone = with(&[(F, NodeFlags::Discarded.bits())], true);
    wrong(&gone, &below(A), "admitted on Below");
    wrong(&serving, &below(B), "admitted on Below");
    wrong(
        &serving,
        &admission(2, Section::Rewrite, (A, Expected::Below(0)), vec![]),
        "admitted on Below",
    );
    wrong(
        &serving,
        &then(detached(1, A, 0), below(A)),
        "admitted on Below",
    );
    // A checked move's: a node not staged for deletion, between parents neither Discarded nor
    // staged for deletion.
    let movable = |snapshot: &Snapshot, from: NodeID, to: NodeID| {
        judged(
            snapshot,
            &admission(
                1,
                Section::MoveNamed,
                (G, Expected::Movable { from, to }),
                vec![],
            ),
        )
    };
    clean(&movable(&plain, A, B).0);
    clean(&movable(&plain, A, A).0);
    for marked in [
        (G, STAGED_DELETE),
        (A, STAGED_DELETE),
        (B, STAGED_DELETE),
        (B, NodeFlags::Discarded.bits()),
    ] {
        caught(
            &movable(&with(&[marked], false), A, B).0,
            1,
            "admitted on Movable",
        );
    }
    // A local-only reattach's: a directory holding no local content moves only to its own
    // place.
    let local = |snapshot: &Snapshot, to: NodeID, name: &str| {
        judged(
            snapshot,
            &admission(
                1,
                Section::Reattach,
                (
                    B,
                    Expected::Local {
                        to,
                        name_hash: hash_string(name),
                    },
                ),
                vec![],
            ),
        )
    };
    clean(&local(&serving, 0, "b").0);
    caught(&local(&serving, A, "b").0, 1, "admitted on Local");
    let mut holding = served();
    holding.blocks[0].records[B as usize].ext_flags |= NodeExtFlags::LocalContent.bits();
    clean(&local(&holding, A, "b").0);
}

#[test]
fn an_in_place_change_writes_only_what_it_admitted() {
    let plain = snapshot();
    let mut is = plain.blocks[0].records[F as usize];
    is.size += 1;
    let at = Expected::At {
        parent: A,
        name_hash: hash_string("f"),
    };
    holds(
        &plain,
        &admission(1, Section::InPlace, (F, at), vec![word(F, is, false)]),
    );
    wrong(
        &plain,
        &written(&plain, 1, Section::InPlace, F, resized),
        "no change of it admitted",
    );
    wrong(
        &plain,
        &admission(1, Section::InPlace, (G, at), vec![word(F, is, false)]),
        "no change of it admitted",
    );
}

/// A settle's phase `section_of` of `a` at `seq`, admitted at `a`'s place, writing `words`.
fn phase(seq: u64, section_of: Section, words: Effect) -> Vec<Record> {
    let place = Expected::At {
        parent: 0,
        name_hash: hash_string("a"),
    };
    admission(seq, section_of, (A, place), vec![words])
}

/// The settle of `a`, dirty, from `seq`: phase 1 setting its pending clear, `between`, then phase
/// 3 clearing it, as a settle that found its children equal writes it.
fn settled(dirty: &Snapshot, seq: u64, between: Vec<Record>) -> Vec<Record> {
    let was = dirty.blocks[0].records[A as usize];
    let next = seq + 1 + between.last().map_or(0, |record| record.seq - seq);
    let mut cleared = was;
    cleared.flags &= !(DIRTY | ACTIONS);
    cleared.size = 7;
    [
        phase(seq, Section::SettlePhase1, word(A, was, true)),
        between,
        phase(next, Section::SettlePhase3, word(A, cleared, false)),
    ]
    .concat()
}

#[test]
fn a_settle_clears_only_what_its_first_phase_saw() {
    let dirty = with(&[(A, DIRTY)], true);
    holds(&dirty, &settled(&dirty, 1, vec![]));
    // A served mark between the phases drops the pending clear: the clear must keep it.
    let was = dirty.blocks[0].records[A as usize];
    let marked = section(2, Section::ServedMark, vec![word(A, was, false)]);
    wrong(
        &dirty,
        &settled(&dirty, 1, marked),
        "no pending clear in force",
    );
    // A publish between the phases changes the count: the clear must keep it.
    let published = checked_attach(2, A, "n", (Check::Live, Expected::Nothing));
    wrong(&dirty, &settled(&dirty, 1, published), "count moved");
    // The pending clear dropped with the directory kept dirty, as its drop leaves it.
    holds(
        &dirty,
        &[
            phase(1, Section::SettlePhase1, word(A, was, true)),
            section(2, Section::SettlePhase3, vec![word(A, was, false)]),
        ]
        .concat(),
    );
    // Two settles of `a` on two tasks, the first's pending clear dropped by a mark and set again
    // by the second's first phase: the first may not clear on the second's.
    let tasked = |records: Vec<Record>, task: usize| {
        records
            .into_iter()
            .map(|mut record| {
                record.task = Some(task);
                record
            })
            .collect::<Vec<_>>()
    };
    let mut cleared = was;
    cleared.flags &= !(DIRTY | ACTIONS);
    cleared.size = 7;
    let overlapped = |finisher: usize| {
        [
            tasked(phase(1, Section::SettlePhase1, word(A, was, true)), 0),
            tasked(
                section(2, Section::ServedMark, vec![word(A, was, false)]),
                2,
            ),
            tasked(phase(3, Section::SettlePhase1, word(A, was, true)), 1),
            tasked(
                phase(4, Section::SettlePhase3, word(A, cleared, false)),
                finisher,
            ),
        ]
        .concat()
    };
    wrong(&dirty, &overlapped(0), "its own first phase");
    holds(&dirty, &overlapped(1));
    // Its content written is a clear too.
    let quiet = with(&[], true);
    let mut sized = quiet.blocks[0].records[A as usize];
    sized.size = 7;
    wrong(
        &quiet,
        &phase(1, Section::SettlePhase3, word(A, sized, false)),
        "no pending clear in force",
    );
}

#[test]
fn a_change_decided_on_no_other_record_is_admitted_first() {
    let dirty = with(&[(A, DIRTY)], true);
    let was = dirty.blocks[0].records[A as usize];
    let mut cleared = was;
    cleared.flags &= !(DIRTY | ACTIONS);
    // A settle's first phase and its clear, unadmitted.
    wrong(
        &dirty,
        &section(1, Section::SettlePhase1, vec![word(A, was, true)]),
        "no change of it admitted",
    );
    wrong(
        &dirty,
        &[
            phase(1, Section::SettlePhase1, word(A, was, true)),
            section(2, Section::SettlePhase3, vec![word(A, cleared, false)]),
        ]
        .concat(),
        "a settle cleared 1, no change of it admitted",
    );
    // A delete level's mark, unadmitted.
    let plain = snapshot();
    wrong(
        &plain,
        &written(&plain, 1, Section::DeleteLevel, F, |node| {
            node.flags |= STAGED_DELETE | DIRTY;
        }),
        "no change of it admitted",
    );
}

#[test]
fn a_mark_writes_a_live_node() {
    let dirtied = |node: &mut Node| node.flags |= DIRTY;
    // A served mark's level: a live child below a live parent.
    let serving = served();
    holds(
        &serving,
        &written(&serving, 1, Section::ServedMark, F, dirtied),
    );
    let gone = with(&[(F, NodeFlags::Discarded.bits())], true);
    wrong(
        &gone,
        &written(&gone, 1, Section::ServedMark, F, dirtied),
        "a served mark of",
    );
    wrong(
        &serving,
        &then(
            detached(1, A, 0),
            written(&serving, 2, Section::ServedMark, F, dirtied),
        ),
        "a served mark of",
    );
    // An unserved mark's dirty half stops at a Discarded node, which its staged half marks.
    let unserved_gone = with(&[(F, NodeFlags::Discarded.bits())], false);
    wrong(
        &unserved_gone,
        &written(&unserved_gone, 1, Section::UnservedMark, F, dirtied),
        "an unserved mark dirtied Discarded",
    );
    holds(
        &unserved_gone,
        &written(&unserved_gone, 1, Section::UnservedMark, F, |node| {
            node.flags |= STAGED_DELETE;
        }),
    );
    let dirty_gone = with(&[(F, NodeFlags::Discarded.bits() | DIRTY)], false);
    wrong(
        &dirty_gone,
        &written(&dirty_gone, 1, Section::UnservedMark, F, |node| {
            node.flags &= !DIRTY;
        }),
        "an unserved mark dirtied Discarded",
    );
    // A rename in place refuses a Discarded node before its own mark writes it.
    wrong(
        &unserved_gone,
        &written(&unserved_gone, 1, Section::RenameInPlace, F, |node| {
            node.flags |= NodeFlags::StagedMove.bits();
        }),
        "a rename of Discarded 2, which wrote it",
    );
}

/// The served reattach of `g`, `mover_flags` added to its own, over `h`, `holder_flags` added to
/// its, in `b` under `h`'s name; its `Word` then as `written` makes it, none where `None`, as the
/// product writes none where its flags stay.
fn over_holder(
    mover_flags: u16,
    holder_flags: u16,
    written: Option<fn(u16) -> u16>,
) -> (Snapshot, Vec<Record>) {
    let made = with(&[(G, mover_flags), (H, holder_flags)], true);
    let mut effects = vec![moved(G, (A, B), "h", Some(H), (Check::Replacing, count(0)))];
    if let Some(written) = written {
        let mut is = made.blocks[0].records[G as usize];
        is.flags = written(is.flags);
        is.parent = B;
        is.name_hash = hash_string("h");
        effects.push(word(G, is, false));
    }
    (made, section(1, Section::Reattach, effects))
}

#[test]
fn a_served_move_over_a_holder_takes_its_path_over() {
    let modified: fn(u16) -> u16 = |flags| (flags & !ACTIONS) | DIRTY_MODIFY;
    // An added file over a holder the revision holds no longer counts as added (H20), which a
    // move that writes no `Word` leaves undone.
    let (made, records) = over_holder(DIRTY_ADD, 0, Some(modified));
    holds(&made, &records);
    let (made, records) = over_holder(DIRTY_ADD, 0, None);
    wrong(&made, &records, "H20");
    // Over an added holder, or as no added mover, the actions stay.
    let (made, records) = over_holder(DIRTY_ADD, DIRTY_ADD, None);
    holds(&made, &records);
    let (made, records) = over_holder(DIRTY_ADD, DIRTY_ADD, Some(modified));
    wrong(&made, &records, "H20");
    let (made, records) = over_holder(DIRTY | DIRTY_MODIFY, 0, None);
    holds(&made, &records);
    // A directory moved clean takes `Dirty` alone, across parents under the moves permit.
    let moved_directory = |dirtied: bool| {
        let made = served();
        let mut effects = vec![moved(B, (0, A), "b", None, (Check::Walk, count(0)))];
        if dirtied {
            let mut is = made.blocks[0].records[B as usize];
            is.flags |= DIRTY;
            is.parent = A;
            effects.push(word(B, is, false));
        }
        let permit = section(0, Section::Unlabelled, vec![Effect::Permit]);
        (made, then(permit, section(1, Section::Reattach, effects)))
    };
    let (made, records) = moved_directory(true);
    holds(&made, &records);
    let (made, records) = moved_directory(false);
    wrong(&made, &records, "H20");
}

#[test]
fn a_build_adds_into_a_directory() {
    let add = |into: NodeID| Effect::BuildAdd {
        node: NEXT,
        parent: into,
        name_hash: hash_string("n"),
        name: Name::new(b"n"),
        kind: Kind::File,
    };
    let built = |into: NodeID, root_kind: Kind| {
        let root = record(A, "r", root_kind);
        let added = record(into, "n", Kind::File);
        [
            section(1, Section::Grab, vec![grab(FREE)]),
            section(2, Section::Grab, vec![word(FREE, initialized(root), false)]),
            section(3, Section::Grab, vec![grab(NEXT)]),
            section(
                4,
                Section::Grab,
                vec![word(NEXT, initialized(added), false)],
            ),
            section(5, Section::BuildAdd, vec![add(into)]),
        ]
        .concat()
    };
    holds(&snapshot(), &built(FREE, Kind::Directory));
    wrong(&snapshot(), &built(FREE, Kind::File), "no directory");
}
