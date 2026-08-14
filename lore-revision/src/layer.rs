// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::types::BranchPoint;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::change;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::find;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_warn;
use crate::metadata;
use crate::node::NodeID;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::clone;
use crate::revision::sync;
use crate::revision::sync::SyncOptions;
use crate::revision::sync::SyncRealizeStats;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum LayerError {
    AlreadyLinked,
    LayerNotFound,
    LocalModifications,
    InvalidArguments,
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    NoRemote,
    NotSupported,
    WriteRequired,
    AddressNotFound,
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
    LinkNotFound,
    LinkPathNotFound,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NodeNotFound,
    NotALayer,
    NotALink,
    NotConnected,
    NothingStaged,
    Oversized,
    PayloadNotFound,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    RevisionNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for LayerError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for the event emitted when a layer is added.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLayerAddEventData {
    /// Path in the outer repository where the layer is placed.
    pub target_path: LoreString,
    /// Identifier of the source repository.
    pub source_repository: RepositoryId,
    /// Path inside the source repository where the layer starts.
    pub source_path: LoreString,
    /// Metadata used to match revisions between the repositories.
    pub metadata: LoreString,
    /// Revision of the source repository.
    pub revision: Hash,
}

/// Data for the event describing a single configured layer.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLayerEntryEventData {
    /// Path in the outer repository where the layer is placed.
    pub target_path: LoreString,
    /// Identifier of the source repository.
    pub source_repository: RepositoryId,
    /// Path inside the source repository where the layer starts.
    pub source_path: LoreString,
    /// Metadata used to match revisions between the repositories.
    pub metadata: LoreString,
    /// Revision of the source repository.
    pub revision: Hash,
}

/// Data for the event describing a layer that has staged changes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLayerStagedEntryEventData {
    /// Path in the outer repository where the layer is placed.
    pub target_path: LoreString,
    /// Identifier of the source repository.
    pub source_repository: RepositoryId,
    /// Number of staged files in the layer.
    pub staged_file_count: u64,
}

