// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_error_set::prelude::*;

use crate::errors::*;
use crate::filter::FilterMode;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_trace;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::util::path::RelativePath;

#[error_set]
pub enum DirtyError {
    AddressNotFound,
    InvalidArguments,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    RevisionNotFound,
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

impl crate::event::EventError for DirtyError {}

#[derive(Default)]
pub struct DirtyStats {
    pub modify_count: std::sync::atomic::AtomicU64,
    pub add_count: std::sync::atomic::AtomicU64,
    pub delete_count: std::sync::atomic::AtomicU64,
}

/// Mark files as dirty in the staged state. Action is determined by checking filesystem existence
/// and current revision state:
/// - File on disk + in revision = Modify
/// - File on disk + not in revision = Add (creates node)
/// - Not on disk + in revision = Delete (recurses for directories)
/// - Not on disk + not in revision + Dirty+Add in staged = Remove node (reverted add)
/// - Not on disk + not in revision + not in staged = Ignore
///
/// Respects ignore and view filters (same as stage).
pub async fn dirty(
    repository: Arc<RepositoryContext>,
    paths: LoreArray<LoreString>,
) -> Result<Hash, DirtyError> {
    let mut relative_paths: Vec<RelativePath> = Vec::with_capacity(paths.as_slice().len());
    for path in paths.as_slice().iter() {
        if let Ok(rp) = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        {
            relative_paths.push(rp);
        } else {
            emit_path_ignore(path.as_str()).await;
            lore_trace!("Ignoring invalid path: {path}");
        }
    }

    dirty_relative_paths(repository, relative_paths).await
}

/// Apply dirty markers for already-resolved relative paths. Skips the
/// absolute → relative conversion needed by [`dirty`]'s public API so
/// callers that already work with `RelativePath` (e.g. `commit_impl`
/// replaying tracked paths against a freshly committed revision) can hand
/// them in directly.
pub(crate) async fn dirty_relative_paths(
    repository: Arc<RepositoryContext>,
    paths: Vec<RelativePath>,
) -> Result<Hash, DirtyError> {
    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or_else(|| state_current.clone());

    let stats = Arc::new(DirtyStats::default());
    let force = execution_context().globals().force();

    for relative_path in paths.iter() {
        if !force
            && repository
                .filter
                .emit_excludes(relative_path, true, FilterMode::Full)
        {
            lore_trace!("Path excluded by filter: {}", relative_path.as_str());
            continue;
        }

        dirty_path(
            repository.clone(),
            state_current.clone(),
            state.clone(),
            relative_path,
            stats.clone(),
        )
        .await?;
    }

    let modify = stats.modify_count.load(Ordering::Relaxed);
    let add = stats.add_count.load(Ordering::Relaxed);
    let delete = stats.delete_count.load(Ordering::Relaxed);
    let total = modify + add + delete;

    lore_debug!("Dirtied {total} paths: {modify} modified, {add} added, {delete} deleted");

    if total == 0 {
        return Ok(state.revision());
    }

    // Staged states should have no revision number
    state.set_revision_number(0);
    state.set_parent_self(current_revision);

    // If this is the first modification of the state (cloned from current), reset other parent
    if state.revision() == current_revision {
        state.set_parent_other(Hash::default());
        state.set_metadata_hash(Hash::default());
    }

    let token = repository
        .try_write_token()
        .expect("dirty requires write access");
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?;

    if signature != current_revision {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<DirtyError>("Failed to serialize staged anchor")?;
    }

    Ok(signature)
}

/// Process a single path and determine the dirty action.
async fn dirty_path(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    relative_path: &RelativePath,
    stats: Arc<DirtyStats>,
) -> Result<(), DirtyError> {
    let absolute_path = relative_path.to_absolute_path(repository.require_path()?);
    let metadata = tokio::fs::metadata(&absolute_path).await;
    let exists_on_disk = metadata.is_ok();
    let is_dir = metadata.as_ref().is_ok_and(|m| m.is_dir());

    // Check if path exists in current revision
    let in_current_revision = state_current
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .is_ok();

    // Check if path exists in staged tree (for reverted add detection)
    let staged_link = state_staged
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .ok();

    if exists_on_disk && is_dir {
        // A new directory is itself an add; mark it so an empty one is tracked
        // even though the child recursion finds no files to anchor it.
        if !in_current_revision && staged_link.is_none() {
            dirty_add_directory(
                repository.clone(),
                state_staged.clone(),
                relative_path,
                stats.clone(),
            )
            .await?;
        }
        // Directory on disk -> recurse children
        lore_trace!("Dirty directory recurse: {}", relative_path.as_str());
        dirty_directory(
            repository.clone(),
            state_current.clone(),
            state_staged.clone(),
            relative_path,
            &absolute_path,
            stats.clone(),
        )
        .await?;
    } else if exists_on_disk && in_current_revision {
        // File on disk + in revision -> Modify
        lore_trace!("Dirty modify: {}", relative_path.as_str());
        let link = staged_link
            .or(state_current
                .find_node_link(repository.clone(), relative_path.as_str())
                .await
                .ok())
            .ok_or_else(|| DirtyError::internal("Node not found for dirty modify"))?;

        state_staged
            .node_mark_dirty(repository.clone(), link.node, NodeFlags::DirtyModify, true)
            .await
            .forward::<DirtyError>("Failed to mark node as dirty")?;

        stats.modify_count.fetch_add(1, Ordering::Relaxed);
    } else if exists_on_disk {
        // Skip when already tracked so a repeated dirty doesn't duplicate the node.
        if staged_link.is_none() {
            lore_trace!("Dirty add: {}", relative_path.as_str());
            dirty_add(
                repository.clone(),
                state_staged.clone(),
                relative_path,
                stats.clone(),
            )
            .await?;
        }
    } else if in_current_revision {
        // Not on disk + in revision -> Delete
        lore_trace!("Dirty delete: {}", relative_path.as_str());
        let link = staged_link
            .or(state_current
                .find_node_link(repository.clone(), relative_path.as_str())
                .await
                .ok())
            .ok_or_else(|| DirtyError::internal("Node not found for dirty delete"))?;

        dirty_delete(
            repository.clone(),
            state_staged.clone(),
            link.node,
            relative_path,
            stats.clone(),
        )
        .await?;
    } else if let Some(link) = staged_link {
        // Not on disk + not in revision + exists in staged tree
        let node = state_staged
            .node(repository.clone(), link.node)
            .await
            .forward::<DirtyError>("Failed to get staged node")?;
        if node.is_dirty_add() {
            lore_trace!(
                "Dirty reverted add, discarding node: {}",
                relative_path.as_str()
            );
            // Clear dirty flags so the node is no longer marked
            let block_index = NodeBlock::index(link.node);
            let node_index = Node::index(link.node);
            let block = state_staged
                .block(repository.clone(), block_index)
                .await
                .forward::<DirtyError>("Failed to get block")?;
            {
                let mut writer = block.write();
                writer.node(node_index).clear_all_change_flags();
                writer.mark_dirty();
            }
            state_staged.block_modified(block.clone(), block_index);
            state_staged.mark_dirty();

            // Discard the node from the tree (unlink from parent, reclaim slot)
            crate::state::node_discard_patch(
                state_staged.clone(),
                repository.clone(),
                link.node,
                |_discarded_node_id, _flags| {},
            )
            .await
            .forward::<DirtyError>("Failed to discard reverted dirty add node")?;

            // Clean up parent dirty flags if no dirty children remain
            let parent_id = node.parent;
            if parent_id.is_valid_or_root_node_id()
                && !state_staged
                    .node_has_dirty_children(repository.clone(), parent_id)
                    .await
                    .forward::<DirtyError>("Failed to check dirty children")?
            {
                let parent_block_index = NodeBlock::index(parent_id);
                let parent_node_index = Node::index(parent_id);
                let parent_block = state_staged
                    .block(repository.clone(), parent_block_index)
                    .await
                    .forward::<DirtyError>("Failed to get parent block")?;
                {
                    let mut writer = parent_block.write();
                    writer.node(parent_node_index).clear_dirty_flags();
                    writer.mark_dirty();
                }
                state_staged.block_modified(parent_block, parent_block_index);
                state_staged.mark_dirty();
            }

            stats.delete_count.fetch_add(1, Ordering::Relaxed);
        } else {
            lore_trace!(
                "Dirty ignore (not on disk, not in revision): {}",
                relative_path.as_str()
            );
        }
    } else {
        // Not on disk + not in revision + not in staged -> Ignore
        lore_trace!(
            "Dirty ignore (not on disk, not in revision): {}",
            relative_path.as_str()
        );
    }

    Ok(())
}

/// Mark a node as Dirty+Delete, recursing into directory children.
///
/// View/ignore-filtered descendants are pruned (not marked): a dirty-delete
/// directory whose contents are excluded — e.g. a view-only path like
/// `Templates` where `/Templates/*` filters the contents but not the directory
/// node itself — must not re-mark its entire filtered subtree as deleted. The
/// directory node passed in is always marked; only its excluded children are
/// skipped. The non-emitting `excludes` is used so a large pruned subtree does
/// not produce a `FilterExclude` event per node.
async fn dirty_delete(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: &RelativePath,
    stats: Arc<DirtyStats>,
) -> Result<(), DirtyError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<DirtyError>("Failed deserializing state node block")?;

