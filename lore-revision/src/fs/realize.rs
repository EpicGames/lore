// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::BitAnd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;
use zerocopy::FromZeros;

use crate::branch::merge::MergeType;
use crate::change;
use crate::change::NodeChange;
use crate::dependency;
use crate::error::LoreErrorExt;
use crate::errors::LocalModifications;
use crate::errors::WriteRequired;
use crate::event;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FilesystemPath;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::hash;
use crate::interface::LoreString;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_info;
use crate::lore_trace;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFileMode;
use crate::node::NodeFlags;
use crate::node::NodeIDExt;
use crate::node::NodeLink;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::progress::DEFAULT_WORK_CHANNEL_CAPACITY;
use crate::repository::BASE_SUFFIX;
use crate::repository::MINE_SUFFIX;
use crate::repository::RepositoryContext;
use crate::repository::THEIRS_SUFFIX;
use crate::repository::clone;
use crate::repository::clone::CloneStats;
use crate::revision::sync::LoreRevisionSyncFileEventData;
use crate::revision::sync::LoreRevisionSyncProgressEventData;
use crate::revision::sync::SyncError;
use crate::revision::sync::SyncOptions;
use crate::revision::sync::SyncRealizeStats;
use crate::revision::sync::SyncVerifyArgs;
use crate::revision::sync::SyncVerifyStats;
use crate::stage;
use crate::state;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::expand_path_ancestors;

