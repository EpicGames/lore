// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(target_family = "unix")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_family = "windows")]
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crossbeam::queue::SegQueue;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinSet;

use super::RepositoryContext;
use crate::branch;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::find;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreNodeType;
use crate::interface::LoreString;
use crate::layer;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_drain_tasks;
use crate::lore_trace;
use crate::metadata::Metadata;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::path::emit_path_ignore;
use crate::state;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

/// Revision status of a repository, describing the current, local, and remote
/// positions of the active branch.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStatusRevisionEventData {
    /// Repository identifier
    pub repository: RepositoryId,
    /// Current branch identifier
    pub branch: BranchId,
    /// Current branch name
    pub branch_name: LoreString,
    /// Current revision identifier
    pub revision: Hash,
    /// Current revision number
    pub revision_number: u64,
    /// Staged revision identifier (zero when nothing is staged)
    pub revision_staged: Hash,
    /// Incoming revision identifier of a pending merge (zero when none)
    pub revision_merged: Hash,
    /// Last revision merged in from the parent branch (calculated and reported if sync point option is set).
    pub revision_merged_parent_branch: Hash,
    /// Local branch latest revision identifier
    pub revision_local: Hash,
    /// Local branch latest revision number
    pub revision_local_number: u64,
    /// Remote branch latest revision identifier (zero if unknown, branch not existing on remote or remote not available)
    pub revision_remote: Hash,
    /// Remote branch latest revision number (zero if corresponding identifier is zero)
    pub revision_remote_number: u64,
    /// Local holds revisions not on the remote history line
    pub is_local_ahead: u8,
    /// Remote holds revisions not present locally
    pub is_remote_ahead: u8,
    /// Remote configured and reachable with a local identity; connectivity only, not authorization
    pub remote_available: u8,
    /// Remote revision query returned an authoritative answer, identity is authorized to access the repository
    pub remote_authorized: u8,
    /// Branch exists on the remote and the query returned a latest revisoin (possibly zero if branch does not exist on remote)
    pub remote_branch_exist: u8,
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::fn_params_excessive_bools)]
impl LoreRepositoryStatusRevisionEventData {
    pub fn new(
        repository: RepositoryId,
        branch: BranchId,
        branch_name: &str,
        revision: Hash,
        revision_number: u64,
        revision_staged: Hash,
        revision_merged: Hash,
        revision_merged_parent_branch: Hash,
        revision_local: Hash,
        revision_local_number: u64,
        revision_remote: Hash,
        revision_remote_number: u64,
        is_local_ahead: bool,
        is_remote_ahead: bool,
        remote_available: bool,
        remote_authorized: bool,
        remote_branch_exist: bool,
    ) -> Self {
        LoreRepositoryStatusRevisionEventData {
            repository,
            branch,
            branch_name: branch_name.into(),
            revision,
            revision_number,
            revision_staged,
            revision_merged,
            revision_merged_parent_branch,
            revision_local,
            revision_local_number,
            revision_remote,
            revision_remote_number,
            is_local_ahead: is_local_ahead.into(),
            is_remote_ahead: is_remote_ahead.into(),
            remote_available: remote_available.into(),
            remote_authorized: remote_authorized.into(),
            remote_branch_exist: remote_branch_exist.into(),
        }
    }
}

/// Status of a single file or node reported by a repository status operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStatusFileEventData {
    /// Path of the file relative to the repository root.
    pub path: LoreString,
    /// Size of the file in bytes.
    pub size: u64,
    /// Change applied to the file, such as add, modify, delete, or move.
    pub action: LoreFileAction,
    /// Kind of node: file, directory, or link.
    pub r#type: LoreNodeType,

    /// Non-zero when the change is staged.
    #[serde(with = "u8_as_bool")]
    pub flag_staged: u8,
    /// Non-zero when the change comes from a merge.
    #[serde(with = "u8_as_bool")]
    pub flag_merged: u8,
    /// Non-zero when the file is in conflict.
    #[serde(with = "u8_as_bool")]
    pub flag_conflict: u8,
    /// Non-zero when the conflict is not yet resolved.
    #[serde(with = "u8_as_bool")]
    pub flag_conflict_unresolved: u8,
    /// Non-zero when the conflict was resolved automatically.
    #[serde(with = "u8_as_bool")]
    pub flag_conflict_automerged: u8,
    /// Non-zero when the local side was chosen to resolve the conflict.
    #[serde(with = "u8_as_bool")]
    pub flag_conflict_mine: u8,
    /// Non-zero when the incoming side was chosen to resolve the conflict.
    #[serde(with = "u8_as_bool")]
    pub flag_conflict_theirs: u8,
    /// Non-zero when the file differs from the recorded state.
    #[serde(with = "u8_as_bool")]
    pub flag_dirty: u8,

    /// Previous path of the file when it was moved or copied. Empty otherwise.
    pub from_path: LoreString,
}

impl LoreRepositoryStatusFileEventData {
    pub fn from_node_change(change: &NodeChange, size: u64) -> Self {
        let node_type = if change.action == FileAction::Add
            || change.action == FileAction::Move
            || change.to.node.is_valid_node_id()
        {
            change.to.flags
        } else {
            change.from.flags
        };
        let node_type = if node_type.contains(NodeFlags::File) {
            LoreNodeType::File
        } else if node_type.contains(NodeFlags::Link) {
            LoreNodeType::Link
        } else {
            LoreNodeType::Directory
        };
        LoreRepositoryStatusFileEventData {
            path: LoreString::from(&change.path),
            size,
            action: LoreFileAction::from(change.action),
            r#type: node_type,
            flag_dirty: change.flags.is_dirty().into(),
            flag_staged: change.flags.is_stage().into(),
            flag_merged: change.flags.is_merge().into(),
            flag_conflict: change.flags.is_conflict().into(),
            flag_conflict_unresolved: change.flags.is_conflict_unresolved().into(),
            flag_conflict_automerged: change.flags.is_conflict_automerged().into(),
            flag_conflict_mine: change.flags.is_conflict_mine().into(),
            flag_conflict_theirs: change.flags.is_conflict_theirs().into(),
            from_path: change.from_path.as_ref().map(|path| path.as_str()).into(),
        }
    }

