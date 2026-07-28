// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::DashSet;
use dashmap::Entry;
use lore_base::lore_spawn;
use lore_base::lore_spawn_guarded;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time;
use tokio_util::task::AbortOnDropHandle;

use super::RepositoryAccess;
use super::RepositoryContext;
use super::RepositoryFormat;
use super::RepositoryWriteToken;
use super::SharedStoreToUseConfig;
use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::dependency;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter;
use crate::filter::FilterMode;
use crate::hash::hash_string_bytes;
use crate::immutable;
use crate::immutable::read_options_from_repository;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::layer;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_info;
use crate::lore_trace;
use crate::lore_warn;
use crate::metadata;
use crate::node::*;
use crate::progress::DEFAULT_WORK_CHANNEL_CAPACITY;
use crate::progress::DiscoveryStats;
use crate::protocol;
use crate::repository;
use crate::repository::FileConfig;
use crate::repository::RepositoryConfig;
use crate::repository::StoreConfig;
use crate::revision;
use crate::state;
use crate::state::State;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::state::is_file_modified;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

#[error_set(clone)]
pub enum CloneError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    AlreadyLinked,
    LayerNotFound,
    InvalidArguments,
    BranchAdvanced,
    BranchAlreadyExists,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    NotConnected,
    MissingIdentity,
}

impl EventError for CloneError {
    fn translated(&self) -> LoreError {
        match self {
            CloneError::Disconnected(_) => LoreError::Connection,
            CloneError::SlowDown(_) => LoreError::SlowDown,
            CloneError::Oversized(_) => LoreError::Oversized,
            CloneError::FileNotFound(_) => LoreError::FileNotFound,
            CloneError::NotFound(_)
            | CloneError::BranchNotFound(_)
            | CloneError::RevisionNotFound(_)
            | CloneError::LayerNotFound(_)
            | CloneError::LinkNotFound(_)
            | CloneError::NodeNotFound(_) => LoreError::NotFound,
            CloneError::AddressNotFound(_) => LoreError::AddressNotFound,
            CloneError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            CloneError::InvalidPath(_) | CloneError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

struct RepositoryCloneGuard {
    pub path: PathBuf,
    pub dotpath: PathBuf,
    pub clean_path_on_drop: bool,
    pub clean_dotpath_on_drop: bool,
}

fn initialize_guard(path: &Path, dotpath: &Path, dry_run: bool) -> RepositoryCloneGuard {
    RepositoryCloneGuard {
        path: path.to_path_buf(),
        dotpath: dotpath.to_path_buf(),
        clean_path_on_drop: dry_run && !path.exists(),
        clean_dotpath_on_drop: dry_run && !dotpath.exists(),
    }
}

impl Drop for RepositoryCloneGuard {
    fn drop(&mut self) {
        if self.clean_dotpath_on_drop {
            #[allow(clippy::disallowed_methods)] // Authorized clone-failure cleanup.
            let _ = std::fs::remove_dir_all(self.dotpath.as_path());
        }
        if self.clean_path_on_drop {
            #[allow(clippy::disallowed_methods)] // Authorized clone-failure cleanup.
            let _ = std::fs::remove_dir_all(self.path.as_path());
        }
    }
}

/// Data for the event emitted when a clone starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryCloneBeginEventData {
    /// Identifier of the repository being cloned.
    pub repository: RepositoryId,
    /// Name of the branch being cloned.
    pub branch: LoreString,
    /// Revision being cloned.
    pub revision: Hash,
    /// Local path the clone is written to.
    pub path: LoreString,
}

/// Progress counts for a clone operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryCloneCountData {
    /// Number of files finished.
    pub file_complete: u64,
    /// Number of files kept as they already matched.
    pub file_retain: u64,
    /// Number of files replaced.
    pub file_replace: u64,
    /// Total number of files discovered to process.
    pub file_count: u64,
    /// Number of files currently being processed.
    pub file_inflight: u64,
    /// Number of fragment fetches currently in flight.
    pub fragment_inflight: u64,
    /// Number of bytes transferred so far.
    pub bytes_transferred: u64,
    /// Total number of bytes to transfer.
    pub bytes_total: u64,
    /// Non-zero once file discovery has finished.
    #[serde(with = "u8_as_bool")]
    pub discovery_complete: u8,
}

impl LoreRepositoryCloneCountData {
    pub fn new(stats: &Arc<CloneStats>) -> Self {
        Self {
            file_complete: stats.complete.file_complete.load(Ordering::Relaxed),
            file_retain: stats.complete.file_retain.load(Ordering::Relaxed),
            file_replace: stats.complete.file_replace.load(Ordering::Relaxed),
            file_count: stats.discovery.total_files.load(Ordering::Relaxed),
            file_inflight: stats.file_inflight_count.load(Ordering::Relaxed),
            // Process-wide remote-fetch counter covers every path (single-fragment, defragment leaves, metadata blocks); reflects total pressure across concurrent operations, not just this clone.
            fragment_inflight: lore_storage::remote_fetch_inflight(),
            bytes_transferred: stats.complete.bytes_transferred.load(Ordering::Relaxed),
            bytes_total: stats.discovery.total_bytes.load(Ordering::Relaxed),
            discovery_complete: stats.discovery.complete.load(Ordering::Relaxed) as u8,
        }
    }
}

/// Data for the event emitted to report clone progress.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryCloneProgressEventData {
    /// Current progress counts.
    pub count: LoreRepositoryCloneCountData,
}

/// Data for the event emitted when a clone finishes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryCloneEndEventData {
    /// Name of the branch that was cloned.
    pub branch: LoreString,
    /// Revision that was cloned.
    pub revision: Hash,
    /// Final progress counts.
    pub count: LoreRepositoryCloneCountData,
}

const CLONE_DIRECTORY_MAX: usize = 10_000;
pub const CLONE_FILE_MAX: usize = 10_000;
pub const CLONE_FILE_DISCOVERY: usize = 100;

#[derive(Default)]
pub struct CloneCompleteStats {
    pub file_complete: AtomicU64,
    pub file_retain: AtomicU64,
    pub file_replace: AtomicU64,
    pub file_count: AtomicU64,
    pub bytes_transferred: AtomicU64,
}

pub struct CloneStats {
    pub discovery: DiscoveryStats,
    pub complete: CloneCompleteStats,

    pub file_inflight: Arc<Semaphore>,
    pub file_inflight_count: AtomicU64,
    pub directory_inflight: AtomicU64,

    /// Parent-dir paths already ensured to exist; skip `create_dir_all` on cache hit. Approximate — collisions/races only cause a redundant idempotent call, never a missing directory.
    pub created_parents: DashSet<u64>,
}

impl Default for CloneStats {
    fn default() -> Self {
        CloneStats {
            discovery: DiscoveryStats::default(),
            complete: CloneCompleteStats::default(),
            file_inflight: Arc::new(Semaphore::new(CLONE_FILE_DISCOVERY)),
            file_inflight_count: AtomicU64::new(0),
            directory_inflight: AtomicU64::new(0),
            created_parents: DashSet::new(),
        }
    }
}

/// Number of pending mtime writes that triggers a batch flush from
/// `clone_execute`'s stack-local buffer.
const CLONE_MTIME_BATCH_SIZE: usize = 256;

#[derive(Default)]
pub struct CloneOptions {
    /// Bare clone, do no sync any files
    pub bare: bool,
    /// Ignore existing files
    pub ignore_existing: bool,
    /// Clone virtually using split-write filesystem
    pub virtually: bool,
    /// Use direct file write
    pub direct_file_write: bool,
    /// Use direct file I/O
    pub direct_file_io: bool,
    /// File containing list of files to prefetch
    pub prefetch: Option<String>,
    /// Whether to use the shared store and options configuring it if desired
    pub shared_store_options: Option<SharedStoreToUseConfig>,
    /// Clone without local repository tracking (memory-only stores)
    pub no_tracking: bool,
    /// Root files for dependency-based selective clone.
    /// When empty: clone all files (existing behavior).
    pub root_files: Vec<String>,
    /// Tags to filter dependencies by during resolution.
    pub dependency_tags: Vec<String>,
    /// Follow transitive dependencies recursively.
    pub dependency_recursive: bool,
    /// Maximum dependency traversal depth. 0 means unlimited.
    pub dependency_depth_limit: u32,
}