/// Data for the event emitted when a layer is removed.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLayerRemoveEventData {
    /// Path in the outer repository where the layer was placed.
    pub target_path: LoreString,
    /// Identifier of the source repository.
    pub source_repository: RepositoryId,
    /// Path inside the source repository where the layer started.
    pub source_path: LoreString,
    /// Revision of the source repository.
    pub revision: Hash,
    /// Set when removal was forced.
    pub forced: u8,
    /// Set when the layer files were purged from disk.
    pub purged: u8,
    /// Number of files removed.
    pub file_count: u64,
    /// Number of directories removed.
    pub directory_count: u64,
    /// Number of modified files encountered.
    pub modified_count: u64,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Layer {
    /// Path in the parent outer repository where the layer should be placed
    pub target_path: String,
    /// Path inside the layer target repository where the layer should start
    pub source_path: String,
    /// Repository of the layer
    pub repository: RepositoryId,
    /// Metadata used to match revisions between outer repository and layer repository
    pub metadata: Option<String>,
    /// Currently synchronized revision of the layer repository
    pub current: Hash,
    /// Currently staged revision of the layer repository
    pub staged: Hash,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
struct LayerConfig {
    layers: Vec<Layer>,
}

async fn load_config(config_path: impl AsRef<Path>) -> Result<LayerConfig, LayerError> {
    Ok(crate::global::load_config(config_path)
        .await
        .internal("Failed to load configuration")?)
}

async fn save_config(
    _: &RepositoryWriteToken,
    config_path: impl AsRef<Path>,
    config: &LayerConfig,
) -> Result<(), LayerError> {
    let config_string = toml::to_string_pretty(&config).internal("Failed to save configuration")?;

    lore_io::IoDriver::global()
        .write_file_bytes(config_path, bytes::Bytes::from(config_string), false)
        .await
        .internal("Failed to save configuration")?;
    Ok(())
}

pub fn layer_config_path(repository_path: impl AsRef<Path>) -> PathBuf {
    let path = repository_path.as_ref();
    let dotpath = path.join(repository::RepositoryFormat::detect(path).dot_dir());
    dotpath.join(repository::LAYER)
}

pub struct LayerState {
    pub repository: Arc<RepositoryContext>,
    pub state_current: Arc<State>,
    pub state_staged: Arc<State>,
}

impl Layer {
    pub async fn deserialize_current_and_staged(
        &self,
        repository: Arc<RepositoryContext>,
    ) -> Result<LayerState, LayerError> {
        let repository = Arc::new(repository.to_layer_context(self.repository).await);

        let state_current = State::deserialize(repository.clone(), self.current)
            .await
            .forward::<LayerError>("Failed deserializing state")?;

        let state_staged = if !self.staged.is_zero() {
            State::deserialize(repository.clone(), self.staged)
                .await
                .forward::<LayerError>("Failed deserializing state")?
        } else {
            state_current.clone()
        };

        Ok(LayerState {
            repository,
            state_current,
            state_staged,
        })
    }
}

pub async fn add(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    target_path: RelativePath,
    source_repository: RepositoryId,
    source_path: RelativePath,
    metadata: Option<&str>,
) -> Result<(), LayerError> {
    let (_state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LayerError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| _state_current.clone());

    lore_debug!("Resolve repository layer source {source_repository} path {source_path}");
    let layer_repository = Arc::new(repository.to_layer_context(source_repository).await);

    let layer_remote = layer_repository
        .remote()
        .await
        .forward::<LayerError>("Not connected")?;

    let repository_metadata = repository::metadata_hash(repository.clone())
        .await
        .forward::<LayerError>("Failed to load repository metadata")?;
    let repository_metadata = repository::metadata(repository.clone(), repository_metadata)
        .await
        .forward::<LayerError>("Failed to load repository metadata")?;

    let default_branch_id = repository_metadata.default_branch;
    let current_branch_id = current_branch;

    // Get the latest revision of the branch in the layer repository
    let layer_latest = if let Ok(layer_latest) =
        branch::load_remote_latest(layer_remote.clone(), layer_repository.id, current_branch_id)
            .await
    {
        lore_debug!("Layer repository branch exists, remote latest revision {layer_latest}");
        layer_latest
    } else {
        // If branch did not exist, create it
        let branch_metadata = branch::metadata(repository.clone(), current_branch_id)
            .await
            .forward::<LayerError>("Failed getting branch metadata")?;
        let branch_name = branch::name(&branch_metadata)
            .forward::<LayerError>("Failed getting branch metadata")?;
        let branch_category = branch::category(&branch_metadata).unwrap_or_default();

        // TODO(mjansson): Do we need to recreate branch hierarchies here, or fine to just branch
        //                 from default branch at current latest?
        let parent_latest = branch::load_remote_latest(
            layer_remote.clone(),
            layer_repository.id,
            default_branch_id,
        )
        .await
        .forward::<LayerError>("Failed getting branch metadata")?;

        lore_debug!("Creating layer repository branch {branch_name} at revision {parent_latest}");

        let user_id = execution_context().user_id().await;

        let branch_stack = vec![BranchPoint {
            branch: default_branch_id,
            revision: parent_latest,
        }];

        let revision = layer_remote
            .revision(layer_repository.id)
            .await
            .forward::<LayerError>("Not connected")?;
        let layer_latest = revision
            .branch_create(
                current_branch_id,
                branch_name,
                branch_category,
                user_id.as_str(),
                &branch_stack,
            )
            .await
            .forward::<LayerError>("Failed to create branch in layer repository")?;

        lore_debug!(
            "Layer repository branch {branch_name} created at latest revision {layer_latest}"
        );
        layer_latest
    };

    lore_debug!("Find matching revision");
    let (layer_revision, _) = find_revision_match(
        repository.clone(),
        layer_repository.clone(),
        current_branch_id,
        state_staged.clone(),
        layer_latest,
        metadata,
    )
    .await?;

    lore_debug!("Load layer revision state {layer_revision}");
    let layer_state = State::deserialize(layer_repository.clone(), layer_revision)
        .await
        .forward::<LayerError>("Failed deserializing state")?;

    lore_debug!("Find layer revision source node for {source_path}");
    let layer_node_link = layer_state
        .find_node_link(layer_repository.clone(), source_path.as_str())
        .await
        .forward_with::<LayerError, _>(|| format!("Invalid path {source_path}"))?;

    lore_debug!("Layer revision source node is {layer_node_link:?}");
    if !layer_node_link.is_valid_or_root() {
        return Err(LayerError::internal(format!("Invalid path {source_path}")));
    }

    // Target node must be in the given layer repository, not in a linked repository
    if layer_node_link.repository != layer_repository.id {
        return Err(LayerError::internal(
            "Layer path is in a linked repository itself, create the layer using the target repository directly",
        ));
    }

    // Target node must be a directory
    let layer_node = layer_state
        .node(layer_repository.clone(), layer_node_link.node)
        .await
        .forward::<LayerError>("Failed deserializing state")?;

    if !layer_node.is_directory() {
        return Err(LayerError::internal(
            "Layer target path must be a directory in the layer repository",
        ));
    }

    let mut config = load_config(layer_config_path(repository.require_path()?)).await?;

    for layer in config.layers.iter() {
        if layer.repository == layer_repository.id
            && layer.target_path.as_str() == target_path.as_str()
        {
            return Err(AlreadyLinked.into());
        }
    }

    config.layers.push(Layer {
        target_path: target_path.to_string(),
        source_path: source_path.to_string(),
        repository: layer_repository.id,
        metadata: metadata.map(|key| key.to_string()),
        current: layer_revision,
        staged: Hash::default(),
    });

    let absolute_path = target_path.to_absolute_path(repository.require_path()?);

    // Materialize layer
    lore_debug!("Connecting remote storage");
    let correlation_id = crate::lore::execution_context()
        .globals()
        .correlation_id
        .to_string();
    let layer_storage = layer_remote
        .session(layer_repository.id, &correlation_id)
        .await
        .forward::<LayerError>("Not connected")?;

    event::LoreEvent::LayerAdd(LoreLayerAddEventData {
        target_path: LoreString::from(&target_path),
        source_repository: layer_repository.id,
        source_path: LoreString::from(&source_path),
        metadata: metadata.into(),
        revision: layer_revision,
    })
    .send();

    // Ensure the target path exist to clone into
    lore_io::IoDriver::global()
        .create_dir_all(&absolute_path)
        .await
        .internal("Failed to create the target directory for layer")?;

    clone::clone_node(
        layer_repository.clone(),
        layer_storage,
        layer_state,
        absolute_path,
        source_path,
        layer_node_link.node,
        Arc::new(clone::CloneOptions {
            ignore_existing: false,
            ..Default::default()
        }),
        Arc::default(), /* Default stats */
    )
    .await
    .forward::<LayerError>("Failed cloning target layer")?;

    save_config(
        token,
        layer_config_path(repository.require_path()?),
        &config,
    )
    .await?;

    Ok(())
}

/// Find the index of a configured layer matching `target_path`. When
/// `source_repository` is zero, ambiguity (multiple layers sharing the target
/// path) is reported as `InvalidArguments`; otherwise an exact match on both
/// `target_path` and `source_repository` is required.
fn resolve_layer_index(
    layers: &[Layer],
    target_path: &str,
    source_repository: RepositoryId,
) -> Result<usize, LayerError> {
    if source_repository.is_zero() {
        let mut matches = layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| layer.target_path.as_str() == target_path);
        let first = matches.next().ok_or(LayerNotFound)?;
        if matches.next().is_some() {
            return Err(InvalidArguments {
                reason: format!(
                    "Multiple layers configured at '{target_path}', specify a source repository to disambiguate"
                ),
            }
            .into());
        }
        Ok(first.0)
    } else {
        layers
            .iter()
            .position(|layer| {
                layer.repository == source_repository && layer.target_path.as_str() == target_path
            })
            .ok_or_else(|| LayerNotFound.into())
    }
}

