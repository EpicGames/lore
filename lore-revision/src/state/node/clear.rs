// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Clears of a node's staged and dirty marks, and of its ancestors' where nothing below needs them.

use std::future::Future;
use std::sync::Arc;

use crate::lore::Address;
use crate::lore::Hash;
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
use crate::state::node::ServedNodes;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;
use crate::state::node::mark::Ext;
use crate::state::node::mark::Mark;

/// A settle's pending clear of a directory (C11a), from the first phase of its clear
/// ([`ServedNodes::node_clear`]) on: a served change below the directory refuses while it stands,
/// and marks the directory again, which drops it, so the last phase clears only where no change
/// landed below meanwhile. The root, which every served change admits and no mark dirties, is
/// kept by its chain count alone. Dropped unfinished, as by a settle that is cancelled or fails,
/// it drops the pending clear. One settle of a directory runs at a time: the commit's and the
/// swap's settles are serialized, and share the one bit.
struct PendingClear<'a> {
    block: &'a Arc<NodeBlock>,
    dir: NodeID,
    changes: u32,
}

/// A directory a settle compares a served one with: one of another State, the committed
/// revision's or a swap's target, read as it stands.
#[derive(Clone, Copy)]
pub struct ClearAgainst<'a> {
    pub state: &'a State,
    pub repository: &'a Arc<RepositoryContext>,
    pub node: NodeID,
}

/// A settle's clear of one served directory ([`ServedNodes::node_clear`]).
pub struct ServedClear<'a> {
    /// The directory's parent and name hash, as the settle's path resolution found them.
    pub place: (NodeID, u64),
    /// The directory whose children, and whose address and size, the cleared directory takes.
    pub against: ClearAgainst<'a>,
    /// The directory whose name the cleared directory must carry, byte for byte: the committed
    /// one for a commit, the pre-swap revision's for a swap. Not read for the root.
    pub spelled: ClearAgainst<'a>,
    /// Buffers a settle pass reuses across its directories.
    pub scratch: &'a mut ClearScratch,
}

/// The buffers of a settle pass's clears, so the pass allocates once rather than per directory.
#[derive(Default)]
pub struct ClearScratch {
    /// The served directory's children, in chain order.
    children: Vec<ChildContent>,
    /// `(name_hash, index)` into `children`, sorted.
    keys: Vec<(u64, u32)>,
    /// The name of `spelled`.
    name: Vec<u8>,
}

/// What a settle's clear compares of a child: its name hash, kind, content hash and, for a file,
/// mode; and, on the served side, whether a committed child has matched it.
#[derive(Clone, Copy)]
struct ChildContent {
    hash: Hash,
    name_hash: u64,
    kind: u16,
    mode: u16,
    used: bool,
}

impl ChildContent {
    #[inline]
    fn of(node: &Node) -> Self {
        let kind = node.flags & (NodeFlags::File | NodeFlags::Link).bits();
        Self {
            hash: node.address.hash,
            name_hash: node.name_hash,
            kind,
            mode: if node.is_file() { node.mode } else { 0 },
            used: false,
        }
    }

    #[inline]
    fn holds(&self, other: &Self) -> bool {
        self.kind == other.kind && self.mode == other.mode && self.hash == other.hash
    }
}

/// How a settle's clear of a served directory ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearOutcome {
    /// The directory took the address and size of the directory compared with, clean.
    Cleared,
    /// The directory stays dirty: a child is dirty or differs, its name differs, or a change
    /// landed meanwhile; its action bits go. Where the directory compared with is detached or no
    /// directory, it is kept before the first phase, its action bits kept, as nothing compared
    /// with matches it.
    Kept,
    /// The directory no longer stands live at its place, or is no directory: nothing changed,
    /// and the settle resolves the path afresh.
    Moved,
}