    pub fn action_as_string_short(&self) -> &'static str {
        self.action.as_string_short()
    }

    pub fn merged_as_string_short(&self) -> &'static str {
        if self.flag_merged != 0 {
            return "(M)";
        }
        ""
    }

    pub fn conflict_as_string_short(&self) -> &'static str {
        if self.flag_conflict != 0 && self.flag_conflict_unresolved != 0 {
            return "!";
        }
        ""
    }
}

/// Counts of directories and files in the repository tree.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStatusCountEventData {
    /// Number of directories in the tree, view-filtered (staged state if
    /// present, otherwise the current revision)
    pub directories: u64,
    /// Number of files in the tree, view-filtered (staged state if present,
    /// otherwise the current revision)
    pub files: u64,
}

/// Aggregate counts of dirty nodes by action type, emitted once at the end of
/// a reconciling status (`--scan` or `--check-dirty`). For `--scan` these are
/// the changes detected against the filesystem; for `--check-dirty` they are
/// the nodes that remained dirty after the filesystem verification.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStatusSummaryEventData {
    /// Number of files added.
    pub adds: u64,
    /// Number of files deleted.
    pub deletes: u64,
    /// Number of files modified.
    pub modifies: u64,
    /// Number of files moved.
    pub moves: u64,
    /// Number of files copied.
    pub copies: u64,
}

/// Thread-safe accumulator for dirty-node counts during the parallel status
/// scan/verify walk. Each spawned task increments the relevant counter via
/// [`StatusSummaryStats::classify`].
#[derive(Default)]
pub struct StatusSummaryStats {
    adds: AtomicU64,
    deletes: AtomicU64,
    modifies: AtomicU64,
    moves: AtomicU64,
    copies: AtomicU64,
}

