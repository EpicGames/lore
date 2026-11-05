// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The C API delete's pending queue: added subtrees a delete detaches under a shared claim, kept
//! whole until the C API commit frees them under its exclusive claim (H18).

use std::sync::Arc;

use crossbeam_queue::SegQueue;
use smallvec::SmallVec;

use super::Discard;
use super::discard::DiscardCounts;
use super::discard::free_resolved;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::SiblingCycleGuard;
use crate::repository::RepositoryContext;
use crate::state::Detach;
use crate::state::State;
use crate::state::StateError;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

impl State {
    /// Detaches `root`, an added node standing in `parent` under `name_hash`, for a C API delete,
    /// and marks it and every node below it `Discarded`, keeping their slots, chains and types for
    /// the C API commit to free ([`Self::node_free_pending`]). No parent is marked: the add of
    /// `root` marked its ancestors. Answers how many nodes it marked, `root` included, where a node
    /// staged for deletion and every node below it are marked but not counted, as the C API
    /// delete's check of an added subtree passes such a subtree over, or `None` where `root` no
    /// longer stands there.
    ///
    /// `root` is detached under its parent's lock and queued in the same poll. The nodes below it
    /// are marked top-down, each run of a chain lying in one block under one write lock of it,
    /// each node's chain head read under that lock: a publish into a node lands under that lock
    /// too, so it lands before the mark and its node is reached, or after it and is refused, as a
    /// C API add refuses a `Discarded` parent ([`Self::node_add_staged`]). Readers show a pending
    /// node as a freed slot reads.
    ///
    /// Runs under the shared claim with the State's moves permit held, so no move or other
    /// delete changes the subtree meanwhile. A node whose block fails to load ends the marking of
    /// its chain, the other chains marked still, and the error is answered once they are: the
    /// part left unmarked lies below a detached root, which the commit still frees whole.
    pub async fn node_discard_pending(
        &self,
        repository: Arc<RepositoryContext>,
        root: NodeID,
        parent: NodeID,
        name_hash: u64,
    ) -> Result<Option<u64>, StateError> {
        if !self
            .detach_child::<Discard>(repository.clone(), root, Detach::At(parent, name_hash))
            .await?
        {
            return Ok(None);
        }
        self.pending
            .get_or_init(|| Box::new(SegQueue::new()))
            .push(root);
        // The block of the last node read, as a chain's nodes mostly share one.
        let (head, mut held) = {
            let block_index = NodeBlock::index(root);
            let block = match self.resident_block(block_index) {
                Some(block) => block,
                None => self.block(repository.clone(), block_index).await?,
            };
            let node = block.node(Node::index(root));
            let head = if node.is_directory() {
                node.child
            } else {
                INVALID_NODE
            };
            (head, Some((block_index, block)))
        };
        let mut count = 1;
        let mut failed = None;
        // The chains still to mark, each from its head, and whether their nodes count.
        // Ten inline, as the flag widens each to 12 bytes and the vector lives across awaits.
        let mut chains: SmallVec<[(NodeID, NodeID, bool); 10]> = SmallVec::new();
        chains.push((root, head, true));
        while let Some((directory, head, counting)) = chains.pop() {
            let mut cycle = SiblingCycleGuard::new(directory);
            let mut at = head;
            while at.is_valid_node_id() {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::Discard).await;
                let block_index = NodeBlock::index(at);
                let block = match &held {
                    Some((index, block)) if *index == block_index => block,
                    _ => {
                        let block = match self.resident_block(block_index) {
                            Some(block) => block,
                            None => match self.block(repository.clone(), block_index).await {
                                Ok(block) => block,
                                Err(error) => {
                                    failed.get_or_insert(error);
                                    break;
                                }
                            },
                        };
                        &held.insert((block_index, block)).1
                    }
                };
                let (dirtied, broken) = {
                    #[cfg(feature = "state-hooks")]
                    let _label = log::label(log::Section::DiscardMarks);
                    let mut writer = block.write();
                    let mut changed = false;
                    let broken = loop {
                        let node = writer.node(Node::index(at));
                        if let Err(error) = node.walk_step(at, directory, &mut cycle) {
                            break Some(error);
                        }
                        node.flags |= NodeFlags::Discarded.bits();
                        changed = true;
                        #[cfg(feature = "state-hooks")]
                        log::record(&self.hooks, log::Effect::Discarding { node: at });
                        // A node staged for deletion is not counted, nor anything below it, live
                        // though a delete that failed part way may have left it.
                        let counted = counting && !node.is_staged_delete();
                        count += u64::from(counted);
                        if node.is_directory() && node.child.is_valid_node_id() {
                            chains.push((at, node.child, counted));
                        }
                        at = node.sibling;
                        if !at.is_valid_node_id() || NodeBlock::index(at) != block_index {
                            break None;
                        }
                    };
                    (changed && writer.mark_dirty(), broken)
                };
                if dirtied {
                    self.block_modified(block.clone(), block_index);
                }
                if let Some(error) = broken {
                    failed.get_or_insert(error);
                    break;
                }
            }
        }
        match failed {
            Some(error) => Err(error),
            None => Ok(Some(count)),
        }
    }

    /// Frees, under an exclusive claim, every subtree C API deletes queued
    /// ([`Self::node_discard_pending`]): each walked through the chains its nodes kept, then freed
    /// a block at a time, skipping a slot already free. The walk fails at a node of another parent
    /// or one met again, where the sweep's walk (`sweep_subtree`) ends the chain: no slot in a
    /// pending subtree is taken again, so either is a broken chain, and the nodes past it, which
    /// the marking never reached, no sweep would free. The walk holds every block it read, so
    /// the frees, which follow it in the same poll, load nothing: an error, or the drain dropped,
    /// before them frees nothing and leaves the queue as it was. The frees move the State's
    /// reclaim count in their poll (C15). The C API commit drains it after its last refusal and
    /// before its snapshot, so a refused commit frees nothing.
    pub async fn node_free_pending(
        &self,
        repository: &Arc<RepositoryContext>,
    ) -> Result<DiscardCounts, StateError> {
        let Some(queue) = self.pending.get() else {
            return Ok(DiscardCounts::default());
        };
        let mut popped = Popped {
            queue,
            roots: std::iter::from_fn(|| queue.pop()).collect(),
            freed: false,
        };
        if popped.roots.is_empty() {
            return Ok(DiscardCounts::default());
        }
        let mut collected = Vec::with_capacity(popped.roots.len());
        // Every block the walk read, by index, kept until the frees are done.
        let mut blocks: SmallVec<[(usize, Arc<NodeBlock>); 4]> = SmallVec::new();
        let mut chains: SmallVec<[(NodeID, NodeID); 16]> = SmallVec::new();
        for &root in &popped.roots {
            chains.push((INVALID_NODE, root));
            while let Some((directory, head)) = chains.pop() {
                let mut cycle = SiblingCycleGuard::new(directory);
                let mut at = head;
                while at.is_valid_node_id() {
                    let block_index = NodeBlock::index(at);
                    let slot = match blocks.binary_search_by_key(&block_index, |(index, _)| *index)
                    {
                        Ok(slot) => slot,
                        Err(slot) => {
                            let block = if let Some(block) = self.resident_block(block_index) {
                                block
                            } else {
                                #[cfg(feature = "state-hooks")]
                                crate::state::node::hooks::reached(
                                    crate::state::node::hooks::Point::BlockLoad,
                                )
                                .await;
                                self.block(repository.clone(), block_index).await?
                            };
                            blocks.insert(slot, (block_index, block));
                            slot
                        }
                    };
                    let block = &blocks[slot].1;
                    let node = block.node(Node::index(at));
                    // The root's own one-node chain: it left its parent's chain, so only the
                    // walk's steps below it check their parent, and a step that meets the root
                    // again fails that check rather than walking it twice.
                    let own = directory == INVALID_NODE;
                    if own {
                        cycle.observe(at).map_err(StateError::from)?;
                    } else {
                        node.walk_step(at, directory, &mut cycle)?;
                    }
                    collected.push(at);
                    if node.is_directory() && node.child.is_valid_node_id() {
                        chains.push((at, node.child));
                    }
                    // A root's sibling is the chain it left, not its own.
                    at = if own { INVALID_NODE } else { node.sibling };
                }
            }
        }
        collected.sort_unstable();
        // Before the frees: they find every block held, so they free all, and a missing block,
        // which would stop them part way, must leak the roots rather than queue freed slots.
        popped.freed = true;
        let freed = free_resolved(self, &blocks, &collected);
        self.note_reclaim();
        freed
    }
}

/// Roots a drain took from the pending queue, pushed back unless the drain reached its frees: a
/// drain that fails or is dropped before them, which run in one poll once every block is loaded,
/// leaves the queue as it found it.
struct Popped<'a> {
    queue: &'a SegQueue<NodeID>,
    roots: Vec<NodeID>,
    /// The drain reached its frees, so the roots are its own whatever the frees answer.
    freed: bool,
}

impl Drop for Popped<'_> {
    fn drop(&mut self) {
        if !self.freed {
            for &root in &self.roots {
                self.queue.push(root);
            }
        }
    }
}
