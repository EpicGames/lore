// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Adds and checked attaches of a node to a directory's chain.

use core::str;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use super::Served;
use super::ServedNodes;
use super::SwapNodes;
use super::Tier;
use super::Unserved;
use super::locks::BlockLocks;
use super::locks::admits_served_change;
use super::locks::chain_unchanged;
use super::locks::check_live_directory;
use super::locks::publish_locked;
use super::mark::UnfinishedChange;
use super::mark::holds_base_marks;
use crate::errors::InvalidArguments;
use crate::errors::NodeNotFound;
use crate::errors::SlowDown;
use crate::hash;
use crate::lore_trace;
use crate::lore_warn;
use crate::node;
use crate::node::Newest;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeBlockWriter;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::PatchKind;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::state::Mark;
use crate::state::State;
use crate::state::StateError;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;

/// Outcome of [`State::node_attach`] and [`State::node_reattach`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    /// The node is attached under this id.
    Attached(NodeID),
    /// A live sibling already held the name; this is its id.
    Exists(NodeID),
}

/// A name no child of a directory held when the directory's chain log stood at `at`, read from
/// `block`, the directory's block, as the VFS provider's listing looks a name up
/// (`fs::fuse::listing::Listing::lookup_to_attach`). An attach or a reattach handed one
/// checks the patches from `at` on under its write locks in place of finding the name by walking
/// the chain. It records the State's reclaim count: once a sweep freed a slot, a patch since `at`
/// may name a node freed and taken again, so the proof speaks of nothing (C15).
#[derive(Clone)]
pub struct Absent {
    at: u32,
    block: Arc<NodeBlock>,
    dir: NodeID,
    name_hash: u64,
    reclaims: u32,
}

impl Absent {
    /// The caller has shown that no child of `dir` held `name_hash` with the log of `block`,
    /// `dir`'s block in `state`, at `at`, as `state` stands now: the VFS provider's listing, or a
    /// test or the stress suite standing in for one.
    #[lore_macro::test_pub]
    pub(crate) fn new(
        state: &State,
        at: u32,
        block: Arc<NodeBlock>,
        dir: NodeID,
        name_hash: u64,
    ) -> Self {
        Self {
            at,
            block,
            dir,
            name_hash,
            reclaims: state.reclaims(),
        }
    }

    /// The log position the absence holds at.
    #[cfg(any(feature = "test-util", feature = "state-hooks"))]
    pub fn at(&self) -> u32 {
        self.at
    }

    /// This absence where it speaks of `name_hash` in `dir`, read from `block`, `dir`'s block in
    /// `state`: one read from a block evicted and loaded again speaks of another log, and one
    /// taken before a sweep freed a slot of nothing.
    pub(in crate::state) fn read_from(
        &self,
        state: &State,
        block: &Arc<NodeBlock>,
        dir: NodeID,
        name_hash: u64,
    ) -> Option<&Self> {
        (Arc::ptr_eq(&self.block, block)
            && self.dir == dir
            && self.name_hash == name_hash
            && self.reclaims == state.reclaims())
        .then_some(self)
    }
}

impl std::fmt::Debug for Absent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Absent(dir {}, hash {:x}, at {})",
            self.dir, self.name_hash, self.at
        )
    }
}

/// What a namespace change knows of its name in its target directory under its write locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::state) enum NameCheck {
    /// Nothing holds the name: the change goes ahead.
    Free,
    /// The directory changed or died since the name was looked up: the change looks again.
    Changed,
    /// This node holds the name.
    Held(NodeID),
    /// The directory's log no longer holds every patch since the absence: the change finds the
    /// name by walking the chain.
    Uncovered,
    /// The served chain rule refused the change: the directory is not dirty, or has a clear
    /// pending. The change marks it and tries again.
    NeedsMark,
}

/// What directory `parent`'s log, its block locked in `locks`, says of the absent name since
/// `absent`: held where the newest patch for its hash attaches a node, free where none does.
pub(in crate::state) fn logged_name(
    locks: &mut BlockLocks<'_>,
    parent: NodeID,
    absent: &Absent,
) -> NameCheck {
    logged_since(locks, parent, (absent.at, absent.name_hash))
}

/// [`logged_name`] of the name hashing to `name_hash`, absent from `parent` when its log stood at
/// `at`, read from the block locked in `locks`.
fn logged_since(
    locks: &mut BlockLocks<'_>,
    parent: NodeID,
    (at, name_hash): (u32, u64),
) -> NameCheck {
    let index = Node::index(parent);
    let Some(held) = locks.entry(parent) else {
        return NameCheck::Changed;
    };
    let directory = held.writer.node(index);
    if directory.is_discarded() || !directory.is_directory() {
        return NameCheck::Changed;
    }
    let Some(log) = held.writer.chain_log_mut(index) else {
        return NameCheck::Uncovered;
    };
    match log.newest(name_hash, at) {
        Newest::Uncovered => NameCheck::Uncovered,
        Newest::Patch(patch) if patch.kind == PatchKind::Attached => NameCheck::Held(patch.node),
        Newest::Patch(_) | Newest::Nothing => NameCheck::Free,
    }
}

/// What a publish checks under the write locks before it prepends.
#[derive(Clone, Copy)]
pub(in crate::state) enum Publish<'a> {
    /// The parent is live.
    Live,
    /// The parent is live and not staged for deletion: a C API add's
    /// ([`State::node_add_staged`]).
    Unstaged,
    /// The parent's chain change count still reads this, so the name lookup that read it holds.
    Unchanged(u32),
    /// The parent's log since the absence holds no attach of the name.
    Absent(&'a Absent),
    /// The parent's log since this position, read from the block the publish locks, holds no
    /// attach of the name hashing to this.
    Logged(u32, u64),
}

