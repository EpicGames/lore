// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use bytes::Bytes;
use futures::FutureExt;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::change::FileAction;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::instance::InstanceId;
use lore_revision::link::LinkFlags;
use lore_revision::lore::*;
use lore_revision::nametable::NameTable;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;
use lore_storage::hash;
use zerocopy::FromZeros;

mod apply;
mod attach;
mod chain_log;
mod diff;
mod discard;
mod hierarchy_walk;
mod link_target;
mod namespace;
mod pending;
mod relocate;
mod reparent;
mod save;
mod scoped_diff;
mod served;
mod stream;
mod sweep;
mod unserved;

/// Each action the walk settles on records a dirty flag and the staged flag standing
/// for the same change.
#[test]
fn an_action_records_matching_dirty_and_staged_flags() {
    use lore_revision::node::NodeFlags;
    use lore_revision::state::SettledAction;

    for (action, dirty, staged) in [
        (
            SettledAction::Add,
            NodeFlags::DirtyAdd,
            NodeFlags::StagedAdd,
        ),
        (
            SettledAction::Modify,
            NodeFlags::DirtyModify,
            NodeFlags::StagedModify,
        ),
        (
            SettledAction::Move,
            NodeFlags::DirtyMove,
            NodeFlags::StagedMove,
        ),
        (
            SettledAction::Delete,
            NodeFlags::DirtyDelete,
            NodeFlags::StagedDelete,
        ),
    ] {
        assert_eq!(dirty, action.dirty(), "dirty flag for {action:?}");
        assert_eq!(staged, action.staged(), "staged flag for {action:?}");
    }
}

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::state::push_distinct;
use lore_revision::state::*;
use lore_storage::FlushOptions;

use crate::fs::filesystem_provider::setup_test_execution;

/// The key every stored modification time is filed under. It has to stay the
/// digest over the path's own lowercase form, or a scan finds nothing it wrote
/// and rehashes every file in the repository.
fn mtime_key_reference(salt: &[u8], instance: InstanceId, path: &RelativePath) -> Hash {
    hash::hash_function_args_slice(
        salt,
        FILE_MTIME,
        instance.data(),
        path.as_str().to_lowercase().as_bytes(),
    )
}

/// Taking the fold [`RelativePath`] carries is only the same key as folding the
/// path here where the two folds agree, which above ASCII is not free: the
/// mapping can change a component's length and can depend on where in a word a
/// character falls.
#[test]
fn the_mtime_key_is_the_digest_over_the_paths_own_lowercase_form() {
    let salt = b"lore";
    let instance = InstanceId::default();
    for name in [
        "Rock.mesh",
        "Assets/Meshes/ROCK.MESH",
        "MIXED_Case-123/PATH/To.TXT",
        // Above ASCII: a fold that changes the length, one that depends on
        // position in a word, and one of each across a separator.
        "\u{0130}stanbul/Map.umap",
        "\u{039f}\u{0394}\u{039f}\u{03a3}",
        "\u{039f}\u{0394}\u{039f}\u{03a3}/Stra\u{00df}e/\u{1e9e}.uasset",
    ] {
        let path = RelativePath::new_from_initial_path(name).expect("a clean relative path");
        assert_eq!(
            file_modified_time_key(salt, instance, &path),
            mtime_key_reference(salt, instance, &path),
            "{name:?}"
        );
    }
}

/// A path built up a component at a time is what the clone and walk paths hand
/// in, and it folds each component as it is appended rather than the whole.
#[test]
fn a_pushed_path_keys_the_same_as_the_whole_of_it() {
    let salt = b"lore";
    let instance = InstanceId::default();
    let mut buf = RelativePathBuf::new();
    buf.push("\u{039f}\u{0394}\u{039f}\u{03a3}");
    buf.push("Stra\u{00df}E");
    buf.push("\u{0130}.uasset");
    let pushed = buf.freeze();
    assert_eq!(
        file_modified_time_key(salt, instance, &pushed),
        mtime_key_reference(salt, instance, &pushed)
    );
}

/// The lowercase form carries offsets of its own, since a fold can change a
/// component's byte length, so a path narrowed to a suffix has to key as that
/// suffix and not as a window into the wrong one.
#[test]
fn a_path_narrowed_to_a_suffix_keys_as_that_suffix() {
    let salt = b"lore";
    let instance = InstanceId::default();
    let mut narrowed = RelativePath::new_from_initial_path("\u{0130}\u{0130}/Assets/Rock.mesh")
        .expect("a clean relative path");
    narrowed.pop_root();
    assert_eq!(narrowed.as_str(), "Assets/Rock.mesh");
    let whole =
        RelativePath::new_from_initial_path("Assets/Rock.mesh").expect("a clean relative path");
    assert_eq!(
        file_modified_time_key(salt, instance, &narrowed),
        file_modified_time_key(salt, instance, &whole)
    );
    assert_eq!(
        file_modified_time_key(salt, instance, &narrowed),
        mtime_key_reference(salt, instance, &narrowed)
    );
}

#[test]
fn resolve_branch_returns_parent_when_branch_is_zero() {
    let link_ref = LinkReference {
        branch: BranchId::default(),
        ..LinkReference::default()
    };
    let parent = BranchId::from([1u8; 16]);
    assert_eq!(link_ref.resolve_branch(parent), parent);
}

#[test]
fn resolve_branch_returns_own_branch_when_non_zero() {
    let own_branch = BranchId::from([2u8; 16]);
    let link_ref = LinkReference {
        branch: own_branch,
        ..LinkReference::default()
    };
    let parent = BranchId::from([1u8; 16]);
    assert_eq!(link_ref.resolve_branch(parent), own_branch);
}

/// An auto-following link resolving to its parent's branch stores zero, whatever it stored
/// before. Another branch, or a link that does not follow, keeps the branch.
#[test]
fn branch_to_store_is_zero_only_for_an_auto_following_link_on_the_parent_branch() {
    let parent = BranchId::from([1u8; 16]);
    let other = BranchId::from([2u8; 16]);
    let follows = LinkFlags::NoFlags.bits();
    let pinned = LinkFlags::DisableAutoFollow.bits();
    for (stored, flags, branch, expected) in [
        (BranchId::default(), follows, parent, BranchId::default()),
        (parent, follows, parent, BranchId::default()),
        (other, follows, parent, BranchId::default()),
        (BranchId::default(), follows, other, other),
        (parent, pinned, parent, parent),
    ] {
        let link_ref = LinkReference {
            branch: stored,
            flags,
            ..LinkReference::default()
        };
        assert_eq!(
            link_ref.branch_to_store(branch, parent),
            expected,
            "stored {stored}, flags {flags}, resolving to {branch}"
        );
    }
}

/// A state with no serialized link list, so the registry lives only in the runtime copy and
/// every read has to come from there.
async fn null_repository() -> Arc<RepositoryContext> {
    null_repository_excluding(&[]).await
}

/// [`null_repository`] whose ignore filter excludes `globs`.
async fn null_repository_excluding(globs: &[&str]) -> Arc<RepositoryContext> {
    null_repository_filtering(globs, &[]).await
}

/// [`null_repository`] whose view excludes `globs`.
async fn null_repository_viewing(globs: &[&str]) -> Arc<RepositoryContext> {
    null_repository_filtering(&[], globs).await
}

/// [`null_repository`] whose ignore filter excludes `ignore` and whose view excludes `view`.
async fn null_repository_filtering(ignore: &[&str], view: &[&str]) -> Arc<RepositoryContext> {
    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");

    let mut filter = lore_revision::filter::Filter::default();
    for glob in ignore {
        filter.ignore.add_exclusion(glob).expect("ignore exclusion");
    }
    for glob in view {
        filter.view.add_exclusion(glob).expect("view exclusion");
    }

    let mut context = RepositoryContext::new_null_context(immutable_store, mutable_store);
    context.filter = Arc::new(filter);
    Arc::new(context)
}

/// Runs `body` against a [`null_repository`], inside the execution scope the State's edits read
/// their globals from.
async fn with_repository<F, Fut>(body: F)
where
    F: FnOnce(Arc<RepositoryContext>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let repository = null_repository().await;
    LORE_CONTEXT
        .scope(setup_test_execution(), body(repository))
        .await;
}

/// Runs `body` against a fresh State over a [`null_repository`], as [`with_repository`] does.
async fn with_state<F, Fut>(body: F)
where
    F: FnOnce(Arc<RepositoryContext>, Arc<State>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    with_repository(|repository| body(repository, State::new())).await;
}

/// A directory record named `name`.
fn directory(name: &str) -> Node {
    Node {
        name_hash: hash::hash_string(name),
        ..Default::default()
    }
}

/// A file record named `name`.
fn file(name: &str) -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash::hash_string(name),
        ..Default::default()
    }
}

/// Adds `node` under `parent` as `name`.
async fn add(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    node: Node,
    name: &str,
) -> NodeID {
    state
        .node_add(repository.clone(), parent, node, name)
        .await
        .expect("adding a node succeeds")
}

/// Adds files below `dir` until block `block` is full, answering them.
async fn fill(
    state: &State,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
    block: usize,
) -> Vec<NodeID> {
    let mut files = Vec::new();
    for index in 0.. {
        let name = format!("f{index}");
        let id = add(state, repository, dir, file(&name), &name).await;
        files.push(id);
        if NodeBlock::index(id) == block && Node::index(id) == BLOCK_NODE_COUNT - 1 {
            break;
        }
    }
    files
}

/// `node`'s record.
async fn record(state: &State, repository: &Arc<RepositoryContext>, node: NodeID) -> Node {
    state
        .node(repository.clone(), node)
        .await
        .expect("the node reads")
}

/// `state` saved to the in-memory store and loaded again: no block is in memory but the ones
/// the load reads, and none is recorded changed.
async fn reload(state: &Arc<State>, repository: &Arc<RepositoryContext>) -> Arc<State> {
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let signature = state
        .serialize(repository.clone(), &token)
        .await
        .expect("the State serializes");
    State::deserialize(repository.clone(), signature)
        .await
        .expect("the State loads")
}

/// A sibling chain that leaves a block and comes back to it holds the block once, so its
/// name table loads once.
#[test]
fn a_chain_returning_to_a_block_holds_it_once() {
    let (first, second) = (NodeBlock::new_zeroed(), NodeBlock::new_zeroed());
    let mut blocks = Vec::new();
    let pushed: Vec<bool> = [&first, &first, &second, &first, &second]
        .into_iter()
        .map(|block| push_distinct(&mut blocks, block))
        .collect();
    assert_eq!(pushed, vec![true, false, true, false, false]);
    assert_eq!(blocks.len(), 2);
}

struct InMemoryMarker;
impl lore_revision::repository::InMemoryContext for InMemoryMarker {}

/// Sets `slot` of file metadata block `block_index` to `value`, as a metadata set does.
async fn set_file_metadata(
    state: &State,
    repository: &Arc<RepositoryContext>,
    block_index: usize,
    slot: usize,
    value: Hash,
) {
    let block = state
        .block_file_metadata(repository.clone(), block_index)
        .await
        .expect("file metadata block");
    let dirtied = {
        let mut writer = block.write();
        writer.node(slot).metadata = value;
        writer.mark_dirty()
    };
    if dirtied {
        state.block_file_metadata_modified(block, block_index);
    }
    state.mark_dirty();
}

async fn file_metadata(
    state: &State,
    repository: &Arc<RepositoryContext>,
    block_index: usize,
    slot: usize,
) -> Hash {
    let block = state
        .block_file_metadata(repository.clone(), block_index)
        .await
        .expect("file metadata block");
    block.read().node(slot).metadata
}

