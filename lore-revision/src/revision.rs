// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod amend;
pub mod bisect;
pub mod cherry_pick;
pub mod diff;
pub mod history;
pub mod info;
pub mod restore;
pub mod revert;
pub mod sync;

use std::str::FromStr;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_transport::RevisionListIdentifier;
use serde::Serialize;
use tokio::join;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::branch;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::is_conflict;
use crate::errors::NotSupported;
use crate::errors::Oversized;
use crate::errors::RevisionNotFound;
use crate::errors::absent_unless;
use crate::errors::absent_unless_refused;
use crate::event;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::find;
use crate::history::find_branch_point;
use crate::lore::*;
use crate::lore_debug;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::node::Node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFlags;
use crate::node::NodeIDExt;
use crate::repository::RepositoryContext;
use crate::state;
use crate::state::State;
use crate::state::StateError;
use crate::state::TreePath;
use crate::state::gather_tree_paths;
use crate::util::path::RelativePath;

/// Base metadata for revision
#[derive(Default)]
pub struct RevisionMetadata {
    /// Commit message
    pub message: String,
    /// Timestamp, number of milliseconds since Unix epoch in UTC
    pub timestamp: u64,
    /// Branch where revision was committed
    pub branch: BranchId,
    /// Created by user names or identifiers (original authors of the revision)
    pub created_by: Option<String>,
    /// Committed by user name or identifier (user committing the revision to the branch)
    pub committed_by: Option<String>,
    /// Reviewed by user names or identifiers (user performing change review and approving revision)
    pub reviewed_by: Option<String>,
    /// Merged by user name or identifier (user performing the merge)
    pub merged_by: Option<String>,
    /// Perforce changelist association
    pub p4_changelist: Option<String>,
    /// Revision this was cherry-picked from
    pub cherry_picked_from: Hash,
    /// Revision this was reverted from
    pub reverted_from: Hash,
    /// Change Request ID
    pub change_request: Option<String>,
}

impl RevisionMetadata {
    pub fn from_metadata(metadata: Metadata) -> Self {
        let mut revision_metadata = RevisionMetadata::default();
        metadata.walk(|key, value, _value_type| {
            let key = std::str::from_utf8(key).unwrap_or("<binary>");
            match key {
                metadata::MESSAGE => {
                    revision_metadata.message =
                        std::str::from_utf8(value).unwrap_or("<binary>").to_string();
                }
                metadata::TIMESTAMP if value.len() == std::mem::size_of::<u64>() => {
                    revision_metadata.timestamp = u64::from_le_bytes(value.try_into().unwrap());
                }
                metadata::BRANCH if value.len() == std::mem::size_of::<Context>() => {
                    revision_metadata.branch = value.into();
                }
                metadata::CREATED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.created_by = Some(value.to_string());
                    }
                }
                metadata::COMMITTED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.committed_by = Some(value.to_string());
                    }
                }
                metadata::REVIEWED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.reviewed_by = Some(value.to_string());
                    }
                }
                metadata::MERGED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.merged_by = Some(value.to_string());
                    }
                }
                metadata::P4_CHANGELIST => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.p4_changelist = Some(value.to_string());
                    }
                }
                metadata::CHERRY_PICKED_FROM if value.len() == std::mem::size_of::<Hash>() => {
                    revision_metadata.cherry_picked_from = value.into();
                }
                metadata::REVERTED_FROM if value.len() == std::mem::size_of::<Hash>() => {
                    revision_metadata.reverted_from = value.into();
                }
                metadata::CHANGE_REQUEST => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.change_request = Some(value.to_string());
                    }
                }
                _ => {}
            }
        });
        revision_metadata
    }
}

#[derive(Debug, Default)]
pub struct DiffResult {
    /// Base revision
    pub base: Hash,
    /// Source revision
    pub source: Hash,
    /// Target revision
    pub target: Hash,
    /// Set of changes
    pub changes: Vec<NodeChange>,
    /// Set of conflicts, first element is source change, second element is target change
    pub conflicts: Vec<(NodeChange, NodeChange)>,
}

/// One item emitted on the sender by a streaming 3-way diff: a single per-path
/// `Change`, or a `Conflict` pair (source-side change, target-side change).
/// Mirrors the `oneof payload` shape of `lore.thin_client.v1.RevisionDiffResponse`.
///
/// `Conflict` boxes its pair so that `Change` (the hot path) does not pay
/// the inline-pair padding cost on every send. `Change` itself stays
/// unboxed because it is the common case and unboxed flow is cheaper than
/// the heap allocation a `Box<NodeChange>` would add per item.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum DiffItem {
    Change(NodeChange),
    Conflict(Box<(NodeChange, NodeChange)>),
}

/// Per-conflict history-walk result, returned by tasks spawned in the
/// parallel history-walk pass. `index` is the conflict's position in
/// `joined_conflicts`, used to re-order outcomes deterministically
/// after the `JoinSet` drains in completion order.
#[derive(Debug, Clone, Copy)]
struct HistoryWalkOutcome {
    index: usize,
    is_merged_from_source: bool,
    is_merged_from_target: bool,
}

/// The small fixed-size facts a streaming 3-way diff reports after the stream
/// completes. Replaces the non-streaming fields of `DiffResult` (everything
/// except the `changes` / `conflicts` vectors, which are streamed instead).
#[derive(Debug, Default, Clone, Copy)]
pub struct Diff3Summary {
    /// Resolved common-ancestor base revision.
    pub base: Hash,
    /// Source revision (from-side).
    pub source: Hash,
    /// Target revision (to-side).
    pub target: Hash,
}

/// The metadata `metadata_hash` names, or `None` where there is none to read.
///
/// That blob carries the branch, date and message of the revision naming it, so a report
/// without it is a report without those, and the warning is what says so. A zero hash names
/// none, and a blob that cannot be read is warned about rather than failing the caller: a
/// revision list leaves the states of a segment in the local store without the metadata they
/// name, so a revision read out of that cache offline has nothing to take those fields from,
/// which is ordinary rather than damage.
pub(crate) async fn reported_metadata(
    repository: Arc<RepositoryContext>,
    metadata_hash: Hash,
) -> Option<Metadata> {
    if metadata_hash.is_zero() {
        return None;
    }

    match Metadata::deserialize(repository, metadata_hash).await {
        Ok(metadata) => Some(metadata),
        Err(err) => {
            lore_warn!(
                "Reporting revision without branch, date and message, metadata {metadata_hash} could not be read: {err}"
            );
            None
        }
    }
}

/// A filter whose view scopes target's walk to what `source_changes` touch:
/// every path excluded, then each changed path and move source re-included
/// with everything below it, through
/// [`FilterInstance::add_path_inclusion`](crate::filter::FilterInstance::add_path_inclusion).
/// Were the blanket exclusion refused, the filter would be empty, and target's
/// walk would run unscoped and without the repository's view and ignore rules.
fn filter_from_source_changes(source_changes: &[NodeChange]) -> Filter {
    let mut filter = Filter::default();
    if let Err(err) = filter.view.add_exclusion("**") {
        lore_warn!("Failed to add target filter global exclusion: {err}");
        return filter;
    }
    for change in source_changes {
        filter.view.add_path_inclusion(change.path());
        if let Some(from_path) = change.move_source() {
            filter.view.add_path_inclusion(from_path);
        }
    }
    filter
}

/// Threshold above which `diff3_collect` skips building a source-derived
/// path filter for target's walk. Tuned to bound filter-construction
/// cost; above this the unfiltered walk is cheaper.
const SOURCE_FILTER_THRESHOLD: usize = 10_000;

#[allow(clippy::doc_overindented_list_items)]
/// Streaming 3-way diff. Drains source's `state::diff` into a sorted
/// `Vec`, then walks target streaming, joining each target change
/// against source via binary search and resolving conflicts /
/// post-merge fixups as items flow through. Memory bound is O(source
/// diff size); target streams without materialising.
///
/// Algorithm:
/// 1. Walk base→source via `state::diff_collect`, retain non-trivial
///    changes, sort by path (matches today's `diff3_collect` head).
/// 2. When source has fewer than `SOURCE_FILTER_THRESHOLD` changes,
///    derive a path-include filter and scope target's walk to
///    source-touched paths (preserves today's optimization and its
///    observable-output side-effect — see spec Open Question #1).
/// 3. Stream target via `state::diff` into a bounded channel. For each
///    target change, binary-search source by path. Equal-path pairs
///    resolve via `is_conflict`; target-only changes emit if the
///    filter is not active or path is filter-included. A target delete
///    of a file the source changed, and a target add at a path the
///    source changed, are halves of the target's move of the file where
///    the other tree holds the node with its ID, a file of the same
///    identity; the move pairs once, and where the source changed both
///    ends, the delete joins where the file stood. The same move on both
///    sides conflicts only where both edit the file.
/// 4. Buffer joined output internally to fold three post-merge passes
///    (moved nodes, directory-delete overlap, history-walk resolution)
///    before emitting. History-walk runs in parallel via
///    a semaphore-bounded `JoinSet` (see the
///    `history_walk_concurrency` parameter of
///    `diff3_with_source_cap`, defaulting to
///    `DEFAULT_HISTORY_WALK_CONCURRENCY`).
///
/// Returns a `Diff3Summary` carrying the base / source / target
/// revisions. Items emit on `tx` as `DiffItem::Change` or
/// `DiffItem::Conflict`.
pub async fn diff3(
    repository: Arc<RepositoryContext>,
    base: Hash,
    source: Hash,
    target: Hash,
    path: Option<RelativePath>,
    include_same: bool,
    tx: mpsc::Sender<Result<DiffItem, StateError>>,
) -> Result<Diff3Summary, StateError> {
    diff3_with_source_cap(
        repository,
        base,
        source,
        target,
        path,
        include_same,
        None,
        None,
        None,
        tx,
    )
    .await
}

/// Default in-flight count for the history-walk parallel pass when
/// the caller does not supply an explicit value. Set empirically via
/// `~/perf/perfkit` on a 100k 3-way fixture: wall-clock improvement
/// from parallelism plateaus around 24, while peak RSS climbs with
/// each additional in-flight walk (each pins an `Arc<State>` for a
/// deserialised revision blob).
pub const DEFAULT_HISTORY_WALK_CONCURRENCY: usize = 24;

