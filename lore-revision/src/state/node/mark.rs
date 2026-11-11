// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Staged and dirty marks of a node and its ancestors.

use std::future::Future;
use std::sync::Arc;

use smallvec::SmallVec;

use super::ServedNodes;
use super::locks::PairLock;
use crate::errors::NodeNotFound;
use crate::errors::SlowDown;
use crate::lore_trace;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

/// The most levels a served mark marks in one pass. A directory found clean this many levels
/// above the target is marked first, so each pass stays within an on-stack path. Four under
/// `state-hooks`, so the scheduler's scenarios reach the deep path with small trees.
#[cfg(not(feature = "state-hooks"))]
const SERVED_MARK_LEVELS: usize = 64;
#[cfg(feature = "state-hooks")]
const SERVED_MARK_LEVELS: usize = 4;

/// The most levels a served mark climbs in all: deeper than any path, so a chain this long is a
/// cycle a corrupt State holds.
const SERVED_MARK_DEPTH: usize = 1 << 16;

/// The nodes a served mark's pass marks, from the target up, each with its block.
type ServedPath = SmallVec<[(NodeID, Arc<NodeBlock>); SERVED_MARK_LEVELS]>;

/// How one pass of a served mark ended.
enum Pass {
    /// Every level is marked.
    Done,
    /// The path moved, or a parent no longer admits the change: pass again.
    Retry,
    /// The block of this index is not in memory: load it, hold it, and pass again; or, for a
    /// resident-only mark, answer the mark not made.
    Load(usize),
    /// This directory is still clean [`SERVED_MARK_LEVELS`] levels up: mark it first.
    Deep(NodeID),
    /// A node on the path is detached.
    NotFound,
}

/// What a served mark answers, by whether it reads only blocks in memory: nothing for one that
/// loads what it needs, whether the mark is made for one that stops at a block not in memory. A
/// type parameter rather than a flag, so the marks that load compile with no resident-only branch
/// on their hot path and answer `()` with no adapter around their futures.
pub(in crate::state) trait MarkAnswer: Sized + 'static {
    /// Whether the mark reads only blocks in memory.
    const RESIDENT_ONLY: bool;
    /// The answer of a mark made.
    const MADE: Self;
    /// The answer of a resident-only mark that needs a block not in memory.
    const NOT_MADE: Self;
}

impl MarkAnswer for () {
    const RESIDENT_ONLY: bool = false;
    const MADE: Self = ();
    const NOT_MADE: Self = ();
}

impl MarkAnswer for bool {
    const RESIDENT_ONLY: bool = true;
    const MADE: Self = true;
    const NOT_MADE: Self = false;
}

/// What a served mark keeps across passes only after a load or a deep directory.
#[derive(Default)]
struct ColdPasses {
    /// Blocks a pass found evicted, loaded and kept alive for the passes after it.
    held: Vec<Arc<NodeBlock>>,
    /// Targets waiting below a deep directory marked first, each with the passes it took.
    waiting: Vec<(NodeID, u32)>,
}

/// Notes an unfinished change on its State where dropped armed ([`State::note_unfinished_change`]):
/// armed by a C API change before it marks the directories above its node, and disarmed once its
/// node changed, so a change that fails or is dropped between leaves a note of the directories it
/// staged with no change below them (H8).
pub(in crate::state) struct UnfinishedChange<'a>(Option<&'a State>);

/// Under `state-hooks`, the log's record of a served mark's level refused on `read`, the word of
/// `of`, the child or the parent it hangs from, Discarded.
#[cfg(feature = "state-hooks")]
fn hook_level_gone(state: &State, child: NodeID, of: NodeID, read: &Node) {
    log::record(
        &state.hooks,
        log::Effect::Refused {
            node: child,
            parent: of,
            reason: log::Reason::NotFound,
            read: log::Read::Word(log::Words(*read)),
            expected: log::Expected::Nothing,
        },
    );
}

