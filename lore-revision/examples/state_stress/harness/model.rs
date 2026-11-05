// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model: what the log says each State holds, started from the State read where a setup ends
//! ([`Snapshot`]), or at the State's `Created` record, and advanced by the log alone, in
//! `(state, seq, sub)` order, each record checked where it stands.
//!
//! A `Word` is held to its section's write set ([`fields`]) and recorded, not computed: whether a
//! mark set the right bits on the right nodes is the invariants'. [`StateModel::compare`] holds the
//! model against the live State at a quiescent point.

mod calls;
pub mod checkpoint;
mod committed;
mod compare;
mod excuses;
pub mod fields;
mod orphans;
mod outcomes;
mod refusals;
mod snapshot;
mod successes;
mod sweeps;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

pub use committed::Committed;
use lore_base::types::Hash;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeLink;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::State;
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
pub use orphans::Orphans;
pub use snapshot::BlockSnapshot;
pub use snapshot::Snapshot;

use self::calls::Calls;
use self::calls::Held;
use self::calls::Permit;
use self::fields::Fields;
use self::fields::Judged;
use self::fields::Role;
use self::fields::Set;
use self::successes::MovedBits;
use crate::harness::catalogue::Tier;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::SelfReported;
use crate::harness::catalogue::call::Spawn;
use crate::harness::fixtures::Tree;

/// Where a slot stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// On its block's free list, or past its `node_count`.
    Free,
    /// Grabbed and in no chain yet: an attach's slot before its publish, a build's root.
    InFlight,
    /// In its parent's chain.
    Chained,
    /// Out of every chain, Discarded.
    Unchained(Unchained),
}

/// How a slot came out of every chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unchained {
    /// Taken out by a detach, or by a discard's unlink.
    Detached(How),
    /// A reattach's replaced holder.
    Replaced,
    /// A node of a build its abandon cut loose, awaiting its free.
    Abandoned,
    /// In the chain of a directory freed before it, as a discard failing part way leaves one.
    Orphaned,
    /// Found so where the model started.
    AtStart,
}

/// A slot of the model.
#[derive(Clone, Debug)]
pub struct Slot {
    /// Its record as last written: its words from its `Word`s and its slot's rules, its parent
    /// and name hash from the structure. A directory's `child` and every `sibling` are the
    /// chains'.
    pub record: Node,
    pub clear_pending: bool,
    /// The chain count of a directory in this slot: the slot's, kept across a free and a grab,
    /// restarted by a load.
    pub changes: u32,
    pub name: Option<Name>,
    pub status: Status,
    /// The directory whose chain holds it: a freed slot stays in the chain of a detached
    /// directory not freed yet, as the directory's links do.
    pub chained_in: Option<NodeID>,
    /// Published since its grab, itself or with its build's root.
    pub published: bool,
    /// Its parent and name hash known to the model: read at the start, told by its
    /// initialization's `Word`, or adopted at its publish or its add into a build. A directory
    /// with no content makes no `Word` at its initialization, so until then, and for good where
    /// its build is abandoned, only the State knows them.
    pub placed: bool,
    /// The root of the unpublished build it was added into.
    pub build: Option<NodeID>,
    /// The `seq` it last joined a chain at: its publish, its build root's for a build's member, or
    /// a move. A node that joins a detached root's subtree after the detach is an orphan.
    pub joined_at: Option<u64>,
    /// The `seq` a `Move` within its directory last repositioned it at, which a walk under way may
    /// pass over, as the children iterators' rules allow (`StateNodeChildrenIterator`).
    pub repositioned_at: Option<u64>,
    /// The `seq` a rename in place last respelled it at, which a collection of dirty paths, reading
    /// its name and a child's at reads of their own, may spell across (`collect_dirty_paths`).
    pub renamed_at: Option<u64>,
    /// The `seq` a `Word` last wrote its words at, which a collection of dirty paths, reading a
    /// child's words before its name, may have read before (`collect_dirty_paths`).
    pub written_at: Option<u64>,
    /// Its detach, or the move that replaced it, where one took it out of every chain.
    pub detached: Option<Detachment>,
    /// A directory whose chain the State may hold otherwise: a free reached a node of it since
    /// its detach, the State's chain still pointing at the slot, or the start found it stale.
    pub chain_freed: bool,
}

/// What took a root out of every chain, for its orphans ([`Orphans`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detachment {
    /// The detach's `seq`, or the replacing move's.
    pub seq: u64,
    /// Whether the State was served then.
    pub served: bool,
    /// For a replaced holder, whether it held nodes at the move: a child made in it inside a
    /// rename's window ([`Orphans`]) explains a publish below it after.
    pub held: bool,
}

impl Slot {
    fn free(record: Node) -> Self {
        Self {
            record,
            clear_pending: false,
            changes: 0,
            name: None,
            status: Status::Free,
            chained_in: None,
            published: false,
            placed: false,
            build: None,
            joined_at: None,
            repositioned_at: None,
            renamed_at: None,
            written_at: None,
            detached: None,
            chain_freed: false,
        }
    }

    fn is_discarded(&self) -> bool {
        self.record.flags & NodeFlags::Discarded.bits() != 0
    }

    fn kind(&self) -> Kind {
        Kind::of(&self.record)
    }
}

/// A block of the model.
#[derive(Clone, Debug)]
pub struct BlockModel {
    /// The number its install gave it: 0 where it was not in memory where the model started and
    /// no install came since.
    pub instance: u64,
    pub node_count: u32,
    /// The free list, its head last.
    pub free: Vec<u32>,
    pub slots: Vec<Slot>,
}

impl BlockModel {
    /// A block holding no node, `node_count` 1 for block 0, whose slot 0 is the root.
    fn empty(index: usize, instance: u64) -> Self {
        let node_count = u32::from(index == 0);
        let mut slots: Vec<Slot> = (0..BLOCK_NODE_COUNT)
            .map(|_| Slot::free(Node::default()))
            .collect();
        if index == 0 {
            slots[0].status = Status::Chained;
            slots[0].published = true;
            slots[0].placed = true;
        }
        Self {
            instance,
            node_count,
            free: Vec::new(),
            slots,
        }
    }
}

/// A State of the model.
#[derive(Clone, Debug)]
pub struct StateModel {
    pub id: u64,
    pub served: bool,
    pub unfinished: bool,
    /// Whether the State held an unfinished change as the model started, which no record of the
    /// run explains.
    pub unfinished_at_start: bool,
    /// Whether a record wrote it since its start or its last checkpoint: a checkpoint saves only
    /// a State a run changed.
    pub written: bool,
    /// Whether a record wrote it since its start: a checkpoint walks no State none changed, which
    /// holds what its start read.
    pub changed: bool,
    /// Its blocks by index, `None` for one the model has no image of.
    pub blocks: Vec<Option<BlockModel>>,
    /// Each directory's chain, its head first.
    chains: BTreeMap<NodeID, VecDeque<NodeID>>,
    /// Each task's `UnfinishedChange` guards armed and not yet finished or dropped: a guard is
    /// armed, finished and dropped in its operation's own future, whose task the scheduler names.
    armed: BTreeMap<Option<usize>, u32>,
    /// Each directory whose settle's first phase set its pending clear, with its count then.
    settles: BTreeMap<(NodeID, Option<usize>), u32>,
    /// Its moves permit, where taken.
    permit: Option<Permit>,
    /// Each node's moves to another directory since its grab: each one's `seq` and the directory
    /// it left, for the parents a node had over a call (H8).
    moved_from: BTreeMap<NodeID, Vec<(u64, NodeID)>>,
    /// Each node's last `seq` holding its base marks, where a `Word` or a free took them since: a
    /// climb read before then may have stopped at it (H8).
    base_marks_until: BTreeMap<NodeID, u64>,
    /// Each link's changes of its pin since its grab: each one's `seq` and the pin it replaced, for
    /// the pins a reader of its words may have read since a `seq` ([`Self::pins_since`]).
    pins_before: BTreeMap<NodeID, Vec<(u64, NodeLink)>>,
    /// The breaches of closure a dropped or failed call's records formed, as (dirty node, clean
    /// directory) pairs, excused while they stand (C12).
    pub excused: BTreeSet<(NodeID, NodeID)>,
    /// The nodes along a swap's changed paths since its apply, which no swap's settle runs in the
    /// harness to end: a clean directory among them may hold children its committed directory
    /// does not (C11c).
    pub swapped: BTreeSet<NodeID>,
    /// Whether a record changed a node of the State, served, in a replay given no calls to tell a
    /// swap's records apart, so its swaps' changed paths are unknown.
    pub swaps_unknown: bool,
    /// The roots the C API delete queued pending and no free reached since.
    pending: BTreeSet<NodeID>,
    /// The plan of each sweep whose call the replay stands in, by the call's id ([`sweeps`]).
    sweeps: BTreeMap<u64, sweeps::SweepPlan>,
}