/// A State that serialized a file metadata block list reads that list for a block added
/// after it, never the list it was loaded with.
#[tokio::test]
async fn a_block_added_after_a_serialize_reads_the_serialized_file_metadata_list() {
    let repository = null_repository().await;
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let (old, new) = (Hash::from([1u8; 32]), Hash::from([2u8; 32]));
    let first = State::new();
    set_file_metadata(&first, &repository, 0, 1, old).await;
    let loaded = first
        .serialize(repository.clone(), &token)
        .await
        .expect("the first State serializes");

    let state = State::deserialize(repository.clone(), loaded)
        .await
        .expect("the State loads");
    assert_eq!(file_metadata(&state, &repository, 0, 1).await, old);
    set_file_metadata(&state, &repository, 0, 1, new).await;
    state
        .serialize(repository.clone(), &token)
        .await
        .expect("the State serializes its edit");

    state
        .block(repository.clone(), 0)
        .await
        .expect("the first node block");
    state.allocate_fresh_block().expect("a second node block");
    state
        .block_file_metadata(repository.clone(), 1)
        .await
        .expect("the second block's file metadata");
    assert_eq!(
        file_metadata(&state, &repository, 0, 1).await,
        new,
        "the first block reads what the last serialize wrote"
    );
}

/// A store that holds every get of the hashes it is told to hold until released, answers every get
/// of the hashes it is told to refuse as missing, and counts every get.
pub(crate) struct HoldingStore {
    inner: Arc<dyn lore_storage::ImmutableStore>,
    held: std::sync::Mutex<Vec<Hash>>,
    refused: std::sync::Mutex<Vec<Hash>>,
    released: tokio::sync::Notify,
    gets: std::sync::atomic::AtomicUsize,
}

/// A store over `backing`'s holding nothing yet, and a repository reading through it.
async fn holding_repository(
    backing: &Arc<RepositoryContext>,
) -> (Arc<HoldingStore>, Arc<RepositoryContext>) {
    let store = HoldingStore::over(backing.immutable_store());
    let mutable = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        store.clone(),
    )
    .await
    .expect("in-memory mutable store");
    let repository = Arc::new(RepositoryContext::new_null_context(store.clone(), mutable));
    (store, repository)
}

impl HoldingStore {
    /// A store over `inner`, holding and refusing nothing yet.
    pub(crate) fn over(inner: Arc<dyn lore_storage::ImmutableStore>) -> Arc<Self> {
        Arc::new(HoldingStore {
            inner,
            held: std::sync::Mutex::new(Vec::new()),
            refused: std::sync::Mutex::new(Vec::new()),
            released: tokio::sync::Notify::new(),
            gets: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Answers every later get of `hash` as missing.
    pub(crate) fn refuse(&self, hash: Hash) {
        self.refused.lock().expect("refused hashes").push(hash);
    }

    fn gets(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn hold(&self, hash: Hash) {
        self.held.lock().expect("held hashes").push(hash);
    }

    fn release(&self) {
        self.held.lock().expect("held hashes").clear();
        self.released.notify_waiters();
    }

    async fn wait_while_held(&self, hash: &Hash) {
        loop {
            let released = self.released.notified();
            if !self.held.lock().expect("held hashes").contains(hash) {
                return;
            }
            released.await;
        }
    }
}

#[async_trait::async_trait]
impl lore_storage::ImmutableStore for HoldingStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, lore_storage::StoreError> {
        self.inner.clone().hold_for_command().await
    }

    async fn get(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        address: Address,
    ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .refused
            .lock()
            .expect("refused hashes")
            .contains(&address.hash)
        {
            return Err(lore_storage::StoreError::from(lore_base::error::NotFound));
        }
        self.wait_while_held(&address.hash).await;
        self.inner.clone().get(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        addresses: &[Address],
        results: &mut [lore_storage::StoreMatchResult],
    ) -> Result<(), lore_storage::StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        address: Address,
    ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        address: Address,
        fragment: lore_base::types::Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), lore_storage::StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        address: Address,
        stats: Arc<lore_storage::StoreObliterateStats>,
    ) -> Result<(), lore_storage::StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, lore_storage::StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, lore_storage::StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), lore_storage::StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), lore_storage::StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: lore_base::types::Partition,
        source_address: Address,
        destination_partition: lore_base::types::Partition,
        destination_context: lore_base::types::Context,
        behavior: lore_storage::immutable_store::CopyBehavior,
    ) -> Result<(), lore_storage::StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

/// Loading a node block of a State whose tree records file metadata leaves no task holding
/// the repository: once the State is dropped, the caller holds the only reference, even
/// while the store holds every read of the file metadata.
#[tokio::test]
async fn loading_a_node_block_leaves_no_task_holding_the_repository() {
    let backing = null_repository().await;
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let first = State::new();
    add_dirty_node(&first, backing.clone(), ROOT_NODE, "a.txt", NodeFlags::File).await;
    set_file_metadata(&first, &backing, 0, 1, Hash::from([1u8; 32])).await;
    let stored = first
        .serialize(backing.clone(), &token)
        .await
        .expect("the State serializes");

    let (store, repository) = holding_repository(&backing).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    let tree = state.tree(repository.clone()).await.expect("the tree");
    assert!(
        !tree.hash_file_metadata.is_zero(),
        "the tree records file metadata"
    );
    store.hold(tree.hash_file_metadata);
    state
        .block(repository.clone(), 0)
        .await
        .expect("the node block");
    drop(state);
    let holders = Arc::strong_count(&repository);
    store.release();
    assert_eq!(
        holders, 1,
        "a task the block load started holds the repository"
    );
}

/// Files under `fill/` in [`stored_with_free_slots`], running past the first node block.
const FILL_NODES: usize = 600;

/// A stored revision of two node blocks, recording file metadata for both: the root holds
/// `a.txt` and `fill/` in the first block, and `fill/` holds [`FILL_NODES`] files running into
/// the second, where the tree's unused chain starts, with free slots.
async fn stored_with_free_slots(repository: &Arc<RepositoryContext>) -> Hash {
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let state = State::new();
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "a.txt",
        NodeFlags::File,
    )
    .await;
    let fill = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "fill",
        NodeFlags::NoFlags,
    )
    .await;
    for index in 0..FILL_NODES {
        let name = format!("f{index}");
        add_dirty_node(&state, repository.clone(), fill, &name, NodeFlags::File).await;
    }
    for block_index in 0..2 {
        set_file_metadata(&state, repository, block_index, 1, Hash::from([1u8; 32])).await;
    }
    state
        .serialize(repository.clone(), &token)
        .await
        .expect("the State serializes")
}

/// A State with no node block in memory puts a fresh block past the tree's blocks, its block
/// count grows by one rather than shrinking to the fresh block's, and serializing it keeps the
/// blocks it never loaded.
#[tokio::test]
async fn a_fresh_block_of_a_state_with_no_block_in_memory_goes_past_the_trees_blocks() {
    let repository = null_repository().await;
    let stored = stored_with_free_slots(&repository).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    let tree = state.tree(repository.clone()).await.expect("the tree");
    let count = state.block_count();
    assert_eq!(count, tree.block_count as usize);
    assert_eq!(count, 2, "the tree holds two blocks");

    let (index, block) = state.allocate_fresh_block().expect("a fresh block");
    assert_eq!(index, count, "the fresh block goes past the tree's blocks");
    assert_eq!(
        state.block_count(),
        count + 1,
        "the block count grows by one"
    );
    assert!(
        state
            .resident_block(index)
            .is_some_and(|resident| Arc::ptr_eq(&resident, &block)),
        "the fresh block is found at its index"
    );

    state.mark_dirty();
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let reserialized = state
        .serialize(repository.clone(), &token)
        .await
        .expect("the State serializes");
    let reloaded = State::deserialize(repository.clone(), reserialized)
        .await
        .expect("the State reloads");
    let fill = reloaded
        .find_subnode(repository.clone(), ROOT_NODE, hash::hash_string("fill"))
        .await
        .expect("the root still holds fill");
    assert_eq!(
        reloaded
            .node_children(repository, fill)
            .await
            .expect("fill's children")
            .len(),
        FILL_NODES,
        "fill still holds its files"
    );
}

/// A State taking new nodes from fresh blocks gives an attach a slot in a block past the
/// stored tree's, reading nothing from the store, though the stored unused chain starts at a
/// block with free slots, and that block, the last, is in memory without its name table or
/// file metadata.
#[tokio::test]
async fn an_attach_on_a_state_taking_fresh_blocks_reads_nothing_from_the_store() {
    let backing = null_repository().await;
    let stored = stored_with_free_slots(&backing).await;
    let (store, repository) = holding_repository(&backing).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    let tree = state.tree(repository.clone()).await.expect("the tree");
    assert_eq!(
        tree.block_unused_first, 1,
        "the chain starts at the last block"
    );
    state
        .take_new_nodes_from_fresh_blocks(repository.clone())
        .await
        .expect("the State takes fresh blocks");
    state
        .block_with_nametable(repository.clone(), 0)
        .await
        .expect("the root's block");
    let _last = state
        .block(repository.clone(), 1)
        .await
        .expect("the last stored block");

    let reads = store.gets();
    assert!(
        state.attach_slot_resident(),
        "the slot needs nothing from the store"
    );
    let attached = state
        .node_attach(
            repository.clone(),
            ROOT_NODE,
            dirty_node("new.txt", NodeFlags::File),
            "new.txt",
        )
        .await
        .expect("the attach");
    assert_eq!(
        store.gets(),
        reads,
        "the attach read nothing from the store"
    );
    let AttachOutcome::Attached(node_id) = attached else {
        panic!("the name is free: {attached:?}");
    };
    assert_eq!(NodeBlock::index(node_id), 2, "the node takes a fresh block");
}

/// On a State taking new nodes from fresh blocks, a node detached from a stored block and a
/// node then attached under its name leave the directory holding the name once, as the new
/// node, in a fresh block; the detached node stays discarded.
#[tokio::test]
async fn an_attach_after_a_detach_in_a_stored_block_holds_the_name_once() {
    let repository = null_repository().await;
    let stored = stored_with_free_slots(&repository).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    state
        .take_new_nodes_from_fresh_blocks(repository.clone())
        .await
        .expect("the State takes fresh blocks");
    let name_hash = lore_storage::hash::hash_string("a.txt");
    let old = state
        .find_subnode(repository.clone(), ROOT_NODE, name_hash)
        .await
        .expect("a.txt");
    state
        .node_detach(
            repository.clone(),
            old,
            lore_revision::state::Detach::Always,
        )
        .await
        .expect("the detach");

    let attached = state
        .node_attach(
            repository.clone(),
            ROOT_NODE,
            dirty_node("a.txt", NodeFlags::File),
            "a.txt",
        )
        .await
        .expect("the attach");
    let AttachOutcome::Attached(new) = attached else {
        panic!("the detached name is free: {attached:?}");
    };
    assert_eq!(NodeBlock::index(new), 2, "the node takes a fresh block");
    assert_eq!(
        state
            .find_subnode(repository.clone(), ROOT_NODE, name_hash)
            .await
            .expect("a.txt again"),
        new
    );
    let children = state
        .node_children(repository.clone(), ROOT_NODE)
        .await
        .expect("the root's children");
    assert!(children.contains(&new) && !children.contains(&old));
    assert_eq!(children.len(), 2, "a.txt and fill/");
    assert!(
        state
            .node(repository.clone(), old)
            .await
            .expect("the detached slot")
            .is_discarded()
    );
}