    let node = block.node(node_index);
    if node.is_dirty_delete() {
        return Ok(());
    }

    lore_trace!("Dirty delete of node {} ({})", node_id, path.as_str());
    stats.delete_count.fetch_add(1, Ordering::Relaxed);

    state
        .node_mark_dirty(repository.clone(), node_id, NodeFlags::DirtyDelete, true)
        .await
        .forward::<DirtyError>("Failed to mark node as dirty delete")?;

    // Recurse into directory children
    if node.is_directory() {
        let force = execution_context().globals().force();
        let mut child_node_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_node_id) = child_node_iter {
            let child_node = state
                .node(repository.clone(), child_node_id)
                .await
                .forward::<DirtyError>("Failed deserializing state node block")?;

            let child_name = state
                .node_name_clone(repository.clone(), child_node_id)
                .await
                .forward::<DirtyError>("Failed to get child name")?;
            let child_path = path.push_into_buf(&child_name).freeze();

            if force
                || !repository.filter.excludes(
                    &child_path,
                    child_node.is_directory(),
                    FilterMode::Full,
                )
            {
                dirty_delete_recurse(
                    repository.clone(),
                    state.clone(),
                    child_node_id,
                    child_path,
                    stats.clone(),
                )
                .await?;
            } else {
                lore_trace!(
                    "Dirty delete skipping filtered child: {}",
                    child_path.as_str()
                );
            }

            child_node
                .walk_step(child_node_id, node_id, &mut cycle)
                .forward::<DirtyError>("Invalid node hierarchy in dirty delete walk")?;
            child_node_iter = child_node.sibling();
        }
    }

    Ok(())
}

