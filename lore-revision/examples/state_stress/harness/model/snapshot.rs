// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A State read for the model where its model starts: where a setup ends, recording off, or, for
//! a State a run loads, from a twin loaded afresh.
//!
//! Nothing is loaded into the State read: its blocks in memory are read as they stand, and the
//! rest, and the names whose table is not in memory, from a twin loaded from the revision the
//! State was last saved as or loaded from, which holds every block not in memory as the State
//! would load it. A setup's residency so keeps its meaning.

use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::node::hooks::log::Name;

/// One block as its model starts.
#[derive(Clone, Debug)]
pub struct BlockSnapshot {
    /// The block's instance where it was in memory, 0 where it was read from a twin.
    pub instance: u64,
    pub node_count: u32,
    /// The free list, its head first.
    pub free: Vec<u32>,
    /// Every slot's record.
    pub records: Vec<Node>,
    /// Every slot's chain count: 0 for a block read from a twin, whose load restarts them.
    pub changes: Vec<u32>,
    pub clear_pending: Vec<bool>,
    /// Each used slot's name, where it reads.
    pub names: Vec<Option<Name>>,
}

impl BlockSnapshot {
    /// A block holding no node: a zeroed block's, `node_count` 1 for block 0, whose slot 0 is the
    /// root.
    pub fn empty(instance: u64, node_count: u32) -> Self {
        Self {
            instance,
            node_count,
            free: Vec::new(),
            records: vec![Node::default(); BLOCK_NODE_COUNT],
            changes: vec![0; BLOCK_NODE_COUNT],
            clear_pending: vec![false; BLOCK_NODE_COUNT],
            names: vec![None; BLOCK_NODE_COUNT],
        }
    }

    /// `block`'s slots and free list, its counts and bits where `resident`, read under one guard.
    fn of(block: &NodeBlock, resident: bool) -> Self {
        let (node_count, free, records) = {
            let reader = block.read();
            let data = reader.node_block();
            let mut free = Vec::with_capacity(data.node_unused_count as usize);
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
        let (instance, changes, clear_pending) = if resident {
            (
                block.hook_instance(),
                (0..BLOCK_NODE_COUNT)
                    .map(|index| block.chain_changes(index))
                    .collect(),
                (0..BLOCK_NODE_COUNT)
                    .map(|index| block.clear_pending(index))
                    .collect(),
            )
        } else {
            (0, vec![0; BLOCK_NODE_COUNT], vec![false; BLOCK_NODE_COUNT])
        };
        Self {
            instance,
            node_count,
            free,
            records,
            changes,
            clear_pending,
            names: vec![None; BLOCK_NODE_COUNT],
        }
    }

    /// Whether each slot is in use: below `node_count` and not on the free list.
    pub(super) fn used(&self) -> Vec<bool> {
        let mut used = vec![false; BLOCK_NODE_COUNT];
        used[..(self.node_count as usize).min(BLOCK_NODE_COUNT)].fill(true);
        for &free in &self.free {
            if let Some(slot) = used.get_mut(free as usize) {
                *slot = false;
            }
        }
        used
    }

    /// The slots in use, in order.
    fn used_slots(&self) -> Vec<usize> {
        let used = self.used();
        (0..BLOCK_NODE_COUNT).filter(|&slot| used[slot]).collect()
    }
}

/// A State as its model starts.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub served: bool,
    pub unfinished: bool,
    pub blocks: Vec<BlockSnapshot>,
}

