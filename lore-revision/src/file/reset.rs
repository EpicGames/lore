// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::branch;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::file::unstage;
use crate::file::unstage::UnstageOptions;
use crate::filter::FilterMode;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore_debug;
use crate::lore_spawn_blocking;
use crate::lore_trace;
use crate::node;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::emit_path_ignore;
use crate::progress::DEFAULT_WORK_CHANNEL_CAPACITY;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::revision::sync;
use crate::revision::sync::SyncRealizeStats;
use crate::runtime::execution_context;
use crate::state;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;

/// Data for the event emitted when a reset operation begins.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetBeginEventData {
    /// Number of paths requested for reset.
    pub path_count: usize,
}

/// Running counts of items processed during a reset operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetCountData {
    /// Number of directories that were reset.
    pub directory_reset_count: u64,
    /// Number of directories that were deleted.
    pub directory_delete_count: u64,
    /// Number of files that were reset.
    pub file_reset_count: u64,
    /// Number of files that were deleted.
    pub file_delete_count: u64,
}

/// Data for the progress event emitted periodically during a reset operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetProgressEventData {
    /// Current counts of items processed.
    pub count: LoreFileResetCountData,
}

/// Data for the event emitted when a reset operation completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetEndEventData {
    /// Final counts of items processed.
    pub count: LoreFileResetCountData,
}

/// Data for the event emitted for each file affected by a reset operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetFileEventData {
    /// Path of the file.
    pub path: LoreString,
    /// Action applied to the file.
    pub action: LoreFileAction,
    /// Previous path of the file, when it was moved.
    pub from_path: LoreString,
}

#[error_set]
pub enum ResetError {
    InvalidArguments,
    InvalidPath,
    RevisionNotFound,
    BranchNotFound,
    AddressNotFound,
    InvalidNodeHierarchy,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    WriteRequired,
    Disconnected,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    PayloadNotFound,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    LayerNotFound,
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
    MissingIdentity,
}

