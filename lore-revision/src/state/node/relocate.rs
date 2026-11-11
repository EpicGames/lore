// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Moves of a node to another place: staged moves and checked reattaches.

use core::str;
use std::future::Future;
use std::sync::Arc;

use futures::FutureExt;
use lore_error_set::prelude::*;

use super::Served;
use super::ServedNodes;
use super::Tier;
use super::Unserved;
use super::attach::Absent;
use super::attach::AttachOutcome;
use super::attach::NameCheck;
use super::attach::logged_name;
use super::detach::Section;
use super::locks::BlockLocks;
use super::locks::ChainAttempts;
#[cfg(feature = "state-hooks")]
use super::locks::ChainPosition;
use super::locks::ParentRead;
use super::locks::Predecessor;
use super::locks::admits_served_change;
use super::locks::chain_unchanged;
use super::locks::check_live_directory;
use super::locks::prepend_locked;
use super::mark::UnfinishedChange;
use crate::errors::InvalidArguments;
use crate::errors::NodeNotFound;
use crate::errors::SlowDown;
use crate::hash;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::node::validate_node_name_for_store;
use crate::repository::RepositoryContext;
use crate::state::Ext;
use crate::state::Mark;
use crate::state::REATTACH_CHANGES;
use crate::state::State;
use crate::state::StateError;
use crate::state::StateNodeChildrenIterator;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

/// How a served reattach finds out whether its new name is free.
#[derive(Clone, Copy, Debug)]
pub enum ReattachCheck<'a> {
    /// By walking the new parent's chain.
    Walk,
    /// Against the new parent's chain log since this absence ([`State::node_reattach_absent`]).
    Absent(&'a Absent),
    /// The name is held by this node, which the move replaces in its critical section
    /// ([`State::node_reattach_replacing`]).
    Replacing(NodeID),
}

/// What a reattach did, read under the move's locks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reattached {
    /// The node moved; `added` where it counted as added (`DirtyAdd`) as it moved, which no clear
    /// can change between.
    Moved { added: bool },
    /// A live sibling already held the name; this is its id.
    Exists(NodeID),
    /// A directory holding no local content, to another place
    /// ([`ServedNodes::node_reattach_local`]). Nothing moved.
    NotLocal,
}

impl Reattached {
    /// As the [`AttachOutcome`] of moving `node_id`. A reattach of any directory never answers
    /// `NotLocal`; were it to, that is an error.
    fn outcome(self, node_id: NodeID) -> Result<AttachOutcome, StateError> {
        match self {
            Self::Moved { .. } => Ok(AttachOutcome::Attached(node_id)),
            Self::Exists(existing) => Ok(AttachOutcome::Exists(existing)),
            Self::NotLocal => Err(StateError::internal(format!(
                "a reattach of any directory refused directory {node_id}"
            ))),
        }
    }
}

impl<'s> ServedNodes<'s> {
    /// Moves `node_id` to `new_name` under `new_parent` as [`State::node_reattach`] does, under
    /// the served chain rule: neither parent is marked first, but the move goes ahead only where
    /// both are live and dirty with no clear pending, marking each first ([`Self::node_mark`])
    /// where it is not.
    #[inline]
    pub fn node_reattach<'a>(
        self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        new_parent: NodeID,
        new_name: &'a str,
        check: ReattachCheck<'a>,
    ) -> impl Future<Output = Result<AttachOutcome, StateError>> + Send + 'a
    where
        's: 'a,
    {
        let (replacing, absent) = replacing_or_absent(check);
        self.state
            .reattach::<Served, false>(repository, node_id, new_parent, new_name, replacing, absent)
            .map(move |moved| moved?.outcome(node_id))
    }

    /// [`Self::node_reattach`] moving a directory to another place, another parent or name hash,
    /// only where it holds local content (`NodeExtFlags::LocalContent`), read under the move's
    /// locks, under which a clear drops it; any other answers [`Reattached::NotLocal`]. A respell,
    /// which keeps the parent and the name's hash, moves any directory. Answers whether the node
    /// counted as added as it moved.
    #[inline]
    pub fn node_reattach_local<'a>(
        self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        new_parent: NodeID,
        new_name: &'a str,
        check: ReattachCheck<'a>,
    ) -> impl Future<Output = Result<Reattached, StateError>> + Send + 'a
    where
        's: 'a,
    {
        let (replacing, absent) = replacing_or_absent(check);
        self.state
            .reattach::<Served, true>(repository, node_id, new_parent, new_name, replacing, absent)
    }
}

/// What a move does to the node's name ([`State::node_move`], [`State::node_rename`]).
#[derive(Clone, Copy, Debug)]
pub enum NameChange<'a> {
    /// The node keeps its name.
    Keep,
    /// The node takes this name, stored and installed inside the move's critical section, so no
    /// reader sees it torn and a failure leaves the old one.
    Rename(&'a str),
}

/// The State's moves permit ([`State::moves_permit`]). While one is held, no other holder moves
/// a directory to another parent or moves through [`State::node_move_staged`] (C10).
pub struct MovesPermit<'a> {
    _permit: tokio::sync::SemaphorePermit<'a>,
    /// The State whose permit this is, which [`State::node_move_staged`] compares with its own.
    state: &'a State,
}

/// Its release, recorded for the scheduler as its take is, and in the log.
#[cfg(feature = "state-hooks")]
impl Drop for MovesPermit<'_> {
    fn drop(&mut self) {
        log::record(&self.state.hooks, log::Effect::Unpermit);
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::Permit(self.state.hooks.ordinal()),
            true,
        );
    }
}

/// What [`State::move_named`] checks: nothing, for [`State::node_move`]'s caller; or the node and
/// both its parents under the locks ([`unmovable`]), for a caller that walked the new parent's
/// ancestors.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Checks {
    None,
    Refusals,
}

/// The mark of a directory a move takes a node out of: its child's path changed, so the hash a
/// commit derives for it changes, and `rehash_directory` skips a directory that is not staged.
const LEFT: Mark = Mark {
    staged: Some((NodeFlags::Staged, false)),
    dirty: Some((NodeFlags::Dirty, Ext::Keep, false)),
};

/// The holder a reattach replaces, or the absence it checks against, that `check` names.
fn replacing_or_absent(check: ReattachCheck<'_>) -> (Option<NodeID>, Option<&Absent>) {
    match check {
        ReattachCheck::Walk => (None, None),
        ReattachCheck::Absent(absent) => (None, Some(absent)),
        ReattachCheck::Replacing(holder) => (Some(holder), None),
    }
}

impl State {
    /// The staged and dirty change a move records on `node`.
    ///
    /// A node staged for addition stays staged for addition: it is in no revision a move
    /// could be recorded against, and its addition already carries whatever parent and
    /// name it ends up under.
    pub fn staged_move_flags(node: &Node) -> (NodeFlags, NodeFlags) {
        if node.is_staged_add() {
            (NodeFlags::StagedAdd, NodeFlags::DirtyAdd)
        } else {
            (NodeFlags::StagedMove, NodeFlags::DirtyMove)
        }
    }