pub async fn remove(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    target_path: RelativePath,
    source_repository: RepositoryId,
    purge: bool,
) -> Result<(), LayerError> {
    let config_path = layer_config_path(repository.require_path()?);
    let mut config = load_config(&config_path).await?;

    let layer_index = resolve_layer_index(&config.layers, target_path.as_str(), source_repository)?;
    let layer = config.layers[layer_index].clone();

    let layer_repository = Arc::new(repository.to_layer_context(layer.repository).await);
    let layer_state = State::deserialize(layer_repository.clone(), layer.current)
        .await
        .forward::<LayerError>("Failed to deserialize layer state")?;

    let source_path = RelativePath::new_from_initial_path(layer.source_path.as_str())
        .forward_with::<LayerError, _>(|| {
            format!("Invalid layer source path {}", layer.source_path)
        })?;
    let source_node_link = layer_state
        .find_node_link(layer_repository.clone(), source_path.as_str())
        .await
        .forward::<LayerError>("Failed to locate layer source node")?;

    let force = execution_context().globals().force();
    let mut tracked_files: Vec<RelativePath> = Vec::new();
    let mut tracked_directories: Vec<RelativePath> = Vec::new();
    let mut modified: Vec<String> = Vec::new();

    walk_layer_subtree(
        layer_repository.clone(),
        layer_state.clone(),
        source_node_link.node,
        target_path.clone(),
        &mut tracked_files,
        &mut tracked_directories,
        &mut modified,
    )
    .await?;

    if !modified.is_empty() && !force {
        lore_warn!(
            "Layer at '{}' has locally modified files (use --force to discard): {}",
            target_path.as_str(),
            modified.join(", ")
        );
        return Err(LocalModifications.into());
    }

    let modified_count = modified.len() as u64;
    let file_count = tracked_files.len() as u64;
    let directory_count = tracked_directories.len() as u64;
    let absolute_root = target_path.to_absolute_path(repository.require_path()?);

    if purge {
        // Full nuke: delete the entire target subtree including untracked
        // content. Force is independent — if there were modifications without
        // --force we already returned above.
        if let Err(err) = crate::util::fs::unlink_recursive(&absolute_root).await {
            lore_warn!(
                "Failed to purge layer root {}: {err}",
                absolute_root.display()
            );
        }
    } else {
        for file in &tracked_files {
            let absolute = file.to_absolute_path(repository.require_path()?);
            if let Err(err) = crate::util::fs::unlink(&absolute).await {
                lore_warn!("Failed to remove layer file {}: {err}", absolute.display());
            }
        }

        // Bottom-up: deepest directories first so empty dirs collapse when
        // their children are gone. Untracked files keep their parent dirs
        // alive — remove_dir fails on non-empty dirs and is skipped silently.
        tracked_directories.sort_by_key(|p| std::cmp::Reverse(p.as_str().split('/').count()));
        for dir in &tracked_directories {
            let absolute = dir.to_absolute_path(repository.require_path()?);
            if let Err(err) = lore_io::IoDriver::global().remove_dir(&absolute).await
                && err.kind() != tokio::io::ErrorKind::NotFound
            {
                lore_debug!(
                    "Skip non-empty or unremovable layer directory {}: {err}",
                    absolute.display()
                );
            }
        }

        if !target_path.is_empty()
            && let Err(err) = lore_io::IoDriver::global().remove_dir(&absolute_root).await
            && err.kind() != tokio::io::ErrorKind::NotFound
        {
            lore_debug!(
                "Skip non-empty or unremovable layer root {}: {err}",
                absolute_root.display()
            );
        }
    }

    config.layers.remove(layer_index);
    save_config(token, &config_path, &config).await?;

    event::LoreEvent::LayerRemove(LoreLayerRemoveEventData {
        target_path: LoreString::from(&target_path),
        source_repository: layer.repository,
        source_path: LoreString::from_str(&layer.source_path),
        revision: layer.current,
        forced: (force && modified_count > 0) as u8,
        purged: purge as u8,
        file_count,
        directory_count,
        modified_count,
    })
    .send();

    Ok(())
}

