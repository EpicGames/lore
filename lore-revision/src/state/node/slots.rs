// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Slot allocation and release: the unused list, fresh blocks and node initialization.

use core::str;
use std::sync::Arc;
use std::sync::Weak;

use lore_error_set::prelude::*;
use zerocopy::FromZeros;

use crate::errors::Oversized;
use crate::lore::Hash;
use crate::lore::TypedBytes;
use crate::lore_warn;
use crate::node;
use crate::node::INVALID_BLOCK;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::repository::RepositoryContext;
use crate::state::MAX_TREE_BLOCK_COUNT;
use crate::state::State;
use crate::state::StateError;
use crate::state::TreeFlags;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

/// What a step that may not read the store answers when what it needs is not in memory.
pub(in crate::state) struct NotInMemory;

impl State {
    /// Clear stale per-node file metadata a freshly grabbed slot may still carry
    /// from whatever used to live there, in `block`, the slot's file metadata
    /// block where one exists. Where none exists there is nothing to carry, so
    /// nothing to clear.
    pub(in crate::state) fn clear_recycled_metadata(
        &self,
        block: Option<Arc<NodeFileMetadataBlock>>,
        node_id: NodeID,
    ) {
        let Some(metadata_block) = block else {
            return;
        };
        let metadata_node_id = node::node_to_file_metadata(node_id);
        let metadata_block_index = NodeFileMetadataBlock::index(metadata_node_id);
        let dirtied = {
            let mut block_lock = metadata_block.write();
            let node_metadata = block_lock.node(NodeFileMetadata::index(metadata_node_id));
            if node_metadata.metadata.is_zero() {
                false
            } else {
                node_metadata.metadata.zero();
                block_lock.mark_dirty()
            }
        };
        if dirtied {
            self.block_file_metadata_modified(metadata_block, metadata_block_index);
        }
    }

