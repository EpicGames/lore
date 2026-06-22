// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::branch::merge;
use crate::branch::merge::MergeType;
use crate::change::NodeChange;
use crate::error::LoreErrorExt;
use crate::errors::*;
use crate::event::EventError;
use crate::event::LoreEvent;
use crate::filter::FilterMode;
use crate::find;
use crate::fs::filesystem_provider::FilesystemProvider;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::StaticDispatchInstanceOperation;
use crate::history;
use crate::interface::LoreBranchLocation;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::layer;
use crate::layer::Layer;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_trace;
use crate::node::Node;
use crate::progress::DiscoveryStats;
use crate::repository;
use crate::repository::BASE_SUFFIX;
use crate::repository::MINE_SUFFIX;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::THEIRS_SUFFIX;
use crate::revision;
use crate::state;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

/// Source and target revisions selected for a sync.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionSyncTargetEventData {
    /// Remote URL
    pub remote: LoreString,
    /// Repository identifier
    pub repository: RepositoryId,
    /// Branch identifier (if any)
    pub branch: BranchId,
    /// Branch name (if any)
    pub branch_name: LoreString,
    /// Current (source) revision identifier
    pub source_revision: Hash,
    /// Current (source) revision number
    pub source_revision_number: u64,
    /// Target revision identifier
    pub target_revision: Hash,
    /// Target revision number
    pub target_revision_number: u64,
    /// Flag indicating revision is the latest revision of the branch
    pub is_latest: u8,
    /// Flag indicating revision was from local revision history, not remote
    pub local: u8,
}

/// Progress counters reported while a sync updates the working files.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionSyncProgressEventData {
    /// Number of files updated so far.
    pub file_update: usize,
    /// Total number of files to update.
    pub file_update_total: usize,
    /// Number of files deleted so far.
    pub file_delete: usize,
    /// Total number of files to delete.
    pub file_delete_total: usize,
    /// Number of files merged automatically so far.
    pub file_automerge: usize,
    /// Number of files with conflicts so far.
    pub file_conflict: usize,
    /// Number of bytes updated so far.
    pub bytes_update: u64,
    /// Total number of bytes to update.
    pub bytes_update_total: u64,
    /// Flag indicating discovery of the work to do has finished.
    #[serde(with = "u8_as_bool")]
    pub discovery_complete: u8,
}

/// The revision that resulted from a sync.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionSyncRevisionEventData {
    /// Branch (if any)
    pub branch: BranchId,
    /// Resulting revision hash signature
    pub revision: Hash,
    /// Resulting revision number, or 0 if sync resulted in a merge
    pub revision_number: u64,
    /// Sync resulted in a staged merge revision
    #[serde(with = "u8_as_bool")]
    pub flag_merge: u8,
    /// Sync resulted in a staged merged revision with conflicts
    #[serde(with = "u8_as_bool")]
    pub flag_conflict: u8,
}

#[error_set]
pub enum SyncError {
    InvalidArguments,
    WriteRequired,
    NoRemote,
    RevisionNotFound,
    LocalModifications,
    Disconnected,
    NotAuthenticated,
    NotAuthorized,
    SlowDown,
    Maintenance,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    InvalidNodeHierarchy,
    InvalidPath,
    LayerNotFound,
    LinkNotFound,
    LinkPathNotFound,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NodeNotFound,
    NotALayer,
    NotALink,
    NotFound,
    NothingStaged,
    NotSupported,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    AddressNotFound,
    NotConnected,
    Oversized,
    PayloadNotFound,
    MissingIdentity,
}

impl EventError for SyncError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

impl From<FsError> for SyncError {
    fn from(value: FsError) -> Self {
        Self::internal_with_context(value, "Failed during internal filesystem operation")
    }
}

