// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::branch;
use crate::branch::push::PushStatistics;
use crate::branch::push::push_fragments;
use crate::branch::push::push_query;
use crate::change;
use crate::change::NodeChange;
use crate::commit;
use crate::commit::CommitOptions;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::find;
use crate::infer;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreEvent;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_trace;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::node;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeLink;
use crate::path::emit_path_ignore;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::revision::DiffResult;
use crate::revision::cherry_pick::LoreCherryPickAbortBeginEventData;
use crate::revision::cherry_pick::LoreCherryPickAbortEndEventData;
use crate::revision::cherry_pick::LoreCherryPickResolveFileEventData;
use crate::revision::cherry_pick::LoreCherryPickResolveRevisionEventData;
use crate::revision::cherry_pick::LoreCherryPickUnresolveFileEventData;
use crate::revision::cherry_pick::LoreCherryPickUnresolveRevisionEventData;
use crate::revision::revert::LoreRevertAbortBeginEventData;
use crate::revision::revert::LoreRevertAbortEndEventData;
use crate::revision::revert::LoreRevertConflictFileEventData;
use crate::revision::revert::LoreRevertResolveFileEventData;
use crate::revision::revert::LoreRevertResolveRevisionEventData;
use crate::revision::revert::LoreRevertUnresolveFileEventData;
use crate::revision::revert::LoreRevertUnresolveRevisionEventData;
use crate::revision::sync;
use crate::revision::sync::LoreRevisionSyncProgressEventData;
use crate::stage;
use crate::stage::StageError;
use crate::state;
use crate::state::LinkMergeEntry;
use crate::state::State;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

/// Data for the event sent when a branch merge starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeStartBeginEventData {
    /// The source branch being merged.
    pub branch: BranchId,
    /// The source revision being merged.
    pub revision: Hash,
    /// The sequential number of the source revision.
    pub revision_number: u64,
}

/// Data for the event sent when a branch merge finishes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeStartEndEventData {
    /// Progress totals collected while applying the merge.
    pub stats: LoreRevisionSyncProgressEventData,
    /// The revision produced by the merge.
    pub signature: Hash,
    /// Set when the merge produced file conflicts.
    pub has_conflicts: u8,
}

/// Data for the event sent when a branch merge abort starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeAbortBeginEventData {
    /// The staged revision being discarded.
    pub state_staged_revision: Hash,
    /// The current revision the working state returns to.
    pub state_current_revision: Hash,
}

/// Data for the event sent when a branch merge abort finishes.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeAbortEndEventData {
    /// Placeholder field. The event carries no payload.
    pub _unused: u32,
}

/// Data for the event sent before files are merged into the working tree.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFileBeginEventData {
    /// The number of files to merge.
    pub count: usize,
}

/// Data for the event sent for each file merged into the working tree.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFileEventData {
    /// The path of the file.
    pub path: LoreString,
    /// The action applied to the file.
    pub action: LoreFileAction,
    /// The size of the file in bytes.
    pub size: u64,
    /// Set when the entry is a regular file.
    #[serde(with = "u8_as_bool")]
    pub is_file: u8,
    /// Set when the entry is a directory.
    #[serde(with = "u8_as_bool")]
    pub is_directory: u8,
    /// Set when the entry is a link.
    #[serde(with = "u8_as_bool")]
    pub is_link: u8,
}

/// Data for the event sent after files are merged into the working tree.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFileEndEventData {
    /// The number of files merged.
    pub count: usize,
}

/// Data for the event sent before the merge synchronizes revisions.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoSyncBeginEventData {
    /// The number of revisions to synchronize.
    pub count: usize,
}

/// Data for the event sent after the merge synchronizes revisions.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoSyncEndEventData {
    /// The number of revisions synchronized.
    pub count: usize,
}

/// Data for the event sent before the merge transfers fragments.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFragmentBeginEventData {
    /// The number of fragments to transfer.
    pub fragments: u64,
}

/// Data for the event sent as the merge transfers fragments.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFragmentProgressEventData {
    /// The number of fragments transferred so far.
    pub complete: u64,
    /// The total number of fragments to transfer.
    pub count: u64,
}

/// Data for the event sent after the merge transfers fragments.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFragmentEndEventData {
    /// The number of fragments transferred.
    pub fragments: u64,
}

/// Data for the event sent for each revision merged into the working tree.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoRevisionEventData {
    /// The revision merged.
    pub revision: Hash,
    /// The sequential number of the revision.
    pub revision_number: u64,
}

/// Data for the event sent for each file the merge left in conflict.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeConflictFileEventData {
    /// The path of the conflicted file.
    pub path: LoreString,
}

/// Reason a link was skipped during a multi-repo merge.
///
/// Numeric values are kept stable so that FFI consumers can switch on them.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize, Debug)]
pub enum LinkMergeSkipReason {
    /// The link's tracked branch has `DisableAutoFollow` set.
    AutoFollowDisabled = 0,
    /// The link is not currently accessible (offline / not cloned).
    Inaccessible = 1,
    /// The link's pin already matches the source branch tip — nothing to merge.
    AlreadyAtSource = 2,
    /// Source and current revisions agree on every file — no synthetic merge needed.
    NoContentDivergence = 3,
}

/// Data for the event sent when a link is skipped during a merge.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeLinkSkippedEventData {
    /// The mount path of the skipped link.
    pub link_path: LoreString,
    /// The repository of the skipped link.
    pub repository: RepositoryId,
    /// The reason the link was skipped.
    pub reason: u8,
}

/// Data for the event sent when a file in a merge is marked unresolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeUnresolveFileEventData {
    /// The path of the file marked unresolved.
    pub path: LoreString,
}

/// Data for the event sent when a revision in a merge is marked unresolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeUnresolveRevisionEventData {
    /// The repository of the revision marked unresolved.
    pub repository: RepositoryId,
    /// The revision marked unresolved.
    pub revision: Hash,
}

/// Data for the event sent when a file in a merge is marked resolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeResolveFileEventData {
    /// The path of the file marked resolved.
    pub path: LoreString,
}

/// Data for the event sent when a revision in a merge is marked resolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeResolveRevisionEventData {
    /// The repository of the revision marked resolved.
    pub repository: RepositoryId,
    /// The revision marked resolved.
    pub revision: Hash,
}

