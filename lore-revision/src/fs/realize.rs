// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Borrow;
use std::ops::BitAnd;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;
use zerocopy::FromZeros;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::branch::merge::MergeType;
use crate::change;
use crate::change::NodeChange;
use crate::dependency;
use crate::errors::LocalModifications;
use crate::errors::WriteRequired;
use crate::event;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::HoldOptions;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::MeasuredNode;
use crate::hash;
use crate::immutable;
use crate::interface::LoreString;
use crate::link::LinkFlags;
use crate::lore::BranchId;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_info;
use crate::lore_trace;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFileMode;
use crate::node::NodeFlags;
use crate::node::NodeID;
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
use crate::repository::clone::CloneContext;
use crate::revision::sync::LoreRevisionSyncFileEventData;
use crate::revision::sync::LoreRevisionSyncProgressEventData;
use crate::revision::sync::SyncError;
use crate::revision::sync::SyncOptions;
use crate::revision::sync::SyncRealizeStats;
use crate::revision::sync::SyncVerifyArgs;
use crate::revision::sync::SyncVerifyStats;
use crate::stage;
use crate::state;
use crate::state::NodeComparison;
use crate::state::NodeMapping;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::expand_path_ancestors;

/// Carries the working tree from `state_current` to `state_target`. A provider that holds its
/// state in a `State` is handed `state_target` alone and carries itself there
/// ([`InstanceOperationImpl::hold_state`]).
///
/// Each context answers for the state beside it: one for a revision change, and two for a view
/// change, where the tree holds what the current view materialized and is left holding what the
/// target view does. Every write is the target context's, since that is the view the tree is left
/// under.
///
/// A reset diffs the working tree against the target state instead. That walk asks the target
/// context's view alone and reads the current state through the context beside it, so the pair
/// carries one view however many are passed here.
pub async fn realize_state(
    repository_current: Arc<RepositoryContext>,
    repository_target: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
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

    if operation.holds_state() {
        let hold = {
            let context = execution_context();
            let globals = context.globals();
            HoldOptions {
                overwrite_local: globals.force(),
                keep_local: options.forward_changes,
                dry_run: globals.dry_run(),
            }
        };
        return operation
            .hold_state(repository_target, state_target, hold)
            .await
            .forward::<SyncError>("Holding the target revision");
    }

    let stats: Arc<SyncRealizeStats> = Arc::default();
    let changes = if !options.reset {
        lore_info!(
            "Calculating deltas {} -> {}",
            state_current.revision_number(),
            state_target.revision_number()
        );
        state::diff_collect(
            repository_current.clone(),
            state_current.clone(),
            repository_target.clone(),
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
        let mut changes = state::diff_filesystem_subtree(
            &operation,
            NodeMapping {
                repository: repository_target.clone(),
                state: state_target.clone(),
                path: RelativePath::new(),
                node: ROOT_NODE,
            },
            NodeMapping {
                repository: repository_current.clone(),
                state: state_current.clone(),
                path: RelativePath::new(),
                node: ROOT_NODE,
            },
            RelativePath::new(),
            options.filter_mode,
            FilesystemDiffIntent::ReportTracked,
            Arc::new(Vec::new()),
        )
        .await
        .forward::<SyncError>(
            "Failed to calculate delta changes between file system and target state",
        )?
        .collect()
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
            repository_target.clone(),
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
                change::FileAction::Delete => inclusion_set.contains(&change.from.mapping.node),
                _ => inclusion_set.contains(&change.to.mapping.node),
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
            repository_current: repository_current.clone(),
            operation: operation.clone(),
            current: NodeMapping::root(repository_current.clone(), state_current.clone()),
            options: options.clone(),
        }))
        .await?
    } else {
        changes
    };

    realize_changes(
        repository_target,
        operation,
        changes,
        None,
        dry_run,
        false, /* Not a merge */
        stats,
    )
    .await?;

    Ok(())
}