/// Details of a single file changed by a sync.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionSyncFileEventData {
    /// Path of the file relative to the repository root.
    pub path: LoreString,
    /// Size of the file in bytes.
    pub size: u64,
    /// Action applied to the file.
    pub action: LoreFileAction,
    /// Flag indicating the entry is a file rather than a directory.
    pub flag_file: u8,
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// Optional partial revision signature to sync to
    pub revision: Option<String>,
    /// Keep local changes
    pub forward_changes: bool,
    /// Reset local modified files to match incoming revision
    pub reset: bool,
    /// Force hash checks of files
    pub force_hash_check: bool,
    /// Filter mode for diff operations during sync
    pub filter_mode: FilterMode,
    /// Root files for dependency-based selective sync.
    /// When empty: sync all files (existing behavior).
    pub root_files: Vec<String>,
    /// Tags to filter dependencies by during resolution.
    pub dependency_tags: Vec<String>,
    /// Follow transitive dependencies recursively.
    pub dependency_recursive: bool,
    /// Maximum dependency traversal depth. 0 means unlimited.
    pub dependency_depth_limit: u32,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            revision: None,
            forward_changes: false,
            reset: false,
            force_hash_check: false,
            filter_mode: FilterMode::View,
            root_files: Vec::new(),
            dependency_tags: Vec::new(),
            dependency_recursive: false,
            dependency_depth_limit: 0,
        }
    }
}