impl StatusSummaryStats {
    /// Increment the counter matching a reported change's action. `Keep` is a
    /// content modification (a filesystem/state diff has no separate "modify"
    /// action — modified files surface as `Keep` with the modify flag set).
    fn classify(&self, change: &NodeChange) {
        let counter = match change.action {
            FileAction::Add => &self.adds,
            FileAction::Delete => &self.deletes,
            FileAction::Move => &self.moves,
            FileAction::Copy => &self.copies,
            FileAction::Keep => &self.modifies,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn event_data(&self) -> LoreRepositoryStatusSummaryEventData {
        LoreRepositoryStatusSummaryEventData {
            adds: self.adds.load(Ordering::Relaxed),
            deletes: self.deletes.load(Ordering::Relaxed),
            modifies: self.modifies.load(Ordering::Relaxed),
            moves: self.moves.load(Ordering::Relaxed),
            copies: self.copies.load(Ordering::Relaxed),
        }
    }
}

#[error_set]
pub enum StatusError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    WriteRequired,
    Oversized,
    InvalidArguments,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    AlreadyLinked,
    LayerNotFound,
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
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
    NotConnected,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for StatusError {
    fn translated(&self) -> LoreError {
        match self {
            StatusError::Disconnected(_) => LoreError::Connection,
            StatusError::SlowDown(_) => LoreError::SlowDown,
            StatusError::Oversized(_) => LoreError::Oversized,
            StatusError::FileNotFound(_) => LoreError::FileNotFound,
            StatusError::NotFound(_)
            | StatusError::LayerNotFound(_)
            | StatusError::RevisionNotFound(_) => LoreError::NotFound,
            StatusError::AddressNotFound(_) => LoreError::AddressNotFound,
            StatusError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            StatusError::InvalidArguments(_) | StatusError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct StatusOptions {
    // Include staged or not
    pub staged: bool,
    /// Reconcile against the filesystem and refresh dirty tracking.
    ///
    /// When `false` (default), status reports the currently tracked state:
    /// the staged revision (if any) plus all files and directories marked
    /// dirty in the repository. No filesystem reads are performed beyond the
    /// existing dirty flags.
    ///
    /// When `true`, the filesystem is walked under each requested path,
    /// every file is reconciled against the current revision, and dirty
    /// flags are set or cleared accordingly. The refreshed flags are
    /// persisted in the staged state so subsequent operations see an
    /// accurate picture without rescanning.
    pub scan: bool,
    /// Verify dirty flags against the filesystem while reporting tracked state.
    ///
    /// Unlike [`scan`](Self::scan), this performs no filesystem walk: it only
    /// re-examines files already marked dirty. For each dirty file the on-disk
    /// content is checked (a size difference is a modification; otherwise, when
    /// the recorded modification time differs, the content is rehashed and
    /// compared). A file that turns out unmodified has its dirty flag cleared
    /// and is dropped from the report, unless it is also staged. Structural
    /// dirty actions (add/move/copy/delete) are always treated as modified.
    ///
    /// The refreshed flags are persisted in the staged state, so this requires
    /// write capability.
    pub check_dirty: bool,
    // Drop the existing staged anchor before computing status.
    // Combine with `scan` to scan from a clean slate.
    pub reset: bool,
    // Include sync point
    pub sync_point: bool,
    // Only emit revision info, skip all diffs
    pub revision_only: bool,
    // Count directories and files (view-filtered) in the staged state if
    // present, otherwise the current revision
    pub count: bool,
}

async fn file_size_from_node_change_id(change: &NodeChange) -> Result<u64, StatusError> {
    if change.action == FileAction::Delete {
        Ok(0)
    } else {
        let size = change
            .to
            .state
            .node(change.to.repository.clone(), change.to.node)
            .await
            .forward::<StatusError>("accessing node path")?
            .size;
        Ok(size)
    }
}

async fn file_size_from_node_change_path(
    repository_path: &Path,
    change: &NodeChange,
) -> Result<u64, StatusError> {
    if change.action == FileAction::Delete {
        Ok(0)
    } else {
        let path_str = change.path.as_str().to_string();
        let result = tokio::fs::metadata(change.path.to_absolute_path(repository_path)).await;
        // The file may have vanished between the diff's filesystem walk and
        // this stat — e.g. a concurrent `branch switch` deleted it from the
        // working directory. Treat the concurrent deletion as benign and
        // report size 0 (matching the `FileAction::Delete` branch above)
        // rather than failing the whole status command with an Internal error.
        // On Windows a file mid-deletion stats as PermissionDenied rather than
        // NotFound, so treat that as benign too.
        if let Err(err) = &result
            && (err.kind() == std::io::ErrorKind::NotFound
                || (cfg!(target_family = "windows")
                    && err.kind() == std::io::ErrorKind::PermissionDenied))
        {
            return Ok(0);
        }
        let metadata =
            result.internal_with(|| format!("accessing metadata for file {path_str}"))?;
        #[cfg(target_family = "windows")]
        let size = metadata.file_size();
        #[cfg(target_family = "unix")]
        let size = metadata.size();
        Ok(size)
    }
}

/// Verify whether a dirty file change reflects a real on-disk modification,
/// clearing the node's dirty flag when it does not.
///
/// Structural dirty actions (add/move/copy/delete) are modifications by
/// definition and always report `true`. For a content modification the file is
/// compared against its tracked node (the staged side of the diff, which
/// carries the tracked content hash and size): a differing size is a modification;
/// otherwise, when the recorded modification time differs, the content is
/// rehashed and compared. A file that turns out unmodified has its dirty flag
/// cleared on the staged node (propagating to parents) and reports `false`.
///
/// A missing or unreadable file is reported as modified — the dirty flag then
/// reflects a real change that the regular diff will surface.
async fn dirty_change_is_modified(
    repository: Arc<RepositoryContext>,
    change: &NodeChange,
) -> Result<bool, StatusError> {
    if change.action != FileAction::Keep {
        return Ok(true);
    }

    let node_state = &change.to;
    if !node_state.node.is_valid_node_id() {
        return Ok(true);
    }
    let node = node_state
        .get_node()
        .await
        .forward::<StatusError>("loading dirty node for verification")?;
    if !node.is_file() {
        return Ok(true);
    }

    let absolute_path = change.path.to_absolute_path(repository.require_path()?);
    let Ok(metadata) = tokio::fs::metadata(&absolute_path).await else {
        return Ok(true);
    };
    if !metadata.is_file() {
        return Ok(true);
    }

    let (file_mtime, file_size) = crate::util::fs::file_mtime_and_size(&metadata);
    let (is_modified, _file_hash) = state::is_file_modified(
        repository.clone(),
        &node,
        file_mtime,
        file_size,
        &change.path,
        false,
    )
    .await
    .forward::<StatusError>("comparing dirty file against filesystem")?;

    if !is_modified {
        node_state
            .state
            .node_clear_dirty(node_state.repository.clone(), node_state.node)
            .await
            .forward::<StatusError>("clearing stale dirty flag")?;
    }

    Ok(is_modified)
}

/// Upper bound on concurrent subtree-counting tasks. Counting is dominated by
/// per-directory node-block reads (and link resolutions that may reach other
/// repositories), so overlapping them up to this many in-flight tasks hides I/O
/// latency while keeping fan-out bounded on huge trees. Scaled to the machine
/// but capped so a many-core host doesn't spawn excessive workers.
const COUNT_MAX_CONCURRENCY: usize = 128;

/// A directory (or resolved link target) whose children still need counting.
struct CountWork {
    state: Arc<state::State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    path: RelativePath,
}

/// Shared state for the bounded worker pool counting view-filtered nodes.
///
/// The recursion is reified as an explicit lock-free `queue` of [`CountWork`]
/// items drained by a fixed set of workers, rather than recursive task
/// spawning, so concurrency is hard-bounded by the worker count regardless of
/// tree shape. `outstanding` tracks items neither fully processed nor yet
/// counted (queued plus in flight); workers exit once it reaches zero. The
/// first error is kept in `error`, after which workers stop processing but
/// keep draining so the counter still reaches zero and every worker terminates.
struct CountShared {
    queue: SegQueue<CountWork>,
    outstanding: AtomicUsize,
    directories: AtomicU64,
    files: AtomicU64,
    error: OnceLock<StatusError>,
    notify: Notify,
}

/// Resolve `source_path` to the work needed to count its subtree, labelling
/// descendant paths under `target_path` for view filtering. The two differ for
/// a layer: the node is resolved at the layer's `source_path` while paths are
/// labelled with the mount `target_path`, so the local view filter matches the
/// working-tree layout. Returns the node's own `(directories, files)`
/// contribution plus an optional root [`CountWork`] for its descendants. A file
/// yields `(0, 1, None)`; a directory or link `(1, 0, Some(work))`. An empty
/// `source_path` is the root of `state` (a layer whose source is the repo
/// root), counted as one directory plus its descendants. An unresolved path
/// yields `(0, 0, None)`.
async fn count_at_path_root(
    state: Arc<state::State>,
    repository: Arc<RepositoryContext>,
    source_path: &RelativePath,
    target_path: &RelativePath,
) -> Result<(u64, u64, Option<CountWork>), StatusError> {
    if source_path.is_empty() {
        return Ok((
            1,
            0,
            Some(CountWork {
                state,
                repository,
                node_id: ROOT_NODE,
                path: target_path.clone(),
            }),
        ));
    }

    let Ok(link) = state
        .find_node_link(repository.clone(), source_path.as_str())
        .await
    else {
        return Ok((0, 0, None));
    };
    if !link.is_valid() {
        return Ok((0, 0, None));
    }

    let (repository, state) = link
        .resolve(repository.clone(), state.clone())
        .await
        .forward::<StatusError>("resolving count path")?;
    let node = state
        .node(repository.clone(), link.node)
        .await
        .forward::<StatusError>("reading count path node")?;

    if node.is_file() {
        return Ok((0, 1, None));
    }

    if node.is_link() {
        let inner = node.linked_node();
        let (repository, state) = inner
            .resolve(repository.clone(), state.clone())
            .await
            .forward::<StatusError>("resolving count path link target")?;
        return Ok((
            1,
            0,
            Some(CountWork {
                state,
                repository,
                node_id: inner.node,
                path: target_path.clone(),
            }),
        ));
    }

    Ok((
        1,
        0,
        Some(CountWork {
            state,
            repository,
            node_id: link.node,
            path: target_path.clone(),
        }),
    ))
}

/// Count `work`'s direct children, honoring the repository's local view filter,
/// adding files/directories to the shared totals and pushing each directory (or
/// resolved link target) back onto the shared stack for later counting. A link
/// node is counted as a directory and descended into.
async fn count_node_children(work: &CountWork, shared: &CountShared) -> Result<(), StatusError> {
    let mut children = state::StateNodeChildrenWithNameIterator::new(
        work.state.clone(),
        work.repository.clone(),
        work.node_id,
    )
    .await
    .forward::<StatusError>("iterating revision tree children")?;

    let mut directories = 0u64;
    let mut files = 0u64;
    let mut pushed = Vec::new();

    while let Some((child_id, child_node, child_name)) = children
        .next()
        .await
        .forward::<StatusError>("reading revision tree node")?
    {
        let is_directory = child_node.is_directory();
        let is_link = child_node.is_link();
        let child_path = work.path.push_into_buf(child_name).freeze();

        if work
            .repository
            .filter
            .excludes(&child_path, is_directory || is_link, FilterMode::View)
        {
            continue;
        }

        if is_directory {
            directories += 1;
            pushed.push(CountWork {
                state: work.state.clone(),
                repository: work.repository.clone(),
                node_id: child_id,
                path: child_path,
            });
        } else if is_link {
            directories += 1;
            let link = child_node.linked_node();
            let (link_repository, link_state) = link
                .resolve(work.repository.clone(), work.state.clone())
                .await
                .forward::<StatusError>("resolving link target for count")?;
            pushed.push(CountWork {
                state: link_state,
                repository: link_repository,
                node_id: link.node,
                path: child_path,
            });
        } else if child_node.is_file() {
            files += 1;
        }
    }

    if directories > 0 {
        shared.directories.fetch_add(directories, Ordering::Relaxed);
    }
    if files > 0 {
        shared.files.fetch_add(files, Ordering::Relaxed);
    }

    if !pushed.is_empty() {
        // Account for the new work before it becomes visible so a worker that
        // pops and finishes a child can't drive `outstanding` to zero early.
        shared.outstanding.fetch_add(pushed.len(), Ordering::AcqRel);
        for work in pushed {
            shared.queue.push(work);
        }
        shared.notify.notify_waiters();
    }

    Ok(())
}

/// A single pool worker: drain the shared queue until no work remains in flight.
///
/// Termination is driven solely by `outstanding` reaching zero, so it is robust
/// regardless of scheduling: a notification interest is registered (`enable`)
/// before each empty-queue check, so a concurrent push or completion can never
/// be missed before the worker parks on `notify`.
async fn count_worker(shared: Arc<CountShared>) -> Result<(), StatusError> {
    loop {
        let notified = shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let Some(work) = shared.queue.pop() else {
            if shared.outstanding.load(Ordering::Acquire) == 0 {
                shared.notify.notify_waiters();
                return Ok(());
            }
            notified.await;
            continue;
        };

        // Once any worker has failed, stop processing but keep draining so
        // `outstanding` still reaches zero and every worker terminates. The
        // `OnceLock` keeps the first error and ignores the rest.
        if shared.error.get().is_none()
            && let Err(err) = count_node_children(&work, &shared).await
        {
            let _ = shared.error.set(err);
        }

        if shared.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            shared.notify.notify_waiters();
        }
    }
}

/// Count directories and files in the subtrees rooted at `roots`, honoring each
/// repository's local view filter, using a pool of at most
/// [`COUNT_MAX_CONCURRENCY`] workers. Returns the summed `(directories, files)`
/// across all roots (excluding the root nodes themselves).
async fn count_subtrees(roots: Vec<CountWork>) -> Result<(u64, u64), StatusError> {
    if roots.is_empty() {
        return Ok((0, 0));
    }

    let queue = SegQueue::new();
    let outstanding = roots.len();
    for work in roots {
        queue.push(work);
    }

    let shared = Arc::new(CountShared {
        queue,
        outstanding: AtomicUsize::new(outstanding),
        directories: AtomicU64::new(0),
        files: AtomicU64::new(0),
        error: OnceLock::new(),
        notify: Notify::new(),
    });

    let workers = lore_base::runtime::processor_count().clamp(1, COUNT_MAX_CONCURRENCY);
    let mut tasks = JoinSet::new();
    for _ in 0..workers {
        let shared = shared.clone();
        lore_spawn!(tasks, count_worker(shared));
    }
    lore_drain_tasks!(tasks, StatusError::internal("Count worker task failed"))?;

    let directories = shared.directories.load(Ordering::Relaxed);
    let files = shared.files.load(Ordering::Relaxed);
    if shared.error.get().is_some() {
        let shared = Arc::into_inner(shared)
            .expect("all count workers have completed and released their references");
        return Err(shared
            .error
            .into_inner()
            .expect("error presence was just observed"));
    }

    Ok((directories, files))
}

/// Resolve the remote's latest revision for a branch, degrading to the existing
/// `NoRemote` state when the remote is unavailable so an unreachable remote never
/// stalls a local status read; transport connect timeouts bound the wait.
///
/// Returns `(latest, authorized, available)`, where `available` reflects
/// connectivity, not query success — a reachable remote that errors is still available.
async fn resolve_remote_latest(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
) -> (Option<Hash>, bool, bool) {
    let remote = match repository.remote().await {
        Ok(r) => r,
        Err(err) => {
            lore_debug!("Remote unavailable for status: {err}");
            return (None, false, false);
        }
    };

    match branch::load_remote(remote, repository.id, branch_id).await {
        Ok(status) => (Some(status.latest), true, true),
        Err(err) if err.is_branch_not_found() => (None, true, true),
        Err(err) => {
            lore_debug!("Remote branch query failed: {err}");
            (None, false, true)
        }
    }
}

pub async fn status(
    repository: Arc<RepositoryContext>,
    paths: Option<Vec<RelativePath>>,
    options: StatusOptions,
) -> Result<(), StatusError> {
    if options.reset {
        crate::instance::delete_staged_anchor(&repository)
            .await
            .forward::<StatusError>("dropping staged anchor for status reset")?;
    }

    let (state_current, state_staged, current_branch) =
        state::State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<StatusError>("deserializing current and staged state")?;

    let mut has_staged = state_staged.is_some();
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    lore_debug!(
        "Repository status, current signature {}, staged signature {}",
        state_current.revision(),
        state_staged.revision()
    );

    let layers = {
        let mut layers = vec![];
        let list = layer::list(repository.clone()).await.unwrap_or_default();
        for layer in list {
            let layer_state = layer
                .deserialize_current_and_staged(repository.clone())
                .await
                .forward::<StatusError>("deserializing layer state")?;

            if !layer_state.state_staged.revision().is_zero()
                && layer_state.state_staged.revision() != layer_state.state_current.revision()
            {
                has_staged = true;
            }

            layers.push((layer, layer_state));
        }
        layers
    };

    // Pre-resolve layer mount metadata for the parent's filesystem walker:
    // for each configured layer, find the source_path node in the layer's
    // staged state. When the walker hits one of these mount paths it switches
    // comparison context to the layer's tree rather than treating the
    // mount-point contents as parent-tree adds.
    let layer_mounts: Arc<Vec<state::LayerMountInfo>> = {
        let mut mounts = Vec::new();
        for (layer, layer_state) in layers.iter() {
            let source_node_link = layer_state
                .state_staged
                .find_node_link(layer_state.repository.clone(), &layer.source_path)
                .await;
            let Ok(source_node_link) = source_node_link else {
                lore_debug!(
                    "Skipping layer mount {} — source path {} not found in layer state",
                    layer.target_path,
                    layer.source_path
                );
                continue;
            };
            mounts.push(state::LayerMountInfo {
                target_path: layer.target_path.clone(),
                repository: layer_state.repository.clone(),
                state: layer_state.state_staged.clone(),
                source_node: source_node_link.node,
            });
        }
        Arc::new(mounts)
    };

    let branch_metadata = branch::metadata(repository.clone(), current_branch)
        .await
        .forward::<StatusError>("loading branch metadata")?;
    let branch = branch::branch_metadata(repository.clone(), current_branch, &branch_metadata)
        .await
        .forward::<StatusError>("loading branch info")?;
    let branch_stack = branch::stack(&branch_metadata);

    let show_staged = options.staged;
    let show_scan = options.scan;
    let check_dirty = options.check_dirty;

    // Accumulates per-action dirty counts across the parallel staged/scan
    // walks; emitted as a single summary event for --scan / --check-dirty.
    let summary = Arc::new(StatusSummaryStats::default());

    let local_latest = branch::load_latest(repository.clone(), branch.id)
        .await
        .unwrap_or_default();

    let local_state = state::State::deserialize(repository.clone(), local_latest)
        .await
        .forward::<StatusError>("deserializing local state")?;

    // Authorized only on an authoritative answer — a latest revision
    // or branch not found; proving the identity is authorized and has access
    let (remote_latest, remote_authorized, remote_available) =
        resolve_remote_latest(&repository, branch.id).await;

    let remote_state = if let Some(remote_latest) = remote_latest {
        state::State::deserialize(repository.clone(), remote_latest)
            .await
            .ok()
    } else {
        None
    };

    let branch_parent = branch_stack
        .first()
        .map(|parent| parent.branch)
        .unwrap_or_default();
    let branch_point = branch_stack
        .first()
        .map(|parent| parent.revision)
        .unwrap_or_default();

    let revision_merged_parent_branch = if options.sync_point {
        if branch_point.is_zero() {
            Hash::default()
        } else {
            let mut search_point = state_current.revision();

            // Repeatedly search for a revision that's the result of a merge and then
            // check if the merged revision was coming from the parent branch.
            loop {
                let Ok(signature) = find::find_revision(
                    repository.clone(),
                    current_branch,
                    search_point,
                    false,
                    None,
                    |state, _metadata| {
                        let is_branch_point = state.revision() == branch_point;
                        let is_merge = !state.parent_other().is_zero();

                        if is_merge || is_branch_point {
                            find::FindMatchResult::Match
                        } else {
                            find::FindMatchResult::Continue
                        }
                    },
                )
                .await
                else {
                    break Hash::default();
                };

                if signature == branch_point {
                    lore_debug!(
                        "Found branch point {} as last merged in from parent branch",
                        signature
                    );
                    break signature;
                }

                let branch_state = state::State::deserialize(repository.clone(), signature)
                    .await
                    .forward::<StatusError>("deserializing branch state")?;
                let parent_state =
                    state::State::deserialize(repository.clone(), branch_state.parent_other())
                        .await
                        .forward::<StatusError>("deserializing parent state")?;
                let parent_state_metadata =
                    Metadata::deserialize(repository.clone(), parent_state.metadata_hash())
                        .await
                        .forward::<StatusError>("deserializing parent metadata")?;
                let parent_state_branch = parent_state_metadata
                    .get_branch()
                    .forward::<StatusError>("reading parent branch from metadata")?;
                if parent_state_branch == branch_parent {
                    lore_debug!(
                        "Found revision {} as last merged in from parent branch",
                        parent_state.revision()
                    );
                    break parent_state.revision();
                }

                search_point = branch_state.parent_self();
            }
        }
    } else {
        Hash::default()
    };

    let mut local_ahead = false;
    let mut remote_ahead = false;

    let last_sync = branch::load_last_sync(repository.clone(), branch.id)
        .await
        .unwrap_or_default();

    // Authoritative answer to "does local have commits not on remote history?":
    // the LATEST_STATUS flag set by commit/push/sync/clone/restore. When
    // Convergent, local_latest is guaranteed to be on the remote history line —
    // any difference can only mean remote moved past us.
    let local_diverged = branch::load_latest_divergent(repository.clone(), branch.id)
        .await
        .unwrap_or(true);

    if local_latest != remote_latest.unwrap_or_default()
        && let Some(remote_state) = remote_state.clone()
    {
        let local_n = local_state.revision_number();
        let remote_n = remote_state.revision_number();
        if !local_diverged {
            remote_ahead = remote_n > local_n;
        } else if remote_n > local_n {
            // Local has unpushed work AND remote moved beyond it.
            local_ahead = true;
            remote_ahead = true;
        } else if local_n > remote_n {
            local_ahead = true;
            // Refine with last_sync: if remote has moved beyond the last
            // recorded sync point, it has commits we don't have.
            if last_sync != remote_latest.unwrap_or_default() {
                remote_ahead = true;
            }
        } else {
            // Same revision number, different hashes — divergent.
            local_ahead = true;
            remote_ahead = true;
        }
    }
    {
        let status = match (remote_ahead, local_ahead) {
            (true, true) => "divergent",
            (true, false) => "remote ahead",
            (false, true) => "local ahead",
            (false, false) => "synchronized",
        };
        lore_debug!(
            "Branch is {}, remote LATEST {}, local LATEST {}, last sync {}",
            status,
            remote_latest.unwrap_or_default(),
            local_latest,
            last_sync
        );
    }

    {
        let data = LoreRepositoryStatusRevisionEventData::new(
            repository.id,
            branch.id,
            branch.name.as_str(),
            state_current.revision(),
            state_current.revision_number(),
            if has_staged {
                state_staged.revision()
            } else {
                Hash::default()
            },
            state_staged.parent_other(),
            revision_merged_parent_branch,
            local_state.revision(),
            local_state.revision_number(),
            remote_latest.unwrap_or_default(),
            if let Some(remote_state) = remote_state {
                remote_state.revision_number()
            } else {
                0
            },
            local_ahead,
            remote_ahead,
            remote_available,
            remote_authorized,
            remote_latest.is_some(),
        );
        lore_debug!("Repository status: {data:?}");
        event::LoreEvent::RepositoryStatusRevision(data).send();
    }

    let paths = match paths.map(RelativePath::dedup_to_supersets) {
        // Caller supplied a path filter that survived dedup — iterate it.
        Some(deduped) if !deduped.is_empty() => deduped.into_iter().map(Some).collect(),
        // No filter, or dedup collapsed to the repository root — scan everything.
        _ => vec![None],
    };

    if options.count {
        let mut directories = 0u64;
        let mut files = 0u64;
        let mut roots = Vec::new();

        for path in paths.iter() {
            match path {
                None => {
                    roots.push(CountWork {
                        state: state_staged.clone(),
                        repository: repository.clone(),
                        node_id: ROOT_NODE,
                        path: RelativePath::default(),
                    });
                }
                Some(path) => {
                    let (path_directories, path_files, work) =
                        count_at_path_root(state_staged.clone(), repository.clone(), path, path)
                            .await?;
                    directories += path_directories;
                    files += path_files;
                    roots.extend(work);
                }
            };

            for (layer, layer_state) in layers.iter() {
                let target_path =
                    RelativePath::new_from_initial_path(&layer.target_path).unwrap_or_default();
                let selected = path.clone().unwrap_or_else(|| target_path.clone());
                if !selected.is_empty() && !selected.overlaps(&layer.target_path) {
                    continue;
                }
                let sub_path = if selected.as_str().len() > target_path.len() {
                    &selected.as_str()[target_path.len()..]
                } else {
                    ""
                };
                let source_subpath =
                    RelativePath::new_from_clean_parts(&layer.source_path, sub_path);
                let target_subpath =
                    RelativePath::new_from_clean_parts(&layer.target_path, sub_path);
                let (layer_directories, layer_files, work) = count_at_path_root(
                    layer_state.state_staged.clone(),
                    layer_state.repository.clone(),
                    &source_subpath,
                    &target_subpath,
                )
                .await?;
                directories += layer_directories;
                files += layer_files;
                roots.extend(work);
            }
        }

        let (subtree_directories, subtree_files) = count_subtrees(roots).await?;
        directories += subtree_directories;
        files += subtree_files;

        lore_debug!("Repository size: {directories} directories, {files} files");
        event::LoreEvent::RepositoryStatusCount(LoreRepositoryStatusCountEventData {
            directories,
            files,
        })
        .send();
    }

    if options.revision_only {
        return Ok(());
    }

    // Compare current state against staged state
    if show_staged && has_staged {
        lore_debug!("Calculating deltas against staged revision");

        let mut tasks = JoinSet::new();
        for path in paths.iter() {
            lore_spawn!(tasks, {
                let repository = repository.clone();
                let state_current = state_current.clone();
                let state_staged = state_staged.clone();
                let path = path.clone();
                let summary = summary.clone();
                async move {
                    let changes = state::diff_collect(
                        repository.clone(),
                        state_current,
                        repository.clone(),
                        state_staged.clone(),
                        path,
                        FilterMode::Full,
                    )
                    .await
                    .forward::<StatusError>("computing diff against staged state")?;
                    lore_debug!("Found {} changes in staged revision", changes.len());

                    for change in changes.iter() {
                        // When scanning, skip dirty-only changes from the
                        // state diff — the scan section re-detects them from
                        // the filesystem and handles set/clear inline.
                        let dominated_by_scan =
                            show_scan && change.flags.is_dirty() && !change.flags.is_stage();
                        if dominated_by_scan
                            || !(change.flags.is_stage() || change.flags.is_dirty())
                        {
                            continue;
                        }

                        let mut cleared_dirty = false;
                        if check_dirty
                            && change.flags.is_dirty()
                            && !dirty_change_is_modified(repository.clone(), change).await?
                        {
                            if !change.flags.is_stage() {
                                continue;
                            }
                            cleared_dirty = true;
                        }

                        // Count nodes that remain dirty (verify did not clear
                        // them) toward the summary; purely-staged changes are
                        // not part of the dirty tracking count.
                        if change.flags.is_dirty() && !cleared_dirty {
                            summary.classify(change);
                        }

                        let size = file_size_from_node_change_id(change).await?;
                        let mut data =
                            LoreRepositoryStatusFileEventData::from_node_change(change, size);
                        if cleared_dirty {
                            data.flag_dirty = 0;
                        }
                        event::LoreEvent::RepositoryStatusFile(data).send();
                    }

                    Ok(())
                }
            });

            for (layer, layer_state) in layers.iter() {
                let target_path =
                    RelativePath::new_from_initial_path(&layer.target_path).unwrap_or_default();
                let path = path.clone().unwrap_or_else(|| target_path.clone());
                if path.is_empty() || path.overlaps(&layer.target_path) {
                    lore_spawn!(tasks, {
                        let repository = layer_state.repository.clone();
                        let state_current = layer_state.state_current.clone();
                        let state_staged = layer_state.state_staged.clone();
                        let source_path = layer.source_path.clone();
                        let sub_path = if path.as_str().len() > target_path.len() {
                            &path.as_str()[target_path.len()..]
                        } else {
                            ""
                        };
                        let path = RelativePath::new_from_clean_parts(&source_path, sub_path);
                        let path = if !path.is_empty() { Some(path) } else { None };
                        async move {
                            let mut changes = state::diff_collect(
                                repository.clone(),
                                state_current,
                                repository.clone(),
                                state_staged.clone(),
                                path,
                                FilterMode::Full,
                            )
                            .await
                            .forward::<StatusError>("computing diff against staged state")?;
                            lore_debug!(
                                "Found {} changes in layer \"{}\" staged revision",
                                target_path,
                                changes.len()
                            );

                            for change in changes.iter_mut() {
                                // TODO(mjansson): Translate paths for file size
                                let size = 0;
                                /*
                                let size = file_size_from_node_change_id(change).await?;
                                */

                                change
                                    .translate_from_layer_path(&source_path, target_path.as_str());

                                event::LoreEvent::RepositoryStatusFile(
                                    LoreRepositoryStatusFileEventData::from_node_change(
                                        change, size,
                                    ),
                                )
                                .send();
                            }

                            Ok(())
                        }
                    });
                }
            }
        }

        lore_drain_tasks!(tasks, StatusError::internal("Recursion task failed"))?;
    }

    // Compare current/staged state against filesystem
    if show_scan {
        lore_debug!(
            "Calculating deltas against filesystem for {} paths",
            paths.len()
        );

        let mut tasks = JoinSet::new();
        for path in paths.iter() {
            let repository = repository.clone();
            let state_current = state_current.clone();
            let state_staged = state_staged.clone();
            let path = path.clone();
            let layer_mounts = layer_mounts.clone();
            let summary = summary.clone();
            let exists = if let Some(path) = path.as_ref() {
                let mut exists_in_state = false;
                let mut exists_in_filesystem = false;

                let state = if has_staged {
                    state_staged.clone()
                } else {
                    state_current.clone()
                };

                let node_link = state
                    .find_node_link(repository.clone(), path.as_str())
                    .await
                    .unwrap_or_default();
                if node_link.is_valid() {
                    exists_in_state = true;
                } else {
                    let absolute_path = path.to_absolute_path(repository.require_path()?);
                    exists_in_filesystem = std::fs::exists(absolute_path).unwrap_or_default();
                }

                if !exists_in_state && !exists_in_filesystem {
                    emit_path_ignore(path.as_str()).await;
                    lore_trace!("Ignoring invalid path: {path}");
                }

                exists_in_state || exists_in_filesystem
            } else {
                true
            };

            if exists {
                lore_spawn!(tasks, {
                    async move {
                        if let Some(path) = path.as_ref() {
                            lore_debug!(
                                "Calculating deltas against filesystem path: {}",
                                path.as_str()
                            );
                        } else {
                            lore_debug!(
                                "Calculating deltas against filesystem for full repository"
                            );
                        }

                        let start = Instant::now();

                        // Scan uses staged state as diff base with scan_dirty=true.
                        // Content hashes in staged state are either zero (add nodes)
                        // or equal to current revision hashes, so the comparison is
                        // effectively filesystem vs committed content.
                        // The current revision is passed as the second pair so the
                        // walk can distinguish "node exists in staged but not in
                        // committed" — i.e. unstaged adds — from regular tracked
                        // files. Dirty flags are set/cleared inline during the walk.
                        let (changes, _stats) = state::diff_filesystem_ex(
                            repository.clone(),
                            state_staged.clone(),
                            repository.clone(),
                            state_current.clone(),
                            path,
                            FilterMode::Full,
                            true, // scan_dirty
                            layer_mounts.clone(),
                        )
                        .await
                        .forward::<StatusError>("computing diff against filesystem")?;

                        lore_debug!(
                            "Scan found {} file system changes in {:.3}s",
                            changes.len(),
                            start.elapsed().as_secs_f64(),
                        );

                        for change in changes.iter() {
                            let size =
                                file_size_from_node_change_path(repository.require_path()?, change)
                                    .await?;

                            // Emit event for display (dirty set/clear handled inline by diff)
                            if !change.flags.is_stage() {
                                summary.classify(change);
                                event::LoreEvent::RepositoryStatusFile(
                                    LoreRepositoryStatusFileEventData::from_node_change(
                                        change, size,
                                    ),
                                )
                                .send();
                            } else {
                                lore_debug!("Ignore staged file {}", change.path);
                            }
                        }

                        Ok(())
                    }
                });
            }

            lore_drain_tasks!(tasks, StatusError::internal("Recursion task failed"))?;
        }
    }

    // Emit the aggregate dirty-node summary for reconciling status runs. For
    // --scan these are the changes detected against the filesystem; for
    // --check-dirty they are the nodes that stayed dirty after verification.
    if show_scan || check_dirty {
        let data = summary.event_data();
        lore_debug!(
            "Status summary: {} added, {} modified, {} deleted, {} moved, {} copied",
            data.adds,
            data.modifies,
            data.deletes,
            data.moves,
            data.copies
        );
        event::LoreEvent::RepositoryStatusSummary(data).send();
    }

    // If the staged state was updated (by scan or other operations), serialize it.
    // When scanning, the state may have been modified even if no staged anchor existed before.
    // Opportunistically serialize only when the context carries write capability.
    // Read-only status invocations leave the dirty state for the next write command to flush.
    if (has_staged || show_scan)
        && state_staged.is_dirty()
        && let Some(token) = repository.try_write_token()
    {
        // Set up staged state metadata if this is a fresh state (cloned from current)
        if !has_staged {
            let current_revision = state_current.revision();
            state_staged.set_revision_number(0);
            state_staged.set_parent_self(current_revision);
            state_staged.set_parent_other(Hash::default());
            state_staged.set_metadata_hash(Hash::default());
        }
        // Serialize the new staged state
        let signature = state_staged
            .serialize(repository.clone(), token)
            .await
            .forward::<StatusError>("serializing staged revision state")?;

        // Serialize the new staged anchor
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<StatusError>("serializing staged revision anchor")?;
    }

    Ok(())
}

#[cfg(test)]
mod remote_resolve_tests {
    use lore_transport::ProtocolError;

    use super::*;
    use crate::errors::Disconnected;
    use crate::lore::BranchId;
    use crate::repository::RemoteState;
    use crate::repository::RepositoryContext;
    use crate::repository::RepositoryFormat;
    use crate::repository::create_client_memory_stores;

    fn disconnected() -> ProtocolError {
        ProtocolError::from(Disconnected)
    }

    async fn context_with_state(state: RemoteState) -> Arc<RepositoryContext> {
        let (immutable, mutable) = create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        Arc::new(RepositoryContext::new_with_state(
            None,
            immutable,
            mutable,
            crate::lore::RepositoryId::default(),
            crate::instance::InstanceId::default(),
            state,
            Arc::default(),
            RepositoryFormat::Lore,
        ))
    }

    #[tokio::test]
    async fn offline_remote_resolves_to_unavailable() {
        let ctx = context_with_state(RemoteState::Offline).await;

        let result = resolve_remote_latest(&ctx, BranchId::default()).await;

        assert_eq!(
            result,
            (None, false, false),
            "offline should degrade to unavailable"
        );
    }

    #[tokio::test]
    async fn failed_remote_resolves_to_unavailable() {
        let ctx = context_with_state(RemoteState::Failed(disconnected())).await;

        let result = resolve_remote_latest(&ctx, BranchId::default()).await;

        assert_eq!(
            result,
            (None, false, false),
            "failed remote should degrade to unavailable"
        );
    }
}