impl<'a> UnfinishedChange<'a> {
    /// A guard armed where `marks`, a mark of the directories above the node to come.
    #[inline]
    pub(in crate::state) fn armed_if(marks: bool, state: &'a State) -> Self {
        #[cfg(feature = "state-hooks")]
        if marks {
            hook_guard(state, log::GuardEvent::Armed);
        }
        Self(marks.then_some(state))
    }

    /// A guard to arm later, once a mark turns out to be needed.
    #[inline]
    pub(in crate::state) fn unarmed() -> Self {
        Self(None)
    }

    /// Arms the guard, before the first mark.
    #[inline]
    pub(in crate::state) fn arm(&mut self, state: &'a State) {
        #[cfg(feature = "state-hooks")]
        hook_guard(state, log::GuardEvent::Armed);
        self.0 = Some(state);
    }

    /// The change went through: nothing to note.
    #[inline]
    pub(in crate::state) fn finished(mut self) {
        #[cfg(feature = "state-hooks")]
        if let Some(state) = self.0 {
            hook_guard(state, log::GuardEvent::Finished);
        }
        self.0 = None;
    }
}

impl Drop for UnfinishedChange<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.0 {
            #[cfg(feature = "state-hooks")]
            hook_guard(state, log::GuardEvent::DroppedArmed);
            state.note_unfinished_change();
        }
    }
}

/// Under `state-hooks`, the log's record of a guard's `event`.
#[cfg(feature = "state-hooks")]
fn hook_guard(state: &State, event: log::GuardEvent) {
    log::record(&state.hooks, log::Effect::Guard { event });
}

/// Whether a directory holding `flags` holds both base bits a C API change's climb sets, as its
/// ancestors then do too, or a climb that set them is on its way up: a climb from it stops there.
/// A climb that failed midway, on a block it could not load, leaves its levels below marked and
/// those above not (C12).
#[inline]
pub(in crate::state) fn holds_base_marks(flags: u16) -> bool {
    let marked = (NodeFlags::Staged | NodeFlags::Dirty).bits();
    flags & marked == marked
}

/// How an unserved mark's target takes extension flags ([`Mark::dirty`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ext {
    /// The target keeps the extension flags it holds, as each ancestor does.
    Keep,
    /// The target takes these in place of the extension flags it holds.
    Set(NodeExtFlags),
}

/// An unserved mark ([`State::node_mark`]): its staged half, its dirty half, or both, each with
/// the target's flags and whether to set them where the target already holds them (`force`).
/// Each ancestor takes the base `Staged` or `Dirty` bit alone, up to the first that holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// The target's staged flags, masked to [`NodeFlags::StagedBits`], and `force`.
    pub staged: Option<(NodeFlags, bool)>,
    /// The target's dirty flags, masked to [`NodeFlags::DirtyBits`], its extension flags, and
    /// `force`.
    pub dirty: Option<(NodeFlags, Ext, bool)>,
}

impl Mark {
    /// No mark: neither half.
    pub(in crate::state) const NONE: Self = Self {
        staged: None,
        dirty: None,
    };

    /// The staged half alone.
    pub const fn staged(flags: NodeFlags, force: bool) -> Self {
        Self {
            staged: Some((flags, force)),
            dirty: None,
        }
    }

    /// The dirty half alone.
    pub const fn dirty(flags: NodeFlags, ext: Ext, force: bool) -> Self {
        Self {
            staged: None,
            dirty: Some((flags, ext, force)),
        }
    }

    /// A staged change and the matching dirty change, both forced, as the working-tree staging
    /// path records them: a staged change recorded without its dirty counterpart leaves the two
    /// views of the tree disagreeing. The target's extension flags are cleared.
    pub const fn staged_and_dirty(staged: NodeFlags, dirty: NodeFlags) -> Self {
        Self {
            staged: Some((staged, true)),
            dirty: Some((dirty, Ext::Set(NodeExtFlags::NoFlags), true)),
        }
    }