pub async fn sync(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: SyncOptions,
) -> Result<(), SyncError> {
    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<SyncError>("Failed to deserialize current revision anchor")?;

    let branch_id = if current_branch.is_zero() {
        let repository_metadata = repository::metadata_hash(repository.clone())
            .await
            .internal("Failed to load repository metadata")?;
        let repository_metadata = repository::metadata(repository.clone(), repository_metadata)
            .await
            .internal("Failed to load repository metadata")?;
        lore_debug!(
            "Currently not on a branch, default to {} {}",
            repository_metadata.default_branch_name,
            repository_metadata.default_branch
        );
        repository_metadata.default_branch
    } else {
        current_branch
    };
    lore_debug!(
        "Current revision is {} on branch {}",
        current_revision,
        branch_id
    );

    let force = execution_context().globals().force();
    let mut location = LoreBranchLocation::Local;

    // Reject a sync that would discard an actually-staged change; dirty-only
    // tracking is carried forward by rebase_staged_anchor below. --force and
    // --reset intentionally discard the staged state instead.
    if !force
        && !options.reset
        && let Some(staged_revision) = crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
        && !staged_revision.is_zero()
    {
        let state_staged = state::State::deserialize(repository.clone(), staged_revision)
            .await
            .forward::<SyncError>("Failed to deserialize staged state")?;
        if state_staged
            .node_has_staged_children(repository.clone(), crate::node::ROOT_NODE)
            .await
            .forward::<SyncError>("Failed to check staged nodes")?
        {
            return Err(InvalidArguments {
                reason: "Unable to sync when there is a staged state".into(),
            }
            .into());
        }
    }

    let local_latest = branch::load_latest(repository.clone(), branch_id)
        .await
        .unwrap_or_default();
    let mut remote_latest = Hash::default();

    let mut local_latest_diverged = branch::load_latest_divergent(repository.clone(), branch_id)
        .await
        .unwrap_or_default();

    let mut revision;
    if let Some(revision_string) = options.revision.as_ref() {
        revision = revision::resolve(
            repository.clone(),
            revision_string,
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .forward::<SyncError>("Failed to find revision")?;
        lore_debug!("Sync resolved revision target is {revision}");
    } else {
        // If there is no revision given, then we determine if the local and remote
        // latest revisions are in line or divergent.
        // - If not divergent (local or remote is a direct descendant of the other), pick the most recent revision
        // - If divergent (local and remote are NOT direct descendant of the other)
        //   - pick the remote if force flag is set
        //   - otherwise pick the local revision if it is ahead of the current revision,
        //   - otherwise pick the remote revision (which will trigger a merge flow)
        //
        // Locally-advanced by another instance: When multiple instances share
        // a mutable store, another instance's commit advances local_latest
        // without any remote involvement. In this case local_latest_diverged
        // is true, remote_latest may be zero (offline) or behind local_latest.
        // The code below handles this correctly:
        // - Offline: falls through to the local_latest.is_zero() check at the bottom
        // - Online: find_branch_point detects linear advancement (local ahead
        //   of remote), picks local_latest with location=Local
        // BranchLatestStatus remains Divergent until the user pushes, which is
        // correct — the divergence is between local and remote.
        lore_debug!("Local latest revision is {local_latest}");

        if let Ok(remote) = repository.remote().await {
            remote_latest = branch::load_remote_latest(remote.clone(), repository.id, branch_id)
                .await
                .unwrap_or_default();
            lore_debug!("Remote latest revision is {remote_latest}");
        }

        if !local_latest_diverged && !remote_latest.is_zero() {
            lore_debug!("Local latest is synchronized with remote, pick remote latest as target");
            revision = remote_latest;
            location = LoreBranchLocation::Remote;
        } else if !remote_latest.is_zero() && !local_latest.is_zero() {
            let (_branch_point, remote_history, local_history) =
                history::find_branch_point(repository.clone(), remote_latest, local_latest)
                    .await
                    .forward::<SyncError>(
                        "Unable to resolve history between local and remote branch",
                    )?;

            if !local_history.is_empty() {
                if !remote_history.is_empty() {
                    if force {
                        lore_debug!(
                            "Local and remote branch has diverged, pick remote latest as force flag is set"
                        );
                        revision = remote_latest;
                        location = LoreBranchLocation::Remote;
                    } else if current_revision != local_latest {
                        lore_debug!(
                            "Local and remote branch has diverged, pick local latest as target as it is ahead of current revision"
                        );
                        revision = local_latest;
                        location = LoreBranchLocation::Local;
                    } else {
                        lore_debug!(
                            "Local and remote branch has diverged, pick remote latest as target as current revision is local latest"
                        );
                        revision = remote_latest;
                        location = LoreBranchLocation::Remote;
                    }
                } else if force {
                    lore_debug!(
                        "Local branch ahead of remote and convergent, but pick remote latest as force flag is set"
                    );
                    revision = remote_latest;
                    location = LoreBranchLocation::Remote;
                    local_latest_diverged = false;
                } else {
                    lore_debug!(
                        "Local branch ahead of remote and convergent, pick local latest as target"
                    );
                    revision = local_latest;
                    location = LoreBranchLocation::Local;
                    local_latest_diverged = false;
                }
            } else if !remote_history.is_empty() {
                lore_debug!(
                    "Remote branch is ahead of local and convergent, pick remote latest as target"
                );
                revision = remote_latest;
                location = LoreBranchLocation::Remote;
            } else {
                lore_debug!("Current revision is at local latest, nothing to sync");
                revision = local_latest;
            }
        } else if !local_latest.is_zero() {
            revision = local_latest;
            location = LoreBranchLocation::Local;
        } else if !remote_latest.is_zero() {
            revision = remote_latest;
            location = LoreBranchLocation::Remote;
        } else {
            return SyncError::from(NoRemote).emit();
        }
    }

    let state_current = state::State::deserialize(repository.clone(), current_revision)
        .await
        .forward_with::<SyncError, _>(|| {
            format!("Failed to deserialize state {current_revision}")
        })?;

    let (layer_revisions, nearest_revision) = Box::pin(sync_load_layer_list(
        repository.clone(),
        branch_id,
        revision,
        state_current.clone(),
    ))
    .await?;

    if let Some(main_revision) = nearest_revision
        && main_revision != revision
    {
        lore_debug!("Sync revision target is {main_revision} after layer matching");
        revision = main_revision;
    }

    let remote_url = repository
        .remote()
        .await
        .clone()
        .map(|remote| remote.remote_url.to_string())
        .unwrap_or_default();

    let (branch_name, at_latest) = if branch_id.is_zero() {
        (String::default(), false)
    } else if let Ok(metadata) = branch::metadata(repository.clone(), branch_id)
        .await
        .inspect_err(|err| lore_debug!("Failed to load branch metadata: {err}"))
    {
        let name = branch::name(&metadata)
            .inspect_err(|err| lore_debug!("Failed to load branch name from metadata: {err}"))
            .unwrap_or_default()
            .to_string();
        let at_latest = (local_latest == revision) || (remote_latest == revision);
        (name, at_latest)
    } else {
        (branch_id.to_string(), false)
    };

    let state_target = state::State::deserialize(repository.clone(), revision)
        .await
        .forward_with::<SyncError, _>(|| format!("Failed to deserialize state {revision}"))?;
    lore_debug!(
        "Target revision is {} -> {} (from {})",
        state_target.revision_number(),
        state_target.revision(),
        location,
    );

    let revision = state_target.revision();
    let revision_number = state_target.revision_number();

    LoreEvent::RevisionSyncTarget(LoreRevisionSyncTargetEventData {
        remote: remote_url.into(),
        repository: repository.id,
        branch: branch_id,
        branch_name: branch_name.into(),
        source_revision: state_current.revision(),
        source_revision_number: state_current.revision_number(),
        target_revision: state_target.revision(),
        target_revision_number: state_target.revision_number(),
        is_latest: at_latest.into(),
        local: (location == LoreBranchLocation::Local).into(),
    })
    .send();

    if revision == current_revision && !force && !options.reset {
        return Ok(());
    }

    if !state_current.revision().is_zero() && !force {
        // Check if we have diverged and need to resort to a merge flow
        if location == LoreBranchLocation::Remote
            && local_latest_diverged
            && find::find_revision(
                repository.clone(),
                current_branch,
                state_target.revision(),
                false,
                None,
                |state, _metadata| {
                    if state.revision() == state_current.revision()
                        || state.parent_other() == state_current.revision()
                    {
                        find::FindMatchResult::Match
                    } else if state.revision_number() < state_current.revision_number() {
                        // Divergence, the remote branch history passed the point
                        // where local revision should have been found
                        find::FindMatchResult::Abort
                    } else {
                        find::FindMatchResult::Continue
                    }
                },
            )
            .await
            .is_err()
        {
            lore_info!("Remote and local branch have diverged, performing merge",);
            let merge_options = merge::MergeStartOptions {
                message: String::new(),
                no_commit: false,
                scope: merge::MergeScope::MainOnly,
            };
            let revision_staged = Box::pin(merge::merge_start(
                repository.clone(),
                token,
                current_branch,
                merge_options,
            ))
            .await
            .internal("Synchronizing with local changes failed to merge with remote revision")?;

            let state_staged = State::deserialize(repository.clone(), revision_staged)
                .await
                .forward_with::<SyncError, _>(|| {
                    format!("Failed to deserialize state {revision_staged}")
                })?;

            LoreEvent::RevisionSyncRevision(LoreRevisionSyncRevisionEventData {
                branch: branch_id,
                revision: state_staged.revision(),
                revision_number: state_staged.revision_number(),
                flag_merge: state_staged.is_merge_or_cherry_pick_or_revert().into(),
                flag_conflict: state_staged.is_conflict().into(),
            })
            .send();

            return Ok(());
        }
    }

    let cache_repository = repository.clone();
    let cache_state = state_target.clone();
    let cache_task = Some(lore_spawn!(async move {
        // Ignore errors during caching
        let _ = cache_state.cache_fragments(cache_repository).await;
    }));

    let state_synced = state_target.clone();
    let result = Box::pin(sync_realize(
        repository.clone(),
        state_current,
        state_target,
        options.clone(),
    ))
    .await;

    // Make sure caching has finished
    if let Some(task) = cache_task {
        let _ = task.await;
    }

    // Safe to handle error when cache task has finished
    result?;

    if !layer_revisions.is_empty() {
        Box::pin(sync_layers(
            repository.clone(),
            token,
            layer_revisions,
            options.clone(),
        ))
        .await?;
    }

    if !execution_context().globals().dry_run() {
        // If the target revision is on a different branch, update the current
        // branch. This allows sync to transparently switch branches.
        // Exception: if the target revision is the branch point where the
        // current branch was created, stay on the current branch.
        let synced_branch = state_synced
            .revision_metadata(repository.clone())
            .await
            .ok()
            .map(|m| m.branch)
            .filter(|b| !b.is_zero())
            .unwrap_or(branch_id);
        if synced_branch != branch_id {
            let is_branch_point = branch::metadata(repository.clone(), branch_id)
                .await
                .ok()
                .map(|m| branch::stack(&m))
                .is_some_and(|stack| stack.first().is_some_and(|bp| bp.revision == revision));
            if !is_branch_point {
                // Warn if another instance has the target branch checked out
                crate::instance::warn_branch_multiple_instance(&repository, synced_branch).await;

                crate::instance::store_current_anchor_branch(&repository, synced_branch)
                    .await
                    .forward::<SyncError>("Failed to serialize current revision anchor")?;
            }
        }
        crate::instance::store_current_anchor(&repository, revision)
            .await
            .forward::<SyncError>("Failed to serialize current revision anchor")?;
        state::rebase_staged_anchor(repository.clone(), revision)
            .await
            .forward::<SyncError>("Failed to rebase staged anchor")?;

        // Set the local branch LATEST to match remote if we synced to that
        // If we synced to a local revision keep the branch LATEST to not lose
        // any local history when going backwards
        if location == LoreBranchLocation::Remote {
            branch::store_latest(
                repository.clone(),
                branch_id,
                revision,
                BranchLatestStatus::Convergent,
            )
            .await
            .internal("Failed to store revision as current branch latest")?;

            branch::store_last_sync(repository, branch_id, revision).await;
        }
    }

    LoreEvent::RevisionSyncRevision(LoreRevisionSyncRevisionEventData {
        branch: branch_id,
        revision,
        revision_number,
        flag_merge: 0,
        flag_conflict: 0,
    })
    .send();

    Ok(())
}

async fn sync_load_layer_list(
    repository: Arc<RepositoryContext>,
    branch_id: BranchId,
    revision: Hash,
    state_current: Arc<State>,
) -> Result<(Vec<(Layer, Hash)>, Option<Hash>), SyncError> {
    let mut layer_revisions = vec![];
    let mut nearest_revision = None;
    if branch_id.is_zero() {
        // Detached sync - layers are handled separately by the caller
        return Ok((layer_revisions, nearest_revision));
    }
    if let Ok(layers) = layer::list(repository.clone()).await {
        // Check which matching revision to sync to for each layer
        // TODO(mjansson): Task parallelize this for multiple layers
        // TODO(mjansson): Handle multiple nearest matches for main revision
        if !layers.is_empty() {
            lore_info!("Resolving layer revisions");
        }
        for layer in layers.iter() {
            let module = Arc::new(repository.to_layer_context(layer.repository).await);
            let Ok(layer_latest) = layer::latest_revision(module.clone(), branch_id).await else {
                // No revision on this branch yet (e.g. newly created branch),
                // skip layer sync - files stay at current state
                lore_debug!(
                    "Layer {} has no revision on branch, skipping",
                    layer.repository
                );
                continue;
            };
            let revision = nearest_revision.unwrap_or(revision);
            let state_target = State::deserialize(repository.clone(), revision)
                .await
                .forward_with::<SyncError, _>(|| {
                    format!("Failed to deserialize state {revision}")
                })?;
            let (layer_revision, main_revision) = layer::find_revision_match(
                repository.clone(),
                module.clone(),
                branch_id,
                state_target.clone(),
                layer_latest,
                layer.metadata.as_deref(),
            )
            .await
            .forward::<SyncError>("Failed to find a matching revision for a layer")?;

            if main_revision != state_current.revision() {
                if let Some(nearest_revision) = nearest_revision
                    && main_revision != nearest_revision
                {
                    return SyncError::internal(
                        "Layers have diverging matching main repository revisions",
                    )
                    .emit();
                }
                nearest_revision.replace(main_revision);
            }

            lore_debug!(
                "Layer {layer:?} found revision {layer_revision} matching main revision {main_revision}"
            );
            layer_revisions.push((layer.clone(), layer_revision));
        }
    }

    Ok((layer_revisions, nearest_revision))
}

async fn sync_layers(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    layer_revisions: Vec<(Layer, Hash)>,
    options: SyncOptions,
) -> Result<(), SyncError> {
    for (layer, layer_revision) in layer_revisions {
        lore_debug!("Synchronizing layer {layer:?}");
        let layer_repository = Arc::new(repository.to_layer_context(layer.repository).await);

        let target_path = RelativePath::new_from_initial_path(layer.target_path.as_str())
            .forward::<SyncError>("Invalid layer path configuration")?;
        let source_path = RelativePath::new_from_initial_path(layer.source_path.as_str())
            .forward::<SyncError>("Invalid layer path configuration")?;

        // TODO(mjansson): Emit as events
        lore_info!("Sync layer {} in {}", layer_repository.id, target_path);

        let layer_current = state::State::deserialize(layer_repository.clone(), layer.current)
            .await
            .forward_with::<SyncError, _>(|| {
                format!("Failed to deserialize state {}", layer.current)
            })?;
        let layer_target = state::State::deserialize(layer_repository.clone(), layer_revision)
            .await
            .forward_with::<SyncError, _>(|| {
                format!("Failed to deserialize state {layer_revision}")
            })?;

        lore_info!(
            "Current state         : {} revision {}",
            layer_current.revision(),
            layer_current.revision_number()
        );
        lore_info!(
            "Synchronizing to state: {} revision {}",
            layer_target.revision(),
            layer_target.revision_number()
        );

        // TODO(mjansson): Sync disjoint layers in parallel
        Box::pin(layer::sync(
            layer_repository,
            layer_current,
            layer_target,
            target_path.clone(),
            source_path.clone(),
            options.clone(),
        ))
        .await
        .forward::<SyncError>("Failed to synchornize a layer")?;

        layer::store_layer_current(
            repository.clone(),
            token,
            target_path.as_str(),
            layer.repository,
            layer_revision,
            None,
        )
        .await
        .forward::<SyncError>("Failed to synchornize a layer")?;
    }

    Ok(())
}

async fn shim_with_operation<T>(
    filesystem: Arc<dyn FilesystemProvider>,
    changes_made: bool,
    callback: impl AsyncFnOnce(Arc<StaticDispatchInstanceOperation>) -> T,
) -> Result<T, FsError> {
    let operation = filesystem.begin_operation().await?;
    let result = callback(operation.clone()).await;
    operation.finalize(changes_made).await?;
    Ok(result)
}

async fn sync_realize(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_target: Arc<State>,
    options: SyncOptions,
) -> Result<(), SyncError> {
    Box::pin(shim_with_operation(
        repository.file_system(),
        false,
        async |operation| {
            crate::fs::realize::realize_state(
                repository,
                operation,
                state_current,
                state_target,
                options,
            )
            .await
        },
    ))
    .await?
}

#[derive(Clone)]
pub struct SyncVerifyArgs<Operation: InstanceOperation + 'static> {
    pub changes: Arc<Vec<NodeChange>>,
    pub repository_current: Arc<RepositoryContext>,
    pub operation: Arc<Operation>,
    pub state_current: Arc<State>,
    pub options: Arc<SyncOptions>,
}