impl Snapshot {
    /// `state` as it stands, at a quiescent point, read with nothing loaded into it. Its block
    /// count is its own once it read its tree, the twin's before: a State loaded reads its tree at
    /// its first need, and counts no block until then.
    pub async fn read(state: &Arc<State>, repository: &Arc<RepositoryContext>) -> Self {
        let mut twin: Option<Arc<State>> = None;
        let count = match state.block_count() {
            0 if !state.revision().is_zero() => {
                let twin = twin_of(&mut twin, state, repository).await;
                twin.tree(repository.clone())
                    .await
                    .expect("the twin's tree reads");
                twin.block_count()
            }
            count => count,
        };
        let mut blocks = Vec::with_capacity(count);
        for index in 0..count {
            let block = if let Some(block) = state.resident_block(index) {
                let mut read = BlockSnapshot::of(&block, true);
                if block.is_nametable_deserialized() {
                    for slot in read.used_slots() {
                        read.names[slot] = block.read_node_and_name(slot, |_, name| {
                            name.ok().map(|name| Name::new(name.as_bytes()))
                        });
                    }
                } else {
                    let twin = twin_of(&mut twin, state, repository).await;
                    read_names(&mut read, &twin, repository, index).await;
                }
                read
            } else {
                let twin = twin_of(&mut twin, state, repository).await;
                let block = twin
                    .block(repository.clone(), index)
                    .await
                    .expect("the twin's block loads");
                let mut read = BlockSnapshot::of(&block, false);
                read_names(&mut read, &twin, repository, index).await;
                read
            };
            blocks.push(block);
        }
        Self {
            served: state.is_served(),
            unfinished: state.has_unfinished_change(),
            blocks,
        }
    }

    /// The record of `node` and its name where read, where its slot is in the snapshot.
    pub(crate) fn slot(&self, node: NodeID) -> Option<(&Node, Option<&Name>)> {
        let block = self.blocks.get(NodeBlock::index(node))?;
        let slot = Node::index(node);
        Some((block.records.get(slot)?, block.names.get(slot)?.as_ref()))
    }

    /// The children of `dir` in its chain, its child then each sibling, each with its record and
    /// its name where read, walked as they are taken: none for a file; `None` for a link, whose
    /// children lie in another State, or a node outside the snapshot. Where the chain leaves the
    /// snapshot or runs longer than it holds nodes, a last `None`.
    pub(crate) fn children(
        &self,
        dir: NodeID,
    ) -> Option<impl Iterator<Item = Option<(NodeID, &Node, Option<&Name>)>>> {
        let (record, _) = self.slot(dir)?;
        if record.is_link() {
            return None;
        }
        let mut next = if record.is_file() {
            None
        } else {
            record.child()
        };
        let mut left = self.blocks.len() * BLOCK_NODE_COUNT;
        Some(std::iter::from_fn(move || {
            let child = next?;
            let slot = left.checked_sub(1).and_then(|rest| {
                left = rest;
                self.slot(child)
            });
            next = slot.and_then(|(record, _)| record.sibling());
            Some(slot.map(|(record, name)| (child, record, name)))
        }))
    }

    /// The State a load of `revision` makes, none of its blocks in memory yet.
    pub async fn of_revision(repository: &Arc<RepositoryContext>, revision: Hash) -> Self {
        let twin = State::deserialize(repository.clone(), revision)
            .await
            .expect("the twin loads");
        twin.tree(repository.clone())
            .await
            .expect("the twin's tree reads");
        let mut blocks = Vec::with_capacity(twin.block_count());
        for index in 0..twin.block_count() {
            let block = twin
                .block(repository.clone(), index)
                .await
                .expect("the twin's block loads");
            let mut read = BlockSnapshot::of(&block, false);
            read_names(&mut read, &twin, repository, index).await;
            blocks.push(read);
        }
        Self {
            blocks,
            ..Self::default()
        }
    }
}

/// The twin of `state`, loaded at its first use from the revision `state` stands on.
async fn twin_of(
    twin: &mut Option<Arc<State>>,
    state: &State,
    repository: &Arc<RepositoryContext>,
) -> Arc<State> {
    if let Some(twin) = twin {
        return twin.clone();
    }
    let revision = state.revision();
    assert!(
        !revision.is_zero(),
        "a State never saved holds every block in memory"
    );
    let loaded = State::deserialize(repository.clone(), revision)
        .await
        .expect("the twin loads");
    *twin = Some(loaded.clone());
    loaded
}

/// The names of `read`'s used slots, block `index`'s, from `twin`, its block and name table
/// loaded once.
async fn read_names(
    read: &mut BlockSnapshot,
    twin: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    index: usize,
) {
    let block = twin
        .block_with_nametable(repository.clone(), index)
        .await
        .expect("the twin's block and its names load");
    for slot in read.used_slots() {
        read.names[slot] = block.read_node_and_name(slot, |_, name| {
            name.ok().map(|name| Name::new(name.as_bytes()))
        });
    }
}