/// How a served attach finds out whether its name is free.
#[derive(Clone, Copy, Debug)]
pub enum AttachCheck<'a> {
    /// By walking the directory's chain.
    Walk,
    /// Against the directory's chain log since this absence, walking only where the log no longer
    /// covers it ([`State::node_attach_absent`]).
    Absent(&'a Absent),
    /// As [`Self::Absent`] where given, else [`Self::Walk`], reading nothing from the store: the
    /// attach answers `None`, attaching nothing, where what it needs is not in memory
    /// ([`State::node_attach_resident`]). Directories its served mark reached before stay
    /// marked, as a cancelled mark leaves them.
    Resident(Option<&'a Absent>),
}

impl<'s> ServedNodes<'s> {
    /// Adds `node` as a new child of directory `parent`, named `name`, unless a live child holds
    /// the name, as [`State::node_attach`] does, under the served chain rule: the publish goes
    /// ahead only where `parent` is live and dirty with no clear pending, marking it first
    /// ([`Self::node_mark`]) where it is not. `None` only for [`AttachCheck::Resident`].
    pub fn node_attach<'a>(
        self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &'a str,
        check: AttachCheck<'a>,
    ) -> impl Future<Output = Result<Option<AttachOutcome>, StateError>> + Send + 'a
    where
        's: 'a,
    {
        let (resident_only, absent) = match check {
            AttachCheck::Walk => (false, None),
            AttachCheck::Absent(absent) => (false, Some(absent)),
            AttachCheck::Resident(absent) => (true, absent),
        };
        self.state
            .attach_child::<Served>(repository, parent, node, name, resident_only, absent)
    }
}

/// What [`SwapNodes::node_attach_tree`] found at its name.
pub enum TreeAttach<'s> {
    /// A live child of the directory holds the name: this one. Nothing was built.
    Exists(NodeID),
    /// The build, its root taken and in no chain.
    Building(TreeBuild<'s>),
}

/// A node of a [`TreeBuild`], which the build adds into where it is a directory. It names its
/// build, so any other refuses it, one whose root took the slot of an abandoned build's included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Built {
    node: NodeID,
    build: u32,
}

/// The builds begun, which numbers each: a number comes back only after 2^32 builds, which no
/// build outlives.
static BUILDS: AtomicU32 = AtomicU32::new(0);

/// A copy of a subtree that a swap builds out of every chain and publishes whole
/// ([`SwapNodes::node_attach_tree`]), so that no request reaches it, or adds to it, before it
/// holds every node (H14, H22). Its root stays `Discarded`, in no chain, until
/// [`Self::publish`]; the nodes below it go into the chains of the build's own directories, which
/// nothing else reaches. A build dropped unpublished, as a cancelled swap drops it, leaves its
/// slots to the next sweep: the root `Discarded`, in no chain, and the nodes below hanging from it
/// (C12, C15).
pub struct TreeBuild<'s> {
    state: &'s State,
    repository: Arc<RepositoryContext>,
    parent: NodeID,
    /// The parent's block, held so that the count and the log position read from it stay
    /// comparable.
    parent_block: Arc<NodeBlock>,
    /// The parent's chain change count, read with the lookup that found the name free.
    changes: u32,
    /// The parent's chain log position, read with that count, where the parent has a log.
    logged: Option<u32>,
    root: NodeID,
    root_block: Arc<NodeBlock>,
    /// The index of the block `into_block` holds.
    into_index: u32,
    /// The block of the directory last added into, where it is not the root's.
    into_block: Option<Arc<NodeBlock>>,
    name_hash: u64,
    /// This build's number, which its nodes' [`Built`] carry.
    build: u32,
}

impl<'s> SwapNodes<'s> {
    /// Begins a copy, from the swap's target, of a subtree whose root `root` goes below directory
    /// `parent` as `name`. The name is looked up first: a live child holding it answers
    /// [`TreeAttach::Exists`] with nothing built. Otherwise the root's slot is taken and
    /// initialized, `Discarded` and in no chain, for [`TreeBuild::add`] to fill and
    /// [`TreeBuild::publish`] to publish. The swap's copies are clean, which is what exempts them
    /// from the served chain rule, so the publish needs only a live parent. A detached parent is
    /// [`NodeNotFound`], and a root that is not a clean copy [`InvalidArguments`]. The root's name
    /// hash is the caller's, as a copy carries its source's, checked under `state-hooks`.
    ///
    /// [`NodeNotFound`]: crate::errors::NodeNotFound
    pub async fn node_attach_tree(
        self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        root: &Node,
        name: &str,
    ) -> Result<TreeAttach<'s>, StateError> {
        check_clean_copy(root)?;
        let state = self.state;
        state.tree(repository.clone()).await?;
        let parent_block = state
            .block(repository.clone(), NodeBlock::index(parent))
            .await?;
        // The parent's record in a block of its own, so no later await holds it.
        let (changes, logged) = {
            let (parent_node, changes, logged) =
                parent_block.node_with_chain_log(Node::index(parent));
            check_live_directory(&parent_node)?;
            match state
                .find_subnode(repository.clone(), parent, root.name_hash)
                .await
            {
                Ok(existing) => return Ok(TreeAttach::Exists(existing)),
                Err(error) if error.is_node_not_found() => {}
                Err(error) => return Err(error),
            }
            (changes, logged)
        };
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Lookup).await;
        let Some((root_id, root_block)) = state
            .prepare_child(repository.clone(), parent, root, name, false)
            .await?
        else {
            return Err(StateError::internal(
                "a slot grab that may load found nothing to load",
            ));
        };
        Ok(TreeAttach::Building(TreeBuild {
            state,
            repository,
            parent,
            parent_block,
            changes,
            logged,
            root: root_id,
            root_block,
            into_index: 0,
            into_block: None,
            name_hash: root.name_hash,
            build: BUILDS.fetch_add(1, Ordering::Relaxed),
        }))
    }
}

impl<'s> TreeBuild<'s> {
    /// The build's root, the first directory it adds into.
    pub fn root(&self) -> Built {
        Built {
            node: self.root,
            build: self.build,
        }
    }

