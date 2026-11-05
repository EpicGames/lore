// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What C11c's second half holds a served State to, beside its closure: each clean directory's
//! children are its committed directory's, but along a swap's changed paths from its apply until
//! its settle. The model notes those paths as it replays a swap's records, by their calls; the
//! context names the committed State its served States settle against ([`Committed`]); the
//! checkpoint compares the two walks (`invariants::committed_children`).

use std::collections::BTreeMap;
use std::sync::Arc;

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::node::hooks::log::Effect;

use super::Replay;
use super::StateModel;
use super::Status;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Call;

/// What a served State of a context settles against ([`super::Modelled::committed`]): the
/// committed State, a State of the context's own, read through `repository`, and each node of the
/// served State with the committed node a settle of it compares with, as the settle hands
/// directories over by node.
pub struct Committed {
    /// The served State, by its log id.
    pub served: u64,
    pub state: Arc<State>,
    pub repository: Arc<RepositoryContext>,
    pub nodes: BTreeMap<NodeID, NodeID>,
}

/// Whether a record's call applies a swap's changes from its target, by its kind: unknown where
/// the replay was given no calls to tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applying {
    Swap,
    Other,
    Unknown,
}

impl Applying {
    /// What `call`, the record's call where one holds it, applies, in a replay given its run's
    /// calls (`called`) or not. A swap's copies, rewrites and detaches apply its target; a settle,
    /// a flip and a served mark change nothing a commit does not hold.
    pub(super) fn of(called: bool, call: Option<&Call>) -> Self {
        let swaps = [
            &catalogue::SWAP_ATTACH,
            &catalogue::SWAP_COPY_TREE,
            &catalogue::SWAP_REWRITE,
            &catalogue::SWAP_DETACH_IF_CLEAN,
        ];
        match call {
            Some(call) if swaps.into_iter().any(|kind| std::ptr::eq(call.kind, kind)) => Self::Swap,
            Some(_) => Self::Other,
            None if called => Self::Other,
            None => Self::Unknown,
        }
    }
}

impl Replay<'_> {
    /// Notes the changed path of a record a swap's call made on a served State, once it applied:
    /// the directories it changed below and those above them, and, for a copy it publishes or
    /// fills, the copy's nodes. Where no call tells a swap's records from another's, a change of a
    /// node leaves the State's changed paths unknown.
    pub(super) fn swapping(&mut self) {
        if !self.state.served {
            return;
        }
        // The nodes whose paths changed, the root ending one, and a copy's root.
        let (changed, copy) = match self.record.effect {
            Effect::Publish { node, parent, .. } | Effect::BuildAdd { node, parent, .. } => {
                ([parent, ROOT_NODE], Some(node))
            }
            Effect::Detach { parent, .. } | Effect::Rename { parent, .. } => {
                ([parent, ROOT_NODE], None)
            }
            Effect::Move { from, to, .. } => ([from, to], None),
            Effect::Word { node, .. } | Effect::Repin { node, .. } => ([node, ROOT_NODE], None),
            _ => return,
        };
        match self.applying {
            Applying::Other => {}
            Applying::Unknown => self.state.swaps_unknown = true,
            Applying::Swap => {
                for dir in changed {
                    self.state.swapped_from(dir);
                }
                if let Some(copy) = copy {
                    let nodes = self.state.subtree(copy);
                    self.state.swapped.extend(nodes);
                }
            }
        }
    }
}

impl StateModel {
    /// Notes `dir` and each directory above it, up its chains, as along a swap's changed paths.
    fn swapped_from(&mut self, dir: NodeID) {
        let mut at = dir;
        for _ in 0..=self.blocks.len() * BLOCK_NODE_COUNT {
            if at == ROOT_NODE {
                return;
            }
            self.swapped.insert(at);
            match self.slot(at) {
                Some(slot) if slot.status == Status::Chained => match slot.chained_in {
                    Some(directory) => at = directory,
                    None => return,
                },
                _ => return,
            }
        }
    }
}
