// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::ops::BitAnd;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::branch;
use crate::change;
use crate::errors::*;
use crate::event;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::fs::filesystem_provider::DirectoryEntry;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::hash;
use crate::interface::LoreArray;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_limit_drain_tasks;
use crate::lore_trace;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::NodeLink;
use crate::node::ROOT_NODE;
use crate::path::emit_path_ignore;
use crate::progress::max_concurrent_stage_directory_tasks;
use crate::repository::BASE_SUFFIX;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::repository::THEIRS_SUFFIX;
use crate::repository::is_reserved_node_name;
use crate::revision::sync;
use crate::revision::sync::SyncRealizeStats;
use crate::state;
use crate::state::AddedSince;
use crate::state::ChainMark;
use crate::state::Ext;
use crate::state::LinkPin;
use crate::state::Mark;
use crate::state::NameChange;
use crate::state::NodeMapping;
use crate::state::OwnFlags;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::state::file_modified_against_node;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

/// Data for the event emitted when a stage operation begins.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageBeginEventData {
    /// Number of paths requested for staging.
    pub path_count: usize,
}

/// Running counts of items processed during a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageCountData {
    /// Number of directories staged as modified.
    pub directory_modify_count: u64,
    /// Number of directories staged as added.
    pub directory_add_count: u64,
    /// Number of directories staged as deleted.
    pub directory_delete_count: u64,
    /// Number of directories staged as moved.
    pub directory_move_count: u64,
    /// Number of files staged as modified.
    pub file_modify_count: u64,
    /// Number of files staged as added.
    pub file_add_count: u64,
    /// Number of files staged as deleted.
    pub file_delete_count: u64,
    /// Number of files staged as moved.
    pub file_move_count: u64,
    /// Total number of items processed.
    pub total_count: u64,
}

impl LoreFileStageCountData {
    pub fn new(stats: Arc<StageStats>) -> Self {
        let directory_modify_count = stats.directory_modify_count.load(Ordering::Relaxed);
        let directory_add_count = stats.directory_add_count.load(Ordering::Relaxed);
        let directory_delete_count = stats.directory_delete_count.load(Ordering::Relaxed);
        let directory_move_count = stats.directory_move_count.load(Ordering::Relaxed);
        let file_modify_count = stats.file_modify_count.load(Ordering::Relaxed);
        let file_add_count = stats.file_add_count.load(Ordering::Relaxed);
        let file_delete_count = stats.file_delete_count.load(Ordering::Relaxed);
        let file_move_count = stats.file_move_count.load(Ordering::Relaxed);
        Self {
            directory_modify_count,
            directory_add_count,
            directory_delete_count,
            directory_move_count,
            file_modify_count,
            file_add_count,
            file_delete_count,
            file_move_count,
            total_count: directory_modify_count
                + directory_add_count
                + directory_delete_count
                + directory_move_count
                + file_modify_count
                + file_add_count
                + file_delete_count
                + file_move_count,
        }
    }
}

/// Data for the progress event emitted periodically during a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageProgressEventData {
    /// Current counts of items processed.
    pub count: LoreFileStageCountData,
}

/// Data for the event emitted when a stage operation completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageEndEventData {
    /// Final counts of items processed.
    pub count: LoreFileStageCountData,
}

/// Data for the event identifying the repository and revision involved in a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageRevisionEventData {
    /// Identifier of the repository.
    pub repository: RepositoryId,
    /// Revision the files are staged against.
    pub revision: Hash,
}

/// Data for the event emitted for each file affected by a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageFileEventData {
    /// Previous path of the file, when it was moved, relative to the root of the working tree.
    pub from_path: LoreString,
    /// Path of the file, relative to the root of the working tree.
    pub path: LoreString,
    /// Action applied to the file.
    pub action: LoreFileAction,
}

#[error_set]
pub enum StageError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    BranchAlreadyExists,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    NotConnected,
    NothingStaged,
    BranchAdvanced,
    Conflict,
    InvalidArguments,
    AlreadyLinked,
    LayerNotFound,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    LinkPathNotFound,
    NotALink,
    NotALayer,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    LocalModifications,
    MaxHistorySearchDepth,
    IdenticalMetadata,
    LockNotFound,
    LockNotOwned,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl crate::event::EventError for StageError {}

#[derive(Default)]
pub struct StageStats {
    pub directory_modify_count: AtomicU64,
    pub directory_add_count: AtomicU64,
    pub directory_delete_count: AtomicU64,
    pub directory_move_count: AtomicU64,
    pub directory_copy_count: AtomicU64,
    pub file_modify_count: AtomicU64,
    pub file_add_count: AtomicU64,
    pub file_delete_count: AtomicU64,
    pub file_move_count: AtomicU64,
    pub file_copy_count: AtomicU64,
    pub link_modify_count: AtomicU64,
    pub link_add_count: AtomicU64,
    pub link_remove_count: AtomicU64,
    directory_checked_count: AtomicU64,
    file_checked_count: AtomicU64,
    task_count: AtomicU64,
}

impl StageStats {
    /// Counts one node staged under `action`, against the directory tally or the file one as
    /// its kind asks.
    fn record_action(&self, action: LoreFileAction, is_directory: bool) {
        let (directory, file) = match action {
            LoreFileAction::Add => (&self.directory_add_count, &self.file_add_count),
            LoreFileAction::Copy => (&self.directory_copy_count, &self.file_copy_count),
            LoreFileAction::Delete => (&self.directory_delete_count, &self.file_delete_count),
            LoreFileAction::Keep => (&self.directory_modify_count, &self.file_modify_count),
            LoreFileAction::Move => (&self.directory_move_count, &self.file_move_count),
        };
        if is_directory { directory } else { file }.fetch_add(1, Ordering::Relaxed);
    }
}

/// How a change in path letter case is handled during staging.
#[derive(Debug, Default, Clone, Copy)]
#[repr(u32)]
pub enum StageCaseChange {
    /// Default, exit with error if case change is detected
    #[default]
    Error = 0,
    /// Treat case change as unintentional, updating the file system to match the repository (a "keep" operation)
    Keep = 1,
    /// Treat case change as a rename, updating the repository tree to match the file system (a "rename" operation)
    Rename = 2,
}

impl StageCaseChange {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Keep,
            2 => Self::Rename,
            _ => Self::Error,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StageOptions {
    /// Case change handling mode
    pub case_change: StageCaseChange,
    /// Additional node flags
    pub node_flags: NodeFlags,
    /// Optional file ID
    pub file_id: Option<Context>,
    /// Do not stage any child nodes if set (no recursion)
    pub no_children: bool,
    /// Force a recursive filesystem scan for directory paths.
    ///
    /// Has no effect on individual file paths — those are always reconciled
    /// against the filesystem regardless of this flag.
    ///
    /// When `false` (default), directory paths stage only the files and child
    /// directories currently marked dirty in the repository state. When
    /// `true`, directory paths are walked recursively on the filesystem and
    /// every file is reconciled, ignoring the dirty flags. A forced stage walks
    /// them whatever this holds.
    pub scan: bool,
}

/// Process links that need reserialization after staging operations
pub(crate) async fn process_link_updates(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_current: Arc<State>,
    state: Arc<State>,
    link_tracker: Arc<link::LinkTracker>,
) -> Result<(), StageError> {
    link::drain_link_tracker(repository, token, state_current, state, &link_tracker, true)
        .await
        .forward::<StageError>("Failed to update link")
}

/// Entries a walk leaves for [`state::apply_pending_discards`] to drop once it has
/// drained: those found behind a nested-repository boundary, and those neither the
/// file system nor any commit holds.
///
/// Collected rather than discarded where they are found: the discard rewrites the
/// sibling chains the walk's own tasks are still reading. The ids index one state,
/// so a walk crossing into a linked state carries `None` from there on, as does a
/// caller that reconciles nothing against the file system: a boundary is still
/// skipped and an absent entry is staged as a delete, only an entry this would
/// drop is left in place.
pub(crate) type PendingDiscards = Option<Arc<DiscardQueue>>;

/// See [`PendingDiscards`].
pub(crate) struct DiscardQueue {
    /// The revision the staged tree is based on, which holds every entry a
    /// commit covers.
    committed: Arc<State>,
    nodes: parking_lot::Mutex<Vec<NodeID>>,
}

impl DiscardQueue {
    /// An empty queue for a staged tree based on `committed`.
    pub(crate) fn new(committed: Arc<State>) -> Self {
        Self {
            committed,
            nodes: parking_lot::Mutex::new(Vec::new()),
        }
    }

    fn push(&self, node: NodeID) {
        self.nodes.lock().push(node);
    }