#[error_set]
pub enum MergeError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    NothingStaged,
    BranchAdvanced,
    Conflict,
    InvalidArguments,
    AlreadyLinked,
    LayerNotFound,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    LinkPathNotFound,
    NotALink,
    NotALayer,
    BranchAlreadyExists,
    NotConnected,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    LocalModifications,
    MaxHistorySearchDepth,
    IdenticalMetadata,
    LockNotFound,
    LockNotOwned,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for MergeError {
    fn translated(&self) -> LoreError {
        match self {
            MergeError::Disconnected(_) => LoreError::Connection,
            MergeError::SlowDown(_) => LoreError::SlowDown,
            MergeError::Oversized(_) => LoreError::Oversized,
            MergeError::FileNotFound(_) => LoreError::FileNotFound,
            MergeError::NotFound(_)
            | MergeError::BranchNotFound(_)
            | MergeError::LayerNotFound(_)
            | MergeError::RevisionNotFound(_) => LoreError::NotFound,
            MergeError::AddressNotFound(_) => LoreError::AddressNotFound,
            MergeError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            MergeError::InvalidPath(_) | MergeError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Controls which repositories participate in a merge operation.
#[derive(Clone, Debug)]
pub enum MergeScope {
    /// Merge only the main repository (no linked repos).
    MainOnly,
    /// Merge a single linked repository at the given mount path.
    Link(String),
    /// Merge all linked repositories together with the main repo.
    All,
}

#[derive(Clone, Debug)]
pub struct MergeStartOptions {
    /// Message to use for an auto commit if no conflicts arise
    pub message: String,
    /// Disable auto commits, even if no conflicts arise.
    pub no_commit: bool,
    /// Which repositories to include in the merge.
    pub scope: MergeScope,
}

/// Result of merging a single repository (main or linked).
pub struct MergeRepositoryResult {
    pub signature: Hash,
    pub has_conflicts: bool,
    pub state_staged: Arc<State>,
    /// Conflict-realization context: the 3-way merge inputs and the conflict
    /// pairs from the diff. Surfaced so `merge_start_all` can remap paths
    /// onto the link's mount and call `realize_conflicts` without re-running
    /// diff3. `None` when no conflicts (`!has_conflicts`).
    pub conflict_context: Option<ConflictRealizeContext>,
}

/// Inputs needed to materialise conflict markers and `.mine`/`.theirs`/
/// `.base` sidecars on disk after a merge. Produced by `merge_repository`,
/// consumed by `merge_start_all` after `stage_link_pin` has placed the
/// link's pre-merge content on disk.
#[derive(Clone)]
pub struct ConflictRealizeContext {
    pub state_base: Arc<State>,
    pub state_from: Arc<State>,
    pub state_to: Arc<State>,
    /// Paths are relative to the merged repository's root — for a linked
    /// repo merge, the caller must remap to the parent's mount path.
    pub conflicts: Arc<Vec<(NodeChange, NodeChange)>>,
}

async fn merge_repository(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    source_branch: BranchId,
    current_branch: BranchId,
    current_signature: Hash,
    state_current: Arc<State>,
) -> Result<MergeRepositoryResult, MergeError> {
    let latest_merge = branch::load_latest(repository.clone(), source_branch)
        .await
        .ok();
    lore_debug!("Local latest {latest_merge:?}");
    let latest_remote = if !execution_context().globals().offline() {
        let remote = repository
            .remote()
            .await
            .forward::<MergeError>("acquiring remote")?;
        match branch::load_remote_latest(remote, repository.id, source_branch).await {
            Ok(remote_latest) => Some(remote_latest),
            Err(err) if err.is_branch_not_found() => None,
            Err(err) => {
                return Err(err).forward::<MergeError>("loading remote branch latest");
            }
        }
    } else {
        None
    };
    lore_debug!("Remote latest {latest_remote:?}");
    if latest_merge.is_none() && latest_remote.is_none() {
        if !branch::exist_local(repository.clone(), source_branch).await {
            return Err(MergeError::internal("Branch not found"));
        }
        return Err(MergeError::internal("Unable to load latest revision"));
    }

    let mut revision = latest_merge.unwrap_or_default();
    if let Some(latest_remote) = latest_remote
        && revision != latest_remote
    {
        let state_merge = state::State::deserialize(repository.clone(), revision)
            .await
            .forward::<MergeError>("deserializing local latest state")?;
        let state_remote = state::State::deserialize(repository.clone(), latest_remote)
            .await
            .forward::<MergeError>("deserializing remote latest state")?;

        // See if remote latest revision is ahead of local latest revision
        if find::find_revision(
            repository.clone(),
            current_branch,
            state_remote.revision(),
            false,
            None,
            |state, _metadata| {
                if state.revision() == state_merge.revision()
                    || state.parent_other() == state_merge.revision()
                {
                    // Remote latest revision is ahead of local latest revision and no divergence, use it
                    lore_debug!("Local LATEST found when iterating remote branch revisions");
                    find::FindMatchResult::Match
                } else if state.revision_number() < state_merge.revision_number() {
                    // Divergence, the remote branch history passed the point
                    // where local latest revision should have been found
                    lore_debug!("Local LATEST is not found when iterating remote branch revisions");
                    find::FindMatchResult::Abort
                } else {
                    find::FindMatchResult::Continue
                }
            },
        )
        .await
        .is_ok()
        {
            // Use the remote latest as we found it to be ahead of local latest
            revision = latest_remote;
        } else {
            // See if local latest revision is ahead of remote latest revision
            find::find_revision(
                repository.clone(),
                current_branch,
                state_merge.revision(),
                false,
                None,
                |state, _metadata| {
                    if state.revision() == state_remote.revision()
                        || state.parent_other() == state_remote.revision()
                    {
                        // Local latest revision is ahead of remote latest revision and no divergence, use it (already set)
                        lore_debug!("Remote LATEST found when iterating local branch revisions");
                        find::FindMatchResult::Match
                    } else if state.revision_number() < state_remote.revision_number() {
                        // Divergence, the local branch history passed the point where remote latest revision should
                        // have been found. If the branch is the current branch we continue, as the merge
                        // is to join the divergence. Otherwise we error out as we do not know which revision
                        // to use in the merge.
                        lore_debug!(
                            "Remote LATEST is not found when iterating local branch revisions"
                        );
                        if current_branch == source_branch {
                            revision = latest_remote;
                            find::FindMatchResult::Match
                        } else {
                            find::FindMatchResult::Abort
                        }
                    } else {
                        find::FindMatchResult::Continue
                    }
                },
            )
            .await
            .forward::<MergeError>("locating divergent source revision")?;
        }
    }

    let revision_number = {
        let state = state::State::deserialize(repository.clone(), revision)
            .await
            .forward::<MergeError>("deserializing merge revision state")?;
        state.revision_number()
    };

    lore_info!("Starting merge of branch {source_branch} revision {revision}");
    event::LoreEvent::BranchMergeStartBegin(LoreBranchMergeStartBeginEventData {
        branch: source_branch,
        revision,
        revision_number,
    })
    .send();

    if current_branch == source_branch && current_signature == revision {
        return Err(MergeError::internal("Cannot merge a branch with itself"));
    }

    let diff = Box::pin(branch::diff3_collect(
        repository.clone(),
        source_branch,
        revision,
        current_branch,
        current_signature,
        None,  /* No path */
        true,  /* Include identical changes for merge tracking */
        false, /* Do not autoresolve, this is done later */
    ))
    .await
    .forward::<MergeError>("running diff3 for merge")?;

    // Capture the diff's revision endpoints before `apply_diff` consumes it —
    // `merge_start_all` needs the corresponding states to realize conflict
    // markers at a remapped mount path without re-running diff3.
    let (diff_base, diff_source, diff_target) = (diff.base, diff.source, diff.target);

    let ApplyDiffResults {
        signature,
        conflicts,
        state_staged,
        stats,
    } = apply_diff(
        repository.clone(),
        token,
        diff,
        state_current,
        MergeType::BranchMerge,
        // If this merge is reconciling a remote LATEST with local LATEST of a branch
        // reverse the parent order in order to keep the remote history as the main
        // history line shows in CLI output and other places
        current_branch == source_branch,
        current_branch,
    )
    .await?;

    let has_conflicts = state_staged.is_conflict();

    event::LoreEvent::BranchMergeStartEnd(LoreBranchMergeStartEndEventData {
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
            MergeType::BranchMerge,
        )
        .await;
    }

    let conflict_context = if has_conflicts {
        let state_base = state::State::deserialize(repository.clone(), diff_base)
            .await
            .forward::<MergeError>("deserializing diff base state")?;
        let state_from = state::State::deserialize(repository.clone(), diff_source)
            .await
            .forward::<MergeError>("deserializing diff source state")?;
        let state_to = state::State::deserialize(repository.clone(), diff_target)
            .await
            .forward::<MergeError>("deserializing diff target state")?;
        Some(ConflictRealizeContext {
            state_base,
            state_from,
            state_to,
            conflicts,
        })
    } else {
        None
    };

    Ok(MergeRepositoryResult {
        signature,
        has_conflicts,
        state_staged,
        conflict_context,
    })
}

#[allow(clippy::large_futures)]
pub async fn merge_start(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    options: MergeStartOptions,
) -> Result<Hash, MergeError> {
    let (state_current, state_staged_opt, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;

    match &options.scope {
        MergeScope::Link(link_path) => {
            // Link-scoped merges work on the staged state if one exists,
            // since the parent repo may have independently staged changes.
            let link_path = link_path.clone();
            let has_staged = state_staged_opt.is_some();
            let state = state_staged_opt.unwrap_or_else(|| state_current.clone());
            validate_merge_in_progress(repository.clone()).await?;
            if !has_staged {
                // No pre-existing staged state — set parent_self so the new staged
                // state knows its parent for commit.
                state.set_parent_self(state_current.revision());
            }
            merge_start_link(
                repository,
                token,
                branch,
                options,
                state,
                current_branch,
                link_path,
            )
            .await
        }
        MergeScope::MainOnly => {
            let source_latest = branch::load_latest(repository.clone(), branch)
                .await
                .ok()
                .unwrap_or_default();
            check_and_capture_dirty_for_merge(
                repository.clone(),
                state_staged_opt.as_ref(),
                source_latest,
                state_current.revision(),
            )
            .await?;
            validate_merge_in_progress(repository.clone()).await?;
            let MergeRepositoryResult {
                mut signature,
                has_conflicts,
                ..
            } = merge_repository(
                repository.clone(),
                token,
                branch,
                current_branch,
                state_current.revision(),
                state_current,
            )
            .await?;

            let dry_run = execution_context().globals().dry_run();
            if !has_conflicts && !dry_run && !options.no_commit {
                signature = auto_commit_merge(repository, token, options.message).await?;
            }

            Ok(signature)
        }
        MergeScope::All => {
            // Resume requires both `is_merge_*()` AND non-empty entries: a
            // stray staged state from another operation (cherry-pick, manual
            // link update) with link merge bytes left over must NOT be
            // treated as resumable. A dirty-only staged state (no merge
            // flags, no actually-staged nodes) is allowed and gets carried
            // through via `merge_carry`.
            let is_resume = if let Some(ref state_staged) = state_staged_opt {
                if state_staged.is_merge_or_cherry_pick_or_revert() {
                    let entries = state_staged
                        .deserialize_link_merge_state(repository.clone())
                        .await
                        .forward::<MergeError>("deserializing link merge state")?;
                    if entries.is_empty() {
                        return Err(MergeError::internal("Cannot merge with staged state"));
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            };
            // Dirty-only carry: capture before the merge writes a new
            // staged anchor. Resume passes skip this (the staged state is
            // already the in-progress merge — the carry was stored on the
            // original `merge start`).
            if !is_resume {
                let source_latest = branch::load_latest(repository.clone(), branch)
                    .await
                    .ok()
                    .unwrap_or_default();
                check_and_capture_dirty_for_merge(
                    repository.clone(),
                    state_staged_opt.as_ref(),
                    source_latest,
                    state_current.revision(),
                )
                .await?;
            }
            if !is_resume {
                validate_merge_in_progress(repository.clone()).await?;
            }
            merge_start_all(
                repository,
                token,
                branch,
                options,
                state_current,
                current_branch,
                if is_resume { state_staged_opt } else { None },
            )
            .await
        }
    }
}

/// Merge a specific linked repository only (`MergeScope::Link`).
async fn merge_start_link(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    options: MergeStartOptions,
    state: Arc<State>,
    current_branch: BranchId,
    link_path: String,
) -> Result<Hash, MergeError> {
    let link_path_rel = RelativePath::from_str(&link_path)
        .internal_with(|| format!("link not found: {link_path}"))?;

    let link::ResolvedLink {
        link_node,
        link_context,
        link_reference,
    } = link::resolve_link_at_path(&state, repository.clone(), &link_path)
        .await
        .forward_with::<MergeError, _>(|| format!("link not found: {link_path}"))?;

    match link::check_link_merge_eligible(&link_context, &link_reference, branch).await {
        link::LinkMergeEligibility::Eligible => {}
        link::LinkMergeEligibility::Skip => {
            event::LoreEvent::BranchMergeLinkSkipped(LoreBranchMergeLinkSkippedEventData {
                link_path: LoreString::from(link_path.as_str()),
                repository: link_context.id,
                reason: LinkMergeSkipReason::Inaccessible as u8,
            })
            .send();
            return Ok(state.revision());
        }
        link::LinkMergeEligibility::AutoFollowDisabled => {
            return Err(MergeError::internal(format!(
                "Link {link_path} has auto-follow disabled"
            )));
        }
    }

    let link_branch = link_reference.resolve_branch(current_branch);

    // Avoid creating a synthetic merge revision when one side is simply
    // ahead. Mirrors `merge_start_all` so `--link <path>` and the default
    // merge behave consistently.
    let mut source_latest = branch::load_latest(link_context.clone(), branch)
        .await
        .ok()
        .unwrap_or_default();
    if let Ok(remote) = link_context.remote().await
        && let Ok(remote_latest) = branch::load_remote_latest(remote, link_context.id, branch).await
        && (source_latest.is_zero() || remote_latest != source_latest)
    {
        source_latest = remote_latest;
    }
    if !source_latest.is_zero()
        && !link::link_has_content_divergence(
            &link_context,
            branch,
            source_latest,
            link_branch,
            link_reference.signature,
        )
        .await
    {
        lore_debug!(
            "Skipping link merge at {link_path} — no content divergence between source and current"
        );
        event::LoreEvent::BranchMergeLinkSkipped(LoreBranchMergeLinkSkippedEventData {
            link_path: LoreString::from(link_path.as_str()),
            repository: link_context.id,
            reason: LinkMergeSkipReason::NoContentDivergence as u8,
        })
        .send();
        return Ok(state.revision());
    }

    // Deserialize the link's current state to use as the merge base
    let link_state = state::State::deserialize(link_context.clone(), link_reference.signature)
        .await
        .forward::<MergeError>("deserializing link state")?;

    // Save pre-merge snapshot for rollback
    let link_merge_entry = LinkMergeEntry {
        local_node: link_reference.local_node,
        reserved: 0,
        base: link_reference,
    };

    // Merge the linked repository
    let result = merge_repository(
        link_context.clone(),
        token,
        branch,
        link_branch,
        link_reference.signature,
        link_state,
    )
    .await
    .forward_with::<MergeError, _>(|| format!("merging link {link_path}"))?;

    let dry_run = execution_context().globals().dry_run();
    if dry_run {
        return Ok(state.revision());
    }

    // The staged link state has revision_number=0; the eventual `lore
    // commit` calls `weave_history` on the link, bumps the number to
    // `max(parent_self, parent_other) + 1`, and updates the parent's pin
    // via `update_link_pin_by_node` in the same commit.
    //
    // Pass the raw branch value (not `link_branch`) to preserve the
    // implicit-branch convention where zero means "follow parent".
    link::stage_link_pin(
        repository.clone(),
        &state,
        &link_context,
        link_path_rel,
        link_node,
        link_reference.signature,
        result.signature,
        link_reference.branch,
    )
    .await
    .forward_with::<MergeError, _>(|| format!("staging link pin for {link_path}"))?;

    state
        .serialize_link_merge_state(repository.clone(), &[link_merge_entry])
        .await
        .forward::<MergeError>("serializing link merge state")?;

    // Don't set merge parents/flags here — the parent repo is not being
    // merged, only the link pin is updated. Keeping the staged state as a
    // normal staged change lets the user commit or abort cleanly.
    // (`parent_self` was set when the staged state was created.)
    //
    // For a conflicted link merge we DO need the merge flags so
    // `merge resolve` / `merge abort` recognise an in-progress merge via
    // `is_merge_or_cherry_pick_or_revert()`.
    if result.has_conflicts {
        state.set_merge_conflict();
    }

    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing staged state")?;

    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<MergeError>("storing staged anchor")?;

    // Auto-commit if no conflicts
    if !result.has_conflicts && !options.no_commit {
        let commit_options = CommitOptions {
            message: options.message,
            link_messages: std::collections::HashMap::new(),
            link: None,
            layer_messages: std::collections::HashMap::new(),
            layer: None,
            stats: false,
        };
        let signature = Box::pin(commit::commit(repository, token, commit_options))
            .await
            .forward::<MergeError>("auto-committing merge")?;
        return Ok(signature);
    }

    Ok(signature)
}

/// A link the parent merge will operate on after eligibility checks pass.
struct EligibleLink {
    link_path: String,
    link_node: Node,
    link_context: Arc<RepositoryContext>,
    link_reference: state::LinkReference,
    resolved_branch: BranchId,
}

/// A link whose merge produced a (possibly conflicted) staged state. Its pin
/// will be applied to the parent's staged state in a subsequent step.
struct MergedLink {
    link_path: String,
    /// Cached `RelativePath` form of `link_path` so the post-loop blocks
    /// don't re-parse it for every `stage_link_pin` call.
    link_path_rel: RelativePath,
    link_node: Node,
    link_context: Arc<RepositoryContext>,
    link_reference: state::LinkReference,
    new_signature: Hash,
}

/// Inputs captured for a link merge that produced conflicts so the caller can
/// realize markers + sidecars on disk *after* `stage_link_pin` runs (the pin
/// step expects the link's pre-merge content on disk; markers are written on
/// top once the pin is in place).
struct PendingConflictRealize {
    link_context: Arc<RepositoryContext>,
    link_path: String,
    context: ConflictRealizeContext,
}

/// Run upfront eligibility checks for every link in `state_current`, returning
/// the eligible set and emitting `BranchMergeLinkSkipped` events for the rest.
async fn enumerate_eligible_links(
    repository: &Arc<RepositoryContext>,
    state_current: &Arc<State>,
    branch: BranchId,
    current_branch: BranchId,
) -> Result<Vec<EligibleLink>, MergeError> {
    let link_list = state_current
        .link_list(repository.clone())
        .await
        .forward::<MergeError>("listing links")?;

    // This loop is sequential. The per-link awaits (`node_path`, `node`,
    // `to_link_context`, `check_link_merge_eligible`) are independent and
    // could fan out via `JoinSet`/`MAX_TASK_COUNT` for workspaces with many
    // links. Deferred — N is small in practice for current users.
    let mut eligible_links = Vec::new();
    for link_reference in &link_list {
        let link_path = state_current
            .node_path(repository.clone(), link_reference.local_node)
            .await
            .forward::<MergeError>("resolving link node path")?;
        let link_node = state_current
            .node(repository.clone(), link_reference.local_node)
            .await
            .forward::<MergeError>("loading link node")?;
        let link_context = Arc::new(
            repository
                .to_link_context(link_node.address.context.into())
                .await,
        );

        match link::check_link_merge_eligible(&link_context, link_reference, branch).await {
            link::LinkMergeEligibility::Eligible => {
                let resolved_branch = link_reference.resolve_branch(current_branch);
                eligible_links.push(EligibleLink {
                    link_path,
                    link_node,
                    link_context,
                    link_reference: *link_reference,
                    resolved_branch,
                });
            }
            link::LinkMergeEligibility::Skip => {
                lore_debug!("Skipping inaccessible link at {link_path}");
                event::LoreEvent::BranchMergeLinkSkipped(LoreBranchMergeLinkSkippedEventData {
                    link_path: LoreString::from(link_path.as_str()),
                    repository: link_context.id,
                    reason: LinkMergeSkipReason::Inaccessible as u8,
                })
                .send();
            }
            link::LinkMergeEligibility::AutoFollowDisabled => {
                lore_debug!("Skipping DisableAutoFollow link at {link_path}");
                event::LoreEvent::BranchMergeLinkSkipped(LoreBranchMergeLinkSkippedEventData {
                    link_path: LoreString::from(link_path.as_str()),
                    repository: link_context.id,
                    reason: LinkMergeSkipReason::AutoFollowDisabled as u8,
                })
                .send();
            }
        }
    }
    Ok(eligible_links)
}

/// Pre-populate `MergedLink`s for entries already in the staged
/// `LinkMergeState` (from a prior resume pass) so the conflict / main-merge
/// tail re-applies their pins instead of regressing them to `state_current`'s
/// pre-merge values.
async fn seed_resumed_merged_links(
    repository: &Arc<RepositoryContext>,
    state_staged_opt: &Option<Arc<State>>,
    link_merge_entries: &[LinkMergeEntry],
) -> Result<Vec<MergedLink>, MergeError> {
    let mut merged_links: Vec<MergedLink> = Vec::new();
    let Some(state_staged) = state_staged_opt else {
        return Ok(merged_links);
    };
    if link_merge_entries.is_empty() {
        return Ok(merged_links);
    }
    let staged_link_list = state_staged
        .link_list(repository.clone())
        .await
        .forward::<MergeError>("listing staged links")?;
    for entry in link_merge_entries {
        let staged_ref = staged_link_list
            .iter()
            .find(|l| l.local_node == entry.local_node)
            .copied();
        let Some(staged_ref) = staged_ref else {
            continue;
        };
        let link_path = state_staged
            .node_path(repository.clone(), staged_ref.local_node)
            .await
            .forward::<MergeError>("resolving staged link node path")?;
        let link_path_rel = RelativePath::from_str(&link_path)
            .internal_with(|| format!("link not found: {link_path}"))?;
        let link_node = state_staged
            .node(repository.clone(), staged_ref.local_node)
            .await
            .forward::<MergeError>("loading staged link node")?;
        let link_context = Arc::new(
            repository
                .to_link_context(link_node.address.context.into())
                .await,
        );
        merged_links.push(MergedLink {
            link_path,
            link_path_rel,
            link_node,
            link_context,
            // `stage_link_pin` is told the old signature so it can realize
            // the file-system delta — use the entry's recorded base.
            link_reference: entry.base,
            new_signature: staged_ref.signature,
        });
    }
    Ok(merged_links)
}

/// Merge all eligible linked repositories, then the main repository
/// (`MergeScope::All`).
///
/// Reads top-down: enumerate eligible links, load existing
/// `LinkMergeState` for resume detection, seed `merged_links` for any
/// already-merged entries, merge each remaining eligible link in turn,
/// and finalize either as a merge-in-conflict (if any link conflicted)
/// or by merging the main repository.
async fn merge_start_all(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    options: MergeStartOptions,
    state_current: Arc<State>,
    current_branch: BranchId,
    state_staged_opt: Option<Arc<State>>,
) -> Result<Hash, MergeError> {
    let dry_run = execution_context().globals().dry_run();

    let eligible_links =
        enumerate_eligible_links(&repository, &state_current, branch, current_branch).await?;

    let mut link_merge_entries = if let Some(ref state_staged) = state_staged_opt {
        state_staged
            .deserialize_link_merge_state(repository.clone())
            .await
            .forward::<MergeError>("deserializing link merge state")?
    } else {
        Vec::new()
    };

    let mut merged_links =
        seed_resumed_merged_links(&repository, &state_staged_opt, &link_merge_entries).await?;
    let mut has_link_conflicts = false;
    let mut pending_conflict_realizes: Vec<PendingConflictRealize> = Vec::new();

    for eligible in &eligible_links {
        // Resume: already merged on a prior pass.
        if link_merge_entries
            .iter()
            .any(|e| e.local_node == eligible.link_reference.local_node)
        {
            lore_debug!(
                "Resuming: skipping already-merged link at {}",
                eligible.link_path
            );
            continue;
        }

        // Check both local and remote: `merge_into --link` may have pushed
        // to remote without touching the local latest.
        let mut source_latest = branch::load_latest(eligible.link_context.clone(), branch)
            .await
            .ok()
            .unwrap_or_default();
        if let Ok(remote) = eligible.link_context.remote().await
            && let Ok(remote_latest) =
                branch::load_remote_latest(remote, eligible.link_context.id, branch).await
            && (source_latest.is_zero() || remote_latest != source_latest)
        {
            // Prefer remote when local and remote diverge — push local-only
            // commits before merging if you want them honored. We do not
            // compare revision numbers here.
            source_latest = remote_latest;
        }

        if !source_latest.is_zero() && eligible.link_reference.signature == source_latest {
            lore_debug!(
                "Skipping link at {} — pin already matches source branch",
                eligible.link_path
            );
            event::LoreEvent::BranchMergeLinkSkipped(LoreBranchMergeLinkSkippedEventData {
                link_path: LoreString::from(eligible.link_path.as_str()),
                repository: eligible.link_context.id,
                reason: LinkMergeSkipReason::AlreadyAtSource as u8,
            })
            .send();
            continue;
        }

        // Avoid creating an unnecessary merge commit when one side is simply
        // ahead of the other.
        if !link::link_has_content_divergence(
            &eligible.link_context,
            branch,
            source_latest,
            eligible.resolved_branch,
            eligible.link_reference.signature,
        )
        .await
        {
            lore_debug!(
                "Skipping link at {} — no content divergence",
                eligible.link_path
            );
            event::LoreEvent::BranchMergeLinkSkipped(LoreBranchMergeLinkSkippedEventData {
                link_path: LoreString::from(eligible.link_path.as_str()),
                repository: eligible.link_context.id,
                reason: LinkMergeSkipReason::NoContentDivergence as u8,
            })
            .send();
            continue;
        }

        let link_state = state::State::deserialize(
            eligible.link_context.clone(),
            eligible.link_reference.signature,
        )
        .await
        .forward::<MergeError>("deserializing eligible link state")?;

        let result = merge_repository(
            eligible.link_context.clone(),
            token,
            branch,
            eligible.resolved_branch,
            eligible.link_reference.signature,
            link_state,
        )
        .await
        .forward_with::<MergeError, _>(|| format!("merging link {}", eligible.link_path))?;

        if result.has_conflicts {
            has_link_conflicts = true;

            // Defer marker realization until after `stage_link_pin` puts the
            // link's pre-merge content on disk; otherwise `stage_link_pin`'s
            // file-system verify rejects the on-disk size as locally
            // modified. The conflict event is deferred for the same reason
            // (see `finalize_link_conflict_state`).
            if !dry_run && let Some(ctx) = result.conflict_context.clone() {
                pending_conflict_realizes.push(PendingConflictRealize {
                    link_context: eligible.link_context.clone(),
                    link_path: eligible.link_path.clone(),
                    context: ctx,
                });
            }
        }

        if !dry_run {
            // We pin the parent at the rev-0 link signature. The eventual
            // `lore commit` runs `weave_history` on the link via
            // `commit_link_node`, bumps the revision number, and updates the
            // parent's pin to the post-bump signature in the same commit.

            let link_merge_entry = LinkMergeEntry {
                local_node: eligible.link_reference.local_node,
                reserved: 0,
                base: eligible.link_reference,
            };
            link_merge_entries.push(link_merge_entry);

            let link_path_rel = RelativePath::from_str(&eligible.link_path)
                .internal_with(|| format!("link not found: {}", eligible.link_path))?;
            merged_links.push(MergedLink {
                link_path: eligible.link_path.clone(),
                link_path_rel,
                link_node: eligible.link_node,
                link_context: eligible.link_context.clone(),
                link_reference: eligible.link_reference,
                new_signature: result.signature,
            });

            // Atomicity checkpoint: persist `link_merge_entries` onto a
            // parent staged state so a mid-loop failure (network, panic)
            // leaves an abortable trail. Without this, the entries live
            // only in this function's local Vec, and a partial run leaves
            // the link's own staged anchors orphaned with no way for
            // `branch_merge_abort` to find them.
            //
            // Deserialize fresh (rather than reuse `state_current`'s Arc)
            // so checkpoint mutations don't bleed into the parent merge's
            // input state. The next phase overwrites this checkpoint.
            let checkpoint =
                state::State::deserialize(repository.clone(), state_current.revision())
                    .await
                    .forward::<MergeError>("deserializing checkpoint state")?;
            checkpoint.set_parent_self(state_current.revision());
            checkpoint
                .serialize_link_merge_state(repository.clone(), &link_merge_entries)
                .await
                .forward::<MergeError>("serializing checkpoint link merge state")?;
            checkpoint.set_merge_conflict();
            let checkpoint_sig = checkpoint
                .serialize(repository.clone(), token)
                .await
                .forward::<MergeError>("serializing checkpoint state")?;
            crate::instance::store_staged_anchor(&repository, checkpoint_sig)
                .await
                .forward::<MergeError>("storing checkpoint anchor")?;
        }
    }

    // If any link merge produced file conflicts, finalise the parent state as
    // a merge in conflict and stop. Main's merge cannot run safely against an
    // unresolved link state.
    if has_link_conflicts && !dry_run {
        let signature = finalize_link_conflict_state(
            &repository,
            token,
            &state_current,
            &merged_links,
            &pending_conflict_realizes,
            &link_merge_entries,
        )
        .await?;
        return Ok(signature);
    }

    finalize_main_merge(
        repository,
        token,
        branch,
        options,
        state_current,
        current_branch,
        &merged_links,
        &link_merge_entries,
        dry_run,
    )
    .await
}

/// Finalize the parent staged state when at least one linked merge produced
/// file conflicts. Stages all merged link pins onto the parent, realizes
/// conflict markers + sidecars on disk, persists the merge metadata, and
/// returns the staged signature.
async fn finalize_link_conflict_state(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_current: &Arc<State>,
    merged_links: &[MergedLink],
    pending_conflict_realizes: &[PendingConflictRealize],
    link_merge_entries: &[LinkMergeEntry],
) -> Result<Hash, MergeError> {
    // `state_current.clone()` clones the `Arc`, not the `State` interior;
    // `conflict_state` and `state_current` alias the same mutable cell.
    // The caller does not read `state_current` again on the conflict path.
    // If a future caller adds such a read, deserialize a fresh `State` here.
    let conflict_state = state_current.clone();
    conflict_state.set_parent_self(state_current.revision());

    for merged in merged_links {
        link::stage_link_pin(
            repository.clone(),
            &conflict_state,
            &merged.link_context,
            merged.link_path_rel.clone(),
            merged.link_node,
            merged.link_reference.signature,
            merged.new_signature,
            merged.link_reference.branch,
        )
        .await
        .forward_with::<MergeError, _>(|| {
            format!("staging conflict link pin for {}", merged.link_path)
        })?;
    }

    // Now that each conflicted link's new state has been realized at the
    // mount path by `stage_link_pin`, write conflict markers (and
    // `.mine`/`.theirs`/`.base` sidecars) on top. Remap each conflict's
    // `change.path` to the link's mount prefix; pass the link's
    // `RepositoryContext` — its `path` is shared with the parent (set by
    // `to_link_context`), so absolute paths resolve to
    // `<parent>/<mount>/<file>` while state block lookups stay in the link.
    for pending in pending_conflict_realizes {
        let mount_path = RelativePath::from_str(&pending.link_path)
            .internal_with(|| format!("link not found: {}", pending.link_path))?;
        let remapped_conflicts: Vec<(NodeChange, NodeChange)> = pending
            .context
            .conflicts
            .iter()
            .map(|(from, to)| {
                let mut from_remapped = from.clone();
                let mut to_remapped = to.clone();
                from_remapped.path = mount_path.join(from.path.as_str());
                to_remapped.path = mount_path.join(to.path.as_str());
                if let Some(ref fp) = from.from_path {
                    from_remapped.from_path = Some(mount_path.join(fp.as_str()));
                }
                if let Some(ref fp) = to.from_path {
                    to_remapped.from_path = Some(mount_path.join(fp.as_str()));
                }
                (from_remapped, to_remapped)
            })
            .collect();
        let conflict_stats = Arc::new(sync::SyncRealizeStats::default());
        sync::realize_conflicts(
            pending.link_context.clone(),
            pending.context.state_base.clone(),
            pending.context.state_from.clone(),
            pending.context.state_to.clone(),
            None, // staging already happened inside apply_diff for the link state
            Arc::new(remapped_conflicts),
            false,
            conflict_stats,
            MergeType::BranchMerge,
        )
        .await
        .forward::<MergeError>("realizing link conflicts")?;

        // Emit per-file conflict events with the mount-prefixed path so
        // consumers see the same shape as for parent-level conflicts.
        for (from, _to) in pending.context.conflicts.iter() {
            let mount_relative = mount_path.join(from.path.as_str());
            event::LoreEvent::BranchMergeConflictFile(LoreBranchMergeConflictFileEventData {
                path: LoreString::from(mount_relative.as_str()),
            })
            .send();
        }
    }

    if !link_merge_entries.is_empty() {
        conflict_state
            .serialize_link_merge_state(repository.clone(), link_merge_entries)
            .await
            .forward::<MergeError>("serializing link merge state")?;
    }
    // Set both Merge and Conflict flags so `merge abort` / `merge resolve`
    // recognise this as an in-progress merge via
    // `is_merge_or_cherry_pick_or_revert()`.
    conflict_state.set_merge_conflict();
    let signature = conflict_state
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing conflict state")?;
    crate::instance::store_staged_anchor(repository, signature)
        .await
        .forward::<MergeError>("storing conflict anchor")?;
    Ok(signature)
}

/// Finalize the parent merge: run main repo merge, apply collected link pin
/// updates to its staged state, persist, and auto-commit if no conflicts.
#[allow(clippy::too_many_arguments)]
async fn finalize_main_merge(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    options: MergeStartOptions,
    state_current: Arc<State>,
    current_branch: BranchId,
    merged_links: &[MergedLink],
    link_merge_entries: &[LinkMergeEntry],
    dry_run: bool,
) -> Result<Hash, MergeError> {
    let MergeRepositoryResult {
        signature: _,
        has_conflicts,
        state_staged,
        ..
    } = merge_repository(
        repository.clone(),
        token,
        branch,
        current_branch,
        state_current.revision(),
        state_current,
    )
    .await?;

    if !dry_run {
        for merged in merged_links {
            link::stage_link_pin(
                repository.clone(),
                &state_staged,
                &merged.link_context,
                merged.link_path_rel.clone(),
                merged.link_node,
                merged.link_reference.signature,
                merged.new_signature,
                merged.link_reference.branch,
            )
            .await
            .forward_with::<MergeError, _>(|| {
                format!("staging merged link pin for {}", merged.link_path)
            })?;
        }

        if !link_merge_entries.is_empty() {
            state_staged
                .serialize_link_merge_state(repository.clone(), link_merge_entries)
                .await
                .forward::<MergeError>("serializing link merge state")?;
        }

        let signature = state_staged
            .serialize(repository.clone(), token)
            .await
            .forward::<MergeError>("serializing staged state")?;
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<MergeError>("storing staged anchor")?;
    }

    if !has_conflicts && !dry_run && !options.no_commit {
        return auto_commit_merge(repository, token, options.message).await;
    }

    Ok(state_staged.revision())
}

/// Auto-commit a merge with no conflicts.
async fn auto_commit_merge(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    message: String,
) -> Result<Hash, MergeError> {
    let commit_options = CommitOptions {
        message,
        link_messages: std::collections::HashMap::new(),
        link: None,
        layer_messages: std::collections::HashMap::new(),
        layer: None,
        stats: false,
    };
    Box::pin(commit::commit(repository, token, commit_options))
        .await
        .forward::<MergeError>("auto-committing merge")
}

pub struct ApplyDiffResults {
    pub signature: Hash,
    pub conflicts: Arc<Vec<(NodeChange, NodeChange)>>,
    pub state_staged: Arc<State>,
    pub stats: Arc<sync::SyncRealizeStats>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MergeType {
    None, // Not a merge in progress
    BranchMerge,
    CherryPick,
    Revert,
}

pub async fn apply_diff(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    mut diff: DiffResult,
    state_current: Arc<State>,
    merge_type: MergeType,
    reverse_parents: bool,
    target_branch: BranchId,
) -> Result<ApplyDiffResults, MergeError> {
    lore_debug!(
        "Branch diff found {} changes and {} conflicts",
        diff.changes.len(),
        diff.conflicts.len()
    );

    change::sort_by_path(&mut diff.changes);
    change::sort_conflict_by_path(&mut diff.conflicts);

    // When applying a parent-level diff during a merge, drop changes that
    // live inside a linked sub-tree. `state::diff` recurses into links when
    // pin hashes differ and emits each file inside the link as a change. If
    // we kept them here, `verify_filesystem` would fail on link files that
    // diverged on disk (e.g. mid-conflict on `merge start --ignore-links`),
    // and `stage_single_node` in realize would attach file nodes under the
    // parent's link node, overwriting `link_node.child`. The link's own tree
    // is merged separately via `merge_repository` on the link context, and
    // `stage_link_pin` handles pin + filesystem updates in the parent. So
    // sub-link changes here are redundant — filter them out before verify.
    if !repository.is_link() {
        diff.changes.retain(|c| c.to.repository.id == repository.id);
        // Drop cross-link conflicts on non-link parents too. For
        // `MergeScope::MainOnly` (`--ignore-links`) we never visit the link
        // contexts, so realizing those conflicts at the parent mount path
        // would write `.mine`/`.theirs`/`.base` sidecars at the wrong absolute
        // path and the in-link state would never gain the merge metadata.
        // For `MergeScope::All` the parent merge runs only AFTER links are
        // merged in place — the link pins have been updated to the merged
        // signature, so `state::diff` no longer surfaces inner-link conflicts
        // at this layer. Either way the parent diff path is the wrong place
        // to realize these.
        diff.conflicts.retain(|(from, to)| {
            from.to.repository.id == repository.id && to.to.repository.id == repository.id
        });
    }

    let stats = Arc::new(sync::SyncVerifyStats::default());
    let mut changes = vec![];

    let state_from = state::State::deserialize(repository.clone(), diff.source)
        .await
        .forward::<MergeError>("deserializing diff source state")?;
    let state_to = state::State::deserialize(repository.clone(), diff.target)
        .await
        .forward::<MergeError>("deserializing diff target state")?;
    let state_base = state::State::deserialize(repository.clone(), diff.base)
        .await
        .forward::<MergeError>("deserializing diff base state")?;

    // Queue up to a given number of parallel tasks to verify filesystem
    const MAX_TASK_COUNT: usize = 1000;
    let mut tasks = JoinSet::new();
    let mut failure = None;
    for change in diff.changes.iter() {
        lore_spawn!(tasks, {
            let stats = stats.clone();
            let change = change.clone();
            let repository = repository.clone();
            let state_current = state_current.clone();
            async move {
                let no_forward_changes = matches!(merge_type, MergeType::CherryPick);
                let no_force_hash_check = false;
                Box::pin(sync::verify_filesystem(
                    change,
                    repository,
                    state_current,
                    no_forward_changes,
                    no_force_hash_check,
                    stats,
                    FilterMode::Full,
                ))
                .await
                .forward::<MergeError>("verifying filesystem for change")
            }
        });
        while tasks.len() > MAX_TASK_COUNT
            && let Some(result) = tasks.join_next().await
        {
            let result = result
                .map_err(|e| MergeError::internal_with_context(e, "task failure"))
                .and_then(|r| r);
            match result {
                Ok(Some(change)) => {
                    changes.push(change);
                }
                _ => {
                    failure = failure.or(result.err());
                }
            }
        }
    }
    // Wait for the remaining tasks
    while let Some(result) = tasks.join_next().await {
        let result = result
            .map_err(|e| MergeError::internal_with_context(e, "task failure"))
            .and_then(|r| r);
        match result {
            Ok(Some(change)) => {
                changes.push(change);
            }
            _ => {
                failure = failure.or(result.err());
            }
        }
    }
    if let Some(err) = failure {
        return Err(err);
    }

    for conflict in diff.conflicts.iter() {
        let (_, change_to) = &conflict;
        lore_spawn!(tasks, {
            let change_to = change_to.clone();
            let stats = stats.clone();
            let repository = repository.clone();
            let state_current = state_current.clone();
            async move {
                let no_forward_changes = matches!(merge_type, MergeType::CherryPick);
                let no_force_hash_check = false;
                Box::pin(sync::verify_filesystem(
                    change_to,
                    repository,
                    state_current,
                    no_forward_changes,
                    no_force_hash_check,
                    stats,
                    FilterMode::Full,
                ))
                .await
                .forward::<MergeError>("verifying filesystem for conflict")
            }
        });
        while tasks.len() > MAX_TASK_COUNT
            && let Some(result) = tasks.join_next().await
        {
            let result = result
                .map_err(|e| MergeError::internal_with_context(e, "task failure"))
                .and_then(|r| r);
            failure = failure.or(result.err());
        }
    }
    // Wait for the remaining tasks
    while let Some(result) = tasks.join_next().await {
        let result = result
            .map_err(|e| MergeError::internal_with_context(e, "task failure"))
            .and_then(|r| r);
        failure = failure.or(result.err());
    }
    if let Some(err) = failure {
        return Err(err);
    }

    lore_debug!("File system verification complete");

    // Prepare the merged staged state
    let state_staged = state::State::deserialize(repository.clone(), diff.target)
        .await
        .forward::<MergeError>("deserializing target state for staging")?;

    // When applying a diff to a linked repository context, skip filesystem
    // realization. The link's repository context shares `path` with the parent,
    // and the diff paths are link-root-relative ("feature-only.txt") rather
    // than parent-relative ("linked/repo/feature-only.txt"). Realizing those
    // paths against the parent path would leak link files into the parent
    // working tree. The actual on-disk realization for the link happens in
    // `stage_link_pin` -> `realize_link_pin_change`, which remaps paths to the
    // mount point before writing.
    let skip_filesystem = repository.is_link();

    // Cross-link changes were filtered out earlier (right after sort) for
    // non-link contexts. For a link context the set is unfiltered and the
    // diff paths are link-root-relative.
    let stats = Arc::new(sync::SyncRealizeStats::default());
    let changes = Arc::new(diff.changes);
    let dry_run = execution_context().globals().dry_run();
    sync::realize_changes(
        repository.clone(),
        changes.clone(),
        Some(state_staged.clone()),
        dry_run || skip_filesystem,
        true, /* is merge */
        stats.clone(),
    )
    .await
    .forward::<MergeError>("realizing non-conflict changes")?;
    lore_debug!("Realized non-conflict changes");

    let conflicts = Arc::new(diff.conflicts);
    sync::realize_conflicts(
        repository.clone(),
        state_base.clone(),
        state_from.clone(),
        state_to.clone(),
        Some(state_staged.clone()),
        conflicts.clone(),
        dry_run || skip_filesystem,
        stats.clone(),
        merge_type,
    )
    .await
    .forward::<MergeError>("realizing conflict changes")?;
    lore_debug!("Realized conflict changes");

    event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(&stats)).send();

    // Serialize as staged state with merged parents, staged states have revision 0
    state_staged.set_revision_number(0);

    {
        let (parent_self, parent_other) = if reverse_parents {
            (state_from.revision(), state_to.revision())
        } else {
            (state_to.revision(), state_from.revision())
        };
        match merge_type {
            MergeType::None => Err(MergeError::internal("Invalid merge type"))?,
            MergeType::BranchMerge => {
                state_staged.set_parent_self(parent_self);
                state_staged.set_parent_other(parent_other);

                // Set file/revision metadata
                merge_metadata(
                    repository.clone(),
                    Arc::new(changes.to_vec()),
                    state_from.clone(),
                    state_staged.clone(),
                )
                .await?;

                // Without overriding `branch`, the metadata inherits the
                // source branch from `merge_revision_metadata`. Push gates
                // `branch_push` on `state.branch() == target_branch` and
                // would fail.
                let metadata_hash = state_staged.metadata_hash();
                let mut metadata = if metadata_hash.is_zero() {
                    Metadata::new()
                } else {
                    Metadata::deserialize(repository.clone(), metadata_hash)
                        .await
                        .forward::<MergeError>("deserializing metadata")?
                };
                metadata
                    .set_branch(target_branch)
                    .forward::<MergeError>("setting metadata branch")?;
                let new_metadata_hash = metadata
                    .serialize(repository.clone())
                    .await
                    .forward::<MergeError>("serializing metadata")?;
                state_staged.set_metadata_hash(new_metadata_hash);
            }
            MergeType::CherryPick => {
                state_staged.set_parent_self(parent_self);
                state_staged.set_parent_other(Hash::default());

                merge_metadata(
                    repository.clone(),
                    Arc::new(changes.to_vec()),
                    state_from.clone(),
                    state_staged.clone(),
                )
                .await?;

                // Set CHERRY_PICKED_FROM metadata
                let metadata_hash = state_staged.metadata_hash();
                let mut metadata = if metadata_hash.is_zero() {
                    Metadata::new()
                } else {
                    Metadata::deserialize(repository.clone(), metadata_hash)
                        .await
                        .forward::<MergeError>("deserializing metadata")?
                };

                metadata
                    .set_hash(metadata::CHERRY_PICKED_FROM, parent_other)
                    .forward::<MergeError>("setting cherry-picked-from metadata")?;

                let new_metadata_hash = metadata
                    .serialize(repository.clone())
                    .await
                    .forward::<MergeError>("serializing metadata")?;

                state_staged.set_metadata_hash(new_metadata_hash);
                state_staged.set_cherry_pick();
            }
            MergeType::Revert => {
                state_staged.set_parent_self(parent_self);
                state_staged.set_parent_other(Hash::default());

                merge_metadata(
                    repository.clone(),
                    Arc::new(changes.to_vec()),
                    state_from.clone(),
                    state_staged.clone(),
                )
                .await?;

                // Set REVERTED_FROM metadata to the revision being reverted (state_base)
                let metadata_hash = state_staged.metadata_hash();
                let mut metadata = if metadata_hash.is_zero() {
                    Metadata::new()
                } else {
                    Metadata::deserialize(repository.clone(), metadata_hash)
                        .await
                        .forward::<MergeError>("deserializing metadata")?
                };

                metadata
                    .set_hash(metadata::REVERTED_FROM, state_base.revision())
                    .forward::<MergeError>("setting reverted-from metadata")?;

                let new_metadata_hash = metadata
                    .serialize(repository.clone())
                    .await
                    .forward::<MergeError>("serializing metadata")?;

                state_staged.set_metadata_hash(new_metadata_hash);
                state_staged.set_revert();
            }
        }
    }

    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing staged state")?;

    if !dry_run {
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<MergeError>("storing staged anchor")?;
    }

    Ok(ApplyDiffResults {
        signature,
        conflicts,
        state_staged,
        stats,
    })
}

pub fn get_merge_type(state: &State) -> MergeType {
    if state.is_cherry_pick() {
        MergeType::CherryPick
    } else if state.is_revert() {
        MergeType::Revert
    } else if state.is_merge() || !state.link_merge_hash().is_zero() {
        MergeType::BranchMerge
    } else {
        MergeType::None
    }
}

// Validates that the current merge type matches the specified merge type.
// If `state` is `None`, the staged state will be retrieved from the repository.
pub async fn validate_merge_type(
    repository: Arc<RepositoryContext>,
    state: impl Into<Option<Arc<State>>>,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let state = state.into();
    let state = if let Some(s) = state {
        s
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<MergeError>("loading current anchor")?;

        let staged_revision = crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
            .unwrap_or(current_revision);

        State::deserialize(repository.clone(), staged_revision)
            .await
            .forward::<MergeError>("deserializing staged state")?
    };

    let current_merge_type = get_merge_type(&state);
    if current_merge_type == merge_type {
        Ok(())
    } else if matches!(current_merge_type, MergeType::None) {
        Err(MergeError::internal("No merge is in progress"))
    } else {
        Err(MergeError::internal("Invalid merge type"))
    }
}

/// Validates that no merge or cherry-pick is already in progress.
/// Returns an error if one is in progress.
/// Refuse the merge if `state_staged` holds actually-staged nodes (same
/// guarantee as before dirty tracking existed). Otherwise snapshot any
/// dirty-only paths into the `merge_carry` blob so they can be re-applied
/// against the eventual merge commit. The merge proceeds with a clean
/// staged state — the carry restores tracking once `commit` finishes.
///
/// `source_revision` is the revision the merge / cherry-pick / revert
/// will record as `parent_other`. `take_matching` does an
/// order-insensitive comparison so any equivalent revision pair matches
/// at commit time.
pub(crate) async fn check_and_capture_dirty_for_merge(
    repository: Arc<RepositoryContext>,
    state_staged_opt: Option<&Arc<State>>,
    source_revision: Hash,
    current_revision: Hash,
) -> Result<(), MergeError> {
    let Some(state_staged) = state_staged_opt else {
        return Ok(());
    };

    let has_staged = state_staged
        .node_has_staged_children(repository.clone(), crate::node::ROOT_NODE)
        .await
        .forward::<MergeError>("checking staged nodes")?;
    if has_staged {
        return Err(MergeError::internal("Cannot merge with staged state"));
    }

    let mut paths: Vec<RelativePath> = Vec::new();
    crate::state::collect_dirty_paths(
        state_staged.clone(),
        repository.clone(),
        crate::node::ROOT_NODE,
        RelativePath::new(),
        &mut paths,
    )
    .await
    .forward::<MergeError>("collecting dirty paths from staged state")?;

    if paths.is_empty() {
        return Ok(());
    }

    crate::merge_carry::store(repository, current_revision, source_revision, &paths)
        .await
        .forward::<MergeError>("storing merge dirty-tracking carry")?;
    Ok(())
}

pub async fn validate_merge_in_progress(
    repository: Arc<RepositoryContext>,
) -> Result<(), MergeError> {
    if let Ok(staged_revision) = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .ok_or("no staged revision")
        && !staged_revision.is_zero()
        && let Ok(state_staged) = State::deserialize(repository.clone(), staged_revision).await
        && state_staged.is_merge_or_cherry_pick_or_revert()
    {
        return Err(MergeError::internal("A merge is already in progress"));
    }
    Ok(())
}

/// Emits conflict file events for conflicts that were not auto-resolved.
pub async fn emit_conflict_events(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    conflicts: &[(change::NodeChange, change::NodeChange)],
    merge_type: MergeType,
) {
    for conflict in conflicts.iter() {
        if let Ok(node) = state_staged
            .find_node(repository.clone(), conflict.0.path.as_str())
            .await
            && (!node.is_staged_merge_conflict() || node.is_staged_merge_resolved())
        {
            // The file was auto resolved
            continue;
        }

        match merge_type {
            MergeType::CherryPick => {
                event::LoreEvent::CherryPickConflictFile(
                    crate::revision::cherry_pick::LoreCherryPickConflictFileEventData {
                        path: conflict.0.path.clone().into(),
                    },
                )
                .send();
            }
            MergeType::BranchMerge => {
                event::LoreEvent::BranchMergeConflictFile(LoreBranchMergeConflictFileEventData {
                    path: conflict.0.path.clone().into(),
                })
                .send();
            }
            MergeType::Revert => {
                event::LoreEvent::RevertConflictFile(LoreRevertConflictFileEventData {
                    path: conflict.0.path.clone().into(),
                })
                .send();
            }
            MergeType::None => {}
        }
    }
}

/// Per-entry view of a link merge ready to be rolled back or replayed.
struct ResolvedAbortEntry {
    link_path: String,
    link_path_rel: RelativePath,
    link_node: Node,
    link_context: Arc<RepositoryContext>,
    link_reference: state::LinkReference,
}

/// Resolve every `LinkMergeEntry` in `state_staged` to the data both abort
/// variants need: link path, mount `RelativePath`, link node, link context,
/// and the (current/merged) link reference. Used by `branch_merge_abort` and
/// `merge_abort_ignore_links` so the per-entry resolution lives in one place.
async fn resolve_abort_entries(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    entries: &[LinkMergeEntry],
) -> Result<Vec<ResolvedAbortEntry>, MergeError> {
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let link_path = state_staged
            .node_path(repository.clone(), entry.local_node)
            .await
            .forward::<MergeError>("resolving link node path")?;
        let link_path_rel = RelativePath::from_str(&link_path)
            .internal_with(|| format!("link abort: {link_path}"))?;
        let link::ResolvedLink {
            link_node,
            link_context,
            link_reference,
        } = link::resolve_link_at_path(state_staged, repository.clone(), &link_path)
            .await
            .forward_with::<MergeError, _>(|| format!("link not found: {link_path}"))?;
        resolved.push(ResolvedAbortEntry {
            link_path,
            link_path_rel,
            link_node,
            link_context,
            link_reference,
        });
    }
    Ok(resolved)
}

pub async fn branch_merge_abort(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link: Option<String>,
    ignore_links: bool,
) -> Result<(), MergeError> {
    // `--link` and `--ignore-links` are mutually exclusive at the CLI layer.
    // If both arrive together (e.g. via a non-CLI caller), `ignore_links`
    // wins because it's the more conservative scope (parent only) and matches
    // how `merge_start` resolves the same combo.
    if ignore_links {
        return merge_abort_ignore_links(repository, token).await;
    }

    if let Some(link_path) = link {
        return merge_abort_link(repository, token, link_path).await;
    }

    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten();

    if let Some(staged_rev) = staged_revision {
        let state_staged = State::deserialize(repository.clone(), staged_rev)
            .await
            .forward::<MergeError>("deserializing staged state")?;

        let entries = state_staged
            .deserialize_link_merge_state(repository.clone())
            .await
            .forward::<MergeError>("deserializing link merge state")?;
        let resolved = resolve_abort_entries(&repository, &state_staged, &entries).await?;

        // Walk entries in reverse to undo link merges. After each entry is
        // restored, write the shrunken `LinkMergeState` to the parent staged
        // state and re-anchor — so a mid-walk failure leaves a consistent
        // on-disk state that re-running `merge abort` can pick up from.
        // Without incremental clears, a retry would call
        // `stage_link_pin(merged → base)` for already-restored entries (a
        // no-op since the pin is already at base) but the entries would hang
        // around in `LinkMergeState` forever.
        for (i, (entry, r)) in entries.iter().zip(resolved.iter()).rev().enumerate() {
            // Marker bytes and `.mine`/`.theirs`/`.base` sidecars are
            // filesystem-only artifacts that the state-to-state diff inside
            // `stage_link_pin` won't touch; collect their paths before the
            // pin rollback so we can clean them explicitly afterwards.
            let conflict_link_state =
                state::State::deserialize(r.link_context.clone(), r.link_reference.signature)
                    .await
                    .forward::<MergeError>("deserializing conflict link state")?;
            let conflict_paths = collect_merge_conflict_files(
                r.link_context.clone(),
                conflict_link_state,
                r.link_node.child,
                RelativePath::new(),
            )
            .await?;

            link::stage_link_pin(
                repository.clone(),
                &state_staged,
                &r.link_context,
                r.link_path_rel.clone(),
                r.link_node,
                r.link_reference.signature,
                entry.base.signature,
                entry.base.branch,
            )
            .await
            .forward_with::<MergeError, _>(|| format!("staging link pin for {}", r.link_path))?;

            if !conflict_paths.is_empty() {
                // Restore content from the pre-merge link state and remove the
                // sidecars (the state-to-state diff above missed them).
                let base_link_state =
                    state::State::deserialize(r.link_context.clone(), entry.base.signature)
                        .await
                        .forward::<MergeError>("deserializing base link state")?;
                link::restore_link_paths_from_state(
                    repository.clone(),
                    r.link_context.clone(),
                    r.link_path_rel.clone(),
                    base_link_state,
                    &conflict_paths,
                )
                .await
                .forward_with::<MergeError, _>(|| {
                    format!("restoring link paths for {}", r.link_path)
                })?;
            }

            // Shrink the persisted `LinkMergeState` to entries we have not
            // restored yet (the head of the slice; we walk reverse, so
            // restored entries are at the tail).
            let remaining = &entries[..entries.len() - 1 - i];
            if remaining.is_empty() {
                state_staged.clear_link_merge_state();
            } else {
                state_staged
                    .serialize_link_merge_state(repository.clone(), remaining)
                    .await
                    .forward::<MergeError>("serializing link merge state")?;
            }
            let signature = state_staged
                .serialize(repository.clone(), token)
                .await
                .forward::<MergeError>("serializing staged state")?;
            crate::instance::store_staged_anchor(&repository, signature)
                .await
                .forward::<MergeError>("storing staged anchor")?;
        }

        // The reverse walk above persists the shrunken LinkMergeState after
        // each entry it restores; by the time we get here the parent staged
        // state is `(LinkMergeState empty, link pins reverted)`. The
        // subsequent `merge_abort` reads that state and runs the parent diff
        // against it. If `merge_abort` fails, the parent still reports an
        // in-progress merge (Merge|Conflict flags weren't cleared), but the
        // entries are gone — re-running `merge abort` skips the walk (entries
        // empty) and retries `merge_abort` idempotently.
    }

    merge_abort(repository, MergeType::BranchMerge).await
}

/// Abort a single linked repository merge by restoring the link pin to its pre-merge state.
async fn merge_abort_link(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: String,
) -> Result<(), MergeError> {
    let link_path_rel =
        RelativePath::from_str(&link_path).internal_with(|| format!("link abort: {link_path}"))?;

    let (_state_current, state_staged_opt, _current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    let state_staged =
        state_staged_opt.ok_or_else(|| MergeError::internal("No merge is in progress"))?;

    // Read LinkMergeState — this is what identifies a link merge in progress
    let entries = state_staged
        .deserialize_link_merge_state(repository.clone())
        .await
        .forward::<MergeError>("deserializing link merge state")?;

    if entries.is_empty() {
        return Err(MergeError::internal("No merge is in progress"));
    }

    // Resolve the link at the path using the existing utility
    let link::ResolvedLink {
        link_node,
        link_context,
        link_reference,
    } = link::resolve_link_at_path(&state_staged, repository.clone(), &link_path)
        .await
        .forward_with::<MergeError, _>(|| format!("link not found: {link_path}"))?;

    // Find matching entry in LinkMergeState
    let entry = entries
        .iter()
        .find(|e| e.local_node == link_reference.local_node)
        .ok_or_else(|| MergeError::internal(format!("link not found: {link_path}")))?;

    // Restore on-disk content, stage restored node, and update link registry back to base
    link::stage_link_pin(
        repository.clone(),
        &state_staged,
        &link_context,
        link_path_rel,
        link_node,
        link_reference.signature,
        entry.base.signature,
        entry.base.branch,
    )
    .await
    .forward_with::<MergeError, _>(|| format!("staging link pin for {link_path}"))?;

    // Clear link merge state
    state_staged.clear_link_merge_state();

    // Re-serialize the staged state and flush
    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing staged state")?;

    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<MergeError>("storing staged anchor")?;

    Ok(())
}

/// Selective main abort: reverts main repo merge but preserves link pin updates.
///
/// Saves the merged link state, performs a full abort (reverting everything),
/// then re-applies only the link pin updates as normal staged changes.
async fn merge_abort_ignore_links(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
) -> Result<(), MergeError> {
    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| MergeError::internal("No merge is in progress"))?;

    let state_staged = State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<MergeError>("deserializing staged state")?;

    if !state_staged.is_merge_or_cherry_pick_or_revert() {
        return Err(MergeError::internal("No merge is in progress"));
    }

    let entries = state_staged
        .deserialize_link_merge_state(repository.clone())
        .await
        .forward::<MergeError>("deserializing link merge state")?;
    let resolved = resolve_abort_entries(&repository, &state_staged, &entries).await?;

    // Full abort first, then replay just the link pin updates as normal
    // staged changes (no merge flags).
    merge_abort(repository.clone(), MergeType::BranchMerge).await?;

    if !resolved.is_empty() {
        let (state_current, _, _) = State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;

        state_current.set_parent_self(state_current.revision());

        for r in &resolved {
            // Re-read the link reference from the post-abort state to find
            // the now-current pin (which is back to pre-merge after abort).
            let link::ResolvedLink {
                link_reference: current_ref,
                ..
            } = link::resolve_link_at_path(&state_current, repository.clone(), &r.link_path)
                .await
                .forward_with::<MergeError, _>(|| format!("link not found: {}", r.link_path))?;

            link::stage_link_pin(
                repository.clone(),
                &state_current,
                &r.link_context,
                r.link_path_rel.clone(),
                r.link_node,
                current_ref.signature,
                r.link_reference.signature,
                r.link_reference.branch,
            )
            .await
            .forward_with::<MergeError, _>(|| format!("restoring link pin for {}", r.link_path))?;
        }

        let signature = state_current
            .serialize(repository.clone(), token)
            .await
            .forward::<MergeError>("serializing staged state")?;

        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<MergeError>("storing staged anchor")?;
    }

    Ok(())
}

pub async fn merge_abort(
    repository: Arc<RepositoryContext>,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<MergeError>("loading current anchor")?;

    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state_staged = State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<MergeError>("deserializing staged state")?;

    validate_merge_type(repository.clone(), state_staged.clone(), merge_type).await?;

    let state_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward::<MergeError>("deserializing current state")?;

    match merge_type {
        MergeType::CherryPick => {
            event::LoreEvent::CherryPickAbortBegin(LoreCherryPickAbortBeginEventData {
                state_staged_revision: state_staged.revision(),
                state_current_revision: state_current.revision(),
            })
            .send();
        }
        MergeType::BranchMerge => {
            event::LoreEvent::BranchMergeAbortBegin(LoreBranchMergeAbortBeginEventData {
                state_staged_revision: state_staged.revision(),
                state_current_revision: state_current.revision(),
            })
            .send();
        }
        MergeType::Revert => {
            event::LoreEvent::RevertAbortBegin(LoreRevertAbortBeginEventData {
                state_staged_revision: state_staged.revision(),
                state_current_revision: state_current.revision(),
            })
            .send();
        }
        MergeType::None => {
            return Err(MergeError::internal("No merge is in progress"));
        }
    }

    let mut changes = state::diff_collect(
        repository.clone(),
        state_current.clone(),
        repository.clone(),
        state_staged.clone(),
        None, /* No subpath */
        FilterMode::View,
    )
    .await
    .forward::<MergeError>("running merge abort diff")?;

    // Reverse to go from staged to current
    change::reverse(&mut changes);
    let changes = Arc::new(changes);

    lore_debug!("Merge abort found {} changes to revert", changes.len());

    // Clean up theirs/base files
    for change in changes.iter() {
        sync::unlink_merge_mine_theirs_base(
            change.path.to_absolute_path(repository.require_path()?),
        )
        .await;
    }

    let stats = Arc::new(sync::SyncRealizeStats::default());
    sync::realize_changes(
        repository.clone(),
        changes.clone(),
        None,
        execution_context().globals().dry_run(),
        false, /* Not a merge */
        stats.clone(),
    )
    .await
    .forward::<MergeError>("realizing abort changes")?;

    match merge_type {
        MergeType::CherryPick => {
            event::LoreEvent::CherryPickAbortEnd(LoreCherryPickAbortEndEventData::default()).send();
        }
        MergeType::BranchMerge => {
            event::LoreEvent::BranchMergeAbortEnd(LoreBranchMergeAbortEndEventData::default())
                .send();
        }
        MergeType::Revert => {
            event::LoreEvent::RevertAbortEnd(LoreRevertAbortEndEventData::default()).send();
        }
        MergeType::None => {
            return Err(MergeError::internal("No merge is in progress"));
        }
    }

    let _ = crate::instance::delete_staged_anchor(&repository).await;

    // Aborting cancels the merge, not the user's unrelated dirty edits: restore
    // the pre-existing dirty-only carry's tracking (its on-disk content was left
    // untouched by the abort) before clearing the carry blob.
    if let Ok(Some(carry)) = crate::merge_carry::load(repository.clone()).await
        && !carry.paths.is_empty()
    {
        let _ = crate::file::dirty::dirty_relative_paths(repository.clone(), carry.paths).await;
    }
    let _ = crate::merge_carry::delete(repository.clone()).await;

    Ok(())
}

pub async fn merge_restart(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), MergeError> {
    let (_state_current, state_staged, branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    let state_staged = state_staged.unwrap_or_else(|| _state_current.clone());

    validate_merge_type(
        repository.clone(),
        state_staged.clone(),
        MergeType::BranchMerge,
    )
    .await?;

    let state_merge = State::deserialize(repository.clone(), state_staged.parent_other())
        .await
        .forward::<MergeError>("deserializing merge parent state")?;

    let state_merge_metadata = state_merge
        .revision_metadata(repository.clone())
        .await
        .forward::<MergeError>("loading merge revision metadata")?;

    let diff = Box::pin(branch::diff3_collect(
        repository.clone(),
        state_merge_metadata.branch,
        state_staged.parent_other(),
        branch,
        state_staged.parent_self(),
        None,
        true,  /* Include identical changes for merge tracking */
        false, /* Do not autoresolve, this is done later */
    ))
    .await
    .forward::<MergeError>("running diff3 for merge restart")?;
    apply_restart_diff(
        repository,
        token,
        paths,
        diff,
        state_staged,
        MergeType::BranchMerge,
    )
    .await
}

pub async fn apply_restart_diff(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    diff: DiffResult,
    state_staged: Arc<State>,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    // Filter diff found to passed in paths
    let mut relative_paths = vec![];
    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path: {path}");
            continue;
        };

        relative_paths.push(relative_path);
    }

    let changes: Vec<_> = diff
        .changes
        .iter()
        .filter_map(|change| {
            if relative_paths.contains(&change.path) {
                Some(change.clone())
            } else {
                None
            }
        })
        .collect();
    let conflicts: Vec<_> = diff
        .conflicts
        .iter()
        .filter_map(|change_tuple| {
            if relative_paths.contains(&change_tuple.0.path)
                || relative_paths.contains(&change_tuple.1.path)
            {
                Some(change_tuple.clone())
            } else {
                None
            }
        })
        .collect();

    lore_info!(
        "Branch diff found {} changes and {} conflicts",
        changes.len(),
        conflicts.len()
    );

    let dry_run = execution_context().globals().dry_run();

    if !changes.is_empty() || !conflicts.is_empty() {
        let state_from = state::State::deserialize(repository.clone(), diff.source)
            .await
            .forward::<MergeError>("deserializing diff source state")?;
        let state_to = state::State::deserialize(repository.clone(), diff.target)
            .await
            .forward::<MergeError>("deserializing diff target state")?;
        let state_base = state::State::deserialize(repository.clone(), diff.base)
            .await
            .forward::<MergeError>("deserializing diff base state")?;

        // Reset conflicts to the original version to facilitate 3-way merge later
        if !conflicts.is_empty() {
            let changes = conflicts.iter().map(|tuple| tuple.1.clone()).collect();

            let stats = Arc::new(sync::SyncRealizeStats::default());
            sync::realize_changes(
                repository.clone(),
                Arc::new(changes),
                None,
                dry_run,
                false, /* is merge */
                stats,
            )
            .await
            .forward::<MergeError>("realizing reset changes")?;
        }

        // Perform all changes
        let stats = Arc::new(sync::SyncRealizeStats::default());
        sync::realize_changes(
            repository.clone(),
            Arc::new(changes),
            Some(state_staged.clone()),
            dry_run,
            true, /* is merge */
            stats.clone(),
        )
        .await
        .forward::<MergeError>("realizing non-conflict changes")?;
        lore_debug!("Realized non-conflict changes");

        sync::realize_conflicts(
            repository.clone(),
            state_base.clone(),
            state_from.clone(),
            state_to.clone(),
            Some(state_staged.clone()),
            Arc::new(conflicts),
            dry_run,
            stats.clone(),
            merge_type,
        )
        .await
        .forward::<MergeError>("realizing conflict changes")?;
        lore_debug!("Realized conflict changes");

        if !dry_run {
            let signature = state_staged
                .serialize(repository.clone(), token)
                .await
                .forward::<MergeError>("serializing staged state")?;
            crate::instance::store_staged_anchor(&repository, signature)
                .await
                .forward::<MergeError>("storing staged anchor")?;
        }
    }

    Ok(())
}

pub async fn branch_merge_unresolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), MergeError> {
    merge_unresolve(repository, token, paths, MergeType::BranchMerge).await
}

pub async fn merge_unresolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    let state_staged = state_staged.unwrap_or(state_current);

    validate_merge_type(repository.clone(), state_staged.clone(), merge_type).await?;

    if !state_staged.is_conflict() {
        return Err(MergeError::internal("No conflict to unresolve"));
    }

    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path: {path}");
            continue;
        };
        lore_debug!(
            "User path [{}] transformed to relative path [{}] in repository {}",
            path.as_str(),
            relative_path.as_str(),
            repository.path_for_display()
        );

        // Filter out paths that don't have a staged node - they can't be a merged change.
        let node_link = state_staged
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if !node_link.is_valid() {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path, does not exist in staged state: {path}");
            continue;
        }

        if node_link.repository != repository.id {
            // TODO(vri): UCS-19234 - Links: Implement merge support for cross-link node links
            return Err(MergeError::internal(
                "Conflict resolution inside linked repositories is not yet supported",
            ));
        }

        // Filter out paths that have a staged node but not the merged flag - they are not a merged change.
        let block_index = NodeBlock::index(node_link.node);
        let node_index = Node::index(node_link.node);
        let block = state_staged
            .block(repository.clone(), block_index)
            .await
            .forward::<MergeError>("deserializing block")?;
        let mut node = block.node(node_index);
        if !node.is_staged_merge() {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path, node is not a staged merge: {path}");
            continue;
        }
        if !node.is_staged_merge_conflict() {
            emit_path_ignore(path.as_str()).await;
            lore_debug!("Ignoring invalid path, node is not a staged merge conflict: {path}");
            continue;
        }

        let resolved_bit = NodeFlags::StagedMergeResolved ^ NodeFlags::StagedMergeConflict;

        let was_modified = if node.is_staged_merge_resolved() {
            node.flags &= !resolved_bit;

            true
        } else {
            false
        };

        lore_debug!(
            "Conflicted {}, node flags {:?}",
            relative_path.as_str(),
            node.flags
        );

        let dirtied = {
            let mut block_writer = block.write();
            if was_modified {
                let write_node = block_writer.node(node_index);
                *write_node = node;
            }
            block_writer.node(node_index).flags &= !resolved_bit;
            block_writer.mark_dirty()
        };

        if dirtied {
            state_staged.block_modified(block.clone(), block_index);
            state_staged.mark_dirty();
        }

        let flags = NodeFlags::from_bits_truncate(node.flags);

        state_staged
            .node_mark(repository.clone(), node_link.node, flags, false)
            .await
            .forward::<MergeError>("marking node")?;

        match merge_type {
            MergeType::CherryPick => {
                event::LoreEvent::CherryPickUnresolveFile(LoreCherryPickUnresolveFileEventData {
                    path: relative_path.into(),
                })
                .send();
            }
            MergeType::BranchMerge => {
                event::LoreEvent::BranchMergeUnresolveFile(LoreBranchMergeUnresolveFileEventData {
                    path: relative_path.into(),
                })
                .send();
            }
            MergeType::Revert => {
                event::LoreEvent::RevertUnresolveFile(LoreRevertUnresolveFileEventData {
                    path: relative_path.into(),
                })
                .send();
            }
            MergeType::None => {
                return Err(MergeError::internal("No merge is in progress"));
            }
        }
    }

    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing staged state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<MergeError>("storing staged anchor")?;

    match merge_type {
        MergeType::CherryPick => {
            event::LoreEvent::CherryPickUnresolveRevision(
                LoreCherryPickUnresolveRevisionEventData {
                    repository: repository.id,
                    revision: signature,
                },
            )
            .send();
        }
        MergeType::BranchMerge => {
            event::LoreEvent::BranchMergeUnresolveRevision(
                LoreBranchMergeUnresolveRevisionEventData {
                    repository: repository.id,
                    revision: signature,
                },
            )
            .send();
        }
        MergeType::Revert => {
            event::LoreEvent::RevertUnresolveRevision(LoreRevertUnresolveRevisionEventData {
                repository: repository.id,
                revision: signature,
            })
            .send();
        }
        MergeType::None => {
            return Err(MergeError::internal("No merge is in progress"));
        }
    }

    Ok(())
}

