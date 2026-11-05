// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Rewrites of a node's content, and the staged delete and its undo.

use std::future::Future;
use std::sync::Arc;

use futures::FutureExt;

use super::ServedNodes;
use super::locks::PairLock;
use super::mark::UnfinishedChange;
use super::mark::holds_base_marks;
use crate::errors::InvalidArguments;
use crate::errors::NodeNotFound;
use crate::errors::SlowDown;
use crate::lore::Address;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::Mark;
use crate::state::State;
use crate::state::StateError;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

/// The content a rewrite gives a node: its mode, its size and its address. A file's address with
/// no context keeps the node's file id, as the node keeps the identity it has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub mode: u16,
    pub size: u64,
    pub address: Address,
}

/// How [`State::node_rewrite`] rewrites a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rewrite {
    /// A file's content.
    Plain,
    /// A file's content where the file is not dirty, whose content a local change may have
    /// replaced.
    IfClean,
    /// A file's content, and its dirty action and extension flags in the same write, so no
    /// reader sees the one without the other. Its ancestors are not marked: a caller marks them
    /// after, from the parent up, as the unserved marks climb ([`State::node_mark`]).
    Dirty(NodeFlags, NodeExtFlags),
    /// A node staged for deletion taken back, its staged flags and delete action dropped, with the
    /// content its kind carries: a file all of it, a link its mode and address, a directory its
    /// mode alone. Refused where its parent is `Discarded` or staged for deletion, tested under
    /// the parent's lock (H18).
    Restore,
}

/// Writes `content` into `node`, keeping its file id where the address carries none.
#[inline]
fn write_content(node: &mut Node, content: &Content) {
    let file_id = node.address.context;
    node.mode = content.mode;
    node.size = content.size;
    node.address = content.address;
    if node.address.context.is_zero() {
        node.address.context = file_id;
    }
}

/// Whether a node holding `flags` is a live file, one neither `Discarded` nor staged for
/// deletion: the only node whose content a rewrite or an edit writes. Two masked tests, as
/// `StagedDelete` needs both its bits.
#[inline]
fn live_file(flags: u16) -> bool {
    let staged_delete = NodeFlags::StagedDelete.bits();
    let kind = (NodeFlags::Discarded | NodeFlags::File).bits();
    flags & kind == NodeFlags::File.bits() && flags & staged_delete != staged_delete
}

/// [`State::node_rewrite`] under the node's block write lock: answers whether it wrote, or the
/// node's flags where it refuses, so the error is built off the lock and the hot path. A live
/// file takes every rewrite; `Restore` never reaches here.
#[inline]
fn rewrite_record(node: &mut Node, content: &Content, rewrite: Rewrite) -> Result<bool, u16> {
    if !live_file(node.flags) {
        return Err(node.flags);
    }
    match rewrite {
        Rewrite::Plain => write_content(node, content),
        Rewrite::IfClean => {
            if node.is_dirty() {
                return Ok(false);
            }
            write_content(node, content);
        }
        Rewrite::Dirty(flags, ext) => {
            write_content(node, content);
            node.flags &= !NodeFlags::DirtyBits;
            node.flags |= flags & NodeFlags::DirtyBits;
            node.ext_flags = ext.bits();
        }
        // Taken by `State::restore_below` before the record is reached.
        Rewrite::Restore => return Err(node.flags),
    }
    Ok(true)
}

/// [`State::node_modify_staged`] under the node's block write lock: a live file's content, and
/// the staged and dirty flags an edit records in the same write, staged and dirty modified, or
/// added still where the file is staged for addition ([`State::staged_edit_flags`]); else the
/// node's flags, the error built off the lock.
#[inline]
fn edit_record(node: &mut Node, content: &Content) -> Result<(), u16> {
    if !live_file(node.flags) {
        return Err(node.flags);
    }
    write_content(node, content);
    let (staged, dirty) = State::staged_edit_flags(node);
    Mark::staged_and_dirty(staged, dirty).apply_to(node);
    Ok(())
}