/// `diff3` with optional tunables.
///
/// * `source_cap` — fail with `StateError::Oversized`, naming the cap, once the
///   source walk has produced more than `n` changes. Counted as they arrive
///   rather than once the walk is done, so a diff that will be rejected is not
///   walked to the end. Bounds peak memory for callers that need a ceiling, and
///   is what they map to `Status::resource_exhausted` through `is_oversized()`
///   rather than by matching the message. Library callers (filesystem diff,
///   merge, capi, CLI) pass `None` via `diff3` to stay unbounded.
/// * `history_walk_concurrency` — permit count for the semaphore
///   gating parallel `is_last_change_merged` history walks. Passing
///   `None` falls back to `DEFAULT_HISTORY_WALK_CONCURRENCY`.
#[allow(clippy::too_many_arguments)]
pub async fn diff3_with_source_cap(
    repository: Arc<RepositoryContext>,
    base: Hash,
    source: Hash,
    target: Hash,
    path: Option<RelativePath>,
    include_same: bool,
    source_cap: Option<usize>,
    history_walk_concurrency: Option<usize>,
    // Decides which subtrees the view excludes and may be adopted whole.
    // `None` disables adoption.
    graft_view: Option<Arc<crate::filter::Filter>>,
    tx: mpsc::Sender<Result<DiffItem, StateError>>,
) -> Result<Diff3Summary, StateError> {
    let (state_base, state_source, state_target) = join!(
        State::deserialize(repository.clone(), base),
        State::deserialize(repository.clone(), source),
        State::deserialize(repository.clone(), target)
    );
    let (state_base, state_source, state_target) = (state_base?, state_source?, state_target?);

    let source_branch = state_source
        .revision_metadata(repository.clone())
        .await?
        .branch;
    let target_branch = state_target
        .revision_metadata(repository.clone())
        .await?
        .branch;

    lore_debug!(
        "Calculating 3-way diff between\n  base {} -> {}\n  source {} -> {}\n  target {} -> {}",
        state_base.revision_number(),
        state_base.revision(),
        state_source.revision_number(),
        state_source.revision(),
        state_target.revision_number(),
        state_target.revision()
    );

    lore_debug!("Diff source branch revisions (streaming)");
    let source_walker_repo = repository.clone();
    let source_walker_state_base = state_base.clone();
    let source_walker_state_source = state_source.clone();
    let source_walker_path = path.clone();
    // Give the source walk the target tree, so it can adopt whole subtrees
    // the target never touched instead of descending.
    let source_walker_graft = graft_view.map(|view| {
        Arc::new(state::GraftOracle::new(
            repository.clone(),
            state_target.clone(),
            view,
        ))
    });
    let mut source_walk = state::ChangeStream::spawn(async move |changes| {
        state::diff(
            source_walker_repo.clone(),
            source_walker_state_base,
            source_walker_repo,
            source_walker_state_source,
            source_walker_path,
            source_walker_graft,
            &changes,
            FilterMode::View,
        )
        .await
    });

    let mut source_changes: Vec<NodeChange> = Vec::new();
    let mut oversized = false;
    while let Some(change) = source_walk.next().await {
        if is_file_id_only_churn(&change) {
            continue;
        }
        source_changes.push(change);
        if let Some(cap) = source_cap
            && source_changes.len() > cap
        {
            oversized = true;
            break;
        }
    }
    if oversized {
        source_walk.abandon().await;
        return Err(StateError::from(Oversized {
            context: format!(
                "source-side diff change count exceeds configured limit of {}",
                source_cap.unwrap_or(0)
            ),
        }));
    }
    source_walk.finish().await?;
    state::detect_and_coalesce_moves(&mut source_changes);

    lore_debug!("Sorting {} source changes", source_changes.len());
    change::sort_by_path(&mut source_changes);

    // The subtree moves between repositories along with a mount either side replaced, so a change
    // the other side made below one cannot be applied on its own.
    let source_mounts = replaced_mount_paths(&source_changes);
    let source_replacements = file_directory_replacements(&source_changes);
    let mut target_mounts: Vec<RelativePath> = Vec::new();
    let mut conflicting_mounts: Vec<RelativePath> = Vec::new();

    let target_filter = if source_changes.len() < SOURCE_FILTER_THRESHOLD {
        Arc::new(filter_from_source_changes(&source_changes))
    } else {
        repository.filter.clone()
    };
    let target_repository = Arc::new(repository.to_filter_context(target_filter));

    lore_debug!("Diff target branch revisions (streaming)");
    let walker_repo = target_repository.clone();
    let walker_state_base = state_base.clone();
    let walker_path = path.clone();
    let walker_state_target = state_target.clone();
    let target_walk = state::ChangeStream::spawn(async move |changes| {
        state::diff(
            walker_repo.clone(),
            walker_state_base,
            walker_repo,
            walker_state_target,
            walker_path,
            // Adoption is a source-side decision.
            None,
            &changes,
            FilterMode::View,
        )
        .await
    });

    // The parts of the join end with this block, so the awaits after it hold none of them.
    let (mut joined_changes, joined_conflicts) = {
        let Joined {
            changes: mut joined_changes,
            conflicts: mut joined_conflicts,
            mut target_changes,
            target_moves,
            target_replacements,
            source_consumed,
        } = Box::pin(join_target_walk(
            target_walk,
            &source_changes,
            &source_mounts,
            &mut target_mounts,
            &mut conflicting_mounts,
            (&state_base, &state_target),
            include_same,
        ))
        .await?;
        for (idx, consumed) in source_consumed.iter().enumerate() {
            if !*consumed {
                joined_changes.push(source_changes[idx].clone());
            }
        }

        for mount in target_mounts.iter() {
            if source_changes
                .iter()
                .any(|change| is_below(mount, change.path()))
            {
                push_unique_path(&mut conflicting_mounts, mount);
            }
        }

        pair_moved_nodes(
            &mut joined_changes,
            &mut target_changes,
            target_moves,
            &mut joined_conflicts,
            include_same,
        )
        .await?;
        let target_replacements = sorted_replacements(target_replacements);
        for path in conflicting_replacements(
            [
                (&source_replacements, &target_replacements, &target_changes),
                (&target_replacements, &source_replacements, &joined_changes),
            ],
            &joined_conflicts,
        ) {
            push_unique_path(&mut conflicting_mounts, &path);
        }
        joined_changes.append(&mut target_changes);
        (joined_changes, joined_conflicts)
    };

    if !joined_conflicts.is_empty() {
        joined_changes.retain(|item| {
            if item.from.flags.is_directory() && item.action == FileAction::Delete {
                for (from, _to) in joined_conflicts.iter() {
                    if from.path().overlaps(item.path()) {
                        return false;
                    }
                }
            }
            true
        });
        lore_debug!("Identify resolved conflicts through merges");
    }

    // Producer loop acquires a permit before spawning, so the in-flight
    // set never exceeds `permits` tasks — each in-flight walk pins an
    // `Arc<State>` over a deserialised revision blob, and at 10k+
    // conflicts unbounded fan-out blows past tens of GB of pinned state.
    //
    // No abort path: on error we stop spawning and let in-flight tasks
    // finish naturally. Cancel-safety is not something the surrounding
    // code is designed to tolerate today (e.g. a task aborted mid-write
    // to a downstream channel could surface a partial value), so let
    // them run to completion.
    let permits = history_walk_concurrency.unwrap_or(DEFAULT_HISTORY_WALK_CONCURRENCY);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(permits));
    let mut history_walks: JoinSet<Result<HistoryWalkOutcome, StateError>> = JoinSet::new();
    let mut outcomes: Vec<HistoryWalkOutcome> = Vec::with_capacity(joined_conflicts.len());
    let mut walk_err: Option<StateError> = None;
    for (idx, (source_conflict, target_conflict)) in joined_conflicts.iter().enumerate() {
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(err) => {
                walk_err = Some(StateError::internal_with_context(
                    err,
                    "history-walk semaphore closed",
                ));
                break;
            }
        };
        while let Some(joined) = history_walks.try_join_next() {
            match joined {
                Ok(Ok(outcome)) => outcomes.push(outcome),
                Ok(Err(err)) => walk_err = walk_err.or(Some(err)),
                Err(join_err) => {
                    walk_err = walk_err.or(Some(StateError::internal_with_context(
                        join_err,
                        "history-walk task failed",
                    )));
                }
            }
        }
        if walk_err.is_some() {
            drop(permit);
            break;
        }
        let source_conflict = source_conflict.clone();
        let target_conflict = target_conflict.clone();
        let state_branch_point = state_base.clone();
        lore_spawn!(history_walks, async move {
            let _permit = permit;
            let is_merged_from_target = is_last_change_merged(
                target_conflict.clone(),
                target_branch,
                source_conflict.clone(),
                source_branch,
                state_branch_point.clone(),
            )
            .await?;
            let is_merged_from_source = is_last_change_merged(
                source_conflict,
                source_branch,
                target_conflict,
                target_branch,
                state_branch_point,
            )
            .await?;
            Ok(HistoryWalkOutcome {
                index: idx,
                is_merged_from_source,
                is_merged_from_target,
            })
        });
    }

    while let Some(joined) = history_walks.join_next().await {
        match joined {
            Ok(Ok(outcome)) => outcomes.push(outcome),
            Ok(Err(err)) => walk_err = walk_err.or(Some(err)),
            Err(join_err) => {
                walk_err = walk_err.or(Some(StateError::internal_with_context(
                    join_err,
                    "history-walk task failed",
                )));
            }
        }
    }
    if let Some(err) = walk_err {
        return Err(err);
    }
    outcomes.sort_unstable_by_key(|o| o.index);

    let mut final_conflicts: Vec<(NodeChange, NodeChange)> = Vec::new();
    for outcome in outcomes {
        let (source_conflict, target_conflict) = &joined_conflicts[outcome.index];
        if !outcome.is_merged_from_source && !outcome.is_merged_from_target {
            lore_debug!("Conflict remains: {:?}", (source_conflict, target_conflict));
            final_conflicts.push((source_conflict.clone(), target_conflict.clone()));
        } else if !outcome.is_merged_from_source {
            let mut change = source_conflict.clone();
            lore_debug!(
                "Conflict resolved: {:?}",
                (source_conflict, target_conflict)
            );
            // A file both branches hold stands where it stood, whichever of them added it, so the
            // change states no move of it.
            if source_conflict.action == FileAction::Add
                && target_conflict.action != FileAction::Delete
            {
                change.action = FileAction::Keep;
            }
            // Since the target was merged into source, the change is
            // actually from the target state to the source current
            // latest state.
            change.from = target_conflict.to.clone();
            joined_changes.push(change);
        } else {
            lore_debug!(
                "Conflict resolved, source change merged: {:?}",
                (source_conflict, target_conflict)
            );
        }
    }
    for mount in conflicting_mounts.iter() {
        lore_debug!("{mount} was replaced on one side and changed on the other, conflict there");
        joined_changes.retain(|change| !mount.covers(change.path()));
        final_conflicts.retain(|(source_conflict, target_conflict)| {
            !mount.covers(source_conflict.path()) && !mount.covers(target_conflict.path())
        });
        final_conflicts.push(
            replacement_conflict(
                &repository,
                &state_base,
                &state_source,
                &state_target,
                mount,
            )
            .await,
        );
    }

    if !final_conflicts.is_empty() {
        lore_debug!("Final {} conflicts", final_conflicts.len());
    }

    let summary = Diff3Summary {
        base,
        source,
        target,
    };

    for change in joined_changes {
        let permit = tx
            .reserve()
            .await
            .map_err(|_closed| StateError::internal("3-way diff receiver dropped"))?;
        permit.send(Ok(DiffItem::Change(change)));
    }
    for (source_change, target_change) in final_conflicts {
        let permit = tx
            .reserve()
            .await
            .map_err(|_closed| StateError::internal("3-way diff receiver dropped"))?;
        permit.send(Ok(DiffItem::Conflict(Box::new((
            source_change,
            target_change,
        )))));
    }
    Ok(summary)
}