impl EventError for ResetError {
    fn translated(&self) -> LoreError {
        match self {
            ResetError::InvalidArguments(_) | ResetError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            ResetError::RevisionNotFound(_)
            | ResetError::BranchNotFound(_)
            | ResetError::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

const RESET_DIRECTORY_MAX: usize = 10_000;
pub const RESET_FILE_MAX: usize = 10_000;
pub const RESET_FILE_DISCOVERY: usize = 100;

pub struct ResetStats {
    directory_reset_count: AtomicU64,
    directory_delete_count: AtomicU64,
    file_reset_count: AtomicU64,
    file_delete_count: AtomicU64,

    file_inflight: Arc<Semaphore>,
    file_inflight_count: AtomicU64,
    directory_inflight: AtomicU64,
}

impl Default for ResetStats {
    fn default() -> Self {
        Self {
            directory_reset_count: AtomicU64::new(0),
            directory_delete_count: AtomicU64::new(0),
            file_reset_count: AtomicU64::new(0),
            file_delete_count: AtomicU64::new(0),
            file_inflight: Arc::new(Semaphore::new(RESET_FILE_DISCOVERY)),
            file_inflight_count: AtomicU64::new(0),
            directory_inflight: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ResetOptions {
    /// Delete untracked files
    pub purge: bool,
    /// Single node, no recursion
    pub single_node: bool,
}

/// Shared context passed through reset walk functions.
#[derive(Clone)]
struct ResetContext {
    repository: Arc<RepositoryContext>,
    state_target: Arc<State>,
    state_staged: Arc<State>,
    options: ResetOptions,
    stats: Arc<ResetStats>,
    file_tx: mpsc::Sender<ResetFileWorkItem>,
}

fn count_data(stats: &ResetStats) -> LoreFileResetCountData {
    LoreFileResetCountData {
        directory_reset_count: stats.directory_reset_count.load(Ordering::Relaxed),
        directory_delete_count: stats.directory_delete_count.load(Ordering::Relaxed),
        file_reset_count: stats.file_reset_count.load(Ordering::Relaxed),
        file_delete_count: stats.file_delete_count.load(Ordering::Relaxed),
    }
}

/// Per-file work item handed from the directory walker (producer) to the
/// realize loop (consumer). The walker has already done filter and staged
/// checks; the consumer only does the per-file realize work, gated by
/// the `file_inflight` semaphore.
struct ResetFileWorkItem {
    repository: Arc<RepositoryContext>,
    state_target: Arc<State>,
    relative_path: RelativePath,
    name: String,
    node_id: u32,
    node: Node,
}

/// Resets one or more files to a specified revision, optionally purging untracked files.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileResetBegin`](crate::interface::LoreEvent::FileResetBegin) | Emitted when reset starts, includes path count |
/// | [`LoreEvent::FileResetProgress`](crate::interface::LoreEvent::FileResetProgress) | Emitted periodically during file reset with progress counts |
/// | [`LoreEvent::FileResetEnd`](crate::interface::LoreEvent::FileResetEnd) | Emitted when reset completes |
/// | [`LoreEvent::FileResetFile`](crate::interface::LoreEvent::FileResetFile) | Emitted for each file that was reset |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file materialized |
/// | [`LoreEvent::FilterExclude`](crate::interface::LoreEvent::FilterExclude) | Emitted for each path excluded by filters |
pub async fn reset(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    revision: LoreString,
    options: ResetOptions,
) -> Result<(), ResetError> {
    let reset_link_count =
        reset_staged_links_under_paths(repository.clone(), token, &paths).await?;

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    let state_target = if revision.is_empty() {
        state_current
    } else {
        let revision_spec = revision.to_string();
        let resolved = revision::resolve(
            repository.clone(),
            revision.as_str(),
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|_err| {
            ResetError::from(RevisionNotFound {
                revision: revision_spec,
            })
        })?;
        state::State::deserialize(repository.clone(), resolved)
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?
    };

    event::LoreEvent::FileResetBegin(LoreFileResetBeginEventData {
        path_count: paths.len(),
    })
    .send();

    let stats = Arc::new(ResetStats::default());
    // Staged link nodes are reset by the prelude above, before the walk runs;
    // count them here so the summary reflects the work done. Links are
    // directory-shaped nodes, so they count as directories like the walk does.
    stats
        .directory_reset_count
        .fetch_add(reset_link_count as u64, Ordering::Relaxed);
    let outer_stats = stats.clone();
    let producer_repository = repository.clone();
    let producer_stats = stats.clone();
    let outer_state_staged = state_staged.clone();

    let result = run_reset_pipeline(stats.clone(), |file_tx| async move {
        let mut producer_failure: Option<ResetError> = None;
        let repository_root = match producer_repository.require_path() {
            Ok(p) => p.to_path_buf(),
            Err(e) => return Some(ResetError::from(e)),
        };
        for path in paths.as_slice().iter() {
            let Ok(relative_path) =
                RelativePath::new_from_user_path(repository_root.as_path(), path.as_str())
            else {
                emit_path_ignore(path.as_str()).await;
                lore_trace!("Ignoring invalid path: {path}");
                continue;
            };

            lore_debug!(
                "User path [{}] transformed to relative path [{}] in repository {}",
                path.as_str(),
                relative_path.as_str(),
                producer_repository.path_for_display()
            );

            let walk_result = reset_walk_path(
                ResetContext {
                    repository: producer_repository.clone(),
                    state_target: state_target.clone(),
                    state_staged: state_staged.clone(),
                    options,
                    stats: producer_stats.clone(),
                    file_tx: file_tx.clone(),
                },
                relative_path.clone(),
            )
            .await;

            if let Err(err) = walk_result {
                producer_failure = wrap_path_error(
                    producer_repository.clone(),
                    &relative_path,
                    path.as_str(),
                    err,
                )
                .await
                .err();
                break;
            }
        }
        producer_failure
    })
    .await;

    let counts = count_data(&outer_stats);
    lore_debug!(
        "Reset complete: {} reset ({} directories, {} files), {} deleted ({} directories, {} files)",
        counts.directory_reset_count + counts.file_reset_count,
        counts.directory_reset_count,
        counts.file_reset_count,
        counts.directory_delete_count + counts.file_delete_count,
        counts.directory_delete_count,
        counts.file_delete_count,
    );

    event::LoreEvent::FileResetEnd(LoreFileResetEndEventData { count: counts }).send();

    // If the staged state was modified (dirty flags cleared), persist it.
    // If no staged or dirty nodes remain, delete the anchor.
    if outer_state_staged.is_dirty() {
        let has_staged = outer_state_staged
            .node_has_staged_children(repository.clone(), ROOT_NODE)
            .await
            .forward::<ResetError>("Failed deserializing state node block")?;
        let has_dirty = outer_state_staged
            .node_has_dirty_children(repository.clone(), ROOT_NODE)
            .await
            .forward::<ResetError>("Failed deserializing state node block")?;

        if !has_staged && !has_dirty {
            crate::instance::delete_staged_anchor(&repository)
                .await
                .forward::<ResetError>("Failed deserializing state node block")?;
        } else {
            let token = repository
                .try_write_token()
                .expect("reset requires write access");
            let signature = outer_state_staged
                .serialize(repository.clone(), token)
                .await
                .forward::<ResetError>("Failed deserializing state node block")?;
            crate::instance::store_staged_anchor(&repository, signature)
                .await
                .forward::<ResetError>("Failed deserializing state node block")?;
        }
    }

    result
}

/// Resets files to the state they were in at the last merged revision on a branch.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## File Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::FileResetBegin`](crate::interface::LoreEvent::FileResetBegin) | Emitted when reset starts |
/// | [`LoreEvent::FileResetProgress`](crate::interface::LoreEvent::FileResetProgress) | Emitted periodically during file reset |
/// | [`LoreEvent::FileResetEnd`](crate::interface::LoreEvent::FileResetEnd) | Emitted when reset completes |
/// | [`LoreEvent::FileResetFile`](crate::interface::LoreEvent::FileResetFile) | Emitted for each file that was reset |
/// | [`LoreEvent::RevisionSyncProgress`](crate::interface::LoreEvent::RevisionSyncProgress) | Emitted during file realization |
/// | [`LoreEvent::RevisionSyncFile`](crate::interface::LoreEvent::RevisionSyncFile) | Emitted for each file materialized |
/// | [`LoreEvent::FilterExclude`](crate::interface::LoreEvent::FilterExclude) | Emitted for each path excluded by filters |
pub async fn reset_to_last_merged(
    repository: Arc<RepositoryContext>,
    paths: LoreArray<LoreString>,
    branch: LoreString,
    options: ResetOptions,
) -> Result<(), ResetError> {
    if branch.is_empty() {
        lore_debug!("Cannot reset to last merged without a branch");
        return Err(BranchNotFound {
            branch: String::new(),
        }
        .into());
    }

    let (state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    let branch_name = branch.to_string();
    let branch = branch::resolve(repository.clone(), branch.as_str())
        .await
        .map_err(|_err| {
            ResetError::from(BranchNotFound {
                branch: branch_name.clone(),
            })
        })?;

    let branch_current = current_branch;
    if branch.id == branch_current {
        lore_debug!("Cannot reset to last merged of the same branch");
        return Err(BranchNotFound {
            branch: branch_name.clone(),
        }
        .into());
    }

    let metadata = branch::metadata(repository.clone(), branch_current)
        .await
        .internal("Failed to load branch metadata")?;
    let stack = branch::stack(&metadata);

    let mut branch_point = Hash::default();
    for parent in stack.iter() {
        if parent.branch == branch.id {
            branch_point = parent.revision;
            break;
        }
    }
    if branch_point.is_zero() {
        lore_debug!("Cannot reset without a branch point");
        return Err(BranchNotFound {
            branch: branch_name,
        }
        .into());
    }

    lore_debug!(
        "Found branch point {branch_point} from branch {branch:?} for current branch {branch_current}"
    );

    event::LoreEvent::FileResetBegin(LoreFileResetBeginEventData {
        path_count: paths.len(),
    })
    .send();

    let stats = Arc::new(ResetStats::default());
    let outer_stats = stats.clone();
    let producer_repository = repository.clone();
    let producer_stats = stats.clone();
    let branch_id = branch.id;

    let result = run_reset_pipeline(stats.clone(), |file_tx| async move {
        let mut producer_failure: Option<ResetError> = None;
        let repository_root = match producer_repository.require_path() {
            Ok(p) => p.to_path_buf(),
            Err(e) => return Some(ResetError::from(e)),
        };
        for path in paths.as_slice().iter() {
            let Ok(relative_path) =
                RelativePath::new_from_user_path(repository_root.as_path(), path.as_str())
            else {
                emit_path_ignore(path.as_str()).await;
                lore_trace!("Ignoring invalid path: {path}");
                continue;
            };

            lore_debug!(
                "User path [{}] transformed to relative path [{}] in repository {}",
                path.as_str(),
                relative_path.as_str(),
                producer_repository.path_for_display()
            );

            // Resolve the revision to reset to. First find that start state where the node exist.
            // Then iterate file history from that point backwards and find the last merge point
            // from the given branch.
            let target_result = resolve_last_merged_target(
                producer_repository.clone(),
                state_current.clone(),
                branch_id,
                branch_point,
                &relative_path,
            )
            .await;

            let state_target = match target_result {
                Ok(state) => state,
                Err(err) => {
                    producer_failure = wrap_path_error(
                        producer_repository.clone(),
                        &relative_path,
                        path.as_str(),
                        err,
                    )
                    .await
                    .err();
                    break;
                }
            };

            let walk_result = reset_walk_path(
                ResetContext {
                    repository: producer_repository.clone(),
                    state_target,
                    state_staged: state_staged.clone(),
                    options,
                    stats: producer_stats.clone(),
                    file_tx: file_tx.clone(),
                },
                relative_path.clone(),
            )
            .await;

            if let Err(err) = walk_result {
                producer_failure = wrap_path_error(
                    producer_repository.clone(),
                    &relative_path,
                    path.as_str(),
                    err,
                )
                .await
                .err();
                break;
            }
        }
        producer_failure
    })
    .await;

    event::LoreEvent::FileResetEnd(LoreFileResetEndEventData {
        count: count_data(&outer_stats),
    })
    .send();

    result
}

/// Resets staged-link nodes under the user-supplied paths by delegating them
/// to `unstage`, which holds the add/remove/pin-change rollbacks. No-op when no
/// staged links overlap the given paths. Returns the number of staged link
/// nodes that were reset so the caller can count them in the reset summary.
async fn reset_staged_links_under_paths(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: &LoreArray<LoreString>,
) -> Result<usize, ResetError> {
    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    let staged_link_paths =
        link::list_staged_node_paths(&state_current, &state_staged, repository.clone())
            .await
            .map_err(|err| {
                ResetError::internal(format!("Failed to list staged link nodes: {err}"))
            })?;
    if staged_link_paths.is_empty() {
        return Ok(0);
    }

    let mut relative_user_paths: Vec<RelativePath> = Vec::new();
    let mut reset_all = false;
    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            continue;
        };
        if relative_path.is_empty() {
            reset_all = true;
            break;
        }
        relative_user_paths.push(relative_path);
    }

    let mut matched: Vec<LoreString> = Vec::new();
    for absolute in staged_link_paths.iter() {
        let Ok(relative_link) =
            RelativePath::new_from_user_path(repository.require_path()?, absolute)
        else {
            continue;
        };
        if reset_all
            || relative_user_paths
                .iter()
                .any(|p| p.covers(&relative_link.as_str()))
        {
            matched.push(LoreString::from(absolute));
        }
    }

    if matched.is_empty() {
        return Ok(0);
    }

    let reset_count = matched.len();
    unstage::unstage(
        repository,
        token,
        LoreArray::from_vec(matched),
        UnstageOptions::default(),
    )
    .await
    .forward::<ResetError>("Failed to reset staged link nodes")?;

    Ok(reset_count)
}

async fn wrap_path_error(
    repository: Arc<RepositoryContext>,
    relative_path: &RelativePath,
    user_path: &str,
    err: ResetError,
) -> Result<(), ResetError> {
    let absolute = relative_path.to_absolute_path(repository.require_path()?);
    let wrapped: Result<(), ResetError> = Err(err);
    match tokio::fs::metadata(&absolute).await {
        Ok(_) => wrapped
            .forward::<ResetError>(&format!("Failed resetting an existing path: {user_path}")),
        _ => wrapped.forward::<ResetError>(&format!(
            "Failed resetting a non-existent path: {user_path}"
        )),
    }
}

async fn resolve_last_merged_target(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    branch_id: BranchId,
    branch_point: Hash,
    relative_path: &RelativePath,
) -> Result<Arc<State>, ResetError> {
    let mut state_start = state_current;
    let node_link = state_start
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .unwrap_or_default();
    if !node_link.is_valid() {
        lore_debug!("Find state where node exist: {relative_path}");
        let revision = state_start.revision();
        if revision != branch_point {
            let parent = state_start.parent_self();
            if let Ok(state) =
                find_start_state(repository.clone(), parent, branch_point, relative_path).await
            {
                state_start = state;
                lore_debug!(
                    "Found revision where node exist: {relative_path} in {} -> {}",
                    state_start.revision(),
                    state_start.revision_number()
                );
            } else {
                state_start = state::State::deserialize(repository.clone(), branch_point)
                    .await
                    .forward::<ResetError>("Failed to deserialize revision state")?;
                lore_debug!(
                    "Found NO revision where node exist: {relative_path} use branch point revision {} -> {}",
                    state_start.revision(),
                    state_start.revision_number()
                );
            }
        }
    }

    let mut state_target = state_start.clone();
    if state_start.revision() != branch_point {
        let state_branch_point = state::State::deserialize(repository.clone(), branch_point)
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;
        if let Ok(state_merge) = find_merge_state(
            repository.clone(),
            state_start,
            state_branch_point.clone(),
            branch_id,
            relative_path,
        )
        .await
        {
            state_target = state_merge;
            lore_debug!(
                "Found revision where node was merged from branch: {relative_path} use branch point revision {} -> {}",
                state_target.revision(),
                state_target.revision_number()
            );
        } else {
            state_target = state_branch_point;
            lore_debug!(
                "Found NO revision where node was merged from branch: {relative_path} use branch point revision {} -> {}",
                state_target.revision(),
                state_target.revision_number()
            );
        }
    }

    Ok(state_target)
}

/// Sets up the producer/consumer pipeline used by both `reset` and
/// `reset_to_last_merged`. The caller-supplied `producer` does the per-path
/// driving and pushes file work items into `file_tx`. The consumer drains the
/// channel and runs `reset_file_realize` for each item, gated by the
/// `file_inflight` semaphore. A ticker emits progress events at 1Hz until
/// both halves complete.
async fn run_reset_pipeline<P, Fut>(stats: Arc<ResetStats>, producer: P) -> Result<(), ResetError>
where
    P: FnOnce(mpsc::Sender<ResetFileWorkItem>) -> Fut,
    Fut: Future<Output = Option<ResetError>> + Send,
{
    let (file_tx, file_rx) = mpsc::channel::<ResetFileWorkItem>(DEFAULT_WORK_CHANNEL_CAPACITY);

    let consumer_stats = stats.clone();
    let mut consumer =
        lore_spawn!(async move { reset_file_consume(file_rx, consumer_stats).await });

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));

    let producer_future = producer(file_tx);
    tokio::pin!(producer_future);

    let mut producer_failure: Option<ResetError> = None;
    let mut producer_done = false;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                event::LoreEvent::FileResetProgress(LoreFileResetProgressEventData {
                    count: count_data(&stats),
                })
                .send();
            },
            failure = &mut producer_future, if !producer_done => {
                producer_failure = failure;
                producer_done = true;
            },
            consumer_result = &mut consumer, if producer_done => {
                let consumer_result = consumer_result
                    .internal("Consumer task failed")
                    .map_err(ResetError::from)?;
                if let Some(err) = producer_failure {
                    return Err(err);
                }
                return consumer_result;
            }
        }
    }
}