/// Recursively collect all file paths with unresolved merge conflicts under a node.
/// If the node is a file with an unresolved merge conflict, returns it directly.
/// If the node is a directory, walks all descendants and collects matching files.
fn collect_merge_conflict_files(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    base_path: RelativePath,
) -> Pin<Box<dyn Future<Output = Result<Vec<RelativePath>, MergeError>> + Send>> {
    Box::pin(async move {
        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<MergeError>("deserializing block")?;
        let node = block.node(node_index);

        if node.is_link() {
            return Err(MergeError::internal(
                "Conflict resolution inside linked repositories is not yet supported",
            ));
        }

        if node.is_file() {
            if node.is_staged_merge_conflict() && node.is_staged_merge_unresolved() {
                return Ok(vec![base_path]);
            }
            return Ok(vec![]);
        }

        // Directory: recurse into children
        let mut collected = Vec::new();
        let mut children =
            StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
                .await
                .forward::<MergeError>("iterating block children")?;

        while let Some((child_id, _child_node, child_name)) = children
            .next()
            .await
            .forward::<MergeError>("getting next child")?
        {
            let child_path = base_path.push_into_buf(&child_name).freeze();
            // Release the block read lock before recursing (see NodeNameLock docs).
            drop(child_name);
            let mut child_files = collect_merge_conflict_files(
                repository.clone(),
                state.clone(),
                child_id,
                child_path,
            )
            .await?;
            collected.append(&mut child_files);
        }

        Ok(collected)
    })
}