/// A record the model could not apply, or one its rules refuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub state: u64,
    pub seq: u64,
    pub sub: u32,
    pub what: String,
    /// Whether it is a `Word`'s write outside its section's set ([`Replay::word`]).
    pub write_set: bool,
}

impl Violation {
    /// The `(state, seq, sub)` of the record it was found at.
    pub fn at(&self) -> (u64, u64, u32) {
        (self.state, self.seq, self.sub)
    }

    /// Whether it is a `Word`'s write outside its section's set ([`Replay::word`]).
    pub fn is_write_set(&self) -> bool {
        self.write_set
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "State {} at seq {} sub {}: {}",
            self.state, self.seq, self.sub, self.what
        )
    }
}

/// The model of a run's States.
#[derive(Clone, Debug, Default)]
pub struct Model {
    states: BTreeMap<u64, StateModel>,
    /// Snapshots of the States a run loads, by revision: a revision's signature names its
    /// content, whichever repository's context loads it.
    sources: BTreeMap<Hash, Snapshot>,
    /// States records came for with no model started, each reported once.
    unknown: BTreeSet<u64>,
    violations: Vec<Violation>,
    /// The section being replayed, `(state, seq)`, and the nodes its structural records name,
    /// each in its role.
    section: Option<(u64, u64)>,
    roles: Vec<(NodeID, Role)>,
    /// The mover a served move of the section moved, with the action and dirty bits its word
    /// must hold once the section ends (H20).
    moved_bits: Option<MovedBits>,
    /// The node the section admitted a change of ([`Effect::Admitted`]).
    admitted: Option<NodeID>,
    /// How each `Word` was judged, by its `(state, seq, sub)`, where kept ([`Self::judging`]).
    judged: Option<BTreeMap<(u64, u64, u32), Judged>>,
    /// Whether a `Word` of no labelled section is a test's own edit (`node_edit_for_tests`).
    admit_unlabelled: bool,
    /// The calls of the records being replayed, where given ([`Self::replay_called`]).
    calls: Option<Calls>,
    /// What the runs replayed admit of a call's outcome beyond the State's answers.
    admits: Admits,
    /// The self-reported outcomes of the calls replayed, counted by what they reported.
    self_reported: BTreeMap<SelfReported, usize>,
    /// Whether the breaches of closure a dropped call leaves stay breaches, as a scenario holding
    /// drops to closure says ([`Self::holding_drops_to_closure`]).
    holding_drops: bool,
}

/// What the runs a model replays admit of a call's outcome beyond the State's answers.
#[derive(Clone, Copy, Debug, Default)]
struct Admits {
    /// The self-reported outcomes they admit, each counted; any other is a violation.
    self_reports: &'static [SelfReported],
    /// Whether they run under the gate, where a section's `seq` and its count's bump fall in one
    /// step, so a count read lock-free is the model's at the reader's own `seq`: as its call
    /// began, for a read with no step before it, and no less than that for one later in the call.
    gated: bool,
    /// Whether they drop calls of kinds their production callers never drop
    /// ([`crate::harness::catalogue::Kind::cancellable`]), as a scenario showing what the model
    /// makes of such a drop does; elsewhere each is a violation.
    uncancellable_drops: bool,
}

impl Model {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the model of State `id` from `snapshot`.
    pub fn start(&mut self, id: u64, snapshot: &Snapshot) {
        self.states.insert(id, StateModel::start(id, snapshot));
    }

    /// Takes a `Word` of no labelled section for a test's own edit, admitting any field.
    pub fn admit_unlabelled(&mut self) {
        self.admit_unlabelled = true;
    }

    /// Takes the runs replayed from here to run under the gate ([`Admits::gated`]).
    pub fn gated(&mut self) {
        self.admits.gated = true;
    }

    /// Holds what a dropped call leaves to closure, excusing none of its breaches.
    pub fn holding_drops_to_closure(&mut self) {
        self.holding_drops = true;
    }

    /// Takes the runs replayed from here to drop calls of kinds their production callers never
    /// drop ([`Admits::uncancellable_drops`]).
    pub fn admitting_uncancellable_drops(&mut self) {
        self.admits.uncancellable_drops = true;
    }

    /// Keeps how each `Word` is judged from here, for [`Self::judged`]: a test holding the model
    /// to its sets alters each `Word` against it.
    pub fn judging(&mut self) {
        self.judged.get_or_insert_default();
    }

    /// The States loaded from the store whose records `records` hold: each needs its snapshot
    /// ([`Self::provide`]) before the replay reaches its `Created`.
    pub fn needs(records: &[Record]) -> BTreeSet<(usize, Hash)> {
        records
            .iter()
            .filter_map(|record| match record.effect {
                Effect::Created { from: Some(from) } => Some(from),
                _ => None,
            })
            .collect()
    }

    /// The snapshot a State loaded from the revision `revision` starts from.
    pub fn provide(&mut self, revision: Hash, snapshot: Snapshot) {
        self.sources.insert(revision, snapshot);
    }

    /// A snapshot of every revision `records` load that has none yet, read from the repository
    /// among `context`'s that loaded it; for one a context made for the load alone loaded, as a
    /// read through a link makes one, from the context `context` loads the revision with.
    pub async fn provide_needed(&mut self, context: &impl Modelled, records: &[Record]) {
        let mut needs = Self::needs(records);
        needs.retain(|(_, revision)| !self.sources.contains_key(revision));
        if needs.is_empty() {
            return;
        }
        let repositories: Vec<Arc<RepositoryContext>> = context
            .states()
            .into_iter()
            .map(|(_, repository)| repository)
            .collect();
        let revisions = context.revisions();
        for (loader, revision) in needs {
            // Loads through several contexts may need one revision.
            if self.sources.contains_key(&revision) {
                continue;
            }
            let repository = (repositories.iter())
                .find(|repository| Arc::as_ptr(repository) as usize == loader)
                .or_else(|| {
                    (revisions.iter())
                        .find(|(loads, _)| *loads == revision)
                        .map(|(_, repository)| repository)
                });
            if let Some(repository) = repository {
                self.provide(revision, Snapshot::of_revision(repository, revision).await);
            }
        }
    }

    pub fn replay(&mut self, records: &[Record]) {
        for record in records {
            self.apply(record);
        }
        self.end_section();
    }

    /// `records` replayed with their run's `calls` and how each of its tasks came to be, for the
    /// rules that need a record's call, each resolution's hops noted as its lookups first. Records
    /// replayed after are judged without them, as a future a call spawned would hold a later record
    /// its task made.
    pub fn replay_called(&mut self, records: &[Record], mut calls: Vec<Call>, spawns: Vec<Spawn>) {
        outcomes::hops_looked_up(&mut calls, records, &spawns);
        self.calls = Some(Calls::new(calls, spawns));
        self.replay(records);
        self.end_calls();
        self.calls = None;
    }