pub struct CloneWorkItem {
    pub repository: Arc<RepositoryContext>,
    pub node: Node,
    pub relative_path: RelativePath,
}

/// Shared context for dependency-driven discovery across all block workers.
struct DependencyDiscoverContext {
    /// Tags to filter dependency edges by. Empty means follow all edges.
    tags: Arc<[String]>,
    /// Follow transitive dependencies (dependencies of dependencies).
    recursive: bool,
    /// Maximum traversal depth. 0 means unlimited.
    depth_limit: u32,
    /// Tracks visited `NodeID`s across all concurrent block workers for dedup.
    visited: DashSet<NodeID>,
}

/// Work item dispatched to a per-block discovery task.
struct BlockDiscoverItem {
    node_id: NodeID,
    /// Expected parent of `node_id` (and of every sibling reached from it).
    /// Used to validate parent-back-pointers as the walk descends.
    expected_parent: NodeID,
    /// Filesystem-relative path from the clone root (dispatcher `repository.path`).
    /// In tree walk mode: the parent directory's path.
    /// In dependency mode: the file's own path.
    relative_path: RelativePath,
    /// When Some, this item is part of a dependency-driven discovery walk.
    /// When None, the existing tree walk (child/sibling iteration) is used.
    dep_context: Option<Arc<DependencyDiscoverContext>>,
    /// Whether this item should have its dependencies loaded. Always true for
    /// root items. For non-root items, true only when recursive is enabled.
    follow_deps: bool,
    /// Current traversal depth (0 for root items, incremented per level).
    depth: u32,
    /// Cycle detector state, carried across block boundaries.
    cycle: SiblingCycleGuard,
    /// Whether any child has been visited, carried across block boundaries.
    visited_child: bool,
}

const BLOCK_DISCOVER_CHANNEL_CAPACITY: usize = 100;

struct BlockDiscoverInner {
    pending: AtomicUsize,
    channels: DashMap<usize, mpsc::Sender<BlockDiscoverItem>>,
    shutdown: AtomicBool,
    error: parking_lot::Mutex<Option<CloneError>>,
}

/// Coordinates per-block discovery tasks for clone operations.
/// Each block index gets a dedicated task that loads the block once and
/// processes all nodes belonging to that block from an unbounded channel.
struct BlockDiscoverDispatcher {
    inner: Arc<BlockDiscoverInner>,
    done: Notify,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
    file_tx: mpsc::Sender<CloneWorkItem>,
}

impl BlockDiscoverDispatcher {
    fn new(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        options: Arc<CloneOptions>,
        stats: Arc<CloneStats>,
        file_tx: mpsc::Sender<CloneWorkItem>,
    ) -> Self {
        Self {
            inner: Arc::new(BlockDiscoverInner {
                pending: AtomicUsize::new(0),
                channels: DashMap::new(),
                shutdown: AtomicBool::new(false),
                error: parking_lot::Mutex::new(None),
            }),
            done: Notify::new(),
            repository,
            state,
            options,
            stats,
            file_tx,
        }
    }

    fn dispatch(self: &Arc<Self>, item: BlockDiscoverItem) {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        self.inner.pending.fetch_add(1, Ordering::AcqRel);

        let block_index = NodeBlock::index(item.node_id);

        // Get or create the sender for this block. Clone the sender and drop
        // the DashMap entry before sending to avoid holding a shard lock during
        // the channel operation.
        #[allow(clippy::disallowed_methods)]
        let (tx, maybe_rx) = match self.inner.channels.entry(block_index) {
            Entry::Occupied(entry) => (entry.get().clone(), None),
            Entry::Vacant(entry) => {
                let (tx, rx) = mpsc::channel(BLOCK_DISCOVER_CHANNEL_CAPACITY);
                let tx_clone = tx.clone();
                entry.insert(tx);
                (tx_clone, Some(rx))
            }
        };

        if let Some(rx) = maybe_rx {
            let dispatcher = Arc::clone(self);
            lore_spawn!(async move {
                block_discover_task(dispatcher, block_index, rx).await;
            });
        }

        // Send without holding any DashMap lock. Use try_send to avoid
        // blocking the current block task -- if two block tasks mutually
        // dispatch to each other's full channels, blocking sends would
        // deadlock. On overflow, spawn a helper task to await the send.
        match tx.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(item)) => {
                let dispatcher = Arc::clone(self);
                lore_spawn!(async move {
                    if tx.send(item).await.is_err() {
                        dispatcher.item_complete();
                    }
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.item_complete();
            }
        }
    }

    fn item_complete(&self) {
        let prev = self.inner.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0);
        if prev == 1 {
            self.inner.channels.clear();
            self.done.notify_one();
        }
    }

    fn set_error(&self, error: CloneError) {
        if self
            .inner
            .shutdown
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        *self.inner.error.lock() = Some(error);
        self.inner.channels.clear();
        self.done.notify_one();
    }

    fn is_shutdown(&self) -> bool {
        self.inner.shutdown.load(Ordering::Acquire)
    }

    async fn join(&self) -> Result<(), CloneError> {
        loop {
            let notified = self.done.notified();
            if self.inner.shutdown.load(Ordering::Acquire) {
                let error = self.inner.error.lock();
                return match &*error {
                    Some(err) => {
                        execution_context().failure.store(true, Ordering::Relaxed);
                        Err(err.clone())
                    }
                    None => Ok(()),
                };
            }
            if self.inner.pending.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
            notified.await;
        }
    }
}

async fn block_discover_task(
    dispatcher: Arc<BlockDiscoverDispatcher>,
    block_index: usize,
    mut rx: mpsc::Receiver<BlockDiscoverItem>,
) {
    let Ok(block) = dispatcher
        .state
        .block_with_nametable(dispatcher.repository.clone(), block_index)
        .await
    else {
        dispatcher.set_error(CloneError::internal(
            "Failed to deserialize revision state node block",
        ));
        return;
    };

    while let Some(item) = rx.recv().await {
        if dispatcher.is_shutdown() {
            dispatcher.item_complete();
            continue;
        }
        if let Err(err) = process_block_item(&dispatcher, &block, block_index, item).await {
            dispatcher.set_error(err);
            return;
        }
    }
}