pub async fn realize_state(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    state_current: Arc<State>,
    state_target: Arc<State>,
    options: SyncOptions,
) -> Result<(), SyncError> {
    /*
    TODO(mjansson): When using a filter it doesn't make sense to cache ALL state fragments,
    but rather only those used by the filter. Improve caching to take this into account.

    if let Some(remote) = repository.remote.as_ref() {
        log_native(LogLevel::Info, "Fetching state fragments");
        let mut tasks = JoinSet::new();
        let state_current = state_current.clone();
        let state_target = state_target.clone();
        let repository_current = repository.clone();
        let repository_target = repository.clone();
        let remote_current = remote.clone();
        let remote_target = remote.clone();
        tasks.spawn(LORE_CONTEXT.scope(execution.clone(),
            async move {
                state_current
                    .cache_fragments(
                        repository_current.store.clone(),
                        repository_current.id,
                        remote_current.as_str(),
                    )
                    .await
            }
        ));
        tasks.spawn(LORE_CONTEXT.scope(execution.clone(),
            async move {
                state_target
                    .cache_fragments(
                        repository_target.store.clone(),
                        repository_target.id,
                        remote_target.as_str(),
                    )
                    .await
            }
        ));
        while let Some(result) = tasks.join_next().await {
            let _ = result.internal("Recursion task failed")?;
        }
    }
    */

    let stats: Arc<SyncRealizeStats> = Arc::default();
    let changes = if !options.reset {
        lore_info!(
            "Calculating deltas {} -> {}",
            state_current.revision_number(),
            state_target.revision_number()
        );
        state::diff_collect(
            repository.clone(),
            state_current.clone(),
            repository.clone(),
            state_target.clone(),
            None, /* No subpath */
            options.filter_mode,
        )
        .await
        .forward::<SyncError>("Failed to calculate delta changes between states")?
    } else {
        lore_info!(
            "Calculating deltas from filesystem -> {}",
            state_target.revision_number()
        );
        let (mut changes, _stats) = operation
            .changes_from_filesystem_to_state(
                repository.clone(),
                state_target.clone(),
                repository.clone(),
                state_current.clone(),
                RelativePath::new(),
                ROOT_NODE,
                ROOT_NODE,
                options.filter_mode | FilterMode::Ignore,
            )
            .await
            .forward::<SyncError>(
                "Failed to calculate delta changes between file system and target state",
            )?;
        /*
        stats.change.file_retain.fetch_add(
            diff_stats.file_retain.load(Ordering::Relaxed) as usize,
            Ordering::Relaxed,
        );
        stats.change.file_replace.fetch_add(
            diff_stats.file_replace.load(Ordering::Relaxed) as usize,
            Ordering::Relaxed,
        );
        */
        change::reverse(changes.as_mut_slice());
        changes
    };

    // Filter changes by dependency set when root_files is specified
    let changes = if !options.root_files.is_empty() {
        let tags: Vec<&str> = options.dependency_tags.iter().map(|s| s.as_str()).collect();
        let root_refs: Vec<&str> = options.root_files.iter().map(|s| s.as_str()).collect();
        let inclusion_set = dependency::resolve::resolve_dependency_file_set(
            repository.clone(),
            state_target.clone(),
            &root_refs,
            &tags,
            options.dependency_recursive,
            options.dependency_depth_limit,
        )
        .await
        .forward::<SyncError>("Failed to resolve dependency set")?;

        let change_count = changes.len();
        let filtered: Vec<NodeChange> = changes
            .into_iter()
            .filter(|change| match change.action {
                change::FileAction::Delete => inclusion_set.contains(&change.from.node),
                _ => inclusion_set.contains(&change.to.node),
            })
            .collect();
        lore_info!(
            "Dependency filter: {} of {} changes in inclusion set",
            filtered.len(),
            change_count
        );
        filtered
    } else {
        changes
    };

    let context = execution_context();
    let globals = context.globals();
    let force = globals.force();
    let dry_run = globals.dry_run();

    let options = Arc::new(options);
    let changes = Arc::new(changes);
    let changes = if !changes.is_empty() && !force && !options.reset {
        lore_info!("Verifying {} changes with local file system", changes.len());
        verify_filesystem_for_changes(Arc::new(SyncVerifyArgs {
            changes: changes.clone(),
            repository_current: repository.clone(),
            operation: operation.clone(),
            state_current: state_current.clone(),
            options: options.clone(),
        }))
        .await?
    } else {
        changes
    };

    realize_changes(
        repository, operation, changes, None, dry_run, false, /* Not a merge */
        stats,
    )
    .await?;

    Ok(())
}
pub async fn verify_filesystem_for_changes(
    args: Arc<SyncVerifyArgs<impl InstanceOperation + 'static>>,
) -> Result<Arc<Vec<NodeChange>>, SyncError> {
    // Queue up to a given number of parallel tasks to verify filesystem
    let mut failure = None;
    const MAX_TASK_COUNT: usize = 1000;
    let mut tasks = JoinSet::new();
    let mut changes = Vec::with_capacity(args.changes.len());
    let stats = Arc::new(SyncVerifyStats::default());
    for change in args.changes.iter() {
        lore_spawn!(tasks, {
            let forward_changes = args.options.forward_changes;
            let force_hash_check = args.options.force_hash_check;
            let filter_mode = args.options.filter_mode;
            let change = change.clone();
            let repository_current = args.repository_current.clone();
            let operation = args.operation.clone();
            let state_current = args.state_current.clone();
            let stats = stats.clone();
            async move {
                Box::pin(verify_filesystem(
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
            }
        });
        while tasks.len() > MAX_TASK_COUNT
            && let Some(result) = tasks.join_next().await
        {
            match result
                .internal("Recursion task failed")
                .map_err(SyncError::from)
                .flatten()
            {
                Ok(change) => {
                    if let Some(change) = change {
                        changes.push(change);
                    }
                }
                Err(err) => {
                    failure = failure.or(Some(err));
                }
            }
        }

        if failure.is_some() {
            break;
        }
    }
    // Wait for the remaining tasks
    while let Some(result) = tasks.join_next().await {
        match result
            .internal("Recursion task failed")
            .map_err(SyncError::from)
            .flatten()
        {
            Ok(change) => {
                if let Some(change) = change {
                    changes.push(change);
                }
            }
            Err(err) => {
                failure = failure.or(Some(err));
            }
        }
    }

    if let Some(err) = failure {
        return Err(err);
    }

    // Re-sort after parallel verification which collects results in completion order.
    // Parent directories must appear before their children so that directory renames
    // in the realize path complete before child file operations are spawned.
    change::sort_by_path(&mut changes);

    Ok(Arc::new(changes))
}

#[allow(clippy::too_many_arguments)]
pub async fn verify_filesystem(
    change: NodeChange,
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    state_current: Arc<State>,
    forward_changes: bool,
    force_full_check: bool,
    stats: Arc<SyncVerifyStats>,
    filter_mode: FilterMode,
) -> Result<Option<NodeChange>, SyncError> {
    lore_trace!("Verify path: {change:?}");
    let modifications = operation
        .is_file_modified(repository.clone(), &change, force_full_check)
        .await?;

    if !modifications.info.exists {
        return match change.action {
            change::FileAction::Add => {
                // Nothing exist in file system, safe to add
                lore_trace!(
                    "Nothing exist in file system for {}, safe to add",
                    change.path
                );
                Ok(Some(change))
            }
            change::FileAction::Delete => {
                // Nothing exists, delete is a no-op
                lore_trace!(
                    "Nothing exist in file system for {}, delete is no-op",
                    change.path
                );
                Ok(Some(change))
            }
            _ => {
                if forward_changes {
                    lore_info!("Keeping modified file as locally deleted: {}", change.path);
                    Ok(None)
                } else {
                    lore_trace!(
                        "Restoring modified file which was locally deleted: {}",
                        change.path
                    );
                    Ok(Some(change))
                }
            }
        };
    }

    let is_file = modifications.info.is_file;
    let mut file_hash = Hash::default();

    let file_size = modifications.info.size;

    if let Some(modification) = modifications.modification {
        // Check if file is modified
        if !modification.modified {
            stats.file_retain.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(change));
        } else {
            if !modification.hash.is_zero() {
                file_hash = modification.hash;
            }
            stats.file_replace.fetch_add(1, Ordering::Relaxed);
        }
    }

    let is_delete = change.action == change::FileAction::Delete;
    let was_link = change.from.flags.bits() & NodeFlags::Link != 0;

    if is_delete && was_link {
        lore_debug!("Link is for delete, skipping filesystem verification");
        return Ok(Some(change));
    }

    let node_to = if !is_delete {
        change
            .to
            .get_node()
            .await
            .forward::<SyncError>("Failed loading node")?
    } else {
        Node::new_zeroed()
    };

    let should_be_file = !is_delete && node_to.is_file();

    if is_file {
        // At this point it is a modified file in file system, either going from directory->file
        // or remaining a file that has been modified. Otherwise, the earlier tests would have
        // earlied out and verified the change.
        if !should_be_file || is_delete {
            if is_delete {
                if forward_changes {
                    lore_info!("Keeping deleted file as locally modified: {}", change.path);
                    return Ok(None);
                }
                lore_error!(
                    "Deleted file is currently modified in file system: {}",
                    change.path
                );

                return Err(LocalModifications.into());
            }

            // If this is a change from file to directory, there will have been a previous change
            // which deletes the existing file node which will have verified the filesystem
            // state already - just allow the new directory to be created
            if forward_changes {
                lore_info!(
                    "Keeping created directory as a locally modified file: {}",
                    change.path
                );
                return Ok(None);
            }

            lore_trace!(
                "Change {} from file to directory, previous change will have deleted it",
                change.path
            );
            return Ok(Some(change));
        }

        // At this point (not deleted and target is a file) the to block is valid and the remaining
        // check is to see if the local file system file matches the target incoming file
        if file_hash.is_zero() {
            file_hash = operation
                .file_hash(change.from.repository.clone(), &change.path, Some(&node_to))
                .await
                .unwrap_or_default();
        }
        lore_trace!(
            "File {} hash {} : to hash {}",
            change.path,
            file_hash,
            node_to.address.hash
        );
        if file_hash != node_to.address.hash {
            if forward_changes {
                lore_info!("Keeping modified file as locally modified: {}", change.path);
                return Ok(None);
            }

            // Hash mismatch with matching size may be caused by chunking strategy
            // differences rather than actual content changes. Compare actual content
            // to determine if the file is truly modified.
            if file_size == node_to.size
                && file_size <= state::CONTENT_COMPARE_MAX_SIZE
                && operation
                    .file_compare(
                        change.from.repository.clone(),
                        node_to.address,
                        &change.path,
                        file_size,
                    )
                    .await?
            {
                lore_trace!(
                    "File {} hash mismatch with target but content equal (chunking compatibility)",
                    change.path
                );
                return Ok(None);
            }

            lore_error!(
                "File has local changes: {} (incoming size {} bytes, file system size {} bytes)",
                change.path,
                node_to.size,
                file_size
            );

            return Err(LocalModifications.into());
        }

        return Ok(None);
    }

    // At this point the local file system has a directory
    if should_be_file || is_delete {
        // If the local directory has any modified files we cannot delete it. If everything
        // matches the current state it's fine to recursively delete the directory. If it
        // has locally added files that are view/ignore filtered these should be retained,
        // which will also keep the directory which will later show up as a local add,
        // reminding the user they have locally added files in that path that should be
        // dealt with manually (we should not delete them and risk data loss!)
        // TODO(mjansson): Add early out path to compare to just get a boolean indicator
        //                 if there are changes or not - we don't want the actual changes
        if forward_changes {
            lore_info!(
                "Keeping modified/deleted file as a local directory: {}",
                change.path
            );
            return Ok(None);
        }

        if is_delete {
            let current_node_link = state_current
                .find_node_link(repository.clone(), change.path.as_str())
                .await
                .unwrap_or_default();
            let (repository_current, state_current) = current_node_link
                .resolve(repository.clone(), state_current.clone())
                .await
                .forward_with::<SyncError, _>(|| {
                    format!("Failed to deserialize state {}", current_node_link.revision)
                })?;
            let subnode_current = current_node_link.node;
            let state_from = change.from.state.clone();
            let (directory_changes, _) = operation
                .changes_from_filesystem_to_state(
                    change.from.repository.clone(),
                    state_from.clone(),
                    repository_current,
                    state_current.clone(),
                    change.path.clone(),
                    change.from.node,
                    subnode_current,
                    filter_mode,
                )
                .await
                .forward::<SyncError>(
                    "Failed to calculate delta changes between file system and target state",
                )?;
            if !directory_changes.is_empty() {
                let mut has_modified_file = false;
                for subchange in directory_changes {
                    if subchange.action == change::FileAction::Add {
                        // Allow locally added files to remain and keep directory
                        lore_trace!(
                            "Allow locally added file in {}",
                            subchange
                                .path
                                .to_absolute_path(change.from.repository.require_path()?)
                                .display()
                        );
                        continue;
                    }

                    let file_info = operation
                        .file_info(FilesystemPath::Repository(&subchange.path))
                        .await
                        .ok();

                    // A tracked entry that is already missing on disk is
                    // effectively pre-aligned with the directory delete the
                    // destination branch is performing. There is nothing to
                    // lose by letting the switch proceed.
                    if subchange.action == change::FileAction::Delete
                        && file_info.as_ref().is_none_or(|info| !info.exists)
                    {
                        lore_trace!(
                            "Skip already-missing tracked entry inside deleted directory: {}",
                            subchange.path
                        );
                        continue;
                    }

                    if !has_modified_file {
                        lore_error!(
                            "Deleted directory has modified files in file system: {}",
                            change.path
                        );
                    }
                    has_modified_file = true;

                    let from_node = subchange.from.get_node().await;
                    if let Some(file_info) = file_info {
                        if file_info.is_dir {
                            lore_info!(
                                "  {} {}/",
                                subchange.action.as_string_short(),
                                subchange.path
                            );
                        } else {
                            let file_hash = operation
                                .file_hash(
                                    change.from.repository.clone(),
                                    &subchange.path,
                                    from_node.as_ref().ok(),
                                )
                                .await
                                .unwrap_or_default();
                            lore_info!(
                                "  {} {} : size {} hash {} mtime {}",
                                subchange.action.as_string_short(),
                                subchange.path,
                                file_info.size,
                                file_hash,
                                file_info.mtime
                            );
                        }
                    } else {
                        lore_info!("  Failed to get local file info");
                    }

                    if subchange.from.node.is_valid_node_id() {
                        if let Ok(node) = from_node {
                            lore_info!(
                                "  Revision state   : mode {:o} size {} hash {}",
                                node.mode,
                                node.size,
                                node.address.hash,
                            );
                        } else {
                            lore_info!("  Revision state node block deserialize failed");
                        }
                    }
                }

                if has_modified_file {
                    return Err(LocalModifications.into());
                }
            }
            return Ok(Some(change));
        }
        // If this is a change from directory to file, there will have been a previous change
        // which deletes the existing directory node which will have verified the filesystem
        // state already - just allow the new file to be written
        lore_trace!(
            "Change {} from directory to file, previous change will have deleted it",
            change.path
        );
        return Ok(Some(change));
    }

    Ok(Some(change))
}

pub async fn realize_changes(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    changes: Arc<Vec<NodeChange>>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    let _ticker = progress_ticker(stats.clone());

    lore_debug!("Realize {} changes", changes.len());

    // Count delete total upfront (cheap iteration, no I/O)
    let file_delete_total = changes
        .iter()
        .filter(|c| c.action == change::FileAction::Delete)
        .count();
    stats
        .complete
        .file_delete_total
        .store(file_delete_total, Ordering::Relaxed);

    // First perform all deletes in case of delete-add for going from/to file & directory
    realize_changes_delete(
        repository.clone(),
        operation.clone(),
        changes.clone(),
        state_stage.clone(),
        dry_run,
        is_merge,
        stats.clone(),
    )
    .await?;
    lore_debug!("Deleted paths realized");

    // For an incoming change that introduces a destination path whose parent
    // directory is absent from the staged Merkle state (e.g. the current
    // branch deleted the parent), pre-stage the missing ancestors as
    // directory nodes so `stage_single_node` for the change resolves its
    // parent. Mirrors the ancestor staging done in `realize_conflicts`.
    // Only Add, Move, and Copy need this: those actions introduce a
    // destination path that the target snapshot need not contain. A Modify
    // on a path beneath a target-deleted directory always pairs with the
    // target's delete as a conflict and is routed through the conflict
    // realization path instead. `change.path` is the destination for Move
    // and Copy (the source location is irrelevant for ancestor staging).
    if let Some(state_stage) = state_stage.as_ref() {
        let mut parent_paths: Vec<RelativePath> = Vec::new();
        for change in changes.iter() {
            if !matches!(
                change.action,
                change::FileAction::Add | change::FileAction::Move | change::FileAction::Copy
            ) {
                continue;
            }
            if let Some(parent) = change.path.parent() {
                parent_paths.push(
                    RelativePath::new_from_initial_path(parent)
                        .expect("parent derived from valid change path"),
                );
            }
        }

        let stage_flags = if is_merge {
            NodeFlags::StagedMerge.bits()
        } else {
            NodeFlags::NoFlags.bits()
        };
        for stage_path in expand_path_ancestors(parent_paths) {
            let node = Node {
                flags: stage_flags,
                ..Default::default()
            };
            stage::stage_single_node(
                repository.clone(),
                state_stage.clone(),
                stage_path,
                node,
                Arc::default(),
                None,
                FilterMode::empty(),
            )
            .await
            .forward::<SyncError>("Failed to stage change")?;
        }
    }

    // Channel pipeline for add/modify changes
    let (tx, rx) = mpsc::channel(DEFAULT_WORK_CHANNEL_CAPACITY);

    let discover_stats = stats.clone();
    let producer = lore_spawn!(async move {
        let result = sync_discover_modify_add(changes, discover_stats.clone(), tx).await;
        discover_stats
            .discovery
            .complete
            .store(true, Ordering::Relaxed);
        // Send a progress event immediately when discovery finishes,
        // ensuring at least one progress event has discoveryComplete=true
        event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(
            &discover_stats,
        ))
        .send();
        result
    });

    let consumer_stats = stats.clone();
    // A view-excluded path is staged into the tree but not written to disk.
    let view_filter = repository.filter.clone();
    let consumer = lore_spawn!(async move {
        sync_execute_modify_add(
            rx,
            operation,
            state_stage,
            dry_run,
            is_merge,
            view_filter,
            consumer_stats,
        )
        .await
    });

    let (producer_result, consumer_result) = tokio::join!(producer, consumer);
    lore_debug!("Modified/added paths realized");

    event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(&stats)).send();

    let producer_result = producer_result.internal("Recursion task failed")?;
    let consumer_result = consumer_result.internal("Recursion task failed")?;
    if let Err(err) = consumer_result {
        execution_context().failure.store(true, Ordering::Relaxed);
        return Err(err);
    }
    if let Err(err) = producer_result {
        execution_context().failure.store(true, Ordering::Relaxed);
        return Err(err);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn realize_conflicts(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    state_base: Arc<State>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    state_stage: Option<Arc<State>>,
    conflicts: Arc<Vec<(NodeChange, NodeChange)>>,
    dry_run: bool,
    stats: Arc<SyncRealizeStats>,
    merge_type: MergeType,
) -> Result<(), SyncError> {
    let _ticker = progress_ticker(stats.clone());

    if let Some(state_stage) = state_stage.as_ref() {
        // Collect all the paths we're realizing files into and ensure they exist
        // They can be removed in case they were deleted but resolved as keep
        let mut parent_paths: Vec<RelativePath> = Vec::with_capacity(conflicts.len());
        for (change_from, change_to) in conflicts.iter() {
            if let Some(parent) = change_to.path.parent() {
                parent_paths.push(
                    RelativePath::new_from_initial_path(parent)
                        .expect("parent is valid from above"),
                );
            }
            // Also ensure parent directories for source change path exist
            // (needed for divergent move conflicts where paths differ)
            if change_from.path != change_to.path
                && let Some(parent) = change_from.path.parent()
            {
                parent_paths.push(
                    RelativePath::new_from_initial_path(parent)
                        .expect("parent is valid from above"),
                );
            }
        }

        // Deduplicate paths to avoid staging the same path twice
        for stage_path in expand_path_ancestors(parent_paths) {
            let node = Node {
                flags: NodeFlags::StagedMerge.bits(),
                ..Default::default()
            };
            stage::stage_single_node(
                repository.clone(),
                state_stage.clone(),
                stage_path,
                node,
                Arc::default(),
                None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                FilterMode::View,
            )
            .await
            .forward::<SyncError>("Failed to stage change")?;
        }
    }

    lore_debug!("Realize {} conflicts", conflicts.len());

    realize_changes_merge(
        repository.clone(),
        operation,
        state_base.clone(),
        state_from.clone(),
        state_to.clone(),
        state_stage.clone(),
        conflicts.clone(),
        dry_run,
        repository.filter.clone(),
        stats.clone(),
        merge_type,
    )
    .await?;
    lore_debug!("Merged paths realized");

    event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(&stats)).send();

    Ok(())
}