    /// The mark each ancestor of this mark's target takes: the base `Staged` and `Dirty` bits of
    /// the halves this mark has, each stopping at the first ancestor holding it. A caller that
    /// set the target's own flags elsewhere, as [`State::node_move`] does under its locks, climbs
    /// with it from the target's parent.
    pub const fn for_ancestors(self) -> Self {
        Self {
            staged: match self.staged {
                Some(_) => Some((NodeFlags::Staged, false)),
                None => None,
            },
            dirty: match self.dirty {
                Some(_) => Some((NodeFlags::Dirty, Ext::Keep, false)),
                None => None,
            },
        }
    }

    /// Marks `node` alone as [`State::node_mark`] marks its target, under a lock its caller
    /// holds; answers whether it wrote.
    #[inline]
    pub(in crate::state) fn apply_to(self, node: &mut Node) -> bool {
        Halves::of(self).apply(node)
    }
}

/// One half of an unserved mark while it climbs: the flags it sets, and whether it sets them on
/// a node already holding them.
#[derive(Clone, Copy)]
struct Half {
    flags: u16,
    force: bool,
}

/// The halves of an unserved mark still climbing, each `None` once it stopped at a node already
/// holding it, or where the mark has none; packed, as the walk's future holds them.
#[derive(Clone, Copy)]
struct Halves {
    staged: Option<Half>,
    dirty: Option<Half>,
    /// The extension flags the target takes, where `set_ext`.
    ext: u32,
    set_ext: bool,
    /// Whether the dirty half met a Discarded node.
    not_found: bool,
}

impl Halves {
    fn of(mark: Mark) -> Self {
        let (ext, set_ext) = match mark.dirty {
            Some((_, Ext::Set(ext), _)) => (ext.bits(), true),
            _ => (0, false),
        };
        Self {
            staged: mark.staged.map(|(flags, force)| Half {
                flags: (flags & NodeFlags::StagedBits).bits(),
                force,
            }),
            dirty: mark.dirty.map(|(flags, _, force)| Half {
                flags: (flags & NodeFlags::DirtyBits).bits(),
                force,
            }),
            ext,
            set_ext,
            not_found: false,
        }
    }

    fn climbing(&self) -> bool {
        self.staged.is_some() || self.dirty.is_some()
    }

    /// One level of the mark on `node`, under its block's write lock: the staged half, then the
    /// dirty half, each where still climbing, each stopping where `node` holds it already.
    /// Answers whether either wrote; a half that wrote climbs on with the base bit alone, and the
    /// extension flags go to the target only.
    #[inline]
    fn apply(&mut self, node: &mut Node) -> bool {
        let mut wrote = false;
        if let Some(Half { mut flags, force }) = self.staged {
            if !force && node.flags & flags == flags {
                self.staged = None;
            } else {
                // The merge flags stand until an unstage drops them: all of them, a conflict's
                // resolution and side included, under a mark setting no merge state of its own;
                // under one that does, a merge and a conflict stand and it sets the rest.
                let merge = NodeFlags::MergeBits.bits();
                if flags & merge == 0 {
                    flags |= node.flags & merge;
                } else {
                    flags |= node.flags & merge & NodeFlags::StagedMergeConflict.bits();
                }
                node.flags &= !NodeFlags::StagedBits.bits();
                node.flags |= (NodeFlags::Staged.bits() | flags) & NodeFlags::StagedBits.bits();
                self.staged = Some(Half {
                    flags: NodeFlags::Staged.bits(),
                    force: false,
                });
                wrote = true;
            }
        }
        if let Some(Half { flags, force }) = self.dirty {
            if node.is_discarded() {
                self.dirty = None;
                self.not_found = true;
            } else if !force
                && node.flags & flags == flags
                && (!self.set_ext || node.ext_flags == self.ext)
            {
                self.dirty = None;
            } else {
                // The latest action replaces the previous one; staged and merge bits stay. The
                // base bit alone on a clean node keeps the action bits its staged action holds,
                // which the two halves share.
                if flags != NodeFlags::Dirty.bits() || node.is_dirty() {
                    node.flags &= !NodeFlags::DirtyBits.bits();
                }
                node.flags |= flags;
                if self.set_ext {
                    node.ext_flags = self.ext;
                    self.set_ext = false;
                }
                self.dirty = Some(Half {
                    flags: NodeFlags::Dirty.bits(),
                    force: false,
                });
                wrote = true;
            }
        }
        wrote
    }
}

