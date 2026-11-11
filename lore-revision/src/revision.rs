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
use smallvec::SmallVec;
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
use crate::state::TreeWalk;
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
            renamed_alike,
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

        let won_mounts = won_mounts(&target_mounts, &source_changes, &mut conflicting_mounts);
        if !won_mounts.is_empty() {
            joined_changes.retain(|change| !is_below_any(&won_mounts, change.path()));
            target_changes
                .retain(|change| !won_mounts.iter().any(|mount| mount.covers(change.path())));
        }
        drop(won_mounts);

        pair_moved_nodes(
            &mut joined_changes,
            &mut target_changes,
            target_moves,
            renamed_alike,
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

/// The paths where one side of a 3-way diff replaced a node with one of another type, and the other
/// made a change there or below that cannot stand beside the replacement.
///
/// Each of `sides` holds one side's replacements, the other side's, each sorted by path, and the
/// other side's changes the join left to apply. A replacement conflicts where a pair in
/// `conflicts` stands at or below it, or where a change of the other side is left there. Where
/// both sides put the same type of node there, the paths below it merge one by one. A pair in
/// `conflicts` where the sides put nodes of different types there conflicts the same way, though
/// neither replaced anything.
#[lore_macro::test_pub]
fn conflicting_replacements(
    sides: [(&[Replacement], &[Replacement], &[NodeChange]); 2],
    conflicts: &[(NodeChange, NodeChange)],
) -> Vec<RelativePath> {
    let mut paths = Vec::new();
    for (source, target) in conflicts {
        if change::node_type_clash(source, target) {
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
    let source_moves = relocated_indices(source_changes, |_| true);

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
                if changes_mode_alone(&target_change) {
                    continue;
                }
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

/// Whether `change` puts another file in place of its node, holding the content and the executable
/// bit the node held: a walk reports one where only the file ID differs, at the path the node stood
/// at or where a rename of a directory above it carried the node.
fn is_file_id_only_churn(change: &NodeChange) -> bool {
    change.action != FileAction::Delete
        && (change.relocated_from().is_none() || is_carried(change))
        && change.from.address.context != change.to.address.context
        && !change.from.address.hash.is_zero()
        && change.from.address.hash == change.to.address.hash
        && !crate::util::fs::mode_changed(change.from.mode, change.to.mode)
}

/// The path a change leaves its node away from, and the empty path, which orders first, for a
/// change leaving its node where it stood.
fn relocation_source(change: &NodeChange) -> &str {
    change.relocated_from().map_or("", RelativePath::as_str)
}

/// The indices of the changes `select` picks among those of `changes` that leave their node at
/// another path, sorted by the path each leaves.
fn relocated_indices(changes: &[NodeChange], select: impl Fn(&NodeChange) -> bool) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..changes.len())
        .filter(|&index| changes[index].relocated_from().is_some() && select(&changes[index]))
        .collect();
    indices.sort_unstable_by(|&lhs, &rhs| {
        relocation_source(&changes[lhs]).cmp(relocation_source(&changes[rhs]))
    });
    indices
}

/// The index of the change standing at `path` among `changes`, sorted by path, or the index it
/// would stand at.
fn index_at(changes: &[NodeChange], path: &str) -> Result<usize, usize> {
    changes.binary_search_by(|change| change.path().as_str().cmp(path))
}

/// The indices of the changes in `changes`, sorted by path, that stand below `ancestor`.
fn indices_below(changes: &[NodeChange], ancestor: &str) -> std::ops::Range<usize> {
    let order = |change: &NodeChange| below_order(change.path().as_str(), ancestor);
    changes.partition_point(|change| order(change).is_lt())
        ..changes.partition_point(|change| order(change).is_le())
}

/// How `path` orders against the paths below `ancestor`, in the order of paths: `Less` where it
/// orders before every one of them, `Equal` where it is one, and `Greater` where it orders after.
fn below_order(path: &str, ancestor: &str) -> std::cmp::Ordering {
    let path = path.as_bytes();
    let (prefix, rest) = path.split_at(path.len().min(ancestor.len()));
    prefix.cmp(ancestor.as_bytes()).then_with(|| {
        rest.first()
            .map_or(std::cmp::Ordering::Less, |b| b.cmp(&b'/'))
    })
}

/// How `candidate` orders against `spelled` with the name at each range of `names`, in order,
/// replaced by the name given with it: bytes compared as `str` orders them.
#[lore_macro::test_pub]
fn cmp_respelled(
    candidate: &str,
    spelled: &str,
    names: &[(usize, usize, &str)],
) -> std::cmp::Ordering {
    let mut rest = candidate.as_bytes();
    let mut copied = 0;
    for &(start, end, name) in names {
        for piece in [&spelled[copied..start], name] {
            if let Some(order) = take_piece(&mut rest, piece) {
                return order;
            }
        }
        copied = end;
    }
    take_piece(&mut rest, &spelled[copied..]).unwrap_or(if rest.is_empty() {
        std::cmp::Ordering::Equal
    } else {
        std::cmp::Ordering::Greater
    })
}

/// Takes `piece` off the start of `rest` where `rest` starts with it, or answers how `rest`
/// orders against a string starting with `piece`.
fn take_piece(rest: &mut &[u8], piece: &str) -> Option<std::cmp::Ordering> {
    let piece = piece.as_bytes();
    let length = rest.len().min(piece.len());
    match rest[..length].cmp(&piece[..length]) {
        std::cmp::Ordering::Equal if rest.len() < piece.len() => Some(std::cmp::Ordering::Less),
        std::cmp::Ordering::Equal => {
            *rest = &rest[length..];
            None
        }
        order => Some(order),
    }
}

/// Whether `change` changes what a node other than a directory holds: a file's content or a link's
/// pin. A file's executable bit merges on its own, as [`change::merged_mode`] gives it.
fn edits(change: &NodeChange) -> bool {
    !change.to.flags.is_directory() && change.from.address.hash != change.to.address.hash
}

/// Whether the two sides' changes of one file or link both change what it holds, a file's content
/// or a link's pin, and leave it different.
fn edits_both(source: &NodeChange, target: &NodeChange) -> bool {
    let holds = NodeFlags::File | NodeFlags::Link;
    source.to.flags.intersects(holds)
        && target.to.flags.intersects(holds)
        && edits(source)
        && edits(target)
        && source.to.address.hash != target.to.address.hash
}

/// The target's mounts, `target_mounts`, that win against the source's changes below them: those
/// below which the source changes only the executable bits of files, which give way to the mount
/// as to a delete. Adds to `conflicting_mounts` each mount below which the source changes anything
/// else.
fn won_mounts(
    target_mounts: &[RelativePath],
    source_changes: &[NodeChange],
    conflicting_mounts: &mut Vec<RelativePath>,
) -> Vec<RelativePath> {
    let mut won = Vec::new();
    for mount in target_mounts {
        let mut below = source_changes
            .iter()
            .filter(|change| is_below(mount, change.path()));
        let Some(first) = below.next() else {
            continue;
        };
        if changes_mode_alone(first) && below.all(changes_mode_alone) {
            won.push(mount.clone());
        } else {
            push_unique_path(conflicting_mounts, mount);
        }
    }
    won
}

/// Whether `path` stands below one of `mounts`.
fn is_below_any(mounts: &[RelativePath], path: &RelativePath) -> bool {
    mounts.iter().any(|mount| is_below(mount, path))
}

/// Whether `change` changes a file's executable bit and nothing else: the file stays where it
/// stood, or where a rename of a directory above it carried it, with the content it held.
fn changes_mode_alone(change: &NodeChange) -> bool {
    change.action == FileAction::Keep && !edits(change) && change.changes_mode()
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

/// Whether the two changes, which move one node from one path, put it at one place in the merged
/// tree: one name in one directory, which the two spell as their sides do, ignoring case. The two
/// spellings are one where the renames of the directories above it agree.
fn moves_to_one_place(source: &NodeChange, target: &NodeChange) -> bool {
    source.path().as_lowercase_str() == target.path().as_lowercase_str()
        && source.path().name() == target.path().name()
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

/// Puts `change`, the source's change of a node the target changed too, onto the target's node:
/// its `from` side where the target holds the node, its `to` side at `at`, holding what the
/// target's node holds where `takes_content` says, and the executable bit `mode` where the merge
/// gives one.
fn onto_target_node(
    change: &mut NodeChange,
    target: &NodeChange,
    at: &RelativePath,
    takes_content: bool,
    mode: Option<u16>,
) {
    change.from = on_target(&change.from, &target.to, target.path());
    if takes_content {
        change.to = on_target(&change.to, &target.to, at);
    } else {
        change.to.mapping.path = at.clone();
    }
    if let Some(mode) = mode {
        change.to.mode = mode;
    }
}

/// What settling a source change with the target's change of its node leaves of the source's.
enum Settled {
    /// The source's change stands, as the settling left it.
    Stands,
    /// The source's change gives way to the target's, with no conflict.
    GivesWay,
    /// The two conflict, and the pair is recorded as a conflict.
    Conflict,
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

/// The executable bit [`change::merged_mode`] gives the file `source` and `target` both leave, and
/// whether it is another bit than `target` leaves it with.
fn mode_against_target(source: &NodeChange, target: &NodeChange) -> (Option<u16>, bool) {
    let mode = change::merged_mode(source, target);
    let differs = mode.is_some_and(|mode| crate::util::fs::mode_changed(mode, target.to.mode));
    (mode, differs)
}

/// What is left to apply of the source's move where the target made the same move: the source's
/// edit, as a change of the node where the target holds it, standing where the source's change
/// stands, with the executable bit the merge gives it. `None` where the source left the node as it
/// was or as the target holds it, or moved a directory, unless `include_same` asks for the changes
/// the two sides make alike.
fn onto_target_move(
    source: &NodeChange,
    target: &NodeChange,
    include_same: bool,
) -> Option<NodeChange> {
    let edited = edits(source) && source.to.address.hash != target.to.address.hash;
    let (mode, mode_differs) = mode_against_target(source, target);
    if !edited && !mode_differs && !include_same {
        return None;
    }
    let mut change = source.clone();
    change.action = FileAction::Keep;
    onto_target_node(&mut change, target, source.path(), !edited, mode);
    Some(change)
}

/// What becomes of the source's change of a path where the target's change stands at the same
/// path. `Stands` holds the change by value: a box would cost an allocation for each change that
/// stands.
#[allow(clippy::large_enum_variant)]
enum AtPath {
    /// The two conflict.
    Conflict,
    /// The source's change gives way to the target's, which leaves what the merge leaves there.
    GivesWay,
    /// The source's change stands, as this change.
    Stands(NodeChange),
}

/// Settles `source` with `target`, the target's change at the path `source` stands at. A change of
/// a file's executable bit alone gives the bit to the file the other side's change leaves, and
/// gives way to a delete or a replacement of the file. Two changes of a link that leave it at one
/// pin give way to the target's, which leaves what each mounts to a link merge where one runs. Any
/// other pair conflicts where [`is_conflict`] says so, and leaves the file the bit
/// [`change::merged_mode`] gives it, on the target's node where the two leave one content, so the
/// realize finds the file holding it. A pair that leaves the path alike lets the source's change
/// stand only for `include_same`.
async fn settle_at_path(
    source: &NodeChange,
    target: &NodeChange,
    include_same: bool,
) -> Result<AtPath, StateError> {
    if source.to.is_link() && target.to.is_link() && source.to.address == target.to.address {
        return Ok(AtPath::GivesWay);
    }
    let (mode, mode_differs) = mode_against_target(source, target);
    if changes_mode_alone(source) {
        let Some(mode) = mode.filter(|_| mode_differs || include_same) else {
            return Ok(AtPath::GivesWay);
        };
        let mut change = source.clone();
        onto_target_node(&mut change, target, target.path(), true, Some(mode));
        return Ok(AtPath::Stands(change));
    }
    if !changes_mode_alone(target) {
        if is_conflict(source, target, true).await? {
            return Ok(AtPath::Conflict);
        }
        if !mode_differs && !include_same {
            return Ok(AtPath::GivesWay);
        }
    }
    let mut change = source.clone();
    if mode.is_some() && source.to.address.hash == target.to.address.hash {
        onto_target_node(&mut change, target, source.path(), true, mode);
    } else if let Some(mode) = mode {
        change.to.mode = mode;
    }
    Ok(AtPath::Stands(change))
}

/// Whether `change` changes its node where it stands, at another path than the node stood at only
/// because a rename of a directory above the node changed the path, rather than moving the node.
fn is_carried(change: &NodeChange) -> bool {
    change.action == FileAction::Keep && change.relocated_from().is_some()
}

/// Settles `source`, a change that leaves its node at another path, with `target`, the target's
/// change of the node at the path the target holds it at. A delete or a replacement of the file
/// wins against a source change of its executable bit alone and conflicts with any other. An edit
/// of a file the source edits too conflicts, and a source change that leaves the content as it was
/// takes the target's. The file takes the executable bit [`change::merged_mode`] gives it.
fn settle_with_target_change(
    source: &mut NodeChange,
    target: &NodeChange,
    conflicts: &mut Vec<(NodeChange, NodeChange)>,
) -> Settled {
    if target.action == FileAction::Delete || replaces(target) {
        if changes_mode_alone(source) {
            return Settled::GivesWay;
        }
        push_conflict(conflicts, source.clone(), target.clone());
        return Settled::Conflict;
    }
    if edits_both(source, target) {
        push_conflict(conflicts, source.clone(), target.clone());
        return Settled::Conflict;
    }
    let mode = change::merged_mode(source, target);
    if source.from.address.hash == source.to.address.hash {
        let at = source.to.mapping.path.clone();
        onto_target_node(source, target, &at, true, mode);
    } else if let Some(mode) = mode {
        source.to.mode = mode;
    }
    Settled::Stands
}

/// Whether `change` renames a directory only in case.
fn renames_directory(change: &NodeChange) -> bool {
    change.action == FileAction::Move
        && change.to.flags.is_directory()
        && change.is_case_only_move()
}

/// The indices of the changes of `changes` that rename a directory only in case, sorted by the
/// folded path each renames, each with no conflict marked.
fn renames_by_folded_path(changes: &[NodeChange]) -> Vec<(usize, bool)> {
    in_folded_order(
        changes,
        (0..changes.len())
            .filter(|&index| renames_directory(&changes[index]))
            .map(|index| (index, false))
            .collect(),
        |&(index, _)| index,
    )
}

/// `order`, whose entries name changes of `changes` by the index `index` reads from each, sorted by
/// the folded path of the change each names.
fn in_folded_order<T>(
    changes: &[NodeChange],
    mut order: Vec<T>,
    index: impl Fn(&T) -> usize,
) -> Vec<T> {
    order.sort_unstable_by(|lhs, rhs| {
        changes[index(lhs)]
            .path()
            .as_lowercase_str()
            .cmp(changes[index(rhs)].path().as_lowercase_str())
    });
    order
}

/// Where among `renames`, indices among `changes` as [`renames_by_folded_path`] answers them, the
/// rename of the directory at `folded`, a path in folded form, stands.
fn rename_position(
    changes: &[NodeChange],
    renames: &[(usize, bool)],
    folded: &str,
) -> Option<usize> {
    renames
        .binary_search_by(|&(index, _)| changes[index].path().as_lowercase_str().cmp(folded))
        .ok()
}

/// The rename among `renames`, indices among `changes` as [`renames_by_folded_path`] answers them,
/// of the directory at `folded`, a path in folded form, with whether it conflicts.
fn rename_at<'a>(
    changes: &'a [NodeChange],
    renames: &[(usize, bool)],
    folded: &str,
) -> Option<(&'a NodeChange, bool)> {
    rename_position(changes, renames, folded).map(|at| (&changes[renames[at].0], renames[at].1))
}

/// Marks the rename of `changes[index]` among `renames` as one that conflicts, where it is one.
fn mark_rename_conflict(changes: &[NodeChange], renames: &mut [(usize, bool)], index: usize) {
    if let Some(at) = rename_position(changes, renames, changes[index].path().as_lowercase_str())
        && renames[at].0 == index
    {
        renames[at].1 = true;
    }
}

/// The spelling [`DirectoryRenames::spell`] gives a path.
#[derive(Clone, Copy)]
enum Spelling {
    /// The source's tree's: each directory the source renames as the source names it, and each
    /// one the target renames as the base does.
    Source,
    /// The target's tree's: each directory the target renames as the target names it, and each
    /// one the source renames as the base does.
    Target,
    /// The merged tree's: each directory a side renames as that side names it. A directory whose
    /// rename conflicts has no merged name.
    Merged,
}

/// A directory above a path whose rename conflicts, which leaves the path no merged spelling.
struct ConflictedRename;

/// The two sides' renames of directories, each of which changes only case: the source's as indices
/// among its changes and the target's as indices among its moves, each sorted by the folded path
/// it renames, with whether the rename conflicts. A path in any side's spelling folds to the
/// folded paths of the renames above it. Both sides renaming one directory to one name is one
/// rename that conflicts with neither, and names the directory so in every spelling.
struct DirectoryRenames {
    source: Vec<(usize, bool)>,
    target: Vec<(usize, bool)>,
    /// The renames both sides make alike that the join took as one, the target's changes, sorted
    /// by folded path.
    alike: Vec<NodeChange>,
    /// The indices of the target's changes that leave their node where it stood, sorted by folded
    /// path, where the source renames a directory.
    targets: Vec<usize>,
}

/// The renames of a pairing where neither side renames a directory.
static NO_RENAMES: DirectoryRenames = DirectoryRenames {
    source: Vec::new(),
    target: Vec::new(),
    alike: Vec::new(),
    targets: Vec::new(),
};

impl DirectoryRenames {
    /// The renames of `sources`, of `target_moves` and those both sides make alike, `alike`, none
    /// marked as conflicting, with `targets` in folded order where the source renames a
    /// directory, or `None` where neither side renames one.
    fn of(
        sources: &[NodeChange],
        targets: &[NodeChange],
        target_moves: &[NodeChange],
        mut alike: Vec<NodeChange>,
    ) -> Option<Box<Self>> {
        let source = renames_by_folded_path(sources);
        alike.sort_unstable_by(|lhs, rhs| {
            lhs.path()
                .as_lowercase_str()
                .cmp(rhs.path().as_lowercase_str())
        });
        let renames = DirectoryRenames {
            alike,
            targets: if source.is_empty() {
                Vec::new()
            } else {
                in_folded_order(targets, (0..targets.len()).collect(), |&index| index)
            },
            source,
            target: renames_by_folded_path(target_moves),
        };
        (!renames.is_empty()).then(|| Box::new(renames))
    }

    /// The positions among [`Self::targets`] of the target's changes, among `targets`, that stand
    /// below `ancestor`, a path in folded form.
    fn targets_below(&self, targets: &[NodeChange], ancestor: &str) -> std::ops::Range<usize> {
        let order =
            |&index: &usize| below_order(targets[index].path().as_lowercase_str(), ancestor);
        self.targets.partition_point(|index| order(index).is_lt())
            ..self.targets.partition_point(|index| order(index).is_le())
    }

    /// `path` with each directory above it that a side renames named as `spelling` names it,
    /// matched ignoring case at every level, or `None` where `path` names each so already. Fails
    /// with [`ConflictedRename`] only for [`Spelling::Merged`].
    ///
    /// At most one lookup in each side's renames per directory above `path`. Allocates only where
    /// a name changes, and takes the folded form `path` holds.
    fn spell(
        &self,
        sources: &[NodeChange],
        target_moves: &[NodeChange],
        path: &RelativePath,
        spelling: Spelling,
    ) -> Result<Option<RelativePath>, ConflictedRename> {
        if self.is_empty() {
            return Ok(None);
        }
        let mut respelled: Option<String> = None;
        let mut copied = 0;
        let spelled = path.as_str();
        for (start, end, name) in self.names(sources, target_moves, path, spelling) {
            let name = name?;
            if name != &spelled[start..end] {
                let out = respelled.get_or_insert_with(|| String::with_capacity(spelled.len()));
                out.push_str(&spelled[copied..start]);
                out.push_str(name);
                copied = end;
            }
        }
        Ok(respelled.map(|mut out| {
            out.push_str(&spelled[copied..]);
            path.respelled(out)
        }))
    }

    /// Whether a directory above `path` has a rename that conflicts, which leaves `path` no merged
    /// spelling. Builds no path.
    fn conflicts_above(
        &self,
        sources: &[NodeChange],
        target_moves: &[NodeChange],
        path: &RelativePath,
    ) -> bool {
        !self.is_empty()
            && self
                .names(sources, target_moves, path, Spelling::Merged)
                .any(|(_, _, name)| name.is_err())
    }

    /// The index among `changes`, sorted by path, of the change standing at `path` as `spelling`
    /// spells it, or `None` where none stands there or `path` has no such spelling. Builds no
    /// path: the change's paths are compared with `path` and the names `spelling` gives the
    /// directories above it.
    fn find_spelled(
        &self,
        sources: &[NodeChange],
        target_moves: &[NodeChange],
        changes: &[NodeChange],
        path: &RelativePath,
        spelling: Spelling,
    ) -> Option<usize> {
        let spelled = path.as_str();
        let mut names: SmallVec<[(usize, usize, &str); 4]> = SmallVec::new();
        if !self.is_empty() {
            for (start, end, name) in self.names(sources, target_moves, path, spelling) {
                let name = name.ok()?;
                if name != &spelled[start..end] {
                    names.push((start, end, name));
                }
            }
        }
        if names.is_empty() {
            return index_at(changes, spelled).ok();
        }
        changes
            .binary_search_by(|change| cmp_respelled(change.path().as_str(), spelled, &names))
            .ok()
    }

    /// Whether neither side renames a directory.
    fn is_empty(&self) -> bool {
        self.source.is_empty() && self.target.is_empty() && self.alike.is_empty()
    }

    /// Each directory above `path` that a side renames: where its name stands in `path`, and the
    /// name `spelling` gives it.
    fn names<'a>(
        &'a self,
        sources: &'a [NodeChange],
        target_moves: &'a [NodeChange],
        path: &'a RelativePath,
        spelling: Spelling,
    ) -> impl Iterator<Item = (usize, usize, Result<&'a str, ConflictedRename>)> + 'a {
        let (spelled, folded) = (path.as_str(), path.as_lowercase_str());
        spelled
            .match_indices('/')
            .zip(folded.match_indices('/'))
            .scan(0, |start, ((end, _), (folded_end, _))| {
                let name_start = std::mem::replace(start, end + 1);
                Some((name_start, end, folded_end))
            })
            .filter_map(move |(start, end, folded_end)| {
                self.name(sources, target_moves, &folded[..folded_end], spelling)
                    .transpose()
                    .map(|name| (start, end, name))
            })
    }

    /// The name `spelling` gives the directory at `folded`, a path in folded form, where a side
    /// renames it.
    fn name<'a>(
        &'a self,
        sources: &'a [NodeChange],
        target_moves: &'a [NodeChange],
        folded: &str,
        spelling: Spelling,
    ) -> Result<Option<&'a str>, ConflictedRename> {
        if let Ok(at) = self
            .alike
            .binary_search_by(|rename| rename.path().as_lowercase_str().cmp(folded))
        {
            return Ok(Some(self.alike[at].path().name()));
        }
        let source = (sources, &self.source[..]);
        let target = (target_moves, &self.target[..]);
        let (own, other) = match spelling {
            Spelling::Source => (source, target),
            Spelling::Target => (target, source),
            Spelling::Merged => {
                return match (
                    rename_at(source.0, source.1, folded),
                    rename_at(target.0, target.1, folded),
                ) {
                    (Some((rename, false)), None) | (None, Some((rename, false))) => {
                        Ok(Some(rename.path().name()))
                    }
                    (Some((source, false)), Some((target, false)))
                        if source.path().name() == target.path().name() =>
                    {
                        Ok(Some(source.path().name()))
                    }
                    (None, None) => Ok(None),
                    _ => Err(ConflictedRename),
                };
            }
        };
        Ok(match rename_at(own.0, own.1, folded) {
            Some((rename, _)) => Some(rename.path().name()),
            None => rename_at(other.0, other.1, folded)
                .map(|(rename, _)| rename.from.mapping.path.name()),
        })
    }

    /// Marks the source's rename at `index` among `sources` as one that conflicts, where it is one.
    fn mark_source_conflict(&mut self, sources: &[NodeChange], index: usize) {
        mark_rename_conflict(sources, &mut self.source, index);
    }

    /// Marks the target's rename at `index` among `target_moves` as one that conflicts, where it
    /// is one.
    fn mark_target_conflict(&mut self, target_moves: &[NodeChange], index: usize) {
        mark_rename_conflict(target_moves, &mut self.target, index);
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
    /// The target's renames of directories, each only in case, that the source makes alike.
    renamed_alike: Vec<NodeChange>,
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
            renamed_alike: Vec::new(),
            target_replacements: Vec::new(),
            source_consumed: vec![false; source_count],
        }
    }

    /// Joins `target_change` with the source's change at `at`, its index among `source_changes`
    /// by path. The same move on both sides conflicts only where both edit the file, and is kept
    /// in [`Self::renamed_alike`] where it renames a directory only in case. Any other pair
    /// settles as [`settle_at_path`] settles it.
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
                return Ok(());
            }
            if let Some(change) = onto_target_move(source_change, &target_change, include_same) {
                self.changes.push(change);
            }
            if renames_directory(&target_change) {
                self.renamed_alike.push(target_change);
            }
        } else {
            match settle_at_path(source_change, &target_change, include_same).await? {
                AtPath::Conflict => {
                    push_conflict(&mut self.conflicts, source_change.clone(), target_change);
                }
                AtPath::GivesWay => {}
                AtPath::Stands(change) => {
                    if self
                        .changes
                        .last()
                        .is_none_or(|last| last.path() != change.path())
                    {
                        self.changes.push(change);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Settles the source change at `index` with `target`, the target's change at the path a move put
/// one of the two at. A target change of the same node gives way to the source's. Any other
/// settles as [`settle_at_path`] settles it. Marks a source change that gives way in
/// `source_gone`.
async fn settle_moved(
    sources: &mut [NodeChange],
    source_gone: &mut [bool],
    conflicts: &mut Vec<(NodeChange, NodeChange)>,
    index: usize,
    target: &NodeChange,
    include_same: bool,
) -> Result<(), StateError> {
    if source_gone[index] || holds_node(target, &sources[index]) {
        return Ok(());
    }
    match settle_at_path(&sources[index], target, include_same).await? {
        AtPath::Conflict => {
            push_conflict(conflicts, sources[index].clone(), target.clone());
            source_gone[index] = true;
        }
        AtPath::GivesWay => source_gone[index] = true,
        AtPath::Stands(change) => sources[index] = change,
    }
    Ok(())
}

/// Pairs what the two sides of a 3-way diff made of one node where either leaves it at another
/// path, which the join by path does not reach: a move, or a node below a directory a move renamed.
///
/// `sources` are the source's changes, `targets` the target's that leave their node where it
/// stood, `target_moves` the target's that leave it elsewhere, and `renamed_alike` the target's
/// renames of directories that the join took as one with the source's. None of the target's moves
/// is emitted, since the target holds them already. A change one side made to a node the other
/// moved follows the node:
///
/// - Where one side moves a file and the other edits it, the edit stands where the move put the
///   file, and a move that leaves the content as it was carries the other side's content.
/// - Where both sides change a file's content, where one side moves a node and the other deletes
///   or replaces it, and where the two move it to different paths, the pair conflicts. A change of
///   a file's executable bit conflicts with none: the file takes the bit [`change::merged_mode`]
///   gives it. Two moves of a node to one place in the merged tree are one move, however each side
///   spells the directories above it.
/// - The target's changes below a directory the source moves travel with the directory, and the
///   source's changes below one the target moves are put where the target put it. A walk reports a
///   directory as moved where a rename changes only its case; a directory moved to another name
///   reaches the diff as a delete and an add, and the files below it as moves.
/// - A change of a node below a directory such a rename moved is a change of the node where the
///   rename put it, not a move of it. It pairs with the other side's change of the node as an edit
///   does. A source change below the target's renames, and one that follows a target move, stands
///   where the merged tree holds the node: each directory above it named as the side that renames
///   it names it ([`DirectoryRenames`]).
/// - A change put at another path is checked against the other side's change at that path,
///   spelled as that side's tree spells it, as the join checks the two sides' changes at one path.
async fn pair_moved_nodes(
    sources: &mut Vec<NodeChange>,
    targets: &mut Vec<NodeChange>,
    target_moves: Vec<NodeChange>,
    renamed_alike: Vec<NodeChange>,
    conflicts: &mut Vec<(NodeChange, NodeChange)>,
    include_same: bool,
) -> Result<(), StateError> {
    if target_moves.is_empty()
        && renamed_alike.is_empty()
        && !sources
            .iter()
            .any(|change| change.relocated_from().is_some())
    {
        return Ok(());
    }
    let mut pairing = MovePairing::new(
        sources,
        targets,
        target_moves,
        renamed_alike,
        conflicts,
        include_same,
    );
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
/// the path each leaves, which of them a pairing has used up, whether the diff lists the changes
/// the two sides make alike, and the two sides' renames of directories, where either side renames
/// one. The renames are boxed, which keeps them out of the future of every diff that pairs moves.
struct MovePairing<'a> {
    sources: &'a mut Vec<NodeChange>,
    targets: &'a mut Vec<NodeChange>,
    target_moves: Vec<NodeChange>,
    conflicts: &'a mut Vec<(NodeChange, NodeChange)>,
    source_gone: Vec<bool>,
    target_gone: Vec<bool>,
    target_move_paired: Vec<bool>,
    include_same: bool,
    renames: Option<Box<DirectoryRenames>>,
}

impl<'a> MovePairing<'a> {
    fn new(
        sources: &'a mut Vec<NodeChange>,
        targets: &'a mut Vec<NodeChange>,
        mut target_moves: Vec<NodeChange>,
        renamed_alike: Vec<NodeChange>,
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
            renames: DirectoryRenames::of(sources, targets, &target_moves, renamed_alike),
            sources,
            targets,
            target_moves,
            conflicts,
            include_same,
        }
    }

    /// The two sides' renames of directories.
    fn renames(&self) -> &DirectoryRenames {
        self.renames.as_deref().unwrap_or(&NO_RENAMES)
    }

    /// `path` spelled as `spelling` asks through the two sides' renames of directories, or `None`
    /// where it is spelled so already or has no merged spelling.
    fn spell(&self, path: &RelativePath, spelling: Spelling) -> Option<RelativePath> {
        self.renames()
            .spell(self.sources, &self.target_moves, path, spelling)
            .ok()
            .flatten()
    }

    /// Whether a directory above `path` has a rename that conflicts.
    fn conflicts_above(&self, path: &RelativePath) -> bool {
        self.renames()
            .conflicts_above(self.sources, &self.target_moves, path)
    }

    /// The index among `changes`, sorted by path, of the change at `path` as `spelling` spells it
    /// through the two sides' renames of directories, as [`DirectoryRenames::find_spelled`] finds
    /// it.
    fn find(
        &self,
        changes: &[NodeChange],
        path: &RelativePath,
        spelling: Spelling,
    ) -> Option<usize> {
        self.renames()
            .find_spelled(self.sources, &self.target_moves, changes, path, spelling)
    }

    /// Pairs each source change that leaves its node at another path with what the target made of
    /// the node at the path it stood at, and marks the renames of directories that conflict.
    ///
    /// - A target change of the node where it stood, or where a rename of a directory above it
    ///   carried it, settles with the source's as [`settle_with_target_change`] settles the two.
    ///   Where either stands below a directory whose rename conflicts, the two conflict instead,
    ///   as the merged tree spells the directory neither way.
    /// - A target move of the node elsewhere conflicts with a source move of it, unless the two
    ///   put it at one place in the merged tree: the same move on both sides, which settles as the
    ///   join settles it.
    /// - A source change a rename carried, against a target move, is left to
    ///   [`Self::pair_target_moves`].
    fn pair_source_moves(&mut self) {
        for index in 0..self.sources.len() {
            let from = relocation_source(&self.sources[index]);
            if from.is_empty() {
                continue;
            }
            let settled = if let Ok(other) = self
                .target_moves
                .binary_search_by(|target| relocation_source(target).cmp(from))
            {
                let (source, target) = (&self.sources[index], &self.target_moves[other]);
                if !is_carried(target) && is_carried(source) {
                    continue;
                }
                self.target_move_paired[other] = true;
                if is_carried(target) {
                    if self.conflicts_above(source.path()) || self.conflicts_above(target.path()) {
                        push_conflict(self.conflicts, source.clone(), target.clone());
                        Settled::Conflict
                    } else {
                        let target = &self.target_moves[other];
                        settle_with_target_change(&mut self.sources[index], target, self.conflicts)
                    }
                } else if moves_to_one_place(source, target) {
                    self.settle_same_move(index, other)
                } else {
                    push_conflict(self.conflicts, source.clone(), target.clone());
                    if let Some(renames) = self.renames.as_deref_mut() {
                        renames.mark_target_conflict(&self.target_moves, other);
                    }
                    Settled::Conflict
                }
            } else if let Ok(other) = index_at(self.targets, from) {
                self.target_gone[other] = true;
                settle_with_target_change(
                    &mut self.sources[index],
                    &self.targets[other],
                    self.conflicts,
                )
            } else {
                Settled::Stands
            };
            self.source_gone[index] = !matches!(settled, Settled::Stands);
            if matches!(settled, Settled::Conflict)
                && let Some(renames) = self.renames.as_deref_mut()
            {
                renames.mark_source_conflict(self.sources, index);
            }
        }
    }

    /// Settles the source's change at `index` with the target's move at `other`, which put one node
    /// at one place in the merged tree, as the join settles a move both sides make: the two
    /// conflict where both edit the file, and otherwise what is left of the source's change is what
    /// [`onto_target_move`] leaves, standing where the source's change stands, so the source's
    /// changes keep their order by path, and the target's move's other half is taken up.
    fn settle_same_move(&mut self, index: usize, other: usize) -> Settled {
        let (source, target) = (&self.sources[index], &self.target_moves[other]);
        if edits_both(source, target) {
            push_conflict(self.conflicts, source.clone(), target.clone());
            return Settled::Conflict;
        }
        let left = onto_target_move(source, target, self.include_same);
        self.take_move_half(other);
        match left {
            Some(change) => {
                self.sources[index] = change;
                Settled::Stands
            }
            None => Settled::GivesWay,
        }
    }

    /// Takes up the target's change where the target's move at `index` put its node, the move's
    /// other half, which the target holds already.
    fn take_move_half(&mut self, index: usize) {
        if let Ok(half) = index_at(self.targets, self.target_moves[index].path().as_str()) {
            self.target_gone[half] = true;
        }
    }

    /// Puts each target change below a directory the source moves, found ignoring case, where the
    /// source's tree spells it, through the renames of both sides, and settles it with the source's
    /// change at that path as [`settle_moved`] does. The target's change is not emitted, since the
    /// target holds it, and is copied only where it lands on a source change.
    async fn carry_target_changes(&mut self) -> Result<(), StateError> {
        for index in 0..self.sources.len() {
            let source = &self.sources[index];
            if self.source_gone[index] || !source.to.flags.is_directory() {
                continue;
            }
            let Some(from) = source.relocated_from() else {
                continue;
            };
            let below = self
                .renames()
                .targets_below(self.targets, from.as_lowercase_str());
            for position in below {
                let other = self.renames().targets[position];
                if self.target_gone[other] {
                    continue;
                }
                self.target_gone[other] = true;
                let Some((at, target)) = self.landing(other) else {
                    continue;
                };
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

    /// Where the target's change at `other` lands among the source's changes: the index of the
    /// source's change at the path the source's tree spells it at, and the target's change put
    /// there. `None` where the source changed nothing there.
    fn landing(&self, other: usize) -> Option<(usize, NodeChange)> {
        let target = &self.targets[other];
        let landed = self.spell(target.path(), Spelling::Source);
        let at = index_at(
            self.sources,
            landed.as_ref().unwrap_or(target.path()).as_str(),
        )
        .ok()?;
        let mut moved = target.clone();
        for side in [&mut moved.from, &mut moved.to] {
            let spelled = if side.mapping.path == *target.path() {
                landed.clone()
            } else {
                self.spell(&side.mapping.path, Spelling::Source)
            };
            if let Some(path) = spelled {
                side.mapping.path = path;
            }
        }
        Some((at, moved))
    }

    /// Pairs each target move the source did not make as well with the source's change of the
    /// node: at the path the node stood at, or where a rename of a directory above it carried it.
    /// Answers the indices of the source changes put at another path, which move once every pair
    /// is found, as finding one reads the sources by path.
    ///
    /// - A delete, a replacement, and an edit of a file the target edits too conflict with the
    ///   move, a delete where the move put the node. The source's changes below a directory it
    ///   deletes wait for that conflict. A delete or a replacement of a file whose executable bit
    ///   alone the target changes, where a rename of a directory above it carried it, wins
    ///   instead, and follows to where the target holds the file.
    /// - Where the move leaves the content as it was, where the source's change gives the file
    ///   another executable bit, or where the diff lists the changes the two sides make alike, the
    ///   source's change follows the move, as [`Self::follow_destination`] says where to, or
    ///   conflicts with it. A change of a file that follows takes the content the move left where
    ///   it holds no edit of its own and is no other file, and the bit [`change::merged_mode`]
    ///   gives the file. The target's change where the move put the node, the move's other half,
    ///   goes with the change that follows.
    /// - Any other source change gives way to the move.
    fn pair_target_moves(&mut self) -> Vec<usize> {
        if self.target_moves.is_empty() {
            return Vec::new();
        }
        let carried = relocated_indices(self.sources, is_carried);
        let mut followed = Vec::new();
        for index in 0..self.target_moves.len() {
            if self.target_move_paired[index] {
                continue;
            }
            let target = &self.target_moves[index];
            let from = relocation_source(target);
            let other = match index_at(self.sources, from) {
                Ok(other) if self.sources[other].relocated_from().is_none() => other,
                Ok(_) => continue,
                Err(_) => match carried
                    .binary_search_by(|&source| relocation_source(&self.sources[source]).cmp(from))
                {
                    Ok(found) => carried[found],
                    Err(_) => continue,
                },
            };
            let source = &self.sources[other];
            if self.source_gone[other] {
                continue;
            }
            if (source.action == FileAction::Delete || replaces(source))
                && changes_mode_alone(target)
            {
                followed.push((other, index, target.path().clone()));
            } else if source.action == FileAction::Delete {
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
            } else if self.include_same
                || target.from.address.hash == target.to.address.hash
                || (source.changes_mode() && change::merged_mode(source, target).is_some())
            {
                if let Some(destination) = self.follow_destination(other, index) {
                    followed.push((other, index, destination));
                } else {
                    let (source, target) = (&self.sources[other], &self.target_moves[index]);
                    push_conflict(self.conflicts, source.clone(), target.clone());
                    self.source_gone[other] = true;
                }
            } else {
                self.source_gone[other] = true;
            }
        }
        followed
            .into_iter()
            .map(|(other, index, destination)| {
                let target = &self.target_moves[index];
                let source = &mut self.sources[other];
                let mode = change::merged_mode(source, target);
                let takes_content = !edits(source)
                    && !replaces(source)
                    && source.to.flags.contains(NodeFlags::File);
                onto_target_node(source, target, &destination, takes_content, mode);
                self.take_move_half(index);
                other
            })
            .collect()
    }

    /// Where the source change at `other` follows the target move at `index` to: the move's
    /// destination as the merged tree spells it. `None` where the two conflict instead: where the
    /// source changed the destination as well, with another change than this one, as an edit at
    /// both ends of a move does, and where the source's change or the destination stands below a
    /// directory whose rename conflicts, which leaves it no merged spelling.
    fn follow_destination(&self, other: usize, index: usize) -> Option<RelativePath> {
        let moved_to = self.target_moves[index].path();
        if self.conflicts_above(self.sources[other].path()) || self.conflicts_above(moved_to) {
            return None;
        }
        if self
            .find(self.sources, moved_to, Spelling::Source)
            .is_some_and(|at| at != other)
        {
            return None;
        }
        Some(
            self.spell(moved_to, Spelling::Merged)
                .unwrap_or_else(|| moved_to.clone()),
        )
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

    /// Spells both sides of each source change through both sides' renames of directories, where
    /// the target renames one, alone or alike with the source: the side the node stood at as the
    /// target's tree spells it, and the side it stands at as the merged tree does, where it has a
    /// merged spelling. Where the source renames no directory, the two spellings of one path are
    /// one, which is built once. Adds to `retargeted` the source changes whose own path that moves,
    /// and those below a directory whose rename conflicts, which the target's tree may spell
    /// otherwise.
    fn follow_target_directory_moves(&mut self, retargeted: &mut Vec<usize>) {
        if self.renames().target.is_empty() && self.renames().alike.is_empty() {
            return;
        }
        for index in 0..self.sources.len() {
            if self.source_gone[index] {
                continue;
            }
            let source = &self.sources[index];
            let from = self.spell(&source.from.mapping.path, Spelling::Target);
            let (to, waits) = if self.renames().source.is_empty()
                && source.to.mapping.path == source.from.mapping.path
            {
                (from.clone(), false)
            } else {
                match self.renames().spell(
                    self.sources,
                    &self.target_moves,
                    &source.to.mapping.path,
                    Spelling::Merged,
                ) {
                    Ok(to) => (to, false),
                    Err(ConflictedRename) => (None, true),
                }
            };
            let moved = if source.action == FileAction::Delete {
                from.is_some()
            } else {
                to.is_some() || waits
            };
            if moved {
                retargeted.push(index);
            }
            let source = &mut self.sources[index];
            if let Some(from) = from {
                source.from.mapping.path = from;
            }
            if let Some(to) = to {
                source.to.mapping.path = to;
            }
        }
    }

    /// Settles each source change put at another path with the target's change where the target's
    /// tree spells that path, as [`settle_moved`] does.
    async fn check_retargeted(&mut self, retargeted: Vec<usize>) -> Result<(), StateError> {
        for index in retargeted {
            if self.source_gone[index] {
                continue;
            }
            let Some(other) = self.target_at(index) else {
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

    /// The index of the target's change, among those that leave their node where it stood, at the
    /// path of the source's change at `index` as the target's tree spells it.
    fn target_at(&self, index: usize) -> Option<usize> {
        self.find(self.targets, self.sources[index].path(), Spelling::Target)
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

/// Walks the tree at `revision`. With `include_last_revision`, attributes each entry to the
/// revision that last changed it
/// ([`TreePath::last_revision`](crate::state::TreePath::last_revision)).
pub async fn tree(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    path: RelativePath,
    max_depth: usize,
    can_read: crate::state::CanReadRepository,
    include_last_revision: bool,
) -> Result<TreeWalk, StateError> {
    lore_debug!(
        "Gathering tree in repository {} revision: {} path: {}",
        repository.id,
        revision,
        path.as_str()
    );
    let state = State::deserialize(repository.clone(), revision).await?;
    gather_tree_paths(
        state,
        repository,
        path,
        max_depth,
        can_read,
        include_last_revision,
    )
    .await
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