pub async fn realize_scratch_file(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    path: impl AsRef<Path>,
    node: Node,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    let path = path.as_ref();
    if let Some(parent_path) = path.parent() {
        operation
            .create_dir_all(FilesystemPath::Scratch(parent_path))
            .await?;
    }

    if node.size > 0 {
        operation
            .set_file_to_immutable_store_contents(
                repository.clone(),
                &node,
                FilesystemPath::Scratch(path),
            )
            .await
            .forward_with::<SyncError, _>(|| {
                format!("Failed to sync file {}", path.to_string_lossy())
            })?;
    } else {
        operation
            .create_file(FilesystemPath::Scratch(path))
            .await
            .forward_with::<SyncError, _>(|| {
                format!("Failed to sync file {}", path.to_string_lossy())
            })?;
    }

    let node_executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
    if node_executable {
        operation
            .make_executable(FilesystemPath::Scratch(path))
            .await?;
    }

    let info = operation.file_info(FilesystemPath::Scratch(path)).await?;

    lore_trace!(
        "Realized file {} {} bytes (target file {} bytes) {}",
        path.display(),
        node.size,
        info.size,
        node.address.hash
    );

    stats.complete.file_update.fetch_add(1, Ordering::Relaxed);
    stats
        .complete
        .bytes_update
        .fetch_add(node.size, Ordering::Relaxed);

    Ok(())
}

