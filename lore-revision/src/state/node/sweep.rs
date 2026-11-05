// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The sweep of detached slots (C15). Nothing else frees a slot flagged `Discarded` that is on no
//! free list and in no chain: every other free reaches slots through live chains, the pending
//! queue or the owner's own id, and a save writes the slot as it is and a load restores it, so it
//! would keep its record, its name and its block's room for good. A sweep frees each with the
//! subtree below it where nothing else runs on the State. A block flags itself where a slot in it
//! turns detached or is taken, and when it loads, so a sweep reads only the blocks that may hold
//! one.

use std::sync::Arc;

use lore_base::lore_debug;
use smallvec::SmallVec;

use super::ServedNodes;
use super::discard::DiscardCounts;
use super::discard::SLOT_WORDS;
use super::discard::free_list_slots;
use super::discard::free_resolved;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeBlockData;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::SiblingCycleGuard;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

impl State {
    /// Frees, under an exclusive claim with nothing else running on the State, the subtrees the
    /// C API delete queued ([`Self::node_free_pending`]), then every slot a block in memory holds
    /// detached or taken and never published, with the subtree below it (C15). A root whose
    /// subtree reaches a block that fails to load stays for a later sweep; only the pending
    /// drain's error is answered.
    pub async fn node_sweep_detached(
        &self,
        repository: &Arc<RepositoryContext>,
    ) -> Result<DiscardCounts, StateError> {
        self.assert_unserved("node_sweep_detached");
        self.sweep_detached(repository, |_| false).await
    }
}

impl ServedNodes<'_> {
    /// [`State::node_sweep_detached`] on a served State, where every request, settle and swap the
    /// provider began has finished, none suspended part way, and the provider holds no
    /// unpublished build: the sweep frees every slot in flight, as it cannot tell one a call still
    /// owns from one a dropped call left (C15). A detached subtree any node of which `retained`
    /// names, the provider's test of whether the kernel still references a node, is kept whole for
    /// a later sweep. Every chain mark and absence proof taken before a sweep that frees a slot
    /// answers stale after it, so a slot taken again never reads as the node a listing saw.
    pub async fn node_sweep_detached(
        &self,
        repository: &Arc<RepositoryContext>,
        retained: impl Fn(NodeID) -> bool,
    ) -> Result<DiscardCounts, StateError> {
        self.state.sweep_detached(repository, retained).await
    }
}

/// The blocks a sweep read, sorted by index, each with its free slots, held until its frees: they
/// find every block a node lies in held, so they load nothing.
#[derive(Default)]
struct Held {
    blocks: Vec<(usize, Arc<NodeBlock>)>,
    free: Vec<FreeSlots>,
}

/// A block's slots that hold no node: on its free list, or past its node count.
struct FreeSlots {
    free_list: [u64; SLOT_WORDS],
    node_count: usize,
}

impl FreeSlots {
    fn of(data: &NodeBlockData) -> Self {
        Self {
            free_list: free_list_slots(data),
            node_count: data.node_count as usize,
        }
    }

    fn holds_no_node(&self, index: usize) -> bool {
        index >= self.node_count || self.free_list[index / 64] & (1 << (index % 64)) != 0
    }
}

impl Held {
    /// Where `block_index` is held, loading it where it is not.
    async fn slot(
        &mut self,
        state: &State,
        repository: &Arc<RepositoryContext>,
        block_index: usize,
    ) -> Result<usize, StateError> {
        match self.find(block_index) {
            Ok(slot) => Ok(slot),
            Err(slot) => {
                let block = match state.resident_block(block_index) {
                    Some(block) => block,
                    None => state.block(repository.clone(), block_index).await?,
                };
                let free = FreeSlots::of(block.read().node_block());
                self.blocks.insert(slot, (block_index, block));
                self.free.insert(slot, free);
                Ok(slot)
            }
        }
    }

    fn find(&self, block_index: usize) -> Result<usize, usize> {
        self.blocks
            .binary_search_by_key(&block_index, |(index, _)| *index)
    }
}

/// The detached roots of a flagged block's records, `data`, and its free slots; none where every
/// `Discarded` record is a free slot, which a count of them tells without walking the free list.
fn roots_in(
    block_index: usize,
    data: &NodeBlockData,
    roots: &mut Vec<NodeID>,
) -> Option<FreeSlots> {
    let records = &data.node[..data.node_count as usize];
    let discarded = records.iter().filter(|node| node.is_discarded()).count();
    if discarded == data.node_unused_count as usize {
        return None;
    }
    let free = FreeSlots::of(data);
    for (index, node) in records.iter().enumerate() {
        if node.is_discarded() && !free.holds_no_node(index) {
            roots.push(NodeBlock::node_id(block_index, index));
        }
    }
    Some(free)
}