    /// Takes the runs replayed from here to admit the self-reported outcomes `admitted`, as a
    /// lowered retry bound or a schedule no explorer bounds reaches them; under the product's
    /// bounds and an explorer's, each is a violation.
    pub fn admitting_self_reports(&mut self, admitted: &'static [SelfReported]) {
        self.admits.self_reports = admitted;
    }

    /// The self-reported outcomes of every call replayed so far, by what they reported.
    pub fn self_reported(&self) -> &BTreeMap<SelfReported, usize> {
        &self.self_reported
    }

    /// The section replayed ends: a served move's mover holds the bits it moved to take.
    fn end_section(&mut self) {
        self.admitted = None;
        let Some(moved) = self.moved_bits.take() else {
            return;
        };
        if let Some(what) = self
            .states
            .get(&moved.state)
            .and_then(|state| moved.unheld(state))
        {
            self.violations.push(Violation {
                state: moved.state,
                seq: moved.seq,
                sub: moved.sub,
                what,
                write_set: false,
            });
        }
    }

    pub fn violations(&self) -> &[Violation] {
        &self.violations
    }

    pub fn state(&self, id: u64) -> Option<&StateModel> {
        self.states.get(&id)
    }

    /// How the `Word` at `(state, seq, sub)` was judged, where kept.
    pub fn judged(&self, at: (u64, u64, u32)) -> Option<Judged> {
        self.judged.as_ref()?.get(&at).copied()
    }

    /// Applies `record`, after every record before it in `(state, seq, sub)` order.
    fn apply(&mut self, record: &Record) {
        self.reach((record.state, record.seq));
        if self.section != Some((record.state, record.seq)) {
            self.end_section();
            self.section = Some((record.state, record.seq));
            self.roles.clear();
        }
        if let Effect::Created { from } = record.effect {
            self.created(record, from);
            return;
        }
        let Some(state) = self.states.get_mut(&record.state) else {
            if self.unknown.insert(record.state) {
                self.violations.push(violation(
                    record,
                    "a record of a State the model never started".into(),
                ));
            }
            return;
        };
        let called = self.calls.is_some();
        let call = self.calls.as_mut().and_then(|calls| calls.holding(record));
        let part_way = call.as_ref().is_some_and(|held| match held.call.outcome {
            Outcome::Dropped => !self.holding_drops,
            Outcome::Failed(_) => true,
            _ => false,
        });
        let applying = committed::Applying::of(called, call.as_ref().map(|held| held.call));
        let sweeping = (call.as_ref())
            .filter(|held| sweeps::sweeps(held.call.kind))
            .map(|held| held.call.id);
        let shared = (call.as_ref()).is_some_and(|held| held.call.kind.tier == Tier::Shared);
        let mut replay = Replay {
            state,
            record,
            call,
            part_way,
            applying,
            sweeping,
            shared,
            roles: &mut self.roles,
            moved_bits: &mut self.moved_bits,
            admitted: &mut self.admitted,
            judged: &mut self.judged,
            violations: &mut self.violations,
            admit_unlabelled: self.admit_unlabelled,
        };
        replay.apply();
    }

    /// A State made: empty, or from the snapshot of what it loaded.
    fn created(&mut self, record: &Record, from: Option<(usize, Hash)>) {
        if self.states.contains_key(&record.state) {
            self.violations
                .push(violation(record, "a State made twice".into()));
            return;
        }
        let started = match from {
            None => StateModel::start(record.state, &Snapshot::default()),
            Some(from) => {
                let Some(snapshot) = self.sources.get(&from.1) else {
                    self.unknown.insert(record.state);
                    self.violations.push(violation(
                        record,
                        format!("a State loaded from {from:?}, which no snapshot was provided of"),
                    ));
                    return;
                };
                StateModel::start(record.state, snapshot)
            }
        };
        self.states.insert(record.state, started);
    }
}

fn violation(record: &Record, what: String) -> Violation {
    Violation {
        state: record.state,
        seq: record.seq,
        sub: record.sub,
        what,
        write_set: false,
    }
}

/// The block of `node` and its slot there.
fn split(node: NodeID) -> (usize, usize) {
    (NodeBlock::index(node), node as usize % BLOCK_NODE_COUNT)
}

/// Whether `node` names a node: 0 ends a chain as the invalid id does, as no chain holds the root.
fn names_node(node: NodeID) -> bool {
    node != ROOT_NODE && node != INVALID_NODE
}

/// What a node hangs from, up its chains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Top {
    /// The root: the node is live.
    Root,
    /// The first node up that is in flight: a build's tree, or the node itself.
    InFlight(NodeID),
    /// The first node up that is in no chain.
    Unchained(NodeID),
    /// A free slot, or a node the model has no image of, or a loop.
    Lost,
}

impl StateModel {
    /// `node`'s pin changing at `seq` from `before`, once for a section, whose repin and `Word`
    /// both tell it.
    fn pin_changing(&mut self, node: NodeID, seq: u64, before: NodeLink) {
        let changes = self.pins_before.entry(node).or_default();
        if changes.last().is_none_or(|&(at, _)| at != seq) {
            changes.push((seq, before));
        }
    }