pub async fn sync_verify_filesystem(
    _repository: Arc<RepositoryContext>,
    args: Arc<SyncVerifyArgs<impl InstanceOperation>>,
) -> Result<Arc<Vec<NodeChange>>, SyncError> {
    crate::fs::realize::verify_filesystem_for_changes(args).await
}

#[derive(Default)]
pub struct SyncVerifyStats {
    pub file_conflict: AtomicUsize,
    pub file_retain: AtomicUsize,
    pub file_replace: AtomicUsize,
}

pub async fn verify_filesystem(
    change: NodeChange,
    repository_current: Arc<RepositoryContext>,
    state_current: Arc<State>,
    forward_changes: bool,
    force_hash_check: bool,
    stats: Arc<SyncVerifyStats>,
    filter_mode: FilterMode,
) -> Result<Option<NodeChange>, SyncError> {
    shim_with_operation(repository_current.file_system(), false, async |operation| {
        Box::pin(crate::fs::realize::verify_filesystem(
            change,
            repository_current,
            operation,
            state_current,
            forward_changes,
            force_hash_check,
            stats,
            filter_mode,
        ))
        .await
    })
    .await?
}

#[derive(Default)]
pub struct SyncCompleteStats {
    pub file_update: AtomicUsize,
    pub file_delete: AtomicUsize,
    pub file_delete_total: AtomicUsize,
    pub file_automerge: AtomicUsize,
    pub file_conflict: AtomicUsize,
    pub bytes_update: AtomicU64,
}