    /// Adds `node`, named `name`, at the head of the chain of `into`, a directory of this build,
    /// and answers it. No name is checked, as a copy's names come from one directory of the
    /// target, and no rule applies, as no request reaches the build. A node of the build that is
    /// not a directory takes nothing, and the add is refused, as is a node of another build and a
    /// node that is not a clean copy. The node's name hash is the caller's, checked under
    /// `state-hooks`.
    pub async fn add(&mut self, into: Built, node: &Node, name: &str) -> Result<Built, StateError> {
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::SwapFill).await;
        check_clean_copy(node)?;
        if into.build != self.build {
            return Err(StateError::from(InvalidArguments {
                reason: format!("node {} is of another build", into.node),
            }));
        }
        let (state, into) = (self.state, into.node);
        // The build's directories mostly share the root's block, which the build holds, and a
        // copy adds a directory's children one after another, so the last one's block is kept.
        if NodeBlock::index(into) != NodeBlock::index(self.root)
            && (self.into_block.is_none() || self.into_index as usize != NodeBlock::index(into))
        {
            let block = state
                .block(self.repository.clone(), NodeBlock::index(into))
                .await?;
            (self.into_index, self.into_block) = (NodeBlock::index(into) as u32, Some(block));
        }
        // The steps of `prepare_child`, as `State::node_add` awaits them: the slot's file
        // metadata block is cleared but not held, as an add that may load needs no resident one.
        let Some(node_id) = state.grab_node_slot(self.repository.clone(), false).await? else {
            return Err(StateError::internal(
                "a slot grab that may load found nothing to load",
            ));
        };
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Grabbed).await;
        let metadata_block = match state
            .try_block_file_metadata_existing(
                self.repository.clone(),
                NodeFileMetadataBlock::index(node::node_to_file_metadata(node_id)),
            )
            .await
        {
            Ok(metadata_block) => metadata_block,
            Err(error) => {
                return Err(state
                    .release_on_error(self.repository.clone(), node_id, error)
                    .await);
            }
        };
        let block = match state
            .initialize_node(self.repository.clone(), node_id, into, node, name)
            .await
        {
            Ok(block) => block,
            Err(error) => {
                return Err(state
                    .release_on_error(self.repository.clone(), node_id, error)
                    .await);
            }
        };
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Initialized).await;
        state.clear_recycled_metadata(metadata_block, node_id);
        let into_block = match &self.into_block {
            Some(block) if self.into_index as usize == NodeBlock::index(into) => block,
            _ => &self.root_block,
        };
        if !publish_private(state, (into, into_block), (node_id, &block)) {
            let refused = StateError::from(InvalidArguments {
                reason: format!("node {into} of a build is not a directory"),
            });
            return Err(state
                .release_on_error(self.repository.clone(), node_id, refused)
                .await);
        }
        Ok(Built {
            node: node_id,
            build: self.build,
        })
    }

    /// Publishes the build's root at the head of the parent's chain, checked against what the
    /// lookup that found the name free read with the parent: where the parent has a chain log, the
    /// log's patches since, as a served create checks its name (`Publish::Absent`), so changes
    /// of other names during the fill cost nothing; otherwise the parent's chain change count. A
    /// check that cannot answer, the count moved or the log no longer covering the lookup, looks
    /// the name up again and publishes again, at most `CHAIN_CHANGE_ATTEMPTS` times, then
    /// [`SlowDown`]. A name found held answers [`AttachOutcome::Exists`], and a parent detached
    /// meanwhile [`NodeNotFound`]: either frees the whole build, as any error does.
    ///
    /// Not an `async fn`, which would hold a second copy of the build.
    ///
    /// [`NodeNotFound`]: crate::errors::NodeNotFound
    #[allow(clippy::manual_async_fn)]
    pub fn publish(
        mut self,
    ) -> impl Future<Output = Result<AttachOutcome, StateError>> + Send + 's {
        async move {
            let mut attempts = 0;
            let outcome = loop {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::BeforePublish)
                    .await;
                // The swap's tier makes no served mark, as the unserved one does not, whose copy of
                // the publish it shares.
                // Under `state-hooks`, a build's publish: its `Unchanged` check meeting a chain log
                // under the locks records the arm only a build's takes.
                #[cfg(feature = "state-hooks")]
                let building = log::build();
                let published = self.state.publish_child::<Unserved>(
                    self.parent,
                    &self.parent_block,
                    self.root,
                    &self.root_block,
                    match self.logged {
                        Some(at) => Publish::Logged(at, self.name_hash),
                        None => Publish::Unchanged(self.changes),
                    },
                );
                #[cfg(feature = "state-hooks")]
                drop(building);
                match published {
                    NameCheck::Free => return Ok(AttachOutcome::Attached(self.root)),
                    NameCheck::Held(existing) => break Ok(AttachOutcome::Exists(existing)),
                    NameCheck::Changed | NameCheck::Uncovered | NameCheck::NeedsMark => {
                        #[cfg(feature = "state-hooks")]
                        log::arm(&self.state.hooks, log::Arm::BuildPublishRetry);
                    }
                }
                attempts += 1;
                if attempts == self.state.chain_change_attempts() {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.state.hooks, log::Arm::SlowDown);
                    break Err(StateError::from(SlowDown));
                }
                self.state.attach_retries.fetch_add(1, Ordering::Relaxed);
                let (parent_node, changes, logged) = self
                    .parent_block
                    .node_with_chain_log(Node::index(self.parent));
                if let Err(error) = check_live_directory(&parent_node) {
                    break Err(error);
                }
                match self
                    .state
                    .find_subnode(self.repository.clone(), self.parent, self.name_hash)
                    .await
                {
                    Ok(existing) => break Ok(AttachOutcome::Exists(existing)),
                    Err(error) if error.is_node_not_found() => {
                        (self.changes, self.logged) = (changes, logged);
                    }
                    Err(error) => break Err(error),
                }
            };
            Box::pin(self.abandon()).await;
            outcome
        }
    }

    /// Frees every node of the build: none was ever published, so no chain any request reaches
    /// or any other change holds is touched (C3). Every node is first cut loose, `Discarded` with
    /// its `child` and `sibling` cleared, with no await between, so an abandon dropped while it
    /// frees leaves detached slots whose chains reach none it freed, which a later grab may reuse
    /// (C1). Where a block of the build cannot be read, nothing is freed: the build is left whole
    /// for the next sweep, as one dropped unpublished is (C12, C15). Not an `async fn`, which would
    /// hold a second copy of the build.
    #[allow(clippy::manual_async_fn)]
    pub fn abandon(self) -> impl Future<Output = ()> + Send + 's {
        async move {
            let (state, repository) = (self.state, &self.repository);
            let mut built = vec![(self.root, self.root_block.clone())];
            let mut at = 0;
            while let Some((dir, block)) = built.get(at) {
                at += 1;
                // The record in a block of its own, so no await holds it.
                let mut next = {
                    let record = block.node(Node::index(*dir));
                    if !record.is_directory() {
                        continue;
                    }
                    record.child
                };
                while next.is_valid_node_id() {
                    let Ok(block) = state
                        .block(repository.clone(), NodeBlock::index(next))
                        .await
                    else {
                        lore_warn!("A swap's unpublished copy at node {next} could not be freed");
                        return;
                    };
                    let sibling = block.node(Node::index(next)).sibling;
                    built.push((next, block));
                    next = sibling;
                }
            }
            #[cfg(feature = "state-hooks")]
            let label = log::label(log::Section::Abandon);
            for (node, block) in &built {
                let dirtied = {
                    let mut writer = block.write();
                    #[cfg(feature = "state-hooks")]
                    if *node == self.root {
                        log::record(&state.hooks, log::Effect::Abandon { root: *node });
                    }
                    let record = writer.node(Node::index(*node));
                    record.flags |= NodeFlags::Discarded.bits();
                    record.child = 0;
                    record.sibling = 0;
                    // Each node is a slot of its own now, freed below or, where the abandon is
                    // dropped first, by the next sweep: its block was flagged at the slot's grab,
                    // and no sweep runs while a build is held (C15).
                    writer.mark_dirty()
                };
                if dirtied {
                    state.block_modified(block.clone(), NodeBlock::index(*node));
                }
            }
            #[cfg(feature = "state-hooks")]
            drop(label);
            for (node, _) in built {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::Abandon).await;
                state.release_node(repository.clone(), node).await;
            }
        }
    }
}