    /// The entries queued, leaving the queue empty.
    pub(crate) fn take(&self) -> Vec<NodeID> {
        std::mem::take(&mut *self.nodes.lock())
    }
}

/// Whether the committed tree holds `node`, which the staged tree holds at `id`.
///
/// Decided by the node's slot rather than its name. The staged tree is derived from the
/// committed one and never moves a node to another slot, so a committed node keeps its id
/// however it has since been respelled, moved or staged for delete, and one of the same name
/// staging added beside it, as a type change does, takes a slot of its own. The kind has to
/// match as well, since a slot the staged tree freed can be reused. The file id is not
/// compared: syncs and merges overwrite a committed file's address, its file id included.
///
/// A slot past the committed tree's blocks is not committed. A block within them that fails to
/// load fails the call.
async fn is_committed(
    committed: &State,
    repository: &Arc<RepositoryContext>,
    id: NodeID,
    node: &Node,
) -> Result<bool, StageError> {
    if id == ROOT_NODE {
        return Ok(true);
    }
    let block_index = NodeBlock::index(id);
    let block_count = committed
        .tree(repository.clone())
        .await
        .forward::<StageError>("Failed to read the committed tree")?
        .block_count as usize;
    if block_index >= block_count {
        return Ok(false);
    }
    let block = committed
        .block(repository.clone(), block_index)
        .await
        .forward::<StageError>("Failed to read the committed node block")?;
    let reader = block.read();
    let index = Node::index(id);
    Ok(reader.is_node_in_use(index) && reader.node(index).node_type() == node.node_type())
}

/// Whether `node`, which the staged tree holds at `id`, and its subtree can leave the staged tree
/// instead of being staged for delete: none of it is committed, a link or a merge entry, which
/// carry a registry entry and flags only a staged delete settles.
pub(crate) async fn drops_cleanly(
    committed: &State,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    id: NodeID,
    node: &Node,
) -> Result<bool, StageError> {
    if !drops_alone(committed, repository, id, node).await? {
        return Ok(false);
    }
    if !node.is_directory() {
        return Ok(true);
    }

    let mut directories = vec![id];
    while let Some(directory) = directories.pop() {
        let mut children =
            StateNodeChildrenIterator::new(state.clone(), repository.clone(), directory)
                .await
                .forward::<StageError>("Failed to list the staged directory")?;
        while let Some((child, child_node)) = children
            .next()
            .await
            .forward::<StageError>("Failed to list the staged directory")?
        {
            if !drops_alone(committed, repository, child, &child_node).await? {
                return Ok(false);
            }
            if child_node.is_directory() {
                directories.push(child);
            }
        }
    }
    Ok(true)
}

/// Whether `node`, which the staged tree holds at `id`, drops cleanly, leaving aside what is
/// below it.
async fn drops_alone(
    committed: &State,
    repository: &Arc<RepositoryContext>,
    id: NodeID,
    node: &Node,
) -> Result<bool, StageError> {
    Ok(!node.is_link()
        && !node.is_staged_merge()
        && !is_committed(committed, repository, id, node).await?)
}

/// Whether the file system holds `path` in any spelling that folds to it, which is how a walk
/// matches entries to nodes. Only a path found missing is absent: a probe that fails for another
/// reason counts as present, so nothing is dropped on it.
async fn is_on_disk(operation: &InstanceOperationImpl, path: &RelativePath) -> bool {
    !matches!(
        util::fs::filesystem_path_and_info(operation, "", path, None).await,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

/// The staged node to drop for `path`, which the staged tree holds as `node`: of the nodes from
/// `node` up to the first ancestor that is committed or on disk, the shallowest that
/// [drops cleanly](drops_cleanly). `None` where `node` itself is committed or on disk, or where
/// none drops cleanly, which leaves a delete to stage.
///
/// `path` is taken from the root of the state `node` indexes, so it crosses no link. `leaf` is
/// the node `node` names.
#[allow(clippy::too_many_arguments)]
async fn uncommitted_absent_root(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    committed: &State,
    node: NodeID,
    leaf: Node,
    path: &RelativePath,
) -> Result<Option<NodeID>, StageError> {
    if is_committed(committed, repository, node, &leaf).await? || is_on_disk(operation, path).await
    {
        return Ok(None);
    }

    let mut parent = leaf.parent;
    let mut chain = vec![(node, leaf)];
    let mut ancestor_path = path.clone();
    while parent != ROOT_NODE {
        ancestor_path.pop_name();
        let parent_node = state
            .node(repository.clone(), parent)
            .await
            .forward::<StageError>("Failed to resolve the staged node")?;
        if is_committed(committed, repository, parent, &parent_node).await?
            || is_on_disk(operation, &ancestor_path).await
        {
            break;
        }
        chain.push((parent, parent_node));
        parent = parent_node.parent;
    }

    for &(id, ref candidate) in chain.iter().rev() {
        if drops_cleanly(committed, repository, state, id, candidate).await? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Whether no commit covers the child the parent tree holds as `held`, which is
/// what makes a `.lore/` inside it a nested working copy rather than parent
/// content.
///
/// A child the parent committed stays part of the parent's tree once a `.lore/`
/// appears inside it, since untracking committed content is an explicit user
/// action, and so does a staged add, matching the scan. `None` is a name the tree
/// holds no child for, which no commit covers. Answered before the boundary is
/// probed, so the file system is left alone for every child already in the tree.
async fn is_uncommitted_child(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    held: Option<NodeID>,
) -> Result<bool, StageError> {
    let Some(held) = held else {
        return Ok(true);
    };

    let node = state
        .node(repository.clone(), held)
        .await
        .forward::<StageError>("Failed to resolve the child node")?;

    Ok(node.is_dirty_add())
}

/// Stage changes from the file system into `base`'s state.
///
/// `remainder_path` names the target below `base`, as `resolve_link_chain` names the same pair, and
/// is the only part checked for case consistency. A mounted state starts at its own root and spells
/// its nodes from there, so the base is named by its node while the walk carries the path the file
/// system, the filter and the reported changes all answer for.
///
/// The filter is asked about the path as what the working tree holds there, else as the node the
/// tree holds, a link as a directory, and as a directory where neither holds anything.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_filesystem_path(
    operation: Arc<InstanceOperationImpl>,
    base: NodeMapping,
    remainder_path: RelativePath,
    stats: Arc<StageStats>,
    options: StageOptions,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    layer_mask: Option<Arc<Vec<String>>>,
    prefixes: Option<Arc<util::fs::ResolvedPrefixes>>,
    discards: PendingDiscards,
) -> Result<NodeLink, StageError> {
    let repository = base.repository.clone();
    let state = base.state.clone();

    lore_trace!(
        "Staging path: {}/{}",
        base.path.as_str(),
        remainder_path.as_str(),
    );

    let (remainder_path, resolved_info) = if remainder_path.is_empty() {
        (remainder_path, None)
    } else {
        let resolved = util::fs::filesystem_path_and_info(
            &operation,
            base.path.as_str(),
            &remainder_path,
            prefixes.as_deref(),
        )
        .await;
        match resolved {
            Ok((resolved, resolved_info)) => (resolved, resolved_info),
            Err(_) => (remainder_path, None),
        }
    };

    let full_relative_path = if base.path.is_empty() {
        remainder_path.clone()
    } else {
        RelativePath::new_from_clean_parts(base.path.as_str(), remainder_path.as_str())
    };

    let staged_info = if let Some(info) = resolved_info {
        Some(info)
    } else {
        // A path the operation cannot describe is an error and not a missing path: reading it as
        // missing would stage the deletion of a file that is there.
        let info = operation
            .file_info(&full_relative_path)
            .await
            .forward_any::<StageError>("Failed to describe a path to stage")?;
        info.exists().then_some(info)
    };

    let force = execution_context().globals().force();
    if let Some(info) = staged_info {
        // The tree is searched only for a path the filter would drop, since a path the tree tracks
        // as the kind the working tree holds is past the ignore slot.
        let excluded = !force
            && repository.filter.excludes_tree(
                &full_relative_path,
                info.is_dir(),
                FilterMode::Full,
            )
            && {
                let tracked = state
                    .relative_path_tracked_as(
                        repository.clone(),
                        base.node,
                        remainder_path.as_str(),
                    )
                    .await
                    .forward::<StageError>("Failed to read the revision the tree was built on")?
                    == Some(info.is_dir());
                repository.filter.emit_excludes(
                    &full_relative_path,
                    info.is_dir(),
                    FilterMode::TRACKED.at(tracked),
                )
            };
        if excluded {
            lore_trace!("Path excluded by filter: {}", full_relative_path.as_str());
            let (replaced_states, replaced_excluded) = repository.filter.child_excludes_tree(
                repository
                    .filter
                    .parent_exclusion_states(&full_relative_path),
                &full_relative_path,
                !info.is_dir(),
                FilterMode::Full,
            );
            if !replaced_excluded {
                Box::pin(async {
                    let Ok((node_link, None)) = state
                        .find_relative(repository.clone(), base.node, remainder_path.as_str())
                        .await
                    else {
                        return Ok(());
                    };
                    stage_replaced_node(
                        repository.clone(),
                        state.clone(),
                        full_relative_path.clone(),
                        KnownChild::Resolved(Some(node_link.node)),
                        base.node,
                        !info.is_dir(),
                        replaced_states,
                        options.node_flags,
                        stats.clone(),
                        link_tracker.clone(),
                    )
                    .await
                })
                .await?;
            }
            return Ok(NodeLink::invalid());
        }

        if info.is_dir() {
            lore_trace!("Stage directory: {}", full_relative_path.as_str());
        } else if info.is_file() {
            lore_trace!("Stage file: {}", full_relative_path.as_str());
        } else {
            return Err(StageError::internal(format!(
                "Failed to stage path {full_relative_path}, unsupported type"
            )));
        }

        let mut descent = StageDescent::at(&base)?;
        if let Some(node_link) = descent
            .descend(
                &operation,
                remainder_path,
                info,
                options,
                &stats,
                link_tracker.as_ref(),
            )
            .await?
        {
            return Ok(node_link);
        }

        // Finally, if the given path is a directory we should recurse and stage everything below it
        if !options.no_children && (descent.node == ROOT_NODE || info.is_dir()) {
            stats.task_count.fetch_add(1, Ordering::Release);

            let result = stage_directory(
                operation.clone(),
                descent.repository.clone(),
                descent.state.clone(),
                descent.absolute_path.as_path(),
                descent.relative_path,
                descent.node,
                1,
                options,
                stats.clone(),
                link_tracker.clone(),
                layer_mask.clone(),
                discards
                    .clone()
                    .filter(|_| descent.repository.id == repository.id),
                descent.states,
            )
            .await;
            stats.task_count.fetch_sub(1, Ordering::Release);
            if !result?
                && descent.unkept != INVALID_NODE
                && descent.repository.id == repository.id
                && let Some(queue) = discards.as_deref()
            {
                discard_unkept(
                    &descent.repository,
                    &descent.state,
                    descent.node,
                    descent.unkept,
                    queue,
                    &stats,
                )
                .await?;
                return Ok(NodeLink::default());
            }
        }

        return Ok(NodeLink {
            node: descent.node,
            repository: descent.repository.id,
            revision: descent.state.revision(),
        });
    }

    // Treat all errors as a non-existing file/directory. Otherwise if a subpath of the queried path
    // is a file we will get platform specific not-a-directory error that cannot be platform independently
    // matched along with not-found errors.
    lore_debug!(
        "Path not found, staging delete: {}/{}",
        repository.path_for_display(),
        full_relative_path.as_str(),
    );
    // The tree is searched before the filter is asked, since a path the tree tracks is past the
    // ignore slot.
    let filter = &repository.filter;
    let mode = FilterMode::Full.absence(force);
    let parent_states = filter.parent_exclusion_states(&full_relative_path);

    let Ok((node_link, linked)) = state
        .find_relative(repository.clone(), base.node, remainder_path.as_str())
        .await
    else {
        if filter
            .child_emit_excludes(parent_states, &full_relative_path, true, mode)
            .1
        {
            lore_trace!("Path excluded by filter: {}", full_relative_path.as_str());
            return Ok(NodeLink::invalid());
        }
        lore_debug!("Path {} does not exist in repository", full_relative_path);
        if operation
            .unversioned_symlink_at(&full_relative_path)
            .await
            .forward::<StageError>("Failed to read the path")?
        {
            lore_trace!("Path is a symbolic link, which is not versioned: {full_relative_path}");
            crate::path::emit_path_ignore(full_relative_path.as_str()).await;
            return Ok(NodeLink::default());
        }
        if !force {
            return Err(StageError::internal(format!(
                "Invalid path {full_relative_path}"
            )));
        }
        lore_debug!("Non-existing path ignored by force flag");
        return Ok(NodeLink::default());
    };
    let (current_repository, node_state) =
        linked.unwrap_or_else(|| (repository.clone(), state.clone()));
    // A block of its own, so the future holds the node only until the discard check.
    let (held, states) = {
        let Some((node, states)) = absent_named_node(
            filter,
            &current_repository,
            &node_state,
            node_link.node,
            &full_relative_path,
            parent_states,
            force,
        )
        .await?
        else {
            lore_trace!("Path excluded by filter: {}", full_relative_path.as_str());
            return Ok(NodeLink::invalid());
        };

        if node_link.repository == repository.id
            && let Some(queue) = discards.as_deref()
            && let Some(root) = uncommitted_absent_root(
                &operation,
                &repository,
                &state,
                &queue.committed,
                node_link.node,
                node,
                &full_relative_path,
            )
            .await?
        {
            lore_debug!("Path {full_relative_path} was never committed, dropping it");
            queue.push(root);
            return Ok(NodeLink::default());
        }
        (NodeFlags::from_bits_retain(node.flags), states)
    };

    // Without its children a missing directory is only located: deleting it would delete
    // everything below it, where the caller stages only some of that.
    if options.no_children {
        return Ok(if node_link.repository == repository.id {
            node_link
        } else {
            NodeLink::default()
        });
    }

    if let Some(ref tracker) = link_tracker
        && node_link.repository != repository.id
    {
        // Record a tracker context for every link crossed to reach the
        // deleted path, so a delete nested two or more levels deep folds
        // its pin up through all intermediate links (not just one level).
        let chain = crate::link::resolve_link_chain(
            base.clone(),
            state.clone(),
            remainder_path.clone(),
            BranchId::default(),
        )
        .await
        .forward::<StageError>("Failed to resolve link chain")?;

        chain.record_tracker_contexts(tracker, &node_state);
    }

    lore_debug!(
        "Path {} exist in repository, stage deletion",
        full_relative_path
    );
    stage_absent_delete(
        current_repository,
        node_state,
        full_relative_path,
        node_link.node,
        held,
        options.node_flags,
        stats,
        link_tracker,
        states,
        force,
    )
    .await?;

    Ok(NodeLink::default())
}

/// The node `state` holds at `id`, which the working tree lacks at the named `path`, with the
/// filter verdict at it from `parent`, or `None` where `filter` leaves it out. The node is asked
/// about as what it is, and the ignore rules leave it out where the tree only marks it, unless
/// `force` puts the stage past them.
///
/// A function of its own, so the node sits in the caller's future only across the discard check
/// that follows, rather than beside the tree's answer too.
async fn absent_named_node(
    filter: &Filter,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    id: NodeID,
    path: &RelativePath,
    parent: FilterStates,
    force: bool,
) -> Result<Option<(Node, FilterStates)>, StageError> {
    let node = state
        .node(repository.clone(), id)
        .await
        .forward::<StageError>("Failed to resolve the staged node")?;
    let is_directory = Node::asked_as_directory(Some(&node));
    let flags = NodeFlags::from_bits_retain(node.flags);
    let mode = filter
        .tracked_mode(
            parent,
            path,
            is_directory,
            FilterMode::TRACKED.absence(force),
            async || state.tracks(repository, id, flags).await,
        )
        .await
        .forward::<StageError>("Failed to read the revision the tree was built on")?;
    let (states, excluded) = filter.child_emit_excludes(parent, path, is_directory, mode);
    Ok((!excluded).then_some((node, states)))
}

/// Discards `unkept`, the first of the directories a descent added from it down to `node` that
/// hold nothing kept, and takes back the adds the descent recorded for them.
async fn discard_unkept(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    mut node: NodeID,
    unkept: NodeID,
    queue: &DiscardQueue,
    stats: &StageStats,
) -> Result<(), StageError> {
    let mut adds = 1;
    while node != unkept && node != ROOT_NODE {
        node = state
            .node(repository.clone(), node)
            .await
            .forward::<StageError>("Failed to resolve an added directory")?
            .parent;
        adds += 1;
    }
    stats.directory_add_count.fetch_sub(adds, Ordering::Relaxed);
    queue.push(unkept);
    Ok(())
}

/// The node a staged path's descent has reached, in the repository and state holding it past
/// any link crossed, with the paths and the filter verdict naming it.
struct StageDescent {
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node: NodeID,
    relative_path: RelativePathBuf,
    absolute_path: PathBuf,
    states: FilterStates,
    /// The first of the directories ending the descent that an unforced stage added and the filter
    /// excludes, which stand only for what is kept below them, or [`INVALID_NODE`].
    unkept: NodeID,
}

impl StageDescent {
    /// A descent standing at `base`, with the filter verdict folded once for its path, which the
    /// descent steps a component at a time from there.
    ///
    /// Not part of [`stage_filesystem_path`]: the path it borrows to fold the verdict would be
    /// reserved there across the directory walk.
    fn at(base: &NodeMapping) -> Result<Self, StageError> {
        let relative_path = base.path.to_buf_with_capacity(RelativePath::COMPONENT_ROOM);
        let repository_root = base.repository.require_path()?;
        Ok(Self {
            repository: base.repository.clone(),
            state: base.state.clone(),
            node: base.node,
            absolute_path: if base.path.is_empty() {
                repository_root.to_path_buf()
            } else {
                base.path
                    .to_absolute_path(repository_root)
                    .forward::<StageError>("placing the staged path below the root")?
            },
            states: base.repository.filter.exclusion_states(&relative_path),
            relative_path,
            unkept: INVALID_NODE,
        })
    }

    /// Stage each component of `remainder_path` in turn and step into the node it names, and
    /// into the linked state where that node is a link.
    ///
    /// Each component is staged under the name `remainder_path` spells it with, which the caller
    /// resolves against the file system, so `stage_node_from_metadata` does not look up its case
    /// again. The final component is the staged path, staged as `info`; every other one is staged
    /// as a directory. `Some` is the link of a component that was not staged, which ends the
    /// descent.
    ///
    /// Its own future: inline, what a step holds across its awaits would be reserved in the
    /// directory walk's state of [`stage_filesystem_path`].
    async fn descend(
        &mut self,
        operation: &Arc<InstanceOperationImpl>,
        mut remainder_path: RelativePath,
        info: FileInfo,
        options: StageOptions,
        stats: &Arc<StageStats>,
        link_tracker: Option<&Arc<crate::link::LinkTracker>>,
    ) -> Result<Option<NodeLink>, StageError> {
        while !remainder_path.is_empty() {
            let is_final_component = remainder_path.parent().is_none();
            let current_name = remainder_path.pop_root();
            if current_name == "." {
                continue;
            }

            let current_info = if is_final_component {
                info
            } else {
                FileInfo::Directory
            };

            // A named path is refused rather than skipped: the caller asked for
            // this path specifically, and staging it into the parent would take
            // content the nested repository owns.
            if current_info.is_dir() {
                let held = self
                    .state
                    .find_subnode(
                        self.repository.clone(),
                        self.node,
                        hash::hash_string(current_name),
                    )
                    .await
                    .ok()
                    .filter(|node| node.is_valid_node_id());
                if is_uncommitted_child(&self.repository, &self.state, held).await?
                    && state::is_nested_repository_root(&mut self.absolute_path, current_name).await
                {
                    return Err(StageError::internal(format!(
                        "Failed to stage path {}, path is a nested repository",
                        self.absolute_path.join(current_name).display()
                    )));
                }
            }

            let StagedChild {
                link: node_link,
                states,
                added,
                prepended: _,
            } = stage_node_from_metadata(
                operation,
                NodeMapping {
                    repository: self.repository.clone(),
                    state: self.state.clone(),
                    path: self.relative_path.clone().freeze(),
                    node: self.node,
                },
                current_name.to_string(),
                current_info,
                options,
                stats.clone(),
                link_tracker.cloned(),
                KnownChild::Unresolved,
                self.states,
            )
            .await?;
            if added {
                stats.record_action(LoreFileAction::Add, true);
            }
            if added && states.excluded(FilterMode::Full) && !execution_context().globals().force()
            {
                if self.unkept == INVALID_NODE {
                    self.unkept = node_link.node;
                }
            } else {
                self.unkept = INVALID_NODE;
            }

            if !node_link.is_valid() {
                return Ok(Some(node_link));
            }

            // Scoped so the node_name_ref read lock drops before node() below; a
            // second shared lock on the same block deadlocks behind a queued writer.
            {
                let final_name = self
                    .state
                    .node_name_ref(self.repository.clone(), node_link.node)
                    .await
                    .forward::<StageError>("Failed to resolve node name")?;
                self.absolute_path.push(&*final_name);
                self.relative_path.push(&final_name);
            }

            self.states = states;

            let node = self
                .state
                .node(self.repository.clone(), node_link.node)
                .await
                .forward::<StageError>(
                    "Node not found in child node list, inconsistent repository state",
                )?;

            self.node = node_link.node;

            // Transition into the link
            if node.is_link() {
                let link_repository_id: RepositoryId = node.address.context.into();
                let link_revision = node.address.hash;
                let linked_node = node.child;

                lore_debug!(
                    "Transition into link with ID {link_repository_id} at revision {link_revision}"
                );

                let linked_repository = self.repository.to_link_context(link_repository_id).await;
                let mut linked_state = State::deserialize(self.repository.clone(), link_revision)
                    .await
                    .forward::<StageError>("Failed to deserialize revision state")?;

                // Track this link for potential reserialization
                if let Some(tracker) = link_tracker {
                    // Reuse potentially existing link state for same repository
                    linked_state = if let Some(existing_context) =
                        tracker.find_link_context(link_repository_id)
                    {
                        existing_context.link_state.clone()
                    } else {
                        linked_state.clone()
                    };

                    let link_context = link::LinkContext {
                        link_repository_id,
                        link_node_id: node_link.node,
                        parent_repository_id: self.repository.id,
                        link_state: linked_state.clone(),
                    };

                    tracker.add_link(link_context);
                }

                self.repository = linked_repository;
                self.state = linked_state;
                self.node = linked_node;
            }
        }

        Ok(None)
    }
}

#[lore_macro::test_pub]
pub(crate) async fn stage_single_node(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    node: Node,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    filter_mode: FilterMode,
) -> Result<NodeLink, StageError> {
    // Ensure old hierarchies are not imported
    let mut node = node;
    if !node.is_link() {
        node.child = 0;
    }
    node.sibling = 0;

    lore_debug!(
        "Staging single path from node: {} : {:?}",
        relative_path.as_str(),
        node
    );

    let force = execution_context().globals().force();
    if !force
        && repository
            .filter
            .emit_excludes(&relative_path, !node.is_file(), filter_mode)
    {
        lore_trace!("Path excluded by filter: {}", relative_path.as_str());
        return Ok(NodeLink::invalid());
    }

    let mut parent_path = relative_path.clone();
    parent_path.pop();

    let base_node = state
        .find_node_link(repository.clone(), parent_path.as_str())
        .await
        .forward_with::<StageError, _>(|| format!("Invalid path {parent_path}"))?;
    if !base_node.is_valid_or_root() {
        return Err(StageError::internal(format!("Invalid path {parent_path}")));
    }

    let name = relative_path.name();
    node.name_hash = hash::hash_string(name);

    let node_flags = NodeFlags::from_bits_retain(node.flags);

    let (repository, state) = base_node
        .resolve(repository, state)
        .await
        .forward::<StageError>("Failed to resolve node path in state")?;

    match state
        .find_subnode(repository.clone(), base_node.node, node.name_hash)
        .await
    {
        Ok(found_node) => {
            // Overwrite existing node
            let block_index = NodeBlock::index(found_node);
            let node_index = Node::index(found_node);
            let block = state
                .block(repository.clone(), block_index)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;

            // What the update needs of the existing node, read where no await holds it.
            let (same_type, place, resized, link) = {
                let existing_node = block.node(node_index);
                lore_debug!(
                    "Found previous node {} flags 0x{:x}",
                    found_node,
                    existing_node.flags
                );
                (
                    node_flags.node_type() == existing_node.node_type(),
                    (base_node.node, existing_node.name_hash),
                    (existing_node.size, existing_node.mode) != (node.size, node.mode),
                    existing_node.is_link(),
                )
            };

            if same_type {
                // Update the existing node: a link takes the whole pin, its node in the linked
                // State included (H17); any other node its content.
                if link {
                    state
                        .node_set_link_target_in(
                            &block,
                            found_node,
                            LinkPin::Whole {
                                address: node.address,
                                child: node.child,
                            },
                        )
                        .forward::<StageError>("Failed to repin the link")?;
                    if resized {
                        state.node_change_in_place(&block, found_node, place, |held| {
                            held.set_size(node.size);
                            held.set_mode(node.mode);
                            true
                        });
                    }
                } else {
                    state.node_change_in_place(&block, found_node, place, |held| {
                        held.overwrite_content(node.address, node.size, node.mode);
                        true
                    });
                }

                state
                    .node_mark(
                        &repository,
                        found_node,
                        Mark::staged(node_flags.bitand(NodeFlags::StagedBits), true),
                    )
                    .await
                    .forward::<StageError>("Failed to mark node as staged")?;

                if let Some(ref tracker) = link_tracker {
                    tracker.on_node_changed(&repository);
                }

                if node.is_directory() {
                    stats.directory_modify_count.fetch_add(1, Ordering::Relaxed);
                } else if node.is_link() {
                    stats.link_modify_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.file_modify_count.fetch_add(1, Ordering::Relaxed);
                }

                return Ok(NodeLink {
                    node: found_node,
                    repository: repository.id,
                    revision: state.revision(),
                });
            }

            // Type mismatch, stage the current node for delete and add a new node
            lore_debug!(
                "Directory/file/link different, stage delete of existing node and recreate new node"
            );
            stage_delete(
                repository.clone(),
                state.clone(),
                relative_path.clone(),
                found_node,
                node_flags.bitand(NodeFlags::StagedBits),
                stats.clone(),
                link_tracker.clone(),
            )
            .await?;

            // Fall through to create a new node
        }
        Err(e) if e.is_node_not_found() => {
            // Fall through to create a new node
        }
        Err(err) if is_refusal(&err) => {
            return Err(err).forward::<StageError>("finding the subnode");
        }
        Err(err) => {
            return Err(StageError::internal_with_context(
                err,
                "Failed to find subnode",
            ));
        }
    }

    // Node did not exist or was replaced, add a new node
    let node_id = state
        .node_add(repository.clone(), base_node.node, node, name)
        .await
        .forward::<StageError>("Failed to add a node to revision tree")?;

    let mark_flags = if node_flags.contains(NodeFlags::StagedMove) {
        node_flags
    } else {
        node_flags | NodeFlags::StagedAdd
    };
    state
        .node_mark(&repository, node_id, Mark::staged(mark_flags, true))
        .await
        .forward::<StageError>("Failed to mark node as staged")?;

    if let Some(ref tracker) = link_tracker {
        tracker.on_node_changed(&repository);
    }

    lore_trace!("Staged new node {node_id} for {relative_path}");

    if node.is_directory() {
        stats.directory_add_count.fetch_add(1, Ordering::Relaxed);
    } else if node.is_link() {
        stats.link_add_count.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.file_add_count.fetch_add(1, Ordering::Relaxed);
    }

    Ok(NodeLink {
        node: node_id,
        repository: repository.id,
        revision: state.revision(),
    })
}

/// Stage the given nodes as merged. Requires all paths to be repository relative paths.
pub(crate) async fn stage_merge_path(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_stage: Arc<State>,
    state_merge: Arc<State>,
    relative_path: RelativePath,
    stats: Arc<StageStats>,
) -> Result<(), StageError> {
    lore_debug!(
        "Staging merge path: {}/{}",
        repository.path_for_display(),
        relative_path.as_str(),
    );

    let diff = Box::pin(branch::diff3_collect(
        repository.clone(),
        state_merge.branch(repository.clone()).await,
        state_merge.revision(),
        state_stage.branch(repository.clone()).await,
        // The self parent of the staged state is the current revision
        state_stage.parent_self(),
        Some(relative_path),
        true,  /* Include identical changes for merge tracking */
        false, /* Do not autoresolve */
    ))
    .await
    .forward::<StageError>("Failed to calculate branch revision diff")?;

    lore_debug!(
        "Branch diff found {} changes and {} conflicts",
        diff.changes.len(),
        diff.conflicts.len()
    );

    // For each diff, check if the conflicts are already realized
    // If so, mark as in conflict
    // If not, mark as merged
    for change in diff.changes.iter() {
        lore_debug!(
            "Merge change: {} (node {})",
            change.path(),
            change.to.mapping.node
        );

        let mut merge_flags = NodeFlags::StagedMerge;

        if sync::exist_merge_artifacts(&operation, change.path()).await
            || (change.action != change::FileAction::Delete
                && change.to.flags.contains(NodeFlags::File)
                && operation
                    .infer_is_conflicted(change.path())
                    .await
                    .forward_with::<StageError, _>(|| {
                        format!("checking {} for conflict markers", change.path())
                    })?)
        {
            lore_debug!(
                "Merge change filesystem state is conflicted: {}",
                change.path()
            );
            merge_flags |= NodeFlags::StagedMergeConflict;
        }

        state_stage
            .node_mark(
                &repository,
                change.to.mapping.node,
                Mark::staged(merge_flags, true),
            )
            .await
            .forward::<StageError>("Failed to mark node as staged")?;

        stats.record_action(
            LoreFileAction::from(change.action),
            change.resolved_side().flags.is_directory(),
        );
    }

    for conflict in diff.conflicts.iter() {
        let change = &conflict.1;
        lore_debug!(
            "Merge conflict: {} (node {})",
            change.path(),
            change.to.mapping.node
        );

        let merge_flags = NodeFlags::StagedMergeConflict;

        state_stage
            .node_mark(
                &repository,
                change.to.mapping.node,
                Mark::staged(merge_flags, true),
            )
            .await
            .forward::<StageError>("Failed to mark node as staged")?;

        stats.record_action(
            LoreFileAction::from(change.action),
            change.resolved_side().flags.is_directory(),
        );
    }

    Ok(())
}

/// The mark a stage records on a node it found changed: the staged action `staged` with the
/// stage's own `node_flags`, and the dirty action `dirty` beside it, so that `flagDirty`
/// reflects the working-tree difference: any uncommitted change is dirty, whether it was
/// recorded by `dirty`, `status --scan` or `stage`/`stage --scan`.
///
/// No dirty action for merge staging: a merge resolution is not a fresh filesystem detection
/// and keeps its own staged and merge flags.
#[lore_macro::test_pub]
pub(crate) fn staged_change(staged: NodeFlags, dirty: NodeFlags, node_flags: NodeFlags) -> Mark {
    Mark {
        staged: Some((staged | node_flags, true)),
        dirty: (!node_flags.contains(NodeFlags::StagedMerge)).then_some((
            dirty,
            Ext::Set(NodeExtFlags::NoFlags),
            true,
        )),
    }
}

/// Mark `node_id` staged for deletion, reporting the node when this call is the one that
/// marked it so a caller walking the subtree knows whether it still has work below.
async fn stage_delete_node(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    relative_path: &RelativePath,
    node_id: NodeID,
    node_flags: NodeFlags,
    stats: &Arc<StageStats>,
    link_tracker: Option<&Arc<crate::link::LinkTracker>>,
) -> Result<Option<Node>, StageError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;

    let node = block.node(node_index);
    if node.is_staged_delete() {
        return Ok(None);
    }

    lore_debug!("Stage delete of node {}", node_id);
    if node.is_directory() {
        stats.directory_delete_count.fetch_add(1, Ordering::Relaxed);
        stats
            .directory_checked_count
            .fetch_add(1, Ordering::Relaxed);
    } else if node.is_link() {
        stats.link_remove_count.fetch_add(1, Ordering::Relaxed);
        stats
            .directory_checked_count
            .fetch_add(1, Ordering::Relaxed);
    } else {
        stats.file_delete_count.fetch_add(1, Ordering::Relaxed);
        event::LoreEvent::FileStageFile(LoreFileStageFileEventData {
            from_path: LoreString::default(),
            path: LoreString::from(relative_path),
            action: LoreFileAction::Delete,
        })
        .send();
        stats.file_checked_count.fetch_add(1, Ordering::Relaxed);
    }

    let mark = staged_change(NodeFlags::StagedDelete, NodeFlags::DirtyDelete, node_flags);
    state
        .node_mark(repository, node_id, mark)
        .await
        .forward::<StageError>("Failed to mark node as staged")?;

    if let Some(tracker) = link_tracker {
        tracker.on_node_changed(repository);
    }

    Ok(Some(node))
}

/// Stages the delete of the node at `node_id` that a node of another kind replaces, dropping its
/// registry entry where it links the repository `link`. Apart from the scan, so the scan's future
/// holds no room for the registry edit.
#[allow(clippy::too_many_arguments)]
async fn stage_delete_replaced(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    node_id: NodeID,
    link: Option<RepositoryId>,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
) -> Result<(), StageError> {
    if let Some(link) = link {
        state
            .link_remove_if_present(repository.clone(), link, node_id)
            .await
            .forward::<StageError>("Failed to remove link")?;
    }
    stage_delete(
        repository,
        state,
        relative_path,
        node_id,
        node_flags,
        stats,
        link_tracker,
    )
    .await
}

pub(crate) async fn stage_delete(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    node_id: NodeID,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
) -> Result<(), StageError> {
    let Some(node) = stage_delete_node(
        &repository,
        &state,
        &relative_path,
        node_id,
        node_flags,
        &stats,
        link_tracker.as_ref(),
    )
    .await?
    else {
        return Ok(());
    };

    // Note that links do not need to recurse into directory, as the subtree exist in
    // the link state tree and not this state tree
    if node.is_directory() {
        let mut children =
            StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
                .await
                .forward::<StageError>("Failed to list directory node children")?;

        while let Some((child_node_id, _child_node, child_name)) =
            children
                .next()
                .await
                .forward::<StageError>("Failed to list directory node children")?
        {
            // Takes the name by value so its block read lock ends here, rather than reaching the
            // delete of the child below (see NodeNameLock docs).
            let child_path = relative_path.join(child_name);
            stage_delete_recurse(
                repository.clone(),
                state.clone(),
                child_path,
                child_node_id,
                node_flags,
                stats.clone(),
                link_tracker.clone(),
            )
            .await?;
        }
    }

    Ok(())
}

fn stage_delete_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    node_id: NodeID,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
) -> Pin<Box<dyn Future<Output = Result<(), StageError>> + Send>> {
    Box::pin(stage_delete(
        repository,
        state,
        relative_path,
        node_id,
        node_flags,
        stats,
        link_tracker,
    ))
}

/// Stages the deletion of the node the tree holds at `path` where it is of the kind the working
/// tree does not hold there, `replaced_directory` saying whether that kind is a directory. The
/// filter leaves out what the working tree holds and keeps a node of the other kind, whose
/// verdict is `states`, so the node is asked about as what it is, as a node no listing entry
/// claims is. `known_child` is the node as the caller established it, and `base_node` the
/// directory a node it left unresolved is looked up in. Nothing is staged where the tree holds
/// no node there or one of the kind the working tree holds.
///
/// A cold path, which its caller boxes: a stage meets it only where the filter tells a file from a
/// directory at a path, as a directory-only rule does.
#[allow(clippy::too_many_arguments)]
async fn stage_replaced_node(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path: RelativePath,
    known_child: KnownChild,
    base_node: NodeID,
    replaced_directory: bool,
    states: FilterStates,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
) -> Result<(), StageError> {
    let node_id = match known_child {
        KnownChild::Resolved(Some(node_id)) => node_id,
        KnownChild::Resolved(None) => return Ok(()),
        KnownChild::Unresolved => match state
            .find_subnode(
                repository.clone(),
                base_node,
                hash::hash_string(path.name()),
            )
            .await
        {
            Ok(node_id) => node_id,
            Err(err) if err.is_node_not_found() => return Ok(()),
            Err(err) => return Err(err).forward::<StageError>("Failed to resolve the child node"),
        },
    };
    let node = state
        .node(repository.clone(), node_id)
        .await
        .forward::<StageError>("Failed to resolve the replaced node")?;
    if node.is_file() == replaced_directory {
        return Ok(());
    }
    stage_absent_delete(
        repository,
        state,
        path,
        node_id,
        NodeFlags::from_bits_retain(node.flags),
        node_flags,
        stats,
        link_tracker,
        states,
        false,
    )
    .await
    .map(|_| ())
}

/// Stage the deletion of `node_id`, which the file system does not hold, and report whether
/// part of it was kept.
///
/// A mount stays absent while the linked repository cannot be read, so the link and every
/// directory on its path stay. A node the view excludes stays too, with every directory on its
/// path, as a view leaves the files it excludes out of the working tree. So does a directory the
/// view excludes that holds no children, which nothing materializes. The ignore file has no say
/// over a node the tree tracks, so a deletion of one is staged as any other; it leaves out only an
/// add marked dirty, which was never staged. A node already staged for deletion keeps nothing.
///
/// `held` is what the tree holds `node_id` as: its type and what is staged on it. `states` is the
/// filter verdict at `relative_path`, which each child steps from, and `force` whether the stage
/// is forced past the ignore file. A child is asked about in the tracked mode.
#[allow(clippy::too_many_arguments)]
async fn stage_absent_delete(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    node_id: NodeID,
    held: NodeFlags,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    states: FilterStates,
    force: bool,
) -> Result<bool, StageError> {
    if held.contains(NodeFlags::StagedDelete) {
        return Ok(false);
    }

    if held.contains(NodeFlags::Link) {
        lore_debug!("Link mount {relative_path} is absent from the file system, keeping the link");
        return Ok(true);
    }

    if !held.is_directory() {
        stage_delete_node(
            &repository,
            &state,
            &relative_path,
            node_id,
            node_flags,
            &stats,
            link_tracker.as_ref(),
        )
        .await?;
        return Ok(false);
    }

    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
            .await
            .forward::<StageError>("Failed to list directory node children")?;

    let mut kept = false;
    let mut had_child = false;
    while let Some((child_node_id, child_held, child_name)) = children
        .next()
        .await
        .forward::<StageError>("Failed to list directory node children")?
        .map(|(id, node, name)| (id, NodeFlags::from_bits_retain(node.flags), name))
    {
        had_child = true;
        let child_path = relative_path.join(child_name);
        let is_directory = !child_held.contains(NodeFlags::File);
        let mode = repository
            .filter
            .tracked_mode(
                states,
                &child_path,
                is_directory,
                FilterMode::TRACKED.absence(force),
                async || state.tracks(&repository, child_node_id, child_held).await,
            )
            .await
            .forward::<StageError>("Failed to read the revision the staged tree was built on")?;
        let (child_states, excluded) =
            repository
                .filter
                .child_excludes_tree(states, &child_path, is_directory, mode);
        if excluded {
            kept = true;
            continue;
        }
        kept |= stage_absent_delete_recurse(
            repository.clone(),
            state.clone(),
            child_path,
            child_node_id,
            child_held,
            node_flags,
            stats.clone(),
            link_tracker.clone(),
            child_states,
            force,
        )
        .await?;
    }

    if kept {
        return Ok(true);
    }
    // An empty directory the view excludes stays, as nothing materializes it, and so does one
    // the ignore rules exclude that the tree only marks. One the tree tracks is deleted past them.
    if !had_child
        && states.excluded(FilterMode::Full)
        && (states.excluded(FilterMode::View)
            || !state
                .tracks(&repository, node_id, held)
                .await
                .forward::<StageError>(
                    "Failed to read the revision the staged tree was built on",
                )?)
    {
        return Ok(true);
    }

    stage_delete_node(
        &repository,
        &state,
        &relative_path,
        node_id,
        node_flags,
        &stats,
        link_tracker.as_ref(),
    )
    .await?;
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn stage_absent_delete_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: RelativePath,
    node_id: NodeID,
    held: NodeFlags,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    states: FilterStates,
    force: bool,
) -> Pin<Box<dyn Future<Output = Result<bool, StageError>> + Send>> {
    Box::pin(stage_absent_delete(
        repository,
        state,
        relative_path,
        node_id,
        held,
        node_flags,
        stats,
        link_tracker,
        states,
        force,
    ))
}

/// Resolve case-variant collisions in a list of filesystem entries.
///
/// On a case-sensitive filesystem, multiple entries differing only in case can coexist
/// (e.g., directories `Assets/` and `assets/`, or files `Readme.txt` and `README.txt`).
///
/// For directories in `Rename` or `Keep` mode, this picks a single winner per
/// case-insensitive group and merges the losers' contents into the winner on disk.
/// For `Rename`, the winner is the variant that differs from the current state name
/// (last alphabetically if multiple). For `Keep`, the winner matches the state name.
///
/// For files (or mixed file/directory groups), case-variant collisions always produce an
/// error because silently discarding content is not safe.
///
/// In `Error` mode (default `stage` without `--case`), no resolution is attempted so the
/// downstream code can report the mismatch to the user.
#[allow(clippy::too_many_arguments)]
async fn resolve_case_variant_collisions(
    operation: &Arc<InstanceOperationImpl>,
    items: &mut Vec<DirectoryEntry>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    directory_node: NodeID,
    relative_path: &RelativePathBuf,
    options: StageOptions,
) -> Result<(), StageError> {
    if matches!(options.case_change, StageCaseChange::Error) {
        return Ok(());
    }

    items.sort_by(|a, b| a.name_hash.cmp(&b.name_hash).then(a.name.cmp(&b.name)));

    // Scan for groups sharing a name_hash, collect resolution info
    let mut removals: Vec<(usize, usize, Option<String>)> = Vec::new();
    let mut group_end = 0;
    while group_end < items.len() {
        let hash = items[group_end].name_hash;
        let group_start = group_end;
        while group_end < items.len() && items[group_end].name_hash == hash {
            group_end += 1;
        }
        if group_end - group_start <= 1 {
            continue;
        }

        let group = &items[group_start..group_end];
        let all_dirs = group.iter().all(|e| e.info.is_dir());

        if !all_dirs {
            let names: Vec<&str> = group.iter().map(|e| e.name.as_str()).collect();
            lore_error!(
                "Multiple files with case-only differences found: {} - remove duplicates before staging",
                names.join(", ")
            );
            return Err(StageError::internal("A name case mismatch was detected"));
        }

        // All directories — pick a winner and unify the filesystem
        let state_name = if let Ok(node_id) = state
            .find_subnode(repository.clone(), directory_node, hash)
            .await
        {
            state
                .node_name_ref(repository.clone(), node_id)
                .await
                .ok()
                .map(|n| n.to_string())
        } else {
            None
        };

        let winner = match (options.case_change, &state_name) {
            (StageCaseChange::Rename, _) => group.last().map(|d| d.name.clone()),
            (_, Some(sn)) => {
                // Keep mode: prefer the variant matching the state name. If none matches,
                // use the state name as the unification target and rename the surviving
                // entry to match so the downstream Keep handler has nothing to do.
                Some(sn.clone())
            }
            (_, None) => group.last().map(|d| d.name.clone()),
        };

        if let Some(ref winner_name) = winner {
            for entry in group {
                if entry.name != *winner_name {
                    let from_path = relative_path.clone().push_and_freeze(&entry.name);
                    let to_path = relative_path.clone().push_and_freeze(winner_name);
                    lore_debug!("Case variant collision: unifying {from_path} into {to_path}");
                    let _ = operation.rename(&from_path, &to_path).await;
                }
            }
        }

        removals.push((group_start, group_end, winner));
    }

    // Remove duplicate entries, keeping only winners (iterate in reverse to preserve indices)
    for (group_start, group_end, winner) in removals.into_iter().rev() {
        if let Some(ref winner_name) = winner {
            // Find an entry matching the winner name, or fall back to the first in the group
            let winner_idx = (group_start..group_end)
                .find(|&idx| items[idx].name == *winner_name)
                .unwrap_or(group_start);
            // If the surviving entry has a different name (e.g., Keep mode where the winner
            // is the state name but no filesystem entry matched), update it so the downstream
            // code sees the correct name and doesn't attempt a redundant rename.
            if items[winner_idx].name != *winner_name {
                items[winner_idx].name.clone_from(winner_name);
            }
            for idx in (group_start..group_end).rev() {
                if idx != winner_idx {
                    items.remove(idx);
                }
            }
        } else {
            for idx in (group_start..group_end - 1).rev() {
                items.remove(idx);
            }
        }
    }

    Ok(())
}

/// The children of a directory node, indexed by name hash, and which of them a
/// file system entry has claimed.
///
/// Ties are held in sibling order: two names differing only in case hash the
/// same, and a claim takes the first child not already claimed.
#[lore_macro::test_pub]
struct DirectoryChildren {
    /// Child node ids in sibling order, [`INVALID_NODE`] where a file system
    /// entry has claimed the child.
    node: Vec<NodeID>,
    /// `(name hash, index into `node`, child node id)` for every child, ordered
    /// by the first two, so a tie on the hash is claimed in sibling order. The
    /// id is held here as well, where a claim has taken it out of `node`.
    by_name_hash: Vec<(u64, u32, NodeID)>,
    /// The chain as the listing began it, which everything linked into it since
    /// sits ahead of while no reattach moves it; `None` for a link's or a file's.
    since: Option<ChainMark>,
}

impl DirectoryChildren {
    /// The children of a directory node in sibling order, with the name hash
    /// each of them carries, and the mark of the chain the listing began from.
    #[lore_macro::test_pub]
    fn new(child: Vec<(NodeID, u64)>, since: Option<ChainMark>) -> Self {
        let mut by_name_hash: Vec<(u64, u32, NodeID)> = child
            .iter()
            .enumerate()
            .map(|(index, &(node, name_hash))| (name_hash, index as u32, node))
            .collect();
        by_name_hash.sort_unstable();
        let node = child.into_iter().map(|(node, _)| node).collect();
        Self {
            node,
            by_name_hash,
            since,
        }
    }

    /// The entries of the index carrying `name_hash`, in sibling order.
    fn matching(&self, name_hash: u64) -> impl Iterator<Item = (u32, NodeID)> + '_ {
        let start = self
            .by_name_hash
            .partition_point(|&(hash, _, _)| hash < name_hash);
        self.by_name_hash[start..]
            .iter()
            .take_while(move |&&(hash, _, _)| hash == name_hash)
            .map(|&(_, index, node)| (index, node))
    }

    /// The first child carrying `name_hash` that nothing has claimed yet, marked
    /// as claimed. `None` where the directory holds no such child, or holds only
    /// ones already claimed.
    #[lore_macro::test_pub]
    fn claim(&mut self, name_hash: u64) -> Option<NodeID> {
        let (index, node) = self
            .matching(name_hash)
            .find(|&(index, _)| self.node[index as usize] != INVALID_NODE)?;
        self.node[index as usize] = INVALID_NODE;
        Some(node)
    }

    /// Marks `node` claimed where the listing holds it: a child a search of the chain
    /// answered, which the listing may hold under the name it had before a rename,
    /// so the pass over what nothing claimed leaves it.
    #[lore_macro::test_pub]
    fn claim_node(&mut self, node: NodeID) {
        if let Some(slot) = self.node.iter_mut().find(|slot| **slot == node) {
            *slot = INVALID_NODE;
        }
    }

    /// Counts a node the stage itself added to the directory, prepending it, in the mark, so
    /// the stage's own adds never make its searches stale nor show as a reattach. Only an add
    /// that landed counts: counting one that did not could hide a reattach.
    fn count_own_add(&mut self) {
        if let Some(since) = self.since.as_mut() {
            since.count_own_prepend();
        }
    }

    /// The first child the listing holds carrying `name_hash`, claimed or not,
    /// which is the one a search of the chain reaches once past what was linked
    /// into it since.
    #[lore_macro::test_pub]
    fn holds(&self, name_hash: u64) -> Option<NodeID> {
        self.matching(name_hash).next().map(|(_, node)| node)
    }

    /// The children no file system entry claimed, in sibling order. Each one is a
    /// path the tree holds and the file system does not.
    #[lore_macro::test_pub]
    fn unclaimed(&self) -> impl Iterator<Item = NodeID> + '_ {
        self.node
            .iter()
            .copied()
            .filter(|&node| node != INVALID_NODE)
    }
}

