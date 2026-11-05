// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The sweeps' rules (C15).
//!
//! A sweep's call is judged by a plan read from the model as the call begins, as nothing runs
//! beside a sweep: each root, a slot `Discarded` and neither live nor free, walked as the sweep
//! walks its subtree; the roots whose subtrees hold a node the call retains, kept whole. Each of its
//! frees must be of a member of a subtree not kept; and, where it answers done with no read failing,
//! every member of a subtree whose root lies in a block in memory as it began must be free at its
//! end, as the sweep scans every such block a slot in which turned detached or was taken. The
//! frees come in the drain's and the sweep's own sections, `DiscardFree`; the scan writes no record.
//! On a served State every free in that section is a sweep's, as no discard and no pending queue
//! runs there, so a sweep made outside every call, as a scenario's check makes one, frees published
//! slots there too.

use std::collections::BTreeSet;

use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Kind;

use super::Model;
use super::Replay;
use super::StateModel;
use super::Status;
use super::Violation;
use super::split;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;

/// Whether `kind` is a sweep's: the unserved one or a VFS provider's.
pub(super) fn sweeps(kind: &catalogue::Kind) -> bool {
    std::ptr::eq(kind, &catalogue::SWEEP) || std::ptr::eq(kind, &catalogue::SERVED_SWEEP)
}

/// What a sweep's call may free and must, read from the model as it began.
#[derive(Clone, Debug)]
pub(super) struct SweepPlan {
    /// The members of every subtree no retained node keeps: what it may free.
    may_free: BTreeSet<NodeID>,
    /// Of those, the members of the subtrees whose roots lie in blocks in memory as it began: what
    /// it must free.
    must_free: BTreeSet<NodeID>,
}

impl StateModel {
    /// Every slot neither free nor live.
    fn unreleased_slots(&self) -> impl Iterator<Item = NodeID> + '_ {
        self.nodes().filter(|&node| {
            self.slot(node)
                .is_some_and(|slot| slot.status != Status::Free)
                && !self.is_live(node)
        })
    }

    /// `root` and every node below it, walked as the sweep walks a root's subtree: through the
    /// chains its directories hold, never the root's own siblings; a chain ending at a free slot, as
    /// a discard that failed part way freed in place, or at a node naming another parent.
    pub(super) fn sweep_members(&self, root: NodeID) -> Vec<NodeID> {
        let mut members = vec![root];
        let directory = |node: NodeID| {
            self.slot(node)
                .is_some_and(|slot| slot.kind() == Kind::Directory)
        };
        let mut directories = if directory(root) {
            vec![root]
        } else {
            Vec::new()
        };
        let most = self.blocks.len() * lore_revision::node::BLOCK_NODE_COUNT;
        while let Some(dir) = directories.pop() {
            for at in self.chains.get(&dir).into_iter().flatten().copied() {
                let Some(slot) = self.slot(at) else {
                    break;
                };
                if slot.status == Status::Free || slot.record.parent != dir {
                    break;
                }
                members.push(at);
                if directory(at) {
                    directories.push(at);
                }
                if members.len() > most {
                    return members;
                }
            }
        }
        members
    }

    /// The plan of sweep `call`, read from the model as it begins. The pending queue's subtrees
    /// the drain before the scan frees whole, whatever is retained, loading what it needs. Of the
    /// other roots, where it retains nodes, only those in blocks in memory are walked, as it scans
    /// no other: a root of a block out of memory, whose subtree holds a retained node, keeps
    /// nothing of a root nested in it that the sweep scans. Where it retains none, every root is,
    /// as the drain may load a block, which the scan then reads.
    fn sweep_plan(&self, call: &Call) -> SweepPlan {
        let (retained, resident) = call.sweeping.as_ref().map_or((&[][..], &[][..]), |on| {
            (on.retained.as_slice(), on.resident.as_slice())
        });
        let scanned = |node: NodeID| resident.binary_search(&split(node).0).is_ok();
        let drained: BTreeSet<NodeID> = (self.pending.iter())
            .flat_map(|&root| self.sweep_members(root))
            .collect();
        let roots: Vec<NodeID> = self
            .unreleased_slots()
            .filter(|&node| self.slot(node).is_some_and(super::Slot::is_discarded))
            .filter(|node| !drained.contains(node))
            .filter(|&node| retained.is_empty() || scanned(node))
            .collect();
        let mut kept = BTreeSet::new();
        let mut walked = Vec::with_capacity(roots.len());
        for root in roots {
            let members = self.sweep_members(root);
            if members
                .iter()
                .any(|member| retained.binary_search(member).is_ok())
            {
                kept.extend(members);
            } else {
                walked.push((root, members));
            }
        }
        let mut plan = SweepPlan {
            may_free: drained.clone(),
            must_free: drained,
        };
        for (root, members) in walked {
            let scanned = scanned(root);
            for member in members.into_iter().filter(|member| !kept.contains(member)) {
                plan.may_free.insert(member);
                if scanned {
                    plan.must_free.insert(member);
                }
            }
        }
        plan
    }
}