/// Puts `node_id`, in flight, at the head of the chain of `into`, a directory of a build no request
/// reaches, and clears its `Discarded` (C2), under the write locks of both blocks, taken in
/// ascending index, one where they share a block; answers false, writing nothing, where `into` is
/// not a directory. Nothing reads a build's chain counts or logs before it is published, so the
/// change is neither counted nor logged. A block is registered, and the State marked, only where
/// the write turned it dirty: one already dirty was, when it turned dirty (C14).
fn publish_private(
    state: &State,
    (into, into_block): (NodeID, &Arc<NodeBlock>),
    (node_id, block): (NodeID, &Arc<NodeBlock>),
) -> bool {
    fn prepend(
        (into, into_writer): (NodeID, &mut NodeBlockWriter<'_>),
        (node_id, node_writer): (NodeID, Option<&mut NodeBlockWriter<'_>>),
    ) -> bool {
        // No listing reaches a build's directory, and a slot's chain log, kept for the block's
        // life, holds no patch once its slot is freed, so none logs its chain.
        #[cfg(feature = "state-hooks")]
        assert!(
            into_writer
                .chain_log_mut(Node::index(into))
                .is_none_or(|log| log.holds_no_patch()),
            "a build's directory logs its chain"
        );
        let parent = into_writer.node(Node::index(into));
        if !parent.is_directory() {
            return false;
        }
        let head = std::mem::replace(&mut parent.child, node_id);
        let node = match node_writer {
            Some(node_writer) => node_writer.node(Node::index(node_id)),
            None => into_writer.node(Node::index(node_id)),
        };
        node.sibling = head;
        node.flags &= !NodeFlags::Discarded.bits();
        true
    }
    let (into_index, index) = (NodeBlock::index(into), NodeBlock::index(node_id));
    #[cfg(feature = "state-hooks")]
    let _label = log::label(log::Section::BuildAdd);
    let (prepended, dirtied) = if into_index == index {
        let mut writer = block.write();
        let prepended = prepend((into, &mut writer), (node_id, None));
        #[cfg(feature = "state-hooks")]
        if prepended {
            hook_build_add(state, into, node_id, &mut writer);
        } else {
            hook_not_directory(state, into, node_id, &mut writer);
        }
        (prepended, [false, prepended && writer.mark_dirty()])
    } else {
        // Declared before the writers, so it closes after they release.
        #[cfg(feature = "state-hooks")]
        let _section;
        #[cfg(feature = "state-hooks")]
        let multi = log::multi();
        let (mut into_writer, mut node_writer) = if into_index < index {
            let into_writer = into_block.write();
            (into_writer, block.write())
        } else {
            #[cfg(feature = "state-hooks")]
            log::arm(&state.hooks, log::Arm::PublishPrivateAbove);
            let node_writer = block.write();
            (into_block.write(), node_writer)
        };
        #[cfg(feature = "state-hooks")]
        {
            _section = multi.opened(Some(&state.hooks));
        }
        let prepended = prepend((into, &mut into_writer), (node_id, Some(&mut node_writer)));
        #[cfg(feature = "state-hooks")]
        if prepended {
            hook_build_add(state, into, node_id, &mut node_writer);
        } else {
            hook_not_directory(state, into, node_id, &mut into_writer);
        }
        (
            prepended,
            [
                prepended && into_writer.mark_dirty(),
                prepended && node_writer.mark_dirty(),
            ],
        )
    };
    if dirtied[0] {
        state.block_modified(into_block.clone(), into_index);
    }
    if dirtied[1] {
        state.block_modified(block.clone(), index);
    }
    prepended
}

/// Under `state-hooks`, the log's record of a build's add refused, `into` no directory, as
/// `into_writer`, its block's writer, reads it.
#[cfg(feature = "state-hooks")]
fn hook_not_directory(
    state: &State,
    into: NodeID,
    node_id: NodeID,
    into_writer: &mut NodeBlockWriter<'_>,
) {
    let read = *into_writer.node(Node::index(into));
    log::record(
        &state.hooks,
        log::Effect::Refused {
            node: node_id,
            parent: into,
            reason: log::Reason::NotDirectory,
            read: log::Read::Word(log::Words(read)),
            expected: log::Expected::Nothing,
        },
    );
}

/// Under `state-hooks`, the log's record of a build's add, `node_writer` holding the node.
#[cfg(feature = "state-hooks")]
fn hook_build_add(
    state: &State,
    into: NodeID,
    node_id: NodeID,
    node_writer: &mut NodeBlockWriter<'_>,
) {
    // The name read and copied only where the thread records.
    if !log::recording() {
        return;
    }
    let index = Node::index(node_id);
    let record = *node_writer.node(index);
    log::record(
        &state.hooks,
        log::Effect::BuildAdd {
            node: node_id,
            parent: into,
            name_hash: record.name_hash,
            name: log::name(node_writer.node_name_bytes(index)),
            kind: log::Kind::of(&record),
        },
    );
}

impl State {
    /// Allocate a fresh node, initialize it, and prepend it to `parent`'s child
    /// chain, returning the new node's ID.
    ///
    /// # Concurrency
    ///
    /// Safe to call concurrently with any other change to `parent`'s chain: the
    /// node is fully initialized before it is published, `Discarded` until then,
    /// and the publish takes the write locks of the parent's block and the
    /// node's, so changes to one directory's chain are serialized and a chain walk
    /// never sees a half-initialized node. Its record is its own
    /// (`initialize_node`): `child` starts empty, but for a link's, and
    /// the publish sets `sibling`.
    ///
    /// **Not** safe for two tasks to add the **same** `(parent, name)`: this is
    /// always-create, not get-or-add, so they produce duplicate siblings. The
    /// find-then-add is a check-then-act at the call site that this cannot close;
    /// callers fanning out across paths must ensure at most one add per
    /// `(parent, name)` (e.g. pre-create the ancestors two paths share, from a
    /// path set holding one case variation of each entry), or use
    /// [`Self::node_attach`], which refuses a name already held.
    ///
    /// The publish **prepends**, and that is relied on rather than incidental: a
    /// caller holding a chain it walked earlier knows everything prepended since
    /// lies ahead of the head it saw, which is what
    /// [`Self::find_subnode_added_since`] walks to. Appending instead would put
    /// a new child past the end of what such a caller holds, and it would stop
    /// finding them. That holds for adds and attaches; a reattach can move the
    /// head the caller saw in front of them, so the search answers that it cannot
    /// tell once the directory's count shows one.
    pub fn node_add<'a>(
        &'a self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &'a str,
    ) -> impl Future<Output = Result<NodeID, StateError>> + Send + 'a {
        self.add_child::<false>(repository, parent, node, name)
    }

    /// Adds `node` under `parent` as [`Self::node_add`] does, staged and dirty added, for the C
    /// API's add (H18): the node publishes with `StagedAdd` and `DirtyAdd` already in its record,
    /// and the publish is refused under the parent's lock where the parent is `Discarded`, as
    /// [`NodeNotFound`], or staged for deletion, as [`InvalidArguments`]. Before the publish, the
    /// parent and its ancestors take the base `Staged` and `Dirty` bits, each up to the first
    /// holding it (H8): none where the parent holds both already, as its ancestors do too, or a
    /// climb that set them is on its way up. A detached directory ends the climb, and the publish
    /// then decides. An add that fails or is dropped after the climb began and before the publish
    /// notes an unfinished change ([`Self::note_unfinished_change`]), as the C API move does.
    pub fn node_add_staged<'a>(
        &'a self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &'a str,
    ) -> impl Future<Output = Result<NodeID, StateError>> + Send + 'a {
        self.add_child::<true>(repository, parent, node, name)
    }

    /// The body of [`Self::node_add`], and with `STAGED` of [`Self::node_add_staged`]. Not an
    /// `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn add_child<'a, const STAGED: bool>(
        &'a self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        mut node: Node,
        name: &'a str,
    ) -> impl Future<Output = Result<NodeID, StateError>> + Send + 'a
    where
        Adding<STAGED>: AddGuard,
    {
        async move {
            // The parent is checked before a slot is allocated: a discarded parent is
            // detached, in flight or free, and the allocator could hand a free one's slot
            // straight back out as the new node's, zeroing the flag that identifies it.
            self.tree(repository.clone()).await?;
            let parent_block = self
                .block(repository.clone(), NodeBlock::index(parent))
                .await?;
            let parent_flags = parent_block.node(Node::index(parent)).flags;
            if STAGED {
                if refuses_child(parent_flags) {
                    return Err(refused_parent(parent_flags));
                }
                STAGED_ADD.apply_to(&mut node);
            } else if parent_flags & NodeFlags::Discarded.bits() != 0 {
                return Err(StateError::internal(
                    "cannot add a child to a discarded node",
                ));
            }
            // The parent and its ancestors are marked before the publish (H8), so an add dropped
            // between leaves no node below a directory the commit does not descend into.
            let climbs = STAGED && parent != ROOT_NODE && !holds_base_marks(parent_flags);
            let unfinished = Adding::<STAGED>::armed_if(climbs, self);
            if climbs
                && let Err(error) = self
                    .node_mark(&repository, parent, STAGED_ADD.for_ancestors())
                    .await
                && !error.is_node_not_found()
            {
                return Err(error);
            }
            // The steps of `prepare_child`, awaited here directly: nested in its future they
            // would grow that of every `node_add`.
            let Some(node_id) = self.grab_node_slot(repository.clone(), false).await? else {
                return Err(StateError::internal(
                    "a slot grab that may load found nothing to load",
                ));
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::Grabbed).await;
            lore_trace!(
                "Block {} node {} added",
                NodeBlock::index(node_id),
                Node::index(node_id)
            );
            let metadata_block = match self
                .try_block_file_metadata_existing(
                    repository.clone(),
                    NodeFileMetadataBlock::index(node::node_to_file_metadata(node_id)),
                )
                .await
            {
                Ok(metadata_block) => metadata_block,
                Err(error) => return Err(self.release_on_error(repository, node_id, error).await),
            };
            let block = match self
                .initialize_node(repository.clone(), node_id, parent, &node, name)
                .await
            {
                Ok(block) => block,
                Err(error) => return Err(self.release_on_error(repository, node_id, error).await),
            };
            #[cfg(feature = "state-hooks")]
            crate::state::node::hooks::reached(crate::state::node::hooks::Point::Initialized).await;
            self.clear_recycled_metadata(metadata_block, node_id);
            let check = if STAGED {
                Publish::Unstaged
            } else {
                Publish::Live
            };
            if self.publish_child::<Unserved>(parent, &parent_block, node_id, &block, check)
                != NameCheck::Free
            {
                Box::pin(self.release_node(repository, node_id)).await;
                if STAGED {
                    return Err(refused_parent(parent_block.node(Node::index(parent)).flags));
                }
                return Err(StateError::internal(
                    "cannot add a child to a discarded node",
                ));
            }
            Adding::<STAGED>::finished(unfinished);
            Ok(node_id)
        }
    }

    /// Add `node` as a new child of `parent` unless a live child already holds
    /// `name`, which is reported as [`AttachOutcome::Exists`].
    ///
    /// The node takes the hash of `name`, whatever `node` carries. The name is
    /// looked up before a slot is taken, so a name already held takes no slot and
    /// stores no name. The publish is [`Self::node_add`]'s, under the same block
    /// write locks, and goes ahead only if the parent's chain change count, which
    /// counts prepends, detaches and renames, has not moved since the lookup began;
    /// every name the namespace primitives and [`Self::node_move_staged`] bring into a chain
    /// is counted, so none of them can have added the name in between. Otherwise the lookup runs
    /// again. A lookup that fails for any reason but the name's absence returns
    /// its error; a detached parent is [`NodeNotFound`].
    pub async fn node_attach(
        &self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
    ) -> Result<AttachOutcome, StateError> {
        self.attach_child::<Unserved>(repository, parent, node, name, false, None)
            .await?
            .ok_or_else(|| StateError::internal("an attach that may load found nothing to load"))
    }

    /// [`Self::node_attach`] of a name `absent`, where given, shows absent from `parent`: under
    /// the publish's write locks the name is checked against the patches of `parent`'s chain log
    /// since the absence, in place of a walk of the chain. A name the log shows attached since is
    /// [`AttachOutcome::Exists`]. Where the log no longer holds every patch since, or the absence
    /// was read from another block than the one locked, the name is found by walking.
    pub async fn node_attach_absent(
        &self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
        absent: Option<&Absent>,
    ) -> Result<AttachOutcome, StateError> {
        self.attach_child::<Unserved>(repository, parent, node, name, false, absent)
            .await?
            .ok_or_else(|| StateError::internal("an attach that may load found nothing to load"))
    }

    /// [`Self::node_attach_absent`], reading nothing from the store: `None`, with nothing in the
    /// namespace changed, where the tree, the parent's block, a block of the parent's chain its
    /// lookup walks, or the slot's block, its name table or its file metadata block is not in
    /// memory. The unused chain may have retired full blocks or gained an empty fresh one
    /// meanwhile, as a loading grab leaves it.
    ///
    /// Without `absent`, or where the absence does not cover the name, the lookup of `name`
    /// walks the parent's chain through blocks in memory alone, so the caller keeps that chain in
    /// memory, as a listing of the parent holds it.
    pub async fn node_attach_resident(
        &self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
        absent: Option<&Absent>,
    ) -> Result<Option<AttachOutcome>, StateError> {
        self.attach_child::<Unserved>(repository, parent, node, name, true, absent)
            .await
    }

    /// [`Self::node_attach_absent`]; with `resident_only`, `None` on the first block it needs that
    /// is not in memory, its name lookup's and its served mark's included, reading nothing from
    /// the store and attaching nothing; directories its served mark reached before stay marked.
    /// In the served tier, after the name lookup, the parent copy read with the count
    /// is pre-tested against the served chain rule and marked where it fails; the mark changes no
    /// chain count, so the count read before it still guards the publish. A publish the rule
    /// refuses under the locks starts the attempt again. An attempt after a lookup that missed
    /// walks only what was prepended since ([`State::find_subnode_since`]), except with
    /// `resident_only`, whose lookup walks the whole chain in memory and reads nothing from the
    /// store. Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub(in crate::state) fn attach_child<'a, T: Tier>(
        &'a self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        mut node: Node,
        name: &'a str,
        resident_only: bool,
        absent: Option<&'a Absent>,
    ) -> impl Future<Output = Result<Option<AttachOutcome>, StateError>> + Send + 'a {
        async move {
            let name_hash = hash::hash_string(name);
            node.name_hash = name_hash;
            let parent_block = if resident_only {
                let resident = self.runtime_read().tree.is_some();
                match self.resident_block(NodeBlock::index(parent)) {
                    Some(block) if resident => block,
                    _ => {
                        #[cfg(feature = "state-hooks")]
                        log::arm(&self.hooks, log::Arm::NeedLoad);
                        return Ok(None);
                    }
                }
            } else {
                self.tree(repository.clone()).await?;
                self.block(repository.clone(), NodeBlock::index(parent))
                    .await?
            };
            let mut absent =
                absent.and_then(|absent| absent.read_from(self, &parent_block, parent, name_hash));
            let mut slot: Option<(NodeID, Arc<NodeBlock>)> = None;
            let mut since: Option<(Option<NodeID>, u32)> = None;
            let mut attempts = 0;
            let outcome = loop {
                if attempts == self.chain_change_attempts() {
                    #[cfg(feature = "state-hooks")]
                    log::arm(&self.hooks, log::Arm::SlowDown);
                    break Err(StateError::from(SlowDown));
                }
                attempts += 1;
                let (parent_node, changes) =
                    parent_block.node_with_chain_changes(Node::index(parent));
                if let Err(error) = check_live_directory(&parent_node) {
                    break Err(error);
                }
                if absent.is_none() {
                    let found = if resident_only {
                        let Some(found) = self.find_subnode_resident(parent, name_hash).transpose()
                        else {
                            break Ok(None);
                        };
                        found
                    } else if let Some(since) = since {
                        Box::pin(self.find_subnode_since(
                            repository.clone(),
                            parent,
                            &parent_block,
                            since,
                            name_hash,
                        ))
                        .await
                    } else {
                        self.find_subnode(repository.clone(), parent, name_hash)
                            .await
                    };
                    match found {
                        Ok(existing) => break Ok(Some(AttachOutcome::Exists(existing))),
                        Err(error) if error.is_node_not_found() => {
                            since = Some((parent_node.child(), changes));
                        }
                        Err(error) => break Err(error),
                    }
                }
                if T::SERVED && !admits_served_change(parent, &parent_node, &parent_block) {
                    let below = (&parent_block, parent_node.parent);
                    let marked = if resident_only {
                        T::served_mark_resident(self, &repository, parent, below).await
                    } else {
                        T::served_mark(self, &repository, parent, below)
                            .await
                            .map(|()| true)
                    };
                    match marked {
                        Ok(true) => {}
                        Ok(false) => break Ok(None),
                        Err(error) => break Err(error),
                    }
                }
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::Lookup).await;
                let (node_id, block) = if let Some(taken) = &slot {
                    taken
                } else {
                    match self
                        .prepare_child(repository.clone(), parent, &node, name, resident_only)
                        .await
                    {
                        Ok(Some(prepared)) => slot.insert(prepared),
                        Ok(None) => break Ok(None),
                        Err(error) => break Err(error),
                    }
                };
                let check = match absent {
                    Some(absent) => Publish::Absent(absent),
                    None => Publish::Unchanged(changes),
                };
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::BeforePublish)
                    .await;
                match self.publish_child::<T>(parent, &parent_block, *node_id, block, check) {
                    NameCheck::Free => break Ok(Some(AttachOutcome::Attached(*node_id))),
                    NameCheck::Held(existing) => break Ok(Some(AttachOutcome::Exists(existing))),
                    NameCheck::Uncovered => {
                        #[cfg(feature = "state-hooks")]
                        log::arm(&self.hooks, log::Arm::AttachUncovered);
                        absent = None;
                    }
                    NameCheck::NeedsMark => {
                        #[cfg(feature = "state-hooks")]
                        log::arm(&self.hooks, log::Arm::AttachNeedsMark);
                        self.served_refused();
                    }
                    NameCheck::Changed => {
                        #[cfg(feature = "state-hooks")]
                        log::arm(&self.hooks, log::Arm::AttachChanged);
                    }
                }
                self.attach_retries.fetch_add(1, Ordering::Relaxed);
            };
            if !matches!(outcome, Ok(Some(AttachOutcome::Attached(_))))
                && let Some((node_id, _)) = slot
            {
                Box::pin(self.release_node(repository, node_id)).await;
            }
            outcome
        }
    }

    /// Takes a slot and fills it with `node`, named `name`, under `parent`, then clears any
    /// file metadata the slot carries from an earlier node, all before the node can be
    /// published: nothing after the publish can fail, and a failed fill leaves the metadata
    /// untouched. Releases the slot on failure. With
    /// `resident_only`, `None`, the slot released, where the slot's block, its name table or its
    /// file metadata is not in memory; a fresh block the grab allocated stays, empty, on the
    /// unused chain.
    pub(in crate::state) async fn prepare_child(
        &self,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: &Node,
        name: &str,
        resident_only: bool,
    ) -> Result<Option<(NodeID, Arc<NodeBlock>)>, StateError> {
        let Some(node_id) = self
            .grab_node_slot(repository.clone(), resident_only)
            .await?
        else {
            return Ok(None);
        };
        #[cfg(feature = "state-hooks")]
        crate::state::node::hooks::reached(crate::state::node::hooks::Point::Grabbed).await;

        lore_trace!(
            "Block {} node {} added",
            NodeBlock::index(node_id),
            Node::index(node_id)
        );
        let metadata_index = NodeFileMetadataBlock::index(node::node_to_file_metadata(node_id));
        let metadata = if resident_only {
            if let Ok(metadata) = self.resident_slot_metadata(node_id, metadata_index) {
                Ok(metadata)
            } else {
                #[cfg(feature = "state-hooks")]
                log::arm(&self.hooks, log::Arm::NeedLoad);
                Box::pin(self.release_node(repository, node_id)).await;
                return Ok(None);
            }
        } else {
            self.try_block_file_metadata_existing(repository.clone(), metadata_index)
                .await
        };
        let metadata_block = match metadata {
            Ok(metadata_block) => metadata_block,
            Err(error) => return Err(self.release_on_error(repository, node_id, error).await),
        };
        if let Some(block) = &metadata_block {
            self.hold_attach_metadata(block);
        }
        match self
            .initialize_node(repository.clone(), node_id, parent, node, name)
            .await
        {
            Ok(block) => {
                #[cfg(feature = "state-hooks")]
                crate::state::node::hooks::reached(crate::state::node::hooks::Point::Initialized)
                    .await;
                self.clear_recycled_metadata(metadata_block, node_id);
                Ok(Some((node_id, block)))
            }
            Err(error) => Err(self.release_on_error(repository, node_id, error).await),
        }
    }

    /// Releases the unpublished slot `node_id` and hands back `error`. A cold path, boxed with
    /// its arguments, so it adds a pointer to the future of every add.
    pub(in crate::state) fn release_on_error(
        &self,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
        error: StateError,
    ) -> Pin<Box<impl Future<Output = StateError> + Send + '_>> {
        Box::pin(async move {
            self.release_node(repository, node_id).await;
            error
        })
    }

    /// Prepends the prepared `node_id` to `parent`'s chain under the write locks of both
    /// blocks, where `parent` is not detached and `check` passes; answers what the check found,
    /// [`NameCheck::Free`] where it prepended. In the served tier, a free name answers
    /// [`NameCheck::NeedsMark`] instead where `parent` does not admit a served change, so a name
    /// found held here answers without a mark; on the absence path, the pre-test may have marked
    /// `parent` before this check, and that mark stands.
    pub(in crate::state) fn publish_child<T: Tier>(
        &self,
        parent: NodeID,
        parent_block: &Arc<NodeBlock>,
        node_id: NodeID,
        block: &Arc<NodeBlock>,
        check: Publish<'_>,
    ) -> NameCheck {
        #[cfg(feature = "state-hooks")]
        let _label = log::label(log::Section::Publish);
        let mut locks = BlockLocks::lock([
            Some((NodeBlock::index(parent), parent_block)),
            Some((NodeBlock::index(node_id), block)),
            None,
            None,
        ]);
        // A build's `Unchanged` check meeting a chain log a listing installed since its lookup
        // read none, whatever the check answers.
        #[cfg(feature = "state-hooks")]
        if log::building()
            && matches!(check, Publish::Unchanged(_))
            && locks
                .entry(parent)
                .is_some_and(|held| held.writer.chain_log_mut(Node::index(parent)).is_some())
        {
            log::arm(&self.hooks, log::Arm::LogBetweenLookupAndPublish);
        }
        let checked = match check {
            Publish::Live
                if locks
                    .node(parent)
                    .is_some_and(|parent| !parent.is_discarded()) =>
            {
                NameCheck::Free
            }
            Publish::Unstaged
                if locks
                    .node(parent)
                    .is_some_and(|parent| !refuses_child(parent.flags)) =>
            {
                NameCheck::Free
            }
            Publish::Unchanged(changes)
                if chain_unchanged(&mut locks, parent, parent_block, changes) =>
            {
                NameCheck::Free
            }
            Publish::Live | Publish::Unstaged | Publish::Unchanged(_) => NameCheck::Changed,
            Publish::Absent(absent) => logged_name(&mut locks, parent, absent),
            Publish::Logged(at, name_hash) => logged_since(&mut locks, parent, (at, name_hash)),
        };
        if checked != NameCheck::Free {
            #[cfg(feature = "state-hooks")]
            self.hook_publish_refused(&mut locks, parent, node_id, check, checked);
            return checked;
        }
        if T::SERVED
            && !locks
                .node(parent)
                .is_some_and(|held| admits_served_change(parent, &held, parent_block))
        {
            return NameCheck::NeedsMark;
        }
        publish_locked(&mut locks, parent, node_id);
        #[cfg(feature = "state-hooks")]
        self.hook_published(&mut locks, parent, node_id, check);
        locks.release(self);
        NameCheck::Free
    }

    /// Under `state-hooks`, the log's record of a publish, under its locks.
    #[cfg(feature = "state-hooks")]
    fn hook_published(
        &self,
        locks: &mut BlockLocks<'_>,
        parent: NodeID,
        node_id: NodeID,
        check: Publish<'_>,
    ) {
        // The name read and copied only where the thread records.
        if !log::recording() {
            return;
        }
        let Some(record) = locks.node(node_id) else {
            return;
        };
        let instance = locks.hook_instance(parent);
        let (check, expected) = match check {
            Publish::Live => (log::Check::Live, log::Expected::Nothing),
            Publish::Unstaged => (log::Check::Unstaged, log::Expected::Nothing),
            Publish::Unchanged(changes) => (
                log::Check::Unchanged,
                log::Expected::Count { changes, instance },
            ),
            Publish::Absent(absent) => (log::Check::Absent, log::Expected::Log { seq: absent.at }),
            Publish::Logged(at, _) => (log::Check::Logged, log::Expected::Log { seq: at }),
        };
        let name = locks.hook_name(node_id);
        log::record(
            &self.hooks,
            log::Effect::Publish {
                node: node_id,
                parent,
                name_hash: record.name_hash,
                name,
                kind: log::Kind::of(&record),
                check,
                expected,
                instance,
            },
        );
    }

    /// Under `state-hooks`, the log's record of a publish refused on what it read under its
    /// locks: a parent `Live` or `Unstaged` refuses, and a name the log shows held. A check that
    /// starts the attempt again records none.
    #[cfg(feature = "state-hooks")]
    fn hook_publish_refused(
        &self,
        locks: &mut BlockLocks<'_>,
        parent: NodeID,
        node_id: NodeID,
        check: Publish<'_>,
        checked: NameCheck,
    ) {
        if !log::recording() {
            return;
        }
        let parent_record = locks.node(parent);
        // The log's position a `Held` answer read from, and the name's hash it looked up.
        let (expected, name_hash) = match check {
            Publish::Absent(absent) => (log::Expected::Log { seq: absent.at }, absent.name_hash),
            Publish::Logged(at, name_hash) => (log::Expected::Log { seq: at }, name_hash),
            _ => (log::Expected::Nothing, 0),
        };
        let refused = match (checked, check) {
            (NameCheck::Held(holder), _) => {
                Some((log::Reason::Exists, log::Read::Holder { holder, name_hash }))
            }
            (NameCheck::Changed, Publish::Live) => parent_record
                .map(|record| (log::Reason::NotFound, log::Read::Word(log::Words(record)))),
            (NameCheck::Changed, Publish::Unstaged) => parent_record.map(|record| {
                let reason = if record.is_discarded() {
                    log::Reason::NotFound
                } else {
                    log::Reason::StagedDelete
                };
                (reason, log::Read::Word(log::Words(record)))
            }),
            _ => None,
        };
        if let Some((reason, read)) = refused {
            log::record(
                &self.hooks,
                log::Effect::Refused {
                    node: node_id,
                    parent,
                    reason,
                    read,
                    expected: if reason == log::Reason::Exists {
                        expected
                    } else {
                        log::Expected::Nothing
                    },
                },
            );
        }
    }
}

