// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::branch;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::file::stage::route_layer_paths;
use crate::file::unstage;
use crate::file::unstage::UnstageOptions;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::create_empty_directory;
use crate::fs::filesystem_provider::with_operation;
use crate::hash::hash_string;
use crate::hash::hash_string_bytes;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::layer;
use crate::link;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore_debug;
use crate::lore_limit_drain_tasks;
use crate::lore_trace;
use crate::node;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::resolve_user_paths;
use crate::progress::DEFAULT_WORK_CHANNEL_CAPACITY;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::repository::is_reserved_node_name;
use crate::revision;
use crate::revision::sync::SyncRealizeStats;
use crate::runtime::execution_context;
use crate::state;
use crate::state::Clear;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;

/// Data for the event emitted when a reset operation begins.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetBeginEventData {
    /// Number of paths requested for reset.
    pub path_count: usize,
}

/// Running counts of items processed during a reset operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetProgressEventData {
    /// Current counts of items processed.
    pub count: LoreFileResetCountData,
}

/// Data for the event emitted when a reset operation completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileResetEndEventData {
    /// Final counts of items processed.
    pub count: LoreFileResetCountData,
}

/// Data for the event emitted for each file affected by a reset operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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

    /// The mounts a purge leaves to each layer's own walk, none where the reset does not purge.
    layer_mounts: Vec<RelativePath>,

    /// The dirty adds the walks drop, discarded once every walk has drained, each with the
    /// directories marked dirty as added it leaves empty.
    discards: state::WalkDiscards,
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
            layer_mounts: Vec::new(),
            discards: state::WalkDiscards::emptying_adds(),
        }
    }
}

impl ResetStats {
    /// The tally a removed path counts against, the two being reported apart.
    fn delete_count(&self, is_directory: bool) -> &AtomicU64 {
        if is_directory {
            &self.directory_delete_count
        } else {
            &self.file_delete_count
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ResetOptions {
    /// Delete what the target revision's view-filtered tree does not hold, whatever the
    /// ignore file says, but for nested repositories
    pub purge: bool,
    /// Single node, no recursion
    pub single_node: bool,
}

/// Which side of a merge a reset to the last merge point restores.
///
/// A merge revision holds the content its conflicts were resolved with, so the side that lost
/// a resolution survives only as one of the merge's two parents. `Resolved` is zero so a
/// caller that names no side restores the merge revision itself.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ResetMergeSide {
    /// Default, restore the merge revision, whichever side its conflicts were resolved with
    #[default]
    Resolved = 0,
    /// Restore `parent[0]`, the content the branch being reset held going into the merge ("mine")
    ParentSelf = 1,
    /// Restore `parent[1]`, the content the merged branch brought in ("theirs")
    ParentOther = 2,
}

impl ResetMergeSide {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::ParentSelf,
            2 => Self::ParentOther,
            _ => Self::Resolved,
        }
    }
}

/// Shared context passed through reset walk functions.
#[derive(Clone)]
struct ResetContext {
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_target: Arc<State>,
    state_staged: Arc<State>,
    options: ResetOptions,
    stats: Arc<ResetStats>,
    file_tx: mpsc::Sender<ResetFileWorkItem>,
}

/// The revision a reset takes each path back to: one for every path, or the point each path
/// was last merged from a branch, which is resolved per path.
enum ResetTarget {
    Revision(Arc<State>),
    LastMergedFrom {
        state_current: Arc<State>,
        branch: BranchId,
        branch_point: Hash,
        merge_side: ResetMergeSide,
    },
}

impl ResetTarget {
    /// The tree `path` is reset against.
    async fn state_for(
        &self,
        repository: &Arc<RepositoryContext>,
        path: &RelativePath,
    ) -> Result<Arc<State>, ResetError> {
        match self {
            ResetTarget::Revision(state) => Ok(state.clone()),
            // Find the start state where the node exists, then iterate its history back from
            // there to the last point it was merged from the branch.
            ResetTarget::LastMergedFrom {
                state_current,
                branch,
                branch_point,
                merge_side,
            } => {
                resolve_last_merged_target(
                    repository.clone(),
                    state_current.clone(),
                    *branch,
                    *branch_point,
                    *merge_side,
                    path,
                )
                .await
            }
        }
    }
}

/// What walking every path a reset names needs.
struct ResetWalk {
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    target: ResetTarget,
    state_staged: Arc<State>,
    paths: Vec<RelativePath>,
    layer_walks: Vec<ResetLayerWalk>,
    options: ResetOptions,
    stats: Arc<ResetStats>,
    file_tx: mpsc::Sender<ResetFileWorkItem>,
}

/// Walk each path in turn, handing every file it reaches to the consumer, and discard the dirty
/// adds the walks dropped, also after a failure.
async fn reset_walk_each_path(walk: ResetWalk) -> Option<ResetError> {
    let failure = reset_walk_paths(&walk).await;
    let discarded = walk
        .stats
        .discards
        .apply()
        .await
        .forward::<ResetError>("Discarding the dirty adds the reset dropped")
        .err();
    failure.or(discarded)
}

/// Walk each path in turn, reporting the first failure, having stopped at it: a path the user
/// named and a target that cannot be resolved are both refusals, and the paths after one are
/// left alone rather than half-applied. A path that does not name anything inside the
/// repository is reported ignored and skipped.
async fn reset_walk_paths(walk: &ResetWalk) -> Option<ResetError> {
    for relative_path in walk.paths.iter() {
        let walked = match walk.target.state_for(&walk.repository, relative_path).await {
            Ok(state_target) => {
                reset_walk_path(
                    ResetContext {
                        operation: walk.operation.clone(),
                        repository: walk.repository.clone(),
                        state_target,
                        state_staged: walk.state_staged.clone(),
                        options: walk.options,
                        stats: walk.stats.clone(),
                        file_tx: walk.file_tx.clone(),
                    },
                    relative_path.clone(),
                    relative_path.clone(),
                )
                .await
            }
            Err(err) => Err(err),
        };

        if let Err(err) = walked {
            return wrap_path_error(&walk.operation, relative_path, err)
                .await
                .err();
        }
    }

    for layer in walk.layer_walks.iter() {
        if let Err(err) = clear_for_directory(&walk.operation, &walk.stats, &layer.mount).await {
            return wrap_path_error(&walk.operation, &layer.mount, err)
                .await
                .err();
        }
        for (mount_path, state_path) in layer.paths.iter() {
            let walked = reset_walk_path(
                ResetContext {
                    operation: walk.operation.clone(),
                    repository: layer.state.repository.clone(),
                    state_target: layer.state.state_current.clone(),
                    state_staged: layer.state.state_staged.clone(),
                    options: walk.options,
                    stats: walk.stats.clone(),
                    file_tx: walk.file_tx.clone(),
                },
                mount_path.clone(),
                state_path.clone(),
            )
            .await;

            if let Err(err) = walked {
                return wrap_path_error(&walk.operation, mount_path, err)
                    .await
                    .err();
            }
        }
    }

    None
}

