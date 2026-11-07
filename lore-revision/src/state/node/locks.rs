// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `BlockLocks`, the predecessor helpers and the chain-change bookkeeping a critical section uses.

use core::str;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_error_set::prelude::*;

use super::discard::format_parent_child_chain;
use crate::errors::InvalidArguments;
use crate::errors::InvalidNodeHierarchy;
use crate::errors::NodeNotFound;
use crate::errors::SlowDown;
use crate::lore_warn;
use crate::node::ChainPatch;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeBlockReader;
use crate::node::NodeBlockWriter;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::PatchKind;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;

/// Most node blocks one namespace change write-locks: a replacing reattach's old and new
/// parents, the node and its predecessor, and the holder and its predecessor.
pub(in crate::state) const MAX_LOCKED_BLOCKS: usize = 6;

/// A node's predecessor in its chain, and the block holding it where it is a node.
#[lore_macro::test_pub]
pub(in crate::state) type FoundPredecessor = (Predecessor, Option<Arc<NodeBlock>>);

/// One write-locked block of a [`BlockLocks`], and whether the change wrote to it.
#[lore_macro::test_pub]
pub(in crate::state) struct LockedBlock<'a> {
    pub(in crate::state) index: usize,
    pub(in crate::state) block: &'a Arc<NodeBlock>,
    pub(in crate::state) writer: NodeBlockWriter<'a>,
    pub(in crate::state) written: bool,
}

impl<'a> LockedBlock<'a> {
    /// Marks the block dirty where the change wrote to it and drops its lock; the block where it
    /// turned dirty, for the caller to register once every lock is dropped.
    fn mark_written(mut self) -> Option<(usize, &'a Arc<NodeBlock>)> {
        (self.written && self.writer.mark_dirty()).then_some((self.index, self.block))
    }
}

/// Write locks on the node blocks one change to a directory's child chain touches.
///
/// Taken in ascending block index, each block once, so changes locking overlapping sets
/// never wait on each other in a cycle; the only other holders of two block locks at once, the
/// pair lock ([`PairLock`]) and a swap build's private publish, take them in that order too. The
/// holder takes no other lock and never awaits: [`Self::release`] registers the blocks it
/// dirtied only after every lock is dropped, since registering takes the state's runtime
/// lock, which other code takes before a block lock. A node whose block is not held reads
/// as `None`, which the change treats as a failed check.
#[lore_macro::test_pub]
pub(in crate::state) struct BlockLocks<'a> {
    held: [Option<LockedBlock<'a>>; MAX_LOCKED_BLOCKS],
    /// Under `state-hooks`, the log's section of the change, taken after its last lock; closed
    /// once its writers have released.
    #[cfg(feature = "state-hooks")]
    _section: Option<crate::state::node::hooks::log::SectionHold>,
}