impl State {
    /// The staged and dirty flags an edit to `node` should record.
    ///
    /// A node staged for addition stays staged for addition however it is edited
    /// afterwards: it is in no revision yet, so there is nothing for a commit to
    /// record a modification against.
    pub fn staged_edit_flags(node: &Node) -> (NodeFlags, NodeFlags) {
        if node.is_staged_add() {
            (NodeFlags::StagedAdd, NodeFlags::DirtyAdd)
        } else {
            (NodeFlags::StagedModify, NodeFlags::DirtyModify)
        }
    }

    /// Marks `node` as `mark` says and its ancestors with it, bottom-up, staged and dirty in one
    /// walk: per node the staged half, then the dirty half, each with its own early-out at a node
    /// already holding it, so each stops at the first ancestor marked. The staged half accepts
    /// Discarded nodes; the dirty half stops at one, and the mark then answers [`NodeNotFound`]
    /// once the staged half is done. A half reaching the root, whose flags no mark changes,
    /// dirties block 0; so does a mark of the root, or of no node, which marks nothing else.
    ///
    /// A staged half replaces the target's staged action and keeps its merge flags, a conflict's
    /// resolution and side with them, where the mark sets no merge bit; a mark that sets one, as
    /// a merge's and a resolve's do, keeps a merge and a conflict and sets the rest.
    ///
    /// The unserved tiers' mark: a dirty node is marked before its parent, so a concurrent reader
    /// may see a dirty node under a clean directory until the walk reaches it. An unserved clear
    /// checks a directory's children again after it clears it, which keeps the marks of
    /// concurrent walks ([`State::node_clear`]).
    pub fn node_mark<'a>(
        &'a self,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
        mark: Mark,
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        // Outside the future, so it holds the halves alone and not the mark beside them.
        let mut halves = Halves::of(mark);
        let mut at = node;
        async move {
            // The block of the last level visited; none before the first.
            let mut last_block = usize::MAX;
            while at.is_valid_node_id() {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::ClimbStep)
                    .await;
                // A run ends where the parent's block differs, so outside the scheduler no two
                // passes share one. The index is taken again after the load, so the future does
                // not hold it across it.
                let block = match self.resident_block(NodeBlock::index(at)) {
                    Some(block) => block,
                    None => self.block(repository.clone(), NodeBlock::index(at)).await?,
                };
                let index = NodeBlock::index(at);
                last_block = index;
                let (parent, dirtied) = {
                    #[cfg(feature = "state-hooks")]
                    let _label = log::label(log::Section::UnservedMark);
                    let mut writer = block.write();
                    // The levels in this block one after another under one guard, each node
                    // written before its parent is read; one level per guard under the
                    // scheduler, which parks at each.
                    let mut wrote = false;
                    let parent = loop {
                        let node = writer.node(Node::index(at));
                        if !halves.apply(node) {
                            break None;
                        }
                        wrote = true;
                        lore_trace!("Node {at} marked, now flags {:x}", node.flags);
                        let parent = node.parent;
                        // A level that wrote leaves a half climbing.
                        if cfg!(feature = "state-hooks")
                            || !parent.is_valid_node_id()
                            || NodeBlock::index(parent) != index
                        {
                            break Some(parent);
                        }
                        at = parent;
                    };
                    (parent, wrote && writer.mark_dirty())
                };
                if dirtied {
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::reached(
                        crate::state::node::hooks::Point::BeforeRegistration,
                    )
                    .await;
                    lore_trace!("Block {index} and state marked dirty");
                    self.block_modified(block, index);
                }
                let Some(parent) = parent else {
                    break;
                };
                at = parent;
            }
            // The walk reached the root, or marks the root or no node. A child of the root in
            // block 0 dirtied it already.
            if halves.climbing() && last_block != 0 {
                let block = match self.resident_block(0) {
                    Some(block) => block,
                    None => self.block(repository.clone(), 0).await?,
                };
                self.dirty_block(&block, 0);
            }
            if halves.not_found {
                return Err(NodeNotFound.into());
            }
            Ok(())
        }
    }
}