fn dirty_delete_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: RelativePath,
    stats: Arc<DirtyStats>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DirtyError>> + Send>> {
    Box::pin(async move { dirty_delete(repository, state, node_id, &path, stats).await })
}

/// Add a new file node to the staged tree with Dirty+Add.
async fn dirty_add(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: &RelativePath,
    stats: Arc<DirtyStats>,
) -> Result<(), DirtyError> {
    // Find or create parent directory nodes along the path
    let parent_path = relative_path.parent();
    let file_name = relative_path.name();

    let parent_node_id = if let Some(p) = parent_path {
        if !p.is_empty() {
            ensure_dirty_parent_dirs(repository.clone(), state.clone(), p).await?
        } else {
            ROOT_NODE
        }
    } else {
        ROOT_NODE
    };

    let node = Node {
        flags: NodeFlags::File.bits(),
        name_hash: crate::hash::hash_string(file_name),
        ..Default::default()
    };

    let node_id = state
        .node_add(repository.clone(), parent_node_id, node, file_name)
        .await
        .forward::<DirtyError>("Failed to add dirty node")?;

    // Mark with propagation so reused committed ancestors are marked up to root,
    // not left clean under a dirty child where a non-scan status walk prunes them.
    state
        .node_mark_dirty(repository.clone(), node_id, NodeFlags::DirtyAdd, true)
        .await
        .forward::<DirtyError>("Failed to mark dirty add and propagate to parents")?;

    stats.add_count.fetch_add(1, Ordering::Relaxed);

    Ok(())
}