#[lore_macro::test_pub]
fn is_below(mount: &RelativePath, path: &RelativePath) -> bool {
    mount.covers(path) && mount != path
}

#[lore_macro::test_pub]
fn push_unique_path(paths: &mut Vec<RelativePath>, path: &RelativePath) {
    if !paths.contains(path) {
        paths.push(path.clone());
    }
}

/// A replacement is walked as a delete of the old node and an add of the new one, so both changes
/// name the same path and it is collected once.
fn replaced_mount_paths(changes: &[NodeChange]) -> Vec<RelativePath> {
    let mut paths: Vec<RelativePath> = Vec::new();
    for change in changes.iter().filter(|c| change::is_link_replacement(c)) {
        push_unique_path(&mut paths, change.path());
    }
    paths
}

/// The paths where one side of a 3-way diff replaced a file with a directory, or a directory with
/// a file, and the other made a change there or below that cannot stand beside the replacement.
///
/// Each of `sides` holds one side's replacements, the other side's, each sorted by path, and the
/// other side's changes the join left to apply. A replacement conflicts where a pair in
/// `conflicts` stands at or below it, or where a change of the other side is left there. Where
/// both sides put the same type of node there, the paths below it merge one by one. A pair in
/// `conflicts` where one side added a file and the other a directory conflicts the same way,
/// though neither replaced anything.
#[lore_macro::test_pub]
fn conflicting_replacements(
    sides: [(&[Replacement], &[Replacement], &[NodeChange]); 2],
    conflicts: &[(NodeChange, NodeChange)],
) -> Vec<RelativePath> {
    let mut paths = Vec::new();
    for (source, target) in conflicts {
        if change::file_directory_clash(source, target) {
            push_unique_path(&mut paths, target.path());
        }
    }
    if sides
        .iter()
        .all(|(replacements, _, _)| replacements.is_empty())
    {
        return paths;
    }
    let conflicted: Vec<&str> = sorted_paths(
        conflicts
            .iter()
            .flat_map(|(source, target)| [source.path(), target.path()]),
    );
    for (replacements, others, other_changes) in sides {
        if replacements.is_empty() {
            continue;
        }
        let changed = sorted_paths(other_changes.iter().map(NodeChange::path));
        for (path, to_type) in replacements {
            if others
                .binary_search_by(|(other, _)| other.as_str().cmp(path.as_str()))
                .is_ok_and(|found| others[found].1 == *to_type)
            {
                continue;
            }
            if covers_any(&conflicted, path) || covers_any(&changed, path) {
                push_unique_path(&mut paths, path);
            }
        }
    }
    paths
}

/// A path one side replaced a file with a directory or a directory with a file at, and the type of
/// node it put there.
type Replacement = (RelativePath, crate::interface::LoreNodeType);

/// The replacements of a file with a directory and of a directory with a file among `changes`,
/// sorted by path.
fn file_directory_replacements(changes: &[NodeChange]) -> Vec<Replacement> {
    sorted_replacements(
        changes
            .iter()
            .filter_map(|change| {
                change::file_directory_replacement(change)
                    .map(|to_type| (change.path().clone(), to_type))
            })
            .collect(),
    )
}

/// `replacements` sorted by path, each path once: a replacement is walked as a delete and an add at
/// one path.
fn sorted_replacements(mut replacements: Vec<Replacement>) -> Vec<Replacement> {
    replacements.sort_unstable_by(|(lhs, _), (rhs, _)| lhs.as_str().cmp(rhs.as_str()));
    replacements.dedup_by(|(lhs, _), (rhs, _)| lhs == rhs);
    replacements
}

fn sorted_paths<'a>(paths: impl Iterator<Item = &'a RelativePath>) -> Vec<&'a str> {
    let mut sorted: Vec<&str> = paths.map(RelativePath::as_str).collect();
    sorted.sort_unstable();
    sorted
}