pub async fn branch_merge_resolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), MergeError> {
    merge_resolve(repository, token, paths, MergeType::BranchMerge).await
}

async fn resolve_single_file(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    relative_path: RelativePath,
    node_link: NodeLink,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let block_index = NodeBlock::index(node_link.node);
    let node_index = Node::index(node_link.node);
    let block = state_staged
        .block(repository.clone(), block_index)
        .await
        .forward::<MergeError>("deserializing block")?;
    let mut node = block.node(node_index);
    if !node.is_staged_merge() {
        lore_warn!(
            "Ignoring invalid path, node is not a staged merge: {}",
            relative_path.as_str()
        );
        return Ok(());
    }
    if !node.is_staged_merge_conflict() {
        lore_warn!(
            "Ignoring invalid path, node is not a staged merge conflict: {}",
            relative_path.as_str()
        );
        return Ok(());
    }

    // Check if file still has conflict markers on disk
    let absolute_path = relative_path.to_absolute_path(repository.require_path()?);
    if infer::infer_is_conflicted_by_path(absolute_path.as_path())
        .await
        .unwrap_or(false)
    {
        lore_warn!(
            "Cannot resolve path with conflict markers still present: {}",
            relative_path.as_str()
        );
        return Ok(());
    }

    let was_modified = if node.is_staged_merge_unresolved() {
        node.flags |= NodeFlags::StagedMergeResolved;

        true
    } else {
        false
    };

    lore_debug!(
        "Resolved {}, node flags {:?}",
        relative_path.as_str(),
        node.flags
    );

    let dirtied = {
        let mut block_writer = block.write();
        if was_modified {
            let write_node = block_writer.node(node_index);
            *write_node = node;
        }
        block_writer.node(node_index).flags |= NodeFlags::StagedMergeResolved;
        block_writer.mark_dirty()
    };

    if dirtied {
        state_staged.block_modified(block.clone(), block_index);
        state_staged.mark_dirty();
    }

    let flags = NodeFlags::from_bits_truncate(node.flags);

    state_staged
        .node_mark(repository.clone(), node_link.node, flags, false)
        .await
        .forward::<MergeError>("marking node")?;

    match merge_type {
        MergeType::CherryPick => {
            event::LoreEvent::CherryPickResolveFile(LoreCherryPickResolveFileEventData {
                path: relative_path.into(),
            })
            .send();
        }
        MergeType::BranchMerge => {
            event::LoreEvent::BranchMergeResolveFile(LoreBranchMergeResolveFileEventData {
                path: relative_path.into(),
            })
            .send();
        }
        MergeType::Revert => {
            event::LoreEvent::RevertResolveFile(LoreRevertResolveFileEventData {
                path: relative_path.into(),
            })
            .send();
        }
        MergeType::None => {
            return Err(MergeError::internal("No merge is in progress"));
        }
    }

    Ok(())
}