/// Mark a new (untracked) directory node as Dirty+Add in the staged tree,
/// creating any missing ancestor directory nodes. Mirrors `dirty_add` for the
/// directory case so a brand-new EMPTY directory is tracked even when the child
/// recursion finds no files to anchor it.
async fn dirty_add_directory(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    relative_path: &RelativePath,
    stats: Arc<DirtyStats>,
) -> Result<(), DirtyError> {
    let parent_path = relative_path.parent();
    let dir_name = relative_path.name();

    let parent_node_id = match parent_path {
        Some(p) if !p.is_empty() => {
            ensure_dirty_parent_dirs(repository.clone(), state.clone(), p).await?
        }
        _ => ROOT_NODE,
    };

    let node = Node {
        name_hash: crate::hash::hash_string(dir_name),
        ..Default::default()
    };
    let new_id = state
        .node_add(repository.clone(), parent_node_id, node, dir_name)
        .await
        .forward::<DirtyError>("Failed to add dirty directory node")?;

    state
        .node_mark_dirty(repository.clone(), new_id, NodeFlags::DirtyAdd, true)
        .await
        .forward::<DirtyError>("Failed to mark dirty add directory and propagate to parents")?;

    stats.add_count.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Walk path segments and create missing directory nodes, returning the
/// final parent node ID. Existing directories are reused; missing ones are
/// created with Dirty flag so they appear in the state tree.
/// The caller is responsible for checking the full path against ignore filters.
async fn ensure_dirty_parent_dirs(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    parent_path: &str,
) -> Result<NodeID, DirtyError> {
    let mut current_node = ROOT_NODE;

    for segment in parent_path.split('/').filter(|s| !s.is_empty()) {
        let name_hash = crate::hash::hash_string(segment);
        if let Ok(child_id) = state
            .find_subnode(repository.clone(), current_node, name_hash)
            .await
        {
            current_node = child_id;
        } else {
            // A directory node carries no File or Link flag. Mark it with
            // propagation so reused committed ancestors are marked up to root.
            let dir_node = Node {
                name_hash,
                ..Default::default()
            };
            let new_id = state
                .node_add(repository.clone(), current_node, dir_node, segment)
                .await
                .forward::<DirtyError>("Failed to create parent directory for dirty add")?;
            state
                .node_mark_dirty(repository.clone(), new_id, NodeFlags::Dirty, true)
                .await
                .forward::<DirtyError>("Failed to mark created parent directory dirty")?;
            current_node = new_id;
        }
    }

    Ok(current_node)
}

/// Recursively mark all children of a moved directory as dirty-moved.
/// Mirrors `mark_children_moved` in stage.rs.
async fn mark_children_dirty_moved(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    parent_node: NodeID,
    move_flag: NodeFlags,
) -> Result<(), crate::state::StateError> {
    fn mark_children_dirty_moved_recursive(
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        parent_node: NodeID,
        move_flag: NodeFlags,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::state::StateError>> + Send>,
    > {
        Box::pin(async move {
            let children = state.node_children(repository.clone(), parent_node).await?;

            for child_id in children {
                let child_node = state.node(repository.clone(), child_id).await?;

                let child_flag = if child_node.is_dirty_add() {
                    NodeFlags::DirtyAdd
                } else {
                    move_flag
                };

                state
                    .node_mark_dirty(repository.clone(), child_id, child_flag, false)
                    .await?;

                if child_node.is_directory() {
                    mark_children_dirty_moved_recursive(
                        repository.clone(),
                        state.clone(),
                        child_id,
                        move_flag,
                    )
                    .await?;
                }
            }

            Ok(())
        })
    }

    mark_children_dirty_moved_recursive(repository, state, parent_node, move_flag).await
}

/// Recursively process a directory, marking each child as dirty based on filesystem state.
fn dirty_directory<'a>(
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    dir_path: &'a RelativePath,
    absolute_path: &'a std::path::Path,
    stats: Arc<DirtyStats>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DirtyError>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = tokio::fs::read_dir(absolute_path).await.map_err(|e| {
            DirtyError::internal_with_context(
                e,
                &format!("Failed to read directory {}", absolute_path.display()),
            )
        })?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| DirtyError::internal_with_context(e, "Failed to read directory entry"))?
        {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let child_path = dir_path.push_into_buf(&name_str).freeze();

            let force = execution_context().globals().force();
            if !force
                && repository
                    .filter
                    .emit_excludes(&child_path, true, FilterMode::Full)
            {
                continue;
            }

            dirty_path(
                repository.clone(),
                state_current.clone(),
                state_staged.clone(),
                &child_path,
                stats.clone(),
            )
            .await?;
        }

        // Also check for files in the current revision that are NOT on disk (deletes)
        if let Ok(dir_link) = state_current
            .find_node_link(repository.clone(), dir_path.as_str())
            .await
        {
            let children = state_current
                .node_children(repository.clone(), dir_link.node)
                .await
                .forward::<DirtyError>("Failed to get directory children")?;

            for &child_id in &children {
                let child_name = state_current
                    .node_name_clone(repository.clone(), child_id)
                    .await
                    .forward::<DirtyError>("Failed to get child name")?;

                let child_path_buf = dir_path.push_into_buf(&child_name);
                let child_abs = absolute_path.join(&child_name);

                // Only process children that are NOT on disk (deletes)
                if tokio::fs::metadata(&child_abs).await.is_err() {
                    let child_rel = child_path_buf.freeze();
                    dirty_path(
                        repository.clone(),
                        state_current.clone(),
                        state_staged.clone(),
                        &child_rel,
                        stats.clone(),
                    )
                    .await?;
                }
            }
        }

        Ok(())
    })
}