/// On a State taking new nodes from fresh blocks, the attach after the one that takes a fresh
/// block's last slot needs nothing from the store either: the full block leaves the chain and
/// the slot comes from the next fresh block.
#[tokio::test]
async fn an_attach_past_a_full_fresh_block_reads_nothing_from_the_store() {
    let backing = null_repository().await;
    let stored = stored_with_free_slots(&backing).await;
    let (store, repository) = holding_repository(&backing).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    state
        .take_new_nodes_from_fresh_blocks(repository.clone())
        .await
        .expect("the State takes fresh blocks");
    state
        .block_with_nametable(repository.clone(), 0)
        .await
        .expect("the root's block");
    let attach = async |name: &str| {
        state
            .node_attach_resident(
                repository.clone(),
                ROOT_NODE,
                dirty_node(name, NodeFlags::File),
                name,
                None,
            )
            .await
            .expect("the attach")
    };
    for index in 0..BLOCK_NODE_COUNT {
        let name = format!("n{index}");
        let attached = attach(&name).await;
        assert!(
            matches!(attached, Some(AttachOutcome::Attached(node)) if NodeBlock::index(node) == 2),
            "{name} fills the first fresh block: {attached:?}"
        );
    }

    let reads = store.gets();
    assert!(
        state.attach_slot_resident(),
        "the slot past the full fresh block needs nothing from the store"
    );
    let attached = attach("past").await;
    assert_eq!(
        store.gets(),
        reads,
        "the attach read nothing from the store"
    );
    assert!(
        matches!(attached, Some(AttachOutcome::Attached(node)) if NodeBlock::index(node) == 3),
        "the node takes the next fresh block: {attached:?}"
    );
}

/// A find by name through blocks in memory only reads nothing from the store: it answers `None`
/// where the parent's block or the chain's runs into one not in memory, and otherwise answers as
/// a find reading blocks does, a detached directory's included.
#[tokio::test]
async fn a_find_through_blocks_in_memory_reads_nothing_from_the_store() {
    let backing = null_repository().await;
    let stored = stored_with_free_slots(&backing).await;
    let (store, repository) = holding_repository(&backing).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    let _root = state
        .block(repository.clone(), 0)
        .await
        .expect("the root's block");
    let fill = state
        .find_subnode(repository.clone(), ROOT_NODE, hash::hash_string("fill"))
        .await
        .expect("fill");
    assert!(
        state.resident_block(1).is_none(),
        "fill's newest files lie in a block not in memory"
    );

    let reads = store.gets();
    assert_eq!(
        state
            .find_subnode_resident(ROOT_NODE, hash::hash_string("fill"))
            .expect("the find"),
        Some(fill)
    );
    assert!(
        state
            .find_subnode_resident(ROOT_NODE, hash::hash_string("missing"))
            .is_err_and(|error| error.is_node_not_found())
    );
    assert_eq!(
        state
            .find_subnode_resident(fill, hash::hash_string("f0"))
            .expect("the find"),
        None,
        "the chain runs into a block not in memory"
    );
    assert_eq!(
        state
            .find_subnode_resident(BLOCK_NODE_COUNT as NodeID, hash::hash_string("f0"))
            .expect("the find"),
        None,
        "the parent's own block is not in memory"
    );
    assert_eq!(store.gets(), reads, "the finds read nothing from the store");

    let _last = state
        .block(repository.clone(), 1)
        .await
        .expect("the last block");
    let reads = store.gets();
    let f0 = state
        .find_subnode_resident(fill, hash::hash_string("f0"))
        .expect("the find");
    assert_eq!(
        f0,
        Some(
            state
                .find_subnode(repository.clone(), fill, hash::hash_string("f0"))
                .await
                .expect("f0")
        )
    );
    assert!(
        state
            .find_subnode_resident(fill, hash::hash_string("missing"))
            .is_err_and(|error| error.is_node_not_found())
    );
    assert!(
        state
            .find_subnode_resident(f0.expect("f0"), hash::hash_string("x"))
            .is_err_and(|error| error.is_node_not_found()),
        "a file holds no names"
    );
    assert_eq!(store.gets(), reads, "the finds read nothing from the store");

    state
        .node_detach(repository.clone(), fill, Detach::Always)
        .await
        .expect("detach fill");
    let reads = store.gets();
    let resident = state
        .find_subnode_resident(fill, hash::hash_string("f0"))
        .map_err(|error| error.is_node_not_found());
    assert_eq!(store.gets(), reads, "the find read nothing from the store");
    let read = state
        .find_subnode(repository.clone(), fill, hash::hash_string("f0"))
        .await
        .map(Some)
        .map_err(|error| error.is_node_not_found());
    assert_eq!(
        resident, read,
        "a detached directory reads as a find reading blocks reads it"
    );
}

/// A find through blocks in memory counts the chain steps it takes, as a find reading blocks
/// does: `b`, at the head of `dir`'s chain, then `a`.
#[tokio::test]
async fn a_find_through_blocks_in_memory_counts_its_steps() {
    let repository = null_repository().await;
    let state = State::new();
    let dir = add_directory(&state, &repository, ROOT_NODE, "dir").await;
    let a = add_directory(&state, &repository, dir, "a").await;
    add_directory(&state, &repository, dir, "b").await;
    let before = counted_steps();
    let found = state
        .find_subnode_resident(dir, hash::hash_string("a"))
        .expect("the find");
    assert_eq!(found, Some(a));
    assert_eq!(
        step_counting::thread_chain_steps() - before,
        2,
        "the find counted 'b' and 'a'"
    );
}

/// Serializing a State that takes new nodes from fresh blocks panics in a debug build.
#[cfg(debug_assertions)]
#[tokio::test]
async fn serializing_a_state_taking_fresh_blocks_panics() {
    let repository = null_repository().await;
    let stored = stored_with_free_slots(&repository).await;
    let state = State::deserialize(repository.clone(), stored)
        .await
        .expect("the State loads");
    state
        .take_new_nodes_from_fresh_blocks(repository.clone())
        .await
        .expect("the State takes fresh blocks");
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "new.txt",
        NodeFlags::File,
    )
    .await;
    let token = RepositoryWriteToken::in_memory(&InMemoryMarker);
    let serialized = AssertUnwindSafe(state.serialize(repository, &token))
        .catch_unwind()
        .await;
    assert!(serialized.is_err(), "the serialize panics");
}

/// A repository with a working tree, so a file on disk can be measured against a node.
async fn working_tree_repository(path: &std::path::Path) -> Arc<RepositoryContext> {
    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");

    // Built with the path rather than assigned one afterwards, so the filesystem provider is
    // rooted where the working tree is.
    use crate::helper::RepositoryContextCreationArgsExt;
    Arc::new(RepositoryContext::new(
        crate::helper::default_repository_creation_args(immutable_store, mutable_store)
            .with_path(path),
    ))
}

fn pseudo_random_bytes(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

fn content_node(content: &[u8]) -> Node {
    let mut node = Node::new_zeroed();
    node.address = Address::zero_context_hash(Hash::hash_buffer(content));
    node.size = content.len() as u64;
    node
}

/// A file written into the working tree, and the path it is known by.
async fn write_working_file(
    repository: &Arc<RepositoryContext>,
    name: &str,
    content: &[u8],
) -> RelativePath {
    let path = lore_revision::util::path::RelativePathBuf::new().push_and_freeze(name);
    lore_io::IoDriver::global()
        .write_file_bytes(
            path.to_absolute_path(repository.require_path().expect("working tree"))
                .expect("a path below the root"),
            bytes::Bytes::copy_from_slice(content),
            false,
        )
        .await
        .expect("write working file");
    path
}

/// An operation on the repository's own filesystem, which is what every caller compares
/// through.
async fn working_operation(repository: &RepositoryContext) -> Arc<InstanceOperationImpl> {
    repository
        .file_system()
        .begin_operation()
        .await
        .expect("beginning an operation")
}

/// What one comparison established answers the next, which is what spares a file measured
/// against several addresses being read once for each.
///
/// The file is removed between the two: a comparison that reached the working tree again
/// would find nothing there and report it unreadable.
#[tokio::test]
async fn what_one_comparison_established_answers_the_next() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(4 * 1024);
    let path = write_working_file(&repository, "established.bin", &content).await;
    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();

    let mut first = content_node(&content);
    first.address = Address::zero_context_hash(Hash::hash_buffer(b"one address"));
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &first,
            first.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Differs
    ));

    lore_io::IoDriver::global()
        .remove_file(
            path.to_absolute_path(repository.require_path().expect("working tree"))
                .expect("a path below the root"),
        )
        .await
        .expect("remove working file");

    let mut second = content_node(&content);
    second.address = Address::zero_context_hash(Hash::hash_buffer(b"another address"));
    assert!(matches!(
        file_matches_node(
            repository,
            &second,
            second.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("an established comparison reads nothing"),
        NodeComparison::Differs
    ));
}

/// A file written right after [`time_before_next_write`] carries a later time, so a time recorded
/// for content served from elsewhere never matches the file a later write puts at the path.
/// Each round can write within the millisecond the time was taken in. The probe directory is
/// in no working tree and holds nothing afterward.
#[tokio::test]
async fn a_file_written_after_the_time_before_the_next_write_carries_a_later_time() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let probe = dir.child("probe");
    let driver = lore_io::IoDriver::global();
    driver
        .create_dir_all(&probe)
        .await
        .expect("create the probe directory");
    let file = dir.path().join("written.bin");
    for round in 0..64u8 {
        let before = time_before_next_write(Some(&probe)).await;
        driver
            .write_file_bytes(&file, Bytes::copy_from_slice(&[round]), false)
            .await
            .expect("write the file");
        let written = lore_revision::util::fs::modified_millis(
            driver.stat(&file).await.expect("stat the file").modified,
        );
        assert!(
            written > before,
            "round {round}: written at {written}, recorded {before}"
        );
    }
    assert_eq!(
        std::fs::read_dir(&probe)
            .expect("list the probe directory")
            .count(),
        0,
        "the probe is removed"
    );
}

/// A directory that cannot be stamped, or none, gives the process clock.
#[tokio::test]
async fn the_time_before_the_next_write_is_the_process_clock_without_a_probe() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let missing = dir.child("missing");
    for probe in [None, Some(missing.as_path())] {
        let before = lore_revision::util::fs::modified_millis(None);
        let time = time_before_next_write(probe).await;
        let after = lore_revision::util::fs::modified_millis(None);
        assert!(
            (before..=after).contains(&time),
            "{probe:?}: {time} not in {before}..={after}"
        );
    }
}

/// A file removed under a run of comparisons reads as unreadable, not as differing.
///
/// Nothing established stands in for the file being there, so the size is measured afresh for
/// every comparison. Reported unmodified, a routine deletion does not read as local work.
#[tokio::test]
async fn a_file_removed_between_comparisons_reads_as_unreadable() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    // Above the minimum cut, so no established hash can answer on its own.
    let content = pseudo_random_bytes(100 * 1024);
    let path = write_working_file(&repository, "removed.bin", &content).await;
    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();

    let mut first = content_node(&content);
    first.address = Address::zero_context_hash(Hash::hash_buffer(b"one address"));
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &first,
            first.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Differs
    ));

    lore_io::IoDriver::global()
        .remove_file(
            path.to_absolute_path(repository.require_path().expect("working tree"))
                .expect("a path below the root"),
        )
        .await
        .expect("remove working file");

    let mut second = content_node(&content);
    second.address = Address::zero_context_hash(Hash::hash_buffer(b"another address"));
    assert!(matches!(
        file_matches_node(
            repository,
            &second,
            second.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("a removed file is not an error"),
        NodeComparison::Unreadable
    ));
}