    /// The pins `node`, a link, held after `seq` but its current one: the one each change of its
    /// pin since replaced, oldest first.
    fn pins_since(&self, node: NodeID, seq: u64) -> impl Iterator<Item = NodeLink> + '_ {
        let changes = self.pins_before.get(&node).map_or(&[][..], Vec::as_slice);
        let since = changes.partition_point(|&(at, _)| at <= seq);
        changes[since..].iter().map(|&(_, pin)| pin)
    }

    /// The model of a State as `snapshot` holds it: the chains walked from every directory not
    /// free, a slot in one of them chained, every other used slot found unchained.
    fn start(id: u64, snapshot: &Snapshot) -> Self {
        let mut blocks: Vec<Option<BlockModel>> = Vec::with_capacity(snapshot.blocks.len());
        for read in &snapshot.blocks {
            let mut slots: Vec<Slot> = read
                .records
                .iter()
                .map(|record| Slot::free(*record))
                .collect();
            let used = read.used();
            for (index, slot) in slots.iter_mut().enumerate() {
                if used[index] {
                    slot.status = Status::Unchained(Unchained::AtStart);
                    slot.published = true;
                    slot.placed = true;
                    slot.name = read.names[index].clone();
                }
                slot.changes = read.changes[index];
                slot.clear_pending = read.clear_pending[index];
            }
            let mut free = read.free.clone();
            free.reverse();
            blocks.push(Some(BlockModel {
                instance: read.instance,
                node_count: read.node_count,
                free,
                slots,
            }));
        }
        let mut model = Self {
            id,
            served: snapshot.served,
            unfinished: snapshot.unfinished,
            unfinished_at_start: snapshot.unfinished,
            written: false,
            changed: false,
            blocks,
            chains: BTreeMap::new(),
            armed: BTreeMap::new(),
            settles: BTreeMap::new(),
            permit: None,
            moved_from: BTreeMap::new(),
            base_marks_until: BTreeMap::new(),
            pins_before: BTreeMap::new(),
            excused: BTreeSet::new(),
            swapped: BTreeSet::new(),
            swaps_unknown: false,
            pending: BTreeSet::new(),
            sweeps: BTreeMap::new(),
        };
        if let Some(root) = model.slot_mut(ROOT_NODE) {
            root.status = Status::Chained;
        }
        let directories: Vec<NodeID> = model
            .nodes()
            .filter(|&node| {
                model.slot(node).is_some_and(|slot| {
                    slot.status != Status::Free && slot.kind() == Kind::Directory
                })
            })
            .collect();
        for directory in directories {
            model.walk_chain(directory);
        }
        // A root detached before the start, how unknown, before every record.
        let served = model.served;
        for node in model.nodes().collect::<Vec<NodeID>>() {
            if let Some(slot) = model
                .slot_mut(node)
                .filter(|slot| slot.status == Status::Unchained(Unchained::AtStart))
            {
                slot.detached = Some(Detachment {
                    seq: 0,
                    served,
                    held: false,
                });
            }
        }
        model
    }

    /// Walks `directory`'s chain as the start finds it, claiming each slot no chain claimed yet
    /// whose parent is `directory`; a slot free, claimed already or another's ends the chain left
    /// stale, the directory marked so. A detached directory's chain run into a slot freed and
    /// taken again so stops there, whatever order the directories are walked in, as the slot's
    /// parent is now another directory.
    fn walk_chain(&mut self, directory: NodeID) {
        let mut chain = VecDeque::new();
        let mut at = self
            .slot(directory)
            .map_or(INVALID_NODE, |slot| slot.record.child);
        let mut stale = false;
        while names_node(at) {
            let Some(slot) = self.slot_mut(at) else {
                stale = true;
                break;
            };
            if slot.status != Status::Unchained(Unchained::AtStart)
                || slot.record.parent != directory
            {
                stale = true;
                break;
            }
            slot.status = Status::Chained;
            slot.chained_in = Some(directory);
            chain.push_back(at);
            at = slot.record.sibling;
        }
        if stale && let Some(slot) = self.slot_mut(directory) {
            slot.chain_freed = true;
        }
        if !chain.is_empty() {
            self.chains.insert(directory, chain);
        }
    }

    /// Every node id below its block's `node_count`: every slot past it is free.
    fn nodes(&self) -> impl Iterator<Item = NodeID> + '_ {
        self.blocks.iter().enumerate().flat_map(|(index, block)| {
            let count = block
                .as_ref()
                .map_or(0, |block| (block.node_count as usize).min(BLOCK_NODE_COUNT));
            (0..count).map(move |slot| (index * BLOCK_NODE_COUNT + slot) as NodeID)
        })
    }

    pub fn block(&self, index: usize) -> Option<&BlockModel> {
        self.blocks.get(index).and_then(Option::as_ref)
    }

    fn block_mut(&mut self, index: usize) -> Option<&mut BlockModel> {
        self.blocks.get_mut(index).and_then(Option::as_mut)
    }

    pub fn slot(&self, node: NodeID) -> Option<&Slot> {
        let (block, index) = split(node);
        self.block(block).map(|block| &block.slots[index])
    }

    fn slot_mut(&mut self, node: NodeID) -> Option<&mut Slot> {
        let (block, index) = split(node);
        self.block_mut(block).map(|block| &mut block.slots[index])
    }

    /// The chain of `directory`, its head first.
    pub fn chain(&self, directory: NodeID) -> Vec<NodeID> {
        self.chains
            .get(&directory)
            .map(|chain| chain.iter().copied().collect())
            .unwrap_or_default()
    }

    /// What `node` hangs from, up its chains.
    fn top(&self, node: NodeID) -> Top {
        let mut at = node;
        for _ in 0..=self.blocks.len() * BLOCK_NODE_COUNT {
            if at == ROOT_NODE {
                return Top::Root;
            }
            let Some(slot) = self.slot(at) else {
                return Top::Lost;
            };
            match slot.status {
                Status::Free => return Top::Lost,
                Status::InFlight => return Top::InFlight(at),
                Status::Unchained(_) => return Top::Unchained(at),
                Status::Chained => match slot.chained_in {
                    Some(directory) => at = directory,
                    None => return Top::Lost,
                },
            }
        }
        Top::Lost
    }

    /// Whether `node` is live: in its parent's chain, and so up to the root.
    pub fn is_live(&self, node: NodeID) -> bool {
        self.top(node) == Top::Root
    }

    /// Whether `node` is `ancestor` or below it, up its chains.
    fn is_below(&self, node: NodeID, ancestor: NodeID) -> bool {
        let mut at = node;
        for _ in 0..=self.blocks.len() * BLOCK_NODE_COUNT {
            if at == ancestor {
                return true;
            }
            match self.slot(at) {
                Some(slot) if slot.status == Status::Chained && at != ROOT_NODE => {
                    match slot.chained_in {
                        Some(directory) => at = directory,
                        None => return false,
                    }
                }
                _ => return false,
            }
        }
        false
    }

    /// Takes `node` out of its chain, where one holds it.
    fn unchain(&mut self, node: NodeID) {
        let Some(directory) = self.slot_mut(node).and_then(|slot| slot.chained_in.take()) else {
            return;
        };
        if let Some(chain) = self.chains.get_mut(&directory) {
            chain.retain(|&member| member != node);
            if chain.is_empty() {
                self.chains.remove(&directory);
            }
        }
    }

    /// Puts `node` at the head of `directory`'s chain.
    fn prepend(&mut self, directory: NodeID, node: NodeID) {
        self.chains.entry(directory).or_default().push_front(node);
        if let Some(slot) = self.slot_mut(node) {
            slot.status = Status::Chained;
            slot.chained_in = Some(directory);
        }
    }

    /// Adds `weight` to the chain count of `directory`.
    fn count(&mut self, directory: NodeID, weight: u32) {
        if let Some(slot) = self.slot_mut(directory) {
            slot.changes = slot.changes.wrapping_add(weight);
        }
    }

    /// `node` and every node its chains reach below it.
    fn subtree(&self, node: NodeID) -> Vec<NodeID> {
        let mut nodes = vec![node];
        let mut at = 0;
        let most = self.blocks.len() * BLOCK_NODE_COUNT;
        while let Some(&next) = nodes.get(at) {
            at += 1;
            if let Some(chain) = self.chains.get(&next) {
                nodes.extend(chain.iter().copied());
            }
            if nodes.len() > most {
                break;
            }
        }
        nodes
    }
}

/// One record applied to its State.
struct Replay<'a> {
    state: &'a mut StateModel,
    record: &'a Record,
    /// The record's call, where the replay was handed its run's.
    call: Option<Held<'a>>,
    /// Whether that call was left part way, failed, or dropped where drops are excused: the breaches
    /// of closure the record forms are the call's to leave.
    part_way: bool,
    /// Whether that call applies a swap's changes from its target ([`Self::swapping`]).
    applying: committed::Applying,
    /// The id of that call where it is a sweep's ([`sweeps`]).
    sweeping: Option<u64>,
    /// Whether that call runs under the C API's shared claim ([`Tier::Shared`]), where no
    /// published slot is freed (C3).
    shared: bool,
    roles: &'a mut Vec<(NodeID, Role)>,
    moved_bits: &'a mut Option<MovedBits>,
    admitted: &'a mut Option<NodeID>,
    judged: &'a mut Option<BTreeMap<(u64, u64, u32), Judged>>,
    violations: &'a mut Vec<Violation>,
    admit_unlabelled: bool,
}

