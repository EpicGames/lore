// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! States for scenarios: an in-memory repository and trees built in it by path.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::RepositoryId;
use lore_revision::node;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFileMetadata;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::Ext;
use lore_revision::state::LinkPin;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use crate::harness::faults::FailingStore;

/// The execution context a run's operations are scoped in.
pub fn execution() -> Arc<lore_revision::interface::ExecutionContext> {
    Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "state-tests".to_string(),
        ),
    )
}

/// A repository context over in-memory stores. Called inside the run's execution scope.
pub async fn repository() -> Arc<RepositoryContext> {
    over(immutable().await).await
}

/// [`repository`] over a [`FailingStore`], which fails no read until told to, and that store: what
/// a tree built in it saves, its links' revisions included, it reads back through the store.
pub async fn failing_repository() -> (Arc<RepositoryContext>, Arc<FailingStore>) {
    let store = FailingStore::new(immutable().await);
    (over(store.clone()).await, store)
}

/// An in-memory immutable store.
async fn immutable() -> Arc<dyn lore_storage::ImmutableStore> {
    lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("an in-memory immutable store")
}

/// A repository context over `immutable` and an in-memory mutable store over it.
async fn over(immutable: Arc<dyn lore_storage::ImmutableStore>) -> Arc<RepositoryContext> {
    let mutable = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .expect("an in-memory mutable store");
    Arc::new(RepositoryContext::new_null_context(immutable, mutable))
}

/// A State and its repository, with the nodes a scenario names, and the revision it stands
/// for: a second State holding those nodes as built, which a settle compares with.
pub struct Tree {
    pub state: Arc<State>,
    pub repository: Arc<RepositoryContext>,
    /// Each node the tree was built with, by path.
    pub nodes: Vec<(String, NodeID)>,
    /// The tree as built, as a commit of it holds it ([`Self::commit`]).
    pub committed: Arc<State>,
    /// Each node of [`Self::committed`], by path.
    committed_nodes: Vec<(String, NodeID)>,
    /// The directories [`Self::fill_block`] and its kin added files to, none recorded in
    /// [`Self::nodes`], so none in [`Self::committed`].
    filled: Mutex<Vec<NodeID>>,
    /// The revisions its links may be pinned to, where [`Self::link`] made one.
    pub linked: Option<Linked>,
}

impl Tree {
    /// A fresh State holding `paths`, each a file or, ending in `/`, a directory; parents are
    /// made as needed. Every node is clean and holds content of its own, by its path; the
    /// committed State holds the same.
    pub async fn new(paths: &[&str]) -> Self {
        Self::new_in(repository().await, paths).await
    }

    /// [`Self::new`] in `repository`.
    pub async fn new_in(repository: Arc<RepositoryContext>, paths: &[&str]) -> Self {
        let mut tree = Self::building_in(repository, paths).await;
        tree.commit().await;
        tree
    }

    /// [`Self::new_in`] with no committed State yet.
    async fn building_in(repository: Arc<RepositoryContext>, paths: &[&str]) -> Self {
        let mut tree = Self {
            state: State::new(),
            committed: State::new(),
            repository,
            nodes: Vec::new(),
            committed_nodes: Vec::new(),
            filled: Mutex::default(),
            linked: None,
        };
        for path in paths {
            tree.make(path).await;
        }
        tree
    }

    /// Builds [`Self::committed`] afresh from [`Self::nodes`] as they stand, each node at its
    /// recorded path with its content, as a commit of the tree stores it.
    pub async fn commit(&mut self) {
        let committed = State::new();
        let mut made: Vec<(String, NodeID)> = Vec::with_capacity(self.nodes.len());
        let mut nodes = self.nodes.clone();
        nodes.sort_by_key(|(path, _)| path.matches('/').count());
        for (path, id) in nodes {
            let record = self
                .state
                .node(self.repository.clone(), id)
                .await
                .expect("a built node reads");
            let (parent, name) = match path.rsplit_once('/') {
                Some((parent, name)) => {
                    let parent = made
                        .iter()
                        .find(|(made, _)| made == parent)
                        .map_or_else(|| panic!("no committed '{parent}'"), |(_, id)| *id);
                    (parent, name)
                }
                None => (ROOT_NODE, path.as_str()),
            };
            let node = Node {
                flags: record.flags & NodeFlags::File.bits(),
                name_hash: record.name_hash,
                address: record.address,
                mode: record.mode,
                size: record.size,
                ..Node::default()
            };
            let committed_id = committed
                .node_add(self.repository.clone(), parent, node, name)
                .await
                .expect("a committed node adds");
            made.push((path, committed_id));
        }
        self.committed = committed;
        self.committed_nodes = made;
    }