/// Create `relative_path` and any missing ancestors, materializing a directory
/// the view filter left without in-view content: one empty in the revision, or
/// one whose children were all filtered out.
async fn create_empty_directory(
    repository: &Arc<RepositoryContext>,
    relative_path: &RelativePath,
) -> Result<(), CloneError> {
    let absolute = relative_path.to_absolute_path(repository.require_path()?);
    tokio::fs::create_dir_all(&absolute)
        .await
        .internal_with(|| format!("Failed to create directory {}", absolute.display()))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_block_item(
    dispatcher: &Arc<BlockDiscoverDispatcher>,
    block: &Arc<NodeBlock>,
    block_index: usize,
    item: BlockDiscoverItem,
) -> Result<(), CloneError> {
    if let Some(dep_ctx) = item.dep_context.clone() {
        return process_block_item_dependency(dispatcher, block, item, dep_ctx).await;
    }

    let mut current_node_id = item.node_id;
    let expected_parent = item.expected_parent;
    let mut cycle = item.cycle;
    let mut visited_child = item.visited_child;

    loop {
        let node_index = Node::index(current_node_id);
        let node = block.node(node_index);

        node.walk_step(current_node_id, expected_parent, &mut cycle)
            .forward::<CloneError>("invalid node hierarchy in revision state")?;

        let node_name = block
            .node_name_ref(node_index)
            .forward::<CloneError>("Failed to deserialize node name")?
            .freeze();

        if node_name.is_empty() {
            return Err(CloneError::internal("Failed to deserialize node name"));
        }

        let node_path_relative = item.relative_path.push_into_buf(&node_name).freeze();

        if !dispatcher.repository.filter.emit_excludes(
            &node_path_relative,
            node.is_directory(),
            FilterMode::View,
        ) {
            visited_child = true;
            if node.is_file() {
                dispatcher
                    .stats
                    .discovery
                    .total_files
                    .fetch_add(1, Ordering::Relaxed);
                dispatcher
                    .stats
                    .discovery
                    .total_bytes
                    .fetch_add(node.size, Ordering::Relaxed);

                if dispatcher
                    .file_tx
                    .send(CloneWorkItem {
                        repository: dispatcher.repository.clone(),
                        node,
                        relative_path: node_path_relative.clone(),
                    })
                    .await
                    .is_err()
                {
                    // Receiver dropped, consumer encountered an error
                    return Err(CloneError::internal("Recursion task failed"));
                }
            } else if node.is_link() {
                if dispatcher.is_shutdown() {
                    dispatcher.item_complete();
                    return Ok(());
                }
                dispatcher.inner.pending.fetch_add(1, Ordering::AcqRel);

                let d = Arc::clone(dispatcher);
                let link_node = node;
                let link_fs_path = node_path_relative.clone();
                let link_options = dispatcher.options.clone();
                let link_stats = dispatcher.stats.clone();
                let link_tx = dispatcher.file_tx.clone();
                let link_repository = dispatcher.repository.clone();
                lore_spawn!(async move {
                    let result = clone_discover_link(
                        link_repository,
                        link_node,
                        link_fs_path,
                        link_options,
                        link_stats,
                        link_tx,
                    )
                    .await;
                    if let Err(err) = result {
                        d.set_error(err);
                    }
                    d.item_complete();
                });
            } else if node.is_directory() {
                if execution_context().globals().dry_run() {
                    let node_path_absolute =
                        node_path_relative.to_absolute_path(dispatcher.repository.require_path()?);
                    lore_info!("{}", node_path_absolute.display());
                }

                if let Some(first_child) = node.child() {
                    dispatcher.dispatch(BlockDiscoverItem {
                        node_id: first_child,
                        expected_parent: current_node_id,
                        relative_path: node_path_relative,
                        dep_context: None,
                        follow_deps: false,
                        depth: 0,
                        cycle: SiblingCycleGuard::new(current_node_id),
                        visited_child: false,
                    });
                } else if !execution_context().globals().dry_run() {
                    create_empty_directory(&dispatcher.repository, &node_path_relative).await?;
                }
            }
        }

        // Follow sibling chain
        match node.sibling() {
            Some(sibling_id) if NodeBlock::index(sibling_id) == block_index => {
                current_node_id = sibling_id;
            }
            Some(sibling_id) => {
                // Different block -- dispatch and stop this walk
                dispatcher.dispatch(BlockDiscoverItem {
                    node_id: sibling_id,
                    expected_parent,
                    relative_path: item.relative_path.clone(),
                    dep_context: None,
                    follow_deps: false,
                    depth: 0,
                    cycle,
                    visited_child,
                });
                break;
            }
            None => {
                if !visited_child
                    && !item.relative_path.is_empty()
                    && !execution_context().globals().dry_run()
                {
                    create_empty_directory(&dispatcher.repository, &item.relative_path).await?;
                }
                break;
            }
        }
    }

    dispatcher.item_complete();
    Ok(())
}

async fn process_block_item_dependency(
    dispatcher: &Arc<BlockDiscoverDispatcher>,
    block: &Arc<NodeBlock>,
    item: BlockDiscoverItem,
    dep_ctx: Arc<DependencyDiscoverContext>,
) -> Result<(), CloneError> {
    let node_index = Node::index(item.node_id);
    let node = block.node(node_index);

    // In dependency mode, relative_path is the file's own path (not parent's)
    if !dispatcher.repository.filter.emit_excludes(
        &item.relative_path,
        node.is_directory(),
        FilterMode::View,
    ) && node.is_file()
    {
        dispatcher
            .stats
            .discovery
            .total_files
            .fetch_add(1, Ordering::Relaxed);
        dispatcher
            .stats
            .discovery
            .total_bytes
            .fetch_add(node.size, Ordering::Relaxed);

        if dispatcher
            .file_tx
            .send(CloneWorkItem {
                repository: dispatcher.repository.clone(),
                node,
                relative_path: item.relative_path.clone(),
            })
            .await
            .is_err()
        {
            return Err(CloneError::internal("Recursion task failed"));
        }
    }

    // Only load and follow dependencies when this item is marked to do so.
    // Root items always follow deps; non-root items only when recursive.
    // Respect depth limit: 0 means unlimited, otherwise stop at the limit.
    if !item.follow_deps || (dep_ctx.depth_limit > 0 && item.depth >= dep_ctx.depth_limit) {
        dispatcher.item_complete();
        return Ok(());
    }

    let dep_data = dependency::load_dependency_data(
        dispatcher.repository.clone(),
        &dispatcher.state,
        item.node_id,
        dependency::DEPENDENCIES_KEY,
    )
    .await;

    if let Ok(dep_data) = dep_data {
        for entry in dep_data.iter() {
            if !entry.matches_tags(&dep_ctx.tags) {
                continue;
            }

            // Dedup across all concurrent workers
            if !dep_ctx.visited.insert(entry.node) {
                continue;
            }

            // Resolve filesystem-relative path for the dependency target
            let Ok(dep_path_str) = dispatcher
                .state
                .node_path(dispatcher.repository.clone(), entry.node)
                .await
            else {
                continue; // Stale dependency, skip silently
            };
            let Ok(dep_relative) = RelativePath::new_from_initial_path(&dep_path_str) else {
                continue;
            };

            event::LoreEvent::DependencyResolveItem(
                dependency::LoreDependencyResolveItemEventData {
                    source: item.relative_path.as_str().into(),
                    target: dep_relative.as_str().into(),
                    tags: LoreArray::from_vec(
                        entry
                            .tags
                            .iter()
                            .map(|t| LoreString::from(t.as_ref()))
                            .collect(),
                    ),
                },
            )
            .send();

            // Create parent directories
            let dep_absolute = dep_relative.to_absolute_path(dispatcher.repository.require_path()?);
            if let Some(parent) = dep_absolute.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }

            // Always dispatch with dependency context so the item goes through
            // dependency mode (relative_path is the file's own path).
            dispatcher.dispatch(BlockDiscoverItem {
                node_id: entry.node,
                expected_parent: INVALID_NODE,
                relative_path: dep_relative,
                dep_context: Some(dep_ctx.clone()),
                follow_deps: dep_ctx.recursive,
                depth: item.depth + 1,
                cycle: SiblingCycleGuard::new(INVALID_NODE),
                visited_child: false,
            });
        }
    }

    dispatcher.item_complete();
    Ok(())
}

#[derive(Default, Debug)]
pub struct CloneLayer {
    /// Repository name or ID
    pub module: String,
    /// Path in module
    pub module_path: String,
    /// Path in current repository
    pub layer_path: String,
    /// Metadata to use to link layer revisions
    pub metadata: Option<String>,
}

pub struct VirtualLayer {
    pub module: Arc<RepositoryContext>,
    /// Path in module
    pub module_path: RelativePath,
    /// Path in current repository
    pub layer_path: RelativePath,
    /// Revision
    pub state: Arc<State>,
}

