// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::link::LinkContext;
use crate::link::LinkTracker;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
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
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;

/// Data for the event emitted when an unstage operation begins.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageBeginEventData {
    /// Number of paths requested for unstaging.
    pub path_count: usize,
}

/// Running counts of items processed during an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageCountData {
    /// Number of directories that were unstaged.
    pub directory_unstaged_count: u64,
    /// Number of directories that were discarded.
    pub directory_discarded_count: u64,
    /// Number of files that were unstaged.
    pub file_unstaged_count: u64,
    /// Number of files that were discarded.
    pub file_discarded_count: u64,
    /// Total number of items processed.
    pub total_count: u64,
}

/// Data for the progress event emitted periodically during an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageProgressEventData {
    /// Current counts of items processed.
    pub count: LoreFileUnstageCountData,
}

/// Data for the event emitted when an unstage operation completes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageEndEventData {
    /// Final counts of items processed.
    pub count: LoreFileUnstageCountData,
}

/// Data for the event identifying the repository and revision involved in an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageRevisionEventData {
    /// Identifier of the repository.
    pub repository: RepositoryId,
    /// Revision the files are unstaged against.
    pub revision: Hash,
}

/// Data for the event emitted for each file affected by an unstage operation.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileUnstageFileEventData {
    /// Path of the file.
    pub path: LoreString,
    /// Action applied to the file.
    pub action: LoreFileAction,
}

#[error_set]
pub enum UnstageError {
    InvalidArguments,
    InvalidPath,
    InvalidNodeHierarchy,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    RevisionNotFound,
    WriteRequired,
    AddressNotFound,
    Oversized,
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
    BranchNotFound,
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

impl EventError for UnstageError {
    fn translated(&self) -> LoreError {
        match self {
            UnstageError::InvalidArguments(_) | UnstageError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Default)]
pub struct UnstageStats {
    pub directory_unstaged_count: AtomicU64,
    pub directory_discarded_count: AtomicU64,
    pub file_unstaged_count: AtomicU64,
    pub file_discarded_count: AtomicU64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnstageOptions {
    /// Single node, no recursion
    pub single_node: bool,
}

pub async fn unstage(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    options: UnstageOptions,
) -> Result<(), UnstageError> {
    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<UnstageError>("Failed to deserialize current revision anchor")?;
    let Ok(staged_revision) = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .ok_or("no staged revision")
    else {
        lore_debug!("No staged state when unstaging, nothing to do");
        return Ok(());
    };

    let state_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward::<UnstageError>(&format!(
            "Failed to deserialize revision state {current_revision}"
        ))?;

    let state_staged = State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<UnstageError>(&format!(
            "Failed to deserialize revision state {staged_revision}"
        ))?;

    event::LoreEvent::FileUnstageBegin(LoreFileUnstageBeginEventData {
        path_count: paths.len(),
    })
    .send();

    let stats = Arc::new(UnstageStats::default());
    let discard = Arc::new(DashMap::<RepositoryId, Vec<u32>>::new());
    let link_tracker = LinkTracker::new();
    let mut clear = false;
    let is_merge_or_cherry_pick_or_revert = state_staged.is_merge_or_cherry_pick_or_revert();

    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path: {path}");
            continue;
        };

        // If we unstage everything, mark for potential clearing, unless we're in a merge/cherry-pick.
        // The actual deletion check also considers dirty nodes (checked later).
        if !is_merge_or_cherry_pick_or_revert && relative_path.is_empty() {
            clear = true;
        }

        lore_debug!(
            "User path [{}] transformed to relative path [{}] in repository {}",
            path.as_str(),
            relative_path.as_str(),
            repository.path_for_display()
        );

        lore_debug!("Unstage options: {:?}", options);