async fn find_start_state(
    repository: Arc<RepositoryContext>,
    parent: Hash,
    branch_point: Hash,
    relative_path: &RelativePath,
) -> Result<Arc<State>, ResetError> {
    let mut parent = parent;
    while !parent.is_zero() {
        let state_parent = state::State::deserialize(repository.clone(), parent)
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;

        let node_link = state_parent
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if node_link.is_valid() {
            return Ok(state_parent);
        }

        if parent == branch_point {
            return Err(ResetError::internal("Failed to find node"));
        }

        let state = state_parent;
        parent = state.parent_self();
    }

    Err(ResetError::internal("Failed to find node"))
}

async fn find_merge_state(
    repository: Arc<RepositoryContext>,
    state_start: Arc<State>,
    state_end: Arc<State>,
    branch: BranchId,
    relative_path: &RelativePath,
) -> Result<Arc<State>, ResetError> {
    let mut state = state_start;
    while state.revision_number() > state_end.revision_number() {
        lore_debug!(
            "Check if revision {} -> {} is a merge from branch {}: merge {} parents are {} and {}",
            state.revision(),
            state.revision_number(),
            branch,
            state.is_merge(),
            state.parent_self(),
            state.parent_other()
        );
        if !state.parent_other().is_zero()
            && let Ok(state_merged) =
                state::State::deserialize(repository.clone(), state.parent_other()).await
            && state_merged.branch(repository.clone()).await == branch
        {
            return Ok(state);
        }

        let node_link = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .map_err(|_err| ResetError::internal("Failed to find node"))?;

        let node_id = node_link.node;

        // File metadata
        let metadata_node_id = node::node_to_file_metadata(node_id);
        let metadata_block_index = node::NodeFileMetadataBlock::index(metadata_node_id);
        let metadata_node_index = node::NodeFileMetadata::index(metadata_node_id);

        let metadata_block = state
            .block_file_metadata(repository.clone(), metadata_block_index)
            .await
            .forward::<ResetError>("Failed to deserialize metadata block")?;

        let parent = {
            let metadata_block_reader = metadata_block.read();
            let metadata_node = metadata_block_reader.node(metadata_node_index);

            metadata_node.revision[0]
        };

        state = state::State::deserialize(repository.clone(), parent)
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;

        lore_debug!(
            "File log found last modified in revision {} -> {}",
            parent,
            state.revision_number()
        );
    }

    Err(ResetError::internal("Failed to find revision"))
}

