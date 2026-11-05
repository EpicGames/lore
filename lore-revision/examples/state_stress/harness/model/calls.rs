// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model's rules that need a record's call ([`Call::holds`]): the served chain rule by entry
//! point, climb first (H8) for the staged changes, the pre-test (H2) per attempt of a served
//! conditional detach, the moves permit (C10), the call's own or its batch's, which a task the
//! call's was spawned from holds, and the served reattach's `added` answer. A record
//! no call holds, as in a model replayed without its run's calls ([`Model::replay_called`]), is
//! judged by the rules that need none: a pre-test's answer, the permit taken once at a time, and a
//! directory's reattach across parents inside a permit.
//!
//! [`Model::replay_called`]: super::Model::replay_called

use std::collections::BTreeMap;
use std::collections::VecDeque;

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_revision::state::node::hooks::log::Words;

use super::Replay;
use super::Status;
use super::names_node;
use super::outcomes::Answered;
use super::outcomes::Claim;
use super::outcomes::by_mark_of;
use super::outcomes::claims;
use super::outcomes::read;
use super::split;
use crate::harness::catalogue;
use crate::harness::catalogue::Writes;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Leaves;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Spawn;

/// The run's calls, each record's found by its task and `seq`, and what the rules keep of each as
/// the replay goes.
#[derive(Clone, Debug)]
pub(super) struct Calls {
    calls: Vec<Call>,
    spawns: Vec<Spawn>,
    /// Each `(state, task)`'s calls, by their index in `calls`, in the order they began.
    by_task: BTreeMap<(u64, usize), Vec<usize>>,
    /// Each call's index by its id, for a future spawned in its own steps ([`Spawn::call`]), and
    /// for the marked listing a read by its mark names ([`Call::listing`]).
    by_id: BTreeMap<u64, usize>,
    /// The rules each call answers to, by its index, read from its kind once.
    rules: Vec<Rules>,
    /// What the rules keep of each call, by its index.
    kept: Vec<Kept>,
    /// The calls not begun yet, by `(state, begin)`, and not ended yet, by `(state, end)`, as the
    /// replay reaches them ([`Model::reach`](super::Model::reach)).
    begins: VecDeque<usize>,
    ends: VecDeque<usize>,
    /// The `(state, seq)` the replay last reached.
    passed: Option<(u64, u64)>,
    /// The open calls' claims of their State not seen yet.
    watching: Vec<Watched>,
    /// The calls whose own write on their target asks for their claim read once its `seq` is
    /// passed ([`Self::after_writes`]).
    after: Vec<usize>,
}

/// An open call's claim not seen yet.
#[derive(Clone, Debug)]
struct Watched {
    /// The call, by its index.
    call: usize,
    /// The claim's bit in its call's [`Answered::seen`].
    bit: u32,
    /// The `seq` it is read from.
    from: u64,
    /// The claim, read to its call's end, or to its own bound ([`Claim::bound`]).
    claim: Claim,
    /// What its claim keeps between reads: the nodes a walk up the parents may have met so far, a
    /// miss's carriers, what holds a walk of a directory's children back, a has-children read's
    /// nodes of its directory or its carriers, the nodes a collection left out ([`Claim::holds`]).
    reach: Vec<NodeID>,
}

/// A record's call, the rules it answers to, and what they keep of it; the calls whose claim waits
/// on their own write, and the call's index among them; and how each task of the run came to be.
pub(super) struct Held<'a> {
    pub(super) call: &'a Call,
    rules: Rules,
    kept: &'a mut Kept,
    after: &'a mut Vec<usize>,
    index: usize,
    spawns: &'a [Spawn],
}