/// Verifies the changes `args` holds against the working copy, each as [`verify_filesystem`]
/// does, and answers those still having work to do, sorted by path.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn verify_filesystem_for_changes(
    args: Arc<SyncVerifyArgs>,
) -> impl Future<Output = Result<Arc<Vec<NodeChange>>, SyncError>> {
    async move {
        let mut failure = None;
        let mut tasks = JoinSet::new();
        let mut changes = Vec::with_capacity(args.changes.len());
        let stats = Arc::new(SyncVerifyStats::default());
        let incoming = Arc::new(IncomingMoves::new(args.changes.clone()));
        for change in args.changes.iter() {
            lore_spawn!(tasks, {
                let forward_changes = args.options.forward_changes;
                let force_hash_check = args.options.force_hash_check;
                let filter_mode = args.options.filter_mode;
                let change = change.clone();
                let repository_current = args.repository_current.clone();
                let operation = args.operation.clone();
                let current = args.current.clone();
                let stats = stats.clone();
                let incoming = incoming.clone();
                async move {
                    let mut change = change;
                    let realize = verify_filesystem(
                        &mut change,
                        repository_current,
                        operation,
                        current,
                        forward_changes,
                        force_hash_check,
                        stats,
                        filter_mode,
                        &incoming,
                    )
                    .await?;

                    Ok(realize.then_some(change))
                }
            });
            while tasks.len() > MAX_CONCURRENT_TREE_TASKS
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
}

/// The address of the content a change brings in. `NodeChangeState` carries it where the
/// producer set it, and the node holds it otherwise.
async fn incoming_address(change: &NodeChange) -> crate::lore::Address {
    if !change.to.address.is_zero() {
        return change.to.address;
    }

    change
        .to
        .get_node()
        .await
        .map(|node| node.address)
        .unwrap_or_default()
}

/// Whether the file holds the content the change starts from, which realizing the change
/// replaces.
///
/// A merge measures the file against the node the current revision holds, and a file reset to
/// an earlier revision matches neither that nor the incoming content while still being content
/// the change accounts for.
async fn holds_the_replaced_content(
    operation: &Arc<InstanceOperationImpl>,
    change: &NodeChange,
    file_size: u64,
    measured: Option<crate::lore::Address>,
    incoming: crate::lore::Address,
    established: &lore_storage::ContentHashes,
) -> Result<bool, SyncError> {
    if !change.from.mapping.node.is_valid_node_id() {
        return Ok(false);
    }

    let node_from = change
        .from
        .get_node()
        .await
        .forward::<SyncError>("Failed loading the node the change starts from")?;
    if !node_from.is_file() || Some(node_from.address) == measured || node_from.address == incoming
    {
        return Ok(false);
    }

    Ok(matches!(
        state::file_matches_node(
            change.from.mapping.repository.clone(),
            &node_from,
            file_size,
            change.path(),
            operation,
            established,
        )
        .await
        .forward::<SyncError>("Failed to compare the file to the node the change starts from")?,
        NodeComparison::Matches
    ))
}

/// How the working file compares to the node it was realized from, and whether the current
/// revision is what holds that node.
///
/// A three-way merge's from side is the base revision, which answers for nothing on disk, so
/// the node the current revision holds at the path stands in. Where the change starts at the
/// current revision the two are the same node, reached by id rather than by path, and where
/// the current revision holds none the from side is all there is. The node is measured in the
/// context that holds it, whose partition its content lives in.
async fn modification_against_measured_node(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    change: &NodeChange,
    current: &NodeMapping,
    force_full_check: bool,
    repository_path: &RelativePath,
    established: &lore_storage::ContentHashes,
) -> Result<crate::fs::filesystem_provider::FileModifiedCheck, SyncError> {
    let info = operation.file_info(repository_path).await?;
    if !info.exists() {
        return Ok(crate::fs::filesystem_provider::FileModifiedCheck {
            info,
            measured: None,
            modification: None,
        });
    }

    let from_is_current = change.from.mapping.state.revision() == current.state.revision();
    let node_link = if from_is_current {
        NodeLink::invalid()
    } else {
        current.node_at(change.path()).await
    };

    let (node, repository_measured, is_current) = if node_link.node.is_valid_node_id() {
        let (repository_current, state_node) = node_link
            .resolve(repository.clone(), current.state.clone())
            .await
            .forward_with::<SyncError, _>(|| {
                format!("Failed to deserialize state {}", node_link.revision)
            })?;
        let node = state_node
            .node(repository_current.clone(), node_link.node)
            .await
            .forward::<SyncError>("Failed loading the node the current revision holds")?;
        (Some(node), repository_current, true)
    } else if change.from.mapping.node.is_valid_node_id() {
        let node = change
            .from
            .get_node()
            .await
            .forward::<SyncError>("Failed to find node")?;
        (
            Some(node),
            change.from.mapping.repository.clone(),
            from_is_current,
        )
    } else {
        (None, repository, from_is_current)
    };

    let modification = match node.as_ref() {
        Some(node) if info.is_file() && node.is_file() => {
            let modification = if force_full_check {
                state::file_modification(
                    repository_measured,
                    node,
                    info.mtime(),
                    info.size(),
                    change.path(),
                    true,
                    operation,
                    established,
                )
                .await
            } else {
                state::file_modified_against_node(
                    repository_measured,
                    node,
                    info.mtime(),
                    info.size(),
                    change.path(),
                    is_current,
                    operation,
                    established,
                )
                .await
            }
            .forward::<SyncError>("Failed to check file modification")?;

            Some(crate::fs::filesystem_provider::FileDifferenceFromNode {
                modified: modification.is_modified(),
                mode_differs: info.mode_differs_from(node.mode),
            })
        }
        _ => None,
    };

    Ok(crate::fs::filesystem_provider::FileModifiedCheck {
        info,
        measured: node.map(|node| MeasuredNode { node, is_current }),
        modification,
    })
}

/// Carries the executable bit `wanted` names onto `path`, which is all a change whose content
/// the working tree already holds has left to apply.
///
/// Writing the file from the store would replace it with the bytes it already holds, so the bit
/// is set on its own. `carried` is the mode the path holds now; where the two already agree,
/// and under a dry run, nothing is written.
async fn carry_file_mode(
    operation: &InstanceOperationImpl,
    path: &RelativePath,
    carried: u16,
    wanted: u16,
) -> Result<(), SyncError> {
    if !util::fs::mode_changed(carried, wanted) || execution_context().globals().dry_run() {
        return Ok(());
    }
    operation
        .make_executable(
            path,
            wanted & NodeFileMode::Executable == NodeFileMode::Executable,
        )
        .await
        .forward_with::<SyncError, _>(|| format!("Failed to set the mode of {path}"))
}

/// Whether the executable bit the file a change realizes carries is the working tree's own, which
/// no revision gave it.
///
/// `measured` is how the file at the change's own path compared to the node it was realized from.
/// A move is realized by renaming the file its source holds, so the bit stands at the source and
/// the change's own path holds nothing to measure until the rename has run; where the source holds
/// no file the rename has carried it already and `measured` is what answers.
async fn mode_is_the_working_tree_own(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    measured: &crate::fs::filesystem_provider::FileModifiedCheck,
) -> Result<bool, SyncError> {
    let measured_differs = measured
        .modification
        .is_some_and(|difference| difference.mode_differs);
    let Some(source) = change.move_source() else {
        return Ok(measured_differs);
    };

    let moved = operation.file_info(source).await?;
    Ok(if moved.is_file() {
        moved.mode_differs_from(change.from.mode)
    } else {
        measured_differs
    })
}

/// The moves of a change set, looked up by the path each moves from.
pub struct IncomingMoves {
    changes: Arc<Vec<NodeChange>>,
    by_source: OnceLock<Vec<usize>>,
}

impl IncomingMoves {
    /// The moves `changes` holds, indexed on the first lookup.
    pub fn new(changes: Arc<Vec<NodeChange>>) -> Self {
        Self {
            changes,
            by_source: OnceLock::new(),
        }
    }

    fn source(change: &NodeChange) -> &str {
        change.move_source().map_or("", RelativePath::as_str)
    }

    /// The move carrying the node at `path` elsewhere, `path` spelled as the tree the moves start
    /// from spells it. A tree can hold two spellings of one name, each moved on its own.
    fn carrying(&self, path: &RelativePath) -> Option<&NodeChange> {
        let changes = &self.changes;
        let by_source = self.by_source.get_or_init(|| {
            let mut by_source: Vec<usize> = (0..changes.len())
                .filter(|&index| changes[index].move_source().is_some())
                .collect();
            by_source.sort_unstable_by(|&a, &b| {
                Self::source(&changes[a]).cmp(Self::source(&changes[b]))
            });
            by_source
        });
        by_source
            .binary_search_by(|&index| Self::source(&changes[index]).cmp(path.as_str()))
            .ok()
            .map(|found| &changes[by_source[found]])
    }
}

/// Whether deleting a directory would lose the entry `change` reports below it.
///
/// Nothing is lost of an added entry, which the non-recursive directory delete keeps, of a
/// tracked entry already missing, or of a file under a case variant of its name holding what the
/// node holds. A file that differs from its node is lost unless `incoming` holds a move that
/// carries it out of the directory to a node holding the same content, compared by size before
/// content.
async fn holds_local_work(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    incoming: Option<&IncomingMoves>,
) -> Result<bool, SyncError> {
    match change.action {
        change::FileAction::Add => return Ok(false),
        change::FileAction::Delete => {
            return Ok(operation
                .file_info(change.path())
                .await
                .is_ok_and(|info| info.exists()));
        }
        _ if change.is_case_variant() => return Ok(false),
        _ => {}
    }
    let Some(moved) = incoming.and_then(|incoming| incoming.carrying(&change.from.mapping.path))
    else {
        return Ok(true);
    };
    let (Some(observed), Ok(node)) = (change.to.observed, moved.to.get_node().await) else {
        return Ok(true);
    };
    let comparison = state::file_matches_node(
        moved.to.mapping.repository.clone(),
        &node,
        observed.size(),
        change.path(),
        operation,
        &lore_storage::ContentHashes::default(),
    )
    .await
    .forward::<SyncError>("Failed to compare the file to the incoming node")?;
    Ok(!matches!(comparison, NodeComparison::Matches))
}

/// Removes the files `directory` holds under a case variant of the name of a file a change in
/// `changes` deletes or moves away directly in it, where they hold the content the change starts
/// from or the content the move carries, and then `directory`, answering whether it removed the
/// directory. `changes` is sorted by the path each starts from.
///
/// A case-sensitive file system keeps a file spelled otherwise than the revision spells it, which
/// the delete or the move of the revision's spelling misses. A file holding other content,
/// compared by size before content, a directory, a link and a file no change folds to stay in
/// place, and keep the directory. The listing is drained before anything is removed, and of its
/// entries only the names of the files that fold to a change are kept.
///
/// Runs only where a directory delete finds the directory holding entries, a cold path its callers
/// box: inline, the content comparison would make every future awaiting a realize as large as its
/// own.
async fn remove_directory_of_case_variants<C: Borrow<NodeChange>>(
    operation: &InstanceOperationImpl,
    directory: &RelativePath,
    changes: &[C],
) -> bool {
    let prefix = format!("{directory}/");
    let first = changes
        .partition_point(|change| change.borrow().from.mapping.path.as_str() < prefix.as_str());
    let mut folded: Vec<(u64, &NodeChange)> = changes[first..]
        .iter()
        .map(Borrow::borrow)
        .map_while(|change| {
            Some((
                change,
                change.from.mapping.path.as_str().strip_prefix(&prefix)?,
            ))
        })
        .filter(|(change, name)| !name.contains('/') && change.from.flags.contains(NodeFlags::File))
        .map(|(change, name)| (hash::hash_string(name), change))
        .collect();
    if folded.is_empty() {
        return false;
    }
    folded.sort_unstable_by_key(|(name_hash, _)| *name_hash);
    let Ok(mut listing) = operation.read_directory(directory).await else {
        return false;
    };
    let mut variants = Vec::new();
    while let Some(entry) = listing.next().await {
        if let Ok(entry) = entry
            && entry.info.is_file()
            && let Ok(found) = folded.binary_search_by_key(&entry.name_hash, |(hash, _)| *hash)
        {
            variants.push((entry.name, entry.info.size(), folded[found].1));
        }
    }
    let mut removed = false;
    for (name, size, change) in &variants {
        let path = directory.join(name);
        let established = lore_storage::ContentHashes::default();
        let carried = change.move_source().map(|_| &change.to);
        for side in std::iter::once(&change.from).chain(carried) {
            let Ok(node) = side.get_node().await else {
                continue;
            };
            if let Ok(NodeComparison::Matches) = state::file_matches_node(
                side.mapping.repository.clone(),
                &node,
                *size,
                &path,
                operation,
                &established,
            )
            .await
            {
                removed |= operation.remove(&path).await.is_ok();
                break;
            }
        }
    }
    removed && operation.remove(directory).await.is_ok()
}

/// Checks `change` against the working copy, refusing the ones that would overwrite local work
/// and settling on it what realizing it has to know, and answers whether it still has work to do.
///
/// A caller that realizes every change it verified, which a merge does to build the staged state
/// out of them, takes the change as this leaves it and disregards the answer. One that realizes
/// only what the working copy still needs, which a sync does, drops the rest.
///
/// A file found to hold the incoming content has its time recorded where it is older than the
/// operation's clock floor ([`InstanceOperationImpl::clock_floor`]), which a comparison reads
/// before reading the file.
///
/// `incoming` holds the moves of the change set the change belongs to, which answer for the target
/// where a directory is deleted.
#[allow(clippy::too_many_arguments)]
pub async fn verify_filesystem(
    change: &mut NodeChange,
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    current: NodeMapping,
    forward_changes: bool,
    force_full_check: bool,
    stats: Arc<SyncVerifyStats>,
    filter_mode: FilterMode,
    incoming: &IncomingMoves,
) -> Result<bool, SyncError> {
    lore_trace!("Verify path: {change:?}");
    let repository_path = change.path().clone();
    // One file is measured against the node it was realized from, the incoming node and the
    // node the change starts at. What comparing it establishes serves all three.
    let established = lore_storage::ContentHashes::default();
    let modifications = modification_against_measured_node(
        &operation,
        repository.clone(),
        change,
        &current,
        force_full_check,
        &repository_path,
        &established,
    )
    .await?;

    if mode_is_the_working_tree_own(&operation, change, &modifications).await? {
        change.flags |= change::Flags::LocalMode;
    }

    if !modifications.info.exists() {
        return match change.action {
            change::FileAction::Add => {
                // Nothing exist in file system, safe to add
                lore_trace!(
                    "Nothing exist in file system for {}, safe to add",
                    change.path()
                );
                Ok(true)
            }
            change::FileAction::Delete => {
                // Nothing exists, delete is a no-op
                lore_trace!(
                    "Nothing exist in file system for {}, delete is no-op",
                    change.path()
                );
                Ok(true)
            }
            _ => {
                if forward_changes {
                    lore_info!(
                        "Keeping modified file as locally deleted: {}",
                        change.path()
                    );
                    Ok(false)
                } else {
                    lore_trace!(
                        "Restoring modified file which was locally deleted: {}",
                        change.path()
                    );
                    Ok(true)
                }
            }
        };
    }

    let is_file = modifications.info.is_file();
    let file_size = modifications.info.size();

    if let Some(modification) = modifications.modification {
        // Check if file is modified
        if !modification.modified {
            if change.action == change::FileAction::Keep
                && let Some(measured) = modifications.measured.as_ref()
                && measured.is_current
                && !measured.node.address.is_zero()
                && measured.node.address == incoming_address(change).await
            {
                if !change.flags.is_local_mode() {
                    carry_file_mode(
                        &operation,
                        change.path(),
                        modifications.info.mode(change.to.mode),
                        change.to.mode,
                    )
                    .await?;
                }
                if modifications.info.mtime() < operation.clock_floor(&repository).await {
                    operation.record_modified_time(
                        &change.from.mapping.repository,
                        change.path(),
                        modifications.info.mtime(),
                    );
                }
                return Ok(false);
            }
            stats.file_retain.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        stats.file_replace.fetch_add(1, Ordering::Relaxed);
    }

    let is_delete = change.action == change::FileAction::Delete;
    let was_link = change.from.is_link();

    if is_delete && was_link {
        lore_debug!("Link is for delete, skipping filesystem verification");
        return Ok(true);
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
                    lore_info!(
                        "Keeping deleted file as locally modified: {}",
                        change.path()
                    );
                    return Ok(false);
                }
                lore_error!(
                    "Deleted file is currently modified in file system: {}",
                    change.path()
                );

                return Err(LocalModifications.into());
            }

            // If this is a change from file to directory, there will have been a previous change
            // which deletes the existing file node which will have verified the filesystem
            // state already - just allow the new directory to be created
            if forward_changes {
                lore_info!(
                    "Keeping created directory as a locally modified file: {}",
                    change.path()
                );
                return Ok(false);
            }

            lore_trace!(
                "Change {} from file to directory, previous change will have deleted it",
                change.path()
            );
            return Ok(true);
        }

        let differs_from = modifications
            .modification
            .and(modifications.measured.as_ref())
            .map(|measured| measured.node.address);
        let comparison = if differs_from == Some(node_to.address) {
            NodeComparison::Differs
        } else {
            state::file_matches_node(
                change.from.mapping.repository.clone(),
                &node_to,
                file_size,
                change.path(),
                &operation,
                &established,
            )
            .await
            .forward::<SyncError>("Failed to compare the file to the incoming node")?
        };

        match comparison {
            NodeComparison::Differs => {
                if forward_changes {
                    lore_info!(
                        "Keeping modified file as locally modified: {}",
                        change.path()
                    );
                    return Ok(false);
                }

                if holds_the_replaced_content(
                    &operation,
                    change,
                    file_size,
                    differs_from,
                    node_to.address,
                    &established,
                )
                .await?
                {
                    return Ok(true);
                }

                lore_error!(
                    "File has local changes: {} (incoming size {} bytes, file system size {} bytes)",
                    change.path(),
                    node_to.size,
                    file_size
                );

                return Err(LocalModifications.into());
            }
            // Neither answer was established, so neither may be acted on: dropping the change
            // would leave the file behind its revision, and reporting local changes would name
            // the wrong problem.
            NodeComparison::Unreadable => {
                return Err(SyncError::internal(format!(
                    "Failed to read {} to compare it against the incoming revision",
                    change.path()
                )));
            }
            NodeComparison::Matches => {
                lore_trace!(
                    "File {} already holds the incoming content, nothing to realize",
                    change.path()
                );

                if !change.flags.is_local_mode() {
                    carry_file_mode(
                        &operation,
                        change.path(),
                        modifications.info.mode(node_to.mode),
                        node_to.mode,
                    )
                    .await?;
                }
                if modifications.info.mtime() < operation.clock_floor(&repository).await {
                    operation.record_modified_time(
                        &change.from.mapping.repository,
                        change.path(),
                        modifications.info.mtime(),
                    );
                }

                return Ok(false);
            }
        }
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
                change.path()
            );
            return Ok(false);
        }

        if is_delete {
            let current_node_link = current.node_at(change.path()).await;
            let (repository_current, state_current) = current_node_link
                .resolve(repository.clone(), current.state.clone())
                .await
                .forward_with::<SyncError, _>(|| {
                    format!("Failed to deserialize state {}", current_node_link.revision)
                })?;
            let subnode_current = current_node_link.node;
            let state_from = change.from.mapping.state.clone();
            let mut directory_changes = state::diff_filesystem_subtree(
                &operation,
                NodeMapping {
                    repository: change.from.mapping.repository.clone(),
                    state: state_from.clone(),
                    path: change.path().clone(),
                    node: change.from.mapping.node,
                },
                NodeMapping {
                    repository: repository_current,
                    state: state_current.clone(),
                    path: change.path().clone(),
                    node: subnode_current,
                },
                change.path().clone(),
                filter_mode,
                FilesystemDiffIntent::Report,
                Arc::new(Vec::new()),
            )
            .await
            .forward::<SyncError>(
                "Failed to calculate delta changes between file system and target state",
            )?;
            let mut has_modified_file = false;
            while let Some(subchange) = directory_changes.next().await {
                let incoming = (!has_modified_file).then_some(incoming);
                if !holds_local_work(&operation, &subchange, incoming).await? {
                    lore_trace!(
                        "Deleted directory entry {} holds no local work",
                        subchange.path()
                    );
                    continue;
                }

                let file_info = operation.file_info(subchange.path()).await.ok();
                if !has_modified_file {
                    lore_error!(
                        "Deleted directory has modified files in file system: {}",
                        change.path()
                    );
                }
                has_modified_file = true;

                let from_node = subchange.from.get_node().await;
                if let Some(file_info) = file_info {
                    if file_info.is_dir() {
                        lore_info!(
                            "  {} {}/",
                            subchange.action.as_string_short(),
                            subchange.path()
                        );
                    } else {
                        lore_info!(
                            "  {} {} : size {} mtime {}",
                            subchange.action.as_string_short(),
                            subchange.path(),
                            file_info.size(),
                            file_info.mtime()
                        );
                    }
                } else {
                    lore_info!("  Failed to get local file info");
                }

                if subchange.from.mapping.node.is_valid_node_id() {
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
            directory_changes.finish().await.forward::<SyncError>(
                "Failed to calculate delta changes between file system and target state",
            )?;
            if has_modified_file {
                return Err(LocalModifications.into());
            }
            return Ok(true);
        }
        // If this is a change from directory to file, there will have been a previous change
        // which deletes the existing directory node which will have verified the filesystem
        // state already - just allow the new file to be written
        lore_trace!(
            "Change {} from directory to file, previous change will have deleted it",
            change.path()
        );
        return Ok(true);
    }

    Ok(true)
}