    /// [`Self::new`] with the link `link` ([`Self::link`]).
    pub async fn linking(paths: &[&str], link: &str) -> Self {
        let mut tree = Self::new(paths).await;
        tree.link(link).await;
        tree
    }

    /// Adds the link `link`, below a directory the tree holds, pinning `src` in [`Linked::first`]:
    /// of the revisions of a second repository its first link saves in the tree's stores, kept in
    /// [`Self::linked`]. The link's id.
    pub async fn link(&mut self, link: &str) -> NodeID {
        if self.linked.is_none() {
            let repository = linked_repository();
            let context = self.repository.to_link_context(repository).await;
            let first = Self::building_in(context.clone(), &["src/a", "src/b", "other/c"]).await;
            let second =
                Self::building_in(context.clone(), &["old/a", "src/c", "src/d", "src/e"]).await;
            // A read walking the other revision than its pin names lists other children.
            for (pinned, other) in [(&first, &second), (&second, &first)] {
                let src = pinned.id("src");
                assert_ne!(
                    pinned.listed(src).await,
                    other.listed(src).await,
                    "the node at `src` in one revision lists other children in the other"
                );
            }
            self.linked = Some(Linked {
                repository,
                first: Self::revision_saved(&context, &first).await,
                second: Self::revision_saved(&context, &second).await,
                dirty: None,
                context,
            });
        }
        let linked = self.linked.as_ref().expect("the revisions are saved");
        let (parent, name) = link
            .rsplit_once('/')
            .map_or((ROOT_NODE, link), |(parent, name)| (self.id(parent), name));
        let pin = Node {
            flags: NodeFlags::Link.bits(),
            name_hash: hash_string(name),
            address: linked.address(linked.first.signature),
            child: linked.first.src,
            ..Node::default()
        };
        let id = (self.state)
            .node_add(self.repository.clone(), parent, pin, name)
            .await
            .expect("the link adds");
        self.nodes.push((link.to_string(), id));
        id
    }

    /// Adds the link `link` ([`Self::link`]), dirty and pinning `src` in the revision `pinned`
    /// names, as the settle leaves one pinning the revision holding markers ([`Linked::dirty`]),
    /// which it saves first where no link saved it yet. The link's id.
    pub async fn dirty_link(&mut self, link: &str, pinned: Pinned) -> NodeID {
        let id = self.link(link).await;
        let linked = self.linked.as_mut().expect("the revisions are saved");
        if linked.dirty.is_none() {
            let dirtied = Self::dirtied(linked.context.clone()).await;
            linked.dirty = Some(Self::revision_saved(&linked.context, &dirtied).await);
        }
        let pin = linked.pin(pinned);
        (self.state)
            .node_set_link_target(&self.repository, id, pin)
            .await
            .expect("the link repins");
        let mark = Mark::dirty(NodeFlags::Dirty, Ext::Set(NodeExtFlags::NoFlags), true);
        (self.state)
            .node_mark(&self.repository, id, mark)
            .await
            .expect("the link is marked");
        id
    }

    /// A revision of the linked repository in `repository` holding dirty markers, as the settle
    /// saves a linked State holding them: below `src`, `a` a dirty file, `b` a clean one, `n` a
    /// dirty-add directory holding the dirty-add file `m`, and `o` a dirty-delete directory holding
    /// `p`.
    async fn dirtied(repository: Arc<RepositoryContext>) -> Self {
        let dirtied =
            Self::building_in(repository, &["src/a", "src/b", "src/n/m", "src/o/p"]).await;
        for (path, flags) in [
            ("src/a", NodeFlags::DirtyModify),
            ("src/n/m", NodeFlags::DirtyAdd),
            ("src/n", NodeFlags::DirtyAdd),
            ("src/o", NodeFlags::DirtyDelete),
        ] {
            let mark = Mark::dirty(flags, Ext::Set(NodeExtFlags::NoFlags), true);
            (dirtied.state)
                .node_mark(&dirtied.repository, dirtied.id(path), mark)
                .await
                .expect("the linked revision's node is marked");
        }
        dirtied
    }