/// The rules a call answers to, by how its kind writes ([`catalogue::Kind::writes`]).
#[derive(Clone, Copy, Debug, Default)]
struct Rules {
    /// It changes its State through a `ServedNodes` entry point, under the served chain rule.
    served: bool,
    /// A staged change, which climbs first (H8).
    staged: bool,
    /// A detach, a served conditional one pre-testing each attempt (H2).
    detaches: bool,
    /// Its done answer is an unlink of its target: a detach, the C API delete's pend.
    unlinks: bool,
    /// It holds the moves permit across its own steps, or its batch holds it across them (C10).
    permitted: bool,
}

impl Rules {
    /// The rules of `kind`, by how it writes, each holding all of a call's records: sound for the
    /// catalogue's kinds, each of which changes its State through one tier alone. A composition of
    /// a served entry point with an unserved one would need them per record.
    fn of(kind: &catalogue::Kind) -> Self {
        let writes = |write| kind.writes.contains(&write);
        Self {
            served: writes(Writes::Served),
            staged: writes(Writes::Staged),
            detaches: writes(Writes::Detaches),
            unlinks: writes(Writes::Detaches) || writes(Writes::Pends),
            permitted: writes(Writes::Permitted),
        }
    }
}

/// What the rules keep of a call as the replay goes.
#[derive(Clone, Debug, Default)]
pub(super) struct Kept {
    /// What its records tell of its answer.
    pub(super) answered: Answered,
    /// A staged change's climb.
    climb: Climb,
    /// A detach's nodes its `Word`s wrote since its last attempt ended, before the next one's
    /// pre-test; and the pre-test of the attempt it is in.
    written: Vec<NodeID>,
    pre_test: Option<PreTested>,
}

/// What a staged change's climb did, for climb first (H8).
#[derive(Clone, Debug, Default)]
struct Climb {
    /// The nodes its marks wrote before its change, and those of them it gave `Staged`, whose
    /// staged half climbs on to their parents.
    marked: Vec<NodeID>,
    staged: Vec<NodeID>,
    /// From its change on: its node, the directory a move took it out of, and each directory it
    /// changed below with their ancestors but the root.
    changed: Option<(NodeID, NodeID, Vec<NodeID>)>,
    /// The directories a climb its kind makes after its change marks: a modify's from the parent
    /// its node moved to, a staged move's from the directory a reattach moved its node out of.
    later: Vec<NodeID>,
}

/// A staged change's change: its node, the parents the node had since the call began, the one then
/// first, where it changes below its own; the directory it changes below as the call names it, an
/// add's parent or a move's destination; and whether the change decided under its own locks that
/// its parent held its base marks, as a delete level does.
struct Change {
    node: NodeID,
    left: Vec<NodeID>,
    joined: Option<NodeID>,
    decided: bool,
}

/// A detach's attempt, from its pre-test on: the parent its node hung from, and whether it refused.
#[derive(Clone, Copy, Debug)]
struct PreTested {
    parent: NodeID,
    refused: bool,
}

/// A State's moves permit, taken by the call holding its `Permit` record, if any, in the task that
/// made the record: a batch's holder, whose futures' calls run inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Permit {
    call: Option<u64>,
    task: Option<usize>,
}

impl Calls {
    pub(super) fn new(calls: Vec<Call>, spawns: Vec<Spawn>) -> Self {
        let mut by_task: BTreeMap<(u64, usize), Vec<usize>> = BTreeMap::new();
        let mut by_id = BTreeMap::new();
        for (index, call) in calls.iter().enumerate() {
            by_id.insert(call.id, index);
            if let Some(task) = call.task {
                by_task.entry((call.state, task)).or_default().push(index);
            }
        }
        for own in by_task.values_mut() {
            own.sort_by_key(|&index| calls[index].begin);
        }
        let rules = calls.iter().map(|call| Rules::of(call.kind)).collect();
        let kept = vec![Kept::default(); calls.len()];
        let queued = |at: fn(&Call) -> u64| {
            let mut order: Vec<usize> = (0..calls.len()).collect();
            order.sort_by_key(|&index| (calls[index].state, at(&calls[index])));
            VecDeque::from(order)
        };
        let (begins, ends) = (queued(|call| call.begin), queued(|call| call.end));
        Self {
            calls,
            spawns,
            by_task,
            by_id,
            rules,
            kept,
            begins,
            ends,
            passed: None,
            watching: Vec::new(),
            after: Vec::new(),
        }
    }