/// Whether the caller has established which child of a directory a name belongs
/// to.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
pub(crate) enum KnownChild {
    /// The child, or that the directory holds none. Taken as given.
    Resolved(Option<NodeID>),
    /// Nothing established; search the directory's chain.
    Unresolved,
}

/// The child of `directory_node` that `name_hash` belongs to, claimed where the
/// listing still holds it unclaimed.
///
/// A child is prepended, so whatever was linked into the chain since the listing
/// was taken lies ahead of it. Only that much is walked; the listing answers from
/// the index behind it, claimed children included. Where a reattach may have put a
/// node in front of what was linked in since, the whole chain is searched. The
/// answer is the one a search of the whole chain reaches; a child a search answered
/// after a reattach is claimed in the listing too, which may hold it under the name
/// it had. A link's or a file's listing, which carries no mark, answers from the
/// listing alone.
///
/// The listing's own answers are taken as given: no writer but the stage's own tasks, each in its
/// own directory, runs beside a stage (the node module's Limitations, "The stage's listing"), so no
/// child the listing holds is renamed, moved or removed meanwhile.
#[lore_macro::test_pub]
async fn claim_child(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    directory_node: NodeID,
    children: &mut DirectoryChildren,
    name_hash: u64,
) -> Result<Option<NodeID>, StageError> {
    if let Some(node) = children.claim(name_hash) {
        return Ok(Some(node));
    }
    let Some(since) = children.since.as_ref() else {
        return Ok(children.holds(name_hash));
    };
    let searched = match state
        .find_subnode_added_since(repository.clone(), since, name_hash)
        .await
        .forward::<StageError>("Failed to search the directory node children")?
    {
        AddedSince::Found(node) => node,
        AddedSince::Absent => return Ok(children.holds(name_hash)),
        AddedSince::Stale => match state
            .find_subnode(repository.clone(), directory_node, name_hash)
            .await
        {
            Ok(node) => node,
            Err(error) if error.is_node_not_found() => return Ok(children.holds(name_hash)),
            Err(error) => {
                return Err(error)
                    .forward::<StageError>("Failed to search the directory node children");
            }
        },
    };
    // A listed child stands ahead of the mark's head only where a reattach moved it.
    if since.reattached() {
        children.claim_node(searched);
    }
    Ok(Some(searched))
}

