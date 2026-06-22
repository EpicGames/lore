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
use serde::Deserialize;
use serde::Serialize;
use tokio::join;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::branch;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::is_conflict;
use crate::errors::Oversized;
use crate::errors::RevisionNotFound;
use crate::event;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::find;
use crate::history::find_branch_point;
use crate::interface::LoreString;
use crate::lore::*;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
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
        let mut revision_metadata = Self::default();
        let _ = metadata.walk(|key, value, _value_type| {
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

/// Returns `None` when any `Filter` builder call fails, signalling
/// the caller to fall back to the unfiltered walk. The pre-streaming
/// implementation absorbed these errors silently into a boolean —
/// the explicit `Option` keeps that fallback path observable.
fn filter_from_source_changes(source_changes: &[NodeChange]) -> Option<Filter> {
    let mut filter = Filter::default();
    if let Err(err) = filter.view.add_exclusion("**") {
        lore_warn!("Failed to add target filter global exclusion: {err}");
        return None;
    }
    for change in source_changes.iter() {
        if let Err(err) = filter.view.add_inclusion(change.path.as_str()) {
            lore_warn!("Failed to add target filter re-inclusion: {err}");
            return None;
        }
        if let Some(from_path) = change.from_path.as_ref()
            && let Err(err) = filter.view.add_inclusion(from_path.as_str())
        {
            lore_warn!("Failed to add target filter re-inclusion of from path: {err}");
            return None;
        }
    }
    Some(filter)
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
///    filter is not active or path is filter-included.
/// 4. Buffer joined output internally to fold three post-merge passes
///    (`Move + from_path`, directory-delete overlap, history-walk
///    resolution) before emitting. History-walk runs in parallel via
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
/// * `source_cap` — abort with `StateError::Oversized` when source's
///   `diff_collect` produces more than `n` items. The error message
///   includes the cap and the produced count. Bounds peak memory for
///   callers that need a ceiling. Library callers (filesystem diff,
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

    lore_info!(
        "Calculating 3-way diff between\n  base {} -> {}\n  source {} -> {}\n  target {} -> {}",
        state_base.revision_number(),
        state_base.revision(),
        state_source.revision_number(),
        state_source.revision(),
        state_target.revision_number(),
        state_target.revision()
    );

    // Stream source so the cap fires mid-walk: drop the channel and
    // abort the producer as soon as we cross `source_cap`, instead of
    // paying for a full walk that we're going to reject. Surfaces as
    // `StateError::Oversized` so callers can map to
    // `Status::resource_exhausted` via `is_oversized()` without
    // string-matching across crates.
    lore_info!("Diff source branch revisions (streaming)");
    let (source_tx, mut source_rx) = mpsc::channel::<Result<NodeChange, StateError>>(256);
    let source_walker_repo = repository.clone();
    let source_walker_state_base = state_base.clone();
    let source_walker_state_source = state_source.clone();
    let source_walker_path = path.clone();
    let source_walker = lore_spawn!(async move {
        let mut sink = state::ChangeSink::Channel(&source_tx);
        state::diff(
            source_walker_repo.clone(),
            source_walker_state_base,
            source_walker_repo,
            source_walker_state_source,
            source_walker_path,
            &mut sink,
            FilterMode::View,
        )
        .await
    });

    let mut source_changes: Vec<NodeChange> = Vec::new();
    let mut oversized = false;
    while let Some(item) = source_rx.recv().await {
        let change = match item {
            Ok(c) => c,
            Err(err) => {
                drop(source_rx);
                let _ = source_walker.await;
                return Err(err);
            }
        };
        let is_file_id_only_churn = !change.from.address.hash.is_zero()
            && change.action != FileAction::Move
            && change.from.address.hash == change.to.address.hash;
        if is_file_id_only_churn {
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
        // Drop the receiver so the producer's next `send` errors on
        // closed channel and `state::diff` exits naturally — no abort,
        // so an in-flight side effect inside `state::diff` is allowed
        // to complete before the task ends.
        drop(source_rx);
        let _ = source_walker.await;
        return Err(StateError::from(Oversized {
            context: format!(
                "source-side diff change count exceeds configured limit of {}",
                source_cap.unwrap_or(0)
            ),
        }));
    }
    match source_walker.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err),
        Err(join_err) => {
            return Err(StateError::internal_with_context(
                join_err,
                "3-way diff source walker task failed",
            ));
        }
    }
    state::detect_and_coalesce_moves(&mut source_changes);

    lore_info!("Sorting {} source changes", source_changes.len());
    change::sort_by_path(&mut source_changes);

    let target_filter = if source_changes.len() < SOURCE_FILTER_THRESHOLD
        && let Some(filter) = filter_from_source_changes(&source_changes)
    {
        Arc::new(filter)
    } else {
        repository.filter.clone()
    };
    let target_repository = Arc::new(repository.to_filter_context(target_filter));

    lore_info!("Diff target branch revisions (streaming)");
    let (target_tx, mut target_rx) = mpsc::channel::<Result<NodeChange, StateError>>(256);
    let walker_repo = target_repository.clone();
    let walker_state_base = state_base.clone();
    let walker_path = path.clone();
    let walker = lore_spawn!(async move {
        let mut sink = state::ChangeSink::Channel(&target_tx);
        state::diff(
            walker_repo.clone(),
            walker_state_base,
            walker_repo,
            state_target,
            walker_path,
            &mut sink,
            FilterMode::View,
        )
        .await
    });

    // Join loop: target streams in; source lives in the sorted Vec.
    // Items emit via `out` (the joined working set) so post-merge
    // passes can fold inline before the final emission to `tx`.
    let mut joined_changes: Vec<NodeChange> = Vec::new();
    let mut joined_conflicts: Vec<(NodeChange, NodeChange)> = Vec::new();
    let mut source_consumed = vec![false; source_changes.len()];
    while let Some(item) = target_rx.recv().await {
        let mut target_change = match item {
            Ok(c) => c,
            Err(e) => {
                return Err(e);
            }
        };
        let is_file_id_only_churn = !target_change.from.address.hash.is_zero()
            && target_change.from.address.hash == target_change.to.address.hash;
        if is_file_id_only_churn {
            continue;
        }
        match source_changes.binary_search_by(|c| c.path.as_str().cmp(target_change.path.as_str()))
        {
            Ok(idx) => {
                source_consumed[idx] = true;
                let source_change = &source_changes[idx];
                if is_conflict(source_change, &target_change, true).await? {
                    let mut sc = source_change.clone();
                    sc.flags = change::Flags::Conflict;
                    target_change.flags = change::Flags::Conflict;
                    joined_conflicts.push((sc, target_change));
                } else if include_same
                    && (joined_changes.is_empty()
                        || joined_changes[joined_changes.len() - 1].path != source_change.path)
                {
                    joined_changes.push(source_change.clone());
                }
            }
            Err(_) => {
                joined_changes.push(target_change);
            }
        }
    }

    match walker.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err),
        Err(join_err) => {
            return Err(StateError::internal_with_context(
                join_err,
                "3-way diff target walker task failed",
            ));
        }
    }

    for (idx, consumed) in source_consumed.iter().enumerate() {
        if !*consumed {
            joined_changes.push(source_changes[idx].clone());
        }
    }

    apply_move_from_path_pass(&mut joined_changes, &mut joined_conflicts);

    if !joined_conflicts.is_empty() {
        joined_changes.retain(|item| {
            if item.from.flags.is_directory() && item.action == FileAction::Delete {
                for (from, _to) in joined_conflicts.iter() {
                    if from.path.overlaps(&item.path) {
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
            // If the file was added on source branch and either added or
            // modified on target branch, show the change as modified.
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
    if !final_conflicts.is_empty() {
        lore_debug!("Final {} conflicts", final_conflicts.len());
    }

    let summary = Diff3Summary {
        base,
        source,
        target,
    };

    for change in joined_changes {
        tx.send(Ok(DiffItem::Change(change)))
            .await
            .map_err(|_send_err| StateError::internal("3-way diff receiver dropped"))?;
    }
    for (source_change, target_change) in final_conflicts {
        tx.send(Ok(DiffItem::Conflict(Box::new((
            source_change,
            target_change,
        )))))
        .await
        .map_err(|_send_err| StateError::internal("3-way diff receiver dropped"))?;
    }
    Ok(summary)
}

/// Resolves `Move + other-change-at-from-path` interactions per the
/// pre-streaming rules:
///   - `Move(X→Y) + Delete(X)` → conflict (divergent move).
///   - Pure rename `Move(X→Y, content unchanged) + Modify(X)` →
///     absorb the modify into the rename (the rename carries the
///     modified content).
///   - `Move(X→Y, content changed) + Modify(X)` → conflict (both
///     branches modified content differently).
fn apply_move_from_path_pass(
    changes: &mut Vec<NodeChange>,
    conflicts: &mut Vec<(NodeChange, NodeChange)>,
) {
    let mut from_path_conflict_pairs: Vec<(usize, usize)> = Vec::new();
    let mut from_path_absorbed: Vec<usize> = Vec::new();
    for (outer_index, change_move) in changes.iter().enumerate() {
        if change_move.action == FileAction::Move
            && let Some(ref from_path) = change_move.from_path
        {
            for (inner_index, change_match) in changes.iter().enumerate() {
                if outer_index != inner_index && change_match.path.as_str() == from_path.as_str() {
                    if change_match.action == FileAction::Delete {
                        from_path_conflict_pairs.push((outer_index, inner_index));
                    } else if change_move.from.address.hash == change_move.to.address.hash {
                        from_path_absorbed.push(inner_index);
                    } else {
                        from_path_conflict_pairs.push((outer_index, inner_index));
                    }
                }
            }
        }
    }
    let mut remove = vec![false; changes.len()];
    for &absorbed_idx in from_path_absorbed.iter() {
        remove[absorbed_idx] = true;
    }
    for &(move_idx, other_idx) in from_path_conflict_pairs.iter() {
        if !remove[move_idx] && !remove[other_idx] {
            let mut move_change = changes[move_idx].clone();
            let mut other_change = changes[other_idx].clone();
            move_change.flags = change::Flags::Conflict;
            other_change.flags = change::Flags::Conflict;
            conflicts.push((move_change, other_change));
            remove[move_idx] = true;
            remove[other_idx] = true;
        }
    }
    let mut i = changes.len();
    while i > 0 {
        i -= 1;
        if remove[i] {
            changes.remove(i);
        }
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
        source_conflict.path.as_str()
    );
    let (last_source_modified_revision, last_source_modified_revision_number) =
        find_last_modified_revision(&source_conflict, state_branch_point.clone()).await?;
    lore_debug!(
        "{} last modified source {} revision {} -> {}",
        source_conflict.path.as_str(),
        source_branch,
        last_source_modified_revision,
        last_source_modified_revision_number,
    );

    lore_debug!(
        "{} find last merged source {} -> target {} revision",
        source_conflict.path.as_str(),
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
            source_conflict.path.as_str(),
            source_branch,
            target_branch,
            last_merged_from_source_revision,
            last_merged_from_source_revision_number,
        );

        if last_source_modified_revision_number <= last_merged_from_source_revision_number {
            lore_debug!(
                "Final change check for {} is MERGED - last source merged revision {}, last source modified revision {}",
                source_conflict.path.as_str(),
                last_merged_from_source_revision_number,
                last_source_modified_revision_number,
            );
            Ok(true)
        } else {
            lore_debug!(
                "Final change merge check for {} is UNMERGED - last source merged revision {}, last source modified revision {}",
                source_conflict.path.as_str(),
                last_merged_from_source_revision_number,
                last_source_modified_revision_number,
            );
            Ok(false)
        }
    } else {
        lore_debug!(
            "Final change merge check for {} is UNMERGED - no merged revision found",
            source_conflict.path.as_str()
        );
        Ok(false)
    }
}

async fn find_last_modified_revision(
    change: &NodeChange,
    state_branch_point: Arc<State>,
) -> Result<(Hash, u64), StateError> {
    if !change.to.node.is_valid_node_id() {
        // File was deleted from the target state, need to walk history to find it
        // TODO(mjansson): Figure out a better way to store this metadata around file deletions
        lore_debug!("Find last modified revision without to state, iterate revisions");

        let repository = change.from.repository.clone();
        let mut state_current = change.to.state.clone();
        let mut state_parent = state_current.clone();
        while state_current.revision_number() >= state_branch_point.revision_number() {
            if let Ok(node_link) = state_current
                .find_node_link(repository.clone(), change.path.as_str())
                .await
            {
                // If the node was existing in the current state, it means it was deleted in the previous (parent) revision
                lore_debug!(
                    "Found {} existing in revision {} - {}",
                    change.path.as_str(),
                    state_current.revision(),
                    state_current.revision_number()
                );
                if node_link.is_valid() {
                    lore_debug!(
                        "Last modified {} in parent revision {} - {}",
                        change.path.as_str(),
                        state_parent.revision(),
                        state_parent.revision_number()
                    );
                    return Ok((state_parent.revision(), state_parent.revision_number()));
                }
            }
            state_parent = state_current.clone();
            state_current =
                State::deserialize(repository.clone(), state_current.parent_self()).await?;
        }

        lore_debug!(
            "Did not find node {} last modified, using branch point revision {} - {}",
            change.path.as_str(),
            state_branch_point.revision(),
            state_branch_point.revision_number()
        );

        return Ok((
            state_branch_point.revision(),
            state_branch_point.revision_number(),
        ));
    };

    let repository = change.to.repository.clone();
    let state = change.to.state.clone();
    let node_id = change.to.node;

    let mut last_modified_revision = state.revision();
    let mut last_modified_revision_number = state.revision_number();

    // Check if node was modified in the LATEST revision
    if let Ok(Some(_node_delta)) = state.node_delta(repository.clone(), node_id).await {
        lore_debug!(
            "{} found last modified revision {} -> {} (HEAD)",
            change.path.as_str(),
            last_modified_revision,
            last_modified_revision_number
        );
    } else {
        lore_debug!(
            "{} not modified in HEAD, walk file history",
            change.path.as_str()
        );
        // Node was not modified in the target LATEST revision, find the
        // previous revision from file history block
        if let Ok(block) = state
            .block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(node_id))
            .await
        {
            let node = block.node(NodeFileMetadata::index(node_id));
            lore_debug!("{} node history {:?}", change.path.as_str(), node);
            if let Ok(state_modified) =
                state::State::deserialize(repository.clone(), node.revision[0]).await
            {
                last_modified_revision = state_modified.revision();
                last_modified_revision_number = state_modified.revision_number();
                lore_debug!(
                    "{} found last modified revision {} -> {}",
                    change.path.as_str(),
                    last_modified_revision_number,
                    last_modified_revision
                );
            } else {
                lore_warn!(
                    "Failed to deserialize last modified state when searching history for {}",
                    change.path.as_str()
                );
            }
        } else {
            lore_warn!(
                "Failed to deserialize file metadata block when searching history for {}",
                change.path.as_str()
            );
        }
    }

    Ok((last_modified_revision, last_modified_revision_number))
}

async fn find_last_merged_revision(
    change: &NodeChange,
    source_branch: BranchId,
    target_branch: BranchId,
    state_branch_point: Arc<State>,
) -> Result<Option<(Hash, u64)>, StateError> {
    let (state_start, node_current) = if change.to.node.is_valid_node_id() {
        (change.to.state.clone(), change.to.node)
    } else {
        // TODO(mjansson): Figure out a better way to store this metadata around file deletions
        lore_debug!("Find last merged revision without to state, iterate revisions");

        let repository = change.to.repository.clone();
        let mut state_current = change.to.state.clone();
        let mut state_parent = state_current.clone();
        loop {
            if let Ok(node_link) = state_current
                .find_node_link(repository.clone(), change.path.as_str())
                .await
            {
                lore_debug!(
                    "Found {} existing in revision {} - {} for last merged",
                    change.path.as_str(),
                    state_current.revision(),
                    state_current.revision_number()
                );

                // If the node was existing in the current state, it means it was deleted in the previous (parent) revision
                if node_link.is_valid() {
                    lore_debug!(
                        "Using {} parent revision {} - {} as last merged start revision",
                        change.path.as_str(),
                        state_parent.revision(),
                        state_parent.revision_number()
                    );
                    break (state_parent, node_link.node);
                }
            }

            if state_current.revision_number() <= state_branch_point.revision_number() {
                lore_debug!(
                    "Did not find node {} last modified, no merged revision",
                    change.path.as_str()
                );
                return Ok(None);
            }

            lore_debug!(
                "Node {} not present in revision {} - {} for last merged start, move to parent",
                change.path.as_str(),
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
    let repository = change.to.repository.clone();
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
                    change.path.as_str(),
                    target_branch,
                    node_current,
                    state_current.revision(),
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
                                change.path.as_str(),
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
                                change.path.as_str(),
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
                    change.path.as_str(),
                    state_current.revision_number(),
                    node,
                    node.revision[0]
                );
            }

            if let Ok(state_previous) =
                state::State::deserialize(repository.clone(), node.revision[0]).await
            {
                if state_previous.revision_number() < state_branch_point.revision_number() {
                    lore_debug!(
                        "{} stop iterating revisions, reached revision {} < branch point modified revision {}",
                        change.path.as_str(),
                        state_previous.revision_number(),
                        state_branch_point.revision_number()
                    );
                    break;
                } else {
                    lore_debug!(
                        "{} step to revision {} -> {}",
                        change.path.as_str(),
                        state_previous.revision_number(),
                        state_previous.revision()
                    );
                    state_current = state_previous;
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

/// Information about a revision being resolved from a signature.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionResolveEventData {
    /// Repository identifier in which repository
    pub repository: RepositoryId,
    /// Identifier of the branch on which resolution is being done
    pub branch: BranchId,
    /// If set to non-empty, the partial hash being resolved
    pub revision: LoreString,
    /// If set to non-zero, the revision number being resolved
    pub revision_number: u64,
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

pub async fn resolve(
    repository: Arc<RepositoryContext>,
    signature: impl AsRef<str>,
    search_limit: Option<usize>,
    search_location: ResolveSearchLocation,
) -> Result<Hash, StateError> {
    let signature = signature.as_ref();
    let original_input = signature.to_string();
    let mut revision = Hash::default();

    let (signature, offset) = if let Some(split) = signature.split_once("~") {
        let prefix = split.0;
        let suffix = split.1;
        if suffix.is_empty() {
            (prefix, Some(1))
        } else {
            let offset: u64 = suffix.parse::<u64>().map_err(|err| {
                lore_debug!("Malformed revision offset {suffix:?}: {err}");
                StateError::from(RevisionNotFound {
                    revision: original_input.clone(),
                })
            })?;

            (prefix, Some(offset))
        }
    } else {
        (signature, None)
    };

    lore_debug!("Resolving signature {signature}, offset {offset:?}");

    let (should_search_remote, should_search_local) = match search_location {
        ResolveSearchLocation::RemoteOrLocal => (true, true),
        ResolveSearchLocation::Remote => (true, false),
        ResolveSearchLocation::Local => (false, true),
    };

    if signature.len() == HASH_STRING_LENGTH && signature.chars().all(|c| c.is_ascii_hexdigit()) {
        revision = Hash::from_str(signature).map_err(|err| {
            lore_debug!("Malformed revision signature {signature:?}: {err}");
            StateError::from(RevisionNotFound {
                revision: original_input.clone(),
            })
        })?;
        lore_debug!("Resolved direct hash signature: {revision}");
    } else if let Some(split) = signature.split_once("@") {
        let prefix = split.0;
        let suffix = split.1;

        lore_debug!("Resolving branch {prefix} signature {signature}");
        let branch = if prefix.is_empty() {
            let (_current_revision, current_branch) =
                crate::instance::load_current_anchor(&repository)
                    .await
                    .forward::<StateError>("Failed deserializing anchor")?;
            current_branch
        } else {
            let branch_status = branch::resolve(repository.clone(), prefix)
                .await
                .map_matched_err("Invalid branch specifier", |m| match m {
                    branch::MatchedBranchError::BranchNotFound(_) => {
                        StateError::from(RevisionNotFound {
                            revision: original_input.clone(),
                        })
                    }
                    other => other.forward::<StateError>("resolving branch for revision"),
                })?;
            branch_status.id
        };

        let remote_latest = if let Ok(remote) = repository.remote().await
            && should_search_remote
        {
            branch::load_remote_latest(remote.clone(), repository.id, branch)
                .await
                .ok()
        } else {
            None
        };

        let local_latest = if should_search_local {
            branch::load_latest(repository.clone(), branch).await.ok()
        } else {
            None
        };

        if suffix.to_uppercase() == "LATEST" || suffix.to_uppercase() == "HEAD" {
            let local = local_latest.filter(|head| !head.is_zero());
            let remote = remote_latest.filter(|head| !head.is_zero());
            revision = match (local, remote) {
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
            };
        } else {
            let revision_number: u64 = suffix.parse::<u64>().map_err(|err| {
                lore_debug!("Invalid revision number {suffix:?}: {err}");
                StateError::from(RevisionNotFound {
                    revision: original_input.clone(),
                })
            })?;

            event::LoreEvent::RevisionResolve(LoreRevisionResolveEventData {
                repository: repository.id,
                branch,
                revision: LoreString::default(),
                revision_number,
                remote: should_search_remote.into(),
                local: should_search_local.into(),
            })
            .send();

            if should_search_remote
                && let Ok(connection) = repository.remote().await
                && let Ok(revision_service) = connection.revision(repository.id).await
                && let Ok(response) = revision_service
                    .revision_list(
                        RevisionListIdentifier {
                            branch,
                            number: revision_number,
                        }
                        .into(),
                    )
                    .await
            {
                if let Ok(item) = response
                    .items
                    .as_slice()
                    .binary_search_by(|item| item.number.cmp(&revision_number))
                {
                    revision = response.items[item].signature;
                    lore_debug!("response revision {}", revision);
                }
                find::cache_revision_list_states(repository.clone(), &response.items).await;
            }

            if revision.is_zero()
                && let Some(head) = remote_latest
                && let Ok(found_revision) =
                    find::revision_by_number(repository.clone(), branch, head, revision_number)
                        .await
            {
                revision = found_revision;
            }
            if revision.is_zero()
                && let Some(head) = local_latest
                && let Ok(found_revision) =
                    find::revision_by_number(repository.clone(), branch, head, revision_number)
                        .await
            {
                revision = found_revision;
            }
        }

        if !revision.is_zero() {
            lore_debug!("Resolved to branch {branch} revision {revision}");
        }
    } else {
        if !signature.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(RevisionNotFound {
                revision: original_input.clone(),
            }
            .into());
        }

        let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<StateError>("Failed deserializing anchor")?;
        let branch = current_branch;

        if signature.len() < HASH_STRING_LENGTH {
            event::LoreEvent::RevisionResolve(LoreRevisionResolveEventData {
                repository: repository.id,
                branch,
                revision: signature.into(),
                revision_number: 0,
                remote: should_search_remote.into(),
                local: should_search_local.into(),
            })
            .send();
        }

        if let Ok(found_revision) =
            find::revision_by_string(repository.clone(), branch, signature, search_limit).await
        {
            revision = found_revision;
            lore_debug!("Resolved partial match revision {revision}");
        }
    }

    if revision.is_zero() {
        return Err(RevisionNotFound {
            revision: original_input.clone(),
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
                    revision: original_input.clone(),
                }
                .into());
            }

            revision = parent;
            counter -= 1;
        }
    }

    Ok(revision)
}