    /// Whether the replay passes to a new `(state, seq)` at `at`.
    pub(super) fn passes(&mut self, at: (u64, u64)) -> bool {
        let new = self.passed != Some(at);
        self.passed = Some(at);
        new
    }

    /// The next call begun by `at`, taken.
    pub(super) fn begun(&mut self, at: (u64, u64)) -> Option<usize> {
        let calls = &self.calls;
        self.begins
            .pop_front_if(|index| (calls[*index].state, calls[*index].begin) <= at)
    }

    /// The next call ended by `at`, taken.
    pub(super) fn ended(&mut self, at: (u64, u64)) -> Option<usize> {
        let calls = &self.calls;
        self.ends
            .pop_front_if(|index| (calls[*index].state, calls[*index].end) <= at)
    }

    /// Call `index`, whether its done answer is an unlink of its target, and what its records told
    /// of its answer.
    pub(super) fn judging(&self, index: usize) -> (&Call, bool, &Answered) {
        (
            &self.calls[index],
            self.rules[index].unlinks,
            &self.kept[index].answered,
        )
    }

    /// Call `index` begun by `at`: what it claims, read there by `holds` where read from by then,
    /// then at each `seq` it is open ([`Self::look`]). A claim bounded short of its call's end, a
    /// read by its own listing's mark, resolves only once that listing's head read, past `at`.
    pub(super) fn watch(
        &mut self,
        index: usize,
        at: (u64, u64),
        holds: impl Fn(&Call, Claim, &mut Vec<NodeID>) -> bool,
    ) {
        let call = &self.calls[index];
        let mark = (call.listing)
            .and_then(|listing| self.by_id.get(&listing))
            .and_then(|&listing| {
                by_mark_of(call, (&self.calls[listing], &self.kept[listing].answered))
            });
        self.kept[index].answered.marked = mark.is_some();
        for (bit, from, claim) in claims(call, mark) {
            let mut reach = Vec::new();
            if let Some((from, claim)) = read(call, claim, from, &self.kept[index].answered)
                && (call.state, from) <= at
                && holds(call, claim, &mut reach)
            {
                self.kept[index].answered.seen |= 1 << bit;
            } else {
                self.watching.push(Watched {
                    call: index,
                    bit,
                    from,
                    claim,
                    reach,
                });
            }
        }
    }

    /// Each call that wrote its target since the `seq` passed last: its claim, what its done answer
    /// leaves, read by `holds` once that write applied.
    pub(super) fn after_writes(&mut self, holds: impl Fn(&Call, Claim, &mut Vec<NodeID>) -> bool) {
        for index in self.after.drain(..) {
            let call = &self.calls[index];
            if let Some(claim) = Claim::of(call) {
                self.kept[index].answered.after_write = Some(holds(call, claim, &mut Vec::new()));
            }
        }
    }

    pub(super) fn unwatch(&mut self, index: usize) {
        self.watching.retain(|watched| watched.call != index);
    }

    /// Each open call's claim not seen yet and read from by `at`, read by `holds`; one seen is
    /// watched no longer, nor one bounded by `at`, read a last time on the model as its bound left
    /// it, as every `seq` holding a record passes here.
    pub(super) fn look(
        &mut self,
        at: (u64, u64),
        holds: impl Fn(&Call, Claim, &mut Vec<NodeID>) -> bool,
    ) {
        let (calls, kept) = (&self.calls, &mut self.kept);
        self.watching.retain_mut(|watched| {
            let call = &calls[watched.call];
            let answered = &kept[watched.call].answered;
            let seen =
                read(call, watched.claim, watched.from, answered).is_some_and(|(from, claim)| {
                    (call.state, from) <= at && holds(call, claim, &mut watched.reach)
                });
            kept[watched.call].answered.seen |= u32::from(seen) << watched.bit;
            !seen && (watched.claim.bound()).is_none_or(|to| at < (call.state, to))
        });
    }

