// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::branch::merge::ApplyDiffResults;
use crate::branch::merge::MergeError;
use crate::branch::merge::MergeType;
use crate::branch::merge::emit_conflict_events;
use crate::branch::merge::merge_abort;
use crate::branch::merge::validate_merge_in_progress;
use crate::branch::merge::validate_merge_type;
use crate::commit;
use crate::commit::CommitOptions;
use crate::event;
use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_info;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision;
use crate::revision::sync::LoreRevisionSyncProgressEventData;
use crate::runtime::execution_context;
use crate::stage;
use crate::stage::StageError;
use crate::state::State;

/// Event data reported at the start of a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertStartBeginEventData {
    /// Branch identifier.
    pub branch: BranchId,
    /// Identifier of the revision being reverted.
    pub revision: Hash,
    /// Number of the revision being reverted.
    pub revision_number: u64,
}

/// Event data reported at the end of a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertStartEndEventData {
    /// Progress statistics for the applied changes.
    pub stats: LoreRevisionSyncProgressEventData,
    /// Resulting revision hash signature.
    pub signature: Hash,
    /// Flag indicating the revert produced conflicts.
    pub has_conflicts: u8,
}

/// Event data reported at the start of aborting a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertAbortBeginEventData {
    /// Identifier of the staged revision being discarded.
    pub state_staged_revision: Hash,
    /// Identifier of the current revision being restored.
    pub state_current_revision: Hash,
}

/// Event data reported at the end of aborting a revert.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertAbortEndEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Event data reported when a file is resolved during a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertResolveFileEventData {
    /// Path of the file.
    pub path: LoreString,
}

/// Event data reported when a revision is resolved during a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertResolveRevisionEventData {
    /// Repository identifier.
    pub repository: RepositoryId,
    /// Identifier of the revision.
    pub revision: Hash,
}

/// Event data reported for a file in conflict during a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertConflictFileEventData {
    /// Path of the file.
    pub path: LoreString,
}

/// Event data reported when a file is unresolved during a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertUnresolveFileEventData {
    /// Path of the file.
    pub path: LoreString,
}

/// Event data reported when a revision is unresolved during a revert.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevertUnresolveRevisionEventData {
    /// Repository identifier.
    pub repository: RepositoryId,
    /// Identifier of the revision.
    pub revision: Hash,
}

#[derive(Clone, Debug)]
pub struct RevertOptions {
    /// Message to use for an auto commit if no conflicts arise
    pub message: String,
    /// Disable auto commits, even if no conflicts arise.
    pub no_commit: bool,
}