/// Whether [`Rewrite::Restore`] refuses a node holding `flags`: it takes only one staged for
/// deletion and not `Discarded`.
#[inline]
fn restore_refuses(flags: u16) -> bool {
    let staged_delete = NodeFlags::StagedDelete.bits();
    flags & (NodeFlags::Discarded.bits() | staged_delete) != staged_delete
}

/// [`Rewrite::Restore`] of `node`, which [`restore_refuses`] admits: it takes the content its
/// kind carries and loses its staged flags with the delete action its two halves share, a dirty
/// node staying dirty.
fn restore_record(node: &mut Node, content: &Content) {
    let file_id = node.address.context;
    node.flags &= !NodeFlags::StagedBits.bits();
    node.mode = content.mode;
    if node.is_file() {
        node.size = content.size;
        node.address = content.address;
        if node.address.context.is_zero() {
            node.address.context = file_id;
        }
    } else {
        node.size = 0;
        node.address = if node.is_link() {
            content.address
        } else {
            Address::default()
        };
    }
}

/// The error of a rewrite `rewrite` given an id that names no node it could rewrite.
#[cold]
fn invalid_rewrite_id(rewrite: Rewrite) -> StateError {
    let reason = if rewrite == Rewrite::Restore {
        "node id does not name a restorable node"
    } else {
        "node id does not name a modifiable node"
    };
    StateError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// The error of a rewrite `rewrite` refused, for a node holding `flags`.
#[cold]
fn rewrite_refused(flags: u16, rewrite: Rewrite) -> StateError {
    let flags = NodeFlags::from_bits_retain(flags);
    let reason = if flags.contains(NodeFlags::Discarded) {
        if rewrite == Rewrite::Restore {
            "cannot restore a discarded node"
        } else {
            "cannot modify a deleted node"
        }
    } else if rewrite == Rewrite::Restore {
        "node is not staged for deletion"
    } else if !flags.contains(NodeFlags::File) {
        "only a file node carries content to modify"
    } else {
        "cannot modify a node staged for deletion"
    };
    StateError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// The error of a restore refused for its parent, which holds `flags`: a parent `Discarded` or
/// staged for deletion takes no node back.
#[cold]
fn restore_refused_by_parent(flags: u16) -> StateError {
    let reason = if flags & NodeFlags::Discarded.bits() != 0 {
        "cannot restore a node whose parent is deleted"
    } else {
        "cannot restore a node whose parent is staged for deletion"
    };
    StateError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// The mark a restore gives the node it takes back; its ancestors take its halves' base bits, as
/// those of an edit's node do.
const MODIFIED: Mark = Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify);

/// The mark a C API delete gives each tracked node it stages for deletion.
const DELETED: Mark = Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete);

impl ServedNodes<'_> {
    /// Rewrites file node `node_id`'s `mode`, `size` and `address`, and marks it with the dirty
    /// action `flags` and extension flags `ext`, as [`Rewrite::Dirty`] does, under the
    /// served chain rule: under the write lock of the node's block and a read lock of `dir`'s,
    /// the directory the caller found it in, the rewrite goes ahead only where the node still
    /// stands below `dir` and `dir` is live and dirty with no clear pending. A node moved below
    /// another directory is followed there. Answers the directory the node stands below where
    /// that directory refuses the rule, rewriting nothing: the caller marks it
    /// ([`Self::node_mark`]) and rewrites again with it. The mark is the caller's, so neither
    /// future holds the other. A detached node, or one below a detached directory, is
    /// [`NodeNotFound`]; a node that is not a file, or an invalid id, is [`InvalidArguments`].
    /// `content` is borrowed, so a caller that rewrites again, as a copy-up does, holds the only
    /// copy.
    pub async fn node_rewrite(
        self,
        repository: &Arc<RepositoryContext>,
        node_id: NodeID,
        dir: NodeID,
        content: &Content,
        (flags, ext): (NodeFlags, NodeExtFlags),
    ) -> Result<Option<NodeID>, StateError> {
        if !node_id.is_valid_node_id() {
            return Err(invalid_rewrite_id(Rewrite::Plain));
        }
        let state = self.state;
        let block_index = NodeBlock::index(node_id);
        let block = match state.resident_block(block_index) {
            Some(block) => block,
            None => state.block(repository.clone(), block_index).await?,
        };
        let mut parent = dir;
        let mut parent_block = if NodeBlock::index(parent) == block_index {
            block.clone()
        } else {
            match state.resident_block(NodeBlock::index(parent)) {
                Some(block) => block,
                None => {
                    state
                        .block(repository.clone(), NodeBlock::index(parent))
                        .await?
                }
            }
        };
        for _ in 0..state.chain_change_attempts() {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::BeforeRewrite)
                .await;
            let moved = {
                #[cfg(feature = "state-hooks")]
                let _label = log::label(log::Section::Rewrite);
                let mut locks = PairLock::lock((parent, &parent_block), (node_id, &block));
                let node = locks.child();
                if node.is_discarded() {
                    #[cfg(feature = "state-hooks")]
                    hook_refused(state, node_id, node_id, log::Reason::ServedRewrite, node);
                    return Err(NodeNotFound.into());
                }
                if !node.is_file() {
                    #[cfg(feature = "state-hooks")]
                    hook_refused(state, node_id, node_id, log::Reason::ServedRewrite, node);
                    // The error built once the locks are dropped.
                    let flags = node.flags;
                    drop(locks);
                    return Err(rewrite_refused(flags, Rewrite::Plain));
                }
                if node.parent != parent {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&state.hooks, log::Arm::RewriteFollow);
                    node.parent
                } else if parent != ROOT_NODE && locks.parent().is_discarded() {
                    #[cfg(feature = "state-hooks")]
                    hook_refused(
                        state,
                        node_id,
                        parent,
                        log::Reason::ServedRewrite,
                        locks.parent(),
                    );
                    return Err(NodeNotFound.into());
                } else if !locks.parent_admits(&parent_block) {
                    return Ok(Some(parent));
                } else {
                    #[cfg(feature = "state-hooks")]
                    log::record(
                        &state.hooks,
                        log::Effect::Admitted {
                            node: node_id,
                            expected: log::Expected::Below(parent),
                        },
                    );
                    let node = locks.child_mut();
                    write_content(node, content);
                    node.flags &= !NodeFlags::DirtyBits;
                    node.flags |= flags & NodeFlags::DirtyBits;
                    node.ext_flags = ext.bits();
                    locks.release(state);
                    return Ok(None);
                }
            };
            parent = moved;
            parent_block = state
                .block(repository.clone(), NodeBlock::index(parent))
                .await?;
        }
        #[cfg(feature = "state-hooks")]
        log::arm(&state.hooks, log::Arm::SlowDown);
        Err(StateError::from(SlowDown))
    }
}