impl<'s> ServedNodes<'s> {
    /// A settle's clear of served directory `dir` (C11b), in three phases, where `clear.against`
    /// is a directory not flagged `Discarded`; otherwise [`ClearOutcome::Kept`], changing nothing. First, under its
    /// write lock: a directory no longer live at `clear.place` is [`ClearOutcome::Moved`];
    /// otherwise its action bits go, whatever the outcome (H20), its clear is marked pending and
    /// its chain count read. Then its children are compared with those of `clear.against`, by
    /// name hash, kind, content hash and file mode, as a multiset: every child clean, none with
    /// local content, each matching one of the other directory's. Then `before_clear` runs, and,
    /// under the write lock again, the directory takes the other directory's address and size,
    /// clean, only where the comparison held, the pending clear survived, the count is unchanged
    /// and the directory still stands at its place carrying `clear.spelled`'s name. A served
    /// change below meanwhile refuses and marks the directory again, which drops the pending
    /// clear; the root, which every served change admits, is kept by its count alone. Dropped
    /// midway, the pending clear goes and the directory stays dirty.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn node_clear<'a>(
        self,
        repository: &'a Arc<RepositoryContext>,
        dir: NodeID,
        clear: ServedClear<'a>,
        before_clear: impl Future<Output = ()> + 'a,
    ) -> impl Future<Output = Result<ClearOutcome, StateError>> + 'a
    where
        's: 'a,
    {
        let ServedClear {
            place,
            against,
            spelled,
            scratch,
        } = clear;
        async move {
            let state = self.state;
            // With its name table, which the last phase reads under the write lock.
            let block = state
                .block_with_nametable(repository.clone(), NodeBlock::index(dir))
                .await?;
            let (against_head, content) = {
                let block_index = NodeBlock::index(against.node);
                let against_block = match against.state.resident_block(block_index) {
                    Some(block) => block,
                    None => {
                        against
                            .state
                            .block(against.repository.clone(), block_index)
                            .await?
                    }
                };
                let node = against_block.node(Node::index(against.node));
                if node.is_discarded() || !node.is_directory() {
                    return Ok(ClearOutcome::Kept);
                }
                (node.child(), (node.address, node.size))
            };
            let Some((pending, head)) = self.begin_settle(&block, dir, place) else {
                #[cfg(feature = "state-hooks")]
                log::arm(&state.hooks, log::Arm::ClearMoved);
                return Ok(ClearOutcome::Moved);
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::ClearPhase).await;
            let equal = children_equal(
                state,
                repository,
                (dir, head),
                &block,
                (against, against_head),
                scratch,
            )
            .await?;
            let named = if !equal || dir == ROOT_NODE {
                true
            } else {
                copy_name(spelled, &mut scratch.name).await?
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::ClearPhase).await;
            before_clear.await;
            let name = (dir != ROOT_NODE).then_some(scratch.name.as_slice());
            Ok(
                if self.finish_settle(pending, place, equal && named, name, content) {
                    ClearOutcome::Cleared
                } else {
                    ClearOutcome::Kept
                },
            )
        }
    }

    /// [`Self::node_clear`]'s first phase: under the write lock, `None` where `dir` is no longer
    /// a live directory at `place`; otherwise its action bits dropped, its clear pending, and
    /// the head of its chain.
    fn begin_settle<'a>(
        self,
        block: &'a Arc<NodeBlock>,
        dir: NodeID,
        place: (NodeID, u64),
    ) -> Option<(PendingClear<'a>, Option<NodeID>)>
    where
        's: 'a,
    {
        let index = Node::index(dir);
        let (pending, head, dirtied) = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::SettlePhase1);
            let mut writer = block.write();
            let node = writer.node(index);
            if node.is_discarded()
                || !node.is_directory()
                || node.parent != place.0
                || node.name_hash != place.1
            {
                return None;
            }
            #[cfg(feature = "state-hooks")]
            log::record(
                &self.state.hooks,
                log::Effect::Admitted {
                    node: dir,
                    expected: log::Expected::At {
                        parent: place.0,
                        name_hash: place.1,
                    },
                },
            );
            let head = node.child();
            let acted = node.flags & NodeFlags::ActionBits.bits() != 0;
            node.flags &= !NodeFlags::ActionBits.bits();
            debug_assert!(
                !block.clear_pending(index),
                "two settles of directory {dir} at once"
            );
            writer.set_clear_pending(index);
            let pending = PendingClear {
                block,
                dir,
                changes: block.chain_changes(index),
            };
            (pending, head, acted && writer.mark_dirty())
        };
        if dirtied {
            self.state
                .block_modified(block.clone(), NodeBlock::index(dir));
        }
        Some((pending, head))
    }

    /// [`Self::node_clear`]'s last phase: under the write lock, the pending clear goes, and the
    /// directory takes `content`, clean, where the comparison held (`equal`), the pending clear
    /// survived, the count is the one the first phase read, and the directory still stands live
    /// at `place` carrying `name` (unchecked where `None`, for the root). Answers whether it did.
    fn finish_settle(
        self,
        pending: PendingClear<'_>,
        place: (NodeID, u64),
        equal: bool,
        name: Option<&[u8]>,
        (address, size): (Address, u64),
    ) -> bool {
        // The pending clear goes below, under the lock the phase takes, not in its drop.
        let pending = std::mem::ManuallyDrop::new(pending);
        let (block, dir, changes) = (pending.block, pending.dir, pending.changes);
        let index = Node::index(dir);
        let dirtied = {
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::SettlePhase3);
            let mut writer = block.write();
            let survived = block.clear_pending(index);
            writer.drop_clear_pending(index);
            if !equal {
                return false;
            }
            // Kept by a race: a change below since phase 1, or the directory moved or renamed.
            if !survived || block.chain_changes(index) != changes {
                drop(writer);
                self.state.clear_overtaken();
                #[cfg(feature = "state-hooks")]
                log::arm(&self.state.hooks, log::Arm::ClearKept);
                return false;
            }
            let node = writer.node(index);
            if node.is_discarded() || node.parent != place.0 || node.name_hash != place.1 {
                #[cfg(feature = "state-hooks")]
                log::arm(&self.state.hooks, log::Arm::ClearKept);
                return false;
            }
            if let Some(name) = name
                && writer.node_name_bytes(index) != Some(name)
            {
                #[cfg(feature = "state-hooks")]
                log::arm(&self.state.hooks, log::Arm::ClearKept);
                return false;
            }
            #[cfg(feature = "state-hooks")]
            log::record(
                &self.state.hooks,
                log::Effect::Admitted {
                    node: dir,
                    expected: log::Expected::At {
                        parent: place.0,
                        name_hash: place.1,
                    },
                },
            );
            let node = writer.node(index);
            node.address = address;
            node.size = size;
            node.clear_dirty_flags();
            writer.mark_dirty()
        };
        if dirtied {
            self.state
                .block_modified(block.clone(), NodeBlock::index(dir));
        }
        true
    }
}