/// A comparison that settles nothing has to read as modified. The address names a list
/// nothing stored, so neither the stored chunking nor a rehash can answer, and
/// overwriting a file that may hold local work is the one outcome there is no
/// recovering from.
#[tokio::test]
async fn a_comparison_that_settles_nothing_reads_as_modified() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(150 * 1024);
    let path = write_working_file(&repository, "settles-nothing.bin", &content).await;

    let mut node = content_node(&content);
    node.address = Address::zero_context_hash(Hash::hash_buffer(b"a list nothing stored"));

    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &node,
            node.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Differs
    ));
    assert!(
        file_modification(
            repository,
            &node,
            1,
            node.size,
            &path,
            true,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error")
        .is_modified()
    );
}

/// Content addressed by its own hash needs nothing from the store to be decided,
/// either way.
#[tokio::test]
async fn an_unfragmented_file_is_decided_by_its_own_hash() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(20 * 1024);
    let path = write_working_file(&repository, "unfragmented.bin", &content).await;
    let node = content_node(&content);

    let operation = working_operation(&repository).await;
    let established = lore_storage::ContentHashes::default();
    assert!(matches!(
        file_matches_node(
            repository.clone(),
            &node,
            node.size,
            &path,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error"),
        NodeComparison::Matches
    ));

    let mut edited = content.clone();
    edited[content.len() / 2] ^= 0xff;
    lore_io::IoDriver::global()
        .write_file_bytes(
            path.to_absolute_path(repository.require_path().expect("working tree"))
                .expect("a path below the root"),
            bytes::Bytes::from(edited),
            false,
        )
        .await
        .expect("rewrite working file");

    // What the first comparison established answers for the content it read, so the rewrite
    // starts again.
    let established = lore_storage::ContentHashes::default();
    assert!(
        file_modification(
            repository,
            &node,
            1,
            node.size,
            &path,
            true,
            &operation,
            &established,
        )
        .await
        .expect("comparing a readable file must not error")
        .is_modified()
    );
}

/// A file of another size is modified without the file being read at all.
#[tokio::test]
async fn a_file_of_another_size_is_modified_unread() {
    let dir = lore_base::test_util::TempDir::new("lore-state-test-");
    let repository = working_tree_repository(dir.path()).await;
    let content = pseudo_random_bytes(20 * 1024);
    let path = write_working_file(&repository, "resized.bin", &content).await;
    let node = content_node(&content[..content.len() - 1]);

    let operation = working_operation(&repository).await;
    assert!(matches!(
        file_modification(
            repository,
            &node,
            1,
            content.len() as u64,
            &path,
            false,
            &operation,
            &lore_storage::ContentHashes::default(),
        )
        .await
        .expect("comparing a readable file must not error"),
        FileModification::ModifiedBySize
    ));
}

fn link_id(byte: u8) -> RepositoryId {
    RepositoryId::from([byte; 16])
}

/// An update made after the registry was read applies to the entry it names and leaves the
/// other entries as they were.
#[tokio::test]
async fn reading_the_link_list_leaves_it_editable() {
    let repository = null_repository().await;
    let state = State::new();

    state
        .link_add(
            repository.clone(),
            link_id(1),
            BranchId::default(),
            Hash::from([1u8; 32]),
            2,
            LinkFlags::NoFlags,
        )
        .await
        .expect("adding the first link");
    state
        .link_add(
            repository.clone(),
            link_id(2),
            BranchId::default(),
            Hash::from([2u8; 32]),
            3,
            LinkFlags::NoFlags,
        )
        .await
        .expect("adding the second link");

    let read = state
        .link_list(repository.clone())
        .await
        .expect("reading the registry");
    assert_eq!(read.len(), 2, "both links must be registered");

    state
        .link_update(
            repository.clone(),
            link_id(1),
            BranchId::default(),
            Hash::from([9u8; 32]),
            2,
        )
        .await
        .expect("updating a link after the registry was read");

    let updated = state
        .link_list(repository)
        .await
        .expect("re-reading the registry");
    assert_eq!(updated.len(), 2, "the update must not drop the other link");
    assert_eq!(
        updated[0].signature,
        Hash::from([9u8; 32]),
        "the update must be visible"
    );
    assert_eq!(
        updated[1].signature,
        Hash::from([2u8; 32]),
        "the untouched link must keep its signature"
    );
}

/// A node carrying `flags`, named for the name table by its hash.
fn dirty_node(name: &str, flags: NodeFlags) -> Node {
    Node {
        name_hash: lore_storage::hash::hash_string(name),
        flags: flags.bits(),
        ..Default::default()
    }
}

/// Adds `name` under `parent` with `flags` and returns it.
async fn add_dirty_node(
    state: &State,
    repository: Arc<RepositoryContext>,
    parent: NodeID,
    name: &str,
    flags: NodeFlags,
) -> NodeID {
    state
        .node_add(repository, parent, dirty_node(name, flags), name)
        .await
        .expect("adding a node to the walked tree")
}

/// The dirty paths of the whole tree, sorted, so the assertions do not depend
/// on the order the sibling chain happens to hold.
async fn walk_dirty_paths(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    options: DirtyWalkOptions,
) -> Vec<String> {
    let mut collected = walk_dirty_paths_in_order(state, repository, options).await;
    collected.sort();
    collected
}

/// An empty name on a directory, which is descended, so the name is taken off
/// where its level ends rather than where the loop body does.
///
/// The directory appended nothing, so its level must take nothing off. A level
/// that popped unconditionally would remove its parent's own last component,
/// and the sibling walked next would be named against the wrong parent.
#[tokio::test]
async fn an_empty_directory_name_leaves_its_parent_on_the_path() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let outer = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "outer",
        NodeFlags::Dirty,
    )
    .await;
    // Added first so the prepending chain walks it last, after the empty name.
    add_dirty_node(&state, repository.clone(), outer, "after.txt", file).await;
    let unnamed = add_dirty_node(&state, repository.clone(), outer, "", NodeFlags::Dirty).await;
    add_dirty_node(&state, repository.clone(), unnamed, "inner.txt", file).await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec!["outer/inner.txt", "outer/after.txt"],
        "the sibling after an empty-named directory keeps its parent's prefix"
    );
}

/// `U+0130` folds to two scalars, so the directory is one byte longer in the
/// buffer's lowercase form than in its written one. The walk pops the whole
/// component off both, or the sibling that follows inherits what is left.
#[tokio::test]
async fn a_component_whose_fold_is_longer_is_popped_off_both_forms() {
    let repository = null_repository().await;
    let state = State::new();

    let folded = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "MESH_\u{130}",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        folded,
        "inner.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "after.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["MESH_\u{130}/inner.txt", "after.txt"],
        "the sibling is named against the root, not against what the fold left"
    );
}

/// A sibling chain longer than one node block, so the walk crosses a block
/// boundary part way along it.
///
/// [`BlockCursor`] holds the block it last read and only fetches another when
/// the node it is asked for is not in that one. A cursor that never moved
/// would read the wrong nodes from the block it opened on, so every chain a
/// test walks has to be long enough to leave it.
#[tokio::test]
async fn a_sibling_chain_spanning_node_blocks_is_walked_whole() {
    const CHILDREN: usize = lore_revision::node::BLOCK_NODE_COUNT + 200;

    let repository = null_repository().await;
    let state = State::new();
    let directory = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "spanning",
        NodeFlags::Dirty,
    )
    .await;
    for index in 0..CHILDREN {
        add_dirty_node(
            &state,
            repository.clone(),
            directory,
            &format!("file_{index:04}.txt"),
            NodeFlags::DirtyModify | NodeFlags::File,
        )
        .await;
    }

    let walked = walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await;
    let expected: Vec<String> = (0..CHILDREN)
        .map(|index| format!("spanning/file_{index:04}.txt"))
        .collect();
    assert_eq!(
        walked, expected,
        "every child is recorded once, whichever block it lives in"
    );
}

/// Points `node`'s sibling link at `sibling`, forging a chain no tree edit
/// produces.
async fn forge_sibling(
    state: &State,
    repository: Arc<RepositoryContext>,
    node: NodeID,
    sibling: NodeID,
) {
    let block = state
        .block(repository, NodeBlock::index(node))
        .await
        .expect("the block holding the node");
    block.write_for_tests().node(Node::index(node)).sibling = sibling;
}

/// The dirty paths under `parent_node`, in the order the walk records them.
async fn walk_dirty_paths_under(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parent_node: NodeID,
    options: DirtyWalkOptions,
) -> Result<Vec<String>, StateError> {
    let mut paths = Vec::new();
    collect_dirty_paths_inner(
        state,
        repository,
        parent_node,
        &mut RelativePathBuf::new(),
        &mut paths,
        options,
    )
    .await?;
    Ok(paths.iter().map(|p| p.as_str().to_string()).collect())
}

/// The dirty paths of the whole tree, in the order the walk records them.
async fn walk_dirty_paths_in_order(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    options: DirtyWalkOptions,
) -> Vec<String> {
    walk_dirty_paths_under(state, repository, ROOT_NODE, options)
        .await
        .expect("walking the tree")
}

/// Adding a node prepends it, so each level's chain is the reverse of the
/// order its children were added in here.
#[tokio::test]
async fn dirty_paths_are_recorded_depth_first_along_each_sibling_chain() {
    let repository = null_repository().await;
    let state = State::new();

    let gamma = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "gamma",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "beta.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let alpha = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "alpha",
        NodeFlags::DirtyModify,
    )
    .await;

    let deep = add_dirty_node(&state, repository.clone(), alpha, "deep", NodeFlags::Dirty).await;
    add_dirty_node(
        &state,
        repository.clone(),
        alpha,
        "one.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        deep,
        "two.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    add_dirty_node(
        &state,
        repository.clone(),
        gamma,
        "four.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        gamma,
        "three.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "alpha",
            "alpha/one.txt",
            "alpha/deep/two.txt",
            "beta.txt",
            "gamma/three.txt",
            "gamma/four.txt",
        ],
        "each subtree is recorded where its directory sits in its parent's chain"
    );
}

/// A new directory holding nodes is carried by them and records no path of its
/// own, since re-applied it would mark everything the working tree holds below
/// it, marked or not. An empty new directory has nothing to carry it, so it is
/// recorded.
#[tokio::test]
async fn a_new_directory_is_recorded_only_where_it_is_empty() {
    let repository = null_repository().await;
    let state = State::new();

    let added = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "added",
        NodeFlags::DirtyAdd,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        added,
        "leaf.txt",
        NodeFlags::DirtyAdd | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "empty",
        NodeFlags::DirtyAdd,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["added/leaf.txt", "empty"],
        "the new directory is carried by its file, and the empty one by itself"
    );
}

/// A directory that carries an action of its own and is also descended is
/// recorded before anything below it, so its action is re-applied before the
/// paths under it.
#[tokio::test]
async fn a_directory_is_recorded_before_the_paths_under_it() {
    let repository = null_repository().await;
    let state = State::new();

    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "later.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let added = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "added",
        NodeFlags::DirtyModify,
    )
    .await;
    let inner = add_dirty_node(
        &state,
        repository.clone(),
        added,
        "inner",
        NodeFlags::DirtyModify,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        inner,
        "leaf.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec!["added", "added/inner", "added/inner/leaf.txt", "later.txt"],
        "a directory precedes its descendants, and its whole subtree precedes its sibling"
    );
}