/// Resolve a single user path against the given (`context`, `state`).
///
/// `node_link` is the entry-node lookup result for `user_relative` against the
/// parent's staged state. For the parent branch `state` IS the parent staged
/// state and `mount` is empty; for the link branch `state` is the link's own
/// state and `mount` is the link's mount path. Directory expansion walks
/// `state`, then strips `mount` to derive the state-relative path before
/// calling `resolve_single_file`.
async fn resolve_path_in_state(
    context: &Arc<RepositoryContext>,
    state: &Arc<State>,
    mount: &str,
    user_relative: RelativePath,
    node_link: node::NodeLink,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let block = state
        .block(context.clone(), NodeBlock::index(node_link.node))
        .await
        .forward::<MergeError>("deserializing block")?;
    let node = block.node(Node::index(node_link.node));

    if node.is_file() {
        resolve_single_file(context, state, user_relative, node_link, merge_type).await?;
        return Ok(());
    }
    if !node.is_directory() {
        return Err(MergeError::internal(
            "Conflict resolution inside linked repositories is not yet supported",
        ));
    }

    // Expanded paths are user-side (rooted at `user_relative`); inside a
    // link they need the mount prefix stripped before they can be resolved
    // against `state` (which is link-relative).
    let expanded = collect_merge_conflict_files(
        context.clone(),
        state.clone(),
        node_link.node,
        user_relative,
    )
    .await?;
    for file_path in expanded {
        let state_relative = if mount.is_empty() {
            file_path.clone()
        } else {
            file_path
                .as_str()
                .strip_prefix(mount)
                .and_then(|s| s.strip_prefix('/'))
                .and_then(|s| RelativePath::from_str(s).ok())
                .unwrap_or_else(|| file_path.clone())
        };
        let file_node_link = state
            .find_node_link(context.clone(), state_relative.as_str())
            .await
            .unwrap_or_default();
        if !file_node_link.is_valid() {
            continue;
        }
        resolve_single_file(context, state, file_path, file_node_link, merge_type).await?;
    }
    Ok(())
}

