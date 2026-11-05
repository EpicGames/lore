// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Detaches of a node from its directory's chain.

use std::future::Future;
use std::sync::Arc;

use super::Served;
use super::ServedNodes;
use super::Swap;
use super::SwapNodes;
use super::Tier;
use super::Unserved;
use super::locks::BlockLocks;
use super::locks::ChainAttempts;
use super::locks::ChainPosition;
use super::locks::ParentRead;
use super::locks::Predecessor;
use super::locks::admits_served_change;
use crate::errors::InvalidArguments;
use crate::errors::NodeNotFound;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::SiblingCycleGuard;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

/// A directory's chain as a caller read it before walking its children: the directory, the block
/// its chain change count was read from, and the count. A detach given it
/// ([`Detach::IfUnchanged`]) goes ahead only where the chain took no change since. Every prepend,
/// removal and reattach of the directory's chain counts under its block's write lock, which the
/// detach holds when it compares; the block must be the one the count came from, as a block
/// loaded again counts from zero. Holding the block keeps it from being dropped meanwhile.
#[derive(Clone)]
pub struct IfUnchanged {
    block: Arc<NodeBlock>,
    directory: NodeID,
    count: u32,
}

impl IfUnchanged {
    /// The chain of `directory`, held in `block`, as a walk read it: `count` read with the
    /// directory's record ([`NodeBlock::node_with_chain_changes`]) before the walk began.
    pub fn new(block: Arc<NodeBlock>, directory: NodeID, count: u32) -> Self {
        Self {
            block,
            directory,
            count,
        }
    }

    /// The chain of `directory`, held in `block`, as it stands now, with the directory's record
    /// read under the same lock. Clones the block's `Arc`; a caller that owns one moves it in
    /// through [`Self::new`].
    pub fn read(block: &Arc<NodeBlock>, directory: NodeID) -> (Self, Node) {
        let (node, count) = block.node_with_chain_changes(Node::index(directory));
        (
            Self {
                block: block.clone(),
                directory,
                count,
            },
            node,
        )
    }

    /// The weighted changes the directory's chain took since this was read (1 for a prepend or
    /// a removal, [`REATTACH_CHANGES`](crate::state::REATTACH_CHANGES) for a reattach out of or
    /// within it), wrapping.
    pub fn changes_since(&self) -> u32 {
        self.block
            .chain_changes(Node::index(self.directory))
            .wrapping_sub(self.count)
    }

    /// The directory whose chain this read.
    pub fn directory(&self) -> NodeID {
        self.directory
    }

    /// Whether `directory`'s chain, held in `block`, took no change since this was read.
    #[inline]
    fn holds(&self, block: &Arc<NodeBlock>, directory: NodeID) -> bool {
        self.directory == directory
            && Arc::ptr_eq(&self.block, block)
            && block.chain_changes(Node::index(directory)) == self.count
    }
}

impl std::fmt::Debug for IfUnchanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IfUnchanged")
            .field("directory", &self.directory)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

/// What one try of a chain change's critical section did.
pub(in crate::state) enum Section {
    /// The change is made.
    Done,
    /// The chain moved since it was walked: walk again.
    Changed,
    /// The served chain rule refused it: walk again, and the walk's pre-test marks the parent.
    NeedsMark,
}

/// When a detach goes ahead, decided under the block write locks it takes.
#[derive(Clone, Copy, Debug)]
pub enum Detach<'a> {
    /// Whatever the node holds.
    Always,
    /// Only where the node still stands in this directory under this name hash.
    At(NodeID, u64),
    /// Only where the node is not `Dirty`.
    IfClean,
    /// Only where the node has no child.
    IfEmpty,
    /// Only where the directory's chain took no change since it was read, as RMDIR's emptiness
    /// check read it (H11).
    IfUnchanged(&'a IfUnchanged),
}