/// Boxed `reset_walk_directory` future with an explicit `Send` bound. Async
/// recursion needs this indirection so the spawned `JoinSet` work units
/// (which require `Send + 'static`) can await the recursive call.
fn reset_walk_directory_recurse(
    ctx: ResetContext,
    directory_path: RelativePath,
    node: Node,
    node_id: NodeID,
) -> Pin<Box<dyn Future<Output = Result<(), ResetError>> + Send>> {
    Box::pin(reset_walk_directory(ctx, directory_path, node, node_id))
}

/// Drive the directory walk for a single user-supplied path.
///
/// For the repository root path this walks every child of `ROOT_NODE` through
/// the directory-inflight bounded scheme; for any other path it resolves the
/// path to a node and dispatches a single walk through `reset_walk_node`.
/// Files end up on the `file_tx` channel for the consumer; directories
/// recurse via the same scheme.
async fn reset_walk_path(ctx: ResetContext, relative_path: RelativePath) -> Result<(), ResetError> {
    let ResetContext {
        repository,
        state_target,
        state_staged,
        options,
        stats,
        file_tx,
    } = ctx;

    lore_debug!(
        "Reset path: {}/{} to revision {} -> {}",
        repository.path_for_display(),
        relative_path.as_str(),
        state_target.revision(),
        state_target.revision_number()
    );

    let full_path = if !relative_path.is_empty() {
        // Find file system case variation that corresponds to user given path
        let repository_path = repository.require_path()?.to_path_buf();
        let fs_path = util::fs::filesystem_path(repository_path.as_path(), &relative_path)
            .await
            .unwrap_or(relative_path.as_str().to_string());
        repository_path.join(fs_path.as_str())
    } else {
        repository.require_path()?.to_path_buf()
    };

    let relative_path = RelativePath::new_from_user_path(
        repository.require_path()?,
        full_path.to_string_lossy().as_ref(),
    )
    .forward::<ResetError>(&format!("Invalid path {relative_path}"))?;

    if relative_path.is_empty() {
        if options.single_node {
            return Ok(());
        }

        lore_debug!("Resetting the repository from root");

        let node = state_target
            .node(repository.clone(), ROOT_NODE)
            .await
            .forward::<ResetError>("Failed to find node")?;

        return reset_walk_directory(
            ResetContext {
                repository,
                state_target,
                state_staged,
                options,
                stats,
                file_tx,
            },
            relative_path,
            node,
            ROOT_NODE,
        )
        .await;
    }

    let node_name = relative_path.name().to_string();

    let node_link = state_target
        .find_node_link(repository.clone(), relative_path.as_str())
        .await;

    match node_link {
        Ok(node_link) => {
            let (target_repository, target_state_current) = node_link
                .resolve(repository.clone(), state_target.clone())
                .await
                .forward::<ResetError>("Failed to deserialize revision state")?;

            // For staged state, only resolve through links (not for same-repo nodes)
            // to avoid re-deserializing to the wrong revision, which loses dirty flags
            let current_state_staged = if node_link.repository != repository.id {
                let (_, resolved_staged) = node_link
                    .resolve(repository.clone(), state_staged.clone())
                    .await
                    .forward::<ResetError>("Failed to deserialize revision state")?;
                resolved_staged
            } else {
                state_staged.clone()
            };

            let node = target_state_current
                .node(target_repository.clone(), node_link.node)
                .await
                .forward::<ResetError>("Failed to find node")?;

            reset_walk_node(
                ResetContext {
                    repository: target_repository,
                    state_target: target_state_current,
                    state_staged: current_state_staged,
                    options,
                    stats,
                    file_tx,
                },
                relative_path,
                node_name,
                node_link.node,
                node,
            )
            .await
        }
        Err(e) if e.is_node_not_found() => {
            // A path absent from the target revision is one of: a file committed
            // in the current revision but added after the target (delete it so the
            // working tree matches the target), a pending dirty-add, or an
            // untracked file (keep on disk unless --purge, discarding add tracking).
            let mut delete_path = options.purge;
            if let Ok(staged_link) = state_staged
                .find_node_link(repository.clone(), relative_path.as_str())
                .await
            {
                let staged_node = state_staged
                    .node(repository.clone(), staged_link.node)
                    .await
                    .forward::<ResetError>("Failed to find staged node")?;

                if staged_node.is_staged() {
                    return Err(InvalidArguments {
                        reason: "Failed to reset staged node".into(),
                    }
                    .into());
                }

                if staged_node.is_dirty_add() {
                    lore_trace!(
                        "Reset dirty add {}, discarding staged node and keeping the file",
                        relative_path.as_str()
                    );

                    let block_index = NodeBlock::index(staged_link.node);
                    let node_index = Node::index(staged_link.node);
                    let block = state_staged
                        .block(repository.clone(), block_index)
                        .await
                        .forward::<ResetError>("Failed to get staged block")?;
                    {
                        let mut writer = block.write();
                        writer.node(node_index).clear_all_change_flags();
                        writer.mark_dirty();
                    }
                    state_staged.block_modified(block.clone(), block_index);
                    state_staged.mark_dirty();

                    crate::state::node_discard_patch(
                        state_staged.clone(),
                        repository.clone(),
                        staged_link.node,
                        |_discarded_node_id, _flags| {},
                    )
                    .await
                    .forward::<ResetError>("Failed to discard reset dirty add node")?;

                    // Dirty parent cleanup
                    let mut parent_id = staged_node.parent;
                    while parent_id.is_valid_node_id() {
                        if state_staged
                            .node_has_dirty_children(repository.clone(), parent_id)
                            .await
                            .forward::<ResetError>("Failed to check dirty children")?
                        {
                            break;
                        }
                        let parent_block_index = NodeBlock::index(parent_id);
                        let parent_node_index = Node::index(parent_id);
                        let parent_block = state_staged
                            .block(repository.clone(), parent_block_index)
                            .await
                            .forward::<ResetError>("Failed to get parent block")?;
                        let parent_node = parent_block.node(parent_node_index);
                        let next_parent = parent_node.parent;
                        let dirtied = {
                            let mut writer = parent_block.write();
                            writer.node(parent_node_index).clear_dirty_flags();
                            writer.mark_dirty()
                        };
                        if dirtied {
                            state_staged.block_modified(parent_block, parent_block_index);
                            state_staged.mark_dirty();
                        }
                        if parent_id == ROOT_NODE {
                            break;
                        }
                        parent_id = next_parent;
                    }
                } else {
                    // Committed in the current revision but absent from the target;
                    // delete it so the working tree matches the target revision.
                    delete_path = true;
                }
            }

            if delete_path {
                lore_trace!("Reset removing path {}", relative_path.as_str());
                stats.file_delete_count.fetch_add(1, Ordering::Relaxed);
                event::LoreEvent::FileResetFile(LoreFileResetFileEventData {
                    path: LoreString::from(&relative_path),
                    action: LoreFileAction::Delete,
                    from_path: LoreString::default(),
                })
                .send();

                util::fs::unlink_recursive(
                    relative_path.to_absolute_path(repository.require_path()?),
                )
                .await
                .internal("Failed to remove path")?;
            }

            Ok(())
        }
        Err(err) => Err(err).forward::<ResetError>("Failed to find node"),
    }
}

