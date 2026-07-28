// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::ops::BitAnd;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::branch;
use crate::change;
use crate::errors::*;
use crate::event;
use crate::filter::FilterMode;
use crate::hash;
use crate::infer::infer_is_conflicted_by_path;
use crate::interface::LoreArray;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_spawn_blocking;
use crate::lore_trace;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::NodeLink;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::emit_path_ignore;
use crate::repository::BASE_SUFFIX;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::repository::THEIRS_SUFFIX;
use crate::revision::sync;
use crate::revision::sync::SyncRealizeStats;
use crate::state;
use crate::state::State;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::state::is_file_modified;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

/// Data for the event emitted when a stage operation begins.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageBeginEventData {
    /// Number of paths requested for staging.
    pub path_count: usize,
}

/// Running counts of items processed during a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageProgressEventData {
    /// Current counts of items processed.
    pub count: LoreFileStageCountData,
}

/// Data for the event emitted when a stage operation completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageEndEventData {
    /// Final counts of items processed.
    pub count: LoreFileStageCountData,
}

/// Data for the event identifying the repository and revision involved in a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageRevisionEventData {
    /// Identifier of the repository.
    pub repository: RepositoryId,
    /// Revision the files are staged against.
    pub revision: Hash,
}

/// Data for the event emitted for each file affected by a stage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileStageFileEventData {
    /// Previous path of the file, when it was moved.
    pub from_path: LoreString,
    /// Path of the file.
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
    /// every file is reconciled, ignoring the dirty flags.
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
    if !link_tracker.has_modifications() {
        return Ok(());
    }

    let links_needing_rehash = link_tracker.get_links_needing_rehash();

    for link_context in links_needing_rehash {
        // Get the current branch from existing link metadata
        let link_reference = state
            .link_find(
                repository.clone(),
                link_context.link_repository_id,
                link_context.link_node_id,
            )
            .await
            .forward::<StageError>("Link not found for update")?;

        let current_link_reference = state_current
            .link_find(
                repository.clone(),
                link_context.link_repository_id,
                link_context.link_node_id,
            )
            .await
            .forward::<StageError>("Link not found for update")?;

        lore_debug!(
            "Setting link parent to {}",
            current_link_reference.signature
        );

        link::reserialize_tracked_link(
            &state,
            repository.clone(),
            token,
            &link_context,
            current_link_reference.signature,
            link_reference.branch,
        )
        .await
        .forward::<StageError>("Failed to update link")?;

        // Mark the link node as staged
        state
            .node_mark(
                repository.clone(),
                link_context.link_node_id,
                NodeFlags::Staged,
                true,
            )
            .await
            .forward::<StageError>("Failed to mark node as staged")?;
    }
    Ok(())
}