/// An add of [`State::add_child`], a C API add's where `STAGED`.
pub(in crate::state) struct Adding<const STAGED: bool>;

/// What an add holds from its climb to its publish: a C API add the guard of its climb
/// ([`UnfinishedChange`]), and a plain add nothing, so `node_add`'s future holds no guard.
pub(in crate::state) trait AddGuard {
    type Guard<'a>: Send;

    /// The guard, armed where `climbs`.
    fn armed_if(climbs: bool, state: &State) -> Self::Guard<'_>;

    /// The add published: nothing to note.
    fn finished(guard: Self::Guard<'_>);
}

impl AddGuard for Adding<true> {
    type Guard<'a> = UnfinishedChange<'a>;

    #[inline]
    fn armed_if(climbs: bool, state: &State) -> UnfinishedChange<'_> {
        UnfinishedChange::armed_if(climbs, state)
    }

    #[inline]
    fn finished(guard: UnfinishedChange<'_>) {
        guard.finished();
    }
}

impl AddGuard for Adding<false> {
    type Guard<'a> = ();

    #[inline]
    fn armed_if(_: bool, _: &State) {}

    #[inline]
    fn finished((): ()) {}
}

/// The marks of a C API add's node ([`State::node_add_staged`]).
const STAGED_ADD: Mark = Mark::staged_and_dirty(NodeFlags::StagedAdd, NodeFlags::DirtyAdd);