fn walk_layer_subtree<'a>(
    layer_repository: Arc<RepositoryContext>,
    layer_state: Arc<State>,
    node: NodeID,
    filesystem_path: RelativePath,
    tracked_files: &'a mut Vec<RelativePath>,
    tracked_directories: &'a mut Vec<RelativePath>,
    modified: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), LayerError>> + Send + 'a>> {
    Box::pin(async move {
        let mut iter = crate::state::StateNodeChildrenWithNameIterator::new(
            layer_state.clone(),
            layer_repository.clone(),
            node,
        )
        .await
        .forward::<LayerError>("Failed to iterate layer state children")?;

        while let Some((child_id, child_node, child_name)) = iter
            .next()
            .await
            .forward::<LayerError>("Failed to iterate layer state children")?
        {
            let name_ref: &str = child_name.as_ref();
            if name_ref.is_empty() {
                continue;
            }
            let child_path = filesystem_path.join(name_ref);
            drop(child_name);

            if child_node.is_directory() {
                tracked_directories.push(child_path.clone());
                walk_layer_subtree(
                    layer_repository.clone(),
                    layer_state.clone(),
                    child_id,
                    child_path,
                    tracked_files,
                    tracked_directories,
                    modified,
                )
                .await?;
            } else {
                let absolute = child_path.to_absolute_path(layer_repository.require_path()?);
                match lore_io::IoDriver::global().metadata(&absolute).await {
                    Ok(metadata) if metadata.is_file() => {
                        let (file_mtime, file_size) =
                            crate::util::fs::file_mtime_and_size(&metadata);
                        let is_modified = state::is_file_modified(
                            layer_repository.clone(),
                            &child_node,
                            file_mtime,
                            file_size,
                            &child_path,
                            true,
                        )
                        .await
                        .map_or(true, |(m, _)| m);
                        if is_modified {
                            modified.push(child_path.as_str().to_string());
                        }
                        tracked_files.push(child_path);
                    }
                    Ok(_) => {
                        modified.push(format!("{} (type changed)", child_path.as_str()));
                        tracked_files.push(child_path);
                    }
                    Err(err) if err.kind() == tokio::io::ErrorKind::NotFound => {
                        modified.push(format!("{} (missing)", child_path.as_str()));
                    }
                    Err(err) => {
                        lore_warn!("Failed to stat layer file {}: {err}", absolute.display());
                        modified.push(format!("{} (stat failed)", child_path.as_str()));
                    }
                }
            }
        }
        Ok(())
    })
}