/// Stage changes from filesystem into the given state
/// The base directory is the point where the relative path starts
/// Only the relative path will be checked for case consistency
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_filesystem_path(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    base_absolute_path: PathBuf,
    base_relative_path: RelativePathBuf,
    base_node: NodeID,
    relative_path: RelativePath,
    stats: Arc<StageStats>,
    options: StageOptions,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
    layer_mask: Option<Arc<Vec<String>>>,
) -> Result<NodeLink, StageError> {
    lore_debug!(
        "Staging path: {}/{}",
        base_absolute_path.display(),
        relative_path.as_str(),
    );

    let full_absolute_path = if !relative_path.is_empty() {
        // Find the file system case variation that corresponds to the user given path
        // If no path found, assume it's a delete and use the user given path
        let fs_path = util::fs::filesystem_path(base_absolute_path.as_path(), &relative_path)
            .await
            .unwrap_or(relative_path.as_str().to_string());
        base_absolute_path.join(fs_path.as_str())
    } else {
        base_absolute_path.clone()
    };

    let mut relative_path = RelativePath::new_from_user_path(
        base_absolute_path.as_path(),
        full_absolute_path.to_string_lossy().as_ref(),
    )
    .forward::<StageError>(&format!("Invalid path {relative_path}"))?;

    let force = execution_context().globals().force();
    if !force
        && repository
            .filter
            .emit_excludes(&relative_path, true, FilterMode::Full)
    {
        lore_trace!("Path excluded by filter: {}", relative_path.as_str());
        return Ok(NodeLink::invalid());
    }

    if let Ok(metadata) = tokio::fs::metadata(&full_absolute_path).await {
        if metadata.is_dir() {
            lore_debug!(
                "Stage directory: {}/{}",
                repository.path_for_display(),
                relative_path.as_str(),
            );
        } else if metadata.is_file() {
            lore_debug!(
                "Stage file: {}/{}",
                repository.path_for_display(),
                relative_path.as_str(),
            );
        } else {
            return Err(StageError::internal(format!(
                "Failed to stage path {}, unsupported type",
                full_absolute_path.display()
            )));
        }

        // iterate the subpaths, do file system real name fetching and stage each node sequentially
        // to do case mismatch resolution. This way the stage_node_from_metadata function does not have to look
        // in the file system for the current existing name case variation but just use what was
        // passed to the function as the stage_directory will enumerate the file system.
        let mut current_repository = repository.clone();
        let mut current_relative_path = base_relative_path;
        let mut current_absolute_path = base_absolute_path;
        let mut current_node = base_node;
        let mut current_state = state.clone();

        while !relative_path.is_empty() {
            let current_name = relative_path.pop_root();
            if current_name == "." {
                continue;
            }

            let current_metadata = tokio::fs::metadata(current_absolute_path.join(current_name))
                .await
                .internal(&format!(
                    "Failed to query file system metadata for path {}",
                    current_absolute_path.join(current_name).display()
                ))?;

            let node_link = stage_node_from_metadata(
                current_repository.clone(),
                current_state.clone(),
                current_absolute_path.as_path(),
                current_relative_path.clone().freeze(),
                current_node,
                current_name.to_string(),
                current_metadata,
                options,
                stats.clone(),
                link_tracker.clone(),
            )
            .await?;

            if !node_link.is_valid() {
                return Ok(node_link);
            }

            // Scoped so the node_name_ref read lock drops before node() below; a
            // second shared lock on the same block deadlocks behind a queued writer.
            {
                let final_name = current_state
                    .node_name_ref(current_repository.clone(), node_link.node)
                    .await
                    .forward::<StageError>("Failed to resolve node name")?;
                current_absolute_path.push(&*final_name);
                current_relative_path.push(&final_name);
            }

            let node = current_state
                .node(current_repository.clone(), node_link.node)
                .await
                .forward::<StageError>(
                    "Node not found in child node list, inconsistent repository state",
                )?;

            current_node = node_link.node;

            // Transition into the link
            if node.is_link() {
                let link_repository_id: RepositoryId = node.address.context.into();
                let link_revision = node.address.hash;
                let linked_node = node.child;

                lore_debug!(
                    "Transition into link with ID {link_repository_id} at revision {link_revision}"
                );

                let linked_repository =
                    Arc::new(current_repository.to_link_context(link_repository_id).await);
                let mut linked_state =
                    State::deserialize(current_repository.clone(), link_revision)
                        .await
                        .forward::<StageError>("Failed to deserialize revision state")?;

                // Track this link for potential reserialization
                if let Some(ref tracker) = link_tracker {
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
                        parent_repository_id: current_repository.id,
                        link_path: current_relative_path.clone(),
                        link_state: linked_state.clone(),
                    };

                    tracker.add_link(link_context);
                }

                current_repository = linked_repository;
                current_state = linked_state;
                current_node = linked_node;
            }
        }

        // Finally, if the given path is a directory we should recurse and stage everything below it
        if !options.no_children && (current_node == ROOT_NODE || metadata.is_dir()) {
            stats.task_count.fetch_add(1, Ordering::Release);

            let result = stage_directory(
                current_repository.clone(),
                current_state.clone(),
                current_absolute_path.as_path(),
                current_relative_path,
                current_node,
                1,
                options,
                stats.clone(),
                link_tracker.clone(),
                layer_mask.clone(),
            )
            .await;
            stats.task_count.fetch_sub(1, Ordering::Release);
            result?;
        }

        return Ok(NodeLink {
            node: current_node,
            repository: current_repository.id,
            revision: current_state.revision(),
        });
    }

    // Treat all errors as a non-existing file/directory. Otherwise if a subpath of the queried path
    // is a file we will get platform specific not-a-directory error that cannot be platform independently
    // matched along with not-found errors.
    lore_debug!(
        "Path not found, staging delete: {}/{}",
        repository.path_for_display(),
        relative_path.as_str(),
    );
    // TODO(mjansson): Find node link could return the found case aware path of the node
    if let Ok(node_link) = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
    {
        // Check if case of repository path matches the given path
        let mut current_repository = repository.clone();
        let node_state = if node_link.repository != repository.id {
            current_repository = Arc::new(repository.to_link_context(node_link.repository).await);
            State::deserialize(current_repository.clone(), node_link.revision)
                .await
                .forward::<StageError>("Failed to deserialize revision state")?
        } else {
            state.clone()
        };

        if let Some(ref tracker) = link_tracker
            && node_link.repository != repository.id
        {
            let link_path = relative_path.clone().into_buf();

            // Find the parent repository's link node
            let parent_link_node_id = state
                .find_link_parent_node(
                    repository.clone(),
                    relative_path.as_str(),
                    node_link.repository,
                )
                .await
                .forward::<StageError>("Failed to find subnode")?;

            let link_context = crate::link::LinkContext {
                link_repository_id: node_link.repository,
                link_node_id: parent_link_node_id,
                parent_repository_id: repository.id,
                link_path,
                link_state: node_state.clone(),
            };

            tracker.add_link(link_context);
        }

        let node_path = node_state
            .node_path(current_repository.clone(), node_link.node)
            .await
            .forward::<StageError>("Failed to resolve node path in state")?;
        if node_path == relative_path.as_str() {
            lore_debug!(
                "Path {} exist in repository with matching case, stage deletion",
                relative_path
            );
            stage_delete(
                current_repository.clone(),
                node_state,
                node_link.node,
                options.node_flags,
                stats.clone(),
                link_tracker.clone(),
            )
            .await?;
        } else {
            lore_debug!(
                "Path {} exist in repository with different case {}",
                relative_path,
                node_path
            );
            stage_delete(
                current_repository.clone(),
                node_state,
                node_link.node,
                options.node_flags,
                stats.clone(),
                link_tracker.clone(),
            )
            .await?;
        }
    } else {
        lore_debug!("Path {} does not exist in repository", relative_path);
        if !force {
            return Err(StageError::internal(format!(
                "Invalid path {relative_path}"
            )));
        } else {
            lore_debug!("Non-existing path ignored by force flag");
        }
    }

    Ok(NodeLink::default())
}

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
            .emit_excludes(&relative_path, true, filter_mode)
    {
        lore_trace!("Path excluded by filter: {}", relative_path.as_str());
        return Ok(NodeLink::invalid());
    }

    let mut parent_path = relative_path.clone();
    parent_path.pop();

    let base_node = state
        .find_node_link(repository.clone(), parent_path.as_str())
        .await
        .forward::<StageError>(&format!("Invalid path {parent_path}"))?;
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

            let existing_node = block.node(node_index);

            lore_debug!(
                "Found previous node {} flags 0x{:x}",
                found_node,
                existing_node.flags
            );

            let existing_flags = NodeFlags::from_bits_retain(existing_node.flags);

            if node_flags.bitand(NodeFlags::File | NodeFlags::Link)
                == existing_flags.bitand(NodeFlags::File | NodeFlags::Link)
            {
                // Update the existing node
                let block_dirtied = {
                    let mut block_writer = block.write();
                    let existing_node = block_writer.node(node_index);
                    existing_node.address = node.address;
                    existing_node.mode = node.mode;
                    existing_node.size = node.size;
                    block_writer.mark_dirty()
                };
                if block_dirtied {
                    state.block_modified(block.clone(), block_index);
                    state.mark_dirty();
                }

                state
                    .node_mark(
                        repository.clone(),
                        found_node,
                        node_flags.bitand(NodeFlags::StagedBits),
                        true,
                    )
                    .await
                    .forward::<StageError>("Failed to mark node as staged")?;

                if let Some(ref tracker) = link_tracker {
                    tracker.on_node_changed(repository.id);
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
        .node_mark(
            repository.clone(),
            node_id,
            mark_flags,
            true, /* mark dirty */
        )
        .await
        .forward::<StageError>("Failed to mark node as staged")?;

    if let Some(ref tracker) = link_tracker {
        tracker.on_node_changed(repository.id);
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
    repository: Arc<RepositoryContext>,
    state_stage: Arc<State>,
    state_merge: Arc<State>,
    relative_path: RelativePath,
    _stats: Arc<StageStats>,
    _options: StageOptions,
    _link_tracker: Option<Arc<crate::link::LinkTracker>>,
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
        lore_debug!("Merge change: {} (node {})", change.path, change.to.node);

        let absolute_path = change.path.to_absolute_path(repository.require_path()?);

        let mut merge_flags = NodeFlags::StagedMerge;

        if sync::exist_merge_mine_theirs_base(absolute_path.as_path()).await
            || infer_is_conflicted_by_path(absolute_path.as_path())
                .await
                .unwrap_or_default()
        {
            lore_debug!(
                "Merge change filesystem state is conflicted: {}",
                change.path
            );
            merge_flags |= NodeFlags::StagedMergeConflict;
        }

        state_stage
            .node_mark(repository.clone(), change.to.node, merge_flags, true)
            .await
            .forward::<StageError>("Failed to mark node as staged")?;
    }

    for conflict in diff.conflicts.iter() {
        let change = &conflict.1;
        lore_debug!("Merge conflict: {} (node {})", change.path, change.to.node);

        let merge_flags = NodeFlags::StagedMergeConflict;

        state_stage
            .node_mark(repository.clone(), change.to.node, merge_flags, true)
            .await
            .forward::<StageError>("Failed to mark node as staged")?;
    }

    Ok(())
}

/// After staging a filesystem-detected change, also mark the node dirty so that
/// `flagDirty` reflects the working-tree difference: any uncommitted change is
/// dirty, whether it was recorded by `dirty`, `status --scan` or `stage`/
/// `stage --scan`. `node_mark` only sets staged bits (it preserves an existing
/// Dirty but never sets one), so the dirty bit must be applied here.
///
/// Skipped for merge staging — a merge resolution is not a fresh filesystem
/// detection and keeps its own staged/merge flags.
async fn mark_staged_node_dirty(
    repository: Arc<RepositoryContext>,
    state: &Arc<State>,
    node_id: NodeID,
    dirty_flags: NodeFlags,
    node_flags: NodeFlags,
) -> Result<(), StageError> {
    if node_flags.contains(NodeFlags::StagedMerge) {
        return Ok(());
    }
    state
        .node_mark_dirty(repository, node_id, dirty_flags, true)
        .await
        .forward::<StageError>("Failed to mark staged node as dirty")
}

pub(crate) async fn stage_delete(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
) -> Result<(), StageError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;

    let node = block.node(node_index);
    if node.is_staged_delete() {
        return Ok(());
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
        let node_path = state
            .node_path(repository.clone(), node_id)
            .await
            .unwrap_or_default();

        event::LoreEvent::FileStageFile(LoreFileStageFileEventData {
            from_path: LoreString::default(),
            path: node_path.into(),
            action: LoreFileAction::Delete,
        })
        .send();
        stats.file_checked_count.fetch_add(1, Ordering::Relaxed);
    }

    let flags = NodeFlags::StagedDelete | node_flags;

    state
        .node_mark(
            repository.clone(),
            node_id,
            flags,
            true, /* Mark dirty */
        )
        .await
        .forward::<StageError>("Failed to mark node as staged")?;

    mark_staged_node_dirty(
        repository.clone(),
        &state,
        node_id,
        NodeFlags::DirtyDelete,
        node_flags,
    )
    .await?;

    if let Some(ref tracker) = link_tracker {
        tracker.on_node_changed(repository.id);
    }

    // Note that links do not need to recurse into directory, as the subtree exist in
    // the link state tree and not this state tree
    if node.is_directory() {
        let mut child_node_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_iter {
            stage_delete_recurse(
                repository.clone(),
                state.clone(),
                child_node_id,
                node_flags,
                stats.clone(),
                link_tracker.clone(),
            )
            .await?;

            let child_node = state
                .node(repository.clone(), child_node_id)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;
            child_node
                .walk_step(child_node_id, node_id, &mut cycle)
                .forward::<StageError>("Invalid node hierarchy in stage delete walk")?;
            child_node_iter = child_node.sibling();
        }
    }

    Ok(())
}