#[derive(Default)]
pub struct SyncRealizeStats {
    pub discovery: DiscoveryStats,
    pub complete: SyncCompleteStats,
}

impl LoreRevisionSyncProgressEventData {
    pub fn new(stats: &Arc<SyncRealizeStats>) -> Self {
        // Read update totals from discovery stats directly since they are
        // incrementally updated by the producer. This ensures file_update_total
        // and bytes_update_total always reflect the current discovered total,
        // even while the producer is still iterating, preventing the consumer's
        // file_update count from exceeding the reported total.
        Self {
            file_update: stats.complete.file_update.load(Ordering::Relaxed),
            file_update_total: stats.discovery.total_files.load(Ordering::Relaxed) as usize,
            file_delete: stats.complete.file_delete.load(Ordering::Relaxed),
            file_delete_total: stats.complete.file_delete_total.load(Ordering::Relaxed),
            file_automerge: stats.complete.file_automerge.load(Ordering::Relaxed),
            file_conflict: stats.complete.file_conflict.load(Ordering::Relaxed),
            bytes_update: stats.complete.bytes_update.load(Ordering::Relaxed),
            bytes_update_total: stats.discovery.total_bytes.load(Ordering::Relaxed),
            discovery_complete: stats.discovery.complete.load(Ordering::Relaxed) as u8,
        }
    }
}