/// Whether `sorted` holds `path` or a path below it.
fn covers_any(sorted: &[&str], path: &RelativePath) -> bool {
    if sorted.binary_search(&path.as_str()).is_ok() {
        return true;
    }
    let prefix = format!("{path}/");
    let first = sorted.partition_point(|candidate| *candidate < prefix.as_str());
    sorted
        .get(first)
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

async fn replacement_conflict(
    repository: &Arc<RepositoryContext>,
    state_base: &Arc<State>,
    state_source: &Arc<State>,
    state_target: &Arc<State>,
    path: &RelativePath,
) -> (NodeChange, NodeChange) {
    let base = mount_node_change_state(repository, state_base, path).await;
    let source = mount_node_change_state(repository, state_source, path).await;
    let target = mount_node_change_state(repository, state_target, path).await;
    (
        mount_conflict_change(base.clone(), source),
        mount_conflict_change(base, target),
    )
}

/// The node `state` holds at `path`, which is the link node itself where the path ends at a mount
/// rather than anything the linked repository holds. A side that holds nothing there is the side
/// that deleted the path.
async fn mount_node_change_state(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    path: &RelativePath,
) -> change::NodeChangeState {
    let mapping = state::NodeMapping::root(repository.clone(), state.clone());
    let link = mapping.node_at(path).await;
    let node = if link.node.is_valid_node_id() {
        state
            .node(repository.clone(), link.node)
            .await
            .unwrap_or_default()
    } else {
        Node::default()
    };
    change::NodeChangeState {
        mapping: state::NodeMapping {
            repository: repository.clone(),
            state: state.clone(),
            path: path.clone(),
            node: link.node,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(node.flags),
        address: node.address,
        mode: node.mode,
    }
}

fn mount_conflict_change(from: change::NodeChangeState, to: change::NodeChangeState) -> NodeChange {
    NodeChange {
        action: FileAction::Keep,
        flags: change::Flags::Conflict,
        from,
        to,
    }
}

/// The target's changes as `target_walk` streams them in, joined with the source's changes at their
/// paths, `source_changes` sorted by path. A target delete of a file the source changed, or moved
/// away from, and a target add at a path the source changed, are halves of the target's move of
/// the file where the other tree holds the node with its ID, a file of the same identity; the move
/// pairs with the source's changes once. Where the source changed both ends of the move, the delete
/// joins where the file stood, as with no move. Records the target's mounts, and the source's
/// mounts the target changed below, in `target_mounts` and `conflicting_mounts`.
#[allow(clippy::too_many_arguments)]
#[lore_macro::test_pub]
async fn join_target_walk<Summary: Default + Send + 'static>(
    mut target_walk: state::ChangeStream<Summary>,
    source_changes: &[NodeChange],
    source_mounts: &[RelativePath],
    target_mounts: &mut Vec<RelativePath>,
    conflicting_mounts: &mut Vec<RelativePath>,
    (state_base, state_target): (&Arc<State>, &Arc<State>),
    include_same: bool,
) -> Result<Joined, StateError> {
    let mut source_moves: Vec<usize> = (0..source_changes.len())
        .filter(|&index| source_changes[index].relocated_from().is_some())
        .collect();
    source_moves.sort_unstable_by(|&lhs, &rhs| {
        relocation_source(&source_changes[lhs]).cmp(relocation_source(&source_changes[rhs]))
    });

    let mut joined = Joined::new(source_changes.len());
    loop {
        let (target_change, at) = {
            let Some(target_change) = target_walk.next().await else {
                break;
            };
            if is_file_id_only_churn(&target_change) {
                continue;
            }
            if change::is_link_replacement(&target_change) {
                push_unique_path(target_mounts, target_change.path());
            }
            if let Some(to_type) = change::file_directory_replacement(&target_change) {
                joined
                    .target_replacements
                    .push((target_change.path().clone(), to_type));
            }
            if let Some(mount) = source_mounts
                .iter()
                .find(|mount| is_below(mount, target_change.path()))
            {
                push_unique_path(conflicting_mounts, mount);
            }
            let at = index_at(source_changes, target_change.path().as_str());
            let half_of_move = match target_change.action {
                FileAction::Delete => {
                    (at.is_ok()
                        || source_moves
                            .binary_search_by(|&index| {
                                relocation_source(&source_changes[index])
                                    .cmp(target_change.path().as_str())
                            })
                            .is_ok())
                        && state::changes_own_file(state_target, &target_change)
                }
                FileAction::Add => {
                    at.is_ok() && state::changes_own_file(state_base, &target_change)
                }
                _ => false,
            };
            let moved = if half_of_move {
                state::whole_move(&target_change).await?
            } else {
                None
            };
            match (target_change.action, moved) {
                // The source changed both ends of the move, so neither of its changes can follow
                // it.
                (FileAction::Delete, Some(moved))
                    if at.is_ok() && index_at(source_changes, moved.path().as_str()).is_ok() =>
                {
                    (target_change, at)
                }
                // The walk lists the moved path too where the source changed it, and joins it
                // there.
                (FileAction::Delete, Some(moved)) => {
                    joined.target_moves.push(moved);
                    continue;
                }
                (_, Some(moved)) => (moved, at),
                (_, None) => (target_change, at),
            }
        };
        joined
            .join(source_changes, target_change, at, include_same)
            .await?;
    }

    target_walk.finish().await?;
    Ok(joined)
}

/// Whether `change` leaves its node where it stood with the content it held, which a walk reports
/// where only the file ID differs.
fn is_file_id_only_churn(change: &NodeChange) -> bool {
    change.action != FileAction::Delete
        && change.relocated_from().is_none()
        && !change.from.address.hash.is_zero()
        && change.from.address.hash == change.to.address.hash
}

/// The path a change leaves its node away from, and the empty path, which orders first, for a
/// change leaving its node where it stood.
fn relocation_source(change: &NodeChange) -> &str {
    change.relocated_from().map_or("", RelativePath::as_str)
}

/// The index of the change standing at `path` among `changes`, sorted by path, or the index it
/// would stand at.
fn index_at(changes: &[NodeChange], path: &str) -> Result<usize, usize> {
    changes.binary_search_by(|change| change.path().as_str().cmp(path))
}

/// [`index_at`] for the path `prefix` followed by `rest`, without joining the two.
fn index_at_joined(changes: &[NodeChange], prefix: &str, rest: &str) -> Result<usize, usize> {
    changes.binary_search_by(|change| {
        let path = change.path().as_str().as_bytes();
        let (front, back) = path.split_at(path.len().min(prefix.len()));
        front
            .cmp(prefix.as_bytes())
            .then_with(|| back.cmp(rest.as_bytes()))
    })
}

/// The indices of the changes in `changes`, sorted by path, that stand below `ancestor`.
fn indices_below(changes: &[NodeChange], ancestor: &str) -> std::ops::Range<usize> {
    let order = |change: &NodeChange| {
        let path = change.path().as_str().as_bytes();
        let (prefix, rest) = path.split_at(path.len().min(ancestor.len()));
        prefix.cmp(ancestor.as_bytes()).then_with(|| {
            rest.first()
                .map_or(std::cmp::Ordering::Less, |b| b.cmp(&b'/'))
        })
    };
    changes.partition_point(|change| order(change).is_lt())
        ..changes.partition_point(|change| order(change).is_le())
}

/// Whether `change` changes what a node other than a directory holds: its content or its mode.
fn edits(change: &NodeChange) -> bool {
    !change.to.flags.is_directory() && change.from.differs_from(&change.to)
}

/// Whether the two sides' changes of one file or link both change what it holds, a file's content
/// or a link's pin, or its mode, and leave it different.
fn edits_both(source: &NodeChange, target: &NodeChange) -> bool {
    let holds = NodeFlags::File | NodeFlags::Link;
    source.to.flags.intersects(holds)
        && target.to.flags.intersects(holds)
        && edits(source)
        && edits(target)
        && source.to.differs_from(&target.to)
}

/// Whether `change` leaves another file, or nothing, where its node stood: the node was a file of
/// an identity, and what stands at the path after the change is not that file.
fn replaces(change: &NodeChange) -> bool {
    change.from.flags.contains(NodeFlags::File)
        && !change.from.address.context.is_zero()
        && change.from.address.context != change.to.address.context
}

/// Whether the two changes move one node from the same path.
fn moves_alike(source: &NodeChange, target: &NodeChange) -> bool {
    source
        .relocated_from()
        .is_some_and(|from| relocation_source(target) == from.as_str())
}

/// Whether `target` holds, at its path, the node `source` takes from where it stood.
fn holds_node(target: &NodeChange, source: &NodeChange) -> bool {
    source.from.mapping.node.is_valid_node_id()
        && target.to.mapping.node == source.from.mapping.node
        && target.to.address.context == source.from.address.context
}

/// The target node `target` names, standing at `path` and read through `side`'s repository.
fn on_target(
    side: &change::NodeChangeState,
    target: &change::NodeChangeState,
    path: &RelativePath,
) -> change::NodeChangeState {
    change::NodeChangeState {
        mapping: state::NodeMapping {
            repository: side.mapping.repository.clone(),
            state: target.mapping.state.clone(),
            path: path.clone(),
            node: target.mapping.node,
        },
        ..target.clone()
    }
}

/// Records `source` and `target` as a pair of changes that conflict.
fn push_conflict(
    conflicts: &mut Vec<(NodeChange, NodeChange)>,
    mut source: NodeChange,
    mut target: NodeChange,
) {
    source.flags = change::Flags::Conflict;
    target.flags = change::Flags::Conflict;
    conflicts.push((source, target));
}

/// What is left to apply of the source's move where the target made the same move: the source's
/// edit, as a change of the node where the target holds it. `None` where the source left the node
/// as it was or as the target holds it, or moved a directory, unless `include_same` asks for the
/// changes the two sides make alike.
fn onto_target_move(
    source: &NodeChange,
    target: &NodeChange,
    include_same: bool,
) -> Option<NodeChange> {
    let edited = edits(source) && source.to.differs_from(&target.to);
    if !edited && !include_same {
        return None;
    }
    let mut change = source.clone();
    change.action = FileAction::Keep;
    change.from = on_target(&source.from, &target.to, target.path());
    if !edited {
        change.to = on_target(&source.to, &target.to, target.path());
    }
    Some(change)
}

/// Puts `side` where a move of the directory at `from` to `to` puts it, where it stands below
/// `from`.
fn move_below(side: &mut change::NodeChangeState, from: &RelativePath, to: &RelativePath) {
    if let Some(rest) = side.mapping.path.below(from)
        && !rest.is_empty()
    {
        side.mapping.path = to.join(rest);
    }
}

/// The changes of a 3-way diff as the target's walk joins them to the source's by path.
#[lore_macro::test_pub]
struct Joined {
    /// The source's changes the join keeps.
    changes: Vec<NodeChange>,
    /// The pairs of changes that conflict, the source's first.
    conflicts: Vec<(NodeChange, NodeChange)>,
    /// The target's changes no source change stands with, which leave their node where it stood.
    target_changes: Vec<NodeChange>,
    /// The target's changes no source change stands with, which leave their node at another path.
    target_moves: Vec<NodeChange>,
    /// The target's replacements of a file with a directory and of a directory with a file, once
    /// for each change that walks one.
    target_replacements: Vec<Replacement>,
    /// Whether a target change stood with each source change.
    source_consumed: Vec<bool>,
}

impl Joined {
    #[lore_macro::test_pub]
    fn new(source_count: usize) -> Self {
        Joined {
            changes: Vec::new(),
            conflicts: Vec::new(),
            target_changes: Vec::new(),
            target_moves: Vec::new(),
            target_replacements: Vec::new(),
            source_consumed: vec![false; source_count],
        }
    }

    /// Joins `target_change` with the source's change at `at`, its index among `source_changes`
    /// by path. The same move on both sides conflicts only where both edit the file. Any other
    /// pair conflicts where [`is_conflict`] says so, and a pair that leaves the path alike keeps
    /// the source's change only for `include_same`.
    #[lore_macro::test_pub]
    async fn join(
        &mut self,
        source_changes: &[NodeChange],
        target_change: NodeChange,
        at: Result<usize, usize>,
        include_same: bool,
    ) -> Result<(), StateError> {
        let Ok(index) = at else {
            if target_change.relocated_from().is_some() {
                self.target_moves.push(target_change);
            } else {
                self.target_changes.push(target_change);
            }
            return Ok(());
        };
        self.source_consumed[index] = true;
        let source_change = &source_changes[index];
        if moves_alike(source_change, &target_change) {
            if edits_both(source_change, &target_change) {
                push_conflict(&mut self.conflicts, source_change.clone(), target_change);
            } else if let Some(change) =
                onto_target_move(source_change, &target_change, include_same)
            {
                self.changes.push(change);
            }
        } else if is_conflict(source_change, &target_change, true).await? {
            push_conflict(&mut self.conflicts, source_change.clone(), target_change);
        } else if include_same
            && self
                .changes
                .last()
                .is_none_or(|last| last.path() != source_change.path())
        {
            self.changes.push(source_change.clone());
        }
        Ok(())
    }
}

/// Settles the source change at `index` with `target`, the target's change at the path a move put
/// one of the two at. A target change of the same node gives way to the source's. Any other
/// conflicts with it unless the two leave the path alike, where the source's change stands only for
/// `include_same`. Marks a source change that gives way in `source_gone`.
async fn settle_moved(
    sources: &[NodeChange],
    source_gone: &mut [bool],
    conflicts: &mut Vec<(NodeChange, NodeChange)>,
    index: usize,
    target: &NodeChange,
    include_same: bool,
) -> Result<(), StateError> {
    let source = &sources[index];
    if source_gone[index] || holds_node(target, source) {
        return Ok(());
    }
    if is_conflict(source, target, true).await? {
        push_conflict(conflicts, source.clone(), target.clone());
        source_gone[index] = true;
    } else if !include_same {
        source_gone[index] = true;
    }
    Ok(())
}

/// Pairs what the two sides of a 3-way diff made of one node where either leaves it at another
/// path, which the join by path does not reach: a move, or a node below a directory a move renamed.
///
/// `sources` are the source's changes, `targets` the target's that leave their node where it
/// stood, and `target_moves` the target's that leave it elsewhere. None of the target's moves is
/// emitted, since the target holds them already. A change one side made to a node the other moved
/// follows the node:
///
/// - Where one side moves a file and the other edits it, the edit stands where the move put the
///   file, and a move that leaves the content as it was carries the other side's content.
/// - Where both sides change what a file holds, its content or its mode, where one side moves a
///   node and the other deletes or replaces it, and where the two move it to different paths, the
///   pair conflicts.
/// - The target's changes below a directory the source moves travel with the directory, and the
///   source's changes below one the target moves are put where the target put it. A walk reports a
///   directory as moved where a rename changes only its case; a directory moved to another name
///   reaches the diff as a delete and an add, and the files below it as moves.
/// - A change put at another path is checked against the other side's change at that path, as the
///   join checks the two sides' changes at one path.
async fn pair_moved_nodes(
    sources: &mut Vec<NodeChange>,
    targets: &mut Vec<NodeChange>,
    target_moves: Vec<NodeChange>,
    conflicts: &mut Vec<(NodeChange, NodeChange)>,
    include_same: bool,
) -> Result<(), StateError> {
    if target_moves.is_empty()
        && !sources
            .iter()
            .any(|change| change.relocated_from().is_some())
    {
        return Ok(());
    }
    let mut pairing = MovePairing::new(sources, targets, target_moves, conflicts, include_same);
    pairing.pair_source_moves();
    pairing.carry_target_changes().await?;
    let mut retargeted = pairing.pair_target_moves();
    pairing.take_directory_deletes_from_target();
    pairing.follow_target_directory_moves(&mut retargeted);
    pairing.check_retargeted(retargeted).await?;
    pairing.finish();
    Ok(())
}

/// The two sides' changes [`pair_moved_nodes`] pairs, sorted by path, the target's moves, sorted by
/// the path each leaves, which of them a pairing has used up, and whether the diff lists the
/// changes the two sides make alike.
struct MovePairing<'a> {
    sources: &'a mut Vec<NodeChange>,
    targets: &'a mut Vec<NodeChange>,
    target_moves: Vec<NodeChange>,
    conflicts: &'a mut Vec<(NodeChange, NodeChange)>,
    source_gone: Vec<bool>,
    target_gone: Vec<bool>,
    target_move_paired: Vec<bool>,
    include_same: bool,
}

