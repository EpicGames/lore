// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The closure exceptions on an unserved State, narrowed to what a dropped or failed call left
//! (C12): each (dirty node, clean directory) pair one of its own records formed, a future it
//! spawned's included, kept while it stands, until the directory turns dirty or the node clean,
//! moves to another directory or leaves the tree. The model checks closure itself on a State
//! holding such pairs, all but those; what excuses closure on the whole State, a task dropped or a
//! read failed outside every call, or an unfinished change the State held as the run began, is the
//! checkpoint's ([`super::checkpoint::rules`]).

use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::Effect;

use super::StateModel;
use super::Status;

/// The node `effect` writes a breach of closure about, by its words or its place: a rename's name
/// changes neither.
pub(super) fn written(effect: &Effect) -> Option<NodeID> {
    match *effect {
        Effect::Word { node, .. } | Effect::Publish { node, .. } | Effect::Move { node, .. } => {
            Some(node)
        }
        _ => None,
    }
}

impl StateModel {
    /// The breaches of closure about `node`: `node` dirty in a clean live directory's chain, and
    /// `node` a clean live directory over a dirty node of its chain.
    pub(super) fn breaches_about(&self, node: NodeID) -> Vec<(NodeID, NodeID)> {
        let mut breaches = Vec::new();
        if let Some(directory) = self.breach_above(node) {
            breaches.push((node, directory));
        }
        if self.clean_directory(node) {
            let dirty = (self.chains.get(&node).into_iter().flatten())
                .filter(|&&child| self.slot(child).is_some_and(|slot| slot.record.is_dirty()));
            breaches.extend(dirty.map(|&child| (child, node)));
        }
        breaches
    }

    /// Excuses the breaches about `node` that a record of a call left part way formed: those
    /// standing now and not before it, `before`.
    pub(super) fn excuse(&mut self, node: NodeID, before: &[(NodeID, NodeID)]) {
        for pair in self.breaches_about(node) {
            if !before.contains(&pair) {
                self.excused.insert(pair);
            }
        }
    }

    /// Keeps the excused pairs that still stand, once a change applied: a breach of the same pair
    /// after one ended is a new one.
    pub(super) fn unexcuse(&mut self) {
        if self.excused.is_empty() {
            return;
        }
        let mut excused = std::mem::take(&mut self.excused);
        excused.retain(|&(node, directory)| self.breach_above(node) == Some(directory));
        self.excused = excused;
    }

    /// The clean live directory whose chain holds `node`, where `node` is dirty, breaching closure,
    /// as the invariants walk the chains.
    fn breach_above(&self, node: NodeID) -> Option<NodeID> {
        let slot = self.slot(node)?;
        let directory = slot.chained_in.filter(|_| slot.status == Status::Chained)?;
        (node != ROOT_NODE && slot.record.is_dirty() && self.clean_directory(directory))
            .then_some(directory)
    }

    /// Whether `node` is a live directory but the root, not dirty.
    fn clean_directory(&self, node: NodeID) -> bool {
        node != ROOT_NODE
            && self
                .slot(node)
                .is_some_and(|slot| slot.record.is_directory() && !slot.record.is_dirty())
            && self.is_live(node)
    }

    /// Each breach of closure the model holds that no dropped or failed call left: a dirty node in
    /// a clean live directory's chain, not excused.
    pub fn closure_breaches(&self) -> Vec<String> {
        self.nodes()
            .filter_map(|node| self.breach_above(node).map(|directory| (node, directory)))
            .filter(|pair| !self.excused.contains(pair))
            .map(|(node, directory)| {
                format!(
                    "dirty {node} under clean {directory}, which no dropped or failed call left"
                )
            })
            .collect()
    }
}