static DIRECTORY_TASK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Process-wide rather than per-operation: the cap has to hold across all the
/// targets `file::stage` walks concurrently, so a semaphore owned by one walk
/// would let N concurrent targets run N times the intended fan-out.
fn directory_task_semaphore() -> &'static Arc<Semaphore> {
    DIRECTORY_TASK_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(max_concurrent_stage_directory_tasks())))
}

/// What [`stage_directory`]'s pass over the children no listing entry claims does with `child`,
/// which the working tree lacks below `relative_path`, whose verdict is `states`: `None` where the
/// filter leaves it out, or where it was never committed and is queued to leave the staged tree,
/// and otherwise its path, the verdict its children step from and what the tree holds it as, for
/// the delete to stage. The ignore rules leave out a child the tree only marks.
///
/// A function of its own, so the node and its path sit in the directory's future only while this
/// runs, rather than in every state that awaits beside it.
async fn unclaimed_child(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    relative_path: &RelativePathBuf,
    states: FilterStates,
    child: NodeID,
    force: bool,
    discards: Option<&DiscardQueue>,
) -> Result<Option<(RelativePath, FilterStates, NodeFlags)>, StageError> {
    let Some((node, node_name)) = state
        .node_and_name_or_skip(repository.clone(), child)
        .await
        .forward::<StageError>("Failed to resolve the child node")?
    else {
        return Ok(None);
    };
    let mut filter_path = relative_path.clone();
    filter_path.push(node_name);
    let filter_path = filter_path.freeze();
    let held = NodeFlags::from_bits_retain(node.flags);
    let mode = repository
        .filter
        .tracked_mode(
            states,
            &filter_path,
            !node.is_file(),
            FilterMode::TRACKED.absence(force),
            async || state.tracks(repository, child, held).await,
        )
        .await
        .forward::<StageError>("Failed to read the revision the staged tree was built on")?;
    let (child_states, excluded) =
        repository
            .filter
            .child_emit_excludes(states, &filter_path, !node.is_file(), mode);
    // A staged add the filter excludes was forced, so it stays
    if excluded && node.is_staged() {
        lore_trace!("Node excluded by filter: {}", filter_path.as_str());
        return Ok(None);
    }

    if let Some(queue) = discards
        && drops_cleanly(&queue.committed, repository, state, child, &node).await?
    {
        lore_trace!("Node was never committed, dropping it: {filter_path}");
        queue.push(child);
        return Ok(None);
    }

    if excluded {
        lore_trace!("Node excluded by filter: {}", filter_path.as_str());
        return Ok(None);
    }
    Ok(Some((filter_path, child_states, held)))
}

/// `states` is the filter verdict for `relative_path`, which each child steps
/// from rather than folding its whole path.
///
/// Answers whether the tree keeps anything below the directory.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_directory(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    absolute_path: &Path,
    relative_path: RelativePathBuf,
    directory_node: NodeID,
    depth: usize,
    options: StageOptions,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    layer_mask: Option<Arc<Vec<String>>>,
    discards: PendingDiscards,
    states: FilterStates,
) -> Result<bool, StageError> {
    let (listing, since) = state
        .node_children_with_name_hash_marked(repository.clone(), directory_node)
        .await
        .forward::<StageError>("Failed to list directory node children")?;
    let mut children = DirectoryChildren::new(listing, since);

    let directory_path = relative_path.clone().freeze();
    let mut file_list = operation
        .read_directory(&directory_path)
        .await
        .forward_any_with::<StageError, _>(|| {
            format!("Failed to list directory files in {directory_path}")
        })?;

    // Collect all filesystem entries, then resolve case variant collisions before staging.
    // On a case-sensitive filesystem, multiple entries differing only in case can coexist
    // (e.g., "Assets" and "assets"). Without resolution, processing both independently causes
    // the second to undo the case rename performed by the first, producing a nondeterministic
    // result depending on iteration order.
    let mut items: Vec<DirectoryEntry> = Vec::new();
    while let Some(item) = file_list.next().await {
        let item = item.forward_any::<StageError>("Unusable directory entry")?;
        // The repository's own directory is not content, so it takes no part in the case
        // resolution below, which moves what the losers of a collision hold on disk.
        if is_reserved_node_name(&item.name) {
            continue;
        }
        items.push(item);
    }

    resolve_case_variant_collisions(
        &operation,
        &mut items,
        repository.clone(),
        state.clone(),
        directory_node,
        &relative_path,
        options,
    )
    .await?;

    let mut directory_tasks = JoinSet::new();
    let mut outcome = Ok(false);
    // Reusable buffer for layer mount path checks: one allocation per directory
    // instead of per child. Sized for the parent path plus a typical name; grows
    // if a child name is longer.
    let parent_path_str = relative_path.as_str();
    let mut child_path_buf = if layer_mask.is_some() {
        String::with_capacity(parent_path_str.len() + 64)
    } else {
        String::new()
    };
    let mut nested_probe: Option<PathBuf> = None;
    for item in items {
        if item.info.is_dir() {
            let directory = item;

            // If this child directory is a configured layer mount, skip it
            // entirely — neither add it as a parent-tree node nor descend.
            // The layer's own staging is dispatched separately by file::stage.
            // Predicate accepts `&[String]` directly to avoid rebuilding a
            // Vec<&str> per child.
            if let Some(ref mask) = layer_mask {
                child_path_buf.clear();
                if !parent_path_str.is_empty() {
                    child_path_buf.push_str(parent_path_str);
                    child_path_buf.push('/');
                }
                child_path_buf.push_str(&directory.name);
                if crate::file::stage::is_path_under_layer_mask(&child_path_buf, mask.as_slice()) {
                    lore_trace!(
                        "Skipping layer mount {} from parent stage walk",
                        child_path_buf
                    );
                    continue;
                }
            }

            lore_trace!(
                "Staging subnode directory: {} ({} {}/)",
                directory.name,
                directory_node,
                relative_path.as_str()
            );

            let claimed = match claim_child(
                &repository,
                &state,
                directory_node,
                &mut children,
                directory.name_hash,
            )
            .await
            {
                Ok(claimed) => claimed,
                Err(err) => {
                    fold_child(Err(err), &mut outcome);
                    break;
                }
            };

            let uncommitted = match is_uncommitted_child(&repository, &state, claimed).await {
                Ok(uncommitted) => uncommitted,
                Err(err) => {
                    fold_child(Err(err), &mut outcome);
                    break;
                }
            };

            if uncommitted {
                let probe = nested_probe.get_or_insert_with(|| absolute_path.to_path_buf());
                if state::is_nested_repository_root(probe, directory.name.as_str()).await {
                    lore_trace!(
                        "Skipping nested repository root {} in {}/",
                        directory.name,
                        relative_path.as_str()
                    );

                    // An entry an earlier walk indexed is left claimed, so the
                    // unclaimed pass below does not stage a delete against a base
                    // the parent never committed, and is queued for discard so
                    // the tree stops holding what the boundary excludes.
                    if let (Some(node), Some(discards)) = (claimed, discards.as_deref()) {
                        discards.push(node);
                    }

                    continue;
                }
            }

            let staged = match stage_node_from_metadata(
                &operation,
                NodeMapping {
                    repository: repository.clone(),
                    state: state.clone(),
                    path: directory_path.clone(),
                    node: directory_node,
                },
                directory.name.clone(),
                directory.info,
                options,
                stats.clone(),
                link_tracker.clone(),
                KnownChild::Resolved(claimed),
                states,
            )
            .await
            {
                Ok(staged) => staged,
                Err(err) => {
                    fold_child(Err(err), &mut outcome);
                    break;
                }
            };
            if staged.prepended {
                children.count_own_add();
            }
            let node_link = staged.link;
            let child_states = staged.states;
            let added = staged.added;

            // Inline fallback rather than a blocking acquire: a parent awaiting
            // its children must never block on a permit a descendant needs, or
            // the bounded fan-out would deadlock.
            if let Ok(permit) = directory_task_semaphore().clone().try_acquire_owned() {
                lore_spawn!(directory_tasks, {
                    let operation = operation.clone();
                    let repository = repository.clone();
                    let state = state.clone();
                    let stats = stats.clone();
                    let relative_path = relative_path.clone();
                    let absolute_path = absolute_path.to_path_buf();
                    let link_tracker = link_tracker.clone();
                    let layer_mask = layer_mask.clone();
                    let discards = discards.clone();
                    async move {
                        let _permit = permit;
                        stage_child_directory(
                            operation,
                            repository,
                            state,
                            absolute_path,
                            relative_path,
                            node_link,
                            added,
                            depth,
                            options,
                            stats,
                            link_tracker,
                            layer_mask,
                            discards,
                            child_states,
                        )
                        .await
                    }
                });
            } else {
                let result = stage_child_directory(
                    operation.clone(),
                    repository.clone(),
                    state.clone(),
                    absolute_path.to_path_buf(),
                    relative_path.clone(),
                    node_link,
                    added,
                    depth,
                    options,
                    stats.clone(),
                    link_tracker.clone(),
                    layer_mask.clone(),
                    discards.clone(),
                    child_states,
                )
                .await;
                fold_child(result, &mut outcome);
            }

            while let Some(result) = directory_tasks.try_join_next() {
                fold_child(
                    result
                        .map_err(|e| StageError::internal_with_context(e, "Failed to join task"))
                        .flatten(),
                    &mut outcome,
                );
            }
        } else if item.info.is_file() {
            let file = item;

            lore_trace!(
                "Staging subnode file: {} ({} {}/)",
                file.name,
                directory_node,
                relative_path.as_str()
            );

            let claimed = match claim_child(
                &repository,
                &state,
                directory_node,
                &mut children,
                file.name_hash,
            )
            .await
            {
                Ok(claimed) => claimed,
                Err(err) => {
                    fold_child(Err(err), &mut outcome);
                    break;
                }
            };

            let result = stage_node_from_metadata(
                &operation,
                NodeMapping {
                    repository: repository.clone(),
                    state: state.clone(),
                    path: directory_path.clone(),
                    node: directory_node,
                },
                file.name.clone(),
                file.info,
                options,
                stats.clone(),
                link_tracker.clone(),
                KnownChild::Resolved(claimed),
                states,
            )
            .await;
            match result {
                Ok(staged) => {
                    if staged.prepended {
                        children.count_own_add();
                    }
                    fold_child(Ok(staged.link.is_valid()), &mut outcome);
                }
                Err(error) => fold_child(Err(error), &mut outcome),
            }
        }

        while let Some(result) = directory_tasks.try_join_next() {
            fold_child(
                result
                    .internal("Recursion task failed")
                    .map_err(StageError::from)
                    .flatten(),
                &mut outcome,
            );
        }

        if outcome.is_err() {
            break;
        }
    }

    let force = execution_context().globals().force();
    for child in children.unclaimed() {
        if outcome.is_err() {
            break;
        }
        let (filter_path, child_states, held) = match unclaimed_child(
            &repository,
            &state,
            &relative_path,
            states,
            child,
            force,
            discards.as_deref(),
        )
        .await
        {
            Ok(Some(unclaimed)) => unclaimed,
            Ok(None) => continue,
            Err(err) => {
                fold_child(Err(err), &mut outcome);
                break;
            }
        };

        let result = stage_absent_delete(
            repository.clone(),
            state.clone(),
            filter_path,
            child,
            held,
            options.node_flags,
            stats.clone(),
            link_tracker.clone(),
            child_states,
            force,
        )
        .await;
        fold_child(result.map(|_| true), &mut outcome);
    }
    // Nothing reads the listing past here: its vectors and the mark's block go before the
    // subtree's tasks are awaited.
    drop(children);

    while let Some(result) = directory_tasks.join_next().await {
        fold_child(
            result
                .internal("Recursion task failed")
                .map_err(StageError::from)
                .flatten(),
            &mut outcome,
        );
    }
    outcome
}

/// Folds what the tree keeps of one child into `outcome`, which holds on to the first failure.
fn fold_child(result: Result<bool, StageError>, outcome: &mut Result<bool, StageError>) {
    match result {
        Ok(kept) => {
            if let Ok(holds) = outcome {
                *holds |= kept;
            }
        }
        Err(err) => {
            if outcome.is_ok() {
                *outcome = Err(err);
            }
        }
    }
}

