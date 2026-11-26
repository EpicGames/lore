// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Borrow;
use std::collections::HashSet;
use std::ops::BitAnd;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use zerocopy::FromZeros;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::branch::merge::MergeType;
use crate::change;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::dependency;
use crate::errors::LocalModifications;
use crate::errors::WriteRequired;
use crate::event;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::filter::FilterPath;
use crate::filter::FilterStates;
use crate::filter::Scope;
use crate::fs::filesystem_provider::FilesystemDiffIntent;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::HoldOptions;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::ListedEntry;
use crate::fs::filesystem_provider::MeasuredNode;
use crate::hash;
use crate::interface::LoreNodeType;
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
use crate::node::ROOT_NODE;
use crate::progress::DEFAULT_WORK_CHANNEL_CAPACITY;
use crate::progress::ProgressTicker;
use crate::repository::BASE_SUFFIX;
use crate::repository::MERGE_ARTIFACT_SUFFIXES;
use crate::repository::MINE_SUFFIX;
use crate::repository::RepositoryContext;
use crate::repository::THEIRS_SUFFIX;
use crate::repository::clone;
use crate::repository::clone::CloneContext;
use crate::revision::sync::KeptRename;
use crate::revision::sync::KeptRenames;
use crate::revision::sync::LoreRevisionSyncFileEventData;
use crate::revision::sync::SyncError;
use crate::revision::sync::SyncOptions;
use crate::revision::sync::SyncRealizeStats;
use crate::revision::sync::SyncVerifyArgs;
use crate::revision::sync::SyncVerifyStats;
use crate::stage;
use crate::state;
use crate::state::HeldNode;
use crate::state::Mark;
use crate::state::NameChange;
use crate::state::NodeComparison;
use crate::state::NodeMapping;
use crate::state::State;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::state::os_diff::ListedSpellings;
use crate::state::os_diff::types_agree;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::expand_path_ancestors;
use crate::util::path::sorted_holds_below;

/// Carries the working tree from `state_current` to `state_target`. A provider that holds its
/// state in a `State` is handed `state_target` alone and carries itself there
/// ([`InstanceOperationImpl::hold_state`]), then reads the held ignore file into the store
/// (`InstanceOperationImpl::warm_held_ignore_file`).
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
                reset_local: options.reset,
                dry_run: globals.dry_run(),
            }
        };
        operation
            .hold_state(repository_target, state_target, hold)
            .await
            .forward::<SyncError>("Holding the target revision")?;
        operation.warm_held_ignore_file().await;
        return Ok(());
    }

    let inclusion_set = if options.root_files.is_empty() {
        None
    } else {
        let tags: Vec<&str> = options.dependency_tags.iter().map(|s| s.as_str()).collect();
        Some(
            dependency::resolve::resolve_dependency_file_set(
                repository_target.clone(),
                state_target.clone(),
                &options.root_files,
                &tags,
                options.dependency_recursive,
                options.dependency_depth_limit,
            )
            .await
            .forward::<SyncError>("Failed to resolve dependency set")?,
        )
    };

    let stats: Arc<SyncRealizeStats> = Arc::default();
    let mut subtrees = None;
    let changes = if !options.reset {
        lore_info!(
            "Calculating deltas {} -> {}",
            state_current.revision_number(),
            state_target.revision_number()
        );
        let (changes, expansion) = diff_for_sync(
            repository_current.clone(),
            state_current.clone(),
            repository_target.clone(),
            state_target.clone(),
            &options,
        )
        .await?;
        subtrees = expansion;
        changes
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

    let changes = if let Some(inclusion_set) = inclusion_set {
        let change_count = changes.len();
        let filtered = changes_in_dependency_set(changes, |change: &NodeChange| {
            let (mapping, state) = match change.action {
                change::FileAction::Delete => (&change.from.mapping, &state_current),
                _ => (&change.to.mapping, &state_target),
            };
            // Node ids are numbered per tree, so a change inside a link can carry the id of a
            // node in the dependency set.
            inclusion_set.contains(&mapping.node) && Arc::ptr_eq(&mapping.state, state)
        });
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

    if lore_base::allocator::memory_stats_requested() {
        lore_debug!(
            "After the delta, {} changes: {}",
            changes.len(),
            lore_base::allocator::memory_summary()
        );
    }

    let options = Arc::new(options);
    let changes = Arc::new(changes);
    let VerifiedChanges { changes, kept } = if !changes.is_empty() && !force && !options.reset {
        let whole = changes
            .iter()
            .filter(|change| change.flags.is_subtree())
            .count();
        if whole == 0 {
            lore_info!("Verifying {} changes with local file system", changes.len());
        } else {
            lore_info!(
                "Verifying {} changes with local file system, {whole} of them directories deleted as a whole",
                changes.len()
            );
        }
        // The list is moved in, so verification can filter it in place and the realize step never
        // holds a second list of the same millions of changes.
        verify_filesystem_for_changes(Arc::new(SyncVerifyArgs {
            changes,
            repository_current: repository_current.clone(),
            operation: operation.clone(),
            current: NodeMapping::root(repository_current.clone(), state_current.clone()),
            options: options.clone(),
            carries_modes: true,
        }))
        .await?
    } else {
        VerifiedChanges {
            changes,
            kept: Vec::new(),
        }
    };

    if lore_base::allocator::memory_stats_requested() {
        lore_debug!(
            "After verification, {} changes to realize: {}",
            changes.len(),
            lore_base::allocator::memory_summary()
        );
    }

    let emptied = if !options.root_files.is_empty() {
        EmptiedDirectories::Excluded
    } else if options.view.is_some() || options.reset {
        EmptiedDirectories::Target
    } else {
        EmptiedDirectories::Unplaced
    };
    realize_changes(
        repository_target,
        operation,
        changes,
        &kept,
        subtrees,
        None,
        dry_run,
        None, /* Not a merge */
        Some(&emptied),
        stats,
    )
    .await?;

    if lore_base::allocator::memory_stats_requested() {
        lore_debug!(
            "After realizing: {}",
            lore_base::allocator::memory_summary()
        );
    }

    Ok(())
}

/// The changes a sync from `state_current` to `state_target` realizes, and the
/// [`Subtrees`](state::Subtrees) that expand those standing for a directory deleted as a whole.
///
/// Such a directory is one change, expanded as verification and realization get to it, so a sync
/// deleting millions of files doesn't hold a change for each. Where the changes are taken one by
/// one, by a dependency set, by keeping local work file by file or by hashing every file, each is
/// listed. Under a target view, but for a dependency set, a delete in a directory the view
/// excludes that holds something it places is marked
/// ([`DiffFlags::PlacesDirectories`](state::DiffFlags::PlacesDirectories)).
///
/// Not an `async fn`, which would hold a second copy of `repository_target` across the walk.
#[allow(clippy::manual_async_fn)]
fn diff_for_sync(
    repository_current: Arc<RepositoryContext>,
    state_current: Arc<State>,
    repository_target: Arc<RepositoryContext>,
    state_target: Arc<State>,
    options: &SyncOptions,
) -> impl Future<Output = Result<(Vec<NodeChange>, Option<Arc<state::Subtrees>>), SyncError>> + '_ {
    let flags = if repository_target.filter.view.is_empty() || !options.root_files.is_empty() {
        state::DiffFlags::empty()
    } else {
        state::DiffFlags::PlacesDirectories
    };
    async move {
        if options.root_files.is_empty() && !options.forward_changes && !options.force_hash_check {
            let (changes, subtrees) = state::diff_collect_subtrees(
                repository_current,
                state_current,
                repository_target,
                state_target,
                flags,
                options.filter_mode,
            )
            .await
            .forward::<SyncError>("Failed to calculate delta changes between states")?;
            return Ok((changes, Some(subtrees)));
        }
        let changes = state::diff_collect_flagged(
            repository_current,
            state_current,
            repository_target,
            state_target,
            flags,
            options.filter_mode,
        )
        .await
        .forward::<SyncError>("Failed to calculate delta changes between states")?;
        Ok((changes, None))
    }
}

/// The changes a verify leaves to realize, and the files a link delete among them keeps.
pub struct VerifiedChanges {
    /// The changes still having work to do, sorted by path.
    pub changes: Arc<Vec<NodeChange>>,
    /// The paths of the changes below the mount of a deleted link that the verify dropped, where
    /// the working tree holds what the change set leaves or local work the sync forwards, which
    /// removing the mount keeps.
    pub kept: Vec<RelativePath>,
}

/// Verifies the changes `args` holds against the working tree, each as [`verify_filesystem`]
/// does, and answers those still having work to do.
///
/// Where [`SyncVerifyArgs::carries_modes`] is set, gives each file found holding the incoming
/// content with the other executable bit the change's bit, once no change has refused; otherwise
/// sets none, and the realize that follows verifies again and sets them.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn verify_filesystem_for_changes(
    args: Arc<SyncVerifyArgs>,
) -> impl Future<Output = Result<VerifiedChanges, SyncError>> {
    async move {
        // Each task answers with its change's verdict and the flags the verify left on it:
        // verification only ever adds flags. The list to realize is the original filtered in
        // place, so a large sync holds the changes once.
        let mut failure = None;
        let mut tasks = JoinSet::new();
        let mut verdicts = vec![(Verdict::Drop, change::Flags::None); args.changes.len()];
        let stats = Arc::new(SyncVerifyStats::default());
        let refuse = stats.clone();
        let incoming = Arc::new(IncomingChanges::new(args.changes.clone()));
        let record = |verdicts: &mut Vec<(Verdict, change::Flags)>,
                      failure: &mut Option<SyncError>,
                      result: Result<
            Result<(usize, Verdict, change::Flags), SyncError>,
            tokio::task::JoinError,
        >| {
            match result
                .internal("Recursion task failed")
                .map_err(SyncError::from)
                .flatten()
            {
                Ok((index, verdict, flags)) => verdicts[index] = (verdict, flags),
                Err(err) => record_verify_failure(&refuse, failure, err),
            }
        };
        for (index, change) in args.changes.iter().enumerate() {
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
                    let verdict = verify_filesystem(
                        &mut change,
                        repository_current,
                        operation,
                        current,
                        forward_changes,
                        false,
                        force_hash_check,
                        stats,
                        filter_mode,
                        &incoming,
                    )
                    .await?;

                    Ok((index, verdict, change.flags))
                }
            });
            while tasks.len() > MAX_CONCURRENT_TREE_TASKS
                && let Some(result) = tasks.join_next().await
            {
                record(&mut verdicts, &mut failure, result);
            }

            if failure.is_some() {
                break;
            }
        }
        // Wait for the remaining tasks
        while let Some(result) = tasks.join_next().await {
            record(&mut verdicts, &mut failure, result);
        }

        if let Some(err) = failure {
            return Err(err);
        }

        let mut kept: Vec<RelativePath> = args
            .changes
            .iter()
            .zip(&verdicts)
            .filter(|(change, (verdict, _))| {
                *verdict != Verdict::Realize
                    && change.action != change::FileAction::Delete
                    && incoming.link_delete_above(change.path()).is_some()
            })
            .map(|(change, _)| change.path().clone())
            .collect();
        // Before any executable bit is carried, so a refusal leaves the working tree untouched.
        let forwarded = verify_mounts(&args, &incoming, &mut verdicts, &kept).await?;
        kept.extend(forwarded);

        if args.carries_modes
            && verdicts
                .iter()
                .any(|(verdict, _)| *verdict == Verdict::CarryMode)
        {
            let carried: crate::BoxFuture<'_, Result<(), SyncError>> =
                Box::pin(carry_held_modes(&args.operation, &args.changes, &verdicts));
            carried.await?;
        }

        // `incoming` holds a reference to the list; with it dropped, the list is taken back without
        // a copy where this holds the only reference to it, which a caller that moves its list in
        // gives it.
        drop(incoming);
        let original = match Arc::try_unwrap(args) {
            Ok(args) => Arc::try_unwrap(args.changes).unwrap_or_else(|shared| (*shared).clone()),
            Err(args) => (*args.changes).clone(),
        };
        // Collecting a filtered `into_iter` of the same type reuses its allocation.
        let mut changes: Vec<NodeChange> = original
            .into_iter()
            .zip(verdicts)
            .filter_map(|(mut change, (verdict, flags))| {
                (verdict == Verdict::Realize).then(|| {
                    change.flags = flags;
                    change
                })
            })
            .collect();

        // Parent directories must appear before their children so that directory renames
        // in the realize path complete before child file operations are spawned.
        change::sort_by_path(&mut changes);

        Ok(VerifiedChanges {
            changes: Arc::new(changes),
            kept,
        })
    }
}

/// The executable bits [`carry_held_modes`] sets at once: more than the IO pool has threads, so
/// that none of its threads sits idle.
const CONCURRENT_CARRIES: usize = 64;

/// Gives the file of each change of `changes` whose verdict is [`Verdict::CarryMode`] the change's
/// executable bit, which the verify found the file holding the incoming content without. Up to
/// [`CONCURRENT_CARRIES`] futures run together, each taking every so many changes in turn, so the
/// bits are set in parallel with an allocation per future rather than per file. Every future runs
/// to its end, or to its own first error, before the first error is answered, so no carry outlives
/// the call.
///
/// Boxed by its caller: only a verify that found such a file reaches it, and inline it would grow
/// every verify's future.
async fn carry_held_modes(
    operation: &InstanceOperationImpl,
    changes: &[NodeChange],
    verdicts: &[(Verdict, change::Flags)],
) -> Result<(), SyncError> {
    let carried = verdicts
        .iter()
        .filter(|(verdict, _)| *verdict == Verdict::CarryMode);
    let parallel = carried.count().min(CONCURRENT_CARRIES);
    let mut carrying: FuturesUnordered<_> = (0..parallel)
        .map(|first| async move {
            for index in (first..changes.len()).step_by(parallel) {
                if verdicts[index].0 == Verdict::CarryMode {
                    let change = &changes[index];
                    let held = change.to.mode ^ NodeFileMode::Executable.bits();
                    carry_file_mode(operation, change.path(), held, change.to.mode).await?;
                }
            }
            Ok::<_, SyncError>(())
        })
        .collect();
    let mut failure = None;
    loop {
        let Some(carried) = carrying.next().await else {
            return failure.map_or(Ok(()), Err);
        };
        if let Err(err) = carried {
            failure = failure.or(Some(err));
        }
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
        .map_or_default(|node| node.address)
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
            0,
            change.path(),
            operation,
            established,
        )
        .await
        .forward::<SyncError>("Failed to compare the file to the node the change starts from")?,
        NodeComparison::Matches
    ))
}

/// The changes of `changes` a dependency set holds, as `included` answers, in their order.
///
/// Where a kept change lies at or below the mount of a link the changes delete, the link delete
/// and every change at or below its mount are kept too: removing the mount removes everything
/// below it, so what replaces it lands whole. The link delete's node is one of the tree the sync
/// leaves, which a dependency set of the tree it reaches does not hold.
#[lore_macro::test_pub]
fn changes_in_dependency_set(
    changes: Vec<NodeChange>,
    included: impl Fn(&NodeChange) -> bool,
) -> Vec<NodeChange> {
    let changes = Arc::new(changes);
    let keep: Vec<bool> = {
        let incoming = IncomingChanges::new(changes.clone());
        let replaced_mounts: HashSet<usize> = changes
            .iter()
            .filter(|change| change.action != change::FileAction::Delete && included(change))
            .filter_map(|change| incoming.link_delete_covering(change.path()))
            .collect();
        changes
            .iter()
            .map(|change| {
                included(change)
                    || incoming
                        .link_delete_covering(change.path())
                        .is_some_and(|index| replaced_mounts.contains(&index))
            })
            .collect()
    };
    let changes = Arc::try_unwrap(changes).unwrap_or_else(|shared| (*shared).clone());
    changes
        .into_iter()
        .zip(keep)
        .filter_map(|(change, keep)| keep.then_some(change))
        .collect()
}