impl Replay<'_> {
    fn fail(&mut self, what: String) {
        self.violations.push(violation(self.record, what));
    }

    /// A reader's read of `node`'s parent word as `parent`: the model's, where it knows the node's
    /// place and the slot is not free.
    fn parent_read(&mut self, node: NodeID, parent: NodeID) {
        let held = (self.state.slot(node))
            .filter(|slot| slot.placed && slot.status != Status::Free)
            .map(|slot| slot.record.parent)
            .filter(|&held| held != parent);
        if let Some(held) = held {
            self.fail(format!(
                "a reader read {node}'s parent as {parent}, its words holding {held}"
            ));
        }
    }

    /// A reader's read of `node`'s name as `name`: the model's, where it knows the name.
    fn name_read(&mut self, node: NodeID, name: &Name) {
        let held = (self.state.slot(node))
            .and_then(|slot| slot.name.as_ref())
            .filter(|&held| held != name);
        if let Some(held) = held {
            let what =
                format!("a reader read {node}'s name as {name:?}, the model holding {held:?}");
            self.fail(what);
        }
    }

    /// The section's structural record names `node` in `role`.
    fn role(&mut self, node: NodeID, role: Role) {
        self.roles.push((node, role));
    }

    fn apply(&mut self) {
        // The node a record of a call left part way writes, and the breaches about it before it.
        let leaving = (self.part_way && !self.state.served)
            .then(|| excuses::written(&self.record.effect))
            .flatten()
            .map(|node| (node, self.state.breaches_about(node)));
        self.called();
        let changes = changes(&self.record.effect);
        self.state.written |= changes;
        self.state.changed |= changes;
        match &self.record.effect {
            Effect::Grab { node, instance } => self.grab(*node, *instance),
            Effect::Free { node, instance } => self.free(*node, *instance),
            Effect::Allocated { block, instance } => self.allocated(*block, *instance),
            Effect::Loaded {
                block,
                instance,
                zeroed,
            } => self.loaded(*block, *instance, *zeroed),
            Effect::Publish {
                node,
                parent,
                name_hash,
                name,
                kind,
                check,
                expected,
                instance,
            } => self.publish(
                (*node, *parent),
                (*name_hash, name, *kind),
                (*check, *expected),
                *instance,
            ),
            Effect::BuildAdd {
                node,
                parent,
                name_hash,
                name,
                kind,
            } => self.build_add(*node, *parent, (*name_hash, name, *kind)),
            Effect::Abandon { root } => self.abandon(*root),
            Effect::Detach {
                node,
                parent,
                how,
                expected,
                instance,
            } => self.unlink((*node, *parent), (*how, *expected), *instance),
            Effect::Pend { root } => self.pend(*root),
            Effect::Discarding { node } => self.discarding(*node),
            Effect::Move {
                node,
                from,
                to,
                name_hash,
                name,
                replaced,
                check,
                expected,
                instances,
            } => self.moved(
                (*node, *replaced),
                (*from, *to),
                (*name_hash, name.as_ref()),
                (*check, *expected),
                *instances,
            ),
            Effect::Rename {
                node,
                parent,
                name_hash,
                name,
                instance,
            } => self.rename(*node, *parent, (*name_hash, name), *instance),
            Effect::Repin { node, child, whole } => self.repin(*node, *child, *whole),
            Effect::Served => self.state.served = true,
            Effect::Unfinished { set } => self.state.unfinished = *set,
            Effect::Guard { event } => self.guard(*event),
            Effect::Word {
                node,
                record,
                clear_pending,
            } => self.word(*node, record, *clear_pending),
            Effect::Refused {
                node,
                parent,
                reason,
                read,
                expected,
            } => self.refused(*node, *parent, *reason, read, *expected),
            Effect::Admitted { node, expected } => self.admitted(*node, *expected),
            Effect::PreTest {
                node,
                refused,
                expected,
                ..
            } => self.pre_tested(*node, *refused, *expected),
            // Made under the block's read lock at a `seq` of its own, so its count is the one the
            // counted sections before its `seq` left.
            Effect::Head {
                node,
                changes,
                instance,
            } => self.counted(
                *node,
                Expected::Count {
                    changes: *changes,
                    instance: *instance,
                },
                "a reader's head read",
            ),
            // Each made under the block's read lock at a `seq` of its own, so it read the words
            // and the name the sections before its `seq` left.
            Effect::Parent { node, parent } => self.parent_read(*node, *parent),
            Effect::Named { node, name } => self.name_read(*node, name),
            // Judged with the record's call ([`Self::called`]), a resolution's hop as a lookup of
            // its call's, which `replay_called` notes before the replay; `Created` is the model's
            // own.
            Effect::Created { .. }
            | Effect::Arm(_)
            | Effect::Permit
            | Effect::Unpermit
            | Effect::Hop { .. } => {}
        }
        if changes {
            self.state.unexcuse();
            self.swapping();
        }
        if let Some((node, before)) = leaving {
            self.state.excuse(node, &before);
        }
    }

    /// Whether `instance` is block `index`'s current one: no record names an instance a load
    /// replaced.
    fn instance(&mut self, index: usize, instance: u64) -> bool {
        match self.state.block(index).map(|block| block.instance) {
            Some(current) if current == instance => true,
            Some(current) => {
                self.fail(format!(
                    "a record of block {index}'s instance {instance}, where its instance is {current}"
                ));
                false
            }
            None => {
                self.fail(format!(
                    "a record of block {index}, which the model has no image of"
                ));
                false
            }
        }
    }

    /// A grab pops its block's free head, or takes the slot at `node_count`; the slot is zeroed,
    /// `Discarded`, in flight.
    fn grab(&mut self, node: NodeID, instance: u64) {
        let (index, slot_index) = split(node);
        if !self.instance(index, instance) {
            return;
        }
        let Some(block) = self.state.block_mut(index) else {
            return;
        };
        let at = slot_index as u32;
        let expected = block.free.last().copied().unwrap_or(block.node_count);
        let was = block.slots[slot_index].status;
        if block.free.last() == Some(&at) {
            block.free.pop();
        } else if at >= block.node_count {
            block.node_count = at + 1;
        } else {
            block.free.retain(|&free| free != at);
        }
        if at != expected {
            self.fail(format!(
                "a grab of {node}, where the block gives slot {expected}"
            ));
        }
        if was != Status::Free {
            self.fail(format!("a grab of {node}, which is not free ({was:?})"));
        }
        // A slot freed in the chain of a detached directory leaves it as it is taken again.
        self.state.unchain(node);
        self.state.moved_from.remove(&node);
        self.state.base_marks_until.remove(&node);
        self.state.pins_before.remove(&node);
        if let Some(slot) = self.state.slot_mut(node) {
            let (changes, clear_pending) = (slot.changes, slot.clear_pending);
            *slot = Slot::free(Node {
                flags: NodeFlags::Discarded.bits(),
                ..Node::default()
            });
            slot.changes = changes;
            slot.clear_pending = clear_pending;
            slot.status = Status::InFlight;
        }
        self.role(node, Role::Target);
    }

    /// A free pushes its slot, of a node in flight, of an abandoned build, or below a detached
    /// root or a freed slot; never of a live one (H6), never of a published one under a shared
    /// claim (C3, H18), nor on a served State but by a sweep (C3), and a sweep's as its plan says
    /// ([`sweeps`]), a build left in flight freed whole. The slot keeps its size, mode and
    /// extension word; its block resets where its used count falls to its free count.
    fn free(&mut self, node: NodeID, instance: u64) {
        let (index, slot_index) = split(node);
        if !self.instance(index, instance) {
            return;
        }
        let Some(slot) = self.state.slot(node) else {
            return;
        };
        let (status, published, chained_in, was) =
            (slot.status, slot.published, slot.chained_in, slot.record);
        if status == Status::Free {
            self.fail(format!("a free of {node}, which is free"));
            return;
        }
        // On a served State every free in the section is a sweep's, as no discard runs there.
        let sweep = self.record.section == Section::DiscardFree
            && (self.sweeping.is_some() || self.state.served);
        match self.state.top(node) {
            Top::Root if status != Status::InFlight => {
                self.fail(format!("a free of live {node}"));
            }
            Top::InFlight(root) if root != node && !sweep => self.fail(format!(
                "a free of {node} in the unpublished build of {root}, not cut loose"
            )),
            Top::InFlight(_) if !sweep && self.state.chains.contains_key(&node) => {
                self.fail(format!(
                    "a free of {node}, a build's root holding its build, not cut loose"
                ));
            }
            _ => {}
        }
        if published && self.shared {
            self.fail(format!(
                "a free of published {node} under a shared claim (C3, H18)"
            ));
        }
        if published && self.state.served && !sweep {
            self.fail(format!("a free of published {node} on a served State"));
        }
        if sweep {
            self.swept(node);
        }
        self.state.pending.remove(&node);
        self.state.base_marks_left(node, &was, self.record.seq);
        // The slot stays in the chain of the detached directory holding it, the State's chain
        // pointing at it until its next grab takes it elsewhere.
        if let Some(directory) = chained_in.and_then(|directory| self.state.slot_mut(directory)) {
            directory.chain_freed = true;
        }
        // Its chain goes with its `child`; the nodes it held are orphans.
        if let Some(chain) = self.state.chains.remove(&node) {
            for member in chain {
                if let Some(slot) = self.state.slot_mut(member) {
                    slot.chained_in = None;
                    if slot.status == Status::Chained {
                        slot.status = Status::Unchained(Unchained::Orphaned);
                    }
                }
            }
        }
        let Some(block) = self.state.block_mut(index) else {
            return;
        };
        let slot = &mut block.slots[slot_index];
        slot.record.flags = NodeFlags::Discarded.bits();
        slot.record.child = 0;
        slot.record.address = Default::default();
        slot.status = Status::Free;
        slot.name = None;
        slot.published = false;
        slot.joined_at = None;
        slot.repositioned_at = None;
        slot.renamed_at = None;
        slot.written_at = None;
        slot.detached = None;
        slot.chain_freed = false;
        slot.build = None;
        block.free.push(slot_index as u32);
        let used = block.node_count - u32::from(index == 0);
        if used == block.free.len() as u32 {
            block.free.clear();
            block.node_count = u32::from(index == 0);
        }
        self.role(node, Role::Target);
    }

    /// A fresh block past the tree's.
    fn allocated(&mut self, index: usize, instance: u64) {
        if self.state.block(index).is_some() {
            self.fail(format!("block {index} allocated, which the model holds"));
        }
        if self.state.blocks.len() <= index {
            self.state.blocks.resize(index + 1, None);
        }
        let mut block = BlockModel::empty(index, instance);
        block.node_count = 0;
        if index == 0 {
            block.slots[0].status = Status::Free;
        }
        self.state.blocks[index] = Some(block);
    }

    /// A block installed: zeroed, empty; from the store, as the model holds it. Either restarts
    /// its counts and pending clear bits.
    fn loaded(&mut self, index: usize, instance: u64, zeroed: bool) {
        self.state
            .settles
            .retain(|&(node, _), _| split(node).0 != index);
        if zeroed {
            if self.state.blocks.len() <= index {
                self.state.blocks.resize(index + 1, None);
            }
            self.state.blocks[index] = Some(BlockModel::empty(index, instance));
            return;
        }
        let Some(block) = self.state.block_mut(index) else {
            self.fail(format!(
                "block {index} loaded from the store, which the model has no image of"
            ));
            return;
        };
        block.instance = instance;
        for slot in &mut block.slots {
            slot.changes = 0;
            slot.clear_pending = false;
        }
    }

    /// Checks a node in flight against what its initialization gave it, where a `Word` told it:
    /// `parent`, the name's hash and the kind. One no `Word` told is a directory with no content,
    /// whose parent and name the publish tells.
    fn initialized(
        &mut self,
        node: NodeID,
        parent: NodeID,
        (name_hash, kind): (u64, Kind),
    ) -> bool {
        let Some((status, record, placed)) = self
            .state
            .slot(node)
            .map(|slot| (slot.status, slot.record, slot.placed))
        else {
            self.fail(format!("{node}, in a block the model has no image of"));
            return false;
        };
        if status != Status::InFlight {
            self.fail(format!("{node} put in a chain, not in flight ({status:?})"));
            return false;
        }
        let agrees = !placed || (record.parent == parent && record.name_hash == name_hash);
        if !agrees || Kind::of(&record) != kind {
            self.fail(format!(
                "{node} put below {parent} as {kind:?} hashed {name_hash:x}, initialized below \
                 {} as {:?} hashed {:x}",
                record.parent,
                Kind::of(&record),
                record.name_hash
            ));
        }
        if let Some(slot) = self.state.slot_mut(node) {
            slot.record.parent = parent;
            slot.record.name_hash = name_hash;
            slot.placed = true;
        }
        true
    }

    /// No other node carrying `name_hash` in `directory`'s chain, not Discarded: on a served State,
    /// whose names are unique by hash (C8), and for a change that checked its name free.
    fn unique(&mut self, directory: NodeID, name_hash: u64, mover: NodeID) {
        let state = &*self.state;
        let held = state.chains.get(&directory).and_then(|chain| {
            chain.iter().copied().find(|&member| {
                member != mover
                    && state.slot(member).is_some_and(|slot| {
                        slot.record.name_hash == name_hash && !slot.is_discarded()
                    })
            })
        });
        if let Some(holder) = held {
            self.fail(format!(
                "{mover} joins {directory}, where {holder} holds its name (C8)"
            ));
        }
    }

    /// A publish puts its node, in flight, at the head of its parent's chain, its build with it,
    /// and counts 1.
    fn publish(
        &mut self,
        (node, parent): (NodeID, NodeID),
        (name_hash, name, kind): (u64, &Name, Kind),
        checked: (Check, Expected),
        instance: u64,
    ) {
        self.role(node, Role::Target);
        if !self.instance(NodeBlock::index(parent), instance) {
            return;
        }
        if !self.initialized(node, parent, (name_hash, kind)) {
            return;
        }
        if self
            .state
            .slot(parent)
            .is_none_or(|slot| slot.status == Status::Free)
        {
            self.fail(format!("{node} published below {parent}, a free slot"));
            return;
        }
        self.published_on((node, parent), name_hash, checked);
        let build: Vec<NodeID> = self.state.subtree(node);
        self.state.prepend(parent, node);
        let seq = self.record.seq;
        for member in build {
            if let Some(slot) = self
                .state
                .slot_mut(member)
                .filter(|slot| slot.status != Status::Free)
            {
                slot.published = true;
                slot.joined_at = Some(seq);
                slot.build = None;
            }
        }
        if let Some(slot) = self.state.slot_mut(node) {
            slot.record.flags &= !NodeFlags::Discarded.bits();
            slot.name = Some(name.clone());
        }
        self.state.count(parent, 1);
    }

    /// A build's add puts its node, in flight, at the head of a directory's chain in an
    /// unpublished build, uncounted.
    fn build_add(
        &mut self,
        node: NodeID,
        parent: NodeID,
        (name_hash, name, kind): (u64, &Name, Kind),
    ) {
        self.role(node, Role::Target);
        let Top::InFlight(root) = self.state.top(parent) else {
            self.fail(format!(
                "{node} added below {parent}, which is in no unpublished build"
            ));
            return;
        };
        if root == node {
            self.fail(format!("{node} added into its own build"));
            return;
        }
        if self
            .state
            .slot(parent)
            .is_some_and(|slot| slot.kind() != Kind::Directory)
        {
            self.fail(format!("{node} added below {parent}, no directory"));
        }
        if !self.initialized(node, parent, (name_hash, kind)) {
            return;
        }
        self.state.prepend(parent, node);
        if let Some(slot) = self.state.slot_mut(node) {
            slot.record.flags &= !NodeFlags::Discarded.bits();
            slot.name = Some(name.clone());
            slot.build = Some(root);
        }
    }

    /// An abandon cuts its unpublished build loose: every node of it `Discarded`, its links'
    /// `child` zeroed, its chains gone, for its frees.
    fn abandon(&mut self, root: NodeID) {
        self.role(root, Role::Target);
        let status = self.state.slot(root).map(|slot| slot.status);
        if status != Some(Status::InFlight) {
            self.fail(format!(
                "an abandon of {root}, no build's root ({status:?})"
            ));
            return;
        }
        for node in self.state.subtree(root) {
            self.state.chains.remove(&node);
            if let Some(slot) = self.state.slot_mut(node) {
                slot.record.flags |= NodeFlags::Discarded.bits();
                if slot.record.is_link() {
                    slot.record.child = 0;
                }
                slot.status = Status::Unchained(Unchained::Abandoned);
                slot.chained_in = None;
                slot.build = None;
            }
        }
    }

    /// An unlink takes its node out of its parent's chain, `Discarded`; a detach counts 1, a
    /// discard none.
    fn unlink(
        &mut self,
        (node, parent): (NodeID, NodeID),
        decided: (How, Expected),
        instance: u64,
    ) {
        let how = decided.0;
        self.role(node, Role::Target);
        if !self.instance(NodeBlock::index(parent), instance) {
            return;
        }
        let held = self
            .state
            .slot(node)
            .is_some_and(|slot| slot.status == Status::Chained && slot.chained_in == Some(parent));
        if !held {
            self.fail(format!(
                "an unlink of {node} from {parent}, whose chain does not hold it"
            ));
            return;
        }
        self.unlinking((node, parent), how, decided.1);
        self.state.unchain(node);
        let detachment = Detachment {
            seq: self.record.seq,
            served: self.state.served,
            held: false,
        };
        if let Some(slot) = self.state.slot_mut(node) {
            slot.status = Status::Unchained(Unchained::Detached(how));
            slot.record.flags |= NodeFlags::Discarded.bits();
            slot.detached = Some(detachment);
        }
        if how != How::Discard {
            self.state.count(parent, 1);
        }
    }

    /// The C API delete's detached root, queued.
    fn pend(&mut self, root: NodeID) {
        let status = self.state.slot(root).map(|slot| slot.status);
        if !matches!(status, Some(Status::Unchained(Unchained::Detached(_)))) {
            self.fail(format!("{root} queued pending, not detached ({status:?})"));
        }
        self.state.pending.insert(root);
    }

    /// A pending mark or a discard's sets its node's `Discarded`, never on a live node.
    fn discarding(&mut self, node: NodeID) {
        self.role(node, Role::Target);
        if self.state.is_live(node) {
            self.fail(format!("a discard's mark of live {node}"));
        }
        let status = self.state.slot(node).map(|slot| slot.status);
        if status.is_none_or(|status| status == Status::Free) {
            self.fail(format!("a discard's mark of {node}, a free slot"));
            return;
        }
        if let Some(slot) = self.state.slot_mut(node) {
            slot.record.flags |= NodeFlags::Discarded.bits();
        }
    }

    /// A move takes its node out of `from`'s chain to the head of `to`'s, its holder unchained;
    /// `from` counts R, `to` 1, and R more for a holder replaced across directories.
    fn moved(
        &mut self,
        (node, replaced): (NodeID, Option<NodeID>),
        (from, to): (NodeID, NodeID),
        (name_hash, name): (u64, Option<&Name>),
        (check, expected): (Check, Expected),
        instances: (u64, u64),
    ) {
        self.role(node, Role::Target);
        if let Some(holder) = replaced {
            self.role(holder, Role::Holder);
        }
        if !self.instance(NodeBlock::index(from), instances.0)
            || !self.instance(NodeBlock::index(to), instances.1)
        {
            return;
        }
        // Its decision's rules first: a Discarded mover may still hang in its chain.
        let unique = self.moving(node, to, check, expected);
        let held = self
            .state
            .slot(node)
            .is_some_and(|slot| slot.status == Status::Chained && slot.chained_in == Some(from));
        if !held {
            let status = self.state.slot(node).map(|slot| slot.status);
            self.fail(format!(
                "a move of {node} out of {from}, whose chain does not hold it ({status:?})"
            ));
            return;
        }
        if self.state.is_below(to, node) {
            self.fail(format!("a move of {node} into its own subtree, below {to}"));
            return;
        }
        if let Some(holder) = replaced {
            let holds = self
                .state
                .slot(holder)
                .is_some_and(|slot| slot.status == Status::Chained && slot.chained_in == Some(to));
            if !holds {
                self.fail(format!(
                    "{holder} replaced in {to}, whose chain does not hold it"
                ));
                return;
            }
            self.state.unchain(holder);
            // Nodes it held but the mover, which leaves it by the move itself.
            let held = self
                .state
                .chains
                .get(&holder)
                .is_some_and(|chain| chain.iter().any(|&member| member != node));
            let detachment = Detachment {
                seq: self.record.seq,
                served: self.state.served,
                held,
            };
            if let Some(slot) = self.state.slot_mut(holder) {
                slot.status = Status::Unchained(Unchained::Replaced);
                slot.record.flags |= NodeFlags::Discarded.bits();
                slot.detached = Some(detachment);
            }
        }
        self.moved_bits(node, replaced);
        if unique {
            self.unique(to, name_hash, node);
        }
        self.state.unchain(node);
        self.state.prepend(to, node);
        // A move into another directory joins it; one within its directory joins nothing.
        let joined = (from != to).then_some(self.record.seq);
        if let Some(seq) = joined {
            self.state
                .moved_from
                .entry(node)
                .or_default()
                .push((seq, from));
        }
        if let Some(slot) = self.state.slot_mut(node) {
            slot.record.parent = to;
            slot.record.name_hash = name_hash;
            slot.joined_at = joined.or(slot.joined_at);
            if from == to {
                slot.repositioned_at = Some(self.record.seq);
            }
            if let Some(name) = name {
                slot.name = Some(name.clone());
            }
        }
        self.state.count(from, REATTACH_CHANGES);
        self.state.count(to, 1);
        if replaced.is_some() && from != to {
            self.state.count(to, REATTACH_CHANGES);
        }
    }

    /// A rename in place keeps its node's position; it counts R where the hash changes, none
    /// for a respell.
    fn rename(
        &mut self,
        node: NodeID,
        parent: NodeID,
        (name_hash, name): (u64, &Name),
        instance: u64,
    ) {
        self.role(node, Role::Target);
        if !self.instance(NodeBlock::index(parent), instance) {
            return;
        }
        if let Some(was) = self.state.slot(node).map(|slot| slot.record) {
            self.renaming(node, &was);
        }
        let Some(slot) = self
            .state
            .slot(node)
            .filter(|slot| slot.status == Status::Chained && slot.chained_in == Some(parent))
        else {
            self.fail(format!(
                "a rename of {node} in {parent}, whose chain does not hold it"
            ));
            return;
        };
        let weight = if slot.record.name_hash == name_hash {
            0
        } else {
            REATTACH_CHANGES
        };
        if let Some(slot) = self.state.slot_mut(node) {
            slot.record.name_hash = name_hash;
            slot.name = Some(name.clone());
            slot.renamed_at = Some(self.record.seq);
        }
        self.state.count(parent, weight);
    }

    /// A repin of a link: the whole pin sets its `child`; the revision alone keeps it.
    fn repin(&mut self, node: NodeID, child: NodeID, whole: bool) {
        self.role(node, Role::Target);
        let Some((live_link, record)) = self
            .state
            .slot(node)
            .map(|slot| (slot.record.is_link() && !slot.is_discarded(), slot.record))
        else {
            self.fail(format!(
                "a repin of {node}, which the model has no image of"
            ));
            return;
        };
        if !live_link {
            self.fail(format!("a repin of {node}, which is no live link"));
            return;
        }
        // Its pin before the repin, which the section's `Word` would otherwise read as the whole
        // pin's `child` already changed.
        self.state
            .pin_changing(node, self.record.seq, record.linked_node());
        if whole {
            if let Some(slot) = self.state.slot_mut(node) {
                slot.record.child = child;
            }
        } else if record.child != child {
            self.fail(format!(
                "a repin of {node}'s revision alone moved its child from {} to {child}",
                record.child
            ));
        }
    }

    fn guard(&mut self, event: GuardEvent) {
        match event {
            GuardEvent::Armed => *self.state.armed.entry(self.record.task).or_default() += 1,
            GuardEvent::Finished | GuardEvent::DroppedArmed => {
                let task = self.record.task;
                match self.state.armed.get(&task).copied().unwrap_or(0) {
                    0 => self.fail(format!("a guard {event:?} with none armed in its task")),
                    armed => {
                        self.state.armed.insert(task, armed - 1);
                    }
                }
            }
        }
    }

    /// A `Word`: held to its section's set for its node's role, then its words replace the model's.
    /// An initialization's sets the parent and name hash of its slot in flight too.
    fn word(&mut self, node: NodeID, words: &Words, clear_pending: bool) {
        let section = self.record.section;
        let Some((status, was, was_pending)) = self
            .state
            .slot(node)
            .map(|slot| (slot.status, slot.record, slot.clear_pending))
        else {
            self.fail(format!("a word of {node}, which the model has no image of"));
            return;
        };
        if status == Status::Free {
            self.fail(format!("a word of {node}, a free slot"));
            return;
        }
        let is = words.0;
        let role = if self.roles.is_empty() {
            Role::Any
        } else {
            self.roles
                .iter()
                .find(|(named, _)| *named == node)
                .map_or(Role::Other, |(_, role)| *role)
        };
        let link = was.is_link() || is.is_link();
        let set = if section == Section::Unlabelled && self.admit_unlabelled {
            Set::ALL
        } else {
            fields::set(section, role, self.state.served, link)
        };
        let changed = Fields::changed((&was, was_pending), (&is, clear_pending));
        let at = (self.record.state, self.record.seq, self.record.sub);
        if let Some(judged) = self.judged.as_mut() {
            judged.insert(
                at,
                Judged {
                    set,
                    was,
                    was_pending,
                },
            );
        }
        if !set.admits(changed, &is) {
            let outside = changed & !set.fields;
            self.violations.push(Violation {
                write_set: true,
                ..violation(
                    self.record,
                    format!(
                        "{section:?} wrote {outside:?} of {node} ({role:?}), changing {changed:?} \
                         outside {:?}",
                        set.fields
                    ),
                )
            });
        }
        self.writing(node, section, (&was, was_pending), (&is, clear_pending));
        self.state.base_marks_left(node, &was, self.record.seq);
        if was.is_link() && (was.address != is.address || was.child != is.child) {
            self.state
                .pin_changing(node, self.record.seq, was.linked_node());
        }
        match section {
            Section::Grab if status != Status::InFlight => {
                self.fail(format!(
                    "an initialization of {node}, not in flight ({status:?})"
                ));
            }
            Section::SettlePhase1 | Section::SettlePhase3 if Kind::of(&is) != Kind::Directory => {
                self.fail(format!("a settle wrote {node}, no directory"));
            }
            Section::Save if node != ROOT_NODE => {
                self.fail(format!("a save wrote {node}, not the root"));
            }
            Section::DiscardMarks if Kind::of(&is) == Kind::Directory => {
                self.unfreed_head(node, is.child);
            }
            _ => {}
        }
        let Some(slot) = self.state.slot_mut(node) else {
            return;
        };
        let record = &mut slot.record;
        record.flags = is.flags;
        record.ext_flags = is.ext_flags;
        record.address = is.address;
        record.size = is.size;
        record.mode = is.mode;
        if link {
            record.child = is.child;
        }
        if section == Section::Grab {
            record.parent = is.parent;
            record.name_hash = is.name_hash;
            slot.placed = true;
        }
        slot.clear_pending = clear_pending;
        slot.written_at = Some(self.record.seq);
    }
}