/// Apply view/ignore filter, staged check, and dispatch a single child node
/// (file, directory, or link) to its appropriate handler. Files are pushed to
/// `file_tx`; directories and links recurse via `reset_walk_directory`.
async fn reset_walk_node(
    ctx: ResetContext,
    relative_path: RelativePath,
    name: String,
    node_id: u32,
    node: Node,
) -> Result<(), ResetError> {
    let ResetContext {
        repository,
        state_target,
        state_staged,
        options,
        stats,
        file_tx,
    } = ctx;

    let force = execution_context().globals().force();
    if !force
        && repository
            .filter
            .emit_excludes(&relative_path, node.is_directory(), FilterMode::Full)
    {
        lore_trace!("Path excluded by filter: {}", relative_path.as_str());
        return Ok(());
    }

    // Check whether the node is staged (applies to all kinds: file/dir/link)
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    if let Ok(staged_block) = state_staged.block(repository.clone(), block_index).await {
        let staged_node = staged_block.node(node_index);
        if staged_node.is_staged() {
            return Err(InvalidArguments {
                reason: "Failed to reset staged node".into(),
            }
            .into());
        }

        // Clear Dirty flag if set (reset restores file to current revision)
        if staged_node.is_dirty() {
            let dirtied = {
                let mut block_writer = staged_block.write();
                block_writer.node(node_index).clear_dirty_flags();
                block_writer.mark_dirty()
            };
            if dirtied {
                state_staged.block_modified(staged_block, block_index);
                state_staged.mark_dirty();
            }

            // Dirty parent cleanup
            let mut parent_id = staged_node.parent;
            while parent_id.is_valid_node_id() {
                if state_staged
                    .node_has_dirty_children(repository.clone(), parent_id)
                    .await
                    .forward::<ResetError>("Failed to check dirty children")?
                {
                    break;
                }
                let parent_block_index = NodeBlock::index(parent_id);
                let parent_node_index = Node::index(parent_id);
                let parent_block = state_staged
                    .block(repository.clone(), parent_block_index)
                    .await
                    .forward::<ResetError>("Failed to get parent block")?;
                let parent_node = parent_block.node(parent_node_index);
                let next_parent = parent_node.parent;
                let dirtied = {
                    let mut block_writer = parent_block.write();
                    block_writer.node(parent_node_index).clear_dirty_flags();
                    block_writer.mark_dirty()
                };
                if dirtied {
                    state_staged.block_modified(parent_block, parent_block_index);
                    state_staged.mark_dirty();
                }
                if parent_id == ROOT_NODE {
                    break;
                }
                parent_id = next_parent;
            }
        }
    }

    // If the node is a link, traverse into the linked repository
    if node.is_link() {
        if options.single_node {
            return Ok(());
        }

        let link = node.linked_node();
        let linked_repository = Arc::new(repository.to_link_context(link.repository).await);
        let linked_state_current =
            state::State::deserialize(linked_repository.clone(), link.revision)
                .await
                .forward::<ResetError>("Failed to deserialize revision state")?;

        let linked_state_staged = match state_staged
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
        {
            Ok(staged_link) if staged_link.repository == link.repository => {
                state::State::deserialize(linked_repository.clone(), staged_link.revision)
                    .await
                    .forward::<ResetError>("Failed to deserialize revision state")?
            }
            _ => linked_state_current.clone(),
        };

        stats.directory_reset_count.fetch_add(1, Ordering::Relaxed);

        let linked_root_node = linked_state_current
            .node(linked_repository.clone(), link.node)
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;

        return reset_walk_directory_recurse(
            ResetContext {
                repository: linked_repository,
                state_target: linked_state_current,
                state_staged: linked_state_staged,
                options,
                stats,
                file_tx,
            },
            relative_path,
            linked_root_node,
            link.node,
        )
        .await;
    }

    // If the node is a directory, recurse
    if node.is_directory() {
        if options.single_node {
            return Ok(());
        }

        stats.directory_reset_count.fetch_add(1, Ordering::Relaxed);

        return reset_walk_directory_recurse(
            ResetContext {
                repository,
                state_target,
                state_staged,
                options,
                stats,
                file_tx,
            },
            relative_path,
            node,
            node_id,
        )
        .await;
    }

    // File: hand off to the consumer
    let item = ResetFileWorkItem {
        repository,
        state_target,
        relative_path,
        name,
        node_id,
        node,
    };
    file_tx
        .send(item)
        .await
        .map_err(|_send_err| ResetError::internal("File consumer dropped"))?;

    Ok(())
}