/// How the working file at `repository_path`, the path a change names or the one a move carries
/// its file from, compares to the node it was realized from, and whether the current revision is
/// what holds that node.
///
/// A three-way merge's from side is the base revision, which answers for nothing on disk, so
/// the node the current revision holds at the path stands in. Where the change starts at a node
/// of the current revision the two are the same node, reached by id rather than by path. An add
/// that starts at the current revision is still measured by path below a link `incoming`
/// deletes, where the current view holds it: the current revision holds the path through that
/// link's mount, which `incoming` opens once for every path below it. Where the current revision
/// holds none the from side is all there is. The node is measured in the context that holds it,
/// whose partition its content lives in.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn modification_against_measured_node<'a>(
    operation: &'a Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    change: &'a NodeChange,
    current: &'a NodeMapping,
    force_full_check: bool,
    repository_path: &'a RelativePath,
    established: &'a lore_storage::ContentHashes,
    incoming: &'a IncomingChanges,
) -> impl Future<Output = Result<crate::fs::filesystem_provider::FileModifiedCheck, SyncError>> + 'a
{
    async move {
        let info = operation.file_info(repository_path).await?;
        if !info.exists() {
            return Ok(crate::fs::filesystem_provider::FileModifiedCheck {
                info,
                measured: None,
                modification: None,
            });
        }

        let from_is_current = change.from.mapping.state.revision() == current.state.revision();
        let held = if !from_is_current {
            current
                .held_at(repository_path)
                .await
                .map(HeldNode::into_parts)
        } else if change.from.mapping.node.is_valid_node_id()
            || !info.is_file()
            || incoming.link_delete_above(repository_path).is_none()
            || repository
                .filter
                .excludes_tree(repository_path, false, FilterMode::View)
        {
            None
        } else {
            incoming.held_below_deleted_link(repository_path).await
        };

        let (node, repository_measured, is_current) = if let Some((held_node, held)) = held {
            let (repository_current, state_node) = held;
            let node = state_node
                .node(repository_current.clone(), held_node)
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
                        repository_path,
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
                        repository_path,
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
}

/// Whether the file at `source`, the path `change` relocates its file from, holds local work:
/// content other than what it was realized from and what the change brings in.
///
/// Where the current revision holds the file at `source`, the realize renames it away and writes
/// the incoming content over it where the change rewrites it, so an edit there would be lost or
/// left at a path that never held it. Nothing there, or no file, holds none, and nor does a file
/// the current revision holds no node for at `source`, as at the source of the current branch's
/// own move in a three-way merge: that file is not the one the change carries.
///
/// Runs only for a change that relocates a file, a cold path its caller boxes: inline, the two
/// comparisons would make the verify of every change as large as their own.
async fn moved_file_holds_local_work(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    change: &NodeChange,
    current: &NodeMapping,
    force_full_check: bool,
    source: &RelativePath,
    incoming: &IncomingChanges,
) -> Result<bool, SyncError> {
    let established = lore_storage::ContentHashes::default();
    let measured = modification_against_measured_node(
        operation,
        repository,
        change,
        current,
        force_full_check,
        source,
        &established,
        incoming,
    )
    .await?;
    let carried = measured
        .measured
        .as_ref()
        .is_some_and(|measured| measured.is_current);
    if !carried
        || !measured
            .modification
            .is_some_and(|difference| difference.modified)
    {
        return Ok(false);
    }

    let incoming = change
        .to
        .get_node()
        .await
        .forward::<SyncError>("Failed loading the node the change brings in")?;
    let comparison = state::file_matches_node(
        change.to.mapping.repository.clone(),
        &incoming,
        measured.info.size(),
        measured.info.mtime(),
        source,
        operation,
        &established,
    )
    .await
    .forward::<SyncError>("Failed to compare the file to the incoming node")?;
    match comparison {
        NodeComparison::Matches => Ok(false),
        NodeComparison::Differs => Ok(true),
        NodeComparison::Unreadable => Err(SyncError::internal(format!(
            "Failed to read {source} to compare it against the incoming revision"
        ))),
    }
}

/// The tree a link's mount shows, with the context and state holding it.
pub(crate) type MountedTree = (NodeID, crate::link::LinkedState);

/// The tree the link node `node` mounts, opened in the revision it pins from `repository`, the
/// context holding the node, or `None` where that revision cannot be opened.
pub(crate) async fn open_mounted_tree(
    repository: &Arc<RepositoryContext>,
    node: &Node,
) -> Option<MountedTree> {
    let link = node.linked_node();
    match link.open(repository).await {
        Ok(linked) => Some((link.node, linked)),
        Err(err) => {
            lore_debug!(
                "Unable to open revision {} a link pins: {err}",
                link.revision
            );
            None
        }
    }
}

/// Whether `change` takes away the content of a link's mount: it deletes the link, or moves it to
/// another repository, whose content shares nothing with what the mount shows.
pub(crate) fn replaces_mount(change: &NodeChange) -> bool {
    change.from.is_link()
        && (change.action == change::FileAction::Delete
            || (change.to.is_link() && change.from.address.context != change.to.address.context))
}

/// The tree the working tree shows at the mount of the link `change` deletes or moves: that of the
/// link the change starts from, or for a merge, whose changes start from its base, that of the
/// link `merge_current` holds at the mount. Where `merge_staged`, the staged state of the merge
/// the merge resumes, holds a link there, its pin stands instead: it names the link's staged
/// state, which holds what an earlier pass merged and resolved in the mount. `None` where the tree
/// the working tree stands on holds no link there, `Some(None)` where the link's revision cannot
/// be opened.
async fn mount_shown(
    change: &NodeChange,
    merge_current: Option<&NodeMapping>,
    merge_staged: Option<&NodeMapping>,
) -> Result<Option<Option<MountedTree>>, SyncError> {
    let (repository, node) = match merge_current {
        None => (
            change.from.mapping.repository.clone(),
            change
                .from
                .get_node()
                .await
                .forward::<SyncError>("Failed loading the link node")?,
        ),
        Some(current) => {
            let staged = match merge_staged {
                Some(staged) => link_held_at(staged, change.path()).await?,
                None => None,
            };
            match staged {
                Some(held) => held,
                None => match link_held_at(current, change.path()).await? {
                    Some(held) => held,
                    None => return Ok(None),
                },
            }
        }
    };
    if !node.is_link() {
        return Ok(None);
    }
    Ok(Some(open_mounted_tree(&repository, &node).await))
}

/// The link node `mapping` holds at `path`, with the repository holding it, or `None` where it
/// holds no link there.
async fn link_held_at(
    mapping: &NodeMapping,
    path: &RelativePath,
) -> Result<Option<(Arc<RepositoryContext>, Node)>, SyncError> {
    let Some(held) = mapping.held_at(path).await else {
        return Ok(None);
    };
    let node = held
        .read()
        .await
        .forward::<SyncError>("Failed loading the link node the working tree stands on")?;
    Ok(node.is_link().then(|| (held.repository().clone(), node)))
}

/// What a sync does with the mount of a link it deletes or moves, once [`verify_mount`] has found
/// it may.
enum MountVerdict {
    /// Remove the mount but what the link does not track.
    Remove,
    /// Remove it, keeping too the files the link tracks that hold local changes, which the sync
    /// forwards.
    KeepModified(Vec<RelativePath>),
    /// Leave the mount as it is: the sync forwards local changes past a link moved to another
    /// repository, whose content would land over them.
    KeepMount,
}

/// Checks the mount of the link `change` deletes or moves, which `shown` maps, for what removing
/// it would lose ([`state::link_mount_work`]), sparing the files `held` names, lowercase and
/// sorted, which the removal keeps.
///
/// A file the link tracks that holds local changes refuses the sync, or is kept where the sync
/// forwards changes. An entry that cannot be read refuses it either way, since what it holds is
/// unknown, and so does an entry the removal keeps where `incoming` writes.
#[allow(clippy::too_many_arguments)]
async fn verify_mount(
    mount: &RelativePath,
    deletes: bool,
    shown: Option<MountedTree>,
    operation: &Arc<InstanceOperationImpl>,
    filter: &Filter,
    root: Option<&Path>,
    held: &[&str],
    incoming: &IncomingWrites<'_>,
    forward_changes: bool,
) -> Result<MountVerdict, SyncError> {
    let work = state::link_mount_work(operation, shown.clone(), mount, forward_changes, |path| {
        held.binary_search(&path.as_lowercase_str()).is_ok()
    })
    .await
    .forward::<SyncError>("Failed comparing the link mount with the file system")?;
    if let Some(path) = &work.unknown {
        lore_error!("Link mount {mount} holds what cannot be read: {path}");
        lore_error!("Make it readable, or use --force to discard it");
        return Err(LocalModifications.into());
    }
    let verdict = if work.modified_count == 0 {
        MountVerdict::Remove
    } else if !forward_changes {
        lore_error!("Link mount {mount} has local changes: {}", work.named());
        // A switch or merge reaches this as a sync does, with no --forward-changes to offer.
        lore_error!("Commit or reset them, or use --force to discard them");
        return Err(LocalModifications.into());
    } else if deletes {
        lore_info!(
            "Keeping local changes in link mount {mount}: {}",
            work.named()
        );
        MountVerdict::KeepModified(work.modified)
    } else {
        lore_info!(
            "Keeping link mount {mount} holding local changes: {}",
            work.named()
        );
        return Ok(MountVerdict::KeepMount);
    };
    if let Some(shown) = shown {
        refuse_kept_entries_in_the_way(operation, filter, root, mount, shown, held, incoming)
            .await?;
    }
    Ok(verdict)
}

/// What the incoming revision writes at a mount the change set takes away, for
/// [`verify_mount`]: the other changes of a change set, or the tree a link moved to another
/// repository mounts there.
async fn incoming_writes<'a>(
    change: &NodeChange,
    incoming: &'a IncomingChanges,
) -> Result<IncomingWrites<'a>, SyncError> {
    if change.action == change::FileAction::Delete {
        return Ok(IncomingWrites::Changes(incoming));
    }
    let node = change
        .to
        .get_node()
        .await
        .forward::<SyncError>("Failed loading the incoming link node")?;
    let Some((node, (repository, state))) =
        open_mounted_tree(&change.to.mapping.repository, &node).await
    else {
        return Err(SyncError::internal(format!(
            "Failed to open the revision the link at {} moves to",
            change.path()
        )));
    };
    Ok(IncomingWrites::Tree(NodeMapping {
        repository,
        state,
        path: change.path().clone(),
        node,
    }))
}

/// Checks the mount of each link `args` deletes or moves whose verdict is to realize it, as
/// [`verify_mount`] does, sparing the files `kept` names, and answers the files to keep beyond
/// them. A move past whose local changes the sync forwards is dropped from `verdicts`.
///
/// Runs once every change has been verified, since the files kept are known only then.
async fn verify_mounts(
    args: &SyncVerifyArgs,
    incoming: &IncomingChanges,
    verdicts: &mut [(Verdict, change::Flags)],
    kept: &[RelativePath],
) -> Result<Vec<RelativePath>, SyncError> {
    let mut forwarded = Vec::new();
    let mut held: Option<Vec<&str>> = None;
    for (index, change) in args.changes.iter().enumerate() {
        if verdicts[index].0 != Verdict::Realize || !replaces_mount(change) {
            continue;
        }
        let Some(shown) = mount_shown(change, None, None).await? else {
            continue;
        };
        let held = held.get_or_insert_with(|| folded_and_sorted(kept));
        let writes = incoming_writes(change, incoming).await?;
        match verify_mount(
            change.path(),
            change.action == change::FileAction::Delete,
            shown,
            &args.operation,
            &change.from.mapping.repository.filter,
            args.repository_current.path(),
            held,
            &writes,
            args.options.forward_changes,
        )
        .await?
        {
            MountVerdict::Remove => {}
            MountVerdict::KeepModified(paths) => forwarded.extend(paths),
            MountVerdict::KeepMount => verdicts[index].0 = Verdict::Drop,
        }
    }
    Ok(forwarded)
}

/// Checks the mount of each link of `replacing` that a merge deletes or moves, as
/// [`verify_mount`] does against the link `current` holds there, or `staged`, the staged state of
/// the merge it resumes, where that holds one ([`mount_shown`]), the merge forwarding nothing.
pub(crate) async fn verify_merge_mounts(
    operation: &Arc<InstanceOperationImpl>,
    current: &NodeMapping,
    staged: Option<&NodeMapping>,
    replacing: &[NodeChange],
    incoming: &IncomingChanges,
) -> Result<(), SyncError> {
    for change in replacing {
        let Some(shown) = mount_shown(change, Some(current), staged).await? else {
            continue;
        };
        let writes = incoming_writes(change, incoming).await?;
        verify_mount(
            change.path(),
            change.action == change::FileAction::Delete,
            shown,
            operation,
            &current.repository.filter,
            current.repository.path(),
            &[],
            &writes,
            false,
        )
        .await?;
    }
    Ok(())
}

/// Refuses removing the mount at `mount` of the tree `shown`, which a link `repository` holds
/// there mounts, for `incoming`, the tree of a link to another repository, where a sync moving the
/// link to that repository refuses ([`verify_mount`]).
pub(crate) async fn verify_mount_replacement(
    operation: &Arc<InstanceOperationImpl>,
    repository: &RepositoryContext,
    mount: &RelativePath,
    shown: Option<MountedTree>,
    incoming: NodeMapping,
) -> Result<(), SyncError> {
    verify_mount(
        mount,
        false,
        shown,
        operation,
        &repository.filter,
        repository.path(),
        &[],
        &IncomingWrites::Tree(incoming),
        false,
    )
    .await
    .map(|_| ())
}

/// `paths` lowercase and sorted, for a binary search by case-folded path.
fn folded_and_sorted(paths: &[RelativePath]) -> Vec<&str> {
    let mut folded: Vec<&str> = paths.iter().map(RelativePath::as_lowercase_str).collect();
    folded.sort_unstable();
    folded
}

/// Why removing a link's mount leaves an entry in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeptKind {
    /// The link does not track it, or tracks another kind of node there.
    Untracked,
    /// The link does not track it, and the ignore rules exclude it.
    Ignored,
    /// The view excludes it.
    ViewExcluded,
    /// It cannot be read, or the link it stands for cannot be opened.
    Unreadable,
    /// The mount itself is a symbolic link or a special file, which the removal does not follow.
    Unfollowed,
    /// The change set leaves it there ([`VerifiedChanges::kept`]).
    Held,
}

impl KeptKind {
    /// What the link does not track at a path the filter answers `states` for.
    fn untracked(states: FilterStates) -> Self {
        if states.excluded(FilterMode::Ignore) {
            KeptKind::Ignored
        } else {
            KeptKind::Untracked
        }
    }

    /// The kind as a refusal names it.
    fn describe(self) -> &'static str {
        match self {
            KeptKind::Untracked => "Untracked",
            KeptKind::Ignored => "Ignored",
            KeptKind::ViewExcluded => "View-excluded",
            KeptKind::Unreadable => "Unreadable",
            KeptKind::Unfollowed => "Symbolic link or special file",
            KeptKind::Held => "Kept",
        }
    }
}

/// An entry removing a link's mount leaves in place with all it holds.
struct KeptEntry {
    path: RelativePath,
    is_directory: bool,
    kind: KeptKind,
}

impl KeptEntry {
    /// What the entry is where it is the mount itself ([`MountWalk::new`]), as a refusal names it.
    fn describe_mount(&self) -> &'static str {
        match (self.kind, self.is_directory) {
            (KeptKind::Unfollowed, _) => "a symbolic link or special file",
            (KeptKind::Unreadable, _) => "a directory that cannot be read",
            (_, true) => "a nested repository",
            (_, false) => "a file",
        }
    }
}

/// What the incoming revision writes at and around a link's mount.
enum IncomingWrites<'a> {
    /// The changes of a change set, of which those the view keeps write.
    Changes(&'a IncomingChanges),
    /// The tree a link mounts there.
    Tree(NodeMapping),
}

impl IncomingWrites<'_> {
    /// Whether the incoming revision writes a node that `kept` stands in the way of
    /// ([`meets_kept_entry`]).
    async fn meet(&self, kept: &KeptEntry, filter: &Filter) -> Result<bool, SyncError> {
        match self {
            IncomingWrites::Changes(incoming) => Ok(incoming.writes_meet(kept, filter)),
            IncomingWrites::Tree(incoming) => incoming_tree_meets(incoming, filter, kept).await,
        }
    }

    /// Whether the incoming revision may write anything at, below or above `mount`.
    fn near(&self, mount: &RelativePath) -> bool {
        match self {
            IncomingWrites::Changes(incoming) => incoming.writes_near(mount),
            IncomingWrites::Tree(_) => true,
        }
    }
}

/// Whether a node written at `path` meets `kept`: at its path where either is not a directory,
/// at a directory above it as anything else, and below it where it is not a directory.
fn meets_kept_entry(kept: &KeptEntry, path: &RelativePath, is_directory: bool) -> bool {
    if path.as_lowercase_str() == kept.path.as_lowercase_str() {
        return !(kept.is_directory && is_directory);
    }
    if path.covers_ignore_case(&kept.path) {
        return !is_directory;
    }
    kept.path.covers_ignore_case(path) && !kept.is_directory
}

/// Whether the tree `incoming` maps at a mount writes a node that meets `kept`
/// ([`meets_kept_entry`]): the node at its path, or the nearest above it.
async fn incoming_tree_meets(
    incoming: &NodeMapping,
    filter: &Filter,
    kept: &KeptEntry,
) -> Result<bool, SyncError> {
    let mut path = kept.path.clone();
    while path.as_str() != incoming.path.as_str() && incoming.path.covers(&path) {
        if let Some(held) = incoming.held_at(&path).await {
            let is_directory = held
                .read()
                .await
                .forward::<SyncError>("Failed loading the incoming node")?
                .is_directory();
            return Ok(!filter.excludes(&path, is_directory, FilterMode::View)
                && meets_kept_entry(kept, &path, is_directory));
        }
        path = path.parent_path();
    }
    Ok(false)
}

