// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Each section's write set, on the State made by hand: a `Word` changing a field inside its
//! section's set for its node's role passes, and one changing a field outside it is caught at the
//! `Word`. The log tests' runs hold each set at least as wide as its section's code writes, and
//! their every `Word`, altered field by field, to the model's judgement (`log::check_model`).

use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
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
use super::H;
use super::INSTANCE;
use super::L;
use super::NEXT;
use super::PINNED;
use super::STATE;
use super::caught;
use super::clean;
use super::moved;
use super::record;
use super::replayed;
use super::section;
use super::served;
use super::snapshot;
use super::unlink;
use super::word;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;
use crate::harness::model::fields::Fields;
use crate::harness::model::fields::Judged;
use crate::harness::model::fields::Role;
use crate::harness::model::fields::set;

/// The `seq` of the section a row's `Word` is in.
const AT: u64 = 100;

/// One section's write set for one role: the records of earlier sections, the records before the
/// `Word` in its own, the node it writes, and the fields it may change.
struct Row {
    section: Section,
    served: bool,
    prior: Vec<Record>,
    before: Vec<Effect>,
    node: NodeID,
    fields: Fields,
}

fn row(section: Section, node: NodeID, fields: Fields) -> Row {
    Row {
        section,
        served: false,
        prior: Vec::new(),
        before: Vec::new(),
        node,
        fields,
    }
}

impl Row {
    fn served(self) -> Self {
        Self {
            served: true,
            ..self
        }
    }

    fn prior(self, prior: Vec<Record>) -> Self {
        Self { prior, ..self }
    }

    fn before(self, before: Vec<Effect>) -> Self {
        Self { before, ..self }
    }
}

/// The free slot grabbed and initialized as `name` below `parent`, in sections from `seq` on.
fn in_flight(seq: u64, parent: NodeID, name: &str, kind: Kind) -> Vec<Record> {
    let mut made = record(parent, name, kind);
    made.flags |= NodeFlags::Discarded.bits();
    [
        section(
            seq,
            Section::Grab,
            vec![Effect::Grab {
                node: FREE,
                instance: INSTANCE,
            }],
        ),
        section(seq + 1, Section::Grab, vec![word(FREE, made, false)]),
    ]
    .concat()
}

/// A build: the free slot as the directory `r` below `a`, the next slot as the file `n` below it.
fn build() -> Vec<Record> {
    let mut added = record(FREE, "n", Kind::File);
    added.flags |= NodeFlags::Discarded.bits();
    [
        in_flight(1, A, "r", Kind::Directory),
        section(
            3,
            Section::Grab,
            vec![Effect::Grab {
                node: NEXT,
                instance: INSTANCE,
            }],
        ),
        section(4, Section::Grab, vec![word(NEXT, added, false)]),
    ]
    .concat()
}