    /// Reparent and/or rename a node, keeping the identity it already has.
    ///
    /// The node keeps its node id, its `file_id` and its children, and the change is
    /// recorded as a **move**: the delta a commit writes names the same node, so a
    /// consumer reads one move rather than a delete of one node and an add of another.
    /// Naming the node's current parent renames it where it is.
    ///
    /// Every node under a moved directory is recorded as moved as well, as the
    /// working-tree staging path records them. Their own records do not change — the
    /// subtree travels by its parent pointers — but their paths do, and the per-node delta
    /// is what `file history` reads to report the move against each of them. The work is
    /// therefore proportional to the subtree rather than to the one node named.
    ///
    /// A node under the moved directory that is staged for deletion keeps its deletion and
    /// is not descended into: it leaves the revision at the commit that freezes the tree,
    /// and recording a move over it would take the deletion off it.
    ///
    /// The name comes with its hash ([`hash::hash_string`]), which a caller that planned the move
    /// computed already. `walked` says the caller walked the destination's ancestors under
    /// `permit` where the node is a directory going to another parent, the node not among them,
    /// as the C API's plan does for a batch whose entries before this one all applied; a file, or
    /// a directory renamed in place, needs no walk.
    ///
    /// In four steps:
    /// 1. The checks, reading copies. Rejected are the root, an unknown or discarded node, a node
    ///    staged for deletion, a destination that is not a directory or is itself staged for
    ///    deletion, a destination the node already sits under by the name it already has, a name
    ///    the node name table would refuse or has no room for, and a destination inside the
    ///    node's own subtree, walked here unless `walked`.
    /// 2. The directory the node leaves, then the destination, each with its ancestors, marked
    ///    staged and dirty (H8): a commit descends into and rehashes both whatever happens next.
    ///    Neither where it is the root, whose flags no mark changes.
    /// 3. The move itself, [`Self::node_move`]'s section: the name stored, the node detached,
    ///    relinked and given its own staged and dirty flags in one critical section, so a failure
    ///    leaves it where it was, under its old name. The node and both parents are refused there
    ///    where Discarded or staged for deletion.
    /// 4. The subtree marked moved; and, where the node left another directory than the one
    ///    read in step 1, as a concurrent reattach moves a file, that one marked as in step 2.
    ///
    /// Nothing is undone: an undo could strip a mark a concurrent add stopped its climb at. A move
    /// that fails or is dropped between its first mark and its node's move notes an unfinished
    /// change ([`Self::note_unfinished_change`]), under which the C API commit counts only nodes
    /// carrying an action as staged, so the marks it left commit nothing of their own. Dropped
    /// between steps 3 and 4, the subtree lacks its staged-move marks: its content commits as it
    /// is, and its file history lacks the move (C12); and a directory a reattach moved the node out
    /// of meanwhile stays unmarked, which no C API caller reaches, as no unserved reattach runs
    /// beside one. A caller that says it walked where it did not can move a directory into its own
    /// subtree: step 4 then answers an internal error for the cycle the move made, the subtree cut
    /// off from the root, with no unfinished change noted.
    ///
    /// **A name a child of the destination already holds is not rejected here.** Like
    /// [`Self::node_add`], this is always-move rather than move-if-vacant, and the check
    /// belongs to the caller: a batch caller has to hold the name against the tree its
    /// whole batch produces rather than the one in front of it, since moving `x` out of a
    /// directory while moving another `x` into it is legal as a batch and would fail under
    /// every ordering if each step checked the name against the intermediate tree.
    ///
    /// # Concurrency
    ///
    /// The caller holds the moves permit, `permit` ([`Self::moves_permit`]), across its whole
    /// batch, its checks included, so no other mover interleaves with it and step 1 stays true
    /// (C10); adds and changes in place under the same shared claim may run alongside, but for a C
    /// API restore of a node below an added directory, which waits for the permit. A C API delete
    /// holds the permit for its whole batch too, so moves and deletes apply one after the other
    /// (H18). No clear runs alongside: one between a mark and the move could clean a directory the
    /// move then fills, which the clear's contract excludes (C9).
    pub async fn node_move_staged(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        destination_parent_id: NodeID,
        (dst_name, name_hash): (&str, u64),
        permit: &MovesPermit<'_>,
        walked: bool,
    ) -> Result<(), StateError> {
        debug_assert!(
            std::ptr::eq(permit.state, &**self),
            "a staged move under another State's moves permit"
        );
        // Trusted from the caller, so checked where the scheduling hooks are, at no cost in
        // production.
        #[cfg(feature = "state-hooks")]
        assert_eq!(
            name_hash,
            hash::hash_string(dst_name),
            "a node moved as '{dst_name}' carries another name's hash"
        );
        if !node_id.is_valid_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "node id does not name a movable node".into(),
            }));
        }
        if !destination_parent_id.is_valid_or_root_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "destination parent id does not name a node".into(),
            }));
        }
        if let Err(error) = validate_node_name_for_store(dst_name) {
            return Err(StateError::from(InvalidArguments {
                reason: format!("destination name is not storable: {error}"),
            }));
        }

        // The checks read copies in scopes no await crosses, so the future holds none of them.
        let node_index = Node::index(node_id);
        let block = self
            .block_with_nametable(repository.clone(), NodeBlock::index(node_id))
            .await?;
        let (source_parent_id, is_directory, own, renamed, fits) = {
            // The node, whether the name changes and whether its table has room for it, under
            // one guard. The hash is case-insensitive, so only a matching hash needs the stored
            // name read back to tell a rename that changes the case from one that changes
            // nothing.
            let (node, renamed, fits) = block.read_node_names(node_index, |node, names| {
                let renamed = if node.name_hash == name_hash {
                    names.name_of(node).map(|name| name != dst_name)
                } else {
                    Ok(true)
                };
                let fits = matches!(renamed, Ok(true)) && names.fits(node, dst_name.len());
                (*node, renamed, fits)
            });
            if node.is_discarded() {
                return Err(StateError::from(InvalidArguments {
                    reason: "cannot move a discarded node".into(),
                }));
            }
            if node.is_staged_delete() {
                return Err(StateError::from(InvalidArguments {
                    reason: "cannot move a node staged for deletion".into(),
                }));
            }
            if node.name_length == 0 {
                return Err(StateError::from(InvalidArguments {
                    reason: "node id does not resolve to a named node".into(),
                }));
            }
            let renamed = renamed.forward::<StateError>("Node name")?;
            let (staged, dirty) = Self::staged_move_flags(&node);
            (
                node.parent,
                node.is_directory(),
                Mark::staged_and_dirty(staged, dirty),
                renamed,
                fits,
            )
        };

        let destination_block = self
            .block(repository.clone(), NodeBlock::index(destination_parent_id))
            .await?;
        {
            let destination = destination_block.node(Node::index(destination_parent_id));
            if destination.is_discarded() {
                return Err(StateError::from(InvalidArguments {
                    reason: "destination parent has been deleted".into(),
                }));
            }
            if destination.is_staged_delete() {
                return Err(StateError::from(InvalidArguments {
                    reason: "destination parent is staged for deletion, so the moved node \
                             would go with it"
                        .into(),
                }));
            }
            if destination.is_link() {
                return Err(StateError::from(InvalidArguments {
                    reason: "destination parent is a link, which addresses a revision this \
                             state does not hold"
                        .into(),
                }));
            }
            if !destination.is_directory() {
                return Err(StateError::from(InvalidArguments {
                    reason: "destination parent is not a directory".into(),
                }));
            }
            if destination_parent_id != ROOT_NODE && destination.name_length == 0 {
                return Err(StateError::from(InvalidArguments {
                    reason: "destination parent id does not resolve to a named node".into(),
                }));
            }
        }
        let reparented = source_parent_id != destination_parent_id;
        if !renamed && !reparented {
            return Err(StateError::from(InvalidArguments {
                reason: "the node is already under that parent by that name".into(),
            }));
        }
        // Refused before any mark, so a full name table leaves nothing marked; one that fills
        // meanwhile fails the move's section instead.
        if renamed && !fits {
            return Err(StateError::from(InvalidArguments {
                reason: "the node's name table has no room for the destination name".into(),
            }));
        }
        // Walked before any mark, rather than in the section's retry loop: under the permit no
        // other mover changes a directory's parent meanwhile.
        if !walked
            && is_directory
            && reparented
            && self
                .is_inside_subtree(repository.clone(), destination_parent_id, node_id)
                .await?
        {
            return Err(StateError::from(InvalidArguments {
                reason: "the new parent is the node itself or one of its descendants".into(),
            }));
        }

        let marks_source = reparented && source_parent_id != ROOT_NODE;
        let marked = marks_source || destination_parent_id != ROOT_NODE;
        let unfinished = UnfinishedChange::armed_if(marked, self);
        if marks_source {
            self.node_mark(&repository, source_parent_id, LEFT).await?;
        }
        self.node_mark_destination(&repository, destination_parent_id, own)
            .await?;
        let from = self
            .move_named(
                &repository,
                (node_id, &block),
                (destination_parent_id, &destination_block),
                renamed.then_some((dst_name, name_hash)),
                Checks::Refusals,
                own,
            )
            .await?;
        unfinished.finished();

        if is_directory {
            self.mark_subtree_moved(repository.clone(), node_id).await?;
        }
        if from != source_parent_id && from != destination_parent_id && from != ROOT_NODE {
            #[cfg(feature = "state-hooks")]
            log::arm(&self.hooks, log::Arm::MoverBeaten);
            self.node_mark(&repository, from, LEFT).await?;
        }
        Ok(())
    }

    /// The State's moves permit, waiting for it: held, no other holder moves a directory to
    /// another parent or moves through [`Self::node_move_staged`] (C10). A
    /// C API move or delete takes it once for its whole batch, after its shared claim (C13), and
    /// a C API add once per restore of a node below an added directory ([`Self::node_restore`]),
    /// its call holding the shared claim.
    pub async fn moves_permit(&self) -> Result<MovesPermit<'_>, StateError> {
        // Taking the permit orders its holders, so the scheduler must not reorder two takes.
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::touched(
            crate::state::node::hooks::Resource::Permit(self.hooks.ordinal()),
            true,
        );
        #[cfg(feature = "state-hooks")]
        let waiting =
            crate::state::node::hooks::waiting(crate::state::node::hooks::Wait::MovesPermit);
        let permit = self.moves.acquire().await.map_err(|error| {
            StateError::internal(format!("the moves permit is closed: {error}"))
        })?;
        #[cfg(feature = "state-hooks")]
        {
            drop(waiting);
            log::record(&self.hooks, log::Effect::Permit);
        }
        Ok(MovesPermit {
            _permit: permit,
            state: self,
        })
    }

    /// Marks `to`, the directory a move lands in ([`Self::node_move`]), and its ancestors as
    /// `own.for_ancestors()` says: the climb the move's own mark of the node leaves to its caller,
    /// after the move, or before it in [`Self::node_move_staged`]. Nothing where `to` is the root,
    /// whose flags no mark changes: a mark of it dirties block 0 alone, which a move from another
    /// directory writes in its section, and which holds nothing a rename under the root changes.
    pub async fn node_mark_destination(
        &self,
        repository: &Arc<RepositoryContext>,
        to: NodeID,
        own: Mark,
    ) -> Result<(), StateError> {
        if to == ROOT_NODE {
            return Ok(());
        }
        self.node_mark(repository, to, own.for_ancestors()).await
    }

    /// Marks `from`, the parent [`Self::node_move`] answers, with `mark`, as its children changed.
    /// Nothing where `from` is `to`, the directory the node moved to, which
    /// [`Self::node_mark_destination`] marks, or the root, whose flags no mark changes.
    pub async fn node_mark_left(
        &self,
        repository: &Arc<RepositoryContext>,
        (from, to): (NodeID, NodeID),
        mark: Mark,
    ) -> Result<(), StateError> {
        if from == to || from == ROOT_NODE {
            return Ok(());
        }
        self.node_mark(repository, from, mark).await
    }

    /// Moves `node_id` under `new_parent`, its name as `name` says, in one critical section
    /// over the blocks of both parents, the node and its predecessor (H1): the node leaves its
    /// chain, takes its new parent and name, and is prepended to the new chain, so it is never
    /// in no chain and never in two. Its own staged and dirty flags take `own` there, as
    /// [`Self::node_mark`] marks its target with a forced mark: the merge flags kept, a conflict's
    /// resolution and side with them, where `own` sets no merge state of its own. No ancestor is
    /// marked, which the caller does with [`Self::node_mark_destination`]: after the move, so a
    /// clear running alongside meets the moved node only below a directory still dirty or marked
    /// after it, or before it, as [`Self::node_move_staged`] does, beside which no clear runs.
    ///
    /// The old parent's chain change count moves by [`REATTACH_CHANGES`] and the new parent's by
    /// one. Under the same parent, a rename keeps the node's place in the chain, as
    /// [`Self::node_rename`] does, and `Keep` moves nothing; `own` applies either way.
    ///
    /// Nothing is checked: the caller resolved the move where no other task changes what it
    /// resolved, the node, both its parents and the new parent's ancestors, as a merge's grafts of
    /// disjoint subtrees running side by side leave each other's, so it rules out a directory
    /// going into its own subtree. A name a child of `new_parent` already holds is not refused, as
    /// [`Self::node_move_staged`] says. A node that left its parent before the section is followed
    /// to where it went. A node its old parent's chain never reaches, every attempt, is a broken
    /// hierarchy (H5): nothing is written. Answers the parent the node left.
    pub async fn node_move(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        new_parent: NodeID,
        name: NameChange<'_>,
        own: Mark,
    ) -> Result<NodeID, StateError> {
        if !node_id.is_valid_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "node id does not name a movable node".into(),
            }));
        }
        if !new_parent.is_valid_or_root_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "new parent id does not name a node".into(),
            }));
        }
        let renamed = match name {
            NameChange::Keep => None,
            NameChange::Rename(new_name) => {
                if let Err(error) = validate_node_name_for_store(new_name) {
                    return Err(StateError::from(InvalidArguments {
                        reason: format!("new name is not storable: {error}"),
                    }));
                }
                Some((new_name, hash::hash_string(new_name)))
            }
        };
        let block_index = NodeBlock::index(node_id);
        let block = match renamed {
            Some(_) => {
                self.block_with_nametable(repository.clone(), block_index)
                    .await?
            }
            None => self.block(repository.clone(), block_index).await?,
        };
        let new_parent_block = self
            .block(repository.clone(), NodeBlock::index(new_parent))
            .await?;
        self.move_named(
            &repository,
            (node_id, &block),
            (new_parent, &new_parent_block),
            renamed,
            Checks::None,
            own,
        )
        .await
    }

    /// [`Self::node_move`] of `node_id`, in `block`, under `new_parent`, in `new_parent_block`,
    /// both ids valid, with the new name, where it changes, already checked storable and hashed,
    /// and `block` holding its name table then, checked as `checks` says.
    async fn move_named(
        &self,
        repository: &Arc<RepositoryContext>,
        (node_id, block): (NodeID, &Arc<NodeBlock>),
        (new_parent, new_parent_block): (NodeID, &Arc<NodeBlock>),
        renamed: Option<(&str, u64)>,
        checks: Checks,
        own: Mark,
    ) -> Result<NodeID, StateError> {
        debug_assert!(
            !self.is_served(),
            "an unserved move of a served State, which moves under the served chain rule"
        );
        let block_index = NodeBlock::index(node_id);
        let new_parent_block_index = NodeBlock::index(new_parent);
        let check = checks != Checks::None;
        let mut attempts = ChainAttempts::default();
        for _ in 0..self.chain_change_attempts() {
            let old_parent = {
                let node = block.node(Node::index(node_id));
                if node.is_discarded() {
                    return Err(NodeNotFound.into());
                }
                node.parent
            };
            if old_parent == new_parent {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(
                    crate::state::node::hooks::Point::BeforeMoveLock,
                )
                .await;
                if self.rename_in_place(
                    (node_id, block),
                    (new_parent, new_parent_block),
                    renamed,
                    check,
                    own,
                )? {
                    return Ok(old_parent);
                }
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::RenameRetry);
                continue;
            }
            let old_parent_block_index = NodeBlock::index(old_parent);
            let old_parent_block = self
                .block(repository.clone(), old_parent_block_index)
                .await?;
            let (position, _) = self
                .find_predecessor(repository.clone(), old_parent, &old_parent_block, node_id)
                .await?;
            #[cfg(feature = "state-hooks")]
            let arm = match &position {
                ChainPosition::Moving => Some(log::Arm::MoveNamedMoving),
                ChainPosition::Missing => Some(log::Arm::MoveNamedMissing),
                _ => None,
            };
            let Some((predecessor, predecessor_block)) = attempts.found(old_parent, position)?
            else {
                #[cfg(feature = "state-hooks")]
                if let Some(arm) = arm {
                    log::arm(&self.hooks, arm);
                }
                continue;
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::BeforeMoveLock)
                .await;
            #[cfg(feature = "state-hooks")]
            let _label = log::label(log::Section::MoveNamed);
            let mut locks = BlockLocks::lock([
                Some((old_parent_block_index, &old_parent_block)),
                predecessor.locked(predecessor_block.as_ref()),
                Some((block_index, block)),
                Some((new_parent_block_index, new_parent_block)),
            ]);
            let node = match locks.node(node_id) {
                Some(node)
                    if !node.is_discarded()
                        && node.parent == old_parent
                        && predecessor.reaches(&mut locks, old_parent, node_id) =>
                {
                    node
                }
                _ => {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::MoveNamedChanged);
                    continue;
                }
            };
            if check && let Some(refused) = unmovable(&mut locks, &node, old_parent, new_parent) {
                #[cfg(feature = "state-hooks")]
                hook_unmovable(self, &mut locks, &node, node_id, refused);
                return Err(refused.error());
            }
            let mut record = node;
            if let Some((new_name, name_hash)) = renamed {
                (record.name_offset, record.name_length) = locks.store_name(node_id, new_name)?;
                record.name_hash = name_hash;
            }
            #[cfg(feature = "state-hooks")]
            if check {
                hook_movable(self, node_id, (old_parent, new_parent));
            }
            record.parent = new_parent;
            own.apply_to(&mut record);
            predecessor.set(&mut locks, old_parent, node.sibling);
            locks.note_detach(old_parent, node.name_hash, node_id, REATTACH_CHANGES);
            if let Some(held) = locks.node_mut(node_id) {
                *held = record;
            }
            prepend_locked(&mut locks, new_parent, node_id);
            // The name read and copied only where the thread records.
            #[cfg(feature = "state-hooks")]
            if log::recording() {
                let instances = (
                    locks.hook_instance(old_parent),
                    locks.hook_instance(new_parent),
                );
                log::record(
                    &self.hooks,
                    log::Effect::Move {
                        node: node_id,
                        from: old_parent,
                        to: new_parent,
                        name_hash: record.name_hash,
                        name: renamed.map(|(name, _)| log::Name::new(name.as_bytes())),
                        replaced: None,
                        check: log::Check::Unchecked,
                        expected: log::Expected::Nothing,
                        instances,
                    },
                );
            }
            locks.release(self);
            return Ok(old_parent);
        }
        Err(attempts.exhausted(self, repository, node_id).await)
    }

    /// Renames `node_id` where it stands, keeping its place in its directory's chain, its
    /// name as `name` says; `Keep` changes nothing. The name is stored and installed under the
    /// write locks of the node's block and its parent's, whose chain counts a changed name hash
    /// [`REATTACH_CHANGES`], as a find that passed the node under its old name must walk again,
    /// and a respelling, which keeps the hash, nothing: it logs a respelling instead, which a
    /// listing's spelling follows. No mark.
    pub async fn node_rename(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        name: NameChange<'_>,
    ) -> Result<(), StateError> {
        let NameChange::Rename(new_name) = name else {
            return Ok(());
        };
        debug_assert!(
            !self.is_served(),
            "an unserved rename of a served State, which renames under the served chain rule"
        );
        if !node_id.is_valid_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "node id does not name a renamable node".into(),
            }));
        }
        if let Err(error) = validate_node_name_for_store(new_name) {
            return Err(StateError::from(InvalidArguments {
                reason: format!("new name is not storable: {error}"),
            }));
        }
        let block = self
            .block_with_nametable(repository.clone(), NodeBlock::index(node_id))
            .await?;
        let renamed = Some((new_name, hash::hash_string(new_name)));
        for _ in 0..self.chain_change_attempts() {
            let parent = {
                let node = block.node(Node::index(node_id));
                if node.is_discarded() {
                    return Err(NodeNotFound.into());
                }
                node.parent
            };
            let parent_block = self
                .block(repository.clone(), NodeBlock::index(parent))
                .await?;
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::window(crate::state::node::hooks::Window::Rename);
            if self.rename_in_place(
                (node_id, &block),
                (parent, &parent_block),
                renamed,
                false,
                Mark::NONE,
            )? {
                return Ok(());
            }
            #[cfg(feature = "state-hooks")]
            log::arm(&self.hooks, log::Arm::RenameRetry);
        }
        #[cfg(feature = "state-hooks")]
        log::arm(&self.hooks, log::Arm::SlowDown);
        Err(StateError::from(SlowDown))
    }

    /// The rename in place of [`Self::node_rename`] and of [`Self::node_move`] under the same
    /// parent: `node_id`, in `block`, under `parent`, in `parent_block`, takes `renamed` where
    /// given and `own` either way, under both blocks' write locks; refused as [`unmovable`] says
    /// where `check`. Answers false, changing nothing, where the node
    /// is no longer under `parent`.
    fn rename_in_place(
        &self,
        (node_id, block): (NodeID, &Arc<NodeBlock>),
        (parent, parent_block): (NodeID, &Arc<NodeBlock>),
        renamed: Option<(&str, u64)>,
        check: bool,
        own: Mark,
    ) -> Result<bool, StateError> {
        #[cfg(feature = "state-hooks")]
        let _label = log::label(log::Section::RenameInPlace);
        let mut locks = BlockLocks::lock([
            Some((NodeBlock::index(parent), parent_block)),
            Some((NodeBlock::index(node_id), block)),
        ]);
        let node = match locks.node(node_id) {
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
            Some(node) if node.parent == parent => node,
            _ => return Ok(false),
        };
        if check && let Some(refused) = unmovable(&mut locks, &node, parent, parent) {
            #[cfg(feature = "state-hooks")]
            hook_unmovable(self, &mut locks, &node, node_id, refused);
            return Err(refused.error());
        }
        let mut record = node;
        if let Some((new_name, name_hash)) = renamed {
            (record.name_offset, record.name_length) = locks.store_name(node_id, new_name)?;
            record.name_hash = name_hash;
            let respell = node.name_hash == name_hash;
            let weight = if respell { 0 } else { REATTACH_CHANGES };
            locks.note_detach(parent, node.name_hash, node_id, weight);
            locks.note_attach(parent, name_hash, node_id, 0);
            // A fold lookup cannot tell the two patches of a same-hash rename apart.
            if respell {
                locks.note_respell(parent);
            }
        }
        #[cfg(feature = "state-hooks")]
        if check {
            hook_movable(self, node_id, (parent, parent));
        }
        let marked = own.apply_to(&mut record);
        if (renamed.is_some() || marked)
            && let Some(held) = locks.node_mut(node_id)
        {
            *held = record;
        }
        #[cfg(feature = "state-hooks")]
        if let Some((name, _)) = renamed
            && log::recording()
        {
            let instance = locks.hook_instance(parent);
            log::record(
                &self.hooks,
                log::Effect::Rename {
                    node: node_id,
                    parent,
                    name_hash: record.name_hash,
                    name: log::Name::new(name.as_bytes()),
                    instance,
                },
            );
        }
        locks.release(self);
        Ok(true)
    }

    /// Whether `candidate` is `node_id` itself or one of its descendants, so moving
    /// `node_id` onto it would detach the subtree from the tree.
    ///
    /// Walks `candidate`'s ancestors rather than `node_id`'s subtree: an ancestor chain is
    /// one node per level, where the subtree can be the whole tree. The walk is guarded
    /// against a cycle the chain should not contain, so a corrupt state fails here rather
    /// than hangs.
    pub(in crate::state) async fn is_inside_subtree(
        &self,
        repository: Arc<RepositoryContext>,
        candidate: NodeID,
        node_id: NodeID,
    ) -> Result<bool, StateError> {
        let mut ancestor = candidate;
        let mut cycle = SiblingCycleGuard::new(node_id);
        while ancestor.is_valid_node_id() {
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::SubtreeStep).await;
            if ancestor == node_id {
                return Ok(true);
            }
            cycle.observe(ancestor).map_err(StateError::from)?;
            ancestor = self.node(repository.clone(), ancestor).await?.parent;
        }
        Ok(false)
    }

    /// Move `node_id` to `new_name` under `new_parent`, in place, unless a live child
    /// of `new_parent` other than the node already holds the name, which is reported
    /// as [`AttachOutcome::Exists`] with nothing changed.
    ///
    /// The node keeps its id and its subtree. Everything that can fail runs before a chain changes:
    /// the new parent must be a live directory outside the node's subtree, the name must be
    /// storable, and the name lookup must succeed. Both parents are marked `Dirty` as
    /// [`Self::node_detach`] marks one. Then, under the write locks of every block involved, taken
    /// in block-index order, the node is spliced out of its chain, takes its new parent and name,
    /// and is prepended to the new chain in one critical section, so it is never in no chain and
    /// never in two. The old parent's chain change count moves by [`REATTACH_CHANGES`] and the new
    /// parent's by one. Moving a directory to another parent holds the State's moves permit
    /// ([`Self::moves_permit`]) from before the subtree check to after the move, so two such moves
    /// cannot form a cycle. A find by name that misses a name during the move walks again
    /// ([`Self::find_subnode`]). A children walk that meets the node in its old chain after the
    /// move restarts from that chain's head, and one that meets it at the head of its own directory
    /// may visit nodes again, which it reports
    /// ([`StateNodeChildrenWithNameIterator::may_repeat`](crate::state::StateNodeChildrenWithNameIterator::may_repeat));
    /// one that has passed the head may omit the node, as a directory read may omit an entry
    /// renamed during it.
    #[inline]
    pub fn node_reattach<'a>(
        &'a self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        new_parent: NodeID,
        new_name: &'a str,
    ) -> impl Future<Output = Result<AttachOutcome, StateError>> + Send + 'a {
        self.reattach::<Unserved, false>(repository, node_id, new_parent, new_name, None, None)
            .map(move |moved| moved?.outcome(node_id))
    }

    /// [`Self::node_reattach`] onto a name `absent`, where given, shows absent from `new_parent`:
    /// the name is checked against the patches of `new_parent`'s chain log since the absence under
    /// the move's write locks, as [`Self::node_attach_absent`] checks it, in place of a walk of
    /// the chain.
    #[inline]
    pub fn node_reattach_absent<'a>(
        &'a self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        new_parent: NodeID,
        new_name: &'a str,
        absent: Option<&'a Absent>,
    ) -> impl Future<Output = Result<AttachOutcome, StateError>> + Send + 'a {
        self.reattach::<Unserved, false>(repository, node_id, new_parent, new_name, None, absent)
            .map(move |moved| moved?.outcome(node_id))
    }

    /// [`Self::node_reattach`] onto `new_name` where `holder`, a live child of `new_parent`,
    /// holds it: in the critical section that moves the node, `holder` leaves the chain and is
    /// marked `Discarded`, keeping its record, `sibling` and slot as [`Self::node_detach`]
    /// leaves them, so a find by name finds the name held by one of the two at every moment.
    /// The holder's directory's chain change count moves by [`REATTACH_CHANGES`]. A name
    /// held by a node other than `holder` is reported as [`AttachOutcome::Exists`]; a holder
    /// already gone leaves a plain reattach.
    #[inline]
    pub fn node_reattach_replacing<'a>(
        &'a self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        new_parent: NodeID,
        new_name: &'a str,
        holder: NodeID,
    ) -> impl Future<Output = Result<AttachOutcome, StateError>> + Send + 'a {
        self.reattach::<Unserved, false>(
            repository,
            node_id,
            new_parent,
            new_name,
            Some(holder),
            None,
        )
        .map(move |moved| moved?.outcome(node_id))
    }

    /// The move of [`Self::node_reattach`], replacing `replacing` where it holds the name
    /// ([`Self::node_reattach_replacing`]), or onto a name `absent` shows free
    /// ([`Self::node_reattach_absent`]). The unserved tiers mark both parents first; the served
    /// tier marks each parent whose record, as the move read it, fails the served chain rule's
    /// pre-test, and a section the rule refuses under the locks starts the move again. An attempt
    /// after a lookup that met no other node of the name walks only what was prepended since
    /// ([`Self::find_subnode_since`]). With `LOCAL_ONLY`, the served tier moves a directory to
    /// another name only where it holds local content ([`ServedNodes::node_reattach_local`]); a
    /// constant, so no future of another copy holds it.
    pub(in crate::state) async fn reattach<T: Tier, const LOCAL_ONLY: bool>(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        new_parent: NodeID,
        new_name: &str,
        replacing: Option<NodeID>,
        absent: Option<&Absent>,
    ) -> Result<Reattached, StateError> {
        // A recorded run's served State moves by the served tier's rules, as the model judges
        // them (H20); a test of its own may move a served State's node through the unserved one.
        #[cfg(feature = "state-hooks")]
        assert!(
            T::SERVED || !self.is_served() || !log::recording(),
            "an unserved reattach of a served State in a recorded run"
        );
        if !node_id.is_valid_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "node id does not name a reattachable node".into(),
            }));
        }
        if !new_parent.is_valid_or_root_node_id() {
            return Err(StateError::from(InvalidArguments {
                reason: "new parent id does not name a node".into(),
            }));
        }
        if let Err(error) = validate_node_name_for_store(new_name) {
            return Err(StateError::from(InvalidArguments {
                reason: format!("new name is not storable: {error}"),
            }));
        }

        let name_hash = hash::hash_string(new_name);
        let block_index = NodeBlock::index(node_id);
        let block = self
            .block_with_nametable(repository.clone(), block_index)
            .await?;
        let new_parent_block_index = NodeBlock::index(new_parent);
        let new_parent_block = self
            .block(repository.clone(), new_parent_block_index)
            .await?;
        let mut absent = absent
            .and_then(|absent| absent.read_from(self, &new_parent_block, new_parent, name_hash));
        let mut attempts = ChainAttempts::default();
        let mut holder_attempts = ChainAttempts::default();
        let mut since: Option<(Option<NodeID>, u32)> = None;
        // Whether the node counted as added as the section that moved it read it.
        let mut moved_added = false;
        for _ in 0..self.chain_change_attempts() {
            let (old_parent, is_directory) = {
                let node = block.node(Node::index(node_id));
                if node.is_discarded() {
                    return Err(NodeNotFound.into());
                }
                // Refused before any mark; the section checks again under its locks.
                if LOCAL_ONLY && refuses_local_only(&node, new_parent, name_hash) {
                    return Ok(Reattached::NotLocal);
                }
                (node.parent, node.is_directory())
            };
            // Two directories moved into each other's subtrees at once would each pass
            // the subtree check below and together form a cycle cut off from the root.
            let moving_directory = if is_directory && old_parent != new_parent {
                Some(self.moves_permit().await?)
            } else {
                None
            };
            let (new_parent_node, changes) =
                new_parent_block.node_with_chain_changes(Node::index(new_parent));
            check_live_directory(&new_parent_node)?;
            if moving_directory.is_some()
                && self
                    .is_inside_subtree(repository.clone(), new_parent, node_id)
                    .await?
            {
                return Err(StateError::from(InvalidArguments {
                    reason: "new parent is the node itself or one of its descendants".into(),
                }));
            }
            let replaced = if absent.is_some() {
                None
            } else {
                let found = if let Some(since) = since {
                    Box::pin(self.find_subnode_since(
                        repository.clone(),
                        new_parent,
                        &new_parent_block,
                        since,
                        name_hash,
                    ))
                    .await
                } else {
                    self.find_subnode(repository.clone(), new_parent, name_hash)
                        .await
                };
                let replaced = match found {
                    Ok(existing) if existing == node_id => None,
                    Ok(existing) if Some(existing) == replacing => Some(existing),
                    Ok(existing) => return Ok(Reattached::Exists(existing)),
                    Err(error) if error.is_node_not_found() => None,
                    Err(error) => return Err(error),
                };
                since = replaced
                    .is_none()
                    .then_some((new_parent_node.child(), changes));
                replaced
            };
            let holder = match replaced {
                Some(holder) => {
                    let holder_block = self
                        .block(repository.clone(), NodeBlock::index(holder))
                        .await?;
                    let (position, _) = self
                        .find_predecessor(repository.clone(), new_parent, &new_parent_block, holder)
                        .await?;
                    #[cfg(feature = "state-hooks")]
                    let arm = hook_reattach_arm(&position);
                    let Some((holder_predecessor, holder_predecessor_block)) =
                        holder_attempts.found(new_parent, position)?
                    else {
                        #[cfg(feature = "state-hooks")]
                        if let Some(arm) = arm {
                            log::arm(&self.hooks, arm);
                        }
                        continue;
                    };
                    Some((
                        holder,
                        holder_block,
                        holder_predecessor,
                        holder_predecessor_block,
                    ))
                }
                None => None,
            };

            let old_parent_block_index = NodeBlock::index(old_parent);
            let old_parent_block = self
                .block(repository.clone(), old_parent_block_index)
                .await?;
            T::premark(self, &repository, old_parent).await?;
            T::premark(self, &repository, new_parent).await?;
            let (position, old_read) = self
                .find_predecessor(repository.clone(), old_parent, &old_parent_block, node_id)
                .await?;
            #[cfg(feature = "state-hooks")]
            let arm = hook_reattach_arm(&position);
            let Some((predecessor, predecessor_block)) = attempts.found(old_parent, position)?
            else {
                #[cfg(feature = "state-hooks")]
                if let Some(arm) = arm {
                    log::arm(&self.hooks, arm);
                }
                continue;
            };
            if T::SERVED
                && !self
                    .reattach_marks::<T>(
                        &repository,
                        (node_id, &block),
                        (old_parent, &old_parent_block, old_read),
                        (new_parent, &new_parent_node, &new_parent_block),
                    )
                    .await?
            {
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::ReattachMovedMark);
                continue;
            }
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::BeforeMoveLock)
                .await;

            let section = 'section: {
                let (holder_locked, holder_predecessor_locked) = match &holder {
                    Some((holder, holder_block, holder_predecessor, holder_predecessor_block)) => (
                        Some((NodeBlock::index(*holder), holder_block)),
                        holder_predecessor.locked(holder_predecessor_block.as_ref()),
                    ),
                    None => (None, None),
                };
                #[cfg(feature = "state-hooks")]
                let _label = log::label(log::Section::Reattach);
                let mut locks = BlockLocks::lock([
                    Some((old_parent_block_index, &old_parent_block)),
                    predecessor.locked(predecessor_block.as_ref()),
                    Some((block_index, &block)),
                    Some((new_parent_block_index, &new_parent_block)),
                    holder_locked,
                    holder_predecessor_locked,
                ]);
                let holder_in_place =
                    holder
                        .as_ref()
                        .is_none_or(|(holder, _, holder_predecessor, _)| {
                            locks.node(*holder).is_some_and(|record| {
                                !record.is_discarded() && record.parent == new_parent
                            }) && holder_predecessor.reaches(&mut locks, new_parent, *holder)
                        });
                // Under `state-hooks`, whether the log's hold was the mover's own name.
                #[cfg(feature = "state-hooks")]
                let mut own_hold = false;
                let name = match absent {
                    Some(absent) => match logged_name(&mut locks, new_parent, absent) {
                        NameCheck::Held(existing) if existing == node_id => {
                            #[cfg(feature = "state-hooks")]
                            {
                                own_hold = true;
                            }
                            NameCheck::Free
                        }
                        checked => checked,
                    },
                    None if chain_unchanged(&mut locks, new_parent, &new_parent_block, changes) => {
                        NameCheck::Free
                    }
                    None => NameCheck::Changed,
                };
                // The rule only for a name found free, so a name found held here answers without
                // a mark; on the absence path the pre-test may have marked a parent already.
                if T::SERVED && name == NameCheck::Free {
                    let mut refuses = |parent: NodeID, parent_block: &NodeBlock| {
                        locks.with_node(parent, |held| {
                            admits_served_change(parent, held, parent_block)
                        }) != Some(true)
                    };
                    if refuses(old_parent, &old_parent_block)
                        || (new_parent != old_parent && refuses(new_parent, &new_parent_block))
                    {
                        break 'section Section::NeedsMark;
                    }
                }
                match (locks.node(node_id), name) {
                    (_, NameCheck::Held(existing)) => {
                        #[cfg(feature = "state-hooks")]
                        log::record(
                            &self.hooks,
                            log::Effect::Refused {
                                node: node_id,
                                parent: new_parent,
                                reason: log::Reason::Exists,
                                read: log::Read::Holder {
                                    holder: existing,
                                    name_hash,
                                },
                                expected: absent.map_or(log::Expected::Nothing, |absent| {
                                    log::Expected::Log { seq: absent.at() }
                                }),
                            },
                        );
                        return Ok(Reattached::Exists(existing));
                    }
                    (Some(node), NameCheck::Free)
                        if !node.is_discarded()
                            && node.parent == old_parent
                            && holder_in_place
                            && predecessor.reaches(&mut locks, old_parent, node_id) =>
                    {
                        // A clear drops `LocalContent` under this block's lock, so this check
                        // decides; it refuses only where a clear landed since the first one.
                        if LOCAL_ONLY && refuses_local_only(&node, new_parent, name_hash) {
                            #[cfg(feature = "state-hooks")]
                            {
                                log::record(
                                    &self.hooks,
                                    log::Effect::Refused {
                                        node: node_id,
                                        parent: node_id,
                                        reason: log::Reason::NotLocal,
                                        read: log::Read::Word(log::Words(node)),
                                        // Where it moves to, which a respell's is not.
                                        expected: log::Expected::At {
                                            parent: new_parent,
                                            name_hash,
                                        },
                                    },
                                );
                                log::arm(&self.hooks, log::Arm::NotLocalLocked);
                            }
                            return Ok(Reattached::NotLocal);
                        }
                        let respell = old_parent == new_parent && node.name_hash == name_hash;
                        let (name_offset, name_length) = locks.store_name(node_id, new_name)?;
                        #[cfg(feature = "state-hooks")]
                        if LOCAL_ONLY {
                            log::record(
                                &self.hooks,
                                log::Effect::Admitted {
                                    node: node_id,
                                    expected: log::Expected::Local {
                                        to: new_parent,
                                        name_hash,
                                    },
                                },
                            );
                        }
                        let mut predecessor = predecessor;
                        let mut replaced = None;
                        // Whether the holder carries a dirty add action: one that carries none
                        // stands at a path the revision holds.
                        let mut holder_added = true;
                        if let Some((holder, _, holder_predecessor, _)) = &holder {
                            let holder_record = locks.node(*holder);
                            holder_added = holder_record.is_none_or(|record| record.is_dirty_add());
                            let holder_sibling =
                                holder_record.map_or(INVALID_NODE, |record| record.sibling);
                            holder_predecessor.set(&mut locks, new_parent, holder_sibling);
                            locks.flag_detached(*holder);
                            replaced = holder_record.map(|record| (*holder, record.name_hash));
                            // A node that followed the holder now follows the holder's predecessor.
                            if let Predecessor::Node(previous) = predecessor
                                && previous == *holder
                            {
                                predecessor = *holder_predecessor;
                            }
                        }
                        let sibling = locks
                            .node(node_id)
                            .map_or(node.sibling, |node| node.sibling);
                        predecessor.set(&mut locks, old_parent, sibling);
                        locks.note_detach(old_parent, node.name_hash, node_id, REATTACH_CHANGES);
                        // Logged after the mover's detach, so the holder's detach and the
                        // mover's attach sit at adjacent sequences in the holder's directory.
                        if let Some((holder, holder_hash)) = replaced {
                            let weight = if new_parent != old_parent {
                                REATTACH_CHANGES
                            } else {
                                0
                            };
                            locks.note_detach(new_parent, holder_hash, holder, weight);
                        }
                        if respell {
                            locks.note_respell(new_parent);
                        }
                        moved_added = node.is_dirty_add();
                        // A served move of a directory, a respell included, dirties it and drops
                        // its pending clear under this lock, so a clear either reads it where it
                        // went or keeps it dirty, and a clean directory moved to a path its
                        // revision may not hold stays dirty. A file keeps its flags: a plain
                        // `Dirty` on a clean file would read as a change to its content.
                        let moved_dir = T::SERVED && node.is_directory();
                        if let Some(record) = locks.node_mut(node_id) {
                            record.parent = new_parent;
                            record.name_offset = name_offset;
                            record.name_length = name_length;
                            record.name_hash = name_hash;
                            if moved_dir && !record.is_dirty() {
                                record.flags &= !NodeFlags::DirtyBits;
                                record.flags |= NodeFlags::Dirty;
                            }
                            // A served move of an added node over one the revision holds takes
                            // that path over: the mover no longer counts as added, so its later
                            // removal records the path deleted (H20).
                            if T::SERVED && !holder_added && record.is_dirty_add() {
                                record.flags &= !NodeFlags::ActionBits.bits();
                                if record.is_file() {
                                    record.flags |= NodeFlags::DirtyModify.bits();
                                }
                            }
                        }
                        if moved_dir {
                            locks.drop_clear_pending(node_id);
                        }
                        prepend_locked(&mut locks, new_parent, node_id);
                        #[cfg(feature = "state-hooks")]
                        if log::recording() {
                            let instances = (
                                locks.hook_instance(old_parent),
                                locks.hook_instance(new_parent),
                            );
                            let (check, expected) = match (absent, &holder) {
                                (Some(absent), _) => (
                                    if own_hold {
                                        log::Check::Logged
                                    } else {
                                        log::Check::Absent
                                    },
                                    log::Expected::Log { seq: absent.at() },
                                ),
                                (None, holder) => (
                                    if holder.is_some() {
                                        log::Check::Replacing
                                    } else {
                                        log::Check::Walk
                                    },
                                    log::Expected::Count {
                                        changes,
                                        instance: new_parent_block.hook_instance(),
                                    },
                                ),
                            };
                            log::record(
                                &self.hooks,
                                log::Effect::Move {
                                    node: node_id,
                                    from: old_parent,
                                    to: new_parent,
                                    name_hash,
                                    name: Some(log::Name::new(new_name.as_bytes())),
                                    replaced: replaced.map(|(holder, _)| holder),
                                    check,
                                    expected,
                                    instances,
                                },
                            );
                        }
                        locks.release(self);
                        Section::Done
                    }
                    (_, NameCheck::Uncovered) => {
                        absent = None;
                        Section::Changed
                    }
                    _ => Section::Changed,
                }
            };
            match section {
                Section::Done => return Ok(Reattached::Moved { added: moved_added }),
                Section::NeedsMark => {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::ReattachNeedsMark);
                    self.served_refused();
                }
                Section::Changed => {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::ReattachChanged);
                }
            }
        }
        Err(attempts.exhausted(self, &repository, node_id).await)
    }

    /// A served move's pre-test marks: the old parent where its record as the move read it,
    /// `old_read`, fails the served chain rule's pre-test, then the new parent likewise. One
    /// future, awaited once, so what the move holds across the marks is stored once in its
    /// future. `Ok(false)` where the node left the old parent while it was marked, which then
    /// turned out detached: the move starts again from where the node went.
    async fn reattach_marks<T: Tier>(
        &self,
        repository: &Arc<RepositoryContext>,
        (node_id, block): (NodeID, &NodeBlock),
        (old_parent, old_parent_block, old_read): (NodeID, &Arc<NodeBlock>, ParentRead),
        (new_parent, new_parent_node, new_parent_block): (NodeID, &Node, &Arc<NodeBlock>),
    ) -> Result<bool, StateError> {
        if !old_read.admits(old_parent, old_parent_block)
            && let Err(error) = T::served_mark(
                self,
                repository,
                old_parent,
                (old_parent_block, old_read.parent()),
            )
            .await
        {
            if !error.is_node_not_found() || block.node(Node::index(node_id)).parent == old_parent {
                return Err(error);
            }
            return Ok(false);
        }
        if new_parent != old_parent
            && !admits_served_change(new_parent, new_parent_node, new_parent_block)
        {
            T::served_mark(
                self,
                repository,
                new_parent,
                (new_parent_block, new_parent_node.parent),
            )
            .await?;
        }
        Ok(true)
    }

    /// Record a move on every node under `node_id`.
    ///
    /// Each node's flag pair is decided from its own staging state rather than inherited,
    /// so a node this handle added stays an addition while a node the revision holds
    /// becomes a move. A node staged for deletion is left as it is and not descended
    /// into: the action bits hold one change at a time, so recording a move over it would
    /// take the deletion off it, and its whole subtree is staged for deletion with it.
    ///
    /// Descent stops at a link too, whose children belong to the linked repository's tree
    /// and not to this one.
    ///
    /// Walks with [`StateNodeChildrenIterator`], which carries each child's record with
    /// its id and holds one block across the siblings that share it — so the walk costs
    /// one read per node and allocates nothing per directory. The pending list holds ids
    /// rather than records, since it can grow to the directory count of the subtree.
    ///
    /// A subtree holding `node_id` itself is a cycle a move into its own subtree made, which the
    /// move's checks exclude: answered as an internal error rather than walked for good under the
    /// moves permit. The error reports a tree the move already broke, its node below itself and
    /// cut off from the root, not a move refused.
    pub(in crate::state) async fn mark_subtree_moved(
        self: &Arc<Self>,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
    ) -> Result<(), StateError> {
        let mut pending = vec![node_id];
        while let Some(parent_id) = pending.pop() {
            let mut children =
                StateNodeChildrenIterator::new(self.clone(), repository.clone(), parent_id).await?;
            while let Some((child_id, child)) = children.next().await? {
                if child_id == node_id {
                    return Err(StateError::internal(format!(
                        "directory {node_id} lies below itself"
                    )));
                }
                if child.is_staged_delete() || child.is_discarded() {
                    continue;
                }
                let (staged, dirty) = Self::staged_move_flags(&child);
                let mark = Mark {
                    staged: Some((staged, false)),
                    dirty: Some((dirty, Ext::Set(NodeExtFlags::NoFlags), false)),
                };
                self.node_mark(&repository, child_id, mark).await?;
                if child.is_directory() {
                    pending.push(child_id);
                }
            }
        }
        Ok(())
    }
}