/// Descend into one child directory of `stage_directory`, and answer whether the tree keeps it.
/// Split out from the child loop so the bounded fan-out there can either spawn this or await it
/// inline without duplicating the body.
///
/// A directory the filter excludes stands on disk only for content below it. Where this stage
/// adds such a directory, one no commit holds, and the descent keeps nothing below it, the
/// directory is discarded rather than staged, and its add is recorded only where it is kept. An
/// add staged before stays, as what it holds can be staged past the filter, and a forced stage
/// keeps every directory, as it leaves nothing out. `added` is whether staging the directory added
/// it.
///
/// `states` is the child's own filter verdict, which staging it already reached.
#[allow(clippy::too_many_arguments)]
async fn stage_child_directory(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    mut absolute_path: PathBuf,
    mut relative_path: RelativePathBuf,
    node_link: NodeLink,
    added: bool,
    depth: usize,
    options: StageOptions,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    layer_mask: Option<Arc<Vec<String>>>,
    discards: PendingDiscards,
    states: FilterStates,
) -> Result<bool, StageError> {
    if !node_link.is_valid() {
        return Ok(false);
    }

    let from_node = state
        .node(repository.clone(), node_link.node)
        .await
        .forward::<StageError>("Failed to resolve node path in state")?;

    if from_node.is_link() {
        let link = from_node.linked_node();

        let linked_repository = repository.to_link_context(link.repository).await;
        let mut linked_state = State::deserialize(linked_repository.clone(), link.revision)
            .await
            .forward::<StageError>("Failed to deserialize linked state")?;

        // Register link with tracker for deferred processing
        if let Some(ref tracker) = link_tracker {
            // Check for existing context and reuse state if available
            linked_state =
                if let Some(existing_context) = tracker.find_link_context(link.repository) {
                    existing_context.link_state.clone()
                } else {
                    linked_state.clone()
                };

            let link_context = link::LinkContext {
                link_repository_id: link.repository,
                link_node_id: node_link.node,
                parent_repository_id: repository.id,
                link_state: linked_state.clone(),
            };

            tracker.add_link(link_context);
        }

        let mut link_relative_path = relative_path.clone();
        // Scoped so the read lock drops before the recurse below.
        {
            let node_name = state
                .node_name_ref(repository.clone(), node_link.node)
                .await
                .forward::<StageError>("Failed to resolve node name")?;
            absolute_path.push(&node_name);
            link_relative_path.push(&node_name);
        }

        let link_states = linked_repository.filter.mount_states(&link_relative_path);

        let result = stage_directory_recurse(
            operation.clone(),
            linked_repository.clone(),
            linked_state.clone(),
            absolute_path.as_path(),
            link_relative_path.clone(),
            link.node,
            depth,
            options,
            stats.clone(),
            link_tracker.clone(),
            layer_mask.clone(),
            None,
            link_states,
        )
        .await;

        stats.task_count.fetch_sub(1, Ordering::Release);

        result.map(|_| true)
    } else {
        // If the directory node was renamed as part of the stage case variation unification,
        // use the updated unified name to recurse into the correct subdirectory on disk
        let node_name = state
            .node_name_ref(repository.clone(), node_link.node)
            .await
            .forward::<StageError>("Failed to resolve node name")?;
        absolute_path.push(&*node_name);
        relative_path.push(node_name);
        // Layer mount directories are filtered out by the mask check at the top
        // of `stage_directory`'s child-iteration loop, so no mask check is
        // needed here.
        stats.task_count.fetch_add(1, Ordering::Release);
        let result = stage_directory_recurse(
            operation,
            repository,
            state,
            absolute_path.as_path(),
            relative_path,
            node_link.node,
            depth + 1,
            options,
            stats.clone(),
            link_tracker.clone(),
            layer_mask.clone(),
            discards.clone(),
            states,
        )
        .await;
        stats.task_count.fetch_sub(1, Ordering::Release);
        if !result?
            && added
            && states.excluded(FilterMode::Full)
            && !execution_context().globals().force()
            && let Some(queue) = discards.as_deref()
        {
            queue.push(node_link.node);
            return Ok(false);
        }
        if added {
            stats.record_action(LoreFileAction::Add, true);
        }
        Ok(true)
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_directory_recurse(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    absolute_path: &Path,
    relative_path: RelativePathBuf,
    directory_node: NodeID,
    depth: usize,
    options: StageOptions,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    layer_mask: Option<Arc<Vec<String>>>,
    discards: PendingDiscards,
    states: FilterStates,
) -> Pin<Box<dyn Future<Output = Result<bool, StageError>> + Send + '_>> {
    Box::pin(stage_directory(
        operation,
        repository,
        state,
        absolute_path,
        relative_path,
        directory_node,
        depth,
        options,
        stats,
        link_tracker,
        layer_mask,
        discards,
        states,
    ))
}

/// Stage one child of `base_node` from the file information the caller already holds.
///
/// `info` describes a path the filesystem holds. Only `is_dir` tells a directory from
/// a file here, so an absent path would be staged as an empty file: a caller holding
/// one stages the deletion instead.
///
/// `known_child` answers which child the name belongs to where the caller has
/// established it, a walk holding the directory's listing among them, and is
/// [`KnownChild::Unresolved`] where nothing has. A resolved answer is taken as
/// given, one holding no child included, so a caller giving one has to account
/// for the children linked into the chain since it looked.
///
/// What staging a child established: the node it linked, and the filter verdict
/// for the child itself, which a walk descending into it steps from.
///
/// The verdict is handed back rather than recomputed below, because staging the
/// child had to reach it to decide whether the child was excluded at all.
#[lore_macro::test_pub]
pub(crate) struct StagedChild {
    pub link: NodeLink,
    pub states: FilterStates,
    /// Whether the stage added the child's node to the directory, prepending it there.
    pub prepended: bool,
    /// A directory no commit holds whose add this call staged, which the caller records once it
    /// keeps it.
    pub added: bool,
}

impl StagedChild {
    /// A child that was not staged, which nothing descends into.
    fn invalid() -> Self {
        Self {
            link: NodeLink::invalid(),
            states: FilterStates::ROOT,
            prepended: false,
            added: false,
        }
    }
}

/// Renames `from` to `to` on the filesystem, which differ only in case. A refusal the stage error
/// declares, such as a read-only instance, stays that error.
async fn unify_case(
    operation: &InstanceOperationImpl,
    from: &RelativePath,
    to: &RelativePath,
) -> Result<(), StageError> {
    operation
        .rename(from, to)
        .await
        .forward_any_with::<StageError, _>(|| {
            format!("Unable to rename file system path {from} to {to}")
        })
}

/// Whether `directory` holds both the spelling the tree carries and the one the file system
/// showed, which a case-sensitive file system can hold side by side as separate entries.
///
/// The two fold together, the node having been claimed by that fold, so one listing answers for
/// both. Read rather than taken from the parent's listing: staging a sibling renames entries in
/// this directory, so an earlier snapshot answers a stale question. A directory that cannot be
/// read is one that cannot be unified either, and reads as holding neither.
async fn holds_both_spellings(
    operation: &InstanceOperationImpl,
    directory: &RelativePath,
    tracked: &str,
    observed: &str,
) -> bool {
    let held = operation
        .names_folding_to(directory, observed)
        .await
        .unwrap_or_default();
    held.iter().any(|spelling| spelling == tracked)
        && held.iter().any(|spelling| spelling == observed)
}

/// Stage the child `name` of `base` from the file information the caller already holds.
///
/// `parent_states` is the filter verdict for `base`'s path, which the child named here steps from
/// rather than folding its whole path.
///
/// A changed file a merge in progress staged past the view ([`crate::commit::merged_past_the_view`]),
/// which only a forced stage reaches, is taken from the working tree as the merge's resolution,
/// again where a forced stage resolved it before: its content hash is dropped, and its size is the
/// working tree's. A zero hash with a nonzero size is content not yet stored, the state
/// a newly staged add has, so the commit reads the file, as it reads a merged file in view. An
/// empty file leaves a zero hash with size 0, which the commit takes from the node as it stands,
/// and the file's mode with it. The file ID and the merge bits stay, and a staged add or move, a
/// merge's included, keeps its action.
#[allow(clippy::too_many_arguments, unused_assignments)]
#[lore_macro::test_pub]
pub(crate) async fn stage_node_from_metadata(
    operation: &Arc<InstanceOperationImpl>,
    base: NodeMapping,
    name: String,
    info: FileInfo,
    options: StageOptions,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    known_child: KnownChild,
    parent_states: FilterStates,
) -> Result<StagedChild, StageError> {
    let NodeMapping {
        repository,
        state,
        path: base_relative_path,
        node: base_node,
    } = base;
    if base_relative_path.is_empty() && (name.is_empty() || name.as_str() == ".") {
        return Ok(StagedChild {
            link: NodeLink {
                node: base_node,
                repository: repository.id,
                revision: state.revision(),
            },
            states: parent_states,
            prepended: false,
            added: false,
        });
    }

    if is_reserved_node_name(&name) {
        lore_trace!("Ignore dot directory {name}");
        return Ok(StagedChild::invalid());
    }
    if name.ends_with(TEMP_FILE_EXTENSION) {
        lore_trace!("Ignore {TEMP_FILE_EXTENSION} file");
        return Ok(StagedChild::invalid());
    }
    if name.ends_with(BASE_SUFFIX) {
        lore_trace!("Ignore {BASE_SUFFIX} file");
        return Ok(StagedChild::invalid());
    }
    if name.ends_with(THEIRS_SUFFIX) {
        lore_trace!("Ignore {THEIRS_SUFFIX} file");
        return Ok(StagedChild::invalid());
    }

    let name_hash = hash::hash_string(name.as_str());

    let found_child = match known_child {
        KnownChild::Resolved(node) => Ok(node),
        KnownChild::Unresolved => match state
            .find_subnode(repository.clone(), base_node, name_hash)
            .await
        {
            Ok(node) => Ok(Some(node)),
            Err(err) if err.is_node_not_found() => Ok(None),
            Err(err) => Err(err),
        },
    };

    // Asked once the tree has been searched: a node it tracks as the kind the working tree holds is
    // past the ignore slot. The node is read only where the ignore rules would drop the path.
    let force = execution_context().globals().force();
    let filter_path = base_relative_path.join(name.as_str());
    let tracked = match found_child {
        Ok(Some(node_id))
            if !force
                && repository
                    .filter
                    .child_excludes_tree(
                        parent_states,
                        &filter_path,
                        info.is_dir(),
                        FilterMode::Full,
                    )
                    .1 =>
        {
            // The flags alone, so the node is not held across the tree's answer.
            let flags = state
                .node(repository.clone(), node_id)
                .await
                .map(|node| NodeFlags::from_bits_retain(node.flags));
            match flags {
                Ok(flags) => state
                    .tracks_as(&repository, node_id, flags, info.is_dir())
                    .await
                    .forward::<StageError>(
                        "Failed to read the revision the staged tree was built on",
                    )?,
                Err(_) => false,
            }
        }
        Ok(Some(_)) => true,
        _ => false,
    };
    let (states, excluded) = repository.filter.child_emit_unless_forced(
        force,
        FilterMode::View,
        parent_states,
        &filter_path,
        info.is_dir(),
        FilterMode::TRACKED.at(tracked),
    );
    if excluded {
        lore_trace!("Node excluded by filter: {}", filter_path.as_str());
        if !matches!(known_child, KnownChild::Resolved(None)) {
            let (replaced_states, replaced_excluded) = repository.filter.child_excludes_tree(
                parent_states,
                &filter_path,
                !info.is_dir(),
                FilterMode::Full,
            );
            if !replaced_excluded {
                Box::pin(stage_replaced_node(
                    repository,
                    state,
                    filter_path,
                    known_child,
                    base_node,
                    !info.is_dir(),
                    replaced_states,
                    options.node_flags,
                    stats,
                    link_tracker,
                ))
                .await?;
            }
        }
        return Ok(StagedChild::invalid());
    }

    lore_trace!("Stage node {}", base_relative_path.join(name.as_str()));

    let relative_path = base_relative_path;

    let node_link = match found_child {
        Ok(Some(found_node_id)) => {
            // Verify that the found node matches the type in the filesystem
            let block_index = NodeBlock::index(found_node_id);
            let node_index = Node::index(found_node_id);
            let block = state
                .block_with_nametable(repository.clone(), block_index)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;

            let node = block.node(node_index);

            lore_trace!("Found node {} with flags 0x{:x}", found_node_id, node.flags);

            // A link mounts as a directory: a file replaces it, and so does a directory once the
            // link is staged for delete.
            let retype = if node.is_link() {
                node.is_staged_delete() || !info.is_dir()
            } else {
                node.is_file() == info.is_dir()
            };
            if retype {
                lore_debug!(
                    "Node kind differs, stage delete of node {} and recreate new node",
                    found_node_id
                );
                stage_delete_replaced(
                    repository.clone(),
                    state.clone(),
                    relative_path.join(name.as_str()),
                    found_node_id,
                    node.is_link().then(|| node.address.context.into()),
                    options.node_flags,
                    stats.clone(),
                    link_tracker.clone(),
                )
                .await?;

                NodeLink::invalid()
            } else {
                NodeLink {
                    node: found_node_id,
                    repository: repository.id,
                    revision: state.revision(),
                }
            }
        }
        Ok(None) => NodeLink::invalid(),
        Err(err) if is_refusal(&err) => {
            return Err(err).forward::<StageError>("finding the subnode");
        }
        Err(err) => {
            return Err(StageError::internal_with_context(
                err,
                "Failed to find subnode",
            ));
        }
    };

    if !node_link.is_valid() {
        // Node did not exist, add new node to state
        lore_trace!("Found no existing node for {name}, creating new node");

        let mut node = if info.is_dir() {
            Node {
                name_hash,
                ..Default::default()
            }
        } else {
            let size = info.size();
            Node {
                flags: NodeFlags::File.bits(),
                mode: info.mode(0),
                name_hash,
                size,
                ..Default::default()
            }
        };

        if node.is_file() && node.address.context.is_zero() {
            if let Some(file_id) = options.file_id {
                // Use the supplied file ID, for example a merge of a file add
                node.address.context = file_id;
            } else {
                // Generate a file ID in case there will be metadata attached to it before
                // commit operation assigns a file ID
                node.address.context = uuid::Uuid::now_v7().into();
                lore_trace!(
                    "Generate file ID for file {} in {}: {}",
                    name,
                    relative_path.as_str(),
                    node.address.context
                );
            }
        }

        let node_id = state
            .node_add(repository.clone(), base_node, node, name.as_str())
            .await
            .forward::<StageError>("Failed to add a node to revision tree")?;

        let mark = staged_change(
            NodeFlags::StagedAdd,
            NodeFlags::DirtyAdd,
            options.node_flags,
        );
        state
            .node_mark(&repository, node_id, mark)
            .await
            .forward::<StageError>("Failed to mark node as staged")?;

        if let Some(ref tracker) = link_tracker {
            tracker.on_node_changed(&repository);
        }

        lore_trace!("Staged new node {node_id} for {name}");

        if info.is_dir() {
            stats
                .directory_checked_count
                .fetch_add(1, Ordering::Relaxed);
        } else {
            stats.file_checked_count.fetch_add(1, Ordering::Relaxed);
            stats.file_add_count.fetch_add(1, Ordering::Relaxed);
            event::LoreEvent::FileStageFile(LoreFileStageFileEventData {
                from_path: LoreString::default(),
                path: relative_path.join(name.as_str()).into(),
                action: LoreFileAction::Add,
            })
            .send();
        }

        return Ok(StagedChild {
            link: NodeLink {
                node: node_id,
                repository: repository.id,
                revision: state.revision(),
            },
            states,
            prepended: true,
            added: info.is_dir(),
        });
    }

    // TODO(vri): UCS-19227 - Links: Handle renaming and mismatching in staging

    let block_index = NodeBlock::index(node_link.node);
    let node_index = Node::index(node_link.node);
    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;

    let node = block.node(node_index);
    let mut event_action = None;
    let mut from_path = None;
    #[cfg(feature = "state-hooks")]
    crate::state::node::hooks::reached(crate::state::node::hooks::Point::StageWindow).await;

    // Undelete if deleted: the forced mark replaces the staged deletion. The merge flags stand
    // until an unstage drops them, as the mark keeps them; under merge options it sets the
    // options' merge state instead, as the stage's own mark below does again. The decisions
    // below read the node as found.
    if node.is_staged_delete() {
        lore_trace!("Undelete node {} before staging", node_link.node);

        let mark = staged_change(
            NodeFlags::StagedModify,
            NodeFlags::DirtyModify,
            options.node_flags,
        );
        state
            .node_mark(&repository, node_link.node, mark)
            .await
            .forward::<StageError>("Failed to mark node as staged")?;

        if let Some(ref tracker) = link_tracker {
            tracker.on_node_changed(&repository);
        }

        event_action = Some(LoreFileAction::Add);
    }

    let mut name = name;

    // Check for case mismatch and get the repository name before dropping the read lock.
    // The read lock held by node_name must be released before any block.write() call to
    // avoid deadlocking on the RwLock.
    let case_mismatch = {
        let node_name = block
            .node_name_ref(node_index)
            .forward::<StageError>("Failed to resolve node name")?;
        if *name != *node_name {
            Some(node_name.to_string())
        } else {
            None
        }
    };

    if let Some(node_name) = case_mismatch {
        match options.case_change {
            StageCaseChange::Keep => {
                // Keep the state name, update the file system to match
                lore_debug!(
                    "Case mismatch handling, keep node and update file system to match {}",
                    node_name
                );

                let from_path = relative_path.join(name);

                name = node_name;
                let to_path = relative_path.join(&name);

                unify_case(operation, &from_path, &to_path).await?;
            }
            StageCaseChange::Rename => {
                // Stage a rename operation, updating the repository to match the file system
                lore_debug!(
                    "Case mismatch handling, staging a move of node from {} to {name}",
                    node_name
                );

                let old_path = relative_path.join(&node_name);
                let new_path = relative_path.join(&name);
                if holds_both_spellings(operation, &relative_path, &node_name, &name).await {
                    lore_debug!(
                        "Case rename: old path {old_path} still exists alongside {new_path}, unifying file system"
                    );
                    unify_case(operation, &old_path, &new_path).await?;
                }

                from_path = Some(relative_path.join(&node_name).to_string());
                event_action = Some(LoreFileAction::Move);

                // The case alone changes, so the node keeps its name hash and its place.
                state
                    .node_rename(
                        repository.clone(),
                        node_link.node,
                        NameChange::Rename(name.as_str()),
                    )
                    .await
                    .forward::<StageError>("storing node name on stage move")?;
                state
                    .node_mark(
                        &repository,
                        node_link.node,
                        Mark::staged(NodeFlags::StagedMove | options.node_flags, true),
                    )
                    .await
                    .forward::<StageError>("Failed to mark node as staged")?;

                if let Some(ref tracker) = link_tracker {
                    tracker.on_node_changed(&repository);
                }
            }
            StageCaseChange::Error => {
                // Error out
                lore_error!(
                    "Node name {node_name} does not match file system name {name} in path {relative_path} - use case rename if you want to rename the repository to match the file system, or case keep if you want to update the file system to match the repository"
                );
                return Err(StageError::internal("A name case mismatch was detected"));
            }
        }
    }

    if !node.is_staged()
        || force
        || options.node_flags.contains(NodeFlags::StagedMerge)
        || node.is_staged_merge_unresolved()
    {
        let mut maybe_content_modified = false;
        // An unstaged-add node is a previously-staged add that the user
        // unstaged; re-staging must promote it back to a staged add even when
        // the filesystem content compares equal to the node's stored hash.
        let was_dirty_add = node.is_dirty_add();
        // Whether the node's own fields are written in place: a node of another kind than the
        // filesystem entry was staged for deletion above, and a new one added.
        let mut write_content = false;
        let mut resolves_merge = false;

        if !info.is_dir() {
            debug_assert!(node.is_file(), "a file entry over a node of another kind");
            let node_path = relative_path.join(name.as_str());
            let stage_file_node = info.mode_differs_from(node.mode)
                || file_modified_against_node(
                    repository.clone(),
                    &node,
                    info.mtime(),
                    info.size(),
                    &node_path,
                    !node.is_staged(),
                    operation,
                    &lore_storage::ContentHashes::default(),
                )
                .await
                .forward::<StageError>("Failed to determine if file is modified")?
                .is_modified();

            if stage_file_node {
                // The size the walk measured. Not the mode: a commit compares the file's mode with
                // the node's to see whether the revision carries a change to it, so a mode
                // recorded here would be compared with itself and a change to the executable bit
                // alone would be lost.
                write_content = true;
                maybe_content_modified = true;
                resolves_merge =
                    crate::commit::merged_past_the_view(&repository, &node_path, &node);
            } else if was_dirty_add {
                maybe_content_modified = true;
            }
        } else if node.mode != 0 {
            debug_assert!(!node.is_file(), "a directory entry over a file node");
            write_content = true;
            maybe_content_modified = true;
        } else if node.is_dirty_add() {
            // A new directory has no content change to detect but must still be
            // staged.
            maybe_content_modified = true;
        }

        if maybe_content_modified
            || force
            || options.node_flags.contains(NodeFlags::StagedMerge)
            || node.is_staged_merge_unresolved()
        {
            if write_content {
                let changed = state.node_change_in_place(
                    &block,
                    node_link.node,
                    (node.parent, node.name_hash),
                    |held| {
                        if info.is_dir() {
                            held.set_mode(0);
                        } else if resolves_merge {
                            let mode = if info.size() == 0 {
                                info.mode(held.mode)
                            } else {
                                held.mode
                            };
                            held.set_content(Address::default(), info.size(), mode);
                        } else {
                            held.set_size(info.size());
                        }
                        true
                    },
                );
                debug_assert!(changed, "the staged node left its place");
            }

            let (staged_flag, dirty_flag, action) = if was_dirty_add || node.is_staged_add() {
                (
                    NodeFlags::StagedAdd,
                    NodeFlags::DirtyAdd,
                    LoreFileAction::Add,
                )
            } else if node.is_staged_move() {
                (
                    NodeFlags::StagedMove,
                    NodeFlags::DirtyMove,
                    LoreFileAction::Move,
                )
            } else {
                (
                    NodeFlags::StagedModify,
                    NodeFlags::DirtyModify,
                    LoreFileAction::Keep,
                )
            };

            let mark = staged_change(staged_flag, dirty_flag, options.node_flags);
            state
                .node_mark(&repository, node_link.node, mark)
                .await
                .forward::<StageError>("Failed to mark node as staged")?;

            lore_trace!(
                "Staged existing node {} as modified for {}",
                node_link.node,
                relative_path.join(name.as_str())
            );

            if let Some(ref tracker) = link_tracker {
                tracker.on_node_changed(&repository);
            }

            if event_action.is_none() {
                event_action = Some(action);
            }
        }
    }

    if node.is_directory() {
        stats
            .directory_checked_count
            .fetch_add(1, Ordering::Relaxed);
    } else {
        stats.file_checked_count.fetch_add(1, Ordering::Relaxed);
    }

    let added =
        node.is_directory() && node.is_dirty_add() && event_action == Some(LoreFileAction::Add);
    if let Some(action) = event_action.filter(|_| !added) {
        stats.record_action(action, node.is_directory());
        if !node.is_directory() {
            event::LoreEvent::FileStageFile(LoreFileStageFileEventData {
                from_path: from_path.into(),
                path: relative_path.join(name.as_str()).into(),
                action,
            })
            .send();
        }
    }

    Ok(StagedChild {
        link: node_link,
        states,
        prepended: false,
        added,
    })
}

#[derive(Copy, Clone, PartialEq)]
pub(crate) enum MergeParent {
    Mine,
    Theirs,
    CherryPick,
    Revert,
}

/// The revision a resolve to `merge_parent` restores the paths of the merge in progress in `state`
/// from.
pub(crate) async fn merge_parent_revision(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    merge_parent: MergeParent,
) -> Result<Hash, StageError> {
    Ok(match merge_parent {
        MergeParent::Mine => state.parents()[0],
        MergeParent::Theirs => state.parents()[1],
        MergeParent::CherryPick => {
            state
                .revision_metadata(repository.clone())
                .await
                .forward::<StageError>("Failed to deserialize revision state")?
                .cherry_picked_from
        }
        MergeParent::Revert => {
            // For revert "theirs", we want the content that the revert would produce,
            // which is the PARENT of the reverted revision (i.e., the state before the change)
            let reverted_from = state
                .revision_metadata(repository.clone())
                .await
                .forward::<StageError>("Failed to deserialize revision state")?
                .reverted_from;
            let reverted_state = State::deserialize(repository.clone(), reverted_from)
                .await
                .forward::<StageError>("Failed to deserialize revision state")?;
            reverted_state.parent_self()
        }
    })
}

// Determine the merge NodeFlags based on the target vs the current repository state
pub(crate) fn determine_merge_action(
    target_exists: bool,
    current_exists: bool,
    target_node: Node,
    current_node: Node,
) -> NodeFlags {
    match (target_exists, current_exists) {
        (false, true) => {
            // The target doesn't exist, but current does -> Delete
            NodeFlags::StagedDelete
        }
        (true, false) => {
            // The target exists, but current does not exist -> Add
            NodeFlags::StagedAdd
        }
        (true, true) => {
            // Both exist, check if content differs for modify operation
            if target_node.address != current_node.address {
                NodeFlags::StagedModify
            } else {
                // Same content, no action needed
                NodeFlags::NoFlags
            }
        }
        (false, false) => {
            // Neither exists, no action
            NodeFlags::NoFlags
        }
    }
}

/// One side of a merge, cherry-pick or revert in progress, and the link mounts it holds.
struct MergeSide {
    state: Arc<State>,
    mounts: Vec<state::LinkMountInfo>,
}

struct MergeSides {
    mine: MergeSide,
    other: MergeSide,
}

impl MergeSides {
    /// The side a resolve to `merge_parent` restores from, and the side it replaces.
    fn restored_and_replaced(&self, merge_parent: MergeParent) -> (&MergeSide, &MergeSide) {
        if merge_parent == MergeParent::Mine {
            (&self.mine, &self.other)
        } else {
            (&self.other, &self.mine)
        }
    }
}

/// The state a resolve walks: the parent repository's, or a linked repository's, whose resolver
/// restores files only.
#[derive(Clone, Copy, PartialEq)]
enum ResolveScope {
    Parent,
    InsideLink,
}

struct MergeInProgress {
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    sides: tokio::sync::OnceCell<MergeSides>,
    abort: &'static str,
}

/// What a resolve of the merge in progress refuses because resolving it would corrupt a link: a
/// type change at a link mount either side of the merge holds, and a folder or link conflict
/// inside a linked repository.
#[derive(Clone)]
pub(crate) struct ResolveRefusal {
    merge: Arc<MergeInProgress>,
    scope: ResolveScope,
}

impl ResolveRefusal {
    /// The refusal for the merge in progress in `state`.
    pub(crate) fn new(repository: &Arc<RepositoryContext>, state: &Arc<State>) -> Self {
        Self {
            merge: Arc::new(MergeInProgress {
                repository: repository.clone(),
                state: state.clone(),
                sides: tokio::sync::OnceCell::new(),
                abort: branch::merge::get_merge_type(state).abort_command(),
            }),
            scope: ResolveScope::Parent,
        }
    }

    /// This refusal for a walk over a linked repository's state.
    pub(crate) fn inside_link(&self) -> Self {
        Self {
            merge: self.merge.clone(),
            scope: ResolveScope::InsideLink,
        }
    }

    /// Both sides of the merge, read once on first use.
    async fn sides(&self) -> Result<&MergeSides, StageError> {
        let merge = &self.merge;
        merge
            .sides
            .get_or_try_init(async || {
                let (repository, state) = (&merge.repository, &merge.state);
                let other = if state.is_cherry_pick() {
                    MergeParent::CherryPick
                } else if state.is_revert() {
                    MergeParent::Revert
                } else {
                    MergeParent::Theirs
                };
                let side = async |merge_parent| {
                    let revision = merge_parent_revision(repository, state, merge_parent).await?;
                    let state = State::deserialize(repository.clone(), revision)
                        .await
                        .forward::<StageError>("Failed to deserialize revision state")?;
                    let mounts = state::collect_link_mounts_below(
                        &state,
                        repository,
                        ROOT_NODE,
                        &RelativePath::new(),
                    )
                    .await
                    .forward_any::<StageError>("Failed to collect the link mounts of the merge")?;
                    Ok::<_, StageError>(MergeSide { state, mounts })
                };
                let (mine, other) = tokio::try_join!(side(MergeParent::Mine), side(other))?;
                Ok(MergeSides { mine, other })
            })
            .await
    }

    /// Whether either side holds a link mounted at `path`.
    async fn mounted_at(&self, path: &RelativePath) -> Result<bool, StageError> {
        let sides = self.sides().await?;
        Ok(sides
            .mine
            .mounts
            .iter()
            .chain(&sides.other.mounts)
            .any(|mount| mount.mount_path.as_lowercase_str() == path.as_lowercase_str()))
    }

    /// The refusal for `node` at `path`, which carries an unresolved conflict and is not a file,
    /// or `None` where a resolve walks into it.
    async fn at_node(
        &self,
        node: &Node,
        path: &RelativePath,
    ) -> Result<Option<NotSupported>, StageError> {
        if self.scope == ResolveScope::InsideLink && !node.is_link() {
            return Ok(Some(self.inside_link_conflict(path)));
        }
        if node.is_link() || self.mounted_at(path).await? {
            return Ok(Some(self.mount_type_change(path)));
        }
        Ok(None)
    }

    /// The refusal for resolving a path inside the link whose node in the parent's state is
    /// `local_node`, mounted at `mount`, where that node carries an unresolved conflict.
    pub(crate) async fn entering_link(
        &self,
        local_node: NodeID,
        mount: &RelativePath,
    ) -> Result<Option<NotSupported>, StageError> {
        let node = self
            .merge
            .state
            .node(self.merge.repository.clone(), local_node)
            .await
            .forward::<StageError>("Failed to read the link node")?;
        Ok(node
            .is_staged_merge_unresolved()
            .then(|| self.mount_type_change(mount)))
    }

    /// The refusal for restoring `paths` in the parent repository from the side `merge_parent`
    /// names, where that side holds a link mount at, above or below one of them, or the side it
    /// replaces holds one below, which restoring removes.
    async fn restoring(
        &self,
        merge_parent: MergeParent,
        paths: &[RelativePath],
    ) -> Result<Option<NotSupported>, StageError> {
        let (restored, replaced) = self.sides().await?.restored_and_replaced(merge_parent);
        let mount = restored
            .mounts
            .iter()
            .find(|mount| {
                paths
                    .iter()
                    .any(|path| mount.mount_path.overlaps_ignore_case(path))
            })
            .or_else(|| {
                replaced.mounts.iter().find(|mount| {
                    paths.iter().any(|path| {
                        path.covers_ignore_case(&mount.mount_path)
                            && path.as_lowercase_str() != mount.mount_path.as_lowercase_str()
                    })
                })
            });
        Ok(mount.map(|mount| self.mount_type_change(&mount.mount_path)))
    }

    /// The refusal for restoring the file at `path` inside a linked repository from a side that
    /// holds `target` there.
    fn restoring_file_inside_link(
        &self,
        target: &Node,
        path: &RelativePath,
    ) -> Option<NotSupported> {
        (!target.is_file()).then(|| self.inside_link_conflict(path))
    }

    fn mount_type_change(&self, mount: &RelativePath) -> NotSupported {
        NotSupported {
            operation: format!(
                "resolving a link mount type change at {mount} - abort with `{}`",
                self.merge.abort
            ),
        }
    }

    fn inside_link_conflict(&self, path: &RelativePath) -> NotSupported {
        NotSupported {
            operation: format!(
                "resolving the folder or link conflict at {path} inside a linked repository - \
                 abort with `{}`",
                self.merge.abort
            ),
        }
    }
}

/// What [`collect_merge_paths`] collected below a node.
#[derive(Default)]
pub(crate) struct StagedMergePaths {
    pub(crate) files: Vec<RelativePath>,
    /// Whether a node collected carries a merge conflict.
    pub(crate) holds_conflict: bool,
    /// The mount and the mounted node of each link met, whose paths are resolved in the linked
    /// repository's own state.
    pub(crate) links: Vec<(RelativePath, NodeLink)>,
}

/// Which nodes [`collect_merge_paths`] collects.
#[derive(Clone, Copy)]
pub(crate) struct MergeWalk {
    /// Collect conflicts not yet resolved rather than every staged merge.
    pub(crate) unresolved_only: bool,
    /// Collect a directory carrying a conflict no node below it carries: the conflict is with a
    /// file the other side put in its place.
    pub(crate) whole_directories: bool,
}

/// Collects the paths of the merged nodes at and below `node_id`, and the links met, with what
/// `refusal` refuses on the way.
pub(crate) fn collect_merge_paths(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    base_path: RelativePath,
    walk: MergeWalk,
    refusal: Option<ResolveRefusal>,
) -> Pin<Box<dyn Future<Output = Result<StagedMergePaths, StageError>> + Send>> {
    Box::pin(async move {
        let block = state
            .block(repository.clone(), NodeBlock::index(node_id))
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        let node = block.node(Node::index(node_id));

        let collects = |node: &Node| {
            if walk.unresolved_only {
                node.is_staged_merge_conflict() && node.is_staged_merge_unresolved()
            } else {
                node.is_staged_merge()
            }
        };

        if node.is_file() {
            if !collects(&node) {
                return Ok(StagedMergePaths::default());
            }
            return Ok(StagedMergePaths {
                files: vec![base_path],
                holds_conflict: node.is_staged_merge_conflict(),
                links: Vec::new(),
            });
        }
        if let Some(refusal) = &refusal
            && node.is_staged_merge_unresolved()
            && let Some(refused) = refusal.at_node(&node, &base_path).await?
        {
            return Err(refused.into());
        }
        if node.is_link() {
            let mut collected = StagedMergePaths::default();
            // A link nested in a linked repository is not merged with it, so it holds no merge.
            if refusal
                .as_ref()
                .is_none_or(|refusal| refusal.scope == ResolveScope::Parent)
            {
                collected.links.push((base_path, node.linked_node()));
            }
            return Ok(collected);
        }

        let mut collected = StagedMergePaths::default();
        let mut children =
            StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;

        while let Some((child_id, _child_node, child_name)) = children
            .next()
            .await
            .forward::<StageError>("Failed deserializing state node block")?
        {
            let child_path = base_path.push_into_buf(&child_name).freeze();
            // Release the block read lock before recursing (see NodeNameLock docs).
            drop(child_name);
            let mut child = collect_merge_paths(
                repository.clone(),
                state.clone(),
                child_id,
                child_path,
                walk,
                refusal.clone(),
            )
            .await?;
            collected.files.append(&mut child.files);
            collected.holds_conflict |= child.holds_conflict;
            collected.links.append(&mut child.links);
        }

        if walk.whole_directories
            && node.is_staged_merge_conflict()
            && collects(&node)
            && !collected.holds_conflict
        {
            return Ok(StagedMergePaths {
                files: vec![base_path],
                holds_conflict: true,
                links: collected.links,
            });
        }
        Ok(collected)
    })
}

/// Refuses to remove `path` from the working tree, which holds what `replaced` holds there, where
/// that loses what no resolve restores: an entry `replaced` does not track, a file changed since,
/// or a linked repository mounted below.
async fn refuse_losing_local_work(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    replaced: &Arc<State>,
    path: &RelativePath,
) -> Result<(), StageError> {
    let node_link = replaced
        .find_node_link(repository.clone(), path.as_str())
        .await
        .unwrap_or_default();
    if !node_link.is_valid() || node_link.crosses_link(repository, replaced) {
        return Err(LocalModifications.into());
    }
    if !state::collect_link_mounts_below(replaced, repository, node_link.node, path)
        .await
        .forward::<StageError>("Failed to read the links below the replaced path")?
        .is_empty()
    {
        return Err(StageError::internal(
            "Links not yet implemented, cannot perform actions in other repositories",
        ));
    }

    let side = NodeMapping {
        repository: repository.clone(),
        state: replaced.clone(),
        path: path.clone(),
        node: node_link.node,
    };
    let mut changes = state::diff_filesystem_subtree(
        operation,
        side.clone(),
        side,
        path.clone(),
        FilterMode::View,
        FilesystemDiffIntent::Report,
        Arc::new(Vec::new()),
    )
    .await
    .forward::<StageError>("Failed to compare the replaced path against the file system")?;
    // A tracked entry already gone from the working tree loses nothing.
    let mut local_work = None;
    while let Some(change) = changes.next().await {
        if change.action != change::FileAction::Delete {
            local_work = Some(change.path().clone());
            break;
        }
    }
    changes
        .finish()
        .await
        .forward::<StageError>("Failed to compare the replaced path against the file system")?;
    if let Some(changed) = local_work {
        lore_error!("Resolving {path} would remove the local changes at {changed}");
        return Err(LocalModifications.into());
    }
    Ok(())
}

pub(crate) async fn stage_from_parent_revision(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    merge_parent: MergeParent,
) -> Result<(), StageError> {
    with_operation(repository.file_system(), async |operation| {
        stage_from_parent_revision_in_operation(
            operation,
            repository.clone(),
            token,
            paths,
            merge_parent,
        )
        .await
    })
    .await
}

/// Gives the staged file `node` the executable bit `mode`, which a resolve realized it with:
/// staging it from the working tree records no bit, and a commit records the staged bit where it
/// reads none from the working tree. Does nothing for an invalid `node`.
async fn stage_resolved_mode(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node: NodeID,
    mode: u16,
) -> Result<(), StageError> {
    if !node.is_valid_node_id() {
        return Ok(());
    }
    crate::fs::realize::carry_staged_mode(state, repository, node, mode)
        .await
        .forward::<StageError>("Failed to stage the resolved file's mode")
}

/// The file `chosen`, the side a resolve takes, with the executable bit of `merged`, the file the
/// merge staged at its path, where the two are one file: a resolve chooses the content where the
/// sides conflict and never undoes a change of the bit, on every file it touches, conflicted or
/// merged cleanly. A file of another identity, added again on that side, keeps its own bit.
fn with_merged_mode(chosen: Node, merged: &Node) -> Node {
    if merged.is_file() && merged.address.context == chosen.address.context {
        Node {
            mode: merged.mode,
            ..chosen
        }
    } else {
        chosen
    }
}

/// Stages `paths` from the revision `merge_parent` names: a file is restored and staged here,
/// and a directory's changes are found and staged by [`stage_from_parent_state`], in a task per
/// directory.
///
/// The directory tasks and the change tasks below them draw on one budget of
/// [`MAX_CONCURRENT_TREE_TASKS`] permits, so that many tasks are live at most whatever the shape
/// of the resolved tree. A directory waits for a permit here, where nothing waited on needs one;
/// a change below a directory runs inline when none is free, since the directory's task holds
/// one itself.
async fn stage_from_parent_revision_in_operation(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    merge_parent: MergeParent,
) -> Result<(), StageError> {
    event::LoreEvent::FileStageBegin(LoreFileStageBeginEventData {
        path_count: paths.len(),
    })
    .send();

    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<StageError>("Failed to deserialize current revision anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    if !state.is_merge_or_cherry_pick_or_revert() {
        return Err(StageError::internal("No merge in progress"));
    }

    // Link-paths need a separate helper that operates against each link's
    // own state (and updates the parent's pin afterwards).
    let mut parent_paths: Vec<LoreString> = Vec::new();
    let mut link_paths: Vec<LinkPath> = Vec::new();
    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            // Invalid path — leave to the existing branch to emit the ignore
            // event and warn.
            parent_paths.push(path.clone());
            continue;
        };
        let node_link = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if node_link.is_valid_or_root() && node_link.repository != repository.id {
            link_paths.push(LinkPath {
                path: relative_path,
                named: Some(path.clone()),
            });
        } else {
            parent_paths.push(path.clone());
        }
    }

    // Every path passes the refusals before any is staged, so a refused resolve stages nothing.
    let refusal = ResolveRefusal::new(&repository, &state);

    if parent_paths.is_empty() {
        let link_groups =
            collect_link_groups(&repository, &state, link_paths, merge_parent, &refusal).await?;
        return Box::pin(stage_link_groups(
            operation,
            repository,
            token,
            state,
            link_groups,
            merge_parent,
        ))
        .await;
    }
    let paths = LoreArray::from_vec(parent_paths);

    let sides = refusal.sides().await?;
    let state_target = sides.restored_and_replaced(merge_parent).0.state.clone();

    // Get current repository state for merge action comparison
    let state_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    // No paths specified = resolve all merge files in the repository
    let paths = if paths.is_empty() {
        LoreArray::from_vec(vec![LoreString::from(".")])
    } else {
        paths
    };

    // Expand directory paths into individual file paths
    let mut resolved_paths = Vec::new();
    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path: {path}");
            continue;
        };
        lore_debug!(
            "User path [{}] transformed to relative path [{}] in repository {}",
            path.as_str(),
            relative_path.as_str(),
            repository.path_for_display()
        );

        // Use is_valid_or_root() since directory/root paths are now supported.
        let node_link = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if !node_link.is_valid_or_root() {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path, does not exist in staged state: {path}");
            continue;
        }

        if node_link.repository != repository.id {
            return Err(StageError::internal(
                "A path left to the parent repository resolves into a linked repository",
            ));
        }

        let block_index = NodeBlock::index(node_link.node);
        let node_index = Node::index(node_link.node);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        let node = block.node(node_index);

        if node.is_file() {
            if node.is_staged_merge() {
                resolved_paths.push(relative_path);
            } else {
                emit_path_ignore(path.as_str()).await;
                lore_debug!("Ignoring invalid path, node is not a staged merge: {path}");
            }
        } else {
            let expanded = collect_merge_paths(
                repository.clone(),
                state.clone(),
                node_link.node,
                relative_path,
                MergeWalk {
                    unresolved_only: false,
                    whole_directories: true,
                },
                Some(refusal.clone()),
            )
            .await?;
            resolved_paths.extend(expanded.files);
            link_paths.extend(expanded.links.into_iter().map(|(mount, _)| LinkPath {
                path: mount,
                named: None,
            }));
        }
    }

    let link_groups =
        collect_link_groups(&repository, &state, link_paths, merge_parent, &refusal).await?;
    if let Some(refused) = refusal.restoring(merge_parent, &resolved_paths).await? {
        return Err(refused.into());
    }

    Box::pin(stage_link_groups(
        operation.clone(),
        repository.clone(),
        token,
        state.clone(),
        link_groups,
        merge_parent,
    ))
    .await?;

    let stats = Arc::new(StageStats::default());
    #[cfg(not(feature = "test-util"))]
    let permits = MAX_CONCURRENT_TREE_TASKS;
    #[cfg(feature = "test-util")]
    let permits = RESOLVE_TASKS.budget();
    let budget = Arc::new(Semaphore::new(permits));
    let mut tasks = JoinSet::new();
    let dispatch_result: Result<(), StageError> = async {
        for relative_path in resolved_paths {
            // Re-resolve node link for the file path
            let node_link = state
                .find_node_link(repository.clone(), relative_path.as_str())
                .await
                .unwrap_or_default();
            if !node_link.is_valid() {
                continue;
            }

            let block_index = NodeBlock::index(node_link.node);
            let node_index = Node::index(node_link.node);
            let block = state
                .block(repository.clone(), block_index)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;
            let node_staged = block.node(node_index);

            let merge_flags = if node_staged.is_staged_merge() {
                match merge_parent {
                    MergeParent::Mine => NodeFlags::StagedMerge | NodeFlags::StagedMergeMine,
                    MergeParent::CherryPick | MergeParent::Revert | MergeParent::Theirs => {
                        NodeFlags::StagedMerge | NodeFlags::StagedMergeTheirs
                    }
                }
            } else {
                NodeFlags::NoFlags
            };
            let merge_conflict_flags = if node_staged.is_staged_merge_conflict() {
                NodeFlags::StagedMergeConflict | NodeFlags::StagedMergeResolved
            } else {
                NodeFlags::NoFlags
            };

            // Get target and current state node links for merge action detection
            let target_node_link = state_target
                .find_node_link(repository.clone(), relative_path.as_str())
                .await
                .unwrap_or_default();
            let current_node_link = state_current
                .find_node_link(repository.clone(), relative_path.as_str())
                .await
                .unwrap_or_default();

            let target_node = state_target
                .node(repository.clone(), target_node_link.node)
                .await
                .unwrap_or_default();
            let current_node = state_current
                .node(repository.clone(), current_node_link.node)
                .await
                .unwrap_or_default();

            // Determine merge action based on target vs current state
            let merge_action_flags = determine_merge_action(
                target_node_link.is_valid(),
                current_node_link.is_valid(),
                target_node,
                current_node,
            );

            let final_flags = merge_flags | merge_conflict_flags | merge_action_flags;

            lore_debug!(
                "Stage {} to {} parent revision, node flags {:?}, merge action: {:?}",
                relative_path.as_str(),
                if matches!(merge_parent, MergeParent::Mine) {
                    "self"
                } else {
                    "other"
                },
                final_flags,
                merge_action_flags
            );

            let options = StageOptions {
                case_change: StageCaseChange::Keep,
                node_flags: final_flags,
                file_id: Some(node_staged.address.context),
                no_children: false,
                scan: true,
            };

            // Check if conflict files exist before any work is done, so we can
            // preserve them for potential unresolve. Commit and abort clean them up.
            let had_conflict_files = sync::exist_merge_artifacts(&operation, &relative_path).await;

            async fn unlink_and_stage(
                operation: Arc<InstanceOperationImpl>,
                repository: Arc<RepositoryContext>,
                relative_path: RelativePath,
                state: Arc<State>,
                stats: Arc<StageStats>,
                options: StageOptions,
                preserve_conflict_files: bool,
            ) -> Result<(), StageError> {
                let _ = operation.remove_recursive(&relative_path).await;

                Box::pin(stage_filesystem_path(
                    operation.clone(),
                    NodeMapping::root(repository.clone(), state.clone()),
                    relative_path.clone(),
                    stats.clone(),
                    options,
                    None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                    None, // No layer mask
                    None, // No prefix map for a path resolved on its own
                    None, // No discard queue
                ))
                .await?;

                if !preserve_conflict_files {
                    sync::unlink_merge_artifacts(&operation, &relative_path).await;
                }
                Ok(())
            }

            if !target_node_link.is_valid() {
                // At this point it's known that the node exist in the merged revision and it's known
                // that it's a staged merge. If the path does not exist in the target state, the merge
                // involves a deleted file. Bring the filesystem up-to-date and stage the delete.
                lore_debug!(
                    "Stage {} from {} deleted node",
                    relative_path.as_str(),
                    if matches!(merge_parent, MergeParent::Mine) {
                        "self"
                    } else {
                        "other"
                    },
                );

                Box::pin(unlink_and_stage(
                    operation.clone(),
                    repository.clone(),
                    relative_path.clone(),
                    state.clone(),
                    stats.clone(),
                    options,
                    false,
                ))
                .await?;

                // If we stage a delete, check parents. If they are also empty in the current state,
                // and missing in the parent we are staging from, unlink them too.
                let mut search_path = relative_path.clone();
                search_path.pop();

                while !search_path.is_empty() {
                    lore_debug!(
                        "Searching parent directory {} after staging a delete",
                        search_path.as_str()
                    );

                    let parent_node_link = state_target
                        .find_node_link(repository.clone(), search_path.as_str())
                        .await
                        .unwrap_or_default();

                    if parent_node_link.is_valid() {
                        // If parent is present in the target state, nothing more to do
                        break;
                    }

                    // Find the count of the still valid child nodes in the currently staged state
                    let current_node_link = state
                        .find_node_link(repository.clone(), search_path.as_str())
                        .await
                        .forward::<StageError>("Failed to find subnode")?;
                    let children = state
                        .node_children(repository.clone(), current_node_link.node)
                        .await
                        .forward::<StageError>("Failed to find subnode")?;

                    let mut valid_children = 0;

                    for child in children.iter() {
                        let valid = match state.node(repository.clone(), *child).await {
                            Ok(node) => !node.is_staged_delete(),
                            Err(_) => false,
                        };

                        if valid {
                            valid_children += 1;
                        }
                    }

                    lore_debug!("Found {} valid children in parent", valid_children);
                    if valid_children != 0 {
                        break;
                    }

                    // No valid children in the currently staged state so remove unlink the parent directory
                    // and stage delete since we also know at this point the parent is missing in the target state
                    lore_debug!(
                        "Stage {} from {} deleted node",
                        search_path.as_str(),
                        if matches!(merge_parent, MergeParent::Mine) {
                            "self"
                        } else {
                            "other"
                        },
                    );
                    Box::pin(unlink_and_stage(
                        operation.clone(),
                        repository.clone(),
                        search_path.clone(),
                        state.clone(),
                        stats.clone(),
                        options,
                        false,
                    ))
                    .await?;

                    // Check parent
                    search_path.pop();
                }
                continue;
            }

            if target_node_link.repository != repository.id {
                return Err(StageError::internal(
                    "A path resolved in the parent repository is restored from a linked repository",
                ));
            }

            let block_index = NodeBlock::index(target_node_link.node);
            let node_index = Node::index(target_node_link.node);
            let block = state_target
                .block(repository.clone(), block_index)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;
            let node = block.node(node_index);

            lore_debug!(
                "Stage {} from {} node {:?}",
                relative_path.as_str(),
                if matches!(merge_parent, MergeParent::Mine) {
                    "self"
                } else {
                    "other"
                },
                node
            );

            // Where the two sides of the conflict hold a file and a directory, the working tree
            // holds the other one.
            if node_staged.node_type() != node.node_type()
                && operation
                    .file_info(&relative_path)
                    .await
                    .is_ok_and(|info| info.exists() && info.is_dir() != node.is_directory())
            {
                // The working tree holds the side not chosen.
                let replaced = if merge_parent == MergeParent::Mine {
                    sides.other.state.clone()
                } else {
                    state_current.clone()
                };
                refuse_losing_local_work(&operation, &repository, &replaced, &relative_path)
                    .await?;
                operation
                    .remove_recursive(&relative_path)
                    .await
                    .forward_with::<StageError, _>(|| {
                        format!("Unable to remove {relative_path}")
                    })?;
            }

            if node.is_file() {
                // Restoring to a merge parent leaves content the current revision does not
                // hold, so the times it lands with state nothing and are left to drop with
                // the operation.
                let node = with_merged_mode(node, &node_staged);
                let mode = node.mode;
                crate::fs::realize::realize_file(
                    repository.clone(),
                    operation.clone(),
                    relative_path.clone(),
                    node,
                    Arc::new(SyncRealizeStats::default()),
                )
                .await
                .forward::<StageError>("Unable to restore path to selected state")?;

                let staged = Box::pin(stage_filesystem_path(
                    operation.clone(),
                    NodeMapping::root(repository.clone(), state.clone()),
                    relative_path.clone(),
                    stats.clone(),
                    options,
                    None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                    None, // No layer mask
                    None, // No prefix map for a path resolved on its own
                    None, // No discard queue
                ))
                .await?;
                stage_resolved_mode(&repository, &state, staged.node, mode).await?;

                if !had_conflict_files {
                    sync::unlink_merge_artifacts(&operation, &relative_path).await;
                }
            } else {
                let permit = budget.clone().acquire_owned().await.map_err(|err| {
                    StageError::internal_with_context(err, "The task budget is closed")
                })?;
                #[cfg(feature = "test-util")]
                let live = RESOLVE_TASKS.enter();
                lore_spawn!(tasks, {
                    let operation = operation.clone();
                    let repository = repository.clone();
                    let state = state.clone();
                    let state_target = state_target.clone();
                    let stats = stats.clone();
                    let budget = budget.clone();
                    async move {
                        let _permit = permit;
                        #[cfg(feature = "test-util")]
                        let _live = live;
                        stage_from_parent_state(
                            operation,
                            repository.clone(),
                            state,
                            repository.clone(),
                            state_target,
                            relative_path,
                            target_node_link.node,
                            options,
                            stats,
                            budget,
                        )
                        .await
                    }
                });
                // Reaps the tasks that have finished and stops dispatch at the first failure;
                // the budget is what bounds the tasks live.
                lore_limit_drain_tasks!(
                    tasks,
                    MAX_CONCURRENT_TREE_TASKS,
                    StageError::internal("Failed to join task")
                )?;
            }
        }
        Ok::<(), StageError>(())
    }
    .await;

    let mut failure = dispatch_result.err();
    while let Some(task) = tasks.join_next().await {
        let joined = task
            .map_err(|e| StageError::internal_with_context(e, "Failed to join task"))
            .and_then(|result| result);
        failure = failure.or(joined.err());
    }
    if let Some(err) = failure {
        return Err(err);
    }

    // TODO(vri): UCS-17955 - Merging and conflict resolution for links
    // Serialize all staged links states recursively

    let count = LoreFileStageCountData::new(stats.clone());
    let total_count = count.total_count;
    event::LoreEvent::FileStageEnd(LoreFileStageEndEventData { count }).send();

    if total_count == 0 {
        return Ok(());
    }

    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize staged revision state")?;
    if !execution_context().globals().dry_run() {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<StageError>("Failed to serialize staged anchor")?;
    }

    event::LoreEvent::FileStageRevision(LoreFileStageRevisionEventData {
        repository: repository.id,
        revision: signature,
    })
    .send();

    Ok(())
}

