// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model (`crate::harness::model`): each replay rule on records made by hand over a State
//! made by hand, each section's write set on a `Word` changing each field, each difference the
//! compare names, real runs of the sections the log tests leave unwritten, and mixes on threads
//! replayed against the State they ran on. Every log test's run is replayed too
//! (`log::check_model`).

mod calls;
mod checkpoint;
mod collected;
mod compare;
mod excuses;
mod fields;
mod has;
mod links;
mod lookups;
mod marks;
mod names;
mod orphans;
mod outcomes;
mod races;
mod refusals;
mod replay;
mod runs;
mod sections;
mod successes;
mod walks;

use lore_base::types::Hash;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Check;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_revision::state::node::hooks::log::Words;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Filter;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Over;
use crate::harness::catalogue::call::Repeats;
use crate::harness::catalogue::call::Walk;
use crate::harness::catalogue::call::Yielded;
use crate::harness::model::BlockSnapshot;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;

/// The id of the State the records made by hand name.
const STATE: u64 = 7;

/// The instance of block 0 in the State made by hand.
const INSTANCE: u64 = 3;

/// The slots of the State made by hand, block 0's: the root, `a/` holding `f` and `g`, `b/`
/// holding `h`, and the link `l`; then a free slot, past them the block's next.
const A: NodeID = 1;
const F: NodeID = 2;
const G: NodeID = 3;
const B: NodeID = 4;
const H: NodeID = 5;
const L: NodeID = 6;
const FREE: NodeID = 7;
const NEXT: NodeID = 8;

/// The node `l` exposes in its linked State.
const PINNED: NodeID = 40;

/// The name of each slot the State made by hand holds.
fn name_of(node: NodeID) -> &'static str {
    ["", "a", "f", "g", "b", "h", "l"][node as usize]
}

/// The State made by hand: block 0 as [`A`] to [`L`] name it, [`FREE`] on its free list.
fn snapshot() -> Snapshot {
    let mut block = BlockSnapshot::empty(INSTANCE, NEXT);
    let made: [(NodeID, NodeID, Kind); 6] = [
        (A, 0, Kind::Directory),
        (F, A, Kind::File),
        (G, A, Kind::File),
        (B, 0, Kind::Directory),
        (H, B, Kind::File),
        (L, 0, Kind::Link),
    ];
    for (node, parent, kind) in made {
        let mut record = record(parent, name_of(node), kind);
        if kind == Kind::Link {
            record.child = PINNED;
        }
        // Each put at the head of its parent's chain, as a publish puts it.
        record.sibling = block.records[parent as usize].child;
        block.records[parent as usize].child = node;
        block.records[node as usize] = record;
        block.names[node as usize] = Some(Name::new(name_of(node).as_bytes()));
    }
    block.records[FREE as usize] = Node {
        flags: NodeFlags::Discarded.bits(),
        sibling: INVALID_NODE,
        ..Node::default()
    };
    block.free = vec![FREE];
    Snapshot {
        blocks: vec![block],
        ..Snapshot::default()
    }
}

/// [`snapshot`] served.
fn served() -> Snapshot {
    Snapshot {
        served: true,
        ..snapshot()
    }
}

/// [`snapshot`] with each node's flags as `changed` adds them, served where `served`.
fn with(changed: &[(NodeID, u16)], served: bool) -> Snapshot {
    let mut made = snapshot();
    made.served = served;
    for &(node, flags) in changed {
        made.blocks[0].records[node as usize].flags |= flags;
    }
    made
}

/// `made` with `node` dirty.
fn dirty(mut made: Snapshot, node: NodeID) -> Snapshot {
    made.blocks[0].records[node as usize].flags |= NodeFlags::Dirty.bits();
    made
}