    /// `built` saved in `repository`.
    async fn revision_saved(repository: &Arc<RepositoryContext>, built: &Self) -> Saved {
        let token = RepositoryWriteToken::in_memory(&InMemory);
        let signature = (built.state)
            .serialize(repository.clone(), &token)
            .await
            .expect("the revision saves");
        Saved {
            signature,
            src: built.id("src"),
        }
    }

    /// The children of `node` with their names' hashes, as its listing answers them.
    async fn listed(&self, node: NodeID) -> Vec<(NodeID, u64)> {
        (self.state)
            .node_children_with_name_hash(self.repository.clone(), node)
            .await
            .expect("the node lists")
    }

    /// The node [`Self::committed`] holds at `path` (without a trailing `/`).
    pub fn committed_id(&self, path: &str) -> NodeID {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return ROOT_NODE;
        }
        self.committed_nodes
            .iter()
            .find(|(p, _)| p == path)
            .map_or_else(|| panic!("no committed node for '{path}'"), |(_, id)| *id)
    }

    /// Each node the tree was built with that [`Self::committed`] holds, with the committed node at
    /// its path: the node a settle of it compares with ([`crate::harness::catalogue::ops::settle`]).
    /// But a directory a fill added files to: the committed one holds none of them.
    pub fn committed_of(&self) -> BTreeMap<NodeID, NodeID> {
        let filled = self.filled.lock().unwrap_or_else(PoisonError::into_inner);
        let committed = |path: &String| {
            (self.committed_nodes.iter())
                .find(|(built, _)| built == path)
                .map(|(_, id)| *id)
        };
        (self.nodes.iter())
            .filter(|(_, id)| !filled.contains(id))
            .filter_map(|(path, id)| committed(path).map(|committed| (*id, committed)))
            .collect()
    }

    /// [`Self::new`] with `before`, then a directory `filler` of files filling the block the
    /// next node would take, then `after`: the nodes of `after` lie in a later block than those
    /// of `before`. The fill files are not recorded in [`Self::nodes`].
    pub async fn spread(before: &[&str], filler: &str, after: &[&str]) -> Self {
        Self::spread_in(repository().await, before, filler, after).await
    }

    /// [`Self::spread`] in `repository`.
    pub async fn spread_in(
        repository: Arc<RepositoryContext>,
        before: &[&str],
        filler: &str,
        after: &[&str],
    ) -> Self {
        let mut tree = Self::building_in(repository, before).await;
        let dir = tree.make(&format!("{filler}/")).await;
        tree.fill_block(dir, "fill").await;
        for path in after {
            tree.make(path).await;
        }
        tree.commit().await;
        tree
    }

    /// This tree as a process loading its State from the store finds it: saved, and loaded
    /// again, so only the blocks the load reads are in memory.
    pub async fn reloaded(self) -> Self {
        let state = self.saved().await;
        Self { state, ..self }
    }

    /// Adds files `{prefix}0`, `{prefix}1` and on to directory `dir` until one takes the last
    /// slot of its block, answering that file; none is recorded in [`Self::nodes`].
    pub async fn fill_block(&self, dir: NodeID, prefix: &str) -> NodeID {
        self.fill_block_to(dir, prefix, BLOCK_NODE_COUNT - 1).await
    }

    /// [`Self::fill_block`] but for the block's last slot: the file in the last slot but one.
    pub async fn fill_block_but_one(&self, dir: NodeID, prefix: &str) -> NodeID {
        self.fill_block_to(dir, prefix, BLOCK_NODE_COUNT - 2).await
    }

    /// Adds files `{prefix}0`, `{prefix}1` and on to directory `dir` until one takes slot `slot`
    /// of its block, answering that file.
    async fn fill_block_to(&self, dir: NodeID, prefix: &str, slot: usize) -> NodeID {
        (self.filled.lock().unwrap_or_else(PoisonError::into_inner)).push(dir);
        for index in 0.. {
            let name = format!("{prefix}{index}");
            let node = Node {
                flags: NodeFlags::File.bits(),
                name_hash: hash_string(&name),
                ..Node::default()
            };
            let id = self
                .state
                .node_add(self.repository.clone(), dir, node, &name)
                .await
                .expect("a fill file adds");
            if Node::index(id) == slot {
                return id;
            }
        }
        unreachable!("a block fills")
    }

    /// This tree's State saved as it stands and loaded again: what a later process finds, which
    /// holds only the changes recorded in a changed block.
    pub async fn saved(&self) -> Arc<State> {
        let signature = self.save().await.expect("the State saves");
        self.load(signature).await
    }

    /// This tree's State saved as it stands, a call of Serialize in a recording run: the
    /// signature it answers, or none where it failed.
    pub async fn save(&self) -> Option<Hash> {
        use crate::harness::catalogue::call::Answer;
        use crate::harness::catalogue::call::Outcome;
        crate::harness::catalogue::call::call(
            &self.state,
            (&crate::harness::catalogue::SERIALIZE, None),
            async {
                let token = RepositoryWriteToken::in_memory(&InMemory);
                match self.state.serialize(self.repository.clone(), &token).await {
                    Ok(signature) => (Some(signature), Outcome::Done(Answer::None)),
                    Err(error) => (None, crate::harness::catalogue::ops::failed(&error)),
                }
            },
        )
        .await
    }

    /// The State a save answered `signature` for, loaded afresh.
    pub async fn load(&self, signature: Hash) -> Arc<State> {
        State::deserialize(self.repository.clone(), signature)
            .await
            .expect("the State loads")
    }

    /// This tree saved and loaded again through a [`FailingStore`] over its store, with no block
    /// in memory: the store fails every read once told to, as a store failing part way through
    /// an operation does.
    pub async fn failing(self) -> (Self, Arc<FailingStore>) {
        let token = RepositoryWriteToken::in_memory(&InMemory);
        let signature = self
            .state
            .serialize(self.repository.clone(), &token)
            .await
            .expect("the State saves");
        let store = FailingStore::new(self.repository.immutable_store());
        let mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            store.clone(),
        )
        .await
        .expect("an in-memory mutable store");
        let repository = Arc::new(RepositoryContext::new_null_context(store.clone(), mutable));
        let state = State::deserialize(repository.clone(), signature)
            .await
            .expect("the State loads");
        (
            Self {
                state,
                repository,
                ..self
            },
            store,
        )
    }

    /// A tree of `d/f` and `d/g` with `d/f` discarded, so its slot heads its block's free list,
    /// and the file metadata of `metadata`'s slot set where given, `d/f`'s or `d/g`'s, so the
    /// file metadata block the two slots share is stored; loaded afresh through a
    /// [`Self::failing`] store, with `d`'s block in memory and its names not. Answers the freed
    /// slot too.
    pub async fn with_a_free_slot(metadata: Option<&str>) -> (Self, Arc<FailingStore>, NodeID) {
        let tree = Self::new(&["d/f", "d/g"]).await;
        let f = tree.id("d/f");
        if let Some(path) = metadata {
            tree.set_file_metadata(tree.id(path), Hash::from([3u8; 32]))
                .await;
        }
        lore_revision::state::node_discard_fused(tree.state.clone(), tree.repository.clone(), f)
            .await
            .expect("'d/f' is discarded");
        let (tree, store) = tree.failing().await;
        tree.state
            .node(tree.repository.clone(), tree.id("d"))
            .await
            .expect("'d' reads");
        let block = tree
            .state
            .resident_block(NodeBlock::index(f))
            .expect("the free slot's block is in memory");
        assert!(
            !block.is_nametable_deserialized(),
            "its names are not in memory"
        );
        (tree, store, f)
    }

    /// Sets the file metadata of `id`'s slot to `value`, as a metadata set writes it: the State
    /// marked dirty where the block turned dirty, so a save writes it.
    pub async fn set_file_metadata(&self, id: NodeID, value: Hash) {
        let slot = node::node_to_file_metadata(id);
        let index = NodeFileMetadataBlock::index(slot);
        let block = self
            .state
            .block_file_metadata(self.repository.clone(), index)
            .await
            .expect("the file metadata block");
        let dirtied = {
            let mut writer = block.write();
            writer.node(NodeFileMetadata::index(slot)).metadata = value;
            writer.mark_dirty()
        };
        if dirtied {
            self.state.block_file_metadata_modified(block, index);
            self.state.mark_dirty();
        }
    }

    /// [`Self::committed`] saved and loaded afresh through [`Self::repository`], so its blocks are
    /// read from the store when asked for, as a commit's stored tree is; through a
    /// [`Self::failing`] tree's store, its reads fail when the store is told to.
    pub async fn reload_committed(&mut self) {
        let token = RepositoryWriteToken::in_memory(&InMemory);
        let signature = self
            .committed
            .serialize(self.repository.clone(), &token)
            .await
            .expect("the committed State saves");
        self.committed = State::deserialize(self.repository.clone(), signature)
            .await
            .expect("the committed State loads");
    }

    /// This tree as a provider serves it: `state`, a State of the same revision loaded afresh,
    /// in place of its own, so its nodes keep their IDs.
    pub fn serving(self, state: Arc<State>) -> Self {
        Self { state, ..self }
    }

    /// The node made for `path` (without a trailing `/`).
    pub fn id(&self, path: &str) -> NodeID {
        let path = path.trim_end_matches('/');
        self.nodes
            .iter()
            .find(|(p, _)| p == path)
            .map_or_else(|| panic!("no node made for '{path}'"), |(_, id)| *id)
    }

    /// Makes `path` and any parent not made yet.
    async fn make(&mut self, path: &str) -> NodeID {
        let directory = path.ends_with('/');
        let path = path.trim_end_matches('/');
        if let Some((_, id)) = self.nodes.iter().find(|(p, _)| p == path) {
            return *id;
        }
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent, name)) => (Box::pin(self.make(&format!("{parent}/"))).await, name),
            None => (ROOT_NODE, path),
        };
        let mut node = Node {
            flags: if directory { 0 } else { NodeFlags::File.bits() },
            name_hash: hash_string(name),
            ..Node::default()
        };
        node.address.hash = lore_base::types::Hash::from_u64(hash_string(path));
        let id = self
            .state
            .node_add(self.repository.clone(), parent, node, name)
            .await
            .expect("a fixture node adds");
        self.nodes.push((path.to_string(), id));
        id
    }
}

