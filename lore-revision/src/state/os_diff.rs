// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The OS provider's diff of the working tree against a state.
//!
//! Reads the live filesystem through [`crate::util::fs`] and the global I/O driver, which is what
//! backing an operation with the OS means. Only [`crate::fs::os::OsOperation`] reaches
//! [`diff_os_filesystem`]: another provider implements
//! [`changes_from_filesystem_to_state`](crate::fs::filesystem_provider::InstanceOperation::changes_from_filesystem_to_state)
//! over whatever it is backed by, and routing that here would read the OS behind its own snapshot.
//!
//! A child of [`crate::state`], so the state internals a diff walks stay that module's own rather
//! than becoming the crate's to reach.

use core::str;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use super::*;
use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::filter::WalkPath;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::StageIntent;
use crate::lore::*;
use crate::lore_drain_tasks;
use crate::lore_trace;
use crate::node::*;
use crate::repository::BASE_SUFFIX;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::RepositoryContext;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::repository::THEIRS_SUFFIX;
use crate::state::ChangeSender;
use crate::state::diff::get_filtered_node_and_path;
use crate::state::diff::get_node_match;
use crate::state::stream::emit;
use crate::util::path::EntryPath;
use crate::util::path::RelativePath;

/// Find-or-create the directory node chain from `ROOT_NODE` down to `path`,
/// marking newly created segments as dirty-add. A path-filtered scan can enter
/// a directory (or a file's parent) present on disk but absent from `state_from`;
/// creating the chain lets adds discovered inside resolve their parent node.
/// Returns the node for the final path segment.
async fn ensure_scan_dir_chain(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    path: &str,
) -> Result<NodeID, StateError> {
    let mut current_node = ROOT_NODE;
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        let name_hash = crate::hash::hash_string(segment);
        if let Ok(child_id) = state
            .find_subnode(repository.clone(), current_node, name_hash)
            .await
        {
            current_node = child_id;
        } else {
            let dir_node = Node {
                flags: NodeFlags::DirtyAdd.bits(),
                name_hash,
                ..Default::default()
            };
            current_node = state
                .node_add(repository.clone(), current_node, dir_node, segment)
                .await
                .forward::<StateError>("scan add: failed to create entry directory node")?;
        }
    }
    Ok(current_node)
}

async fn diff_filesystem_subtree_impl(
    mut ctx: FilesystemDiffContext,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let absolute_path = ctx
        .filesystem_path
        .to_absolute_path(ctx.from.repository.require_path()?);

    match crate::fs::os::list_path(absolute_path)
        .await
        .forward::<StateError>("Failed to list the path")?
    {
        crate::fs::os::PathListingResult::Directory { listing } => {
            // A path-filtered scan can enter a directory present on disk but
            // absent from state_from (an untracked add). Create its dirty-add
            // node chain so adds discovered inside resolve their parent node.
            if ctx.intent.marks_dirty()
                && !ctx.from.node.is_valid_or_root_node_id()
                && !ctx.filesystem_path.is_empty()
            {
                let entry_node = ensure_scan_dir_chain(
                    ctx.from.repository.clone(),
                    ctx.from.state.clone(),
                    ctx.filesystem_path.as_str(),
                )
                .await?;
                ctx.from.node = entry_node;
            }
            diff_filesystem_directory(ctx, listing, changes).await
        }
        crate::fs::os::PathListingResult::File { item } => {
            // A path-filtered scan of a new file: ensure its parent directory
            // chain exists so the add resolves its parent node.
            if ctx.intent.marks_dirty()
                && !ctx.from.node.is_valid_node_id()
                && let Some(parent) = ctx.filesystem_path.parent()
                && !parent.is_empty()
            {
                ensure_scan_dir_chain(ctx.from.repository.clone(), ctx.from.state.clone(), parent)
                    .await?;
            }
            diff_filesystem_single_file(ctx, item, changes).await
        }
        crate::fs::os::PathListingResult::NotFound => {
            // Path doesn't exist on filesystem - everything in state is deleted
            diff_filesystem_missing(
                ctx.from,
                ctx.filesystem_path,
                ctx.states,
                ctx.filter_mode,
                ctx.intent,
                changes,
            )
            .await
        }
    }
}

/// How the file at `file_path` compares to `from_node`: a type change, a modification, or
/// neither. Reports what it found and leaves both trees alone.
///
/// A file is modified where its executable bit differs from the node's or its content does.
/// The bit settles the answer on its own, so it is tested before anything is read.
///
/// `current_node` is what the working copy last held, which is what says whether a recorded
/// modification time can answer for `from_node`.
async fn compare_single_file_against_state(
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    from_node: Option<&Node>,
    current_node: Option<&Node>,
    observed: &FileInfo,
    file_path: &impl WalkPath,
    stats: &FilesystemDiffStats,
) -> Result<SingleFileCompareResult, StateError> {
    let Some(from_node) = from_node else {
        // No state node - this is a new file
        return Ok(SingleFileCompareResult::NewFile);
    };

    let state_is_file = from_node.is_file();
    let _state_is_directory = from_node.is_directory();
    let _state_is_link = from_node.is_link();

    // Handle type changes
    let filesystem_is_file = observed.is_file();
    if filesystem_is_file && !state_is_file {
        // Filesystem has file, state has directory or link
        return Ok(SingleFileCompareResult::TypeChangedToFile);
    }

    if !filesystem_is_file && state_is_file {
        // Filesystem has directory, state has file
        return Ok(SingleFileCompareResult::TypeChangedToDirectory);
    }

    // At this point, both are files - check for modifications
    if state_is_file && filesystem_is_file {
        if observed.mode_differs_from(from_node.mode) {
            return Ok(SingleFileCompareResult::Modified);
        }

        // Force hash check if the from state doesn't match current state
        // (timestamp tracking only tells us if file matches what was last written,
        // which is the current state)
        let force_hash_check =
            current_node.is_none_or(|n| n.address.hash != from_node.address.hash);

        let modification = file_modified_against_node(
            repository,
            from_node,
            observed.mtime(),
            observed.size(),
            file_path,
            !force_hash_check,
            operation,
            &lore_storage::ContentHashes::default(),
        )
        .await?;
        stats.classify(&modification);

        if modification.is_modified() {
            return Ok(SingleFileCompareResult::Modified);
        }
    }

    Ok(SingleFileCompareResult::Unmodified)
}

/// Settle a delete for `node_id` and every node below it, which is what a commit needs to
/// drop a subtree the file system no longer holds.
///
/// A node already settled is left alone along with its subtree, so a second pass over the
/// same replacement costs one node read. A link's subtree lives in the linked state and is
/// not walked here, as staging a delete does not.
async fn settle_subtree_delete(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    let mut stack = vec![node_id];
    while let Some(node_id) = stack.pop() {
        let node = state.node(repository.clone(), node_id).await?;
        if node.is_staged_delete() {
            continue;
        }
        mark_settled(state, repository, node_id, SettledAction::Delete, intent).await?;
        if !node.is_directory() {
            continue;
        }
        let mut child = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child_id) = child {
            stack.push(child_id);
            let child_node = state.node(repository.clone(), child_id).await?;
            child_node.walk_step(child_id, node_id, &mut cycle)?;
            child = child_node.sibling();
        }
    }
    Ok(())
}

/// Settle a modification for a node whose content the file system already matches, which is
/// what force stages and what carries a merge's flags onto a node needing no other change.
async fn settle_insisted_modification(
    ctx: &FileDiffContext,
    file_path: &RelativePath,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    mark_settled(
        &ctx.state_from,
        &ctx.repository_from,
        ctx.from_node_id,
        SettledAction::Modify,
        ctx.intent,
    )
    .await?;
    emit_change(
        ctx.create_from_change_state(file_path.clone()),
        ctx.new_file_change_state(file_path.clone()),
        FileAction::Keep,
        change::Flags::Modify,
        changes,
        filter_mode,
    )
    .await
}