#[allow(clippy::too_many_arguments)]
pub async fn clone(
    repository_url: &str,
    identity: &str,
    path: &Path,
    revision: Option<String>,
    view: Option<&Path>,
    layer: Option<CloneLayer>,
    options: CloneOptions,
) -> Result<(), CloneError> {
    let context = execution_context();
    let call = context.globals();

    // Parse the URL
    let (remote_url, name) = repository::parse_url(repository_url, false)
        .forward::<CloneError>("Invalid repository URL")?;

    let mut dotpath = path.to_path_buf();
    dotpath.push(repository::DOT_LORE);
    let mut guard = initialize_guard(path, dotpath.as_path(), call.dry_run());

    // Resolve the repository name
    let repository_data = repository::resolve_by_name(&remote_url, &name, identity)
        .await
        .forward::<CloneError>("Failed to resolve repository")?;

    // Spin up connection to actual repository
    // TODO(mjansson): Make use of the initial connection for repository resolve
    // and "upgrade" it to an authorized connection with revision client
    let remote = protocol::connect(remote_url.as_str(), identity, repository_data.id)
        .await
        .forward_with::<CloneError, _>(|| format!("Failed to connect to remote {remote_url}"))?;

    let resolved_identity = if !identity.is_empty() {
        Some(identity.to_string())
    } else {
        // `protocol::connect` with empty identity routes through
        // `auth_exchange`, which picks a cached identity scoped to the
        // server's auth_url. `remote.identity()` is that resolved user_id —
        // the same form (JWT `sub`) that production commits use for
        // `created-by`/`committed-by`, so the display layer's user_id
        // → name lookup in `lore log` still works.
        let resolved = remote.identity();
        (!resolved.is_empty()).then(|| resolved.to_string())
    };

    let repository_config = RepositoryConfig {
        remote_url: Some(remote_url),
        identity: resolved_identity,
        shared_store_to_use: options.shared_store_options.clone(),
        store: Some(StoreConfig::client_default()),
        file: Some(FileConfig::default()),
    };

    let repository_metadata = {
        // Dummy repository context just to be able to load the repository
        // metadata from the remote
        // TODO(mjansson): Clean up this connect flow
        let (immutable_store, mutable_store) =
            repository::create_client_memory_stores()
                .await
                .forward::<CloneError>("Failed to initialize repository on disk")?;

        let repository = Arc::new(RepositoryContext::new(
            Some(path.to_path_buf()),
            immutable_store,
            mutable_store,
            repository_data.id,
            crate::instance::InstanceId::default(),
            Ok(remote.clone()),
            Arc::default(),
            RepositoryFormat::Lore,
        ));

        repository.set_disable_upload(true);

        repository::metadata(repository, repository_data.metadata)
            .await
            .forward::<CloneError>("Repository not found")?
    };

    // Mint a single write token at the start of clone and share siblings into
    // each of the three contexts we construct (`create_local`,
    // `load_and_connect_with_token`, filter-view rebuild). Refcounted sharing
    // keeps the per-path write mutex held end-to-end without re-acquiring it
    // — a re-acquire attempt would deadlock, since the guard is non-reentrant.
    let write_token = RepositoryWriteToken::acquire(path).await;

    let repository_id = repository_data.id;
    let default_branch_id = repository_metadata.default_branch;
    let default_branch_name = repository_metadata.default_branch_name.clone();
    let no_tracking = options.no_tracking;
    let access = if no_tracking {
        RepositoryAccess::NoStore
    } else {
        RepositoryAccess::ReadWrite
    };

    let prefetch_branch_fut = {
        let remote = remote.clone();
        let revision_pinned = revision.is_some();
        async move {
            if revision_pinned {
                Ok::<_, CloneError>(None)
            } else {
                let status = branch::load_remote(remote, repository_id, default_branch_id)
                    .await
                    .forward::<CloneError>("Failed to load repository state from remote")?;
                Ok(Some(status))
            }
        }
    };

    let local_init_fut = async move {
        {
            let _repository = repository::create_local(
                path,
                &write_token,
                repository_id,
                default_branch_id,
                default_branch_name,
                repository_config,
                no_tracking,
            )
            .await
            .forward::<CloneError>("Failed to initialize repository on disk")?;
        }
        lore_debug!("Initialized repository on disk in {}", path.display());

        // Share the outer write token to whichever context we load. The Client
        // token's per-path mutex (acquired at the top of clone) serializes
        // concurrent same-path clones; the mutex is meaningful even in NoStore
        // mode because both clones still race on the destination directory's
        // file materialization. Sharing here means downstream helpers
        // (`branch::create` → `store_name_to_id` / `metadata_store` /
        // `store_latest`) see a context with write capability and can populate
        // the (in-memory or on-disk) mutable store.
        let load_token = Some(write_token.share());
        repository::load_and_connect_with_token(path, access, load_token)
            .await
            .forward::<CloneError>("Failed to load revision state")
    };

    let (repository, prefetched_branch) = tokio::try_join!(local_init_fut, prefetch_branch_fut)?;

    // Copy the view definition if given
    let filter_view = if let Some(view) = view {
        let mut view_target = dotpath.clone();
        view_target.push(repository::VIEW_FILTER);
        tokio::fs::copy(view, &view_target)
            .await
            .internal_with(|| {
                format!(
                    "Failed to copy file from {} to {}",
                    view.display(),
                    view_target.display()
                )
            })?;

        Some(
            filter::load_view(view_target.as_path()).forward_with::<CloneError, _>(|| {
                format!("Failed to load view {}", view.display())
            })?,
        )
    } else {
        None
    };

    // Rebuild the context with the filter view applied. The helper inherits
    // stores, repo lock, and the write token — the latter via `share()` so
    // the rebuilt context carries a sibling of the outer `write_token` and
    // keeps the per-path mutex guard held for downstream writes (state
    // serialization, anchor persistence, branch creation, etc.) regardless
    // of whether the stores are in-memory (`--no-tracking`) or on-disk.
    let new_context = repository.with_filter_and_remote(
        Arc::new(filter_view.unwrap_or_default()),
        Ok(remote.clone()),
    );
    let repository = Arc::new(new_context);

    repository.set_disable_upload(true);

    // Prune stale instances in the background when using a shared store.
    // AbortOnDropHandle ensures the task is cancelled if clone fails early.
    let prune_task = if options
        .shared_store_options
        .as_ref()
        .and_then(|o| o.use_shared_store)
        .unwrap_or(false)
    {
        let prune_repo = repository.clone();
        Some(AbortOnDropHandle::new(lore_spawn!(async move {
            let _ = crate::instance::instance_prune(prune_repo).await;
        })))
    } else {
        None
    };

    let revision = if let Some(revision) = revision {
        revision::resolve(
            repository.clone(),
            revision,
            call.search_limit(),
            call.search_location(),
        )
        .await
        .forward::<CloneError>("Invalid revision signature")?
    } else {
        Hash::default()
    };

    // If a revision was given, make sure it's on the expected branch
    let branch_id = if !revision.is_zero() {
        let state = state::State::deserialize(repository.clone(), revision)
            .await
            .forward::<CloneError>("Failed to load revision state")?;
        let metadata = metadata::Metadata::deserialize(repository.clone(), state.metadata_hash())
            .await
            .forward::<CloneError>("Failed to load revision metadata")?;
        metadata
            .get_branch()
            .forward::<CloneError>("Failed to load revision metadata")?
    } else {
        repository_metadata.default_branch
    };

    let branch = if let Some(branch) = prefetched_branch {
        branch
    } else {
        branch::load_remote(remote.clone(), repository.id, branch_id)
            .await
            .forward::<CloneError>("Failed to load repository state from remote")?
    };

    let mut revision = if revision.is_zero() {
        if branch.latest.is_zero() {
            if branch_id != repository_metadata.default_branch {
                return Err(CloneError::internal(
                    "Cloned an empty repository without revisions - did you try to clone a non-existing branch?",
                ));
            }
            lore_warn!("Cloned an empty repository without revisions - did you forget to push?");
        }
        branch.latest
    } else {
        revision
    };

    // Resolve layers
    let mut layers = None;
    if let Some(layer) = layer {
        // Try resolving using repository service
        let repository_id = {
            let repository_service = remote
                .repository()
                .await
                .forward::<CloneError>("Failed to resolve given layer repository")?;
            let response = repository_service
                .query(None, Some(layer.module.as_str()))
                .await
                .forward::<CloneError>("Failed to resolve given layer repository")?;
            response.id
        };
        lore_info!(
            "Resolved layer repository: {} -> {}",
            layer.module,
            repository_id
        );

        lore_info!("Resolving layer revisions");
        let module = Arc::new(repository.to_layer_context(repository_id).await);
        let layer_latest = layer::latest_revision(module.clone(), branch_id)
            .await
            .forward::<CloneError>("Failed to get layer latest revision")?;
        let state_target = state::State::deserialize(repository.clone(), revision)
            .await
            .forward::<CloneError>("Failed to load revision state")?;
        let (layer_revision, main_revision) = layer::find_revision_match(
            repository.clone(),
            module.clone(),
            branch_id,
            state_target.clone(),
            layer_latest,
            layer.metadata.as_deref(),
        )
        .await
        .forward::<CloneError>("Unable to find a matching revision for layer")?;

        lore_debug!(
            "Layer {layer:?} found revision {layer_revision} matching main revision {main_revision}"
        );
        if main_revision != revision {
            revision = main_revision;
            lore_info!(
                "Using main repository revision {revision} to match layer metadata {}",
                layer.metadata.as_deref().unwrap_or_default()
            );
        }

        let layer_path =
            RelativePath::new_from_initial_path(layer.layer_path.to_lowercase().as_str())
                .unwrap_or_default();
        let module_path =
            RelativePath::new_from_initial_path(layer.module_path.as_str()).unwrap_or_default();

        let state = State::deserialize(module.clone(), layer_revision)
            .await
            .forward::<CloneError>("Failed to load revision state")?;

        layers = Some(VirtualLayer {
            module,
            module_path,
            layer_path,
            state,
        });
    }

    let (state, metadata) = tokio::try_join!(
        async {
            State::deserialize(repository.clone(), revision)
                .await
                .forward::<CloneError>("Failed to load revision state")
        },
        async {
            metadata::Metadata::deserialize(repository.clone(), branch.metadata)
                .await
                .forward::<CloneError>("Failed to load repository state from remote")
        }
    )?;

    let branch_name = branch::name(&metadata).unwrap_or_default().to_string();

    let store_task: tokio::task::JoinHandle<Result<(), CloneError>> = lore_spawn!({
        let repository = repository.clone();
        let branch_name = branch_name.clone();
        let branch_metadata = branch.metadata;
        async move {
            if !branch::exist_local(repository.clone(), branch_id).await {
                branch::mutable_store_metadata(repository.clone(), branch_id, branch_metadata)
                    .await
                    .forward::<CloneError>("Failed to create local branch")?;
                branch::store_name_to_id(repository.clone(), branch_id, branch_name)
                    .await
                    .forward::<CloneError>("Failed to create local branch")?;
            }
            if !revision.is_zero() {
                branch::store_latest(
                    repository.clone(),
                    branch_id,
                    revision,
                    BranchLatestStatus::Convergent,
                )
                .await
                .forward::<CloneError>("Failed to create local branch")?;
                branch::store_last_sync(repository.clone(), branch_id, revision).await;
            }
            Ok::<(), CloneError>(())
        }
    });

    event::LoreEvent::RepositoryCloneBegin(LoreRepositoryCloneBeginEventData {
        repository: repository.id,
        branch: LoreString::from(branch_name.as_str()),
        revision,
        path: path.into(),
    })
    .send();

    let stats = Arc::new(CloneStats::default());

    let materialize_result = clone_materialize(
        repository.clone(),
        state,
        Arc::new(options),
        layers,
        remote.clone(),
        path,
        revision,
        branch_id,
        stats.clone(),
    )
    .await;

    let store_result: Result<(), CloneError> = match store_task.await {
        Ok(r) => r,
        Err(e) if e.is_cancelled() => Ok(()),
        Err(e) => Err(CloneError::internal(format!("Store task failed: {e}"))),
    };

    let _ = repository.flush(call.sync_data()).await;

    if !call.dry_run() {
        guard.clean_path_on_drop = false;
        guard.clean_dotpath_on_drop = false;
    }

    if let Some(task) = prune_task {
        let _ = task.await;
    }

    event::LoreEvent::RepositoryCloneEnd(LoreRepositoryCloneEndEventData {
        branch: branch_name.into(),
        revision,
        count: LoreRepositoryCloneCountData::new(&stats),
    })
    .send();

    materialize_result.and(store_result)
}