fn stage_delete_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    node_flags: NodeFlags,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
) -> Pin<Box<dyn Future<Output = Result<(), StageError>> + Send>> {
    Box::pin(stage_delete(
        repository,
        state,
        node_id,
        node_flags,
        stats,
        link_tracker,
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
async fn resolve_case_variant_collisions(
    items: &mut Vec<util::fs::FileListItem>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    directory_node: NodeID,
    absolute_path: &Path,
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
        let all_dirs = group.iter().all(|e| e.metadata.is_dir());

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
                    let from_path = absolute_path.join(&entry.name);
                    let to_path = absolute_path.join(winner_name);
                    lore_debug!(
                        "Case variant collision: unifying {} into {}",
                        from_path.display(),
                        to_path.display()
                    );
                    let _ = util::fs::unify_name_case_rename(&from_path, &to_path);
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_directory(
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
) -> Result<(), StageError> {
    let mut children = state
        .node_children(repository.clone(), directory_node)
        .await
        .forward::<StageError>("Failed to list directory node children")?;

    let mut current_block_index = 0;
    let mut current_block = state
        .block(repository.clone(), current_block_index)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;
    let mut children_name = vec![];
    for child in children.iter() {
        let block_index = NodeBlock::index(*child);
        let node_index = Node::index(*child);
        if block_index != current_block_index {
            current_block_index = block_index;
            current_block = state
                .block(repository.clone(), block_index)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;
        }
        children_name.push(current_block.node(node_index).name_hash);
    }

    let mut file_list =
        util::fs::list_directory(absolute_path.to_path_buf()).internal(&format!(
            "Failed to list directory files in {}",
            absolute_path.to_string_lossy()
        ))?;

    // Collect all filesystem entries, then resolve case variant collisions before staging.
    // On a case-sensitive filesystem, multiple entries differing only in case can coexist
    // (e.g., "Assets" and "assets"). Without resolution, processing both independently causes
    // the second to undo the case rename performed by the first, producing a nondeterministic
    // result depending on iteration order.
    let mut items: Vec<util::fs::FileListItem> = Vec::new();
    while let Some(item) = file_list.recv().await {
        if item.metadata.is_dir() || item.metadata.is_file() {
            items.push(item);
        }
    }

    resolve_case_variant_collisions(
        &mut items,
        repository.clone(),
        state.clone(),
        directory_node,
        absolute_path,
        options,
    )
    .await?;

    let mut directory_tasks = JoinSet::new();
    let mut failure = None;
    // Reusable buffer for layer mount path checks: one allocation per directory
    // instead of per child. Sized for the parent path plus a typical name; grows
    // if a child name is longer.
    let parent_path_str = relative_path.as_str();
    let mut child_path_buf = if layer_mask.is_some() {
        String::with_capacity(parent_path_str.len() + 64)
    } else {
        String::new()
    };
    for item in items {
        if item.metadata.is_dir() {
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

            let node_link = match stage_node_from_metadata(
                repository.clone(),
                state.clone(),
                absolute_path,
                relative_path.clone().freeze(),
                directory_node,
                directory.name.clone(),
                directory.metadata.clone(),
                options,
                stats.clone(),
                link_tracker.clone(),
            )
            .await
            {
                Ok(node_link) => node_link,
                Err(err) => {
                    failure = failure.or(Some(err));
                    break;
                }
            };

            lore_spawn!(directory_tasks, {
                let repository = repository.clone();
                let state = state.clone();
                let stats = stats.clone();
                let mut relative_path = relative_path.clone();
                let mut absolute_path = absolute_path.to_path_buf();
                let link_tracker = link_tracker.clone();
                let layer_mask = layer_mask.clone();
                async move {
                    if !node_link.is_valid() {
                        return Ok(());
                    }

                    let from_node =
                        state
                            .node(repository.clone(), node_link.node)
                            .await
                            .forward::<StageError>("Failed to resolve node path in state")?;

                    if from_node.is_link() {
                        let link = from_node.linked_node();

                        let linked_repository =
                            Arc::new(repository.to_link_context(link.repository).await);
                        let mut linked_state =
                            State::deserialize(linked_repository.clone(), link.revision)
                                .await
                                .forward::<StageError>("Failed to deserialize linked state")?;

                        // Register link with tracker for deferred processing
                        if let Some(ref tracker) = link_tracker {
                            // Check for existing context and reuse state if available
                            linked_state = if let Some(existing_context) =
                                tracker.find_link_context(link.repository)
                            {
                                existing_context.link_state.clone()
                            } else {
                                linked_state.clone()
                            };

                            let link_context = link::LinkContext {
                                link_repository_id: link.repository,
                                link_node_id: node_link.node,
                                parent_repository_id: repository.id,
                                link_path: relative_path.clone(),
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

                        let result = stage_directory_recurse(
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
                        )
                        .await;

                        stats.task_count.fetch_sub(1, Ordering::Release);

                        result
                    } else {
                        // If the directory node was renamed as part of the stage case variation unification,
                        // use the updated unified name to recurse into the correct subdirectory on disk
                        let node_name = state
                            .node_name_ref(repository.clone(), node_link.node)
                            .await
                            .forward::<StageError>("Failed to resolve node name")?;
                        absolute_path.push(&*node_name);
                        relative_path.push(node_name);
                        // Layer mount directories are filtered out by the
                        // mask check at the top of `stage_directory`'s
                        // child-iteration loop, so no mask check is needed here.
                        stats.task_count.fetch_add(1, Ordering::Release);
                        let result = stage_directory_recurse(
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
                        )
                        .await;
                        stats.task_count.fetch_sub(1, Ordering::Release);
                        result
                    }
                }
            });

            for (index, child_name) in children_name.iter().enumerate() {
                if directory.name_hash == *child_name {
                    children.remove(index);
                    children_name.remove(index);
                    break;
                }
            }

            while let Some(result) = directory_tasks.try_join_next() {
                failure = failure.or(result
                    .map_err(|e| StageError::internal_with_context(e, "Failed to join task"))
                    .flatten()
                    .err());
            }
        } else if item.metadata.is_file() {
            let file = item;

            lore_trace!(
                "Staging subnode file: {} ({} {}/)",
                file.name,
                directory_node,
                relative_path.as_str()
            );

            let result = stage_node_from_metadata(
                repository.clone(),
                state.clone(),
                absolute_path,
                relative_path.clone().freeze(),
                directory_node,
                file.name.clone(),
                file.metadata.clone(),
                options,
                stats.clone(),
                link_tracker.clone(),
            )
            .await;
            failure = failure.or(result.err());

            for (index, child_name) in children_name.iter().enumerate() {
                if file.name_hash == *child_name {
                    children.remove(index);
                    children_name.remove(index);
                    break;
                }
            }
        }

        while let Some(result) = directory_tasks.try_join_next() {
            failure = failure.or(result
                .internal("Recursion task failed")
                .map_err(StageError::from)
                .flatten()
                .err());
        }

        if failure.is_some() {
            break;
        }
    }

    // Remaining child nodes no longer exist, stage deletion unless filtered out
    for child in children {
        if failure.is_some() {
            break;
        }
        let node_name = match state
            .node_name_ref(repository.clone(), child)
            .await
            .forward::<StageError>("Failed to resolve node name")
        {
            Ok(node_name) => node_name,
            Err(err) => {
                failure = Some(err);
                break;
            }
        };
        let mut filter_path = relative_path.clone();
        filter_path.push(node_name);
        if repository
            .filter
            .emit_excludes(&filter_path.clone().freeze(), true, FilterMode::Full)
        {
            lore_trace!("Node excluded by filter: {}", filter_path.as_str());
            continue;
        }

        let result = stage_delete(
            repository.clone(),
            state.clone(),
            child,
            options.node_flags,
            stats.clone(),
            link_tracker.clone(),
        )
        .await;
        failure = failure.or(result.err());
    }

    while let Some(result) = directory_tasks.join_next().await {
        failure = failure.or(result
            .internal("Recursion task failed")
            .map_err(StageError::from)
            .flatten()
            .err());
    }
    if let Some(err) = failure {
        return Err(err);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stage_directory_recurse(
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
) -> Pin<Box<dyn Future<Output = Result<(), StageError>> + Send + '_>> {
    Box::pin(stage_directory(
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
    ))
}

#[allow(clippy::too_many_arguments, unused_assignments)]
pub(crate) async fn stage_node_from_metadata(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    base_absolute_path: &Path,
    base_relative_path: RelativePath,
    base_node: NodeID,
    name: String,
    metadata: std::fs::Metadata,
    options: StageOptions,
    stats: Arc<StageStats>,
    link_tracker: Option<Arc<crate::link::LinkTracker>>,
) -> Result<NodeLink, StageError> {
    if base_relative_path.is_empty() && (name.is_empty() || name.as_str() == ".") {
        return Ok(NodeLink {
            node: base_node,
            repository: repository.id,
            revision: state.revision(),
        });
    }

    if name == DOT_URC || name == DOT_LORE {
        lore_trace!("Ignore dot directory {name}");
        return Ok(NodeLink::invalid());
    }
    if name.ends_with(TEMP_FILE_EXTENSION) {
        lore_trace!("Ignore {TEMP_FILE_EXTENSION} file");
        return Ok(NodeLink::invalid());
    }
    if name.ends_with(BASE_SUFFIX) {
        lore_trace!("Ignore {BASE_SUFFIX} file");
        return Ok(NodeLink::invalid());
    }
    if name.ends_with(THEIRS_SUFFIX) {
        lore_trace!("Ignore {THEIRS_SUFFIX} file");
        return Ok(NodeLink::invalid());
    }

    let force = execution_context().globals().force();
    let filter_path = base_relative_path.join(name.as_str());
    if !force
        && repository
            .filter
            .emit_excludes(&filter_path, true, FilterMode::Full)
    {
        lore_trace!("Node excluded by filter: {}", filter_path.as_str());
        return Ok(NodeLink::invalid());
    }

    lore_trace!(
        "Stage node {} (in {}/)",
        base_relative_path.join(name.as_str()),
        base_absolute_path.display()
    );

    let relative_path = base_relative_path;
    let absolute_path = base_absolute_path.to_path_buf();

    let name_hash = hash::hash_string(name.as_str());

    // Find the node
    let node_link = match state
        .find_subnode(repository.clone(), base_node, name_hash)
        .await
    {
        Ok(found_node_id) => {
            // Verify that the found node matches the type in the filesystem
            let block_index = NodeBlock::index(found_node_id);
            let node_index = Node::index(found_node_id);
            let block = state
                .block_with_nametable(repository.clone(), block_index)
                .await
                .forward::<StageError>("Failed deserializing state node block")?;

            let node = block.node(node_index);

            lore_trace!("Found node {} with flags 0x{:x}", found_node_id, node.flags);

            if (node.is_link() && !metadata.is_dir())
                || (node.is_directory() && !metadata.is_dir())
                || (node.is_file() && metadata.is_dir())
            {
                // Type mismatch, stage the current node for delete and add a new new node
                lore_debug!(
                    "Directory/file different, stage delete of node {} and recreate new node",
                    found_node_id
                );
                stage_delete(
                    repository.clone(),
                    state.clone(),
                    found_node_id,
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
        Err(e) if e.is_node_not_found() => NodeLink::invalid(),
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

        let mut node = if metadata.is_dir() {
            Node {
                name_hash,
                ..Default::default()
            }
        } else {
            let size = util::fs::file_size(&metadata);
            Node {
                flags: NodeFlags::File.bits(),
                mode: util::fs::metadata_to_mode(&metadata, 0),
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

        state
            .node_mark(
                repository.clone(),
                node_id,
                NodeFlags::StagedAdd | options.node_flags,
                true, /* mark dirty */
            )
            .await
            .forward::<StageError>("Failed to mark node as staged")?;

        mark_staged_node_dirty(
            repository.clone(),
            &state,
            node_id,
            NodeFlags::DirtyAdd,
            options.node_flags,
        )
        .await?;

        if let Some(ref tracker) = link_tracker {
            tracker.on_node_changed(repository.id);
        }

        lore_debug!("Staged new node {node_id} for {name}");

        if metadata.is_dir() {
            stats
                .directory_checked_count
                .fetch_add(1, Ordering::Relaxed);
            stats.directory_add_count.fetch_add(1, Ordering::Relaxed);
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

        return Ok(NodeLink {
            node: node_id,
            repository: repository.id,
            revision: state.revision(),
        });
    }

    // TODO(vri): UCS-19227 - Links: Handle renaming and mismatching in staging

    let block_index = NodeBlock::index(node_link.node);
    let node_index = Node::index(node_link.node);
    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;

    let mut node = block.node(node_index);
    let mut event_action = None;
    let mut from_path = None;

    // Undelete if deleted
    if node.is_staged_delete() {
        lore_trace!("Undelete node {} before staging", node_link.node);

        let dirtied = {
            let mut block_writer = block.write();
            block_writer.node(node_index).clear_staged_flags();
            block_writer.mark_dirty()
        };
        if dirtied {
            state.block_modified(block.clone(), block_index);
            state.mark_dirty();
        }
        state
            .node_mark(
                repository.clone(),
                node_link.node,
                NodeFlags::StagedModify | options.node_flags,
                true, /* Mark dirty */
            )
            .await
            .forward::<StageError>("Failed to mark node as staged")?;

        mark_staged_node_dirty(
            repository.clone(),
            &state,
            node_link.node,
            NodeFlags::DirtyModify,
            options.node_flags,
        )
        .await?;

        if let Some(ref tracker) = link_tracker {
            tracker.on_node_changed(repository.id);
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
        // Case mismatch handling
        match options.case_change {
            StageCaseChange::Keep => {
                // Keep the state name, update the file system to match
                lore_debug!(
                    "Case mismatch handling, keep node and update file system to match {}",
                    node_name
                );

                let from_path = absolute_path.join(name);

                name = node_name;
                let to_path = absolute_path.join(&name);

                lore_spawn_blocking!(move || {
                    util::fs::unify_name_case_rename(from_path.as_path(), to_path.as_path())
                        .map_err(|e| {
                            format!(
                                "Unable to rename file system path {} to {}: {e}",
                                from_path.display(),
                                to_path.display()
                            )
                        })
                })
                .await
                .map_err(|e| StageError::internal_with_context(e, "Failed to join task"))?
                .map_err(StageError::internal)?;
            }
            StageCaseChange::Rename => {
                // Stage a rename operation, updating the repository to match the file system
                lore_debug!(
                    "Case mismatch handling, staging a move of node from {} to {name}",
                    node_name
                );

                // On case-sensitive file systems the old-cased path may still exist alongside
                // the new one (e.g. both "Assets" and "assets" as separate directories).
                // If so, unify the file system by merging the old into the new so that the
                // stage picks up contents from both.
                // Use filesystem_name_exists to check for an exact-case match rather than
                // Path::exists which is case-insensitive on Windows/macOS.
                let parent_path = absolute_path.clone();
                let old_name = node_name.clone();
                let new_name = name.clone();
                lore_spawn_blocking!(move || {
                    let old_path = parent_path.join(&old_name);
                    let new_path = parent_path.join(&new_name);
                    if util::fs::filesystem_name_exists(parent_path.as_path(), &old_name)
                        && util::fs::filesystem_name_exists(parent_path.as_path(), &new_name)
                    {
                        lore_debug!(
                            "Case rename: old path {} still exists alongside {}, unifying file system",
                            old_path.display(),
                            new_path.display()
                        );
                        util::fs::unify_name_case_rename(old_path.as_path(), new_path.as_path())
                            .map_err(|e| {
                                format!(
                                    "Unable to rename file system path {} to {}: {e}",
                                    old_path.display(),
                                    new_path.display()
                                )
                            })
                    } else {
                        Ok(())
                    }
                })
                .await
                .map_err(|e| StageError::internal_with_context(e, "Failed to join task"))?
                .map_err(StageError::internal)?;

                // Set updated node name
                node.name_hash = name_hash;

                from_path = Some(relative_path.join(&node_name).to_string());
                event_action = Some(LoreFileAction::Move);

                let dirtied = {
                    let mut block_writer = block.write();
                    (node.name_offset, node.name_length) = block_writer
                        .node_name_store(name.as_str(), node.name_offset, node.name_length)
                        .forward::<StageError>("storing node name on stage move")?;
                    *block_writer.node(node_index) = node;
                    block_writer.mark_dirty()
                };
                if dirtied {
                    state.block_modified(block.clone(), block_index);
                    state.mark_dirty();
                }
                state
                    .node_mark(
                        repository.clone(),
                        node_link.node,
                        NodeFlags::StagedMove | options.node_flags,
                        true, /* Mark dirty */
                    )
                    .await
                    .forward::<StageError>("Failed to mark node as staged")?;

                if let Some(ref tracker) = link_tracker {
                    tracker.on_node_changed(repository.id);
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

        if !metadata.is_dir() {
            let stage_file_node = if !node.is_file() {
                lore_debug!("Stage node type change to file for node {}", node_link.node);
                true
            } else {
                let no_force_hash_check = false;
                let node_path = relative_path.join(name.as_str());

                let (mtime, size) = crate::util::fs::file_mtime_and_size(&metadata);
                is_file_modified(
                    repository.clone(),
                    &node,
                    mtime,
                    size,
                    &node_path,
                    no_force_hash_check,
                )
                .await
                .forward::<StageError>("Failed to determine if file is modified")?
                .0
            };

            if stage_file_node {
                node.flags |= NodeFlags::File;
                node.child = 0;
                node.mode = util::fs::metadata_to_mode(&metadata, node.mode);
                node.size = util::fs::file_size(&metadata);
                maybe_content_modified = true;
            } else if was_dirty_add {
                maybe_content_modified = true;
            }
        } else if node.is_file() || node.mode != 0 {
            node.flags &= !NodeFlags::File;
            node.mode = 0;
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
            let dirtied = {
                let mut block_writer = block.write();
                let write_node = block_writer.node(node_index);
                *write_node = node;
                block_writer.mark_dirty()
            };

            if dirtied {
                state.block_modified(block, block_index);
                state.mark_dirty();
            }

            let (staged_flag, dirty_flag) = if was_dirty_add {
                (NodeFlags::StagedAdd, NodeFlags::DirtyAdd)
            } else {
                (NodeFlags::StagedModify, NodeFlags::DirtyModify)
            };

            state
                .node_mark(
                    repository.clone(),
                    node_link.node,
                    staged_flag | options.node_flags,
                    true, /* Mark dirty */
                )
                .await
                .forward::<StageError>("Failed to mark node as staged")?;

            mark_staged_node_dirty(
                repository.clone(),
                &state,
                node_link.node,
                dirty_flag,
                options.node_flags,
            )
            .await?;

            lore_debug!(
                "Staged existing node {} as modified for {}",
                node_link.node,
                relative_path.join(name.as_str())
            );

            if let Some(ref tracker) = link_tracker {
                tracker.on_node_changed(repository.id);
            }

            if event_action.is_none() {
                event_action = Some(if was_dirty_add {
                    LoreFileAction::Add
                } else {
                    LoreFileAction::Keep
                });
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

    if let Some(action) = event_action {
        match action {
            LoreFileAction::Add => {
                if node.is_directory() {
                    stats.directory_add_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.file_add_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            LoreFileAction::Copy => {
                if node.is_directory() {
                    stats.directory_copy_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.file_copy_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            LoreFileAction::Delete => {
                if node.is_directory() {
                    stats.directory_delete_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.file_delete_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            LoreFileAction::Keep => {
                if node.is_directory() {
                    stats.directory_modify_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.file_modify_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            LoreFileAction::Move => {
                if node.is_directory() {
                    stats.directory_move_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.file_move_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if !node.is_directory() {
            event::LoreEvent::FileStageFile(LoreFileStageFileEventData {
                from_path: from_path.into(),
                path: relative_path.join(name.as_str()).into(),
                action,
            })
            .send();
        }
    }

    Ok(node_link)
}

#[derive(Copy, Clone, PartialEq)]
pub(crate) enum MergeParent {
    Mine,
    Theirs,
    CherryPick,
    Revert,
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

/// Recursively collect all file paths with the staged merge flag under a node.
/// If the node is a file with a staged merge, returns it directly.
/// If the node is a directory, walks all descendants and collects matching files.
pub(crate) fn collect_staged_merge_files(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    base_path: RelativePath,
) -> Pin<Box<dyn Future<Output = Result<Vec<RelativePath>, StageError>> + Send>> {
    Box::pin(async move {
        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        let node = block.node(node_index);

        if node.is_link() {
            // TODO(vri): UCS-17955 - Merging and conflict resolution for links
            return Err(StageError::internal(
                "Links not yet implemented, cannot perform actions in other repositories",
            ));
        }

        if node.is_file() {
            if node.is_staged_merge() {
                return Ok(vec![base_path]);
            }
            return Ok(vec![]);
        }

        // Directory: recurse into children
        let mut collected = Vec::new();
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
            let mut child_files =
                collect_staged_merge_files(repository.clone(), state.clone(), child_id, child_path)
                    .await?;
            collected.append(&mut child_files);
        }

        Ok(collected)
    })
}

pub(crate) async fn stage_from_parent_revision(
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
    let mut link_paths: Vec<LoreString> = Vec::new();
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
            link_paths.push(path.clone());
        } else {
            parent_paths.push(path.clone());
        }
    }

    if !link_paths.is_empty() {
        Box::pin(stage_link_paths_from_parent_revision(
            repository.clone(),
            token,
            state.clone(),
            LoreArray::from_vec(link_paths),
            merge_parent,
        ))
        .await?;
    }

    if parent_paths.is_empty() {
        return Ok(());
    }
    let paths = LoreArray::from_vec(parent_paths);

    let state_target_hash = match merge_parent {
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
    };

    let state_target = State::deserialize(repository.clone(), state_target_hash)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

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
            // TODO(vri): UCS-17955 - Merging and conflict resolution for links
            return Err(StageError::internal(
                "Links not yet implemented, cannot perform actions in other repositories",
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
        } else if node.is_directory() {
            let expanded = collect_staged_merge_files(
                repository.clone(),
                state.clone(),
                node_link.node,
                relative_path,
            )
            .await?;
            resolved_paths.extend(expanded);
        } else {
            // TODO(vri): UCS-17955 - Merging and conflict resolution for links
            return Err(StageError::internal(
                "Links not yet implemented, cannot perform actions in other repositories",
            ));
        }
    }

    let stats = Arc::new(StageStats::default());
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
            let had_conflict_files = sync::exist_merge_mine_theirs_base(
                relative_path.to_absolute_path(repository.require_path()?),
            )
            .await;

            async fn unlink_and_stage(
                repository: Arc<RepositoryContext>,
                relative_path: RelativePath,
                state: Arc<State>,
                stats: Arc<StageStats>,
                options: StageOptions,
                preserve_conflict_files: bool,
            ) -> Result<(), StageError> {
                let absolute_path = relative_path.to_absolute_path(repository.require_path()?);
                let _ = util::fs::unlink_recursive(absolute_path.as_path()).await;

                Box::pin(stage_filesystem_path(
                    repository.clone(),
                    state.clone(),
                    repository.require_path()?.to_path_buf(),
                    RelativePathBuf::new(),
                    ROOT_NODE,
                    relative_path.clone(),
                    stats.clone(),
                    options,
                    None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                    None, // No layer mask
                ))
                .await?;

                if !preserve_conflict_files {
                    sync::unlink_merge_mine_theirs_base(absolute_path.as_path()).await;
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
                // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                return Err(StageError::internal(
                    "Links not yet implemented, cannot perform actions in other repositories",
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

            if node.is_file() {
                sync::realize_file(
                    repository.clone(),
                    &relative_path,
                    node,
                    Arc::new(SyncRealizeStats::default()),
                )
                .await
                .forward::<StageError>("Unable to restore path to selected state")?;

                Box::pin(stage_filesystem_path(
                    repository.clone(),
                    state.clone(),
                    repository.require_path()?.to_path_buf(),
                    RelativePathBuf::new(),
                    ROOT_NODE,
                    relative_path.clone(),
                    stats.clone(),
                    options,
                    None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                    None, // No layer mask
                ))
                .await?;

                if !had_conflict_files {
                    sync::unlink_merge_mine_theirs_base(
                        relative_path.to_absolute_path(repository.require_path()?),
                    )
                    .await;
                }
            } else {
                lore_spawn!(tasks, {
                    let repository = repository.clone();
                    let state = state.clone();
                    let state_target = state_target.clone();
                    let stats = stats.clone();
                    async move {
                        stage_from_parent_state(
                            repository.clone(),
                            state,
                            repository.clone(),
                            state_target,
                            relative_path,
                            target_node_link.node,
                            options,
                            stats,
                        )
                        .await
                    }
                });
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
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<StageError>("Failed to serialize staged anchor")?;

    event::LoreEvent::FileStageRevision(LoreFileStageRevisionEventData {
        repository: repository.id,
        revision: signature,
    })
    .send();

    Ok(())
}

/// Stage paths inside linked repositories from one of the link's merge
/// parents (Mine = `parent_self`, Theirs = `parent_other`).
///
/// Mirrors `stage_from_parent_revision`'s per-file logic but operates on
/// each affected link's staged state. After all paths for a link are
/// processed the link state is re-serialized and the parent's pin updated
/// via `link::update_link_pin_by_path`.
pub(crate) async fn stage_link_paths_from_parent_revision(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state: Arc<State>,
    paths: LoreArray<LoreString>,
    merge_parent: MergeParent,
) -> Result<(), StageError> {
    // Group by link repository id so each link's state is loaded once.
    struct LinkGroup {
        link_context: Arc<RepositoryContext>,
        link_path: String,
        link_path_rel: RelativePath,
        link_state_staged: Arc<State>,
        link_state_target: Arc<State>,
        link_state_current: Arc<State>,
        // (mount-prefixed user-path, link-relative path)
        files: Vec<(RelativePath, RelativePath)>,
        link_branch: BranchId,
    }
    let mut groups: Vec<LinkGroup> = Vec::new();

    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path: {path}");
            continue;
        };

        let link_list = state
            .link_list(repository.clone())
            .await
            .forward::<StageError>("Failed to read parent link list")?;

        let mut chosen_link: Option<state::LinkReference> = None;
        let mut chosen_mount: Option<String> = None;
        for link_ref in &link_list {
            let mount = state
                .node_path(repository.clone(), link_ref.local_node)
                .await
                .unwrap_or_default();
            if relative_path.as_str() == mount
                || relative_path.as_str().starts_with(&format!("{mount}/"))
            {
                chosen_link = Some(*link_ref);
                chosen_mount = Some(mount);
                break;
            }
        }
        let (Some(link_ref), Some(mount)) = (chosen_link, chosen_mount) else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path, does not resolve to any link: {path}");
            continue;
        };

        // For a `source_path == "/"` link, the mount-relative path IS the
        // link-relative path. Non-trivial source paths would need the
        // source_path prepended; not yet supported.
        let after_mount = relative_path
            .as_str()
            .strip_prefix(&mount)
            .unwrap_or(relative_path.as_str());
        let link_relative_str = after_mount.strip_prefix('/').unwrap_or(after_mount);
        let link_relative =
            RelativePath::from_str(link_relative_str).unwrap_or_else(|_| RelativePath::new());

        let group_index = groups
            .iter()
            .position(|g| g.link_context.id == link_ref.repository);
        let group = if let Some(idx) = group_index {
            &mut groups[idx]
        } else {
            let link_context = Arc::new(repository.to_link_context(link_ref.repository).await);
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
            let link_state_target = state::State::deserialize(link_context.clone(), target_hash)
                .await
                .forward::<StageError>("Failed to deserialize link target state")?;
            // `link_state_current` is approximated by `parent_self` — only
            // used for `determine_merge_action` so a precise distinction
            // (an actual pre-merge HEAD) isn't needed.
            let link_state_current =
                state::State::deserialize(link_context.clone(), link_state_staged.parent_self())
                    .await
                    .forward::<StageError>("Failed to deserialize link current state")?;

            let link_path_rel =
                RelativePath::from_str(&mount).unwrap_or_else(|_| RelativePath::new());

            groups.push(LinkGroup {
                link_context,
                link_path: mount.clone(),
                link_path_rel,
                link_state_staged,
                link_state_target,
                link_state_current,
                files: Vec::new(),
                link_branch: link_ref.branch,
            });
            groups.last_mut().expect("just pushed")
        };

        let node_link = group
            .link_state_staged
            .find_node_link(group.link_context.clone(), link_relative.as_str())
            .await
            .unwrap_or_default();
        if !node_link.is_valid_or_root() {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid link path, does not exist in link staged state: {path}");
            continue;
        }
        let block = group
            .link_state_staged
            .block(group.link_context.clone(), NodeBlock::index(node_link.node))
            .await
            .forward::<StageError>("Failed deserializing link state node block")?;
        let node = block.node(Node::index(node_link.node));
        if node.is_file() {
            if node.is_staged_merge() {
                group.files.push((relative_path, link_relative));
            } else {
                emit_path_ignore(path.as_str()).await;
                lore_debug!("Ignoring invalid path, link node is not a staged merge: {path}");
            }
        } else if node.is_directory() {
            let expanded = collect_staged_merge_files(
                group.link_context.clone(),
                group.link_state_staged.clone(),
                node_link.node,
                link_relative.clone(),
            )
            .await?;
            for f in expanded {
                let user = group.link_path_rel.join(f.as_str());
                group.files.push((user, f));
            }
        } else {
            return Err(StageError::internal(
                "Link nodes inside a link are not supported",
            ));
        }
    }

    if groups.is_empty() {
        return Ok(());
    }

    let stats = Arc::new(StageStats::default());

    for group in &groups {
        // On-disk anchor for this link. Files inside live at
        // `<base>/<link_relative>`; passing this to `stage_filesystem_path`
        // makes its on-disk lookup mount-prefixed while state staging stays
        // at the link-relative path against the link's state.
        let mount_base_absolute = group
            .link_path_rel
            .to_absolute_path(repository.require_path()?);

        for (mount_path, link_relative) in &group.files {
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
            let target_node_link = group
                .link_state_target
                .find_node_link(group.link_context.clone(), link_relative.as_str())
                .await
                .unwrap_or_default();
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
            // `stage_filesystem_path`. Anchor at the link's mount so the
            // disk side is mount-prefixed while the state side stays
            // link-relative.
            if !target_node_link.is_valid() {
                // No file at the path → `stage_filesystem_path` stages a
                // delete; we just clear the on-disk file first.
                let absolute = link_relative.to_absolute_path(mount_base_absolute.as_path());
                let _ = util::fs::unlink_recursive(absolute.as_path()).await;
                Box::pin(stage_filesystem_path(
                    group.link_context.clone(),
                    group.link_state_staged.clone(),
                    mount_base_absolute.clone(),
                    RelativePathBuf::new(),
                    ROOT_NODE,
                    link_relative.clone(),
                    stats.clone(),
                    options,
                    None,
                    None,
                ))
                .await?;
                sync::unlink_merge_mine_theirs_base(absolute.as_path()).await;
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
                sync::realize_file(
                    group.link_context.clone(),
                    mount_path,
                    node_t,
                    Arc::new(SyncRealizeStats::default()),
                )
                .await
                .forward::<StageError>("Unable to restore link path to selected state")?;

                Box::pin(stage_filesystem_path(
                    group.link_context.clone(),
                    group.link_state_staged.clone(),
                    mount_base_absolute.clone(),
                    RelativePathBuf::new(),
                    ROOT_NODE,
                    link_relative.clone(),
                    stats.clone(),
                    options,
                    None,
                    None,
                ))
                .await?;

                let abs = link_relative.to_absolute_path(mount_base_absolute.as_path());
                sync::unlink_merge_mine_theirs_base(abs.as_path()).await;
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
            &group.link_path,
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
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<StageError>("Failed to store parent staged anchor")?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_from_parent_state(
    repository_current: Arc<RepositoryContext>,
    state_current: Arc<State>,
    repository_target: Arc<RepositoryContext>,
    state_target: Arc<State>,
    relative_path: RelativePath,
    node_id_target: NodeID,
    options: StageOptions,
    stats: Arc<StageStats>,
) -> Result<(), StageError> {
    let block_index = NodeBlock::index(node_id_target);
    let node_index = Node::index(node_id_target);
    let block = state_target
        .block(repository_target.clone(), block_index)
        .await
        .forward::<StageError>("Failed deserializing state node block")?;
    let node = block.node(node_index);

    let node_link_current = state_current
        .find_node_link(repository_current.clone(), relative_path.as_str())
        .await
        .unwrap_or_default();
    let (repository_current, state_current) = node_link_current
        .resolve(repository_current.clone(), state_current.clone())
        .await
        .forward::<StageError>("Failed to resolve node path in state")?;
    let node_current = if node_link_current.is_valid() {
        state_current
            .node(repository_current.clone(), node_link_current.node)
            .await
            .forward::<StageError>("Failed to resolve node path in state")?
    } else {
        Node::default()
    };

    let (mut changes, _) = state::diff_filesystem_subtree(
        repository_target.clone(),
        state_target.clone(),
        repository_current.clone(),
        state_current.clone(),
        relative_path.clone(),
        node.parent,
        node_current.parent,
        FilterMode::Full,
        Arc::new(Vec::new()),
    )
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
        if !change.to.node.is_valid_node_id() {
            // Change is a new file in file system, ignore
            continue;
        }

        let block_index = NodeBlock::index(change.to.node);
        let node_index = Node::index(change.to.node);
        let block = state_current
            .block(change.to.repository.clone(), block_index)
            .await
            .forward::<StageError>("Failed deserializing state node block")?;
        let dirtied = {
            let mut block_writer = block.write();
            block_writer.node(node_index).clear_staged_flags();
            block_writer.mark_dirty()
        };
        if dirtied {
            state_current.block_modified(block, block_index);
            state_current.mark_dirty();
        }
    }

    // Perform the changes
    sync::realize_changes(
        repository_current.clone(),
        changes.clone(),
        None,
        false, /* No dry run */
        false, /* Not a merge */
        Arc::new(SyncRealizeStats::default()),
    )
    .await
    .forward::<StageError>("Unable to restore path to selected state")?;

    // Stage the files that were reverted to the given target state
    let mut tasks = JoinSet::new();
    let dispatch_result: Result<(), StageError> = async {
        for change in changes.iter() {
            let mut relative_path = change.path.clone();
            let absolute_path = relative_path.to_absolute_path(repository_current.require_path()?);
            relative_path.pop();
            let parent_node_link = state_current
                .find_node_link(repository_current.clone(), relative_path.as_str())
                .await
                .forward::<StageError>("Failed to find subnode")?;
            let file_name = if relative_path.is_empty() {
                change.path.to_string()
            } else {
                change.path.as_str()[(relative_path.len() + 1)..].to_string()
            };

            let (repository, state) = parent_node_link
                .resolve(repository_current.clone(), state_current.clone())
                .await
                .forward::<StageError>("Failed to resolve node path in state")?;
            let stats = stats.clone();
            let relative_path = change.path.clone();
            lore_spawn!(tasks, async move {
                let metadata = tokio::fs::metadata(absolute_path.as_path())
                    .await
                    .internal(&format!(
                        "Failed to query file system metadata for path {}",
                        absolute_path.display()
                    ))?;
                stage_node_from_metadata(
                    repository,
                    state,
                    absolute_path.as_path(),
                    relative_path,
                    parent_node_link.node,
                    file_name,
                    metadata,
                    options,
                    stats,
                    None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                )
                .await
            });
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
    final_result
}