impl<'a> MovePairing<'a> {
    fn new(
        sources: &'a mut Vec<NodeChange>,
        targets: &'a mut Vec<NodeChange>,
        mut target_moves: Vec<NodeChange>,
        conflicts: &'a mut Vec<(NodeChange, NodeChange)>,
        include_same: bool,
    ) -> Self {
        change::sort_by_path(sources);
        change::sort_by_path(targets);
        target_moves
            .sort_unstable_by(|lhs, rhs| relocation_source(lhs).cmp(relocation_source(rhs)));
        MovePairing {
            source_gone: vec![false; sources.len()],
            target_gone: vec![false; targets.len()],
            target_move_paired: vec![false; target_moves.len()],
            sources,
            targets,
            target_moves,
            conflicts,
            include_same,
        }
    }

    /// Pairs each source change that leaves its node at another path with what the target made of
    /// the node at the path it stood at. A target move of it elsewhere conflicts, as do a delete, a
    /// replacement, and an edit of a file the source edits too. A source move that leaves the
    /// content as it was takes the target's.
    fn pair_source_moves(&mut self) {
        for index in 0..self.sources.len() {
            let from = relocation_source(&self.sources[index]);
            if from.is_empty() {
                continue;
            }
            if let Ok(other) = self
                .target_moves
                .binary_search_by(|target| relocation_source(target).cmp(from))
            {
                self.target_move_paired[other] = true;
                push_conflict(
                    self.conflicts,
                    self.sources[index].clone(),
                    self.target_moves[other].clone(),
                );
                self.source_gone[index] = true;
            } else if let Ok(other) = index_at(self.targets, from) {
                self.target_gone[other] = true;
                let (source, target) = (&self.sources[index], &self.targets[other]);
                if target.action == FileAction::Delete
                    || replaces(target)
                    || edits_both(source, target)
                {
                    push_conflict(self.conflicts, source.clone(), target.clone());
                    self.source_gone[index] = true;
                } else if source.from.address.hash == source.to.address.hash {
                    let from = on_target(&source.from, &target.to, target.path());
                    let to = on_target(&source.to, &target.to, &source.to.mapping.path);
                    let source = &mut self.sources[index];
                    source.from = from;
                    source.to = to;
                }
            }
        }
    }