    /// The call holding `record`, if any: one of its task's, the latest begun first, as a call made
    /// in another's window begins after it; or the one a future up its spawns was spawned in.
    pub(super) fn holding(&mut self, record: &Record) -> Option<Held<'_>> {
        let index = self.holder(record)?;
        Some(Held {
            call: &self.calls[index],
            rules: self.rules[index],
            kept: &mut self.kept[index],
            after: &mut self.after,
            index,
            spawns: &self.spawns,
        })
    }

    fn holder(&self, record: &Record) -> Option<usize> {
        let task = record.task?;
        let holds = |index: usize| self.calls[index].holds(record, &self.spawns);
        if let Some(own) = self.by_task.get(&(record.state, task)) {
            let begun = own.partition_point(|&index| self.calls[index].begin <= record.seq);
            if let Some(&index) = own[..begun].iter().rev().find(|&&index| holds(index)) {
                return Some(index);
            }
        }
        let mut at = task;
        for _ in 0..self.spawns.len() {
            let spawn = self.spawns.get(at)?;
            if let Some(&index) = spawn.call.and_then(|id| self.by_id.get(&id))
                && holds(index)
            {
                return Some(index);
            }
            at = spawn.spawner?;
        }
        None
    }
}

/// Whether `effect` is `call`'s write on its target of what its done answer leaves there: its
/// place or its name by a `Move` or a `Rename`, its pin by a repin, its words by a `Word`. A
/// rename's own mark after it, or a move's marks below its node, write words its answer leaves
/// nothing in.
fn writes_target(effect: &Effect, call: &Call) -> bool {
    let placing = matches!(call.outcome, Outcome::Done(Answer::Moved { .. }))
        || matches!(call.leaves, Some(Leaves::Placed | Leaves::Named));
    let node = match (effect, call.leaves) {
        (Effect::Move { node, .. } | Effect::Rename { node, .. }, _) if placing => *node,
        (Effect::Repin { node, .. }, Some(Leaves::Pinned { .. })) => *node,
        (Effect::Word { node, .. }, Some(leaves))
            if !placing && !matches!(leaves, Leaves::Pinned { .. }) =>
        {
            *node
        }
        _ => return false,
    };
    call.target == Some(node)
}

/// Whether `task` was spawned, at any depth, from `ancestor`, as `spawns` tell.
fn spawned_from(spawns: &[Spawn], task: Option<usize>, ancestor: usize) -> bool {
    let mut at = task;
    for _ in 0..spawns.len() {
        let Some(spawner) = at.and_then(|task| spawns.get(task)?.spawner) else {
            return false;
        };
        if spawner == ancestor {
            return true;
        }
        at = Some(spawner);
    }
    false
}

/// Whether a climb may stop at `word`: it holds the base `Staged` bit, and `Dirty` or `Discarded`,
/// where the dirty half stops without writing.
fn climb_stops(word: &Node) -> bool {
    let has = |flag: NodeFlags| word.flags & flag.bits() == flag.bits();
    has(NodeFlags::Staged) && (has(NodeFlags::Dirty) || has(NodeFlags::Discarded))
}