/// Settle a modification for a directory node force or a merge asked for.
///
/// A directory holds no content to compare, so it is settled on being asked rather than on
/// anything the walk found. The descent below it runs either way.
///
/// The keep it emits stands between the revision and the file system, so its `to` side states
/// `observed` rather than a node.
#[allow(clippy::too_many_arguments)]
async fn settle_insisted_directory(
    node_list: &StateChildrenNodes,
    node_id: NodeID,
    node: &Node,
    path: &RelativePath,
    observed: FileInfo,
    changes: &ChangeSender,
    intent: FilesystemDiffIntent,
    filter_mode: FilterMode,
) -> Result<(), StateError> {
    mark_settled(
        &node_list.state,
        &node_list.repository,
        node_id,
        SettledAction::Modify,
        intent,
    )
    .await?;
    let from = NodeChangeState {
        mapping: NodeMapping {
            repository: node_list.repository.clone(),
            state: node_list.state.clone(),
            path: path.clone(),
            node: node_id,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(node.flags),
        address: node.address,
        mode: node.mode,
    };
    let to = NodeChangeState {
        observed: Some(observed),
        ..from.invalid(path.clone())
    };
    emit_change(
        from,
        to,
        FileAction::Keep,
        change::Flags::Modify,
        changes,
        filter_mode,
    )
    .await
}

/// Report the delete of the node a type change displaced and the add of what replaced it,
/// settling both where the intent stages.
///
/// The displaced node keeps its place beside the replacement and carries the staged delete,
/// which is how a commit drops the old content and records the new.
///
/// Returns the replacement node, [`INVALID_NODE`] where the intent minted none, so a walk
/// descends into a directory it created.
async fn emit_type_replacement(
    ctx: &FileDiffContext,
    file_path: &RelativePath,
    is_directory: bool,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<NodeID, StateError> {
    let staging = ctx.intent.stage().is_some();
    if staging {
        settle_subtree_delete(
            &ctx.state_from,
            &ctx.repository_from,
            ctx.from_node_id,
            ctx.intent,
        )
        .await?;
    }

    add_change(
        ctx.create_from_change_state(file_path.clone()),
        ctx.invalid_change_state(file_path.clone()),
        FileAction::Delete,
        change::Flags::None,
        changes,
        filter_mode,
        ctx.states,
    )
    .await?;

    let replacement = if staging {
        ctx.add_new_node(file_path, is_directory).await?
    } else {
        INVALID_NODE
    };
    let to_state = if replacement.is_valid_node_id() {
        ctx.settled_change_state(replacement, file_path.clone())
            .await?
    } else if is_directory {
        ctx.new_directory_change_state(file_path.clone())
    } else {
        ctx.new_file_change_state(file_path.clone())
    };

    add_change(
        ctx.invalid_change_state(file_path.clone()),
        to_state,
        FileAction::Add,
        change::Flags::None,
        changes,
        filter_mode,
        ctx.states,
    )
    .await?;
    Ok(replacement)
}

/// Emit an Add+Dirty reconciliation change for a file whose node exists in
/// `state_from` (staged) but not in the current state. The file's presence
/// on disk is the add, and the node carries the `DirtyAdd` flag (re-marked
/// here if it was cleared by stale reconciliation). The compare framework
/// is bypassed because comparing the filesystem hash against the staged
/// node's zero address is meaningless for an add.
///
/// A dirty-move destination is exempt: it only looks like a node missing from
/// the current state because the walk matches by name, while the same node is
/// present in the current revision under its source path. Emitting an add for
/// it would drop the move provenance and overwrite `DirtyMove` with `DirtyAdd`,
/// so the move is left to the state diff, which coalesces it by file context.
#[allow(clippy::too_many_arguments)]
async fn emit_unstaged_add(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    from_node_id: NodeID,
    from_node: Node,
    file_path: &RelativePath,
    observed: &FileInfo,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
    filter_mode: FilterMode,
    states: FilterStates,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    if from_node.is_dirty_move() {
        lore_trace!("File {file_path} is a dirty move destination, not an unstaged add");
        return Ok(());
    }
    let staging = intent.stage().is_some();
    if staging {
        record_observed_size(&state, &repository, from_node_id, observed).await?;
    }
    if staging || !from_node.is_dirty_add() {
        mark_settled(
            &state,
            &repository,
            from_node_id,
            SettledAction::Add,
            intent,
        )
        .await?;
    }
    let block_index = NodeBlock::index(from_node_id);
    let node_index = Node::index(from_node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let node = block.node(node_index);
    add_change(
        NodeChangeState {
            mapping: NodeMapping {
                repository: repository.clone(),
                state: state.clone(),
                path: file_path.clone(),
                node: INVALID_NODE,
            },
            observed: None,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
            mode: 0,
        },
        NodeChangeState {
            mapping: NodeMapping {
                repository: repository.clone(),
                state: state.clone(),
                path: file_path.clone(),
                node: from_node_id,
            },
            observed: None,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
            mode: node.mode,
        },
        change::FileAction::Add,
        change::Flags::None,
        changes,
        filter_mode,
        states,
    )
    .await?;
    stats.file_add.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Record on `node_id` the size the walk measured at its path.
///
/// The mode is not recorded. A commit reads the file's mode and compares it with the node's to
/// see whether the revision has to carry a change to it, so a mode recorded here would be
/// compared against itself and a change to the executable bit alone would be lost.
///
/// The node is a file already: a path whose type the tree disagrees with is replaced rather
/// than recorded, which is [`emit_type_replacement`]'s.
async fn record_observed_size(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    observed: &FileInfo,
) -> Result<(), StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state.block(repository.clone(), block_index).await?;
    let dirtied = {
        let mut locked_block = block.write();
        locked_block.node(node_index).size = observed.size();
        locked_block.mark_dirty()
    };
    if dirtied {
        state.block_modified(block, block_index);
        state.mark_dirty();
    }
    Ok(())
}

/// Emit a single Add change for `node_id` without recursing into its subtree —
/// the caller's walk recursion surfaces the children. Used to report a dirty-add
/// directory exactly once per scan (unlike `add_change`, which recurses the whole
/// hierarchy for a directory add and would double-count against the recursion).
async fn emit_dirty_add_node_single(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: &RelativePath,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    if intent.stage().is_some() || !node.is_dirty_add() {
        mark_settled(&state, &repository, node_id, SettledAction::Add, intent).await?;
    }
    emit_add_node_single(repository, state, node_id, path, changes, stats).await
}

/// Emit a single Add change for `node_id` as it stands, without marking it and without
/// recursing into its subtree.
async fn emit_add_node_single(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    path: &RelativePath,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    emit(
        changes,
        NodeChange {
            action: change::FileAction::Add,
            flags: compute_change_flags(&node),
            from: NodeChangeState {
                mapping: NodeMapping {
                    repository: repository.clone(),
                    state: state.clone(),
                    path: path.clone(),
                    node: INVALID_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            },
            to: NodeChangeState {
                mapping: NodeMapping {
                    repository: repository.clone(),
                    state: state.clone(),
                    path: path.clone(),
                    node: node_id,
                },
                observed: None,
                flags: NodeFlags::from_bits_retain(node.flags),
                address: node.address,
                mode: node.mode,
            },
        },
    )
    .await?;
    stats.file_add.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Handle the result of a single file comparison and create appropriate changes.
///
/// This is the unified code path for handling single file node changes in both
/// `diff_filesystem_single_file` and `diff_filesystem_directory`.
///
/// # Arguments
/// * `ctx` - Context containing state references for creating changes
/// * `compare_result` - Result of the file comparison
/// * `file_path` - Path to the file (relative)
/// * `from_path` - Original path for rename detection (None if not a rename)
/// * `is_filesystem_directory` - True if the filesystem item is a directory
/// * `changes` - Where the changes are emitted
/// * `stats` - Statistics to update
///
/// # Rename Handling
/// When `from_path` is Some, this indicates the file was renamed. The function handles
/// renames for both modified and unmodified content:
/// - Unmodified + Rename: Generates a Move action (file content matches but name changed)
/// - Modified + Rename: Generates a Move action with modified content
///
/// # Returns
/// The node a staged type replacement minted, [`INVALID_NODE`] otherwise, so a walk
/// descends into a directory that replaced a file.
#[allow(clippy::too_many_arguments)]
async fn handle_single_file_compare_result(
    ctx: &FileDiffContext,
    compare_result: SingleFileCompareResult,
    file_path: &impl WalkPath,
    from_path: Option<&RelativePath>,
    is_filesystem_directory: bool,
    changes: &ChangeSender,
    stats: &FilesystemDiffStats,
    filter_mode: FilterMode,
) -> Result<NodeID, StateError> {
    match compare_result {
        SingleFileCompareResult::Unmodified => {
            // Handle rename case: content is unchanged but filename differs
            if let Some(original_path) = from_path {
                lore_trace!(
                    "File {} renamed from {}, content unmodified, add move change",
                    file_path,
                    original_path
                );
                // Settled before the change is recorded, so `compute_change_flags` loads
                // the marked node, as the modified arm below does.
                if ctx.intent.stage().is_some() && ctx.from_node_id.is_valid_node_id() {
                    mark_settled(
                        &ctx.state_from,
                        &ctx.repository_from,
                        ctx.from_node_id,
                        SettledAction::Move,
                        ctx.intent,
                    )
                    .await?;
                }
                let item_path = file_path.to_path();
                add_change(
                    ctx.create_from_change_state(
                        from_path.cloned().unwrap_or_else(|| item_path.clone()),
                    ),
                    ctx.new_file_change_state(item_path.clone()),
                    change::FileAction::Move,
                    change::Flags::None,
                    changes,
                    filter_mode,
                    ctx.states,
                )
                .await?;
                stats.file_replace.fetch_add(1, Ordering::Relaxed);
            } else if ctx.insists {
                lore_trace!("File {} unmodified, staged anyway", file_path);
                settle_insisted_modification(ctx, &file_path.to_path(), changes, filter_mode)
                    .await?;
                stats.file_replace.fetch_add(1, Ordering::Relaxed);
            } else {
                lore_trace!("File {} unmodified, retain", file_path);
                stats.file_retain.fetch_add(1, Ordering::Relaxed);

                // Scan: clear stale Dirty on retained file
                if ctx.intent.marks_dirty()
                    && ctx.from_node_id.is_valid_node_id()
                    && ctx.from_node.is_some_and(|n| n.is_dirty())
                {
                    ctx.state_from
                        .node_clear_dirty(ctx.repository_from.clone(), ctx.from_node_id)
                        .await?;
                }
            }
        }
        SingleFileCompareResult::Modified => {
            let action = if from_path.is_some() {
                lore_trace!("File {} renamed and modified, add move change", file_path);
                change::FileAction::Move
            } else {
                lore_trace!("File {} modified, add change", file_path);
                change::FileAction::Keep
            };

            // Scan: persist Dirty on the modified node before recording the change so
            // compute_change_flags loads the dirty node and includes Dirty in the event.
            if ctx.intent.marks_dirty() && ctx.from_node_id.is_valid_node_id() {
                if ctx.intent.stage().is_some() {
                    record_observed_size(
                        &ctx.state_from,
                        &ctx.repository_from,
                        ctx.from_node_id,
                        &ctx.observed,
                    )
                    .await?;
                }
                let settled = if action == change::FileAction::Move {
                    SettledAction::Move
                } else {
                    SettledAction::Modify
                };
                mark_settled(
                    &ctx.state_from,
                    &ctx.repository_from,
                    ctx.from_node_id,
                    settled,
                    ctx.intent,
                )
                .await?;
            }

            let item_path = file_path.to_path();
            add_change(
                ctx.create_from_change_state(
                    from_path.cloned().unwrap_or_else(|| item_path.clone()),
                ),
                ctx.new_file_change_state(item_path.clone()),
                action,
                change::Flags::Modify,
                changes,
                filter_mode,
                ctx.states,
            )
            .await?;

            stats.file_replace.fetch_add(1, Ordering::Relaxed);
        }
        SingleFileCompareResult::NewFile => {
            lore_trace!("File {} is new (not in state)", file_path);

            // Scan: create the Dirty+Add node in state first, then route add_change
            // through its NodeID so compute_change_flags loads it and sets Dirty.
            let to_state = if !is_filesystem_directory && ctx.intent.marks_dirty() {
                NodeChangeState {
                    mapping: NodeMapping {
                        repository: ctx.repository_from.clone(),
                        state: ctx.state_from.clone(),
                        path: file_path.to_path(),
                        node: ctx.add_new_node(&file_path.to_path(), false).await?,
                    },
                    observed: None,
                    flags: NodeFlags::File | NodeFlags::DirtyAdd,
                    address: Address::default(),
                    mode: 0,
                }
            } else if is_filesystem_directory {
                ctx.new_directory_change_state(file_path.to_path())
            } else {
                ctx.new_file_change_state(file_path.to_path())
            };

            add_change(
                ctx.invalid_change_state(file_path.to_path()),
                to_state,
                FileAction::Add,
                change::Flags::None,
                changes,
                filter_mode,
                ctx.states,
            )
            .await?;

            stats.file_add.fetch_add(1, Ordering::Relaxed);
        }
        SingleFileCompareResult::TypeChangedToFile => {
            lore_trace!(
                "Type changed at {} - state has directory/link, filesystem has file, delete + add",
                file_path
            );
            return emit_type_replacement(ctx, &file_path.to_path(), false, changes, filter_mode)
                .await;
        }
        SingleFileCompareResult::TypeChangedToDirectory => {
            lore_trace!(
                "Type changed at {} - state has file, filesystem has directory, delete + add",
                file_path
            );
            return emit_type_replacement(ctx, &file_path.to_path(), true, changes, filter_mode)
                .await;
        }
    }
    Ok(INVALID_NODE)
}

/// Handle diff for a directory path.
/// All items from the listing are children of `node_path`.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_directory(
    ctx: FilesystemDiffContext,
    file_listing: lore_io::DirStream,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    /// A staging walk includes the children staged for delete, so a path the file system
    /// still holds takes its delete back rather than being added a second time beside it.
    async fn collect_node_list(
        traversal: &NodeMapping,
        include_deleted: bool,
    ) -> Result<StateChildrenNodes, StateError> {
        let NodeMapping {
            repository,
            state,
            node: node_id,
            ..
        } = traversal;
        Ok(if node_id.is_valid_or_root_node_id() {
            let node = state.node(repository.clone(), *node_id).await?;
            if node.is_directory() {
                state
                    .collect_children_unsorted(
                        repository.clone(),
                        *node_id,
                        include_deleted,
                        true, /* Traverse links */
                    )
                    .await?
            } else {
                // State has a file where filesystem has directory - treat as delete + add
                // Return state node as single item for delete comparison
                StateChildrenNodes {
                    repository: repository.clone(),
                    state: state.clone(),
                    children: vec![StateNamedNode {
                        node: *node_id,
                        name: node.name_hash,
                    }],
                }
            }
        } else {
            StateChildrenNodes {
                repository: repository.clone(),
                state: state.clone(),
                children: vec![],
            }
        })
    }
    // Collect state node lists (always directory mode here)
    let staging = ctx.intent.stage().is_some();
    let mut node_list = collect_node_list(&ctx.from, staging).await?;

    let mut current_node_list = collect_node_list(&ctx.current, false).await?;

    let mut tasks = JoinSet::new();
    let mut stats = FilesystemDiffStats::default();
    let mut pending_discards: Vec<NodeID> = Vec::new();

    // TODO(mjansson) Use (radix) sorter on name for scaling to directories with many entries
    named_node_sort(&mut node_list.children);
    named_node_sort(&mut current_node_list.children);

    let mut node_list_found = vec![false; node_list.children.len()];

    // Run the walk in a helper so any `?` early-out still hits the
    // drain below — otherwise the JoinSet drops with subtree-recursion
    // tasks still running, leaking the Arc<RepositoryContext> clones.
    let work_result = diff_filesystem_directory_walk(
        &ctx,
        file_listing,
        &node_list,
        &current_node_list,
        &mut node_list_found,
        &mut tasks,
        changes,
        &mut stats,
        &mut pending_discards,
    )
    .await;
    let drain_result = lore_drain_tasks!(tasks, StateError::internal("Task failure"));
    work_result?;
    drain_result?;
    apply_pending_discards(
        node_list.state.clone(),
        node_list.repository.clone(),
        pending_discards,
    )
    .await?;
    Ok(stats)
}

/// Emit a single `Delete` change for one node, reloading it so any dirty flags
/// just persisted by [`State::node_mark_dirty`] are reflected in the record.
async fn emit_single_delete(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    path: &RelativePath,
    changes: &ChangeSender,
) -> Result<(), StateError> {
    let block = state
        .block(repository.clone(), NodeBlock::index(node_id))
        .await?;
    let node = block.node(Node::index(node_id));
    let flags = compute_change_flags(&node);
    let from = NodeChangeState {
        mapping: NodeMapping {
            repository,
            state,
            path: path.clone(),
            node: node_id,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(node.flags),
        address: node.address,
        mode: node.mode,
    };
    let to = from.invalid(path.clone());
    emit(
        changes,
        NodeChange {
            action: FileAction::Delete,
            flags,
            from,
            to,
        },
    )
    .await
}

/// Emit the buffered ancestor-directory deletes, outermost first, and clear the
/// buffer so sibling subtrees don't re-emit them. When the intent marks dirty each
/// directory is marked `DirtyDelete` first so a later bare `stage` (which walks
/// dirty flags rather than rescanning) picks up the directory deletion.
async fn flush_pending_dir_deletes(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    changes: &ChangeSender,
    pending: &mut Vec<(NodeID, RelativePath)>,
    intent: FilesystemDiffIntent,
) -> Result<(), StateError> {
    for (node_id, path) in std::mem::take(pending) {
        if intent.marks_dirty() {
            mark_settled(state, repository, node_id, SettledAction::Delete, intent).await?;
        }
        emit_single_delete(state.clone(), repository.clone(), node_id, &path, changes).await?;
    }
    Ok(())
}

/// Walk a revision subtree that is absent from the filesystem and emit `Delete`
/// changes for only the portion that was actually materialized on disk under the
/// active filter, returning whether anything materialized.
///
/// Materialization mirrors clone/checkout discovery: excluded children are
/// pruned (never written), a non-excluded file or link materializes, and a
/// directory materializes when a descendant does or — when it has no children —
/// when the empty directory itself is not excluded.
///
/// A directory can evaluate as "not excluded" only because the filter let the
/// diff descend through it (a view re-inclusion's generated traversal rules, or a
/// glob matching the directory but not content deeper inside it) while nothing
/// under it is in view. Such a directory is never written, so its delete record
/// is buffered in `pending` and emitted only once a materializing descendant
/// proves it existed on disk; if none does, the buffered entry is dropped. This
/// keeps the report from claiming a delete for a directory that was never there.
///
/// When the intent marks dirty, every node a delete is emitted for — the
/// materializing leaf and each flushed ancestor directory — is marked
/// `DirtyDelete` so the persisted dirty-tracking state records the deletion at
/// the granularity it is reported. `node_mark_dirty` short-circuits on a node
/// already carrying the base `Dirty` bit (which `DirtyDelete` includes), so a
/// sibling's upward propagation never clobbers a directory's `DirtyDelete`.
///
/// A staging intent settles the whole tree subtree, so a commit removes what the view
/// leaves out too: an excluded child is descended rather than skipped, and reported along
/// with the rest of the descent. Narrowing those reports to the in-view set is the
/// view-filtered delete work, which needs a walk that marks without reporting.
#[allow(clippy::too_many_arguments)]
async fn emit_filesystem_subtree_deletes(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
    node: &Node,
    path: &RelativePath,
    states: FilterStates,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    changes: &ChangeSender,
    pending: &mut Vec<(NodeID, RelativePath)>,
) -> Result<bool, StateError> {
    // Caller guarantees `node` is not filter-excluded.
    if node.is_file() || node.is_link() {
        flush_pending_dir_deletes(&state, &repository, changes, pending, intent).await?;
        if intent.marks_dirty() {
            mark_settled(&state, &repository, node_id, SettledAction::Delete, intent).await?;
        }
        emit_single_delete(state, repository, node_id, path, changes).await?;
        return Ok(true);
    }

    pending.push((node_id, path.clone()));
    let depth = pending.len();

    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id).await?;
    let mut had_child = false;
    let mut any_materialized = false;
    while let Some((child_id, child_node, child_name)) = children.next().await? {
        had_child = true;
        let child_path = path.push_into_buf(&child_name).freeze();
        // Release the block read lock before recursing (see NodeNameLock docs).
        drop(child_name);
        let (child_states, excluded) = repository.filter.child_excludes_tree(
            states,
            &child_path,
            child_node.is_directory(),
            filter_mode,
        );
        if excluded && intent.stage().is_none() {
            continue;
        }
        if Box::pin(emit_filesystem_subtree_deletes(
            state.clone(),
            repository.clone(),
            child_id,
            &child_node,
            &child_path,
            child_states,
            filter_mode,
            intent,
            changes,
            pending,
        ))
        .await?
        {
            any_materialized = true;
        }
    }

    if any_materialized {
        // The first materializing descendant already flushed this directory.
        return Ok(true);
    }

    if !had_child && repository.filter.should_descend(states, path, filter_mode) {
        // Empty in-view directory: clone/checkout writes it, so its absence is a
        // real deletion. It is the materializing leaf here, and its own buffered
        // entry (pushed above) is flushed and marked along with its ancestors.
        flush_pending_dir_deletes(&state, &repository, changes, pending, intent).await?;
        return Ok(true);
    }

    // Nothing under this directory materialized: drop its buffered entry.
    pending.truncate(depth - 1);
    Ok(false)
}

/// Whether staging leaves the name alone: a scratch file of the client's own making, or the
/// base and theirs sides a merge resolution has not consumed yet. A marking walk reports
/// them, since the working tree does hold them.
///
/// The mine side is not among them, as `stage` does not skip it either: a resolution that
/// keeps the working copy leaves it beside the resolved file.
fn staging_ignores_name(name: &str) -> bool {
    name.ends_with(TEMP_FILE_EXTENSION)
        || name.ends_with(BASE_SUFFIX)
        || name.ends_with(THEIRS_SUFFIX)
}

/// The index in `node_list` of the child an entry named `name_hash` claims, `None` where the
/// directory holds none.
///
/// Two children can share a folded name: a staged replacement sits beside the node it
/// displaced, and two spellings of one name can both be committed. A staging walk claims one
/// child per entry and prefers one not staged for delete, so a replacement is not replaced
/// again; only a run of more than one child is read to decide. Every other intent reports
/// against whichever the search reached, as it has.
async fn claim_named_child(
    node_list: &StateChildrenNodes,
    claimed: &[bool],
    name_hash: u64,
    staging: bool,
) -> Result<Option<usize>, StateError> {
    let children = node_list.children.as_slice();
    let Ok(found) = children.binary_search_by(|child| child.name.cmp(&name_hash)) else {
        return Ok(None);
    };
    if !staging {
        return Ok(Some(found));
    }

    let mut start = found;
    while start > 0 && children[start - 1].name == name_hash {
        start -= 1;
    }
    let mut end = found + 1;
    while end < children.len() && children[end].name == name_hash {
        end += 1;
    }
    if end - start == 1 {
        return Ok((!claimed[start]).then_some(start));
    }

    let mut displaced = None;
    for index in start..end {
        if claimed[index] {
            continue;
        }
        let node = node_list
            .state
            .node(node_list.repository.clone(), children[index].node)
            .await?;
        if !node.is_staged_delete() {
            return Ok(Some(index));
        }
        displaced = displaced.or(Some(index));
    }
    Ok(displaced)
}

/// Whether the tree and the file system agree on the type at a path. A type change replaces
/// the node rather than settling it, so staging decides only where they agree.
fn types_agree(from_node: &Node, is_file: bool, is_directory: bool) -> bool {
    (from_node.is_file() && is_file)
        || ((from_node.is_directory() || from_node.is_link()) && is_directory)
}

/// How a staging walk treats a node the tree holds, taking back a staged delete before
/// anything is compared.
///
/// A node already carrying a staged action is left as it is, so staging the same tree twice
/// reports the second pass as staging nothing. Force and a merge both ask for the action
/// again regardless, a merge because its own flags have to reach the node.
///
/// Taking back a delete clears the staged flags before marking, which is what drops a merge's
/// own flags: [`State::node_mark`] preserves those, and a node whose delete was taken back
/// carries no merge.
async fn staged_entry(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    node: &Node,
    stage: StageIntent,
    forced: bool,
) -> Result<StagedEntry, StateError> {
    let insists = forced
        || stage.node_flags.contains(NodeFlags::StagedMerge)
        || node.is_staged_merge_unresolved();
    if node.is_staged_delete() {
        state.node_clear_staged(repository.clone(), node_id).await?;
        mark_settled(
            state,
            repository,
            node_id,
            SettledAction::Modify,
            FilesystemDiffIntent::Stage(stage),
        )
        .await?;
        return Ok(StagedEntry::Undeleted);
    }
    if node.is_staged() && !insists {
        return Ok(StagedEntry::Settled);
    }
    Ok(StagedEntry::Compare { insisted: insists })
}

/// Match each filesystem item from `file_receiver` against `node_list` (the
/// `from` state's children) and `current_node_list` (the `current` state's
/// children), emitting changes through `changes`, marking matched entries in
/// `node_list_found`, spawning subtree-recursion tasks into `tasks`, and
/// queueing stale directory nodes into `pending_discards`. Items with no
/// match in `node_list` are buffered and processed as new adds once the
/// receiver is drained. Must only be called from [`diff_filesystem_directory`],
/// which sorts `node_list` and `current_node_list` by name beforehand — the
/// binary searches here assume that ordering.
///
/// A scan reconciles a node the current revision does not hold and no walked
/// entry matched — removed from disk, or a directory the walk declined as a
/// nested working copy — by queueing it for discard rather than reporting a
/// `Delete`: with no committed base there is nothing to delete from, and no
/// mutation verb would clear the entry.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_directory_walk(
    ctx: &FilesystemDiffContext,
    mut file_listing: lore_io::DirStream,
    node_list: &StateChildrenNodes,
    current_node_list: &StateChildrenNodes,
    node_list_found: &mut [bool],
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
    pending_discards: &mut Vec<NodeID>,
) -> Result<(), StateError> {
    let repository_root = ctx.from.repository.require_path()?;
    let staging = ctx.intent.stage().is_some();
    let forced = staging && execution_context().globals().force();
    let mut nested_probe: Option<std::path::PathBuf> = None;
    let mut new_file_list = vec![];
    let mut entry_buffer = ctx
        .filesystem_path
        .to_buf_with_capacity(RelativePath::COMPONENT_ROOM);
    while let Some(entry) = file_listing.next().await {
        let Some(item) = crate::fs::os::file_list_item(entry)
            .forward::<StateError>("Unusable directory entry")?
        else {
            continue;
        };
        if item.name == DOT_URC || item.name == DOT_LORE {
            continue;
        }
        if staging && staging_ignores_name(item.name.as_str()) {
            continue;
        }

        let entry = EntryPath::enter(&mut entry_buffer, item.name.as_str());

        let (item_states, excluded) = ctx.from.repository.filter.child_emit_excludes(
            ctx.states,
            entry.path(),
            item.metadata.is_dir(),
            ctx.filter_mode,
        );
        if excluded {
            continue;
        }

        let Some(current_index) =
            claim_named_child(node_list, node_list_found, item.name_hash, staging).await?
        else {
            new_file_list.push(item);
            continue;
        };

        let from_named_node = &node_list.children[current_index];
        node_list_found[current_index] = true;

        let current_match = match current_node_list
            .children
            .as_slice()
            .binary_search_by(|child| child.name.cmp(&item.name_hash))
        {
            Ok(index) => {
                let current_node_id = current_node_list.children[index].node;
                get_node_match(
                    current_node_list,
                    current_node_id,
                    item.name.as_str(),
                    &ctx.current.path,
                )
                .await?
                .map(|matched| (current_node_id, matched))
            }
            Err(_) => None,
        };
        let current_node_id = current_match
            .as_ref()
            .map_or(INVALID_NODE, |(node_id, _)| *node_id);
        let current_node = current_match
            .as_ref()
            .map_or_else(Node::default, |(_, matched)| matched.node);

        let Some(from_match) = get_node_match(
            node_list,
            from_named_node.node,
            item.name.as_str(),
            &ctx.from.path,
        )
        .await?
        else {
            continue;
        };
        let from_node = from_match.node;

        let was_file = from_node.is_file();
        let was_directory = from_node.is_directory();
        let was_link = from_node.is_link();

        let is_directory = item.metadata.is_dir();
        let is_file = item.metadata.is_file();

        let is_rename = from_match.renamed();

        let staged = match ctx.intent.stage() {
            Some(stage) if types_agree(&from_node, is_file, is_directory) => {
                staged_entry(
                    &node_list.state,
                    &node_list.repository,
                    from_named_node.node,
                    &from_node,
                    stage,
                    forced,
                )
                .await?
            }
            _ => StagedEntry::Compare { insisted: false },
        };

        if was_file && is_file {
            match staged {
                StagedEntry::Settled => continue,
                StagedEntry::Undeleted => {
                    emit_add_node_single(
                        node_list.repository.clone(),
                        node_list.state.clone(),
                        from_named_node.node,
                        &entry.to_path(),
                        changes,
                        stats,
                    )
                    .await?;
                    continue;
                }
                StagedEntry::Compare { .. } => {}
            }

            // A node in state_from but not in state_current is an unstaged
            // add — the file's presence on disk is the add. Comparing the
            // filesystem hash against the staged node's zero address would
            // misclassify, so emit Add+Dirty directly and skip the compare.
            if ctx.intent.marks_dirty() && !current_node_id.is_valid_node_id() {
                emit_unstaged_add(
                    node_list.repository.clone(),
                    node_list.state.clone(),
                    from_named_node.node,
                    from_node,
                    &entry.to_path(),
                    &FileInfo::from_metadata(&item.metadata),
                    changes,
                    stats,
                    ctx.filter_mode,
                    item_states,
                    ctx.intent,
                )
                .await?;
                continue;
            }

            let current_node_ref = if current_node_id.is_valid_node_id() {
                Some(&current_node)
            } else {
                None
            };

            let observed = FileInfo::from_metadata(&item.metadata);
            let compare_result = compare_single_file_against_state(
                &ctx.operation,
                node_list.repository.clone(),
                Some(&from_node),
                current_node_ref,
                &observed,
                entry.path(),
                stats,
            )
            .await?;

            // Create context for generating changes
            let file_ctx = FileDiffContext {
                repository_from: node_list.repository.clone(),
                state_from: node_list.state.clone(),
                from_node_id: from_named_node.node,
                from_node: Some(from_node),
                parent_node_id: Some(ctx.from.node),
                intent: ctx.intent,
                states: item_states,
                observed,
                insists: matches!(staged, StagedEntry::Compare { insisted: true }),
            };

            handle_single_file_compare_result(
                &file_ctx,
                compare_result,
                entry.path(),
                from_match.renamed_path.as_ref(),
                false, // filesystem item is a file, not directory
                changes,
                stats,
                ctx.filter_mode,
            )
            .await?;
        } else if was_link && is_directory {
            let item_path = entry.to_path();
            let from_path = from_match.path(&ctx.from.path, item.name.as_str());
            let current_path = current_match
                .as_ref()
                .map_or_else(RelativePath::new, |(_, matched)| {
                    matched.path(&ctx.current.path, item.name.as_str())
                });
            if staged == StagedEntry::Undeleted {
                emit_add_node_single(
                    node_list.repository.clone(),
                    node_list.state.clone(),
                    from_named_node.node,
                    &item_path,
                    changes,
                    stats,
                )
                .await?;
            }
            let link = from_node.linked_node();
            let (link_from, state_from) = link
                .resolve(ctx.from.repository.clone(), ctx.from.state.clone())
                .await?;
            let subnode_from = link.node;

            let (link_current, state_current, subnode_current) = if current_node.is_link() {
                let link = current_node.linked_node();
                let (linked_repository, state) = link
                    .resolve(ctx.current.repository.clone(), ctx.current.state.clone())
                    .await?;
                (linked_repository, state, link.node)
            } else {
                // Current state has no matching link (staged-add link or link replacing
                // a non-link in current). Use the from-side linked state for both sides
                // so files already tracked in the linked tree aren't misclassified as
                // unstaged adds.
                (link_from.clone(), state_from.clone(), subnode_from)
            };
            let from_item_states = ctx
                .from
                .repository
                .filter
                .child_excludes_tree(ctx.from_states, &from_path, true, ctx.filter_mode)
                .0;
            diff_filesystem_subtree_dispatch(
                FilesystemDiffContext {
                    operation: ctx.operation.clone(),
                    from: NodeMapping {
                        repository: link_from,
                        state: state_from,
                        path: from_path,
                        node: subnode_from,
                    },
                    current: NodeMapping {
                        repository: link_current,
                        state: state_current,
                        path: current_path,
                        node: subnode_current,
                    },
                    filesystem_path: item_path,
                    states: item_states,
                    from_states: from_item_states,
                    filter_mode: ctx.filter_mode,
                    intent: ctx.intent,
                    layer_mounts: ctx.layer_mounts.clone(),
                    // Crossing into the linked state; parent's link mounts
                    // are paths in the parent tree and do not apply here.
                    link_mounts: Arc::new(vec![]),
                },
                tasks,
                changes,
                stats,
            )
            .await?;
        } else if was_directory && is_directory {
            let item_path = entry.to_path();
            let from_path = from_match.path(&ctx.from.path, item.name.as_str());
            let current_path = current_match
                .as_ref()
                .map_or_else(RelativePath::new, |(_, matched)| {
                    matched.path(&ctx.current.path, item.name.as_str())
                });
            let uncommitted = ctx.intent.marks_dirty() && !current_node_id.is_valid_node_id();
            if uncommitted {
                let probe = nested_probe
                    .get_or_insert_with(|| ctx.filesystem_path.to_absolute_path(repository_root));
                if is_nested_repository_root(probe, item.name.as_str()).await {
                    lore_trace!("Discarding zombie entry for nested repository root {item_path}");
                    node_list_found[current_index] = false;
                    continue;
                }
            }
            if staged == StagedEntry::Undeleted {
                emit_add_node_single(
                    node_list.repository.clone(),
                    node_list.state.clone(),
                    from_named_node.node,
                    &item_path,
                    changes,
                    stats,
                )
                .await?;
            } else if staged == StagedEntry::Settled {
                // The node already carries its staged action; the descent below still runs.
            } else if uncommitted {
                emit_dirty_add_node_single(
                    node_list.repository.clone(),
                    node_list.state.clone(),
                    from_named_node.node,
                    &item_path,
                    changes,
                    stats,
                    ctx.intent,
                )
                .await?;
            } else if is_rename {
                let measured = if from_node.address == current_node.address
                    && from_node.mode == current_node.mode
                {
                    change::Flags::None
                } else {
                    change::Flags::Modify
                };
                add_change(
                    NodeChangeState {
                        mapping: NodeMapping {
                            repository: node_list.repository.clone(),
                            state: node_list.state.clone(),
                            path: from_path.clone(),
                            node: from_named_node.node,
                        },
                        observed: None,
                        flags: NodeFlags::from_bits_retain(from_node.flags),
                        address: from_node.address,
                        mode: from_node.mode,
                    },
                    NodeChangeState {
                        mapping: NodeMapping {
                            repository: current_node_list.repository.clone(),
                            state: current_node_list.state.clone(),
                            path: item_path.clone(),
                            node: current_node_id,
                        },
                        observed: None,
                        flags: NodeFlags::from_bits_retain(current_node.flags),
                        address: current_node.address,
                        mode: current_node.mode,
                    },
                    FileAction::Move,
                    measured,
                    changes,
                    ctx.filter_mode,
                    item_states,
                )
                .await?;
            } else if matches!(staged, StagedEntry::Compare { insisted: true }) {
                settle_insisted_directory(
                    node_list,
                    from_named_node.node,
                    &from_node,
                    &item_path,
                    FileInfo::from_metadata(&item.metadata),
                    changes,
                    ctx.intent,
                    ctx.filter_mode,
                )
                .await?;
            }
            let from_item_states = ctx
                .from
                .repository
                .filter
                .child_excludes_tree(ctx.from_states, &from_path, true, ctx.filter_mode)
                .0;
            let repository_from = node_list.repository.clone();
            let state_from = node_list.state.clone();
            let repository_current = current_node_list.repository.clone();
            let state_current = current_node_list.state.clone();
            let subnode_from = from_named_node.node;
            let current_is_link = current_node.is_link();
            let (repository_current, state_current, subnode_current) = if current_is_link {
                let link = current_node.linked_node();
                let (linked_repository, state) = link
                    .resolve(repository_current.clone(), state_current.clone())
                    .await?;
                (linked_repository, state, link.node)
            } else {
                (repository_current, state_current.clone(), current_node_id)
            };
            // Stay in the parent's link mounts when recursing into a normal
            // sub-directory; reset when crossing into a linked state because
            // those mount paths are in the parent tree, not the linked tree.
            let link_mounts_recurse = if current_is_link {
                Arc::new(vec![])
            } else {
                ctx.link_mounts.clone()
            };
            diff_filesystem_subtree_dispatch(
                FilesystemDiffContext {
                    operation: ctx.operation.clone(),
                    from: NodeMapping {
                        repository: repository_from,
                        state: state_from,
                        path: from_path,
                        node: subnode_from,
                    },
                    current: NodeMapping {
                        repository: repository_current,
                        state: state_current,
                        path: current_path,
                        node: subnode_current,
                    },
                    filesystem_path: item_path,
                    states: item_states,
                    from_states: from_item_states,
                    filter_mode: ctx.filter_mode,
                    intent: ctx.intent,
                    layer_mounts: ctx.layer_mounts.clone(),
                    link_mounts: link_mounts_recurse,
                },
                tasks,
                changes,
                stats,
            )
            .await?;
        } else {
            // Type change: file <-> directory
            let file_ctx = FileDiffContext {
                repository_from: node_list.repository.clone(),
                state_from: node_list.state.clone(),
                from_node_id: from_named_node.node,
                from_node: Some(from_node),
                parent_node_id: Some(ctx.from.node),
                intent: ctx.intent,
                states: item_states,
                observed: FileInfo::from_metadata(&item.metadata),
                insists: false,
            };

            // Determine the type change direction
            let compare_result = if is_file {
                SingleFileCompareResult::TypeChangedToFile
            } else {
                SingleFileCompareResult::TypeChangedToDirectory
            };

            lore_trace!(
                "Filesystem type (file/directory) differs for node {} in path {}, add delete and add changes",
                from_named_node.node,
                entry.path()
            );

            let replacement = handle_single_file_compare_result(
                &file_ctx,
                compare_result,
                entry.path(),
                None,
                is_directory,
                changes,
                stats,
                ctx.filter_mode,
            )
            .await?;

            // A directory that replaced a file is walked against the node the replacement
            // minted, so the content it holds is staged with it. A file that replaced a
            // directory holds none.
            if is_directory && replacement.is_valid_node_id() {
                let item_path = entry.to_path();
                diff_filesystem_subtree_dispatch(
                    FilesystemDiffContext {
                        operation: ctx.operation.clone(),
                        from: NodeMapping {
                            repository: ctx.from.repository.clone(),
                            state: ctx.from.state.clone(),
                            path: item_path.clone(),
                            node: replacement,
                        },
                        current: NodeMapping {
                            repository: ctx.current.repository.clone(),
                            state: ctx.current.state.clone(),
                            path: RelativePath::new(),
                            node: INVALID_NODE,
                        },
                        filesystem_path: item_path,
                        states: item_states,
                        from_states: item_states,
                        filter_mode: ctx.filter_mode,
                        intent: ctx.intent,
                        layer_mounts: ctx.layer_mounts.clone(),
                        link_mounts: ctx.link_mounts.clone(),
                    },
                    tasks,
                    changes,
                    stats,
                )
                .await?;
            }
        }
    }

    // Nodes that were not iterated are deleted in file system
    for (index, from_named_node) in node_list.children.iter().enumerate() {
        if node_list_found[index] {
            continue;
        }

        let Some((from_node, from_node_states)) = get_filtered_node_and_path(
            node_list,
            from_named_node.node,
            &ctx.from.path,
            ctx.from_states,
            ctx.filter_mode,
        )
        .await?
        else {
            continue;
        };

        // A staging walk sees the nodes already staged for delete, and the file system
        // still not holding one says nothing new about it.
        if staging && from_node.node.is_staged_delete() {
            continue;
        }

        if ctx.intent.marks_dirty() && from_node.node.is_directory() {
            let in_current = current_node_list
                .children
                .as_slice()
                .binary_search_by(|child| child.name.cmp(&from_named_node.name))
                .is_ok();
            if !in_current {
                lore_trace!(
                    "Queueing reverted uncommitted directory node {} (no entry at {}, not in current)",
                    from_named_node.node,
                    from_node.path
                );
                pending_discards.push(from_named_node.node);
                continue;
            }
        }

        // Emit deletes only for the materialized portion of the subtree,
        // suppressing directories the filter merely descended through but never
        // wrote to disk (see emit_filesystem_subtree_deletes).
        if from_node.node.is_directory() {
            let mut pending = Vec::new();
            emit_filesystem_subtree_deletes(
                node_list.state.clone(),
                node_list.repository.clone(),
                from_named_node.node,
                &from_node.node,
                &from_node.path,
                from_node_states,
                ctx.filter_mode,
                ctx.intent,
                changes,
                &mut pending,
            )
            .await?;
            continue;
        }

        // A leaf node present in state_from but not in state_current, with
        // no file on disk, is an unstaged add that the user reverted by
        // removing the file. Discard the node so state_staged matches the
        // filesystem rather than emitting a Delete change for a node that
        // shouldn't exist.
        let in_current = current_node_list
            .children
            .as_slice()
            .binary_search_by(|child| child.name.cmp(&from_named_node.name))
            .is_ok();
        if ctx.intent.marks_dirty() && from_node.node.is_file() && !in_current {
            lore_trace!(
                "Queueing reverted-DirtyAdd node {} (no file at {}, not in current)",
                from_named_node.node,
                from_node.path
            );
            pending_discards.push(from_named_node.node);
            continue;
        }

        // Scan: persist Dirty+Delete on the missing node before recording the change
        // so compute_change_flags loads the dirty node and includes Dirty in the event.
        if ctx.intent.marks_dirty() {
            mark_settled(
                &node_list.state,
                &node_list.repository,
                from_named_node.node,
                SettledAction::Delete,
                ctx.intent,
            )
            .await?;
        }

        lore_trace!(
            "Filesystem does not have node {} in path {}, add deleted change",
            from_named_node.node,
            ctx.filesystem_path
        );

        add_change(
            NodeChangeState {
                mapping: NodeMapping {
                    repository: node_list.repository.clone(),
                    state: node_list.state.clone(),
                    path: from_node.path.clone(),
                    node: from_named_node.node,
                },
                observed: None,
                flags: NodeFlags::from_bits_retain(from_node.node.flags),
                address: from_node.node.address,
                mode: from_node.node.mode,
            },
            NodeChangeState {
                mapping: NodeMapping {
                    repository: node_list.repository.clone(),
                    state: node_list.state.clone(),
                    path: from_node.path.clone(),
                    node: INVALID_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            },
            FileAction::Delete,
            change::Flags::None,
            changes,
            ctx.filter_mode,
            from_node_states,
        )
        .await?;
    }

    // Remaining files/directories are added (all are children of node_path)
    'new_file_iter: for file in new_file_list.iter() {
        // For directory listing, new items are children
        let child_file_path = ctx
            .filesystem_path
            .push_into_buf(file.name.as_str())
            .freeze();

        let (child_states, excluded) = ctx.from.repository.filter.child_emit_excludes(
            ctx.states,
            &child_file_path,
            file.metadata.is_dir(),
            ctx.filter_mode,
        );
        if excluded {
            continue 'new_file_iter;
        }

        let is_directory = file.metadata.is_dir();

        if is_directory {
            // A directory on disk with no `state_from` node that matches a
            // link in `state_current` is a link add, not a per-file add: the
            // mounted content belongs to the linked repository. Skip the
            // entry; the link node is reported via `state::diff_collect` (in
            // `lore status`) and `link list`, not via `file diff`.
            //
            // The realistic scan-side caller (`lore status` via
            // `diff_filesystem`) stages the link in `state_from` before
            // status runs, so the link is matched in the paired
            // `was_link && is_directory` branch above and never reaches here;
            // the `continue` fires with a dirty-marking intent only in a
            // constructed corner case, where skipping dirty-add is still
            // correct (the link is not new in the working state).
            if ctx
                .link_mounts
                .iter()
                .any(|m| m.target_path == child_file_path.as_str())
            {
                lore_trace!(
                    "Filesystem path {child_file_path} matches a link in the current state, skipping link-internal content"
                );
                continue 'new_file_iter;
            }
            // Layer mount detection: if this directory's parent-relative path
            // matches a configured layer mount, switch comparison context to
            // the layer's repo and state for the recursion. The layer mount
            // itself is NOT emitted as an "add" — its content is owned by the
            // layer's pinned revision, not the parent's tree.
            if let Some(mount) = ctx
                .layer_mounts
                .iter()
                .find(|m| m.target_path == child_file_path.as_str())
            {
                // A staging walk leaves the mount alone: the layer's own tree is staged by
                // its own walk against its own state, and staging it from here as well
                // would settle the same nodes twice.
                if staging {
                    lore_trace!("Filesystem path {child_file_path} is a layer mount, skipping");
                    continue 'new_file_iter;
                }
                lore_trace!(
                    "Filesystem path {child_file_path} is a layer mount, recursing into layer state"
                );
                let layer_repository = mount.repository.clone();
                let layer_state = mount.state.clone();
                let layer_source_node = mount.source_node;
                diff_filesystem_subtree_dispatch(
                    FilesystemDiffContext {
                        operation: ctx.operation.clone(),
                        from: NodeMapping {
                            repository: layer_repository.clone(),
                            state: layer_state.clone(),
                            path: child_file_path.clone(),
                            node: layer_source_node,
                        },
                        current: NodeMapping {
                            repository: layer_repository,
                            state: layer_state,
                            path: child_file_path.clone(),
                            node: layer_source_node,
                        },
                        filesystem_path: child_file_path,
                        states: child_states,
                        // The layer is walked at its mount path, which is the
                        // path on disk.
                        from_states: child_states,
                        filter_mode: ctx.filter_mode,
                        intent: ctx.intent,
                        // Non-overlapping layers: no nested mounts inside a layer.
                        layer_mounts: Arc::new(vec![]),
                        // Crossing into the layer state; parent's link mounts
                        // are paths in the parent tree and do not apply here.
                        link_mounts: Arc::new(vec![]),
                    },
                    tasks,
                    changes,
                    stats,
                )
                .await?;
                continue 'new_file_iter;
            }
            let probe = nested_probe
                .get_or_insert_with(|| ctx.filesystem_path.to_absolute_path(repository_root));
            if is_nested_repository_root(probe, file.name.as_str()).await {
                lore_trace!("Skipping nested repository root {child_file_path}");
                continue 'new_file_iter;
            }
            lore_trace!("Filesystem has new directory in path {child_file_path}, recursing");

            // Scan: persist a Dirty+Add node for the new directory before
            // recursing, so files inside it resolve their parent and the staged
            // anchor rebase can descend the dirty subtree. Emit it as a single
            // node (the recursion below surfaces the children) and recurse
            // against it so a rescan matches the persisted subtree.
            let mut dir_from_root = INVALID_NODE;
            let mut dir_from_path = RelativePath::new();
            let mut dir_from_states = FilterStates::ROOT;
            if ctx.intent.marks_dirty() {
                // The new directory is a child of the directory currently being
                // walked; its node is the correct parent even across link/layer
                // boundaries (resolving by parent path would not match there).
                let dir_parent_node = ctx.from.node;
                let dir_node = Node {
                    flags: NodeFlags::DirtyAdd.bits(),
                    name_hash: crate::hash::hash_string(file.name.as_str()),
                    ..Default::default()
                };
                let new_dir_id = ctx
                    .from
                    .state
                    .node_add(
                        ctx.from.repository.clone(),
                        dir_parent_node,
                        dir_node,
                        file.name.as_str(),
                    )
                    .await
                    .forward::<StateError>("scan add: failed to add new directory node")?;
                emit_dirty_add_node_single(
                    ctx.from.repository.clone(),
                    ctx.from.state.clone(),
                    new_dir_id,
                    &child_file_path,
                    changes,
                    stats,
                    ctx.intent,
                )
                .await?;
                ctx.from
                    .state
                    .node_mark_dirty(
                        ctx.from.repository.clone(),
                        dir_parent_node,
                        NodeFlags::Dirty,
                        false,
                    )
                    .await?;
                dir_from_root = new_dir_id;
                dir_from_path = child_file_path.clone();
                dir_from_states = child_states;
            }

            let repository_from = ctx.from.repository.clone();
            let state_from = ctx.from.state.clone();
            let repository_current = ctx.current.repository.clone();
            let state_current = ctx.current.state.clone();
            diff_filesystem_subtree_dispatch(
                FilesystemDiffContext {
                    operation: ctx.operation.clone(),
                    from: NodeMapping {
                        repository: repository_from,
                        state: state_from,
                        path: dir_from_path,
                        node: dir_from_root,
                    },
                    current: NodeMapping {
                        repository: repository_current,
                        state: state_current,
                        path: RelativePath::new(),
                        node: INVALID_NODE,
                    },
                    filesystem_path: child_file_path.clone(),
                    states: child_states,
                    from_states: dir_from_states,
                    filter_mode: ctx.filter_mode,
                    intent: ctx.intent,
                    layer_mounts: ctx.layer_mounts.clone(),
                    // Same parent state; deeper paths may still match a link.
                    link_mounts: ctx.link_mounts.clone(),
                },
                tasks,
                changes,
                stats,
            )
            .await?;

            // The single Dirty+Add directory node emitted above is the scan's
            // report for this new directory; skip the transient change below.
            if ctx.intent.marks_dirty() {
                continue 'new_file_iter;
            }
        }

        let file_ctx = FileDiffContext {
            repository_from: ctx.from.repository.clone(),
            state_from: ctx.from.state.clone(),
            from_node_id: INVALID_NODE,
            from_node: None,
            parent_node_id: Some(ctx.from.node),
            intent: ctx.intent,
            states: child_states,
            observed: FileInfo::from_metadata(&file.metadata),
            insists: false,
        };

        lore_trace!("Filesystem has new item in path {child_file_path}, add add change");

        handle_single_file_compare_result(
            &file_ctx,
            SingleFileCompareResult::NewFile,
            &child_file_path,
            None,
            is_directory,
            changes,
            stats,
            ctx.filter_mode,
        )
        .await?;
    }

    while let Some(joined) = tasks.join_next().await {
        diff_filesystem_subtree_merge_task(joined, stats)?;
    }

    Ok(())
}

/// The subtree walks a directory has in flight, each reporting what it counted.
///
/// Nothing else comes back: a subtree emits its changes to the caller through a clone of the
/// sender rather than collecting them for the parent to fold in.
type SubtreeTasks = JoinSet<Result<FilesystemDiffStats, StateError>>;

fn diff_filesystem_task_semaphore() -> &'static Arc<Semaphore> {
    DIFF_FILESYSTEM_TASK_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_TREE_TASKS)))
}

