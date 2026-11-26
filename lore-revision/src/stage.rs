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
use smallvec::SmallVec;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::branch;
use crate::change;
use crate::change::FileAction;
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
use crate::infer::ConflictMarkers;
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
use crate::revision;
use crate::revision::sync;
use crate::revision::sync::SyncRealizeStats;
use crate::state;
use crate::state::AddedSince;
use crate::state::ChainMark;
use crate::state::Ext;
use crate::state::FoundNode;
use crate::state::LinkPin;
use crate::state::Mark;
use crate::state::NameChange;
use crate::state::NodeMapping;
use crate::state::OwnFlags;
use crate::state::State;
use crate::state::StateError;
use crate::state::StateNodeChildrenIterator;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::state::Stop;
use crate::state::file_modified_against_node;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

/// Data for the event emitted when a stage operation begins.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageBeginEventData {
    /// Number of paths requested for staging.
    pub path_count: usize,
}

/// Running counts of items processed during a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageProgressEventData {
    /// Current counts of items processed.
    pub count: LoreFileStageCountData,
}

/// Data for the event emitted when a stage operation completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageEndEventData {
    /// Final counts of items processed.
    pub count: LoreFileStageCountData,
}

/// Data for the event identifying the repository and revision involved in a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageRevisionEventData {
    /// Identifier of the repository.
    pub repository: RepositoryId,
    /// Revision the files are staged against.
    pub revision: Hash,
}

/// Data for the event emitted for each file affected by a stage operation. For a deleted
/// directory, it is emitted for each file the view shows below it, and for none the view leaves
/// out.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
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
    Cancelled,
    LinkPinsDiverge,
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
    state: Arc<State>,
    link_tracker: Arc<link::LinkTracker>,
) -> Result<(), StageError> {
    link::drain_link_tracker(repository, token, state, &link_tracker, true)
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
#[lore_macro::test_pub]
pub(crate) type PendingDiscards = Option<Arc<DiscardQueue>>;

/// See [`PendingDiscards`].
#[lore_macro::test_pub]
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
///
/// `known` answers which child of `base`'s node the first component of `remainder_path` names,
/// where the caller established it ([`KnownChild`]). The walk takes it as given, and searches from
/// it for the rest of the path, but for a path the filter leaves out, whose replaced node it
/// searches for from `base`.
///
/// Nothing at or below a link is staged where the provider holds its state: it serves a link from
/// its node, so what the working tree shows there is no change.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_filesystem_path(
    operation: Arc<InstanceOperationImpl>,
    base: NodeMapping,
    remainder_path: RelativePath,
    known: KnownChild,
    stats: Arc<StageStats>,
    options: StageOptions,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    layer_mask: Option<Arc<Vec<String>>>,
    prefixes: Option<Arc<util::fs::ResolvedPrefixes>>,
    discards: PendingDiscards,
) -> impl Future<Output = Result<NodeLink, StageError>> {
    stage_filesystem_path_reaching(
        operation,
        base,
        remainder_path,
        known,
        stats,
        options,
        link_tracker,
        layer_mask,
        prefixes,
        discards,
        None,
    )
}