impl ServedNodes<'_> {
    /// A copy-up's rewrite of file `node_id`, found below `dir`, to `mode` and `size` with local
    /// content (no address, its file id kept): [`Self::node_rewrite`], and where the directory
    /// the file stands below refuses the served chain rule, that directory marked
    /// ([`Self::node_mark`]) and the rewrite tried again, at most `CHAIN_CHANGE_ATTEMPTS`
    /// times, then [`SlowDown`]. A mark that finds the directory detached answers
    /// [`NodeNotFound`] where the file still stands below it, in a detached subtree; a file that
    /// left it first is followed by the next rewrite.
    pub async fn node_copy_up(
        self,
        repository: &Arc<RepositoryContext>,
        node_id: NodeID,
        dir: NodeID,
        (mode, size): (u16, u64),
        flags: (NodeFlags, NodeExtFlags),
    ) -> Result<(), StateError> {
        let mut dir = dir;
        for _ in 0..self.state.chain_change_attempts() {
            // Built per rewrite, so the mark's state does not carry it.
            let Some(refusing) = self
                .node_rewrite(
                    repository,
                    node_id,
                    dir,
                    &Content {
                        mode,
                        size,
                        address: Address::default(),
                    },
                    flags,
                )
                .await?
            else {
                return Ok(());
            };
            #[cfg(feature = "state-hooks")]
            log::arm(&self.state.hooks, log::Arm::CopyUpRemark);
            dir = refusing;
            if let Err(error) = self.node_mark(repository, refusing).await
                && (!error.is_node_not_found()
                    || self.state.node(repository.clone(), node_id).await?.parent == refusing)
            {
                return Err(error);
            }
        }
        #[cfg(feature = "state-hooks")]
        log::arm(&self.state.hooks, log::Arm::SlowDown);
        Err(StateError::from(SlowDown))
    }
}