impl Model {
    /// Sweep `call`, begun where the replay stands: its plan, read from its State's model.
    pub(super) fn sweep_begun(&mut self, call: &Call) {
        if let Some(state) = self.states.get_mut(&call.state) {
            let plan = state.sweep_plan(call);
            state.sweeps.insert(call.id, plan);
        }
    }

    /// Sweep `call`, ended where the replay stands: where it answered done with no read failing,
    /// every slot its plan must free is free.
    pub(super) fn sweep_ended(&mut self, call: &Call) {
        let Some(state) = self.states.get_mut(&call.state) else {
            return;
        };
        let Some(plan) = state.sweeps.remove(&call.id) else {
            return;
        };
        if !matches!(call.outcome, Outcome::Done(_)) || call.failed_reads != 0 {
            return;
        }
        let left: Vec<NodeID> = plan
            .must_free
            .iter()
            .copied()
            .filter(|&node| {
                state
                    .slot(node)
                    .is_some_and(|slot| slot.status != Status::Free)
            })
            .collect();
        if !left.is_empty() {
            self.violations.push(Violation {
                state: call.state,
                seq: call.end,
                sub: 0,
                what: format!(
                    "a call of '{}' answered done, leaving {left:?} of the subtrees it scanned \
                     unfreed, none of which it retains",
                    call.kind.name
                ),
                write_set: false,
            });
        }
    }
}

impl Replay<'_> {
    /// A discard's write of directory `node`'s head as `head`: where `head` is a node of its chain
    /// past the first, the discard failed part way on a block it could not load and made the first
    /// node it left unfreed the head, every node before it freed (`take_unfreed`), which leave the
    /// chain.
    pub(super) fn unfreed_head(&mut self, node: NodeID, head: NodeID) {
        let Some(at) = (self.state.chains.get(&node))
            .and_then(|chain| chain.iter().position(|&member| member == head))
            .filter(|&at| at > 0)
        else {
            return;
        };
        let passed: Vec<NodeID> = self.state.chains[&node].iter().take(at).copied().collect();
        let unfreed: Vec<NodeID> = (passed.iter().copied())
            .filter(|&member| {
                self.state
                    .slot(member)
                    .is_some_and(|slot| slot.status != Status::Free)
            })
            .collect();
        if !unfreed.is_empty() {
            self.fail(format!(
                "a discard made {head} the head of {node}, passing over {unfreed:?}, not freed"
            ));
            return;
        }
        if let Some(chain) = self.state.chains.get_mut(&node) {
            chain.drain(..at);
        }
        for member in passed {
            if let Some(slot) = self.state.slot_mut(member) {
                slot.chained_in = None;
            }
        }
    }

    /// A free in the drain's or a sweep's section, of `node`: one a sweep's call makes is of a member
    /// of a subtree its plan does not keep.
    pub(super) fn swept(&mut self, node: NodeID) {
        let Some(call) = self.sweeping else {
            return;
        };
        let Some(plan) = self.state.sweeps.get(&call) else {
            self.fail(format!(
                "a free of {node} in a sweep's call {call}, which the model holds no plan of"
            ));
            return;
        };
        if !plan.may_free.contains(&node) {
            self.fail(format!(
                "a sweep freed {node}, live, free, retained, or in no subtree it walks"
            ));
        }
    }
}