/// [`stage_filesystem_path`], which sets `reached_link`, where given, to the node it staged at the
/// end of `remainder_path` in the linked State it reached across a link, at the path the tree spells
/// it at once staged. It leaves `reached_link` as it is where that node is in `base`'s State, or
/// where it stages no node there.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_filesystem_path_reaching(
    operation: Arc<InstanceOperationImpl>,
    base: NodeMapping,
    remainder_path: RelativePath,
    known: KnownChild,
    stats: Arc<StageStats>,
    options: StageOptions,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    layer_mask: Option<Arc<Vec<String>>>,
    prefixes: Option<Arc<util::fs::ResolvedPrefixes>>,
    discards: PendingDiscards,
    reached_link: Option<&mut Option<NodeMapping>>,
) -> Result<NodeLink, StageError> {
    debug_assert!(
        matches!(known, KnownChild::Unresolved) || !remainder_path.is_empty(),
        "a known child for an empty path"
    );
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
                FilterMode::TRACKED,
            );
            if !replaced_excluded {
                Box::pin(async {
                    let Some(node) = state
                        .locate_node_below(repository.clone(), base.node, remainder_path.as_str())
                        .await
                        .ok()
                        .and_then(|found| found.own_node())
                    else {
                        return Ok(());
                    };
                    stage_replaced_node(
                        repository.clone(),
                        state.clone(),
                        full_relative_path.clone(),
                        KnownChild::Resolved(Some(node)),
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
                known,
                info,
                options,
                &stats,
                link_tracker.as_ref(),
            )
            .await?
        {
            return Ok(node_link);
        }
        if let Some(reached) = reached_link
            && !Arc::ptr_eq(&descent.state, &state)
        {
            *reached = Some(descent.mapping());
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

    let Ok(found) = find_known_relative(
        &repository,
        &state,
        base.node,
        remainder_path.as_str(),
        known,
        operation.holds_state(),
    )
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
        if let Some(queue) = discards.as_deref()
            && Box::pin(moved_away_from(
                &repository,
                &queue.committed,
                &state,
                &full_relative_path,
            ))
            .await?
            .is_some()
        {
            lore_debug!(
                "Path {full_relative_path} is the source of a move, staged at its destination"
            );
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
    let node_link = found.identity();
    if node_link.node == INVALID_NODE {
        return Ok(NodeLink::invalid());
    }
    let own = !found.crossed_link();
    let (node_id, numbering) = found.held((repository.clone(), state.clone())).into_parts();
    let (current_repository, node_state) = match (link_tracker.as_ref(), numbering) {
        // Every link crossed is tracked, so a delete nested two or more levels deep folds its pin
        // up through all of them.
        (Some(tracker), _) if !own => {
            let chain = crate::link::resolve_tracked_link_chain(
                base.clone(),
                state.clone(),
                remainder_path.clone(),
                tracker,
            )
            .await
            .forward::<StageError>("Failed to resolve link chain")?;
            (chain.innermost.repository, chain.innermost.state)
        }
        (_, numbering) => numbering,
    };
    // A block of its own, so the future holds the node only until the discard check.
    let (held, states) = {
        let Some((node, states)) = absent_named_node(
            filter,
            &current_repository,
            &node_state,
            node_id,
            &full_relative_path,
            parent_states,
            force,
        )
        .await?
        else {
            lore_trace!("Path excluded by filter: {}", full_relative_path.as_str());
            return Ok(NodeLink::invalid());
        };

        if own
            && let Some(queue) = discards.as_deref()
            && let Some(root) = uncommitted_absent_root(
                &operation,
                &repository,
                &state,
                &queue.committed,
                node_id,
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
        return Ok(if own { node_link } else { NodeLink::default() });
    }

    lore_debug!(
        "Path {} exist in repository, stage deletion",
        full_relative_path
    );
    let links_registered = node_state.registers_link();
    stage_absent_delete(
        current_repository,
        node_state,
        full_relative_path,
        node_id,
        held,
        options.node_flags,
        stats,
        link_tracker,
        states,
        force,
        links_registered,
    )
    .await?;

    Ok(NodeLink::default())
}

/// The node the staged tree `state` holds moved away from `path`, where `committed`, the revision
/// it is staged on, holds it at `path` outside any link: marked moved, dirty or staged, with the
/// file ID it has there. `None` where `committed` holds no such node at `path`. Its callers take
/// it only for a path the staged tree holds no node at, or an added or a moved one, on a path they
/// box.
pub(crate) async fn moved_away_from(
    repository: &Arc<RepositoryContext>,
    committed: &State,
    state: &State,
    path: &RelativePath,
) -> Result<Option<NodeID>, StageError> {
    let stop = committed
        .find_to_link::<true>(repository, ROOT_NODE, path.as_str(), true)
        .await
        .forward::<StageError>("Failed to look up a path in the revision the stage is based on")?;
    let (Stop::End, node) = (stop.at, stop.node) else {
        return Ok(None);
    };
    if node == ROOT_NODE {
        return Ok(None);
    }
    let was = committed
        .node(repository.clone(), node)
        .await
        .forward::<StageError>("Failed to read a node of the revision the stage is based on")?;
    let now = match state.node(repository.clone(), node).await {
        Ok(now) => now,
        Err(error) if error.is_node_not_found() => return Ok(None),
        Err(error) => {
            return Err(error).forward::<StageError>("Failed to read the staged node a move took");
        }
    };
    Ok((!now.is_discarded()
        && (now.is_dirty_move() || now.is_staged_move())
        && now.address.context == was.address.context)
        .then_some(node))
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
    let (states, reason) = filter
        .child_tracked_reason(
            parent,
            path,
            is_directory,
            FilterMode::TRACKED.absence(force),
            async || state.tracks(repository, id, flags).await,
        )
        .await
        .forward::<StageError>("Failed to read the revision the tree was built on")?;
    Ok((!Filter::emit(path, reason)).then_some((node, states)))
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

/// The node `path` names below `base`, as [`State::locate_node_below`] finds it, with `known` the
/// child of `base` its first component names where the caller established it: that child for a
/// path of one component, else a search from it for the rest of the path. A known child that is a
/// link leaves the search to start at `base`, which crosses the link. Where `ends_at_links`, a
/// search reaching a link at or above `path` answers [`INVALID_NODE`] instead, reading nothing of
/// the link. `Err` where the tree holds no node at `path`.
async fn find_known_relative(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    base: NodeID,
    path: &str,
    known: KnownChild,
    ends_at_links: bool,
) -> Result<FoundNode, ()> {
    let own = |node| FoundNode::own(node, repository, state);
    let (start, rest) = match (known, path.split_once('/')) {
        (KnownChild::Unresolved, _) => (base, path),
        (KnownChild::Resolved(None), _) => return Err(()),
        (KnownChild::Resolved(Some(node)), None) if !ends_at_links => {
            return Ok(own(node));
        }
        (KnownChild::Resolved(Some(node)), rest) => {
            let is_link = state
                .node(repository.clone(), node)
                .await
                .map_err(drop)?
                .is_link();
            match rest {
                _ if is_link && ends_at_links => return Ok(FoundNode::default()),
                None => return Ok(own(node)),
                Some(_) if is_link => (base, path),
                Some((_, rest)) => (node, rest),
            }
        }
    };
    if !ends_at_links {
        return state
            .locate_node_below(repository.clone(), start, rest)
            .await
            .map_err(drop);
    }
    let stop = state
        .find_to_link::<true>(repository, start, rest, true)
        .await
        .map_err(drop)?;
    match stop.at {
        Stop::End => Ok(own(stop.node)),
        Stop::Link(_) => Ok(FoundNode::default()),
        Stop::Missing => Err(()),
    }
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
    /// Not part of [`stage_filesystem_path_reaching`]: the path it borrows to fold the verdict would
    /// be reserved there across the directory walk.
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

    /// The node the descent stands at, in the State holding it, at the path the tree spells it at.
    fn mapping(&self) -> NodeMapping {
        NodeMapping {
            repository: self.repository.clone(),
            state: self.state.clone(),
            path: self.relative_path.clone().freeze(),
            node: self.node,
        }
    }

    /// Stage each component of `remainder_path` in turn and step into the node it names, and
    /// into the linked state where that node is a link. A provider holding its state serves
    /// nothing below a link, so there the descent ends, staging nothing.
    ///
    /// Each component is staged under the name `remainder_path` spells it with, which the caller
    /// resolves against the file system, so `stage_node_from_metadata` does not look up its case
    /// again. The first component is the child `known` answers where the caller established it,
    /// and every later one is looked up in its directory, once, a node not marked deleted before
    /// one that is ([`State::find_live_subnode`]): the search a directory takes for the
    /// nested repository check answers its stage as well. The final component is the staged
    /// path, staged as `info`; every other one is staged as a directory. `Some` is the link of a
    /// component that was not staged, which ends the descent.
    ///
    /// A node a search found marked dirty as added and not staged, other than a link, has the node
    /// of the other kind it replaced beside it staged deleted, a file for a directory and a
    /// directory for a file, as a dirty mark of a type change leaves the two, where the view keeps
    /// that kind at the path ([`stage_replaced_node`]). That costs one scan of the directory's
    /// children for each such node on the path; a node a listing answered costs none. A link on
    /// either side is left to the commit, which takes the deletion of a node marked dirty as
    /// deleted beside a staged one of its name.
    ///
    /// Its own future: inline, what a step holds across its awaits would be reserved in the
    /// directory walk's state of [`stage_filesystem_path_reaching`].
    #[allow(clippy::too_many_arguments)]
    async fn descend(
        &mut self,
        operation: &Arc<InstanceOperationImpl>,
        mut remainder_path: RelativePath,
        mut known: KnownChild,
        info: FileInfo,
        options: StageOptions,
        stats: &Arc<StageStats>,
        link_tracker: Option<&Arc<crate::link::LinkTracker>>,
    ) -> Result<Option<NodeLink>, StageError> {
        while !remainder_path.is_empty() {
            let is_final_component = remainder_path.parent().is_none();
            let current_name = remainder_path.pop_root();
            let mut current_known = std::mem::replace(&mut known, KnownChild::Unresolved);
            if current_name == "." {
                continue;
            }
            let searched = matches!(current_known, KnownChild::Unresolved);

            let current_info = if is_final_component {
                info
            } else {
                FileInfo::Directory
            };

            // A named path is refused rather than skipped: the caller asked for
            // this path specifically, and staging it into the parent would take
            // content the nested repository owns.
            if current_info.is_dir() {
                if matches!(current_known, KnownChild::Unresolved) {
                    current_known = match self
                        .state
                        .find_live_subnode(
                            self.repository.clone(),
                            self.node,
                            hash::hash_string(current_name),
                        )
                        .await
                    {
                        Ok((node, _)) => KnownChild::Resolved(Some(node)),
                        Err(error) if error.is_node_not_found() => KnownChild::Resolved(None),
                        Err(_) => KnownChild::Unresolved,
                    };
                }
                let held = match current_known {
                    KnownChild::Resolved(held) => held,
                    KnownChild::Unresolved => None,
                }
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
                marked_added,
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
                current_known,
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

            let parent = (self.node, self.states);
            self.states = states;

            let node = self
                .state
                .node(self.repository.clone(), node_link.node)
                .await
                .forward::<StageError>(
                    "Node not found in child node list, inconsistent repository state",
                )?;

            self.node = node_link.node;

            let replaced = if searched && marked_added && !node.is_link() {
                let replaced_directory = !node.is_directory();
                let (replaced_states, out_of_view) = self.repository.filter.child_excludes_tree(
                    parent.1,
                    &self.relative_path,
                    replaced_directory,
                    FilterMode::TRACKED,
                );
                if out_of_view {
                    None
                } else {
                    self.state
                        .find_subnode_beside(
                            self.repository.clone(),
                            parent.0,
                            (node_link.node, hash::hash_string(current_name)),
                            |held| {
                                !held.is_staged_delete()
                                    && if replaced_directory {
                                        held.is_directory()
                                    } else {
                                        held.is_file()
                                    }
                            },
                        )
                        .await
                        .forward::<StageError>(
                            "Failed to search for a node a type change replaced",
                        )?
                        .map(|(replaced, _)| (replaced, replaced_directory, replaced_states))
                }
            } else {
                None
            };
            if let Some((replaced, replaced_directory, replaced_states)) = replaced {
                Box::pin(stage_replaced_node(
                    self.repository.clone(),
                    self.state.clone(),
                    self.relative_path.clone().freeze(),
                    KnownChild::Resolved(Some(replaced)),
                    parent.0,
                    replaced_directory,
                    replaced_states,
                    options.node_flags,
                    stats.clone(),
                    link_tracker.cloned(),
                ))
                .await?;
            }

            if node.is_link() && operation.holds_state() {
                return Ok(Some(NodeLink::invalid()));
            }
            if node.is_link() {
                let link_repository_id: RepositoryId = node.address.context.into();
                let link_revision = node.address.hash;
                let linked_node = node.child;

                lore_debug!(
                    "Transition into link with ID {link_repository_id} at revision {link_revision}"
                );

                let linked_repository = self.repository.to_link_context(link_repository_id).await;
                let linked_state = if let Some(tracker) = link_tracker {
                    tracker
                        .track(
                            (&self.repository, &self.state),
                            node_link.node,
                            link_repository_id,
                            link_revision,
                        )
                        .await
                } else {
                    State::deserialize(linked_repository.clone(), link_revision).await
                }
                .forward::<StageError>("Failed to deserialize revision state")?;

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
        .locate_node(repository.clone(), parent_path.as_str())
        .await
        .forward_with::<StageError, _>(|| format!("Invalid path {parent_path}"))?;
    if !base_node.is_valid_or_root() {
        return Err(StageError::internal(format!("Invalid path {parent_path}")));
    }

    let name = relative_path.name();
    node.name_hash = hash::hash_string(name);

    let node_flags = NodeFlags::from_bits_retain(node.flags);

    let (base_id, (repository, state)) = base_node.held((repository, state)).into_parts();

    match state
        .find_subnode(repository.clone(), base_id, node.name_hash)
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
                    (base_id, existing_node.name_hash),
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
        .node_add(repository.clone(), base_id, node, name)
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
                    })?
                    == ConflictMarkers::Present)
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

/// Whether staging the deletion of a node held as `held`, with the stage's own `node_flags`, leaves
/// the node as it is: it holds the staged deletion and every staged flag the stage sets, as a node
/// holding them is left by [`State::node_mark`]'s unforced mark. A merge's flags the node lacks
/// mark it again.
pub(crate) fn deletion_staged_already(held: NodeFlags, node_flags: NodeFlags) -> bool {
    held.contains(NodeFlags::StagedDelete | (node_flags & NodeFlags::StagedBits))
}

/// Counts the deletion of `node` at `path` in `stats`, and sends a file's Delete event.
fn report_staged_delete(stats: &StageStats, node: &Node, path: &RelativePath) {
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
            path: LoreString::from(path),
            action: LoreFileAction::Delete,
        })
        .send();
        stats.file_checked_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether an absent directory the tree holds as `held` may hold a change a pending merge staged:
/// it carries a `Staged` bit while a merge is pending.
pub(crate) fn holds_pending_merge_change(state: &State, held: NodeFlags) -> bool {
    held.contains(NodeFlags::Staged) && state.is_merge()
}

/// Mark `node_id` staged for deletion, reporting the node when this call is the one that
/// marked it so a caller walking the subtree knows whether it still has work below. A merge's
/// stage keeps an add the node holds beside the deletion, an add taken back, as the revision it
/// was staged on lacks the node; any other stage records the deletion alone, keeping no add.
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
    if deletion_staged_already(NodeFlags::from_bits_retain(node.flags), node_flags) {
        return Ok(None);
    }

    lore_debug!("Stage delete of node {}", node_id);
    report_staged_delete(stats, &node, relative_path);

    let held_add = if node_flags.contains(NodeFlags::StagedMerge) {
        NodeFlags::from_bits_retain(node.flags) & NodeFlags::StagedAdd
    } else {
        NodeFlags::NoFlags
    };
    let mark = staged_change(
        NodeFlags::StagedDelete | held_add,
        NodeFlags::DirtyDelete,
        node_flags,
    );
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

/// Stages the deletion of the node `node_id` at `relative_path`, and of everything below a
/// directory this call marks, with the stage's own `node_flags`.
#[lore_macro::test_pub]
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
/// verdict is `states`, so the node is asked about as what it is: the callers ask in
/// [`FilterMode::TRACKED`], the view alone, as for a node the tree holds, which is past the ignore
/// slot, and [`stage_absent_delete`] leaves out one only marked added. `known_child` is the node as
/// the caller established it, and `base_node` the directory a node it left unresolved is looked up
/// in. Nothing is staged where the tree holds no node there or one of the kind the working tree
/// holds.
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
    let links_registered = state.registers_link();
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
        links_registered,
    )
    .await
    .map(|_| ())
}

/// What staging the deletion of a node the working tree lacks makes of it.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Absence {
    /// The working tree held the node, and its deletion is staged with everything below it.
    Deleted,
    /// The working tree never held the node under the view, so its absence deletes nothing.
    NeverHeld,
    /// The node stays: a link, or a directory holding what staging keeps.
    Kept,
}

/// What staging made of the children of an absent directory, folded as a walk settles each.
///
/// Where `keeps_partial`, a child the filter leaves out or never held keeps the directory: in a tree
/// that registers a link ([`State::registers_link`]) it may hold a link node the registry names,
/// and in a directory that may hold a pending merge's change ([`holds_pending_merge_change`]), that
/// change.
pub(crate) struct AbsentChildren {
    kept: bool,
    held: bool,
    keeps_partial: bool,
}

impl AbsentChildren {
    /// The children of a directory that keeps part of what it holds where `keeps_partial` says,
    /// already kept where `kept` says.
    pub(crate) fn of(keeps_partial: bool, kept: bool) -> Self {
        Self {
            kept,
            held: false,
            keeps_partial,
        }
    }

    /// Folds in what staging made of one child, a child the filter leaves out as never held.
    pub(crate) fn add(&mut self, child: Absence) {
        match child {
            Absence::Deleted => self.held = true,
            Absence::NeverHeld => self.kept |= self.keeps_partial,
            Absence::Kept => self.kept = true,
        }
    }

    /// What the directory is: kept where a child stays, deleted where `keeps_partial` or the working
    /// tree held the directory, as the view `creates` it or as it held a child, and never held
    /// otherwise.
    pub(crate) fn settle(self, creates: bool) -> Absence {
        if self.kept {
            Absence::Kept
        } else if self.keeps_partial || self.held || creates {
            Absence::Deleted
        } else {
            Absence::NeverHeld
        }
    }
}

/// Stage the deletion of `node_id`, which the file system does not hold, and answer what it made
/// of the node.
///
/// A mount stays absent while the linked repository cannot be read, so the link stays. A node the
/// view leaves out is not on disk, so its absence alone deletes nothing. The ignore file has no say
/// over a node the tree tracks, and leaves out only an add marked dirty, which was never staged. A
/// directory the working tree held, one the view keeps ([`FilterStates::creates_directory`]) or
/// one holding a node it held, is staged as deleted with everything below it, what the filter
/// leaves out included, which committing drops with it, and one the view keeps and hides everything
/// below ([`Filter::view_hides_below`]) is staged so at once, what it holds unread. Where
/// `keeps_partial`, as in a tree that registers a link, and in a directory that may hold a pending
/// merge's change ([`holds_pending_merge_change`]), a directory stays with every directory on its
/// path wherever staging keeps or leaves out a node below it, as [`AbsentChildren`] says. A node
/// already staged for deletion is marked again only as [`deletion_staged_already`] says.
///
/// `held` is what the tree holds `node_id` as: its type and what is staged on it. `states` is the
/// filter verdict at `relative_path`, which each child steps from, and `force` whether the stage
/// is forced past the ignore file. A child is asked about in the tracked mode.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn stage_absent_delete(
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
    keeps_partial: bool,
) -> impl Future<Output = Result<Absence, StageError>> + Send {
    async move {
        if deletion_staged_already(held, node_flags) {
            return Ok(Absence::Deleted);
        }

        if held.contains(NodeFlags::Link) {
            lore_debug!(
                "Link mount {relative_path} is absent from the file system, keeping the link"
            );
            return Ok(Absence::Kept);
        }

        let keeps_partial = keeps_partial || holds_pending_merge_change(&state, held);
        if held.is_directory()
            && (keeps_partial
                || !states.creates_directory()
                || !repository.filter.view_hides_below(states, &relative_path))
        {
            match stage_absent_children(
                &repository,
                &state,
                &relative_path,
                node_id,
                held,
                node_flags,
                &stats,
                link_tracker.as_ref(),
                states,
                force,
                keeps_partial,
            )
            .await?
            {
                Absence::Deleted => {}
                absence => return Ok(absence),
            }
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
        Ok(Absence::Deleted)
    }
}

/// Stages the deletion of the children of the absent directory `node_id` that the working tree
/// held, as [`stage_absent_delete`] does, and answers what it makes of the directory
/// ([`AbsentChildren::settle`]).
///
/// An empty directory the view excludes was never held, as nothing materializes it, and neither
/// was one the ignore rules exclude that the tree only marks. One the tree tracks is deleted past
/// them.
#[allow(clippy::too_many_arguments)]
async fn stage_absent_children(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    relative_path: &RelativePath,
    node_id: NodeID,
    held: NodeFlags,
    node_flags: NodeFlags,
    stats: &Arc<StageStats>,
    link_tracker: Option<&Arc<crate::link::LinkTracker>>,
    states: FilterStates,
    force: bool,
    keeps_partial: bool,
) -> Result<Absence, StageError> {
    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
            .await
            .forward::<StageError>("Failed to list directory node children")?;

    let mut settled = AbsentChildren::of(keeps_partial, false);
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
        let (child_states, reason) = repository
            .filter
            .child_tracked_reason(
                states,
                &child_path,
                is_directory,
                FilterMode::TRACKED.absence(force),
                async || state.tracks(repository, child_node_id, child_held).await,
            )
            .await
            .forward::<StageError>("Failed to read the revision the staged tree was built on")?;
        if reason.is_some() {
            settled.add(Absence::NeverHeld);
            continue;
        }
        settled.add(
            stage_absent_delete_recurse(
                repository.clone(),
                state.clone(),
                child_path,
                child_node_id,
                child_held,
                node_flags,
                stats.clone(),
                link_tracker.cloned(),
                child_states,
                force,
                keeps_partial,
            )
            .await?,
        );
    }

    if !had_child
        && states.excluded(FilterMode::Full)
        && (states.excluded(FilterMode::View)
            || !state
                .tracks(repository, node_id, held)
                .await
                .forward::<StageError>(
                    "Failed to read the revision the staged tree was built on",
                )?)
    {
        return Ok(Absence::NeverHeld);
    }
    Ok(settled.settle(states.creates_directory()))
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
    keeps_partial: bool,
) -> Pin<Box<dyn Future<Output = Result<Absence, StageError>> + Send>> {
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
        keeps_partial,
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

/// Most entries whose name hashes [`DirectoryChildren::shared_by_entries`] sorts on the stack; it
/// allocates for a longer listing.
const ENTRIES_SORTED_ON_STACK: usize = 256;

/// The children of a directory node by name hash: `(name hash, sibling index, child node id)`,
/// ordered by the first two, so the children carrying one hash come in sibling order.
#[lore_macro::test_pub]
pub(crate) struct NameHashIndex(Vec<(u64, u32, NodeID)>);

impl NameHashIndex {
    /// The index of a directory's children in sibling order, each with the name hash it carries,
    /// built in the listing's allocation.
    pub(crate) fn new(child: Vec<(NodeID, u64)>) -> Self {
        let mut by_name_hash: Vec<(u64, u32, NodeID)> = child
            .into_iter()
            .enumerate()
            .map(|(index, (node, name_hash))| (name_hash, index as u32, node))
            .collect();
        by_name_hash.sort_unstable();
        Self(by_name_hash)
    }

    /// Where the index holds the children carrying `name_hash`.
    fn start(&self, name_hash: u64) -> usize {
        self.0.partition_point(|&(hash, _, _)| hash < name_hash)
    }

    /// The children carrying `name_hash`, from `start`, where the index holds them
    /// ([`Self::start`]), in sibling order, each with its sibling index.
    fn matching_from(
        &self,
        start: usize,
        name_hash: u64,
    ) -> impl Iterator<Item = (u32, NodeID)> + Clone + '_ {
        self.0[start..]
            .iter()
            .take_while(move |&&(hash, _, _)| hash == name_hash)
            .map(|&(_, index, node)| (index, node))
    }

    /// The children carrying `name_hash`, in sibling order, each with its sibling index.
    fn matching(&self, name_hash: u64) -> impl Iterator<Item = (u32, NodeID)> + Clone + '_ {
        self.matching_from(self.start(name_hash), name_hash)
    }

    /// Every child carrying `name_hash`, in sibling order.
    pub(crate) fn carrying(&self, name_hash: u64) -> impl Iterator<Item = NodeID> + Clone + '_ {
        self.matching(name_hash).map(|(_, node)| node)
    }
}

/// The children of a directory node, indexed by name hash, and which of them a
/// file system entry has claimed.
///
/// Ties are held in sibling order: two names differing only in case hash the
/// same, and a claim takes the first child not already claimed.
#[lore_macro::test_pub]
pub(crate) struct DirectoryChildren {
    /// Child node ids in sibling order, [`INVALID_NODE`] where a file system
    /// entry has claimed the child.
    node: Vec<NodeID>,
    /// Every child, by name hash. The id is held here as well, where a claim has
    /// taken it out of `node`.
    by_name_hash: NameHashIndex,
    /// The chain as the listing began it, which everything linked into it since
    /// sits ahead of while no reattach moves it; `None` for a link's or a file's.
    since: Option<ChainMark>,
    /// The name hashes two or more of the file system entries claiming the
    /// children carry, once a claim has asked ([`Self::shared_by_entries`]).
    shared_hashes: Option<Box<[u64]>>,
    /// Whether the walk added a child for one of the entries ([`Self::count_own_add`]).
    own_added: bool,
}

impl DirectoryChildren {
    /// The children of a directory node in sibling order, with the name hash
    /// each of them carries, and the mark of the chain the listing began from.
    #[lore_macro::test_pub]
    pub(crate) fn new(child: Vec<(NodeID, u64)>, since: Option<ChainMark>) -> Self {
        Self {
            node: child.iter().map(|&(node, _)| node).collect(),
            by_name_hash: NameHashIndex::new(child),
            since,
            shared_hashes: None,
            own_added: false,
        }
    }

    /// The name hashes two or more of `entries`, the file system entries claiming the children,
    /// carry, sorted: found on the first call, and kept for the later ones, which give the same
    /// entries. The entries come in the order the file system lists them, so the call sorts a copy
    /// of their hashes, `O(n log n)` once per directory, and allocates only for a hash two of them
    /// share or for more than [`ENTRIES_SORTED_ON_STACK`] entries.
    fn shared_by_entries(&mut self, entries: &[DirectoryEntry]) -> &[u64] {
        self.shared_hashes.get_or_insert_with(|| {
            let mut name_hashes: SmallVec<[u64; ENTRIES_SORTED_ON_STACK]> =
                entries.iter().map(|entry| entry.name_hash).collect();
            name_hashes.sort_unstable();
            let mut shared = Vec::new();
            for pair in name_hashes.windows(2) {
                if pair[0] == pair[1] && shared.last() != Some(&pair[0]) {
                    shared.push(pair[0]);
                }
            }
            shared.into_boxed_slice()
        })
    }

    /// Whether nothing linked into the chain since the listing carries `name_hash`, the name hash
    /// of one of `entries` or of a child none of them names, so the listing answers for it alone:
    /// it carries no mark, as a link's or a file's listing does; or the chain changed since the
    /// listing only by the walk's own adds, each for an entry no listed child answered, and the
    /// walk made none or no other entry carries `name_hash`.
    fn answers_alone(&mut self, state: &State, entries: &[DirectoryEntry], name_hash: u64) -> bool {
        match self.since.as_ref().map(|since| since.unchanged(state)) {
            None => true,
            Some(unchanged) => {
                unchanged
                    && (!self.own_added
                        || self
                            .shared_by_entries(entries)
                            .binary_search(&name_hash)
                            .is_err())
            }
        }
    }

    /// The first child carrying `name_hash` that nothing has claimed yet, marked
    /// as claimed, with where the listing holds the children carrying it
    /// ([`Self::carrying_from`]). `None` where the directory holds no such child, or holds only
    /// ones already claimed.
    #[lore_macro::test_pub]
    fn claim(&mut self, name_hash: u64) -> Option<(NodeID, usize)> {
        let start = self.by_name_hash.start(name_hash);
        let (index, node) = self
            .by_name_hash
            .matching_from(start, name_hash)
            .find(|&(index, _)| self.node[index as usize] != INVALID_NODE)?;
        self.node[index as usize] = INVALID_NODE;
        Some((node, start))
    }

    /// The first child carrying `name_hash`, claimed or not, with every child carrying it marked
    /// claimed, so the pass over what nothing claimed leaves each of them, and where the listing
    /// holds them ([`Self::carrying_from`]). `None` where the directory holds no such child.
    #[lore_macro::test_pub]
    fn claim_every(&mut self, name_hash: u64) -> Option<(NodeID, usize)> {
        let start = self.by_name_hash.start(name_hash);
        let mut first = None;
        for (index, node) in self.by_name_hash.matching_from(start, name_hash) {
            self.node[index as usize] = INVALID_NODE;
            first.get_or_insert(node);
        }
        first.map(|node| (node, start))
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

    /// Counts a node the walk itself added to the directory for one of its file system entries,
    /// prepending it, in the mark, so the walk's own adds never make its searches stale nor show
    /// as a reattach, and notes that it added one. Only an add that landed counts: counting one
    /// that did not could hide a reattach, or another writer's add from
    /// [`ChainMark::unchanged`].
    #[lore_macro::test_pub]
    pub(crate) fn count_own_add(&mut self) {
        if let Some(since) = self.since.as_mut() {
            since.count_own_prepend();
        }
        self.own_added = true;
    }

    /// Drops the mark of the chain the listing began from, ahead of a pass that may free a slot of
    /// the chain, since no slot may be freed while a mark lives ([`ChainMark`]). The listing
    /// answers alone from then on.
    pub(crate) fn drop_mark(&mut self) {
        self.since = None;
    }

    /// The first child the listing holds carrying `name_hash`, claimed or not,
    /// which is the one a search of the chain reaches once past what was linked
    /// into it since.
    #[lore_macro::test_pub]
    pub(crate) fn holds(&self, name_hash: u64) -> Option<NodeID> {
        self.carrying(name_hash).next()
    }

    /// Every child the listing holds carrying `name_hash`, claimed or not, in sibling order.
    pub(crate) fn carrying(&self, name_hash: u64) -> impl Iterator<Item = NodeID> + Clone + '_ {
        self.carrying_from(self.by_name_hash.start(name_hash), name_hash)
    }

    /// [`Self::carrying`] from `start`, where the listing holds the children carrying
    /// `name_hash`, as a claim answers it, or where it holds none.
    pub(crate) fn carrying_from(
        &self,
        start: usize,
        name_hash: u64,
    ) -> impl Iterator<Item = NodeID> + Clone + '_ {
        self.by_name_hash
            .matching_from(start, name_hash)
            .map(|(_, node)| node)
    }

    /// Where the listing holds the children carrying `name_hash`.
    pub(crate) fn start_of(&self, name_hash: u64) -> usize {
        self.by_name_hash.start(name_hash)
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

    /// [`Self::unclaimed`], each child with its name hash, in name hash order.
    #[lore_macro::test_pub]
    pub(crate) fn unclaimed_by_name_hash(&self) -> impl Iterator<Item = (NodeID, u64)> + '_ {
        self.by_name_hash
            .0
            .iter()
            .filter(|&&(_, index, _)| self.node[index as usize] != INVALID_NODE)
            .map(|&(name_hash, _, node)| (node, name_hash))
    }
}