impl State {
    /// Rewrites node `node` as `rewrite` says, in the unserved tiers, under its block's write
    /// lock: a file's content, where it is a file not staged for deletion, or, for
    /// [`Rewrite::Restore`], a node staged for deletion taken back. Answers whether it wrote,
    /// false only for [`Rewrite::IfClean`] on a dirty file. Its ancestors are not marked.
    ///
    /// Refused, as [`InvalidArguments`]: an invalid id, a Discarded node, a node that is not a
    /// file but to `Restore`, a file staged for deletion but to `Restore`, and to `Restore` a
    /// node not staged for deletion or one whose parent is `Discarded` or staged for deletion,
    /// tested under the parent's read lock (`Restore` may answer [`SlowDown`] where the node's
    /// parent keeps changing). A directory's size and address are derived at commit, and a
    /// link's address is its target, so neither holds content to rewrite; a file staged for
    /// deletion is restored, never modified, which the check under the lock keeps against a
    /// delete since the caller's.
    ///
    /// The block is taken where it is in memory, read from the store only where it is not.
    /// Nothing here touches a parent or a chain, so rewrites of distinct nodes are independent,
    /// within a block too. `content` is borrowed, so the future holds no copy of it.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn node_rewrite<'a>(
        &'a self,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
        content: &'a Content,
        rewrite: Rewrite,
    ) -> impl Future<Output = Result<bool, StateError>> + Send + 'a {
        async move {
            if !node.is_valid_node_id() {
                return Err(invalid_rewrite_id(rewrite));
            }
            if rewrite == Rewrite::Restore {
                // Boxed: its locks and retries would otherwise grow every rewrite's future. Its
                // callers run where no C API delete does.
                return match Box::pin(self.restore_below(repository, node, content, None, true))
                    .await?
                {
                    Restored::Below(_) => Ok(true),
                    Restored::AddedParent => Err(StateError::internal(
                        "an ordered restore asked for the moves permit",
                    )),
                };
            }
            let block = match self.resident_block(NodeBlock::index(node)) {
                Some(block) => block,
                None => {
                    self.block(repository.clone(), NodeBlock::index(node))
                        .await?
                }
            };
            let dirtied = {
                #[cfg(feature = "state-hooks")]
                let _label = log::label(log::Section::Rewrite);
                let mut writer = block.write();
                match rewrite_record(writer.node(Node::index(node)), content, rewrite) {
                    Ok(true) => {
                        #[cfg(feature = "state-hooks")]
                        if rewrite == Rewrite::IfClean {
                            log::record(
                                &self.hooks,
                                log::Effect::Admitted {
                                    node,
                                    expected: log::Expected::Clean,
                                },
                            );
                        }
                        writer.mark_dirty()
                    }
                    Ok(false) => {
                        #[cfg(feature = "state-hooks")]
                        hook_refused(
                            self,
                            node,
                            node,
                            log::Reason::IfClean,
                            writer.node(Node::index(node)),
                        );
                        return Ok(false);
                    }
                    Err(flags) => {
                        #[cfg(feature = "state-hooks")]
                        hook_refused(
                            self,
                            node,
                            node,
                            log::Reason::Rewrite,
                            writer.node(Node::index(node)),
                        );
                        drop(writer);
                        return Err(rewrite_refused(flags, rewrite));
                    }
                }
            };
            if dirtied {
                self.block_modified(block, NodeBlock::index(node));
            }
            Ok(true)
        }
    }

    /// Takes node `node`, staged for deletion, back into the revision with `content`
    /// ([`Rewrite::Restore`]), for the C API's add of a deleted name: marked staged and dirty
    /// modified in the same write, then its ancestors staged and dirty: it is in the revision the
    /// State was loaded from, so what is staged is a change to it. Only the node named is
    /// restored; its children stay staged for deletion until each is. Unlike the C API's other
    /// changes, the climb comes after the write (H8): the delete that staged the node marked its
    /// parent and their ancestors, before the node where through the C API, so a restore dropped
    /// between finds them staged and leaves no change below an unstaged directory.
    ///
    /// Refused as [`InvalidArguments`]: an invalid id, a node not staged for deletion or
    /// `Discarded`, and a node whose parent is `Discarded` or staged for deletion, tested under
    /// the parent's read lock and the node's write lock, so a restore and a delete of the parent
    /// land in some order (H18). A node below an added directory, where a tracked node stands
    /// only once a move put it there, is restored under the State's moves permit, which a C API
    /// delete holds from its check of an added subtree to its detach, so a caller must not hold
    /// the permit itself: tokio's semaphore is not reentrant. A parent a delete detached since
    /// ends the climb, the restore having come first. [`SlowDown`] where the node's parent keeps
    /// changing.
    #[allow(clippy::manual_async_fn)]
    pub fn node_restore<'a>(
        &'a self,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
        content: &'a Content,
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        async move {
            if !node.is_valid_node_id() {
                return Err(invalid_rewrite_id(Rewrite::Restore));
            }
            let parent = match self
                .restore_below(repository, node, content, Some(MODIFIED), false)
                .await?
            {
                Restored::Below(parent) => parent,
                // Boxed: rare, and its permit would otherwise grow every restore's future.
                Restored::AddedParent => {
                    Box::pin(self.restore_ordered(repository, node, content)).await?
                }
            };
            climb_from(self, repository, parent, MODIFIED).await
        }
    }

    /// [`Self::restore_below`] of `node`, below an added directory, under the moves permit.
    async fn restore_ordered(
        &self,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
        content: &Content,
    ) -> Result<NodeID, StateError> {
        let permit = self.moves_permit().await?;
        let restored = self
            .restore_below(repository, node, content, Some(MODIFIED), true)
            .await;
        drop(permit);
        match restored? {
            Restored::Below(parent) => Ok(parent),
            Restored::AddedParent => Err(StateError::internal(
                "a restore under the moves permit asked for the permit",
            )),
        }
    }

    /// Rewrites file `node`'s content as an edit records it, for the C API's modify: the content,
    /// and the staged and dirty flags of an edit in the same write, staged and dirty modified, or
    /// added still where the file is staged for addition ([`State::staged_edit_flags`]). Before
    /// the rewrite, its parent and their ancestors are marked staged and dirty with the base bits,
    /// each up to the first holding them (H8), and a modify that fails or is dropped after the
    /// climb began and before the rewrite notes an unfinished change
    /// ([`Self::note_unfinished_change`]), but under the root, whose climb only dirties block 0;
    /// a file another task moved meanwhile climbs from its new parent after. Refused as
    /// [`State::node_rewrite`] refuses a [`Rewrite::Plain`], a node read as no live file before
    /// any mark. A detached directory ends a climb, and the rewrite decides.
    pub async fn node_modify_staged(
        &self,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
        content: &Content,
    ) -> Result<(), StateError> {
        if !node.is_valid_node_id() {
            return Err(invalid_rewrite_id(Rewrite::Plain));
        }
        let block_index = NodeBlock::index(node);
        let block = match self.resident_block(block_index) {
            Some(block) => block,
            None => self.block(repository.clone(), block_index).await?,
        };
        // The node, and its parent where the block holds it too, under one guard.
        let (parent, flags, parent_flags) = {
            let reader = block.read();
            let record = reader.node(Node::index(node));
            let parent = record.parent;
            let parent_flags = (parent != ROOT_NODE && NodeBlock::index(parent) == block_index)
                .then(|| reader.node(Node::index(parent)).flags);
            (parent, record.flags, parent_flags)
        };
        if !live_file(flags) {
            return Err(rewrite_refused(flags, Rewrite::Plain));
        }
        let climbs = parent == ROOT_NODE
            || !holds_base_marks(if let Some(flags) = parent_flags {
                flags
            } else {
                let parent_block = match self.resident_block(NodeBlock::index(parent)) {
                    Some(block) => block,
                    None => {
                        self.block(repository.clone(), NodeBlock::index(parent))
                            .await?
                    }
                };
                parent_block.node(Node::index(parent)).flags
            });
        // The root's climb marks nothing: it dirties block 0 alone.
        let unfinished = UnfinishedChange::armed_if(climbs && parent != ROOT_NODE, self);
        if climbs {
            climb_from(self, repository, parent, MODIFIED).await?;
        }
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::BeforeRewrite).await;
        let (moved_to, dirtied) = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::StagedModify);
            let mut writer = block.write();
            let record = writer.node(Node::index(node));
            if let Err(flags) = edit_record(record, content) {
                #[cfg(feature = "state-hooks")]
                hook_refused(self, node, node, log::Reason::Rewrite, record);
                drop(writer);
                return Err(rewrite_refused(flags, Rewrite::Plain));
            }
            let moved_to = record.parent;
            (moved_to, writer.mark_dirty())
        };
        unfinished.finished();
        if dirtied {
            // The index taken again, so the future does not hold it across the climb.
            self.block_modified(block, NodeBlock::index(node));
        }
        if moved_to != parent {
            #[cfg(feature = "state-hooks")]
            log::arm(&self.hooks, log::Arm::ModifyMovedClimb);
            return climb_from(self, repository, moved_to, MODIFIED).await;
        }
        Ok(())
    }

    /// [`Rewrite::Restore`] of `node` under its block's write lock and its parent's read lock, and
    /// with `mark` it takes `mark` in the same section: refused where the parent is `Discarded` or
    /// staged for deletion, so a restore and a delete of its parent land in some order, the
    /// delete then reaching the node restored (H18). Answers the parent. A node that moved
    /// meanwhile is followed. Unless `ordered`, its caller holding the moves permit or running
    /// where no C API delete does, a node below an added directory is not restored but answered
    /// [`Restored::AddedParent`]: a delete of the added subtree checks it whole before it detaches
    /// it, and only the permit orders the restore against both.
    async fn restore_below(
        &self,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
        content: &Content,
        mark: Option<Mark>,
        ordered: bool,
    ) -> Result<Restored, StateError> {
        let block_index = NodeBlock::index(node);
        let block = match self.resident_block(block_index) {
            Some(block) => block,
            None => self.block(repository.clone(), block_index).await?,
        };
        for _ in 0..self.chain_change_attempts() {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::BeforeRewrite)
                .await;
            let parent = block.node(Node::index(node)).parent;
            let parent_block = if NodeBlock::index(parent) == block_index {
                block.clone()
            } else {
                match self.resident_block(NodeBlock::index(parent)) {
                    Some(block) => block,
                    None => {
                        self.block(repository.clone(), NodeBlock::index(parent))
                            .await?
                    }
                }
            };
            // Where the parent's block is in memory no await separates the read from the locks, so
            // a move of the node lands between them only from another thread.
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::RestoreLock).await;
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::Restore);
            let mut locks = PairLock::lock((parent, &parent_block), (node, &block));
            let flags = locks.child().flags;
            if restore_refuses(flags) {
                #[cfg(feature = "state-hooks")]
                hook_refused(self, node, node, log::Reason::Restore, locks.child());
                drop(locks);
                return Err(rewrite_refused(flags, Rewrite::Restore));
            }
            if locks.child().parent != parent {
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::RestoreMovedRetry);
                continue;
            }
            if parent != ROOT_NODE {
                let parent_flags = locks.parent().flags;
                let staged_delete = NodeFlags::StagedDelete.bits();
                if parent_flags & NodeFlags::Discarded.bits() != 0
                    || parent_flags & staged_delete == staged_delete
                {
                    #[cfg(feature = "state-hooks")]
                    hook_refused(self, node, parent, log::Reason::Restore, locks.parent());
                    drop(locks);
                    return Err(restore_refused_by_parent(parent_flags));
                }
                if !ordered && locks.parent().is_staged_add() {
                    return Ok(Restored::AddedParent);
                }
            }
            let child = locks.child_mut();
            restore_record(child, content);
            if let Some(mark) = mark {
                mark.apply_to(child);
            }
            locks.release(self);
            return Ok(Restored::Below(parent));
        }
        #[cfg(feature = "state-hooks")]
        log::arm(&self.hooks, log::Arm::SlowDown);
        Err(StateError::from(SlowDown))
    }

    /// Stages tracked node `node`, standing in `parent`, for deletion for a C API delete, and
    /// answers the head of its chain where it is a directory, read under the same locks: the
    /// node's block's write lock and its parent's read lock. A child published into the node
    /// before the mark is reached through that head; one after it is refused, as a C API add
    /// refuses a parent staged for deletion ([`Self::node_add_staged`]). Before the node, the
    /// parent and its ancestors take the base `Staged` and `Dirty` bits where the parent, not the
    /// root, lacks them (H8), climbed with no lock held and the node then marked in a second
    /// section; a climb that fails answers its error with the node unmarked, and a delete that
    /// fails or is dropped after the climb began and before the node's mark notes an unfinished
    /// change ([`Self::note_unfinished_change`]). A level below a node this delete marked finds
    /// its parent holding both, and marks in one section. A node under the root is marked first,
    /// then block 0 dirtied, all a climb from the root does.
    ///
    /// A node already staged for deletion, whose subtree then is too, is [`LevelMark::Staged`]; a
    /// node added, `Discarded`, or no longer in `parent` is [`LevelMark::Other`], for the caller to
    /// take as it reads it.
    pub async fn node_delete_level(
        &self,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
        parent: NodeID,
    ) -> Result<LevelMark, StateError> {
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Discard).await;
        let block_index = NodeBlock::index(node);
        let block = match self.resident_block(block_index) {
            Some(block) => block,
            None => self.block(repository.clone(), block_index).await?,
        };
        let parent_block = match self.resident_block(NodeBlock::index(parent)) {
            Some(block) => block,
            None => {
                self.block(repository.clone(), NodeBlock::index(parent))
                    .await?
            }
        };
        let mut unfinished = UnfinishedChange::unarmed();
        let mut climbed = false;
        let head = loop {
            // The locks' scope ends before the climb's await, as a borrowed guard's would not.
            {
                #[cfg(feature = "state-hooks")]
                let _label = log::label(log::Section::DeleteLevel);
                let mut locks = PairLock::lock((parent, &parent_block), (node, &block));
                {
                    let held = locks.child();
                    if held.is_discarded() || held.parent != parent || held.is_staged_add() {
                        #[cfg(feature = "state-hooks")]
                        log::record(
                            &self.hooks,
                            log::Effect::Refused {
                                node,
                                parent: node,
                                reason: log::Reason::DeleteLevelOther,
                                read: log::Read::Word(log::Words(*held)),
                                expected: log::Expected::At {
                                    parent,
                                    name_hash: held.name_hash,
                                },
                            },
                        );
                        return Ok(LevelMark::Other);
                    }
                    if held.is_staged_delete() {
                        #[cfg(feature = "state-hooks")]
                        log::record(
                            &self.hooks,
                            log::Effect::Refused {
                                node,
                                parent: node,
                                reason: log::Reason::DeleteLevelStaged,
                                read: log::Read::Word(log::Words(*held)),
                                // What `Other` tested first.
                                expected: log::Expected::At {
                                    parent,
                                    name_hash: held.name_hash,
                                },
                            },
                        );
                        return Ok(LevelMark::Staged);
                    }
                }
                if climbed || parent == ROOT_NODE || holds_base_marks(locks.parent().flags) {
                    #[cfg(feature = "state-hooks")]
                    log::record(
                        &self.hooks,
                        log::Effect::Admitted {
                            node,
                            expected: log::Expected::Deletable(parent),
                        },
                    );
                    let child = locks.child_mut();
                    DELETED.apply_to(child);
                    let head = if child.is_directory() {
                        child.child()
                    } else {
                        None
                    };
                    locks.release(self);
                    break head;
                }
            }
            unfinished.arm(self);
            self.node_mark(repository, parent, DELETED.for_ancestors())
                .await?;
            climbed = true;
        };
        unfinished.finished();
        // All a climb from the root does, on the block the level holds: no mark changes the
        // root's flags.
        if parent == ROOT_NODE {
            self.dirty_block(&parent_block, 0);
        }
        Ok(LevelMark::Marked(head))
    }
}