pub async fn list(repository: Arc<RepositoryContext>) -> Result<Vec<Layer>, LayerError> {
    let config = load_config(layer_config_path(repository.require_path()?)).await?;
    Ok(config.layers)
}

/// Information about a layer with staged changes, including the count of files
/// modified since the layer's `current` revision.
#[derive(Clone, Debug)]
pub struct StagedLayerInfo {
    pub target_path: String,
    pub repository: RepositoryId,
    pub staged_file_count: u64,
}

/// Walk the configured layers and emit a `LayerStagedEntry` event for each
/// layer with `staged != current` and at least one staged file. Returns the
/// list for callers that want it as a value.
///
/// Mirrors `link::list::list_staged` for use by the CLI's per-layer message
/// prompt.
pub async fn list_staged(
    repository: Arc<RepositoryContext>,
) -> Result<Vec<StagedLayerInfo>, LayerError> {
    let layers = list(repository.clone()).await?;
    let mut result = Vec::new();
    for layer in layers {
        if layer.staged.is_zero() || layer.staged == layer.current {
            continue;
        }
        let layer_repository = Arc::new(repository.to_layer_context(layer.repository).await);
        let staged_state = State::deserialize(layer_repository.clone(), layer.staged)
            .await
            .forward::<LayerError>("Failed to deserialize layer staged state")?;

        // Walk from the layer's source_path node and count nodes flagged staged.
        let source_node_link = staged_state
            .find_node_link(layer_repository.clone(), &layer.source_path)
            .await
            .forward::<LayerError>("Failed to locate layer source node")?;
        let staged_file_count = count_staged_files(
            layer_repository.clone(),
            staged_state,
            source_node_link.node,
        )
        .await;

        if staged_file_count == 0 {
            continue;
        }

        let info = StagedLayerInfo {
            target_path: layer.target_path.clone(),
            repository: layer.repository,
            staged_file_count,
        };

        event::LoreEvent::LayerStagedEntry(LoreLayerStagedEntryEventData {
            target_path: LoreString::from_str(&layer.target_path),
            source_repository: layer.repository,
            staged_file_count,
        })
        .send();

        result.push(info);
    }
    Ok(result)
}