    /// Puts each target change below a directory the source moves where the source put the
    /// directory, and settles it with the source's change at the path it lands at as
    /// [`settle_moved`] does. The target's change is not emitted, since the target holds it, and
    /// is copied only where it lands on a source change.
    async fn carry_target_changes(&mut self) -> Result<(), StateError> {
        for index in 0..self.sources.len() {
            let source = &self.sources[index];
            if self.source_gone[index] || !source.to.flags.is_directory() {
                continue;
            }
            let Some(from) = source.relocated_from().cloned() else {
                continue;
            };
            let to = source.path().clone();
            for other in indices_below(self.targets, from.as_str()) {
                if self.target_gone[other] {
                    continue;
                }
                self.target_gone[other] = true;
                let rest = &self.targets[other].path().as_str()[from.as_str().len()..];
                let Ok(at) = index_at_joined(self.sources, to.as_str(), rest) else {
                    continue;
                };
                let mut target = self.targets[other].clone();
                move_below(&mut target.from, &from, &to);
                move_below(&mut target.to, &from, &to);
                settle_moved(
                    self.sources,
                    &mut self.source_gone,
                    self.conflicts,
                    at,
                    &target,
                    self.include_same,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Pairs each target move the source did not make as well with the source's change at the path
    /// the node stood at. A delete, a replacement, and an edit of a file the target edits too
    /// conflict, a delete where the move put the node, and the source's changes below a directory
    /// it deletes wait for that conflict. The source's change follows the move to where it put the
    /// node where the move leaves the content as it was, or where the diff lists the changes the
    /// two sides make alike, and gives way to the move otherwise. Answers the indices of the
    /// source changes put at another path, which move once every pair is found, as finding one
    /// reads the sources by path.
    fn pair_target_moves(&mut self) -> Vec<usize> {
        let mut followed = Vec::new();
        for index in 0..self.target_moves.len() {
            if self.target_move_paired[index] {
                continue;
            }
            let target = &self.target_moves[index];
            let Ok(other) = index_at(self.sources, relocation_source(target)) else {
                continue;
            };
            let source = &self.sources[other];
            if self.source_gone[other] || source.relocated_from().is_some() {
                continue;
            }
            if source.action == FileAction::Delete {
                let mut source = source.clone();
                source.from = on_target(&source.from, &target.to, target.path());
                source.to.mapping.path = target.path().clone();
                push_conflict(self.conflicts, source, target.clone());
                let below = indices_below(self.sources, self.sources[other].path().as_str());
                self.source_gone[other] = true;
                self.source_gone[below].fill(true);
            } else if replaces(source) || edits_both(source, target) {
                push_conflict(self.conflicts, source.clone(), target.clone());
                self.source_gone[other] = true;
            } else if self.include_same || target.from.address.hash == target.to.address.hash {
                followed.push((other, index));
            } else {
                self.source_gone[other] = true;
            }
        }
        followed
            .into_iter()
            .map(|(other, index)| {
                let target = &self.target_moves[index];
                let from = on_target(&self.sources[other].from, &target.to, target.path());
                let source = &mut self.sources[other];
                source.from = from;
                source.to.mapping.path = target.path().clone();
                other
            })
            .collect()
    }

    /// Takes the `from` side of each directory the source deletes from the target's tree, which
    /// the working tree holds, where a pairing used up a target change below it, such as an edit
    /// of a file the source moves out of it.
    fn take_directory_deletes_from_target(&mut self) {
        for index in 0..self.sources.len() {
            let source = &self.sources[index];
            if source.action != FileAction::Delete
                || !source.from.flags.is_directory()
                || self.source_gone[index]
            {
                continue;
            }
            if let Some(other) = indices_below(self.targets, source.path().as_str())
                .find(|&other| self.target_gone[other])
            {
                self.sources[index].from.mapping.state =
                    self.targets[other].to.mapping.state.clone();
            }
        }
    }

    /// Puts each side of a source change below directories the target moves where the target put
    /// the deepest of them, adding the source changes whose own path that moves to `retargeted`.
    fn follow_target_directory_moves(&mut self, retargeted: &mut Vec<usize>) {
        self.target_moves
            .retain(|target| target.to.flags.is_directory());
        if self.target_moves.is_empty() {
            return;
        }
        for (index, source) in self.sources.iter_mut().enumerate() {
            if self.source_gone[index] {
                continue;
            }
            let resolves_from = source.action == FileAction::Delete;
            let mut moved = false;
            for (is_from, side) in [(true, &mut source.from), (false, &mut source.to)] {
                let path = side.mapping.path.as_str();
                let Some((end, other)) = path.match_indices('/').rev().find_map(|(end, _)| {
                    self.target_moves
                        .binary_search_by(|target| relocation_source(target).cmp(&path[..end]))
                        .ok()
                        .map(|other| (end, other))
                }) else {
                    continue;
                };
                side.mapping.path = self.target_moves[other].path().join(&path[end + 1..]);
                moved |= is_from == resolves_from;
            }
            if moved {
                retargeted.push(index);
            }
        }
    }

    /// Settles each source change put at another path with the target's change at that path as
    /// [`settle_moved`] does.
    async fn check_retargeted(&mut self, retargeted: Vec<usize>) -> Result<(), StateError> {
        for index in retargeted {
            if self.source_gone[index] {
                continue;
            }
            let Ok(other) = index_at(self.targets, self.sources[index].path().as_str()) else {
                continue;
            };
            if self.target_gone[other] {
                continue;
            }
            self.target_gone[other] = true;
            settle_moved(
                self.sources,
                &mut self.source_gone,
                self.conflicts,
                index,
                &self.targets[other],
                self.include_same,
            )
            .await?;
        }
        Ok(())
    }

    /// Drops the changes a pairing used up.
    fn finish(self) {
        let mut index = 0;
        self.sources.retain(|_| {
            index += 1;
            !self.source_gone[index - 1]
        });
        let mut index = 0;
        self.targets.retain(|_| {
            index += 1;
            !self.target_gone[index - 1]
        });
    }
}

/// `Vec`-returning drain over streaming `diff3`. Exists so callers
/// that need the whole set materialised (filesystem diff, merge, capi,
/// CLI, legacy handlers) keep the historical `DiffResult` contract.
pub async fn diff3_collect(
    repository: Arc<RepositoryContext>,
    base: Hash,
    source: Hash,
    target: Hash,
    path: Option<RelativePath>,
    include_same: bool,
) -> Result<DiffResult, StateError> {
    let (summary, items) = crate::util::collect_stream::collect_stream_with_summary(|tx| {
        diff3(repository, base, source, target, path, include_same, tx)
    })
    .await?;
    Ok(diff_result_from_summary_and_items(summary, items))
}

/// Re-assemble a `DiffResult` from a drained streaming `diff3` output.
/// Used by both `revision::diff3_collect` and `branch::diff3_collect`
/// after `collect_stream_with_summary` returns.
pub(crate) fn diff_result_from_summary_and_items(
    summary: Diff3Summary,
    items: Vec<DiffItem>,
) -> DiffResult {
    let mut diff = DiffResult {
        base: summary.base,
        source: summary.source,
        target: summary.target,
        changes: Vec::new(),
        conflicts: Vec::new(),
    };
    for item in items {
        match item {
            DiffItem::Change(c) => diff.changes.push(c),
            DiffItem::Conflict(pair) => diff.conflicts.push(*pair),
        }
    }
    diff
}

async fn is_last_change_merged(
    source_conflict: NodeChange,
    source_branch: BranchId,
    target_conflict: NodeChange,
    target_branch: BranchId,
    state_branch_point: Arc<State>,
) -> Result<bool, StateError> {
    lore_debug!(
        "{} find last modified source revision",
        source_conflict.path().as_str()
    );
    let (last_source_modified_revision, last_source_modified_revision_number) =
        find_last_modified_revision(&source_conflict, state_branch_point.clone()).await?;
    lore_debug!(
        "{} last modified source {} revision {} -> {}",
        source_conflict.path().as_str(),
        source_branch,
        last_source_modified_revision,
        last_source_modified_revision_number,
    );

    lore_debug!(
        "{} find last merged source {} -> target {} revision",
        source_conflict.path().as_str(),
        source_branch,
        target_branch,
    );
    if let Some((last_merged_from_source_revision, last_merged_from_source_revision_number)) =
        find_last_merged_revision(
            &target_conflict,
            source_branch,
            target_branch,
            state_branch_point.clone(),
        )
        .await?
    {
        lore_debug!(
            "{} last merged from source {} into target {} revision {} -> {}",
            source_conflict.path().as_str(),
            source_branch,
            target_branch,
            last_merged_from_source_revision,
            last_merged_from_source_revision_number,
        );

        if last_source_modified_revision_number <= last_merged_from_source_revision_number {
            lore_debug!(
                "Final change check for {} is MERGED - last source merged revision {}, last source modified revision {}",
                source_conflict.path().as_str(),
                last_merged_from_source_revision_number,
                last_source_modified_revision_number,
            );
            Ok(true)
        } else {
            lore_debug!(
                "Final change merge check for {} is UNMERGED - last source merged revision {}, last source modified revision {}",
                source_conflict.path().as_str(),
                last_merged_from_source_revision_number,
                last_source_modified_revision_number,
            );
            Ok(false)
        }
    } else {
        lore_debug!(
            "Final change merge check for {} is UNMERGED - no merged revision found",
            source_conflict.path().as_str()
        );
        Ok(false)
    }
}

async fn find_last_modified_revision(
    change: &NodeChange,
    state_branch_point: Arc<State>,
) -> Result<(Hash, u64), StateError> {
    if !change.to.mapping.node.is_valid_node_id() {
        // File was deleted from the target state, need to walk history to find it
        // TODO(mjansson): Figure out a better way to store this metadata around file deletions
        lore_debug!("Find last modified revision without to state, iterate revisions");

        let repository = change.from.mapping.repository.clone();
        let mut state_current = change.to.mapping.state.clone();
        let mut state_parent = state_current.clone();
        while state_current.revision_number() >= state_branch_point.revision_number() {
            if let Ok(node_link) = state_current
                .find_node_link(repository.clone(), change.path().as_str())
                .await
            {
                // If the node was existing in the current state, it means it was deleted in the previous (parent) revision
                lore_debug!(
                    "Found {} existing in revision {} - {}",
                    change.path().as_str(),
                    state_current.revision(),
                    state_current.revision_number()
                );
                if node_link.is_valid() {
                    lore_debug!(
                        "Last modified {} in parent revision {} - {}",
                        change.path().as_str(),
                        state_parent.revision(),
                        state_parent.revision_number()
                    );
                    return Ok((state_parent.revision(), state_parent.revision_number()));
                }
            }

            let parent_revision = state_current.parent_self();
            if parent_revision.is_zero() {
                break;
            }

            state_parent = state_current.clone();
            state_current = State::deserialize(repository.clone(), parent_revision).await?;
        }

        lore_debug!(
            "Did not find node {} last modified, using branch point revision {} - {}",
            change.path().as_str(),
            state_branch_point.revision(),
            state_branch_point.revision_number()
        );

        return Ok((
            state_branch_point.revision(),
            state_branch_point.revision_number(),
        ));
    };

    let repository = change.to.mapping.repository.clone();
    let state = change.to.mapping.state.clone();
    let node_id = change.to.mapping.node;

    let mut last_modified_revision = state.revision();
    let mut last_modified_revision_number = state.revision_number();

    // Check if node was modified in the LATEST revision
    if let Ok(Some(_node_delta)) = state.node_delta(repository.clone(), node_id).await {
        lore_debug!(
            "{} found last modified revision {} -> {} (HEAD)",
            change.path().as_str(),
            last_modified_revision,
            last_modified_revision_number
        );
    } else {
        lore_debug!(
            "{} not modified in HEAD, walk file history",
            change.path().as_str()
        );
        // Node was not modified in the target LATEST revision, find the
        // previous revision from file history block
        if let Ok(block) = state
            .block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(node_id))
            .await
        {
            let node = block.node(NodeFileMetadata::index(node_id));
            lore_debug!("{} node history {:?}", change.path().as_str(), node);
            if let Ok(state_modified) =
                state::State::deserialize(repository.clone(), node.revision[0]).await
            {
                last_modified_revision = state_modified.revision();
                last_modified_revision_number = state_modified.revision_number();
                lore_debug!(
                    "{} found last modified revision {} -> {}",
                    change.path().as_str(),
                    last_modified_revision_number,
                    last_modified_revision
                );
            } else {
                lore_warn!(
                    "Failed to deserialize last modified state when searching history for {}",
                    change.path().as_str()
                );
            }
        } else {
            lore_warn!(
                "Failed to deserialize file metadata block when searching history for {}",
                change.path().as_str()
            );
        }
    }

    Ok((last_modified_revision, last_modified_revision_number))
}

/// Finds the last revision of `source_branch` that the target branch merged the file of `change`
/// from, walking the file's history back from the target side of `change` to the branch point.
///
/// Each step reads the earlier revision's history at the node ID the history records for the file
/// there, as a revision need not number the file as its first parent does. The walk ends at a
/// revision that [adds the file without a merge](added_without_merge), the start revision included.
#[lore_macro::test_pub]
async fn find_last_merged_revision(
    change: &NodeChange,
    source_branch: BranchId,
    target_branch: BranchId,
    state_branch_point: Arc<State>,
) -> Result<Option<(Hash, u64)>, StateError> {
    let (state_start, mut node_current) = if change.to.mapping.node.is_valid_node_id() {
        (change.to.mapping.state.clone(), change.to.mapping.node)
    } else {
        // TODO(mjansson): Figure out a better way to store this metadata around file deletions
        lore_debug!("Find last merged revision without to state, iterate revisions");

        let repository = change.to.mapping.repository.clone();
        let mut state_current = change.to.mapping.state.clone();
        let mut state_parent = state_current.clone();
        loop {
            if let Ok(node_link) = state_current
                .find_node_link(repository.clone(), change.path().as_str())
                .await
            {
                lore_debug!(
                    "Found {} existing in revision {} - {} for last merged",
                    change.path().as_str(),
                    state_current.revision(),
                    state_current.revision_number()
                );

                // If the node was existing in the current state, it means it was deleted in the previous (parent) revision
                if node_link.is_valid() {
                    lore_debug!(
                        "Using {} parent revision {} - {} as last merged start revision",
                        change.path().as_str(),
                        state_parent.revision(),
                        state_parent.revision_number()
                    );
                    break (state_parent, node_link.node);
                }
            }

            if state_current.revision_number() <= state_branch_point.revision_number() {
                lore_debug!(
                    "Did not find node {} last modified, no merged revision",
                    change.path().as_str()
                );
                return Ok(None);
            }

            lore_debug!(
                "Node {} not present in revision {} - {} for last merged start, move to parent",
                change.path().as_str(),
                state_current.revision(),
                state_current.revision_number()
            );

            state_parent = state_current.clone();
            state_current =
                State::deserialize(repository.clone(), state_current.parent_self()).await?;
        }
    };

    // Now walk the history for the file from the target branch LATEST revision and
    // see if we arrive on source branch along any merge, and if that target revision
    // is later than the last modified revision we identified from the source branch file history
    let repository = change.to.mapping.repository.clone();
    if node_current.is_valid_node_id()
        && let Some(delta) = state_start
            .node_delta(repository.clone(), node_current)
            .await?
        && added_without_merge(delta.action, delta.flags)
    {
        return Ok(None);
    }
    let mut state_current = state_start.clone();
    while state_current.revision_number() > state_branch_point.revision_number()
        && node_current.is_valid_node_id()
    {
        let node_index = NodeFileMetadataBlock::index(node_current);
        if let Ok(block) = state_current
            .block_file_metadata(repository.clone(), node_index)
            .await
        {
            let node = block.node(NodeFileMetadata::index(node_current));
            if !node.revision[1].is_zero() {
                // TODO(mjansson): Follow merges from other branches bounded by revision number
                // to also catch merges that happen through other branches
                lore_debug!(
                    "{} branch {} revision {} node {} history is a merge, {:?} check if other parent {} is from branch {}",
                    change.path().as_str(),
                    target_branch,
                    state_current.revision(),
                    node_current,
                    node,
                    node.revision[1],
                    source_branch
                );
                if let Ok(state_other) =
                    state::State::deserialize(repository.clone(), node.revision[1]).await
                {
                    if let Ok(state_metadata) =
                        state_other.revision_metadata(repository.clone()).await
                    {
                        if state_metadata.branch == source_branch {
                            lore_debug!(
                                "{} revision merged from branch {} revision {} -> {}",
                                change.path().as_str(),
                                source_branch,
                                state_other.revision(),
                                state_other.revision_number()
                            );
                            return Ok(Some((
                                state_other.revision(),
                                state_other.revision_number(),
                            )));
                        } else {
                            lore_debug!(
                                "{} revision {} -> {} is NOT a merge from branch {}",
                                change.path().as_str(),
                                state_other.revision(),
                                state_other.revision_number(),
                                source_branch,
                            );
                        }
                    } else {
                        lore_warn!("Failed to deserialize other parent state metadata");
                    }
                } else {
                    lore_warn!(
                        "Failed to deserialize other parent state {}",
                        node.revision[1]
                    );
                }
            } else {
                lore_debug!(
                    "{} revision {} node history {:?} is not a merge, continue search in branch node history {}",
                    change.path().as_str(),
                    state_current.revision_number(),
                    node,
                    node.revision[0]
                );
            }

            if added_without_merge(node.action[0], node.flags[0]) {
                break;
            }

            if let Ok(state_previous) =
                state::State::deserialize(repository.clone(), node.revision[0]).await
            {
                if state_previous.revision_number() < state_branch_point.revision_number() {
                    lore_debug!(
                        "{} stop iterating revisions, reached revision {} < branch point modified revision {}",
                        change.path().as_str(),
                        state_previous.revision_number(),
                        state_branch_point.revision_number()
                    );
                    break;
                } else {
                    lore_debug!(
                        "{} step to revision {} -> {}",
                        change.path().as_str(),
                        state_previous.revision_number(),
                        state_previous.revision()
                    );
                    state_current = state_previous;
                    node_current = node.node[0];
                }
            } else {
                lore_warn!("Failed to deserialize state when walking source branch file history");
                break;
            }
        } else {
            lore_warn!(
                "Failed to deserialize file metadata block when walking source branch file history"
            );
            break;
        }
    }

    Ok(None)
}

/// Whether `action` and `flags` record a file added in its revision without a merge, which leaves
/// no history of the file before that revision: a node reused for the file still holds the history
/// of the file that had it before.
fn added_without_merge(action: u16, flags: u16) -> bool {
    action == FileAction::Add as u16 && flags & change::Flags::Merge.bits() == 0
}

pub struct TreeResult {
    /// list of paths at this level
    pub paths: Vec<TreePath>,
}

pub async fn tree(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    path: RelativePath,
    max_depth: usize,
    can_read: crate::state::CanReadRepository,
) -> Result<TreeResult, StateError> {
    lore_debug!(
        "Gathering tree in repository {} revision: {} path: {}",
        repository.id,
        revision,
        path.as_str()
    );
    let state = State::deserialize(repository.clone(), revision).await?;
    let paths = gather_tree_paths(state, repository, path, max_depth, can_read).await?;
    Ok(TreeResult { paths })
}

/// What a revision specifier names on a branch.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub enum LoreRevisionResolveTarget {
    /// A revision by its number on the branch.
    Number = 1,
    /// The latest revision of the branch.
    Latest = 2,
    /// A revision by its whole hash signature, taken on the branch.
    Signature = 3,
}

/// Information about a revision being resolved on a branch.
///
/// Reported before the lookup runs, since finding a revision by number can walk
/// a long stretch of history. A specifier that names no branch resolves to
/// itself and reports nothing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionResolveEventData {
    /// Repository identifier in which repository
    pub repository: RepositoryId,
    /// Identifier of the branch on which resolution is being done
    pub branch: BranchId,
    /// What the specifier names on the branch
    pub target: LoreRevisionResolveTarget,
    /// The revision number being resolved, zero unless `target` is `Number`
    pub revision_number: u64,
    /// The revision being resolved, zero unless `target` is `Signature`
    pub revision: Hash,
    /// Resolving using remote data
    pub remote: u8,
    /// Resolving using local data
    pub local: u8,
}

#[derive(Default)]
pub enum ResolveSearchLocation {
    #[default]
    RemoteOrLocal,
    Remote,
    Local,
}

/// Reduces a fallible result to an optional value, propagating the failures
/// `propagate` selects only when `backup_source` cannot answer in its place.
///
/// `backup_source` is a latest another source has already produced. A usable one
/// means the resolution can still succeed without this result, so every failure
/// here becomes `Ok(None)` and the caller falls through to that source. With no
/// usable `backup_source` this result is the last one that can answer, and a
/// failure `propagate` selects has to reach the caller rather than arrive as a
/// missing revision.
///
/// A zero `backup_source` counts as none, leaving this result as the sole
/// answer.
#[track_caller]
fn absent_unless_sole_source<T, Source: ErrorSet>(
    result: Result<T, Source>,
    backup_source: Option<Hash>,
    propagate: impl FnOnce(&Source) -> bool,
    context: &str,
) -> Result<Option<T>, StateError> {
    if backup_source.is_some_and(|hash| !hash.is_zero()) {
        return Ok(result.ok());
    }
    absent_unless(result, propagate, context)
}

/// A revision resolved from a specifier, with the branch it is taken on.
pub struct ResolvedRevision {
    /// The revision the specifier resolved to.
    pub revision: Hash,
    /// The branch [`Self::revision`] is taken on: the branch the specifier named
    /// where the revision is the one that branch was created at, and the branch
    /// the revision itself records otherwise.
    pub branch: BranchId,
}

/// The branch `revision` records, which is the branch it was committed on.
async fn recorded_branch(
    repository: Arc<RepositoryContext>,
    revision: Hash,
) -> Result<BranchId, StateError> {
    let read = State::deserialize(repository.clone(), revision)
        .await?
        .recorded_branch(repository);
    read.await
}

/// The branch `revision` is taken on, given the branch a specifier named and the
/// metadata that branch resolved to.
///
/// A revision records the branch it was committed on, which answers for every
/// revision a branch holds but one: the revision the branch was created at
/// belongs to the branch that was branched from, being the last one the two
/// share. Naming a branch is what picks that revision's other side, and it is
/// answered from the branch's own metadata, so it is taken first and costs no
/// read of the revision.
async fn resolved_branch(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    named: Option<(BranchId, Hash)>,
) -> Result<BranchId, StateError> {
    if let Some((branch, branch_metadata)) = named {
        let metadata = if branch_metadata.is_zero() {
            branch::metadata(repository.clone(), branch).await
        } else {
            branch::load_metadata(repository.clone(), branch_metadata).await
        }
        .forward::<StateError>("loading branch metadata")?;

        if branch::stack(&metadata)
            .first()
            .is_some_and(|branch_point| branch_point.revision == revision)
        {
            return Ok(branch);
        }
    }

    // A revision taken from a cached revision list carries no metadata blob to
    // read a branch from, which is ordinary rather than damage, so the branch a
    // specifier named answers where the revision cannot answer for itself.
    Ok(absent_unless::<_, _, StateError>(
        recorded_branch(repository, revision).await,
        StateError::is_slow_down,
        "reading the branch a revision records",
    )?
    .unwrap_or_else(|| named.map(|(branch, _)| branch).unwrap_or_default()))
}

/// True when `signature` is a complete revision hash signature.
fn is_hash_signature(signature: &str) -> bool {
    signature.len() == HASH_STRING_LENGTH && signature.chars().all(|c| c.is_ascii_hexdigit())
}

/// True when `signature` is a revision number rather than a hash signature.
///
/// A signature of the full hash length is a hash even when it is all digits:
/// the two forms only overlap at that one length, and a hash is what a caller
/// pasting 64 characters means.
fn is_revision_number(signature: &str) -> bool {
    !signature.is_empty()
        && signature.len() != HASH_STRING_LENGTH
        && signature.chars().all(|c| c.is_ascii_digit())
}

/// The failure for a specifier that names neither a complete hash signature nor
/// a revision number.
///
/// Hex digits alone make a hash signature of the wrong length, which is a form
/// that is not supported rather than a revision that could not be found:
/// reporting it as missing would send the caller looking for a revision they
/// never named.
fn unresolvable(signature: &str, original_input: &str) -> StateError {
    if !signature.is_empty() && signature.chars().all(|c| c.is_ascii_hexdigit()) {
        lore_debug!("Partial revision hash signature {signature:?} is not supported");
        return NotSupported {
            operation: format!(
                "partial revision hash signature {signature:?} - give the whole \
                 {HASH_STRING_LENGTH} character signature, a revision number, or \
                 <branch>@<revision number>"
            ),
        }
        .into();
    }

    lore_debug!("Malformed revision specifier {signature:?}");
    RevisionNotFound {
        revision: original_input.to_string(),
    }
    .into()
}

/// The branch this instance is on.
async fn current_anchor_branch(
    repository: &Arc<RepositoryContext>,
) -> Result<BranchId, StateError> {
    let (_current_revision, current_branch) = crate::instance::load_current_anchor(repository)
        .await
        .forward::<StateError>("Failed deserializing anchor")?;
    Ok(current_branch)
}

/// Reads a revision number [`is_revision_number`] has already accepted,
/// reporting one too large to hold as a revision that was not found.
fn parse_revision_number(signature: &str, original_input: &str) -> Result<u64, StateError> {
    signature.parse::<u64>().map_err(|err| {
        lore_debug!("Invalid revision number {signature:?}: {err}");
        StateError::from(RevisionNotFound {
            revision: original_input.to_string(),
        })
    })
}

/// Reads a hash signature [`is_hash_signature`] has already accepted.
fn parse_hash_signature(signature: &str, original_input: &str) -> Result<Hash, StateError> {
    Hash::from_str(signature).map_err(|err| {
        lore_debug!("Malformed revision signature {signature:?}: {err}");
        StateError::from(RevisionNotFound {
            revision: original_input.to_string(),
        })
    })
}

/// What the part of a `[branch]@<target>` specifier after the `@` names.
enum BranchTarget {
    /// The branch's latest revision.
    Latest,
    /// A revision by its number on the branch.
    Number(u64),
    /// A revision by its whole hash signature, taken on the branch.
    Signature(Hash),
}

/// What the part of `[branch]@<target>` after the `@` names.
///
/// Read before the branch it belongs to is looked up, since it is a property of
/// the input alone: a specifier that can never resolve is refused without
/// spending a store or remote round trip on the branch first, and names the
/// part of itself that is wrong.
fn branch_target(suffix: &str, original_input: &str) -> Result<BranchTarget, StateError> {
    if suffix.eq_ignore_ascii_case("LATEST") || suffix.eq_ignore_ascii_case("HEAD") {
        Ok(BranchTarget::Latest)
    } else if is_revision_number(suffix) {
        Ok(BranchTarget::Number(parse_revision_number(
            suffix,
            original_input,
        )?))
    } else if is_hash_signature(suffix) {
        Ok(BranchTarget::Signature(parse_hash_signature(
            suffix,
            original_input,
        )?))
    } else {
        Err(unresolvable(suffix, original_input))
    }
}

/// The latests a lookup on `branch` can anchor on, as `(remote, local)`.
///
/// A remote that is throttled or can't be reached answers no latest, which the local latest covers.
/// A remote that refuses the caller fails the lookup.
async fn branch_latests(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    should_search_remote: bool,
    should_search_local: bool,
) -> Result<(Option<Hash>, Option<Hash>), StateError> {
    let remote_latest = if should_search_remote
        && let Some(remote) = absent_unless_refused::<_, _, StateError>(
            repository.remote().await,
            "connecting to the remote",
        )? {
        absent_unless_refused::<_, _, StateError>(
            branch::load_remote_latest(remote, repository.id, branch).await,
            "loading remote branch latest",
        )?
    } else {
        None
    };

    let local_latest = if should_search_local {
        // The remote latest stands in for this one: a lookup answers from it
        // alone when the local one is absent.
        absent_unless_sole_source(
            branch::load_latest(repository.clone(), branch).await,
            remote_latest,
            branch::BranchError::is_slow_down,
            "loading branch latest",
        )?
    } else {
        None
    };

    Ok((remote_latest, local_latest))
}

/// The latest revision of `branch`, zero when neither side has one.
///
/// The two sides only disagree between syncs. A remote that is ahead and
/// convergent has revisions this instance has not seen yet and answers for
/// both; anything else keeps the local one, which is the revision the working
/// tree was realized against.
async fn branch_latest(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    should_search_remote: bool,
    should_search_local: bool,
) -> Result<Hash, StateError> {
    event::LoreEvent::RevisionResolve(LoreRevisionResolveEventData {
        repository: repository.id,
        branch,
        target: LoreRevisionResolveTarget::Latest,
        revision_number: 0,
        revision: Hash::default(),
        remote: should_search_remote.into(),
        local: should_search_local.into(),
    })
    .send();

    let (remote_latest, local_latest) = branch_latests(
        repository.clone(),
        branch,
        should_search_remote,
        should_search_local,
    )
    .await?;

    let local = local_latest.filter(|head| !head.is_zero());
    let remote = remote_latest.filter(|head| !head.is_zero());
    Ok(match (local, remote) {
        (Some(local), Some(remote)) if local == remote => local,
        (Some(local), Some(remote)) => {
            match find_branch_point(repository.clone(), remote, local).await {
                Ok((_branch_point, remote_history, local_history)) => {
                    if local_history.is_empty() && !remote_history.is_empty() {
                        lore_debug!(
                            "Remote latest {remote} is ahead of local latest {local} and convergent, using remote"
                        );
                        remote
                    } else {
                        local
                    }
                }
                Err(err) => {
                    lore_debug!(
                        "Failed to find branch point between local {local} and remote {remote}, falling back to local: {err}"
                    );
                    local
                }
            }
        }
        (Some(local), None) => local,
        (None, Some(remote)) => remote,
        (None, None) => Hash::default(),
    })
}

/// The revision numbered `revision_number` on `branch`, zero when the branch
/// has no such revision.
///
/// The remote answers from its revision list where it can, since that costs one
/// request against a walk of every revision in between. The number the list
/// labels an item with picks it, and the item's state, which its signature fixes,
/// confirms it, as [`find::verified_number`] reads it. Where no item is picked or
/// confirmed, whichever latest the list did not answer from anchors that walk.
///
/// Numbering starts at one, so zero names no revision and is refused before any
/// lookup rather than after walking the whole history to find nothing.
async fn resolve_revision_number(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    should_search_remote: bool,
    should_search_local: bool,
) -> Result<Hash, StateError> {
    if revision_number == 0 {
        lore_debug!("Revision number 0 names no revision");
        return Ok(Hash::default());
    }

    let (remote_latest, local_latest) = branch_latests(
        repository.clone(),
        branch,
        should_search_remote,
        should_search_local,
    )
    .await?;

    let mut revision = Hash::default();

    if should_search_remote
        && let Some(connection) = absent_unless_refused::<_, _, StateError>(
            repository.remote().await,
            "connecting to the remote",
        )?
        && let Some(revision_service) = absent_unless_refused::<_, _, StateError>(
            connection.revision(repository.id).await,
            "connecting to the remote revision service",
        )?
        && let Some(response) = absent_unless_refused::<_, _, StateError>(
            revision_service
                .revision_list(
                    RevisionListIdentifier {
                        branch,
                        number: revision_number,
                    }
                    .into(),
                )
                .await,
            "listing the remote revisions",
        )?
    {
        // A page the remote serves from a cached segment holds the number at any
        // position, not first.
        if let Some(item) = response
            .items
            .iter()
            .find(|item| item.number == revision_number)
            && find::verified_number(item) == Some(revision_number)
        {
            revision = item.signature;
            lore_debug!("response revision {}", revision);
        }
        find::cache_revision_list_states(repository.clone(), &response.items).await;
    }

    if revision.is_zero()
        && let Some(head) = remote_latest
    {
        // The local latest stands in for this search: the local-anchored
        // attempt below runs next. A zero one cannot — a search anchored on it
        // finds nothing.
        let found = absent_unless_sole_source(
            find::revision_by_number(repository.clone(), branch, head, revision_number).await,
            local_latest,
            find::FindError::is_slow_down,
            "finding revision by number",
        )?;
        if let Some(found_revision) = found {
            revision = found_revision;
        }
    }
    if revision.is_zero()
        && let Some(head) = local_latest
        && let Some(found_revision) = absent_unless::<_, _, StateError>(
            find::revision_by_number(repository.clone(), branch, head, revision_number).await,
            find::FindError::is_slow_down,
            "finding revision by number",
        )?
    {
        revision = found_revision;
    }

    Ok(revision)
}

/// Resolves a revision specifier to the revision it names.
///
/// See [`resolve_in_branch`] for the specifier forms. This reads nothing of the
/// revision itself, so a caller that only needs the revision does not pay for
/// the branch it is taken on.
pub(crate) async fn resolve(
    repository: Arc<RepositoryContext>,
    signature: impl AsRef<str>,
    search_location: ResolveSearchLocation,
) -> Result<Hash, StateError> {
    resolve_revision(repository, signature, None, search_location)
        .await
        .map(|(revision, _named_branch)| revision)
}

/// Boxed version of [`resolve`] for cross-crate use.
pub fn resolve_boxed(
    repository: Arc<RepositoryContext>,
    signature: impl AsRef<str> + Send,
    search_location: ResolveSearchLocation,
) -> crate::BoxFuture<'static, Result<Hash, StateError>> {
    let signature = signature.as_ref().to_owned();
    Box::pin(resolve(repository, signature, search_location))
}