/// Deletes what stands at `path` where it is no directory, so the walk can put a directory back
/// there, as a reset deletes a directory standing where it restores a file. The delete is boxed: a
/// walk meets it only where the working tree replaced a directory.
async fn clear_for_directory(
    operation: &InstanceOperationImpl,
    stats: &Arc<ResetStats>,
    path: &RelativePath,
) -> Result<(), ResetError> {
    match operation.file_info(path).await {
        Ok(info) if info.exists() && !info.is_dir() => {
            Box::pin(reset_delete_path(operation, stats, path, false, false)).await
        }
        _ => Ok(()),
    }
}

fn count_data(stats: &ResetStats) -> LoreFileResetCountData {
    LoreFileResetCountData {
        directory_reset_count: stats.directory_reset_count.load(Ordering::Relaxed),
        directory_delete_count: stats.directory_delete_count.load(Ordering::Relaxed),
        file_reset_count: stats.file_reset_count.load(Ordering::Relaxed),
        file_delete_count: stats.file_delete_count.load(Ordering::Relaxed),
    }
}

/// A routed layer's states, its mount, and its paths paired as (on disk under the mount, in the
/// layer's tree under `source_path`).
#[derive(Clone)]
struct ResetLayerWalk {
    layer: layer::Layer,
    state: Arc<layer::LayerState>,
    mount: RelativePath,
    paths: Vec<(RelativePath, RelativePath)>,
}

/// The shared state of a reset over `layers`, holding their mounts where the reset purges.
fn reset_stats(layers: &[layer::Layer], options: ResetOptions) -> Result<ResetStats, ResetError> {
    let layer_mounts = if options.purge {
        layers
            .iter()
            .map(|layer| {
                RelativePath::new_from_initial_path(&layer.target_path)
                    .forward_with::<ResetError, _>(|| {
                        format!("Invalid layer target path {}", layer.target_path)
                    })
            })
            .collect::<Result<_, _>>()?
    } else {
        Vec::new()
    };
    Ok(ResetStats {
        layer_mounts,
        ..Default::default()
    })
}

async fn resolve_layer_walks(
    repository: Arc<RepositoryContext>,
    layers: &[layer::Layer],
    layer_jobs: &[(usize, Vec<RelativePath>)],
) -> Result<Vec<ResetLayerWalk>, ResetError> {
    let mut walks = Vec::with_capacity(layer_jobs.len());
    for (layer_index, remains) in layer_jobs {
        let layer = &layers[*layer_index];

        let mount_path = RelativePath::new_from_initial_path(&layer.target_path)
            .forward_with::<ResetError, _>(|| {
                format!("Invalid layer target path {}", layer.target_path)
            })?;
        let source_path = RelativePath::new_from_initial_path(&layer.source_path)
            .forward_with::<ResetError, _>(|| {
                format!("Invalid layer source path {}", layer.source_path)
            })?;

        let state = layer
            .deserialize_current_and_staged(repository.clone())
            .await
            .forward::<ResetError>("Failed to deserialize layer state")?;

        let paths = remains
            .iter()
            .map(|remain| {
                (
                    mount_path.join(remain.as_str()),
                    source_path.join(remain.as_str()),
                )
            })
            .collect();
        walks.push(ResetLayerWalk {
            layer: layer.clone(),
            state: Arc::new(state),
            mount: mount_path,
            paths,
        });
    }
    Ok(walks)
}