pub async fn realize_file(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    path: &RelativePath,
    node: Node,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    let mut parent_path = path.clone();
    let parent_path = parent_path.pop();
    if parent_path != path {
        operation
            .create_dir_all(FilesystemPath::Repository(parent_path))
            .await?;
    }

    if node.size > 0 {
        operation
            .set_file_to_immutable_store_contents(
                repository.clone(),
                &node,
                FilesystemPath::Repository(path),
            )
            .await
            .forward_with::<SyncError, _>(|| format!("Failed to sync file {path}"))?;
    } else {
        operation
            .create_file(FilesystemPath::Repository(path))
            .await
            .forward_with::<SyncError, _>(|| format!("Failed to sync file {path}"))?;
    }

    let node_executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
    if node_executable {
        operation
            .make_executable(FilesystemPath::Repository(path))
            .await?;
    }

    let info = operation
        .file_info(FilesystemPath::Repository(path))
        .await?;

    lore_trace!(
        "Realized file {} {} bytes (target file {} bytes) {}",
        path.to_string(),
        node.size,
        info.size,
        node.address.hash
    );

    stats.complete.file_update.fetch_add(1, Ordering::Relaxed);
    stats
        .complete
        .bytes_update
        .fetch_add(node.size, Ordering::Relaxed);

    Ok(())
}

fn progress_ticker(stats: Arc<SyncRealizeStats>) -> AbortOnDropHandle<()> {
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    AbortOnDropHandle::new(lore_spawn!(async move {
        loop {
            ticker.tick().await;
            event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(&stats))
                .send();
        }
    }))
}

async fn realize_changes_delete(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    changes: Arc<Vec<NodeChange>>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    // Sort the changes by path length and iterate in descending order. This will make sure that
    // directories have files deleted first, before attempting to delete the directory.
    // If a directory still have local files which are view/ignore filtered, these directories
    // should not be deleted in order to prevent any potential data loss. Instead the user
    // should manually clean out these directories (or use purge).
    // This also requires all deleted to execute in sequence.
    // TODO(mjansson): Group and let unrelated paths execute in parallel with spawn
    let mut delete_changes: Vec<NodeChange> = changes
        .iter()
        .filter(|change| change.action == change::FileAction::Delete)
        .cloned()
        .collect();
    change::sort_by_path(delete_changes.as_mut_slice());
    for change in delete_changes.iter().rev() {
        let change = change.clone();

        let (state_from, stats) = (change.from.state.clone(), stats.clone());

        // TODO(mjansson): File system virtualization
        /*
        if (repository->virtualized) {
            err = repository->virtualization->delete_node(repository, change->path);
            return err;
        }
        */

        let absolute_path = change
            .path
            .to_absolute_path(change.from.repository.require_path()?);
        let path = &change.path;

        let is_link = change.from.flags.bits() & NodeFlags::Link != 0;

        let is_file = if is_link {
            false
        } else if change.from.node.is_valid_node_id() {
            let block = state_from
                .block(
                    change.from.repository.clone(),
                    NodeBlock::index(change.from.node),
                )
                .await
                .forward::<SyncError>("Failed deserializing state node block")?;
            let node = block.node(Node::index(change.from.node));

            node.is_file()
        } else {
            // This can happen if a local path needs to be deleted as a
            // result of a <state> vs <filesystem> diff.
            operation
                .file_info(FilesystemPath::Repository(path))
                .await?
                .is_file
        };

        lore_trace!("D {}", change.path);
        let mut deleted = true;
        if !dry_run {
            let mut retry = util::fs::file_unlink_retry();
            loop {
                let absolute_path = absolute_path.as_path();
                if is_link {
                    if let Err(err) = operation
                        .remove_recursive(FilesystemPath::Repository(path))
                        .await
                    {
                        lore_debug!(
                            "Unable to unlink linked repository files at {}: {} (attempt {} of {}",
                            absolute_path.display(),
                            err,
                            retry.counter() + 1,
                            retry.limit()
                        );
                        if !retry.wait().await {
                            return SyncError::internal(format!(
                                "Failed to remove file or directory from local file system {}",
                                absolute_path.display()
                            ))
                            .emit();
                        }
                    } else {
                        break;
                    }
                } else if let Err(err) = operation.remove(FilesystemPath::Repository(path)).await {
                    // Retry if it is a file, otherwise assume the directory has local files
                    if is_file {
                        lore_trace!(
                            "Unable to unlink local path {}: {} (attempt {} of {})",
                            absolute_path.display(),
                            err,
                            retry.counter() + 1,
                            retry.limit()
                        );
                        if !retry.wait().await {
                            return SyncError::internal(format!(
                                "Failed to remove file or directory from local file system {}",
                                absolute_path.display()
                            ))
                            .emit();
                        }
                    } else {
                        // Directory unlink failed, keep it and the locally added/modified files
                        deleted = false;
                        break;
                    }
                } else {
                    break;
                }
            }

            state::file_modified_time_clear(repository.clone(), change.path.as_str()).await;
        }

        stats.complete.file_delete.fetch_add(1, Ordering::Relaxed);

        if deleted {
            event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(
                &change, 0, is_file,
            ))
            .send();
        }

        if let Some(state_stage) = state_stage.clone() {
            let node_link = match state_stage
                .find_node_link(change.from.repository.clone(), change.path.as_str())
                .await
            {
                Ok(node_link) => node_link,
                Err(e) if e.is_node_not_found() => NodeLink::invalid(),
                Err(err) => Err(err).forward::<SyncError>("Failed to stage change")?,
            };

            if node_link.is_valid() {
                stage::stage_delete(
                    change.from.repository.clone(),
                    state_stage.clone(),
                    node_link.node,
                    if is_merge {
                        NodeFlags::StagedMerge
                    } else {
                        NodeFlags::NoFlags
                    },
                    Arc::new(stage::StageStats::default()),
                    None, // TODO(vri): UCS-18008 - Investigate link tracking for sync/realize_changes
                )
                .await
                .forward::<SyncError>("Failed to stage change")?;
            }
        }
    }

    Ok(())
}