impl Detach<'_> {
    /// Under `state-hooks`, how the log names this detach, and what its condition compares
    /// against, `node` read as the condition reads it.
    #[cfg(feature = "state-hooks")]
    fn hook(self, node: &Node) -> (log::How, log::Expected) {
        match self {
            Self::Always => (log::How::Always, log::Expected::Nothing),
            Self::IfClean => (log::How::IfClean, log::Expected::Nothing),
            Self::IfEmpty => (log::How::IfEmpty, log::Expected::Head(node.child)),
            Self::At(parent, name_hash) => (log::How::At, log::Expected::At { parent, name_hash }),
            Self::IfUnchanged(unchanged) => (
                log::How::IfUnchanged,
                log::Expected::Count {
                    changes: unchanged.count,
                    instance: unchanged.block.hook_instance(),
                },
            ),
        }
    }

    /// Under `state-hooks`, why this condition refused `node`, and what it read.
    #[cfg(feature = "state-hooks")]
    fn hook_refusal(
        self,
        node: &Node,
        block: &Arc<NodeBlock>,
        node_id: NodeID,
    ) -> (log::Reason, log::Read) {
        match self {
            Self::IfEmpty => (log::Reason::IfEmpty, log::Read::Head(node.child)),
            Self::At(..) => (log::Reason::At, log::Read::Word(log::Words(*node))),
            Self::IfUnchanged(_) => (
                log::Reason::IfUnchanged,
                log::Read::Count {
                    changes: block.chain_changes(Node::index(node_id)),
                    instance: block.hook_instance(),
                },
            ),
            Self::Always | Self::IfClean => {
                (log::Reason::IfClean, log::Read::Word(log::Words(*node)))
            }
        }
    }

    /// Whether this condition refuses the detach of `node`, the record of `node_id`, whose block
    /// is `block`. Tested on the copy each attempt reads before any mark, so a refused detach
    /// marks no parent (H2), and again on the record under the locks, which decides.
    #[inline]
    fn refuses(self, node: &Node, block: &Arc<NodeBlock>, node_id: NodeID) -> bool {
        match self {
            Self::Always => false,
            Self::IfClean => node.is_dirty(),
            Self::IfEmpty => node.is_directory() && node.child().is_some(),
            Self::At(dir, name_hash) => node.parent != dir || node.name_hash != name_hash,
            Self::IfUnchanged(unchanged) => !unchanged.holds(block, node_id),
        }
    }
}

impl<'s> ServedNodes<'s> {
    /// Detaches `node_id` where `when` allows, as [`State::node_detach`] does, under the served
    /// chain rule: its parent is not marked first, but the detach goes ahead only where the parent
    /// is live and dirty with no clear pending, marking it first ([`Self::node_mark`]) where it is
    /// not. A detach `when` refuses on the record read before that mark marks nothing; one refused
    /// under the locks after it leaves the parent marked, the safe direction. Answers whether it
    /// detached.
    pub fn node_detach<'a>(
        self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        when: Detach<'a>,
    ) -> impl Future<Output = Result<bool, StateError>> + Send + 'a
    where
        's: 'a,
    {
        self.state.detach_child::<Served>(repository, node_id, when)
    }
}

impl<'s> SwapNodes<'s> {
    /// Detaches `node_id` where `when` allows, decided under the block write locks the detach
    /// takes, answering whether it detached. The swap removes clean nodes, empty directories
    /// and local files at their recorded place, which leave dirty closure intact, so no parent
    /// is marked: the swap's settle reconciles the directories it touched with the target.
    pub fn node_detach<'a>(
        self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        when: Detach<'a>,
    ) -> impl Future<Output = Result<bool, StateError>> + Send + 'a
    where
        's: 'a,
    {
        self.state.detach_child::<Swap>(repository, node_id, when)
    }
}