/// Diffs one subtree, spawned while the budget allows and inline once it does not, then folds
/// back whatever has finished.
///
/// Inline rather than a blocking acquire: a parent holds its permit until its children finish,
/// so waiting on one would wait on a descendant that cannot start.
async fn diff_filesystem_subtree_dispatch(
    subtree: FilesystemDiffContext,
    tasks: &mut SubtreeTasks,
    changes: &ChangeSender,
    stats: &mut FilesystemDiffStats,
) -> Result<(), StateError> {
    if let Ok(permit) = diff_filesystem_task_semaphore().clone().try_acquire_owned() {
        let changes = changes.clone();
        lore_spawn!(tasks, async move {
            let _permit = permit;
            diff_filesystem_subtree_recurse(subtree, &changes).await
        });
    } else {
        stats.append(diff_filesystem_subtree_recurse(subtree, changes).await?);
    }
    while let Some(joined) = tasks.try_join_next() {
        diff_filesystem_subtree_merge_task(joined, stats)?;
    }
    Ok(())
}

/// Folds a joined subtree task's count into the parent directory's.
fn diff_filesystem_subtree_merge_task(
    joined: Result<Result<FilesystemDiffStats, StateError>, JoinError>,
    stats: &mut FilesystemDiffStats,
) -> Result<(), StateError> {
    stats.append(
        joined
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()?,
    );
    Ok(())
}