/// A directory's children, each with the name hash it carries, and the mark of the chain the
/// listing began from.
#[lore_macro::test_pub]
pub(crate) type DirectoryListing = (Vec<(NodeID, u64)>, ChainMark);

/// The children of `node` listed, where `node` is a directory; `None` for any other node, which
/// holds no children of its own: a link's listing would follow it into the State it names.
#[lore_macro::test_pub]
pub(crate) async fn directory_children(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node: NodeID,
) -> Result<Option<DirectoryListing>, StateError> {
    if !state.node(repository.clone(), node).await?.is_directory() {
        return Ok(None);
    }
    let (listed, mark) = state
        .node_children_with_name_hash_marked(repository.clone(), node)
        .await?;
    Ok(mark.map(|mark| (listed, mark)))
}

/// Whether it is established which child of a directory a name belongs to. `T` is the child, its
/// node by default.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KnownChild<T = NodeID> {
    /// The child, or that the directory holds none. Taken as given.
    Resolved(Option<T>),
    /// Nothing established; search the directory's chain.
    Unresolved,
}

/// A child a claim found for an entry, for [`claim_live`] and the dirty walk to look for a live one
/// among the children carrying its name hash.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Claimed {
    pub(crate) node: NodeID,
    /// Where the listing holds the children carrying the name hash, where the listing answered;
    /// `None` where a search did.
    pub(crate) start: Option<usize>,
}