/// A scenario's context whose States the model follows.
pub trait Modelled {
    /// Every State its setup made that a run writes or reads, with its repository.
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)>;

    /// Makes the context whole for a checkpoint's reads and save, at a quiescent point: a store a
    /// run made fail reads again.
    fn mend(&self) {}

    /// Whether a fault the context injects failed a read of the run's outside every call's own
    /// steps, which may have stopped an operation part way where no call's records tell what it
    /// left, as one dropped outside every call stops; a call's own failed read leaves what its
    /// records tell.
    fn faulted(&self) -> bool {
        false
    }

    /// The revisions of other repositories a run may load afresh beside its States, as a read
    /// through a link loads the revision its pin names, each with a context that loads it. A
    /// context wrapping a tree whose links a run reads through passes the tree's on.
    fn revisions(&self) -> Vec<(Hash, Arc<RepositoryContext>)> {
        Vec::new()
    }

    /// What each of its States a provider would serve settles against, a committed State among
    /// [`Self::states`], which the checkpoint holds a served one's clean directories to (C11c);
    /// none for a context whose settles compare with no State of its own.
    fn committed(&self) -> Vec<Committed> {
        Vec::new()
    }
}

impl Modelled for Tree {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        vec![
            (self.state.clone(), self.repository.clone()),
            (self.committed.clone(), self.repository.clone()),
        ]
    }

    fn committed(&self) -> Vec<Committed> {
        vec![Committed {
            served: self.state.hooks().id(),
            state: self.committed.clone(),
            repository: self.repository.clone(),
            nodes: self.committed_of(),
        }]
    }

    fn revisions(&self) -> Vec<(Hash, Arc<RepositoryContext>)> {
        let linked = self.linked.iter();
        let revisions = linked.flat_map(|linked| {
            let saved = [linked.first, linked.second]
                .into_iter()
                .chain(linked.dirty);
            saved.map(|saved| (saved.signature, linked.context.clone()))
        });
        revisions.collect()
    }
}