/// Refuses removing the mount at `mount` of the tree `shown` maps there where an entry the
/// removal leaves in place stands where `incoming` writes. A file `held` names is the incoming
/// revision's own.
async fn refuse_kept_entries_in_the_way(
    operation: &InstanceOperationImpl,
    filter: &Filter,
    root: Option<&Path>,
    mount: &RelativePath,
    shown: MountedTree,
    held: &[&str],
    incoming: &IncomingWrites<'_>,
) -> Result<(), SyncError> {
    if !incoming.near(mount) {
        return Ok(());
    }
    let mut walk = MountWalk::new(operation, filter, root, mount, shown, held).await?;
    loop {
        let Some(entry) = walk.next().await else {
            break;
        };
        let MountEntry::Kept(kept) = entry? else {
            continue;
        };
        if kept.kind != KeptKind::Held && incoming.meet(&kept, filter).await? {
            if kept.path.as_str() == mount.as_str() {
                lore_error!(
                    "Link mount {mount} is {} in the way of the incoming revision",
                    kept.describe_mount()
                );
            } else {
                lore_error!(
                    "{} {} in link mount {mount} is in the way of the incoming revision",
                    kept.kind.describe(),
                    kept.path
                );
            }
            lore_error!("Move it out of the way, or use --force to discard it");
            return Err(LocalModifications.into());
        }
    }
    Ok(())
}

/// What a walk of a link's mount finds ([`MountWalk`]).
enum MountEntry {
    /// A file the link tracks, or a copy a conflicted merge left beside one, which removing the
    /// mount removes.
    Tracked(RelativePath),
    /// An entry removing the mount leaves in place with all it holds.
    Kept(KeptEntry),
    /// A directory the link tracks whose entries the walk has all yielded, none of them kept,
    /// which removing the mount removes once they are gone.
    Left(RelativePath),
}

/// A directory a [`MountWalk`] is listing, with the node the link's tree holds there.
struct MountFrame {
    directory: RelativePath,
    listing: crate::fs::filesystem_provider::DirectoryListing,
    node: NodeID,
    tree: crate::link::LinkedState,
    states: FilterStates,
    /// Whether the change set leaves the directory there.
    held: bool,
    /// Whether no entry yielded below the directory is kept.
    emptied: bool,
}

/// A walk of the working tree under a link's mount against the tree the link mounts there, which
/// tells what the link tracks from what it does not, as removing the mount does.
///
/// An entry the link does not track, or tracks as another kind of node, is kept with all it holds,
/// as is a symbolic link or a special file, a nested repository, and an entry the view excludes,
/// that cannot be read, or that the change set leaves there. A copy a conflicted merge left beside
/// a file the link holds as a merge conflict goes with that file ([`is_conflict_artifact`]). A
/// directory the link tracks is walked. Each entry is weighed as its directory's listing yields
/// it, so the walk holds one listing per level of the directories it walks.
struct MountWalk<'a> {
    operation: &'a InstanceOperationImpl,
    filter: &'a Filter,
    root: Option<&'a Path>,
    held: &'a [&'a str],
    frames: Vec<MountFrame>,
    /// The mount itself where it is no directory to walk.
    kept_mount: Option<KeptEntry>,
}

impl<'a> MountWalk<'a> {
    /// A walk of the mount at `mount`, which `shown` maps. `held` names the files and directories
    /// the change set leaves there, lowercase and sorted, and `root` is the working tree's, to tell
    /// a nested repository by.
    async fn new(
        operation: &'a InstanceOperationImpl,
        filter: &'a Filter,
        root: Option<&'a Path>,
        mount: &RelativePath,
        shown: MountedTree,
        held: &'a [&'a str],
    ) -> Result<Self, SyncError> {
        let mut walk = Self {
            operation,
            filter,
            root,
            held,
            frames: Vec::new(),
            kept_mount: None,
        };
        let kept = |is_directory, kind| {
            Some(KeptEntry {
                path: mount.clone(),
                is_directory,
                kind,
            })
        };
        if state::holds_untracked_kind(operation, mount).await {
            walk.kept_mount = kept(false, KeptKind::Unfollowed);
            return Ok(walk);
        }
        let info = operation.file_info(mount).await?;
        if !info.exists() {
            return Ok(walk);
        }
        if !info.is_dir() || is_nested_repository(root, mount).await {
            walk.kept_mount = kept(info.is_dir(), KeptKind::Untracked);
            return Ok(walk);
        }
        let (node, tree) = shown;
        match operation.read_directory(mount).await {
            Ok(listing) => walk.frames.push(MountFrame {
                directory: mount.clone(),
                listing,
                node,
                tree,
                states: filter.mount_states(mount),
                held: false,
                emptied: true,
            }),
            Err(_) => walk.kept_mount = kept(true, KeptKind::Unreadable),
        }
        Ok(walk)
    }

    /// The next entry, a directory left once its entries are, or `None` once the walk is done.
    async fn next(&mut self) -> Option<Result<MountEntry, SyncError>> {
        if let Some(kept) = self.kept_mount.take() {
            return Some(Ok(MountEntry::Kept(kept)));
        }
        loop {
            let frame = self.frames.last_mut()?;
            let Some(listed) = frame.listing.next_with_untracked().await else {
                let frame = self.frames.pop()?;
                if frame.emptied && !frame.held {
                    return Some(Ok(MountEntry::Left(frame.directory)));
                }
                self.keep_last();
                continue;
            };
            match self.weigh(listed).await {
                Ok(Some(entry)) => {
                    if !matches!(entry, MountEntry::Tracked(_)) {
                        self.keep_last();
                    }
                    return Some(Ok(entry));
                }
                Ok(None) => {}
                Err(err) => return Some(Err(err)),
            }
        }
    }

    /// Marks the directory being listed as keeping an entry, which its removal then keeps too.
    fn keep_last(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            frame.emptied = false;
        }
    }

    /// What the listed entry is to removing the mount, or `None` where it is a directory the walk
    /// now lists.
    async fn weigh(
        &mut self,
        listed: Result<ListedEntry, FsError>,
    ) -> Result<Option<MountEntry>, SyncError> {
        let (name, is_directory, trackable) = match listed? {
            ListedEntry::Tracked(entry) => (entry.name, entry.info.is_dir(), true),
            ListedEntry::Untracked(name) => (name, false, false),
        };
        let Some(frame) = self.frames.last() else {
            return Ok(None);
        };
        let path = frame.directory.join(name.as_str());
        let (parent, (repository, state)) = (frame.node, frame.tree.clone());
        let (states, _) =
            self.filter
                .child_excludes_tree(frame.states, &path, is_directory, FilterMode::Full);
        let kept = |path: RelativePath, kind| -> Result<Option<MountEntry>, SyncError> {
            Ok(Some(MountEntry::Kept(KeptEntry {
                path,
                is_directory,
                kind,
            })))
        };
        let held = self.held.binary_search(&path.as_lowercase_str()).is_ok();
        if held && !is_directory {
            return kept(path, KeptKind::Held);
        }
        if !trackable || crate::repository::is_reserved_node_name(name.as_str()) {
            return kept(path, KeptKind::untracked(states));
        }
        if states.excluded(FilterMode::View) {
            return kept(path, KeptKind::ViewExcluded);
        }
        let child = match state
            .find_subnode(repository.clone(), parent, hash::hash_string(name.as_str()))
            .await
        {
            Ok(child) => child,
            Err(err) if err.is_node_not_found() => {
                if !is_directory && is_conflict_artifact(&frame.tree, parent, name.as_str()).await {
                    return Ok(Some(MountEntry::Tracked(path)));
                }
                return kept(path, KeptKind::untracked(states));
            }
            Err(err) => {
                return Err(err)
                    .forward::<SyncError>("Failed looking up an entry in a link's tree");
            }
        };
        let node = state
            .node(repository.clone(), child)
            .await
            .forward::<SyncError>("Failed loading a node a link tracks")?;
        if node.is_file() {
            if is_directory {
                return kept(path, KeptKind::untracked(states));
            }
            return Ok(Some(MountEntry::Tracked(path)));
        }
        if !is_directory || is_nested_repository(self.root, &path).await {
            return kept(path, KeptKind::untracked(states));
        }
        let (node, tree) = if node.is_link() {
            let Some(nested) = open_mounted_tree(&repository, &node).await else {
                return kept(path, KeptKind::Unreadable);
            };
            nested
        } else {
            (child, (repository, state))
        };
        let Ok(listing) = self.operation.read_directory(&path).await else {
            return kept(path, KeptKind::Unreadable);
        };
        self.frames.push(MountFrame {
            directory: path,
            listing,
            node,
            tree,
            states,
            held,
            emptied: true,
        });
        Ok(None)
    }
}

/// Whether `name` is a copy a conflicted merge left beside a file `tree` holds in `parent` as a
/// merge conflict: it belongs to the merge, as the conflicted file does, and goes with it.
async fn is_conflict_artifact(tree: &crate::link::LinkedState, parent: NodeID, name: &str) -> bool {
    let Some(conflicted) = MERGE_ARTIFACT_SUFFIXES
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
    else {
        return false;
    };
    let (repository, state) = tree;
    let Ok(child) = state
        .find_subnode(repository.clone(), parent, hash::hash_string(conflicted))
        .await
    else {
        return false;
    };
    state
        .node(repository.clone(), child)
        .await
        .is_ok_and(|node| node.is_file() && node.is_staged_merge_conflict())
}

/// Whether the directory at `path` is the root of a repository of its own, which the working tree
/// below `root` holds. Without a root on disk there is none.
async fn is_nested_repository(root: Option<&Path>, path: &RelativePath) -> bool {
    let Some(mut parent) = root.and_then(|root| path.parent_path().to_absolute_path(root).ok())
    else {
        return false;
    };
    state::is_nested_repository_root(&mut parent, path.name()).await
}

/// What removing a link's mount left in place, counted by why.
#[derive(Default)]
pub(crate) struct KeptEntries {
    untracked: usize,
    ignored: usize,
    view_excluded: usize,
    unreadable: usize,
}

impl KeptEntries {
    fn count(&mut self, kind: KeptKind) {
        match kind {
            KeptKind::Untracked | KeptKind::Unfollowed => self.untracked += 1,
            KeptKind::Ignored => self.ignored += 1,
            KeptKind::ViewExcluded => self.view_excluded += 1,
            KeptKind::Unreadable => self.unreadable += 1,
            KeptKind::Held => {}
        }
    }

    /// Tells the user what stayed in `mount`, where anything did.
    pub(crate) fn report(&self, mount: &RelativePath) {
        let counts = [
            (self.untracked, "untracked"),
            (self.ignored, "ignored"),
            (self.view_excluded, "view-excluded"),
            (self.unreadable, "unreadable"),
        ];
        let parts: Vec<String> = counts
            .iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, kind)| format!("{count} {kind}"))
            .collect();
        let Some((last, rest)) = parts.split_last() else {
            return;
        };
        let listed = if rest.is_empty() {
            last.clone()
        } else {
            format!("{} and {last}", rest.join(", "))
        };
        let total: usize = counts.iter().map(|(count, _)| count).sum();
        let entries = if total == 1 { "entry" } else { "entries" };
        lore_warn!("Kept {listed} {entries} in link mount {mount}");
    }
}