impl Replay<'_> {
    /// The rules of the record's call, before the record applies, and those that need none: the
    /// moves permit's take first and its release last, so a call's own records between fall inside
    /// it.
    pub(super) fn called(&mut self) {
        let held = self.call.take();
        let id = held.as_ref().map(|held| held.call.id);
        match self.record.effect {
            Effect::Permit => self.permit(true, id),
            Effect::Move { node, from, to, .. } if self.record.section == Section::Reattach => {
                self.reattach_permitted(node, (from, to), id);
            }
            _ => {}
        }
        if let Some(Held {
            call,
            rules,
            kept,
            after,
            index,
            spawns,
        }) = held
        {
            self.answering(call, &mut kept.answered);
            if writes_target(&self.record.effect, call) && !after.contains(&index) {
                after.push(index);
            }
            let section = self.record.section;
            if section != Section::Unlabelled && !call.kind.sections.contains(&section) {
                self.fail(format!(
                    "a record in {section:?} in a call of '{}', whose kind writes in {:?}",
                    call.kind.name, call.kind.sections
                ));
            }
            if rules.served {
                self.chain_rule(call.kind);
            }
            if rules.staged {
                self.climb(call.begin, &mut kept.climb);
            }
            if rules.detaches {
                self.pre_test_order(kept);
            }
            // A future the call spawned may record past its end, outside its steps' permit.
            if rules.permitted && self.record.seq < call.end {
                self.within_permit(call, spawns);
            }
            if let (Effect::Move { node, .. }, Outcome::Done(Answer::Moved { added })) =
                (&self.record.effect, &call.outcome)
            {
                self.added(*node, *added);
            }
        }
        if let Effect::Unpermit = self.record.effect {
            self.permit(false, id);
        }
    }

    /// The served chain rule: a publish, an unlink, a move, a rewrite's `Word` or a mark level's,
    /// made through a `ServedNodes` entry point, below a parent its word in force admits: not
    /// Discarded, Dirty, no pending clear; the root always. A served State renames only by a
    /// move, as `move_named` and `node_rename` refuse one.
    fn chain_rule(&mut self, kind: &catalogue::Kind) {
        let (node, parents) = match self.record.effect {
            Effect::Publish { node, parent, .. } | Effect::Detach { node, parent, .. } => {
                (node, [parent, ROOT_NODE])
            }
            Effect::Move { node, from, to, .. } => (node, [from, to]),
            Effect::Word { node, .. }
                if matches!(self.record.section, Section::Rewrite | Section::ServedMark) =>
            {
                let Some(parent) = self.state.slot(node).map(|slot| slot.record.parent) else {
                    return;
                };
                (node, [parent, ROOT_NODE])
            }
            _ => return,
        };
        for parent in parents {
            if parent == ROOT_NODE {
                continue;
            }
            let Some((word, pending)) = self
                .state
                .slot(parent)
                .map(|slot| (slot.record, slot.clear_pending))
            else {
                continue;
            };
            let why = if word.is_discarded() {
                "Discarded"
            } else if !word.is_dirty() {
                "not dirty"
            } else if pending {
                "its clear pending"
            } else {
                continue;
            };
            self.fail(format!(
                "a change of {node} through '{}' below {parent}, {why}: the served chain rule \
                 refuses it",
                kind.name
            ));
            return;
        }
    }

    /// Climb first (H8): a staged change's marks of the directories it changes below and their
    /// ancestors precede its change, its guard armed, and a directory not holding its base marks
    /// from the call's begin to the change was marked by it. Its change is its publish, move or
    /// rename, or its modify's or delete level's `Word`.
    fn climb(&mut self, begin: u64, climb: &mut Climb) {
        match (&self.record.effect, &climb.changed) {
            (&Effect::Arm(arm), Some((node, from, _))) => {
                let start = match arm {
                    Arm::ModifyMovedClimb => self.state.slot(*node).map(|slot| slot.record.parent),
                    Arm::MoverBeaten => Some(*from),
                    _ => None,
                };
                if let Some(start) = start {
                    climb.later.extend(self.state.ancestry(start));
                }
            }
            (
                &Effect::Word {
                    node,
                    record: Words(is),
                    ..
                },
                changed,
            ) if self.record.section == Section::UnservedMark => match changed {
                None => {
                    let was_staged = self
                        .state
                        .slot(node)
                        .is_some_and(|slot| slot.record.is_staged());
                    if is.is_staged() && !was_staged && !climb.staged.contains(&node) {
                        climb.staged.push(node);
                    }
                    let task = self.record.task;
                    if self.state.armed.get(&task).copied().unwrap_or(0) == 0 {
                        self.fail(format!(
                            "a staged change's climb marked {node} with no guard armed (H8)"
                        ));
                    }
                    if !climb.marked.contains(&node) {
                        climb.marked.push(node);
                    }
                }
                Some((_, _, above)) if above.contains(&node) && !climb.later.contains(&node) => {
                    self.fail(format!(
                        "a staged change's climb marked {node} after its change (H8)"
                    ));
                }
                Some(_) => {}
            },
            (_, None) => {
                let Some(change) = self.staged_change(begin) else {
                    return;
                };
                let climbed = |parent: NodeID, decided: bool| {
                    parent == ROOT_NODE
                        || climb.marked.contains(&parent)
                        || if decided {
                            self.state
                                .slot(parent)
                                .is_some_and(|slot| climb_stops(&slot.record))
                        } else {
                            self.state.held_base_marks(parent, begin)
                        }
                };
                let Change {
                    node,
                    left,
                    joined,
                    decided,
                } = change;
                // One of the parents the node had since the call began is the one it read.
                let left_skipped = (!left.is_empty()
                    && !left.iter().any(|&parent| climbed(parent, decided)))
                .then(|| left[0]);
                let joined_skipped = joined
                    .filter(|&parent| Some(parent) != left_skipped && !climbed(parent, false));
                // Each level its staged half set went on to its parent, which holds `Staged` now,
                // from this climb or from before it; but the root, and a detached directory, which
                // ends a climb. A mark of the parent's dirty half alone does not carry it.
                let stopped = climb.staged.iter().copied().find_map(|level| {
                    let parent = self.state.slot(level)?.record.parent;
                    let carried = parent == ROOT_NODE
                        || self.state.slot(parent).is_some_and(|slot| {
                            slot.record.is_staged() || slot.record.is_discarded()
                        });
                    (!carried).then_some((level, parent))
                });
                if let Some((level, parent)) = stopped {
                    self.fail(format!(
                        "a staged change's climb stopped at {level}, below {parent} lacking Staged \
                         (H8)"
                    ));
                }
                for parent in [left_skipped, joined_skipped].into_iter().flatten() {
                    self.fail(format!(
                        "a staged change of {node} skipped its climb at {parent}, lacking its base \
                         marks since the call began (H8)"
                    ));
                }
                let mut above = Vec::new();
                for &parent in left.iter().chain(&joined) {
                    above.extend(self.state.ancestry(parent));
                }
                let from = match self.record.effect {
                    Effect::Move { from, .. } => from,
                    _ => ROOT_NODE,
                };
                climb.changed = Some((node, from, above));
            }
            _ => {}
        }
    }

    /// The staged change the record makes, where it is its change.
    fn staged_change(&self, begin: u64) -> Option<Change> {
        let (node, left, joined, decided) = match self.record.effect {
            Effect::Publish { node, parent, .. } => (node, Vec::new(), Some(parent), false),
            Effect::Move { node, from, to, .. } => (
                node,
                self.state.parents_since(node, begin, from),
                Some(to),
                false,
            ),
            Effect::Rename { node, parent, .. } => (
                node,
                self.state.parents_since(node, begin, parent),
                Some(parent),
                false,
            ),
            Effect::Word { node, .. } if self.record.section == Section::StagedModify => {
                let parent = self.state.slot(node)?.record.parent;
                (
                    node,
                    self.state.parents_since(node, begin, parent),
                    None,
                    false,
                )
            }
            // Decided under the level's locks, on the parent it hangs from.
            Effect::Word { node, .. } if self.record.section == Section::DeleteLevel => {
                let parent = self.state.slot(node)?.record.parent;
                (node, vec![parent], None, true)
            }
            _ => return None,
        };
        Some(Change {
            node,
            left,
            joined,
            decided,
        })
    }

    /// The pre-test (H2), per attempt: no `Word` on the parent before an attempt's pre-test, as each
    /// attempt tests before it marks, and none after one that refused, as the detach then ends. An
    /// attempt ends where the detach goes round again, as its arms tell.
    fn pre_test_order(&mut self, kept: &mut Kept) {
        match self.record.effect {
            Effect::PreTest { node, refused, .. } => {
                let Some(parent) = self.state.slot(node).map(|slot| slot.record.parent) else {
                    return;
                };
                if kept.written.contains(&parent) {
                    self.fail(format!(
                        "a conditional detach of {node} wrote {parent} before its pre-test (H2)"
                    ));
                }
                kept.written.clear();
                kept.pre_test = Some(PreTested { parent, refused });
            }
            Effect::Arm(
                Arm::DetachNeedsMark
                | Arm::DetachChanged
                | Arm::DetachMoving
                | Arm::DetachMissing
                | Arm::DetachMovedMark,
            ) => kept.pre_test = None,
            Effect::Word { node, .. } => match kept.pre_test {
                None if !kept.written.contains(&node) => kept.written.push(node),
                Some(tested) if tested.refused && tested.parent == node => {
                    self.fail(format!(
                        "a conditional detach wrote {node} after its pre-test refused (H2)"
                    ));
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// A pre-test's answer, held to its condition on the model at its `seq`: the node not Discarded,
    /// and
    /// refused where its condition fails as `expected` names it.
    pub(super) fn pre_tested(&mut self, node: NodeID, refused: bool, expected: Expected) {
        let Some(slot) = self.state.slot(node) else {
            self.fail(format!(
                "a pre-test of {node}, which the model has no image of"
            ));
            return;
        };
        let (word, changes, chain_freed) = (slot.record, slot.changes, slot.chain_freed);
        if word.is_discarded() {
            self.fail(format!(
                "a pre-test of Discarded {node}, which a detach answers before it"
            ));
            return;
        }
        let refuses = match expected {
            // `IfClean`, whose condition carries nothing.
            Expected::Nothing => word.is_dirty(),
            Expected::Head(head) => {
                let front = self
                    .state
                    .chains
                    .get(&node)
                    .and_then(|chain| chain.front().copied());
                // A link's `child` is its pin, which no chain holds.
                let agrees = chain_freed
                    || !word.is_directory()
                    || if names_node(head) {
                        front == Some(head)
                    } else {
                        front.is_none()
                    };
                if !agrees {
                    self.fail(format!(
                        "a pre-test read {node}'s head {head}, where the model's chain begins at \
                         {front:?}"
                    ));
                    return;
                }
                word.is_directory() && names_node(head)
            }
            Expected::At { parent, name_hash } => {
                word.parent != parent || word.name_hash != name_hash
            }
            Expected::Count {
                changes: expected,
                instance,
            } => {
                let held = self.state.block(split(node).0).map(|block| block.instance);
                changes != expected || held != Some(instance)
            }
            other => {
                self.fail(format!(
                    "a pre-test of {node} expecting {other:?}, which no condition reads"
                ));
                return;
            }
        };
        if refuses != refused {
            self.fail(format!(
                "a pre-test of {node} answered refused {refused} on {expected:?}, where the model \
                 at its seq answers {refuses}"
            ));
        }
    }

    /// The moves permit taken, by the record's task, or about to be released: one holder at a time
    /// on a State (C10), released by the call that took it.
    pub(super) fn permit(&mut self, taken: bool, call: Option<u64>) {
        match (taken, self.state.permit) {
            (true, None) => {
                self.state.permit = Some(Permit {
                    call,
                    task: self.record.task,
                });
            }
            (true, Some(holder)) => self.fail(format!(
                "the moves permit taken while {holder:?} holds it (C10)"
            )),
            (false, Some(holder)) if holder.call == call => self.state.permit = None,
            (false, held) => self.fail(format!(
                "the moves permit released by {call:?}, where {held:?} holds it (C10)"
            )),
        }
    }

    /// A record of a kind holding the moves permit across its own steps, inside its own, or inside
    /// its batch's: one taken by its own task, in whose call's window it runs, or by a task its own
    /// was spawned from, up `spawns` (C10), as a C API delete's levels and detaches run in its
    /// batch's task or in futures it spawns, under the permit it holds.
    fn within_permit(&mut self, call: &Call, spawns: &[Spawn]) {
        let inside = self.state.permit.is_some_and(|permit| {
            permit.call == Some(call.id)
                || permit.task.is_some_and(|holder| {
                    call.task == Some(holder) || spawned_from(spawns, call.task, holder)
                })
        });
        if !inside {
            self.fail(format!(
                "a record of '{}' outside its own moves permit or its batch's (C10)",
                call.kind.name
            ));
        }
    }

    /// A reattach of a directory across parents, inside a moves permit: its call's own, where a
    /// call holds the move (C10). Every tier's reattach takes one for it.
    fn reattach_permitted(
        &mut self,
        node: NodeID,
        (from, to): (NodeID, NodeID),
        call: Option<u64>,
    ) {
        let directory = self
            .state
            .slot(node)
            .is_some_and(|slot| slot.status != Status::Free && slot.kind() == Kind::Directory);
        if !directory || from == to {
            return;
        }
        let inside = match call {
            Some(_) => self.state.permit.is_some_and(|permit| permit.call == call),
            None => self.state.permit.is_some(),
        };
        if !inside {
            self.fail(format!(
                "a reattach of directory {node} from {from} to {to} outside its moves permit (C10)"
            ));
        }
    }

    /// A reattach's `added` answer: the mover's `DirtyAdd` in force at its move.
    fn added(&mut self, node: NodeID, added: bool) {
        let Some(word) = self.state.slot(node).map(|slot| slot.record) else {
            return;
        };
        if word.is_dirty_add() != added {
            self.fail(format!(
                "a reattach of {node} answered added {added}, where its DirtyAdd in force at its \
                 move is {}",
                word.is_dirty_add()
            ));
        }
    }
}

impl super::StateModel {
    /// Whether `node` held its base marks at some `seq` from `begin` on: now, or until a `Word`
    /// or a free took them at or after `begin`.
    fn held_base_marks(&self, node: NodeID, begin: u64) -> bool {
        self.slot(node)
            .is_some_and(|slot| climb_stops(&slot.record))
            || self
                .base_marks_until
                .get(&node)
                .is_some_and(|&until| until >= begin)
    }

    /// The parents `node` had from `begin` on, the one then first, `now` its parent now.
    fn parents_since(&self, node: NodeID, begin: u64, now: NodeID) -> Vec<NodeID> {
        let mut parents: Vec<NodeID> = self
            .moved_from
            .get(&node)
            .into_iter()
            .flatten()
            .filter(|&&(seq, _)| seq >= begin)
            .map(|&(_, from)| from)
            .collect();
        if !parents.contains(&now) {
            parents.push(now);
        }
        parents
    }

    /// `node` and the directories above it up its chains, but the root.
    fn ancestry(&self, node: NodeID) -> Vec<NodeID> {
        let mut ancestry = Vec::new();
        let mut at = node;
        while at != ROOT_NODE && ancestry.len() <= self.blocks.len() * BLOCK_NODE_COUNT {
            ancestry.push(at);
            match self.slot(at) {
                Some(slot) if slot.status == Status::Chained => match slot.chained_in {
                    Some(directory) => at = directory,
                    None => break,
                },
                _ => break,
            }
        }
        ancestry
    }

    /// Notes that `node`'s words change at `seq` from `was`: a climb read it holding its base marks
    /// until then.
    pub(super) fn base_marks_left(&mut self, node: NodeID, was: &Node, seq: u64) {
        if climb_stops(was) {
            self.base_marks_until.insert(node, seq);
        }
    }
}