/// A path inside a linked repository for [`collect_link_groups`] to resolve.
struct LinkPath {
    path: RelativePath,
    /// The path as the caller named it, where a walk did not meet it, to report it ignored.
    named: Option<LoreString>,
}

/// Reports `named` as ignored, where the caller named the path rather than a walk meeting it.
async fn emit_named_path_ignore(named: Option<&LoreString>) {
    if let Some(named) = named {
        emit_path_ignore(named.as_str()).await;
    }
}

/// A file a resolve restores inside a linked repository.
struct LinkFile {
    /// The path from the parent's root.
    user: RelativePath,
    below_mount: RelativePath,
    /// The path in the link's own states, which each number their nodes as they please.
    in_link: RelativePath,
    /// The node the target holds at `in_link`.
    target: NodeLink,
}

/// The files a resolve restores inside one linked repository, from one of the link's merge
/// parents (Mine = `parent_self`, Theirs = `parent_other`).
struct LinkGroup {
    local_node: NodeID,
    link_context: Arc<RepositoryContext>,
    link_path_rel: RelativePath,
    /// The node of `link_state_staged` mounted at `link_path_rel`.
    mounted: NodeID,
    source_path: RelativePath,
    link_state_staged: Arc<State>,
    link_state_target: Arc<State>,
    link_state_current: Arc<State>,
    files: Vec<LinkFile>,
    link_branch: BranchId,
}