/// Removes the mount at `mount` of the tree `shown` maps there but for what the link does not track
/// ([`MountWalk`]), which stays with the directories leading to it, and the files `held` names,
/// lowercase and sorted, and answers what stayed. A forced removal takes everything. Where `shown`
/// is `None`, the link's revision cannot be opened, so only an empty mount goes. A provider holding
/// its state has no mount to remove: the link it serves at `mount` is taken away
/// ([`InstanceOperationImpl::place_link`]).
///
/// `root` is the working tree's, to tell a nested repository by.
pub(crate) async fn remove_link_mount(
    operation: &InstanceOperationImpl,
    filter: &Filter,
    root: Option<&Path>,
    mount: &RelativePath,
    shown: Option<MountedTree>,
    held: &[&str],
) -> Result<KeptEntries, SyncError> {
    let mut kept = KeptEntries::default();
    if operation.holds_state() {
        operation.place_link(mount, None, false).await?;
        return Ok(kept);
    }
    if execution_context().globals().force() {
        operation.remove_recursive(mount).await?;
        return Ok(kept);
    }
    let Some(shown) = shown else {
        let _ = operation.remove_empty_directory(mount).await;
        return Ok(kept);
    };
    let mut walk = MountWalk::new(operation, filter, root, mount, shown, held).await?;
    loop {
        let Some(entry) = walk.next().await else {
            break;
        };
        match entry? {
            MountEntry::Tracked(path) => operation.remove(&path).await?,
            MountEntry::Kept(entry) => kept.count(entry.kind),
            MountEntry::Left(directory) => {
                if operation.remove_empty_directory(&directory).await.is_err() {
                    walk.keep_last();
                }
            }
        }
    }
    Ok(kept)
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

/// What [`verify_filesystem`] answers for a change whose working file holds the content it
/// brings in with the executable bit `held`: the bit to carry where it differs from the change's
/// own and is no local one, and nothing to do otherwise.
fn held_content_verdict(change: &NodeChange, held: u16) -> Verdict {
    if !change.flags.is_local_mode() && util::fs::mode_changed(held, change.to.mode) {
        Verdict::CarryMode
    } else {
        Verdict::Drop
    }
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

/// The moves, link deletes and writes of a change set, looked up by path.
pub struct IncomingChanges {
    changes: Arc<Vec<NodeChange>>,
    by_source: OnceLock<Vec<usize>>,
    deleted_links: OnceLock<Vec<DeletedLink>>,
    writes: OnceLock<Vec<usize>>,
}

/// A link delete of a change set, and the tree it mounts, opened once for every path below it.
struct DeletedLink {
    index: usize,
    mounted: tokio::sync::OnceCell<Option<(NodeID, crate::link::LinkedState)>>,
}

impl IncomingChanges {
    /// The changes `changes` holds, indexed on the first lookup of each kind.
    pub fn new(changes: Arc<Vec<NodeChange>>) -> Self {
        Self {
            changes,
            by_source: OnceLock::new(),
            deleted_links: OnceLock::new(),
            writes: OnceLock::new(),
        }
    }

    /// The changes writing a node, those deleting none, sorted by their case-folded path.
    fn writes(&self) -> &[usize] {
        let changes = &self.changes;
        self.writes.get_or_init(|| {
            let mut writes: Vec<usize> = (0..changes.len())
                .filter(|&index| changes[index].action != change::FileAction::Delete)
                .collect();
            writes.sort_unstable_by(|&a, &b| {
                changes[a]
                    .path()
                    .as_lowercase_str()
                    .cmp(changes[b].path().as_lowercase_str())
            });
            writes
        })
    }

    /// The changes writing a node at `folded`, a case-folded path, one for each spelling there.
    fn writes_at<'s>(&'s self, folded: &'s str) -> impl Iterator<Item = &'s NodeChange> + 's {
        let writes = self.writes();
        let first =
            writes.partition_point(|&index| self.changes[index].path().as_lowercase_str() < folded);
        writes[first..]
            .iter()
            .map(|&index| &self.changes[index])
            .take_while(move |write| write.path().as_lowercase_str() == folded)
    }

    /// The changes writing a node below the directory at `folded`, a case-folded path.
    fn writes_below<'s>(&'s self, folded: &str) -> impl Iterator<Item = &'s NodeChange> + 's {
        let inside = format!("{folded}/");
        let writes = self.writes();
        let first = writes.partition_point(|&index| {
            self.changes[index].path().as_lowercase_str() < inside.as_str()
        });
        writes[first..]
            .iter()
            .map(|&index| &self.changes[index])
            .take_while(move |write| write.path().as_lowercase_str().starts_with(inside.as_str()))
    }

    /// Whether the change set writes a node at `path`, below it, or at a directory above it.
    fn writes_near(&self, path: &RelativePath) -> bool {
        let folded = path.as_lowercase_str();
        let mut above = folded;
        loop {
            if self.writes_at(above).next().is_some() {
                return true;
            }
            let Some((parent, _)) = above.rsplit_once('/') else {
                break;
            };
            above = parent;
        }
        self.writes_below(folded).next().is_some()
    }

    /// Whether a change the view keeps writes a node that `kept` stands in the way of
    /// ([`meets_kept_entry`]).
    fn writes_meet(&self, kept: &KeptEntry, filter: &Filter) -> bool {
        let in_view = |write: &&NodeChange| {
            !filter.excludes(
                write.path(),
                write.to.flags.is_directory(),
                FilterMode::View,
            )
        };
        let folded = kept.path.as_lowercase_str();
        if self
            .writes_at(folded)
            .filter(in_view)
            .any(|write| meets_kept_entry(kept, write.path(), write.to.flags.is_directory()))
        {
            return true;
        }
        let mut below = folded;
        while let Some((above, _)) = below.rsplit_once('/') {
            if self
                .writes_at(above)
                .filter(in_view)
                .any(|write| !write.to.flags.is_directory())
            {
                return true;
            }
            below = above;
        }
        !kept.is_directory && self.writes_below(folded).any(|write| in_view(&write))
    }

    /// The delete of a link mounted above `path`, which removes everything below the mount from
    /// the working tree.
    pub(crate) fn link_delete_above(&self, path: &RelativePath) -> Option<&NodeChange> {
        self.deleted_link(path, false)
            .map(|(deleted, _)| &self.changes[deleted.index])
    }

    /// The index of the delete of a link mounted at or above `path`, the outermost where several
    /// are.
    fn link_delete_covering(&self, path: &RelativePath) -> Option<usize> {
        self.deleted_link(path, true)
            .map(|(deleted, _)| deleted.index)
    }

    /// The node at `path` in the tree that the link deleted above `path` mounts, with the context
    /// and state holding it, or `None` where that tree holds none or the link cannot be opened.
    /// The link's tree is opened on the first path below its mount and shared by the rest.
    async fn held_below_deleted_link(
        &self,
        path: &RelativePath,
    ) -> Option<(NodeID, crate::link::LinkedState)> {
        let (deleted, mount_components) = self.deleted_link(path, false)?;
        let link = &self.changes[deleted.index];
        let (mounted, (repository, state)) = deleted
            .mounted
            .get_or_init(|| open_deleted_link(link))
            .await
            .as_ref()?;
        let below = path
            .as_str()
            .splitn(mount_components + 1, '/')
            .nth(mount_components)?;
        let held = state
            .locate_node_below(repository.clone(), *mounted, below)
            .await
            .ok()?
            .held((repository.clone(), state.clone()));
        held.id().is_valid_node_id().then(|| held.into_parts())
    }

    /// The link delete mounted above `path`, or at it where `at_path` says so, the outermost
    /// where several are, with the number of components of its mount.
    ///
    /// Matched ignoring case, since on a case-insensitive file system the delete removes the
    /// mount under any spelling. On a case-sensitive one a case variant of the mount is a
    /// directory of its own, and a file there is measured as though the link held it.
    fn deleted_link(&self, path: &RelativePath, at_path: bool) -> Option<(&DeletedLink, usize)> {
        let changes = &self.changes;
        let deleted_links = self.deleted_links.get_or_init(|| {
            let mut deleted_links: Vec<DeletedLink> = (0..changes.len())
                .filter(|&index| {
                    changes[index].action == change::FileAction::Delete
                        && changes[index].from.is_link()
                })
                .map(|index| DeletedLink {
                    index,
                    mounted: tokio::sync::OnceCell::new(),
                })
                .collect();
            deleted_links.sort_unstable_by(|a, b| {
                changes[a.index]
                    .path()
                    .as_lowercase_str()
                    .cmp(changes[b.index].path().as_lowercase_str())
            });
            deleted_links
        });
        if deleted_links.is_empty() {
            return None;
        }
        // Split the lowercase spelling itself: lowercasing can change byte lengths.
        let folded = path.as_lowercase_str();
        let at = at_path.then_some(folded.len());
        folded
            .match_indices('/')
            .map(|(end, _)| end)
            .chain(at)
            .find_map(|end| {
                deleted_links
                    .binary_search_by(|deleted| {
                        changes[deleted.index]
                            .path()
                            .as_lowercase_str()
                            .cmp(&folded[..end])
                    })
                    .ok()
                    // Lowercasing keeps every separator, so the count holds for `path` too.
                    .map(|found| {
                        (
                            &deleted_links[found],
                            folded[..end].matches('/').count() + 1,
                        )
                    })
            })
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

/// The node the link `delete` deletes mounts, in the context and state of the revision it pins,
/// or `None` where the link cannot be read or opened.
async fn open_deleted_link(delete: &NodeChange) -> Option<(NodeID, crate::link::LinkedState)> {
    let node = match delete.from.get_node().await {
        Ok(node) if node.is_link() => node,
        Ok(_) => return None,
        Err(err) => {
            lore_warn!("Unable to read the link at {}: {err}", delete.path());
            return None;
        }
    };
    let link = node.linked_node();
    let repository = delete
        .from
        .mapping
        .repository
        .to_link_context(link.repository)
        .await;
    match State::deserialize(repository.clone(), link.revision).await {
        Ok(state) => Some((link.node, (repository, state))),
        Err(err) => {
            lore_warn!(
                "Unable to open revision {} the link at {} pins: {err}",
                link.revision,
                delete.path()
            );
            None
        }
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
    incoming: Option<&IncomingChanges>,
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
        observed.mtime(),
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
    let mut folded = files_directly_in(directory, changes);
    if folded.is_empty() {
        return false;
    }
    folded.sort_unstable_by_key(|(name_hash, _)| *name_hash);
    remove_case_variants(operation, directory, &folded).await
}

/// [`remove_directory_of_case_variants`] for the directory the subtree change `change` deletes,
/// whose files are the tree's level below it and the files of `listed`, sorted by path, directly in
/// it.
///
/// The level is walked for the hashes of its files' names first, 8 bytes each, and the directory
/// is listed only where one of those or of `listed` names a file: a directory whose files all move
/// elsewhere costs no listing. The level is walked again, holding its changes, only where the
/// listing holds a file one of them folds to.
async fn remove_subtree_directory_of_case_variants(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    subtrees: &Arc<state::Subtrees>,
    listed: &[NodeChange],
) -> bool {
    let directory = change.path();
    let listed = files_directly_in(directory, listed);
    let mut deleted = Vec::new();
    if !walk_level_files(subtrees, change, |name_hash, _| deleted.push(name_hash)).await {
        return false;
    }
    deleted.extend(listed.iter().map(|(name_hash, _)| *name_hash));
    if deleted.is_empty() {
        return false;
    }
    deleted.sort_unstable();
    let Some(mut on_disk) = list_files(operation, directory, |entry| {
        deleted
            .binary_search(&entry.name_hash)
            .ok()
            .map(|_| entry.name_hash)
    })
    .await
    else {
        return false;
    };
    if on_disk.is_empty() {
        return false;
    }
    on_disk.sort_unstable();
    let folds = |name_hash: &u64| on_disk.binary_search(name_hash).is_ok();
    let mut below = Vec::new();
    if !walk_level_files(subtrees, change, |name_hash, child| {
        if folds(&name_hash) {
            below.push((name_hash, child));
        }
    })
    .await
    {
        return false;
    }
    let mut folded: Vec<(u64, &NodeChange)> = below
        .iter()
        .map(|(name_hash, child)| (*name_hash, child))
        .chain(listed.into_iter().filter(|(name_hash, _)| folds(name_hash)))
        .collect();
    folded.sort_unstable_by_key(|(name_hash, _)| *name_hash);
    remove_case_variants(operation, directory, &folded).await
}

/// Hands `found` each file of the tree's level below the subtree change `change`, with the hash of
/// its name, but for the ones listed on their own, and answers whether the walk completed.
async fn walk_level_files(
    subtrees: &Arc<state::Subtrees>,
    change: &NodeChange,
    mut found: impl FnMut(u64, NodeChange),
) -> bool {
    let mut level = subtrees.expand_level(change);
    loop {
        let Some(child) = level.next().await else {
            break;
        };
        if child.from.flags.contains(NodeFlags::File)
            && let Some(name_hash) = child
                .from
                .mapping
                .path
                .as_str()
                .rsplit('/')
                .next()
                .map(hash::hash_string)
        {
            found(name_hash, child);
        }
    }
    match level.finish().await {
        Ok(()) => true,
        Err(err) => {
            lore_warn!(
                "Keeping {}: the tree below it could not be walked: {err}",
                change.path()
            );
            false
        }
    }
}

/// The files that the changes of `changes`, sorted by the path each starts from, delete or move
/// away directly in `directory`, each with the hash of its name.
fn files_directly_in<'a, C: Borrow<NodeChange>>(
    directory: &RelativePath,
    changes: &'a [C],
) -> Vec<(u64, &'a NodeChange)> {
    let prefix = format!("{directory}/");
    let first = changes
        .partition_point(|change| change.borrow().from.mapping.path.as_str() < prefix.as_str());
    changes[first..]
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
        .collect()
}

/// What `take` keeps of each file `directory` holds, drained from its listing, or `None` where it
/// can't be listed.
async fn list_files<T>(
    operation: &InstanceOperationImpl,
    directory: &RelativePath,
    mut take: impl FnMut(crate::fs::filesystem_provider::DirectoryEntry) -> Option<T>,
) -> Option<Vec<T>> {
    let mut listing = operation.read_directory(directory).await.ok()?;
    let mut kept = Vec::new();
    while let Some(entry) = listing.next().await {
        if let Ok(entry) = entry
            && entry.info.is_file()
            && let Some(taken) = take(entry)
        {
            kept.push(taken);
        }
    }
    Some(kept)
}

/// Removes each file `directory` holds that folds to a change in `folded`, sorted by name hash, and
/// holds the content the change starts from or the content its move carries, and then `directory`,
/// answering whether it removed the directory. Of the listing, only the files that fold to a change
/// are kept.
async fn remove_case_variants(
    operation: &InstanceOperationImpl,
    directory: &RelativePath,
    folded: &[(u64, &NodeChange)],
) -> bool {
    let Some(variants) = list_files(operation, directory, |entry| {
        let found = folded
            .binary_search_by_key(&entry.name_hash, |(hash, _)| *hash)
            .ok()?;
        Some((entry.name, entry.info.size(), folded[found].1))
    })
    .await
    else {
        return false;
    };
    let mut removed = false;
    for (name, size, change) in &variants {
        let path = directory.join(name);
        if holds_the_tracked_content(operation, change, &path, *size).await {
            removed |= operation.remove(&path).await.is_ok();
        }
    }
    removed && operation.remove(directory).await.is_ok()
}

/// Whether the file of `size` bytes at `path` holds the content `change` starts from, or the
/// content it carries where it is a move, which makes it the file the change tracks wherever the
/// working tree spells it. A side whose node cannot be read holds nothing.
async fn holds_the_tracked_content(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    path: &RelativePath,
    size: u64,
) -> bool {
    let carried = change.move_source().map(|_| &change.to);
    for side in std::iter::once(&change.from).chain(carried) {
        let Ok(node) = side.get_node().await else {
            continue;
        };
        if holds_content_of(
            operation,
            side.mapping.repository.clone(),
            &node,
            path,
            size,
        )
        .await
        {
            return true;
        }
    }
    false
}

/// Whether the file of `size` bytes at `path` holds the content of `node`, which `repository`
/// holds. A comparison that fails holds nothing.
async fn holds_content_of(
    operation: &InstanceOperationImpl,
    repository: Arc<RepositoryContext>,
    node: &Node,
    path: &RelativePath,
    size: u64,
) -> bool {
    matches!(
        state::file_matches_node(
            repository,
            node,
            size,
            0,
            path,
            operation,
            &lore_storage::ContentHashes::default(),
        )
        .await,
        Ok(NodeComparison::Matches)
    )
}

/// What a realize reads its moves against: the revision a merge applies to, where the realize is
/// a merge's ([`tracked_source`]), the listing of each directory it reads for another spelling of
/// a move's source, held for the rest of the realize, and the directories it has moved.
struct RealizeMoves {
    merge_current: Option<MergeCurrent>,
    listings: HeldSpellings,
    /// Each directory the realize has moved, from its source as the changes name it, read through
    /// the directory moves before it, to its destination, in the order moved.
    directories: Vec<(RelativePath, RelativePath)>,
}

impl RealizeMoves {
    /// Where the directory moves realized before it carried `source`, the source of a move, the
    /// path the working tree holds it at; `None` where none of them did. A change names its source
    /// as the revision it starts from spells it, and a directory renamed only in case is one change
    /// that carries its children, so a source below it names a path the working tree no longer
    /// holds once the rename is realized.
    fn moved_source(&self, source: &RelativePath) -> Option<RelativePath> {
        let mut moved: Option<RelativePath> = None;
        for (from, to) in &self.directories {
            if let Some(below) = moved.as_ref().unwrap_or(source).below(from)
                && !below.is_empty()
            {
                moved = Some(to.join(below));
            }
        }
        moved
    }
}

/// The listing of each directory a realize reads for the spellings its entries hold
/// ([`ListedSpellings`]), keyed by the directory's path, read once and dropped with the realize.
#[derive(Default)]
struct HeldSpellings(std::collections::HashMap<String, ListedSpellings>);

impl HeldSpellings {
    /// The listing of the directory holding `path`, read the first time it is asked for, boxed:
    /// inline, the listing would make the future of every change realized as large as its own.
    async fn of(
        &mut self,
        operation: &InstanceOperationImpl,
        path: &RelativePath,
    ) -> Option<&mut ListedSpellings> {
        let directory = path.parent().unwrap_or_default();
        if !self.0.contains_key(directory) {
            let listed = Box::pin(ListedSpellings::read(operation, &path.parent_path())).await;
            self.0.insert(directory.to_owned(), listed);
        }
        self.0.get_mut(directory)
    }
}

/// The node the working tree holds at `from`, the source of `change`, a move, as the revision it
/// stands on tracks it, with the repository holding its content. A merge's change starts from its
/// base, so where `merge_current`, the revision the merge applies to, holds a node at `from`, that
/// node is the one, as [`modification_against_measured_node`] takes it; otherwise it is the node
/// the move starts from.
async fn tracked_source(
    change: &NodeChange,
    merge_current: Option<&Arc<State>>,
    from: &RelativePath,
) -> Option<(Arc<RepositoryContext>, Node)> {
    let repository = change.from.mapping.repository.clone();
    if let Some(current) = merge_current
        && let Some(held) = NodeMapping::root(repository.clone(), current.clone())
            .held_at(from)
            .await
    {
        let node = held.read().await.ok()?;
        return Some((held.repository().clone(), node));
    }
    let node = change.from.get_node().await.ok()?;
    Some((repository, node))
}

/// Where the working tree holds no entry spelled as `from`, the source of a move, renames to `to`
/// the entry its directory holds `from` as in another spelling, answering whether it did. The
/// entry is the one [`ListedSpellings::claim`] pairs with the node the working tree holds at the
/// source ([`tracked_source`]), from the listing of the directory `moves` holds for the realize.
/// A file is moved only where it holds that node's content or the content the move carries: one
/// holding other content is local work, and stays where it stands. A directory, or a link's mount,
/// is moved with what it holds.
///
/// Nothing is renamed where the working tree holds an entry at `to` already, as one an earlier
/// pass carried, an untracked one or one of another kind: the spelling the working tree holds the
/// source in stays where it stands. Only where the listing holds another spelling of the source's
/// name ([`ListedSpellings::holds_another_spelling`]) is `to` described, and the entry paired,
/// boxed. Its caller leaves out a source the view excludes.
async fn rename_held_spelling(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    moves: &mut RealizeMoves,
    from: &RelativePath,
    to: &RelativePath,
) -> bool {
    let RealizeMoves {
        merge_current,
        listings,
        ..
    } = moves;
    let Some(listed) = listings.of(operation, from).await else {
        return false;
    };
    listed.holds_another_spelling(from.name())
        && !operation
            .file_info(to)
            .await
            .is_ok_and(|info| info.exists())
        && Box::pin(rename_paired_spelling(
            operation,
            change,
            merge_current.as_ref().map(|current| &current.state),
            listed,
            from,
            to,
        ))
        .await
}

/// [`rename_held_spelling`] where `listed`, the listing of the directory holding `from`, holds
/// another spelling of its name: pairs the entry with the source's node, compares its content, and
/// renames it to `to` ([`InstanceOperationImpl::rename_creating_parent`]). A cold path, boxed by its
/// caller.
#[allow(clippy::manual_async_fn)]
fn rename_paired_spelling<'a>(
    operation: &'a InstanceOperationImpl,
    change: &'a NodeChange,
    merge_current: Option<&'a Arc<State>>,
    listed: &'a mut ListedSpellings,
    from: &'a RelativePath,
    to: &'a RelativePath,
) -> impl Future<Output = bool> + 'a {
    async move {
        let Some((repository, node)) = tracked_source(change, merge_current, from).await else {
            return false;
        };
        let Some(name) = listed.claim(from.name(), &node) else {
            return false;
        };
        let held = from.parent_path().join(&name);
        let Ok(info) = operation.file_info(&held).await else {
            return false;
        };
        if !types_agree(&node, info.is_file(), info.is_dir()) {
            return false;
        }
        if info.is_file()
            && !holds_content_of(operation, repository, &node, &held, info.size()).await
            && !holds_the_carried_content(operation, change, &held, info.size()).await
        {
            return false;
        }
        lore_trace!("Moving {held}, which the working tree holds {from} as, to {to}");
        operation.rename_creating_parent(&held, to).await.is_ok()
    }
}

/// Holds on to the first failure of a verification in `failure`, and marks `stats` refused where `err`
/// refuses the sync for local modifications, so a check of a deleted directory still running stops at
/// the next difference it finds.
#[lore_macro::test_pub]
pub(crate) fn record_verify_failure(
    stats: &SyncVerifyStats,
    failure: &mut Option<SyncError>,
    err: SyncError,
) {
    if err.is_local_modifications() {
        stats.refused.store(true, Ordering::Relaxed);
    }
    *failure = failure.take().or(Some(err));
}

/// Whether the file of `size` bytes at `path` holds the content `change`, a move, carries.
async fn holds_the_carried_content(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    path: &RelativePath,
    size: u64,
) -> bool {
    let Ok(node) = change.to.get_node().await else {
        return false;
    };
    holds_content_of(
        operation,
        change.to.mapping.repository.clone(),
        &node,
        path,
        size,
    )
    .await
}