async fn store_layer_walk_states(
    repository: Arc<RepositoryContext>,
    walks: &[ResetLayerWalk],
) -> Result<(), ResetError> {
    if execution_context().globals().dry_run() {
        return Ok(());
    }

    for walk in walks {
        // An untouched staged state means the walk found nothing to clear, so the layer's
        // existing pin still describes work this reset did not address.
        if !walk.state.state_staged.is_dirty() {
            continue;
        }

        let token = repository
            .try_write_token()
            .expect("reset requires write access");

        layer::store_staged_or_clear(repository.clone(), token, &walk.layer, &walk.state)
            .await
            .forward::<ResetError>("Failed to store layer staged state")?;
    }

    Ok(())
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
    let relative_paths = resolve_user_paths(&repository, &paths).await?;

    let layers = layer::list(repository.clone())
        .await
        .forward::<ResetError>("Failed to list layers")?;
    let (parent_paths, layer_jobs) = route_layer_paths(&layers, relative_paths);

    // A revision spec names a revision of this repository, which says nothing about which
    // revision of a layer to restore. Silently substituting the layer's pin would reset a
    // path to something other than what was asked for.
    if !revision.is_empty()
        && let Some((layer_index, _)) = layer_jobs.first()
    {
        return Err(InvalidArguments {
            reason: format!(
                "Unable to reset a path in layer {} to an explicit revision",
                layers[*layer_index].target_path
            ),
        }
        .into());
    }

    let reset_link_count =
        reset_staged_links_under_paths(repository.clone(), token, &paths).await?;

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());
    let state_current_revision = state_current.revision();

    let state_target = if revision.is_empty() {
        state_current
    } else {
        let revision_spec = revision.to_string();
        let resolved = revision::resolve(
            repository.clone(),
            revision.as_str(),
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|err| {
            forward_refusal_or(err, "resolving the target revision", |err| {
                ResetError::RevisionNotFound(
                    RevisionNotFound {
                        revision: revision_spec,
                    }
                    .chain_err_from(err, "target revision not found"),
                )
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

    let layer_walks = resolve_layer_walks(repository.clone(), &layers, &layer_jobs).await?;

    let stats = Arc::new(reset_stats(&layers, options)?);
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
    let producer_layer_walks = layer_walks.clone();

    let target_revision = state_target.revision();
    let modified_times = Arc::new(crate::state::RecordedModifiedTimes::default());
    // One operation covers the whole reset: it resolves the case each path is held in and
    // writes every file, and one opened per file would freeze a filesystem per file.
    let result = with_operation(repository.file_system(), async |operation| {
        let producer_operation = operation.clone();
        let walked = run_reset_pipeline(operation.clone(), stats.clone(), |file_tx| async move {
            reset_walk_each_path(ResetWalk {
                operation: producer_operation,
                repository: producer_repository,
                target: ResetTarget::Revision(state_target),
                state_staged,
                paths: parent_paths,
                layer_walks: producer_layer_walks,
                options,
                stats: producer_stats,
                file_tx,
            })
            .await
        })
        .await;
        modified_times.absorb(operation.take_modified_times());
        walked
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
        if !outer_state_staged
            .node_has_staged_or_dirty_children(repository.clone(), ROOT_NODE)
            .await
            .forward::<ResetError>("Failed deserializing state node block")?
        {
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

    store_layer_walk_states(repository.clone(), &layer_walks).await?;

    // Only a reset to the revision the working copy is on leaves files holding what that
    // revision addresses; a reset to any other revision states nothing about it.
    if target_revision == state_current_revision {
        modified_times.store(repository.clone()).await;
    } else {
        modified_times.discard();
    }

    result
}

/// Resets files to the state they were in at the last merged revision on a branch.
///
/// `merge_side` picks which side of that merge to restore, which matters only where the merge
/// held a conflict: the merge revision holds what the conflict was resolved with, and the other
/// side survives only as a parent of it. Where no merge from the branch is found the branch
/// point is restored whatever `merge_side` names, the branch having contributed nothing else.
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
    merge_side: ResetMergeSide,
    options: ResetOptions,
) -> Result<(), ResetError> {
    if branch.is_empty() {
        lore_debug!("Cannot reset to last merged without a branch");
        return Err(BranchNotFound {
            branch: String::new(),
        }
        .into());
    }

    let paths = resolve_user_paths(&repository, &paths).await?;

    let (state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<ResetError>("Failed to deserialize revision state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    let branch_name = branch.to_string();
    let branch = match branch::resolve(repository.clone(), branch.as_str()).await {
        Ok(resolved) => resolved,
        Err(err) if is_refusal(&err) => {
            return Err(err).forward_any("resolving branch for last-merged reset");
        }
        Err(err) => {
            return Err(ResetError::BranchNotFound(
                BranchNotFound {
                    branch: branch_name.clone(),
                }
                .chain_err_from(err, "branch for last-merged reset not found"),
            ));
        }
    };

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
        .forward::<ResetError>("Failed to load branch metadata")?;
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

    let stats = Arc::new(reset_stats(
        &layer::list(repository.clone())
            .await
            .forward::<ResetError>("Failed to list layers")?,
        options,
    )?);

    event::LoreEvent::FileResetBegin(LoreFileResetBeginEventData {
        path_count: paths.len(),
    })
    .send();

    let outer_stats = stats.clone();
    let producer_repository = repository.clone();
    let producer_stats = stats.clone();
    let branch_id = branch.id;

    let modified_times = Arc::new(crate::state::RecordedModifiedTimes::default());
    // One operation covers the whole reset, as it does for a reset to a named revision.
    let result = with_operation(repository.file_system(), async |operation| {
        let producer_operation = operation.clone();
        let walked = run_reset_pipeline(operation.clone(), stats.clone(), |file_tx| async move {
            reset_walk_each_path(ResetWalk {
                operation: producer_operation,
                repository: producer_repository,
                target: ResetTarget::LastMergedFrom {
                    state_current,
                    branch: branch_id,
                    branch_point,
                    merge_side,
                },
                state_staged,
                paths,
                layer_walks: Vec::new(),
                options,
                stats: producer_stats,
                file_tx,
            })
            .await
        })
        .await;
        modified_times.absorb(operation.take_modified_times());
        walked
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
                forward_refusal_or(err, "listing staged link nodes", |err| {
                    ResetError::internal_with_context(err, "listing staged link nodes")
                })
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
                .any(|p| p.covers_ignore_case(&relative_link))
        {
            matched.push(LoreString::from(absolute));
        }
    }

    if matched.is_empty() {
        return Ok(0);
    }

    let reset_count = matched.len();
    Box::pin(unstage::unstage(
        repository,
        token,
        LoreArray::from_vec(matched),
        UnstageOptions {
            discard_local: true,
            ..Default::default()
        },
    ))
    .await
    .forward::<ResetError>("Failed to reset staged link nodes")?;

    Ok(reset_count)
}

/// Names in `err` whether the working tree holds the path it failed on, which separates a path
/// the user misspelled from one the reset could not apply.
async fn wrap_path_error(
    operation: &InstanceOperationImpl,
    relative_path: &RelativePath,
    err: ResetError,
) -> Result<(), ResetError> {
    let existence = if operation
        .file_info(relative_path)
        .await
        .is_ok_and(|info| info.exists())
    {
        "an existing"
    } else {
        "a non-existent"
    };
    Err::<(), ResetError>(err).forward_with::<ResetError, _>(|| {
        format!("Failed resetting {existence} path: {relative_path}")
    })
}

async fn resolve_last_merged_target(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    branch_id: BranchId,
    branch_point: Hash,
    merge_side: ResetMergeSide,
    relative_path: &RelativePath,
) -> Result<Arc<State>, ResetError> {
    let mut state_start = state_current;
    let node_link = absent_unless_refused::<_, _, ResetError>(
        state_start
            .find_node_link(repository.clone(), relative_path.as_str())
            .await,
        "finding the node",
    )?
    .unwrap_or_default();
    if !node_link.is_valid() {
        lore_debug!("Find state where node exist: {relative_path}");
        let revision = state_start.revision();
        if revision != branch_point {
            let parent = state_start.parent_self();
            if let Some(state) = absent_unless_refused::<_, _, ResetError>(
                find_start_state(repository.clone(), parent, branch_point, relative_path).await,
                "finding the last revision holding the node",
            )? {
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
        if let Some(state_merge) = absent_unless::<_, _, ResetError>(
            find_merge_state(
                repository.clone(),
                state_start,
                state_branch_point.clone(),
                branch_id,
                relative_path,
            )
            .await,
            |err| is_refusal(err) || err.is_not_supported(),
            "finding the last merge from the branch",
        )? {
            lore_debug!(
                "Found revision where node was merged from branch: {relative_path} in merge revision {} -> {}",
                state_merge.revision(),
                state_merge.revision_number()
            );
            // A merge revision holds the content its conflicts were resolved with, so a side
            // that lost a resolution survives only as one of the merge's parents: `parent[0]`
            // is the branch the merge was made on and `parent[1]` is the branch it merged in.
            let parent = match merge_side {
                ResetMergeSide::Resolved => None,
                ResetMergeSide::ParentSelf => Some(state_merge.parent_self()),
                ResetMergeSide::ParentOther => Some(state_merge.parent_other()),
            };

            state_target = state_merge.clone();
            if let Some(parent) = parent {
                if parent.is_zero() {
                    return Err(RevisionNotFound {
                        revision: format!("{merge_side:?} of merge {}", state_merge.revision()),
                    }
                    .into());
                }
                state_target = state::State::deserialize(repository.clone(), parent)
                    .await
                    .forward::<ResetError>("Failed to deserialize merge parent state")?;
            }

            lore_debug!(
                "Resetting {relative_path} to {merge_side:?} of that merge, revision {} -> {}",
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
async fn run_reset_pipeline<P, Fut>(
    operation: Arc<InstanceOperationImpl>,
    stats: Arc<ResetStats>,
    producer: P,
) -> Result<(), ResetError>
where
    P: FnOnce(mpsc::Sender<ResetFileWorkItem>) -> Fut,
    Fut: Future<Output = Option<ResetError>> + Send,
{
    let (file_tx, file_rx) = mpsc::channel::<ResetFileWorkItem>(DEFAULT_WORK_CHANNEL_CAPACITY);

    let consumer_stats = stats.clone();
    let mut consumer =
        lore_spawn!(async move { reset_file_consume(operation, file_rx, consumer_stats).await });

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

fn last_merged_inside_link(relative_path: &RelativePath) -> ResetError {
    NotSupported {
        operation: format!(
            "Resetting {relative_path} inside a link to its last merge from a branch"
        ),
    }
    .into()
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

        let node_link = absent_unless_refused::<_, _, ResetError>(
            state_parent
                .find_node_link(repository.clone(), relative_path.as_str())
                .await,
            "finding the node",
        )?
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
        let node_link = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .map_err(|err| {
                forward_refusal_or(err, "finding the node", |err| {
                    ResetError::internal_with_context(err, "Failed to find node")
                })
            });

        if let Ok(node_link) = &node_link
            && node_link.crosses_link(&repository, &state)
        {
            return Err(last_merged_inside_link(relative_path));
        }

        if !state.parent_other().is_zero()
            && let Some(state_merged) = absent_unless_refused::<_, _, ResetError>(
                state::State::deserialize(repository.clone(), state.parent_other()).await,
                "reading the merged-in parent",
            )?
            && absent_unless_refused::<_, _, ResetError>(
                state_merged.recorded_branch(repository.clone()).await,
                "reading the merged-in parent's branch",
            )? == Some(branch)
        {
            return Ok(state);
        }

        let node_link = node_link?;

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
    states: FilterStates,
) -> Pin<Box<dyn Future<Output = Result<(), ResetError>> + Send>> {
    Box::pin(reset_walk_directory(
        ctx,
        directory_path,
        node,
        node_id,
        states,
    ))
}

/// Drive the directory walk for a single user-supplied path.
///
/// For the repository root path this walks every child of `ROOT_NODE` through
/// the directory-inflight bounded scheme; any other path goes to
/// [`reset_walk_named_path`]. Files end up on the `file_tx` channel for the
/// consumer; directories recurse via the same scheme.
///
/// `state_path` names the path in `state_target` and `state_staged`, `relative_path` names
/// it on disk. The two differ only for a layer mounted somewhere other than the path it
/// occupies in its own repository. Only the entry lookup needs `state_path`: the walk
/// proceeds by node id from there and builds every path below it from `relative_path`.
async fn reset_walk_path(
    ctx: ResetContext,
    relative_path: RelativePath,
    state_path: RelativePath,
) -> Result<(), ResetError> {
    lore_debug!(
        "Reset path: {}/{} to revision {} -> {}",
        ctx.repository.path_for_display(),
        relative_path.as_str(),
        ctx.state_target.revision(),
        ctx.state_target.revision_number()
    );

    let (relative_path, state_path) = if relative_path.is_empty() {
        (relative_path, state_path)
    } else {
        let resolved = util::fs::filesystem_path(&ctx.operation, "", &relative_path, None).await;
        match resolved {
            // The case the filesystem reports is the case the state holds too, unless the
            // two paths were already distinct.
            Ok(resolved) if state_path.as_str() == relative_path.as_str() => {
                (resolved.clone(), resolved)
            }
            Ok(resolved) => (resolved, state_path),
            Err(_) => (relative_path, state_path),
        }
    };

    if relative_path.is_empty() {
        if ctx.options.single_node {
            return Ok(());
        }

        lore_debug!("Resetting the repository from root");

        let node = ctx
            .state_target
            .node(ctx.repository.clone(), ROOT_NODE)
            .await
            .forward::<ResetError>("Failed to find node")?;

        return reset_walk_directory(ctx, relative_path, node, ROOT_NODE, FilterStates::ROOT).await;
    }

    reset_walk_named_path(ctx, relative_path, state_path).await
}

/// Resolves a path below the root to a node and walks it through `reset_walk_node`, or settles
/// a path the target revision does not hold against the staged state and the working tree.
///
/// Its own future: inline, what it holds across its lookups would be reserved in the root
/// walk's state of [`reset_walk_path`].
async fn reset_walk_named_path(
    ctx: ResetContext,
    relative_path: RelativePath,
    state_path: RelativePath,
) -> Result<(), ResetError> {
    let ResetContext {
        operation,
        repository,
        state_target,
        state_staged,
        options,
        stats,
        file_tx,
    } = ctx;

    let parent_states = repository.filter.parent_exclusion_states(&relative_path);

    let node_name = relative_path.name().to_string();

    let found = state_target
        .find_relative_held(repository.clone(), ROOT_NODE, state_path.as_str())
        .await;

    match found {
        Ok((node_link, (target_repository, target_state_current))) => {
            let current_state_staged = if !Arc::ptr_eq(&target_state_current, &state_target) {
                refuse_staged(&state_staged, &repository, &state_path).await?;
                target_state_current.clone()
            } else {
                state_staged.clone()
            };

            let node = target_state_current
                .node(target_repository.clone(), node_link.node)
                .await
                .forward::<ResetError>("Failed to find node")?;

            let replacement = replacing_dirty_add(
                &current_state_staged,
                &target_repository,
                node_link.node,
                node.name_hash,
            )
            .await?;

            reset_walk_node(
                ResetContext {
                    operation: operation.clone(),
                    repository: target_repository.clone(),
                    state_target: target_state_current,
                    state_staged: current_state_staged.clone(),
                    options,
                    stats: stats.clone(),
                    file_tx,
                },
                relative_path,
                node_name,
                node_link.node,
                node,
                parent_states,
            )
            .await?;

            if let Some(replacement) = replacement {
                stats
                    .discards
                    .queue(current_state_staged, target_repository, vec![replacement]);
            }
            Ok(())
        }
        Err(e) if e.is_node_not_found() => {
            // A path absent from the target revision is one of: a file committed
            // in the current revision but added after the target (delete it so the
            // working tree matches the target, unless the view excludes it, which
            // only --purge deletes), a pending dirty-add, or an untracked file (keep
            // on disk unless --purge, discarding add tracking). A path inside a link is refused,
            // since its node is numbered in the linked tree, not in `state_staged`.
            let mut delete_path = options.purge;
            if let Ok(staged_link) = state_staged
                .find_node_link(repository.clone(), state_path.as_str())
                .await
            {
                if staged_link.crosses_link(&repository, &state_staged) {
                    return Err(NotSupported {
                        operation: format!(
                            "Resetting {relative_path} inside a link, which the target revision \
                             does not hold"
                        ),
                    }
                    .into());
                }
                // The flags alone, so the record is not held across the awaits below.
                let (staged, dirty_add, link, directory) = state_staged
                    .node(repository.clone(), staged_link.node)
                    .await
                    .forward::<ResetError>("Failed to find staged node")
                    .map(|node| {
                        (
                            node.is_staged(),
                            node.is_dirty_add(),
                            node.is_link(),
                            node.is_directory(),
                        )
                    })?;

                // `reset_staged_links_under_paths` unstaged this link, which a dry run does not
                // store, so the link is still staged here.
                if link && execution_context().globals().dry_run() {
                    return Ok(());
                }
                if staged {
                    return Err(staged_node_refusal());
                }

                if dirty_add {
                    lore_trace!(
                        "Reset dirty add {}, discarding staged node and keeping the file",
                        relative_path.as_str()
                    );
                    stats.discards.queue(
                        state_staged.clone(),
                        repository.clone(),
                        vec![staged_link.node],
                    );
                } else if !delete_path {
                    let (_, excluded) = repository.filter.child_emit_unless_forced(
                        execution_context().globals().force(),
                        FilterMode::empty(),
                        parent_states,
                        &relative_path,
                        directory,
                        FilterMode::View,
                    );
                    delete_path = !excluded;
                }
            }

            if delete_path {
                Box::pin(purge_path(&operation, &stats, &repository, &relative_path)).await?;
            }

            Ok(())
        }
        Err(err) => Err(err).forward::<ResetError>("Failed to find node"),
    }
}

/// Refuses `state_path` where the staged tree holds it as staged, read through the staged tree's
/// own links: inside a link, the staged revision can number its nodes other than the target's pin.
async fn refuse_staged(
    state_staged: &State,
    repository: &Arc<RepositoryContext>,
    state_path: &RelativePath,
) -> Result<(), ResetError> {
    let (staged_link, held) = match state_staged
        .find_relative(repository.clone(), ROOT_NODE, state_path.as_str())
        .await
    {
        Ok(found) => found,
        Err(err) if err.is_node_not_found() => return Ok(()),
        Err(err) => return Err(err).forward::<ResetError>("Failed to find staged node"),
    };
    let staged = state_staged
        .found_node(repository.clone(), &staged_link, held)
        .await
        .forward::<ResetError>("Failed to find staged node")?
        .is_staged();
    if staged {
        return Err(staged_node_refusal());
    }
    Ok(())
}

/// The refusal of a reset that would discard a staged change.
fn staged_node_refusal() -> ResetError {
    InvalidArguments {
        reason: "Failed to reset staged node".into(),
    }
    .into()
}

/// The dirty add standing beside `node_id` in `state_staged` under the same name, which took its
/// place on disk. Only a deleted node is replaced, and the add is linked after it, so it lies
/// ahead of `node_id` in the chain.
async fn replacing_dirty_add(
    state_staged: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    name_hash: u64,
) -> Result<Option<NodeID>, ResetError> {
    let Ok(staged_node) = state_staged.node(repository.clone(), node_id).await else {
        return Ok(None);
    };
    if !staged_node.is_dirty_delete() {
        return Ok(None);
    }
    let Some(sibling) = state_staged
        .find_subnode_ahead_of(repository.clone(), staged_node.parent, node_id, name_hash)
        .await
        .forward::<ResetError>("Failed to search the staged siblings")?
    else {
        return Ok(None);
    };
    let sibling_node = state_staged
        .node(repository.clone(), sibling)
        .await
        .forward::<ResetError>("Failed to find staged node")?;
    Ok(sibling_node.is_dirty_add().then_some(sibling))
}

/// `dirty_adds` is sorted by name hash.
fn replaced_dirty_adds(
    dirty_adds: &[(NodeID, u64)],
    node_id: NodeID,
    name_hash: u64,
) -> impl Iterator<Item = NodeID> + '_ {
    let start = dirty_adds.partition_point(|&(_, hash)| hash < name_hash);
    dirty_adds[start..]
        .iter()
        .take_while(move |&&(_, hash)| hash == name_hash)
        .filter(move |&&(id, _)| id != node_id)
        .map(|&(id, _)| id)
}

/// Removes `relative_path` from the working tree, counted against the tally `is_directory`
/// names it for. `keep_repositories` keeps each nested repository below it, with the directories
/// leading to one.
async fn reset_delete_path(
    operation: &InstanceOperationImpl,
    stats: &Arc<ResetStats>,
    relative_path: &RelativePath,
    is_directory: bool,
    keep_repositories: bool,
) -> Result<(), ResetError> {
    lore_trace!("Reset removing path {}", relative_path.as_str());
    stats
        .delete_count(is_directory)
        .fetch_add(1, Ordering::Relaxed);
    event::LoreEvent::FileResetFile(LoreFileResetFileEventData {
        path: LoreString::from(relative_path),
        action: LoreFileAction::Delete,
        from_path: LoreString::default(),
    })
    .send();

    let removed = if keep_repositories {
        operation
            .remove_recursive_keeping_repositories(relative_path)
            .await
    } else {
        operation.remove_recursive(relative_path).await
    };
    removed.forward::<ResetError>("Failed to remove path")?;
    Ok(())
}

/// Removes what the working tree holds at `relative_path`, as [`reset_delete_path`] does, but
/// for a nested repository, which is left in place at any depth with the directories leading to
/// it, and a layer mount: a mount is left to the layer's own walk, and a directory on the way to
/// one is purged around it. A path below a mount is left as it is where `repository` is not a
/// layer, as a walk of the parent repository judges nothing the layer holds; the layer's own walk
/// purges it. A path the working tree does not hold is left as it is.
async fn purge_path(
    operation: &Arc<InstanceOperationImpl>,
    stats: &Arc<ResetStats>,
    repository: &RepositoryContext,
    relative_path: &RelativePath,
) -> Result<(), ResetError> {
    let mount = stats.layer_mounts.iter().find(|mount| {
        relative_path.covers_ignore_case(mount)
            || (!repository.is_layer() && mount.covers_ignore_case(relative_path))
    });
    let around_mount = match mount {
        Some(mount) if relative_path.len() < mount.len() => true,
        Some(_) => return Ok(()),
        None => false,
    };
    let Ok(info) = operation.file_info(relative_path).await else {
        return Ok(());
    };
    if !info.exists() {
        return Ok(());
    }
    if info.is_dir() {
        let repository_root = repository.require_path()?;
        if around_mount {
            return Box::pin(purge_untracked_children(
                operation,
                stats,
                repository_root,
                relative_path,
                None,
            ))
            .await;
        }
        let mut parent = relative_path
            .parent_path()
            .to_absolute_path(repository_root)
            .forward::<ResetError>("placing the purged path's parent below the root")?;
        if state::is_nested_repository_root(&mut parent, relative_path.name()).await {
            lore_trace!("Leaving nested repository {relative_path} in place");
            return Ok(());
        }
    }
    reset_delete_path(operation, stats, relative_path, info.is_dir(), true).await
}

/// Apply the view filter, staged check, and dispatch a single child node
/// (file, directory, or link) to its appropriate handler. Files are pushed to
/// `file_tx`; directories and links recurse via `reset_walk_directory`.
/// `parent_states` is the filter verdict for the directory holding
/// `relative_path`, which this node's own query steps from. A reset asks the view
/// alone, and a purge deletes a node the view excludes, boxed, so a walk that does
/// not purge holds no room for it.
async fn reset_walk_node(
    ctx: ResetContext,
    relative_path: RelativePath,
    name: String,
    node_id: u32,
    node: Node,
    parent_states: FilterStates,
) -> Result<(), ResetError> {
    let ResetContext {
        operation,
        repository,
        state_target,
        state_staged,
        options,
        stats,
        file_tx,
    } = ctx;

    let force = execution_context().globals().force();
    let (states, excluded) = repository.filter.child_emit_unless_forced(
        force,
        FilterMode::empty(),
        parent_states,
        &relative_path,
        node.is_directory(),
        FilterMode::View,
    );
    if excluded {
        lore_trace!("Path excluded by filter: {}", relative_path.as_str());
        if options.purge {
            Box::pin(purge_path(&operation, &stats, &repository, &relative_path)).await?;
        }
        return Ok(());
    }

    // Check whether the node is staged (applies to all kinds: file/dir/link)
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    // The flags alone, so neither the block nor the record is held across the clear below.
    let staged_flags = state_staged
        .block(repository.clone(), block_index)
        .await
        .ok()
        .map(|staged_block| {
            let staged_node = staged_block.node(node_index);
            (staged_node.is_staged(), staged_node.is_dirty())
        });
    if let Some((staged, dirty)) = staged_flags {
        if staged {
            return Err(staged_node_refusal());
        }

        // Clear Dirty flag if set (reset restores file to current revision), then a dirty clear's
        // climb clears each parent that nothing below keeps dirty, up to one already clean.
        if dirty {
            state_staged
                .node_clear(&repository, node_id, Clear::Dirty)
                .await
                .forward::<ResetError>("Failed to clear the dirty flags")?;
        }
    }

    // If the node is a link, traverse into the linked repository
    if node.is_link() {
        if options.single_node {
            return Ok(());
        }

        let link = node.linked_node();
        let linked_repository = repository.to_link_context(link.repository).await;
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

        let link_states = linked_repository.filter.mount_states(&relative_path);

        return reset_walk_directory_recurse(
            ResetContext {
                operation: operation.clone(),
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
            link_states,
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
                operation: operation.clone(),
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
            states,
        )
        .await;
    }

    // File: hand off to the consumer
    let permit = file_tx
        .reserve()
        .await
        .map_err(|_closed| ResetError::internal("File consumer dropped"))?;
    permit.send(ResetFileWorkItem {
        repository,
        state_target,
        relative_path,
        name,
        node_id,
        node,
    });

    Ok(())
}

/// Walk the children of a directory node, dispatching each to
/// `reset_walk_node`. Subdirectory walks are gated by the `directory_inflight`
/// counter: spawn when under `RESET_DIRECTORY_MAX`, run inline (degrade to
/// sync) once over. A file standing at the directory's path is deleted first. A
/// directory the revision holds with no children is created
/// here unless the view excludes it, and purge (when enabled) runs for every
/// directory once its child walks have completed. The purge is boxed, so a walk
/// that does not purge holds no room for it.
async fn reset_walk_directory(
    ctx: ResetContext,
    directory_path: RelativePath,
    node: Node,
    node_id: NodeID,
    states: FilterStates,
) -> Result<(), ResetError> {
    let ResetContext {
        operation,
        repository,
        state_target,
        state_staged,
        options,
        stats,
        file_tx,
    } = ctx;
    lore_trace!("Resetting directory: {}", directory_path.as_str());

    if !directory_path.is_empty() {
        clear_for_directory(&operation, &stats, &directory_path).await?;
    }

    let mut child_node_iter = node.child();

    if child_node_iter.is_none() {
        if !directory_path.is_empty() && !states.excluded(FilterMode::View) {
            create_empty_directory::<ResetError>(&operation, &directory_path).await?;
        }
        if !options.purge {
            return Ok(());
        }
        return Box::pin(purge_untracked_children(
            &operation,
            &stats,
            repository.require_path()?,
            &directory_path,
            None,
        ))
        .await;
    }

    let mut child_dirs: JoinSet<Result<(), ResetError>> = JoinSet::new();
    let mut held: Vec<HeldChild> = vec![];
    let mut failure: Option<ResetError> = None;
    let mut cycle = SiblingCycleGuard::new(node_id);
    let mut dirty_adds = state_staged
        .node_dirty_add_children(repository.clone(), node_id)
        .await
        .forward::<ResetError>("Listing the staged children")?;
    dirty_adds.sort_unstable_by_key(|&(_, name_hash)| name_hash);
    let mut replaced = Vec::new();

    while let Some(child_node_id) = child_node_iter {
        let child_node = match state_target.node(repository.clone(), child_node_id).await {
            Ok(child_node) => child_node,
            Err(err) => {
                failure = Some(forward_refusal_or(err, "reading a node block", |err| {
                    ResetError::internal_with_context(err, "Failed deserializing state node block")
                }));
                break;
            }
        };

        if let Err(err) = child_node
            .walk_step(child_node_id, node_id, &mut cycle)
            .forward::<ResetError>("Invalid node hierarchy in reset walk")
        {
            failure = Some(err);
            break;
        }

        let child_node_name = match state_target
            .node_name_clone_or_skip(repository.clone(), child_node_id)
            .await
            .forward::<ResetError>("Failed to get node name")
        {
            Ok(Some(name)) => name,
            Ok(None) => {
                child_node_iter = child_node.sibling();
                continue;
            }
            Err(err) => {
                failure = Some(err);
                break;
            }
        };

        let child_node_path = directory_path.join(&child_node_name);
        if options.purge {
            held.push(HeldChild {
                folded: child_node.name_hash,
                exact: hash_string_bytes(child_node_name.as_bytes()),
                node: child_node_id,
            });
        }
        replaced.extend(replaced_dirty_adds(
            &dirty_adds,
            child_node_id,
            child_node.name_hash,
        ));

        if child_node.is_directory() || child_node.is_link() {
            // Directory/link work recurses; gate via directory_inflight.
            let inflight = stats.directory_inflight.fetch_add(1, Ordering::Relaxed);

            let task_ctx = ResetContext {
                operation: operation.clone(),
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
                let result = reset_walk_node(
                    task_ctx,
                    task_path,
                    task_name,
                    child_node_id,
                    task_node,
                    states,
                )
                .await;
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
                    operation: operation.clone(),
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
                states,
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
    stats
        .discards
        .queue(state_staged, repository.clone(), replaced);

    if !options.purge {
        return Ok(());
    }

    let held = HeldChildren::new(state_target, repository.clone(), held);
    Box::pin(purge_untracked_children(
        &operation,
        &stats,
        repository.require_path()?,
        &directory_path,
        Some(held),
    ))
    .await
}

/// Removes the children the working tree holds under `directory_path` that the target revision
/// does not, which is what `--purge` adds to restoring the ones it does. No filter is asked:
/// an untracked path is removed whether an ignore rule or the view matches it or not.
///
/// `held` holds the children the revision holds, each of them settled by the walk already,
/// restored or, where the view excludes it, removed, and an entry [`HeldChildren::leaves`] answers
/// for is left in place. An entry [`is_reserved_node_name`] answers for, such as `.lore` or
/// `.urc`, and a nested repository, a directory holding `.lore`, are left in place at any depth,
/// with the directories leading to them. What stands at a layer mount is left to the layer's own
/// walk, whatever its kind, and a directory on the way to one is purged around it.
///
/// Each removal runs in a task, and the tasks are drained to [`MAX_CONCURRENT_TREE_TASKS`] after
/// each spawn. The first failure, of the listing, a removal or a purge around a mount, ends the
/// listing, and it is answered once the tasks still live have been awaited.
///
/// A directory the working tree does not hold has nothing to purge.
#[lore_macro::test_pub]
async fn purge_untracked_children(
    operation: &Arc<InstanceOperationImpl>,
    stats: &Arc<ResetStats>,
    repository_root: &Path,
    directory_path: &RelativePath,
    held: Option<HeldChildren>,
) -> Result<(), ResetError> {
    if !operation
        .file_info(directory_path)
        .await
        .forward_any::<ResetError>("Failed to describe a directory to purge")?
        .is_dir()
    {
        return Ok(());
    }

    let mut filesystem_children = operation
        .read_directory(directory_path)
        .await
        .forward_any_with::<ResetError, _>(|| {
            format!("Failed to list directory files in {directory_path}")
        })?;

    let mut probe: Option<PathBuf> = None;
    let mut tasks = JoinSet::new();
    let mut failure = None;
    while let Some(filesystem_child) = filesystem_children.next().await {
        #[cfg(feature = "test-util")]
        let filesystem_child = PURGE_TASKS.entry(filesystem_child);
        let filesystem_child =
            match filesystem_child.forward_any::<ResetError>("Unusable directory entry") {
                Ok(filesystem_child) => filesystem_child,
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            };
        let name = filesystem_child.name.as_str();
        if is_reserved_node_name(name) {
            continue;
        }
        if let Some(held) = held.as_ref()
            && held
                .leaves(operation, directory_path, name, filesystem_child.name_hash)
                .await
        {
            continue;
        }

        let child_path = directory_path.join(name);
        let is_directory = filesystem_child.info.is_dir();
        if let Some(mount) = stats
            .layer_mounts
            .iter()
            .find(|mount| child_path.covers_ignore_case(mount))
            && (is_directory || mount.len() == child_path.len())
        {
            if mount.len() != child_path.len()
                && let Err(err) = Box::pin(purge_untracked_children(
                    operation,
                    stats,
                    repository_root,
                    &child_path,
                    None,
                ))
                .await
            {
                failure = Some(err);
                break;
            }
            continue;
        }
        if is_directory {
            let probe = match directory_path
                .to_absolute_path_cached(repository_root, &mut probe)
                .forward::<ResetError>("placing the purged directory below the root")
            {
                Ok(probe) => probe,
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            };
            if state::is_nested_repository_root(probe, name).await {
                lore_trace!("Leaving nested repository {child_path} in place");
                continue;
            }
        }

        lore_trace!("Child node {name} not found, removing path from disk");

        stats
            .delete_count(is_directory)
            .fetch_add(1, Ordering::Relaxed);

        let child_operation = operation.clone();
        #[cfg(feature = "test-util")]
        let live = PURGE_TASKS.enter();
        lore_spawn!(tasks, async move {
            #[cfg(feature = "test-util")]
            let _live = live;
            #[cfg(feature = "test-util")]
            PURGE_TASKS.wait_if_held().await;
            child_operation
                .remove_recursive_keeping_repositories(&child_path)
                .await
                .forward::<ResetError>("Failed to remove invalid node")
        });
        if let Err(err) = lore_limit_drain_tasks!(
            tasks,
            MAX_CONCURRENT_TREE_TASKS,
            ResetError::internal("Recursion task failed")
        ) {
            failure = Some(err);
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        let result = result
            .internal("Recursion task failed")
            .map_err(ResetError::from)
            .flatten();
        failure = failure.or(result.err());
    }

    failure.map_or(Ok(()), Err)
}

/// Counts and holds the removal tasks purges spawn, and fails a purge's listing, for a test.
#[cfg(feature = "test-util")]
pub static PURGE_TASKS: crate::test_util::TaskProbe = crate::test_util::TaskProbe::new();

/// A child the target revision holds in a directory a purge lists.
#[lore_macro::test_pub]
struct HeldChild {
    /// The hash of its name folded, which [`hash_string`] gives.
    folded: u64,
    /// The hash of its name as spelled, which [`hash_string_bytes`] gives.
    exact: u64,
    node: NodeID,
}

/// The children the target revision holds in a directory a purge lists, sorted by the folded hash
/// of their names, with the tree to read a child's name from.
#[lore_macro::test_pub]
struct HeldChildren {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    children: Vec<HeldChild>,
}

impl HeldChildren {
    #[lore_macro::test_pub]
    fn new(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        mut children: Vec<HeldChild>,
    ) -> Self {
        children.sort_unstable_by_key(|child| child.folded);
        Self {
            state,
            repository,
            children,
        }
    }

    /// Whether the purge leaves the entry `name` of `directory`, folding to `folded`: one a child
    /// [`holds`](Self::holds), or the temporary file a write in progress puts such a child in
    /// place through. The storage layer names that from the child's extension: `a.txt.~loretemp`
    /// for `a.txt`, and `a..~loretemp` for `a`, which has none.
    async fn leaves(
        &self,
        operation: &InstanceOperationImpl,
        directory: &RelativePath,
        name: &str,
        folded: u64,
    ) -> bool {
        if self.holds(operation, directory, name, name, folded).await {
            return true;
        }
        let Some(written) = name.strip_suffix(TEMP_FILE_EXTENSION) else {
            return false;
        };
        for written in std::iter::once(written).chain(written.strip_suffix('.')) {
            if self
                .holds(operation, directory, name, written, hash_string(written))
                .await
            {
                return true;
            }
        }
        false
    }

    /// Whether a child holds the entry `name` of `directory`, which stands for the name `written`
    /// its start spells, folding to `folded`: a child spelled as `written`, or one whose name it
    /// only folds to unless [`names_the_child`](Self::names_the_child) answers that the entry is
    /// another one. It is then an entry of its own, as on a file system that does not fold case,
    /// and the purge removes it beside the child the reset restores.
    async fn holds(
        &self,
        operation: &InstanceOperationImpl,
        directory: &RelativePath,
        name: &str,
        written: &str,
        folded: u64,
    ) -> bool {
        let first = self.children.partition_point(|child| child.folded < folded);
        let folding = || {
            self.children[first..]
                .iter()
                .take_while(move |child| child.folded == folded)
        };
        let exact = hash_string_bytes(written.as_bytes());
        if folding().any(|child| child.exact == exact) {
            return true;
        }
        let suffix = &name[written.len()..];
        for child in folding() {
            if Box::pin(self.names_the_child(operation, directory, name, suffix, child.node)).await
            {
                return true;
            }
        }
        false
    }

    /// Whether the purge holds the entry `name` of `directory` as the child at `node`, compared
    /// with the child's name followed by `suffix`, which is how the entry spells past the name it
    /// stands for: all but where the entry is there and that names another entry. An entry gone at
    /// the check, as a directory the reset of the child replaces is for a moment, is held, since
    /// its name may name the child once the reset has written it; so is one that cannot be told
    /// apart from the child, and one folding to a child whose name the tree cannot read. An
    /// untracked hard link to the child, under a name that folds to the child's, names the child's
    /// file and is held too.
    ///
    /// A cold path, boxed by its caller: it reads the child's name from the tree.
    async fn names_the_child(
        &self,
        operation: &InstanceOperationImpl,
        directory: &RelativePath,
        name: &str,
        suffix: &str,
        node: NodeID,
    ) -> bool {
        let Ok(mut child) = self
            .state
            .node_name_clone(self.repository.clone(), node)
            .await
        else {
            return true;
        };
        child.push_str(suffix);
        !matches!(
            operation
                .names_one_entry(&directory.join(name), &directory.join(&child))
                .await,
            Ok(Some(false))
        )
    }
}

/// Drain the file work channel and run per-file reset work. Each task
/// `acquire_owned()`s a permit from `file_inflight` before being spawned;
/// the permit is dropped when the task completes, which back-pressures the
/// loop and ultimately the directory walker producing the items. Permits
/// grow with queue backlog up to `RESET_FILE_MAX`, mirroring clone.
async fn reset_file_consume(
    operation: Arc<InstanceOperationImpl>,
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
        let task_operation = operation.clone();
        lore_spawn!(tasks, async move {
            let _permit = permit;
            task_stats
                .file_inflight_count
                .fetch_add(1, Ordering::Relaxed);
            let result = reset_file_realize(task_operation, item, task_stats.clone()).await;
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
/// needed, otherwise realize through the operation the reset holds. Filter and staged
/// checks were already done by the walker.
async fn reset_file_realize(
    operation: Arc<InstanceOperationImpl>,
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

    let info = operation.file_info(&relative_path).await;

    let force = execution_context().globals().force();

    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    if !force
        && let Ok(info) = info.as_ref()
        && info.is_file()
    {
        let file_modified = state::file_modification(
            repository.clone(),
            &node,
            info.mtime(),
            info.size(),
            &relative_path,
            true, /* Force hash check */
            &operation,
            &lore_storage::ContentHashes::default(),
        )
        .await
        .forward::<ResetError>("Failed to check whether file changed")?
        .is_modified();

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

                operation
                    .rename(&relative_path, &to_path)
                    .await
                    .forward::<ResetError>("Failed renaming file")?;
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

    // If being reset from a directory to a file the directory must be deleted before the file is
    // created.
    if info.is_ok_and(|info| info.is_dir()) {
        reset_delete_path(&operation, &stats, &relative_path, true, false).await?;
    }

    stats.file_reset_count.fetch_add(1, Ordering::Relaxed);
    event::LoreEvent::FileResetFile(LoreFileResetFileEventData {
        path: relative_path.as_str().into(),
        action: LoreFileAction::Keep,
        from_path: LoreString::default(),
    })
    .send();

    crate::fs::realize::realize_file(
        repository.clone(),
        operation,
        relative_path,
        node,
        Arc::new(SyncRealizeStats::default()),
    )
    .await
    .forward::<ResetError>("Unable to restore path to selected state")
}