/// Mark a file as dirty-moved. Relocates the node in the staged tree from source to destination.
/// No filesystem checks — fully caller-trusted.
/// Propagates Dirty to both source parent (child removed) and destination parent (child added).
pub async fn dirty_move(
    repository: Arc<RepositoryContext>,
    from_path: String,
    to_path: String,
) -> Result<Hash, DirtyError> {
    let from_path =
        RelativePath::new_from_user_path(repository.require_path()?, from_path.as_str())
            .forward::<DirtyError>(&format!("Invalid path {from_path}"))?;
    let to_path = RelativePath::new_from_user_path(repository.require_path()?, to_path.as_str())
        .forward::<DirtyError>(&format!("Invalid path {to_path}"))?;

    if from_path.as_str() == to_path.as_str() {
        return Err(DirtyError::internal("Cannot move a path to itself"));
    }

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or_else(|| state_current.clone());

    // Find source node (must exist)
    let from_node_link = state
        .find_node_link(repository.clone(), from_path.as_str())
        .await
        .forward::<DirtyError>(&format!("Path {from_path} does not exist"))?;

    let from_block_index = NodeBlock::index(from_node_link.node);
    let from_node_index = Node::index(from_node_link.node);
    let from_block = state
        .block(repository.clone(), from_block_index)
        .await
        .forward::<DirtyError>("Failed deserializing state node block")?;
    let mut node = from_block.node(from_node_index);
    let old_parent = node.parent;

    // Find or create the destination parent
    let to_parent_path = to_path.parent();
    let to_parent_id = match to_parent_path {
        Some(p) if !p.is_empty() => {
            state
                .find_node_link(repository.clone(), p)
                .await
                .forward::<DirtyError>("Destination parent not found")?
                .node
        }
        _ => ROOT_NODE,
    };

    // Unlink from old parent
    if node.parent != to_parent_id {
        let parent_block_index = NodeBlock::index(node.parent);
        let parent_node_index = Node::index(node.parent);
        let parent_block = state
            .block(repository.clone(), parent_block_index)
            .await
            .forward::<DirtyError>("Failed deserializing state node block")?;
        let parent_node = parent_block.node(parent_node_index);

        if parent_node.child == from_node_link.node {
            let dirtied = {
                let mut writer = parent_block.write();
                writer.node(parent_node_index).child = node.sibling;
                writer.mark_dirty()
            };
            if dirtied {
                state.block_modified(parent_block, parent_block_index);
                state.mark_dirty();
            }
        } else {
            let mut child_id = parent_node.child().unwrap_or_default();
            let mut cycle = SiblingCycleGuard::new(node.parent);
            while let Some(sibling) = {
                let child = state
                    .node(repository.clone(), child_id)
                    .await
                    .forward::<DirtyError>("Failed deserializing state node block")?;
                child
                    .walk_step(child_id, node.parent, &mut cycle)
                    .forward::<DirtyError>("Invalid node hierarchy in dirty unlink walk")?;
                child.sibling()
            } {
                if sibling == from_node_link.node {
                    let child_block_index = NodeBlock::index(child_id);
                    let child_node_index = Node::index(child_id);
                    let child_block =
                        state
                            .block(repository.clone(), child_block_index)
                            .await
                            .forward::<DirtyError>("Failed deserializing state node block")?;
                    let dirtied = {
                        let mut writer = child_block.write();
                        writer.node(child_node_index).sibling = node.sibling;
                        writer.mark_dirty()
                    };
                    if dirtied {
                        state.block_modified(child_block, child_block_index);
                        state.mark_dirty();
                    }
                    break;
                }
                child_id = sibling;
            }
        }

        // Link into new parent's child list
        let new_parent_block_index = NodeBlock::index(to_parent_id);
        let new_parent_node_index = Node::index(to_parent_id);
        let new_parent_block = state
            .block(repository.clone(), new_parent_block_index)
            .await
            .forward::<DirtyError>("Failed deserializing state node block")?;
        let sibling_node_id = new_parent_block.node(new_parent_node_index).child;
        let dirtied = {
            let mut writer = new_parent_block.write();
            writer.node(new_parent_node_index).child = from_node_link.node;
            writer.mark_dirty()
        };
        if dirtied {
            state.block_modified(new_parent_block, new_parent_block_index);
            state.mark_dirty();
        }
        node.sibling = sibling_node_id;
        node.parent = to_parent_id;
    }

    // Update name if changed
    let to_name = to_path.name();
    let from_name = from_path.name();
    if from_name != to_name {
        node.name_hash = crate::hash::hash_string(to_name);
        from_block
            .deserialize_nametable(repository.clone())
            .await
            .forward::<DirtyError>("Failed deserializing name table")?;
        (node.name_offset, node.name_length) = from_block
            .write()
            .node_name_store(to_name, node.name_offset, node.name_length)
            .forward::<DirtyError>("Failed to store node name")?;
    }

    // Write updated node back
    let dirtied = {
        let mut writer = from_block.write();
        *writer.node(from_node_index) = node;
        writer.mark_dirty()
    };
    if dirtied {
        state.block_modified(from_block, from_block_index);
        state.mark_dirty();
    }

    // Mark the node as DirtyMove
    let dirty_move_flag = if node.is_dirty_add() {
        NodeFlags::DirtyAdd
    } else {
        NodeFlags::DirtyMove
    };
    state
        .node_mark_dirty(
            repository.clone(),
            from_node_link.node,
            dirty_move_flag,
            true,
        )
        .await
        .forward::<DirtyError>("Failed to mark node as dirty move")?;

    // If this is a directory move, recursively mark all children as dirty-moved
    if node.is_directory() {
        mark_children_dirty_moved(
            repository.clone(),
            state.clone(),
            from_node_link.node,
            dirty_move_flag,
        )
        .await
        .forward::<DirtyError>("Failed to mark children as dirty moved")?;
    }

    // Propagate dirty to source parent (child removed)
    if old_parent != to_parent_id {
        state
            .node_mark_dirty(repository.clone(), old_parent, NodeFlags::Dirty, false)
            .await
            .forward::<DirtyError>("Failed to propagate dirty to source parent")?;
    }

    // Persist
    state.set_revision_number(0);
    state.set_parent_self(current_revision);
    if state.revision() == current_revision {
        state.set_parent_other(Hash::default());
        state.set_metadata_hash(Hash::default());
    }

    let token = repository
        .try_write_token()
        .expect("dirty_move requires write access");
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?;

    if signature != current_revision {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<DirtyError>("Failed to serialize staged anchor")?;
    }

    Ok(signature)
}

