// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model held against the live State at a quiescent point, over the blocks the State holds in
//! memory, loading none.

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::State;
use lore_revision::state::node::hooks::log::Kind;

use super::StateModel;
use super::Status;
use super::Unchained;
use super::names_node;

impl StateModel {
    /// Where the live `state` and the model differ, over the blocks `state` holds in memory: each
    /// block's instance, `node_count` and free list in order; each slot's chain count and pending
    /// clear bit; each used slot's record as a `Word` compares it, its parent and name hash, and
    /// its name where its block's name table is in memory, a free one's `Discarded` alone; the
    /// chain of each directory, live or detached, that no free reached since its detach, in order,
    /// where every block it reaches is in memory; whether the State is served and holds an
    /// unfinished change; and that no task holds a guard armed.
    pub fn compare(&self, state: &State) -> Vec<String> {
        let id = self.id;
        let mut differences = Vec::new();
        if state.is_served() != self.served {
            differences.push(format!(
                "State {id}: served {}, the model {}",
                state.is_served(),
                self.served
            ));
        }
        for (task, armed) in self.armed.iter().filter(|(_, armed)| **armed > 0) {
            differences.push(format!(
                "State {id}: task {task:?} holds {armed} guards armed at a quiescent point"
            ));
        }
        if state.has_unfinished_change() != self.unfinished {
            differences.push(format!(
                "State {id}: an unfinished change {}, the model {}",
                state.has_unfinished_change(),
                self.unfinished
            ));
        }
        for index in 0..state.block_count().max(self.blocks.len()) {
            let Some(block) = state.resident_block(index) else {
                continue;
            };
            let names = block.is_nametable_deserialized();
            let Some(modelled) = self.block(index) else {
                differences.push(format!(
                    "State {id}: block {index} is in memory, the model has no image of it"
                ));
                continue;
            };
            if block.hook_instance() != modelled.instance {
                differences.push(format!(
                    "State {id}: block {index}'s instance {}, the model's {}",
                    block.hook_instance(),
                    modelled.instance
                ));
            }
            let (node_count, free, records) = {
                let reader = block.read();
                let data = reader.node_block();
                let mut free = Vec::new();
                let mut at = data.node_unused;
                for _ in 0..data.node_unused_count {
                    if at as usize >= BLOCK_NODE_COUNT {
                        break;
                    }
                    free.push(at);
                    at = data.node[at as usize].sibling;
                }
                (data.node_count, free, data.node.to_vec())
            };
            if node_count != modelled.node_count {
                differences.push(format!(
                    "State {id}: block {index}'s node_count {node_count}, the model's {}",
                    modelled.node_count
                ));
            }
            let modelled_free: Vec<u32> = modelled.free.iter().rev().copied().collect();
            if free != modelled_free {
                differences.push(format!(
                    "State {id}: block {index}'s free list {free:?}, the model's {modelled_free:?}"
                ));
            }
            for (slot_index, (record, slot)) in records.iter().zip(&modelled.slots).enumerate() {
                let node = (index * BLOCK_NODE_COUNT + slot_index) as NodeID;
                if block.chain_changes(slot_index) != slot.changes {
                    differences.push(format!(
                        "State {id}: {node}'s chain count {}, the model's {}",
                        block.chain_changes(slot_index),
                        slot.changes
                    ));
                }
                if block.clear_pending(slot_index) != slot.clear_pending {
                    differences.push(format!(
                        "State {id}: {node}'s pending clear {}, the model's {}",
                        block.clear_pending(slot_index),
                        slot.clear_pending
                    ));
                }
                if slot_index >= node_count as usize {
                    continue;
                }
                if slot.status == Status::Free {
                    if record.flags & NodeFlags::Discarded.bits() == 0 {
                        differences.push(format!(
                            "State {id}: {node} is free in the model, not Discarded in the State"
                        ));
                    }
                    continue;
                }
                // A slot whose parent and name the model never learnt, a directory with no
                // content in flight or an abandoned build's root, is compared by its words.
                let structure = slot.placed;
                if !same(record, &slot.record, structure) {
                    differences.push(format!(
                        "State {id}: {node} is {record:?}, the model's {:?}",
                        slot.record
                    ));
                }
                // A name a record told; a slot never published was told none.
                if let Some(name) = slot.name.as_ref().filter(|_| names) {
                    // Compared where it is stored; copied only to name a difference.
                    let differs = block.read_node_and_name(slot_index, |_, stored| match stored {
                        Ok(stored) if stored.as_bytes() == name.as_bytes() => None,
                        Ok(stored) => Some(format!("'{stored}'")),
                        Err(error) => Some(format!("unreadable ({error:?})")),
                    });
                    if let Some(stored) = differs {
                        differences.push(format!(
                            "State {id}: {node}'s name {stored}, the model's '{}'",
                            String::from_utf8_lossy(name.as_bytes())
                        ));
                    }
                }
            }
        }
        // A directory in flight or of an abandoned build holds no chain the model keeps; one a
        // free reached since its detach holds one the State's points into a slot gone elsewhere.
        let directories: Vec<NodeID> = self
            .nodes()
            .filter(|&node| {
                self.slot(node).is_some_and(|slot| {
                    slot.kind() == Kind::Directory
                        && !slot.chain_freed
                        && !matches!(
                            slot.status,
                            Status::Free
                                | Status::InFlight
                                | Status::Unchained(Unchained::Abandoned)
                        )
                })
            })
            .collect();
        for directory in directories {
            if let Some(chain) = chain_of(state, directory)
                && chain != self.chain(directory)
            {
                differences.push(format!(
                    "State {id}: {directory}'s chain {chain:?}, the model's {:?}",
                    self.chain(directory)
                ));
            }
        }
        differences
    }
}

/// Whether two records agree on what the model holds of a node: what a `Word` compares, and,
/// where `structure`, the parent and name hash.
fn same(state: &Node, model: &Node, structure: bool) -> bool {
    state.flags == model.flags
        && state.ext_flags == model.ext_flags
        && state.address == model.address
        && state.size == model.size
        && state.mode == model.mode
        && (!structure || (state.parent == model.parent && state.name_hash == model.name_hash))
        && (!state.is_link() || state.child == model.child)
}

/// `directory`'s chain in the live State, where every block it reaches is in memory.
fn chain_of(state: &State, directory: NodeID) -> Option<Vec<NodeID>> {
    let record = |node: NodeID| {
        state
            .resident_block(NodeBlock::index(node))
            .map(|block| block.node(Node::index(node)))
    };
    let mut chain = Vec::new();
    let most = state.block_count() * BLOCK_NODE_COUNT;
    let mut at = record(directory)?.child;
    while names_node(at) {
        if chain.len() > most {
            break;
        }
        chain.push(at);
        at = record(at)?.sibling;
    }
    Some(chain)
}