/// Handle diff for a single file path.
/// The item is the file at `node_path` (not a child).
///
/// This function uses the unified single-file comparison logic via
/// `compare_single_file_against_state` and `handle_single_file_compare_result`.
#[allow(clippy::too_many_arguments)]
async fn diff_filesystem_single_file(
    ctx: FilesystemDiffContext,
    file_item: crate::fs::os::FileListItem,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let stats = FilesystemDiffStats::default();

    // Path is already correct - file_item represents node_path itself
    // No path manipulation needed!

    // Get the state nodes for comparison
    let from_node = if ctx.from.node.is_valid_node_id() {
        ctx.from
            .state
            .node(ctx.from.repository.clone(), ctx.from.node)
            .await
            .ok()
    } else {
        None
    };

    let current_node = if ctx.current.node.is_valid_node_id() {
        ctx.current
            .state
            .node(ctx.current.repository.clone(), ctx.current.node)
            .await
            .ok()
    } else {
        None
    };

    let observed = FileInfo::from_metadata(&file_item.metadata);

    // Staging decides on a node the tree holds before anything is compared, as the directory
    // walk does.
    let mut insisted = false;
    if let Some(stage) = ctx.intent.stage()
        && file_item.metadata.is_file()
        && let Some(node) = from_node
        && node.is_file()
    {
        match staged_entry(
            &ctx.from.state,
            &ctx.from.repository,
            ctx.from.node,
            &node,
            stage,
            execution_context().globals().force(),
        )
        .await?
        {
            StagedEntry::Settled => return Ok(stats),
            StagedEntry::Undeleted => {
                emit_add_node_single(
                    ctx.from.repository.clone(),
                    ctx.from.state.clone(),
                    ctx.from.node,
                    &ctx.filesystem_path,
                    changes,
                    &stats,
                )
                .await?;
                return Ok(stats);
            }
            StagedEntry::Compare { insisted: asked } => insisted = asked,
        }
    }

    // A node in state_from but not in state_current is an unstaged add —
    // the file's presence on disk is the add. Skip the compare and emit
    // Add+Dirty directly.
    if ctx.intent.marks_dirty()
        && file_item.metadata.is_file()
        && ctx.from.node.is_valid_node_id()
        && !ctx.current.node.is_valid_node_id()
        && let Some(node) = from_node
        && node.is_file()
    {
        emit_unstaged_add(
            ctx.from.repository.clone(),
            ctx.from.state.clone(),
            ctx.from.node,
            node,
            &ctx.filesystem_path,
            &observed,
            changes,
            &stats,
            ctx.filter_mode,
            ctx.states,
            ctx.intent,
        )
        .await?;
        return Ok(stats);
    }

    let compare_result = compare_single_file_against_state(
        &ctx.operation,
        ctx.from.repository.clone(),
        from_node.as_ref(),
        current_node.as_ref(),
        &observed,
        &ctx.filesystem_path,
        &stats,
    )
    .await?;

    // Create the context for generating changes
    let file_ctx = FileDiffContext {
        repository_from: ctx.from.repository.clone(),
        state_from: ctx.from.state.clone(),
        from_node_id: ctx.from.node,
        from_node,
        parent_node_id: None,
        intent: ctx.intent,
        states: ctx.states,
        observed,
        insists: insisted,
    };

    handle_single_file_compare_result(
        &file_ctx,
        compare_result,
        &ctx.filesystem_path,
        None, // No rename detection for single file path
        file_item.metadata.is_dir(),
        changes,
        &stats,
        ctx.filter_mode,
    )
    .await?;

    Ok(stats)
}