/// Walk the children of a directory node, dispatching each to
/// `reset_walk_node`. Subdirectory walks are gated by the `directory_inflight`
/// counter: spawn when under `RESET_DIRECTORY_MAX`, run inline (degrade to
/// sync) once over. The directory itself is created on disk first; purge
/// (when enabled) runs after all child directory tasks complete.
async fn reset_walk_directory(
    ctx: ResetContext,
    directory_path: RelativePath,
    node: Node,
    node_id: NodeID,
) -> Result<(), ResetError> {
    let ResetContext {
        repository,
        state_target,
        state_staged,
        options,
        stats,
        file_tx,
    } = ctx;
    lore_trace!("Resetting directory: {}", directory_path.as_str());

    let mut child_node_iter = node.child();

    // Empty directory in revision: must be created explicitly because no
    // realize_file inside it will lazily create it. For non-empty
    // directories, parent-dir creation happens in `realize_file` (the
    // consumer side), matching clone's behaviour.
    if child_node_iter.is_none() {
        if !directory_path.is_empty() {
            let absolute_path = directory_path.to_absolute_path(repository.require_path()?);
            match tokio::fs::create_dir(absolute_path.as_path()).await {
                Ok(_) => {
                    lore_trace!("Created empty directory: {}", directory_path.as_str());
                }
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::AlreadyExists {
                        lore_trace!("Directory already exists: {}", directory_path.as_str());
                    } else if let Ok(metadata) = tokio::fs::metadata(absolute_path.as_path()).await
                    {
                        if metadata.is_dir() {
                            lore_trace!("Directory already exists: {}", directory_path.as_str());
                        } else {
                            return Err(err).internal("Failed to create directory")?;
                        }
                    } else {
                        return Err(err).internal("Failed to create directory")?;
                    }
                }
            }
        }
        return Ok(());
    }

    let mut child_dirs: JoinSet<Result<(), ResetError>> = JoinSet::new();
    let mut node_children_names: Vec<String> = vec![];
    let mut failure: Option<ResetError> = None;
    let mut cycle = SiblingCycleGuard::new(node_id);

    while let Some(child_node_id) = child_node_iter {
        let child_node_name = state_target
            .node_name_clone(repository.clone(), child_node_id)
            .await
            .forward::<ResetError>("Failed to get node name")?;

        let child_node_path = directory_path.join(&child_node_name);
        node_children_names.push(child_node_name.clone());

        let Ok(child_node) = state_target.node(repository.clone(), child_node_id).await else {
            failure = Some(ResetError::internal(
                "Failed deserializing state node block",
            ));
            break;
        };

        if let Err(err) = child_node
            .walk_step(child_node_id, node_id, &mut cycle)
            .forward::<ResetError>("Invalid node hierarchy in reset walk")
        {
            failure = Some(err);
            break;
        }

        if child_node.is_directory() || child_node.is_link() {
            // Directory/link work recurses; gate via directory_inflight.
            let inflight = stats.directory_inflight.fetch_add(1, Ordering::Relaxed);

            let task_ctx = ResetContext {
                repository: repository.clone(),
                state_target: state_target.clone(),
                state_staged: state_staged.clone(),
                options,
                stats: stats.clone(),
                file_tx: file_tx.clone(),
            };
            let task_path = child_node_path;
            let task_name = child_node_name;
            let task_node = child_node;

            let future = async move {
                let task_stats = task_ctx.stats.clone();
                let result =
                    reset_walk_node(task_ctx, task_path, task_name, child_node_id, task_node).await;
                task_stats
                    .directory_inflight
                    .fetch_sub(1, Ordering::Relaxed);
                result
            };

            if inflight as usize > RESET_DIRECTORY_MAX {
                if let Err(err) = future.await {
                    failure = Some(err);
                    break;
                }
            } else {
                lore_spawn!(child_dirs, future);
            }
        } else {
            // File / other leaf: walking is cheap (filter + staged + push to channel).
            let result = reset_walk_node(
                ResetContext {
                    repository: repository.clone(),
                    state_target: state_target.clone(),
                    state_staged: state_staged.clone(),
                    options,
                    stats: stats.clone(),
                    file_tx: file_tx.clone(),
                },
                child_node_path,
                child_node_name,
                child_node_id,
                child_node,
            )
            .await;
            if let Err(err) = result {
                failure = Some(err);
                break;
            }
        }

        child_node_iter = child_node.sibling();
    }

    while let Some(result) = child_dirs.join_next().await {
        let inner = result
            .internal("Recursion task failed")
            .map_err(ResetError::from)
            .flatten();
        failure = failure.or(inner.err());
    }

    if let Some(err) = failure {
        return Err(err);
    }

    if !options.purge {
        return Ok(());
    }

    // Find all filesystem children and check whether they have been reset,
    // otherwise remove the path. The directory may not exist on disk if all
    // its tracked children were filter-excluded — nothing to purge in that
    // case.
    let absolute_dir = directory_path.to_absolute_path(repository.require_path()?);
    if tokio::fs::metadata(&absolute_dir).await.is_err() {
        return Ok(());
    }
    let mut filesystem_children = util::fs::list_directory(absolute_dir).internal(&format!(
        "Failed to list directory files in {}",
        directory_path.as_str()
    ))?;

    let force = execution_context().globals().force();
    let mut tasks = JoinSet::new();
    while let Some(filesystem_child) = filesystem_children.recv().await {
        if filesystem_child.name == DOT_URC || filesystem_child.name == DOT_LORE {
            continue;
        }

        let child_path = directory_path.join(&filesystem_child.name);

        if !force
            && repository.filter.emit_excludes(
                &child_path,
                filesystem_child.metadata.is_dir(),
                FilterMode::Full,
            )
        {
            lore_trace!("Path excluded by filter: {}", child_path.as_str());
            continue;
        }

        if !node_children_names.contains(&filesystem_child.name) {
            lore_trace!(
                "Child node {} not found, removing path from disk",
                filesystem_child.name.as_str()
            );

            stats.directory_delete_count.fetch_add(1, Ordering::Relaxed);

            let absolute_path = child_path.to_absolute_path(repository.require_path()?);
            lore_spawn!(tasks, async move {
                util::fs::unlink_recursive(absolute_path.as_path()).await
            });
        }
    }

    while let Some(result) = tasks.join_next().await {
        result
            .internal("Recursion task failed")
            .map_err(ResetError::from)?
            .internal("Failed to remove invalid node")?;
    }

    Ok(())
}