impl<'a> BlockLocks<'a> {
    #[lore_macro::test_pub]
    pub(in crate::state) fn lock<const N: usize>(
        given: [Option<(usize, &'a Arc<NodeBlock>)>; N],
    ) -> Self {
        const { assert!(N <= MAX_LOCKED_BLOCKS) };
        let mut blocks: [Option<(usize, &'a Arc<NodeBlock>)>; MAX_LOCKED_BLOCKS] =
            [None; MAX_LOCKED_BLOCKS];
        blocks[..N].copy_from_slice(&given);
        blocks.sort_unstable_by_key(|block| block.map_or(usize::MAX, |(index, _)| index));
        let mut held: [Option<LockedBlock<'a>>; MAX_LOCKED_BLOCKS] = Default::default();
        let mut count = 0;
        #[cfg(feature = "state-hooks")]
        let multi = crate::state::node::hooks::log::multi();
        for (index, block) in blocks.into_iter().flatten() {
            if count > 0
                && held[count - 1]
                    .as_ref()
                    .is_some_and(|last| last.index == index)
            {
                continue;
            }
            held[count] = Some(LockedBlock {
                index,
                block,
                writer: block.write(),
                written: false,
            });
            count += 1;
        }
        #[cfg(feature = "state-hooks")]
        let section = multi.opened(
            held[0]
                .as_ref()
                .and_then(|first| first.block.hook_state())
                .map(|state| &**state),
        );
        Self {
            held,
            #[cfg(feature = "state-hooks")]
            _section: section,
        }
    }

    #[inline]
    pub(in crate::state) fn entry(&mut self, node_id: NodeID) -> Option<&mut LockedBlock<'a>> {
        let index = NodeBlock::index(node_id);
        self.held
            .iter_mut()
            .flatten()
            .find(|held| held.index == index)
    }

    /// A copy of `node_id`'s record. Inlined, so a caller testing a field of it copies nothing.
    #[inline]
    pub(in crate::state) fn node(&mut self, node_id: NodeID) -> Option<Node> {
        let held = self.entry(node_id)?;
        Some(*held.writer.node(Node::index(node_id)))
    }

    /// `f` of `node_id`'s record, read in place.
    #[inline]
    pub(in crate::state) fn with_node<R>(
        &mut self,
        node_id: NodeID,
        f: impl FnOnce(&Node) -> R,
    ) -> Option<R> {
        let held = self.entry(node_id)?;
        Some(f(held.writer.node(Node::index(node_id))))
    }

    #[inline]
    pub(in crate::state) fn node_mut(&mut self, node_id: NodeID) -> Option<&mut Node> {
        let held = self.entry(node_id)?;
        held.written = true;
        Some(held.writer.node(Node::index(node_id)))
    }

    /// Flags `node_id`, just detached from its chain, `Discarded`, and its block for the next
    /// sweep, which frees it once nothing retains it (C15); writes nothing where its block is not
    /// held.
    #[inline]
    pub(in crate::state) fn flag_detached(&mut self, node_id: NodeID) {
        let Some(held) = self.entry(node_id) else {
            return;
        };
        held.written = true;
        held.writer.node(Node::index(node_id)).flags |= NodeFlags::Discarded.bits();
        held.writer.mark_unswept();
    }

    /// Under `state-hooks`, `node_id`'s name as its block's table holds it, for the log.
    #[cfg(feature = "state-hooks")]
    pub(in crate::state) fn hook_name(
        &mut self,
        node_id: NodeID,
    ) -> crate::state::node::hooks::log::Name {
        let held = self.entry(node_id);
        crate::state::node::hooks::log::name(
            held.and_then(|held| held.writer.node_name_bytes(Node::index(node_id))),
        )
    }

    /// Under `state-hooks`, the instance of `node_id`'s block, 0 where it is not held.
    #[cfg(feature = "state-hooks")]
    pub(in crate::state) fn hook_instance(&mut self, node_id: NodeID) -> u64 {
        self.entry(node_id)
            .map_or(0, |held| held.block.hook_instance())
    }

    /// Drops `node_id`'s pending clear, if any, under its block's write lock.
    #[inline]
    pub(in crate::state) fn drop_clear_pending(&mut self, node_id: NodeID) {
        if let Some(held) = self.entry(node_id) {
            held.writer.drop_clear_pending(Node::index(node_id));
        }
    }

    /// Counts a change of `weight` to the chain under `directory`, 1 for a prepend or a removal
    /// and [`REATTACH_CHANGES`] for a move out of it or a rename in it to another name hash, and
    /// logs `node_id`'s attach under `name_hash` where the directory has a chain log.
    ///
    /// [`REATTACH_CHANGES`]: crate::state::REATTACH_CHANGES
    pub(in crate::state) fn note_attach(
        &mut self,
        directory: NodeID,
        name_hash: u64,
        node_id: NodeID,
        weight: u32,
    ) {
        self.note(directory, weight, PatchKind::Attached, name_hash, node_id);
    }

    /// [`Self::note_attach`] for a detach.
    #[lore_macro::test_pub]
    pub(in crate::state) fn note_detach(
        &mut self,
        directory: NodeID,
        name_hash: u64,
        node_id: NodeID,
        weight: u32,
    ) {
        self.note(directory, weight, PatchKind::Detached, name_hash, node_id);
    }

    /// Counts a rename in place under `directory` that kept the name hash.
    pub(in crate::state) fn note_respell(&mut self, directory: NodeID) {
        if let Some(held) = self.entry(directory)
            && let Some(log) = held.writer.chain_log_mut(Node::index(directory))
        {
            log.respell();
        }
    }

    /// Adds `weight` to `directory`'s chain change count and appends the patch to its log, if it
    /// has one ([`Self::log_patch`]); a directory with no log costs the empty slot search.
    pub(in crate::state) fn note(
        &mut self,
        directory: NodeID,
        weight: u32,
        kind: PatchKind,
        name_hash: u64,
        node_id: NodeID,
    ) {
        let index = Node::index(directory);
        let Some(held) = self.entry(directory) else {
            return;
        };
        if weight != 0 {
            held.writer.note_chain_changes(index, weight);
        }
        if held.writer.chain_log_mut(index).is_some() {
            self.log_patch(directory, kind, name_hash, node_id);
        }
    }

    /// Appends the patch to `directory`'s log, which a listing watches, with a weak reference to
    /// `node_id`'s block, which every primitive holds locked too. Without that block no patch can
    /// be written, so the log leaves every earlier reader stale. Out of line, so a directory
    /// without a log, the only kind outside a VFS provider's State, carries none of it.
    #[inline(never)]
    fn log_patch(&mut self, directory: NodeID, kind: PatchKind, name_hash: u64, node_id: NodeID) {
        let index = Node::index(directory);
        let block = self.entry(node_id).map(|held| Arc::downgrade(held.block));
        let Some(log) = self
            .entry(directory)
            .and_then(|held| held.writer.chain_log_mut(index))
        else {
            return;
        };
        if let Some(block) = block {
            log.append(ChainPatch {
                name_hash,
                block,
                node: node_id,
                kind,
            });
        } else {
            log.skip(kind);
            // Not an assert: release builds keep debug assertions and abort, and the skip is a
            // correct fallback.
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                lore_warn!(
                    "Directory {directory}: a chain change was logged without the block of child \
                     {node_id}; its listings rebuild"
                );
            }
        }
    }

    /// Stores `name` in `node_id`'s block, reusing the slot the node's name holds when it
    /// fits, and returns the slot it takes.
    pub(in crate::state) fn store_name(
        &mut self,
        node_id: NodeID,
        name: &str,
    ) -> Result<(u32, u32), StateError> {
        let Some(held) = self.entry(node_id) else {
            return Err(StateError::internal("the node's block is not locked"));
        };
        let node = *held.writer.node(Node::index(node_id));
        let stored = held
            .writer
            .node_name_store(name, node.name_offset, node.name_length);
        #[cfg(feature = "state-hooks")]
        if stored.is_err()
            && let Some(state) = held.block.hook_state()
        {
            crate::state::node::hooks::log::name_table_full(state, node_id);
        }
        let slot = stored.forward::<StateError>("Storing the node's new name")?;
        held.written = true;
        Ok(slot)
    }

    /// Marks each block written to dirty, drops every lock, then registers the blocks that
    /// became dirty with `state`, which marks it dirty: a block dirty already was registered,
    /// and the State marked, when it turned dirty (C14). One registration per block: a batch
    /// under one runtime lock measured 10 to 26 instructions more per change, in steady state
    /// too, for a saving only where two blocks turn dirty in one release.
    pub(in crate::state) fn release(self, state: &State) {
        let Self {
            held,
            #[cfg(feature = "state-hooks")]
                _section: section,
        } = self;
        let mut dirtied: [Option<(usize, &Arc<NodeBlock>)>; MAX_LOCKED_BLOCKS] =
            [None; MAX_LOCKED_BLOCKS];
        for (slot, held) in held.into_iter().enumerate() {
            dirtied[slot] = held.and_then(LockedBlock::mark_written);
        }
        // Closed once every writer has released, before the registrations, outside the section.
        #[cfg(feature = "state-hooks")]
        drop(section);
        for (index, block) in dirtied.into_iter().flatten() {
            state.block_modified(block.clone(), index);
        }
    }
}

/// Whether `parent`, its record read from `block`, admits a served change below it (C11a):
/// not `Discarded`, and dirty with no clear pending, the root counting as dirty and never
/// pending. Under the parent's block lock this decides; on a copy read without it, it is the
/// pre-test.
#[inline]
pub(in crate::state) fn admits_served_change(
    parent_id: NodeID,
    parent: &Node,
    block: &NodeBlock,
) -> bool {
    flags_admit_served_change(parent_id, parent.flags, block)
}

/// [`admits_served_change`] of a directory whose record holds `flags`.
#[inline]
fn flags_admit_served_change(parent_id: NodeID, flags: u16, block: &NodeBlock) -> bool {
    flags & NodeFlags::Discarded.bits() == 0
        && (parent_id == ROOT_NODE
            || (flags & NodeFlags::Dirty.bits() != 0
                && !block.clear_pending(Node::index(parent_id))))
}

/// What a walk of a directory's chain read of the directory with its head: its flags, for the
/// served chain rule's pre-test, and its own parent, where the mark that follows a failed
/// pre-test starts.
#[derive(Clone, Copy, Debug)]
pub(in crate::state) struct ParentRead {
    flags: u16,
    parent: NodeID,
}

impl ParentRead {
    #[inline]
    pub(in crate::state) fn of(node: &Node) -> Self {
        Self {
            flags: node.flags,
            parent: node.parent,
        }
    }

    /// [`admits_served_change`] on this copy of directory `parent_id`'s record, read from
    /// `block`, its block.
    #[inline]
    pub(in crate::state) fn admits(self, parent_id: NodeID, block: &NodeBlock) -> bool {
        flags_admit_served_change(parent_id, self.flags, block)
    }

    /// The directory's own parent.
    #[inline]
    pub(in crate::state) fn parent(self) -> NodeID {
        self.parent
    }
}

/// The locks of a node and its parent, for a served mark's level or a rewrite with its parent:
/// the node's block write-locked; the parent's block read-locked, since the step only tests the
/// parent, so readers of that block and steps below its other directories go on meanwhile; one
/// write guard where both share a block. The root's block is not locked for the root, which
/// always admits a served change. Taken in ascending block index, so it orders with
/// [`BlockLocks`] and never waits on it in a cycle. Released as [`BlockLocks`] releases: the
/// node's block, where written, is marked dirty under its guard, and registered, with the State
/// marked, only after every guard is dropped (C13).
pub(in crate::state) struct PairLock<'a> {
    parent: NodeID,
    child: NodeID,
    held: LockedBlock<'a>,
    parent_guard: Option<NodeBlockReader<'a>>,
    /// Under `state-hooks`, the log's section, taken once both locks are.
    #[cfg(feature = "state-hooks")]
    _section: Option<crate::state::node::hooks::log::SectionHold>,
}

impl<'a> PairLock<'a> {
    pub(in crate::state) fn lock(
        (parent, parent_block): (NodeID, &'a Arc<NodeBlock>),
        (child, child_block): (NodeID, &'a Arc<NodeBlock>),
    ) -> Self {
        let (parent_index, child_index) = (NodeBlock::index(parent), NodeBlock::index(child));
        #[cfg(feature = "state-hooks")]
        let multi = crate::state::node::hooks::log::multi();
        let locked = || LockedBlock {
            index: child_index,
            block: child_block,
            writer: child_block.write(),
            written: false,
        };
        let (held, parent_guard) = if parent_index == child_index || parent == ROOT_NODE {
            (locked(), None)
        } else if parent_index < child_index {
            let parent_guard = parent_block.read();
            (locked(), Some(parent_guard))
        } else {
            let held = locked();
            (held, Some(parent_block.read()))
        };
        #[cfg(feature = "state-hooks")]
        let section = multi.opened(child_block.hook_state().map(|state| &**state));
        Self {
            parent,
            child,
            held,
            parent_guard,
            #[cfg(feature = "state-hooks")]
            _section: section,
        }
    }

    /// The parent's record; not for the root, whose block may not be held.
    #[inline]
    pub(in crate::state) fn parent(&mut self) -> &Node {
        #[cfg(feature = "state-hooks")]
        assert!(self.parent != ROOT_NODE, "the root's block is not locked");
        match &self.parent_guard {
            Some(guard) => guard.node(Node::index(self.parent)),
            None => self.held.writer.node(Node::index(self.parent)),
        }
    }

    /// Whether the parent admits a served change below it ([`admits_served_change`]), its block
    /// `parent_block`.
    #[inline]
    pub(in crate::state) fn parent_admits(&mut self, parent_block: &NodeBlock) -> bool {
        let parent = self.parent;
        parent == ROOT_NODE || admits_served_change(parent, self.parent(), parent_block)
    }

    /// The node's record.
    #[inline]
    pub(in crate::state) fn child(&mut self) -> &Node {
        self.held.writer.node(Node::index(self.child))
    }

    /// Drops the node's pending clear, if any, under its block's write lock.
    #[inline]
    pub(in crate::state) fn drop_child_clear_pending(&mut self) {
        self.held.writer.drop_clear_pending(Node::index(self.child));
    }

    /// The node's record, to change; its block is marked dirty at the release.
    #[inline]
    pub(in crate::state) fn child_mut(&mut self) -> &mut Node {
        self.held.written = true;
        self.held.writer.node(Node::index(self.child))
    }

    /// Marks the node's block dirty where the step wrote it, drops every lock, then registers
    /// the block with `state` and marks it dirty where it turned dirty: a block already dirty was
    /// registered, and the State marked, when it turned dirty. Answers whether the step wrote
    /// the node, its block then dirty.
    pub(in crate::state) fn release(self, state: &State) -> bool {
        let Self {
            held,
            parent_guard,
            #[cfg(feature = "state-hooks")]
                _section: section,
            ..
        } = self;
        let written = held.written;
        let dirtied = held.mark_written();
        drop(parent_guard);
        // Closed once both locks are released, before the registration, outside the section.
        #[cfg(feature = "state-hooks")]
        drop(section);
        if let Some((index, block)) = dirtied {
            state.block_modified(block.clone(), index);
        }
        written
    }
}

/// Puts `node_id` at the head of `parent`'s child chain and counts and logs the change, both
/// blocks locked and checked by the caller.
#[inline]
pub(in crate::state) fn prepend_locked(
    locks: &mut BlockLocks<'_>,
    parent: NodeID,
    node_id: NodeID,
) {
    put_at_head(locks, parent, node_id, 0);
}

/// Publishes the node `node_id`, in flight since its grab, at the head of `parent`'s chain:
/// clears the `Discarded` it carried (C2, H8), prepends it, and counts and logs the change, both
/// blocks locked and checked by the caller.
#[inline]
pub(in crate::state) fn publish_locked(
    locks: &mut BlockLocks<'_>,
    parent: NodeID,
    node_id: NodeID,
) {
    put_at_head(locks, parent, node_id, NodeFlags::Discarded.bits());
}

/// Prepends `node_id` to `parent`'s chain with the flags `clear` cleared from its record, and
/// counts and logs the change; writes nothing where either block is not held.
#[inline]
fn put_at_head(locks: &mut BlockLocks<'_>, parent: NodeID, node_id: NodeID, clear: u16) {
    let Some(head) = locks.with_node(parent, |parent| parent.child) else {
        return;
    };
    let Some(name_hash) = locks.node_mut(node_id).map(|node| {
        node.flags &= !clear;
        node.sibling = head;
        node.name_hash
    }) else {
        return;
    };
    if let Some(parent_node) = locks.node_mut(parent) {
        parent_node.child = node_id;
    }
    locks.note_attach(parent, name_hash, node_id, 1);
}

/// Refuses a node that cannot take children: a detached node, reported as
/// [`NodeNotFound`], or a file or a link.
pub(in crate::state) fn check_live_directory(node: &Node) -> Result<(), StateError> {
    if node.is_discarded() {
        return Err(NodeNotFound.into());
    }
    if !node.is_directory() {
        return Err(StateError::from(InvalidArguments {
            reason: "the parent is not a directory".into(),
        }));
    }
    Ok(())
}

/// Whether, under the locks, `parent` is still a live directory whose chain change count
/// has not moved since `changes` was read with the name lookup that started from it.
/// Every name the namespace primitives and [`State::node_move_staged`] bring into or take out of a
/// chain comes through one of those, so for a State changed only through them an unchanged
/// count means the lookup's answer still holds.
#[lore_macro::test_pub]
#[inline]
pub(in crate::state) fn chain_unchanged(
    locks: &mut BlockLocks<'_>,
    parent: NodeID,
    parent_block: &NodeBlock,
    changes: u32,
) -> bool {
    locks
        .with_node(parent, |parent| {
            !parent.is_discarded() && parent.is_directory()
        })
        .unwrap_or(false)
        && parent_block.chain_changes(Node::index(parent)) == changes
}

/// Where a walk from a directory's head found a node.
#[lore_macro::test_pub]
pub(in crate::state) enum ChainPosition {
    /// Reached from this predecessor; the block holding it where it is a node.
    Found(Predecessor, Option<Arc<NodeBlock>>),
    /// The walk reached the end of the chain without it.
    Missing,
    /// The walk met this node of another directory, the directory's change count reading
    /// `changes` with the head: moved out, or a broken chain if it is met again unchanged.
    Misparented {
        node: NodeID,
        actual_parent: NodeID,
        changes: u32,
    },
    /// The walk came back to a node it had passed while the chain changed.
    Moving,
}

/// What the attempts of one chain change found where they looked for its node, so a
/// broken hierarchy is told from churn: a node met misparented twice running in one
/// directory's chain, that directory's change count unchanged, is a broken chain, and a node
/// every attempt misses under the same parent is missing from its chain.
#[lore_macro::test_pub]
#[derive(Default)]
pub(in crate::state) struct ChainAttempts {
    missing_from: Option<NodeID>,
    misses: u32,
    last_misparented: Option<(NodeID, NodeID, u32)>,
}

impl ChainAttempts {
    /// The predecessor `position` found in `parent`'s chain, or `None` to load and try again.
    #[lore_macro::test_pub]
    pub(in crate::state) fn found(
        &mut self,
        parent: NodeID,
        position: ChainPosition,
    ) -> Result<Option<FoundPredecessor>, StateError> {
        match position {
            ChainPosition::Found(predecessor, predecessor_block) => {
                self.missing_from = None;
                Ok(Some((predecessor, predecessor_block)))
            }
            ChainPosition::Missing => {
                if self.missing_from != Some(parent) {
                    self.misses = 0;
                }
                self.missing_from = Some(parent);
                self.misses += 1;
                Ok(None)
            }
            ChainPosition::Misparented {
                node,
                actual_parent,
                changes,
            } => {
                if self.last_misparented == Some((parent, node, changes)) {
                    return Err(StateError::from(InvalidNodeHierarchy {
                        node,
                        expected_parent: parent,
                        actual_parent,
                    }));
                }
                self.last_misparented = Some((parent, node, changes));
                self.missing_from = None;
                Ok(None)
            }
            ChainPosition::Moving => {
                self.missing_from = None;
                Ok(None)
            }
        }
    }

    /// The error once [`CHAIN_CHANGE_ATTEMPTS`] attempts at `node_id` have run out: a
    /// broken hierarchy when every one missed it under the same parent, else [`SlowDown`].
    ///
    /// [`CHAIN_CHANGE_ATTEMPTS`]: crate::state::CHAIN_CHANGE_ATTEMPTS
    #[lore_macro::test_pub]
    pub(in crate::state) async fn exhausted(
        self,
        state: &State,
        repository: &Arc<RepositoryContext>,
        node_id: NodeID,
    ) -> StateError {
        match self.missing_from {
            Some(parent_id) if self.misses == state.chain_change_attempts() => {
                // Boxed: a cold path, which would otherwise size every chain change's future.
                let chain = Box::pin(format_parent_child_chain(state, repository, parent_id)).await;
                StateError::internal(format!(
                    "Move hierarchy broken: node {node_id} not in the child chain of its parent \
                     {parent_id} (observed: {chain})"
                ))
            }
            _ => {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::log::arm(
                    &state.hooks,
                    crate::state::node::hooks::log::Arm::SlowDown,
                );
                StateError::from(SlowDown)
            }
        }
    }
}

/// What precedes a node in its parent's child chain.
#[lore_macro::test_pub]
#[derive(Debug, Clone, Copy)]
pub(in crate::state) enum Predecessor {
    /// The parent, through its `child`.
    Head,
    /// This live node, through its `sibling`.
    Node(NodeID),
}

impl Predecessor {
    pub(in crate::state) fn locked(
        self,
        block: Option<&Arc<NodeBlock>>,
    ) -> Option<(usize, &Arc<NodeBlock>)> {
        match self {
            Self::Head => None,
            Self::Node(previous) => block.map(|block| (NodeBlock::index(previous), block)),
        }
    }

    /// Whether this predecessor, under the locks, still reaches `node_id` in `parent`'s chain.
    pub(in crate::state) fn reaches(
        self,
        locks: &mut BlockLocks<'_>,
        parent: NodeID,
        node_id: NodeID,
    ) -> bool {
        match self {
            Self::Head => locks
                .node(parent)
                .is_some_and(|parent| parent.child == node_id),
            Self::Node(previous) => locks.node(previous).is_some_and(|previous| {
                !previous.is_discarded() && previous.parent == parent && previous.sibling == node_id
            }),
        }
    }

    pub(in crate::state) fn set(self, locks: &mut BlockLocks<'_>, parent: NodeID, target: NodeID) {
        let pointer = match self {
            Self::Head => locks.node_mut(parent).map(|parent| &mut parent.child),
            Self::Node(previous) => locks
                .node_mut(previous)
                .map(|previous| &mut previous.sibling),
        };
        if let Some(pointer) = pointer {
            *pointer = target;
        }
    }
}