/// Every section's set, row by row, as the code writes each (`model::fields::set`).
fn rows() -> Vec<Row> {
    use Fields as X;
    let marks = X::DIRTY | X::STAGED | X::ACTION | X::EXT;
    let reattach = || vec![moved(F, (A, B), "f", None)];
    vec![
        row(
            Section::Grab,
            FREE,
            X::ALL & !(X::DISCARDED | X::CLEAR_PENDING),
        )
        .prior(section(
            1,
            Section::Grab,
            vec![Effect::Grab {
                node: FREE,
                instance: INSTANCE,
            }],
        )),
        row(Section::Publish, FREE, X::NONE)
            .prior(in_flight(1, A, "n", Kind::File))
            .before(vec![Effect::Publish {
                node: FREE,
                parent: A,
                name_hash: hash_string("n"),
                name: Name::new(b"n"),
                kind: Kind::File,
                check: Check::Live,
                expected: Expected::Nothing,
                instance: INSTANCE,
            }]),
        row(Section::BuildAdd, NEXT, X::NONE)
            .prior(build())
            .before(vec![Effect::BuildAdd {
                node: NEXT,
                parent: FREE,
                name_hash: hash_string("n"),
                name: Name::new(b"n"),
                kind: Kind::File,
            }]),
        row(Section::Abandon, FREE, X::NONE)
            .prior(build())
            .before(vec![Effect::Abandon { root: FREE }]),
        row(Section::Detach, F, X::NONE).before(vec![unlink(F, A, How::Always)]),
        row(Section::DiscardMarks, F, X::NONE).before(vec![unlink(F, A, How::Discard)]),
        row(Section::Release, F, X::NONE),
        row(Section::DiscardFree, F, X::NONE),
        row(Section::Allocator, F, X::NONE),
        row(Section::Unlabelled, F, X::NONE),
        row(
            Section::ServedMark,
            A,
            X::DIRTY | X::ACTION | X::CLEAR_PENDING,
        )
        .served(),
        row(Section::UnservedMark, A, marks | X::MERGE),
        row(
            Section::Reattach,
            F,
            X::DIRTY | X::ACTION | X::CLEAR_PENDING,
        )
        .served()
        .before(reattach()),
        row(Section::Reattach, B, X::NONE)
            .served()
            .before(reattach()),
        row(Section::Reattach, F, X::NONE).before(reattach()),
        row(Section::Reattach, H, X::NONE)
            .served()
            .before(vec![moved(F, (A, B), "h", Some(H))]),
        row(Section::MoveNamed, F, marks | X::MERGE).before(reattach()),
        row(Section::MoveNamed, A, X::NONE).before(reattach()),
        row(Section::RenameInPlace, F, marks | X::MERGE).before(vec![Effect::Rename {
            node: F,
            parent: A,
            name_hash: hash_string("x"),
            name: Name::new(b"x"),
            instance: INSTANCE,
        }]),
        row(Section::RenameInPlace, F, marks | X::MERGE),
        row(Section::SettlePhase1, A, X::ACTION | X::CLEAR_PENDING),
        row(
            Section::SettlePhase3,
            A,
            X::CLEAR_PENDING | X::DIRTY | X::ACTION | X::EXT | X::ADDRESS | X::SIZE,
        ),
        row(Section::UnservedClear, F, marks | X::MERGE),
        row(Section::ClearPropagation, A, marks | X::MERGE),
        row(
            Section::Rewrite,
            F,
            X::CONTENT | X::DIRTY | X::ACTION | X::EXT,
        ),
        row(Section::StagedModify, F, X::CONTENT | marks),
        row(Section::Restore, F, X::CONTENT | marks | X::MERGE),
        row(Section::Restore, L, X::CONTENT | marks | X::MERGE),
        row(Section::DeleteLevel, F, marks),
        row(Section::InPlace, F, X::CONTENT | marks | X::MERGE),
        row(Section::InPlace, L, X::SIZE | X::MODE | marks | X::MERGE),
        row(Section::Owned, F, X::CONTENT | marks | X::MERGE),
        row(Section::Repin, L, X::ADDRESS).before(vec![Effect::Repin {
            node: L,
            child: PINNED,
            whole: true,
        }]),
        row(Section::Save, ROOT_NODE, X::ADDRESS | X::SIZE),
    ]
}

/// The model of `row`'s State with its records before its `Word` replayed.
fn before(row: &Row) -> (Model, Vec<Record>) {
    let started: Snapshot = if row.served { served() } else { snapshot() };
    let mut records = row.prior.clone();
    records.extend(section(AT, row.section, row.before.clone()));
    (replayed(&started, &records), records)
}