async fn count_staged_files(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> u64 {
    let mut count = 0u64;
    let children = match crate::state::StateNodeChildrenIterator::new(
        state.clone(),
        repository.clone(),
        node_id,
    )
    .await
    {
        Ok(iter) => iter,
        Err(err) => {
            lore_warn!("Failed to iterate children for layer staged file count: {err}");
            return 0;
        }
    };

    let mut iter = children;
    while let Ok(Some((child_id, child_node))) = iter.next().await {
        if !child_node.is_staged() {
            continue;
        }
        if child_node.is_file() {
            count += 1;
        } else if child_node.is_directory() {
            count += Box::pin(count_staged_files(
                repository.clone(),
                state.clone(),
                child_id,
            ))
            .await;
        }
    }

    count
}

pub async fn sync(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_target: Arc<State>,
    target_path: RelativePath,
    source_path: RelativePath,
    options: SyncOptions,
) -> Result<(), LayerError> {
    let stats: Arc<SyncRealizeStats> = Arc::default();
    let changes = if !options.reset {
        lore_info!(
            "Calculating deltas {} -> {}",
            state_current.revision_number(),
            state_target.revision_number()
        );
        let changes = state::diff_collect(
            repository.clone(),
            state_current.clone(),
            repository.clone(),
            state_target.clone(),
            if !source_path.is_empty() {
                Some(source_path.clone())
            } else {
                None
            },
            options.filter_mode,
        )
        .await
        .forward::<LayerError>("Failed to calculate state diff when synchronizing")?;

        if target_path != source_path {
            // TODO(mjansson): Rewrite changes paths
            return Err(LayerError::internal("Not implemented"));
        }

        changes
    } else {
        if target_path != source_path {
            // TODO(mjansson): File system diff not implemented when repository subpath
            //                 and filesystem subpath are not equal
            return Err(LayerError::internal("Not implemented"));
        }

        // Reverse the changes since diff filesystem returns changes from state to filesystem,
        // while we want to do filesystem to state
        lore_info!(
            "Calculating deltas from filesystem -> {}",
            state_target.revision_number()
        );
        let (mut changes, _diff_stats) = state::diff_filesystem(
            repository.clone(),
            state_target.clone(),
            repository.clone(),
            state_current.clone(),
            if !source_path.is_empty() {
                Some(source_path)
            } else {
                None
            },
            options.filter_mode,
            Arc::new(Vec::new()),
        )
        .await
        .forward::<LayerError>("Failed to calculate file system diff when synchronizing")?;

        change::reverse(changes.as_mut_slice());
        changes
    };

    let options = Arc::new(options);
    let changes = Arc::new(changes);
    let force = execution_context().globals().force();
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .forward::<LayerError>("Failed to start filesystem operation")?;
    let changes = if !changes.is_empty() && !force {
        lore_info!(
            "Verifying {} layer changes with local file system",
            changes.len()
        );
        sync::sync_verify_filesystem(
            repository.clone(),
            Arc::new(sync::SyncVerifyArgs {
                changes: changes.clone(),
                repository_current: repository.clone(),
                operation: operation.clone(),
                state_current: state_current.clone(),
                options: options.clone(),
            }),
        )
        .await
        .forward::<LayerError>("Failed to verify file system during layer sync")?
    } else {
        changes
    };

    crate::fs::realize::realize_changes(
        repository.clone(),
        operation.clone(),
        changes,
        None,
        execution_context().globals().dry_run(),
        false, /* Not a merge */
        stats,
    )
    .await
    .forward::<LayerError>("Failed to sync layer files")?;

    operation
        .finalize(true)
        .await
        .forward::<LayerError>("Failed to finalize operation")?;

    Ok(())
}

pub async fn latest_revision(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Hash, LayerError> {
    let local_latest = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();

    let remote_latest = if let Ok(remote) = repository.remote().await {
        branch::load_remote_latest(remote, repository.id, branch)
            .await
            .unwrap_or_default()
    } else {
        local_latest
    };

    if local_latest.is_zero() {
        if remote_latest.is_zero() {
            return Err(LayerError::internal(
                "Failed to find latest revision for layer branch",
            ));
        }
        return Ok(remote_latest);
    } else if remote_latest.is_zero() {
        return Ok(local_latest);
    }

    let Ok(local_state) = State::deserialize(repository.clone(), local_latest).await else {
        return Ok(remote_latest);
    };
    let Ok(remote_state) = State::deserialize(repository.clone(), remote_latest).await else {
        return Ok(local_latest);
    };

    if local_state.revision_number() > remote_state.revision_number() {
        Ok(local_latest)
    } else {
        Ok(remote_latest)
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn find_revision_match(
    repository: Arc<RepositoryContext>,
    layer: Arc<RepositoryContext>,
    _branch: BranchId,
    state: Arc<State>,
    latest: Hash,
    metadata: Option<&str>,
) -> Result<(Hash, Hash), LayerError> {
    let Some(metadata) = metadata else {
        lore_debug!("Layer has no metadata link, use latest revision {latest}");
        return Ok((latest, state.revision()));
    };

    // Find the revision with matching metadata
    let search_limit = execution_context()
        .globals()
        .search_limit()
        .unwrap_or(find::DEFAULT_SEARCH_LIMIT);
    let search_nearest = execution_context().globals().search_nearest();
    lore_debug!(
        "Find revision with matching metadata: {metadata} (search limit: {search_limit}, search nearest: {search_nearest})",
    );

    // Start by building a set of revisions to match against
    let mut source_revisions = vec![];
    if execution_context().globals().search_nearest() {
        lore_debug!("Batch load revisions for source history");
        let revisions = find::batch_load_history(repository.clone(), state.revision()).await;
        source_revisions = revisions.into_iter().map(|hash| (hash, None)).collect();
    }
    if source_revisions.is_empty() {
        source_revisions.push((state.revision(), None));
    }

    lore_debug!("Batch load revisions for target history from revision {latest}");
    let target_revisions = find::batch_load_history(layer.clone(), latest).await;
    let mut target_revisions: Vec<(Hash, Option<Vec<u8>>)> = target_revisions
        .into_iter()
        .map(|hash| (hash, None))
        .collect();

    let mut target_search_count = target_revisions.len();
    loop {
        lore_debug!("Iterate {} source revisions", source_revisions.len());
        for source_revision in source_revisions.iter_mut() {
            if source_revision.1.is_none() {
                let state = state::State::deserialize(repository.clone(), source_revision.0)
                    .await
                    .forward::<LayerError>("Failed deserializing state")?;
                let revision_metadata = state.metadata_hash();
                let revision_metadata =
                    metadata::Metadata::deserialize(repository.clone(), revision_metadata)
                        .await
                        .forward::<LayerError>("Failed to deserialize revision metadata")?;
                let current_value = revision_metadata
                    .get_binary(metadata)
                    .forward::<LayerError>("Failed to get the metadata value for revision link")?;
                source_revision.1.replace(current_value.to_vec());
            }

            let Some(source_value) = source_revision.1.as_deref() else {
                continue;
            };

            lore_debug!(
                "Iterate {} target revisions for source revision metadata {:?}",
                target_revisions.len(),
                source_value
            );
            for target_revision in target_revisions.iter_mut() {
                if target_revision.1.is_none() {
                    let state = state::State::deserialize(layer.clone(), target_revision.0)
                        .await
                        .forward::<LayerError>("Failed deserializing state")?;
                    let revision_metadata = state.metadata_hash();
                    let revision_metadata =
                        metadata::Metadata::deserialize(layer.clone(), revision_metadata)
                            .await
                            .forward::<LayerError>("Failed to deserialize revision metadata")?;
                    let target_value = revision_metadata
                        .get_binary(metadata)
                        .forward::<LayerError>(
                            "Failed to get the metadata value for revision link",
                        )?;
                    target_revision.1.replace(target_value.to_vec());
                }

                let Some(target_value) = target_revision.1.as_deref() else {
                    continue;
                };

                if target_value == source_value {
                    lore_debug!(
                        "Found matching metadata for source revision {} target revision {} value {:?}",
                        source_revision.0,
                        target_revision.0,
                        target_value
                    );
                    return Ok((target_revision.0, source_revision.0));
                }
            }
        }

        if target_search_count >= search_limit {
            return Err(LayerError::internal(
                "Failed to find matching revision for link metadata",
            ));
        }

        if search_nearest {
            lore_debug!("Batch load additional revisions for source history");
            let last_revision = source_revisions
                .last()
                .map(|tuple| tuple.0)
                .unwrap_or_default();
            let revisions = find::batch_load_history(repository.clone(), last_revision).await;
            let mut additional = revisions.into_iter().map(|hash| (hash, None)).collect();
            source_revisions.append(&mut additional);
        }

        lore_debug!("Batch load additional revisions for target history");
        let last_revision = target_revisions
            .last()
            .map(|tuple| tuple.0)
            .unwrap_or_default();
        let revisions = find::batch_load_history(layer.clone(), last_revision).await;
        let mut additional: Vec<_> = revisions.into_iter().map(|hash| (hash, None)).collect();
        target_search_count += additional.len();
        if source_revisions.len() > 1 {
            target_revisions.append(&mut additional);
        } else {
            target_revisions = additional;
        }
    }
}

pub async fn store_layer_current(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    target_path: &str,
    layer_repository: RepositoryId,
    current: Hash,
    staged: Option<Hash>,
) -> Result<(), LayerError> {
    let mut config = load_config(layer_config_path(repository.require_path()?)).await?;

    for layer in config.layers.iter_mut() {
        if layer.repository == layer_repository && layer.target_path.as_str() == target_path {
            layer.current = current;
            if let Some(staged) = staged {
                layer.staged = staged;
            }
            save_config(
                token,
                layer_config_path(repository.require_path()?),
                &config,
            )
            .await?;
            lore_debug!("Saved layer config: {config:?}");
            return Ok(());
        }
    }

    Err(LayerNotFound.into())
}

pub async fn store_layer_current_batch(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    updates: &[(RepositoryId, &str, Hash)],
) -> Result<(), LayerError> {
    if updates.is_empty() {
        return Ok(());
    }

    let mut config = load_config(layer_config_path(repository.require_path()?)).await?;

    for (layer_repository, target_path, current) in updates {
        for layer in config.layers.iter_mut() {
            if layer.repository == *layer_repository && layer.target_path.as_str() == *target_path {
                layer.current = *current;
                break;
            }
        }
    }

    save_config(
        token,
        layer_config_path(repository.require_path()?),
        &config,
    )
    .await?;
    lore_debug!(
        "Saved layer config (batch update, {} layers): {config:?}",
        updates.len()
    );

    Ok(())
}

pub async fn store_layer_staged(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    target_path: &str,
    layer_repository: RepositoryId,
    staged: Hash,
) -> Result<(), LayerError> {
    let mut config = load_config(layer_config_path(repository.require_path()?)).await?;

    for layer in config.layers.iter_mut() {
        if layer.repository == layer_repository && layer.target_path.as_str() == target_path {
            layer.staged = staged;
            save_config(
                token,
                layer_config_path(repository.require_path()?),
                &config,
            )
            .await?;
            lore_debug!("Saved layer config: {config:?}");
            return Ok(());
        }
    }

    Err(LayerNotFound.into())
}