pub async fn realize_changes(
    repository: Arc<RepositoryContext>,
    changes: Arc<Vec<NodeChange>>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    shim_with_operation(repository.file_system(), false, async |operation| {
        crate::fs::realize::realize_changes(
            repository,
            operation,
            changes,
            state_stage,
            dry_run,
            is_merge,
            stats,
        )
        .await
    })
    .await?
}

#[allow(clippy::too_many_arguments)]
pub async fn realize_conflicts(
    repository: Arc<RepositoryContext>,
    state_base: Arc<State>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    state_stage: Option<Arc<State>>,
    conflicts: Arc<Vec<(NodeChange, NodeChange)>>,
    dry_run: bool,
    stats: Arc<SyncRealizeStats>,
    merge_type: MergeType,
) -> Result<(), SyncError> {
    shim_with_operation(repository.file_system(), false, async |operation| {
        crate::fs::realize::realize_conflicts(
            repository,
            operation,
            state_base,
            state_from,
            state_to,
            state_stage,
            conflicts,
            dry_run,
            stats,
            merge_type,
        )
        .await
    })
    .await?
}

pub async fn realize_file(
    repository: Arc<RepositoryContext>,
    path: &RelativePath,
    node: Node,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    shim_with_operation(repository.file_system(), false, async |operation| {
        crate::fs::realize::realize_file(repository, operation, path, node, stats).await
    })
    .await?
}