impl<'s> ServedNodes<'s> {
    /// Marks directory `node` `Dirty` with its ancestors, top-down, as the served chain rule
    /// requires (C11a): a directory already dirty keeps its action and extension flags, and one
    /// that is not takes the base `Dirty` bit alone.
    ///
    /// Each level is marked under its parent's block lock and its own, and only where the parent
    /// is still dirty with no clear pending, so no step of the mark, nor its cancellation, leaves
    /// a clean directory above a dirty one: a clean detach of an ancestor either lands first, and
    /// the mark fails with [`NodeNotFound`], or finds it dirty and refuses. A directory with a
    /// settle's clear pending takes `Dirty` again and loses the pending clear, so the settle keeps
    /// it dirty.
    pub fn node_mark<'a>(
        self,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a
    where
        's: 'a,
    {
        self.state.served_mark(repository, node)
    }
}

/// Whether every node of `path`, a served mark's climb from its target, or the part of it below
/// a level, still hangs from the next, its last from `ancestor`, read top-down; an empty path
/// hangs from anything. A node directly below a Discarded directory stays there, as no change
/// moves a node out of a Discarded directory, so for a Discarded `ancestor`, read before, each
/// link read top-down still held when the next was read: the target was in a detached subtree
/// when its own link was read. (A non-Discarded directory inside a detached subtree may still
/// lose a child to a served reattach where it admits one, which H11's windows can leave
/// possible; the answer is then a spurious `NodeNotFound`, never a lost change.)
fn hangs_from(path: &[(NodeID, Arc<NodeBlock>)], ancestor: NodeID) -> bool {
    let mut above = ancestor;
    path.iter().rev().all(|(node, block)| {
        let hangs = block.read().node(Node::index(*node)).parent == above;
        above = *node;
        hangs
    })
}