impl Claimed {
    /// A child a search answered.
    fn searched(node: NodeID) -> Self {
        Self { node, start: None }
    }
}

/// The listing's answer to the claim of `name_hash`, the name hash of one of `entries`, the
/// directory's file system entries, or of a child none of them names, where the listing answers
/// alone; [`KnownChild::Unresolved`] where only a search of the chain does ([`claim_child`]).
///
/// The listing answers with the first listed child carrying `name_hash` that nothing claimed,
/// claimed now. Failing that, it answers with the listed child carrying it, claimed or not, where
/// nothing linked into the chain since the listing carries it
/// ([`DirectoryChildren::answers_alone`]).
///
/// The listing's own answers are taken as given: no writer but the walk's own tasks, each in its
/// own directory, runs beside a stage's walk or a `lore dirty` walk (the node module's
/// Limitations, "Staging's listings"), so no child the listing holds is renamed, moved or removed
/// meanwhile.
#[lore_macro::test_pub]
pub(crate) fn claim_from_listing(
    children: &mut DirectoryChildren,
    state: &State,
    entries: &[DirectoryEntry],
    name_hash: u64,
) -> KnownChild<Claimed> {
    if let Some((node, start)) = children.claim(name_hash) {
        return KnownChild::Resolved(Some(Claimed {
            node,
            start: Some(start),
        }));
    }
    if children.answers_alone(state, entries, name_hash) {
        KnownChild::Resolved(children.holds(name_hash).map(Claimed::searched))
    } else {
        KnownChild::Unresolved
    }
}

/// The child carrying `name_hash`, the name hash of one of `entries`, the directory's file system
/// entries, or of a child none of them names, as a search of the whole chain finds it, where the
/// listing answers alone; [`KnownChild::Unresolved`] where only a search of the chain does
/// ([`claim_child`]).
///
/// The listing answers with the first listed child carrying `name_hash`, claimed or not, and
/// claims every child carrying it ([`DirectoryChildren::claim_every`]). That is the child a search
/// reaches: the walk adds a child only for a name hash the listing holds none of, and no other
/// writer runs beside it, as [`claim_from_listing`] has it. Failing that, it answers that the
/// directory holds none, where nothing linked into the chain since the listing carries
/// `name_hash` ([`DirectoryChildren::answers_alone`]).
#[lore_macro::test_pub]
pub(crate) fn find_from_listing(
    children: &mut DirectoryChildren,
    state: &State,
    entries: &[DirectoryEntry],
    name_hash: u64,
) -> KnownChild<Claimed> {
    if let Some((node, start)) = children.claim_every(name_hash) {
        return KnownChild::Resolved(Some(Claimed {
            node,
            start: Some(start),
        }));
    }
    if children.answers_alone(state, entries, name_hash) {
        KnownChild::Resolved(None)
    } else {
        KnownChild::Unresolved
    }
}