/// A child the walk passes over — clean, staged where staged paths are
/// skipped, or a directory with nothing dirty under it — leaves its level
/// walking: the siblings behind it are still visited, in chain order.
#[tokio::test]
async fn a_passed_over_child_does_not_end_its_sibling_chain() {
    let repository = null_repository().await;
    let state = State::new();

    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "last.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "empty",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "staged.txt",
        NodeFlags::DirtyModify | NodeFlags::StagedModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "clean.txt",
        NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "first.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths_under(
            state,
            repository,
            ROOT_NODE,
            DirtyWalkOptions {
                skip_staged: true,
                force: false
            }
        )
        .await
        .expect("walking the tree"),
        vec!["first.txt", "last.txt"],
        "the chain is walked past a clean child, a staged child and an empty directory"
    );
}

/// Descent costs a stack entry, not a frame, so nesting past the depth the
/// stack is sized for is walked whole and in constant stack.
#[tokio::test]
async fn a_path_nested_deeper_than_the_walk_stack_is_recorded_in_full() {
    const DEPTH: usize = 1024;

    let repository = null_repository().await;
    let state = State::new();

    let mut parent = ROOT_NODE;
    for level in 0..DEPTH {
        parent = add_dirty_node(
            &state,
            repository.clone(),
            parent,
            &format!("d{level}"),
            NodeFlags::Dirty,
        )
        .await;
    }
    add_dirty_node(
        &state,
        repository.clone(),
        parent,
        "leaf.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    let expected = (0..DEPTH)
        .map(|level| format!("d{level}"))
        .chain(std::iter::once("leaf.txt".to_string()))
        .collect::<Vec<String>>()
        .join("/");
    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec![expected],
        "the only dirty node is the leaf, named under all {DEPTH} levels above it"
    );
}

/// Every level guards its own sibling chain, so a cycle is reported against
/// the directory whose chain holds it and not against the walk's root.
#[tokio::test]
async fn a_sibling_cycle_below_the_root_is_reported_against_its_own_parent() {
    let repository = null_repository().await;
    let state = State::new();

    let sub = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "sub",
        NodeFlags::Dirty,
    )
    .await;
    let second = add_dirty_node(
        &state,
        repository.clone(),
        sub,
        "second.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let first = add_dirty_node(
        &state,
        repository.clone(),
        sub,
        "first.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    forge_sibling(&state, repository.clone(), second, first).await;

    let error = walk_dirty_paths_under(
        state,
        repository,
        ROOT_NODE,
        DirtyWalkOptions {
            skip_staged: false,
            force: false,
        },
    )
    .await
    .expect_err("a sibling chain that loops must be reported");
    let hierarchy = error
        .as_invalid_node_hierarchy()
        .unwrap_or_else(|| panic!("a looping chain is an invalid hierarchy, got {error}"));
    assert_eq!(
        hierarchy.expected_parent, sub,
        "the guard that tripped belongs to the level holding the cycle"
    );
}

/// Only a directory holds a chain to walk. A link's children live in another
/// repository's state and a file has none, so a walk rooted at either records
/// nothing, and from above they are recorded without being descended.
#[tokio::test]
async fn a_link_or_file_walk_root_is_not_descended() {
    let repository = null_repository().await;
    let state = State::new();

    let link = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "link",
        NodeFlags::DirtyModify | NodeFlags::Link,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        link,
        "linked.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let file = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "file.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        file,
        "under.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert!(
        walk_dirty_paths_under(
            state.clone(),
            repository.clone(),
            link,
            DirtyWalkOptions {
                skip_staged: false,
                force: false
            }
        )
        .await
        .expect("walking a link")
        .is_empty(),
        "a link is not descended"
    );
    assert!(
        walk_dirty_paths_under(
            state.clone(),
            repository.clone(),
            file,
            DirtyWalkOptions {
                skip_staged: false,
                force: false
            }
        )
        .await
        .expect("walking a file")
        .is_empty(),
        "a file is not descended"
    );
    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["file.txt", "link"],
        "both are recorded from above, neither is descended"
    );
}

/// A `DirtyDelete` or `DirtyMove` directory is re-applied whole, so the walk
/// records it and stops. Descending would collect descendants the parent
/// action already covers.
///
/// A directory that is merely propagated-dirty is the opposite case and is in
/// the same tree: it carries no action of its own, contributes no path, and
/// must still be descended to reach the file that made it dirty.
#[tokio::test]
async fn a_deleted_or_moved_directory_is_recorded_without_descending() {
    let repository = null_repository().await;
    let state = State::new();

    let removed = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "removed",
        NodeFlags::DirtyDelete,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        removed,
        "inside.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    let moved = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "moved",
        NodeFlags::DirtyMove,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        moved,
        "carried.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    let touched = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "touched",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        touched,
        "edited.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["moved", "removed", "touched/edited.txt"],
        "a deleted or moved directory is recorded and not descended, \
             and a propagated-dirty directory is descended and not recorded"
    );
}

/// The buffer the walk names into carries the whole ancestry, and a sibling
/// visited after a descent is named against its own parent again.
///
/// A child is prepended into the sibling chain, so the subdirectory added
/// last at each level is walked first and the file beside it is named once the
/// descent has returned.
#[tokio::test]
async fn a_nested_dirty_file_carries_the_whole_prefix() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let mut parent = ROOT_NODE;
    for (directory, name) in [
        ("assets", "a.txt"),
        ("meshes", "b.txt"),
        ("rock", "c.txt"),
        ("detail", "d.txt"),
    ] {
        parent = add_dirty_node(
            &state,
            repository.clone(),
            parent,
            directory,
            NodeFlags::Dirty,
        )
        .await;
        add_dirty_node(&state, repository.clone(), parent, name, file).await;
    }
    add_dirty_node(&state, repository.clone(), parent, "e.txt", file).await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "assets/a.txt",
            "assets/meshes/b.txt",
            "assets/meshes/rock/c.txt",
            "assets/meshes/rock/detail/d.txt",
            "assets/meshes/rock/detail/e.txt",
        ],
    );
}

/// Three subtrees under one parent, each of a different depth. Every path is
/// named against the parent and not against whatever the subtree walked
/// before it left behind.
#[tokio::test]
async fn sibling_subtrees_at_one_depth_are_named_against_their_parent() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    for (subtree, depth) in [("left", 1usize), ("middle", 2), ("right", 3)] {
        let mut parent = add_dirty_node(
            &state,
            repository.clone(),
            ROOT_NODE,
            subtree,
            NodeFlags::Dirty,
        )
        .await;
        for level in 0..depth {
            add_dirty_node(&state, repository.clone(), parent, "leaf.txt", file).await;
            parent = add_dirty_node(
                &state,
                repository.clone(),
                parent,
                &format!("level_{level}"),
                NodeFlags::Dirty,
            )
            .await;
        }
        add_dirty_node(&state, repository.clone(), parent, "leaf.txt", file).await;
    }

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "left/leaf.txt",
            "left/level_0/leaf.txt",
            "middle/leaf.txt",
            "middle/level_0/leaf.txt",
            "middle/level_0/level_1/leaf.txt",
            "right/leaf.txt",
            "right/level_0/leaf.txt",
            "right/level_0/level_1/leaf.txt",
            "right/level_0/level_1/level_2/leaf.txt",
        ],
    );
}

/// A directory carrying an action of its own is recorded and then descended,
/// and the path recorded for it is its own however deep its children go.
#[tokio::test]
async fn a_directory_is_recorded_before_it_is_descended() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    add_dirty_node(&state, repository.clone(), ROOT_NODE, "tail.txt", file).await;
    let added = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "added",
        NodeFlags::DirtyModify,
    )
    .await;
    add_dirty_node(&state, repository.clone(), added, "leaf.txt", file).await;
    let deeper = add_dirty_node(
        &state,
        repository.clone(),
        added,
        "deeper",
        NodeFlags::DirtyModify,
    )
    .await;
    add_dirty_node(&state, repository.clone(), deeper, "deep.txt", file).await;

    assert_eq!(
        walk_dirty_paths_in_order(state, repository, DirtyWalkOptions::default()).await,
        vec![
            "added",
            "added/deeper",
            "added/deeper/deep.txt",
            "added/leaf.txt",
            "tail.txt",
        ],
        "a directory is recorded before it is descended, and its own path \
             is not extended by what its children append"
    );
}

/// A node the filter excludes is neither recorded nor descended, and the
/// siblings walked after it are still named against their own parent.
///
/// The excluded node sits in the middle of the chain: children are prepended,
/// so `blocked` is walked between `gamma` and `alpha`.
#[tokio::test]
async fn siblings_after_a_filtered_node_keep_their_own_prefix() {
    let repository = null_repository_excluding(&["blocked"]).await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let alpha = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "alpha",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(&state, repository.clone(), alpha, "one.txt", file).await;
    let blocked = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "blocked",
        NodeFlags::DirtyAdd,
    )
    .await;
    add_dirty_node(&state, repository.clone(), blocked, "hidden.txt", file).await;
    let gamma = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "gamma",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(&state, repository.clone(), gamma, "two.txt", file).await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["alpha/one.txt", "gamma/two.txt"],
        "the excluded subtree is pruned and the sibling after it is named \
             against the root"
    );
}

/// A node whose name is empty names its parent, since
/// [`RelativePathBuf::push`] ignores an empty component. The sibling after it
/// is still named against the parent, so nothing was taken off for a
/// component that was never appended.
#[tokio::test]
async fn an_empty_node_name_names_its_parent() {
    let repository = null_repository().await;
    let state = State::new();

    let file = NodeFlags::DirtyModify | NodeFlags::File;
    let outer = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "outer",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(&state, repository.clone(), outer, "x.txt", file).await;
    add_dirty_node(&state, repository.clone(), outer, "", file).await;

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["outer", "outer/x.txt"],
    );
}

/// The commit walk records nothing for a node that is also staged: its action
/// belongs to the revision being written. Every other caller wants both.
#[tokio::test]
async fn a_staged_node_is_recorded_only_when_staged_nodes_are_wanted() {
    let repository = null_repository().await;
    let state = State::new();

    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "staged.txt",
        NodeFlags::DirtyModify | NodeFlags::File | NodeFlags::StagedModify,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "dirty_only.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(
            state.clone(),
            repository.clone(),
            DirtyWalkOptions {
                skip_staged: true,
                force: false,
            },
        )
        .await,
        vec!["dirty_only.txt"],
        "a staged node is already in the commit and must not be re-applied"
    );
    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["dirty_only.txt", "staged.txt"],
        "without skip_staged both carry an action to record"
    );
}

/// A path the view excludes cannot be re-applied against a checkout that
/// never materializes it, so it is not recorded. `force` records it anyway.
#[tokio::test]
async fn a_filtered_path_is_recorded_only_under_force() {
    let repository = null_repository_viewing(&["ignored.txt"]).await;
    let state = State::new();

    for name in ["ignored.txt", "kept.txt"] {
        add_dirty_node(
            &state,
            repository.clone(),
            ROOT_NODE,
            name,
            NodeFlags::DirtyModify | NodeFlags::File,
        )
        .await;
    }

    assert_eq!(
        walk_dirty_paths(
            state.clone(),
            repository.clone(),
            DirtyWalkOptions::default()
        )
        .await,
        vec!["kept.txt"],
        "an excluded path has no checkout to be re-applied against"
    );
    assert_eq!(
        walk_dirty_paths(
            state,
            repository,
            DirtyWalkOptions {
                skip_staged: false,
                force: true,
            },
        )
        .await,
        vec!["ignored.txt", "kept.txt"],
        "force bypasses the filter"
    );
}