#[allow(clippy::too_many_arguments)]
async fn clone_materialize(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    options: Arc<CloneOptions>,
    layers: Option<VirtualLayer>,
    remote: Arc<lore_transport::Connection>,
    _path: &Path,
    revision: Hash,
    branch_id: crate::lore::BranchId,
    stats: Arc<CloneStats>,
) -> Result<(), CloneError> {
    if options.virtually {
        lore_info!("Serving virtualized filesystem at state {revision}");
        if let Some(layer) = layers.as_ref() {
            lore_info!(
                "Experimental support for virtualized layer at state {}",
                layer.state.revision()
            );
        }

        #[cfg(all(target_family = "windows", feature = "vfs"))]
        {
            //crate::swfs::serve::serve(_path, repository.clone(), state);
            crate::projfs::serve::serve(
                _path,
                repository.clone(),
                state,
                layers,
                options.prefetch.as_deref(),
            );
        }
        #[cfg(target_family = "windows")]
        {
            lore_error!("Virtual repositories not supported, build with \"--features=vfs\"");
            return Err(NotSupported {
                operation: "Virtual repositories not supported, build with \"--features=vfs\""
                    .to_string(),
            }
            .into());
        }
        #[cfg(not(target_family = "windows"))]
        {
            lore_error!("Virtual repositories not yet supported on this platform");
            return Err(NotSupported {
                operation: "Virtual repositories not yet supported on this platform".to_string(),
            }
            .into());
        }
    }

    let mut clone_result = Ok(());
    let mut cache_task = None;

    if !options.bare && !revision.is_zero() {
        lore_info!("Pull state {revision}");
        let correlation_id = execution_context().globals().correlation_id.to_string();
        let _storage = remote
            .session(repository.id, &correlation_id)
            .await
            .forward::<CloneError>("Failed to load repository state from remote")?;

        let ticker_stats = stats.clone();
        let _ticker = AbortOnDropHandle::new(lore_spawn!({
            async move {
                let mut ticker = time::interval(time::Duration::from_millis(100));
                loop {
                    ticker.tick().await;
                    event::LoreEvent::RepositoryCloneProgress(
                        LoreRepositoryCloneProgressEventData {
                            count: LoreRepositoryCloneCountData::new(&ticker_stats),
                        },
                    )
                    .send();
                }
            }
        }));

        let cache_repository = repository.clone();
        let cache_state = state.clone();
        cache_task = Some(lore_spawn!(async move {
            let _ = cache_state.cache_fragments(cache_repository).await;
        }));

        clone_result =
            clone_in_path(repository.clone(), state, options.clone(), stats.clone()).await;
    }

    event::LoreEvent::RepositoryCloneProgress(LoreRepositoryCloneProgressEventData {
        count: LoreRepositoryCloneCountData::new(&stats),
    })
    .send();

    if clone_result.is_err() {
        execution_context()
            .failure
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    if let Some(task) = cache_task {
        let _ = task.await;
    }

    crate::instance::store_current_anchor_branch(&repository, branch_id)
        .await
        .forward::<CloneError>("Failed to write current state anchor to repository")?;
    crate::instance::store_current_anchor(&repository, revision)
        .await
        .forward::<CloneError>("Failed to write current state anchor to repository")?;

    clone_result
}

#[allow(clippy::too_many_arguments)]
async fn clone_in_path(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) -> Result<(), CloneError> {
    let (file_tx, file_rx) = mpsc::channel(DEFAULT_WORK_CHANNEL_CAPACITY);

    let dispatcher = Arc::new(BlockDiscoverDispatcher::new(
        repository.clone(),
        state.clone(),
        options.clone(),
        stats.clone(),
        file_tx,
    ));

    if options.root_files.is_empty() {
        // Tree walk mode (existing behavior)
        let root_node = state
            .node(repository.clone(), 0)
            .await
            .forward::<CloneError>("Failed to deserialize revision state node block")?;

        if let Some(first_child) = root_node.child() {
            dispatcher.dispatch(BlockDiscoverItem {
                node_id: first_child,
                expected_parent: ROOT_NODE,
                relative_path: RelativePath::new(),
                dep_context: None,
                follow_deps: false,
                depth: 0,
                cycle: SiblingCycleGuard::new(ROOT_NODE),
                visited_child: false,
            });
        }
    } else {
        // Dependency discovery mode
        let tags: Arc<[String]> = options.dependency_tags.clone().into();
        let dep_ctx = Arc::new(DependencyDiscoverContext {
            tags,
            recursive: options.dependency_recursive,
            depth_limit: options.dependency_depth_limit,
            visited: DashSet::new(),
        });

        event::LoreEvent::DependencyResolveBegin(dependency::LoreDependencyResolveBeginEventData {
            root_count: options.root_files.len() as u64,
        })
        .send();

        for root_path in &options.root_files {
            let relative = RelativePath::new_from_initial_path(root_path)
                .forward_with::<CloneError, _>(|| format!("Invalid path: {root_path}"))?;
            let node_link = state
                .find_node_link(repository.clone(), relative.as_str())
                .await
                .forward_with::<CloneError, _>(|| format!("Root file not found: {root_path}"))?;
            if !node_link.is_valid() {
                return Err(CloneError::internal(format!(
                    "Root file not found: {root_path}"
                )));
            }
            let node_id = node_link.node;
            let absolute = relative.to_absolute_path(repository.require_path()?);
            if let Some(parent) = absolute.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            dep_ctx.visited.insert(node_id);
            dispatcher.dispatch(BlockDiscoverItem {
                node_id,
                expected_parent: INVALID_NODE,
                relative_path: relative,
                dep_context: Some(dep_ctx.clone()),
                follow_deps: true,
                depth: 0,
                cycle: SiblingCycleGuard::new(INVALID_NODE),
                visited_child: false,
            });
        }

        event::LoreEvent::DependencyResolveEnd(dependency::LoreDependencyResolveEndEventData {
            resolved_count: dep_ctx.visited.len() as u64,
        })
        .send();
    }

    let discover_stats = stats.clone();
    let producer = lore_spawn!(async move {
        let result = dispatcher.join().await;
        discover_stats
            .discovery
            .complete
            .store(true, Ordering::Relaxed);
        // Send a progress event immediately when discovery finishes,
        // ensuring at least one progress event has discoveryComplete=true
        event::LoreEvent::RepositoryCloneProgress(LoreRepositoryCloneProgressEventData {
            count: LoreRepositoryCloneCountData::new(&discover_stats),
        })
        .send();
        drop(dispatcher);
        result
    });

    let consumer_options = options.clone();
    let consumer_stats = stats.clone();
    let consumer_repository = repository.clone();
    let consumer = lore_spawn!(async move {
        clone_execute(
            file_rx,
            consumer_repository,
            consumer_options,
            consumer_stats,
        )
        .await
    });

    let (producer_result, consumer_result) = tokio::join!(producer, consumer);
    producer_result
        .internal("Recursion task failed")?
        .inspect_err(|_| {
            execution_context()
                .failure
                .store(true, std::sync::atomic::Ordering::Relaxed);
        })?;
    consumer_result
        .internal("Recursion task failed")?
        .inspect_err(|_| {
            execution_context()
                .failure
                .store(true, std::sync::atomic::Ordering::Relaxed);
        })?;

    Ok(())
}

async fn clone_discover_link(
    repository: Arc<RepositoryContext>,
    node: Node,
    link_fs_path: RelativePath,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
    tx: mpsc::Sender<CloneWorkItem>,
) -> Result<(), CloneError> {
    let link = node.linked_node();
    let linked_repository_id = link.repository;
    let signature = link.revision;
    let link_node = link.node;

    lore_debug!("Resolve link {linked_repository_id} node {link_node}");
    let linked_repository = Arc::new(repository.to_link_context(linked_repository_id).await);
    if let Ok(link_remote) = linked_repository.remote().await {
        let correlation_id = execution_context().globals().correlation_id.to_string();
        if link_remote
            .session(linked_repository.id, &correlation_id)
            .await
            .is_ok()
        {
            let link_state = State::deserialize(linked_repository.clone(), signature)
                .await
                .forward::<CloneError>("Failed to load revision state")?;

            let absolute_path = link_fs_path.to_absolute_path(repository.require_path()?);
            lore_info!(
                "Clone link {} in {}",
                linked_repository.id,
                absolute_path.display()
            );
            // Discovery no longer pre-creates parent dirs; create the full chain here and cache the link dir in stats so later files under it cache-hit.
            let link_dir_hash = hash_string_bytes(absolute_path.as_os_str().as_encoded_bytes());
            if !stats.created_parents.contains(&link_dir_hash) {
                tokio::fs::create_dir_all(absolute_path.as_path())
                    .await
                    .internal_with(|| {
                        format!("Failed to create directory {}", absolute_path.display())
                    })?;
                stats.created_parents.insert(link_dir_hash);
            }

            // Use a link-scoped dispatcher for the link's own state/repository
            let link_dispatcher = Arc::new(BlockDiscoverDispatcher::new(
                linked_repository.clone(),
                link_state.clone(),
                options,
                stats,
                tx,
            ));

            let root_node = link_state
                .node(linked_repository.clone(), link_node)
                .await
                .forward::<CloneError>("Failed to deserialize revision state node block")?;

            if let Some(first_child) = root_node.child() {
                link_dispatcher.dispatch(BlockDiscoverItem {
                    node_id: first_child,
                    expected_parent: link_node,
                    relative_path: link_fs_path,
                    dep_context: None,
                    follow_deps: false,
                    depth: 0,
                    cycle: SiblingCycleGuard::new(link_node),
                    visited_child: false,
                });
            }

            link_dispatcher.join().await?;
        } else {
            lore_debug!("Failed connecting to link remote storage, assume no access rights");
        }
    }

    Ok(())
}

pub async fn clone_execute(
    mut rx: mpsc::Receiver<CloneWorkItem>,
    repository: Arc<RepositoryContext>,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) -> Result<(), CloneError> {
    let mut failure = None;
    let mut tasks: JoinSet<Result<Option<(Hash, u64)>, CloneError>> = JoinSet::new();
    // Permit cap grows monotonically with queue depth; unused permits cost nothing. Starts from whatever caller configured (default CLONE_FILE_DISCOVERY).
    let mut current_permits = stats.file_inflight.available_permits();
    // Stack-local mtime batch: each `clone_file` returns its (key, mtime) pair
    // (or None for retain/dry-run/zero-byte cases) on completion; we collect
    // them as we drain the JoinSet and fire-and-forget a batched mutable-store
    // write when the buffer hits CLONE_MTIME_BATCH_SIZE. No shared lock.
    let mut mtime_batch: Vec<(Hash, u64)> = Vec::with_capacity(CLONE_MTIME_BATCH_SIZE);
    let mtime_partition = repository.id;
    let mtime_store = repository.try_mutable_store_arc();

    while let Some(item) = rx.recv().await {
        // Target = queue backlog + headroom so the next recv never stalls on acquire; jump to max once discovery is done and no more items will arrive.
        let target = if stats.discovery.complete.load(Ordering::Relaxed) {
            CLONE_FILE_MAX
        } else {
            // +1 accounts for the item we just received.
            (rx.len() + 1 + CLONE_FILE_DISCOVERY).clamp(CLONE_FILE_DISCOVERY, CLONE_FILE_MAX)
        };
        if target > current_permits {
            stats.file_inflight.add_permits(target - current_permits);
            current_permits = target;
        }

        // Acquire permit before spawning to provide backpressure to the channel.
        // When all permits are held, this blocks the loop and stops draining the
        // channel, allowing it to fill up and apply backpressure to the producer.
        let permit = Arc::clone(&stats.file_inflight)
            .acquire_owned()
            .await
            .expect("file_inflight semaphore closed unexpectedly");

        let absolute_path = item
            .relative_path
            .to_absolute_path(item.repository.require_path()?);
        let item_options = options.clone();
        let item_stats = stats.clone();
        lore_spawn!(tasks, async move {
            let _permit = permit;
            item_stats
                .complete
                .file_count
                .fetch_add(1, Ordering::Relaxed);
            item_stats
                .file_inflight_count
                .fetch_add(1, Ordering::Relaxed);
            let result = clone_file(
                item.repository,
                item.node,
                absolute_path,
                item.relative_path,
                item_options,
                item_stats.clone(),
            )
            .await;
            item_stats
                .file_inflight_count
                .fetch_sub(1, Ordering::Relaxed);
            result
        });

        while let Some(result) = tasks.try_join_next() {
            match result
                .map_err(|e| CloneError::internal_with_context(e, "Recursion task failed"))
                .and_then(|r| r)
            {
                Ok(Some(entry)) => {
                    mtime_batch.push(entry);
                    if mtime_batch.len() >= CLONE_MTIME_BATCH_SIZE
                        && let Some(store) = mtime_store.clone()
                    {
                        let drained = std::mem::take(&mut mtime_batch);
                        lore_spawn_guarded!(state::file_modified_time_store_batch(
                            store,
                            mtime_partition,
                            drained,
                        ));
                    }
                }
                Ok(None) => {}
                Err(err) => failure = failure.or(Some(err)),
            }
        }
        if failure.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        match result
            .map_err(|e| CloneError::internal_with_context(e, "Recursion task failed"))
            .and_then(|r| r)
        {
            Ok(Some(entry)) => mtime_batch.push(entry),
            Ok(None) => {}
            Err(err) => failure = failure.or(Some(err)),
        }
    }

    // Flush any remaining mtimes that didn't fill a threshold batch.
    if !mtime_batch.is_empty()
        && let Some(store) = mtime_store
    {
        lore_spawn_guarded!(state::file_modified_time_store_batch(
            store,
            mtime_partition,
            mtime_batch,
        ));
    }

    if let Some(err) = failure {
        execution_context()
            .failure
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Err(err)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn clone_node(
    repository: Arc<RepositoryContext>,
    storage: Arc<lore_transport::StorageSession>,
    state: Arc<State>,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    node: NodeID,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) -> Result<(), CloneError> {
    let mut failure = None;
    let mut tasks = JoinSet::new();

    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node)
            .await
            .forward::<CloneError>("Failed to deserialize revision state node block")?;
    while let Some((child_id, child_node, child_name)) = children
        .next()
        .await
        .forward::<CloneError>("Failed to deserialize revision state node block")?
    {
        if child_name.is_empty() {
            return Err(CloneError::internal("Failed to deserialize node name"));
        }
        let child_relative_path = relative_path.push_into_buf(&child_name).freeze();
        let child_absolute_path = absolute_path.join(child_name);

        if !repository.filter.emit_excludes(
            &child_relative_path,
            child_node.is_directory(),
            FilterMode::View,
        ) {
            if child_node.is_file() {
                spawn_clone_file(
                    &mut tasks,
                    repository.clone(),
                    child_node,
                    child_absolute_path,
                    child_relative_path.clone(),
                    options.clone(),
                    stats.clone(),
                )
                .await;
            } else if child_node.is_link() {
                spawn_clone_link(
                    &mut tasks,
                    repository.clone(),
                    child_node,
                    child_absolute_path,
                    options.clone(),
                    stats.clone(),
                );
            } else if child_node.is_directory() {
                let result = spawn_clone_directory(
                    &mut tasks,
                    repository.clone(),
                    storage.clone(),
                    state.clone(),
                    child_id,
                    child_absolute_path,
                    child_relative_path.clone(),
                    options.clone(),
                    stats.clone(),
                )
                .await;
                failure = failure.or(result.err());
            }
        }

        while let Some(result) = tasks.try_join_next() {
            failure = failure.or(result
                .map_err(|e| CloneError::internal_with_context(e, "Recursion task failed"))
                .and_then(|r| r)
                .err());
        }
        if failure.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        failure = failure.or(result
            .map_err(|e| CloneError::internal_with_context(e, "Recursion task failed"))
            .and_then(|r| r)
            .err());
    }

    if let Some(err) = failure {
        execution_context()
            .failure
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Err(err)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn clone_child_node(
    repository: Arc<RepositoryContext>,
    storage: Arc<lore_transport::StorageSession>,
    state: Arc<State>,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    node: NodeID,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) -> Pin<Box<dyn Future<Output = Result<(), CloneError>> + Send>> {
    Box::pin(clone_node(
        repository,
        storage,
        state,
        absolute_path,
        relative_path,
        node,
        options,
        stats,
    ))
}

/// Ensure the parent directory of `path` exists; second and later files under the same parent hit the `DashSet` cache and skip the syscall.
async fn ensure_parent_dir(path: &Path, stats: &CloneStats) -> Result<(), CloneError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    // Hash raw OS-string bytes without allocation; case variants on Windows at worst trigger a redundant idempotent create_dir_all.
    let parent_hash = hash_string_bytes(parent.as_os_str().as_encoded_bytes());
    if stats.created_parents.contains(&parent_hash) {
        return Ok(());
    }
    tokio::fs::create_dir_all(parent)
        .await
        .internal_with(|| format!("Failed to create directory {}", parent.display()))?;
    stats.created_parents.insert(parent_hash);
    Ok(())
}

async fn clone_file(
    repository: Arc<RepositoryContext>,
    node: Node,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) -> Result<Option<(Hash, u64)>, CloneError> {
    let context = execution_context();
    let call = context.globals();
    let force = call.force();
    let metadata = tokio::fs::metadata(absolute_path.as_path()).await;
    if let Ok(metadata) = metadata {
        if options.ignore_existing {
            lore_trace!("Ignore existing file {}", absolute_path.display());
            return Ok(None);
        }

        // Check if the existing file matches what we will realize from state
        let (mtime, size) = util::fs::file_mtime_and_size(&metadata);
        if !is_file_modified(
            repository.clone(),
            &node,
            mtime,
            size,
            &relative_path,
            force,
        )
        .await
        .map_or(true, |(modified, _)| modified)
        {
            // Existing file is identical, just use it
            #[cfg(not(target_family = "windows"))]
            {
                // Skip on Windows: both helpers are no-ops there.
                let node_executable =
                    node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
                if node_executable != util::fs::file_is_executable(&metadata) {
                    util::fs::metadata_set_executable(
                        absolute_path.as_path(),
                        &metadata,
                        node_executable,
                    )
                    .await;
                }
            }

            lore_trace!("Retain {}", absolute_path.display());
            stats.complete.file_retain.fetch_add(1, Ordering::Relaxed);
            stats.complete.file_complete.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        if !force {
            lore_error!(
                "File already exist in file system and not identical {}",
                absolute_path.display()
            );
            return Err(CloneError::internal(format!(
                "File already exist in file system: {}",
                absolute_path.display()
            )));
        }
        if !call.dry_run() {
            let mut retry = util::fs::file_unlink_retry();
            while let Err(err) = util::fs::unlink_recursive(absolute_path.as_path()).await {
                lore_trace!(
                    "Unable to unlink local directory {}: {} (attempt {} of {})",
                    absolute_path.as_path().display(),
                    err,
                    retry.counter() + 1,
                    retry.limit()
                );
                if !retry.wait().await {
                    return Err(CloneError::internal(format!(
                        "Failed to force delete existing file {}",
                        absolute_path.as_path().display()
                    )));
                }
            }
        }
        stats.complete.file_replace.fetch_add(1, Ordering::Relaxed);
        lore_trace!("Replace {}", absolute_path.display());
    } else {
        lore_trace!("Create {}", absolute_path.display());
    }

    if !call.dry_run() {
        // Discovery no longer pre-creates dirs; create per-file parent just-in-time via the cache.
        ensure_parent_dir(absolute_path.as_path(), &stats).await?;

        // `read_into_file` returns the file's metadata when its single-fragment
        // path captures it on the open write handle; on that path we skip the
        // post-write stat entirely. Multi-fragment, mmap, and zero-size paths
        // still need a separate metadata query.
        let captured_metadata = if node.size > 0 {
            let (fragment, metadata) = immutable::read_into_file(
                repository.clone(),
                node.address,
                absolute_path.as_path(),
                read_options_from_repository(&repository),
            )
            .await
            .forward_with::<CloneError, _>(|| {
                format!("Failed to clone file {}", absolute_path.display())
            })
            .inspect_err(|_| {
                execution_context()
                    .failure
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            })?;
            stats
                .complete
                .bytes_transferred
                .fetch_add(fragment.size_content, Ordering::Relaxed);
            metadata
        } else {
            // Zero sized file, just create
            tokio::fs::OpenOptions::new()
                .read(false)
                .write(true)
                .truncate(true)
                .create(true)
                .open(absolute_path.as_path())
                .await
                .internal_with(|| format!("Failed to clone file {}", absolute_path.display()))?;
            None
        };

        let metadata = if let Some(metadata) = captured_metadata {
            metadata
        } else {
            tokio::fs::metadata(absolute_path.as_path())
                .await
                .internal_with(|| format!("Failed to clone file {}", absolute_path.display()))?
        };

        #[cfg(not(target_family = "windows"))]
        {
            // Skip on Windows: both helpers are no-ops there.
            let node_executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
            if node_executable != util::fs::file_is_executable(&metadata) {
                util::fs::metadata_set_executable(
                    absolute_path.as_path(),
                    &metadata,
                    node_executable,
                )
                .await;
            }
        }

        // Compute the (mtime_key, mtime) pair and return it; the caller
        // (`clone_execute`) collects pairs in a stack-local buffer and
        // fire-and-forgets a batched mutable-store write when the buffer fills,
        // so each `clone_file` task avoids awaiting its own bucket write.
        let key = state::file_modified_time_key(
            repository.salt(),
            repository.instance_id,
            relative_path.as_str(),
        );
        let mtime = util::fs::file_mtime(&metadata);

        stats.complete.file_complete.fetch_add(1, Ordering::Relaxed);
        return Ok(Some((key, mtime)));
    }

    stats.complete.file_complete.fetch_add(1, Ordering::Relaxed);

    Ok(None)
}

async fn spawn_clone_file(
    tasks: &mut JoinSet<Result<(), CloneError>>,
    repository: Arc<RepositoryContext>,
    node: Node,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) {
    let permit = Arc::clone(&stats.file_inflight)
        .acquire_owned()
        .await
        .expect("file_inflight semaphore closed unexpectedly");
    let mtime_partition = repository.id;
    let mtime_store = repository.try_mutable_store_arc();
    lore_spawn!(tasks, async move {
        let _permit = permit;
        stats.complete.file_count.fetch_add(1, Ordering::Relaxed);
        stats.file_inflight_count.fetch_add(1, Ordering::Relaxed);
        let result = clone_file(
            repository,
            node,
            absolute_path,
            relative_path,
            options,
            stats.clone(),
        )
        .await;
        stats.file_inflight_count.fetch_sub(1, Ordering::Relaxed);
        // Link sub-clones don't share the consumer-loop mtime batch; small
        // workload, so just inline-store the mtime here. Result is squashed
        // back to `()` so the JoinSet shape stays the same as elsewhere.
        match result {
            Ok(Some((key, mtime))) => {
                if let Some(store) = mtime_store {
                    state::file_modified_time_store_batch(
                        store,
                        mtime_partition,
                        vec![(key, mtime)],
                    )
                    .await;
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(err) => Err(err),
        }
    });
}

fn spawn_clone_link(
    tasks: &mut JoinSet<Result<(), CloneError>>,
    repository: Arc<RepositoryContext>,
    node: Node,
    absolute_path: PathBuf,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) {
    lore_spawn!(tasks, async move {
        let link = node.linked_node();
        let linked_repository_id = link.repository;
        let signature = link.revision;
        let link_node = link.node;

        lore_debug!("Resolve link {linked_repository_id} node {link_node}");
        let linked_repository = Arc::new(repository.to_link_context(linked_repository_id).await);
        if let Ok(link_remote) = linked_repository.remote().await {
            let options = options.clone();
            let stats = stats.clone();

            let correlation_id = execution_context().globals().correlation_id.to_string();
            if let Ok(link_storage) = link_remote
                .session(linked_repository.id, &correlation_id)
                .await
            {
                let link_state = State::deserialize(linked_repository.clone(), signature)
                    .await
                    .forward::<CloneError>("Failed to load revision state")?;

                let link_relative_path = link_state
                    .node_path(linked_repository.clone(), link_node)
                    .await
                    .forward::<CloneError>("Failed to resolve link path")?;
                let link_relative_path =
                    RelativePath::new_from_initial_path(link_relative_path.as_str())
                        .forward::<CloneError>("Failed to resolve link path")?;

                lore_info!(
                    "Clone link {} in {}",
                    linked_repository.id,
                    absolute_path.display()
                );
                tokio::fs::create_dir(absolute_path.as_path())
                    .await
                    .internal_with(|| {
                        format!("Failed to create directory {}", absolute_path.display())
                    })?;

                clone_child_node(
                    linked_repository,
                    link_storage,
                    link_state,
                    absolute_path,
                    link_relative_path,
                    link_node,
                    options,
                    stats,
                )
                .await?;
            } else {
                // TODO(mjansson): Differentiate between not authorized and other connection failures
                lore_debug!("Failed connecting to link remote storage, assume no access rights");
            }
        }

        Ok(())
    });
}

#[allow(clippy::too_many_arguments)]
async fn spawn_clone_directory(
    tasks: &mut JoinSet<Result<(), CloneError>>,
    repository: Arc<RepositoryContext>,
    storage: Arc<lore_transport::StorageSession>,
    state: Arc<State>,
    node: NodeID,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    options: Arc<CloneOptions>,
    stats: Arc<CloneStats>,
) -> Result<(), CloneError> {
    let inflight = stats
        .directory_inflight
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let future = async move {
        if !execution_context().globals().dry_run() {
            let result = tokio::fs::create_dir(absolute_path.as_path()).await;
            if result.is_err() && !absolute_path.is_dir() {
                stats
                    .directory_inflight
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                return Err(CloneError::internal(format!(
                    "Failed to create directory {}",
                    absolute_path.display()
                )));
            }
        } else {
            lore_info!("{}", absolute_path.display());
        }

        let result = clone_child_node(
            repository,
            storage,
            state,
            absolute_path,
            relative_path,
            node,
            options,
            stats.clone(),
        )
        .await;

        stats
            .directory_inflight
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        result
    };

    if inflight as usize > CLONE_DIRECTORY_MAX {
        future.await
    } else {
        lore_spawn!(tasks, future);
        Ok(())
    }
}

/*
static int
urc_repository_clone_module_in_path(urc_repository_t* repository, urc_state_t* state, uint32_t root_node,
                                    urc_path_t* local_path, urc_clone_context_t* context) {
    // TODO: This needs tracking and free on clone complete
    urc_clone_repository_context_t* repository_context = urc_calloc(1, sizeof(urc_clone_repository_context_t));
    repository_context->clone_context = context;
    repository_context->repository = repository;
    repository_context->state = state;

    urc_nametable_deserialize(repository->store, repository->id, state);

    urc_relative_path_t* relative_path = urc_relative_path_allocate();

    int err = urc_repository_clone_node(repository, state, root_node, local_path, relative_path, repository_context, 0);

    urc_relative_path_free(relative_path);

    return err;
}
*/