/// A node's record named `name` below `parent`, of `kind`, as an initialization writes it.
fn record(parent: NodeID, name: &str, kind: Kind) -> Node {
    Node {
        flags: match kind {
            Kind::File => NodeFlags::File.bits(),
            Kind::Link => NodeFlags::Link.bits(),
            Kind::Directory => 0,
        },
        parent,
        name_hash: hash_string(name),
        ..Node::default()
    }
}

/// The model of the State made by hand, started from `snapshot`, with `records` replayed.
fn replayed(snapshot: &Snapshot, records: &[Record]) -> Model {
    let mut model = Model::new();
    model.start(STATE, snapshot);
    model.replay(records);
    model
}

/// A call of `kind`, id 1, by task 0 over `[begin, end)` on the State made by hand, on no node,
/// answering `outcome`, noting nothing: the call each test's calls change.
fn base_call(
    kind: &'static crate::harness::catalogue::Kind,
    (begin, end): (u64, u64),
    outcome: crate::harness::catalogue::call::Outcome,
) -> Call {
    Call {
        kind,
        sweeping: None,
        id: 1,
        task: Some(0),
        state: STATE,
        target: None,
        destination: None,
        name: None,
        spelling: None,
        holder: None,
        place: None,
        listing: None,
        prepends: 0,
        taken: Vec::new(),
        leaves: None,
        lookups: Vec::new(),
        walks: Vec::new(),
        paths: Vec::new(),
        names: Vec::new(),
        failed_reads: 0,
        begin,
        end,
        windows: Vec::new(),
        outcome,
    }
}

/// A call of `kind` by task 0 over `[begin, end)` on `dir`, walking what `over` names from its
/// begin, yielding `yielded`, reporting `repeats` and keeping by `filter`.
fn walk_of(
    kind: &'static crate::harness::catalogue::Kind,
    (dir, over): (NodeID, Over),
    (begin, end): (u64, u64),
    yielded: Vec<Yielded>,
    (repeats, filter): (Repeats, Filter),
) -> Call {
    Call {
        target: Some(dir),
        walks: vec![Walk {
            dir,
            from: begin,
            yielded,
            repeats,
            skipped: 0,
            filter,
            over,
        }],
        ..base_call(kind, (begin, end), Outcome::Answer(Answer::None))
    }
}

/// [`replayed_with`], under the gate, where a section's `seq` and its count's bump fall in one
/// step, as in the scheduler's runs.
fn gated(made: &Snapshot, records: &[Record], calls: Vec<Call>) -> Model {
    gated_providing(made, Vec::new(), records, calls)
}

/// [`gated`], with the snapshot of each revision of `revisions` provided ([`replayed_providing`]).
fn gated_providing(
    made: &Snapshot,
    revisions: Vec<(Hash, Snapshot)>,
    records: &[Record],
    calls: Vec<Call>,
) -> Model {
    let mut model = Model::new();
    model.start(STATE, made);
    model.admit_unlabelled();
    model.gated();
    for (revision, snapshot) in revisions {
        model.provide(revision, snapshot);
    }
    model.replay_called(records, calls, Vec::new());
    model
}

/// A reader's read of `node`'s name as `name` at `seq`, by task 0.
fn name_read(seq: u64, node: NodeID, name: &str) -> Vec<Record> {
    section(
        seq,
        Section::Unlabelled,
        vec![Effect::Named {
            node,
            name: Name::new(name.as_bytes()),
        }],
    )
}

/// The model of `made` with `records` replayed with `calls`, a test's own edits admitted.
fn replayed_with(made: &Snapshot, records: &[Record], calls: Vec<Call>) -> Model {
    replayed_providing(made, Vec::new(), records, calls)
}

/// [`replayed_with`], with the snapshot of each revision of `revisions` provided, as the loads of
/// them in a run would have them provided.
fn replayed_providing(
    made: &Snapshot,
    revisions: Vec<(Hash, Snapshot)>,
    records: &[Record],
    calls: Vec<Call>,
) -> Model {
    let mut model = Model::new();
    model.start(STATE, made);
    model.admit_unlabelled();
    for (revision, snapshot) in revisions {
        model.provide(revision, snapshot);
    }
    model.replay_called(records, calls, Vec::new());
    model
}