/// The ignore file has no say over a node the tree tracks, one the revision it was built on holds,
/// so a dirty mark on one is recorded below a directory the ignore file names. An add or a copy
/// merely marked dirty there is not: the revision holds neither, and the ignore file leaves them
/// out.
#[tokio::test]
async fn a_tracked_path_the_ignore_file_names_is_recorded_and_a_marked_add_or_copy_is_not() {
    let repository = null_repository_excluding(&["build/"]).await;
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");

    let committed = State::new();
    let build = add_dirty_node(
        &committed,
        repository.clone(),
        ROOT_NODE,
        "build",
        NodeFlags::NoFlags,
    )
    .await;
    let tracked = add_dirty_node(
        &committed,
        repository.clone(),
        build,
        "tracked.o",
        NodeFlags::File,
    )
    .await;
    committed.set_revision_number(1);
    let revision = committed
        .serialize(repository.clone(), token)
        .await
        .expect("serializing the committed revision");

    let state = State::deserialize(repository.clone(), revision)
        .await
        .expect("deserializing the revision to build on");
    state.reparent_onto(revision);
    state
        .node_mark(
            &repository,
            tracked,
            Mark::dirty(
                NodeFlags::DirtyModify,
                Ext::Set(NodeExtFlags::NoFlags),
                true,
            ),
        )
        .await
        .expect("marking the tracked file");
    for (name, action) in [
        ("new.o", NodeFlags::DirtyAdd),
        ("copy.o", NodeFlags::DirtyCopy),
    ] {
        add_dirty_node(
            &state,
            repository.clone(),
            build,
            name,
            action | NodeFlags::File,
        )
        .await;
    }

    assert_eq!(
        walk_dirty_paths(state, repository, DirtyWalkOptions::default()).await,
        vec!["build/tracked.o"],
        "the tracked file is carried, and the add and the copy beside it are not"
    );
}

/// Excluding a directory prunes its subtree: the filter is asked about the
/// directory before the walk descends into it, so a path below an excluded
/// directory is never reached.
#[tokio::test]
async fn an_excluded_directory_is_not_descended() {
    let repository = null_repository_viewing(&["build"]).await;
    let state = State::new();

    let build = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "build",
        NodeFlags::Dirty,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        build,
        "artifact.o",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "kept.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;

    assert_eq!(
        walk_dirty_paths(
            state.clone(),
            repository.clone(),
            DirtyWalkOptions::default()
        )
        .await,
        vec!["kept.txt"],
        "the subtree of an excluded directory is not walked"
    );
    assert_eq!(
        walk_dirty_paths(
            state,
            repository,
            DirtyWalkOptions {
                skip_staged: false,
                force: true,
            },
        )
        .await,
        vec!["build/artifact.o", "kept.txt"],
        "force reaches what the exclusion pruned"
    );
}

/// The walk descends only directories. Nothing below a link is in this state,
/// and `Node::child` means nothing on a file - it holds a modification time.
#[tokio::test]
async fn walking_from_anything_but_a_directory_records_nothing() {
    let repository = null_repository().await;
    let state = State::new();

    let file = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "file.txt",
        NodeFlags::DirtyModify | NodeFlags::File,
    )
    .await;
    let link = add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "link",
        NodeFlags::Dirty | NodeFlags::Link,
    )
    .await;

    for (label, node) in [("a file", file), ("a link", link)] {
        let mut paths = Vec::new();
        collect_dirty_paths_inner(
            state.clone(),
            repository.clone(),
            node,
            &mut RelativePathBuf::new(),
            &mut paths,
            DirtyWalkOptions::default(),
        )
        .await
        .expect("walking from a non-directory");
        assert!(paths.is_empty(), "walking from {label} records nothing");
    }
}

/// Most states are read for their header alone, so the block-loading permits
/// are allocated by the first block load and not by reading the state.
#[tokio::test]
async fn the_block_loading_permits_are_allocated_by_the_first_block_load() {
    let repository = null_repository().await;
    let state = State::new();
    add_dirty_node(
        &state,
        repository.clone(),
        ROOT_NODE,
        "file.txt",
        NodeFlags::File,
    )
    .await;
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .expect("serializing the state");

    let loaded = State::deserialize(repository.clone(), signature)
        .await
        .expect("deserializing the state");
    assert!(
        loaded.block_loading.get().is_none(),
        "reading the state allocated the permits"
    );

    loaded
        .block(repository.clone(), 0)
        .await
        .expect("loading the first node block");
    assert!(
        loaded.block_loading.get().is_some(),
        "loading a block allocated no permits"
    );
}

/// The changes a diff from `from` to `to` emits, as each one's action and path, in the order
/// it emits them. The diff runs in an execution context, which the events it sends need.
async fn diff_in_order(
    repository: Arc<RepositoryContext>,
    from: Arc<State>,
    to: Arc<State>,
) -> Vec<(FileAction, String)> {
    let walk = async move {
        ChangeStream::spawn(async move |changes| {
            diff(
                repository.clone(),
                from,
                repository,
                to,
                None,
                None,
                &changes,
                FilterMode::Full,
            )
            .await
        })
        .collect()
        .await
    };
    LORE_CONTEXT
        .scope(setup_test_execution(), walk)
        .await
        .expect("diffing the trees")
        .iter()
        .map(|change| {
            let path = change.resolved_side().mapping.path.as_str().to_string();
            (change.action, path)
        })
        .collect()
}

/// Asserts that `emitted` holds what `expected` names, as often, and names every directory
/// before what it holds. A path named twice, as an unnamed directory is under its parent's
/// name, is held to that at its first.
///
/// A hierarchy is walked by several tasks at once, so siblings come in no fixed order.
fn assert_emitted_parents_first(
    emitted: Vec<(FileAction, String)>,
    expected: &[(FileAction, String)],
    message: &str,
) {
    let by_path = |changes: &mut Vec<(FileAction, String)>| {
        changes.sort_by(|lhs, rhs| (&lhs.1, lhs.0 as u16).cmp(&(&rhs.1, rhs.0 as u16)));
    };
    let mut sorted = emitted.clone();
    by_path(&mut sorted);
    let mut wanted = expected.to_vec();
    by_path(&mut wanted);
    assert_eq!(sorted, wanted, "{message}");
    for (index, (_, path)) in emitted.iter().enumerate() {
        if emitted[..index].iter().any(|(_, earlier)| earlier == path) {
            continue;
        }
        let below = format!("{path}/");
        assert!(
            emitted[..index]
                .iter()
                .all(|(_, earlier)| !earlier.starts_with(&below)),
            "{message}: {path} came after something it holds, in {emitted:?}"
        );
    }
}

/// A directory added or deleted as a whole is emitted before what it holds, and a link below
/// it is emitted without what it holds.
#[tokio::test]
async fn a_hierarchy_is_emitted_parents_first() {
    let repository = null_repository().await;
    let empty = State::new();
    let tree = State::new();
    let directory = NodeFlags::NoFlags;

    let top = add_dirty_node(&tree, repository.clone(), ROOT_NODE, "top", directory).await;
    let mount = add_dirty_node(&tree, repository.clone(), top, "mount", NodeFlags::Link).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        mount,
        "held.txt",
        NodeFlags::File,
    )
    .await;
    add_dirty_node(&tree, repository.clone(), top, "empty", directory).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        top,
        "middle.txt",
        NodeFlags::File,
    )
    .await;
    let first = add_dirty_node(&tree, repository.clone(), top, "first", directory).await;
    let inner = add_dirty_node(&tree, repository.clone(), first, "inner", directory).await;
    add_dirty_node(&tree, repository.clone(), inner, "two.txt", NodeFlags::File).await;
    add_dirty_node(&tree, repository.clone(), first, "one.txt", NodeFlags::File).await;

    let order = [
        "top",
        "top/first",
        "top/first/one.txt",
        "top/first/inner",
        "top/first/inner/two.txt",
        "top/middle.txt",
        "top/empty",
        "top/mount",
    ];
    for (action, from, to) in [
        (FileAction::Add, empty.clone(), tree.clone()),
        (FileAction::Delete, tree, empty),
    ] {
        let expected: Vec<_> = order
            .iter()
            .map(|path| (action, path.to_string()))
            .collect();
        assert_emitted_parents_first(
            diff_in_order(repository.clone(), from, to).await,
            &expected,
            &format!(
                "a {action:?} emits each directory before what it holds, and nothing a link holds"
            ),
        );
    }
}

/// A child whose stored name the read path refuses, written before a rule refused it, is left
/// out of a directory added or deleted as a whole, as a listing leaves it out, and its siblings
/// are emitted.
#[tokio::test]
async fn a_hierarchy_leaves_out_a_child_whose_name_the_read_path_refuses() {
    let repository = null_repository().await;
    let empty = State::new();
    let tree = State::new();

    let top = add_dirty_node(
        &tree,
        repository.clone(),
        ROOT_NODE,
        "top",
        NodeFlags::NoFlags,
    )
    .await;
    add_dirty_node(&tree, repository.clone(), top, "kept.txt", NodeFlags::File).await;
    let planted = add_dirty_node(&tree, repository.clone(), top, "planted", NodeFlags::File).await;
    let block_index = NodeBlock::index(planted);
    let block = tree
        .block_with_nametable(repository.clone(), block_index)
        .await
        .expect("the block reads back");
    {
        let mut writer = block.write_for_tests();
        let node = *writer.node(Node::index(planted));
        let (offset, length) = writer
            .node_name_store_unchecked(".URC", node.name_offset, node.name_length)
            .expect("the unchecked store takes the name");
        let node = writer.node(Node::index(planted));
        node.name_hash = hash::hash_string(".URC");
        node.name_offset = offset;
        node.name_length = length;
        writer.mark_dirty();
    }
    tree.block_modified_for_tests(block, block_index);

    for (action, from, to) in [
        (FileAction::Add, empty.clone(), tree.clone()),
        (FileAction::Delete, tree, empty),
    ] {
        assert_eq!(
            diff_in_order(repository.clone(), from, to).await,
            vec![
                (action, "top".to_string()),
                (action, "top/kept.txt".to_string())
            ],
        );
    }
}

/// A child the filter excludes, and a directory with an empty name, leave the walk's path as
/// their parent's, so the siblings after them are named under it. The unnamed directory is
/// named as its parent, which it appends nothing to.
#[tokio::test]
async fn siblings_after_an_excluded_or_unnamed_child_are_named_under_their_parent() {
    let repository = null_repository_excluding(&["skipped"]).await;
    let tree = State::new();
    let directory = NodeFlags::NoFlags;

    let top = add_dirty_node(&tree, repository.clone(), ROOT_NODE, "top", directory).await;
    add_dirty_node(&tree, repository.clone(), top, "last.txt", NodeFlags::File).await;
    let unnamed = add_dirty_node(&tree, repository.clone(), top, "", directory).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        unnamed,
        "inner.txt",
        NodeFlags::File,
    )
    .await;
    add_dirty_node(
        &tree,
        repository.clone(),
        top,
        "middle.txt",
        NodeFlags::File,
    )
    .await;
    let skipped = add_dirty_node(&tree, repository.clone(), top, "skipped", directory).await;
    add_dirty_node(
        &tree,
        repository.clone(),
        skipped,
        "hidden.txt",
        NodeFlags::File,
    )
    .await;

    let expected: Vec<_> = [
        "top",
        "top/middle.txt",
        "top",
        "top/inner.txt",
        "top/last.txt",
    ]
    .iter()
    .map(|path| (FileAction::Add, path.to_string()))
    .collect();
    assert_emitted_parents_first(
        diff_in_order(repository, State::new(), tree).await,
        &expected,
        "nothing under the excluded directory is emitted, and every sibling keeps the prefix",
    );
}