/// What [`verify_filesystem`] leaves to do with a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing: the working tree holds what the change brings in, or keeps local work over it.
    Drop,
    /// Realize the change.
    Realize,
    /// Give the file the change's executable bit: it holds the rest of what the change brings in.
    CarryMode,
}

/// Checks `change` against the working tree, refusing the ones that would overwrite local work
/// and settling on it what realizing it has to know, and answers what is left to do with it.
///
/// A caller that realizes every change it verified, which a merge does to build the staged state
/// out of them, takes the change as this leaves it and disregards the answer, and says so in
/// `realizes_every_change`. One that realizes only what the working tree still needs, which a
/// sync does, acts on the answer. This changes no executable bit: [`verify_filesystem_for_changes`]
/// carries the bits it answers [`Verdict::CarryMode`] for once every change has passed, where its
/// caller asks for it ([`SyncVerifyArgs::carries_modes`]), and a caller realizing every change
/// carries them in the realize.
///
/// A file found to hold the incoming content has its time recorded where it is older than the
/// operation's clock floor ([`InstanceOperationImpl::clock_floor`]), which a comparison reads
/// before reading the file.
///
/// `incoming` holds the change set the change belongs to, or at least its moves and link deletes:
/// the moves answer for the target where a directory is deleted, and the link deletes for a file
/// below the mount they remove. A change below such a mount that this drops names a file the
/// mount's removal keeps ([`VerifiedChanges::kept`]).
#[allow(clippy::too_many_arguments)]
pub async fn verify_filesystem(
    change: &mut NodeChange,
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    current: NodeMapping,
    forward_changes: bool,
    realizes_every_change: bool,
    force_full_check: bool,
    stats: Arc<SyncVerifyStats>,
    filter_mode: FilterMode,
    incoming: &IncomingChanges,
) -> Result<Verdict, SyncError> {
    lore_trace!("Verify path: {change:?}");
    if let Some(source) = change.relocated_from()
        && change.from.flags.contains(NodeFlags::File)
        && Box::pin(moved_file_holds_local_work(
            &operation,
            repository.clone(),
            change,
            &current,
            force_full_check,
            source,
            incoming,
        ))
        .await?
    {
        if forward_changes && !realizes_every_change {
            lore_info!("Keeping the locally modified file a move carries away: {source}");
            return Ok(Verdict::Drop);
        }
        lore_error!(
            "File has local changes: {source}, which the incoming revision moves to {}",
            change.path()
        );
        return Err(LocalModifications.into());
    }
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
        incoming,
    )
    .await?;

    if mode_is_the_working_tree_own(&operation, change, &modifications).await? {
        change.flags |= change::Flags::LocalMode;
    }

    if !modifications.info.exists() {
        if operation
            .unversioned_symlink_at_or_above(&repository_path)
            .await?
        {
            // A caller realizing every change would write over it whatever this answers.
            if forward_changes && !realizes_every_change {
                lore_info!("Keeping the symbolic link at {}", change.path());
                return Ok(Verdict::Drop);
            }
            lore_error!("A symbolic link stands at or above {}", change.path());
            return Err(LocalModifications.into());
        }
        return match change.action {
            change::FileAction::Add => {
                // Nothing exist in file system, safe to add
                lore_trace!(
                    "Nothing exist in file system for {}, safe to add",
                    change.path()
                );
                Ok(Verdict::Realize)
            }
            change::FileAction::Delete => {
                // Nothing exists, delete is a no-op
                lore_trace!(
                    "Nothing exist in file system for {}, delete is no-op",
                    change.path()
                );
                Ok(Verdict::Realize)
            }
            _ => {
                if forward_changes {
                    lore_info!(
                        "Keeping modified file as locally deleted: {}",
                        change.path()
                    );
                    Ok(Verdict::Drop)
                } else {
                    lore_trace!(
                        "Restoring modified file which was locally deleted: {}",
                        change.path()
                    );
                    Ok(Verdict::Realize)
                }
            }
        };
    }

    let is_file = modifications.info.is_file();
    let file_size = modifications.info.size();

    if let Some(modification) = modifications.modification {
        // Check if file is modified
        if !modification.modified {
            if holds_the_content(change, modifications.measured.as_ref()) {
                change.flags |= change::Flags::HoldsContent;
            }
            // An add measures against the current revision only through the mount of a link the
            // change set deletes, whose removal keeps the file this leaves in place.
            if matches!(
                change.action,
                change::FileAction::Keep | change::FileAction::Add
            ) && let Some(measured) = modifications.measured.as_ref()
                && measured.is_current
                && !measured.node.address.is_zero()
                && measured.node.address == incoming_address(change).await
            {
                if modifications.info.mtime() < operation.clock_floor(&repository).await {
                    operation.record_modified_time(
                        &change.from.mapping.repository,
                        change.path(),
                        modifications.info.mtime(),
                    );
                }
                return Ok(held_content_verdict(
                    change,
                    modifications.info.mode(change.to.mode),
                ));
            }
            stats.file_retain.fetch_add(1, Ordering::Relaxed);
            return Ok(Verdict::Realize);
        }
        stats.file_replace.fetch_add(1, Ordering::Relaxed);
    }

    let is_delete = change.action == change::FileAction::Delete;
    let was_link = change.from.is_link();

    if is_delete && was_link {
        lore_debug!("Link is for delete, its mount is verified once every change is");
        return Ok(Verdict::Realize);
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
                    return Ok(Verdict::Drop);
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
                return Ok(Verdict::Drop);
            }

            lore_trace!(
                "Change {} from file to directory, previous change will have deleted it",
                change.path()
            );
            return Ok(Verdict::Realize);
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
                0,
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
                    return Ok(Verdict::Drop);
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
                    return Ok(Verdict::Realize);
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

                if modifications.info.mtime() < operation.clock_floor(&repository).await {
                    operation.record_modified_time(
                        &change.from.mapping.repository,
                        change.path(),
                        modifications.info.mtime(),
                    );
                }

                return Ok(held_content_verdict(
                    change,
                    modifications.info.mode(change.to.mode),
                ));
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
            return Ok(Verdict::Drop);
        }

        if is_delete {
            let (subnode_current, (repository_current, state_current)) = current
                .node_at(change.path())
                .await
                .held((repository.clone(), current.state.clone()))
                .into_parts();
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
            loop {
                let Some(subchange) = directory_changes.next().await else {
                    break;
                };
                if stats.refused.load(Ordering::Relaxed) {
                    directory_changes.abandon().await;
                    return Err(LocalModifications.into());
                }
                let incoming = (!has_modified_file).then_some(incoming);
                if !holds_local_work(&operation, &subchange, incoming).await? {
                    lore_trace!(
                        "Deleted directory entry {} holds no local work",
                        subchange.path()
                    );
                    continue;
                }

                if !has_modified_file {
                    lore_error!(
                        "Deleted directory has modified files in file system: {}",
                        change.path()
                    );
                }
                has_modified_file = true;
                Box::pin(report_local_work(&operation, &subchange)).await;
            }
            directory_changes.finish().await.forward::<SyncError>(
                "Failed to calculate delta changes between file system and target state",
            )?;
            if has_modified_file {
                return Err(LocalModifications.into());
            }
            return Ok(Verdict::Realize);
        }
        // If this is a change from directory to file, there will have been a previous change
        // which deletes the existing directory node which will have verified the filesystem
        // state already - just allow the new file to be written
        lore_trace!(
            "Change {} from directory to file, previous change will have deleted it",
            change.path()
        );
        return Ok(Verdict::Realize);
    }

    Ok(Verdict::Realize)
}

/// What a realize of a merge's changes, which start from the merge's base, reads the working tree
/// against.
#[derive(Clone)]
pub struct MergeCurrent {
    /// The revision the merge applies to, which the working tree stands on.
    pub state: Arc<State>,
    /// The staged state of the merge it resumes, whose link pins name what the mounts of the links
    /// merged on an earlier pass show ([`mount_shown`]).
    pub staged: Option<Arc<State>>,
}

impl MergeCurrent {
    /// The merge of `state`, resuming none.
    pub fn new(state: Arc<State>) -> Self {
        MergeCurrent {
            state,
            staged: None,
        }
    }

    /// [`Self::state`] and [`Self::staged`] from their root in `repository`, for [`mount_shown`].
    fn roots(&self, repository: &Arc<RepositoryContext>) -> (NodeMapping, Option<NodeMapping>) {
        (
            NodeMapping::root(repository.clone(), self.state.clone()),
            self.staged
                .as_ref()
                .map(|staged| NodeMapping::root(repository.clone(), staged.clone())),
        )
    }
}

/// Realizes `changes` in the working tree, deletes first, and stages them in `state_stage` where
/// given. A link delete removes its mount but for the files `kept` names below it
/// ([`VerifiedChanges::kept`]). `subtrees` expands the subtree changes `changes` holds; a subtree
/// change without `subtrees`, or beside a `state_stage`, is refused.
///
/// `merge_current` is, for a merge, what the working tree stands on, as a merge's changes start
/// from its base; it is `None` for changes that start from the revision the working tree stands
/// on.
///
/// `emptied` says which directories the realize removes once its deletes and moves are done, which
/// it does last ([`remove_emptied_directories`]); with `None` it leaves them to its caller.
///
/// A change staged in `state_stage` inside a directory marked dirty as moved stages the
/// directory's whole move, as staging inside it does.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn, clippy::too_many_arguments)]
pub(crate) fn realize_changes<'a>(
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    changes: Arc<Vec<NodeChange>>,
    kept: &'a [RelativePath],
    subtrees: Option<Arc<state::Subtrees>>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    merge_current: Option<MergeCurrent>,
    emptied: Option<&'a EmptiedDirectories>,
    stats: Arc<SyncRealizeStats>,
) -> impl Future<Output = Result<(), SyncError>> + 'a {
    async move {
        if (subtrees.is_none() || state_stage.is_some())
            && changes.iter().any(|change| change.flags.is_subtree())
        {
            return Err(SyncError::internal(
                "A subtree change to realize without the subtrees expanding it, or with a staged state",
            ));
        }
        let ticker = progress_ticker(stats.clone());
        let is_merge = merge_current.is_some();

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
        let delete_context = Arc::new(DeleteContext {
            repository: repository.clone(),
            operation: operation.clone(),
            state_stage: state_stage.clone(),
            dry_run,
            merge_current: merge_current.clone(),
            stats: stats.clone(),
        });
        let kept_directories =
            realize_changes_delete(&delete_context, &changes, kept, subtrees.as_ref()).await?;
        lore_debug!("Deleted paths realized");

        if let Some(state_stage) = state_stage.as_ref() {
            stage_destination_ancestors(&repository, state_stage, &changes, is_merge).await?;
        }

        // Channel pipeline for add/modify changes
        let (tx, rx) = mpsc::channel(DEFAULT_WORK_CHANNEL_CAPACITY);

        let discover_stats = stats.clone();
        let discover_changes = changes.clone();
        let producer = lore_spawn!(async move {
            let result =
                sync_discover_modify_add(discover_changes, discover_stats.clone(), tx).await;
            discover_stats
                .discovery
                .complete
                .store(true, Ordering::Relaxed);
            // Send a progress event immediately when discovery finishes,
            // ensuring at least one progress event has discoveryComplete=true
            discover_stats.send_progress();
            result
        });

        let consumer_stats = stats.clone();
        // A view-excluded path is staged into the tree but not written to disk.
        let view_filter = repository.filter.clone();
        let consumer_operation = operation.clone();
        let staging = state_stage.clone();
        let consumer = lore_spawn!(sync_execute_modify_add(
            rx,
            consumer_operation,
            state_stage,
            dry_run,
            merge_current,
            view_filter,
            consumer_stats,
        ));

        let (producer_result, consumer_result) = tokio::join!(producer, consumer);
        lore_debug!("Modified/added paths realized");
        report_kept_renames(&stats.kept_renames);

        drop(ticker);
        stats.send_progress();

        let drained = match staging.as_deref() {
            Some(state) if state.holds_promoted_moves() => Box::pin(
                crate::file::stage::stage_promoted_moves_of_call(&repository, state, None),
            )
            .await
            .forward::<SyncError>("Failed to stage the move of a directory a change is in")
            .map(drop),
            _ => Ok(()),
        };
        let producer_result = producer_result.internal("Recursion task failed")?;
        let consumer_result = consumer_result.internal("Recursion task failed")?;
        consumer_result?;
        producer_result?;
        drained?;

        remove_directories_moved_out_of(&operation, &changes, kept_directories).await;
        if let Some(emptied) = emptied
            && !dry_run
            && !repository.is_layer()
        {
            remove_emptied_directories(
                &operation,
                &repository,
                &changes,
                &RelativePath::new(),
                emptied,
                &stats,
            )
            .await;
        }

        Ok(())
    }
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
        if sorted_holds_below(
            &moves,
            |moved| moved.from.mapping.path.as_str(),
            change.path(),
        ) && (operation.remove(change.path()).await.is_ok()
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
    let ticker = progress_ticker(stats.clone());

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

    drop(ticker);
    stats.send_progress();

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

fn progress_ticker(stats: Arc<SyncRealizeStats>) -> ProgressTicker<SyncRealizeStats> {
    ProgressTicker::start(stats, std::time::Duration::from_millis(100), |stats| {
        stats.send_progress_event();
    })
}

/// What realizing a delete needs beside the change.
#[lore_macro::test_pub]
struct DeleteContext {
    repository: Arc<RepositoryContext>,
    operation: Arc<InstanceOperationImpl>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    /// What a merge's realize reads the working tree against ([`realize_changes`]).
    merge_current: Option<MergeCurrent>,
    stats: Arc<SyncRealizeStats>,
}

/// Where a directory delete that finds the directory holding entries looks for the changes its
/// files may be case variants of; see [`remove_directory_of_case_variants`].
#[derive(Clone, Copy)]
enum CaseVariants<'a> {
    /// The deletes listed, sorted by path.
    Listed(&'a [NodeChange]),
    /// The level of the tree below the subtree change deleting the directory.
    Below {
        subtrees: &'a Arc<state::Subtrees>,
        /// The deletes listed, sorted by path, which hold the files below it listed on their own.
        listed: &'a [NodeChange],
    },
}

/// Realizes the deletes `changes` holds, deepest first, and answers the directory deletes that
/// found their directory holding entries and kept it. A link delete removes its mount but for the
/// files `kept` names below it.
#[lore_macro::test_pub]
async fn realize_changes_delete(
    context: &Arc<DeleteContext>,
    changes: &[NodeChange],
    kept: &[RelativePath],
    subtrees: Option<&Arc<state::Subtrees>>,
) -> Result<Vec<NodeChange>, SyncError> {
    // Sort the changes by path length and iterate in descending order. This will make sure that
    // directories have files deleted first, before attempting to delete the directory.
    // If a directory still have local files which are view/ignore filtered, these directories
    // should not be deleted in order to prevent any potential data loss. Instead the user
    // should manually clean out these directories (or use purge).
    let mut delete_changes: Vec<NodeChange> = changes
        .iter()
        .filter(|change| change.action == change::FileAction::Delete)
        .cloned()
        .collect();
    change::sort_by_path(delete_changes.as_mut_slice());
    let mut move_sources: Vec<&str> = Vec::new();
    if subtrees.is_some() {
        move_sources = changes
            .iter()
            .filter(|change| change.move_source().is_some())
            .map(|change| change.from.mapping.path.as_str())
            .collect();
        move_sources.sort_unstable();
    }
    let kept_lowercase = folded_and_sorted(kept);
    let mut kept = Vec::new();
    for change in delete_changes.iter().rev() {
        let variants = match subtrees.filter(|_| change.flags.is_subtree()) {
            Some(subtrees) => {
                kept.extend(
                    Box::pin(realize_subtree_delete(
                        context,
                        change,
                        subtrees,
                        &delete_changes,
                        &move_sources,
                    ))
                    .await?,
                );
                CaseVariants::Below {
                    subtrees,
                    listed: &delete_changes,
                }
            }
            None => CaseVariants::Listed(&delete_changes),
        };
        if !realize_delete(context, change, variants, &kept_lowercase).await? {
            kept.push(change.clone());
        }
    }

    Ok(kept)
}

/// Deletes what the subtree change `change` stands for below its directory, answering the
/// directories that stayed because they hold entries and hold one of `move_sources`, the sorted
/// paths moves start from. `listed` holds the deletes listed, sorted by path.
///
/// The files go as the walk finds them, up to [`MAX_CONCURRENT_TREE_TASKS`] at once, since none
/// waits on another, and are reported as they go, in no fixed order. The first that fails to go
/// stops the walk at the next file it reaches once its task has ended, and is the answer; the files
/// already deleted stay deleted, as a delete that fails among listed changes leaves the ones before
/// it. The directories, named by path and node alone, go once their files have, deepest first.
/// A directory the working tree no longer holds is walked as well, so the time recorded for each
/// file below it is cleared and each file is reported.
///
/// Its caller boxes it, one allocation per subtree change beside the task and channel its walk
/// already spawns: inline, it would make every realize as large as its walk, whether or not it
/// deletes a whole directory.
#[lore_macro::test_pub]
#[allow(clippy::while_let_loop)] // Keep the read directory out of the await state.
async fn realize_subtree_delete(
    context: &Arc<DeleteContext>,
    change: &NodeChange,
    subtrees: &Arc<state::Subtrees>,
    listed: &[NodeChange],
    move_sources: &[&str],
) -> Result<Vec<NodeChange>, SyncError> {
    let mut expansion = subtrees.expand(change);
    let mut directories = DirectoryLog::default();
    let mut tasks = JoinSet::new();
    let mut failure = None;
    loop {
        let Some(below) = expansion.next().await else {
            break;
        };
        let is_file = below
            .from
            .flags
            .intersects(NodeFlags::File | NodeFlags::Link);
        if is_file {
            while let Some(result) = tasks.try_join_next() {
                keep_first_failure(&mut failure, result);
            }
            while failure.is_none()
                && tasks.len() >= MAX_CONCURRENT_TREE_TASKS
                && let Some(result) = tasks.join_next().await
            {
                keep_first_failure(&mut failure, result);
            }
            if failure.is_some() {
                break;
            }
        }
        context
            .stats
            .complete
            .file_delete_total
            .fetch_add(1, Ordering::Relaxed);
        if is_file {
            lore_spawn!(tasks, delete_below(context.clone(), below));
        } else {
            directories.push(&below.from.mapping.path, below.from.mapping.node);
        }
    }
    let walked = if failure.is_some() {
        expansion.abandon().await;
        Ok(())
    } else {
        expansion
            .finish()
            .await
            .forward::<SyncError>("Failed to walk a deleted directory")
    };
    while let Some(result) = tasks.join_next().await {
        keep_first_failure(&mut failure, result);
    }
    if let Some(err) = failure {
        return Err(err);
    }
    walked?;

    let mut kept = Vec::new();
    let mut reader = directories.into_rev_reader();
    loop {
        let Some((path, node)) = reader.next() else {
            break;
        };
        let directory = NodeChange {
            action: change::FileAction::Delete,
            flags: change::Flags::Subtree,
            from: NodeChangeState {
                mapping: NodeMapping {
                    repository: change.from.mapping.repository.clone(),
                    state: change.from.mapping.state.clone(),
                    path: path.clone(),
                    node,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: crate::lore::Address::default(),
                mode: 0,
            },
            to: change.to.invalid(path),
        };
        if !realize_delete(
            context,
            &directory,
            CaseVariants::Below { subtrees, listed },
            &[],
        )
        .await?
            && sorted_holds_below(move_sources, |path| path, directory.path())
        {
            kept.push(directory);
        }
    }
    Ok(kept)
}

/// Holds on to the first failure of the tasks `result` comes from in `failure`.
fn keep_first_failure(
    failure: &mut Option<SyncError>,
    result: Result<Result<(), SyncError>, tokio::task::JoinError>,
) {
    if let Err(err) = result
        .internal("Deleting a file below a deleted directory")
        .map_err(SyncError::from)
        .flatten()
    {
        *failure = failure.take().or(Some(err));
    }
}

/// Removes the file or link `change` deletes from the working tree, unless under a dry run, and
/// counts and reports it. A link's mount goes but for the files `kept` names below it, lowercase
/// and sorted.
#[lore_macro::test_pub]
async fn remove_file(
    context: &DeleteContext,
    change: &NodeChange,
    kept: &[&str],
) -> Result<(), SyncError> {
    let path = change.path();
    let is_link = change.from.is_link();
    lore_trace!("D {path}");
    if !context.dry_run {
        unlink_retrying(context, change, kept).await?;
        state::file_modified_time_clear(context.repository.clone(), path).await;
    }
    context
        .stats
        .complete
        .file_delete
        .fetch_add(1, Ordering::Relaxed);
    event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(change, 0, !is_link))
        .send();
    Ok(())
}

/// [`remove_file`] for a file or link below a directory deleted as a whole, owning what it removes
/// so that it runs as a task of its own.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn delete_below(
    context: Arc<DeleteContext>,
    change: NodeChange,
) -> impl Future<Output = Result<(), SyncError>> + Send {
    async move { remove_file(&context, &change, &[]).await }
}