/// Under `state-hooks`, the arm a reattach's walk to its node took where it found none.
#[cfg(feature = "state-hooks")]
fn hook_reattach_arm(position: &ChainPosition) -> Option<log::Arm> {
    match position {
        ChainPosition::Moving => Some(log::Arm::ReattachMoving),
        ChainPosition::Missing => Some(log::Arm::ReattachMissing),
        _ => None,
    }
}

/// Under `state-hooks`, the log's record of the refusal [`unmovable`] decided, on the word it
/// read: the node's own, or the parent's, where the locks hold it.
#[cfg(feature = "state-hooks")]
fn hook_unmovable(
    state: &State,
    locks: &mut BlockLocks<'_>,
    node: &Node,
    node_id: NodeID,
    refused: Unmovable,
) {
    if !log::recording() {
        return;
    }
    let read = match refused {
        Unmovable::StagedDelete => Some((node_id, *node)),
        Unmovable::Parent(parent) => locks.node(parent).map(|held| (parent, held)),
    };
    if let Some((parent, read)) = read {
        log::record(
            &state.hooks,
            log::Effect::Refused {
                node: node_id,
                parent,
                reason: log::Reason::Unmovable,
                read: log::Read::Word(log::Words(read)),
                expected: log::Expected::Nothing,
            },
        );
    }
}