impl State {
    /// [`ServedNodes::node_mark`]: a fast path under the target's block lock, else passes that
    /// each climb to the first ancestor admitting a served change and mark the path below it.
    ///
    /// A pass is synchronous outside `state-hooks`: it climbs through blocks in memory, keeping
    /// each level and its block in an on-stack path, then descends it top-down, one pair lock per
    /// level. A block not in memory ends the pass; it is loaded and held for the rest of the
    /// mark, so the next pass finds it. No lock is held across an await.
    pub(in crate::state) fn served_mark<'a>(
        &'a self,
        repository: &'a Arc<RepositoryContext>,
        target: NodeID,
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        self.served_mark_from(repository, target, None)
    }

    /// [`Self::served_mark`] of directory `target`, held in `block`, below `parent`, as a
    /// caller read them from a record that failed the served chain rule's pre-test: the mark
    /// starts its first pass there, skipping the fast path that would test the record again.
    /// A stale `parent` only makes the descent retry: each level checks the parent under its
    /// lock. A `target` detached since the record was read is found at its own level, after the
    /// levels above it are marked: the safe direction. Answers as [`Self::served_mark_from`]
    /// does.
    pub(in crate::state) fn served_mark_below<'a, A: MarkAnswer>(
        &'a self,
        repository: &'a Arc<RepositoryContext>,
        target: NodeID,
        (block, parent): (&Arc<NodeBlock>, NodeID),
    ) -> impl Future<Output = Result<A, StateError>> + Send + 'a {
        self.served_mark_from(repository, target, Some((parent, block.clone())))
    }

    /// [`Self::served_mark`], `from` the target's parent and block where the caller knows the
    /// target fails the rule. A directory found clean [`SERVED_MARK_LEVELS`] levels up is marked
    /// first, the target waiting on a list, so a path of any depth takes no recursion; past
    /// [`SERVED_MARK_DEPTH`] levels the parent chain is taken for a cycle, which only a corrupt
    /// State holds.
    ///
    /// A resident-only mark ([`MarkAnswer::RESIDENT_ONLY`]) answers [`MarkAnswer::NOT_MADE`] at
    /// the first block it needs that is not in memory, which it does not read; the levels it
    /// marked before stay marked, as a cancelled mark leaves them. That holds for a `target`
    /// below the root: the root, or no node, which no resident caller passes (the root admits
    /// every served change), dirties the root block, loading it.
    async fn served_mark_from<A: MarkAnswer>(
        &self,
        repository: &Arc<RepositoryContext>,
        mut target: NodeID,
        mut from: Option<(NodeID, Arc<NodeBlock>)>,
    ) -> Result<A, StateError> {
        if !target.is_valid_node_id() {
            return self.dirty_root_block(repository).await.map(|()| A::MADE);
        }
        if from.is_none()
            && let Some(block) = self.resident_block(NodeBlock::index(target))
        {
            let node_index = Node::index(target);
            let parent = {
                let reader = block.read();
                let node = reader.node(node_index);
                if node.is_discarded() {
                    return Err(NodeNotFound.into());
                }
                if node.is_dirty() && !block.clear_pending(node_index) {
                    return Ok(A::MADE);
                }
                node.parent
            };
            from = Some((parent, block));
        }
        // What only a load or a deep directory needs, boxed so the common mark's future does not
        // carry it; unallocated until then.
        let mut cold: Option<Box<ColdPasses>> = None;
        let mut attempts = 0;
        while attempts < self.chain_change_attempts() {
            attempts += 1;
            let pass = 'pass: {
                let mut path = ServedPath::new();
                let mut at = if let Some((parent, block)) = from.take() {
                    path.push((target, block));
                    parent
                } else {
                    let block_index = NodeBlock::index(target);
                    let Some(block) = self.resident_block(block_index) else {
                        break 'pass Pass::Load(block_index);
                    };
                    let node_index = Node::index(target);
                    let (parent, discarded, admits) = {
                        let reader = block.read();
                        let node = reader.node(node_index);
                        (
                            node.parent,
                            node.is_discarded(),
                            node.is_dirty() && !block.clear_pending(node_index),
                        )
                    };
                    if discarded {
                        break 'pass Pass::NotFound;
                    }
                    if admits {
                        break 'pass Pass::Done;
                    }
                    path.push((target, block));
                    parent
                };
                let mut cycle = SiblingCycleGuard::new(target);
                let top_block = loop {
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::reached(crate::state::node::hooks::Point::ClimbStep)
                        .await;
                    if at == INVALID_NODE {
                        break 'pass Pass::NotFound;
                    }
                    let block_index = NodeBlock::index(at);
                    let block = match path.last() {
                        Some((last, block)) if NodeBlock::index(*last) == block_index => {
                            block.clone()
                        }
                        _ => match self.resident_block(block_index) {
                            Some(block) => block,
                            None => break 'pass Pass::Load(block_index),
                        },
                    };
                    if at == ROOT_NODE {
                        break block;
                    }
                    if cycle.observe(at).is_err() {
                        break 'pass Pass::Retry;
                    }
                    let node_index = Node::index(at);
                    let (parent, discarded, admits) = {
                        let reader = block.read();
                        let node = reader.node(node_index);
                        (
                            node.parent,
                            node.is_discarded(),
                            node.is_dirty() && !block.clear_pending(node_index),
                        )
                    };
                    if discarded {
                        break 'pass if hangs_from(&path, at) {
                            Pass::NotFound
                        } else {
                            Pass::Retry
                        };
                    }
                    if admits {
                        break block;
                    }
                    if path.len() == SERVED_MARK_LEVELS {
                        break 'pass Pass::Deep(at);
                    }
                    path.push((at, block));
                    at = parent;
                };
                // Top-down, one pair lock per level and none held in between, so a level is
                // marked only below one already dirty.
                let mut parent = (at, &top_block);
                for (level, (child, child_block)) in path.iter().enumerate().rev() {
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::reached(
                        crate::state::node::hooks::Point::DescentStep,
                    )
                    .await;
                    match self.served_mark_level(parent, (*child, child_block)) {
                        Pass::Done => parent = (*child, child_block),
                        // A detached level is the target's only where the path below still
                        // hangs from it.
                        Pass::NotFound if !hangs_from(&path[..level], *child) => {
                            break 'pass Pass::Retry;
                        }
                        pass => break 'pass pass,
                    }
                }
                Pass::Done
            };
            match pass {
                // A target resumes with the passes it took, so one whose deep directory keeps
                // turning clean again still runs out of attempts.
                Pass::Done => {
                    let Some(below) = cold.as_mut().and_then(|cold| cold.waiting.pop()) else {
                        return Ok(A::MADE);
                    };
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::MarkWaiting);
                    (target, attempts) = below;
                }
                Pass::NotFound => {
                    // A deep directory marked first speaks for its own path: the target waiting
                    // below it is not found only where its whole chain still hangs from a
                    // detached directory; otherwise it moved away, and its mark starts again.
                    let Some((below, passes)) =
                        cold.as_ref().and_then(|cold| cold.waiting.first().copied())
                    else {
                        return Err(NodeNotFound.into());
                    };
                    // Boxed: a cold climb that would otherwise grow every served mark's future.
                    match Box::pin(self.in_detached_subtree(repository, below, A::RESIDENT_ONLY))
                        .await?
                    {
                        Some(true) => return Err(NodeNotFound.into()),
                        Some(false) => {}
                        None => return Ok(A::NOT_MADE),
                    }
                    if let Some(cold) = cold.as_mut() {
                        cold.waiting.clear();
                    }
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::MarkWaiting);
                    (target, attempts) = (below, passes);
                }
                Pass::Retry => {
                    #[cfg(feature = "state-hooks")]
                    {
                        log::arm(&self.hooks, log::Arm::MarkRetry);
                        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Retry)
                            .await;
                    }
                }
                Pass::Load(block_index) => {
                    if A::RESIDENT_ONLY {
                        #[cfg(feature = "state-hooks")]
                        log::arm(&self.hooks, log::Arm::NeedLoad);
                        return Ok(A::NOT_MADE);
                    }
                    #[cfg(feature = "state-hooks")]
                    {
                        log::arm(&self.hooks, log::Arm::MarkLoad);
                        crate::state::node::hooks::reached(
                            crate::state::node::hooks::Point::NeedLoad,
                        )
                        .await;
                    }
                    let block = self.block(repository.clone(), block_index).await?;
                    let held = &mut cold.get_or_insert_default().held;
                    if !held.iter().any(|kept| Arc::ptr_eq(kept, &block)) {
                        held.push(block);
                    }
                }
                Pass::Deep(at) => {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::MarkDeep);
                    let waiting = &mut cold.get_or_insert_default().waiting;
                    if (waiting.len() + 1) * SERVED_MARK_LEVELS > SERVED_MARK_DEPTH {
                        return Err(StateError::internal(format!(
                            "a served mark of {target} climbed past {SERVED_MARK_DEPTH} levels: \
                             its parent chain holds a cycle"
                        )));
                    }
                    waiting.push((target, attempts));
                    target = at;
                    attempts = 0;
                }
            }
        }
        #[cfg(feature = "state-hooks")]
        log::arm(&self.hooks, log::Arm::SlowDown);
        Err(StateError::from(SlowDown))
    }

    /// Whether `target` stands, now, in a detached subtree: its chain read bottom-up, loading
    /// blocks as needed, to the first Discarded node or the root, then checked top-down
    /// ([`hangs_from`]). For a served mark whose deep directory turned out detached; bounded by
    /// [`SERVED_MARK_DEPTH`], past which the chain is taken for a cycle. `None` only with
    /// `resident_only`, at the first block not in memory, which it does not read.
    async fn in_detached_subtree(
        &self,
        repository: &Arc<RepositoryContext>,
        target: NodeID,
        resident_only: bool,
    ) -> Result<Option<bool>, StateError> {
        let mut path: Vec<(NodeID, Arc<NodeBlock>)> = Vec::new();
        let mut at = target;
        loop {
            if at == INVALID_NODE {
                return Ok(Some(true));
            }
            let block_index = NodeBlock::index(at);
            let block = match self.resident_block(block_index) {
                Some(block) => block,
                None if resident_only => {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::NeedLoad);
                    return Ok(None);
                }
                None => self.block(repository.clone(), block_index).await?,
            };
            let node = block.node(Node::index(at));
            if node.is_discarded() {
                return Ok(Some(hangs_from(&path, at)));
            }
            if at == ROOT_NODE {
                return Ok(Some(false));
            }
            if path.len() == SERVED_MARK_DEPTH {
                return Err(StateError::internal(format!(
                    "the chain of {target} climbed past {SERVED_MARK_DEPTH} levels: it holds a \
                     cycle"
                )));
            }
            path.push((at, block));
            at = node.parent;
        }
    }

    /// One level of a served mark's descent: under the pair lock, `child`, still below `parent`,
    /// which still admits a served change, takes `Dirty` and loses any pending clear. Reaching the
    /// root dirties its block, as every walk that marks up to the root does.
    fn served_mark_level(
        &self,
        (parent, parent_block): (NodeID, &Arc<NodeBlock>),
        (child, child_block): (NodeID, &Arc<NodeBlock>),
    ) -> Pass {
        #[cfg(feature = "state-hooks")]
        let _label = log::label(log::Section::ServedMark);
        let mut locks = PairLock::lock((parent, parent_block), (child, child_block));
        let (discarded, below, dirty) = {
            let node = locks.child();
            (node.is_discarded(), node.parent, node.is_dirty())
        };
        if discarded {
            #[cfg(feature = "state-hooks")]
            hook_level_gone(self, child, child, locks.child());
            return Pass::NotFound;
        }
        // A child moved away from `parent` follows its new path, whatever became of `parent`.
        if below != parent {
            return Pass::Retry;
        }
        if parent != ROOT_NODE {
            let held = locks.parent();
            // The child still hangs from it, so it is in a detached subtree.
            if held.is_discarded() {
                #[cfg(feature = "state-hooks")]
                hook_level_gone(self, child, parent, held);
                return Pass::NotFound;
            }
            if !held.is_dirty() || parent_block.clear_pending(Node::index(parent)) {
                drop(locks);
                self.served_refused();
                return Pass::Retry;
            }
        }
        if !dirty {
            let node = locks.child_mut();
            node.flags &= !NodeFlags::DirtyBits;
            node.flags |= NodeFlags::Dirty;
        }
        locks.drop_child_clear_pending();
        let written = locks.release(self);
        // A child written in block 0 left it dirty already.
        if parent == ROOT_NODE && !(written && NodeBlock::index(child) == 0) {
            self.dirty_block(parent_block, 0);
        }
        Pass::Done
    }

    /// Dirties block 0 and the State, as a mark reaching the root does. A mark of the root, or of
    /// no node, does only this, as the unserved marks do.
    async fn dirty_root_block(
        &self,
        repository: &Arc<RepositoryContext>,
    ) -> Result<(), StateError> {
        let block = match self.resident_block(0) {
            Some(block) => block,
            None => self.block(repository.clone(), 0).await?,
        };
        self.dirty_block(&block, 0);
        Ok(())
    }

    /// Marks `block`, of index `index`, dirty, and registers it and marks the State where it
    /// turned dirty.
    pub(in crate::state) fn dirty_block(&self, block: &Arc<NodeBlock>, index: usize) {
        let dirtied = block.write().mark_dirty();
        if dirtied {
            lore_trace!("Block {index} and state marked dirty");
            self.block_modified(block.clone(), index);
        }
    }
}