pub async fn merge_resolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let (_state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    let state_staged = state_staged.unwrap_or_else(|| _state_current.clone());

    validate_merge_type(repository.clone(), state_staged.clone(), merge_type).await?;

    if !state_staged.is_conflict() {
        return Err(MergeError::internal("No conflict to resolve"));
    }

    let paths = if paths.is_empty() {
        LoreArray::from_vec(vec![LoreString::from(".")])
    } else {
        paths
    };

    // Cache per-link state across the path loop so multiple paths into the
    // same link share one deserialize and one re-serialize.
    struct TouchedLink {
        link_state: Arc<State>,
        link_context: Arc<RepositoryContext>,
        link_branch: BranchId,
        mount_path: String,
    }
    let mut touched_links: std::collections::HashMap<RepositoryId, TouchedLink> =
        std::collections::HashMap::new();
    let mut cached_link_list: Option<Vec<state::LinkReference>> = None;

    for path in paths.as_slice().iter() {
        let Ok(relative_path) =
            RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        else {
            emit_path_ignore(path.as_str()).await;
            lore_warn!("Ignoring invalid path: {path}");
            continue;
        };
        lore_debug!(
            "User path [{}] transformed to relative path [{}] in repository {}",
            path.as_str(),
            relative_path.as_str(),
            repository.path_for_display()
        );

        // Filter out paths that don't have a staged node - they can't be a merged change.
        // Use is_valid_or_root() since directory/root paths are now supported.
        let node_link = state_staged
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
            .unwrap_or_default();
        if !node_link.is_valid_or_root() {
            emit_path_ignore(path.as_str()).await;
            lore_warn!("Ignoring invalid path, does not exist in staged state: {path}");
            continue;
        }

        // Route through the link's state; pin update is deferred to the
        // post-loop block so multiple paths into the same link share one
        // final write.
        if node_link.repository != repository.id {
            let link_list = if let Some(ref ll) = cached_link_list {
                ll
            } else {
                let ll = state_staged
                    .link_list(repository.clone())
                    .await
                    .forward::<MergeError>("listing links")?;
                cached_link_list = Some(ll);
                cached_link_list.as_ref().unwrap()
            };

            let touched = match touched_links.entry(node_link.repository) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let link_context =
                        Arc::new(repository.to_link_context(node_link.repository).await);
                    let link_state =
                        state::State::deserialize(link_context.clone(), node_link.revision)
                            .await
                            .forward::<MergeError>("deserializing link state")?;
                    let link_ref = link_list
                        .iter()
                        .find(|l| l.repository == node_link.repository)
                        .copied();
                    let (mount_path, link_branch) = if let Some(link_ref) = link_ref {
                        let mp = state_staged
                            .node_path(repository.clone(), link_ref.local_node)
                            .await
                            .forward::<MergeError>("resolving link mount path")?;
                        (mp, link_ref.branch)
                    } else {
                        (String::new(), BranchId::default())
                    };
                    e.insert(TouchedLink {
                        link_state,
                        link_context,
                        link_branch,
                        mount_path,
                    })
                }
            };

            resolve_path_in_state(
                &touched.link_context,
                &touched.link_state,
                &touched.mount_path,
                relative_path,
                node_link,
                merge_type,
            )
            .await?;
            continue;
        }

        resolve_path_in_state(
            &repository,
            &state_staged,
            "",
            relative_path,
            node_link,
            merge_type,
        )
        .await?;
    }

    // Re-serialize each touched link once and update the parent's pin; the
    // parent serialize+anchor below picks up all of these in a single write.
    for (_repo_id, touched) in touched_links {
        if touched.mount_path.is_empty() {
            continue;
        }
        let new_link_sig = touched
            .link_state
            .serialize(touched.link_context.clone(), token)
            .await
            .forward::<MergeError>("serializing link state")?;
        link::update_link_pin_by_path(
            &state_staged,
            repository.clone(),
            &touched.mount_path,
            touched.link_branch,
            new_link_sig,
        )
        .await
        .forward::<MergeError>("updating link pin")?;
    }

    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing staged state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<MergeError>("storing staged anchor")?;

    match merge_type {
        MergeType::CherryPick => {
            event::LoreEvent::CherryPickResolveRevision(LoreCherryPickResolveRevisionEventData {
                repository: repository.id,
                revision: signature,
            })
            .send();
        }
        MergeType::BranchMerge => {
            event::LoreEvent::BranchMergeResolveRevision(LoreBranchMergeResolveRevisionEventData {
                repository: repository.id,
                revision: signature,
            })
            .send();
        }
        MergeType::Revert => {
            event::LoreEvent::RevertResolveRevision(LoreRevertResolveRevisionEventData {
                repository: repository.id,
                revision: signature,
            })
            .send();
        }
        MergeType::None => {
            return Err(MergeError::internal("No merge is in progress"));
        }
    }

    Ok(())
}