impl State {
    /// Where `node_id` sits in `parent`'s child chain, as a walk from the head finds it
    /// now. A cycle guard trip or a misparented node is `Moving` when the directory's
    /// change count moved since the head was read; otherwise the trip is a cycle error and
    /// the node is reported for [`ChainAttempts`] to judge. With it, the parent as read with
    /// the head.
    pub(in crate::state) async fn find_predecessor(
        &self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        parent_block: &Arc<NodeBlock>,
        node_id: NodeID,
    ) -> Result<(ChainPosition, ParentRead), StateError> {
        let (read, mut current, changes) = {
            let (parent_node, changes) = parent_block.node_with_chain_changes(Node::index(parent));
            (ParentRead::of(&parent_node), parent_node.child(), changes)
        };
        if current == Some(node_id) {
            return Ok((ChainPosition::Found(Predecessor::Head, None), read));
        }
        let mut cycle = SiblingCycleGuard::new(parent);
        while let Some(previous_id) = current {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::PredecessorStep)
                .await;
            if let Err(trip) = cycle.observe(previous_id) {
                if parent_block.chain_changes(Node::index(parent)) == changes {
                    return Err(StateError::from(trip));
                }
                return Ok((ChainPosition::Moving, read));
            }
            let block = self
                .block(repository.clone(), NodeBlock::index(previous_id))
                .await?;
            let previous = block.node(Node::index(previous_id));
            if previous.parent != parent {
                if parent_block.chain_changes(Node::index(parent)) != changes {
                    return Ok((ChainPosition::Moving, read));
                }
                return Ok((
                    ChainPosition::Misparented {
                        node: previous_id,
                        actual_parent: previous.parent,
                        changes,
                    },
                    read,
                ));
            }
            if previous.sibling() == Some(node_id) {
                return Ok((
                    ChainPosition::Found(Predecessor::Node(previous_id), Some(block)),
                    read,
                ));
            }
            current = previous.sibling();
        }
        Ok((ChainPosition::Missing, read))
    }

    /// Remove `node_id` from its parent's child chain and mark it `Discarded` in the same
    /// critical section, where `when` allows, after dirtying the parent.
    ///
    /// The node, the parent and the predecessor are loaded first, then their
    /// blocks are write-locked in block-index order and the predecessor is checked to
    /// still reach the node from the parent it names; a change in between makes
    /// it load and try again, up to [`CHAIN_CHANGE_ATTEMPTS`] times, then fail with
    /// [`SlowDown`]. A node its parent's chain never reaches is a broken hierarchy.
    /// The node's own `sibling` is read under the locks and kept.
    ///
    /// The tier decides the parent's mark: the unserved tier marks it before a detach; the served
    /// tier marks it only where the parent record the walk read fails the served chain rule's
    /// pre-test, and a section the rule refuses under the locks walks again; the swap and discard
    /// tiers mark nothing. A conditional detach tests its condition on the record each attempt
    /// reads before any mark, so one it refuses there marks no parent (H2), and again under the
    /// locks, which decides. The root and an invalid id are refused as not detachable, and a detach
    /// under a detached parent is [`NodeNotFound`], as the unserved mark before it finds.
    ///
    /// [`CHAIN_CHANGE_ATTEMPTS`]: crate::state::CHAIN_CHANGE_ATTEMPTS
    pub(in crate::state) async fn detach_child<T: Tier>(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        when: Detach<'_>,
    ) -> Result<bool, StateError> {
        if !node_id.is_valid_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "node id does not name a detachable node".into(),
            }));
        }
        let block_index = NodeBlock::index(node_id);
        let block = self.block(repository.clone(), block_index).await?;
        let mut attempts = ChainAttempts::default();
        // The attempt numbers the log's pre-tests, under `state-hooks` alone.
        #[cfg_attr(not(feature = "state-hooks"), allow(unused_variables))]
        for attempt in 0..self.chain_change_attempts() {
            let parent = {
                // Under `state-hooks`, the copy, the condition's count load and the log's record of
                // the pre-test under one read guard.
                #[cfg(feature = "state-hooks")]
                let reader = block.read();
                #[cfg(feature = "state-hooks")]
                let node = *reader.node(Node::index(node_id));
                #[cfg(not(feature = "state-hooks"))]
                let node = block.node(Node::index(node_id));
                if node.is_discarded() {
                    return Err(NodeNotFound.into());
                }
                let refused = when.refuses(&node, &block, node_id);
                #[cfg(feature = "state-hooks")]
                {
                    if T::SERVED && !matches!(when, Detach::Always) {
                        log::record(
                            &self.hooks,
                            log::Effect::PreTest {
                                node: node_id,
                                attempt,
                                refused,
                                expected: when.hook(&node).1,
                            },
                        );
                    }
                    drop(reader);
                }
                if refused {
                    return Ok(false);
                }
                node.parent
            };
            let parent_block_index = NodeBlock::index(parent);
            let parent_block = self.block(repository.clone(), parent_block_index).await?;
            T::premark(self, &repository, parent).await?;
            let (position, read) = self
                .find_predecessor(repository.clone(), parent, &parent_block, node_id)
                .await?;
            #[cfg(feature = "state-hooks")]
            let arm = match &position {
                ChainPosition::Moving if T::SERVED => Some(log::Arm::DetachMoving),
                ChainPosition::Moving => Some(log::Arm::UnservedDetachMoving),
                ChainPosition::Missing if T::SERVED => Some(log::Arm::DetachMissing),
                ChainPosition::Missing => Some(log::Arm::UnservedDetachMissing),
                _ => None,
            };
            let Some((predecessor, predecessor_block)) = attempts.found(parent, position)? else {
                #[cfg(feature = "state-hooks")]
                if let Some(arm) = arm {
                    log::arm(&self.hooks, arm);
                }
                continue;
            };
            if T::SERVED
                && !read.admits(parent, &parent_block)
                && let Err(error) =
                    T::served_mark(self, &repository, parent, (&parent_block, read.parent())).await
            {
                // A node that left the parent meanwhile is detached from where it went.
                if !error.is_node_not_found() || block.node(Node::index(node_id)).parent == parent {
                    return Err(error);
                }
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::DetachMovedMark);
                continue;
            }
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::PredecessorStep)
                .await;

            let section = {
                #[cfg(feature = "state-hooks")]
                let _label = log::label(log::Section::Detach);
                let mut locks = BlockLocks::lock([
                    Some((parent_block_index, &parent_block)),
                    predecessor.locked(predecessor_block.as_ref()),
                    Some((block_index, &block)),
                    None,
                ]);
                // Whether the parent is detached, and in the served tier whether it admits the
                // change, read once.
                let (parent_gone, parent_admits) = locks
                    .with_node(parent, |held| {
                        (
                            held.is_discarded(),
                            T::SERVED && admits_served_change(parent, held, &parent_block),
                        )
                    })
                    .unwrap_or((false, false));
                match locks.node(node_id) {
                    // A node detached meanwhile is not found, as the pre-test answers, whatever
                    // the condition reads of it.
                    Some(node) if node.is_discarded() => {
                        #[cfg(feature = "state-hooks")]
                        log::record(
                            &self.hooks,
                            log::Effect::Refused {
                                node: node_id,
                                parent: node_id,
                                reason: log::Reason::NotFound,
                                read: log::Read::Word(log::Words(node)),
                                expected: log::Expected::Nothing,
                            },
                        );
                        return Err(NodeNotFound.into());
                    }
                    Some(node) if when.refuses(&node, &block, node_id) => {
                        #[cfg(feature = "state-hooks")]
                        {
                            let (reason, read) = when.hook_refusal(&node, &block, node_id);
                            // The condition reads the node's own word, head or count.
                            log::record(
                                &self.hooks,
                                log::Effect::Refused {
                                    node: node_id,
                                    parent: node_id,
                                    reason,
                                    read,
                                    expected: when.hook(&node).1,
                                },
                            );
                        }
                        return Ok(false);
                    }
                    // A node that left the parent is detached from where it went.
                    Some(node) if node.parent != parent => Section::Changed,
                    _ if parent_gone => {
                        #[cfg(feature = "state-hooks")]
                        if let Some(held) = locks.node(parent) {
                            log::record(
                                &self.hooks,
                                log::Effect::Refused {
                                    node: node_id,
                                    parent,
                                    reason: log::Reason::NotFound,
                                    read: log::Read::Word(log::Words(held)),
                                    expected: log::Expected::Nothing,
                                },
                            );
                        }
                        return Err(NodeNotFound.into());
                    }
                    _ if T::SERVED && !parent_admits => Section::NeedsMark,
                    Some(node)
                        if node.parent == parent
                            && predecessor.reaches(&mut locks, parent, node_id) =>
                    {
                        predecessor.set(&mut locks, parent, node.sibling);
                        locks.flag_detached(node_id);
                        locks.note_detach(parent, node.name_hash, node_id, 1);
                        #[cfg(feature = "state-hooks")]
                        {
                            let (how, expected) = when.hook(&node);
                            let instance = locks.hook_instance(parent);
                            log::record(
                                &self.hooks,
                                log::Effect::Detach {
                                    node: node_id,
                                    parent,
                                    how,
                                    expected,
                                    instance,
                                },
                            );
                            if T::PENDS {
                                log::record(&self.hooks, log::Effect::Pend { root: node_id });
                            }
                        }
                        locks.release(self);
                        Section::Done
                    }
                    _ => Section::Changed,
                }
            };
            match section {
                Section::Done => return Ok(true),
                Section::NeedsMark => {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::DetachNeedsMark);
                    self.served_refused();
                }
                Section::Changed => {
                    #[cfg(feature = "state-hooks")]
                    log::arm(
                        &self.hooks,
                        if T::SERVED {
                            log::Arm::DetachChanged
                        } else {
                            log::Arm::UnservedDetachChanged
                        },
                    );
                }
            }
        }
        Err(attempts.exhausted(self, &repository, node_id).await)
    }

    /// Removes `node_id` from its parent's child chain in place, keeping its slot, where `when`
    /// allows, decided under the block write locks the detach takes; answers whether it detached.
    ///
    /// The parent, and each of its ancestors not yet dirty, is marked `Dirty` first, keeping its
    /// action and extension flags, unless `when` refuses on the record read before the mark.
    /// Then, under the write locks of the parent's block, the predecessor's and the node's, the
    /// predecessor that reached the node is pointed at the node's own `sibling` and the node is marked
    /// `Discarded`. The node keeps its record, its `sibling` and its slot until a sweep frees it
    /// where nothing else runs on the State (C15), so a reader that copied its predecessor before
    /// the change walks on through it to the rest of the chain, and no add meanwhile is handed its
    /// slot.
    ///
    /// A node already detached is reported as [`NodeNotFound`]. A node a concurrent reattach
    /// moves first is detached from where it went, unless `when` is [`Detach::At`] its old place.
    /// For [`Detach::IfClean`], a change below the node that marks its ancestors dirty
    /// ([`Self::node_mark`]) reaches the node under the same block write lock and refuses a
    /// `Discarded` one, so either the mark lands first and this refuses, or this lands first and
    /// the mark fails with [`NodeNotFound`], however deep below the node the change is. For
    /// [`Detach::IfEmpty`] the locks include the directory's own, where an attach below it
    /// publishes.
    pub fn node_detach<'a>(
        &'a self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        when: Detach<'a>,
    ) -> impl Future<Output = Result<bool, StateError>> + Send + 'a {
        self.detach_child::<Unserved>(repository, node_id, when)
    }
}