/// Whether served directory `dir`, held in `block`, its chain starting at `head` as the first
/// phase read it, holds exactly the children of `against`, its chain starting at `against_head`:
/// each child clean with no local content, the two equal as multisets of [`ChildContent`].
/// Children in the same order cost one pass with no sort. A walk that finds the chain changed
/// under it answers false; the last phase's count check would refuse the clear anyway.
async fn children_equal(
    state: &State,
    repository: &Arc<RepositoryContext>,
    (dir, head): (NodeID, Option<NodeID>),
    block: &Arc<NodeBlock>,
    (against, against_head): (ClearAgainst<'_>, Option<NodeID>),
    scratch: &mut ClearScratch,
) -> Result<bool, StateError> {
    let ClearScratch { children, keys, .. } = scratch;
    children.clear();
    keys.clear();
    let seed = Some((NodeBlock::index(dir), block));
    let complete = walk_chain(state, repository, dir, head, true, seed, |node| {
        if node.is_dirty() || node.ext_flags().contains(NodeExtFlags::LocalContent) {
            return false;
        }
        children.push(ChildContent::of(node));
        true
    })
    .await?;
    if !complete {
        return Ok(false);
    }
    // In chain order first: a committed directory lists its children as the served one it was
    // stored from does. From the first child out of order on, the rest match by name hash.
    let mut matched = 0;
    let mut in_order = true;
    let complete = walk_chain(
        against.state,
        against.repository,
        against.node,
        against_head,
        false,
        None,
        |node| {
            let wanted = ChildContent::of(node);
            if in_order {
                if let Some(child) = children.get_mut(matched)
                    && child.name_hash == wanted.name_hash
                    && child.holds(&wanted)
                {
                    child.used = true;
                    matched += 1;
                    return true;
                }
                in_order = false;
                keys.extend(
                    children
                        .iter()
                        .enumerate()
                        .skip(matched)
                        .map(|(at, child)| (child.name_hash, at as u32)),
                );
                keys.sort_unstable();
            }
            let first = keys.partition_point(|&(name_hash, _)| name_hash < wanted.name_hash);
            for &(name_hash, at) in &keys[first..] {
                if name_hash != wanted.name_hash {
                    break;
                }
                let child = &mut children[at as usize];
                if !child.used && child.holds(&wanted) {
                    child.used = true;
                    matched += 1;
                    return true;
                }
            }
            false
        },
    )
    .await?;
    Ok(complete && matched == children.len())
}

/// How a stretch of a chain walk over blocks in memory ended ([`walk_stretch`]).
enum Stretch {
    /// The walk reached the chain's end, or stopped: whether it reached the end.
    Done(bool),
    /// The block of this index, holding the child to resume at, is not in memory.
    Load(NodeID, usize),
    /// One child read, the walk parks before the next.
    Park(Option<NodeID>),
}

/// Walks the chain of directory `dir` in `state` from `at` over blocks in memory, handing each
/// child's record to `visit` under its block's read lock, until `visit` answers false, the chain
/// ends or changes (a child detached or hanging from elsewhere, or a cycle), a block is not in
/// memory, or, with `one`, a child is read. Children one after another in one block are read
/// under one guard; the block in `seed`, one the caller holds, is not looked up.
fn walk_stretch(
    state: &State,
    dir: NodeID,
    mut at: Option<NodeID>,
    seed: Option<(usize, &Arc<NodeBlock>)>,
    cycle: &mut SiblingCycleGuard,
    one: bool,
    visit: &mut impl FnMut(&Node) -> bool,
) -> Stretch {
    while let Some(mut child) = at {
        let block_index = NodeBlock::index(child);
        let mut resident: Option<Arc<NodeBlock>> = None;
        let block = match seed {
            Some((index, block)) if index == block_index => block,
            _ => match state.resident_block(block_index) {
                Some(block) => &*resident.insert(block),
                None => return Stretch::Load(child, block_index),
            },
        };
        if cycle.observe(child).is_err() {
            #[cfg(feature = "state-hooks")]
            hook_walk_changed(state, one);
            return Stretch::Done(false);
        }
        let reader = block.read();
        at = loop {
            let node = reader.node(Node::index(child));
            if node.is_discarded() || node.parent != dir {
                #[cfg(feature = "state-hooks")]
                hook_walk_changed(state, one);
                return Stretch::Done(false);
            }
            if !visit(node) {
                return Stretch::Done(false);
            }
            match node.sibling() {
                Some(next) if !one && NodeBlock::index(next) == block_index => {
                    if cycle.observe(next).is_err() {
                        return Stretch::Done(false);
                    }
                    child = next;
                }
                next if one => return Stretch::Park(next),
                next => break next,
            }
        };
    }
    Stretch::Done(true)
}

/// Under `state-hooks`, the arm of a served walk, one parking before each child (`one`), that
/// found its chain changed.
#[cfg(feature = "state-hooks")]
fn hook_walk_changed(state: &State, one: bool) {
    if one {
        log::arm(&state.hooks, log::Arm::ServedWalkChanged);
    }
}

/// Walks the chain of directory `dir` in `state` from `head`, handing each child's record to
/// `visit` under its block's read lock, until `visit` answers false. Answers whether it reached
/// the chain's end: false where `visit` stopped it, or where the chain changed under the walk (a
/// child detached or hanging from elsewhere, or a cycle). Synchronous over blocks in memory; a
/// block loaded is held until the next is, so the walk resumes in it. A walk of a served chain
/// (`served`) parks before each child under `state-hooks`, so changes interleave with it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn walk_chain<'a>(
    state: &'a State,
    repository: &'a Arc<RepositoryContext>,
    dir: NodeID,
    head: Option<NodeID>,
    served: bool,
    seed: Option<(usize, &'a Arc<NodeBlock>)>,
    mut visit: impl FnMut(&Node) -> bool + 'a,
) -> impl Future<Output = Result<bool, StateError>> + 'a {
    async move {
        // A served walk under the scheduler parks before each child, so it reads one per stretch.
        let one = served && cfg!(feature = "state-hooks");
        let mut cycle = SiblingCycleGuard::new(dir);
        // The block last loaded, held so the walk resumes in it in memory.
        let mut _loaded: Option<Arc<NodeBlock>> = None;
        let mut at = head;
        loop {
            #[cfg(feature = "state-hooks")]
            if one && at.is_some() {
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::IterStep)
                    .await;
            }
            match walk_stretch(state, dir, at, seed, &mut cycle, one, &mut visit) {
                Stretch::Done(complete) => return Ok(complete),
                Stretch::Park(next) => at = next,
                Stretch::Load(child, block_index) => {
                    _loaded = Some(state.block(repository.clone(), block_index).await?);
                    at = Some(child);
                }
            }
        }
    }
}