/// Handle diff when filesystem path doesn't exist.
/// Everything in state under this path is considered deleted.
async fn diff_filesystem_missing(
    from: NodeMapping,
    filesystem_path: RelativePath,
    states: FilterStates,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let stats = FilesystemDiffStats::default();

    // Add delete changes for all nodes under the from node
    if from.node.is_valid_node_id() {
        let from_node = from.state.node(from.repository.clone(), from.node).await?;

        lore_trace!(
            "Filesystem path {} does not exist, marking state node {} as deleted",
            filesystem_path,
            from.node
        );

        // Scan: mark missing file as Dirty+Delete
        if intent.marks_dirty() {
            mark_settled(
                &from.state,
                &from.repository,
                from.node,
                SettledAction::Delete,
                intent,
            )
            .await?;
        }

        add_change(
            NodeChangeState {
                mapping: NodeMapping {
                    repository: from.repository.clone(),
                    state: from.state.clone(),
                    path: from.path.clone(),
                    node: from.node,
                },
                observed: None,
                flags: NodeFlags::from_bits_retain(from_node.flags),
                address: from_node.address,
                mode: from_node.mode,
            },
            NodeChangeState {
                mapping: NodeMapping {
                    repository: from.repository,
                    state: from.state,
                    path: from.path,
                    node: INVALID_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            },
            FileAction::Delete,
            change::Flags::None,
            changes,
            filter_mode,
            states,
        )
        .await?;
    }

    Ok(stats)
}

/// Walks one subtree, boxed so the walk can descend into itself.
fn diff_filesystem_subtree_recurse<'a>(
    ctx: FilesystemDiffContext,
    changes: &'a ChangeSender,
) -> Pin<Box<dyn Future<Output = Result<FilesystemDiffStats, StateError>> + Send + 'a>> {
    Box::pin(diff_filesystem_subtree_impl(ctx, changes))
}

/// Diffs the working tree under `diff.filesystem_path` against the trees it names, emitting a
/// change per difference through `changes`.
///
/// The module's entry, and what
/// [`OsOperation`](crate::fs::os::OsOperation)'s
/// `changes_from_filesystem_to_state` answers with.
pub(crate) async fn diff_os_filesystem(
    diff: FilesystemDiffContext,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    diff_filesystem_subtree_recurse(diff, changes).await
}