pub async fn realize_changes(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
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
    let kept = realize_changes_delete(
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
            if let Some(parent) = change.path().parent() {
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
    let discover_changes = changes.clone();
    let producer = lore_spawn!(async move {
        let result = sync_discover_modify_add(discover_changes, discover_stats.clone(), tx).await;
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
    let consumer_operation = operation.clone();
    let consumer = lore_spawn!(async move {
        sync_execute_modify_add(
            rx,
            consumer_operation,
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
    consumer_result?;
    producer_result?;

    remove_directories_moved_out_of(&operation, &changes, kept).await;

    Ok(())
}

/// Removes the directories in `kept`, which their deletes found holding entries, out of which a
/// move in `changes` carries an entry, deepest first. A directory stays where it holds an entry
/// other than a case variant of a file moved out of it that holds what the move starts from or
/// carries.
///
/// Deletes run before moves, so a directory holding the source of a move still holds it when its
/// delete runs.
async fn remove_directories_moved_out_of(
    operation: &InstanceOperationImpl,
    changes: &[NodeChange],
    kept: Vec<NodeChange>,
) {
    if kept.is_empty() {
        return;
    }
    let mut moves: Vec<&NodeChange> = changes
        .iter()
        .filter(|change| change.move_source().is_some())
        .collect();
    moves.sort_unstable_by(|a, b| {
        a.from
            .mapping
            .path
            .as_str()
            .cmp(b.from.mapping.path.as_str())
    });
    for change in kept {
        let prefix = format!("{}/", change.path());
        let first =
            moves.partition_point(|moved| moved.from.mapping.path.as_str() < prefix.as_str());
        if moves
            .get(first)
            .is_some_and(|moved| moved.from.mapping.path.as_str().starts_with(&prefix))
            && (operation.remove(change.path()).await.is_ok()
                || Box::pin(remove_directory_of_case_variants(
                    operation,
                    change.path(),
                    &moves,
                ))
                .await)
        {
            event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(
                &change, 0, false,
            ))
            .send();
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn realize_conflicts(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
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
            if let Some(parent) = change_to.path().parent() {
                parent_paths.push(
                    RelativePath::new_from_initial_path(parent)
                        .expect("parent is valid from above"),
                );
            }
            // Also ensure parent directories for source change path exist
            // (needed for divergent move conflicts where paths differ)
            if change_from.path() != change_to.path()
                && let Some(parent) = change_from.path().parent()
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

/// Whether `path` names something in the tree `repository` tracks, which an operation has to
/// mediate because the provider behind it may be virtualizing the tree.
///
/// A repository with no working tree has nothing for a path to be inside.
#[lore_macro::test_pub]
fn is_inside_repository(repository: &RepositoryContext, path: &Path) -> bool {
    repository
        .require_path()
        .is_ok_and(|root| path.starts_with(root))
}

/// Writes `node`'s content to `path`, which names a file the repository does not track: the
/// three versions an automatic merge attempt writes to the temporary directory to run a
/// three-way merge over, and unlinks after.
///
/// Reaches the filesystem directly rather than through an operation, which is what the
/// callers already do to probe and unlink these paths. An operation names paths relative to
/// the tracked tree's root and may be virtualizing what is under it, so a path inside that
/// tree is refused here rather than written behind the provider's back. The copies a
/// conflicted merge leaves beside the file are in the tree and go through
/// [`realize_sidecar_file`].
pub async fn realize_scratch_file(
    repository: Arc<RepositoryContext>,
    path: impl AsRef<Path>,
    node: Node,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    let path = path.as_ref();
    if is_inside_repository(&repository, path) {
        return Err(SyncError::internal(format!(
            "Refusing to write scratch file {} inside the repository root",
            path.to_string_lossy()
        )));
    }
    if let Some(parent_path) = path.parent() {
        lore_io::IoDriver::global()
            .create_dir_all(parent_path)
            .await
            .map_err(|err| {
                SyncError::internal(format!("Failed to create scratch directory: {err}"))
            })?;
    }

    let written = if node.size > 0 {
        let options = immutable::read_options_from_repository(&repository);
        immutable::read_into_file(repository, node.address, path, None, options)
            .await
            .map_err(|err| {
                SyncError::internal(format!(
                    "Failed to sync file {}: {err}",
                    path.to_string_lossy()
                ))
            })?
            .1
    } else {
        Some(
            lore_io::IoDriver::global()
                .write_file_bytes(path, bytes::Bytes::new(), false)
                .await
                .map_err(|err| {
                    SyncError::internal(format!(
                        "Failed to sync file {}: {err}",
                        path.to_string_lossy()
                    ))
                })?,
        )
    };

    if node.mode & NodeFileMode::Executable == NodeFileMode::Executable {
        util::fs::metadata_set_executable(path, written.as_ref(), true).await;
    }

    lore_trace!(
        "Realized file {} {} bytes {}",
        path.display(),
        node.size,
        node.address.hash
    );

    stats.complete.file_update.fetch_add(1, Ordering::Relaxed);
    stats
        .complete
        .bytes_update
        .fetch_add(node.size, Ordering::Relaxed);

    Ok(())
}

/// Writes `node`'s content to the path `node` occupies, and records the modified time it
/// lands with.
///
/// Writing the file is what establishes that the path holds the node's content, so this is
/// where that is recorded rather than anywhere it is merely observed to be true.
///
/// Takes the path by value, so a task can be spawned on the future itself. Not an `async fn`,
/// which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn realize_file(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    path: RelativePath,
    node: Node,
    stats: Arc<SyncRealizeStats>,
) -> impl Future<Output = Result<(), SyncError>> {
    async move {
        let info = write_node_to_path(&repository, &operation, &path, &node, &stats).await?;
        operation.record_modified_time(&repository, &path, info.mtime());
        Ok(())
    }
}

/// Writes `node`'s content to a path beside the file it belongs to, such as the mine, theirs
/// and base copies a conflicted merge leaves behind.
///
/// Records no modified time: the cache states which node a path holds, and these paths hold
/// none.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn realize_sidecar_file(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    path: &RelativePath,
    node: Node,
    stats: Arc<SyncRealizeStats>,
) -> impl Future<Output = Result<(), SyncError>> + '_ {
    async move {
        write_node_to_path(&repository, &operation, path, &node, &stats).await?;
        Ok(())
    }
}

/// Writes `node`'s content to `path`, creating the parent directory where it is missing and
/// applying the node's mode, and reports what the file looks like once written.
async fn write_node_to_path(
    repository: &Arc<RepositoryContext>,
    operation: &Arc<InstanceOperationImpl>,
    path: &RelativePath,
    node: &Node,
    stats: &Arc<SyncRealizeStats>,
) -> Result<super::filesystem_provider::FileInfo, SyncError> {
    let info = operation
        .write_node(repository.clone(), node, path)
        .await
        .forward_with::<SyncError, _>(|| format!("Failed to sync file {path}"))?;

    lore_trace!(
        "Realized file {} {} bytes (target file {} bytes) {}",
        &path,
        node.size,
        info.size(),
        node.address.hash
    );

    stats.complete.file_update.fetch_add(1, Ordering::Relaxed);
    stats
        .complete
        .bytes_update
        .fetch_add(node.size, Ordering::Relaxed);

    Ok(info)
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

/// Realizes the deletes `changes` holds, deepest first, and answers the directory deletes that
/// found their directory holding entries and kept it.
async fn realize_changes_delete(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    changes: Arc<Vec<NodeChange>>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    stats: Arc<SyncRealizeStats>,
) -> Result<Vec<NodeChange>, SyncError> {
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
    let mut kept = Vec::new();
    for change in delete_changes.iter().rev() {
        let change = change.clone();

        let (state_from, stats) = (change.from.mapping.state.clone(), stats.clone());

        // TODO(mjansson): File system virtualization
        /*
        if (repository->virtualized) {
            err = repository->virtualization->delete_node(repository, change->path);
            return err;
        }
        */

        let change_path = change.path().clone();

        let is_link = change.from.is_link();

        let is_file = if is_link {
            false
        } else if change.from.mapping.node.is_valid_node_id() {
            let block = state_from
                .block(
                    change.from.mapping.repository.clone(),
                    NodeBlock::index(change.from.mapping.node),
                )
                .await
                .forward::<SyncError>("Failed deserializing state node block")?;
            let node = block.node(Node::index(change.from.mapping.node));

            node.is_file()
        } else {
            // This can happen if a local path needs to be deleted as a
            // result of a <state> vs <filesystem> diff.
            operation.file_info(&change_path).await?.is_file()
        };

        lore_trace!("D {}", change.path());
        let mut deleted = true;
        if !dry_run {
            let mut retry = util::fs::file_unlink_retry();
            loop {
                if is_link {
                    if let Err(err) = operation.remove_recursive(&change_path).await {
                        lore_debug!(
                            "Unable to unlink linked repository files at {}: {} (attempt {} of {}",
                            change_path,
                            err,
                            retry.counter() + 1,
                            retry.limit()
                        );
                        if !retry.wait().await {
                            return Err(SyncError::internal(format!(
                                "Failed to remove file or directory from local file system {change_path}",
                            )));
                        }
                    } else {
                        break;
                    }
                } else if let Err(err) = operation.remove(&change_path).await {
                    // Retry if it is a file, otherwise assume the directory has local files
                    if is_file {
                        lore_trace!(
                            "Unable to unlink local path {}: {} (attempt {} of {})",
                            change_path,
                            err,
                            retry.counter() + 1,
                            retry.limit()
                        );
                        if !retry.wait().await {
                            return Err(SyncError::internal(format!(
                                "Failed to remove file or directory from local file system {change_path}",
                            )));
                        }
                    } else {
                        deleted = Box::pin(remove_directory_of_case_variants(
                            &operation,
                            &change_path,
                            &delete_changes,
                        ))
                        .await;
                        break;
                    }
                } else {
                    // Success.
                    break;
                }
            }

            state::file_modified_time_clear(repository.clone(), change.path()).await;
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
                .find_node_link(
                    change.from.mapping.repository.clone(),
                    change.path().as_str(),
                )
                .await
            {
                Ok(node_link) => node_link,
                Err(e) if e.is_node_not_found() => NodeLink::invalid(),
                Err(err) => Err(err).forward::<SyncError>("Failed to stage change")?,
            };

            if node_link.is_valid() {
                stage::stage_delete(
                    change.from.mapping.repository.clone(),
                    state_stage.clone(),
                    change.path().clone(),
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

                if is_link {
                    remove_link_registry_entry(
                        &change.from.mapping.repository,
                        &state_stage,
                        change.from.address.context.into(),
                        node_link.node,
                    )
                    .await?;
                }
            }
        }

        if !deleted {
            kept.push(change);
        }
    }

    if !dry_run && !repository.is_layer() {
        remove_emptied_excluded_directories(
            &operation,
            &repository,
            &delete_changes,
            &RelativePath::new(),
            &stats,
        )
        .await;
    }

    Ok(kept)
}

/// Removes each directory the view excludes below `floor` and above a delete in `changes` that the
/// deletes left empty, deepest first, reporting each one removed. Each removal counts as a delete in
/// `stats`, done and in total, and a progress event follows the last.
///
/// A directory the view excludes stands on disk only for content below it. One still holding
/// anything, a local file included, is kept. `floor` is the root, or the mount of a layer, which
/// stands whatever the view and is realized under the layer's context, so the layer's sync removes
/// the directories below it rather than the realize. A realize under a link's context keeps every
/// directory. The directories of a link the realized tree mounts are treated as any other. Without
/// a rule re-including below a directory it excludes, a view leaves none of them on disk to remove.
/// Ancestors a delete shares with the one before it are gathered once, which sorted deletes make
/// the most of.
pub(crate) async fn remove_emptied_excluded_directories(
    operation: &InstanceOperationImpl,
    repository: &RepositoryContext,
    changes: &[NodeChange],
    floor: &RelativePath,
    stats: &Arc<SyncRealizeStats>,
) {
    if !repository.filter.view.reincludes() || repository.is_link() {
        return;
    }
    let mut directories = Vec::new();
    let mut previous = "";
    for change in changes
        .iter()
        .filter(|change| change.action == change::FileAction::Delete)
    {
        let mut directory = change.path().parent_path();
        while directory
            .below(floor)
            .is_some_and(|below| !below.is_empty())
            && !directory.covers(&previous)
        {
            let parent = directory.parent_path();
            directories.push(directory);
            directory = parent;
        }
        previous = change.path().as_str();
    }
    directories.sort_unstable_by(|left, right| right.as_str().cmp(left.as_str()));
    directories.dedup_by(|left, right| left.as_str() == right.as_str());
    let mut removed = false;
    for directory in directories {
        if repository
            .filter
            .excludes(&directory, true, FilterMode::View)
            && operation.remove_empty_directory(&directory).await.is_ok()
        {
            stats
                .complete
                .file_delete_total
                .fetch_add(1, Ordering::Relaxed);
            stats.complete.file_delete.fetch_add(1, Ordering::Relaxed);
            event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData {
                path: LoreString::from(&directory),
                size: 0,
                action: change::FileAction::Delete.into(),
                flag_file: 0,
            })
            .send();
            removed = true;
        }
    }
    if removed {
        event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(stats))
            .send();
    }
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

        let (repository, state_to) = (
            change.to.mapping.repository.clone(),
            change.to.mapping.state.clone(),
        );
        let block = state_to
            .block(repository.clone(), NodeBlock::index(change.to.mapping.node))
            .await
            .forward::<SyncError>("Failed deserializing state node block")?;
        let node = block.node(Node::index(change.to.mapping.node));

        if node.is_file() {
            stats.discovery.total_files.fetch_add(1, Ordering::Relaxed);
            stats
                .discovery
                .total_bytes
                .fetch_add(node.size, Ordering::Relaxed);
        }

        let Ok(permit) = tx.reserve().await else {
            // Receiver dropped, consumer encountered an error
            return Err(SyncError::internal("Recursion task failed"));
        };
        permit.send(SyncWorkItem {
            change: change.clone(),
            node,
        });
    }
    Ok(())
}

async fn sync_execute_modify_add(
    mut rx: mpsc::Receiver<SyncWorkItem>,
    operation: Arc<InstanceOperationImpl>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
) -> Result<(), SyncError> {
    const MAX_TASK_COUNT: usize = 10000;
    let mut tasks = JoinSet::new();
    // Finish independent changes even when one file cannot be written. The first
    // failure remains the terminal result; warnings identify every failed attempt.
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
        sync_error = sync_error.or(result
            .inspect_err(|err| {
                lore_warn!("Sync could not materialize content: {err}\n{}", err.trace());
            })
            .err());

        while let Some(result) = tasks.try_join_next() {
            sync_error = sync_error.or(result
                .internal("Recursion task failed")
                .map_err(SyncError::from)
                .flatten()
                .inspect_err(|err| {
                    lore_warn!("Sync could not materialize content: {err}\n{}", err.trace());
                })
                .err());
        }
        while tasks.len() > MAX_TASK_COUNT
            && let Some(result) = tasks.join_next().await
        {
            sync_error = sync_error.or(result
                .internal("Recursion task failed")
                .map_err(SyncError::from)
                .flatten()
                .inspect_err(|err| {
                    lore_warn!("Sync could not materialize content: {err}\n{}", err.trace());
                })
                .err());
        }
    }

    while let Some(result) = tasks.join_next().await {
        sync_error = sync_error.or(result
            .internal("Recursion task failed")
            .map_err(SyncError::from)
            .flatten()
            .inspect_err(|err| {
                lore_warn!("Sync could not materialize content: {err}\n{}", err.trace());
            })
            .err());
    }

    if let Some(err) = sync_error {
        Err(err)
    } else {
        Ok(())
    }
}

/// Record a link node the change just staged in the staged state's link registry.
///
/// A link the registry has never seen gets an entry, and one whose row names another pin is
/// brought to the node's, the row being what every later link operation reads the pin from.
pub(crate) async fn stage_link_registry_entry(
    repository: &Arc<RepositoryContext>,
    state_stage: &Arc<State>,
    change: &NodeChange,
    node: Node,
    staged_node: NodeID,
) -> Result<(), SyncError> {
    let link_id: RepositoryId = node.address.context.into();

    let replaced: RepositoryId = change.from.address.context.into();
    if change.from.is_link() && replaced != link_id {
        lore_debug!(
            "Link at {} moves from repository {replaced} to {link_id}",
            change.path()
        );
        state_stage
            .link_remove(repository.clone(), replaced, staged_node)
            .await
            .ok();
    }

    let existing = state_stage
        .link_find(repository.clone(), link_id, staged_node)
        .await
        .ok();
    if existing.is_some_and(|reference| reference.signature == node.address.hash) {
        lore_trace!(
            "Link {link_id} at {} is already in the registry",
            change.path()
        );
        return Ok(());
    }

    let incoming = change
        .to
        .mapping
        .state
        .link_find(
            change.to.mapping.repository.clone(),
            link_id,
            change.to.mapping.node,
        )
        .await;

    if let Some(existing) = existing {
        let branch = incoming
            .as_ref()
            .map_or(existing.branch, |reference| reference.branch);
        lore_debug!(
            "Moving link {link_id} at {} to revision {} branch {branch}",
            change.path(),
            node.address.hash
        );
        return state_stage
            .link_update(
                repository.clone(),
                link_id,
                branch,
                node.address.hash,
                staged_node,
            )
            .await
            .forward_with::<SyncError, _>(|| {
                format!(
                    "Failed to update link {link_id} at {} in the link registry",
                    change.path()
                )
            });
    }

    let (branch, flags) = match incoming {
        Ok(reference) => (
            reference.branch,
            LinkFlags::from_bits_retain(reference.flags),
        ),
        Err(err) => {
            lore_warn!(
                "Incoming state has no link registry entry for link {link_id} at {}: {err}. Tracking the parent branch",
                change.path()
            );
            (BranchId::default(), LinkFlags::NoFlags)
        }
    };

    lore_debug!(
        "Adding link {link_id} at {} to the link registry, node {staged_node} revision {} branch {branch}",
        change.path(),
        node.address.hash
    );

    state_stage
        .link_add(
            repository.clone(),
            link_id,
            branch,
            node.address.hash,
            staged_node,
            flags,
        )
        .await
        .forward_with::<SyncError, _>(|| {
            format!(
                "Failed to add link {link_id} at {} to the link registry",
                change.path()
            )
        })
}

/// Drop a deleted link node's [`LinkReference`](crate::state::LinkReference)
/// from the staged state's link registry.
pub(crate) async fn remove_link_registry_entry(
    repository: &Arc<RepositoryContext>,
    state_stage: &Arc<State>,
    link_id: RepositoryId,
    node_id: NodeID,
) -> Result<(), SyncError> {
    match state_stage
        .link_remove(repository.clone(), link_id, node_id)
        .await
    {
        Ok(()) => {
            lore_debug!("Removed link {link_id} node {node_id} from the link registry");
            Ok(())
        }
        Err(err) if err.is_link_not_found() => Ok(()),
        Err(err) => Err(err).forward::<SyncError>("Failed to remove link from the link registry"),
    }
}

/// Whether the rename a move was realized by left the destination holding the content it should, so
/// that writing it from the immutable store would replace it with the same bytes.
///
/// Two things have to hold. `renamed` reports a rename that completed, which is what says the source
/// was there to be carried: a sparse working tree holds nothing at a source its view excludes, and a
/// tree the move has already been applied to holds nothing at it either. And the two sides address
/// the same content, since a move that rewrites the file carries the old bytes to the destination and
/// the new ones are only in the store.
///
/// The rename answers the first for itself, so nothing here infers from a view whether the file was
/// on disk. A view could not answer it: the source is materialized under the view the tree is carried
/// *from* while realize holds the one it is carried *to*, and neither tells a source that was never
/// materialized from one an earlier pass already carried away.
fn rename_carried_the_content(change: &NodeChange, renamed: bool) -> bool {
    renamed && change.from.address.hash == change.to.address.hash
}

/// `node` with the executable bit the working file carries, where [`change::Flags::LocalMode`]
/// states that the bit is the working tree's own. Writing a node applies the mode it holds, which
/// would revert the one local change a chmod is.
///
/// The file stands at the path the change names, or at the source a move carries it from: a move
/// yet to be realized holds it at the source, and one realized over its own result at the
/// destination. Only a change holding a local bit pays for the read.
async fn node_with_a_local_mode(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    mut node: Node,
) -> Result<Node, SyncError> {
    if !change.flags.is_local_mode() {
        return Ok(node);
    }

    let mut info = operation.file_info(change.path()).await?;
    if !info.is_file()
        && let Some(source) = change.move_source()
    {
        info = operation.file_info(source).await?;
    }
    if info.is_file() {
        node.mode = info.mode(node.mode);
    }
    Ok(node)
}

/// Rewrites a mount whose link node moved to another subtree of the repository it points at.
///
/// The diff reports such a move as the link node alone, so the content the mount exposes has to be
/// carried here.
async fn realize_moved_mount(
    operation: &Arc<InstanceOperationImpl>,
    change: &NodeChange,
    node: Node,
) -> Result<(), SyncError> {
    if !change.from.is_link() {
        return Ok(());
    }

    let repository = change.to.mapping.repository.clone();
    // Another repository's subtree shares nothing with the one it replaces, so it is cloned afresh.
    if change.from.address.context != node.address.context {
        let path = change.path();
        operation
            .remove_recursive(path)
            .await
            .forward::<SyncError>("Failed removing the subtree the link exposed")?;
        operation
            .create_dir_all(path)
            .await
            .forward::<SyncError>("Failed recreating the link directory")?;
        return Box::pin(clone_added_link(&repository, operation, &node, path)).await;
    }

    let from_node = change
        .from
        .mapping
        .state
        .node(repository.clone(), change.from.mapping.node)
        .await
        .forward::<SyncError>("Failed reading the link node the mount stands at")?;
    if from_node.mounts_same_source(&node) && change.from.address == node.address {
        return Ok(());
    }

    let link_context = repository
        .to_link_context(node.address.context.into())
        .await;
    realize_link_pin_change_boxed(
        operation,
        repository,
        link_context,
        crate::link::LinkPinRealizeTarget {
            link_path: change.path().clone(),
            old_signature: change.from.address.hash,
            new_signature: node.address.hash,
            current_node: from_node.child,
            mount_source: crate::link::LinkMountSource::Node(node.child),
        },
    )
    .await
    .forward::<SyncError>("Failed realizing the subtree a link exposes")
}

/// [`realize_link_pin_change_in_operation`](crate::link::realize_link_pin_change_in_operation) for
/// a moved mount, the cold path. Boxed with an explicit `Send`, since that realize reaches
/// [`realize_moved_mount`] again.
fn realize_link_pin_change_boxed(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    link_context: Arc<RepositoryContext>,
    target: crate::link::LinkPinRealizeTarget,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), crate::link::LinkError>> + Send + '_>> {
    Box::pin(crate::link::realize_link_pin_change_in_operation(
        operation,
        repository,
        link_context,
        target,
    ))
}

/// Realizes on disk a node a change adds, modifies or moves, and stages it in `state_stage` when
/// given.
///
/// Cloning an added link and recording a staged link in the registry are boxed. Only a link
/// reaches them, and inline they would make the future of every change realized as large as
/// theirs.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn realize_change_modify_add(
    tasks: &mut JoinSet<Result<(), SyncError>>,
    operation: Arc<InstanceOperationImpl>,
    change: NodeChange,
    node: Node,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    is_merge: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
) -> impl Future<Output = Result<(), SyncError>> + '_ {
    async move {
        // During a merge this is the filter-free walk context, not the instance's.
        // The instance's view arrives separately as `view_filter`. The staging
        // below needs the first, the disk writes need the second.
        let repository = change.to.mapping.repository.clone();
        let size = node.size;
        let is_file = node.is_file();
        let path = change.path();

        // Only on-disk work honours the view. This gates directory creation, link
        // cloning and the file write. A directory the view excludes is created by
        // the first write below it. The move rename below is not gated, because
        // it repositions a path an earlier in-view realize may have written.
        let write_to_disk = !view_filter.excludes(path, node.is_directory(), FilterMode::View);

        lore_trace!(
            "{}{} {}",
            change.action.as_string_short(),
            if change.flags.is_conflict() { "!" } else { " " },
            path
        );

        event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(
            &change, size, is_file,
        ))
        .send();

        let to_path = path.clone();

        // Read ahead of the rename below, which carries a move's file away from the source it stands
        // at, and of the recovery from a rename that failed, which removes the destination.
        let realized = node_with_a_local_mode(&operation, &change, node).await?;

        let renamed = if !dry_run
            && change.action == change::FileAction::Move
            && let Some(from_path) = change.move_source()
        {
            let renamed = operation.rename(from_path, &to_path).await.is_ok();
            if !renamed {
                lore_trace!("Failed renaming move node, fall back to deleting and recreating");
                operation
                    .remove_recursive(&to_path)
                    .await
                    .forward::<SyncError>("Failed to realize move/rename")?;
            }
            renamed
        } else {
            false
        };

        if (node.is_directory() || node.is_link()) && write_to_disk {
            if !dry_run
                && operation.create_dir_all(&to_path).await.is_err()
                && operation
                    .file_info(&to_path)
                    .await
                    .is_ok_and(|info| !info.is_dir())
            {
                return Err(SyncError::internal(format!(
                    "Failed to create directory {path}"
                )));
            }

            // When a link is added, the linked contents are not marked
            // for add. That's why we can just clone the linked files
            if node.is_link() && change.action == change::FileAction::Add {
                Box::pin(clone_added_link(&repository, &operation, &node, path)).await?;
            } else if node.is_link() && !dry_run {
                realize_moved_mount(&operation, &change, node).await?;
            }
        } else if node.is_file() && !dry_run && write_to_disk {
            if rename_carried_the_content(&change, renamed) {
                if !change.flags.is_local_mode() {
                    carry_file_mode(&operation, path, change.from.mode, realized.mode).await?;
                }
            } else {
                lore_spawn!(
                    tasks,
                    realize_file(
                        repository.clone(),
                        operation.clone(),
                        path.clone(),
                        realized,
                        stats.clone(),
                    )
                );
            }
        }

        if let Some(state_stage) = state_stage.clone() {
            if change.action == change::FileAction::Move
                && let Some(from_path) = change.move_source()
            {
                let from_node_id = state_stage
                    .find_node_link(repository.clone(), from_path.as_str())
                    .await
                    .forward::<SyncError>("Failed to stage change")?
                    .node;
                relink_moved_node(
                    &state_stage,
                    &repository,
                    &change,
                    from_path,
                    from_node_id,
                    is_merge,
                )
                .await?;
            } else {
                let mut node = node;
                if is_merge {
                    node.flags |= NodeFlags::StagedMerge;
                    // A directory the incoming side adds is staged as an add, including one
                    // that already stands in the staged tree as the parent of an add below it.
                    if change.action == change::FileAction::Add && node.is_directory() {
                        node.flags |= NodeFlags::StagedAdd;
                    }
                }
                if change.action == change::FileAction::Move {
                    node.flags &= !NodeFlags::StagedAdd;
                    node.flags |= NodeFlags::StagedMove;
                }

                let staged_node = stage::stage_single_node(
                    repository.clone(),
                    state_stage.clone(),
                    change.path().clone(),
                    node,
                    Arc::new(stage::StageStats::default()),
                    None, // TODO(vri): UCS-18008 - Investigate link tracking for sync/realize_changes
                    FilterMode::View,
                )
                .await
                .forward::<SyncError>("Failed to stage change")?;

                if node.is_link() && staged_node.node.is_valid_node_id() {
                    Box::pin(stage_link_registry_entry(
                        &repository,
                        &state_stage,
                        &change,
                        node,
                        staged_node.node,
                    ))
                    .await?;
                }
            }
        }

        Ok(())
    }
}

/// Clones the content of the link `node` adds at `path` into the working tree.
async fn clone_added_link(
    repository: &Arc<RepositoryContext>,
    operation: &Arc<InstanceOperationImpl>,
    node: &Node,
    path: &RelativePath,
) -> Result<(), SyncError> {
    let link_id = node.address.context;
    let link_revision = node.address.hash;

    let link = repository.to_link_context(link_id.into()).await;
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
        .forward_with::<SyncError, _>(|| format!("Failed to deserialize state {link_revision}"))?;

    let clone_path = path.clone();

    let clone_ctx = CloneContext {
        repository: link.clone(),
        state: link_state,
        operation: operation.clone(),
        options: Arc::default(),
        stats: Arc::default(),
        modified_times: Arc::new(crate::state::RecordedModifiedTimes::default()),
    };
    let clone_states = link.filter.mount_states(&clone_path);
    clone::clone_node(
        clone_ctx,
        link_storage,
        clone_path,
        node.child,
        clone_states,
    )
    .await
    .forward::<SyncError>("Failed to sync link")
}

/// Stages a realized move by relinking `from_node_id`, the staged node at `from_path`, under
/// the path the change moves it to, keeping its identity, and marks it staged as a move.
/// Answers with the node's new parent.
///
/// A function of its own because its locals live across several awaits: kept in
/// [`realize_change_modify_add`] they would take space in the future of every change realized.
pub(crate) async fn relink_moved_node(
    state_stage: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    change: &NodeChange,
    from_path: &RelativePath,
    from_node_id: NodeID,
    is_merge: bool,
) -> Result<NodeID, SyncError> {
    // Relinking keeps the existing node, which preserves its identity and from_path tracking.
    let block_index = NodeBlock::index(from_node_id);
    let node_index = Node::index(from_node_id);
    let block = state_stage
        .block(repository.clone(), block_index)
        .await
        .forward::<SyncError>("Failed deserializing state node block")?;
    let mut from_node = block.node(node_index);

    // Determine the new parent node
    let mut parent_path = change.path().clone();
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

        if old_parent_node.child == from_node_id {
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
                let child = state_stage
                    .node(repository.clone(), child_id)
                    .await
                    .forward::<SyncError>("Failed deserializing state node block")?;
                child
                    .walk_step(child_id, old_parent_id, &mut cycle)
                    .forward::<SyncError>("Invalid node hierarchy in revision state")?;
                child.sibling()
            } {
                if sibling == from_node_id {
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
            parent_node.child = from_node_id;
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
    let to_name = change.path().name();
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
        .node_mark(repository.clone(), from_node_id, mark_flags, true)
        .await
        .forward::<SyncError>("Failed to stage change")?;

    Ok(new_parent_id)
}

#[allow(clippy::too_many_arguments)]
async fn realize_changes_merge(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
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
    let mut tasks = JoinSet::new();
    let mut failure = None;
    for index in 0..merges.len() {
        lore_spawn!(
            tasks,
            realize_file_merge(
                repository.clone(),
                operation.clone(),
                state_base.clone(),
                state_from.clone(),
                state_to.clone(),
                state_stage.clone(),
                merges.clone(),
                index,
                dry_run,
                view_filter.clone(),
                stats.clone(),
                merge_type,
            )
        );

        while tasks.len() > MAX_CONCURRENT_TREE_TASKS
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

/// Merges the file of the pair of changes at `index` in `merges`, which both sides changed.
///
/// Takes the shared list and an index rather than the pair, so that the task holds an `Arc` in place
/// of two changes. Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn realize_file_merge(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    state_base: Arc<State>,
    state_from: Arc<State>,
    _state_to: Arc<State>,
    state_stage: Option<Arc<State>>,
    merges: Arc<Vec<(NodeChange, NodeChange)>>,
    index: usize,
    dry_run: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
    merge_type: MergeType,
) -> impl Future<Output = Result<(), SyncError>> {
    async move {
        let (change_from, change_to) = &merges[index];
        // Handle conflicts
        lore_trace!("Try merge file {}", change_to.path());
        let mut resolved = false;
        let mut conflict = true;
        let mut size = 0;

        let in_view = !view_filter.excludes_tree(change_to.path(), false, FilterMode::View);

        // A view-excluded path has no working-tree file, so there is nothing to
        // compare and no merge result to edit. Adopt the incoming side, recorded
        // with the same flag `merge resolve theirs` sets.
        let take_theirs = !in_view;
        if take_theirs {
            conflict = false;
        }

        if change_from.path() == change_to.path() {
            if in_view {
                (size, resolved) = merge_file_in_view(
                    &repository,
                    &operation,
                    &state_base,
                    &state_from,
                    change_from,
                    change_to,
                    dry_run,
                )
                .await?;
                conflict = !resolved;
            }

            if let Some(state_stage) = &state_stage {
                let staged_size = stage_merged_file(
                    &repository,
                    &operation,
                    state_stage,
                    change_from,
                    change_to,
                    take_theirs,
                    conflict,
                    resolved,
                    merge_type,
                )
                .await?;
                if take_theirs {
                    size = staged_size;
                }
            }
        } else if change_from.path().overlaps(change_to.path()) {
            // A conflict on paths that are NOT equal.
            // For example:
            //   File 'some_path' vs file 'some_path/some_file'
            // Both file and dir cannot exist at the same time, so mark as
            // conflicted in the target state if given
            lore_info!(
                "Merge overlapping paths {} and {}",
                change_from.path(),
                change_to.path()
            );
            lore_trace!("Change from: {change_from:?}");
            lore_trace!("Change to: {change_to:?}");

            if let Some(state_stage) = &state_stage {
                lore_trace!("Staging conflict node in target state");
                let mut node = if change_to.to.mapping.node.is_valid_node_id() {
                    change_to
                        .to
                        .mapping
                        .state
                        .node(
                            change_to.to.mapping.repository.clone(),
                            change_to.to.mapping.node,
                        )
                        .await
                        .forward::<SyncError>("Failed to resolve node in merge revisions")?
                } else if change_from.to.mapping.node.is_valid_node_id() {
                    change_from
                        .to
                        .mapping
                        .state
                        .node(
                            change_from.to.mapping.repository.clone(),
                            change_from.to.mapping.node,
                        )
                        .await
                        .forward::<SyncError>("Failed to resolve node in merge revisions")?
                } else {
                    // Should not happen
                    lore_error!(
                        "Unexpected merge conflict of deleted file in both incoming revisions"
                    );
                    return Err(SyncError::internal("Invalid change data"));
                };

                node.flags |= NodeFlags::StagedMergeConflict;

                stage::stage_single_node(
                    repository.clone(),
                    state_stage.clone(),
                    change_to.path().clone(),
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
                change_from.path(),
                change_to.path()
            );

            if let Some(state_stage) = &state_stage {
                stage_divergent_move(
                    &repository,
                    &operation,
                    state_stage,
                    change_from,
                    dry_run,
                    merge_type,
                )
                .await?;
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
            change_to, size, true, /* is file */
        ))
        .send();

        Ok(())
    }
}

/// Writes the sidecars of a file both sides changed at the same in-view path, and merges them as
/// text into the file where it is diffable and theirs is a file. Removes the sidecars once merged
/// without conflict, and in a dry run. Returns the size of theirs, 0 where it is no file, and
/// whether the merge left no conflict.
///
/// A function of its own, so that the sidecar paths are not reserved in the states of the other
/// cases of [`realize_file_merge`].
async fn merge_file_in_view(
    repository: &Arc<RepositoryContext>,
    operation: &Arc<InstanceOperationImpl>,
    state_base: &State,
    state_from: &State,
    change_from: &NodeChange,
    change_to: &NodeChange,
    dry_run: bool,
) -> Result<(u64, bool), SyncError> {
    let mine_path = change_from.path().append_into_buf(MINE_SUFFIX).freeze();
    let theirs_path = change_from.path().append_into_buf(THEIRS_SUFFIX).freeze();
    let base_path = change_from.path().append_into_buf(BASE_SUFFIX).freeze();
    let mut size = 0;
    let mut resolved = false;

    let mut has_theirs = false;
    if change_from.to.mapping.node.is_valid_node_id() {
        lore_trace!(
            "Change from has valid to node, realize theirs file {}",
            &theirs_path
        );
        let node_to = state_from
            .block(
                repository.clone(),
                NodeBlock::index(change_from.to.mapping.node),
            )
            .await
            .forward::<SyncError>("Failed deserializing state node block")?
            .node(Node::index(change_from.to.mapping.node));

        // TODO(vri): Implement merging links/link nodes

        if node_to.is_directory() {
            lore_trace!("Change from is a directory, no theirs file");
        } else if node_to.is_file() {
            realize_sidecar_file(
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

    if change_to.to.mapping.node.is_valid_node_id() {
        // Diff3 function takes care of identifying identical files
        // and mergeable operations such as delete in both branches etc
        // Only thing remaining is identifying diffable files and split
        // the mergeable files from unresolvable conflicts
        let absolute_path = change_from
            .path()
            .to_absolute_path(repository.require_path()?);
        if has_theirs && operation.infer_is_diffable(change_to.path()).await? {
            lore_trace!(
                "Merge identified text file for merge: {}",
                absolute_path.display()
            );

            if change_from.from.mapping.node.is_valid_node_id() {
                lore_trace!(
                    "Change from has valid from node, realize base file {}",
                    &base_path
                );
                let node_from = state_base
                    .block(
                        repository.clone(),
                        NodeBlock::index(change_from.from.mapping.node),
                    )
                    .await
                    .forward::<SyncError>("Failed deserializing state node block")?
                    .node(Node::index(change_from.from.mapping.node));
                realize_sidecar_file(
                    repository.clone(),
                    operation.clone(),
                    &base_path,
                    node_from,
                    Arc::default(),
                )
                .await?;
            } else {
                lore_trace!("Change from has no valid from node, empty base file");
                let _ = operation.write_file(&base_path, Bytes::new()).await;
            }

            operation
                .copy_file(change_to.path(), &mine_path)
                .await
                .forward_with::<SyncError, _>(|| format!("Failed to sync file {mine_path}"))?;

            let mode = if dry_run {
                crate::merge::MergeTextMode::DryRun
            } else {
                let write_token = repository
                    .try_write_token()
                    .ok_or_else(|| SyncError::from(WriteRequired))?;
                crate::merge::MergeTextMode::Write(write_token)
            };
            resolved = match crate::merge::merge3_text_in_operation(
                operation,
                &base_path,
                &mine_path,
                &theirs_path,
                change_to.path(),
                mode,
            )
            .await
            {
                Err(err) => {
                    lore_debug!(
                        "Merge as text failed base {}, mine {}, theirs {} - fallback to binary file conflict to {}: {}",
                        &base_path,
                        change_to.path(),
                        &theirs_path,
                        absolute_path.display(),
                        err
                    );
                    false
                }
                Ok(true) => {
                    lore_debug!(
                        "Merged as text with conflict markers, base {}, mine {}, theirs {}: {}",
                        &base_path,
                        change_to.path(),
                        &theirs_path,
                        absolute_path.display()
                    );
                    false
                }
                Ok(false) => {
                    lore_trace!(
                        "Merged as text without any line conflicts: {}",
                        absolute_path.display()
                    );
                    true
                }
            };

            if resolved {
                let _ = operation.remove(&base_path).await;
                let _ = operation.remove(&theirs_path).await;
                let _ = operation.remove(&mine_path).await;
            }
        } else {
            lore_debug!(
                "Merge identified binary file for unresolved conflict: {}",
                absolute_path.display()
            );

            // Realize the base file for binary conflicts so users can compare
            if change_from.from.mapping.node.is_valid_node_id() {
                lore_trace!("Realize base file for binary conflict {}", &base_path);
                let node_from = state_base
                    .block(
                        repository.clone(),
                        NodeBlock::index(change_from.from.mapping.node),
                    )
                    .await
                    .forward::<SyncError>("Failed deserializing state node block")?
                    .node(Node::index(change_from.from.mapping.node));
                if node_from.is_file() {
                    realize_sidecar_file(
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
        let _ = operation.remove(&base_path).await;
        let _ = operation.remove(&theirs_path).await;
        let _ = operation.remove(&mine_path).await;
    }

    Ok((size, resolved))
}

/// Stages the node a file both sides changed at the same path resolves to, flagged with how its
/// merge left it, and records a conflict on `state_stage` where one is left. Removes the file where
/// the conflict deletes it. Returns the node's size.
///
/// A function of its own, so that the node is not reserved in the states of the other cases of
/// [`realize_file_merge`].
#[allow(clippy::too_many_arguments)]
async fn stage_merged_file(
    repository: &Arc<RepositoryContext>,
    operation: &InstanceOperationImpl,
    state_stage: &Arc<State>,
    change_from: &NodeChange,
    change_to: &NodeChange,
    take_theirs: bool,
    conflict: bool,
    resolved: bool,
    merge_type: MergeType,
) -> Result<u64, SyncError> {
    let mut node = if take_theirs && change_from.to.mapping.node.is_valid_node_id() {
        change_from
            .to
            .mapping
            .state
            .node(
                change_from.to.mapping.repository.clone(),
                change_from.to.mapping.node,
            )
            .await
            .forward::<SyncError>("Failed to resolve node in merge revisions")?
    } else if change_to.to.mapping.node.is_valid_node_id() {
        change_to
            .to
            .mapping
            .state
            .node(
                change_to.to.mapping.repository.clone(),
                change_to.to.mapping.node,
            )
            .await
            .forward::<SyncError>("Failed to resolve node in merge revisions")?
    } else if change_from.to.mapping.node.is_valid_node_id() {
        change_from
            .to
            .mapping
            .state
            .node(
                change_from.to.mapping.repository.clone(),
                change_from.to.mapping.node,
            )
            .await
            .forward::<SyncError>("Failed to resolve node in merge revisions")?
    } else {
        // Should not happen
        lore_error!("Unexpected merge conflict of deleted file in both incoming revisions");
        return Err(SyncError::internal("Invalid change data"));
    };
    let size = node.size;
    if take_theirs && !change_from.to.mapping.node.is_valid_node_id() {
        // The incoming side deleted the path, so adopting it deletes.
        node.flags |= NodeFlags::StagedDelete;
    } else if conflict && !change_to.to.mapping.node.is_valid_node_id() {
        node.flags |= NodeFlags::StagedDelete;

        operation.remove(change_to.path()).await?;
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
        change_to.to.mapping.repository.clone()
    } else {
        repository.clone()
    };
    stage::stage_single_node(
        stage_repository,
        state_stage.clone(),
        change_to.path().clone(),
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
    Ok(size)
}

/// Writes the file a divergent move's source side moved at the path it moved it to, outside a dry
/// run, and stages it there as a merge conflict.
///
/// A function of its own, so that the node is not reserved in the states of the other cases of
/// [`realize_file_merge`].
async fn stage_divergent_move(
    repository: &Arc<RepositoryContext>,
    operation: &Arc<InstanceOperationImpl>,
    state_stage: &Arc<State>,
    change_from: &NodeChange,
    dry_run: bool,
    merge_type: MergeType,
) -> Result<(), SyncError> {
    let mut node = if change_from.to.mapping.node.is_valid_node_id() {
        change_from
            .to
            .mapping
            .state
            .node(
                change_from.to.mapping.repository.clone(),
                change_from.to.mapping.node,
            )
            .await
            .forward::<SyncError>("Failed to resolve node in merge revisions")?
    } else {
        lore_debug!("Divergent move conflict with no valid source node");
        return Err(SyncError::internal("Invalid change data"));
    };

    if !dry_run && node.is_file() {
        realize_file(
            repository.clone(),
            operation.clone(),
            change_from.path().clone(),
            node,
            Arc::default(),
        )
        .await?;
    }

    node.flags = NodeFlags::from_bits_truncate(node.flags)
        .bitand(NodeFlags::File | NodeFlags::Link)
        .bits();
    node.flags |= NodeFlags::StagedMergeConflict;

    stage::stage_single_node(
        repository.clone(),
        state_stage.clone(),
        change_from.path().clone(),
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
    Ok(())
}

impl LoreRevisionSyncFileEventData {
    pub(crate) fn new(node_change: &NodeChange, size: u64, file: bool) -> Self {
        Self {
            path: LoreString::from(node_change.path()),
            size,
            action: node_change.action.into(),
            flag_file: file.into(),
        }
    }
}