/// Descent costs a stack entry, not a frame, so a hierarchy a thousand levels deep is emitted
/// whole, in the stack a shallow one takes.
#[tokio::test]
async fn a_deeply_nested_hierarchy_is_emitted_in_full() {
    const DEPTH: usize = 1024;

    let repository = null_repository().await;
    let tree = State::new();

    let mut parent = ROOT_NODE;
    let mut path = RelativePath::new();
    let mut expected = Vec::with_capacity(DEPTH + 1);
    for level in 0..DEPTH {
        let name = format!("d{level}");
        parent = add_dirty_node(&tree, repository.clone(), parent, &name, NodeFlags::NoFlags).await;
        path = path.push_into_buf(&name).freeze();
        expected.push((FileAction::Add, path.as_str().to_string()));
    }
    add_dirty_node(
        &tree,
        repository.clone(),
        parent,
        "leaf.txt",
        NodeFlags::File,
    )
    .await;
    expected.push((
        FileAction::Add,
        path.push_into_buf("leaf.txt").freeze().as_str().to_string(),
    ));

    assert_eq!(
        diff_in_order(repository, State::new(), tree).await,
        expected,
        "every one of the {DEPTH} levels is emitted, and the leaf below them"
    );
}

/// A file metadata block's load and the deprecated name table's load keep the fields of the
/// tree they read rather than the tree, so neither holds a tree across its read.
#[tokio::test]
async fn tree_readers_hold_its_fields_not_the_tree() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();

            let block_read = NodeFileMetadataBlockData::read_box_from_immutable_compat(
                repository.clone(),
                Address::default(),
                true,
            );
            let name_table_read = NameTable::deserialize(repository.clone(), Hash::default());
            let load = state.block_file_metadata_load(repository.clone(), 0);
            let name_table = state.nametable_load(repository);

            for (what, future, read) in [
                ("load", size_of_val(&load), size_of_val(&block_read)),
                (
                    "name table load",
                    size_of_val(&name_table),
                    size_of_val(&name_table_read),
                ),
            ] {
                assert!(
                    future < read + size_of::<Tree>(),
                    "the {what} holds {future} bytes over a read of {read}"
                );
            }
        })
        .await;
}

/// Dropping repeats keeps each node once, at the copy that came last.
#[test]
fn dropping_repeats_keeps_the_last_copy_of_each_node() {
    let mut items: Vec<(NodeID, u32)> = (0..300u32).map(|at| ((at * 7) % 61, at)).collect();
    drop_repeated_nodes(&mut items, |&(id, at)| (id, at));
    let ids: Vec<NodeID> = items.iter().map(|&(id, _)| id).collect();
    assert_eq!(ids, (0..61).collect::<Vec<NodeID>>());
    for &(id, at) in &items {
        let last = (0..300u32).rev().find(|at| (at * 7) % 61 == id);
        assert_eq!(Some(at), last, "node {id} must keep its last copy");
    }
}

async fn add_directory(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    name: &str,
) -> NodeID {
    state
        .node_add(repository.clone(), parent, directory(name), name)
        .await
        .expect("adding a directory must succeed")
}

/// An add whose parent is detached after the add checked it, while the add waits for its
/// slot, refuses the publish under the locks: the detached parent gains no child.
#[tokio::test]
async fn an_add_whose_parent_is_detached_before_its_publish_attaches_nothing() {
    let repository = null_repository().await;
    let state = State::new();
    let parent = add_directory(&state, &repository, ROOT_NODE, "parent").await;
    let head = state
        .block(repository.clone(), state.unused_head() as usize)
        .await
        .expect("the block slots come from");
    let mut filler = 0;
    while !head.read().is_full() {
        add_directory(&state, &repository, ROOT_NODE, &format!("f{filler}")).await;
        filler += 1;
    }
    let permit = state.unused.acquire().await.expect("the allocation permit");

    let mut add =
        std::pin::pin!(state.node_add(repository.clone(), parent, directory("child"), "child"));
    assert!(
        futures::poll!(add.as_mut()).is_pending(),
        "the add waits for the allocation permit, past its check of the parent"
    );
    state
        .node_detach(
            repository.clone(),
            parent,
            lore_revision::state::Detach::Always,
        )
        .await
        .expect("the detach");
    drop(permit);
    let added = add.await;

    assert!(
        added.is_err(),
        "an add under a parent detached before its publish must fail, got {added:?}"
    );
    assert_eq!(
        state
            .node(repository.clone(), parent)
            .await
            .expect("the detached parent")
            .child(),
        None,
        "the detached parent must gain no child"
    );
}

/// An attach whose name lookup started from a chain whose head then moves away, gains
/// the attached name as a new child and comes back must see, under its locks, that the
/// chain changed.
#[tokio::test]
async fn an_attach_sees_the_head_it_scanned_from_move_away_and_back() {
    let repository = null_repository().await;
    let state = State::new();
    let parent = add_directory(&state, &repository, ROOT_NODE, "parent").await;
    let other = add_directory(&state, &repository, ROOT_NODE, "other").await;
    let head = add_directory(&state, &repository, parent, "head").await;
    let parent_block = state
        .block(repository.clone(), NodeBlock::index(parent))
        .await
        .expect("the parent's block");
    let (_, scanned) = parent_block.node_with_chain_changes(Node::index(parent));

    state
        .node_reattach(repository.clone(), head, other, "head")
        .await
        .expect("the head moves away");
    state
        .node_attach(repository.clone(), parent, directory("x"), "x")
        .await
        .expect("x attaches");
    state
        .node_reattach(repository.clone(), head, parent, "head")
        .await
        .expect("the head moves back");

    let mut locks = BlockLocks::lock([
        Some((NodeBlock::index(parent), &parent_block)),
        None,
        None,
        None,
    ]);
    assert!(
        !chain_unchanged(&mut locks, parent, &parent_block, scanned),
        "a chain that gained a name since the lookup must not pass as unchanged"
    );
}

/// An absence read from another block than the directory's block an attach locks, as a block
/// evicted and loaded again leaves it, says nothing of the locked block's log: the attach and
/// the reattach find the name by walking, and never attach it twice.
#[tokio::test]
async fn an_absence_read_from_another_block_walks_and_never_attaches_a_duplicate() {
    let repository = null_repository().await;
    let state = State::new();
    let dir = add_directory(&state, &repository, ROOT_NODE, "dir").await;
    let mover = add_directory(&state, &repository, ROOT_NODE, "mover").await;
    let held = add_directory(&state, &repository, dir, "held").await;
    let block = state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the directory's block");
    // Installed after `held` was attached, so the log names no patch for it.
    let (mark, _) = block.watch_chain(Node::index(dir), None);
    let absent = Absent::new(
        &state,
        mark.seq,
        NodeBlock::new_zeroed(),
        dir,
        hash::hash_string("held"),
    );

    let attached = state
        .node_attach_absent(
            repository.clone(),
            dir,
            directory("held"),
            "held",
            Some(&absent),
        )
        .await
        .expect("the attach");
    assert_eq!(attached, AttachOutcome::Exists(held));
    let moved = state
        .node_reattach_absent(repository.clone(), mover, dir, "held", Some(&absent))
        .await
        .expect("the reattach");
    assert_eq!(moved, AttachOutcome::Exists(held));
    assert_eq!(
        state
            .node_children(repository.clone(), dir)
            .await
            .expect("the directory's children"),
        vec![held]
    );
}

/// The chain steps finds by name have taken on this thread, counting from now on. Left on:
/// tests count at once, and one turning counting off would cut another's count short.
fn counted_steps() -> u64 {
    step_counting::set_chain_step_counting(true);
    step_counting::thread_chain_steps()
}

/// `dir` and `mover` below the root of `state`, `dir`'s block, and the position of `dir`'s log,
/// installed now.
async fn watched_directory(
    state: &State,
    repository: &Arc<RepositoryContext>,
) -> (NodeID, NodeID, Arc<NodeBlock>, u32) {
    let dir = add_directory(state, repository, ROOT_NODE, "dir").await;
    let mover = add_directory(state, repository, ROOT_NODE, "mover").await;
    let block = state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the directory's block");
    let (mark, _) = block.watch_chain(Node::index(dir), None);
    (dir, mover, block, mark.seq)
}

/// An absence read from the directory's own block is checked against its log under the write
/// locks, with no walk: a name the log shows taken since is held, any other is free.
#[tokio::test]
async fn an_absence_read_from_the_directorys_block_is_checked_against_its_log() {
    let repository = null_repository().await;
    let state = State::new();
    let (dir, mover, block, at) = watched_directory(&state, &repository).await;
    let absent = |name: &str| Absent::new(&state, at, block.clone(), dir, hash::hash_string(name));
    let before = counted_steps();

    let attach = |name: &'static str| {
        let (state, repository, absent) = (&state, repository.clone(), absent(name));
        async move {
            state
                .node_attach_absent(repository, dir, directory(name), name, Some(&absent))
                .await
                .expect("the attach")
        }
    };
    let AttachOutcome::Attached(taken) = attach("taken").await else {
        panic!("the name is free");
    };
    assert_eq!(attach("taken").await, AttachOutcome::Exists(taken));
    assert_eq!(
        state
            .node_reattach_absent(
                repository.clone(),
                mover,
                dir,
                "taken",
                Some(&absent("taken"))
            )
            .await
            .expect("the reattach"),
        AttachOutcome::Exists(taken)
    );
    assert_eq!(
        state
            .node_reattach_absent(
                repository.clone(),
                mover,
                dir,
                "moved",
                Some(&absent("moved"))
            )
            .await
            .expect("the reattach"),
        AttachOutcome::Attached(mover)
    );
    assert_eq!(
        step_counting::thread_chain_steps(),
        before,
        "no find walked the chain"
    );
}

/// An attach that took a slot and then found its name held under the publish's locks hands the
/// slot back.
#[tokio::test]
async fn an_attach_whose_absence_turns_out_held_hands_its_slot_back() {
    let repository = null_repository().await;
    let state = State::new();
    let (dir, _, block, at) = watched_directory(&state, &repository).await;
    let absent = Absent::new(&state, at, block, dir, hash::hash_string("taken"));
    let attach = async || {
        state
            .node_attach_absent(
                repository.clone(),
                dir,
                directory("taken"),
                "taken",
                Some(&absent),
            )
            .await
            .expect("the attach")
    };
    let AttachOutcome::Attached(taken) = attach().await else {
        panic!("the name is free");
    };
    let slots = state
        .block(repository.clone(), NodeBlock::index(taken))
        .await
        .expect("the block slots come from");
    let free = || {
        let reader = slots.read();
        BLOCK_NODE_COUNT - reader.node_count() + reader.raw().node_unused_count as usize
    };
    let before = free();

    assert_eq!(attach().await, AttachOutcome::Exists(taken));
    assert_eq!(
        free(),
        before,
        "an attach that found its name held must hand its slot back"
    );
}