pub async fn revert(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    revision: Hash,
    options: RevertOptions,
) -> Result<Hash, MergeError> {
    let (state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;

    // Refuse on actually-staged nodes; tolerate dirty-only tracking by
    // snapshotting it into a `merge_carry` blob so it can be replayed
    // against the eventual revert commit (see `commit_impl`). Revert
    // commits set `parent_other = 0` (the reverted source is recorded
    // in `REVERTED_FROM` metadata, not the parent chain), so the carry
    // mirrors that for `take_matching` to fire.
    branch::merge::check_and_capture_dirty_for_merge(
        repository.clone(),
        state_staged.as_ref(),
        Hash::default(),
        state_current.revision(),
    )
    .await?;

    // Note: Reverting the current HEAD is valid (creates a commit that undoes it)
    // So we don't check for MergeSelf here

    // Check if a merge/cherry-pick/revert is already in progress
    validate_merge_in_progress(repository.clone()).await?;

    let target_revision = state_current.revision();

    // Get source revision info for the revision being reverted
    let source_state = State::deserialize(repository.clone(), revision)
        .await
        .forward::<MergeError>("deserializing source state")?;
    let source_revision_number = source_state.revision_number();
    let source_revision = source_state.revision();
    // For revert, we need the parent of the revision being reverted
    let base_revision = source_state.parent_self();

    lore_info!("Starting revert of revision {source_revision}");
    event::LoreEvent::RevertStartBegin(LoreRevertStartBeginEventData {
        branch: current_branch,
        revision: source_revision,
        revision_number: source_revision_number,
    })
    .send();

    // Revert uses diff3(source, base, target) to apply the inverse (source->base)
    let include_same = true;
    let diff = revision::diff3_collect(
        repository.clone(),
        source_revision,
        base_revision,
        target_revision,
        None,
        include_same,
    )
    .await
    .forward::<MergeError>("running diff3 for revert")?;

    let ApplyDiffResults {
        signature,
        conflicts,
        state_staged,
        stats,
    } = branch::merge::apply_diff(
        repository.clone(),
        token,
        diff,
        state_current.clone(),
        MergeType::Revert,
        false,
        current_branch,
    )
    .await?;

    let has_conflicts = state_staged.is_conflict();

    event::LoreEvent::RevertStartEnd(LoreRevertStartEndEventData {
        stats: LoreRevisionSyncProgressEventData::new(&stats),
        signature,
        has_conflicts: has_conflicts as u8,
    })
    .send();

    if has_conflicts {
        emit_conflict_events(
            repository.clone(),
            &state_staged,
            &conflicts,
            MergeType::Revert,
        )
        .await;
    }

    let dry_run = execution_context().globals().dry_run();
    let signature = if !has_conflicts && !dry_run && !options.no_commit {
        let commit_options = CommitOptions {
            message: options.message,
            link_messages: std::collections::HashMap::new(),
            link: None,
            layer_messages: std::collections::HashMap::new(),
            layer: None,
            stats: false,
        };

        Box::pin(commit::commit(repository.clone(), token, commit_options))
            .await
            .forward::<MergeError>("committing revert")?
    } else {
        signature
    };

    Ok(signature)
}

pub async fn revert_abort(repository: Arc<RepositoryContext>) -> Result<(), MergeError> {
    merge_abort(repository, MergeType::Revert).await
}

pub async fn revert_restart(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), MergeError> {
    let (state_current, state_staged, _current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    validate_merge_type(repository.clone(), state_staged.clone(), MergeType::Revert).await?;

    let state_staged_metadata = state_staged
        .revision_metadata(repository.clone())
        .await
        .forward::<MergeError>("loading staged revision metadata")?;

    let state_revert = State::deserialize(repository.clone(), state_staged_metadata.reverted_from)
        .await
        .forward::<MergeError>("deserializing reverted-from state")?;

    // For revert: diff3(source_revision, base_revision, target_revision)
    // where source_revision is the revision being reverted and base_revision is its parent
    let include_same = true;
    let diff = revision::diff3_collect(
        repository.clone(),
        state_revert.revision(),
        state_revert.parent_self(),
        state_current.revision(),
        None,
        include_same,
    )
    .await
    .forward::<MergeError>("running diff3 for revert restart")?;
    branch::merge::apply_restart_diff(
        repository,
        token,
        paths,
        diff,
        state_staged,
        branch::merge::MergeType::Revert,
    )
    .await
}

pub async fn revert_unresolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), MergeError> {
    branch::merge::merge_unresolve(repository, token, paths, MergeType::Revert).await
}

pub async fn revert_resolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), MergeError> {
    branch::merge::merge_resolve(repository, token, paths, MergeType::Revert).await
}

pub async fn revert_resolve_mine(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), StageError> {
    validate_merge_type(repository.clone(), None, MergeType::Revert)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    Box::pin(stage::stage_from_parent_revision(
        repository,
        token,
        paths,
        stage::MergeParent::Mine,
    ))
    .await
}

pub async fn revert_resolve_theirs(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), StageError> {
    validate_merge_type(repository.clone(), None, MergeType::Revert)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    Box::pin(stage::stage_from_parent_revision(
        repository,
        token,
        paths,
        stage::MergeParent::Revert,
    ))
    .await
}