/// The second repository a fixture's links pin, in their tree's stores.
fn linked_repository() -> RepositoryId {
    let mut id = RepositoryId::default();
    id.data_mut()[15] = 7;
    id
}

/// Revisions of the second repository a fixture's links pin, saved in their tree's stores: the
/// first holding `src/a`, `src/b` and `other/c`; the second holding `old/a`, `src/c`, `src/d` and
/// `src/e`, as renames of `src` to `old` and of `other` into its path, a delete and adds would
/// leave the first, though built afresh, so its node ids follow no history. The node at `src` in
/// either lists other children in the other.
#[derive(Debug)]
pub struct Linked {
    pub repository: RepositoryId,
    /// The tree's context aimed at the repository, which loads its revisions.
    pub context: Arc<RepositoryContext>,
    pub first: Saved,
    pub second: Saved,
    /// A third, holding dirty markers below `src`, as a dirty link pins, where a dirty link of the
    /// tree's saved it ([`Tree::dirty_link`]).
    pub dirty: Option<Saved>,
}

/// A revision of the [`Linked`] repository: its signature, and the node at `src` in it.
#[derive(Clone, Copy, Debug)]
pub struct Saved {
    pub signature: Hash,
    pub src: NodeID,
}

/// Which of the [`Linked`] revisions a pin names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pinned {
    First,
    Second,
    Dirty,
}

impl Linked {
    /// The revision `pinned` names.
    pub fn revision(&self, pinned: Pinned) -> Saved {
        match pinned {
            Pinned::First => self.first,
            Pinned::Second => self.second,
            Pinned::Dirty => self.dirty.expect("a dirty link saved the revision"),
        }
    }

    /// The address of the revision `hash` of the repository.
    pub fn address(&self, hash: Hash) -> Address {
        Address {
            hash,
            context: self.repository.into(),
        }
    }

    /// A whole pin of `src` in the revision `pinned` names.
    pub fn pin(&self, pinned: Pinned) -> LinkPin {
        let saved = self.revision(pinned);
        LinkPin::Whole {
            address: self.address(saved.signature),
            child: saved.src,
        }
    }
}

/// Saves a State to the in-memory stores, which need no per-path lock.
pub(crate) struct InMemory;

impl lore_revision::repository::InMemoryContext for InMemory {}