/// Drain the file work channel and run per-file reset work. Each task
/// `acquire_owned()`s a permit from `file_inflight` before being spawned;
/// the permit is dropped when the task completes, which back-pressures the
/// loop and ultimately the directory walker producing the items. Permits
/// grow with queue backlog up to `RESET_FILE_MAX`, mirroring clone.
async fn reset_file_consume(
    mut rx: mpsc::Receiver<ResetFileWorkItem>,
    stats: Arc<ResetStats>,
) -> Result<(), ResetError> {
    let mut tasks: JoinSet<Result<(), ResetError>> = JoinSet::new();
    let mut current_permits = stats.file_inflight.available_permits();
    let mut consume_error: Option<ResetError> = None;

    while let Some(item) = rx.recv().await {
        let target =
            (rx.len() + 1 + RESET_FILE_DISCOVERY).clamp(RESET_FILE_DISCOVERY, RESET_FILE_MAX);
        if target > current_permits {
            stats.file_inflight.add_permits(target - current_permits);
            current_permits = target;
        }

        let permit = Arc::clone(&stats.file_inflight)
            .acquire_owned()
            .await
            .expect("file_inflight semaphore closed unexpectedly");

        let task_stats = stats.clone();
        lore_spawn!(tasks, async move {
            let _permit = permit;
            task_stats
                .file_inflight_count
                .fetch_add(1, Ordering::Relaxed);
            let result = reset_file_realize(item, task_stats.clone()).await;
            task_stats
                .file_inflight_count
                .fetch_sub(1, Ordering::Relaxed);
            result
        });

        while let Some(result) = tasks.try_join_next() {
            let inner = result
                .internal("File task failed")
                .map_err(ResetError::from)
                .flatten();
            consume_error = consume_error.or(inner.err());
        }

        if consume_error.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        let inner = result
            .internal("File task failed")
            .map_err(ResetError::from)
            .flatten();
        consume_error = consume_error.or(inner.err());
    }

    if let Some(err) = consume_error {
        return Err(err);
    }

    Ok(())
}