pub async fn merge_resolve_mine(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), StageError> {
    validate_merge_type(repository.clone(), None, MergeType::BranchMerge)
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

pub async fn merge_resolve_theirs(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), StageError> {
    validate_merge_type(repository.clone(), None, MergeType::BranchMerge)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    Box::pin(stage::stage_from_parent_revision(
        repository,
        token,
        paths,
        stage::MergeParent::Theirs,
    ))
    .await
}

#[derive(Clone, Debug)]
pub struct MergeIntoOptions {
    /// Message to use
    pub message: String,
    /// Optional link path to scope the merge to a single linked repository
    pub link: Option<String>,
    /// Skip link discovery entirely; merge only the main repository.
    pub ignore_links: bool,
}

async fn merge_metadata_task(
    repository: Arc<RepositoryContext>,
    change: NodeChange,
    state_source: Arc<State>,
    state_staged: Arc<State>,
) -> Result<(), MergeError> {
    let metadata_hash;

    if let Ok(node_link) = state_source
        .find_node_link(repository.clone(), change.path.as_str())
        .await
        && node_link.is_valid()
    {
        let metadata_node = node::node_to_file_metadata(node_link.node);
        let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
        let metadata_node_index = NodeFileMetadata::index(metadata_node);

        let metadata_block = state_source
            .block_file_metadata(repository.clone(), metadata_block_index)
            .await
            .forward::<MergeError>("deserializing metadata block")?;

        let block_reader = metadata_block.read();
        let node = block_reader.node(metadata_node_index);

        metadata_hash = node.metadata;
    } else {
        lore_debug!(
            "Merge metadata skipped due to missing 'source' node for {}",
            change.path
        );
        return Ok(());
    }

    if let Ok(node_link) = state_staged
        .find_node_link(repository.clone(), change.path.as_str())
        .await
        && node_link.is_valid()
    {
        let metadata_node = node::node_to_file_metadata(node_link.node);
        let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
        let metadata_node_index = NodeFileMetadata::index(metadata_node);

        let metadata_block = state_staged
            .block_file_metadata(repository.clone(), metadata_block_index)
            .await
            .forward::<MergeError>("deserializing staged metadata block")?;

        let dirtied = {
            let mut block_writer = metadata_block.write();
            let node = block_writer.node(metadata_node_index);

            node.metadata = metadata_hash;

            block_writer.mark_dirty()
        };

        if dirtied {
            state_staged.block_file_metadata_modified(metadata_block, metadata_block_index);
            state_staged.mark_dirty();
        }

        lore_trace!("Merged metadata for {}", change.path);
    } else {
        lore_debug!(
            "Merge metadata skipped due to missing 'staged' node for {}",
            change.path
        );
        return Ok(());
    }

    Ok(())
}

async fn merge_file_metadata(
    repository: Arc<RepositoryContext>,
    changes: Arc<Vec<NodeChange>>,
    state_source: Arc<State>,
    state_staged: Arc<State>,
) -> Result<(), MergeError> {
    let mut tasks = JoinSet::new();
    for change in changes.iter() {
        let repository = repository.clone();
        let state_source = state_source.clone();
        let state_staged = state_staged.clone();
        let change = change.clone();

        lore_spawn!(tasks, {
            async move { merge_metadata_task(repository, change, state_source, state_staged).await }
        });
    }

    let mut failure = None;
    while let Some(result) = tasks.join_next().await {
        failure = failure.or(result
            .map_err(|e| MergeError::internal_with_context(e, "task failure"))
            .and_then(|r| r)
            .err());
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(())
}

fn merge_revision_metadata(
    state_source: Arc<State>,
    state_staged: Arc<State>,
) -> Result<(), MergeError> {
    // Common revision metadata fields will be overwritten later on.
    // This is just to bring along the metadata attached via urc_revision_metadata_set.

    let metadata_hash = state_source.metadata_hash();
    if !metadata_hash.is_zero() {
        lore_debug!("Merged revision metadata");
    }
    state_staged.set_metadata_hash(metadata_hash);

    Ok(())
}

pub async fn merge_metadata(
    repository: Arc<RepositoryContext>,
    changes: Arc<Vec<NodeChange>>,
    state_source: Arc<State>,
    state_staged: Arc<State>,
) -> Result<(), MergeError> {
    merge_file_metadata(
        repository.clone(),
        changes,
        state_source.clone(),
        state_staged.clone(),
    )
    .await?;

    merge_revision_metadata(state_source.clone(), state_staged.clone())?;

    Ok(())
}

/// Link-scoped merge into: updates only the link pin on the target branch.
///
/// Instead of diffing all parent changes, this resolves the link in the current
/// and target states, updates the target's link pin to match the current state,
/// then commits and pushes the result to the target branch.
#[allow(clippy::too_many_arguments)]
async fn merge_into_link(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    target_branch: BranchId,
    message: String,
    state_current: Arc<State>,
    state_branch: Arc<State>,
    branch_latest: Hash,
    link_path: &str,
) -> Result<(), MergeError> {
    // Resolve link in current state (source) to get the updated pin
    let link_path_owned = link_path.to_string();
    let current_link = link::resolve_link_at_path(&state_current, repository.clone(), link_path)
        .await
        .forward_with::<MergeError, _>(|| format!("link not found: {link_path_owned}"))?;

    // Prepare staged state from target branch latest
    let state_staged = state::State::deserialize(repository.clone(), state_branch.revision())
        .await
        .forward::<MergeError>("deserializing staged state")?;

    // Update the link pin on the staged state to match the current state's pin
    link::update_link_pin_by_path(
        &state_staged,
        repository.clone(),
        link_path,
        current_link.link_reference.branch,
        current_link.link_reference.signature,
    )
    .await
    .forward::<MergeError>("updating link pin")?;

    // Get or create metadata chunk
    let metadata_hash = state_staged.metadata_hash();
    if metadata_hash.is_zero() {
        return Err(MergeError::internal("Failed to deserialize metadata"));
    }
    let original_metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward::<MergeError>("deserializing metadata")?;

    let metadata = commit::prepare_commit_metadata(
        repository.clone(),
        original_metadata,
        target_branch,
        message,
        None,
        None,
        None,
    )
    .await
    .forward::<MergeError>("preparing commit metadata")?;

    // Own tracker scoped to this rehash step: await_all always runs before
    // propagating the rehash result so no spawned leader outlives the
    // function holding references to local state.
    let rehash_tracker = std::sync::Arc::new(lore_storage::write_tracker::WriteTracker::new());
    let rehash_result = commit::commit_files_and_rehash(
        repository.clone(),
        token.share(),
        state_staged.clone(),
        repository.require_path()?,
        metadata.clone(),
        None,
        std::sync::Arc::new(std::collections::HashMap::new()),
        target_branch,
        rehash_tracker.clone(),
    )
    .await;
    let drain_result = rehash_tracker.await_all().await;
    rehash_result.forward::<MergeError>("rehashing commit")?;
    drain_result.forward::<MergeError>("draining rehash tracker")?;

    let state_new = state_staged;
    state_new.reset_merge_conflict_flags();
    state_new.set_parent_self(branch_latest);
    state_new.set_parent_other(state_current.revision());

    state_new.set_metadata_hash(
        metadata
            .serialize(repository.clone())
            .await
            .forward::<MergeError>("serializing metadata")?,
    );

    commit::weave_history(repository.clone(), state_new.clone())
        .await
        .forward::<MergeError>("weaving history")?;

    let signature = state_new
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing state")?;

    // Collect and push fragments
    let fragments = state::collect_new_fragments(
        repository.clone(),
        state_branch.clone(),
        state_new.clone(),
        true,
    )
    .await
    .forward::<MergeError>("collecting new fragments")?;

    let mut revision = signature;
    let mut revision_number = state_new.revision_number();

    if let Ok(remote) = repository.remote().await {
        let stats = Arc::new(PushStatistics::default());
        let correlation_id = execution_context().globals().correlation_id.to_string();
        let storage_protocol = remote
            .session(repository.id, &correlation_id)
            .await
            .forward::<MergeError>("opening storage session")?;
        let revision_protocol = remote
            .revision(repository.id)
            .await
            .forward::<MergeError>("acquiring revision protocol")?;

        let missing_fragments = push_query(
            storage_protocol.clone(),
            fragments,
            remote.environment.max_query_batch(),
        )
        .await
        .forward::<MergeError>("querying fragments")?;

        push_fragments(
            repository.clone(),
            storage_protocol,
            missing_fragments,
            stats,
        )
        .await
        .forward::<MergeError>("pushing fragments")?;

        let response = revision_protocol
            .branch_push(target_branch, signature, false, false)
            .await
            .forward::<MergeError>("pushing branch")?;

        if response.fast_forward_merged {
            return Err(MergeError::internal(format!(
                "Branch was fast-forward merged to revision {}",
                response.revision
            )));
        }
        if response.revision_number == 0 {
            return Err(MergeError::internal(format!(
                "Branch on remote has moved to revision {}",
                response.revision
            )));
        }

        revision = response.revision;
        revision_number = response.revision_number;
    }

    LoreEvent::BranchMergeIntoRevision(LoreBranchMergeIntoRevisionEventData {
        revision,
        revision_number,
    })
    .send();

    Ok(())
}

pub async fn merge_into(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    options: MergeIntoOptions,
) -> Result<(), MergeError> {
    let (state_current, _state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;

    if branch == current_branch {
        return Err(MergeError::internal("Cannot merge a branch with itself"));
    }

    let branch_latest = if let Ok(remote) = repository.remote().await {
        branch::load_remote_latest(remote.clone(), repository.id, branch)
            .await
            .forward::<MergeError>("loading remote branch latest")?
    } else {
        Hash::default()
    };
    if branch_latest.is_zero() {
        return Err(MergeError::internal("Invalid branch latest revision"));
    }

    let branch_metadata = branch::metadata(repository.clone(), current_branch)
        .await
        .forward::<MergeError>("loading branch metadata")?;
    let stack = branch::stack(&branch_metadata);
    let branch_point = stack
        .first()
        .map(|parent| parent.revision)
        .unwrap_or_default();
    if branch_point.is_zero() {
        return Err(MergeError::internal("Invalid branch"));
    }

    let current_latest = branch::load_latest(repository.clone(), current_branch)
        .await
        .forward::<MergeError>("loading branch latest")?;
    if current_latest == branch_latest {
        return Err(MergeError::internal("Invalid branch latest revision"));
    }

    // Verify branch latest was merged into current branch
    let result = find::find_revision(
        repository.clone(),
        current_branch,
        current_latest,
        false,
        None,
        |state, _metadata| {
            let is_branch_point = state.revision() == branch_point;

            if state.parent_other() == branch_latest /* merged in */ || state.parent_self() == branch_latest /* branch point */ {
                find::FindMatchResult::Match
            } else if is_branch_point {
                find::FindMatchResult::Abort
            } else {
                find::FindMatchResult::Continue
            }
        },
    )
    .await;
    if result.is_err() {
        return Err(MergeError::internal(
            "Target branch to merge into has a newer revision, merge target branch first",
        ));
    }

    let state_branch = state::State::deserialize(repository.clone(), branch_latest)
        .await
        .forward::<MergeError>("deserializing branch state")?;

    // When --link is specified, we update only the link pin on the target branch
    // instead of diffing all changes. This avoids applying internal link changes
    // through the parent diff machinery.
    if let Some(ref link_path) = options.link {
        return merge_into_link(
            repository,
            token,
            branch,
            options.message,
            state_current,
            state_branch,
            branch_latest,
            link_path,
        )
        .await;
    }

    // Determine how to get from the branch latest state into our current state
    let mut changes = state::diff_collect(
        repository.clone(),
        state_branch.clone(),
        repository.clone(),
        state_current.clone(),
        None,
        FilterMode::View,
    )
    .await
    .forward::<MergeError>("running diff")?;

    // `state::diff` recurses into links when pin hashes differ; for
    // `--ignore-links` we want a parent-only changeset, so drop those.
    // The link pin itself remains whatever `state_branch` has on the
    // target side.
    if options.ignore_links {
        changes.retain(|c| c.to.repository.id == repository.id);
    }

    change::sort_by_path(&mut changes);

    let changes_count = changes.len();
    LoreEvent::BranchMergeIntoFileBegin(LoreBranchMergeIntoFileBeginEventData {
        count: changes_count,
    })
    .send();

    for change in &changes {
        let node = {
            if change.action == change::FileAction::Delete {
                let block = change
                    .from
                    .state
                    .block(
                        change.from.repository.clone(),
                        NodeBlock::index(change.from.node),
                    )
                    .await
                    .forward::<MergeError>("deserializing block")?;
                block.node(Node::index(change.from.node))
            } else {
                let block = change
                    .to
                    .state
                    .block(
                        change.to.repository.clone(),
                        NodeBlock::index(change.to.node),
                    )
                    .await
                    .forward::<MergeError>("deserializing block")?;
                block.node(Node::index(change.to.node))
            }
        };

        LoreEvent::BranchMergeIntoFile(LoreBranchMergeIntoFileEventData {
            path: LoreString::from(&change.path),
            action: change.action.into(),
            size: node.size,
            is_file: node.is_file() as u8,
            is_directory: node.is_directory() as u8,
            is_link: node.is_link() as u8,
        })
        .send();
    }

    LoreEvent::BranchMergeIntoFileEnd(LoreBranchMergeIntoFileEndEventData {
        count: changes_count,
    })
    .send();

    // Prepare the staged state, starting from the branch latest state
    let state_staged = state::State::deserialize(repository.clone(), state_branch.revision())
        .await
        .forward::<MergeError>("deserializing staged state")?;

    LoreEvent::BranchMergeIntoSyncBegin(LoreBranchMergeIntoSyncBeginEventData {
        count: changes_count,
    })
    .send();

    // Apply the changes on the state (but not disk)
    let stats = Arc::new(sync::SyncRealizeStats::default());
    sync::realize_changes(
        repository.clone(),
        Arc::new(changes.clone()),
        Some(state_staged.clone()),
        true, /* No changes on disk */
        true, /* Merge */
        stats,
    )
    .await
    .forward::<MergeError>("realizing changes")?;
    lore_debug!("Realized changes on state");

    LoreEvent::BranchMergeIntoSyncEnd(LoreBranchMergeIntoSyncEndEventData {
        count: changes_count,
    })
    .send();

    // Apply the metadata on the state
    merge_metadata(
        repository.clone(),
        Arc::new(changes.clone()),
        state_current.clone(),
        state_staged.clone(),
    )
    .await?;
    lore_debug!("Merged metadata on state");

    // Get or create metadata chunk
    let metadata_hash = state_staged.metadata_hash();
    if metadata_hash.is_zero() {
        return Err(MergeError::internal("Failed to deserialize metadata"));
    }
    let original_metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward::<MergeError>("deserializing metadata")?;

    let metadata = commit::prepare_commit_metadata(
        repository.clone(),
        original_metadata,
        branch,
        options.message.clone(),
        None,
        None,
        None,
    )
    .await
    .forward::<MergeError>("preparing commit metadata")?;

    // Own tracker scoped to this rehash step: await_all always runs before
    // propagating the rehash result so no spawned leader outlives the
    // function holding references to local state.
    let rehash_tracker = std::sync::Arc::new(lore_storage::write_tracker::WriteTracker::new());
    let rehash_result = commit::commit_files_and_rehash(
        repository.clone(),
        token.share(),
        state_staged.clone(),
        repository.require_path()?,
        metadata.clone(),
        None,
        std::sync::Arc::new(std::collections::HashMap::new()),
        current_branch,
        rehash_tracker.clone(),
    )
    .await;
    let drain_result = rehash_tracker.await_all().await;
    rehash_result.forward::<MergeError>("rehashing commit")?;
    drain_result.forward::<MergeError>("draining rehash tracker")?;
    lore_debug!("Rehashed state");

    let state_new = state_staged;
    state_new.reset_merge_conflict_flags();
    state_new.set_parent_self(branch_latest);
    state_new.set_parent_other(state_current.revision());

    state_new.set_metadata_hash(
        metadata
            .serialize(repository.clone())
            .await
            .forward::<MergeError>("serializing metadata")?,
    );

    commit::weave_history(repository.clone(), state_new.clone())
        .await
        .forward::<MergeError>("weaving history")?;

    let signature = state_new
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing state")?;

    // Check missing fragments on server
    lore_debug!(
        "Calculating new fragments from {} to {}",
        state_branch.revision(),
        state_new.revision()
    );
    let fragments = state::collect_new_fragments(
        repository.clone(),
        state_branch.clone(),
        state_new.clone(),
        true, /* Ignore already durably stored fragments */
    )
    .await
    .forward::<MergeError>("collecting new fragments")?;

    let mut revision = signature;
    let mut revision_number = state_new.revision_number();

    if let Ok(remote) = repository.remote().await {
        let stats = Arc::new(PushStatistics::default());

        LoreEvent::BranchMergeIntoFragmentBegin(LoreBranchMergeIntoFragmentBeginEventData {
            fragments: fragments.len() as u64,
        })
        .send();

        let correlation_id = execution_context().globals().correlation_id.to_string();
        let storage_protocol = remote
            .session(repository.id, &correlation_id)
            .await
            .forward::<MergeError>("opening storage session")?;
        let revision_protocol = remote
            .revision(repository.id)
            .await
            .forward::<MergeError>("acquiring revision protocol")?;

        let missing_fragments = push_query(
            storage_protocol.clone(),
            fragments,
            remote.environment.max_query_batch(),
        )
        .await
        .forward::<MergeError>("querying fragments")?;

        let mut push_task = lore_spawn!({
            let repository = repository.clone();
            let stats = stats.clone();
            async move { push_fragments(repository, storage_protocol, missing_fragments, stats).await }
        });

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        let result = loop {
            tokio::select! {
                _ = ticker.tick() => {
                    LoreEvent::BranchMergeIntoFragmentProgress(LoreBranchMergeIntoFragmentProgressEventData {
                        complete: stats.fragment_complete.load(Ordering::Relaxed) as u64,
                        count: stats.fragment_count.load(Ordering::Relaxed) as u64,
                    }).send();
                },
                result = &mut push_task => {
                    break result.map_err(|e| MergeError::internal_with_context(e, "task failure"))?;
                }
            }
        };
        result.forward::<MergeError>("pushing fragments")?;

        LoreEvent::BranchMergeIntoFragmentEnd(LoreBranchMergeIntoFragmentEndEventData {
            fragments: stats.fragment_complete.load(Ordering::Relaxed) as u64,
        })
        .send();

        let response = revision_protocol
            .branch_push(branch, signature, false, false)
            .await
            .forward::<MergeError>("pushing branch")?;

        if response.fast_forward_merged {
            return Err(MergeError::internal(format!(
                "Branch was fast-forward merged to revision {}",
                response.revision
            )));
        }
        if response.revision_number == 0 {
            return Err(MergeError::internal(format!(
                "Branch on remote has moved to revision {}",
                response.revision
            )));
        }

        revision = response.revision;
        revision_number = response.revision_number;
    }

    LoreEvent::BranchMergeIntoRevision(LoreBranchMergeIntoRevisionEventData {
        revision,
        revision_number,
    })
    .send();

    Ok(())
}