/// `f` moved from `a` into `b` at `seq`, by another task.
fn moved_out(seq: u64) -> Vec<Record> {
    by(
        1,
        section(seq, Section::MoveNamed, vec![moved(F, (A, B), "f", None)]),
    )
}

/// A rename's record of `node` under `parent` as `name`, at `seq`, by task 0.
fn renamed(seq: u64, node: NodeID, parent: NodeID, name: &str) -> Vec<Record> {
    section(
        seq,
        Section::RenameInPlace,
        vec![Effect::Rename {
            node,
            parent,
            name_hash: hash_string(name),
            name: Name::new(name.as_bytes()),
            instance: INSTANCE,
        }],
    )
}

/// `f`'s words in `made` given `flags` on top in place at `seq`, by a test's own edit.
fn flagging(made: &Snapshot, seq: u64, flags: NodeFlags) -> Vec<Record> {
    flagging_of(made, seq, F, flags)
}

/// `node`'s words in `made` given `flags` on top in place at `seq`, by a test's own edit.
fn flagging_of(made: &Snapshot, seq: u64, node: NodeID, flags: NodeFlags) -> Vec<Record> {
    rewritten_of(node, seq, flagged_of(made, node, flags))
}

/// `f`'s words in `made` marked `Discarded` in place at `seq`, by a test's own edit.
fn discarding(made: &Snapshot, seq: u64) -> Vec<Record> {
    flagging(made, seq, NodeFlags::Discarded)
}

/// `f`'s words rewritten to `record` at `seq` by another task, a test's own edit.
fn rewritten(seq: u64, record: Node) -> Vec<Record> {
    rewritten_of(F, seq, record)
}

/// `node`'s words rewritten to `record` at `seq` by another task, a test's own edit.
fn rewritten_of(node: NodeID, seq: u64, record: Node) -> Vec<Record> {
    by(
        1,
        section(seq, Section::Unlabelled, vec![word(node, record, false)]),
    )
}

/// `f`'s words in `made` with `flags` set.
fn flagged(made: &Snapshot, flags: NodeFlags) -> Node {
    flagged_of(made, F, flags)
}

/// `node`'s words in `made` with `flags` set.
fn flagged_of(made: &Snapshot, node: NodeID, flags: NodeFlags) -> Node {
    let mut record = made.blocks[0].records[node as usize];
    record.flags |= flags.bits();
    record
}

/// `records`, made by `task`.
fn by(task: usize, records: Vec<Record>) -> Vec<Record> {
    records
        .into_iter()
        .map(|record| Record {
            task: Some(task),
            ..record
        })
        .collect()
}

/// The records of one section at `seq`, labelled `section`, numbered in order.
fn section(seq: u64, section: Section, effects: Vec<Effect>) -> Vec<Record> {
    effects
        .into_iter()
        .enumerate()
        .map(|(sub, effect)| Record {
            seq,
            sub: sub as u32,
            section,
            state: STATE,
            task: Some(0),
            effect,
        })
        .collect()
}

/// A `Word` of `node` holding `record`, its pending clear bit `clear_pending`.
fn word(node: NodeID, record: Node, clear_pending: bool) -> Effect {
    Effect::Word {
        node,
        record: Words(record),
        clear_pending,
    }
}

/// The model's record of `node`.
fn record_of(model: &Model, node: NodeID) -> Node {
    model
        .state(STATE)
        .and_then(|state| state.slot(node))
        .map(|slot| slot.record)
        .expect("the model holds the node")
}

/// The violations `model` found, as text.
fn found(model: &Model) -> Vec<String> {
    model.violations().iter().map(ToString::to_string).collect()
}

/// Asserts `model` found no violation.
fn clean(model: &Model) {
    assert!(
        model.violations().is_empty(),
        "no violation: {:#?}",
        found(model)
    );
}