/// Whether a C API add refuses a parent holding `flags`: one `Discarded` or staged for deletion.
#[inline]
fn refuses_child(flags: u16) -> bool {
    let staged_delete = NodeFlags::StagedDelete.bits();
    flags & NodeFlags::Discarded.bits() != 0 || flags & staged_delete == staged_delete
}

/// Refuses `node` where it is not a clean copy: a swap's copies are clean, which is what exempts
/// them from the served chain rule (C11c).
fn check_clean_copy(node: &Node) -> Result<(), StateError> {
    if node.flags & NodeFlags::DirtyBits.bits() != 0 || node.ext_flags != 0 {
        return Err(StateError::from(InvalidArguments {
            reason: "a swap attaches only clean copies".into(),
        }));
    }
    Ok(())
}

/// Why a C API add refused a parent holding `flags`: a `Discarded` parent is [`NodeNotFound`],
/// one staged for deletion [`InvalidArguments`], as is one admitting a child again since.
#[cold]
fn refused_parent(flags: u16) -> StateError {
    if flags & NodeFlags::Discarded.bits() != 0 {
        NodeNotFound.into()
    } else {
        StateError::from(InvalidArguments {
            reason: "the parent is staged for deletion, so a child added under it would go with it"
                .into(),
        })
    }
}