/// The child of `directory_node` that `name_hash` belongs to, where its listing does not answer
/// alone ([`claim_from_listing`], [`find_from_listing`]).
///
/// A child is prepended, so whatever was linked into the chain since the listing
/// was taken lies ahead of it. Only that much is walked; the listing answers from
/// the index behind it, claimed children included. Where a reattach may have put a
/// node in front of what was linked in since, the whole chain is searched. The
/// answer is the one a search of the whole chain reaches; a child a search answered
/// after a reattach is claimed in the listing too, which may hold it under the name
/// it had. A link's or a file's listing, which carries no mark, answers from the
/// listing alone.
#[lore_macro::test_pub]
pub(crate) async fn claim_child(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    directory_node: NodeID,
    children: &mut DirectoryChildren,
    name_hash: u64,
) -> Result<Option<Claimed>, StageError> {
    let Some(since) = children.since.as_ref() else {
        return Ok(children.holds(name_hash).map(Claimed::searched));
    };
    let searched = match state
        .find_subnode_added_since(repository.clone(), since, name_hash)
        .await
        .forward::<StageError>("Failed to search the directory node children")?
    {
        AddedSince::Found(node) => node,
        AddedSince::Absent => return Ok(children.holds(name_hash).map(Claimed::searched)),
        AddedSince::Stale => match state
            .find_subnode(repository.clone(), directory_node, name_hash)
            .await
        {
            Ok(node) => node,
            Err(error) if error.is_node_not_found() => {
                return Ok(children.holds(name_hash).map(Claimed::searched));
            }
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
    Ok(Some(Claimed::searched(searched)))
}

/// `claimed`, the child of `children`'s directory an entry carrying `name_hash` claimed, or,
/// where it is marked deleted, the other child carrying the name hash that
/// [`State::live_among`] prefers, claimed now too, so the pass over what nothing claimed leaves
/// both.
async fn claim_live(
    repository: &Arc<RepositoryContext>,
    state: &State,
    children: &mut DirectoryChildren,
    name_hash: u64,
    claimed: Option<Claimed>,
) -> Result<Option<NodeID>, StageError> {
    let Some(Claimed { node: found, start }) = claimed else {
        return Ok(None);
    };
    let start = start.unwrap_or_else(|| children.start_of(name_hash));
    let live = state
        .live_among(repository, children.carrying_from(start, name_hash), found)
        .await
        .forward::<StageError>("Failed to read a child of a listed name")?;
    if live != found {
        children.claim_node(live);
    }
    Ok(Some(live))
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
    let (child_states, reason) = repository
        .filter
        .child_tracked_reason(
            states,
            &filter_path,
            !node.is_file(),
            FilterMode::TRACKED.absence(force),
            async || state.tracks(repository, child, held).await,
        )
        .await
        .forward::<StageError>("Failed to read the revision the staged tree was built on")?;
    let excluded = Filter::emit(&filter_path, reason);
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
    for item in &items {
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

            let claimed =
                match claim_from_listing(&mut children, &state, &items, directory.name_hash) {
                    KnownChild::Resolved(claimed) => claimed,
                    KnownChild::Unresolved => match claim_child(
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
                    },
                };
            let claimed = match claim_live(
                &repository,
                &state,
                &mut children,
                directory.name_hash,
                claimed,
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

            let claimed = match claim_from_listing(&mut children, &state, &items, file.name_hash) {
                KnownChild::Resolved(claimed) => claimed,
                KnownChild::Unresolved => match claim_child(
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
                },
            };
            let claimed =
                match claim_live(&repository, &state, &mut children, file.name_hash, claimed).await
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

    // Nothing reads the entries past the claims: they go before the deletes and the subtree's
    // tasks.
    drop(items);

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
            state.registers_link(),
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
/// A link is kept and not descended where the provider holds its state, which serves the link
/// from its node.
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

    if from_node.is_link() && operation.holds_state() {
        return Ok(true);
    }
    if from_node.is_link() {
        let link = from_node.linked_node();

        let linked_repository = repository.to_link_context(link.repository).await;
        let linked_state = if let Some(ref tracker) = link_tracker {
            tracker
                .track(
                    (&repository, &state),
                    node_link.node,
                    link.repository,
                    link.revision,
                )
                .await
        } else {
            State::deserialize(linked_repository.clone(), link.revision).await
        }
        .forward::<StageError>("Failed to deserialize linked state")?;

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
    /// Whether the stage found the child's node marked dirty as added and not staged, as a dirty
    /// mark of a type change leaves the node it adds beside the node it replaces.
    pub marked_added: bool,
}

impl StagedChild {
    /// A child that was not staged, which nothing descends into.
    fn invalid() -> Self {
        Self {
            link: NodeLink::invalid(),
            states: FilterStates::ROOT,
            prepended: false,
            added: false,
            marked_added: false,
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

/// Stage the child `name` of `base` from the file information the caller already holds, the child
/// `known_child` answers or, unresolved, the one a search finds, a node not marked deleted before
/// one that is ([`State::find_live_subnode`]).
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
/// merge's included, keeps its action. A file marked dirty as moved and not staged is staged as
/// moved, whether or not its content changed ([`State::staged_edit_flags`]). A directory marked dirty
/// as moved and not staged takes a bare staged bit, outside a merge, which the caller stages as the
/// directory's whole move once its walks are done ([`crate::file::stage::stage_promoted_moves`]).
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
            marked_added: false,
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
            .find_live_subnode(repository.clone(), base_node, name_hash)
            .await
        {
            Ok((node, _)) => Ok(Some(node)),
            Err(err) if err.is_node_not_found() => Ok(None),
            Err(err) => Err(err),
        },
    };

    // Asked once the tree has been searched: a node it tracks as the kind the working tree holds is
    // past the ignore slot. The node is read only where the ignore rules would drop the path.
    let force = execution_context().globals().force();
    let filter_path = base_relative_path.join(name.as_str());
    let (states, excluded) = if force {
        repository.filter.child_emit_unless_forced(
            true,
            FilterMode::View,
            parent_states,
            &filter_path,
            info.is_dir(),
            FilterMode::TRACKED,
        )
    } else {
        let (states, mut reason, asks) = repository.filter.child_untracked_reason(
            parent_states,
            &filter_path,
            info.is_dir(),
            FilterMode::TRACKED,
        );
        // The flags alone, so the node is not held across the tree's answer.
        if asks
            && let Ok(Some(node_id)) = found_child
            && let Ok(flags) = state
                .node(repository.clone(), node_id)
                .await
                .map(|node| NodeFlags::from_bits_retain(node.flags))
            && state
                .tracks_as(&repository, node_id, flags, info.is_dir())
                .await
                .forward::<StageError>("Failed to read the revision the staged tree was built on")?
        {
            reason = repository.filter.tracked_reason(
                states,
                &filter_path,
                info.is_dir(),
                FilterMode::TRACKED,
            );
        }
        (states, Filter::emit(&filter_path, reason))
    };
    if excluded {
        lore_trace!("Node excluded by filter: {}", filter_path.as_str());
        if !matches!(known_child, KnownChild::Resolved(None)) {
            let (replaced_states, replaced_excluded) = repository.filter.child_excludes_tree(
                parent_states,
                &filter_path,
                !info.is_dir(),
                FilterMode::TRACKED,
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
            marked_added: false,
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

    if node.is_directory()
        && node.is_dirty_move()
        && !node.is_staged()
        && !options.node_flags.contains(NodeFlags::StagedMerge)
    {
        state
            .node_mark(
                &repository,
                node_link.node,
                Mark::staged(NodeFlags::Staged, false),
            )
            .await
            .forward::<StageError>("Failed to stage the move of a directory")?;
        if let Some(ref tracker) = link_tracker {
            tracker.on_node_changed(&repository);
        }
    } else if !node.is_staged()
        || force
        || options.node_flags.contains(NodeFlags::StagedMerge)
        || node.is_staged_merge_unresolved()
    {
        let mut maybe_content_modified = false;
        // An unstaged-add node is a previously-staged add that the user
        // unstaged; re-staging must promote it back to a staged add even when
        // the filesystem content compares equal to the node's stored hash.
        let was_dirty_add = node.is_dirty_add();
        let dirty_moved_file = node.is_file() && node.is_dirty_move() && !node.is_staged();
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
            } else if was_dirty_add || dirty_moved_file {
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

            let (staged_flag, dirty_flag) = if was_dirty_add {
                (NodeFlags::StagedAdd, NodeFlags::DirtyAdd)
            } else if dirty_moved_file && !write_content {
                State::staged_move_flags(&node)
            } else {
                State::staged_edit_flags(&node)
            };
            let action = LoreFileAction::from(FileAction::from_node_flags(staged_flag.bits()));

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
            if action == LoreFileAction::Move && from_path.is_none() {
                from_path =
                    Box::pin(committed_path(&repository, &state, (node_link.node, &node))).await?;
            }
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
        marked_added: node.is_dirty_add() && !node.is_staged(),
    })
}

/// The path `node`, numbered `id` in `state`, has in the revision `state` is staged on, which the
/// stage event of its move names as the path it came from. `None` where that revision holds no
/// node of its kind and file ID at `id`. Boxed by its caller, as only a file staged as moved takes
/// it.
async fn committed_path(
    repository: &Arc<RepositoryContext>,
    state: &State,
    (id, node): (NodeID, &Node),
) -> Result<Option<String>, StageError> {
    let mut name = String::new();
    let place = state
        .base_place_below(
            repository,
            (state.committed_revision(), id, ROOT_NODE),
            (
                NodeFlags::from_bits_retain(node.flags),
                node.address.context,
            ),
            &mut name,
        )
        .await
        .forward::<StageError>("Failed to read the path a moved file had")?;
    Ok(place.map(|(_, path)| path.to_string()))
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
    /// The mounts of both sides, lowercase and sorted, as lookups match them.
    mount_paths: Vec<String>,
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
/// path inside a conflict at a link mount, a conflict whose resolve removes or adds a link below
/// it, and a folder or link conflict inside a linked repository. It also finds the conflicts at
/// link mounts a resolve settles whole.
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
                let mut mount_paths: Vec<String> = mine
                    .mounts
                    .iter()
                    .chain(&other.mounts)
                    .map(|mount| mount.mount_path.as_lowercase_str().to_string())
                    .collect();
                mount_paths.sort_unstable();
                mount_paths.dedup();
                Ok(MergeSides {
                    mine,
                    other,
                    mount_paths,
                })
            })
            .await
    }

    /// Whether either side holds a link mounted at `path`.
    async fn mounted_at(&self, path: &RelativePath) -> Result<bool, StageError> {
        let path = path.as_lowercase_str();
        Ok(self
            .sides()
            .await?
            .mount_paths
            .binary_search_by(|mount| mount.as_str().cmp(path))
            .is_ok())
    }

    /// Whether `node`, which the staged state holds at `path`, carries a conflict at a link mount
    /// of the parent repository, one not yet resolved where `unresolved_only`. A resolve settles it
    /// whole, with [`MountResolve`].
    async fn is_mount_conflict(
        &self,
        node: &Node,
        path: &RelativePath,
        unresolved_only: bool,
    ) -> Result<bool, StageError> {
        let conflicted = if unresolved_only {
            node.is_staged_merge_unresolved()
        } else {
            node.is_staged_merge_conflict()
        };
        Ok(self.scope == ResolveScope::Parent && conflicted && self.mounted_at(path).await?)
    }

    /// Whether the staged state holds a conflict at a link mount at `path`, as
    /// [`Self::is_mount_conflict`] describes.
    pub(crate) async fn is_mount_conflict_at(
        &self,
        path: &RelativePath,
        unresolved_only: bool,
    ) -> Result<bool, StageError> {
        if !self.mounted_at(path).await? {
            return Ok(false);
        }
        let (repository, state) = (&self.merge.repository, &self.merge.state);
        let Some((_, node)) = staged_nodes_at(repository, state, path)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(false);
        };
        self.is_mount_conflict(&node, path, unresolved_only).await
    }

    /// Checks the conflicts at `mounts` for a resolve to `merge_parent` against the working tree,
    /// which holds the side each was last resolved to, this branch's where none.
    pub(crate) async fn check_mounts(
        &self,
        operation: &Arc<InstanceOperationImpl>,
        mut mounts: Vec<RelativePath>,
        merge_parent: MergeParent,
    ) -> Result<MountResolve, StageError> {
        // A resolve naming a mount and a path above it reaches the mount twice.
        mounts.sort_unstable_by(|a, b| a.as_lowercase_str().cmp(b.as_lowercase_str()));
        mounts.dedup_by(|a, b| a.as_lowercase_str() == b.as_lowercase_str());
        let (repository, state) = (&self.merge.repository, &self.merge.state);
        let mut checked = Vec::with_capacity(mounts.len());
        let mut chosen_branch = None;
        for mount in mounts {
            let nodes = staged_nodes_at(repository, state, &mount).await?;
            let holds_other = nodes.iter().any(|(_, node)| node.is_staged_merge_theirs());
            let replaced = if holds_other == (merge_parent != MergeParent::Mine) {
                None
            } else {
                let sides = self.sides().await?;
                let (held, chosen) = if holds_other {
                    (&sides.other.state, &sides.mine.state)
                } else {
                    (&sides.mine.state, &sides.other.state)
                };
                if holds_other && self.merged_replaced_link(&nodes).await? {
                    return Err(self.merged_mount_conflict(&mount).into());
                }
                if chosen_branch.is_none() {
                    chosen_branch = Some(
                        link::parent_branch(chosen, repository)
                            .await
                            .forward::<StageError>("Failed to read the resolved side's branch")?,
                    );
                }
                let held = revision::mount_node_change_state(repository, held, &mount).await;
                Box::pin(refuse_losing_mount_work(
                    operation, repository, &nodes, &held, &mount,
                ))
                .await?;
                Some((
                    held,
                    revision::mount_node_change_state(repository, chosen, &mount).await,
                ))
            };
            checked.push(MountSide { mount, replaced });
        }
        Ok(MountResolve {
            merge_parent,
            chosen_branch: chosen_branch.unwrap_or_default(),
            mounts: checked,
        })
    }

    /// Whether the merge merged the repository of a link that `nodes`, the staged nodes at a
    /// mount, hold replaced, which taking this branch's side again does not restore.
    async fn merged_replaced_link(&self, nodes: &[(NodeID, Node)]) -> Result<bool, StageError> {
        let state = &self.merge.state;
        if state.link_merge_hash().is_zero() {
            return Ok(false);
        }
        let entries = state
            .deserialize_link_merge_state(self.merge.repository.clone())
            .await
            .forward::<StageError>("Failed to read the link merge state")?;
        Ok(nodes.iter().any(|(node_id, node)| {
            node.is_link()
                && node.is_staged_delete()
                && entries.iter().any(|entry| entry.local_node == *node_id)
        }))
    }

    /// The refusal for `node` at `path` inside a linked repository, which carries an unresolved
    /// conflict and is not a file.
    fn at_node(&self, node: &Node, path: &RelativePath) -> Option<NotSupported> {
        match self.scope {
            ResolveScope::Parent => None,
            ResolveScope::InsideLink if node.is_link() => Some(self.nested_mount_conflict(path)),
            ResolveScope::InsideLink => Some(self.inside_link_conflict(path)),
        }
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
            .then(|| self.inside_mount_conflict(mount)))
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
        if let Some(mount) = restored.mounts.iter().find(|mount| {
            paths
                .iter()
                .any(|path| mount.mount_path.covers_ignore_case(path))
        }) {
            return Ok(Some(self.inside_mount_conflict(&mount.mount_path)));
        }
        let mount = restored
            .mounts
            .iter()
            .find(|mount| {
                paths
                    .iter()
                    .any(|path| path.covers_ignore_case(&mount.mount_path))
            })
            .or_else(|| {
                replaced.mounts.iter().find(|mount| {
                    paths.iter().any(|path| {
                        path.covers_ignore_case(&mount.mount_path)
                            && path.as_lowercase_str() != mount.mount_path.as_lowercase_str()
                    })
                })
            });
        Ok(mount.map(|mount| self.mount_over_conflict(&mount.mount_path)))
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

    fn inside_mount_conflict(&self, mount: &RelativePath) -> NotSupported {
        NotSupported {
            operation: format!(
                "resolving a path inside the conflict at the link mount {mount} - resolve {mount} \
                 itself, or abort with `{}`",
                self.merge.abort
            ),
        }
    }

    fn mount_over_conflict(&self, mount: &RelativePath) -> NotSupported {
        NotSupported {
            operation: format!(
                "resolving a conflict that removes or adds the link mounted at {mount} - abort \
                 with `{}`",
                self.merge.abort
            ),
        }
    }

    fn merged_mount_conflict(&self, mount: &RelativePath) -> NotSupported {
        NotSupported {
            operation: format!(
                "resolving the conflict at the link mount {mount} back to this branch's link, \
                 whose repository the merge merged - abort with `{}` and merge again",
                self.merge.abort
            ),
        }
    }

    fn nested_mount_conflict(&self, mount: &RelativePath) -> NotSupported {
        NotSupported {
            operation: format!(
                "resolving the conflict at the link mount {mount} inside a linked repository - \
                 abort with `{}`",
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
    /// The conflicts at link mounts met, which a resolve settles whole.
    pub(crate) mounts: Vec<RelativePath>,
}

/// Which nodes [`collect_merge_paths`] collects.
#[derive(Clone, Copy)]
pub(crate) struct MergeWalk {
    /// Which merged nodes to collect.
    pub(crate) nodes: MergeNodes,
    /// Collect a directory carrying a conflict no node below it carries: the conflict is with a
    /// file the other side put in its place.
    pub(crate) whole_directories: bool,
}

/// The merged nodes a [`MergeWalk`] collects.
#[derive(Clone, Copy)]
pub(crate) enum MergeNodes {
    /// Every node the merge staged.
    Merged,
    /// Conflicts, resolved or not.
    Conflicts,
    /// Conflicts not yet resolved.
    Unresolved,
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

        let collects = |node: &Node| match walk.nodes {
            MergeNodes::Merged => node.is_staged_merge(),
            MergeNodes::Conflicts => node.is_staged_merge_conflict(),
            MergeNodes::Unresolved => node.is_staged_merge_unresolved(),
        };

        if let Some(refusal) = &refusal
            && refusal
                .is_mount_conflict(
                    &node,
                    &base_path,
                    matches!(walk.nodes, MergeNodes::Unresolved),
                )
                .await?
        {
            return Ok(StagedMergePaths {
                holds_conflict: true,
                mounts: vec![base_path],
                ..StagedMergePaths::default()
            });
        }
        if node.is_file() {
            if !collects(&node) {
                return Ok(StagedMergePaths::default());
            }
            return Ok(StagedMergePaths {
                files: vec![base_path],
                holds_conflict: node.is_staged_merge_conflict(),
                ..StagedMergePaths::default()
            });
        }
        if let Some(refusal) = &refusal
            && node.is_staged_merge_unresolved()
            && let Some(refused) = refusal.at_node(&node, &base_path)
        {
            return Err(refused.into());
        }
        if node.is_link() {
            let mut collected = StagedMergePaths::default();
            collected.links.push((base_path, node.linked_node()));
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
            collected.mounts.append(&mut child.mounts);
        }

        if walk.whole_directories
            && node.is_staged_merge_conflict()
            && collects(&node)
            && !collected.holds_conflict
        {
            return Ok(StagedMergePaths {
                files: vec![base_path],
                holds_conflict: true,
                ..collected
            });
        }
        Ok(collected)
    })
}

/// Conflicts at link mounts of the parent repository that a resolve settles whole, each checked
/// against the working tree, to apply once every other path the resolve names passed its
/// refusals.
pub(crate) struct MountResolve {
    merge_parent: MergeParent,
    /// The branch the side the resolve takes was committed on, where it replaces any mount.
    chosen_branch: BranchId,
    mounts: Vec<MountSide>,
}

/// A conflict at `mount`, with the side the working tree holds there and the side replacing it,
/// where the resolve takes the other.
struct MountSide {
    mount: RelativePath,
    replaced: Option<(change::NodeChangeState, change::NodeChangeState)>,
}

/// The branch the side a resolve takes was committed on, and the branch the merge lands on.
#[derive(Clone, Copy)]
struct ResolveBranches {
    chosen: BranchId,
    current: BranchId,
}

/// The branches the links a resolve took need to land on, to create once the staged state is
/// stored.
pub(crate) struct MissingLinkBranches {
    branches: Vec<branch::merge::MissingBranch>,
    resolve: ResolveBranches,
}

impl MissingLinkBranches {
    pub(crate) async fn create(
        self,
        repository: &Arc<RepositoryContext>,
    ) -> Result<(), StageError> {
        if self.branches.is_empty() || execution_context().globals().dry_run() {
            return Ok(());
        }
        link::create_branches_for_merged_links(
            &[link::MergedLinks {
                holder: repository,
                incoming: &self.branches,
                branch: self.resolve.current,
            }],
            self.resolve.chosen,
        )
        .await
        .forward::<StageError>("Failed to create the branches the resolved links follow")
    }
}

impl MountResolve {
    pub(crate) fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// Settles each conflict to the side the resolve takes. Mine keeps what the merge staged. A
    /// side the working tree does not hold replaces it there and in the staged state `state`, as a
    /// merge applying that side's change would: a link it holds is mounted with its row, as the
    /// parent's branch, `branch`, stores it.
    pub(crate) async fn apply(
        self,
        operation: &Arc<InstanceOperationImpl>,
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        token: &RepositoryWriteToken,
        branch: BranchId,
    ) -> Result<MissingLinkBranches, StageError> {
        let side = match self.merge_parent {
            MergeParent::Mine => NodeFlags::StagedMergeMine,
            MergeParent::CherryPick | MergeParent::Revert | MergeParent::Theirs => {
                NodeFlags::StagedMergeTheirs
            }
        };
        let mut missing = MissingLinkBranches {
            branches: Vec::new(),
            resolve: ResolveBranches {
                chosen: self.chosen_branch,
                current: branch,
            },
        };
        // Each repository the links taken follow is merged once for all its mounts, as a merge
        // bringing them in merges it.
        let mut landing: Vec<(RepositoryId, Vec<link::IncomingLink>)> = Vec::new();
        for MountSide { mount, replaced } in &self.mounts {
            if let Some(replaced) = replaced
                && let Some(link) = Box::pin(replace_mount(
                    operation,
                    repository,
                    state,
                    mount,
                    replaced,
                    missing.resolve,
                ))
                .await?
            {
                let link_repository = link.link_reference.repository;
                match landing.iter_mut().find(|(id, _)| *id == link_repository) {
                    Some((_, mounts)) => mounts.push(link),
                    None => landing.push((link_repository, vec![link])),
                }
            }
        }
        for (_, mounts) in landing {
            if let Some(branch) = Box::pin(branch::merge::land_resolved_links(
                repository,
                token,
                state,
                mounts,
                missing.resolve.chosen,
                missing.resolve.current,
            ))
            .await
            .forward::<StageError>("Failed to merge the resolved links' repository")?
            {
                missing.branches.push(branch);
            }
        }
        for MountSide { mount, .. } in &self.mounts {
            for (node_id, node) in staged_nodes_at(repository, state, mount).await? {
                let action = NodeFlags::from_bits_retain(node.flags) & NodeFlags::ActionBits;
                state
                    .node_mark(
                        repository,
                        node_id,
                        Mark::staged(
                            action
                                | side
                                | NodeFlags::StagedMerge
                                | NodeFlags::StagedMergeConflict
                                | NodeFlags::StagedMergeResolved,
                            true,
                        ),
                    )
                    .await
                    .forward::<StageError>("Failed to mark the link mount conflict resolved")?;
            }
        }
        Ok(missing)
    }
}

/// Replaces `held`, what the working tree and the staged state `state` hold at `mount`, with
/// `chosen`, the side a resolve takes there. Answers the following link `chosen` holds, which is
/// left to land. A dry run, which checked the resolve already, writes nothing.
async fn replace_mount(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    mount: &RelativePath,
    (held, chosen): &(change::NodeChangeState, change::NodeChangeState),
    branches: ResolveBranches,
) -> Result<Option<link::IncomingLink>, StageError> {
    if execution_context().globals().dry_run() {
        return Ok(None);
    }
    // Realizing a change at a mount resolves the path inside the link, so the link staged there is
    // taken out of the staged state here.
    for (node_id, node) in staged_nodes_at(repository, state, mount).await? {
        if node.is_link()
            && !node.is_staged_delete()
            && !(chosen.is_link() && chosen.address.context == node.address.context)
        {
            stage_delete_replaced(
                repository.clone(),
                state.clone(),
                mount.clone(),
                node_id,
                Some(node.address.context.into()),
                NodeFlags::StagedMerge,
                Arc::default(),
                None,
            )
            .await?;
        }
    }
    let changes = mount_changes(
        repository,
        &held.mapping.state,
        &chosen.mapping.state,
        mount,
    )
    .await?;
    crate::fs::realize::realize_changes(
        repository.clone(),
        operation.clone(),
        Arc::new(changes),
        &[],
        None,
        Some(state.clone()),
        false,
        Some(crate::fs::realize::MergeCurrent {
            state: held.mapping.state.clone(),
            staged: Some(state.clone()),
        }),
        None,
        Arc::new(SyncRealizeStats::default()),
    )
    .await
    .forward::<StageError>("Failed to realize the side a link mount conflict resolves to")?;
    if !chosen.is_link() {
        return Ok(None);
    }
    store_chosen_link_row(repository, state, &chosen.mapping.state, mount, branches).await
}

/// The nodes `state` holds at `mount`: the node there, the link node itself at a mount, and the
/// node of another kind a resolve replaced it with beside it, which keeps its name.
async fn staged_nodes_at(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    mount: &RelativePath,
) -> Result<SmallVec<[(NodeID, Node); 2]>, StageError> {
    let mut nodes = SmallVec::new();
    let Some(found) = NodeMapping::root(repository.clone(), state.clone())
        .node_at(mount)
        .await
        .own_node()
        .filter(NodeIDExt::is_valid_node_id)
    else {
        return Ok(nodes);
    };
    let read = async |node_id| {
        state
            .node(repository.clone(), node_id)
            .await
            .forward::<StageError>("Failed to read the node at a link mount")
    };
    nodes.push((found, read(found).await?));
    let parent_path = mount
        .as_str()
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent);
    let parent = state
        .locate_node(repository.clone(), parent_path)
        .await
        .and_then(FoundNode::into_own_node)
        .forward::<StageError>("Failed to find the directory holding a link mount")?;
    if let Some((beside, node)) = state
        .find_subnode_beside(
            repository.clone(),
            parent,
            (found, hash::hash_string(mount.name())),
            |_| true,
        )
        .await
        .forward::<StageError>("Failed to find the node a resolve replaced at a link mount")?
    {
        nodes.push((beside, node));
    }
    Ok(nodes)
}

/// The changes from `from` to `to` at and below `mount`, sorted by path, a link node among them
/// rather than the files of the repository it links.
async fn mount_changes(
    repository: &Arc<RepositoryContext>,
    from: &Arc<State>,
    to: &Arc<State>,
    mount: &RelativePath,
) -> Result<Vec<change::NodeChange>, StageError> {
    let mut filter = Filter::excluding_all_from_view()
        .forward::<StageError>("Failed to narrow the diff to the link mount")?;
    filter.view.add_path_inclusion(mount);
    let narrowed = Arc::new(repository.to_filter_context(Arc::new(filter)));
    let (from, to) = (from.clone(), to.clone());
    let mut changes: Vec<change::NodeChange> = state::ChangeStream::spawn(async move |changes| {
        state::diff(
            narrowed.clone(),
            from,
            narrowed,
            to,
            None,
            None,
            &changes,
            FilterMode::View,
        )
        .await
    })
    .collect()
    .await
    .forward::<StageError>("Failed to compare the sides of a link mount conflict")?;
    changes.retain(|change| {
        change.to.mapping.repository.id == repository.id && mount.covers(change.path())
    });
    change::sort_by_path(&mut changes);
    Ok(changes)
}

/// Refuses to replace what the working tree holds at `mount`, the side `held` of a conflict there,
/// where that loses work not committed: an edit in the mount of the link among `staged`, the
/// staged nodes there, its pin naming what its merge left in the mount, or what
/// [`refuse_losing_local_work`] refuses for a file or a folder.
async fn refuse_losing_mount_work(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    staged: &[(NodeID, Node)],
    held: &change::NodeChangeState,
    mount: &RelativePath,
) -> Result<(), StageError> {
    if held.is_link() {
        let Some((_, node)) = staged
            .iter()
            .find(|(_, node)| node.is_link() && !node.is_staged_delete())
        else {
            return Ok(());
        };
        let shown = crate::fs::realize::open_mounted_tree(repository, node).await;
        return link::remove::verify_no_local_changes_under_link(operation, shown, mount)
            .await
            .forward::<StageError>("Failed to check the link mount for local changes");
    }
    if held.mapping.node.is_valid_node_id() {
        return refuse_losing_local_work(operation, repository, &held.mapping.state, mount).await;
    }
    if operation
        .file_info(mount)
        .await
        .forward_with::<StageError, _>(|| format!("Failed to describe {mount}"))?
        .exists()
    {
        return Err(LocalModifications.into());
    }
    Ok(())
}

/// Stores the row of the link `chosen` holds at `mount`, which a resolve just staged there, as the
/// parent's branch stores it. Answers the link where it follows that branch, to land as a merge
/// bringing it in does.
async fn store_chosen_link_row(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    chosen: &Arc<State>,
    mount: &RelativePath,
    branches: ResolveBranches,
) -> Result<Option<link::IncomingLink>, StageError> {
    let rows = async |state: &Arc<State>| {
        let rows = link::LinkRows::read(state, repository.clone())
            .await
            .forward::<StageError>("Failed to read the link rows")?;
        link::row_at(repository, &rows, mount.as_str(), None)
            .await
            .forward::<StageError>("Failed to read the link row at a mount")
    };
    let (Some((chosen_row, _)), Some((staged_row, _))) =
        tokio::try_join!(rows(chosen), rows(state))?
    else {
        return Ok(None);
    };
    let carried = state::LinkReference {
        local_node: staged_row.local_node,
        ..chosen_row.carried(branches.chosen, branches.current)
    };
    if !staged_row.stores_same(&carried) {
        state
            .link_replace(repository.clone(), carried)
            .await
            .forward::<StageError>("Failed to store the resolved link row")?;
    }
    Ok(carried.is_tracking().then(|| link::IncomingLink {
        link_path: mount.to_string(),
        link_reference: state::LinkReference {
            local_node: staged_row.local_node,
            ..chosen_row
        },
    }))
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
    let Some(node_id) = replaced
        .locate_node(repository.clone(), path.as_str())
        .await
        .unwrap_or_default()
        .own_node()
        .filter(NodeIDExt::is_valid_node_id)
    else {
        return Err(LocalModifications.into());
    };
    if !state::collect_link_mounts_below(replaced, repository, node_id, path)
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
        node: node_id,
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

/// Removes `relative_path` from the working tree with what it holds, stages it from there with
/// `options`, a deletion where nothing is left, and removes the copies a merge wrote beside it
/// unless `preserve_conflict_files` keeps them for an unresolve.
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
        KnownChild::Unresolved,
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

/// Stages the deletion of each directory above `relative_path`, a file whose deletion `state`
/// stages and the working tree lacks, nearest first, that `side` lacks and that holds no child in
/// `state` but nodes staged for deletion: a merge resolution keeping the deletion leaves no
/// directory only the merge's change of the file kept. Each is removed from the working tree as an
/// empty directory before its deletion is staged, the copies a merge wrote beside the file removed
/// before the first; the walk stops at one that still holds entries, such as an untracked or
/// ignored file, which stays with its node, as it does at one `side` holds or a live child keeps.
/// Where the walk removes nothing, the copies stay until the commit.
pub(crate) async fn stage_emptied_ancestors(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    side: &State,
    relative_path: &RelativePath,
    stats: &Arc<StageStats>,
    options: StageOptions,
) -> Result<(), StageError> {
    let mut search_path = relative_path.clone();
    search_path.pop();
    let mut unlinked = false;
    while !search_path.is_empty() {
        match side
            .locate_node(repository.clone(), search_path.as_str())
            .await
        {
            Ok(in_side) if in_side.is_valid() => break,
            Ok(_) => {}
            Err(err) if err.is_node_not_found() => {}
            Err(err) => {
                return Err(err).forward::<StageError>("Failed to find the directory in the side");
            }
        }
        let staged = state
            .locate_node(repository.clone(), search_path.as_str())
            .await
            .and_then(FoundNode::into_own_node)
            .forward::<StageError>("Failed to find subnode")?;
        let children = state
            .node_children(repository.clone(), staged)
            .await
            .forward::<StageError>("Failed to find subnode")?;
        let mut live = false;
        for child in children.iter() {
            if !state
                .node(repository.clone(), *child)
                .await
                .forward::<StageError>("Failed to read a child of a directory left by a merge")?
                .is_staged_delete()
            {
                live = true;
                break;
            }
        }
        if live {
            break;
        }
        if !unlinked {
            sync::unlink_merge_artifacts(operation, relative_path).await;
            unlinked = true;
        }
        if !removed_empty_directory(operation, &search_path).await? {
            break;
        }
        lore_debug!("Stage {search_path} deleted, as nothing in it stays");
        Box::pin(stage_filesystem_path(
            operation.clone(),
            NodeMapping::root(repository.clone(), state.clone()),
            search_path.clone(),
            KnownChild::Unresolved,
            stats.clone(),
            options,
            None,
            None,
            None,
            None,
        ))
        .await?;
        search_path.pop();
    }
    Ok(())
}

/// Removes the directory at `path` where it holds no entries, and answers whether the working tree
/// then lacks it, as it lacks one it never held. A dry run removes nothing, and answers whether
/// the working tree lacks it already.
async fn removed_empty_directory(
    operation: &Arc<InstanceOperationImpl>,
    path: &RelativePath,
) -> Result<bool, StageError> {
    let removal = if execution_context().globals().dry_run() {
        None
    } else {
        match operation.remove_empty_directory(path).await {
            Ok(()) => return Ok(true),
            Err(err) => Some(err),
        }
    };
    let held = operation
        .file_info(path)
        .await
        .forward_any::<StageError>("Failed to describe a directory left by a merge")?
        .exists();
    if held && let Some(err) = removal {
        lore_debug!("Directory {path} left by a merge stays: {err}");
    }
    Ok(!held)
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

    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<StageError>("Failed to deserialize current revision anchor")?;
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = State::deserialize_staged(repository.clone(), staged_revision, current_revision)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    if !state.is_merge_or_cherry_pick_or_revert() {
        return Err(StageError::internal("No merge in progress"));
    }

    // Every path passes the refusals before any is staged, so a refused resolve stages nothing.
    let refusal = ResolveRefusal::new(&repository, &state);

    // Link-paths need a separate helper that operates against each link's
    // own state (and updates the parent's pin afterwards).
    let mut parent_paths: Vec<LoreString> = Vec::new();
    let mut link_paths: Vec<LinkPath> = Vec::new();
    let mut mounts: Vec<RelativePath> = Vec::new();
    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            // Invalid path — leave to the existing branch to emit the ignore
            // event and warn.
            parent_paths.push(path.clone());
            continue;
        };
        if refusal.is_mount_conflict_at(&relative_path, false).await? {
            mounts.push(relative_path);
            continue;
        }
        let found = state
            .locate_node(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if found.is_valid_or_root() && found.crossed_link() {
            link_paths.push(LinkPath {
                path: relative_path,
                named: Some(path.clone()),
            });
        } else {
            parent_paths.push(path.clone());
        }
    }

    if parent_paths.is_empty() {
        let link_groups =
            collect_link_groups(&repository, &state, link_paths, merge_parent, &refusal).await?;
        let mounts = Box::pin(refusal.check_mounts(&operation, mounts, merge_parent)).await?;
        let resolves_mounts = !mounts.is_empty();
        let staged_links = Box::pin(stage_link_groups(
            operation.clone(),
            repository.clone(),
            token,
            state.clone(),
            link_groups,
            merge_parent,
        ))
        .await?;
        let missing =
            Box::pin(mounts.apply(&operation, &repository, &state, token, current_branch)).await?;
        if staged_links || resolves_mounts {
            store_resolved_state(&repository, token, &state, missing).await?;
        }
        return Ok(());
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
        let found = state
            .locate_node(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if !found.is_valid_or_root() {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path, does not exist in staged state: {path}");
            continue;
        }

        let Some(node_id) = found.own_node() else {
            return Err(StageError::internal(
                "A path left to the parent repository resolves into a linked repository",
            ));
        };

        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
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
                node_id,
                relative_path,
                MergeWalk {
                    nodes: MergeNodes::Merged,
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
            mounts.extend(expanded.mounts);
        }
    }

    let link_groups =
        collect_link_groups(&repository, &state, link_paths, merge_parent, &refusal).await?;
    if let Some(refused) = refusal.restoring(merge_parent, &resolved_paths).await? {
        return Err(refused.into());
    }
    let mounts = Box::pin(refusal.check_mounts(&operation, mounts, merge_parent)).await?;

    let staged_links = Box::pin(stage_link_groups(
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
            let Some(node_id) = state
                .locate_node(repository.clone(), relative_path.as_str())
                .await
                .unwrap_or_default()
                .own_node()
                .filter(NodeIDExt::is_valid_node_id)
            else {
                continue;
            };

            let block_index = NodeBlock::index(node_id);
            let node_index = Node::index(node_id);
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
            let target_found = state_target
                .locate_node(repository.clone(), relative_path.as_str())
                .await
                .unwrap_or_default();
            let current_found = state_current
                .locate_node(repository.clone(), relative_path.as_str())
                .await
                .unwrap_or_default();

            let target_node = state_target
                .found_node(repository.clone(), &target_found)
                .await
                .unwrap_or_default();
            let current_node = state_current
                .found_node(repository.clone(), &current_found)
                .await
                .unwrap_or_default();

            // Determine merge action based on target vs current state
            let merge_action_flags = determine_merge_action(
                target_found.is_valid(),
                current_found.is_valid(),
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

            if !target_found.is_valid() {
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
                stage_emptied_ancestors(
                    &operation,
                    &repository,
                    &state,
                    &state_target,
                    &relative_path,
                    &stats,
                    options,
                )
                .await?;
                continue;
            }

            let Some(target_id) = target_found.own_node() else {
                return Err(StageError::internal(
                    "A path resolved in the parent repository is restored from a linked repository",
                ));
            };

            let block_index = NodeBlock::index(target_id);
            let node_index = Node::index(target_id);
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
                    KnownChild::Unresolved,
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
                            target_id,
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
    let drained = if state.holds_promoted_moves() {
        Box::pin(crate::file::stage::stage_promoted_moves_of_call(
            &repository,
            &state,
            None,
        ))
        .await
        .map(drop)
    } else {
        Ok(())
    };
    if let Some(err) = failure {
        return Err(err);
    }
    drained?;
    let resolves_mounts = !mounts.is_empty();
    let missing =
        Box::pin(mounts.apply(&operation, &repository, &state, token, current_branch)).await?;

    // TODO(vri): UCS-17955 - Merging and conflict resolution for links
    // Serialize all staged links states recursively

    let count = LoreFileStageCountData::new(stats.clone());
    let total_count = count.total_count;
    event::LoreEvent::FileStageEnd(LoreFileStageEndEventData { count }).send();

    if total_count == 0 && !staged_links && !resolves_mounts {
        return Ok(());
    }

    let signature = store_resolved_state(&repository, token, &state, missing).await?;

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
    target: NodeID,
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

/// Whether the link mounted at `mount` inside `group`'s link, mounting `node_link`, was merged with
/// it and holds something its merge left to resolve.
async fn nested_link_holds_a_merge(
    group: &LinkGroup,
    mount: &RelativePath,
    node_link: NodeLink,
) -> Result<bool, StageError> {
    let Some(below) = mount.below(&group.link_path_rel) else {
        return Ok(false);
    };
    let in_link = group.source_path.join(below);
    let Some((local_node, link_node)) = crate::link::link_node_to(
        &group.link_state_staged,
        &group.link_context,
        in_link.as_str(),
        node_link.repository,
    )
    .await
    .forward::<StageError>("Failed to read a nested link")?
    else {
        return Ok(false);
    };
    let merged = group
        .link_state_staged
        .deserialize_link_merge_state(group.link_context.clone())
        .await
        .forward::<StageError>("Failed to read the link's link merge state")?;
    if !merged
        .iter()
        .any(|entry| entry.local_node == local_node && !entry.is_conflict())
    {
        return Ok(false);
    }
    let nested_context = group
        .link_context
        .to_link_context(node_link.repository)
        .await;
    let nested_state = state::State::deserialize(nested_context.clone(), link_node.address.hash)
        .await
        .forward::<StageError>("Failed to deserialize the nested link's state")?;
    if !nested_state.link_merge_hash().is_zero() {
        return Ok(true);
    }
    let unresolved = collect_merge_paths(
        nested_context,
        nested_state,
        link_node.child,
        mount.clone(),
        MergeWalk {
            nodes: MergeNodes::Unresolved,
            whole_directories: true,
        },
        None,
    )
    .await?;
    Ok(!unresolved.files.is_empty())
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
        let found = group
            .link_state_staged
            .locate_node(group.link_context.clone(), in_link.as_str())
            .await
            .unwrap_or_default();
        if !found.is_valid_or_root() {
            emit_named_path_ignore(named.as_ref()).await;
            lore_debug!("Ignoring invalid link path, does not exist in link staged state: {user}");
            continue;
        }
        let Some(node_id) = found.own_node() else {
            return Err(resolve_inside_nested_link(&user));
        };
        let block = group
            .link_state_staged
            .block(group.link_context.clone(), NodeBlock::index(node_id))
            .await
            .forward::<StageError>("Failed deserializing link state node block")?;
        let node = block.node(Node::index(node_id));
        let files = if node.is_file() {
            if !node.is_staged_merge() {
                emit_named_path_ignore(named.as_ref()).await;
                lore_debug!("Ignoring invalid path, link node is not a staged merge: {user}");
                continue;
            }
            vec![(user, below_mount, in_link)]
        } else {
            let collected = collect_merge_paths(
                group.link_context.clone(),
                group.link_state_staged.clone(),
                node_id,
                user,
                MergeWalk {
                    nodes: MergeNodes::Merged,
                    whole_directories: false,
                },
                Some(refusal.clone()),
            )
            .await?;
            for (nested, node_link) in collected.links {
                if nested_link_holds_a_merge(group, &nested, node_link).await? {
                    return Err(resolve_inside_nested_link(&nested));
                }
            }
            collected.files.into_iter().filter_map(in_group).collect()
        };
        for (user, below_mount, in_link) in files {
            let target = match group
                .link_state_target
                .locate_node(group.link_context.clone(), in_link.as_str())
                .await
            {
                Ok(found) => match found.own_node() {
                    Some(target) => target,
                    None => return Err(resolve_inside_nested_link(&user)),
                },
                Err(err) if err.is_node_not_found() => INVALID_NODE,
                Err(err) => {
                    Err(err).forward::<StageError>("Failed to find the link target node")?
                }
            };
            if target.is_valid_node_id() {
                let target_node = group
                    .link_state_target
                    .node(group.link_context.clone(), target)
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

/// The refusal of a resolve of `path` to one side, which a link nested in the resolved link holds.
fn resolve_inside_nested_link(path: &RelativePath) -> StageError {
    NotSupported {
        operation: format!("resolving {path} to one side inside a nested link"),
    }
    .into()
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
) -> Result<bool, StageError> {
    if groups.is_empty() {
        return Ok(false);
    }

    let stats = Arc::new(StageStats::default());

    for group in groups.iter().filter(|group| !group.files.is_empty()) {
        for LinkFile {
            user: mount_path,
            below_mount,
            in_link: link_relative,
            target,
        } in &group.files
        {
            let Some(staged_id) = group
                .link_state_staged
                .locate_node(group.link_context.clone(), link_relative.as_str())
                .await
                .unwrap_or_default()
                .own_node()
            else {
                continue;
            };
            let staged_block = group
                .link_state_staged
                .block(group.link_context.clone(), NodeBlock::index(staged_id))
                .await
                .forward::<StageError>("Failed deserializing link state node block")?;
            let node_staged = staged_block.node(Node::index(staged_id));

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
            let current_found = group
                .link_state_current
                .locate_node(group.link_context.clone(), link_relative.as_str())
                .await
                .unwrap_or_default();
            let target_node = group
                .link_state_target
                .node(group.link_context.clone(), *target)
                .await
                .unwrap_or_default();
            let current_node = group
                .link_state_current
                .found_node(group.link_context.clone(), &current_found)
                .await
                .unwrap_or_default();
            let merge_action_flags = determine_merge_action(
                target.is_valid_node_id(),
                current_found.is_valid(),
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
            if !target.is_valid_node_id() {
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
                    KnownChild::Unresolved,
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
                .block(group.link_context.clone(), NodeBlock::index(*target))
                .await
                .forward::<StageError>("Failed deserializing link target state node block")?;
            let node_t = target_block.node(Node::index(*target));

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
                    KnownChild::Unresolved,
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
                    staged_id,
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

    Ok(true)
}

/// Stores `state` as [`store_staged_state`] does, and then creates the branches `missing`, which
/// links the resolve took need, so a resolve refused before leaves no branch behind.
async fn store_resolved_state(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: &Arc<State>,
    missing: MissingLinkBranches,
) -> Result<Hash, StageError> {
    let signature = store_staged_state(repository, token, state).await?;
    missing.create(repository).await?;
    Ok(signature)
}

/// Serializes `state` and stores it as the staged revision, which a dry run leaves as it was.
async fn store_staged_state(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: &Arc<State>,
) -> Result<Hash, StageError> {
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize staged revision state")?;
    if !execution_context().globals().dry_run() {
        crate::instance::store_staged_anchor(repository, signature)
            .await
            .forward::<StageError>("Failed to store staged anchor")?;
    }
    Ok(signature)
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
    let Ok(found) = state.locate_node(repository.clone(), path.as_str()).await else {
        return Ok(());
    };
    let (node_id, (repository, state)) = found.held((repository, state)).into_parts();
    stage_delete(
        repository,
        state,
        path.clone(),
        node_id,
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
    let (node_current, (repository_current, state_current)) = state_current
        .locate_node(repository_current.clone(), relative_path.as_str())
        .await
        .unwrap_or_default()
        .held((repository_current, state_current))
        .into_parts();

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
            node: node_current,
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
        let Some(staged) = state_current
            .locate_node(repository_current.clone(), change.path().as_str())
            .await
            .unwrap_or_default()
            .own_node()
            .filter(NodeIDExt::is_valid_node_id)
        else {
            continue;
        };

        let block = state_current
            .block(repository_current.clone(), NodeBlock::index(staged))
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        state_current.node_clear_own(&block, staged, OwnFlags::Staged);
    }

    // Perform the changes
    crate::fs::realize::realize_changes(
        repository_current.clone(),
        operation.clone(),
        changes.clone(),
        &[],
        None, /* No subtrees */
        None,
        false, /* No dry run */
        None,  /* Not a merge */
        Some(&crate::fs::realize::EmptiedDirectories::Excluded),
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
            let (parent_node, (repository, state)) = state_current
                .locate_node(repository_current.clone(), parent_path.as_str())
                .await
                .forward::<StageError>("Failed to find subnode")?
                .held((repository_current.clone(), state_current.clone()))
                .into_parts();
            let file_name = if parent_path.is_empty() {
                change.path().to_string()
            } else {
                change.path().as_str()[(parent_path.len() + 1)..].to_string()
            };

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
                        node: parent_node,
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
    if node_current.is_valid_node_id()
        && !changes.iter().any(|change| change.path() == &relative_path)
        && state_current
            .node(repository_current.clone(), node_current)
            .await
            .is_ok_and(|node| node.is_directory())
    {
        state_current
            .node_mark(
                &repository_current,
                node_current,
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