/// Asserts `model` found one violation at least, at `seq`, saying `what`.
fn caught(model: &Model, seq: u64, what: &str) {
    assert!(
        model
            .violations()
            .iter()
            .any(|violation| violation.seq == seq && violation.what.contains(what)),
        "a violation at seq {seq} saying '{what}': {:#?}",
        found(model)
    );
}

fn grab(node: NodeID) -> Effect {
    Effect::Grab {
        node,
        instance: INSTANCE,
    }
}

fn free(node: NodeID) -> Effect {
    Effect::Free {
        node,
        instance: INSTANCE,
    }
}

fn publish(node: NodeID, parent: NodeID, name: &str, kind: Kind) -> Effect {
    Effect::Publish {
        node,
        parent,
        name_hash: hash_string(name),
        name: Name::new(name.as_bytes()),
        kind,
        check: Check::Live,
        expected: Expected::Nothing,
        instance: INSTANCE,
    }
}

fn unlink(node: NodeID, parent: NodeID, how: How) -> Effect {
    Effect::Detach {
        node,
        parent,
        how,
        expected: Expected::Nothing,
        instance: INSTANCE,
    }
}

fn moved(
    node: NodeID,
    (from, to): (NodeID, NodeID),
    name: &str,
    replaced: Option<NodeID>,
) -> Effect {
    Effect::Move {
        node,
        from,
        to,
        name_hash: hash_string(name),
        name: Some(Name::new(name.as_bytes())),
        replaced,
        check: Check::Unchecked,
        expected: Expected::Nothing,
        instances: (INSTANCE, INSTANCE),
    }
}

/// The detach of `node` from `parent`, in a section at `seq`.
fn detached(seq: u64, node: NodeID, parent: NodeID) -> Vec<Record> {
    section(
        seq,
        Section::Detach,
        vec![unlink(node, parent, How::Always)],
    )
}

/// A guard's `event`, in a section of no label at `seq`.
fn guard(seq: u64, event: GuardEvent) -> Vec<Record> {
    section(seq, Section::Unlabelled, vec![Effect::Guard { event }])
}

/// A check's expected count of `changes`, in block 0's instance.
fn count(changes: u32) -> Expected {
    Expected::Count {
        changes,
        instance: INSTANCE,
    }
}

/// `record` as its initialization leaves it: `Discarded` until its publish.
fn initialized(record: Node) -> Node {
    Node {
        flags: record.flags | NodeFlags::Discarded.bits(),
        ..record
    }
}

/// An attach's records from `seq` on: the free slot grabbed, initialized as `name` below
/// `parent`, and published there.
fn attach(seq: u64, parent: NodeID, name: &str) -> Vec<Record> {
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
                publish(FREE, parent, name, Kind::File),
                word(FREE, made, false),
            ],
        ),
    ]
    .concat()
}

/// A read of `dir`'s head at `seq`, its count `changes`, by task 0.
fn head_of(dir: NodeID, seq: u64, changes: u32) -> Vec<Record> {
    section(
        seq,
        Section::Unlabelled,
        vec![Effect::Head {
            node: dir,
            changes,
            instance: INSTANCE,
        }],
    )
}

/// The walk's read of `a/`'s head at `seq`, its count `changes`, by task 0.
fn head(seq: u64, changes: u32) -> Vec<Record> {
    head_of(A, seq, changes)
}

/// `f` repositioned in `a/` at `seq` by another task, a `Move` within its directory, named `name`.
fn repositioned(seq: u64, name: &str) -> Vec<Record> {
    by(
        1,
        section(seq, Section::Reattach, vec![moved(F, (A, A), name, None)]),
    )
}

/// `n`, its slot [`FREE`], published into `a/` by another task: its grab at `seq`, its publish at
/// `seq + 2`.
fn published(seq: u64) -> Vec<Record> {
    by(1, attach(seq, A, "n"))
}