    /// Fill in a freshly grabbed slot with `node`, named `name`, below `parent`, returning the
    /// block that holds it.
    ///
    /// The slot is initialized fully before it is reachable, and stays `Discarded` until its
    /// publish clears that under the publish's locks (C2), so an attach dropped before its
    /// publish leaves a detached slot, never a typed one outside every chain (H8). The record is
    /// the node's own: `sibling` is set at the publish and `child` starts empty, but for a link,
    /// whose `child` is its source node in the linked State. The node's name hash is the
    /// caller's, checked under `state-hooks`.
    pub(in crate::state) async fn initialize_node(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        parent: NodeID,
        node: &Node,
        name: &str,
    ) -> Result<Arc<NodeBlock>, StateError> {
        #[cfg(feature = "state-hooks")]
        assert_eq!(
            node.name_hash,
            crate::hash::hash_string(name),
            "a node added as '{name}' carries another name's hash"
        );
        let block_index = NodeBlock::index(node_id);
        let block = self.block_with_nametable(repository, block_index).await?;
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Grab);
            let mut block_lock = block.write();
            let stored = block_lock.node_name_store(name, 0, 0);
            // A name the rules refuse is the caller's error, not a table with no room.
            #[cfg(feature = "state-hooks")]
            if stored.is_err() && crate::node::validate_node_name_for_store(name).is_ok() {
                log::name_table_full(&self.hooks, node_id);
            }
            let (name_offset, name_length) =
                stored.forward::<StateError>("Storing new node name")?;
            let target_node = block_lock.node(Node::index(node_id));
            *target_node = *node;
            target_node.flags |= NodeFlags::Discarded.bits();
            target_node.parent = parent;
            if !node.is_link() {
                target_node.child = 0;
            }
            target_node.name_offset = name_offset;
            target_node.name_length = name_length;
            block_lock.mark_dirty()
        };
        if dirtied {
            self.block_modified(block.clone(), block_index);
        }

        Ok(block)
    }

    /// Grab a free node slot, extending the tree when nothing is available.
    ///
    /// The common path takes no allocator permit. `grab_node_unused` mutates
    /// only the block it is called on and only under that block's write lock,
    /// so concurrent grabbers on the same block are already handed distinct
    /// slots. A block holds [`BLOCK_NODE_COUNT`] of them, so all but one add per
    /// block full is pure per-block work; the permit is needed only for the
    /// transitions that mutate the unused chain or the block vector.
    ///
    /// With `resident_only`, `None` where the head of the unused chain is not in memory, after
    /// retiring any full heads before it.
    pub(in crate::state) async fn grab_node_slot(
        &self,
        repository: Arc<RepositoryContext>,
        resident_only: bool,
    ) -> Result<Option<NodeID>, StateError> {
        loop {
            let head = self.unused_head();
            if (head as usize) < self.block_count() {
                let block = if resident_only {
                    let Some(block) = self.resident_block(head as usize) else {
                        #[cfg(feature = "state-hooks")]
                        log::arm(&self.hooks, log::Arm::NeedLoad);
                        return Ok(None);
                    };
                    block
                } else {
                    self.block(repository.clone(), head as usize).await?
                };
                if let Some(node_id) = self.try_grab_in(&block, head as usize) {
                    return Ok(Some(node_id));
                }

                {
                    let _permit = self.acquire_unused().await?;
                    self.retire_full_block(head, &block);
                }
                // The permit free and the chain perhaps empty, before the push that refills it.
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::Retry).await;
                continue;
            }

            let _permit = self.acquire_unused().await?;
            // Whoever held the permit before may have already published a block
            // with room, in which case the retry finds it on the fast path.
            if self.unused_head() != head {
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::AllocatorHeadMoved);
                continue;
            }

            if let Some((idx, block)) = self.try_recycle_last_block()
                && let Some(node_id) = self.try_grab_in(&block, idx)
            {
                self.push_unused_block_list(idx, &block);
                return Ok(Some(node_id));
            }

            let (idx, block) = self.allocate_fresh_block()?;
            let node_id = self.try_grab_in(&block, idx).ok_or_else(|| {
                StateError::internal(
                    "grab_node_unused returned INVALID on a freshly-allocated block",
                )
            })?;
            self.push_unused_block_list(idx, &block);
            return Ok(Some(node_id));
        }
    }

    /// Whether an attach is sure to take its slot without reading the store, as on a State
    /// taking new nodes from fresh blocks ([`Self::take_new_nodes_from_fresh_blocks`]): those
    /// blocks are dirty, so they stay in memory with their name tables, and have no stored file
    /// metadata.
    pub fn attach_slot_resident(&self) -> bool {
        self.runtime_read().fresh_blocks_from.is_some()
    }

    /// Makes new nodes take their slots from fresh blocks past the tree's, never from the free
    /// slots the tree's blocks hold: empties the unused chain and keeps the tree's last block from
    /// being recycled. A fresh block holds its name table in memory and has no stored file
    /// metadata, so an attach reads nothing from the store for its slot.
    ///
    /// Only for a State that is never serialized: the tree's free slots stay unused, and a
    /// serialize would store a tree whose unused chain leaves them out. Called before any writer
    /// runs on the State.
    pub async fn take_new_nodes_from_fresh_blocks(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<(), StateError> {
        self.tree(repository).await?;
        let mut runtime = self.runtime_write();
        let Some(tree) = runtime.tree.as_mut() else {
            return Err(StateError::internal("the tree is not loaded"));
        };
        tree.block_unused_first = INVALID_BLOCK;
        runtime.fresh_blocks_from = Some(runtime.block_count());
        Ok(())
    }

    /// What [`Self::try_block_file_metadata_existing`] answers for `block_index` from memory: the
    /// resident block, or `None` where the tree records no file metadata, the block is a fresh one
    /// of a State taking new nodes from fresh blocks, or the address list in memory has no entry or
    /// a zero hash at the index.
    pub(in crate::state) fn resident_file_metadata(
        &self,
        block_index: usize,
    ) -> Result<Option<Arc<NodeFileMetadataBlock>>, NotInMemory> {
        let runtime = self.runtime_read();
        if let Some(block) = runtime
            .block_file_metadata
            .get(block_index)
            .and_then(Weak::upgrade)
        {
            return Ok(Some(block));
        }
        let tree = runtime.tree.as_ref().ok_or(NotInMemory)?;
        // The stored list holds no entry past the block count of the tree it was stored with.
        if tree.hash_file_metadata.is_zero()
            || runtime
                .fresh_blocks_from
                .is_some_and(|from| block_index >= from)
        {
            return Ok(None);
        }
        // A list in memory covers every block the tree had when it was read or written, so a
        // block past its end was added since and has nothing stored.
        let list = runtime.block_file_metadata_address.as_type_slice::<Hash>();
        if list.is_empty() {
            return Err(NotInMemory);
        }
        if list.get(block_index).is_none_or(Hash::is_zero) {
            Ok(None)
        } else {
            Err(NotInMemory)
        }
    }

    /// [`Self::resident_file_metadata`] for the slot `node_id`, whose block and name table must be
    /// in memory too.
    pub(in crate::state) fn resident_slot_metadata(
        &self,
        node_id: NodeID,
        block_index: usize,
    ) -> Result<Option<Arc<NodeFileMetadataBlock>>, NotInMemory> {
        let block = self
            .resident_block(NodeBlock::index(node_id))
            .ok_or(NotInMemory)?;
        if !block.is_nametable_deserialized() {
            return Err(NotInMemory);
        }
        self.resident_file_metadata(block_index)
    }

    /// Holds `block`, the file metadata block of the block an attach took its slot from, in place
    /// of the one held before: loaded blocks are held only weakly, and the next attach, taking its
    /// slot from the same block, must find it in memory.
    pub(in crate::state) fn hold_attach_metadata(&self, block: &Arc<NodeFileMetadataBlock>) {
        let held = self
            .runtime_read()
            .attach_file_metadata
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(held, block));
        if !held {
            self.runtime_write().attach_file_metadata = Some(block.clone());
        }
    }

    #[lore_macro::test_pub]
    pub(in crate::state) async fn acquire_unused(
        &self,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, StateError> {
        // Taking the permit orders its holders, so the scheduler must not reorder two takes.
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::UnusedPermit(self.hooks.ordinal()),
            true,
        );
        #[cfg(feature = "state-hooks")]
        let _waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::UnusedPermit);
        self.unused.acquire().await.map_err(|error| {
            StateError::internal(format!("node allocation semaphore is closed: {error}"))
        })
    }

    #[lore_macro::test_pub]
    pub(in crate::state) fn unused_head(&self) -> u32 {
        self.runtime_read()
            .tree
            .as_ref()
            .map_or(INVALID_BLOCK, |tree| tree.block_unused_first)
    }

    /// Take one slot out of `block`, recording the dirty state a successful grab
    /// produces. `None` means the block had nothing left. The slot is `Discarded` from here to
    /// its publish (H8), and its block flagged for the next sweep, which frees it where a call
    /// dropped or failed meanwhile left it in flight (C15).
    #[lore_macro::test_pub]
    pub(in crate::state) fn try_grab_in(
        &self,
        block: &Arc<NodeBlock>,
        block_index: usize,
    ) -> Option<NodeID> {
        let (node_id, dirtied) = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Grab);
            let mut block_writer = block.write();
            let node_id = block_writer.grab_node_unused(block_index as u32);
            if node_id.is_valid_node_id() {
                block_writer.node(Node::index(node_id)).flags = NodeFlags::Discarded.bits();
                block_writer.mark_unswept();
                (node_id, block_writer.mark_dirty())
            } else {
                (node_id, false)
            }
        };
        if !node_id.is_valid_node_id() {
            return None;
        }
        if dirtied {
            self.block_modified(block.clone(), block_index);
        }
        Some(node_id)
    }

    /// Takes an exhausted block off the head of the unused chain.
    ///
    /// Fullness is re-tested here rather than trusted from the caller's failed
    /// grab: [`Self::release_node`] can return a slot to this block in between,
    /// and retiring it then would strand a block that has room. Holding the
    /// permit keeps that release from landing inside this check.
    pub(in crate::state) fn retire_full_block(&self, head: u32, block: &Arc<NodeBlock>) {
        let popped_dirty = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Allocator);
            let mut runtime = self.runtime_write();
            let Some(tree) = runtime.tree.as_mut() else {
                return;
            };
            if tree.block_unused_first != head {
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::RetireRetest);
                return;
            }
            let mut block_writer = block.write();
            if !block_writer.is_full() {
                #[cfg(feature = "state-hooks")]
                {
                    drop(block_writer);
                    log::arm(&self.hooks, log::Arm::RetireRetest);
                }
                return;
            }
            tree.block_unused_first = block_writer.block_unused_next();
            tree.flags |= TreeFlags::Dirty;
            block_writer.node_block().block_unused_next = INVALID_BLOCK;
            block_writer.mark_dirty()
        };
        if popped_dirty {
            self.block_modified(block.clone(), head as usize);
        }
    }

    /// Return a grabbed but unpublished node slot to its block's free list.
    ///
    /// Initialization runs after the allocator hands the slot out, so a failure
    /// there would otherwise leave it reachable from no chain until the next sweep
    /// (C15). The allocation permit is held for the free, which keeps it out of
    /// [`Self::retire_full_block`]'s test of the block's fullness; a grab from the head of the
    /// unused chain takes none.
    pub(in crate::state) async fn release_node(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
    ) {
        let block_index = NodeBlock::index(node_id);
        let Ok(block) = self.block(repository, block_index).await else {
            lore_warn!(
                "Node {node_id} is left to the next sweep: its block could not be read back"
            );
            return;
        };
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::UnusedPermit(self.hooks.ordinal()),
            true,
        );
        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::UnusedPermit);
        let _permit = self.unused.acquire().await;
        #[cfg(feature = "state-hooks")]
        drop(waiting);
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Release);
            let mut block_writer = block.write();
            block_writer.discard_node(block_index, Node::index(node_id));
            block_writer.mark_dirty()
        };
        if dirtied {
            self.block_modified(block, block_index);
        }
    }

    /// Return the most recently allocated block when it still has at least one
    /// free slot, without touching the unused chain. The caller is expected to
    /// attempt a grab before deciding whether to splice the block into the
    /// chain — so a block whose internal bookkeeping diverges from `is_full()`
    /// is not introduced into the chain where it could mislead future scans.
    pub(in crate::state) fn try_recycle_last_block(&self) -> Option<(usize, Arc<NodeBlock>)> {
        let runtime = self.runtime_read();
        let block_index = runtime.block.len().checked_sub(1)?;
        if runtime
            .fresh_blocks_from
            .is_some_and(|from| block_index < from)
        {
            return None;
        }
        let block = runtime.block[block_index].upgrade()?;
        if block.read().is_full() {
            return None;
        }
        Some((block_index, block))
    }

    /// Allocate a fresh `NodeBlock` and push it onto the runtime's block vector, leaving it
    /// out of the unused chain. Errors only when the per-tree block limit is reached.
    ///
    /// Staying out of the chain is what makes the caller's grab certain to succeed: a
    /// newly-zeroed block has `node_count == 0`, well below `BLOCK_NODE_COUNT`, and the chain
    /// is the only way another grabber reaches a block, so while the caller holds the
    /// allocation permit nothing else can take a slot from this one. The caller splices it in
    /// with [`Self::push_unused_block_list`] once it has taken its own, as the recycling path
    /// does.
    ///
    /// The block goes at [`Self::block_count`], past every block the tree has, whether or not
    /// any of them is in memory. It is zeroed before the runtime write lock is taken, since every
    /// reader of the State waits on that lock.
    #[lore_macro::test_pub]
    pub(in crate::state) fn allocate_fresh_block(
        &self,
    ) -> Result<(usize, Arc<NodeBlock>), StateError> {
        let block = NodeBlock::new_zeroed();
        block.write().mark_dirty();
        let mut runtime = self.runtime_write();
        let block_index = runtime.block_count();
        if block_index >= MAX_TREE_BLOCK_COUNT as usize {
            return Err(StateError::from(Oversized {
                context: format!("tree block count limit reached: {MAX_TREE_BLOCK_COUNT}"),
            }));
        }
        if block_index == 0 {
            block.write().node_block().node_count = 1;
        }
        runtime.block.resize(block_index, Weak::default());
        runtime.block.push(Arc::downgrade(&block));
        #[cfg(feature = "state-hooks")]
        block.set_hook_index(
            &self.hooks,
            block_index,
            crate::state::node::hooks::log::Install::Allocated,
        );

        if let Some(tree) = runtime.tree.as_mut() {
            tree.block_count = 1 + block_index as u32;
            tree.flags |= TreeFlags::Dirty;
        }
        drop(runtime);
        self.block_modified(block.clone(), block_index);
        Ok((block_index, block))
    }

    /// Insert `block` at the head of the unused chain when it isn't already
    /// there. The chain is a prepend-only singly-linked list whose head
    /// `tree.block_unused_first` advances only as exhausted blocks are popped;
    /// the most recently allocated block is therefore either at the head or
    /// not in the chain at all, so a non-head index here means the block can
    /// be safely linked at the front without traversing to deduplicate.
    pub(in crate::state) fn push_unused_block_list(
        &self,
        block_index: usize,
        block: &Arc<NodeBlock>,
    ) {
        let block_idx_u32 = block_index as u32;
        let mut runtime = self.runtime_write();
        let Some(tree) = runtime.tree.as_mut() else {
            return;
        };
        if tree.block_unused_first == block_idx_u32 {
            return;
        }
        let prior_head = tree.block_unused_first;
        tree.block_unused_first = block_idx_u32;
        tree.flags |= TreeFlags::Dirty;
        let newly_dirty = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Allocator);
            let mut block_writer = block.write();
            block_writer.node_block().block_unused_next = prior_head;
            block_writer.mark_dirty()
        };
        drop(runtime);
        if newly_dirty {
            self.block_modified(block.clone(), block_index);
        }
    }
}