/// Per-file reset work: stat, modified-check (vs. node hash), case-rename if
/// needed, otherwise realize via `sync::realize_file`. Filter and staged
/// checks were already done by the walker.
async fn reset_file_realize(
    item: ResetFileWorkItem,
    stats: Arc<ResetStats>,
) -> Result<(), ResetError> {
    let ResetFileWorkItem {
        repository,
        state_target,
        relative_path,
        name,
        node_id,
        node,
    } = item;

    let node_path = relative_path.to_absolute_path(repository.require_path()?);
    let metadata = tokio::fs::metadata(&node_path).await;

    let force = execution_context().globals().force();

    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    if !force && let Ok(file_metadata) = metadata {
        let (mtime, size) = crate::util::fs::file_mtime_and_size(&file_metadata);
        let (file_modified, _) = state::is_file_modified(
            repository.clone(),
            &node,
            mtime,
            size,
            &relative_path,
            true, /* Force hash check */
        )
        .await
        .forward::<ResetError>("Failed to check whether file changed")?;

        if !file_modified {
            let block = state_target
                .block_with_nametable(repository.clone(), block_index)
                .await
                .forward::<ResetError>("Failed deserializing state node block")?;
            let node_name = block
                .node_name_ref(node_index)
                .forward::<ResetError>("Failed to get node name")?;
            if *name != *node_name {
                lore_trace!(
                    "Node case variation in file system detected renaming, {} -> {}",
                    name,
                    node_name
                );
                let mut buf = relative_path.clone().into_buf();
                buf.pop();
                let to_path = buf.push_and_freeze(node_name);

                stats.file_reset_count.fetch_add(1, Ordering::Relaxed);
                event::LoreEvent::FileResetFile(LoreFileResetFileEventData {
                    path: to_path.as_str().into(),
                    action: LoreFileAction::Move,
                    from_path: relative_path.as_str().into(),
                })
                .send();

                let to_path = to_path.to_absolute_path(repository.require_path()?);
                lore_spawn_blocking!(move || {
                    util::fs::unify_name_case_rename(node_path.as_path(), to_path.as_path())
                })
                .await
                .map_err(std::io::Error::other)
                .flatten()
                .internal("Failed renaming file")?;
            } else {
                lore_trace!("File {relative_path} is not modified, no reset");
            }
            return Ok(());
        }
    }

    lore_trace!(
        "Recovering node {name} with path {}",
        relative_path.as_str()
    );

    stats.file_reset_count.fetch_add(1, Ordering::Relaxed);
    event::LoreEvent::FileResetFile(LoreFileResetFileEventData {
        path: relative_path.as_str().into(),
        action: LoreFileAction::Keep,
        from_path: LoreString::default(),
    })
    .send();

    sync::realize_file(
        repository.clone(),
        &relative_path,
        node,
        Arc::new(SyncRealizeStats::default()),
    )
    .await
    .internal("Unable to restore path to selected state")?;

    Ok(())
}