/// Under `state-hooks`, the log's record of a refusal on `read`, the word of `parent`, or of
/// `node` itself where `parent` is `node`.
#[cfg(feature = "state-hooks")]
fn hook_refused(state: &State, node: NodeID, parent: NodeID, reason: log::Reason, read: &Node) {
    log::record(
        &state.hooks,
        log::Effect::Refused {
            node,
            parent,
            reason,
            read: log::Read::Word(log::Words(*read)),
            expected: log::Expected::Nothing,
        },
    );
}

/// What [`State::restore_below`] did.
enum Restored {
    /// Restored the node, standing below this parent.
    Below(NodeID),
    /// Left the node, standing below an added directory, for a restore under the moves permit.
    AddedParent,
}

/// What a C API delete's mark of a node found ([`State::node_delete_level`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelMark {
    /// The node is staged for deletion now; the head of its chain where it is a directory.
    Marked(Option<NodeID>),
    /// The node was staged for deletion already.
    Staged,
    /// The node is added, `Discarded`, or no longer in the parent: not this level's to mark.
    Other,
}

/// Marks `parent` and its ancestors with the base bits of `mark`'s halves, each up to the first
/// holding them: the climb after a change that set the node's own flags under its lock. A parent
/// a delete detached since ends the climb, the change having come first.
fn climb_from<'a>(
    state: &'a State,
    repository: &'a Arc<RepositoryContext>,
    parent: NodeID,
    mark: Mark,
) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
    state
        .node_mark(repository, parent, mark.for_ancestors())
        .map(|marked| match marked {
            Err(error) if !error.is_node_not_found() => Err(error),
            _ => Ok(()),
        })
}