/// Logs, as an error, the local work the entry `change` below a deleted directory holds: what the
/// working tree holds at its path and what the revision holds there.
///
/// Runs only where a sync is refused, a cold path its caller boxes.
async fn report_local_work(operation: &InstanceOperationImpl, change: &NodeChange) {
    let path = change.path();
    let revision = if change.from.mapping.node.is_valid_node_id() {
        match change.from.get_node().await {
            Ok(node) => format!(
                ", revision mode {:o} size {} hash {}",
                node.mode, node.size, node.address.hash
            ),
            Err(_) => ", revision node unreadable".to_string(),
        }
    } else {
        String::new()
    };
    match operation.file_info(path).await {
        Ok(info) if info.is_dir() => {
            lore_error!("Deleted directory holds local work in file system: {path}/{revision}");
        }
        Ok(info) => lore_error!(
            "Deleted file is currently modified in file system: {path}, size {} mtime {}{revision}",
            info.size(),
            info.mtime()
        ),
        Err(_) => lore_error!(
            "Deleted file is currently modified in file system: {path}, not readable{revision}"
        ),
    }
}

/// Removes the file `change` deletes, or the mount of the link it deletes but for what the link
/// does not track and the files `kept` names below it ([`remove_link_mount`]), trying again as
/// [`util::fs::file_unlink_retry`] allows.
async fn unlink_retrying(
    context: &DeleteContext,
    change: &NodeChange,
    kept: &[&str],
) -> Result<(), SyncError> {
    let operation = &context.operation;
    let path = change.path();
    let is_link = change.from.is_link();
    let shown = if is_link {
        let (current, staged) = context
            .merge_current
            .as_ref()
            .map(|current| current.roots(&context.repository))
            .unzip();
        Box::pin(mount_shown(
            change,
            current.as_ref(),
            staged.flatten().as_ref(),
        ))
        .await?
        .flatten()
    } else {
        None
    };
    let mut retry = util::fs::file_unlink_retry();
    loop {
        let removed = if is_link {
            Box::pin(remove_link_mount(
                operation,
                &change.from.mapping.repository.filter,
                context.repository.path(),
                path,
                shown.clone(),
                kept,
            ))
            .await
            .map(|left| left.report(path))
        } else {
            operation.remove(path).await.map_err(SyncError::from)
        };
        let Err(err) = removed else {
            return Ok(());
        };
        lore_trace!(
            "Unable to unlink local path {path}: {err} (attempt {} of {})",
            retry.counter() + 1,
            retry.limit()
        );
        if !retry.wait().await {
            return Err(err)
                .forward_with::<SyncError, _>(|| format!("removing {path} from the working tree"));
        }
    }
}

/// The directories below a subtree change, in the order the walk names them, each as its node and
/// the part of its path that differs from the one before it.
///
/// A large tree can hold millions of directories, most holding a file or two, under long shared
/// prefixes, which this holds once rather than in every path. Every [`Self::CHUNK`]th path is held
/// whole, so the log reads back one chunk at a time.
#[lore_macro::test_pub]
#[derive(Default)]
struct DirectoryLog {
    /// Per directory: its node (4 bytes), how many bytes of the previous path it keeps (2), how
    /// many it adds (4), and those bytes.
    bytes: Vec<u8>,
    /// Where each chunk starts in `bytes`.
    chunks: Vec<usize>,
    count: usize,
    previous: Vec<u8>,
}

impl DirectoryLog {
    const CHUNK: usize = 4096;

    #[lore_macro::test_pub]
    fn push(&mut self, path: &RelativePath, node: NodeID) {
        let path = path.as_str().as_bytes();
        let kept = if self.count.is_multiple_of(Self::CHUNK) {
            self.chunks.push(self.bytes.len());
            0
        } else {
            self.previous
                .iter()
                .zip(path)
                .take(usize::from(u16::MAX))
                .take_while(|(a, b)| a == b)
                .count()
        };
        let added = &path[kept..];
        self.bytes.extend_from_slice(&node.to_le_bytes());
        self.bytes.extend_from_slice(&(kept as u16).to_le_bytes());
        self.bytes
            .extend_from_slice(&(added.len() as u32).to_le_bytes());
        self.bytes.extend_from_slice(added);
        self.previous.clear();
        self.previous.extend_from_slice(path);
        self.count += 1;
    }

    /// Reads every directory back, last named first. A walk names a directory before anything
    /// below it, so this names every directory after what it holds.
    #[lore_macro::test_pub]
    fn into_rev_reader(self) -> DirectoryLogReader {
        DirectoryLogReader {
            next_chunk: self.chunks.len(),
            log: self,
            paths: Vec::new(),
            entries: Vec::new(),
        }
    }
}

/// [`DirectoryLog::into_rev_reader`]: decodes a chunk at a time into one buffer of its paths, laid
/// end to end, and the end and node of each, which every chunk reuses.
#[lore_macro::test_pub]
struct DirectoryLogReader {
    log: DirectoryLog,
    /// The chunk after the one decoded.
    next_chunk: usize,
    paths: Vec<u8>,
    /// Per directory of the chunk decoded and not yet read: where its path starts and ends in
    /// `paths`, and its node.
    entries: Vec<(usize, usize, NodeID)>,
}

impl DirectoryLogReader {
    /// The next directory, or `None` once every one has been read.
    #[lore_macro::test_pub]
    fn next(&mut self) -> Option<(RelativePath, NodeID)> {
        if self.entries.is_empty() {
            self.decode_previous_chunk()?;
        }
        let (start, end, node) = self.entries.pop()?;
        let path = String::from_utf8_lossy(&self.paths[start..end]);
        Some((RelativePath::new_from_clean_parts(&path, ""), node))
    }

    /// Decodes the chunk before the one last decoded, or answers `None` where there is none.
    fn decode_previous_chunk(&mut self) -> Option<()> {
        let chunk = self.next_chunk.checked_sub(1)?;
        self.next_chunk = chunk;
        let bytes = &self.log.bytes;
        let start = self.log.chunks[chunk];
        let end = self
            .log
            .chunks
            .get(chunk + 1)
            .copied()
            .unwrap_or(bytes.len());
        let field = |at: usize, width: usize| {
            bytes[at..at + width]
                .iter()
                .rev()
                .fold(0usize, |value, byte| value << 8 | usize::from(*byte))
        };
        self.paths.clear();
        self.entries.clear();
        let mut previous = 0..0;
        let mut at = start;
        while at < end {
            let node = field(at, 4) as NodeID;
            let kept = field(at + 4, 2);
            let added = field(at + 6, 4);
            at += 10;
            let path_start = self.paths.len();
            self.paths
                .extend_from_within(previous.start..previous.start + kept);
            self.paths.extend_from_slice(&bytes[at..at + added]);
            at += added;
            previous = path_start..self.paths.len();
            self.entries.push((previous.start, previous.end, node));
        }
        Some(())
    }
}

/// Deletes what `change` names from the working tree, and from the staged state where the context
/// has one, answering whether it is gone: a directory still holding entries stays, unless what it
/// holds are case variants of files `variants` deletes directly in it. A dry run answers that it
/// is gone. A subtree change names a directory, so its node isn't read. A link's mount goes but
/// for the files `kept` names below it, lowercase and sorted.
async fn realize_delete(
    context: &DeleteContext,
    change: &NodeChange,
    variants: CaseVariants<'_>,
    kept: &[&str],
) -> Result<bool, SyncError> {
    let DeleteContext {
        repository,
        operation,
        state_stage,
        dry_run,
        merge_current,
        stats,
    } = context;
    let (dry_run, is_merge) = (*dry_run, merge_current.is_some());
    let state_from = change.from.mapping.state.clone();

    // TODO(mjansson): File system virtualization
    /*
    if (repository->virtualized) {
        err = repository->virtualization->delete_node(repository, change->path);
        return err;
    }
    */

    let change_path = change.path().clone();

    let is_link = change.from.is_link();

    let is_file = if is_link || change.flags.is_subtree() {
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

    let mut deleted = true;
    if is_link || is_file {
        remove_file(context, change, kept).await?;
    } else {
        lore_trace!("D {}", change.path());
        if !dry_run {
            if operation.remove(&change_path).await.is_err() {
                // Type-erased: a caller's `Send` check of the comparisons below overflows the
                // recursion limit.
                let variants_removed: crate::BoxFuture<'_, bool> =
                    match variants {
                        CaseVariants::Listed(deletes) => Box::pin(
                            remove_directory_of_case_variants(operation, &change_path, deletes),
                        ),
                        CaseVariants::Below { subtrees, listed } => {
                            Box::pin(remove_subtree_directory_of_case_variants(
                                operation, change, subtrees, listed,
                            ))
                        }
                    };
                deleted = variants_removed.await;
            }
            state::file_modified_time_clear(repository.clone(), change.path()).await;
        }
        stats.complete.file_delete.fetch_add(1, Ordering::Relaxed);
        if deleted {
            event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(
                change, 0, false,
            ))
            .send();
        }
    }

    if let Some(state_stage) = state_stage {
        stage_realized_delete(
            &change.from.mapping.repository,
            state_stage,
            change,
            is_merge,
        )
        .await?;
    }

    Ok(deleted)
}

/// Which directories a realize removes once its deletes and moves leave them empty. Each is one the
/// view excludes, as one it keeps stays whatever it holds.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
pub(crate) enum EmptiedDirectories {
    /// Those the view excludes, where it re-includes below a directory it excludes.
    Excluded,
    /// Those [`Excluded`](Self::Excluded) takes but the ones above a delete or a move's source in
    /// a linked tree, the changes carrying the tree to another revision under one view, from a walk
    /// marking a delete beside what the view places
    /// ([`DiffFlags::PlacesDirectories`](state::DiffFlags::PlacesDirectories)).
    Unplaced,
    /// Every one the view excludes, whether or not it re-includes below it: the realize carries the
    /// tree to another view, or resets it to the target revision from the working tree.
    Target,
}

/// Removes each directory below `floor` above a delete or a move's source in `changes`
/// ([`emptied_directory_candidates`]) that is empty and that the view excludes, deepest first,
/// reporting each one removed. Each removal counts as a delete in `stats`, done and in total, and a
/// progress event follows the last.
///
/// A directory stays where something stays below it ([`StayingDirectories`]): a write in
/// `changes` the view keeps, a file the walk found beside a delete, a directory the view keeps, or
/// one still holding anything, a local file included, which an `rmdir` refusing it finds. One the
/// disk doesn't hold keeps none above it. Under [`EmptiedDirectories::Unplaced`] a directory above
/// a delete in a linked tree stays, as the link's mount does. `floor` is the root, or the mount of
/// a layer, which stands whatever the view and is realized under the layer's context, so the
/// layer's sync removes the directories below it rather than the realize. A realize under a link's
/// context keeps every directory. Under a view without a rule re-including below a directory it
/// excludes, no excluded directory is on disk to remove but under [`EmptiedDirectories::Target`],
/// and with no view every directory stays. Ancestors a change shares with the one before it are
/// gathered once, which sorted changes make the most of.
#[lore_macro::test_pub]
pub(crate) async fn remove_emptied_directories(
    operation: &InstanceOperationImpl,
    repository: &RepositoryContext,
    changes: &[NodeChange],
    floor: &RelativePath,
    emptied: &EmptiedDirectories,
    stats: &Arc<SyncRealizeStats>,
) {
    let filter = &repository.filter;
    let nothing_to_remove = match emptied {
        EmptiedDirectories::Target => filter.view.is_empty(),
        _ => !filter.view.reincludes(),
    };
    if repository.is_link() || nothing_to_remove {
        return;
    }
    let mut staying = emptied_directory_candidates(changes, floor);
    if staying.directories.is_empty() {
        return;
    }
    staying.mark_writes(filter, changes, emptied);
    let mut removed = false;
    let mut next = 0;
    loop {
        let index = next;
        let Some((directory, stays)) = staying.directories.get(index) else {
            break;
        };
        next += 1;
        if *stays {
            continue;
        }
        if !filter.excludes(directory, true, FilterMode::View) {
            staying.mark(index);
            continue;
        }
        match operation.remove_empty_directory(directory).await {
            Ok(()) => {}
            Err(error) if error.is_file_not_found() => continue,
            Err(_) => {
                staying.mark(index);
                continue;
            }
        }
        stats
            .complete
            .file_delete_total
            .fetch_add(1, Ordering::Relaxed);
        stats.complete.file_delete.fetch_add(1, Ordering::Relaxed);
        event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData {
            path: LoreString::from(directory),
            size: 0,
            action: change::FileAction::Delete.into(),
            flag_file: 0,
        })
        .send();
        removed = true;
    }
    if removed {
        stats.send_progress();
    }
}

/// The directories [`remove_emptied_directories`] may remove, deepest first, each marked where it
/// stays on disk. A directory staying keeps every directory above it, so marking one marks its
/// ancestors among them, and stops at the first already marked, whose ancestors are.
struct StayingDirectories {
    /// Each directory with whether it stays, sorted by path, descending, which puts a directory
    /// after every directory below it.
    directories: Vec<(RelativePath, bool)>,
}