/// A reattach whose absence the directory's log has moved past finds its name by walking: a
/// name attached among the patches the log no longer holds is held.
#[tokio::test]
async fn a_reattach_whose_absence_left_the_log_walks_and_finds_the_holder() {
    let repository = null_repository().await;
    let state = State::new();
    let (dir, mover, block, at) = watched_directory(&state, &repository).await;
    let absent = Absent::new(&state, at, block.clone(), dir, hash::hash_string("target"));
    let holder = add_directory(&state, &repository, dir, "target").await;
    let capacity = block
        .read()
        .chain_log(Node::index(dir))
        .map_or(0, ChainLog::capacity);
    for i in 0..capacity {
        add_directory(&state, &repository, dir, &format!("d{i}")).await;
    }
    let before = counted_steps();
    let moved = state
        .node_reattach_absent(repository.clone(), mover, dir, "target", Some(&absent))
        .await
        .expect("the reattach");
    assert_eq!(moved, AttachOutcome::Exists(holder));
    assert!(
        step_counting::thread_chain_steps() > before,
        "the reattach walked the chain"
    );
}

/// An absence read from another directory's log in the same block, or for another name, says
/// nothing of this name in this directory: the attach finds it by walking.
#[tokio::test]
async fn an_absence_of_another_directory_or_name_walks() {
    let repository = null_repository().await;
    let state = State::new();
    let dir = add_directory(&state, &repository, ROOT_NODE, "dir").await;
    let other = add_directory(&state, &repository, ROOT_NODE, "other").await;
    let held = add_directory(&state, &repository, dir, "held").await;
    let block = state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the directories' block");
    assert_eq!(NodeBlock::index(other), NodeBlock::index(dir));
    // Installed after `held` was attached, so `dir`'s log names no patch for it.
    let (dir_mark, _) = block.watch_chain(Node::index(dir), None);
    let (other_mark, _) = block.watch_chain(Node::index(other), None);
    let held_hash = hash::hash_string("held");
    for absent in [
        Absent::new(&state, other_mark.seq, block.clone(), other, held_hash),
        Absent::new(
            &state,
            dir_mark.seq,
            block.clone(),
            dir,
            hash::hash_string("free"),
        ),
    ] {
        let before = counted_steps();
        let attached = state
            .node_attach_absent(
                repository.clone(),
                dir,
                directory("held"),
                "held",
                Some(&absent),
            )
            .await
            .expect("the attach");
        assert_eq!(attached, AttachOutcome::Exists(held), "{absent:?}");
        assert!(
            step_counting::thread_chain_steps() > before,
            "{absent:?}: the attach walked the chain"
        );
    }
}

/// A reattach onto a name the mover itself has taken since the absence moves it in place: the
/// log's attach of the name is the mover's own, not a holder's.
#[tokio::test]
async fn a_reattach_onto_a_name_the_mover_took_since_the_absence_moves_it() {
    let repository = null_repository().await;
    let state = State::new();
    let (dir, mover, block, at) = watched_directory(&state, &repository).await;
    let absent = Absent::new(&state, at, block, dir, hash::hash_string("target"));
    state
        .node_reattach(repository.clone(), mover, dir, "target")
        .await
        .expect("the mover takes the name");
    let before = counted_steps();
    let moved = state
        .node_reattach_absent(repository.clone(), mover, dir, "TARGET", Some(&absent))
        .await
        .expect("the respelling");
    assert_eq!(moved, AttachOutcome::Attached(mover));
    assert_eq!(step_counting::thread_chain_steps(), before, "no walk");
    assert_eq!(
        state
            .node_children(repository.clone(), dir)
            .await
            .expect("the directory's children"),
        vec![mover]
    );
}

/// A node met misparented under two different parents whose change counts happen to be
/// equal is two moves, not a broken chain; met twice under one parent with one count, it
/// is broken.
#[test]
fn chain_attempts_judge_a_misparented_node_per_parent() {
    let misparented = |actual_parent| ChainPosition::Misparented {
        node: 7,
        actual_parent,
        changes: 2,
    };
    let mut attempts = ChainAttempts::default();
    assert!(matches!(attempts.found(10, misparented(20)), Ok(None)));
    assert!(
        matches!(attempts.found(11, misparented(21)), Ok(None)),
        "the same node and count under another parent must be tried again"
    );
    assert!(
        attempts
            .found(11, misparented(21))
            .is_err_and(|error| error.is_invalid_node_hierarchy()),
        "the same node and count under the same parent twice running is broken"
    );
}

/// Attempts that all missed their node under one parent judge it missing from that chain, a
/// broken hierarchy; attempts that did not all miss it there ran out to churn, `SlowDown`.
#[tokio::test]
async fn chain_attempts_out_of_tries_tell_a_node_missing_from_its_chain_from_churn() {
    let repository = null_repository().await;
    let state = State::new();
    let dir = add_directory(&state, &repository, ROOT_NODE, "dir").await;
    let other = add_directory(&state, &repository, ROOT_NODE, "other").await;
    let run = async |positions: &dyn Fn(u32) -> (NodeID, ChainPosition)| {
        let mut attempts = ChainAttempts::default();
        for attempt in 0..CHAIN_CHANGE_ATTEMPTS {
            let (parent, position) = positions(attempt);
            assert!(matches!(attempts.found(parent, position), Ok(None)));
        }
        attempts.exhausted(&state, &repository, 7).await
    };

    let missing = run(&|_| (dir, ChainPosition::Missing)).await;
    assert!(
        missing.is_internal(),
        "a node every attempt missed under one parent is missing from its chain, got \
         {missing:?}"
    );
    let moved = run(&|attempt| {
        let parent = if attempt == 0 { other } else { dir };
        (parent, ChainPosition::Missing)
    })
    .await;
    assert!(
        moved.is_slow_down(),
        "a node missed under two parents is churn, got {moved:?}"
    );
    let moving = run(&|attempt| {
        let position = if attempt == CHAIN_CHANGE_ATTEMPTS - 1 {
            ChainPosition::Moving
        } else {
            ChainPosition::Missing
        };
        (dir, position)
    })
    .await;
    assert!(
        moving.is_slow_down(),
        "a node met moving on the last attempt is churn, got {moving:?}"
    );
}

/// Keeping the last copy in walk order leaves each node once, at its highest index, in
/// the order of those indices.
#[test]
fn keeping_the_last_copy_in_walk_order_restores_walk_order() {
    let mut items: Vec<(NodeID, u32, char)> = vec![
        (7, 0, 'a'),
        (3, 1, 'b'),
        (9, 2, 'c'),
        (3, 3, 'd'),
        (5, 4, 'e'),
        (7, 5, 'f'),
        (9, 6, 'g'),
        (5, 7, 'h'),
    ];
    keep_last_in_walk_order(&mut items, |&(id, index, _)| (id, index));
    assert_eq!(
        items,
        vec![(3, 3, 'd'), (7, 5, 'f'), (9, 6, 'g'), (5, 7, 'h')]
    );
}

/// A find through blocks in memory that meets a node chained in its directory but naming
/// another as its parent, twice with no change between, answers a broken hierarchy, as a find
/// reading blocks does, rather than walking on past it.
#[tokio::test]
async fn a_resident_find_meeting_a_broken_chain_answers_as_a_find_reading_blocks() {
    let repository = null_repository().await;
    let state = State::new();
    let dir = add_directory(&state, &repository, ROOT_NODE, "dir").await;
    let other = add_directory(&state, &repository, ROOT_NODE, "other").await;
    add_directory(&state, &repository, dir, "b").await;
    let broken = add_directory(&state, &repository, dir, "x").await;
    let block = state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the directory's block");
    assert_eq!(
        block.node_with_chain_changes(Node::index(dir)).0.child(),
        Some(broken),
        "x heads the chain, before b"
    );
    block.write_for_tests().node(Node::index(broken)).parent = other;

    let read = state
        .find_subnode(repository.clone(), dir, hash::hash_string("b"))
        .await;
    let resident = state.find_subnode_resident(dir, hash::hash_string("b"));

    assert!(
        read.is_err_and(|error| error.is_invalid_node_hierarchy()),
        "a find reading blocks answers a broken hierarchy"
    );
    assert!(
        resident.is_err_and(|error| error.is_invalid_node_hierarchy()),
        "a resident find answers a broken hierarchy"
    );
}

/// A walk that meets a node moved out of its directory, moved back in and moved out
/// again, one move per pass, restarts each time rather than taking the node for a
/// broken chain: the move back in counts as a change between the two passes.
#[tokio::test]
async fn a_walk_meeting_a_node_moved_out_back_in_and_out_again_restarts() {
    let repository = null_repository().await;
    let state = State::new();
    let dir = add_directory(&state, &repository, ROOT_NODE, "dir").await;
    let other = add_directory(&state, &repository, ROOT_NODE, "other").await;
    add_directory(&state, &repository, dir, "a").await;
    let y = add_directory(&state, &repository, dir, "y").await;
    let block = state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the parent's block");
    let (parent, changes) = block.node_with_chain_changes(Node::index(dir));
    assert_eq!(parent.child(), Some(y));
    let mut walk = ChainWalk::new(dir, block, changes);

    let move_y = async |into: NodeID| {
        state
            .node_reattach(repository.clone(), y, into, "y")
            .await
            .expect("the move must succeed");
        state.node(repository.clone(), y).await.expect("y")
    };
    let moved_out = move_y(other).await;
    assert_eq!(
        walk.step(y, &moved_out).expect("first pass"),
        ChainStep::Restart
    );
    move_y(dir).await;
    assert_eq!(walk.head_again(), Some(y));
    let moved_out_again = move_y(other).await;
    assert_eq!(
        walk.step(y, &moved_out_again)
            .expect("a node moved back in between two passes is no broken chain"),
        ChainStep::Restart
    );
}

/// A walk from a parent read before its head moved to another directory restarts from
/// the current head and visits every remaining child.
#[tokio::test]
async fn walk_children_restarts_past_a_child_moved_away() {
    let repository = null_repository().await;
    let state = State::new();
    let elsewhere = add_directory(&state, &repository, ROOT_NODE, "elsewhere").await;
    let dir = add_directory(&state, &repository, ROOT_NODE, "dir").await;
    let a = add_directory(&state, &repository, dir, "a").await;
    let head = add_directory(&state, &repository, dir, "head").await;
    let block = state
        .block(repository.clone(), NodeBlock::index(dir))
        .await
        .expect("the parent's block");
    let (stale, changes) = block.node_with_chain_changes(Node::index(dir));
    state
        .node_reattach(repository.clone(), head, elsewhere, "head")
        .await
        .expect("the head moves away");

    let mut visited = Vec::new();
    let walked = state
        .walk_children(
            repository.clone(),
            stale.child(),
            ChainWalk::new(dir, block, changes),
            None,
            |id, _| {
                visited.push(id);
                None::<()>
            },
        )
        .await
        .expect("the walk must restart, not fail");
    assert_eq!(visited, vec![a]);
    assert!(walked.may_repeat, "a restarted walk may repeat");
}

#[tokio::test]
async fn a_node_link_crosses_a_link_where_its_repository_or_revision_differs() {
    with_state(|repository, state| async move {
        state.set_revision(Hash::from([1; 32]));
        let link = |node, repository, revision: u8| NodeLink {
            node,
            repository,
            revision: Hash::from([revision; 32]),
        };
        let own = repository.id;
        let other = RepositoryId::from([9; 16]);

        assert!(!link(1, own, 1).crosses_link(&repository, &state));
        assert!(!link(ROOT_NODE, own, 1).crosses_link(&repository, &state));
        assert!(link(1, other, 1).crosses_link(&repository, &state));
        assert!(link(1, own, 2).crosses_link(&repository, &state));
        assert!(link(ROOT_NODE, other, 1).crosses_link(&repository, &state));
        assert!(!NodeLink::invalid().crosses_link(&repository, &state));
    })
    .await;
}