        let mut task = {
            let repository = repository.clone();
            let state_current = state_current.clone();
            let state_staged = state_staged.clone();
            let discard = discard.clone();
            let stats = stats.clone();
            let link_tracker = link_tracker.clone();
            lore_spawn!(async move {
                unstage_path(
                    repository,
                    state_current,
                    state_staged,
                    relative_path,
                    discard,
                    options,
                    stats,
                    link_tracker,
                )
                .await
            })
        };

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let result = loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let directory_unstaged_count = stats.directory_unstaged_count.load(Ordering::Relaxed);
                    let directory_discarded_count = stats.directory_discarded_count.load(Ordering::Relaxed);
                    let file_unstaged_count = stats.file_unstaged_count.load(Ordering::Relaxed);
                    let file_discarded_count = stats.file_discarded_count.load(Ordering::Relaxed);

                    event::LoreEvent::FileUnstageProgress(LoreFileUnstageProgressEventData {
                        count: LoreFileUnstageCountData {
                            directory_unstaged_count,
                            directory_discarded_count,
                            file_unstaged_count,
                            file_discarded_count,
                            total_count: directory_unstaged_count
                                + directory_discarded_count
                                + file_unstaged_count
                                + file_discarded_count,
                        },
                    }).send();
                },
                result = &mut task => {
                    break result.internal("Recursion task failed").map_err(UnstageError::from)?;
                }
            }
        };

        result?;
    }

    if !clear && !is_merge_or_cherry_pick_or_revert {
        let has_staged = state_staged
            .node_has_staged_children(repository.clone(), ROOT_NODE)
            .await
            .forward::<UnstageError>("Failed to find subnode")?;
        let has_dirty = state_staged
            .node_has_dirty_children(repository.clone(), ROOT_NODE)
            .await
            .forward::<UnstageError>("Failed to find subnode")?;
        clear = !has_staged && !has_dirty;
    };

    // Even if we plan to clear, check for dirty nodes — preserve anchor if dirty remain
    if clear {
        let has_dirty = state_staged
            .node_has_dirty_children(repository.clone(), ROOT_NODE)
            .await
            .forward::<UnstageError>("Failed to find subnode")?;
        if has_dirty {
            lore_debug!("Dirty nodes remain, preserving staged anchor");
            clear = false;
        }
    }

    if clear {
        lore_debug!("Unstaged all, clean by removing staged state anchor");
        if crate::instance::delete_staged_anchor(&repository)
            .await
            .is_err()
        {
            clear = false;
        }
    }

    if !clear {
        discard_nodes(
            repository.clone(),
            state_staged.clone(),
            discard,
            link_tracker.clone(),
        )
        .await?;
    }

    process_link_unstage_updates(
        repository.clone(),
        token,
        state_current.clone(),
        state_staged.clone(),
        link_tracker.clone(),
    )
    .await?;

    let directory_unstaged_count = stats.directory_unstaged_count.load(Ordering::Relaxed);
    let directory_discarded_count = stats.directory_discarded_count.load(Ordering::Relaxed);
    let file_unstaged_count = stats.file_unstaged_count.load(Ordering::Relaxed);
    let file_discarded_count = stats.file_discarded_count.load(Ordering::Relaxed);
    let total_count = directory_unstaged_count
        + directory_discarded_count
        + file_unstaged_count
        + file_discarded_count;

    event::LoreEvent::FileUnstageEnd(LoreFileUnstageEndEventData {
        count: LoreFileUnstageCountData {
            directory_unstaged_count,
            directory_discarded_count,
            file_unstaged_count,
            file_discarded_count,
            total_count,
        },
    })
    .send();

    if total_count == 0 || clear {
        lore_debug!(
            "Nothing unstaged or nothing remains staged, not serializing new staged state and anchor"
        );
        return Ok(());
    }

    state_staged.mark_dirty();

    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<UnstageError>("Failed to serialize staged revision state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<UnstageError>("Failed to serialize staged anchor")?;

    event::LoreEvent::FileUnstageRevision(LoreFileUnstageRevisionEventData {
        repository: repository.id,
        revision: signature,
    })
    .send();

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn unstage_path(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    relative_path: RelativePath,
    discard: Arc<DashMap<RepositoryId, Vec<u32>>>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    lore_debug!(
        "Unstaging path: {}/{}",
        repository.path_for_display(),
        relative_path.as_str(),
    );

    let repository_root = repository.require_path()?.to_path_buf();
    let full_path = if !relative_path.is_empty() {
        // Find file system case variation that corresponds to user given path
        let fs_path = util::fs::filesystem_path(repository_root.as_path(), &relative_path)
            .await
            .unwrap_or(relative_path.as_str().to_string());
        repository_root.join(fs_path.as_str())
    } else {
        repository_root.clone()
    };

    let relative_path = RelativePath::new_from_user_path(
        repository.require_path()?,
        full_path.to_string_lossy().as_ref(),
    )
    .forward::<UnstageError>(&format!("Invalid path {relative_path}"))?;

    let force = execution_context().globals().force();
    if !force
        && repository
            .filter
            .emit_excludes(&relative_path, true, FilterMode::Full)
    {
        lore_trace!("Path excluded by filter: {}", relative_path.as_str());
        return Ok(());
    }

    // Path is repository root, unstage as directory
    if relative_path.is_empty() {
        if options.single_node {
            return Ok(());
        }

        lore_debug!("Unstaging the repository from root");

        return unstage_directory(
            repository.clone(),
            state_current.clone(),
            state_staged.clone(),
            relative_path.clone(),
            ROOT_NODE,
            discard.clone(),
            options,
            stats.clone(),
            link_tracker.clone(),
        )
        .await;
    }

    let node_name = relative_path.name().to_string();

    unstage_node(
        repository,
        state_current,
        state_staged,
        relative_path,
        node_name,
        discard,
        options,
        stats,
        link_tracker,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn unstage_directory(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    directory_path: RelativePath,
    directory_node: NodeID,
    discard: Arc<DashMap<RepositoryId, Vec<u32>>>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    lore_trace!(
        "Unstaging directory: path='{}', node={}, repository={}",
        directory_path.as_str(),
        directory_node,
        repository.id
    );

    let children = state_staged
        .node_children(repository.clone(), directory_node)
        .await
        .forward::<UnstageError>("Failed to list directory node children")?;

    // TODO(vri): UCS-12399 - Convert to separate tasks
    for child in children.iter() {
        let child_node_name = state_staged
            .node_name_clone(repository.clone(), *child)
            .await
            .forward::<UnstageError>("Failed to get node name")?;

        let child_node_path = directory_path.join(child_node_name.as_str());

        lore_trace!(
            "Unstaging child node: node={}, name='{}', path='{}' in repository {}",
            *child,
            child_node_name.as_str(),
            child_node_path.as_str(),
            repository.id
        );

        unstage_node_recurse(
            repository.clone(),
            state_current.clone(),
            state_staged.clone(),
            child_node_path,
            child_node_name,
            discard.clone(),
            options,
            stats.clone(),
            link_tracker.clone(),
        )
        .await?;
    }

    Ok(())
}

async fn unstage_parent_chain(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    starting_parent: NodeID,
    link_tracker: Arc<LinkTracker>,
    stop_at_staged_add: bool,
) -> Result<(), UnstageError> {
    let mut parent_node_id = starting_parent;
    while !state_staged
        .node_has_staged_children(repository.clone(), parent_node_id)
        .await
        .forward::<UnstageError>("Failed to check node children")?
    {
        lore_trace!("Unstage parent node {parent_node_id}");

        let parent_block_index = NodeBlock::index(parent_node_id);
        let parent_node_index = Node::index(parent_node_id);
        let parent_block = state_staged
            .block(repository.clone(), parent_block_index)
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;
        let parent = parent_block.node(parent_node_index);

        // A parent that is itself a staged ADD is a legitimate independent
        // staged node (not merely staged to carry a descendant), so leave
        // it staged — unstaging a child must not unstage the parent.
        if stop_at_staged_add && parent.is_staged_add() {
            break;
        }

        let dirtied = {
            let mut block_writer = parent_block.write();
            let parent_node = block_writer.node(parent_node_index);
            if stop_at_staged_add {
                parent_node.clear_staged_flags();
            } else {
                parent_node.clear_all_change_flags();
            }
            block_writer.mark_dirty()
        };

        link_tracker.on_node_changed(repository.id);

        if dirtied {
            state_staged.block_modified(parent_block.clone(), parent_block_index);
            state_staged.mark_dirty();
        }

        if parent_node_id == ROOT_NODE {
            break;
        }

        parent_node_id = parent.parent;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn unstage_node(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    node_path: RelativePath,
    name: String,
    discard: Arc<DashMap<RepositoryId, Vec<u32>>>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    if name.is_empty() || name.as_str() == "." {
        return Ok(());
    }

    if name == DOT_URC || name == DOT_LORE {
        lore_debug!("Ignore dot directory {name}");
        return Ok(());
    }

    if !execution_context().globals().force()
        && repository
            .filter
            .emit_excludes(&node_path, true, FilterMode::Full)
    {
        lore_debug!("Node excluded by filter: {}", node_path.as_str());
        return Ok(());
    }

    lore_trace!(
        "Unstage node '{}' at path '{}' in repository {}",
        name.as_str(),
        node_path.as_str(),
        repository.id
    );

    // Find the node
    let node_link = match state_staged
        .find_node_link(repository.clone(), node_path.as_str())
        .await
    {
        Ok(found_node_link) => {
            let mut current_repository = repository.clone();

            let current_state_staged = if found_node_link.repository != repository.id {
                lore_debug!(
                    "Transition into linked repository: from {} to {}, node={}",
                    repository.id,
                    found_node_link.repository,
                    found_node_link.node
                );

                current_repository =
                    Arc::new(repository.to_link_context(found_node_link.repository).await);

                State::deserialize(current_repository.clone(), found_node_link.revision)
                    .await
                    .forward::<UnstageError>(&format!(
                        "Failed to deserialize revision state {}",
                        found_node_link.revision
                    ))?
            } else {
                state_staged.clone()
            };

            let current_state = if found_node_link.repository != repository.id {
                State::deserialize(current_repository.clone(), found_node_link.revision)
                    .await
                    .forward::<UnstageError>(&format!(
                        "Failed to deserialize revision state {}",
                        found_node_link.revision
                    ))?
            } else {
                state_current.clone()
            };

            if found_node_link.repository != repository.id {
                // Find the parent repository's link node
                let parent_link_node_id = state_staged
                    .find_link_parent_node(
                        repository.clone(),
                        node_path.as_str(),
                        found_node_link.repository,
                    )
                    .await
                    .forward::<UnstageError>("Failed to find subnode")?;

                let link_context = crate::link::LinkContext {
                    link_repository_id: found_node_link.repository,
                    link_node_id: parent_link_node_id,
                    parent_repository_id: repository.id,
                    link_path: node_path.clone().into_buf(),
                    link_state: current_state_staged.clone(),
                };

                link_tracker.add_link(link_context);
            }

            let found_node_id = found_node_link.node;

            // Get the actual path of the node in the current repository context
            let resolved_node_path = if found_node_link.repository != repository.id {
                current_state_staged
                    .node_path(current_repository.clone(), found_node_id)
                    .await
                    .unwrap_or_else(|_| node_path.to_string())
            } else {
                node_path.to_string()
            };

            let block_index = NodeBlock::index(found_node_id);
            let node_index = Node::index(found_node_id);

            let block = current_state_staged
                .block_with_nametable(current_repository.clone(), block_index)
                .await
                .forward::<UnstageError>("Failed deserializing state node block")?;
            let mut node = block.node(node_index);

            lore_debug!("Found node {found_node_id}");

            if !node.is_staged() && !execution_context().globals().force() {
                lore_debug!("Node {found_node_id} is not staged");
                return Ok(());
            }

            // Unstage clears the stage flags but preserves the dirty flag: a
            // staged ADD therefore survives as a dirty add (and a directory add
            // demotes its whole subtree likewise), rather than being discarded.
            // The staged anchor is not removed here — the end-of-unstage logic
            // removes it only when no staged AND no dirty nodes remain. A
            // staged-add LINK is still discarded so its registry entry is cleaned.
            let mut keep_as_dirty_add = false;
            if node.is_staged_add() {
                if node.is_link() {
                    lore_debug!("Discarding staged-add link {found_node_id}");

                    link::reset::reset_staged_add_link(
                        current_repository.clone(),
                        current_state.clone(),
                        current_state_staged.clone(),
                        found_node_id,
                        node,
                        node_path.clone(),
                    )
                    .await
                    .forward::<UnstageError>("Failed to reset staged-add link")?;

                    stats.file_discarded_count.fetch_add(1, Ordering::Relaxed);
                    event::LoreEvent::FileUnstageFile(LoreFileUnstageFileEventData {
                        path: LoreString::from(&resolved_node_path),
                        action: LoreFileAction::Delete,
                    })
                    .send();

                    {
                        let mut block_writer = block.write();
                        block_writer.node(node_index).clear_staged_flags();
                        if block_writer.mark_dirty() {
                            current_state_staged.block_modified(block.clone(), block_index);
                        }
                    }

                    discard
                        .entry(current_repository.id)
                        .or_default()
                        .push(found_node_id);

                    unstage_parent_chain(
                        current_repository.clone(),
                        current_state_staged.clone(),
                        node.parent,
                        link_tracker.clone(),
                        false,
                    )
                    .await?;

                    return Ok(());
                }

                lore_debug!("Unstaging staged add {found_node_id}: keep as dirty add");

                // Clearing the staged flags on a dirty node preserves Dirty +
                // action bits, leaving a plain dirty add.
                {
                    let mut block_writer = block.write();
                    block_writer.node(node_index).clear_staged_flags();
                    if block_writer.mark_dirty() {
                        current_state_staged.block_modified(block.clone(), block_index);
                        current_state_staged.mark_dirty();
                    }
                }
                node.clear_staged_flags();
                link_tracker.on_node_changed(current_repository.id);

                if node.is_directory() {
                    demote_subnodes_to_dirty(
                        current_repository.clone(),
                        current_state_staged.clone(),
                        found_node_id,
                        stats.clone(),
                    )
                    .await?;
                }

                keep_as_dirty_add = true;
            }

            // Default values for the non-keep paths below; only read for link
            // nodes, which always go through the `!keep_as_dirty_add` branch.
            let mut was_staged_delete = false;
            let mut current_node = node;
            if !keep_as_dirty_add {
                let current_block = current_state
                    .block(current_repository.clone(), block_index)
                    .await
                    .forward::<UnstageError>("Failed deserializing state node block")?;

                current_node = current_block.node(node_index);

                was_staged_delete = node.is_staged_delete();

                let is_staged_update_link =
                    link::is_staged_pin_change(&node, &current_node, current_repository.clone())
                        .await
                        .forward::<UnstageError>("Failed to check link staged pin change")?;

                let was_modified = if node.is_staged_modify() && node.is_file() {
                    node.flags |= NodeFlags::File;
                    node.child = current_node.child;
                    node.mode = current_node.mode;
                    node.size = current_node.size;
                    true
                } else if is_staged_update_link {
                    link::reset::reset_staged_update_link(
                        current_repository.clone(),
                        current_state.clone(),
                        current_state_staged.clone(),
                        found_node_id,
                        node,
                        current_node,
                        node_path.clone(),
                    )
                    .await
                    .forward::<UnstageError>("Failed to reset staged-update link")?;

                    node.flags |= NodeFlags::Link;
                    node.address.hash = current_node.address.hash;
                    node.child = current_node.child;
                    true
                } else {
                    false
                };

                node.clear_staged_flags();

                link_tracker.on_node_changed(current_repository.id);

                let dirtied = {
                    let mut block_writer = block.write();
                    {
                        let write_node = block_writer.node(node_index);
                        if was_modified {
                            *write_node = node;
                        } else {
                            write_node.flags = node.flags;
                        }
                    }
                    block_writer.mark_dirty()
                };

                if dirtied {
                    current_state_staged.block_modified(block.clone(), block_index);
                    current_state_staged.mark_dirty();
                }

                // After clearing Staged, re-check filesystem: clear Dirty if file matches current
                // revision. If still differs, preserve Dirty.
                if node.is_dirty() && node.is_file() {
                    let current_repository_root = current_repository.require_path()?;
                    let absolute_path = current_repository_root.join(resolved_node_path.as_str());
                    if let Ok(file_metadata) = tokio::fs::metadata(&absolute_path).await {
                        let node_path_rel = crate::util::path::RelativePath::new_from_user_path(
                            current_repository_root,
                            absolute_path.to_string_lossy().as_ref(),
                        )
                        .forward::<UnstageError>("Invalid path")?;
                        let (file_mtime, file_size) =
                            crate::util::fs::file_mtime_and_size(&file_metadata);
                        let (file_modified, _) = crate::state::is_file_modified(
                            current_repository.clone(),
                            &current_node,
                            file_mtime,
                            file_size,
                            &node_path_rel,
                            true, /* Force hash check */
                        )
                        .await
                        .forward::<UnstageError>("Failed to check if file was modified")?;

                        if !file_modified {
                            // File matches current revision — clear Dirty
                            node.clear_dirty_flags();
                            let dirtied = {
                                let mut block_writer = block.write();
                                block_writer.node(node_index).flags = node.flags;
                                block_writer.mark_dirty()
                            };
                            if dirtied {
                                current_state_staged.block_modified(block.clone(), block_index);
                                current_state_staged.mark_dirty();
                            }
                        }
                    }
                    // If file doesn't exist on disk — could be a delete, preserve Dirty
                }
            }

            unstage_parent_chain(
                current_repository.clone(),
                current_state_staged.clone(),
                node.parent,
                link_tracker.clone(),
                true,
            )
            .await?;

            // Dirty parent cleanup: if the node is no longer dirty, walk up and clear
            // Dirty on parents that have no remaining dirty children
            if !node.is_dirty() {
                let mut dirty_parent_id = node.parent;
                while dirty_parent_id.is_valid_node_id() {
                    if current_state_staged
                        .node_has_dirty_children(current_repository.clone(), dirty_parent_id)
                        .await
                        .forward::<UnstageError>("Failed to check node children")?
                    {
                        break;
                    }

                    let dp_block_index = NodeBlock::index(dirty_parent_id);
                    let dp_node_index = Node::index(dirty_parent_id);
                    let dp_block = current_state_staged
                        .block(current_repository.clone(), dp_block_index)
                        .await
                        .forward::<UnstageError>("Failed deserializing state node block")?;
                    let dp_node = dp_block.node(dp_node_index);

                    let dirtied = {
                        let mut block_writer = dp_block.write();
                        block_writer.node(dp_node_index).clear_dirty_flags();
                        block_writer.mark_dirty()
                    };

                    if dirtied {
                        current_state_staged.block_modified(dp_block.clone(), dp_block_index);
                        current_state_staged.mark_dirty();
                    }

                    if dirty_parent_id == ROOT_NODE {
                        break;
                    }

                    dirty_parent_id = dp_node.parent;
                }
            }

            if node.is_link() {
                lore_debug!(
                    "Processing link node {found_node_id} at path '{}' in repository {}",
                    node_path.as_str(),
                    current_repository.id
                );

                let link_metadata = node.linked_node();

                let linked_repository = Arc::new(
                    current_repository
                        .to_link_context(link_metadata.repository)
                        .await,
                );

                let linked_state =
                    State::deserialize(linked_repository.clone(), link_metadata.revision)
                        .await
                        .forward::<UnstageError>("Failed to unstage link nodes")?;

                let link_context = LinkContext {
                    link_repository_id: link_metadata.repository,
                    link_node_id: found_node_id,
                    parent_repository_id: current_repository.id,
                    link_path: node_path.clone().into_buf(),
                    link_state: linked_state.clone(),
                };

                link_tracker.add_link(link_context);

                // If we're unstaging a link removal, restore the link registry
                // entry and re-materialize the linked content on disk.
                if was_staged_delete {
                    link::reset::reset_staged_remove_link(
                        current_repository.clone(),
                        current_state.clone(),
                        current_state_staged.clone(),
                        found_node_id,
                        current_node,
                        node_path.clone(),
                    )
                    .await
                    .forward::<UnstageError>("Failed to reset staged-remove link")?;

                    node.clear_dirty_flags();
                    let dirtied = {
                        let mut block_writer = block.write();
                        block_writer.node(node_index).clear_dirty_flags();
                        block_writer.mark_dirty()
                    };
                    if dirtied {
                        current_state_staged.block_modified(block.clone(), block_index);
                        current_state_staged.mark_dirty();
                    }
                }

                if options.single_node {
                    lore_debug!("Single node option set, skipping link directory processing");
                    return Ok(());
                }

                if current_node.is_link() {
                    let current_link_metadata = current_node.linked_node();

                    let linked_state_current = State::deserialize(
                        linked_repository.clone(),
                        current_link_metadata.revision,
                    )
                    .await
                    .forward::<UnstageError>("Failed to unstage link nodes")?;

                    let linked_node_path = linked_state
                        .node_path(linked_repository.clone(), node.child)
                        .await
                        .unwrap_or_default();

                    let linked_node_path = RelativePath::new_from_initial_path(linked_node_path)
                        .forward::<UnstageError>("Failed to find subnode")?;

                    unstage_directory(
                        linked_repository.clone(),
                        linked_state_current.clone(),
                        linked_state.clone(),
                        linked_node_path,
                        node.child,
                        discard.clone(),
                        options,
                        stats.clone(),
                        link_tracker.clone(),
                    )
                    .await?;
                }
            } else if node.is_directory() {
                if options.single_node {
                    return Ok(());
                }

                let resolved_path = RelativePath::new_from_initial_path(resolved_node_path.clone())
                    .forward::<UnstageError>("Failed to find subnode")?;

                unstage_directory(
                    current_repository.clone(),
                    current_state.clone(),
                    current_state_staged.clone(),
                    resolved_path,
                    found_node_id,
                    discard.clone(),
                    options,
                    stats.clone(),
                    link_tracker.clone(),
                )
                .await?;

                stats
                    .directory_unstaged_count
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                stats.file_unstaged_count.fetch_add(1, Ordering::Relaxed);
                event::LoreEvent::FileUnstageFile(LoreFileUnstageFileEventData {
                    path: LoreString::from(&node_path),
                    action: LoreFileAction::Keep,
                })
                .send();
            }

            found_node_link
        }
        Err(e) if e.is_node_not_found() => NodeLink::invalid(),
        Err(err) => Err(err).forward::<UnstageError>("Failed to find subnode")?,
    };

    // We don't care about invalid node links
    if !node_link.is_valid() {
        lore_debug!("Node {name} with path {} not valid", node_path.as_str());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn unstage_node_recurse<'a>(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    node_path: RelativePath,
    name: String,
    discard: Arc<DashMap<RepositoryId, Vec<u32>>>,
    options: UnstageOptions,
    stats: Arc<UnstageStats>,
    link_tracker: Arc<LinkTracker>,
) -> Pin<Box<dyn Future<Output = Result<(), UnstageError>> + Send + 'a>> {
    Box::pin(unstage_node(
        repository,
        state_current,
        state_staged,
        node_path,
        name,
        discard,
        options,
        stats,
        link_tracker,
    ))
}

async fn process_link_unstage_updates(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    if !link_tracker.has_modifications() {
        lore_debug!("No link modifications detected, skipping link unstage updates");
        return Ok(());
    }

    let unstage_links = link_tracker.get_links_needing_rehash();

    for link_context in unstage_links.iter() {
        let current_link_ref = state_current
            .link_find(
                repository.clone(),
                link_context.link_repository_id,
                link_context.link_node_id,
            )
            .await
            .forward::<UnstageError>("Failed to unstage link nodes")?;

        let staged_link_ref = state_staged
            .link_find(
                repository.clone(),
                link_context.link_repository_id,
                link_context.link_node_id,
            )
            .await
            .forward::<UnstageError>("Failed to unstage link nodes")?;

        let new_signature = link::reserialize_tracked_link(
            &state_staged,
            repository.clone(),
            token,
            link_context,
            current_link_ref.signature,
            staged_link_ref.branch,
        )
        .await
        .forward::<UnstageError>("Failed to unstage link nodes")?;

        lore_debug!(
            "Updating link node hash: node={}, old_hash={}, new_hash={}",
            link_context.link_node_id,
            staged_link_ref.signature,
            new_signature
        );
    }

    lore_debug!(
        "All {} link unstage updates completed successfully",
        unstage_links.len()
    );

    Ok(())
}

/// Demote a staged-add subtree to dirty in place: clear the staged flags on every
/// descendant (preserving Dirty + action bits), so an unstaged directory add
/// survives as a tree of dirty adds rather than being discarded. Each demoted node
/// is counted as unstaged and (for files) emits a Keep unstage event, matching the
/// normal per-node unstage path. Self-contained — it walks the subtree directly and
/// never re-enters `unstage_node`.
fn demote_subnodes_to_dirty<'a>(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    stats: Arc<UnstageStats>,
) -> Pin<Box<dyn Future<Output = Result<(), UnstageError>> + Send + 'a>> {
    Box::pin(async move {
        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<UnstageError>("Failed deserializing state node block")?;
        let node = block.node(node_index);

        let mut child_node_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_iter {
            let child_block_index = NodeBlock::index(child_node_id);
            let child_node_index = Node::index(child_node_id);
            let child_block = state
                .block(repository.clone(), child_block_index)
                .await
                .forward::<UnstageError>("Failed deserializing state node block")?;
            let child_node = child_block.node(child_node_index);
            child_node
                .walk_step(child_node_id, node_id, &mut cycle)
                .forward::<UnstageError>("Invalid node hierarchy in unstage walk")?;
            let next_child_sibling = child_node.sibling();

            // Clear staged flags (preserves Dirty + action bits when Dirty is set).
            let dirtied = {
                let mut block_writer = child_block.write();
                block_writer.node(child_node_index).clear_staged_flags();
                block_writer.mark_dirty()
            };
            if dirtied {
                state.block_modified(child_block.clone(), child_block_index);
                state.mark_dirty();
            }

            if child_node.is_directory() {
                stats
                    .directory_unstaged_count
                    .fetch_add(1, Ordering::Relaxed);
                demote_subnodes_to_dirty(
                    repository.clone(),
                    state.clone(),
                    child_node_id,
                    stats.clone(),
                )
                .await?;
            } else {
                stats.file_unstaged_count.fetch_add(1, Ordering::Relaxed);
                let child_path = state
                    .node_path(repository.clone(), child_node_id)
                    .await
                    .unwrap_or_default();
                event::LoreEvent::FileUnstageFile(LoreFileUnstageFileEventData {
                    path: child_path.into(),
                    action: LoreFileAction::Keep,
                })
                .send();
            }

            child_node_iter = next_child_sibling;
        }

        Ok(())
    })
}

// TODO(vri): UCS-12299 - Unify codepaths to discard nodes
async fn discard_nodes(
    base_repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    discard_map: Arc<DashMap<RepositoryId, Vec<u32>>>,
    link_tracker: Arc<LinkTracker>,
) -> Result<(), UnstageError> {
    if discard_map.is_empty() {
        return Ok(());
    }

    for entry in discard_map.iter() {
        let (repository_id, node_ids) = (entry.key(), entry.value());

        if node_ids.is_empty() {
            continue;
        }

        if *repository_id == base_repository.id {
            // Process in base repository context
            discard_nodes_for_repository(
                base_repository.clone(),
                state_staged.clone(),
                node_ids.clone(),
            )
            .await?;
        } else {
            // Get linked state from link tracker
            if let Some(linked_context) = link_tracker.find_link_context(*repository_id) {
                let linked_repository =
                    Arc::new(base_repository.to_link_context(*repository_id).await);

                discard_nodes_for_repository(
                    linked_repository,
                    linked_context.link_state.clone(),
                    node_ids.clone(),
                )
                .await?;
            }
        }
    }

    Ok(())
}

async fn discard_nodes_for_repository(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_ids: Vec<u32>,
) -> Result<(), UnstageError> {
    lore_debug!(
        "Discarding {} nodes in repository {}",
        node_ids.len(),
        repository.id
    );

    for node_id in node_ids.iter() {
        lore_debug!("Discarding node {} patch", *node_id);

        state::node_discard_patch(state.clone(), repository.clone(), *node_id, {
            move |discarded_node_id, _flags| {
                lore_debug!("Discarded node {discarded_node_id} with patching");
            }
        })
        .await
        .forward::<UnstageError>("Failed to discard node")?;
    }

    Ok(())
}