/// Under `state-hooks`, the log's record of a checked move of `node_id` from and to `parents` that
/// [`unmovable`] admitted.
#[cfg(feature = "state-hooks")]
fn hook_movable(state: &State, node_id: NodeID, (from, to): (NodeID, NodeID)) {
    log::record(
        &state.hooks,
        log::Effect::Admitted {
            node: node_id,
            expected: log::Expected::Movable { from, to },
        },
    );
}

/// Whether [`ServedNodes::node_reattach_local`] refuses to move `node`, as read, to the name
/// hashed `name_hash` under `new_parent`: a directory holding no local content, to another place.
fn refuses_local_only(node: &Node, new_parent: NodeID, name_hash: u64) -> bool {
    let respell = node.parent == new_parent && node.name_hash == name_hash;
    node.is_directory() && !respell && !node.ext_flags().contains(NodeExtFlags::LocalContent)
}

/// What [`State::node_move_staged`]'s section refuses a move for, under the locks.
#[derive(Clone, Copy)]
enum Unmovable {
    /// The node staged for deletion.
    StagedDelete,
    /// A parent it leaves or joins Discarded or staged for deletion.
    Parent(NodeID),
}

impl Unmovable {
    /// The error the C API answers, as it refuses the move when it plans the call.
    #[cold]
    fn error(self) -> StateError {
        let reason = match self {
            Self::StagedDelete => "cannot move a node staged for deletion".into(),
            Self::Parent(parent) => format!("directory {parent} is deleted or staged for deletion"),
        };
        StateError::from(InvalidArguments { reason })
    }
}

/// What [`State::node_move_staged`]'s section refuses a move of `node`, as read under the locks,
/// for, if anything: a
/// node staged for deletion, or a parent it leaves or joins that is Discarded or staged for
/// deletion.
fn unmovable(
    locks: &mut BlockLocks<'_>,
    node: &Node,
    old_parent: NodeID,
    new_parent: NodeID,
) -> Option<Unmovable> {
    if node.is_staged_delete() {
        return Some(Unmovable::StagedDelete);
    }
    let parents = if old_parent == new_parent {
        &[old_parent][..]
    } else {
        &[old_parent, new_parent][..]
    };
    parents.iter().copied().find_map(|parent| {
        let gone = locks.with_node(parent, |held| {
            held.is_discarded() || held.is_staged_delete()
        });
        (gone != Some(false)).then_some(Unmovable::Parent(parent))
    })
}