#[test]
fn each_section_changes_its_set_alone() {
    for row in rows() {
        let (model, records) = before(&row);
        clean(&model);
        let slot = model
            .state(STATE)
            .and_then(|state| state.slot(row.node))
            .expect("the row's node");
        let (record, pending) = (slot.record, slot.clear_pending);
        for field in Fields::each().filter(|field| Fields::compared(&record).contains(*field)) {
            let (mut altered, mut altered_pending) = (record, pending);
            field.alter(&mut altered, &mut altered_pending);
            let mut written = records.clone();
            written.push(Record {
                seq: AT,
                sub: row.before.len() as u32,
                section: row.section,
                state: STATE,
                task: Some(0),
                effect: word(row.node, altered, altered_pending),
            });
            let started = if row.served { served() } else { snapshot() };
            let model = replayed(&started, &written);
            let at = (STATE, AT, row.before.len() as u32);
            // The write set's finding alone: an altered word may break a rule of its section's
            // decision too.
            let found = model
                .violations()
                .iter()
                .any(|violation| violation.at() == at && violation.is_write_set());
            assert_eq!(
                found,
                !row.fields.contains(field),
                "{:?} writing {field:?} of {} ({}): {:#?}",
                row.section,
                row.node,
                if row.served { "served" } else { "unserved" },
                model.violations()
            );
        }
    }
}

#[test]
fn a_mark_changes_merge_bits_only_to_merge_state() {
    let staged_merge = NodeFlags::StagedMerge.bits();
    let mut started = snapshot();
    started.blocks[0].records[F as usize].flags |= staged_merge;
    let file = NodeFlags::File.bits();
    for marking in [
        Section::UnservedMark,
        Section::MoveNamed,
        Section::RenameInPlace,
    ] {
        let marked = |flags: u16| {
            let mut record = started.blocks[0].records[F as usize];
            record.flags = flags;
            section(1, marking, vec![word(F, record, false)])
        };
        caught(
            &replayed(&started, &marked(file | NodeFlags::Staged.bits())),
            1,
            "merge",
        );
        clean(&replayed(
            &started,
            &marked(file | NodeFlags::StagedMergeConflict.bits()),
        ));
    }
}

/// A `Word` altered back to the words before it changes nothing, and is caught for nothing: the
/// judgement is against the model's words before the `Word`, not the `Word` as written.
#[test]
fn a_word_altered_back_to_the_words_before_it_is_not_caught() {
    let was = snapshot().blocks[0].records[F as usize];
    let judged = Judged {
        set: set(Section::UnservedMark, Role::Any, false, false),
        was,
        was_pending: false,
    };
    let merged = Node {
        flags: was.flags | NodeFlags::StagedMerge.bits(),
        ..was
    };
    assert!(!judged.catches(&merged, false), "a mark sets merge state");
    let (mut altered, mut pending) = (merged, false);
    Fields::MERGE.alter(&mut altered, &mut pending);
    assert_eq!(altered.flags & NodeFlags::MergeBits.bits(), 0);
    assert!(
        !judged.catches(&altered, pending),
        "altered back to no merge state, it changes nothing"
    );
    let rewritten = Judged {
        set: set(Section::Rewrite, Role::Any, false, false),
        ..judged
    };
    assert!(
        rewritten.catches(&merged, false),
        "a rewrite sets no merge bit"
    );
}

#[test]
fn a_repins_word_carries_its_records_child() {
    let mut pinned = snapshot().blocks[0].records[L as usize];
    pinned.child = PINNED + 1;
    let records = section(
        1,
        Section::Repin,
        vec![
            Effect::Repin {
                node: L,
                child: PINNED,
                whole: true,
            },
            word(L, pinned, false),
        ],
    );
    caught(&replayed(&snapshot(), &records), 1, "child");
}

#[test]
fn a_tests_own_edit_is_admitted_where_the_model_takes_it() {
    let edited = Node {
        flags: NodeFlags::File.bits() | NodeFlags::Dirty.bits(),
        size: 9,
        ..snapshot().blocks[0].records[F as usize]
    };
    let records = section(1, Section::Unlabelled, vec![word(F, edited, false)]);
    caught(&replayed(&snapshot(), &records), 1, "Unlabelled");
    let mut model = Model::new();
    model.start(STATE, &snapshot());
    model.admit_unlabelled();
    model.replay(&records);
    clean(&model);
}