impl StayingDirectories {
    /// Whether the directory at `index` stays.
    fn stays(&self, index: usize) -> bool {
        self.directories[index].1
    }

    /// The index of the directory at `path`, if it is one of them.
    fn find(&self, path: &str) -> Option<usize> {
        position(&self.directories, path)
    }

    /// The index of the nearest of them at or above `path`.
    fn nearest_at_or_above(&self, path: &str) -> Option<usize> {
        let mut path = path;
        loop {
            if let Some(index) = self.find(path) {
                return Some(index);
            }
            path = path.rsplit_once('/')?.0;
        }
    }

    /// Marks the directory at `index` and each one above it, which sort after it.
    fn mark(&mut self, index: usize) {
        let (marked, above) = self.directories.split_at_mut(index + 1);
        let (directory, stays) = &mut marked[index];
        *stays = true;
        let mut path = directory.as_str();
        while let Some((parent, _)) = path.rsplit_once('/') {
            if let Some(index) = position(above, parent) {
                if above[index].1 {
                    return;
                }
                above[index].1 = true;
            }
            path = parent;
        }
    }

    /// Marks each directory at or above a write in `changes` the view keeps ([`keeps_write`]),
    /// and each directory above a delete or a move's source the walk found something the view
    /// places beside ([`change::Flags::KeptBeside`]). Under
    /// [`EmptiedDirectories::Unplaced`] it marks each directory above a delete or a move's source
    /// in a linked tree as well, since the walk names no link's mount. A write is asked about only
    /// below a directory not yet marked, and changes in one directory, adjacent in sorted changes,
    /// look its candidate up once.
    fn mark_writes(
        &mut self,
        filter: &Filter,
        changes: &[NodeChange],
        emptied: &EmptiedDirectories,
    ) {
        let mut previous: Option<(&str, Option<usize>)> = None;
        for change in changes {
            let in_link = matches!(emptied, EmptiedDirectories::Unplaced)
                && change.from.mapping.repository.is_link();
            if (in_link || change.flags.contains(change::Flags::KeptBeside))
                && let Some((parent, _)) = change
                    .move_source()
                    .and_then(|source| source.as_str().rsplit_once('/'))
                && let Some(index) = self.nearest_at_or_above(parent)
                && !self.stays(index)
            {
                self.mark(index);
            }
            let path = change.path().as_str();
            let (path, places) = if change.action == change::FileAction::Delete {
                let stays = in_link || change.flags.contains(change::Flags::KeptBeside);
                match path.rsplit_once('/') {
                    Some((parent, _)) if stays => (parent, None),
                    _ => continue,
                }
            } else {
                (path, Some(change))
            };
            let nearest = self.find(path).or_else(|| {
                let parent = path.rsplit_once('/')?.0;
                match previous {
                    Some((directory, nearest)) if directory == parent => nearest,
                    _ => {
                        let nearest = self.nearest_at_or_above(parent);
                        previous = Some((parent, nearest));
                        nearest
                    }
                }
            });
            let Some(index) = nearest.filter(|&index| !self.stays(index)) else {
                continue;
            };
            if let Some(write) = places
                && !keeps_write(filter, write)
            {
                continue;
            }
            self.mark(index);
        }
    }
}

/// Whether `filter`'s view keeps `write`, a change deleting nothing, at its path: a file, a link
/// or a directory it keeps, which the realize places on disk whatever is written below it.
fn keeps_write(filter: &Filter, write: &NodeChange) -> bool {
    let path = write.path();
    match write.to.flags.node_type() {
        LoreNodeType::File => !filter.excludes(path, false, FilterMode::View),
        LoreNodeType::Link => !filter.excludes_tree(path, true, FilterMode::View),
        LoreNodeType::Directory => !filter.exclusion_states(path).excluded(FilterMode::View),
    }
}

/// The index of the directory at `path` in `directories`, sorted by path, descending.
fn position(directories: &[(RelativePath, bool)], path: &str) -> Option<usize> {
    directories
        .binary_search_by(|(directory, _)| path.cmp(directory.as_str()))
        .ok()
}

/// Each directory below `floor` above a delete or a move's source in `changes`, deepest first,
/// none of them marked yet.
fn emptied_directory_candidates(
    changes: &[NodeChange],
    floor: &RelativePath,
) -> StayingDirectories {
    let mut directories = Vec::new();
    let mut previous = "";
    for change in changes {
        let vacated = match change.action {
            change::FileAction::Delete => change.path(),
            _ => match change.move_source() {
                Some(source) => source,
                None => continue,
            },
        };
        let mut directory = vacated.parent_path();
        while directory
            .below(floor)
            .is_some_and(|below| !below.is_empty())
            && !directory.covers(&previous)
        {
            let parent = directory.parent_path();
            directories.push((directory, false));
            directory = parent;
        }
        previous = vacated.as_str();
    }
    directories.sort_unstable_by(|(left, _), (right, _)| right.as_str().cmp(left.as_str()));
    directories.dedup_by(|(left, _), (right, _)| left.as_str() == right.as_str());
    StayingDirectories { directories }
}

struct SyncWorkItem {
    change: NodeChange,
    node: Node,
}

/// Sends each change that deletes nothing to `tx`, with the node its `to` side names given the
/// mode `to` states.
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
        let mut node = block.node(Node::index(change.to.mapping.node));
        node.mode = change.to.mode;

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