/// Groups `paths`, which lie inside linked repositories, by link, and checks each against
/// `refusal` without staging anything. A link the merge did not merge holds nothing to resolve.
async fn collect_link_groups(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    paths: Vec<LinkPath>,
    merge_parent: MergeParent,
    refusal: &ResolveRefusal,
) -> Result<Vec<LinkGroup>, StageError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let refusal = refusal.inside_link();
    let mut groups: Vec<LinkGroup> = Vec::new();

    let links = crate::link::links_and_merged(state, repository.clone())
        .await
        .forward::<StageError>("Failed to read the links of the merge")?;
    let mut mounts = Vec::with_capacity(links.len());
    for (link_ref, merged) in links {
        let mount = state
            .node_path(repository.clone(), link_ref.local_node)
            .await
            .forward::<StageError>("Failed to resolve a link mount path")?;
        mounts.push((
            link_ref,
            merged,
            RelativePath::from_str(&mount).unwrap_or_default(),
        ));
    }

    for LinkPath {
        path: relative_path,
        named,
    } in paths
    {
        let Some(&(link_ref, merged, ref mount)) = mounts
            .iter()
            .filter(|(_, _, mount)| relative_path.below(mount).is_some())
            .max_by_key(|(_, _, mount)| mount.as_str().len())
        else {
            emit_named_path_ignore(named.as_ref()).await;
            lore_debug!("Ignoring path, does not resolve to any link: {relative_path}");
            continue;
        };

        let group_index = groups
            .iter()
            .position(|g| g.local_node == link_ref.local_node);
        let group = if let Some(idx) = group_index {
            &mut groups[idx]
        } else {
            if let Some(refused) = refusal
                .entering_link(link_ref.local_node as NodeID, mount)
                .await?
            {
                return Err(refused.into());
            }
            if !merged {
                emit_named_path_ignore(named.as_ref()).await;
                lore_debug!("Ignoring path, the link holding it is not merged: {relative_path}");
                continue;
            }
            let link_context = repository.to_link_context(link_ref.repository).await;
            let link_state_staged =
                state::State::deserialize(link_context.clone(), link_ref.signature)
                    .await
                    .forward::<StageError>("Failed to deserialize link staged state")?;
            // The link's merge metadata: `parent_self` == link's pre-merge
            // head on the target branch ("mine"); `parent_other` == link's
            // source branch head ("theirs"). Non-conflicting nodes have no
            // merge flags so we stage their existing content; conflicted
            // nodes restore from the chosen parent.
            let target_hash = match merge_parent {
                MergeParent::Mine => link_state_staged.parent_self(),
                MergeParent::CherryPick | MergeParent::Revert | MergeParent::Theirs => {
                    link_state_staged.parent_other()
                }
            };
            let mounted = state
                .node(repository.clone(), link_ref.local_node)
                .await
                .forward::<StageError>("Failed to load the link node")?
                .child;
            // `link_state_current` is approximated by `parent_self` — only
            // used for `determine_merge_action` so a precise distinction
            // (an actual pre-merge HEAD) isn't needed.
            let (link_state_target, link_state_current, source_path) = tokio::try_join!(
                async {
                    state::State::deserialize(link_context.clone(), target_hash)
                        .await
                        .forward::<StageError>("Failed to deserialize link target state")
                },
                async {
                    state::State::deserialize(link_context.clone(), link_state_staged.parent_self())
                        .await
                        .forward::<StageError>("Failed to deserialize link current state")
                },
                async {
                    crate::link::link_source_path(link_context.clone(), &link_state_staged, mounted)
                        .await
                        .forward::<StageError>("Failed to resolve the link source path")
                },
            )?;

            groups.push(LinkGroup {
                local_node: link_ref.local_node,
                link_context,
                link_path_rel: mount.clone(),
                mounted,
                source_path,
                link_state_staged,
                link_state_target,
                link_state_current,
                files: Vec::new(),
                link_branch: link_ref.branch,
            });
            groups.last_mut().expect("just pushed")
        };

        let in_group = |user: RelativePath| {
            let below_mount = RelativePath::from_str(user.below(&group.link_path_rel)?).ok()?;
            let in_link = group.source_path.join(below_mount.as_str());
            Some((user, below_mount, in_link))
        };
        let Some((user, below_mount, in_link)) = in_group(relative_path) else {
            continue;
        };
        let node_link = group
            .link_state_staged
            .find_node_link(group.link_context.clone(), in_link.as_str())
            .await
            .unwrap_or_default();
        if !node_link.is_valid_or_root() {
            emit_named_path_ignore(named.as_ref()).await;
            lore_debug!("Ignoring invalid link path, does not exist in link staged state: {user}");
            continue;
        }
        let block = group
            .link_state_staged
            .block(group.link_context.clone(), NodeBlock::index(node_link.node))
            .await
            .forward::<StageError>("Failed deserializing link state node block")?;
        let node = block.node(Node::index(node_link.node));
        let files = if node.is_file() {
            if !node.is_staged_merge() {
                emit_named_path_ignore(named.as_ref()).await;
                lore_debug!("Ignoring invalid path, link node is not a staged merge: {user}");
                continue;
            }
            vec![(user, below_mount, in_link)]
        } else {
            collect_merge_paths(
                group.link_context.clone(),
                group.link_state_staged.clone(),
                node_link.node,
                user,
                MergeWalk {
                    unresolved_only: false,
                    whole_directories: false,
                },
                Some(refusal.clone()),
            )
            .await?
            .files
            .into_iter()
            .filter_map(in_group)
            .collect()
        };
        for (user, below_mount, in_link) in files {
            let target = match group
                .link_state_target
                .find_node_link(group.link_context.clone(), in_link.as_str())
                .await
            {
                Ok(target) => target,
                Err(err) if err.is_node_not_found() => NodeLink::invalid(),
                Err(err) => {
                    Err(err).forward::<StageError>("Failed to find the link target node")?
                }
            };
            if target.is_valid() {
                let target_node = group
                    .link_state_target
                    .node(group.link_context.clone(), target.node)
                    .await
                    .forward::<StageError>("Failed to read the link target node")?;
                if let Some(refused) = refusal.restoring_file_inside_link(&target_node, &user) {
                    return Err(refused.into());
                }
            }
            group.files.push(LinkFile {
                user,
                below_mount,
                in_link,
                target,
            });
        }
    }
    Ok(groups)
}