/// Copies the name of `spelled` into `name`; false where it cannot be read, which refuses the
/// clear.
async fn copy_name(spelled: ClearAgainst<'_>, name: &mut Vec<u8>) -> Result<bool, StateError> {
    let block = spelled
        .state
        .block_with_nametable(spelled.repository.clone(), NodeBlock::index(spelled.node))
        .await?;
    Ok(
        block.read_node_and_name(Node::index(spelled.node), |node, held| {
            name.clear();
            match held {
                Ok(held) if !node.is_discarded() => {
                    name.extend_from_slice(held.as_bytes());
                    true
                }
                _ => false,
            }
        }),
    )
}

impl Drop for PendingClear<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "state-hooks")]
        let _label = log::label(log::Section::SettlePhase3);
        self.block.write().drop_clear_pending(Node::index(self.dir));
    }
}

/// Which marks an unserved clear drops ([`State::node_clear`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clear {
    /// The node's staged flags, the node alone: an ancestor's staged bits stand for a sibling's
    /// action as much as for this one's.
    Staged,
    /// The node's dirty flags, and each ancestor's that nothing below keeps dirty.
    Dirty,
    /// The dirty flags of the node, a directory one of whose children turned clean or left, and of
    /// each ancestor, each where nothing below keeps it dirty: a dirty clear's climb, from the
    /// directory itself.
    Vacated,
}