/// Realizes each change `rx` hands over ([`realize_change_modify_add`]), one after another, and
/// answers the first failure once every write it spawned has ended.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn sync_execute_modify_add(
    mut rx: mpsc::Receiver<SyncWorkItem>,
    operation: Arc<InstanceOperationImpl>,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    merge_current: Option<MergeCurrent>,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
) -> impl Future<Output = Result<(), SyncError>> {
    const MAX_TASK_COUNT: usize = 10000;
    let mut moves = RealizeMoves {
        merge_current,
        listings: HeldSpellings::default(),
        directories: Vec::new(),
    };
    async move {
        let mut tasks = JoinSet::new();
        // Finish independent changes even when one file cannot be written. The first
        // failure remains the terminal result; warnings identify every failed attempt.
        let mut sync_error = None;

        while let Some(item) = rx.recv().await {
            let result = realize_change_modify_add(
                &mut tasks,
                &mut moves,
                operation.clone(),
                item.change,
                item.node,
                state_stage.clone(),
                dry_run,
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

/// Whether `change` leaves its file in place with the content it held, and `measured`, the node an
/// unmodified working file matches, holds that content: the working file then needs only the
/// change's executable bit. A move is left out, as realizing it renames the file and can remove
/// what stands at its destination.
fn holds_the_content(change: &NodeChange, measured: Option<&MeasuredNode>) -> bool {
    change.action == change::FileAction::Keep
        && change.from.address.hash == change.to.address.hash
        && measured.is_some_and(|measured| measured.node.address.hash == change.to.address.hash)
}

/// Whether the destination holds the content the change ends at, so that writing it from the
/// immutable store would replace it with the same bytes: where the verify found the file holding
/// it ([`change::Flags::HoldsContent`]), or where the rename a move was realized by carried it.
///
/// For the rename, two things have to hold. `renamed` reports a rename that completed, which is
/// what says the source was there to be carried: a sparse working tree holds nothing at a source
/// its view excludes, and a tree the move has already been applied to holds nothing at it either.
/// And the two sides address the same content, since a move that rewrites the file carries the old
/// bytes to the destination and the new ones are only in the store.
///
/// The rename answers the first for itself, so nothing here infers from a view whether the file was
/// on disk. A view could not answer it: the source is materialized under the view the tree is carried
/// *from* while realize holds the one it is carried *to*, and neither tells a source that was never
/// materialized from one an earlier pass already carried away.
fn content_is_in_place(change: &NodeChange, renamed: bool) -> bool {
    (renamed && change.from.address.hash == change.to.address.hash) || change.flags.holds_content()
}

/// `mode`, the mode of the node `change` writes, with the executable bit the working file carries,
/// where [`change::Flags::LocalMode`] states that the bit is the working tree's own. Writing a node
/// applies the mode it holds, which would revert the one local change a chmod is.
///
/// The file stands at the path the change names, or at the source a move carries it from, as
/// `moves` reads it ([`RealizeMoves::moved_source`]): a move yet to be realized holds it at the
/// source, and one realized over its own result at the destination. Only a change holding a local
/// bit pays for the read.
async fn local_mode(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    moves: &RealizeMoves,
    mode: u16,
) -> Result<u16, SyncError> {
    if !change.flags.is_local_mode() {
        return Ok(mode);
    }

    let held = {
        let info = operation.file_info(change.path()).await?;
        info.is_file().then(|| info.mode(mode))
    };
    let held = match (held, change.move_source()) {
        (None, Some(source)) => {
            let moved = moves.moved_source(source);
            let info = operation
                .file_info(moved.as_ref().unwrap_or(source))
                .await?;
            info.is_file().then(|| info.mode(mode))
        }
        (held, _) => held,
    };
    Ok(held.unwrap_or(mode))
}

/// Rewrites a mount whose link node moved to another subtree of the repository it points at.
///
/// The diff reports such a move as the link node alone, so the content the mount exposes has to be
/// carried here. For a merge, whose change starts from its base, `merge_current` holds what the
/// mount shows ([`mount_shown`]).
async fn realize_moved_mount(
    operation: &Arc<InstanceOperationImpl>,
    change: &NodeChange,
    node: &Node,
    merge_current: Option<&MergeCurrent>,
) -> Result<(), SyncError> {
    if !change.from.is_link() {
        return Ok(());
    }

    let repository = change.to.mapping.repository.clone();
    // Another repository's subtree shares nothing with the one it replaces, so it is cloned afresh.
    if change.from.address.context != node.address.context {
        let path = change.path();
        let (current, staged) = merge_current
            .map(|current| current.roots(&change.from.mapping.repository))
            .unzip();
        let shown = mount_shown(change, current.as_ref(), staged.flatten().as_ref())
            .await?
            .flatten();
        let link = Box::pin(open_pinned_link(&repository, node, path)).await?;
        remove_link_mount(
            operation,
            &change.from.mapping.repository.filter,
            repository.path(),
            path,
            shown,
            &[],
        )
        .await
        .forward::<SyncError>("Failed removing the subtree the link exposed")?
        .report(path);
        operation
            .create_dir_all(path)
            .await
            .forward::<SyncError>("Failed recreating the link directory")?;
        return Box::pin(clone_opened_link(link, operation, node, path)).await;
    }

    let from_node = change
        .from
        .mapping
        .state
        .node(repository.clone(), change.from.mapping.node)
        .await
        .forward::<SyncError>("Failed reading the link node the mount stands at")?;
    if from_node.mounts_same_source(node) && change.from.address == node.address {
        return Ok(());
    }

    let link_context = repository
        .to_link_context(node.address.context.into())
        .await;
    crate::link::realize_link_pin_change_boxed(
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
        false,
    )
    .await
    .forward::<SyncError>("Failed realizing the subtree a link exposes")
}

/// Recovers from the refusal of a move's rename, `refused`: removes what `to` holds, for the node
/// to be written there, unless `to` names the entry `from` does, as another spelling of its name
/// does on a file system that folds case, or cannot be told apart from it. Such a move leaves the
/// source where it stands, the node being written over the destination as it is, and is counted in
/// `kept` for the warning the realize ends with.
async fn recover_refused_move(
    operation: &InstanceOperationImpl,
    from: &RelativePath,
    to: &RelativePath,
    refused: FsError,
    kept: &KeptRenames,
) -> Result<(), SyncError> {
    let identity = operation.names_one_entry(from, to).await;
    if matches!(identity, Ok(Some(false) | None)) {
        lore_trace!("Failed renaming move node, fall back to deleting and recreating: {refused}");
        return operation
            .remove_recursive(to)
            .await
            .forward::<SyncError>("removing the destination of a refused move");
    }
    kept.count.fetch_add(1, Ordering::Relaxed);
    kept.first.get_or_init(|| {
        let reason = match identity {
            Err(err) => {
                format!(
                    "{refused}, and the destination cannot be told apart from the source: {err}"
                )
            }
            Ok(_) => refused.to_string(),
        };
        KeptRename {
            from: from.clone(),
            to: to.clone(),
            reason,
        }
    });
    Ok(())
}

/// Renames `from`, the source of `change`, a move, to `to`, from the path the directory moves
/// realized before it carried it to ([`RealizeMoves::moved_source`]), and adds a directory it moves
/// to them. The rename creates the directory `to` lands in where it is missing
/// ([`InstanceOperationImpl::rename_creating_parent`]): a directory the realize adds stands on disk
/// only as the parent of what lands in it, unless the realize creates it ahead of what it holds
/// ([`creates_directory_ahead`]). Where the rename fails, it takes the entry the working
/// tree holds the source in under another spelling, where `view_filter` holds the source
/// ([`rename_held_spelling`]), and otherwise recovers as [`recover_refused_move`] describes,
/// counting a rename left undone in `kept`.
/// Answers whether a rename carried the source to `to`.
///
/// A function of its own, so that the source is not reserved in every state of
/// [`realize_change_modify_add`].
async fn rename_move_source(
    operation: &InstanceOperationImpl,
    change: &NodeChange,
    moves: &mut RealizeMoves,
    from: &RelativePath,
    to: &RelativePath,
    view_filter: &crate::filter::Filter,
    kept: &KeptRenames,
) -> Result<bool, SyncError> {
    let moved = moves.moved_source(from);
    let source = moved.as_ref().unwrap_or(from);
    let renamed = match operation.rename_creating_parent(source, to).await {
        Ok(()) => true,
        Err(refused) => {
            let respelled = !view_filter.excludes(
                from,
                !change.from.flags.contains(NodeFlags::File),
                FilterMode::View,
            ) && rename_held_spelling(operation, change, moves, source, to).await;
            if !respelled {
                recover_refused_move(operation, source, to, refused, kept).await?;
            }
            respelled
        }
    };
    if renamed && change.to.flags.is_directory() {
        moves.directories.push((source.clone(), to.clone()));
    }
    Ok(renamed)
}

/// Warns once for the renames left undone, `kept`, with how many and the first, which is all
/// reported of them.
pub(crate) fn report_kept_renames(kept: &KeptRenames) {
    if let Some(KeptRename { from, to, reason }) = kept.first.get() {
        let count = kept.count.load(Ordering::Relaxed);
        lore_warn!(
            "Left {count} {} undone, which leaves the source in place; the first is {from} to \
             {to}: {reason}",
            if count == 1 { "rename" } else { "renames" }
        );
    }
}

/// Whether a write of the working tree creates the directory `node_id` of `state`, at `path` with
/// the view verdict `states`, ahead of what it holds: `filter`'s view keeps the directory
/// ([`FilterStates::creates_directory`]) and none of its entries is a file or directory the view
/// keeps. One with such an entry is created as a parent: of the first file written below it, or of
/// the first directory below it created ahead. Without a view, a directory is created ahead of what
/// it holds only where it is empty.
///
/// Where the view excludes everything below the directory ([`Filter::view_hides_below`]), its
/// entries are not read.
#[lore_macro::test_pub]
pub(crate) async fn creates_directory_ahead(
    filter: &Filter,
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    node: &Node,
    path: &RelativePath,
    states: FilterStates,
) -> Result<bool, state::StateError> {
    if !states.creates_directory() || node.child().is_none() {
        return Ok(states.creates_directory());
    }
    if filter.view.is_empty() {
        return Ok(false);
    }
    if filter.view_hides_below(states, path) {
        return Ok(true);
    }
    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id).await?;
    let mut child_path = path.push_into_buf("");
    while let Some((_, child, name)) = children.next().await? {
        if child.is_link() {
            continue;
        }
        child_path.push(name);
        if keeps_entry(filter, states, &child_path, &child) {
            return Ok(false);
        }
        child_path.pop();
    }
    Ok(true)
}

/// Whether `filter`'s view keeps the file or directory `child` at `path` below a directory with the
/// view verdict `states`, by the verdict on `child` itself: a directory it keeps is one it creates
/// ([`FilterStates::creates_directory`]).
pub(crate) fn keeps_entry(
    filter: &Filter,
    states: FilterStates,
    path: &impl FilterPath,
    child: &Node,
) -> bool {
    let (_, reason) = filter.child_exclude_reason(
        states,
        path,
        child.is_directory(),
        FilterMode::View,
        Scope::Node,
    );
    reason.is_none() && (child.is_file() || child.is_directory())
}

/// Realizes on disk a node a change adds, modifies or moves, and stages it in `state_stage` when
/// given.
///
/// A move is renamed as [`rename_move_source`] describes, against what `moves` holds for the
/// realize. The change is staged as [`stage_realized_change`] describes.
///
/// Cloning an added link is boxed. Only a link reaches it, and inline it would make the future of
/// every change realized as large as its own.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn realize_change_modify_add<'a>(
    tasks: &'a mut JoinSet<Result<(), SyncError>>,
    moves: &'a mut RealizeMoves,
    operation: Arc<InstanceOperationImpl>,
    change: NodeChange,
    node: Node,
    state_stage: Option<Arc<State>>,
    dry_run: bool,
    view_filter: Arc<crate::filter::Filter>,
    stats: Arc<SyncRealizeStats>,
) -> impl Future<Output = Result<(), SyncError>> + 'a {
    async move {
        // During a merge this is the filter-free walk context, not the instance's.
        // The instance's view arrives separately as `view_filter`. The staging
        // below needs the first, the disk writes need the second.
        let repository = change.to.mapping.repository.clone();
        let size = node.size;
        let is_file = node.is_file();
        let path = change.path();

        // Only on-disk work honours the view. This gates directory creation, link cloning and the
        // file write. A link's content is written only here, so a link is kept wherever the view
        // could re-include something below it. The move rename below is not gated by the view,
        // because it repositions a path an earlier in-view realize may have written. Nothing of a
        // link is written to a provider holding its state, which serves the link from its node:
        // the link is placed there, and taken away from where it moves from.
        let held_link = node.is_link() && operation.holds_state();
        if held_link && !dry_run {
            if let Some(from_path) = change.move_source() {
                operation.place_link(from_path, None, false).await?;
            }
            operation
                .place_link(path, Some(&node), execution_context().globals().force())
                .await?;
        }
        let write_to_disk = if node.is_link() {
            !held_link && !view_filter.excludes_tree(path, true, FilterMode::View)
        } else if node.is_directory() {
            creates_directory_ahead(
                &view_filter,
                &change.to.mapping.state,
                &change.to.mapping.repository,
                change.to.mapping.node,
                &node,
                path,
                view_filter.exclusion_states(path),
            )
            .await
            .forward::<SyncError>("Reading the entries of a directory")?
        } else {
            !view_filter.excludes(path, false, FilterMode::View)
        };

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
        let realized = Node {
            mode: local_mode(&operation, &change, moves, node.mode).await?,
            ..node
        };

        let renamed = if !dry_run
            && !held_link
            && change.action == change::FileAction::Move
            && let Some(from_path) = change.move_source()
        {
            rename_move_source(
                &operation,
                &change,
                moves,
                from_path,
                &to_path,
                &view_filter,
                &stats.kept_renames,
            )
            .await?
        } else {
            false
        };

        if (node.is_directory() || node.is_link()) && write_to_disk {
            // When a link is added, the linked contents are not marked
            // for add. That's why we can just clone the linked files
            if node.is_link() && change.action == change::FileAction::Add {
                Box::pin(realize_added_link(
                    &repository,
                    &operation,
                    &node,
                    path,
                    dry_run,
                ))
                .await?;
            } else {
                if !dry_run {
                    create_directory(&operation, path).await?;
                }
                // Boxed: a mount moves rarely, and its realize would grow every change's future.
                if node.is_link() && !dry_run {
                    Box::pin(realize_moved_mount(
                        &operation,
                        &change,
                        &node,
                        moves.merge_current.as_ref(),
                    ))
                    .await?;
                }
            }
        } else if node.is_file() && !dry_run && write_to_disk {
            if content_is_in_place(&change, renamed) {
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

        if let Some(state_stage) = &state_stage {
            stage_realized_change(
                &repository,
                state_stage,
                &change,
                node,
                moves.merge_current.is_some(),
            )
            .await?;
        }

        Ok(())
    }
}

/// Creates the directory at `path`, where nothing but a directory stands.
async fn create_directory(
    operation: &Arc<InstanceOperationImpl>,
    path: &RelativePath,
) -> Result<(), SyncError> {
    if operation.create_dir_all(path).await.is_err()
        && operation
            .file_info(path)
            .await
            .is_ok_and(|info| !info.is_dir())
    {
        return Err(SyncError::internal(format!(
            "Failed to create directory {path}"
        )));
    }
    Ok(())
}

/// Clones the link `node` adds at `path` into a mount created there, once the repository it pins
/// is reached and its pinned revision read, so a link that can't be reached leaves no mount behind.
async fn realize_added_link(
    repository: &Arc<RepositoryContext>,
    operation: &Arc<InstanceOperationImpl>,
    node: &Node,
    path: &RelativePath,
    dry_run: bool,
) -> Result<(), SyncError> {
    let link = open_pinned_link(repository, node, path).await?;
    if !dry_run {
        create_directory(operation, path).await?;
    }
    clone_opened_link(link, operation, node, path).await
}

/// The repository a link pins, connected to and read at the pinned revision.
struct OpenedLink {
    context: Arc<RepositoryContext>,
    storage: Arc<lore_transport::StorageSession>,
    state: Arc<State>,
}

/// Connects to the repository the link `node` at `path` pins and reads the revision it pins.
async fn open_pinned_link(
    repository: &Arc<RepositoryContext>,
    node: &Node,
    path: &RelativePath,
) -> Result<OpenedLink, SyncError> {
    let link_revision = node.address.hash;
    let context = repository
        .to_link_context(node.address.context.into())
        .await;
    let opened = async {
        let link_remote = context
            .remote()
            .await
            .forward::<SyncError>("Failed to connect to link remote")?;
        let correlation_id = execution_context().globals().correlation_id.to_string();
        let storage = link_remote
            .session(context.id, &correlation_id)
            .await
            .forward::<SyncError>("Failed to connect to link remote")?;
        let state = State::deserialize(context.clone(), link_revision)
            .await
            .forward_with::<SyncError, _>(|| {
                format!("Failed to deserialize state {link_revision}")
            })?;
        Ok::<_, SyncError>((storage, state))
    };
    let (storage, state) = opened
        .await
        .forward_with::<SyncError, _>(|| format!("Failed to fetch the link at {path}"))?;
    Ok(OpenedLink {
        context,
        storage,
        state,
    })
}

/// Clones the content of the link `node` at `path` into the working tree.
async fn clone_opened_link(
    link: OpenedLink,
    operation: &Arc<InstanceOperationImpl>,
    node: &Node,
    path: &RelativePath,
) -> Result<(), SyncError> {
    let OpenedLink {
        context,
        storage,
        state,
    } = link;
    let clone_ctx = CloneContext {
        repository: context.clone(),
        state,
        operation: operation.clone(),
        options: Arc::default(),
        stats: Arc::default(),
        modified_times: operation.modified_times(),
    };
    let clone_states = context.filter.mount_states(path);
    clone::clone_node(clone_ctx, storage, path.clone(), node.child, clone_states)
        .await
        .forward_with::<SyncError, _>(|| format!("Failed to fetch the link at {path}"))
}

/// Stages a realized move by relinking `from_node`, the staged node at `from_path`, to `to_path`,
/// keeping its identity; marks it staged as a move and stages the directory it left, where a move
/// stages one ([`crate::node::Node::stages_when_left`]). Answers with the node's new parent.
///
/// A function of its own because its locals live across several awaits: kept in
/// [`realize_change_modify_add`] they would take space in the future of every change realized.
#[lore_macro::test_pub]
pub(crate) async fn relink_moved_node(
    state_stage: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    from_path: &RelativePath,
    from_node: NodeID,
    to_path: &RelativePath,
    is_merge: bool,
) -> Result<NodeID, SyncError> {
    let new_parent_id = state_stage
        .locate_node(repository.clone(), to_path.parent().unwrap_or(""))
        .await
        .forward::<SyncError>("Failed to stage change")?
        .identity()
        .node;

    let from_name = from_path.name();
    let to_name = to_path.name();
    let name = if from_name != to_name {
        NameChange::Rename(to_name)
    } else {
        NameChange::Keep
    };
    let mut mark_flags = NodeFlags::StagedMove;
    if is_merge {
        mark_flags |= NodeFlags::StagedMerge;
    }
    let own = Mark::staged(mark_flags, true);
    let left = state_stage
        .node_move(repository.clone(), from_node, new_parent_id, name, own)
        .await
        .forward::<SyncError>("Failed to stage change")?;
    state_stage
        .node_mark_destination(repository, new_parent_id, own)
        .await
        .forward::<SyncError>("Failed to stage change")?;
    if left != new_parent_id
        && left != ROOT_NODE
        && state_stage
            .node(repository.clone(), left)
            .await
            .forward::<SyncError>("Failed to stage change")?
            .stages_when_left()
    {
        state_stage
            .node_mark_left(
                repository,
                (left, new_parent_id),
                Mark::staged(NodeFlags::Staged, false),
            )
            .await
            .forward::<SyncError>("Failed to stage change")?;
    }

    Ok(new_parent_id)
}

/// Stages in `state_stage` the directories above the destination of each add, move and copy of
/// `changes`, as a merge stages them where `is_merge` says, so that the change finds its parent
/// where the staged tree lacks it, as when the current branch deleted it. A modify below a deleted
/// directory pairs with the delete as a conflict and needs none.
pub(crate) async fn stage_destination_ancestors(
    repository: &Arc<RepositoryContext>,
    state_stage: &Arc<State>,
    changes: &[NodeChange],
    is_merge: bool,
) -> Result<(), SyncError> {
    let parent_paths = changes
        .iter()
        .filter(|change| {
            matches!(
                change.action,
                change::FileAction::Add | change::FileAction::Move | change::FileAction::Copy
            ) && change.path().parent().is_some()
        })
        .map(|change| change.path().parent_path())
        .collect();
    let flags = if is_merge {
        NodeFlags::StagedMerge.bits()
    } else {
        NodeFlags::NoFlags.bits()
    };
    for stage_path in expand_path_ancestors(parent_paths) {
        stage::stage_single_node(
            repository.clone(),
            state_stage.clone(),
            stage_path,
            Node {
                flags,
                ..Default::default()
            },
            Arc::default(),
            None,
            FilterMode::empty(),
        )
        .await
        .forward::<SyncError>("Failed to stage change")?;
    }
    Ok(())
}

/// Stages in `state_stage` the delete `change` realizes, as a merge stages it where `is_merge`
/// says: the node at its path, where the staged tree holds one, staged for deletion, and a link
/// taken out of the link registry.
pub(crate) async fn stage_realized_delete(
    repository: &Arc<RepositoryContext>,
    state_stage: &Arc<State>,
    change: &NodeChange,
    is_merge: bool,
) -> Result<(), SyncError> {
    let node_link = match state_stage
        .locate_node(repository.clone(), change.path().as_str())
        .await
    {
        Ok(found) => found.identity(),
        Err(e) if e.is_node_not_found() => return Ok(()),
        Err(err) => return Err(err).forward::<SyncError>("Failed to stage change"),
    };
    if !node_link.is_valid() {
        return Ok(());
    }

    stage::stage_delete(
        repository.clone(),
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

    if change.from.is_link() {
        state_stage
            .link_remove_if_present(
                repository.clone(),
                change.from.address.context.into(),
                node_link.node,
            )
            .await
            .forward::<SyncError>("Failed to remove link from the link registry")?;
    }
    Ok(())
}

/// Stages in `state_stage` the add, modify or move `change` realizes, `node` being the node its
/// `to` side names with the mode it states, as a merge stages it where `is_merge` says.
///
/// A move relinks the staged node ([`relink_moved_node`]), which keeps its identity and the nodes
/// below it. A moved file or link the move modifies then takes `node`'s content, and one it does
/// not keeps the staged node's, a file taking the change's executable bit
/// ([`carry_staged_mode`]). Any other change stages `node` at the change's path, a merge's with
/// `StagedMerge` and a directory a merge adds with `StagedAdd`.
///
/// Recording a staged link in the registry is boxed: only a link reaches it, and inline it would
/// make the future of every change staged as large as its own.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub(crate) fn stage_realized_change<'a>(
    repository: &'a Arc<RepositoryContext>,
    state_stage: &'a Arc<State>,
    change: &'a NodeChange,
    mut node: Node,
    is_merge: bool,
) -> impl Future<Output = Result<(), SyncError>> + 'a {
    async move {
        let path = change.path();
        if change.action == change::FileAction::Move
            && let Some(from_path) = change.move_source()
        {
            let from_node = state_stage
                .locate_node(repository.clone(), from_path.as_str())
                .await
                .forward::<SyncError>("Failed to stage change")?
                .identity()
                .node;
            relink_moved_node(
                state_stage,
                repository,
                from_path,
                from_node,
                path,
                is_merge,
            )
            .await?;
            if node.is_directory() {
                return Ok(());
            }
            if !change.flags.contains(change::Flags::Modify) {
                if node.is_file() {
                    carry_staged_mode(state_stage, repository, from_node, node.mode)
                        .await
                        .forward::<SyncError>("Failed to stage change")?;
                }
                return Ok(());
            }
        }

        if is_merge {
            node.flags |= NodeFlags::StagedMerge;
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
            path.clone(),
            node,
            Arc::new(stage::StageStats::default()),
            None, // TODO(vri): UCS-18008 - Investigate link tracking for sync/realize_changes
            FilterMode::View,
        )
        .await
        .forward::<SyncError>("Failed to stage change")?;

        if node.is_link() && staged_node.node.is_valid_node_id() {
            Box::pin(stage_link_registry_entry(
                repository,
                state_stage,
                change,
                node,
                staged_node.node,
            ))
            .await?;
        }
        Ok(())
    }
}

/// Gives the staged file `node_id` the executable bit `mode`, where it holds another.
pub(crate) async fn carry_staged_mode(
    state_stage: &State,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    mode: u16,
) -> Result<(), state::StateError> {
    let staged = state_stage.node(repository.clone(), node_id).await?;
    if !util::fs::mode_changed(staged.mode, mode) {
        return Ok(());
    }
    state_stage
        .node_rewrite(
            repository,
            node_id,
            &state::Content {
                mode,
                size: staged.size,
                address: staged.address,
            },
            state::Rewrite::Plain,
        )
        .await?;
    Ok(())
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

/// Merges the file of the pair of changes at `index` in `merges`, which both sides changed, giving a
/// file both leave the executable bit [`change::merged_mode`] gives it, whatever the content's
/// merge leaves.
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
                if !dry_run && let Some(mode) = change::merged_mode(change_from, change_to) {
                    carry_file_mode(&operation, change_to.path(), change_to.to.mode, mode).await?;
                }
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
                        .get_node()
                        .await
                        .forward::<SyncError>("Failed to resolve node in merge revisions")?
                } else if change_from.to.mapping.node.is_valid_node_id() {
                    change_from
                        .to
                        .get_node()
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
            .to_absolute_path(repository.require_path()?)
            .forward::<SyncError>("placing the merged file below the root")?;
        let mine_is_file = change_to.to.flags.contains(NodeFlags::File);
        if has_theirs && mine_is_file && operation.infer_is_diffable(change_to.path()).await? {
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

/// Stages [`merged_file_node`] and records a conflict on `state_stage` where one is left. Removes
/// the file where the conflict deletes it. Returns the node's size.
///
/// A deletion at a path this branch lacks is staged as an add taken back: the node is added
/// staged for deletion, carrying `StagedAdd` beside `StagedDelete`, so that staging the file the
/// working tree holds there restores the add.
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
    let node = if take_theirs && change_from.to.mapping.node.is_valid_node_id() {
        change_from.to.get_node().await
    } else if change_to.to.mapping.node.is_valid_node_id() {
        change_to.to.get_node().await
    } else if change_from.to.mapping.node.is_valid_node_id() {
        change_from.to.get_node().await
    } else {
        // Should not happen
        lore_error!("Unexpected merge conflict of deleted file in both incoming revisions");
        return Err(SyncError::internal("Invalid change data"));
    }
    .forward::<SyncError>("Failed to resolve node in merge revisions")?;
    let node = merged_file_node(
        node,
        change_from,
        change_to,
        take_theirs,
        conflict,
        resolved,
    );
    let size = node.size;
    lore_trace!(
        "Staging conflict node in target state with flags {:x}",
        node.flags
    );
    let takes_deletion = take_theirs && !change_from.to.mapping.node.is_valid_node_id();
    if conflict && !takes_deletion && !change_to.to.mapping.node.is_valid_node_id() {
        operation.remove(change_to.path()).await?;
    }

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

/// `node`, the side a merge stages of a file both sides changed at the same path, with the
/// executable bit [`change::merged_mode`] gives it and flagged with how the merge left it.
pub(crate) fn merged_file_node(
    mut node: Node,
    change_from: &NodeChange,
    change_to: &NodeChange,
    take_theirs: bool,
    conflict: bool,
    resolved: bool,
) -> Node {
    if let Some(mode) = change::merged_mode(change_from, change_to) {
        node.mode = mode;
    }
    node.flags = NodeFlags::from_bits_truncate(node.flags)
        .bitand(NodeFlags::File | NodeFlags::Link)
        .bits();

    if take_theirs && !change_from.to.mapping.node.is_valid_node_id() {
        // The incoming side deleted the path, so adopting it deletes.
        node.flags |= NodeFlags::StagedDelete;
    } else if conflict && !change_to.to.mapping.node.is_valid_node_id() {
        node.flags |= NodeFlags::StagedDelete;
    } else if change_to.action == change::FileAction::Move {
        node.flags |= NodeFlags::StagedMove;
    }

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
    node
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
            .get_node()
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