/// Stages the files of `groups` from the merge parent `merge_parent` names, then re-serializes
/// each link's state and updates the parent's pin via `link::update_link_pin_by_path`.
async fn stage_link_groups(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: Arc<State>,
    groups: Vec<LinkGroup>,
    merge_parent: MergeParent,
) -> Result<(), StageError> {
    if groups.is_empty() {
        return Ok(());
    }

    let stats = Arc::new(StageStats::default());

    for group in groups.iter().filter(|group| !group.files.is_empty()) {
        for LinkFile {
            user: mount_path,
            below_mount,
            in_link: link_relative,
            target: target_node_link,
        } in &group.files
        {
            let staged_node_link = group
                .link_state_staged
                .find_node_link(group.link_context.clone(), link_relative.as_str())
                .await
                .unwrap_or_default();
            if !staged_node_link.is_valid_or_root() {
                continue;
            }
            let staged_block = group
                .link_state_staged
                .block(
                    group.link_context.clone(),
                    NodeBlock::index(staged_node_link.node),
                )
                .await
                .forward::<StageError>("Failed deserializing link state node block")?;
            let node_staged = staged_block.node(Node::index(staged_node_link.node));

            let merge_flags = if node_staged.is_staged_merge() {
                match merge_parent {
                    MergeParent::Mine => NodeFlags::StagedMerge | NodeFlags::StagedMergeMine,
                    MergeParent::CherryPick | MergeParent::Revert | MergeParent::Theirs => {
                        NodeFlags::StagedMerge | NodeFlags::StagedMergeTheirs
                    }
                }
            } else {
                NodeFlags::NoFlags
            };
            let merge_conflict_flags = if node_staged.is_staged_merge_conflict() {
                NodeFlags::StagedMergeConflict | NodeFlags::StagedMergeResolved
            } else {
                NodeFlags::NoFlags
            };

            // Look up the file in the link's target/current states for the
            // merge-action flags (Add / Modify / Delete / NoFlags).
            let current_node_link = group
                .link_state_current
                .find_node_link(group.link_context.clone(), link_relative.as_str())
                .await
                .unwrap_or_default();
            let target_node = group
                .link_state_target
                .node(group.link_context.clone(), target_node_link.node)
                .await
                .unwrap_or_default();
            let current_node = group
                .link_state_current
                .node(group.link_context.clone(), current_node_link.node)
                .await
                .unwrap_or_default();
            let merge_action_flags = determine_merge_action(
                target_node_link.is_valid(),
                current_node_link.is_valid(),
                target_node,
                current_node,
            );

            let final_flags = merge_flags | merge_conflict_flags | merge_action_flags;

            let options = StageOptions {
                case_change: StageCaseChange::Keep,
                node_flags: final_flags,
                file_id: Some(node_staged.address.context),
                no_children: false,
                scan: true,
            };

            // Mirror `stage_from_parent_revision`: realize the chosen
            // target's content on disk, then read it back into state via
            // `stage_filesystem_path`, anchored at the mount and the node of
            // the link's own state mounted there.
            if !target_node_link.is_valid() {
                // No file at the path → `stage_filesystem_path` stages a
                // delete; we just clear the on-disk file first.
                let _ = operation.remove_recursive(mount_path).await;
                Box::pin(stage_filesystem_path(
                    operation.clone(),
                    NodeMapping {
                        repository: group.link_context.clone(),
                        state: group.link_state_staged.clone(),
                        path: group.link_path_rel.clone(),
                        node: group.mounted,
                    },
                    below_mount.clone(),
                    stats.clone(),
                    options,
                    None,
                    None,
                    None, // No prefix map for a path resolved on its own
                    None, // No discard queue
                ))
                .await?;
                sync::unlink_merge_artifacts(&operation, mount_path).await;
                continue;
            }

            let target_block = group
                .link_state_target
                .block(
                    group.link_context.clone(),
                    NodeBlock::index(target_node_link.node),
                )
                .await
                .forward::<StageError>("Failed deserializing link target state node block")?;
            let node_t = target_block.node(Node::index(target_node_link.node));

            if node_t.is_file() {
                // `link_context.path` shares the parent's path and
                // `mount_path` is parent-relative, so realizing through the
                // link context writes to `<parent>/<mount>/<file>`.
                let node_t = with_merged_mode(node_t, &node_staged);
                let mode = node_t.mode;
                crate::fs::realize::realize_file(
                    group.link_context.clone(),
                    operation.clone(),
                    mount_path.clone(),
                    node_t,
                    Arc::new(SyncRealizeStats::default()),
                )
                .await
                .forward::<StageError>("Unable to restore link path to selected state")?;

                Box::pin(stage_filesystem_path(
                    operation.clone(),
                    NodeMapping {
                        repository: group.link_context.clone(),
                        state: group.link_state_staged.clone(),
                        path: group.link_path_rel.clone(),
                        node: group.mounted,
                    },
                    below_mount.clone(),
                    stats.clone(),
                    options,
                    None,
                    None,
                    None, // No prefix map for a path resolved on its own
                    None, // No discard queue
                ))
                .await?;
                stage_resolved_mode(
                    &group.link_context,
                    &group.link_state_staged,
                    staged_node_link.node,
                    mode,
                )
                .await?;

                sync::unlink_merge_artifacts(&operation, mount_path).await;
            }
        }

        let new_link_sig = group
            .link_state_staged
            .serialize(group.link_context.clone(), token)
            .await
            .forward::<StageError>("Failed to serialize link state")?;
        crate::link::update_link_pin_by_path(
            &state,
            repository.clone(),
            group.link_path_rel.as_str(),
            group.link_branch,
            new_link_sig,
        )
        .await
        .forward::<StageError>("Failed to update link pin")?;
    }

    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize parent staged state")?;
    if !execution_context().globals().dry_run() {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<StageError>("Failed to store parent staged anchor")?;
    }

    Ok(())
}

/// Stage the deletion of `path`, which realizing the target revision removed because
/// that revision does not hold it.
///
/// A path the tree does not hold either is already what the revision says, and stages
/// nothing. The node is resolved through its link so a deletion inside a linked
/// repository is staged against that repository's own state.
async fn stage_realized_delete(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path: &RelativePath,
    options: StageOptions,
    stats: Arc<StageStats>,
) -> Result<(), StageError> {
    let Ok(node_link) = state
        .find_node_link(repository.clone(), path.as_str())
        .await
    else {
        return Ok(());
    };
    let (repository, state) = node_link
        .resolve(repository, state)
        .await
        .forward::<StageError>("Failed to resolve node path in state")?;
    stage_delete(
        repository,
        state,
        path.clone(),
        node_link.node,
        options.node_flags,
        stats,
        None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
    )
    .await
}

/// Finds the changes between the working tree and `state_target` below `relative_path`, restores
/// the target's content, and stages each change in a task drawn from `budget`, the budget shared
/// with the directory tasks of [`stage_from_parent_revision_in_operation`]. A change runs inline
/// when no permit is free, never waiting for one: this task holds a permit itself, and directory
/// tasks each waiting for a permit a change needs would wait on each other.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_from_parent_state(
    operation: Arc<InstanceOperationImpl>,
    repository_current: Arc<RepositoryContext>,
    state_current: Arc<State>,
    repository_target: Arc<RepositoryContext>,
    state_target: Arc<State>,
    relative_path: RelativePath,
    node_id_target: NodeID,
    options: StageOptions,
    stats: Arc<StageStats>,
    budget: Arc<Semaphore>,
) -> Result<(), StageError> {
    let node_link_current = state_current
        .find_node_link(repository_current.clone(), relative_path.as_str())
        .await
        .unwrap_or_default();
    let (repository_current, state_current) = node_link_current
        .resolve(repository_current.clone(), state_current.clone())
        .await
        .forward::<StageError>("Failed to resolve node path in state")?;

    let mut changes = state::diff_filesystem_subtree(
        &operation,
        NodeMapping {
            repository: repository_target.clone(),
            state: state_target.clone(),
            path: relative_path.clone(),
            node: node_id_target,
        },
        NodeMapping {
            repository: repository_current.clone(),
            state: state_current.clone(),
            path: relative_path.clone(),
            node: node_link_current.node,
        },
        relative_path.clone(),
        FilterMode::Full,
        FilesystemDiffIntent::Report,
        Arc::new(Vec::new()),
    )
    .await
    .forward::<StageError>("Failed to calculate diff between file system and target state")?
    .collect()
    .await
    .forward::<StageError>("Failed to calculate diff between file system and target state")?;

    // Reverse changes to make it change from file system to state
    change::reverse(changes.as_mut_slice());

    lore_debug!(
        "Stage {} to parent revision, found {} changes",
        relative_path.as_str(),
        changes.len()
    );

    let changes = Arc::new(changes);

    // Unstage nodes that were already staged but whose disk state had to change to have them included in output
    for change in changes.iter().rev() {
        // The reverse above puts the target's side on `to`, whose node is the target's own, so
        // the staged node is found by path.
        let staged = state_current
            .find_node_link(repository_current.clone(), change.path().as_str())
            .await
            .unwrap_or_default();
        if !staged.is_valid() || staged.repository != repository_current.id {
            continue;
        }

        let block = state_current
            .block(repository_current.clone(), NodeBlock::index(staged.node))
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        state_current.node_clear_own(&block, staged.node, OwnFlags::Staged);
    }

    // Perform the changes
    crate::fs::realize::realize_changes(
        repository_current.clone(),
        operation.clone(),
        changes.clone(),
        None,
        false, /* No dry run */
        false, /* Not a merge */
        Arc::new(SyncRealizeStats::default()),
    )
    .await
    .forward::<StageError>("Unable to restore path to selected state")?;

    // Stage the files that were reverted to the given target state. A directory is staged before
    // what it holds, and before anything below it looks up its node.
    let mut in_path_order: Vec<&change::NodeChange> = changes.iter().collect();
    in_path_order.sort_by(|lhs, rhs| lhs.path().as_str().cmp(rhs.path().as_str()));
    let mut tasks = JoinSet::new();
    let dispatch_result: Result<(), StageError> = async {
        for change in in_path_order {
            let stages_directory =
                change.to.mapping.node.is_valid_node_id() && change.to.flags.is_directory();
            let mut parent_path = change.path().clone();
            parent_path.pop();
            let parent_node_link = state_current
                .find_node_link(repository_current.clone(), parent_path.as_str())
                .await
                .forward::<StageError>("Failed to find subnode")?;
            let file_name = if parent_path.is_empty() {
                change.path().to_string()
            } else {
                change.path().as_str()[(parent_path.len() + 1)..].to_string()
            };

            let (repository, state) = parent_node_link
                .resolve(repository_current.clone(), state_current.clone())
                .await
                .forward::<StageError>("Failed to resolve node path in state")?;
            // The conflict, the side and the action are the resolved path's own, and a restored
            // file keeps the identity the target gives it. What a directory holds is staged by its
            // own changes.
            let options = if change.path() == &relative_path {
                StageOptions {
                    no_children: stages_directory,
                    ..options
                }
            } else {
                StageOptions {
                    node_flags: options.node_flags & NodeFlags::StagedMerge,
                    file_id: change
                        .to
                        .mapping
                        .node
                        .is_valid_node_id()
                        .then_some(change.to.address.context),
                    no_children: stages_directory,
                    ..options
                }
            };
            let stats = stats.clone();
            let file_path = change.path().clone();
            let operation = operation.clone();
            let stage_change = async move {
                #[cfg(feature = "test-util")]
                RESOLVE_TASKS.wait_if_held().await;
                let info = operation
                    .file_info(&file_path)
                    .await
                    .forward::<StageError>("Failed to query file information")?;
                if !info.exists() {
                    return stage_realized_delete(repository, state, &file_path, options, stats)
                        .await;
                }
                // One change per task, each naming a whole path, so the base is
                // folded here rather than threaded from a walk.
                let parent_states = repository.filter.exclusion_states(&parent_path);
                stage_node_from_metadata(
                    &operation,
                    NodeMapping {
                        repository,
                        state,
                        path: parent_path,
                        node: parent_node_link.node,
                    },
                    file_name,
                    info,
                    options,
                    stats.clone(),
                    None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                    KnownChild::Unresolved,
                    parent_states,
                )
                .await
                .map(|staged| {
                    if staged.added {
                        stats.record_action(LoreFileAction::Add, true);
                    }
                })
            };
            if !stages_directory && let Ok(permit) = budget.clone().try_acquire_owned() {
                #[cfg(feature = "test-util")]
                let live = RESOLVE_TASKS.enter();
                lore_spawn!(tasks, async move {
                    let _permit = permit;
                    #[cfg(feature = "test-util")]
                    let _live = live;
                    stage_change.await
                });
            } else {
                stage_change.await?;
            }
            // Reaps the tasks that have finished and stops dispatch at the first failure; the
            // budget is what bounds the tasks live.
            lore_limit_drain_tasks!(
                tasks,
                MAX_CONCURRENT_TREE_TASKS,
                StageError::internal("Failed to join task")
            )?;
        }
        Ok::<(), StageError>(())
    }
    .await;

    let mut final_result = dispatch_result;
    while let Some(task) = tasks.join_next().await {
        final_result = match task {
            Ok(result) => final_result.and(result.map(|_| ())),
            Err(task_err) => final_result.and(Err(StageError::internal_with_context(
                task_err,
                "Failed to join task",
            ))),
        };
    }
    final_result?;

    // The directory carries the resolution even where the working tree already held the
    // target's content, which leaves nothing above to stage there.
    if node_link_current.is_valid()
        && !changes.iter().any(|change| change.path() == &relative_path)
        && state_current
            .node(repository_current.clone(), node_link_current.node)
            .await
            .is_ok_and(|node| node.is_directory())
    {
        state_current
            .node_mark(
                &repository_current,
                node_link_current.node,
                Mark::staged(options.node_flags, true),
            )
            .await
            .forward::<StageError>("Failed to mark node as staged")?;
        stats.directory_modify_count.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// Counts and holds the tasks one-side resolutions spawn, and sizes their budget, for a test.
#[cfg(feature = "test-util")]
pub static RESOLVE_TASKS: crate::test_util::TaskProbe = crate::test_util::TaskProbe::new();