/// The accessors, wrappers and constructors a `test-util` build adds to this module's types, for
/// tests.
#[cfg(feature = "test-util")]
mod test_util {
    use super::*;

    impl State {
        /// Stage a single node for deletion, leaving it in the tree.
        ///
        /// Returns whether the node took the tag; `false` means it already carried
        /// it and nothing was written. The node keeps its name, its parent and its
        /// place in the sibling chain — only flags change — so the revision still
        /// reads it and the commit that freezes the tree is what discards it. This
        /// is the tagging half of a deletion; a node staged for addition has nothing
        /// to delete in the revision it was loaded from and is discarded outright
        /// instead. The C API delete marks through [`Self::node_delete_level`].
        ///
        /// Recursion is the caller's: this stages the one node it is given, not the
        /// subtree under it.
        ///
        /// # Concurrency
        ///
        /// Safe for distinct nodes to run concurrently. Each tag takes the target's block write
        /// lock, and no parent or sibling pointer is written, so no directory's child chain is
        /// touched. The walk to the root that marks ancestors staged takes one block write lock
        /// at a time and stops at the first ancestor already marked.
        pub async fn node_delete(
            &self,
            repository: Arc<RepositoryContext>,
            node_id: NodeID,
        ) -> Result<bool, StateError> {
            if !node_id.is_valid_or_root_node_id() {
                return Err(StateError::from(InvalidArguments {
                    reason: "node id does not name a deletable node".into(),
                }));
            }
            let block_index = NodeBlock::index(node_id);
            let block = self.block(repository.clone(), block_index).await?;
            {
                let node = block.node(Node::index(node_id));
                if node.is_discarded() {
                    return Err(StateError::from(InvalidArguments {
                        reason: "cannot delete a discarded node".into(),
                    }));
                }
                if node.is_staged_delete() {
                    return Ok(false);
                }
            }

            self.node_mark(
                &repository,
                node_id,
                Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
            )
            .await?;
            Ok(true)
        }
    }
}