/// Mark a file as dirty-copied. Creates a new destination node with Dirty+Copy.
/// Source node is unchanged. No filesystem checks — fully caller-trusted.
pub async fn dirty_copy(
    repository: Arc<RepositoryContext>,
    from_path: String,
    to_path: String,
) -> Result<Hash, DirtyError> {
    let from_path =
        RelativePath::new_from_user_path(repository.require_path()?, from_path.as_str())
            .forward::<DirtyError>(&format!("Invalid path {from_path}"))?;
    let to_path = RelativePath::new_from_user_path(repository.require_path()?, to_path.as_str())
        .forward::<DirtyError>(&format!("Invalid path {to_path}"))?;

    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DirtyError>("Failed to deserialize revision state")?;
    let current_revision = state_current.revision();
    let state = state_staged.unwrap_or_else(|| state_current.clone());

    // Verify source exists
    let _from_link = state
        .find_node_link(repository.clone(), from_path.as_str())
        .await
        .forward::<DirtyError>(&format!("Source path {from_path} does not exist"))?;

    // Find destination parent
    let to_parent_path = to_path.parent();
    let to_name = to_path.name();
    let to_parent_id = match to_parent_path {
        Some(p) if !p.is_empty() => {
            state
                .find_node_link(repository.clone(), p)
                .await
                .forward::<DirtyError>("Destination parent not found")?
                .node
        }
        _ => ROOT_NODE,
    };

    // Create destination node with Dirty+Copy
    let node = Node {
        flags: (NodeFlags::File | NodeFlags::DirtyCopy).bits(),
        name_hash: crate::hash::hash_string(to_name),
        ..Default::default()
    };

    let _node_id = state
        .node_add(repository.clone(), to_parent_id, node, to_name)
        .await
        .forward::<DirtyError>("Failed to add copy destination node")?;

    // Propagate dirty to destination parent
    state
        .node_mark_dirty(repository.clone(), to_parent_id, NodeFlags::Dirty, false)
        .await
        .forward::<DirtyError>("Failed to propagate dirty to destination parent")?;

    // Persist
    state.set_revision_number(0);
    state.set_parent_self(current_revision);
    if state.revision() == current_revision {
        state.set_parent_other(Hash::default());
        state.set_metadata_hash(Hash::default());
    }

    let token = repository
        .try_write_token()
        .expect("dirty_copy requires write access");
    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<DirtyError>("Failed to serialize staged revision state")?;

    if signature != current_revision {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<DirtyError>("Failed to serialize staged anchor")?;
    }

    Ok(signature)
}