pub async fn realize_scratch_file(
    repository: Arc<RepositoryContext>,
    path: impl AsRef<Path>,
    node: Node,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    shim_with_operation(repository.file_system(), false, async |operation| {
        crate::fs::realize::realize_scratch_file(repository, operation, path, node, stats).await
    })
    .await?
}

pub async fn exist_merge_mine_theirs_base(absolute_path: impl AsRef<Path>) -> bool {
    if let Some(file_name) = absolute_path.as_ref().file_name() {
        let mut mine_name = file_name.to_os_string();
        mine_name.push(MINE_SUFFIX);

        let mut absolute_path = absolute_path.as_ref().to_path_buf();
        absolute_path.set_file_name(mine_name);
        if tokio::fs::metadata(&absolute_path)
            .await
            .is_ok_and(|m| m.is_file())
        {
            return true;
        }

        let mut theirs_name = file_name.to_os_string();
        theirs_name.push(THEIRS_SUFFIX);

        absolute_path.set_file_name(theirs_name);
        if tokio::fs::metadata(&absolute_path)
            .await
            .is_ok_and(|m| m.is_file())
        {
            return true;
        }

        let mut base_name = file_name.to_os_string();
        base_name.push(BASE_SUFFIX);

        absolute_path.set_file_name(base_name);
        tokio::fs::metadata(absolute_path)
            .await
            .is_ok_and(|m| m.is_file())
    } else {
        false
    }
}

pub async fn unlink_merge_mine_theirs_base(absolute_path: impl AsRef<Path>) {
    if let Some(file_name) = absolute_path.as_ref().file_name() {
        let mut mine_name = file_name.to_os_string();
        mine_name.push(MINE_SUFFIX);

        let mut theirs_name = file_name.to_os_string();
        theirs_name.push(THEIRS_SUFFIX);

        let mut base_name = file_name.to_os_string();
        base_name.push(BASE_SUFFIX);

        let mut absolute_path = absolute_path.as_ref().to_path_buf();
        absolute_path.set_file_name(mine_name);
        lore_trace!("Delete merge artifact file {}", absolute_path.display());
        let _ = util::fs::unlink(absolute_path.as_path()).await;

        absolute_path.set_file_name(theirs_name);
        lore_trace!("Delete merge artifact file {}", absolute_path.display());
        let _ = util::fs::unlink(absolute_path.as_path()).await;

        absolute_path.set_file_name(base_name);
        lore_trace!("Delete merge artifact file {}", absolute_path.display());
        let _ = util::fs::unlink(absolute_path.as_path()).await;
    }
}