struct SyncWorkItem {
    change: NodeChange,
    node: Node,
}

async fn sync_discover_modify_add(
    changes: Arc<Vec<NodeChange>>,
    stats: Arc<SyncRealizeStats>,
    tx: mpsc::Sender<SyncWorkItem>,
) -> Result<(), SyncError> {
    for change in changes.as_ref().iter() {
        if change.action == change::FileAction::Delete {
            continue;
        }

        let (repository, state_to) = (change.to.repository.clone(), change.to.state.clone());
        let block = state_to
            .block(repository.clone(), NodeBlock::index(change.to.node))
            .await
            .forward::<SyncError>("Failed deserializing state node block")?;
        let node = block.node(Node::index(change.to.node));

        if node.is_file() {
            stats.discovery.total_files.fetch_add(1, Ordering::Relaxed);
            stats
                .discovery
                .total_bytes
                .fetch_add(node.size, Ordering::Relaxed);
        }

        if tx
            .send(SyncWorkItem {
                change: change.clone(),
                node,
            })
            .await
            .is_err()
        {
            // Receiver dropped, consumer encountered an error
            return SyncError::internal("Recursion task failed").emit();
        }
    }
    Ok(())
}

async fn sync_execute_modify_add(
    mut rx: mpsc::Receiver<SyncWorkItem>,
    operation: Arc<impl InstanceOperation + 'static>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    const MAX_TASK_COUNT: usize = 10000;
    let mut tasks = JoinSet::new();
    let mut sync_error = None;

    while let Some(item) = rx.recv().await {
        let result = realize_change_modify_add(
            &mut tasks,
            operation.clone(),
            item.change,
            item.node,
            state_stage.clone(),
            dry_run,
            is_merge,
            view_filter.clone(),
            stats.clone(),
        )
        .await;
        sync_error = sync_error.or(result.err());

        while let Some(result) = tasks.try_join_next() {
            sync_error = sync_error.or(result
                .internal("Recursion task failed")
                .map_err(SyncError::from)
                .flatten()
                .err());
        }
        while tasks.len() > MAX_TASK_COUNT
            && let Some(result) = tasks.join_next().await
        {
            sync_error = sync_error.or(result
                .internal("Recursion task failed")
                .map_err(SyncError::from)
                .flatten()
                .err());
        }

        if sync_error.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        sync_error = sync_error.or(result
            .internal("Recursion task failed")
            .map_err(SyncError::from)
            .flatten()
            .err());
    }

    if let Some(err) = sync_error {
        Err(err)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn realize_change_modify_add(
    tasks: &mut JoinSet<Result<(), SyncError>>,
    operation: Arc<impl InstanceOperation + 'static>,
    change: NodeChange,
    node: Node,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    // During a merge this is the filter-free walk context, not the instance's.
    // The instance's view arrives separately as `view_filter`. The staging
    // below needs the first, the disk writes need the second.
    let repository = change.to.repository.clone();
    let size = node.size;
    let is_file = node.is_file();
    let path = &change.path;

    // Only on-disk work honours the view. This gates directory creation, link
    // cloning and the file write. The move rename below is not gated, because
    // it repositions a path an earlier in-view realize may have written.
    let write_to_disk = !view_filter.excludes(path, node.is_directory(), FilterMode::View);

    // A move is realized by renaming the file already on disk, which lets the
    // content write be skipped when the content did not change. That only holds
    // when the source was in view and so had a file to rename. A sparse working
    // tree holds nothing at an excluded source, so the rename finds no file and
    // the destination has to be written from the immutable store like any add.
    let moved_from_in_view = change.from_path.as_ref().is_some_and(|from_path| {
        !view_filter.excludes(from_path, node.is_directory(), FilterMode::View)
    });

    lore_trace!(
        "{}{} {}",
        change.action.as_string_short(),
        if change.flags.is_conflict() { "!" } else { " " },
        path
    );

    event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(&change, size, is_file))
        .send();

    if !dry_run
        && change.action == change::FileAction::Move
        && let Some(from_path) = change.from_path.as_ref()
    {
        let to_path = path.clone();
        if operation
            .unify_case_rename(
                FilesystemPath::Repository(from_path),
                FilesystemPath::Repository(&to_path),
            )
            .await
            .is_err()
        {
            lore_trace!("Failed renaming move node, fall back to deleting and recreating");
            operation
                .remove_recursive(FilesystemPath::Repository(&to_path))
                .await
                .forward::<SyncError>("Failed to realize move/rename")?;
        }
    }

    if (node.is_directory() || node.is_link()) && write_to_disk {
        if !dry_run
            && operation
                .create_dir_all(FilesystemPath::Repository(path))
                .await
                .is_err()
            && operation
                .file_info(FilesystemPath::Repository(path))
                .await
                .is_ok_and(|info| !info.is_dir)
        {
            return SyncError::internal(format!("Failed to create directory {path}")).emit();
        }

        // When a link is added, the linked contents are not marked
        // for add. That's why we can just clone the linked files
        if node.is_link() && change.action == change::FileAction::Add {
            let link_id = node.address.context;
            let link_revision = node.address.hash;

            let link = Arc::new(repository.to_link_context(link_id.into()).await);
            let link_remote = link
                .remote()
                .await
                .forward::<SyncError>("Failed to connect to link remote")?;
            let correlation_id = execution_context().globals().correlation_id.to_string();
            let link_storage = link_remote
                .session(link.id, &correlation_id)
                .await
                .forward::<SyncError>("Failed to connect to link remote")?;
            let link_state = State::deserialize(link.clone(), link_revision)
                .await
                .forward_with::<SyncError, _>(|| {
                    format!("Failed to deserialize state {link_revision}")
                })?;

            let linked_node_path = link_state
                .node_path(link.clone(), node.child)
                .await
                .forward::<SyncError>("Failed to find linked node path")?;

            let source_path = RelativePath::new_from_initial_path(&linked_node_path)
                .forward::<SyncError>("Failed to find linked node path")?;

            let absolute_path = path.to_absolute_path(repository.require_path()?);

            let clone_stats = Arc::new(CloneStats::default());
            // Don't use the existing operation because virtualization needs to be resolved for the
            // linked repository separately.
            clone::clone_node(
                link.clone(),
                link_storage,
                link_state,
                absolute_path,
                source_path,
                node.child,
                Arc::default(), /* Default options */
                clone_stats.clone(),
            )
            .await
            .forward::<SyncError>("Failed to sync link")?;
        }
    } else if node.is_file() && !dry_run && write_to_disk {
        // For move changes where content didn't change, the rename already positioned the file correctly and the current branch's content should be preserved.
        if change.action != change::FileAction::Move
            || !moved_from_in_view
            || change.from.address.hash != change.to.address.hash
        {
            lore_spawn!(tasks, {
                let repository = repository.clone();
                let operation = operation.clone();
                let stats = stats.clone();
                let change_path = change.path.clone();
                async move { realize_file(repository, operation, &change_path, node, stats).await }
            });
        }
    }

    if let Some(state_stage) = state_stage.clone() {
        if change.action == change::FileAction::Move
            && let Some(from_path) = change.from_path.as_ref()
        {
            // For move actions, relink the existing node instead of creating a new one to preserve node identity and from_path tracking.
            // Find the node at the original path in the staged state
            let from_node_link = state_stage
                .find_node_link(repository.clone(), from_path.as_str())
                .await
                .forward::<SyncError>("Failed to stage change")?;
            let block_index = NodeBlock::index(from_node_link.node);
            let node_index = Node::index(from_node_link.node);
            let block = state_stage
                .block(repository.clone(), block_index)
                .await
                .forward::<SyncError>("Failed deserializing state node block")?;
            let mut from_node = block.node(node_index);

            // Determine the new parent node
            let mut parent_path = change.path.clone();
            parent_path.pop();
            let new_parent_node_link = state_stage
                .find_node_link(repository.clone(), parent_path.as_str())
                .await
                .forward::<SyncError>("Failed to stage change")?;
            let new_parent_id = new_parent_node_link.node;

            // Unlink the node from its current parent
            if from_node.parent != new_parent_id {
                let old_parent_block_index = NodeBlock::index(from_node.parent);
                let old_parent_node_index = Node::index(from_node.parent);
                let old_parent_block = state_stage
                    .block(repository.clone(), old_parent_block_index)
                    .await
                    .forward::<SyncError>("Failed deserializing state node block")?;
                let old_parent_node = old_parent_block.node(old_parent_node_index);

                if old_parent_node.child == from_node_link.node {
                    let dirtied = {
                        let mut block = old_parent_block.write();
                        block.node(old_parent_node_index).child = from_node.sibling;
                        block.mark_dirty()
                    };
                    if dirtied {
                        state_stage.block_modified(old_parent_block, old_parent_block_index);
                        state_stage.mark_dirty();
                    }
                } else {
                    let old_parent_id = from_node.parent;
                    let mut child_id = old_parent_node.child().unwrap_or_default();
                    let mut cycle = SiblingCycleGuard::new(old_parent_id);
                    while let Some(sibling) = {
                        let child =
                            state_stage
                                .node(repository.clone(), child_id)
                                .await
                                .forward::<SyncError>("Failed deserializing state node block")?;
                        child
                            .walk_step(child_id, old_parent_id, &mut cycle)
                            .forward::<SyncError>("Invalid node hierarchy in revision state")?;
                        child.sibling()
                    } {
                        if sibling == from_node_link.node {
                            let child_block_index = NodeBlock::index(child_id);
                            let child_node_index = Node::index(child_id);
                            let child_block = state_stage
                                .block(repository.clone(), child_block_index)
                                .await
                                .forward::<SyncError>("Failed deserializing state node block")?;
                            let dirtied = {
                                let mut block = child_block.write();
                                block.node(child_node_index).sibling = from_node.sibling;
                                block.mark_dirty()
                            };
                            if dirtied {
                                state_stage.block_modified(child_block, child_block_index);
                                state_stage.mark_dirty();
                            }
                            break;
                        }
                        child_id = sibling;
                    }
                }

                // Relink the same node to the new parent under the new path
                let new_parent_block_index = NodeBlock::index(new_parent_id);
                let new_parent_node_index = Node::index(new_parent_id);
                let new_parent_block = state_stage
                    .block(repository.clone(), new_parent_block_index)
                    .await
                    .forward::<SyncError>("Failed deserializing state node block")?;
                let sibling_node_id;
                let dirtied = {
                    let mut block = new_parent_block.write();
                    let parent_node = block.node(new_parent_node_index);
                    sibling_node_id = parent_node.child;
                    parent_node.child = from_node_link.node;
                    block.mark_dirty()
                };
                if dirtied {
                    state_stage.block_modified(new_parent_block, new_parent_block_index);
                    state_stage.mark_dirty();
                }
                from_node.sibling = sibling_node_id;
                from_node.parent = new_parent_id;
            }

            // Update node name if changed
            let from_name = from_path.name();
            let to_name = change.path.name();
            if from_name != to_name {
                block
                    .deserialize_nametable(repository.clone())
                    .await
                    .forward::<SyncError>("Failed deserializing state node block")?;
                from_node.name_hash = hash::hash_string(to_name);
                (from_node.name_offset, from_node.name_length) = block
                    .write()
                    .node_name_store(to_name, from_node.name_offset, from_node.name_length)
                    .forward::<SyncError>("Failed to store node name")?;
            }

            // Write back updated node data
            let dirtied = {
                let mut block = block.write();
                *block.node(node_index) = from_node;
                block.mark_dirty()
            };
            if dirtied {
                state_stage.block_modified(block, block_index);
                state_stage.mark_dirty();
            }

            // Mark the node with StagedMove and StagedMerge flags
            let mut mark_flags = NodeFlags::StagedMove;
            if is_merge {
                mark_flags |= NodeFlags::StagedMerge;
            }
            state_stage
                .node_mark(repository.clone(), from_node_link.node, mark_flags, true)
                .await
                .forward::<SyncError>("Failed to stage change")?;
        } else {
            let mut node = node;
            if is_merge {
                node.flags |= NodeFlags::StagedMerge;
            }
            if change.action == change::FileAction::Move {
                node.flags &= !NodeFlags::StagedAdd;
                node.flags |= NodeFlags::StagedMove;
            }

            stage::stage_single_node(
                repository.clone(),
                state_stage.clone(),
                change.path,
                node,
                Arc::new(stage::StageStats::default()),
                None, // TODO(vri): UCS-18008 - Investigate link tracking for sync/realize_changes
                FilterMode::View,
            )
            .await
            .forward::<SyncError>("Failed to stage change")?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn realize_changes_merge(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    state_base: Arc<State>,
    state_from: Arc<State>,
    state_to: Arc<State>,
    state_stage: Option<Arc<State>>,
    merges: Arc<Vec<(NodeChange, NodeChange)>>,
    dry_run: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
    merge_type: MergeType,
) -> Result<(), SyncError> {
    const MAX_TASK_COUNT: usize = 1000;
    let mut tasks = JoinSet::new();
    let mut failure = None;
    for (change_from, change_to) in merges.as_ref().iter() {
        lore_spawn!(tasks, {
            let repository = repository.clone();
            let operation = operation.clone();
            let state_base = state_base.clone();
            let state_from = state_from.clone();
            let state_to = state_to.clone();
            let state_stage = state_stage.clone();
            let change_from = change_from.clone();
            let change_to = change_to.clone();
            let view_filter = view_filter.clone();
            let stats = stats.clone();
            async move {
                realize_file_merge(
                    repository,
                    operation,
                    state_base,
                    state_from,
                    state_to,
                    state_stage,
                    change_from,
                    change_to,
                    dry_run,
                    view_filter,
                    stats,
                    merge_type,
                )
                .await
            }
        });

        while tasks.len() > MAX_TASK_COUNT
            && let Some(result) = tasks.join_next().await
        {
            failure = failure.or(result
                .internal("Recursion task failed")
                .map_err(SyncError::from)
                .flatten()
                .err());
        }

        if failure.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        failure = failure.or(result
            .internal("Recursion task failed")
            .map_err(SyncError::from)
            .flatten()
            .err());
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn realize_file_merge(
    repository: Arc<RepositoryContext>,
    operation: Arc<impl InstanceOperation + 'static>,
    state_base: Arc<State>,
    state_from: Arc<State>,
    _state_to: Arc<State>,
    state_stage: Option<Arc<State>>,
    change_from: NodeChange,
    change_to: NodeChange,
    dry_run: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
    merge_type: MergeType,
) -> Result<(), SyncError> {
    // Handle conflicts
    lore_trace!("Try merge file {}", change_to.path);
    let mut resolved = false;
    let mut conflict = true;
    let mut size = 0;

    let in_view = !view_filter.excludes(&change_to.path, false, FilterMode::View);

    // A view-excluded path has no working-tree file, so there is nothing to
    // compare and no merge result to edit. Adopt the incoming side, recorded
    // with the same flag `merge resolve theirs` sets.
    let take_theirs = !in_view;
    if take_theirs {
        conflict = false;
    }

    if change_from.path == change_to.path {
        // Fetch base / theirs version for conflicting files and try to text merge,
        // if that fails fall back to leaving mine/theirs/base in the file system
        let mine_path = change_from.path.append_into_buf(MINE_SUFFIX).freeze();
        let theirs_path = change_from.path.append_into_buf(THEIRS_SUFFIX).freeze();
        let base_path = change_from.path.append_into_buf(BASE_SUFFIX).freeze();

        if in_view {
            let mut has_theirs = false;
            if change_from.to.node.is_valid_node_id() {
                lore_trace!("Change from has valid to node, realize theirs file {theirs_path}");
                let node_to = state_from
                    .block(repository.clone(), NodeBlock::index(change_from.to.node))
                    .await
                    .forward::<SyncError>("Failed deserializing state node block")?
                    .node(Node::index(change_from.to.node));

                // TODO(vri): Implement merging links/link nodes

                if node_to.is_directory() {
                    lore_trace!("Change from is a directory, no theirs file");
                } else if node_to.is_file() {
                    realize_file(
                        repository.clone(),
                        operation.clone(),
                        &theirs_path,
                        node_to,
                        Arc::default(),
                    )
                    .await?;
                    has_theirs = true;
                    size = node_to.size;
                }
            } else {
                lore_trace!("Change from has no valid to node, no theirs file");
            }

            if change_to.to.node.is_valid_node_id() {
                // Diff3 function takes care of identifying identical files
                // and mergeable operations such as delete in both branches etc
                // Only thing remaining is identifying diffable files and split
                // the mergeable files from unresolvable conflicts
                let absolute_path = change_from
                    .path
                    .to_absolute_path(repository.require_path()?);
                if has_theirs
                    && operation
                        .infer_is_diffable(FilesystemPath::Repository(&change_from.path))
                        .await?
                {
                    lore_trace!(
                        "Merge identified text file for merge: {}",
                        absolute_path.display()
                    );

                    if change_from.from.node.is_valid_node_id() {
                        lore_trace!(
                            "Change from has valid from node, realize base file {base_path}"
                        );
                        let node_from = state_base
                            .block(repository.clone(), NodeBlock::index(change_from.from.node))
                            .await
                            .forward::<SyncError>("Failed deserializing state node block")?
                            .node(Node::index(change_from.from.node));
                        realize_file(
                            repository.clone(),
                            operation.clone(),
                            &base_path,
                            node_from,
                            Arc::default(),
                        )
                        .await?;
                    } else {
                        lore_trace!("Change from has no valid from node, empty base file");
                        let _ = operation
                            .create_file(FilesystemPath::Repository(&base_path))
                            .await;
                    }

                    // Realize the "mine" file as the current file
                    let mine_abs_path = mine_path.to_absolute_path(repository.require_path()?);
                    operation
                        .copy_to_scratch_file(
                            FilesystemPath::Repository(&change_from.path),
                            &mine_abs_path,
                        )
                        .await
                        .forward_with::<SyncError, _>(|| {
                            format!("Failed to sync file {mine_path}")
                        })?;

                    // Try performing a text merge
                    let mode = if dry_run {
                        crate::merge::MergeTextMode::DryRun
                    } else {
                        let write_token = repository
                            .try_write_token()
                            .ok_or_else(|| SyncError::from(WriteRequired))?;
                        crate::merge::MergeTextMode::Write(write_token)
                    };
                    let merged = match operation
                        .merge3_text_by_path(
                            &base_path,
                            &mine_path,
                            &theirs_path,
                            &change_to.path,
                            mode,
                        )
                        .await
                    {
                        Err(err) => {
                            // Could not merge, maybe file from binary to text, fall back to
                            // mine/theirs conflict handling
                            lore_debug!(
                                "Merge as text failed base {}, mine {}, theirs {} - fallback to binary file conflict to {}: {}",
                                base_path,
                                change_to.path,
                                theirs_path,
                                absolute_path.display(),
                                err
                            );
                            false
                        }
                        Ok(true) => {
                            // Merged with conflict markers
                            lore_debug!(
                                "Merged as text with conflict markers, base {}, mine {}, theirs {}: {}",
                                base_path,
                                change_to.path,
                                theirs_path,
                                absolute_path.display()
                            );
                            true
                        }
                        Ok(false) => {
                            // Merged with no conflicts
                            lore_trace!(
                                "Merged as text without any line conflicts: {}",
                                absolute_path.display()
                            );
                            conflict = false;
                            resolved = true;
                            true
                        }
                    };

                    if merged && !conflict {
                        let _ = operation
                            .remove(FilesystemPath::Repository(&base_path))
                            .await;
                        let _ = operation
                            .remove(FilesystemPath::Repository(&theirs_path))
                            .await;
                        let _ = operation
                            .remove(FilesystemPath::Repository(&mine_path))
                            .await;
                    }
                } else {
                    lore_debug!(
                        "Merge identified binary file for unresolved conflict: {}",
                        absolute_path.display()
                    );

                    // Realize the base file for binary conflicts so users can compare
                    if change_from.from.node.is_valid_node_id() {
                        lore_trace!("Realize base file for binary conflict {base_path}");
                        let node_from = state_base
                            .block(repository.clone(), NodeBlock::index(change_from.from.node))
                            .await
                            .forward::<SyncError>("Failed deserializing state node block")?
                            .node(Node::index(change_from.from.node));
                        if node_from.is_file() {
                            realize_file(
                                repository.clone(),
                                operation.clone(),
                                &base_path,
                                node_from,
                                Arc::default(),
                            )
                            .await?;
                        }
                    }
                }
            } else {
                lore_trace!("Target state node does not exist (deleted)");
            }

            if dry_run {
                let _ = operation
                    .remove(FilesystemPath::Repository(&base_path))
                    .await;
                let _ = operation
                    .remove(FilesystemPath::Repository(&theirs_path))
                    .await;
                let _ = operation
                    .remove(FilesystemPath::Repository(&mine_path))
                    .await;
            }
        }

        if let Some(state_stage) = state_stage.clone() {
            let mut node = if take_theirs && change_from.to.node.is_valid_node_id() {
                change_from
                    .to
                    .state
                    .node(change_from.to.repository.clone(), change_from.to.node)
                    .await
                    .forward::<SyncError>("Failed to resolve node in merge revisions")?
            } else if change_to.to.node.is_valid_node_id() {
                change_to
                    .to
                    .state
                    .node(change_to.to.repository.clone(), change_to.to.node)
                    .await
                    .forward::<SyncError>("Failed to resolve node in merge revisions")?
            } else if change_from.to.node.is_valid_node_id() {
                change_from
                    .to
                    .state
                    .node(change_from.to.repository.clone(), change_from.to.node)
                    .await
                    .forward::<SyncError>("Failed to resolve node in merge revisions")?
            } else {
                // Should not happen
                lore_error!("Unexpected merge conflict of deleted file in both incoming revisions");
                return Err(SyncError::internal("Invalid change data"));
            };
            if take_theirs {
                size = node.size;
            }
            if take_theirs && !change_from.to.node.is_valid_node_id() {
                // The incoming side deleted the path, so adopting it deletes.
                node.flags |= NodeFlags::StagedDelete;
            } else if conflict && !change_to.to.node.is_valid_node_id() {
                node.flags |= NodeFlags::StagedDelete;

                operation
                    .remove(FilesystemPath::Repository(&change_from.path))
                    .await?;
            }

            node.flags = NodeFlags::from_bits_truncate(node.flags)
                .bitand(NodeFlags::File | NodeFlags::Link)
                .bits();

            node.flags |= NodeFlags::StagedMerge;
            if take_theirs {
                node.flags |= NodeFlags::StagedMergeTheirs;
            }
            if conflict {
                node.flags |= NodeFlags::StagedMergeConflict;
            }
            if resolved {
                node.flags |= NodeFlags::StagedMergeResolved;
            }
            if change_to.action == change::FileAction::Move {
                node.flags &= !NodeFlags::StagedAdd;
                node.flags |= NodeFlags::StagedMove;
            }

            lore_trace!(
                "Staging conflict node in target state with flags {:x}",
                node.flags
            );

            // The change's own context carries no view filter, so a
            // view-excluded node reaches the staged state instead of being
            // filtered out of it.
            let stage_repository = if take_theirs {
                change_to.to.repository.clone()
            } else {
                repository.clone()
            };
            stage::stage_single_node(
                stage_repository,
                state_stage.clone(),
                change_to.path.clone(),
                node,
                Arc::default(),
                None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                FilterMode::View,
            )
            .await
            .forward::<SyncError>("Failed to stage change")?;

            if conflict {
                match merge_type {
                    MergeType::CherryPick => state_stage.set_cherry_pick_conflict(),
                    MergeType::BranchMerge => state_stage.set_merge_conflict(),
                    MergeType::Revert => state_stage.set_revert_conflict(),
                    MergeType::None => return Err(SyncError::internal("Invalid change data")),
                }
            }
        }
    } else if change_from.path.overlaps(&change_to.path) {
        // A conflict on paths that are NOT equal.
        // For example:
        //   File 'some_path' vs file 'some_path/some_file'
        // Both file and dir cannot exist at the same time, so mark as
        // conflicted in the target state if given
        lore_info!(
            "Merge overlapping paths {} and {}",
            change_from.path,
            change_to.path
        );
        lore_trace!("Change from: {change_from:?}");
        lore_trace!("Change to: {change_to:?}");

        if let Some(state_stage) = state_stage.clone() {
            lore_trace!("Staging conflict node in target state");
            let mut node = if change_to.to.node.is_valid_node_id() {
                change_to
                    .to
                    .state
                    .node(change_to.to.repository.clone(), change_to.to.node)
                    .await
                    .forward::<SyncError>("Failed to resolve node in merge revisions")?
            } else if change_from.to.node.is_valid_node_id() {
                change_from
                    .to
                    .state
                    .node(change_from.to.repository.clone(), change_from.to.node)
                    .await
                    .forward::<SyncError>("Failed to resolve node in merge revisions")?
            } else {
                // Should not happen
                lore_error!("Unexpected merge conflict of deleted file in both incoming revisions");
                return Err(SyncError::internal("Invalid change data"));
            };

            node.flags |= NodeFlags::StagedMergeConflict;

            stage::stage_single_node(
                repository.clone(),
                state_stage.clone(),
                change_to.path.clone(),
                node,
                Arc::default(),
                None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                FilterMode::View,
            )
            .await
            .forward::<SyncError>("Failed to stage change")?;

            match merge_type {
                MergeType::CherryPick => state_stage.set_cherry_pick_conflict(),
                MergeType::BranchMerge => state_stage.set_merge_conflict(),
                MergeType::Revert => state_stage.set_revert_conflict(),
                MergeType::None => return Err(SyncError::internal("Invalid change data")),
            }
        }
    } else {
        // Paths don't match and don't overlap - divergent move conflict.
        // The source branch moved the file to a new location while the target
        // branch also changed (moved/deleted) the file at the original location.
        lore_debug!(
            "Merge divergent move conflict: source {} vs target {}",
            change_from.path,
            change_to.path
        );

        if let Some(state_stage) = state_stage.clone() {
            let node = if change_from.to.node.is_valid_node_id() {
                change_from
                    .to
                    .state
                    .node(change_from.to.repository.clone(), change_from.to.node)
                    .await
                    .forward::<SyncError>("Failed to resolve node in merge revisions")?
            } else {
                lore_debug!("Divergent move conflict with no valid source node");
                return SyncError::internal("Invalid change data").emit();
            };

            // Realize the source file content on disk at the source move destination
            if !dry_run && node.is_file() {
                realize_file(
                    repository.clone(),
                    operation.clone(),
                    &change_from.path,
                    node,
                    Arc::default(),
                )
                .await?;
            }

            // Stage the node as a merge conflict at the source move destination
            let mut node = node;
            node.flags = NodeFlags::from_bits_truncate(node.flags)
                .bitand(NodeFlags::File | NodeFlags::Link)
                .bits();
            node.flags |= NodeFlags::StagedMergeConflict;

            stage::stage_single_node(
                repository.clone(),
                state_stage.clone(),
                change_from.path.clone(),
                node,
                Arc::default(),
                None, // TODO(vri): UCS-17955 - Merging and conflict resolution for links
                FilterMode::View,
            )
            .await
            .forward::<SyncError>("Failed to stage change")?;

            match merge_type {
                MergeType::CherryPick => state_stage.set_cherry_pick_conflict(),
                MergeType::BranchMerge => state_stage.set_merge_conflict(),
                MergeType::Revert => state_stage.set_revert_conflict(),
                MergeType::None => return Err(SyncError::internal("Invalid change data")),
            }
        }
    }

    if !conflict {
        stats
            .complete
            .file_automerge
            .fetch_add(1, Ordering::Relaxed);
    } else {
        stats.complete.file_conflict.fetch_add(1, Ordering::Relaxed);
    }

    event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(
        &change_to, size, true, /* is file */
    ))
    .send();

    Ok(())
}

impl LoreRevisionSyncFileEventData {
    fn new(node_change: &NodeChange, size: u64, file: bool) -> Self {
        Self {
            path: LoreString::from(&node_change.path),
            size,
            action: node_change.action.into(),
            flag_file: file.into(),
        }
    }
}