impl State {
    /// The sweep of [`Self::node_sweep_detached`] and its served form. It drains the pending
    /// queue first, as every node of a pending subtree is `Discarded` and the scan would take each
    /// as a root of its own, walking the subtree once per node. It then scans every flagged block
    /// in memory, under its read lock, for `Discarded` slots that hold a node, and walks each such
    /// root's subtree. Then, with no await from there to its
    /// frees, so a sweep dropped before takes nothing, it takes the scanned blocks' flags and the
    /// State's notes, flags again the block of every root kept, and frees every subtree no node of
    /// which `retained` names and whose walk read every block it needed, in one pass.
    async fn sweep_detached(
        &self,
        repository: &Arc<RepositoryContext>,
        retained: impl Fn(NodeID) -> bool,
    ) -> Result<DiscardCounts, StateError> {
        let mut counts = self.node_free_pending(repository).await?;
        let (candidates, noted) = self.unswept_blocks();
        let mut held = Held::default();
        let mut flagged: Vec<&Arc<NodeBlock>> = Vec::new();
        let mut roots: Vec<NodeID> = Vec::new();
        for (block_index, block) in &candidates {
            let free = {
                let reader = block.read();
                if !reader.is_unswept() {
                    continue;
                }
                flagged.push(block);
                roots_in(*block_index, reader.node_block(), &mut roots)
            };
            if let Some(free) = free {
                let slot = held.find(*block_index).unwrap_or_else(|slot| slot);
                held.blocks.insert(slot, (*block_index, block.clone()));
                held.free.insert(slot, free);
            }
        }
        // The subtrees of the roots freed, and the roots kept with what their walks read: a root
        // nested in a kept subtree, as a discard that failed part way leaves one, is kept with it.
        let mut collected: Vec<NodeID> = Vec::new();
        let mut kept_roots: Vec<NodeID> = Vec::new();
        let mut kept: Vec<NodeID> = Vec::new();
        for &root in &roots {
            let start = collected.len();
            let walked = self
                .sweep_subtree(repository, &mut held, root, &mut collected)
                .await;
            if let Err(error) = &walked {
                lore_debug!("A sweep keeps detached node {root}: {error}");
            }
            if walked.is_err() || collected[start..].iter().any(|&member| retained(member)) {
                kept_roots.push(root);
                kept.extend(collected.drain(start..));
            }
        }
        kept.sort_unstable();
        collected.retain(|member| kept.binary_search(member).is_err());
        collected.sort_unstable();
        collected.dedup();
        for block in flagged {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Sweep);
            block.write().take_unswept();
        }
        for &root in &kept_roots {
            if let Ok(slot) = held.find(NodeBlock::index(root)) {
                #[cfg(feature = "state-hooks")]
                let _label = log::label(log::Section::Sweep);
                held.blocks[slot].1.write().mark_unswept();
            }
        }
        self.settle_unswept(
            &noted,
            kept_roots.iter().map(|&root| NodeBlock::index(root)),
        );
        counts.add(free_resolved(self, &held.blocks, &collected)?);
        if !collected.is_empty() {
            self.note_reclaim();
            lore_debug!(
                "A sweep freed {} detached nodes, kept {}",
                collected.len(),
                kept_roots.len()
            );
        }
        Ok(counts)
    }

    /// Appends `root` and every node of the subtree below it to `members`, through the chains its
    /// nodes kept. A root's own `sibling` is the chain it left and is never followed. A chain ends
    /// at a slot holding no node, where a discard that failed part way freed in place and the
    /// freed slot's `sibling` continues the free list; at a node of another parent, the chain's slot
    /// taken again; or at a cycle. The pending drain's walk fails at the last two instead
    /// ([`State::node_free_pending`]). Links are leaves. Fails where a block fails to load, leaving
    /// what it read in `members`.
    async fn sweep_subtree(
        &self,
        repository: &Arc<RepositoryContext>,
        held: &mut Held,
        root: NodeID,
        members: &mut Vec<NodeID>,
    ) -> Result<(), StateError> {
        let slot = held.slot(self, repository, NodeBlock::index(root)).await?;
        let node = held.blocks[slot].1.node(Node::index(root));
        members.push(root);
        // The chains still to walk, each with its directory.
        let mut chains: SmallVec<[(NodeID, NodeID); 8]> = SmallVec::new();
        if node.is_directory() && node.child.is_valid_node_id() {
            chains.push((root, node.child));
        }
        while let Some((directory, head)) = chains.pop() {
            let mut cycle = SiblingCycleGuard::new(directory);
            let mut at = head;
            while at.is_valid_node_id() {
                let slot = held.slot(self, repository, NodeBlock::index(at)).await?;
                if held.free[slot].holds_no_node(Node::index(at)) {
                    break;
                }
                let node = held.blocks[slot].1.node(Node::index(at));
                if node.parent != directory || cycle.observe(at).is_err() {
                    break;
                }
                members.push(at);
                if node.is_directory() && node.child.is_valid_node_id() {
                    chains.push((at, node.child));
                }
                at = node.sibling;
            }
        }
        Ok(())
    }
}