/// Resolves a revision specifier to the revision it names and the branch it is
/// taken on.
///
/// A revision is named by its whole hash signature, by `[branch]@<number>`, by
/// `[branch]@LATEST`, or by `<branch>@<hash>`, each taking an optional
/// `~<count>` suffix walking that many parent revisions (a bare `~` walks one).
/// The `@` is optional, a target given without it applying to the branch the
/// instance is on. Partial signatures are not resolved, which is what leaves a
/// number free to be read as a revision number.
///
/// A revision identifies the branch it was created on. A branch point can also
/// identify the child branch by naming that child branch.
pub async fn resolve_in_branch(
    repository: Arc<RepositoryContext>,
    signature: impl AsRef<str>,
    search_location: ResolveSearchLocation,
) -> Result<ResolvedRevision, StateError> {
    let (revision, named_branch) =
        resolve_revision(repository.clone(), signature, None, search_location).await?;

    // Taken for the revision reached rather than the one named, since a `~<count>`
    // walk can cross the branch point into the branch this one was created from.
    let branch = resolved_branch(repository, revision, named_branch).await?;

    Ok(ResolvedRevision { revision, branch })
}

/// The revision a specifier names, and the branch it named to find it in.
///
/// See [`resolve_in_branch`] for the specifier forms. A target given without a
/// branch is taken on `implied_branch`, or on the branch the instance is on
/// where that is `None`. The branch is reported as named rather than as the
/// revision is taken on, which reads the revision.
pub(crate) async fn resolve_revision(
    repository: Arc<RepositoryContext>,
    signature: impl AsRef<str>,
    implied_branch: Option<BranchId>,
    search_location: ResolveSearchLocation,
) -> Result<(Hash, Option<(BranchId, Hash)>), StateError> {
    let original_input = signature.as_ref();
    let mut revision;
    // The branch the specifier named, with the metadata it resolved to, which
    // answers for the revision it was created at.
    let mut named_branch = None;

    let (signature, offset) = if let Some(split) = original_input.split_once("~") {
        let prefix = split.0;
        let suffix = split.1;
        if suffix.is_empty() {
            (prefix, Some(1))
        } else {
            let offset: u64 = suffix.parse::<u64>().map_err(|err| {
                lore_debug!("Malformed revision offset {suffix:?}: {err}");
                StateError::from(RevisionNotFound {
                    revision: original_input.to_string(),
                })
            })?;

            (prefix, Some(offset))
        }
    } else {
        (original_input, None)
    };

    lore_debug!("Resolving signature {signature}, offset {offset:?}");

    let (should_search_remote, should_search_local) = match search_location {
        ResolveSearchLocation::RemoteOrLocal => (true, true),
        ResolveSearchLocation::Remote => (true, false),
        ResolveSearchLocation::Local => (false, true),
    };

    if is_hash_signature(signature) {
        revision = parse_hash_signature(signature, original_input)?;
        lore_debug!("Resolved direct hash signature: {revision}");
    } else {
        // An empty prefix names no branch, which is what makes the `@`
        // optional: no branch reads as a target, so a specifier carrying one
        // and a specifier carrying none cannot be confused.
        let (prefix, suffix) = signature.split_once("@").unwrap_or(("", signature));
        let target = branch_target(suffix, original_input)?;

        lore_debug!("Resolving branch {prefix} signature {signature}");
        let (branch, branch_metadata) = if prefix.is_empty() {
            let branch = match implied_branch {
                Some(branch) => branch,
                None => current_anchor_branch(&repository).await?,
            };
            (branch, Hash::default())
        } else {
            let branch_status = branch::resolve(repository.clone(), prefix)
                .await
                .map_matched_err("Invalid branch specifier", |m| match m {
                    branch::MatchedBranchError::BranchNotFound(_) => {
                        StateError::from(RevisionNotFound {
                            revision: original_input.to_string(),
                        })
                    }
                    other => other.forward::<StateError>("resolving branch for revision"),
                })?;
            (branch_status.id, branch_status.metadata)
        };
        named_branch = Some((branch, branch_metadata));

        match target {
            BranchTarget::Latest => {
                revision = branch_latest(
                    repository.clone(),
                    branch,
                    should_search_remote,
                    should_search_local,
                )
                .await?;
            }
            BranchTarget::Number(revision_number) => {
                event::LoreEvent::RevisionResolve(LoreRevisionResolveEventData {
                    repository: repository.id,
                    branch,
                    target: LoreRevisionResolveTarget::Number,
                    revision_number,
                    revision: Hash::default(),
                    remote: should_search_remote.into(),
                    local: should_search_local.into(),
                })
                .send();
                revision = resolve_revision_number(
                    repository.clone(),
                    branch,
                    revision_number,
                    should_search_remote,
                    should_search_local,
                )
                .await?;
            }
            BranchTarget::Signature(signature) => {
                event::LoreEvent::RevisionResolve(LoreRevisionResolveEventData {
                    repository: repository.id,
                    branch,
                    target: LoreRevisionResolveTarget::Signature,
                    revision_number: 0,
                    revision: signature,
                    remote: should_search_remote.into(),
                    local: should_search_local.into(),
                })
                .send();

                revision = signature;
            }
        }

        if !revision.is_zero() {
            lore_debug!("Resolved to branch {branch} revision {revision}");
        }
    }

    if revision.is_zero() {
        return Err(RevisionNotFound {
            revision: original_input.to_string(),
        }
        .into());
    }

    if let Some(offset) = offset {
        let mut counter = offset;
        while counter > 0 {
            let state = State::deserialize(repository.clone(), revision).await?;
            let parent = state.parent_self();

            if parent.is_zero() {
                return Err(RevisionNotFound {
                    revision: original_input.to_string(),
                }
                .into());
            }

            revision = parent;
            counter -= 1;
        }
    }

    Ok((revision, named_branch))
}