impl State {
    /// Clears `node`'s marks as `clear` says, in the unserved tiers. The node itself is cleared
    /// whatever it holds; its staged flags, or its action bits where it is staged, stay with a
    /// dirty clear (see [`Node::clear_dirty_flags`]). [`Clear::Vacated`] takes the node as the
    /// climb's first ancestor instead, for a directory one of whose children turned clean or left.
    ///
    /// A dirty clear climbs on, each ancestor in four phases, holding no lock between them: an
    /// ancestor already clean ends the walk, as only a walk that carries on above it, or marks it
    /// again, cleans one; one with a dirty child ends it too, staying dirty; otherwise it is
    /// cleared under its write lock, which reads its chain head, and its children are checked again
    /// from there, the ancestor marked again where one turned dirty. A mark writes a node before it
    /// reads its parent, and the clear writes the ancestor before it reads the children again, each
    /// under the block's lock: so a concurrent mark that stopped at the ancestor still dirty is
    /// found by the second check, and one reading it after finds it clean and marks it itself, as
    /// does an add that marks the parent after its publish. Not covered: a writer that marks a
    /// parent before it dirties a node below it (an unserved reattach), which no operation clearing
    /// runs alongside; and a clear dropped between its clear of an ancestor and its re-mark's
    /// write, which can leave the ancestor clean over a child marked meanwhile, as a mark dropped
    /// midway, or an add dropped between its publish and its parent's mark, leaves one. The second
    /// check finds a child detached, hanging from elsewhere or in a cycle, not a child moved within
    /// the directory behind it, which only a writer marking before it moves could leave dirty
    /// unseen. The root is never cleared. The inverse of [`State::node_mark`].
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn node_clear<'a>(
        &'a self,
        repository: &'a Arc<RepositoryContext>,
        node: NodeID,
        clear: Clear,
    ) -> impl Future<Output = Result<(), StateError>> + Send + 'a {
        async move {
            if !node.is_valid_node_id() {
                return Ok(());
            }
            // Scoped, so the block's own binding holds no storage once it moves into `held`.
            let (mut at, mut held) = {
                let block = match self.resident_block(NodeBlock::index(node)) {
                    Some(block) => block,
                    None => {
                        self.block(repository.clone(), NodeBlock::index(node))
                            .await?
                    }
                };
                if clear == Clear::Vacated {
                    (node, (NodeBlock::index(node), block))
                } else {
                    let (parent, dirtied) = {
                        #[cfg(feature = "state-hooks")]
                        let _label = log::label(log::Section::UnservedClear);
                        let mut writer = block.write();
                        let held = writer.node(Node::index(node));
                        let was = (held.flags, held.ext_flags);
                        if clear == Clear::Staged {
                            held.clear_staged_flags();
                        } else {
                            held.clear_dirty_flags();
                        }
                        let changed = (held.flags, held.ext_flags) != was;
                        let parent = held.parent;
                        (parent, changed && writer.mark_dirty())
                    };
                    if dirtied {
                        #[cfg(feature = "state-hooks")]
                        crate::state::node::hooks::reached(
                            crate::state::node::hooks::Point::BeforeRegistration,
                        )
                        .await;
                        self.block_modified(block.clone(), NodeBlock::index(node));
                    }
                    if clear == Clear::Staged {
                        return Ok(());
                    }
                    (parent, (NodeBlock::index(node), block))
                }
            };
            while at.is_valid_node_id() {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::ClearPhase)
                    .await;
                if held.0 != NodeBlock::index(at) {
                    let block = match self.resident_block(NodeBlock::index(at)) {
                        Some(block) => block,
                        None => self.block(repository.clone(), NodeBlock::index(at)).await?,
                    };
                    held = (NodeBlock::index(at), block);
                }
                let head = {
                    let reader = held.1.read();
                    let node = reader.node(Node::index(at));
                    if node.is_discarded() || !node.is_dirty() || !node.is_directory() {
                        return Ok(());
                    }
                    node.child()
                };
                if !walk_chain(
                    self,
                    repository,
                    at,
                    head,
                    false,
                    Some((held.0, &held.1)),
                    |child| !child.is_dirty(),
                )
                .await?
                {
                    return Ok(());
                }
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::ClearPhase)
                    .await;
                let (head, parent, dirtied) = {
                    #[cfg(feature = "state-hooks")]
                    let _label = log::label(log::Section::UnservedClear);
                    let mut writer = held.1.write();
                    let node = writer.node(Node::index(at));
                    // Cleared meanwhile by a walk that carries on above it or marks it again, or
                    // detached.
                    if node.is_discarded() || !node.is_dirty() || !node.is_directory() {
                        return Ok(());
                    }
                    node.clear_dirty_flags();
                    (node.child(), node.parent, writer.mark_dirty())
                };
                if dirtied {
                    #[cfg(feature = "state-hooks")]
                    crate::state::node::hooks::reached(
                        crate::state::node::hooks::Point::BeforeRegistration,
                    )
                    .await;
                    self.block_modified(held.1.clone(), held.0);
                }
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::ClearPhase)
                    .await;
                let failed = match walk_chain(
                    self,
                    repository,
                    at,
                    head,
                    false,
                    Some((held.0, &held.1)),
                    |child| !child.is_dirty(),
                )
                .await
                {
                    Ok(true) => {
                        at = parent;
                        continue;
                    }
                    Ok(false) => None,
                    // Boxed, on this path alone: held across the mark below, the error would
                    // grow every clear's future.
                    Err(error) => Some(Box::new(error)),
                };
                // A child dirty, the chain changed, or the walk failed: the directory is marked
                // again before anything is answered. Not boxed: the mark's future fits within the
                // walk's, the clear's largest state.
                self.clear_rechecked();
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::ClearRecheck);
                let marked = self
                    .node_mark(
                        repository,
                        at,
                        Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
                    )
                    .await;
                if let Some(error) = failed {
                    return Err(*error);
                }
                // A directory detached meanwhile, or below one detached, keeps no closure.
                return match marked {
                    Err(error) if error.is_node_not_found() => {
                        self.clear_found_gone();
                        Ok(())
                    }
                    marked => marked,
                };
            }
            Ok(())
        }
    }
}