/// Whether a record of `effect` changes its State, as the model takes it: all but a load, a
/// pre-test, a refusal or an admission, a race arm, a guard's events, a reader's head, parent or
/// name read, a resolution's hop, and the moves permit's take and release; a State's making starts
/// its model.
pub fn changes(effect: &Effect) -> bool {
    !matches!(
        effect,
        Effect::Created { .. }
            | Effect::Loaded { .. }
            | Effect::PreTest { .. }
            | Effect::Refused { .. }
            | Effect::Arm(_)
            | Effect::Head { .. }
            | Effect::Parent { .. }
            | Effect::Named { .. }
            | Effect::Hop { .. }
            | Effect::Permit
            | Effect::Unpermit
            | Effect::Guard { .. }
            | Effect::Admitted { .. }
    )
}

/// Where a run's model starts: each State of its context, read where its setup ended.
#[derive(Clone, Debug, Default)]
pub struct Start {
    states: Vec<(u64, Snapshot)>,
}

impl Start {
    /// Reads `context`'s States, at the end of its setup, with no scheduler installed.
    pub async fn read(context: &impl Modelled) -> Self {
        let mut states = Vec::new();
        for (state, repository) in context.states() {
            states.push((
                state.hooks().id(),
                Snapshot::read(&state, &repository).await,
            ));
        }
        Self { states }
    }

    /// The model of a run starting here, with a snapshot of every State `records` load, read from
    /// `context`'s repositories, and nothing replayed.
    pub async fn model(&self, context: &impl Modelled, records: &[Record]) -> Model {
        let mut model = Model::new();
        for (id, snapshot) in &self.states {
            model.start(*id, snapshot);
        }
        model.provide_needed(context, records).await;
        model
    }
}

/// Where `model` and the live States of `context` differ, each named.
pub fn compared(model: &Model, context: &impl Modelled) -> Vec<String> {
    let mut differences = Vec::new();
    for (state, _) in context.states() {
        match model.state(state.hooks().id()) {
            Some(modelled) => differences.extend(modelled.compare(&state)),
            None => differences.push(format!("State {} has no model", state.hooks().id())),
        }
    }
    differences
}
