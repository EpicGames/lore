// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

use futures::StreamExt;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_transport::Connection;
use serde::Deserialize;
use serde::Serialize;
use tokio::join;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::branch;
use crate::branch::push::PushProgress;
use crate::branch::push::push_fragments;
use crate::branch::push::push_query;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::commit;
use crate::commit::CommitOptions;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::find;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::infer::ConflictMarkers;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreEvent;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::link::Mount;
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
use crate::metadata::MetadataInherit;
use crate::node;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::NodeLink;
use crate::node::ROOT_NODE;
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
use crate::state::LinkMergeResolution;
use crate::state::LinkPin;
use crate::state::LinkReference;
use crate::state::Mark;
use crate::state::NodeMapping;
use crate::state::State;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;
use crate::util::request_tracker::StoreRequestTracker;
use crate::util::serde::u8_as_bool;

/// Data for the event sent when a branch merge starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeAbortBeginEventData {
    /// The staged revision being discarded.
    pub state_staged_revision: Hash,
    /// The current revision the working state returns to.
    pub state_current_revision: Hash,
}

/// Data for the event sent when a branch merge abort finishes.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeAbortEndEventData {
    /// Placeholder field. The event carries no payload.
    pub _unused: u32,
}

/// Data for the event sent before files are merged into the working tree.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFileBeginEventData {
    /// The number of files to merge.
    pub count: usize,
}

/// Data for the event sent for each file merged into the working tree.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFileEndEventData {
    /// The number of files merged.
    pub count: usize,
}

/// Data for the event sent before the merge synchronizes revisions.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoSyncBeginEventData {
    /// The number of revisions to synchronize.
    pub count: usize,
}

/// Data for the event sent after the merge synchronizes revisions.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoSyncEndEventData {
    /// The number of revisions synchronized.
    pub count: usize,
}

/// Data for the event sent before the merge transfers fragments.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFragmentBeginEventData {
    /// The number of fragments to transfer.
    pub fragments: u64,
}

/// Data for the event sent as the merge transfers fragments.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFragmentProgressEventData {
    /// The number of fragments transferred so far.
    pub complete: u64,
    /// The total number of fragments to transfer.
    pub count: u64,
}

/// Data for the event sent after the merge transfers fragments.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoFragmentEndEventData {
    /// The number of fragments transferred.
    pub fragments: u64,
}

/// Data for the event sent for each revision merged into the working tree.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeIntoRevisionEventData {
    /// The revision merged.
    pub revision: Hash,
    /// The sequential number of the revision.
    pub revision_number: u64,
}

/// Data for the event sent for each file the merge left in conflict.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeLinkSkippedEventData {
    /// The mount path of the skipped link.
    pub link_path: LoreString,
    /// The repository of the skipped link.
    pub repository: RepositoryId,
    /// The reason the link was skipped.
    pub reason: u8,
}

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// Which side a divergent link row was settled to.
#[derive(Copy, Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub enum LoreLinkPinResolution {
    /// The row this branch held.
    Mine = 0,
    /// The row the merged branch held.
    Theirs = 1,
}

impl From<LoreLinkPinResolution> for LinkMergeResolution {
    fn from(resolution: LoreLinkPinResolution) -> Self {
        match resolution {
            LoreLinkPinResolution::Mine => Self::Mine,
            LoreLinkPinResolution::Theirs => Self::Theirs,
        }
    }
}

/// One side of a link row the two branches disagree on.
#[repr(C)]
#[derive(Clone, PartialEq, Default, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreLinkPinSideData {
    /// The branch the row resolves to.
    pub branch: BranchId,
    /// The revision the row pins.
    pub revision: Hash,
    /// The row's flags: `1` where the link does not follow the parent's branches.
    pub flags: u32,
    /// The path in the linked repository the row mounts, empty when it cannot be read.
    pub source_path: LoreString,
    /// Message of the revision the row pins, empty when it cannot be read.
    pub message: LoreString,
}

/// Data for the event sent when a link row diverges across a merge.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeLinkPinConflictEventData {
    /// The mount path of the divergent link.
    pub link_path: LoreString,
    /// The repository both sides point at.
    pub repository: RepositoryId,
    /// The row this branch holds.
    pub mine: LoreLinkPinSideData,
    /// The row the merged branch holds.
    pub theirs: LoreLinkPinSideData,
}

/// Data for the event sent when a divergent link row is settled.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeLinkPinResolvedEventData {
    /// The mount path of the settled link.
    pub link_path: LoreString,
    /// The side the row was settled to.
    pub resolution: LoreLinkPinResolution,
}

/// Data for the event sent when a file in a merge is marked unresolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeUnresolveFileEventData {
    /// The path of the file marked unresolved.
    pub path: LoreString,
}

/// Data for the event sent when a revision in a merge is marked unresolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeUnresolveRevisionEventData {
    /// The repository of the revision marked unresolved.
    pub repository: RepositoryId,
    /// The revision marked unresolved.
    pub revision: Hash,
}

/// Data for the event sent when a file in a merge is marked resolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchMergeResolveFileEventData {
    /// The path of the file marked resolved.
    pub path: LoreString,
}

/// Data for the event sent when a revision in a merge is marked resolved.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
    /// Per-link auto commit messages, keyed by mount path.
    pub link_messages: std::collections::HashMap<String, String>,
    /// Disable auto commits, even if no conflicts arise.
    pub no_commit: bool,
    /// Which repositories to include in the merge.
    pub scope: MergeScope,
    /// Metadata keys carried from the source revision onto the merge revision.
    pub inherit_metadata: MetadataInherit,
}

/// The revisions a merge's three-way diff ran between, other than the target.
#[derive(Clone, Copy)]
pub struct MergeDiffEndpoints {
    pub base: Hash,
    pub source: Hash,
}

/// Result of merging a single repository (main or linked).
pub struct MergeRepositoryResult {
    pub signature: Hash,
    pub has_conflicts: bool,
    pub state_staged: Arc<State>,
    /// Endpoints for reconciling state the diff does not carry, such as link pins.
    pub endpoints: MergeDiffEndpoints,
    /// Conflict-realization context: the 3-way merge inputs and the conflict
    /// pairs from the diff. Surfaced so `merge_start_all` can spell them from
    /// the link's mount and call `realize_conflicts` without re-running
    /// diff3. `None` when no conflicts (`!has_conflicts`).
    pub conflict_context: Option<ConflictRealizeContext>,
}

/// Inputs needed to materialise conflict markers and `.mine`/`.theirs`/
/// `.base` sidecars on disk after a merge. Produced by `merge_repository`,
/// consumed by [`realize_link_conflicts`] after `stage_link_pin` has placed the
/// link's pre-merge content on disk.
#[derive(Clone)]
pub struct ConflictRealizeContext {
    pub state_base: Arc<State>,
    pub state_from: Arc<State>,
    pub state_to: Arc<State>,
    /// Paths are relative to the merged repository's root, which for a linked repo merge is not
    /// the working tree: [`mount_conflicts`] names each by its node to spell it from the mount.
    pub conflicts: Arc<Vec<(NodeChange, NodeChange)>>,
}

/// A merge of one repository worked out from its branches, with nothing written yet.
struct PreparedMerge {
    source: MergeSource,
    /// The revision the merge starts from, at the path the working tree holds it. `None` where
    /// the working tree does not hold it.
    current: Option<NodeMapping>,
    /// The staged state of the merge this one resumes, whose link pins name what the mounts of the
    /// links merged on an earlier pass show.
    resumed: Option<Arc<State>>,
    diff: PreparedDiff,
}

/// The revisions a merge of one repository runs between.
#[derive(Clone, Copy)]
struct MergeSource {
    source_branch: BranchId,
    current_branch: BranchId,
    /// The source revision the merge takes in.
    revision: Hash,
    revision_number: u64,
    current_signature: Hash,
}

/// Merges the latest of `source_branch` into `current_signature` on `current_branch`, checked
/// against the working tree, and stages the result. `current` is the revision the merge starts
/// from, at the path the working tree holds it.
#[allow(clippy::too_many_arguments)]
async fn merge_repository(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    source_branch: BranchId,
    current_branch: BranchId,
    current_signature: Hash,
    current: NodeMapping,
    inherit: &MetadataInherit,
) -> Result<MergeRepositoryResult, MergeError> {
    let source = resolve_merge_source(
        &repository,
        source_branch,
        current_branch,
        current_signature,
    )
    .await?;
    let merge = prepare_merge(repository.clone(), source, Some(current)).await?;
    apply_prepared_merge(repository, token, merge, inherit).await
}

/// The revision of `source_branch` a merge into `current_signature` on `current_branch` takes in.
async fn resolve_merge_source(
    repository: &Arc<RepositoryContext>,
    source_branch: BranchId,
    current_branch: BranchId,
    current_signature: Hash,
) -> Result<MergeSource, MergeError> {
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

    merge_source_at(
        repository,
        source_branch,
        revision,
        current_branch,
        current_signature,
    )
    .await
}

/// The merge of `revision` on `source_branch` into `current_signature` on `current_branch`.
async fn merge_source_at(
    repository: &Arc<RepositoryContext>,
    source_branch: BranchId,
    revision: Hash,
    current_branch: BranchId,
    current_signature: Hash,
) -> Result<MergeSource, MergeError> {
    if current_branch == source_branch && current_signature == revision {
        return Err(MergeError::internal("Cannot merge a branch with itself"));
    }

    let revision_number = state::State::deserialize(repository.clone(), revision)
        .await
        .forward::<MergeError>("deserializing merge revision state")?
        .revision_number();

    Ok(MergeSource {
        source_branch,
        current_branch,
        revision,
        revision_number,
        current_signature,
    })
}

/// Diff `source`'s revisions for a merge that starts from `current`.
async fn prepare_merge(
    repository: Arc<RepositoryContext>,
    source: MergeSource,
    current: Option<NodeMapping>,
) -> Result<PreparedMerge, MergeError> {
    // Diff over the full tree. A view-scoped diff drops the other branch's
    // out-of-view changes and leaves this branch divergent from it.
    let diff = branch::diff3_collect_with_graft(
        full_tree_context(&repository),
        source.source_branch,
        source.revision,
        source.current_branch,
        source.current_signature,
        None,  /* No path */
        true,  /* Include identical changes for merge tracking */
        false, /* Do not autoresolve, this is done later */
        // The view decides which subtrees are out of view. The walk stays
        // full-tree.
        Some(repository.filter.clone()),
    )
    .await
    .forward::<MergeError>("running diff3 for merge")?;

    Ok(PreparedMerge {
        source,
        diff: prepare_diff(&repository, diff),
        current,
        resumed: None,
    })
}

/// Work out the merge of `source_branch` into `current_signature` and check it against the
/// working tree, refusing it where a local modification would be overwritten. `current` is the
/// revision the merge starts from, at the path the working tree holds it, `None` where it holds
/// none of it. `resumed` is the staged state of the merge it resumes.
async fn check_merge(
    repository: &Arc<RepositoryContext>,
    source_branch: BranchId,
    current_branch: BranchId,
    current_signature: Hash,
    current: Option<NodeMapping>,
    resumed: Option<Arc<State>>,
) -> Result<PreparedMerge, MergeError> {
    let source =
        resolve_merge_source(repository, source_branch, current_branch, current_signature).await?;
    let mut merge = prepare_merge(repository.clone(), source, None).await?;
    merge.resumed = resumed;
    let Some(current) = current else {
        return Ok(merge);
    };
    with_operation(repository.file_system(), async |operation| {
        let PreparedDiff {
            changes, conflicts, ..
        } = &mut merge.diff;
        verify_diff_against_filesystem(
            &operation,
            repository,
            &current,
            merge.resumed.as_ref(),
            changes,
            conflicts,
            MergeType::BranchMerge,
        )
        .await?;
        // Applying checks again where it writes; clearing what this check found lets that check
        // settle it afresh.
        for change in changes
            .iter_mut()
            .chain(conflicts.iter_mut().map(|(_, change_to)| change_to))
        {
            change
                .flags
                .remove(change::Flags::LocalMode | change::Flags::HoldsContent);
        }
        Ok::<_, MergeError>(())
    })
    .await?;
    merge.current = Some(current);
    Ok(merge)
}

/// The nodes of the links of `state_current` that `diff` replaces, which land with the tree rather
/// than as a carried pin.
async fn links_the_diff_replaces(
    repository: &Arc<RepositoryContext>,
    state_current: &Arc<State>,
    diff: &PreparedDiff,
) -> Result<Vec<NodeID>, MergeError> {
    let replaced: HashSet<&str> = diff
        .changes
        .iter()
        .chain(diff.conflicts.iter().map(|(_, change_to)| change_to))
        .filter(|change| change.from.is_link() || change.to.is_link())
        .map(|change| change.path().as_lowercase_str())
        .collect();
    let mut nodes = Vec::new();
    if replaced.is_empty() {
        return Ok(nodes);
    }
    let link_list = state_current
        .link_list(repository.clone())
        .await
        .forward::<MergeError>("listing links")?;
    for link_reference in &link_list {
        let link_path = state_current
            .node_path(repository.clone(), link_reference.local_node)
            .await
            .forward::<MergeError>("resolving link node path")?;
        if replaced.contains(link_path.to_lowercase().as_str()) {
            nodes.push(link_reference.local_node);
        }
    }
    Ok(nodes)
}

/// Check the pins a repository's merge carries onto links not merged in their own right against
/// the working tree, which materializes the repository at `mount`, refusing where a local
/// modification would be overwritten. `left_out` names the links the merge carries no pin to.
/// Returns the rows both sides moved, which the merge carries no pin to either.
///
/// The pins are carried after the repository's merge is written. Its diff carries no pin, so
/// `state_current` stands in for the merged state to work them out before.
async fn check_link_pins(
    repository: &Arc<RepositoryContext>,
    state_current: &Arc<State>,
    current_branch: BranchId,
    state_base: &Arc<State>,
    state_source: &Arc<State>,
    left_out: &[NodeID],
    mount: Option<&Mount>,
) -> Result<Vec<link::LinkPinConflict>, MergeError> {
    let link::LinkPinClassification {
        mut planned,
        conflicts,
    } = link::classify_link_pins_before_merge(
        repository.clone(),
        state_current,
        current_branch,
        state_source,
        link::LinkPinResolution::ThreeWay(state_base.clone()),
        left_out,
    )
    .await
    .forward::<MergeError>("comparing link pins with the merged branch")?;
    let Some(mount) = mount else {
        return Ok(conflicts);
    };
    planned.retain_mut(|planned| {
        let Ok(path) = RelativePath::from_str(&planned.link_path);
        mount
            .spell(&path)
            .map(|path| planned.link_path = path.to_string())
            .is_some()
    });
    if !planned.is_empty() {
        with_operation(repository.file_system(), async |operation| {
            link::verify_link_pins(&operation, repository, &planned)
                .await
                .forward::<MergeError>("checking link pins against the working tree")
        })
        .await?;
    }
    Ok(conflicts)
}

/// Apply `merge` over the working tree and store its staged state.
async fn apply_prepared_merge(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    merge: PreparedMerge,
    inherit: &MetadataInherit,
) -> Result<MergeRepositoryResult, MergeError> {
    let PreparedMerge {
        source:
            MergeSource {
                source_branch,
                current_branch,
                revision,
                revision_number,
                ..
            },
        current,
        resumed,
        diff,
    } = merge;

    lore_info!("Starting merge of branch {source_branch} revision {revision}");
    event::LoreEvent::BranchMergeStartBegin(LoreBranchMergeStartBeginEventData {
        branch: source_branch,
        revision,
        revision_number,
    })
    .send();

    // Capture the diff's revision endpoints before `apply_prepared_diff` consumes
    // it — `merge_start_all` needs the corresponding states to realize conflict
    // markers at a remapped mount path without re-running diff3.
    let (diff_base, diff_source, diff_target) = (diff.base, diff.source, diff.target);

    let ApplyDiffResults {
        signature,
        conflicts,
        state_staged,
        stats,
    } = apply_prepared_diff(
        repository.clone(),
        token,
        diff,
        current,
        resumed,
        MergeType::BranchMerge,
        // If this merge is reconciling a remote LATEST with local LATEST of a branch
        // reverse the parent order in order to keep the remote history as the main
        // history line shows in CLI output and other places
        current_branch == source_branch,
        current_branch,
        inherit,
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
        endpoints: MergeDiffEndpoints {
            base: diff_base,
            source: diff_source,
        },
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

    validate_link_messages(&repository, &state_current, &options).await?;

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
                state_staged,
                endpoints,
                ..
            } = merge_repository(
                repository.clone(),
                token,
                branch,
                current_branch,
                state_current.revision(),
                NodeMapping::root(repository.clone(), state_current.clone()),
                &options.inherit_metadata,
            )
            .await?;

            let dry_run = execution_context().globals().dry_run();

            // The parent's link list is parent state, so `--ignore-links` skips
            // merging the linked repositories, not the comparison. Carrying a
            // row would realize linked content, which the flag does suppress.
            let state_base = state::State::deserialize(repository.clone(), endpoints.base)
                .await
                .forward::<MergeError>("deserializing diff base state")?;
            let state_source = state::State::deserialize(repository.clone(), endpoints.source)
                .await
                .forward::<MergeError>("deserializing diff source state")?;
            warn_of_unmerged_links(&repository, &state_current, &state_base, &state_source).await?;
            let classification = link::classify_link_pins(
                repository.clone(),
                &state_staged,
                current_branch,
                &state_source,
                link::LinkPinResolution::ThreeWay(state_base),
                &[],
            )
            .await
            .forward::<MergeError>("comparing link pins")?;
            let unresolved_pins = !classification.conflicts.is_empty();
            send_link_pin_conflicts(&repository, current_branch, &classification.conflicts).await;

            if unresolved_pins && !dry_run {
                record_link_merge_state(
                    repository.clone(),
                    &state_staged,
                    &[],
                    &classification.conflicts,
                )
                .await?;
                signature = state_staged
                    .serialize(repository.clone(), token)
                    .await
                    .forward::<MergeError>("serializing staged state")?;
                crate::instance::store_staged_anchor(&repository, signature)
                    .await
                    .forward::<MergeError>("storing staged anchor")?;
            }

            if !has_conflicts && !unresolved_pins && !dry_run && !options.no_commit {
                signature = auto_commit_merge(repository, token, options).await?;
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
                    // A divergent row is outstanding work, not a link a prior
                    // pass merged, so it does not make this state resumable.
                    entries.iter().any(|entry| !entry.is_conflict())
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

/// Merge a specific linked repository, with the links nested in it (`MergeScope::Link`).
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
        parent_repository,
        ..
    } = link::resolve_link_at_path(&state, repository.clone(), &link_path)
        .await
        .forward_with::<MergeError, _>(|| format!("link not found: {link_path}"))?;
    if parent_repository.id != repository.id {
        return Err(NotSupported {
            operation: format!(
                "merging only the link at {link_path}, which is nested in another link; merge \
                 the link holding it"
            ),
        }
        .into());
    }

    let eligible = EligibleLink {
        link_path: link_path.clone(),
        mount: Some(link_path_rel.clone()),
        link_node,
        link_context: link_context.clone(),
        link_reference,
        resolved_branch: link_reference.resolve_branch(current_branch),
    };
    match link::check_link_merge_eligible(&link_context, &link_reference, branch).await {
        link::LinkMergeEligibility::Eligible => {}
        link::LinkMergeEligibility::Skip => {
            skip_link(&eligible, LinkMergeSkipReason::Inaccessible);
            return Ok(state.revision());
        }
        link::LinkMergeEligibility::AutoFollowDisabled => {
            return Err(MergeError::internal(format!(
                "Link {link_path} has auto-follow disabled"
            )));
        }
    }
    let Some(planned) = plan_link_merges(vec![eligible], branch, &[repository.id])
        .await?
        .pop()
    else {
        return Ok(state.revision());
    };

    // Save pre-merge snapshot for rollback
    let link_merge_entry = LinkMergeEntry::rollback(link_reference);

    let dry_run = execution_context().globals().dry_run();
    let WrittenLink {
        signature: link_signature,
        outcome,
        ..
    } = write_link_merge(
        MergeWrite {
            token,
            inherit: &options.inherit_metadata,
            dry_run,
        },
        planned,
    )
    .await?;
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
        link_signature,
        link_reference.branch,
        link::LinkMountSource::SamePath,
        link::LinkPinRealize::WorkingTree,
    )
    .await
    .forward_with::<MergeError, _>(|| format!("staging link pin for {link_path}"))?;
    // The markers are written over what `stage_link_pin` realized at each mount.
    for pending in &outcome.pending_conflicts {
        realize_link_conflicts(pending).await?;
    }

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
    let has_conflicts = outcome.has_conflicts || outcome.unresolved_pins;
    if has_conflicts {
        state.set_merge_conflict();
    }

    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing staged state")?;

    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<MergeError>("storing staged anchor")?;
    create_nested_branches(&outcome.missing_branches, branch).await?;

    // Auto-commit if no conflicts
    if !has_conflicts && !options.no_commit {
        return auto_commit_merge(repository, token, options).await;
    }

    Ok(signature)
}

/// A link a merge will operate on after eligibility checks pass.
struct EligibleLink {
    /// The link's path in the repository holding it.
    link_path: String,
    /// Where the working tree materializes the link, `None` where it materializes nothing of it.
    mount: Option<RelativePath>,
    link_node: Node,
    link_context: Arc<RepositoryContext>,
    link_reference: state::LinkReference,
    resolved_branch: BranchId,
}

impl EligibleLink {
    /// The path the merge reports the link at.
    fn shown_path(&self) -> &str {
        self.mount
            .as_ref()
            .map_or(self.link_path.as_str(), |mount| mount.as_str())
    }
}

/// A link merge checked against the working tree, waiting to be written.
struct PlannedLinkMerge {
    eligible: EligibleLink,
    source: MergeSource,
    /// The revision the merge starts from, at the path the working tree holds it. `None` where
    /// the working tree holds none of it.
    current: Option<NodeMapping>,
    /// `None` where neither side of the merge holds a link, or the link is `MAX_LINK_DEPTH` deep.
    nested: Option<NestedLinkMerges>,
}

/// What a link's merge does to the links its repository holds, checked against the working tree.
struct NestedLinkMerges {
    planned: Vec<PlannedLinkMerge>,
    incoming: Vec<IncomingLinks>,
    /// Where the working tree materializes the link's repository.
    mount: Option<Mount>,
    /// The base and source of the link's merge, between which the pins of the links not merged
    /// in their own right are carried.
    state_base: Arc<State>,
    state_source: Arc<State>,
}

/// A link whose merge produced a (possibly conflicted) staged state. Its pin
/// will be applied to the parent's staged state in a subsequent step.
struct MergedLink {
    link_path: String,
    /// `link_path` as a `RelativePath`, which `finalize_link_conflict_state` stages the pin at.
    /// `finalize_main_merge` stages it at the path `link::merged_link_path` resolves instead.
    link_path_rel: RelativePath,
    link_node: Node,
    link_context: Arc<RepositoryContext>,
    link_reference: state::LinkReference,
    new_signature: Hash,
    /// Whether staging the pin realizes it in the working tree. A link merged on an earlier pass
    /// of the merge is already realized there, with the resolutions made since on top.
    realize: link::LinkPinRealize,
}

/// Inputs captured for a link merge that produced conflicts so the caller can
/// realize markers + sidecars on disk *after* `stage_link_pin` runs (the pin
/// step expects the link's pre-merge content on disk; markers are written on
/// top once the pin is in place).
struct PendingConflictRealize {
    link_context: Arc<RepositoryContext>,
    link_path: String,
    link_node: Node,
    context: ConflictRealizeContext,
}

/// Where `mapping`'s node is materialized under `mount_path`, or `None` where the link exposes no
/// subtree holding it.
///
/// `source_path` is resolved against `mapping`'s own revision: each revision of the linked
/// repository numbers its nodes as it pleases, so the subtree the link exposes is a different
/// node in each.
async fn mounted_node_path(
    mapping: &NodeMapping,
    mount_path: &RelativePath,
    source_path: &RelativePath,
) -> Option<RelativePath> {
    let subtree_node = if source_path.is_empty() {
        ROOT_NODE
    } else {
        mapping
            .state
            .find_node_link(mapping.repository.clone(), source_path.as_str())
            .await
            .ok()
            .filter(NodeLink::is_valid)?
            .node
    };

    let below = mapping
        .state
        .node_path_below(mapping.repository.clone(), mapping.node, subtree_node)
        .await
        .ok()??;
    Some(mount_path.join(below.as_str()))
}

/// Where a change is materialized under `mount_path`: the merged side where it holds a node, and
/// the pre-merge side otherwise, which is what a delete leaves.
async fn conflict_mount_path(
    change: &NodeChange,
    mount_path: &RelativePath,
    source_path: &RelativePath,
) -> Option<RelativePath> {
    let side = if change.to.mapping.node.is_valid_or_root_node_id() {
        &change.to
    } else {
        &change.from
    };
    mounted_node_path(&side.mapping, mount_path, source_path).await
}

/// `change` with both its sides spelled from `mount_path`, or `None` where the link exposes
/// nothing holding it.
///
/// A move stands at two paths, so its source is re-derived from the node the pre-merge side
/// holds; one the link does not expose leaves that side empty, as a source that cannot be
/// spelled. Every other change stands at one path, which both sides take.
async fn mount_conflict_change(
    change: &NodeChange,
    mount_path: &RelativePath,
    source_path: &RelativePath,
) -> Option<NodeChange> {
    let mounted = conflict_mount_path(change, mount_path, source_path).await?;
    let mut change = change.clone();
    if change.action == FileAction::Move {
        change.from.mapping.path = mounted_node_path(&change.from.mapping, mount_path, source_path)
            .await
            .unwrap_or_default();
    } else {
        change.from.mapping.path = mounted.clone();
    }
    change.to.mapping.path = mounted;
    Some(change)
}

/// The conflicts a link's merge produced, spelled from the mount they are materialized at.
///
/// A merge covers the whole of the linked repository while the working tree materializes only the
/// subtree the link exposes, so each conflict is named by its node below that subtree's root. One
/// the link does not expose is dropped: nothing on disk holds it, so there is nothing to mark.
///
/// The two sides are spelled apart: a move conflicting with a change at the path it moved from
/// pairs two changes at different paths.
async fn mount_conflicts(
    pending: &PendingConflictRealize,
    mount_path: &RelativePath,
) -> Result<Vec<(NodeChange, NodeChange)>, MergeError> {
    let source_path = link::pinned_source_path(pending.link_context.clone(), &pending.link_node)
        .await
        .forward::<MergeError>("resolving the path a conflicted link exposes")?;

    let mut mounted = Vec::with_capacity(pending.context.conflicts.len());
    for (from, to) in pending.context.conflicts.iter() {
        let (Some(from_mounted), Some(to_mounted)) = (
            mount_conflict_change(from, mount_path, &source_path).await,
            mount_conflict_change(to, mount_path, &source_path).await,
        ) else {
            lore_debug!(
                "Conflict at {} is outside what the link at {mount_path} exposes, not realized",
                to.path()
            );
            continue;
        };

        mounted.push((from_mounted, to_mounted));
    }
    Ok(mounted)
}

/// Run upfront eligibility checks for every link in `state_current`, returning
/// the eligible set and emitting `BranchMergeLinkSkipped` events for the rest.
/// `mount` is where the working tree materializes `repository`.
async fn enumerate_eligible_links(
    repository: &Arc<RepositoryContext>,
    state_current: &Arc<State>,
    branch: BranchId,
    current_branch: BranchId,
    mount: Option<&Mount>,
) -> Result<Vec<EligibleLink>, MergeError> {
    let link_list = state_current
        .link_list(repository.clone())
        .await
        .forward::<MergeError>("listing links")?;

    let checked = link::try_join_links(
        link_list
            .into_iter()
            .map(async |link_reference| {
                let (link_path, link_node) = tokio::try_join!(
                    state_current.node_path(repository.clone(), link_reference.local_node),
                    state_current.node(repository.clone(), link_reference.local_node),
                )
                .forward::<MergeError>("reading a link")?;
                let link_context = repository
                    .to_link_context(link_node.address.context.into())
                    .await;
                let eligibility =
                    link::check_link_merge_eligible(&link_context, &link_reference, branch).await;
                let Ok(path) = RelativePath::from_str(&link_path);
                let eligible = EligibleLink {
                    mount: mount.and_then(|mount| mount.spell(&path)),
                    link_path,
                    link_node,
                    link_context,
                    link_reference,
                    resolved_branch: link_reference.resolve_branch(current_branch),
                };
                Ok::<_, MergeError>((eligible, eligibility))
            })
            .collect(),
    )
    .await?;

    let mut eligible_links = Vec::with_capacity(checked.len());
    for (eligible, eligibility) in checked {
        let reason = match eligibility {
            link::LinkMergeEligibility::Eligible => {
                eligible_links.push(eligible);
                continue;
            }
            link::LinkMergeEligibility::Skip => LinkMergeSkipReason::Inaccessible,
            link::LinkMergeEligibility::AutoFollowDisabled => {
                LinkMergeSkipReason::AutoFollowDisabled
            }
        };
        lore_debug!("Skipping link at {}: {reason:?}", eligible.shown_path());
        skip_link(&eligible, reason);
    }
    Ok(eligible_links)
}

/// Pre-populate `MergedLink`s for entries already in the staged
/// `LinkMergeState` (from an earlier pass) so the conflict / main-merge
/// tail re-applies their pins instead of regressing them to `state_current`'s
/// pre-merge values.
///
/// Drops from `link_merge_entries` each entry whose link the staged state still pins at its
/// pre-merge revision. A pass that failed after checkpointing a link and before staging its pin
/// leaves such an entry, and the caller merges that link again.
async fn seed_resumed_merged_links(
    repository: &Arc<RepositoryContext>,
    state_staged_opt: &Option<Arc<State>>,
    link_merge_entries: &mut Vec<LinkMergeEntry>,
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
    let staged_row = |entry: &LinkMergeEntry| {
        staged_link_list
            .iter()
            .find(|l| l.local_node == entry.local_node)
            .copied()
    };
    link_merge_entries.retain(|entry| {
        staged_row(entry).is_none_or(|staged_ref| staged_ref.signature != entry.base.signature)
    });
    for entry in link_merge_entries.iter() {
        let Some(staged_ref) = staged_row(entry) else {
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
        let link_context = repository
            .to_link_context(link_node.address.context.into())
            .await;
        merged_links.push(MergedLink {
            link_path,
            link_path_rel,
            link_node,
            link_context,
            // `stage_link_pin` reads the mounted path at the old signature,
            // so it is told the entry's recorded base.
            link_reference: entry.base,
            new_signature: staged_ref.signature,
            realize: link::LinkPinRealize::StateOnly,
        });
    }
    Ok(merged_links)
}

/// Reports each conflict left unresolved inside `merged_links`, the links merged on an earlier pass
/// of a merge on `current_branch`, files and link rows alike, and returns whether there is one.
async fn report_unresolved_link_conflicts(
    merged_links: &[MergedLink],
    current_branch: BranchId,
) -> Result<bool, MergeError> {
    let levels = link::try_join_links(
        merged_links
            .iter()
            .map(async |merged| {
                let link_state =
                    state::State::deserialize(merged.link_context.clone(), merged.new_signature)
                        .await
                        .forward::<MergeError>("deserializing resumed link state")?;
                merged_levels(
                    merged.link_context.clone(),
                    link_state,
                    merged.link_node.child,
                    merged.link_path_rel.clone(),
                    merged.link_reference.resolve_branch(current_branch),
                )
                .await
            })
            .collect(),
    )
    .await?;
    let mut any = false;
    for level in levels.into_iter().flatten() {
        let files = stage::collect_merge_paths(
            level.link_context.clone(),
            level.merged,
            level.node,
            level.mount,
            UNRESOLVED_CONFLICTS,
            None,
        )
        .await
        .forward::<MergeError>("collecting the merged link's conflicted paths")?
        .files;
        any |= !files.is_empty() || !level.unresolved.is_empty();
        for path in files {
            event::LoreEvent::BranchMergeConflictFile(LoreBranchMergeConflictFileEventData {
                path: LoreString::from(path.as_str()),
            })
            .send();
        }
        send_link_pin_conflicts(&level.link_context, level.branch, &level.unresolved).await;
    }
    Ok(any)
}

/// A repository a merge merged, at its mount, with the rows of its merge still unresolved.
struct MergedLevel {
    link_context: Arc<RepositoryContext>,
    merged: Arc<State>,
    node: NodeID,
    mount: RelativePath,
    /// The branch the repository's merge is on.
    branch: BranchId,
    unresolved: Vec<link::LinkPinConflict>,
}

/// `merged`, the merge of `link_context` on `branch` mounting its `node` at `mount`, and each link
/// nested in it that the merge merged too, at every depth.
async fn merged_levels(
    link_context: Arc<RepositoryContext>,
    merged: Arc<State>,
    node: NodeID,
    mount: RelativePath,
    branch: BranchId,
) -> Result<Vec<MergedLevel>, MergeError> {
    let mut levels = Vec::new();
    let mut pending = vec![(link_context, merged, node, mount, branch)];
    while let Some((link_context, merged, node, mount, branch)) = pending.pop() {
        let rows = merged_link_rows(&link_context, &merged, node, &mount).await?;
        pending.extend(rows.nested.into_iter().map(|nested| {
            (
                nested.link_context,
                nested.merged,
                nested.node,
                nested.mount,
                nested.base.resolve_branch(branch),
            )
        }));
        levels.push(MergedLevel {
            link_context,
            merged,
            node,
            mount,
            branch,
            unresolved: rows.unresolved,
        });
    }
    Ok(levels)
}

/// A link nested in a merged link that the merge merged too.
struct MergedNestedLink {
    link_context: Arc<RepositoryContext>,
    mount: RelativePath,
    /// The merge's staged state, with its node the mount materializes.
    merged: Arc<State>,
    node: NodeID,
    /// The link's node in the repository holding it.
    local_node: NodeID,
    /// The link's row before the merge.
    base: state::LinkReference,
}

/// The link rows of `merged`, a repository's merge mounting its `node` at `mount`.
#[derive(Default)]
struct MergedLinkRows {
    /// The links nested in it that the merge merged too.
    nested: Vec<MergedNestedLink>,
    /// The rows both sides moved that are still unresolved, named at their mounts.
    unresolved: Vec<link::LinkPinConflict>,
}

/// The link rows of `merged`, a repository's merge mounting its `node` at `mount`, as its link
/// merge state records them. A link the mount leaves out of the working tree is left out.
async fn merged_link_rows(
    link_context: &Arc<RepositoryContext>,
    merged: &Arc<State>,
    node: NodeID,
    mount: &RelativePath,
) -> Result<MergedLinkRows, MergeError> {
    let entries = merged
        .deserialize_link_merge_state(link_context.clone())
        .await
        .forward::<MergeError>("deserializing the merged link's link merge state")?;
    let mut rows = MergedLinkRows::default();
    if entries.is_empty() {
        return Ok(rows);
    }
    let mount = Mount::of_link(link_context, merged, node, mount.clone())
        .await
        .forward::<MergeError>("resolving the path the link exposes")?;
    for entry in entries {
        let (path, link_node) = tokio::try_join!(
            merged.node_path(link_context.clone(), entry.local_node),
            merged.node(link_context.clone(), entry.local_node),
        )
        .forward::<MergeError>("reading a nested link")?;
        let Ok(spelled) = RelativePath::from_str(&path);
        let Some(nested_mount) = mount.spell(&spelled) else {
            continue;
        };
        if entry.is_conflict() {
            if entry.is_unresolved() {
                rows.unresolved.push(link::LinkPinConflict {
                    link_path: nested_mount.to_string(),
                    source: entry.source,
                    source_mount: entry.source_mount,
                    target: entry.target,
                    target_mount: entry.target_mount,
                });
            }
            continue;
        }
        let nested_context = link_context.to_link_context(entry.base.repository).await;
        let nested_merged =
            state::State::deserialize(nested_context.clone(), link_node.address.hash)
                .await
                .forward::<MergeError>("deserializing the nested link's merge")?;
        rows.nested.push(MergedNestedLink {
            link_context: nested_context,
            mount: nested_mount,
            merged: nested_merged,
            node: link_node.child,
            local_node: entry.local_node,
            base: entry.base,
        });
    }
    Ok(rows)
}

/// Merge all eligible linked repositories, then the main repository
/// (`MergeScope::All`).
///
/// Reads top-down: enumerate eligible links, load existing
/// `LinkMergeState` for resume detection, seed `merged_links` for any
/// already-merged entries and merge again those whose pin was never staged,
/// check the main repository's merge against the working tree, skip the links
/// its diff removes, plan the merge of the following links it brings in, check
/// the merge of each remaining eligible link, write each link's merge in turn,
/// and finalize either as a merge-in-conflict (if any link conflicted) or by
/// writing the main repository's merge. A resumed merge whose main repository is merged
/// already is refused.
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

    let mut eligible_links = enumerate_eligible_links(
        &repository,
        &state_current,
        branch,
        current_branch,
        Some(&Mount::root()),
    )
    .await?;

    // A resumed merge compares the rows again, so it keeps only the links it merged.
    let mut link_merge_entries = if let Some(ref state_staged) = state_staged_opt {
        let mut entries = state_staged
            .deserialize_link_merge_state(repository.clone())
            .await
            .forward::<MergeError>("deserializing link merge state")?;
        entries.retain(|entry| !entry.is_conflict());
        entries
    } else {
        Vec::new()
    };

    let mut merged_links =
        seed_resumed_merged_links(&repository, &state_staged_opt, &mut link_merge_entries).await?;
    if let Some(state_staged) = &state_staged_opt {
        if report_unresolved_link_conflicts(&merged_links, current_branch).await? {
            return Ok(state_staged.revision());
        }
        // Only the main repository's merge gives the staged state a second parent of its own,
        // and the working tree holds what it wrote, which merging again would refuse as local
        // modifications.
        if state_staged.parent_other() != state_current.parent_other() {
            return Err(NotSupported {
                operation: "starting a merge whose main repository is merged already. Resolve \
                            its conflicts with `lore branch merge resolve` and commit it"
                    .into(),
            }
            .into());
        }
    }

    // The main repository's merge starts from `state_current` whatever the links' merges
    // write, so it is checked here, before any of theirs is written, and its diff decides which
    // links the merge removes and which it brings in.
    let main_merge = check_merge(
        &repository,
        branch,
        current_branch,
        state_current.revision(),
        Some(NodeMapping::root(repository.clone(), state_current.clone())),
        state_staged_opt.clone(),
    )
    .await?;
    let removed_links = links_the_merge_removes(&main_merge, &eligible_links);
    refuse_resumed_links_deleting_their_mount(&merged_links, &removed_links).await?;
    let (state_base, state_source) = tokio::try_join!(
        state::State::deserialize(repository.clone(), main_merge.diff.base),
        state::State::deserialize(repository.clone(), main_merge.diff.source),
    )
    .forward::<MergeError>("deserializing diff endpoint states")?;
    let incoming_links = plan_incoming_links(
        &repository,
        &state_current,
        &state_base,
        &state_source,
        branch,
        current_branch,
    )
    .await?;

    eligible_links.retain(|eligible| {
        // Resume: already merged on a prior pass.
        if link_merge_entries
            .iter()
            .any(|e| e.local_node == eligible.link_reference.local_node)
        {
            lore_debug!(
                "Resuming: skipping already-merged link at {}",
                eligible.link_path
            );
            return false;
        }
        kept_by_the_merge(eligible, &removed_links)
    });
    let planned_links = plan_link_merges(eligible_links, branch, &[repository.id]).await?;

    let mut left_out =
        links_the_diff_replaces(&repository, &state_current, &main_merge.diff).await?;
    left_out.extend(
        merged_links
            .iter()
            .map(|merged| merged.link_reference.local_node)
            .chain(
                planned_links
                    .iter()
                    .map(|planned| planned.eligible.link_reference.local_node),
            ),
    );
    check_link_pins(
        &repository,
        &state_current,
        current_branch,
        &state_base,
        &state_source,
        &left_out,
        Some(&Mount::root()),
    )
    .await?;

    let write = MergeWrite {
        token,
        inherit: &options.inherit_metadata,
        dry_run,
    };
    let mut nested = LinkMergeOutcome::default();
    for planned in planned_links {
        let WrittenLink {
            eligible,
            signature,
            outcome,
        } = write_link_merge(write, planned).await?;
        nested.absorb(outcome);

        if !dry_run {
            // We pin the parent at the rev-0 link signature. The eventual
            // `lore commit` runs `weave_history` on the link via
            // `commit_link_node`, bumps the revision number, and updates the
            // parent's pin to the post-bump signature in the same commit.

            let link_merge_entry = LinkMergeEntry::rollback(eligible.link_reference);
            link_merge_entries.push(link_merge_entry);

            let Ok(link_path_rel) = RelativePath::from_str(&eligible.link_path);
            merged_links.push(MergedLink {
                link_path: eligible.link_path,
                link_path_rel,
                link_node: eligible.link_node,
                link_context: eligible.link_context,
                link_reference: eligible.link_reference,
                new_signature: signature,
                realize: link::LinkPinRealize::WorkingTree,
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
            // input state. The next phase overwrites this checkpoint. A
            // resumed merge builds on its staged state, which keeps the pins
            // an earlier pass realized.
            let checkpoint = state::State::deserialize(
                repository.clone(),
                state_staged_opt
                    .as_ref()
                    .map_or(state_current.revision(), |staged| staged.revision()),
            )
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
    // unresolved link state, which is why a resumed merge stops above while a
    // link merged on an earlier pass still holds conflicts.
    if nested.has_conflicts && !dry_run {
        let signature = finalize_link_conflict_state(
            &repository,
            token,
            &state_current,
            &merged_links,
            &nested.pending_conflicts,
            &link_merge_entries,
        )
        .await?;
        create_nested_branches(&nested.missing_branches, branch).await?;
        return Ok(signature);
    }

    Box::pin(finalize_main_merge(
        repository,
        token,
        branch,
        options,
        current_branch,
        main_merge,
        &merged_links,
        &link_merge_entries,
        incoming_links,
        &nested,
        &state_base,
        &state_source,
        dry_run,
    ))
    .await
}

/// Reports `link` left out of the merge for `reason`.
fn skip_link(link: &EligibleLink, reason: LinkMergeSkipReason) {
    event::LoreEvent::BranchMergeLinkSkipped(LoreBranchMergeLinkSkippedEventData {
        link_path: LoreString::from(link.shown_path()),
        repository: link.link_context.id,
        reason: reason as u8,
    })
    .send();
}

/// Whether `eligible` stays, given `removed`, the links the merge of the repository holding it
/// removes. A removed link is reported.
fn kept_by_the_merge(eligible: &EligibleLink, removed: &HashSet<NodeID>) -> bool {
    let kept = !removed.contains(&eligible.link_reference.local_node);
    if !kept {
        lore_info!(
            "Skipping link at {}, which the merged branch removes",
            eligible.shown_path()
        );
    }
    kept
}

/// Checks the merge of `branch` into each of `eligible` against the working tree, with the merges
/// of the links nested in each, leaving out each link whose merge would change nothing. `holders`
/// are the repositories holding `eligible`, the main repository first.
///
/// Every merge is checked before any is written, so a refused one leaves no merge started. Each
/// link's diff is dropped once checked and worked out again from the same revisions to be
/// written, so only one link's diff is held at a time.
async fn plan_link_merges(
    eligible: Vec<EligibleLink>,
    branch: BranchId,
    holders: &[RepositoryId],
) -> Result<Vec<PlannedLinkMerge>, MergeError> {
    // `merge into --link` may have pushed to the remote without touching the local latest.
    let latest = link::try_join_links(
        eligible
            .iter()
            .map(|eligible| branch::load_latest_preferring_remote(&eligible.link_context, branch))
            .collect(),
    )
    .await
    .forward_any::<MergeError>("loading the merged branch's latest")?;
    let mut planned_links = Vec::with_capacity(eligible.len());
    for (eligible, source_latest) in eligible.into_iter().zip(latest) {
        if !source_latest.is_zero() && eligible.link_reference.signature == source_latest {
            lore_debug!(
                "Skipping link at {} — pin already matches source branch",
                eligible.shown_path()
            );
            skip_link(&eligible, LinkMergeSkipReason::AlreadyAtSource);
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
                eligible.shown_path()
            );
            skip_link(&eligible, LinkMergeSkipReason::NoContentDivergence);
            continue;
        }

        let link_state = state::State::deserialize(
            eligible.link_context.clone(),
            eligible.link_reference.signature,
        )
        .await
        .forward::<MergeError>("deserializing eligible link state")?;
        let source = link::link_source_path(
            eligible.link_context.clone(),
            &link_state,
            eligible.link_node.child,
        )
        .await
        .forward::<MergeError>("resolving the path the link exposes")?;
        let current = eligible.mount.clone().map(|path| NodeMapping {
            repository: eligible.link_context.clone(),
            state: link_state.clone(),
            path,
            node: eligible.link_node.child,
        });
        let checked = check_merge(
            &eligible.link_context,
            branch,
            eligible.resolved_branch,
            eligible.link_reference.signature,
            current.clone(),
            None,
        )
        .await
        .forward_with::<MergeError, _>(|| format!("merging link {}", eligible.shown_path()))?;
        refuse_deleting_the_mount(&eligible, &source, &checked)?;
        if eligible.mount.is_none() && !checked.diff.conflicts.is_empty() {
            return Err(NotSupported {
                operation: format!(
                    "merging the link at {}, which conflicts where the working tree holds none \
                     of it",
                    eligible.shown_path()
                ),
            }
            .into());
        }
        let merge_source = checked.source;
        let nested = Box::pin(plan_nested_link_merges(
            &eligible,
            checked,
            &link_state,
            source,
            branch,
            holders,
        ))
        .await?;
        planned_links.push(PlannedLinkMerge {
            eligible,
            source: merge_source,
            current,
            nested,
        });
    }
    Ok(planned_links)
}

/// Checks what `checked`, the merge of `link`'s repository from `link_state`, does to the links
/// that repository holds: the links its diff deletes are removed, the others are merged in their
/// own right, the links the merged branch adds are brought in, and the pins of the rest are
/// carried. A link to a repository among `holders`, the repositories holding `link`, and a link
/// past `MAX_LINK_DEPTH` keep only their carried pins. `source` is the subtree the link mounts.
/// The diff is dropped before the nested links' merges are checked.
async fn plan_nested_link_merges(
    link: &EligibleLink,
    checked: PreparedMerge,
    link_state: &Arc<State>,
    source: RelativePath,
    branch: BranchId,
    holders: &[RepositoryId],
) -> Result<Option<NestedLinkMerges>, MergeError> {
    let link_context = &link.link_context;
    let state_source = state::State::deserialize(link_context.clone(), checked.diff.source)
        .await
        .forward::<MergeError>("deserializing the link's diff source state")?;
    let (current_links, source_links) = tokio::try_join!(
        link_state.link_list(link_context.clone()),
        state_source.link_list(link_context.clone()),
    )
    .forward::<MergeError>("listing the link's links")?;
    if current_links.is_empty() && source_links.is_empty() {
        return Ok(None);
    }

    let at_limit = holders.len() >= state::MAX_LINK_DEPTH;
    let chain = [holders, &[link_context.id]].concat();
    let mount = link.mount.clone().map(|path| Mount { path, source });
    let mut eligible = if at_limit {
        Vec::new()
    } else {
        enumerate_eligible_links(
            link_context,
            link_state,
            branch,
            link.resolved_branch,
            mount.as_ref(),
        )
        .await?
    };
    let removed = links_the_merge_removes(&checked, &eligible);
    eligible.retain(|nested| {
        if chain.contains(&nested.link_context.id) {
            lore_debug!(
                "Skipping link at {}, back to a repository the merge is inside",
                nested.shown_path()
            );
            return false;
        }
        kept_by_the_merge(nested, &removed)
    });
    let mut left_out = links_the_diff_replaces(link_context, link_state, &checked.diff).await?;
    let base = checked.diff.base;
    drop(checked);

    let state_base = state::State::deserialize(link_context.clone(), base)
        .await
        .forward::<MergeError>("deserializing the link's diff base state")?;
    let incoming = if at_limit {
        Vec::new()
    } else {
        let mut incoming = plan_incoming_links(
            link_context,
            link_state,
            &state_base,
            &state_source,
            branch,
            link.resolved_branch,
        )
        .await?;
        incoming.retain(|incoming| !chain.contains(&incoming.link_context.id));
        incoming
    };
    let planned = plan_link_merges(eligible, branch, &chain).await?;
    left_out.extend(
        planned
            .iter()
            .map(|planned| planned.eligible.link_reference.local_node),
    );
    let conflicts = check_link_pins(
        link_context,
        link_state,
        link.resolved_branch,
        &state_base,
        &state_source,
        &left_out,
        mount.as_ref(),
    )
    .await?;
    // A divergent row is settled at its mount, which the working tree has to hold.
    if let Some(conflict) = conflicts.iter().find(|conflict| {
        let Ok(path) = RelativePath::from_str(&conflict.link_path);
        mount
            .as_ref()
            .is_none_or(|mount| mount.spell(&path).is_none())
    }) {
        return Err(NotSupported {
            operation: format!(
                "merging the link at {}, inside which both branches moved the pin of the link \
                 at {}, where the working tree holds none of it",
                link.shown_path(),
                conflict.link_path
            ),
        }
        .into());
    }
    Ok(Some(NestedLinkMerges {
        planned,
        incoming,
        mount,
        state_base,
        state_source,
    }))
}

/// What writing a merge's link merges takes.
#[derive(Clone, Copy)]
struct MergeWrite<'a> {
    token: &'a RepositoryWriteToken,
    inherit: &'a MetadataInherit,
    dry_run: bool,
}

/// What a link's merge, with the merges nested in it, leaves for the main repository's merge.
#[derive(Default)]
struct LinkMergeOutcome {
    has_conflicts: bool,
    /// Whether a row inside the link awaits the user's choice.
    unresolved_pins: bool,
    /// The conflicts to mark at their mounts once the main repository stages the pins.
    pending_conflicts: Vec<PendingConflictRealize>,
    /// Created once the merge can no longer fail.
    missing_branches: Vec<NestedMissingBranches>,
}

impl LinkMergeOutcome {
    fn absorb(&mut self, other: LinkMergeOutcome) {
        self.has_conflicts |= other.has_conflicts;
        self.unresolved_pins |= other.unresolved_pins;
        self.pending_conflicts.extend(other.pending_conflicts);
        self.missing_branches.extend(other.missing_branches);
    }
}

/// A link whose merge [`write_link_merge`] wrote.
struct WrittenLink {
    eligible: EligibleLink,
    /// The staged revision of the merge, which the link is pinned at.
    signature: Hash,
    outcome: LinkMergeOutcome,
}

/// Writes `planned`'s merge, and in it the merges of the links its repository holds.
async fn write_link_merge(
    write: MergeWrite<'_>,
    planned: PlannedLinkMerge,
) -> Result<WrittenLink, MergeError> {
    let PlannedLinkMerge {
        eligible,
        source,
        current,
        nested,
    } = planned;
    let link_context = &eligible.link_context;
    let merge = prepare_merge(link_context.clone(), source, current)
        .await
        .forward_with::<MergeError, _>(|| format!("merging link {}", eligible.shown_path()))?;
    let result = apply_prepared_merge(link_context.clone(), write.token, merge, write.inherit)
        .await
        .forward_with::<MergeError, _>(|| format!("merging link {}", eligible.shown_path()))?;

    let mut outcome = LinkMergeOutcome {
        has_conflicts: result.has_conflicts,
        ..LinkMergeOutcome::default()
    };
    // Defer marker realization until after `stage_link_pin` puts the
    // link's pre-merge content on disk; otherwise `stage_link_pin`'s
    // file-system verify rejects the on-disk size as locally
    // modified. The conflict event is deferred for the same reason
    // (see `finalize_link_conflict_state`).
    if !write.dry_run
        && let Some(context) = &result.conflict_context
        && let Some(mount) = &eligible.mount
    {
        outcome.pending_conflicts.push(PendingConflictRealize {
            link_context: link_context.clone(),
            link_path: mount.to_string(),
            link_node: eligible.link_node,
            context: context.clone(),
        });
    }

    let signature = match nested {
        Some(nested) => {
            let (signature, nested_outcome) =
                Box::pin(stage_nested(write, &eligible, &result, nested)).await?;
            outcome.absorb(nested_outcome);
            signature
        }
        None => result.signature,
    };
    Ok(WrittenLink {
        eligible,
        signature,
        outcome,
    })
}

/// Stages in `merged`, the merge of `link`'s repository, what `nested` plans for the links that
/// repository holds, returning the merge's revision and what the nested merges leave.
///
/// The pins are staged in the state alone: staging `link`'s pin realizes them in the working tree
/// with the rest of its merge.
async fn stage_nested(
    write: MergeWrite<'_>,
    link: &EligibleLink,
    merged: &MergeRepositoryResult,
    nested: NestedLinkMerges,
) -> Result<(Hash, LinkMergeOutcome), MergeError> {
    let link_context = &link.link_context;
    let state_staged = &merged.state_staged;
    let rows = link::LinkRows::read(state_staged, link_context.clone())
        .await
        .forward::<MergeError>("listing the merged link's links")?;
    let mut outcome = LinkMergeOutcome::default();
    let mut merged_nodes = Vec::with_capacity(nested.planned.len());
    let mut entries = Vec::with_capacity(nested.planned.len());
    let mut missing = Vec::new();
    for planned in nested.planned {
        let link_path = link::merged_link_path(
            link_context,
            &rows,
            planned.eligible.link_reference.local_node,
            planned.eligible.link_context.id,
            planned.eligible.resolved_branch,
            link.resolved_branch,
        )
        .await
        .forward_with::<MergeError, _>(|| {
            format!("locating the merged link {}", planned.eligible.shown_path())
        })?;
        let Some(link_path) = link_path else {
            lore_info!(
                "The merge removes or replaces link {}, which leaves out the merge of its \
                 repository",
                planned.eligible.shown_path()
            );
            continue;
        };
        let WrittenLink {
            eligible,
            signature,
            outcome: link_outcome,
        } = write_link_merge(write, planned).await?;
        outcome.absorb(link_outcome);
        merged_nodes.push(eligible.link_reference.local_node);
        entries.push(LinkMergeEntry::rollback(eligible.link_reference));
        if write.dry_run {
            continue;
        }
        link::stage_link_pin(
            link_context.clone(),
            state_staged,
            &eligible.link_context,
            link_path,
            eligible.link_node,
            eligible.link_reference.signature,
            signature,
            eligible.link_reference.branch,
            link::LinkMountSource::SamePath,
            link::LinkPinRealize::StateOnly,
        )
        .await
        .forward_with::<MergeError, _>(|| {
            format!("staging merged link pin for {}", eligible.shown_path())
        })?;
    }

    for incoming in &nested.incoming {
        let landed = landed_mounts(link_context, state_staged, incoming).await?;
        if landed.is_empty() {
            continue;
        }
        if let IncomingMerge::MissingBranch(pin) = incoming.merge {
            missing.push(missing_branch(incoming, &landed, pin));
        }
        let landed = land_incoming_links(
            link_context,
            write,
            state_staged,
            incoming,
            landed,
            link::LinkPinRealize::StateOnly,
        )
        .await?;
        merged_nodes.extend(landed.nodes);
        entries.extend(landed.entries);
        outcome.has_conflicts |= landed.has_conflicts;
        let Some(mount) = &nested.mount else {
            continue;
        };
        for mut pending in landed.conflicts {
            let Ok(path) = RelativePath::from_str(&pending.link_path);
            if let Some(path) = mount.spell(&path) {
                pending.link_path = path.to_string();
                outcome.pending_conflicts.push(pending);
            }
        }
    }
    if !missing.is_empty() {
        outcome.missing_branches.push(NestedMissingBranches {
            holder: link_context.clone(),
            holder_branch: link.resolved_branch,
            missing,
        });
    }

    let link::LinkPinClassification { planned, conflicts } = link::classify_link_pins(
        link_context.clone(),
        state_staged,
        link.resolved_branch,
        &nested.state_source,
        link::LinkPinResolution::ThreeWay(nested.state_base),
        &merged_nodes,
    )
    .await
    .forward::<MergeError>("comparing link pins with the merged branch")?;
    if let Some(mount) = &nested.mount {
        let spelled: Vec<link::LinkPinConflict> = conflicts
            .iter()
            .filter_map(|conflict| {
                let Ok(path) = RelativePath::from_str(&conflict.link_path);
                Some(link::LinkPinConflict {
                    link_path: mount.spell(&path)?.to_string(),
                    ..*conflict
                })
            })
            .collect();
        send_link_pin_conflicts(link_context, link.resolved_branch, &spelled).await;
    }
    outcome.unresolved_pins |= !conflicts.is_empty();
    if write.dry_run || (merged_nodes.is_empty() && planned.is_empty() && conflicts.is_empty()) {
        return Ok((merged.signature, outcome));
    }
    link::apply_link_pins(
        link_context.clone(),
        state_staged,
        planned,
        link::LinkPinRealize::StateOnly,
    )
    .await
    .forward::<MergeError>("carrying link pins from the merged branch")?;
    record_link_merge_state(link_context.clone(), state_staged, &entries, &conflicts).await?;
    let signature = state_staged
        .serialize(link_context.clone(), write.token)
        .await
        .forward::<MergeError>("serializing the merged link's staged state")?;
    Ok((signature, outcome))
}

/// The branches a nested merge needs for the links it brings in.
struct NestedMissingBranches {
    /// The repository holding the links, on `holder_branch`.
    holder: Arc<RepositoryContext>,
    holder_branch: BranchId,
    missing: Vec<MissingBranch>,
}

/// Creates the branches `missing`, needed by the links that nested merges of `merged_branch` bring
/// in.
async fn create_nested_branches(
    missing: &[NestedMissingBranches],
    merged_branch: BranchId,
) -> Result<(), MergeError> {
    for nested in missing {
        link::create_branches_for_merged_links(
            nested.holder.clone(),
            &nested.missing,
            nested.holder_branch,
            merged_branch,
        )
        .await
        .forward::<MergeError>("creating branches for merged links")?;
    }
    Ok(())
}

/// Refuses a merge resumed with a link among `merged_links`, merged on an earlier pass, whose merge
/// deletes the subtree it mounts or a directory above it, unless the merge removes the link
/// itself, as `removed_links` says.
async fn refuse_resumed_links_deleting_their_mount(
    merged_links: &[MergedLink],
    removed_links: &HashSet<NodeID>,
) -> Result<(), MergeError> {
    for merged in merged_links {
        if merged.link_node.child == ROOT_NODE
            || removed_links.contains(&merged.link_reference.local_node)
        {
            continue;
        }
        let link_state =
            state::State::deserialize(merged.link_context.clone(), merged.new_signature)
                .await
                .forward::<MergeError>("deserializing resumed link state")?;
        if commit::staged_for_deletion(&merged.link_context, &link_state, merged.link_node.child)
            .await
            .forward::<MergeError>("reading the resumed link's mount")?
        {
            let mounted = link::link_source_path(
                merged.link_context.clone(),
                &link_state,
                merged.link_node.child,
            )
            .await
            .forward::<MergeError>("resolving the path the link exposes")?;
            return Err(deleted_mount(&merged.link_path, &mounted));
        }
    }
    Ok(())
}

/// Refuses `merge` of `link`, which mounts `mounted`, where it deletes that subtree or a directory
/// above it.
fn refuse_deleting_the_mount(
    link: &EligibleLink,
    mounted: &RelativePath,
    merge: &PreparedMerge,
) -> Result<(), MergeError> {
    if link.link_node.child != ROOT_NODE
        && merge.diff.changes.iter().any(|change| {
            change.action == FileAction::Delete && mounted.below(change.path()).is_some()
        })
    {
        return Err(deleted_mount(link.shown_path(), mounted));
    }
    Ok(())
}

/// The refusal of a merge of the link at `link_path` that deletes `mounted`, the subtree it
/// mounts, or a directory above it: the link would mount nothing.
fn deleted_mount(link_path: &str, mounted: &RelativePath) -> MergeError {
    NotSupported {
        operation: format!(
            "merging the link at {link_path}, which mounts {mounted}, a path the merged branch \
             deletes"
        ),
    }
    .into()
}

/// The nodes of the links among `eligible` whose node `merge`'s diff deletes, as
/// [`link::merge_deletes_link`] decides it.
fn links_the_merge_removes(merge: &PreparedMerge, eligible: &[EligibleLink]) -> HashSet<NodeID> {
    eligible
        .iter()
        .filter(|link| {
            link::merge_deletes_link(&merge.diff.changes, &link.link_path, link.link_context.id)
        })
        .map(|link| link.link_reference.local_node)
        .collect()
}

/// Warns of each following link a merge of `state_source` into `state_target` brings in without
/// merging its repository, which a merge would.
async fn warn_of_unmerged_links(
    repository: &Arc<RepositoryContext>,
    state_target: &Arc<State>,
    state_base: &Arc<State>,
    state_source: &Arc<State>,
) -> Result<(), MergeError> {
    let (incoming, kept) = tokio::try_join!(
        link::incoming_following_links(repository, state_target, state_base, state_source),
        link::repositories_the_merge_keeps(repository, state_target, state_base, state_source),
    )
    .forward::<MergeError>("finding the links the merge brings in")?;
    for mount in incoming
        .iter()
        .filter(|(link_id, _)| !kept.contains_key(link_id))
        .flat_map(|(_, mounts)| mounts)
    {
        lore_warn!(
            "The merge brings in link {} without merging its repository, so a push of a commit \
             inside it fails. Merge without `--ignore-links` to merge it",
            mount.link_path
        );
    }
    Ok(())
}

/// The following links a merge brings in from one linked repository, with the merge of the
/// repository into the branch they land on.
struct IncomingLinks {
    link_context: Arc<RepositoryContext>,
    mounts: Vec<link::IncomingLink>,
    merge: IncomingMerge,
}

enum IncomingMerge {
    /// The links land at this revision of the branch, its latest unless the target keeps linking
    /// the repository elsewhere.
    Latest(Hash),
    Merge(MergeSource),
    /// The repository holds no branch for the links to land on, which the merge creates at this
    /// revision the links land at.
    MissingBranch(Hash),
}

/// The following links a merge of `state_source` into `state_target` brings in, with the merge of
/// each repository into `target_branch` they take: of the latest of `merged_branch`, or of the
/// links' pin where the repository holds no such branch. The links to a repository the target
/// keeps linking take no merge and land at the target's pin, as one repository's links share it.
async fn plan_incoming_links(
    repository: &Arc<RepositoryContext>,
    state_target: &Arc<State>,
    state_base: &Arc<State>,
    state_source: &Arc<State>,
    merged_branch: BranchId,
    target_branch: BranchId,
) -> Result<Vec<IncomingLinks>, MergeError> {
    let (incoming, kept) = tokio::try_join!(
        link::incoming_following_links(repository, state_target, state_base, state_source),
        link::repositories_the_merge_keeps(repository, state_target, state_base, state_source),
    )
    .forward::<MergeError>("finding the links the merge brings in")?;
    link::try_join_links(
        incoming
            .into_iter()
            .map(async |(link_id, mounts)| {
                let link_context = repository.to_link_context(link_id).await;
                if let Some(&pin) = kept.get(&link_id) {
                    let target_latest =
                        branch::load_latest_preferring_remote(&link_context, target_branch)
                            .await
                            .forward_any::<MergeError>("loading the linked branch's latest")?;
                    return Ok(IncomingLinks {
                        link_context,
                        mounts,
                        merge: if target_latest.is_zero() {
                            IncomingMerge::MissingBranch(pin)
                        } else {
                            IncomingMerge::Latest(pin)
                        },
                    });
                }
                let (target_latest, source_latest) = tokio::try_join!(
                    branch::load_latest_preferring_remote(&link_context, target_branch),
                    branch::load_latest_preferring_remote(&link_context, merged_branch),
                )
                .forward_any::<MergeError>("loading the linked branches' latest")?;
                let revision = if source_latest.is_zero() {
                    mounts[0].link_reference.signature
                } else {
                    source_latest
                };
                let merge = if target_latest.is_zero() {
                    IncomingMerge::MissingBranch(mounts[0].link_reference.signature)
                } else if revision == target_latest {
                    IncomingMerge::Latest(target_latest)
                } else {
                    IncomingMerge::Merge(
                        merge_source_at(
                            &link_context,
                            merged_branch,
                            revision,
                            target_branch,
                            target_latest,
                        )
                        .await?,
                    )
                };
                Ok(IncomingLinks {
                    link_context,
                    mounts,
                    merge,
                })
            })
            .collect(),
    )
    .await
}

/// The mounts of `incoming` that `state_staged`, the merge of the repository holding them, holds,
/// each with its node there.
async fn landed_mounts<'a>(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    incoming: &'a IncomingLinks,
) -> Result<Vec<(&'a link::IncomingLink, NodeID, Node)>, MergeError> {
    let mut landed = Vec::with_capacity(incoming.mounts.len());
    for mount in &incoming.mounts {
        match link::link_node_to(
            state_staged,
            repository,
            &mount.link_path,
            incoming.link_context.id,
        )
        .await
        .forward::<MergeError>("locating a link the merge brings in")?
        {
            Some((node_id, node)) => landed.push((mount, node_id, node)),
            None => lore_info!(
                "The merge replaces link {}, which leaves out the merge of its repository",
                mount.link_path
            ),
        }
    }
    Ok(landed)
}

/// A linked repository with no branch for the links a merge brings in to land on, with the
/// links.
type MissingBranch = (RepositoryId, Vec<link::IncomingLink>);

/// The branch `landed`, links a merge brings in, need, which the merge creates at `pin`, the
/// revision they land at.
fn missing_branch(
    incoming: &IncomingLinks,
    landed: &[(&link::IncomingLink, NodeID, Node)],
    pin: Hash,
) -> MissingBranch {
    (
        incoming.link_context.id,
        landed
            .iter()
            .map(|(mount, ..)| link::IncomingLink {
                link_path: mount.link_path.clone(),
                link_reference: state::LinkReference {
                    signature: pin,
                    ..mount.link_reference
                },
            })
            .collect(),
    )
}

/// Stages each of `landed`, links a merge brings in, at `pin`, the revision its repository's
/// merge lands at.
async fn pin_landed_mounts(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    link_context: &Arc<RepositoryContext>,
    landed: &[(&link::IncomingLink, NodeID, Node)],
    pin: Hash,
    realize: link::LinkPinRealize,
) -> Result<(), MergeError> {
    for (mount, _, node) in landed {
        if mount.link_reference.signature == pin {
            continue;
        }
        let link_path = RelativePath::from_str(&mount.link_path)
            .internal_with(|| format!("link not found: {}", mount.link_path))?;
        link::stage_link_pin(
            repository.clone(),
            state_staged,
            link_context,
            link_path,
            *node,
            mount.link_reference.signature,
            pin,
            BranchId::default(),
            link::LinkMountSource::SamePath,
            realize,
        )
        .await
        .forward_with::<MergeError, _>(|| format!("pinning merged link {}", mount.link_path))?;
    }
    Ok(())
}

/// What landing the links a merge brings in from one repository leaves to the rest of the merge.
struct LandedIncomingLinks {
    nodes: Vec<NodeID>,
    entries: Vec<LinkMergeEntry>,
    has_conflicts: bool,
    /// The conflicts the merge leaves in each link, named by the link's path in `state_staged`.
    conflicts: Vec<PendingConflictRealize>,
}

/// Merges the repository of `incoming` into the branch its links land on, and stages each link
/// `state_staged`, the merge of the repository holding them, holds at the result, realized as
/// `realize` says.
///
/// The merge holding the links realized each at its incoming pin, so the merge is staged from
/// there. Each link's merge entry records the latest the merge is staged on.
async fn land_incoming_links(
    repository: &Arc<RepositoryContext>,
    write: MergeWrite<'_>,
    state_staged: &Arc<State>,
    incoming: &IncomingLinks,
    landed: Vec<(&link::IncomingLink, NodeID, Node)>,
    realize: link::LinkPinRealize,
) -> Result<LandedIncomingLinks, MergeError> {
    let link_context = &incoming.link_context;
    let (pin, merged) = match incoming.merge {
        IncomingMerge::Latest(pin) | IncomingMerge::MissingBranch(pin) => (pin, None),
        IncomingMerge::Merge(source) => {
            let merge = prepare_merge(link_context.clone(), source, None).await?;
            if merge.diff.is_empty() {
                (source.current_signature, None)
            } else {
                let result =
                    apply_prepared_merge(link_context.clone(), write.token, merge, write.inherit)
                        .await
                        .forward_with::<MergeError, _>(|| {
                            format!("merging link {}", incoming.mounts[0].link_path)
                        })?;
                (result.signature, Some((source.current_signature, result)))
            }
        }
    };

    let mut landed_links = LandedIncomingLinks {
        nodes: landed.iter().map(|(_, node_id, _)| *node_id).collect(),
        entries: Vec::new(),
        has_conflicts: merged
            .as_ref()
            .is_some_and(|(_, result)| result.has_conflicts),
        conflicts: Vec::new(),
    };
    if write.dry_run {
        return Ok(landed_links);
    }
    pin_landed_mounts(
        repository,
        state_staged,
        link_context,
        &landed,
        pin,
        realize,
    )
    .await?;
    let Some((staged_on, result)) = &merged else {
        return Ok(landed_links);
    };
    for (mount, node_id, node) in landed {
        landed_links
            .entries
            .push(LinkMergeEntry::rollback(state::LinkReference {
                local_node: node_id,
                signature: *staged_on,
                ..mount.link_reference
            }));
        if let Some(context) = &result.conflict_context {
            landed_links.conflicts.push(PendingConflictRealize {
                link_context: link_context.clone(),
                link_path: mount.link_path.clone(),
                link_node: node,
                context: context.clone(),
            });
        }
    }
    Ok(landed_links)
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
            link::LinkMountSource::SamePath,
            merged.realize,
        )
        .await
        .forward_with::<MergeError, _>(|| {
            format!("staging conflict link pin for {}", merged.link_path)
        })?;
    }

    // The markers are written over what `stage_link_pin` realized at each mount.
    for pending in pending_conflict_realizes {
        realize_link_conflicts(pending).await?;
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

/// The path `side` mounts, a node of the revision it pins, and that revision's message, each empty
/// when the linked repository cannot be read — a conflict stays reportable against a repository
/// the user is offline from.
async fn link_pin_side(
    repository: &Arc<RepositoryContext>,
    parent_branch: BranchId,
    side: &LinkReference,
    mount: NodeID,
) -> LoreLinkPinSideData {
    let link_context = repository.to_link_context(side.repository()).await;
    let (source_path, message) =
        match State::deserialize(link_context.clone(), side.signature()).await {
            Ok(state) => {
                let (source_path, metadata) = join!(
                    link::link_source_path(link_context.clone(), &state, mount),
                    state.revision_metadata(link_context.clone()),
                );
                (
                    source_path
                        .map_or_default(|path| LoreString::from(link::display_path(path.as_str()))),
                    metadata.map_or_default(|metadata| LoreString::from(metadata.message.as_str())),
                )
            }
            Err(_) => (LoreString::default(), LoreString::default()),
        };

    LoreLinkPinSideData {
        branch: side.resolve_branch(parent_branch),
        revision: side.signature(),
        flags: side.flags(),
        source_path,
        message,
    }
}

/// Send an event for each of `conflicts`, whose rows are as a parent on `parent_branch` stores
/// them. Each side reads its linked repository, possibly from the remote, so the reads overlap.
async fn send_link_pin_conflicts(
    repository: &Arc<RepositoryContext>,
    parent_branch: BranchId,
    conflicts: &[link::LinkPinConflict],
) {
    let rows: Vec<_> = conflicts
        .iter()
        .map(|conflict| {
            (
                repository.clone(),
                LoreString::from(conflict.link_path.as_str()),
                (conflict.source, conflict.source_mount),
                (conflict.target, conflict.target_mount),
            )
        })
        .collect();
    let mut events = futures::stream::iter(rows)
        .map(|(repository, link_path, source, target)| async move {
            let (mine, theirs) = join!(
                link_pin_side(&repository, parent_branch, &target.0, target.1),
                link_pin_side(&repository, parent_branch, &source.0, source.1)
            );
            LoreBranchMergeLinkPinConflictEventData {
                link_path,
                repository: target.0.repository(),
                mine,
                theirs,
            }
        })
        .buffered(link::MAX_CONCURRENT_LINK_TASKS);

    while let Some(event) = events.next().await {
        event::LoreEvent::BranchMergeLinkPinConflict(event).send();
    }
}

/// Persist the merge's link rows: the pre-merge snapshots abort rolls back, and
/// the divergent rows `merge resolve` settles.
async fn record_link_merge_state(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    rollback_entries: &[LinkMergeEntry],
    conflicts: &[link::LinkPinConflict],
) -> Result<(), MergeError> {
    if !conflicts.is_empty() {
        state_staged.set_conflict();
    }

    let entries: Vec<LinkMergeEntry> = rollback_entries
        .iter()
        .copied()
        .chain(conflicts.iter().map(|conflict| {
            LinkMergeEntry::conflict(
                conflict.source,
                conflict.source_mount,
                conflict.target,
                conflict.target_mount,
            )
        }))
        .collect();

    if entries.is_empty() {
        state_staged.clear_link_merge_state();
        return Ok(());
    }

    state_staged
        .serialize_link_merge_state(repository, &entries)
        .await
        .forward::<MergeError>("serializing link merge state")?;
    Ok(())
}

/// Writes the conflict markers and sidecars of `pending` at the mount of its link, once its pin
/// is staged, and reports each conflicted file.
///
/// The link's `RepositoryContext` shares its `path` with the parent, so absolute paths resolve
/// below the mount while state block lookups stay in the link.
async fn realize_link_conflicts(pending: &PendingConflictRealize) -> Result<(), MergeError> {
    let mount_path = RelativePath::from_str(&pending.link_path)
        .internal_with(|| format!("link not found: {}", pending.link_path))?;
    let conflicts = Arc::new(mount_conflicts(pending, &mount_path).await?);
    sync::realize_conflicts(
        pending.link_context.clone(),
        pending.context.state_base.clone(),
        pending.context.state_from.clone(),
        pending.context.state_to.clone(),
        None, // staging already happened inside apply_diff for the link state
        conflicts.clone(),
        false,
        Arc::new(sync::SyncRealizeStats::default()),
        MergeType::BranchMerge,
    )
    .await
    .forward::<MergeError>("realizing link conflicts")?;

    for (from, _to) in conflicts.iter() {
        event::LoreEvent::BranchMergeConflictFile(LoreBranchMergeConflictFileEventData {
            path: LoreString::from(from.path().as_str()),
        })
        .send();
    }
    Ok(())
}

/// Finalize the parent merge: apply `main_merge`, apply collected link pin
/// updates to its staged state, persist, and auto-commit if no conflicts.
///
/// A link merged in its own right takes the pin its merge produced where the merged state holds
/// its node, at the path a move of the node, as a rename in case, or of a directory above it
/// carries it to; the pin of one the merge removes or replaces is dropped. Among those is a link
/// removed inside an out-of-view subtree the merge adopts whole, which the diff sets apart: its
/// repository is merged first, and that merge is left out here. A per-link message for a link the
/// merge removes is left out of the commit the merge makes.
///
/// The repository of each following link the merge brings in is merged into the branch the link
/// lands on, and the link staged at the result, with its conflicts realized in the mount.
#[allow(clippy::too_many_arguments)]
async fn finalize_main_merge(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    branch: BranchId,
    mut options: MergeStartOptions,
    current_branch: BranchId,
    main_merge: PreparedMerge,
    merged_links: &[MergedLink],
    link_merge_entries: &[LinkMergeEntry],
    incoming_links: Vec<IncomingLinks>,
    nested: &LinkMergeOutcome,
    state_base: &Arc<State>,
    state_source: &Arc<State>,
    dry_run: bool,
) -> Result<Hash, MergeError> {
    let MergeRepositoryResult {
        signature: _,
        mut has_conflicts,
        state_staged,
        ..
    } = apply_prepared_merge(
        repository.clone(),
        token,
        main_merge,
        &options.inherit_metadata,
    )
    .await?;

    let staged_links = link::LinkRows::read(&state_staged, repository.clone())
        .await
        .forward::<MergeError>("listing the merged links")?;
    let mut merged_nodes: Vec<NodeID> = Vec::with_capacity(merged_links.len());
    let mut left_out: HashSet<NodeID> = HashSet::new();
    for merged in merged_links {
        let link_path = link::merged_link_path(
            &repository,
            &staged_links,
            merged.link_reference.local_node,
            merged.link_context.id,
            merged.link_reference.resolve_branch(current_branch),
            current_branch,
        )
        .await
        .forward_with::<MergeError, _>(|| {
            format!("locating the merged link {}", merged.link_path)
        })?;
        let Some(link_path) = link_path else {
            lore_info!(
                "The merge removes or replaces link {}, which leaves out the merge of its \
                 repository",
                merged.link_path
            );
            left_out.insert(merged.link_reference.local_node);
            continue;
        };
        merged_nodes.push(merged.link_reference.local_node);
        if !dry_run {
            link::stage_link_pin(
                repository.clone(),
                &state_staged,
                &merged.link_context,
                link_path,
                merged.link_node,
                merged.link_reference.signature,
                merged.new_signature,
                merged.link_reference.branch,
                link::LinkMountSource::SamePath,
                merged.realize,
            )
            .await
            .forward_with::<MergeError, _>(|| {
                format!("staging merged link pin for {}", merged.link_path)
            })?;
        }
    }
    // A link the merge removes or replaces keeps no row for the link merge state to restore it
    // through, and the parent's abort restores it.
    let mut link_merge_entries: Vec<LinkMergeEntry> = link_merge_entries
        .iter()
        .filter(|entry| !left_out.contains(&entry.local_node))
        .copied()
        .collect();

    let mut missing_branches = Vec::new();
    for incoming in &incoming_links {
        let landed = landed_mounts(&repository, &state_staged, incoming).await?;
        if landed.is_empty() {
            continue;
        }
        if let IncomingMerge::MissingBranch(pin) = incoming.merge {
            missing_branches.push(missing_branch(incoming, &landed, pin));
        }
        let landed = land_incoming_links(
            &repository,
            MergeWrite {
                token,
                inherit: &options.inherit_metadata,
                dry_run,
            },
            &state_staged,
            incoming,
            landed,
            link::LinkPinRealize::WorkingTree,
        )
        .await?;
        for pending in &landed.conflicts {
            realize_link_conflicts(pending).await?;
        }
        merged_nodes.extend(landed.nodes);
        link_merge_entries.extend(landed.entries);
        has_conflicts |= landed.has_conflicts;
    }
    if has_conflicts && !dry_run {
        state_staged.set_merge_conflict();
    }

    // Links merged above already carry the pin their own merge produced. The
    // comparison runs on a dry run too, so `--dry-run` reports a row the real
    // merge would stop on.
    let classification = link::classify_link_pins(
        repository.clone(),
        &state_staged,
        current_branch,
        state_source,
        link::LinkPinResolution::ThreeWay(state_base.clone()),
        &merged_nodes,
    )
    .await
    .forward::<MergeError>("comparing link pins with the merged branch")?;
    let link::LinkPinClassification { planned, conflicts } = classification;
    let conflicts: Vec<link::LinkPinConflict> = conflicts
        .into_iter()
        .filter(|conflict| !left_out.contains(&conflict.target.local_node))
        .collect();
    let unresolved_pins = !conflicts.is_empty() || nested.unresolved_pins;
    send_link_pin_conflicts(&repository, current_branch, &conflicts).await;

    if !dry_run {
        link::apply_link_pins(
            repository.clone(),
            &state_staged,
            planned,
            link::LinkPinRealize::WorkingTree,
        )
        .await
        .forward::<MergeError>("carrying link pins from the merged branch")?;

        if nested.unresolved_pins {
            state_staged.set_conflict();
        }
        record_link_merge_state(
            repository.clone(),
            &state_staged,
            &link_merge_entries,
            &conflicts,
        )
        .await?;

        let signature = state_staged
            .serialize(repository.clone(), token)
            .await
            .forward::<MergeError>("serializing staged state")?;
        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<MergeError>("storing staged anchor")?;

        // Linked repositories are changed only once the merge cannot fail above, so a refused
        // merge leaves no branch behind.
        link::create_branches_for_merged_links(
            repository.clone(),
            &missing_branches,
            current_branch,
            branch,
        )
        .await
        .forward::<MergeError>("creating branches for merged links")?;
        create_nested_branches(&nested.missing_branches, branch).await?;
    }

    if !has_conflicts && !unresolved_pins && !dry_run && !options.no_commit {
        if !options.link_messages.is_empty() {
            Box::pin(leave_out_messages_of_links_gone(
                &repository,
                &state_staged,
                &mut options.link_messages,
            ))
            .await?;
        }
        return auto_commit_merge(repository, token, options).await;
    }

    Ok(state_staged.revision())
}

/// Leaves out of `link_messages` each message whose path `state_staged`, the merge's result,
/// holds no link at, as for a link the merge removes, which the commit the merge makes would
/// refuse. A cold path its caller boxes, taken only where per-link messages were given.
async fn leave_out_messages_of_links_gone(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    link_messages: &mut std::collections::HashMap<String, String>,
) -> Result<(), MergeError> {
    for (path, message) in std::mem::take(link_messages) {
        if link::holds_link_at(state_staged, repository, &path)
            .await
            .forward::<MergeError>("resolving a link message path")?
        {
            link_messages.insert(path, message);
        } else {
            lore_info!("Leaving out the message for link {path}, which the merge removes");
        }
    }
    Ok(())
}

async fn validate_link_messages(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    options: &MergeStartOptions,
) -> Result<(), MergeError> {
    if options.link_messages.is_empty() {
        return Ok(());
    }
    if matches!(options.scope, MergeScope::MainOnly) {
        return Err(InvalidArguments {
            reason: "Per-link messages need a merge that includes linked repositories".into(),
        }
        .into());
    }
    link::validate_link_message_paths(state, repository, options.link_messages.keys())
        .await
        .forward::<MergeError>("validating link message paths")?;
    Ok(())
}

/// Auto-commit a merge with no conflicts.
async fn auto_commit_merge(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: MergeStartOptions,
) -> Result<Hash, MergeError> {
    let commit_options = CommitOptions {
        message: options.message,
        link_messages: options.link_messages,
        link: None,
        layer_messages: std::collections::HashMap::new(),
        layer: None,
    };
    commit::commit_boxed(repository, token, commit_options)
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

impl MergeType {
    /// The command that aborts this merge, cherry-pick or revert.
    pub(crate) fn abort_command(self) -> &'static str {
        match self {
            MergeType::CherryPick => "lore revision cherry-pick abort",
            MergeType::Revert => "lore revision revert abort",
            MergeType::None | MergeType::BranchMerge => "lore branch merge abort",
        }
    }
}

/// The same repository with no view filter, so tree operations reach every
/// node.
fn full_tree_context(repository: &Arc<RepositoryContext>) -> Arc<RepositoryContext> {
    Arc::new(repository.to_filter_context(Arc::new(Filter::default())))
}

/// Node counts from reconciling one grafted subtree.
#[derive(Default, Clone, Copy)]
struct GraftCounts {
    overwritten: usize,
    added: usize,
    moved: usize,
    deleted: usize,
}

impl GraftCounts {
    fn touched(&self) -> usize {
        self.overwritten + self.added + self.moved + self.deleted
    }
}

/// Copy a node's file metadata from source.
///
/// `merge_metadata` covers per-file changes. A graft emits one change for the
/// whole subtree, so the nodes below it are copied here instead. Without this
/// the staged state differs from a full-tree merge of the same content.
async fn graft_copy_file_metadata(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    source_repository: Arc<RepositoryContext>,
    state_source: Arc<State>,
    source_id: NodeID,
    staged_id: NodeID,
) -> Result<(), MergeError> {
    let source_metadata_node = node::node_to_file_metadata(source_id);
    let source_block = state_source
        .block_file_metadata(
            source_repository,
            NodeFileMetadataBlock::index(source_metadata_node),
        )
        .await
        .forward::<MergeError>("deserializing grafted source metadata block")?;
    let metadata_hash = {
        let block_reader = source_block.read();
        block_reader
            .node(NodeFileMetadata::index(source_metadata_node))
            .metadata
    };

    let staged_metadata_node = node::node_to_file_metadata(staged_id);
    let block_index = NodeFileMetadataBlock::index(staged_metadata_node);
    let staged_block = state_staged
        .block_file_metadata(repository, block_index)
        .await
        .forward::<MergeError>("deserializing grafted staged metadata block")?;
    let dirtied = {
        let mut block_writer = staged_block.write();
        let node = block_writer.node(NodeFileMetadata::index(staged_metadata_node));
        node.metadata = metadata_hash;
        block_writer.mark_dirty()
    };
    if dirtied {
        state_staged.block_file_metadata_modified(staged_block, block_index);
    }
    Ok(())
}

/// Point an existing staged node at source's content, in place.
///
/// An overwrite keeps the node's ID, slot and sibling position, and does not
/// take the single-permit slot allocator that `node_add` holds.
#[lore_macro::test_pub]
async fn graft_overwrite_node(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    staged_id: NodeID,
    source_node: &Node,
) -> Result<(), MergeError> {
    let block_index = NodeBlock::index(staged_id);
    let node_index = Node::index(staged_id);
    let block = state_staged
        .block(repository.clone(), block_index)
        .await
        .forward::<MergeError>("loading graft overwrite block")?;
    // A link overwritten by a link takes the source's whole pin, its node in the linked State
    // included (H17); any other node the source's content.
    let (place, link, resized) = {
        let staged = block.node(node_index);
        (
            (staged.parent, staged.name_hash),
            staged.is_link() && source_node.is_link(),
            (staged.size, staged.mode) != (source_node.size, source_node.mode),
        )
    };
    if link {
        state_staged
            .node_set_link_target_in(
                &block,
                staged_id,
                LinkPin::Whole {
                    address: source_node.address,
                    child: source_node.child,
                },
            )
            .forward::<MergeError>("repinning grafted link")?;
        if resized {
            state_staged.node_change_in_place(&block, staged_id, place, |held| {
                held.set_size(source_node.size);
                held.set_mode(source_node.mode);
                true
            });
        }
    } else {
        state_staged.node_change_in_place(&block, staged_id, place, |held| {
            held.overwrite_content(source_node.address, source_node.size, source_node.mode);
            true
        });
    }

    // `StagedMerge` holds the `Staged` bit and no action bit. That is what a
    // per-file merge leaves on a node whose content came from source, and the
    // staged state must match it. `node_mark` also marks the parent
    // directories, so the commit recomputes them.
    state_staged
        .node_mark(
            &repository,
            staged_id,
            Mark::staged(NodeFlags::StagedMerge, true),
        )
        .await
        .forward::<MergeError>("staging grafted node")?;
    Ok(())
}

/// Stage `directory` and the directories above it `StagedMerge`, up to the first
/// that already carries it, as a per-file merge stages the ancestors of an add or
/// a move. The root takes `Staged` only, as there.
async fn graft_stage_ancestors(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    mut directory: NodeID,
) -> Result<(), MergeError> {
    while directory != ROOT_NODE {
        let node = state_staged
            .node(repository.clone(), directory)
            .await
            .forward::<MergeError>("resolving adopted node ancestor")?;
        if node.is_staged_merge() {
            return Ok(());
        }
        state_staged
            .node_mark(
                &repository,
                directory,
                Mark::staged(NodeFlags::StagedMerge, false),
            )
            .await
            .forward::<MergeError>("staging adopted node ancestor")?;
        directory = node.parent;
    }
    state_staged
        .node_mark(
            &repository,
            ROOT_NODE,
            Mark::staged(NodeFlags::Staged, false),
        )
        .await
        .forward::<MergeError>("staging adopted node ancestor")
}

/// Create a node at `path` in the staged tree, under an existing parent, staged
/// as a per-file merge stages an add: `StagedAdd` and `StagedMerge` for a file, a
/// link or a directory, and the ancestors staged by [`graft_stage_ancestors`].
///
/// Adds arrive in path order, so the parent is already there.
async fn graft_add_node(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    path: &RelativePath,
    source_node: &Node,
) -> Result<NodeID, MergeError> {
    let mut parent_path = path.clone();
    let parent_path = parent_path.pop();
    let parent = state_staged
        .find_node_link(repository.clone(), parent_path.as_str())
        .await
        .forward::<MergeError>("resolving adopted node parent")?;
    if !parent.is_valid_or_root() {
        return Err(MergeError::internal("Invalid adopted node parent"));
    }

    let mut node = Node {
        name_hash: source_node.name_hash,
        mode: source_node.mode,
        size: source_node.size,
        address: source_node.address,
        flags: source_node.flags,
        ..Default::default()
    };
    node.take_mount_source(source_node);
    // Source's nodes are committed, so they bring no staged or dirty state.
    node.clear_all_change_flags();
    node.flags |= (NodeFlags::StagedAdd | NodeFlags::StagedMerge).bits();

    let node_id = state_staged
        .node_add(repository.clone(), parent.node, node, path.name())
        .await
        .forward::<MergeError>("adding adopted node")?;
    graft_stage_ancestors(repository, state_staged, parent.node).await?;
    Ok(node_id)
}

/// Move the staged node `change` moves to the path it adopts and point it at
/// source's content, staged as a per-file merge stages a move: relinked with
/// `StagedMove` and `StagedMerge`, and the new ancestors staged by
/// [`graft_stage_ancestors`]. The overwrite before the relink marks the old
/// ancestors staged, so the commit recomputes them too.
async fn graft_move_node(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    change: &NodeChange,
    source_node: &Node,
) -> Result<NodeID, MergeError> {
    let from = change.from.mapping.node;
    graft_overwrite_node(repository.clone(), state_staged.clone(), from, source_node).await?;
    let parent = crate::fs::realize::relink_moved_node(
        &state_staged,
        &repository,
        &change.from.mapping.path,
        from,
        change.path(),
        true,
    )
    .await
    .forward::<MergeError>("relinking adopted move")?;
    graft_stage_ancestors(repository, state_staged, parent).await?;
    Ok(from)
}

/// Drop each link the reconcile deletes from the staged link registry, as a
/// per-file merge does. This runs before the adds, so a link the subtree moves
/// to a new path and a new pin can be recorded at the new path.
async fn graft_unregister_deleted_links(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    changes: &[NodeChange],
) -> Result<(), MergeError> {
    for deleted in changes
        .iter()
        .filter(|change| change.action == FileAction::Delete && change.from.is_link())
    {
        state_staged
            .link_remove_if_present(
                repository.clone(),
                deleted.from.address.context.into(),
                deleted.from.mapping.node,
            )
            .await
            .forward::<MergeError>("removing adopted link")?;
    }
    Ok(())
}

/// Adopt source's version of an out-of-view subtree.
///
/// Only valid for a subtree the view excludes. Realization skips those paths
/// (`fs/realize.rs`) and commit adopts their staged hash (`commit.rs`), so the
/// merge is a tree operation only and the files inside need no three-way
/// merge. Each add, move, overwrite and delete takes the staged flags a per-file
/// merge gives it, which mark its parent directories, so the commit discards the
/// deletes, recomputes every directory the reconcile changed, and records the
/// changes in the revision delta as a merge with no view does.
///
/// An add and a delete of one file pair into a move by
/// [`state::detect_and_coalesce_moves`], as the diff of a merge with no view pairs
/// them. An add creates its node: a type change is a delete and an add at one path,
/// so the add sits beside the node of the other kind, which the delete stages for
/// delete, as staging a type change does. Every other change acts on the staged node
/// the walk reports it for, since after a type change its path resolves to the added
/// node.
///
/// A link is reconciled as a node and never descended. One that pins another
/// revision of the same subtree is left to the merge's pass over the link pins, as
/// in a merge with no view.
async fn apply_graft_copy(
    repository: Arc<RepositoryContext>,
    state_staged: Arc<State>,
    change: &NodeChange,
) -> Result<usize, MergeError> {
    let link = state_staged
        .find_node_link(repository.clone(), change.path().as_str())
        .await
        .forward::<MergeError>("resolving graft path")?;
    if !link.is_valid_or_root() {
        return Err(MergeError::internal("Invalid graft path"));
    }

    // Diff the staged subtree against source's to find what differs. The walk
    // pairs children by name and stops at a directory whose address already
    // matches, so the result is proportional to the difference rather than to
    // the size of the subtree. No oracle here, because this walk has to descend.
    let path = change.path().clone();
    let staged =
        state::node_change_state(&repository, &state_staged, link.node, path.clone()).await;
    let source = change.to.clone();
    let mut changes = state::ChangeStream::spawn(async move |changes| {
        state::diff_subtree_flagged(
            staged,
            source,
            path,
            None,
            state::DiffFlags::LeavesLinkPins,
            &changes,
            FilterMode::empty(),
        )
        .await
    })
    .collect()
    .await
    .forward::<MergeError>("diffing the adopted subtree")?;
    change::sort_by_path(&mut changes);
    state::detect_and_coalesce_moves(&mut changes);
    graft_unregister_deleted_links(&repository, &state_staged, &changes).await?;

    let mut counts = GraftCounts::default();

    // Adds, moves and overwrites in path order, so a parent exists before its
    // children.
    for adopted in changes.iter() {
        if adopted.action == FileAction::Delete {
            continue;
        }
        let source_node = adopted
            .to
            .mapping
            .state
            .node(
                adopted.to.mapping.repository.clone(),
                adopted.to.mapping.node,
            )
            .await
            .forward::<MergeError>("resolving adopted node")?;

        let staged_id = if adopted.move_source().is_some() {
            counts.moved += 1;
            graft_move_node(
                repository.clone(),
                state_staged.clone(),
                adopted,
                &source_node,
            )
            .await?
        } else if adopted.action == FileAction::Add {
            counts.added += 1;
            graft_add_node(
                repository.clone(),
                state_staged.clone(),
                adopted.path(),
                &source_node,
            )
            .await?
        } else {
            graft_overwrite_node(
                repository.clone(),
                state_staged.clone(),
                adopted.from.mapping.node,
                &source_node,
            )
            .await?;
            counts.overwritten += 1;
            adopted.from.mapping.node
        };

        if source_node.is_link() {
            crate::fs::realize::stage_link_registry_entry(
                &repository,
                &state_staged,
                adopted,
                source_node,
                staged_id,
            )
            .await
            .forward::<MergeError>("registering adopted link")?;
        }

        graft_copy_file_metadata(
            repository.clone(),
            state_staged.clone(),
            adopted.to.mapping.repository.clone(),
            adopted.to.mapping.state.clone(),
            adopted.to.mapping.node,
            staged_id,
        )
        .await?;
    }

    for adopted in changes.iter() {
        if adopted.action != FileAction::Delete {
            continue;
        }
        state_staged
            .node_mark(
                &repository,
                adopted.from.mapping.node,
                Mark::staged(NodeFlags::StagedDelete | NodeFlags::StagedMerge, true),
            )
            .await
            .forward::<MergeError>("staging adopted delete")?;
        counts.deleted += 1;
    }

    Ok(counts.touched())
}

/// Check every path the diff touches against the working copy, refusing the merge where a
/// local modification would be overwritten.
///
/// A linked repository's diff is spelled from its own root, so each change is verified where
/// `current` stands, below the link's mount. A change outside the subtree the link exposes has
/// nothing on disk to verify. `resumed` is the staged state of the merge this one resumes.
async fn verify_diff_against_filesystem(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    current: &NodeMapping,
    resumed: Option<&Arc<State>>,
    changes: &mut [NodeChange],
    conflicts: &mut [(NodeChange, NodeChange)],
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let conflicts = conflicts.iter_mut().map(|(_, change_to)| change_to);
    if current.path.is_empty() {
        return verify_spelled_diff(
            operation,
            repository,
            current,
            resumed,
            changes.iter_mut().collect(),
            conflicts.collect(),
            merge_type,
        )
        .await;
    }

    let mount = Mount::of_link(
        repository,
        &current.state,
        current.node,
        current.path.clone(),
    )
    .await
    .forward::<MergeError>("resolving the path the link exposes")?;
    let mut mounted_changes = spell_from_mount(changes.iter_mut(), &mount);
    let mut mounted_conflicts = spell_from_mount(conflicts, &mount);
    verify_spelled_diff(
        operation,
        repository,
        current,
        resumed,
        mounted_changes
            .iter_mut()
            .map(|(_, mounted)| mounted)
            .collect(),
        mounted_conflicts
            .iter_mut()
            .map(|(_, mounted)| mounted)
            .collect(),
        merge_type,
    )
    .await?;
    for (change, mounted) in mounted_changes.into_iter().chain(mounted_conflicts) {
        change.flags = mounted.flags;
    }
    Ok(())
}

/// Each of `changes` the subtree `mount` materializes holds, paired with a copy spelled from the
/// mount.
fn spell_from_mount<'a>(
    changes: impl Iterator<Item = &'a mut NodeChange>,
    mount: &Mount,
) -> Vec<(&'a mut NodeChange, NodeChange)> {
    changes
        .filter(|change| change.path().below(&mount.source).is_some())
        .map(|change| {
            let mut mounted = change.clone();
            mounted.from.mapping.path = match change.action {
                FileAction::Move => change.move_source().and_then(|path| mount.spell(path)),
                _ => mount.spell(&change.from.mapping.path),
            }
            .unwrap_or_default();
            mounted.to.mapping.path = mount.spell(&change.to.mapping.path).unwrap_or_default();
            (change, mounted)
        })
        .collect()
}

/// [`verify_diff_against_filesystem`] for changes spelled as the working tree holds them, and
/// the incoming side of each conflict.
///
/// The conflicts are checked after the changes, so the failure reported is the earliest one
/// the merge would have hit. The moves and link deletes among the changes answer for both, as
/// [`crate::fs::realize::verify_filesystem`] describes, and are copied only where the diff
/// deletes a directory or a link, with what it writes below a deleted link. The mount of a link
/// the merge deletes or moves to another repository is checked once both have passed
/// ([`crate::fs::realize::verify_merge_mounts`]), against the link `resumed`, the staged state of
/// the merge this one resumes, holds there where it holds one: a resolution an earlier pass
/// staged in the link is no local change.
async fn verify_spelled_diff(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    current: &NodeMapping,
    resumed: Option<&Arc<State>>,
    changes: Vec<&mut NodeChange>,
    conflicts: Vec<&mut NodeChange>,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let deletes_a_directory_or_link = changes.iter().chain(&conflicts).any(|change| {
        change.action == FileAction::Delete
            && (change.from.flags.is_directory() || change.from.is_link())
    });
    let answering = if deletes_a_directory_or_link {
        // A deleted link's mount is checked against what the merge writes there.
        let link_deletes = crate::fs::realize::IncomingChanges::new(Arc::new(
            changes
                .iter()
                .chain(&conflicts)
                .filter(|change| change.action == FileAction::Delete && change.from.is_link())
                .map(|change| (**change).clone())
                .collect(),
        ));
        let writes_below_a_link_delete = |change: &&&mut NodeChange| {
            change.action != FileAction::Delete
                && link_deletes.link_delete_above(change.path()).is_some()
        };
        changes
            .iter()
            .filter(|change| {
                change.move_source().is_some()
                    || (change.action == FileAction::Delete && change.from.is_link())
                    || writes_below_a_link_delete(change)
            })
            .chain(conflicts.iter().filter(writes_below_a_link_delete))
            .map(|change| (**change).clone())
            .collect()
    } else {
        Vec::new()
    };
    let replacing: Vec<NodeChange> = changes
        .iter()
        .chain(&conflicts)
        .filter(|change| crate::fs::realize::replaces_mount(change))
        .map(|change| (**change).clone())
        .collect();
    let incoming = Arc::new(crate::fs::realize::IncomingChanges::new(Arc::new(
        answering,
    )));
    verify_changes_against_filesystem(
        operation, repository, current, changes, &incoming, merge_type,
    )
    .await?;
    verify_changes_against_filesystem(
        operation, repository, current, conflicts, &incoming, merge_type,
    )
    .await?;
    let resumed = resumed.map(|state| NodeMapping::root(current.repository.clone(), state.clone()));
    crate::fs::realize::verify_merge_mounts(
        operation,
        current,
        resumed.as_ref(),
        &replacing,
        &incoming,
    )
    .await
    .forward::<MergeError>("verifying the mounts of the links the merge takes away")?;
    lore_debug!("File system verification complete");
    Ok(())
}

/// Verify one set of changes, reporting the first failure once every task in flight has
/// drained: a task is reading the working copy and has to finish reading it.
///
/// What the verify settles on a change is carried back onto it, since the merge realizes every
/// change it verified rather than the ones the working copy still needs.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn verify_changes_against_filesystem<'a>(
    operation: &'a Arc<InstanceOperationImpl>,
    repository: &'a Arc<RepositoryContext>,
    current: &'a NodeMapping,
    mut changes: Vec<&'a mut NodeChange>,
    incoming: &'a Arc<crate::fs::realize::IncomingChanges>,
    merge_type: MergeType,
) -> impl Future<Output = Result<(), MergeError>> + 'a {
    fn collect(
        joined: Result<Result<(usize, change::Flags), MergeError>, tokio::task::JoinError>,
        changes: &mut [&mut NodeChange],
        failure: &mut Option<MergeError>,
    ) {
        let result = joined
            .map_err(|e| MergeError::internal_with_context(e, "task failure"))
            .and_then(|result| result);
        match result {
            Ok((index, flags)) => changes[index].flags = flags,
            Err(err) => *failure = failure.take().or(Some(err)),
        }
    }

    async move {
        let stats = Arc::new(sync::SyncVerifyStats::default());
        let mut tasks = JoinSet::new();
        let mut failure = None;
        for index in 0..changes.len() {
            let change = changes[index].clone();
            lore_spawn!(tasks, {
                let stats = stats.clone();
                let repository = repository.clone();
                let operation = operation.clone();
                let current = current.clone();
                let incoming = incoming.clone();
                async move {
                    let mut change = change;
                    let no_forward_changes = matches!(merge_type, MergeType::CherryPick);
                    let realizes_every_change = true;
                    let no_force_hash_check = false;
                    crate::fs::realize::verify_filesystem(
                        &mut change,
                        repository,
                        operation,
                        current,
                        no_forward_changes,
                        realizes_every_change,
                        no_force_hash_check,
                        stats,
                        FilterMode::Full,
                        &incoming,
                    )
                    .await
                    .forward::<MergeError>("verifying filesystem for change")?;

                    Ok((index, change.flags))
                }
            });
            while tasks.len() > MAX_CONCURRENT_TREE_TASKS
                && let Some(joined) = tasks.join_next().await
            {
                collect(joined, &mut changes, &mut failure);
            }
        }
        while let Some(joined) = tasks.join_next().await {
            collect(joined, &mut changes, &mut failure);
        }
        match failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// Copy the subtrees the target branch never touched into the staged tree.
///
/// Node operations only, the filter playing no part, and the subtrees sit at disjoint paths
/// so they need no ordering between them. A graft inside a directory marked dirty as moved stages
/// the directory's whole move, as staging inside it does.
async fn apply_diff_grafts(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    grafts: &[NodeChange],
) -> Result<(), MergeError> {
    if grafts.is_empty() {
        return Ok(());
    }

    let graft_repository = full_tree_context(repository);
    let mut graft_tasks = JoinSet::new();
    for graft in grafts.iter() {
        let graft_repository = graft_repository.clone();
        let state_staged = state_staged.clone();
        let graft = graft.clone();
        lore_spawn!(graft_tasks, async move {
            apply_graft_copy(graft_repository, state_staged, &graft).await
        });
    }

    let mut adopted_nodes = 0usize;
    let mut graft_failure = None;
    while let Some(joined) = graft_tasks.join_next().await {
        match joined
            .internal("Graft task failed")
            .map_err(MergeError::from)
        {
            Ok(Ok(nodes)) => adopted_nodes += nodes,
            Ok(Err(err)) | Err(err) => graft_failure = graft_failure.or(Some(err)),
        }
    }
    let drained = if state_staged.holds_promoted_moves() {
        Box::pin(crate::file::stage::stage_promoted_moves_of_call(
            repository,
            state_staged,
            None,
        ))
        .await
        .forward::<MergeError>("staging the moves of directories the grafts are in")
        .map(drop)
    } else {
        Ok(())
    };
    if let Some(err) = graft_failure {
        return Err(err);
    }
    drained?;

    lore_info!(
        "Grafted {} out-of-view subtrees, {} nodes touched",
        grafts.len(),
        adopted_nodes
    );
    Ok(())
}

/// Write each conflicting path back to the version the merge started from, which is what
/// leaves the three sides in place for the three-way merge that follows.
async fn restart_reset_conflicts(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    conflicts: &[(NodeChange, NodeChange)],
    dry_run: bool,
) -> Result<(), MergeError> {
    if conflicts.is_empty() {
        return Ok(());
    }

    let changes = conflicts.iter().map(|tuple| tuple.1.clone()).collect();
    crate::fs::realize::realize_changes(
        repository.clone(),
        operation.clone(),
        Arc::new(changes),
        &[],
        None,
        dry_run,
        None, /* Not a merge */
        Arc::new(sync::SyncRealizeStats::default()),
    )
    .await
    .forward::<MergeError>("realizing reset changes")
}

/// What realizing a diff writes with: the trees the three-way merge reads and the staged
/// tree it records into.
struct RealizeDiff<'a> {
    operation: &'a Arc<InstanceOperationImpl>,
    repository: &'a Arc<RepositoryContext>,
    state_staged: &'a Arc<State>,
    state_base: &'a Arc<State>,
    state_from: &'a Arc<State>,
    state_to: &'a Arc<State>,
    /// The staged state of the merge this one resumes.
    resumed: Option<&'a Arc<State>>,
    changes: &'a Arc<Vec<NodeChange>>,
    conflicts: &'a Arc<Vec<(NodeChange, NodeChange)>>,
    merge_type: MergeType,
    stats: &'a Arc<sync::SyncRealizeStats>,
    /// Record the changes in the staged tree without writing the working copy, which a dry
    /// run asks for and a linked context requires.
    skip_filesystem: bool,
}

/// Write the diff over the working copy, non-conflicting changes first so a conflict's
/// siblings land beside content already in place.
///
/// The times the writes land with are dropped: a merge leaves a staged tree rather than the
/// current revision, so nothing they would vouch for holds yet.
async fn realize_diff_over_filesystem(args: RealizeDiff<'_>) -> Result<(), MergeError> {
    crate::fs::realize::realize_changes(
        args.repository.clone(),
        args.operation.clone(),
        args.changes.clone(),
        &[],
        Some(args.state_staged.clone()),
        args.skip_filesystem,
        // The merge applies to the target.
        Some(crate::fs::realize::MergeCurrent {
            state: args.state_to.clone(),
            staged: args.resumed.cloned(),
        }),
        args.stats.clone(),
    )
    .await
    .forward::<MergeError>("realizing non-conflict changes")?;
    lore_debug!("Realized non-conflict changes");

    crate::fs::realize::realize_conflicts(
        args.repository.clone(),
        args.operation.clone(),
        args.state_base.clone(),
        args.state_from.clone(),
        args.state_to.clone(),
        Some(args.state_staged.clone()),
        args.conflicts.clone(),
        args.skip_filesystem,
        args.stats.clone(),
        args.merge_type,
    )
    .await
    .forward::<MergeError>("realizing conflict changes")?;
    lore_debug!("Realized conflict changes");

    args.operation.take_modified_times().discard();
    Ok(())
}

/// A diff sorted and narrowed to what applying it to one repository writes.
struct PreparedDiff {
    base: Hash,
    source: Hash,
    target: Hash,
    changes: Vec<NodeChange>,
    conflicts: Vec<(NodeChange, NodeChange)>,
    /// Subtrees the target branch never touched. Nothing inside one needs a three-way merge,
    /// so it skips per-file verify and realize.
    grafts: Vec<NodeChange>,
}

impl PreparedDiff {
    /// Whether applying the diff changes nothing: the target holds every change of the source.
    fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.conflicts.is_empty() && self.grafts.is_empty()
    }
}

/// `diff` sorted, without what applying it to `repository` leaves to another repository, and
/// with its grafts set apart.
fn prepare_diff(repository: &RepositoryContext, mut diff: DiffResult) -> PreparedDiff {
    lore_debug!(
        "Branch diff found {} changes and {} conflicts",
        diff.changes.len(),
        diff.conflicts.len()
    );

    change::sort_by_path(&mut diff.changes);
    change::sort_conflict_by_path(&mut diff.conflicts);

    // `state::diff` reports the files inside a link whose pin differs, which are left to the
    // linked repository's own merge. Staged here, they would attach under the link node and
    // replace its mount.
    diff.changes
        .retain(|c| c.to.mapping.repository.id == repository.id);
    diff.conflicts.retain(|(from, to)| {
        from.to.mapping.repository.id == repository.id
            && to.to.mapping.repository.id == repository.id
    });

    let grafts: Vec<NodeChange> = diff
        .changes
        .extract_if(.., |change| change.action == FileAction::Graft)
        .collect();

    PreparedDiff {
        base: diff.base,
        source: diff.source,
        target: diff.target,
        changes: diff.changes,
        conflicts: diff.conflicts,
        grafts,
    }
}

/// `current` is the revision the diff applies to, at the path the working tree holds it.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn apply_diff<'a>(
    repository: Arc<RepositoryContext>,
    token: &'a RepositoryWriteToken,
    diff: DiffResult,
    current: NodeMapping,
    merge_type: MergeType,
    reverse_parents: bool,
    target_branch: BranchId,
    inherit: &'a MetadataInherit,
) -> impl Future<Output = Result<ApplyDiffResults, MergeError>> + 'a {
    let diff = prepare_diff(&repository, diff);
    apply_prepared_diff(
        repository,
        token,
        diff,
        Some(current),
        None,
        merge_type,
        reverse_parents,
        target_branch,
        inherit,
    )
}

/// [`apply_diff`] for a prepared diff. `current` is `None` where the working tree does not hold
/// the revision the diff applies to, which leaves it unchecked. `resumed` is the staged state of
/// the merge it resumes.
#[allow(clippy::too_many_arguments)]
async fn apply_prepared_diff(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    diff: PreparedDiff,
    current: Option<NodeMapping>,
    resumed: Option<Arc<State>>,
    merge_type: MergeType,
    reverse_parents: bool,
    target_branch: BranchId,
    inherit: &MetadataInherit,
) -> Result<ApplyDiffResults, MergeError> {
    let PreparedDiff {
        base,
        source,
        target,
        mut changes,
        mut conflicts,
        grafts,
    } = diff;

    let state_from = state::State::deserialize(repository.clone(), source)
        .await
        .forward::<MergeError>("deserializing diff source state")?;
    let state_to = state::State::deserialize(repository.clone(), target)
        .await
        .forward::<MergeError>("deserializing diff target state")?;
    let state_base = state::State::deserialize(repository.clone(), base)
        .await
        .forward::<MergeError>("deserializing diff base state")?;

    // Prepare the merged staged state
    let state_staged = state::State::deserialize(repository.clone(), target)
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

    let stats = Arc::new(sync::SyncRealizeStats::default());
    let dry_run = execution_context().globals().dry_run();

    // One operation covers the whole diff: every path it verifies and realizes is in the
    // same filesystem, and one opened per change would freeze and thaw it once per file.
    let (changes, conflicts) = with_operation(repository.file_system(), async |operation| {
        if let Some(current) = &current {
            verify_diff_against_filesystem(
                &operation,
                &repository,
                current,
                resumed.as_ref(),
                &mut changes,
                &mut conflicts,
                merge_type,
            )
            .await?;
        }
        let changes = Arc::new(changes);
        let conflicts = Arc::new(conflicts);
        apply_diff_grafts(&repository, &state_staged, &grafts).await?;
        let state_current = current.as_ref().map_or(&state_to, |current| &current.state);
        link::check_incoming_mount_overlaps(repository.clone(), state_current, &changes)
            .await
            .forward::<MergeError>("checking incoming link source paths")?;
        realize_diff_over_filesystem(RealizeDiff {
            operation: &operation,
            repository: &repository,
            state_staged: &state_staged,
            state_base: &state_base,
            state_from: &state_from,
            state_to: &state_to,
            resumed: resumed.as_ref(),
            changes: &changes,
            conflicts: &conflicts,
            merge_type,
            stats: &stats,
            skip_filesystem: dry_run || skip_filesystem,
        })
        .await?;

        Ok::<_, MergeError>((changes, conflicts))
    })
    .await?;

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
                    changes.clone(),
                    state_from.clone(),
                    state_staged.clone(),
                    inherit,
                )
                .await?;

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
                stamp_merged_by(&mut metadata).await?;
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
                    changes.clone(),
                    state_from.clone(),
                    state_staged.clone(),
                    inherit,
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
                    changes.clone(),
                    state_from.clone(),
                    state_staged.clone(),
                    inherit,
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
        RelativePathBuf::new(),
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
            .find_node(repository.clone(), conflict.0.path().as_str())
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
                        path: conflict.0.path().clone().into(),
                    },
                )
                .send();
            }
            MergeType::BranchMerge => {
                event::LoreEvent::BranchMergeConflictFile(LoreBranchMergeConflictFileEventData {
                    path: conflict.0.path().clone().into(),
                })
                .send();
            }
            MergeType::Revert => {
                event::LoreEvent::RevertConflictFile(LoreRevertConflictFileEventData {
                    path: conflict.0.path().clone().into(),
                })
                .send();
            }
            MergeType::None => {}
        }
    }
}

/// The node a link to `link_repository` mounted before the merge, for an abort to return it to
/// with its pin, or `None` for a link the merge brings in.
///
/// The merge may have remounted the link onto a subtree the pin it returns to does not hold.
async fn pre_merge_mount(
    repository: &Arc<RepositoryContext>,
    state_current: &Arc<State>,
    link_path: &str,
    link_repository: RepositoryId,
) -> Result<Option<NodeID>, MergeError> {
    Ok(
        link::link_node_to(state_current, repository, link_path, link_repository)
            .await
            .forward_with::<MergeError, _>(|| format!("resolving link {link_path}"))?
            .map(|(_, node)| node.child),
    )
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
            ..
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

/// The revision the merge of `entry`'s link produced, which a resolve of a divergent row on that
/// link moves the row off, where the conflict row keeps it.
fn merged_pin(entries: &[LinkMergeEntry], entry: &LinkMergeEntry, current: Hash) -> Hash {
    entries
        .iter()
        .find(|other| other.is_conflict() && other.local_node == entry.local_node)
        .map_or(current, |conflict| conflict.target.signature)
}

/// Rolls the merge of `link` back to `entry`'s base, the row it held before the merge, mounting
/// `mount_source`.
///
/// The rollback's state diff leaves conflict markers, sidecars and the side a resolve kept on disk,
/// so the files `merged`, the revision the link's own merge produced, holds in conflict are
/// restored from the base after it.
async fn roll_back_link_merge(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    link: &ResolvedAbortEntry,
    entry: &LinkMergeEntry,
    merged: Hash,
    mount_source: link::LinkMountSource,
) -> Result<(), MergeError> {
    let base = &entry.base;
    let staged_node = link::stage_link_row(
        repository.clone(),
        state_staged,
        &link.link_context,
        &link.link_path,
        link.link_node,
        &link.link_reference,
        base,
        mount_source,
        link::LinkPinRealize::WorkingTree,
    )
    .await
    .forward_with::<MergeError, _>(|| format!("staging link pin for {}", link.link_path))?;

    // A divergent row alone merged nothing inside the link.
    if entry.is_conflict() {
        return Ok(());
    }
    let (merged_link_state, base_link_state, staged_link_node) = tokio::try_join!(
        state::State::deserialize(link.link_context.clone(), merged),
        state::State::deserialize(link.link_context.clone(), base.signature),
        state_staged.node(repository.clone(), staged_node),
    )
    .forward::<MergeError>("reading the rolled-back link")?;
    restore_conflicted_paths(
        repository,
        &link.link_context,
        &link.link_path_rel,
        (merged_link_state, link.link_node.child),
        (base_link_state, staged_link_node.child),
    )
    .await
    .forward_with::<MergeError, _>(|| format!("restoring link paths for {}", link.link_path))
}

/// Restores from `base`, the side a link's merge is rolled back to, each file a conflict marks in
/// `merged`, the side the merge staged, at `mount`, and does the same in each link nested in it
/// that the merge merged. Each side is a state of the linked repository, with its node mounted.
async fn restore_conflicted_paths(
    repository: &Arc<RepositoryContext>,
    link_context: &Arc<RepositoryContext>,
    mount: &RelativePath,
    (merged, merged_node): (Arc<State>, NodeID),
    (base, base_node): (Arc<State>, NodeID),
) -> Result<(), MergeError> {
    let conflicted = stage::collect_merge_paths(
        link_context.clone(),
        merged.clone(),
        merged_node,
        RelativePath::new(),
        stage::MergeWalk {
            nodes: stage::MergeNodes::Conflicts,
            whole_directories: true,
        },
        None,
    )
    .await
    .forward::<MergeError>("collecting the link's conflicted paths")?
    .files;
    link::restore_link_paths_from_state(
        repository.clone(),
        link_context.clone(),
        mount.clone(),
        base.clone(),
        base_node,
        &conflicted,
    )
    .await
    .forward::<MergeError>("restoring the link's conflicted paths")?;

    let nested_links = merged_link_rows(link_context, &merged, merged_node, mount)
        .await?
        .nested;
    if nested_links.is_empty() {
        return Ok(());
    }
    let base_links = base
        .link_list(link_context.clone())
        .await
        .forward::<MergeError>("listing the rolled-back link's links")?;
    for nested in nested_links {
        // The rollback removes a link the merge brought in, with all it holds.
        if !base_links.iter().any(|row| {
            row.local_node == nested.local_node && row.repository == nested.base.repository
        }) {
            continue;
        }
        let (nested_base, base_link_node) = tokio::try_join!(
            state::State::deserialize(nested.link_context.clone(), nested.base.signature),
            base.node(link_context.clone(), nested.local_node),
        )
        .forward::<MergeError>("reading the rolled-back nested link")?;
        Box::pin(restore_conflicted_paths(
            repository,
            &nested.link_context,
            &nested.mount,
            (nested.merged, nested.node),
            (nested_base, base_link_node.child),
        ))
        .await?;
    }
    Ok(())
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
        let (current_revision, _) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<MergeError>("loading current anchor")?;
        let state_current = State::deserialize(repository.clone(), current_revision)
            .await
            .forward::<MergeError>("deserializing current state")?;

        // Walk entries in reverse to undo link merges. After each entry is
        // restored, write the shrunken `LinkMergeState` to the parent staged
        // state and re-anchor — so a mid-walk failure leaves a consistent
        // on-disk state that re-running `merge abort` can pick up from.
        // Without incremental clears, a retry would restore already-restored
        // entries again and the entries would hang around in `LinkMergeState`
        // forever.
        for (i, (entry, r)) in entries.iter().zip(resolved.iter()).rev().enumerate() {
            // The main repository's abort removes a link the merge brings in.
            let Some(pre_merge_root) =
                pre_merge_mount(&repository, &state_current, &r.link_path, r.link_context.id)
                    .await?
            else {
                continue;
            };
            // A link merged in its own right holds a rollback row too, which restores the row it
            // held before that merge rather than the pin the merge produced. That row comes
            // earlier, so its own pass stores the shrunken state.
            if entry.is_conflict()
                && entries
                    .iter()
                    .any(|other| !other.is_conflict() && other.local_node == entry.local_node)
            {
                continue;
            }
            roll_back_link_merge(
                &repository,
                &state_staged,
                r,
                entry,
                merged_pin(&entries, entry, r.link_reference.signature),
                link::LinkMountSource::Node(pre_merge_root),
            )
            .await?;

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

    let (state_current, state_staged_opt, _current_branch) =
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
        ..
    } = link::resolve_link_at_path(&state_staged, repository.clone(), &link_path)
        .await
        .forward_with::<MergeError, _>(|| format!("link not found: {link_path}"))?;

    // A link merged in its own right rolls back through its rollback row, not its conflict row.
    let rows = || {
        entries
            .iter()
            .filter(|e| e.local_node == link_reference.local_node)
    };
    let entry = rows()
        .find(|e| !e.is_conflict())
        .or_else(|| rows().next())
        .ok_or_else(|| MergeError::internal(format!("link not found: {link_path}")))?;
    let merged = merged_pin(&entries, entry, link_reference.signature);

    let mount_source =
        match pre_merge_mount(&repository, &state_current, &link_path, link_context.id).await? {
            Some(node) => link::LinkMountSource::Node(node),
            None => link::LinkMountSource::SamePath,
        };
    roll_back_link_merge(
        &repository,
        &state_staged,
        &ResolvedAbortEntry {
            link_path,
            link_path_rel,
            link_node,
            link_context,
            link_reference,
        },
        entry,
        merged,
        mount_source,
    )
    .await?;

    // The other links keep their rows, for their own abort, resolve or commit.
    let remaining: Vec<LinkMergeEntry> = entries
        .iter()
        .filter(|entry| entry.local_node != link_reference.local_node)
        .copied()
        .collect();
    if remaining.is_empty() {
        state_staged.clear_link_merge_state();
    } else {
        state_staged
            .serialize_link_merge_state(repository.clone(), &remaining)
            .await
            .forward::<MergeError>("serializing link merge state")?;
    }

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

        state_current.reparent_onto(state_current.revision());

        for r in &resolved {
            // Re-read the link reference from the post-abort state to find
            // the now-current pin (which is back to pre-merge after abort). A
            // link the merge brings in is gone with the main repository's merge.
            let Some((current_node_id, current_node)) =
                link::link_node_to(&state_current, &repository, &r.link_path, r.link_context.id)
                    .await
                    .forward_with::<MergeError, _>(|| format!("resolving link {}", r.link_path))?
            else {
                continue;
            };
            let current_ref = state_current
                .link_find(repository.clone(), r.link_context.id, current_node_id)
                .await
                .forward_with::<MergeError, _>(|| format!("link not found: {}", r.link_path))?;

            link::stage_link_row(
                repository.clone(),
                &state_current,
                &r.link_context,
                &r.link_path,
                current_node,
                &current_ref,
                &r.link_reference,
                link::LinkMountSource::Node(r.link_node.child),
                link::LinkPinRealize::WorkingTree,
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

    let dry_run = execution_context().globals().dry_run();
    // One operation covers the abort: the merge artifacts it removes and the changes it
    // reverts are in the same working tree.
    let modified_times = with_operation(repository.file_system(), async |operation| {
        if !dry_run {
            for change in changes.iter() {
                sync::unlink_merge_artifacts(&operation, change.path()).await;
            }
        }

        crate::fs::realize::realize_changes(
            repository.clone(),
            operation.clone(),
            changes.clone(),
            &[],
            None,
            dry_run,
            None, /* Not a merge */
            Arc::new(sync::SyncRealizeStats::default()),
        )
        .await
        .forward::<MergeError>("realizing abort changes")?;

        Ok::<_, MergeError>(operation.take_modified_times())
    })
    .await?;

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

    // The times the revert wrote with speak for the current revision, which is what the abort
    // restored the working tree to and what it leaves current.
    if !dry_run {
        modified_times.store(repository.clone()).await;
    }

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
            if relative_paths.contains(change.path()) {
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
            if relative_paths.contains(change_tuple.0.path())
                || relative_paths.contains(change_tuple.1.path())
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

        let stats = Arc::new(sync::SyncRealizeStats::default());
        let changes = Arc::new(changes);
        let conflicts = Arc::new(conflicts);

        // One operation covers the restart, as it covers a whole diff application.
        with_operation(repository.file_system(), async |operation| {
            restart_reset_conflicts(&operation, &repository, &conflicts, dry_run).await?;
            realize_diff_over_filesystem(RealizeDiff {
                operation: &operation,
                repository: &repository,
                state_staged: &state_staged,
                state_base: &state_base,
                state_from: &state_from,
                state_to: &state_to,
                resumed: None,
                changes: &changes,
                conflicts: &conflicts,
                merge_type,
                stats: &stats,
                skip_filesystem: dry_run,
            })
            .await
        })
        .await?;

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
        let node = block.node(node_index);
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

        state_staged.node_change_in_place(
            &block,
            node_link.node,
            (node.parent, node.name_hash),
            |content| content.set_merge_resolved(false),
        );

        lore_debug!(
            "Conflicted {}, node flags {:?}",
            relative_path.as_str(),
            block.node(node_index).flags
        );

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

/// The conflicts a resolve or an abort still has to act on, a directory replaced by a file as one.
const UNRESOLVED_CONFLICTS: stage::MergeWalk = stage::MergeWalk {
    nodes: stage::MergeNodes::Unresolved,
    whole_directories: true,
};

pub async fn branch_merge_resolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), MergeError> {
    merge_resolve(repository, token, paths, MergeType::BranchMerge).await
}

/// Marks the conflict of the file at `relative_path`, which `node_link` finds in the State of
/// `mapping`, resolved, where the working tree holds no conflict markers in it. A file staged for
/// deletion, or one the working tree lacks, is staged from the working tree first
/// ([`stage_resolved_from_working_tree`]); `side` is the current revision, the tree the merge was
/// staged on, where the resolve reaches the parent's own tree.
async fn resolve_single_file(
    operation: &Arc<InstanceOperationImpl>,
    mapping: &NodeMapping,
    side: Option<&State>,
    relative_path: RelativePath,
    node_link: NodeLink,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let (repository, state_staged) = (&mapping.repository, &mapping.state);
    let block_index = NodeBlock::index(node_link.node);
    let node_index = Node::index(node_link.node);
    let block = state_staged
        .block(repository.clone(), block_index)
        .await
        .forward::<MergeError>("deserializing block")?;
    let node = block.node(node_index);
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

    let markers = operation
        .infer_is_conflicted(&relative_path)
        .await
        .forward_with::<MergeError, _>(|| {
            format!("checking {relative_path} for conflict markers")
        })?;
    if markers == ConflictMarkers::Present {
        lore_warn!(
            "Cannot resolve path with conflict markers still present: {}",
            relative_path.as_str()
        );
        return Ok(());
    }

    let absent = markers == ConflictMarkers::NoFile;
    if node.is_staged_delete() || absent {
        Box::pin(stage_resolved_from_working_tree(
            operation,
            mapping,
            side.filter(|_| absent),
            &relative_path,
        ))
        .await?;
    }

    state_staged.node_change_in_place(
        &block,
        node_link.node,
        (node.parent, node.name_hash),
        |content| content.set_merge_resolved(true),
    );

    lore_debug!(
        "Resolved {}, node flags {:?}",
        relative_path.as_str(),
        block.node(node_index).flags
    );

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

/// Stages the file at `relative_path`, a conflict the State of `mapping` holds, from the working
/// tree under the merge's flags: the commit takes what the working tree holds at a staged path, so
/// a file there restores the add a deletion took back, and a missing one is a deletion. Where
/// `side`, the current revision, is given for a file the working tree lacks, each directory above
/// it that only the merge kept goes with it ([`stage::stage_emptied_ancestors`]).
///
/// Cold, as a resolve reaches it only for a conflict at a deleted or missing file, so its caller
/// boxes it, and it boxes the stage.
async fn stage_resolved_from_working_tree(
    operation: &Arc<InstanceOperationImpl>,
    mapping: &NodeMapping,
    side: Option<&State>,
    relative_path: &RelativePath,
) -> Result<(), MergeError> {
    let Some(below) = relative_path.below_ignore_case(&mapping.path) else {
        return Err(MergeError::internal(
            "A resolved path lies outside its tree",
        ));
    };
    let Ok(below) = RelativePath::from_str(below);
    let options = stage::StageOptions {
        case_change: stage::StageCaseChange::Keep,
        node_flags: NodeFlags::StagedMerge,
        ..Default::default()
    };
    let stats = Arc::new(stage::StageStats::default());
    Box::pin(stage::stage_filesystem_path(
        operation.clone(),
        mapping.clone(),
        below,
        stage::KnownChild::Unresolved,
        stats.clone(),
        options,
        None,
        None,
        None,
        None,
    ))
    .await
    .forward::<MergeError>("staging the resolved file from the working tree")?;
    if let Some(side) = side {
        stage::stage_emptied_ancestors(
            operation,
            &mapping.repository,
            &mapping.state,
            side,
            relative_path,
            &stats,
            options,
        )
        .await
        .forward::<MergeError>("staging the deletion of the directories a deletion empties")?;
    }
    Ok(())
}

/// The files at and below `user_relative`, which `node_link` finds in `state`, that a resolve marks
/// resolved, with their nodes, and the mount and mounted node of each link the walk meets.
async fn files_to_resolve(
    context: &Arc<RepositoryContext>,
    state: &Arc<State>,
    user_relative: RelativePath,
    node_link: NodeLink,
    refusal: stage::ResolveRefusal,
) -> Result<FilesToResolve, MergeError> {
    let block = state
        .block(context.clone(), NodeBlock::index(node_link.node))
        .await
        .forward::<MergeError>("deserializing block")?;
    if block.node(Node::index(node_link.node)).is_file() {
        return Ok(FilesToResolve {
            files: vec![(user_relative, node_link)],
            links: Vec::new(),
        });
    }
    let expanded = stage::collect_merge_paths(
        context.clone(),
        state.clone(),
        node_link.node,
        user_relative.clone(),
        UNRESOLVED_CONFLICTS,
        Some(refusal),
    )
    .await
    .forward::<MergeError>("collecting the conflicted paths below a directory")?;
    let mut files = Vec::with_capacity(expanded.files.len());
    for file_path in expanded.files {
        let Some(below) = file_path.below(&user_relative) else {
            continue;
        };
        let file_node_link = state
            .find_relative_node_link(context.clone(), node_link.node, below)
            .await
            .unwrap_or_default();
        if file_node_link.is_valid() {
            files.push((file_path, file_node_link));
        }
    }
    Ok(FilesToResolve {
        files,
        links: expanded.links,
    })
}

/// What [`files_to_resolve`] collected.
struct FilesToResolve {
    files: Vec<(RelativePath, NodeLink)>,
    links: Vec<(RelativePath, NodeLink)>,
}

/// A link a resolve has reached, deserialized once however many of its paths are named.
struct TouchedLink {
    /// The link's own tree at its mount: the node of its State mounted there.
    mounted: NodeMapping,
    mount: Mount,
    /// The refusal for the links nested in this one.
    refusal: stage::ResolveRefusal,
    link_branch: BranchId,
    /// What the resolve reached of the links nested inside this one.
    nested: ResolveCache,
}

/// A link of the parent's registry, where it is mounted and whether the merge merged it.
struct LinkMount {
    link: state::LinkReference,
    mount: RelativePath,
    merged: bool,
}

/// What resolving one path leaves for the next: each mount's link state deserialized once, and the
/// links of the repository holding them read once.
///
/// The pin update is deferred to the caller, so paths into one mount share a single final write.
/// Mounts of the same linked repository each pin their own state, so each is kept apart.
#[derive(Default)]
struct ResolveCache {
    /// Each mount reached, by its link node, or `None` for one the merge did not merge.
    touched_links: std::collections::HashMap<NodeID, Option<TouchedLink>>,
    links: Option<Vec<LinkMount>>,
}

/// The links of `state_staged`, the merge of a repository the working tree materializes at
/// `mount`, with their mounts, read once. A link the mount leaves out has none.
async fn link_mounts<'a>(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    mount: &Mount,
    cache: &'a mut Option<Vec<LinkMount>>,
) -> Result<&'a [LinkMount], MergeError> {
    if let Some(links) = cache {
        return Ok(links);
    }
    let links = link::links_and_merged(state_staged, repository.clone())
        .await
        .forward::<MergeError>("listing the links of the merge")?;
    let mut mounts = Vec::with_capacity(links.len());
    for (link, merged) in links {
        let path = state_staged
            .node_path(repository.clone(), link.local_node)
            .await
            .forward::<MergeError>("resolving link mount path")?;
        let Ok(path) = RelativePath::from_str(&path);
        if let Some(mount) = mount.spell(&path) {
            mounts.push(LinkMount {
                link,
                mount,
                merged,
            });
        }
    }
    Ok(cache.insert(mounts))
}

/// The files at and below `relative_path`, which `node_link` finds inside a link of
/// `state_staged`, the merge of a repository the working tree materializes at `mount`, that a
/// resolve marks resolved in the link's own state, each with the link's tree at its mount. A link
/// the merge did not merge holds none, and a link nested in the link is walked the same way.
async fn files_to_resolve_in_link(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    mount: &Mount,
    relative_path: RelativePath,
    node_link: NodeLink,
    refusal: &stage::ResolveRefusal,
    cache: &mut ResolveCache,
) -> Result<Vec<(NodeMapping, RelativePath, NodeLink)>, MergeError> {
    let links = link_mounts(repository, state_staged, mount, &mut cache.links).await?;
    let Some(link_mount) = links
        .iter()
        .filter(|link_mount| relative_path.below(&link_mount.mount).is_some())
        .max_by_key(|link_mount| link_mount.mount.as_str().len())
    else {
        lore_debug!("Ignoring {relative_path}, no link holds it");
        return Ok(Vec::new());
    };
    let local_node = link_mount.link.local_node as NodeID;
    let touched = match cache.touched_links.entry(local_node) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => {
            if let Some(refused) = refusal
                .entering_link(local_node, &link_mount.mount)
                .await
                .forward::<MergeError>("checking the link mount")?
            {
                return Err(refused.into());
            }
            let touched = if link_mount.merged {
                let link_context = repository.to_link_context(link_mount.link.repository).await;
                let link_state =
                    state::State::deserialize(link_context.clone(), link_mount.link.signature)
                        .await
                        .forward::<MergeError>("deserializing link state")?;
                let node = state_staged
                    .node(repository.clone(), local_node)
                    .await
                    .forward::<MergeError>("reading the link node")?
                    .child;
                let mount =
                    Mount::of_link(&link_context, &link_state, node, link_mount.mount.clone())
                        .await
                        .forward::<MergeError>("resolving the path the link exposes")?;
                Some(TouchedLink {
                    refusal: stage::ResolveRefusal::new(&link_context, &link_state),
                    mounted: NodeMapping {
                        repository: link_context,
                        state: link_state,
                        path: link_mount.mount.clone(),
                        node,
                    },
                    mount,
                    link_branch: link_mount.link.branch,
                    nested: ResolveCache::default(),
                })
            } else {
                None
            };
            e.insert(touched)
        }
    };
    let Some(touched) = touched else {
        lore_debug!("Ignoring {relative_path}, the link holding it is not merged");
        return Ok(Vec::new());
    };
    let TouchedLink {
        mounted,
        mount,
        refusal: nested_refusal,
        nested,
        ..
    } = touched;
    if node_link.repository != mounted.repository.id {
        return Box::pin(files_to_resolve_in_link(
            &mounted.repository,
            &mounted.state,
            mount,
            relative_path,
            node_link,
            nested_refusal,
            nested,
        ))
        .await;
    }
    let collected = files_to_resolve(
        &mounted.repository,
        &mounted.state,
        relative_path,
        node_link,
        refusal.inside_link(),
    )
    .await?;
    let mut files: Vec<_> = collected
        .files
        .into_iter()
        .map(|(path, node_link)| (mounted.clone(), path, node_link))
        .collect();
    for (nested_mount, mounted_node) in collected.links {
        files.append(
            &mut Box::pin(files_to_resolve_in_link(
                &mounted.repository,
                &mounted.state,
                mount,
                nested_mount,
                mounted_node,
                nested_refusal,
                nested,
            ))
            .await?,
        );
    }
    Ok(files)
}

/// Serializes each link `cache` reached, after the links nested in it, and pins `state`, the
/// state of the repository holding the links, at the result.
async fn pin_touched_links(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    token: &RepositoryWriteToken,
    cache: ResolveCache,
) -> Result<(), MergeError> {
    for (local_node, touched) in cache.touched_links {
        let Some(touched) = touched else {
            continue;
        };
        let link_context = &touched.mounted.repository;
        Box::pin(pin_touched_links(
            link_context,
            &touched.mounted.state,
            token,
            touched.nested,
        ))
        .await?;
        let signature = touched
            .mounted
            .state
            .serialize(link_context.clone(), token)
            .await
            .forward::<MergeError>("serializing link state")?;
        link::update_link_pin_by_node(
            state,
            repository.clone(),
            link_context.id,
            touched.link_branch,
            signature,
            local_node,
        )
        .await
        .forward::<MergeError>("updating link pin")?;
    }
    Ok(())
}

/// Resolves each of `paths` against the state that holds it, which for a path inside a link is
/// the link's own rather than the parent's.
async fn resolve_paths(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    state_current: &State,
    paths: &LoreArray<LoreString>,
    merge_type: MergeType,
    cache: &mut ResolveCache,
) -> Result<(), MergeError> {
    let refusal = stage::ResolveRefusal::new(repository, state_staged);
    // Every path is checked before any is marked, so a refused resolve reports nothing resolved.
    let mut files = Vec::new();
    let root = NodeMapping::root(repository.clone(), state_staged.clone());
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

        if node_link.repository != repository.id {
            files.append(
                &mut files_to_resolve_in_link(
                    repository,
                    state_staged,
                    &Mount::root(),
                    relative_path,
                    node_link,
                    &refusal,
                    cache,
                )
                .await?,
            );
            continue;
        }

        let collected = files_to_resolve(
            repository,
            state_staged,
            relative_path,
            node_link,
            refusal.clone(),
        )
        .await?;
        files.extend(
            collected
                .files
                .into_iter()
                .map(|(path, node_link)| (root.clone(), path, node_link)),
        );
        for (mount, mounted) in collected.links {
            files.append(
                &mut files_to_resolve_in_link(
                    repository,
                    state_staged,
                    &Mount::root(),
                    mount,
                    mounted,
                    &refusal,
                    cache,
                )
                .await?,
            );
        }
    }

    for (mapping, file_path, node_link) in files {
        let side = (mapping.repository.id == repository.id).then_some(state_current);
        resolve_single_file(operation, &mapping, side, file_path, node_link, merge_type).await?;
    }
    Ok(())
}

pub async fn merge_resolve(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    merge_type: MergeType,
) -> Result<(), MergeError> {
    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    validate_merge_type(repository.clone(), state_staged.clone(), merge_type).await?;

    if !state_staged.is_conflict() {
        return Err(MergeError::internal("No conflict to resolve"));
    }

    let paths = if paths.is_empty() {
        LoreArray::from_vec(vec![LoreString::from(".")])
    } else {
        paths
    };

    let mut selected =
        select_link_pin_paths(&repository, &state_staged, paths.as_slice(), true).await?;
    let paths = LoreArray::from_vec(std::mem::take(&mut selected.remaining));

    let mut cache = ResolveCache::default();

    // One operation covers every path: resolving reads the working tree to tell a conflict still
    // marked up from one settled, and stages a deleted or missing file from it, removing a
    // directory only the merge's change of the file kept.
    if !paths.is_empty() {
        with_operation(repository.file_system(), async |operation| {
            resolve_paths(
                &operation,
                &repository,
                &state_staged,
                &state_current,
                &paths,
                merge_type,
                &mut cache,
            )
            .await
        })
        .await?;
    }

    pin_touched_links(&repository, &state_staged, token, cache).await?;

    // Settled once the files resolved, so a refused resolve leaves every row unsettled. Naming an
    // unsettled row without a side keeps the row this branch holds, which is what the working tree
    // holds at the mount.
    if !selected.is_empty() {
        Box::pin(settle_link_pins(
            &repository,
            &state_staged,
            token,
            selected,
            LoreLinkPinResolution::Mine,
            link::LinkPinRealize::WorkingTree,
        ))
        .await?;
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

/// Report every conflict the open merge still holds, files and link rows alike.
pub async fn merge_conflicts_list(repository: Arc<RepositoryContext>) -> Result<(), MergeError> {
    let (state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    let Some(state_staged) = state_staged else {
        return Ok(());
    };

    let diff_repository = repository.clone();
    let diff_current = state_current.clone();
    let diff_staged = state_staged.clone();
    let mut changes = state::ChangeStream::spawn(async move |changes| {
        state::diff(
            diff_repository.clone(),
            diff_current,
            diff_repository,
            diff_staged,
            None,
            None,
            &changes,
            FilterMode::empty(),
        )
        .await
    });
    while let Some(change) = changes.next().await {
        if change.flags.is_conflict_unresolved() {
            event::LoreEvent::BranchMergeConflictFile(LoreBranchMergeConflictFileEventData {
                path: LoreString::from(change.path().as_str()),
            })
            .send();
        }
    }
    changes
        .finish()
        .await
        .forward::<MergeError>("walking the staged state for conflicts")?;

    let levels = merged_levels(
        repository,
        state_staged,
        ROOT_NODE,
        RelativePath::new(),
        current_branch,
    )
    .await?;
    for level in &levels {
        send_link_pin_conflicts(&level.link_context, level.branch, &level.unresolved).await;
    }
    Ok(())
}

/// The divergent link rows `paths` name in `state_staged`'s link merge state, and in those of the
/// links it merged, at every depth, with the paths that name something else, for the caller to
/// resolve as files. A folder names every row mounted under it, as it does every conflicted file
/// below it, so only a path naming a mount itself is left out of the files, unless the mount's
/// node is in conflict in the tree too, as a link both sides added is. With `unresolved_only`, a
/// row already settled is left as it is.
async fn select_link_pin_paths(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    paths: &[LoreString],
    unresolved_only: bool,
) -> Result<SelectedLinkPins, MergeError> {
    let repository_path = repository.require_path()?;
    let relative: Vec<Option<RelativePath>> = paths
        .iter()
        .map(|path| RelativePath::new_from_user_path(repository_path, path.as_str()).ok())
        .collect();
    let mut claimed = vec![false; paths.len()];
    let mut selected = select_link_pins_at(
        repository,
        state_staged,
        &Mount::root(),
        &relative,
        unresolved_only,
        &mut claimed,
    )
    .await?;
    selected.remaining = paths
        .iter()
        .zip(claimed)
        .filter(|(_, claimed)| !claimed)
        .map(|(path, _)| path.clone())
        .collect();
    Ok(selected)
}

/// [`select_link_pin_paths`] in `state_staged`, the staged state of a repository the working tree
/// materializes at `mount`, setting `claimed` for each of `relative` that names a mount itself.
async fn select_link_pins_at(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    mount: &Mount,
    relative: &[Option<RelativePath>],
    unresolved_only: bool,
    claimed: &mut [bool],
) -> Result<SelectedLinkPins, MergeError> {
    if state_staged.link_merge_hash().is_zero() {
        return Ok(SelectedLinkPins::default());
    }
    let entries = state_staged
        .deserialize_link_merge_state(repository.clone())
        .await
        .forward::<MergeError>("deserializing link merge state")?;
    let mut selected = SelectedLinkPins::default();
    let mut conflicts = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let link_path = state_staged
            .link_merge_entry_path(repository.clone(), entry)
            .await
            .forward::<MergeError>("resolving link node path")?;
        let Ok(link_path) = RelativePath::from_str(&link_path);
        let Some(shown) = mount.spell(&link_path) else {
            continue;
        };
        let link_node = state_staged
            .node(repository.clone(), entry.local_node)
            .await
            .forward::<MergeError>("reading the link node")?;
        if entry.is_conflict() {
            conflicts.push((
                index,
                link_path,
                shown,
                entry.is_unresolved(),
                link_node.is_staged_merge_conflict(),
            ));
            continue;
        }
        let link_context = repository.to_link_context(entry.base.repository).await;
        let link_state = state::State::deserialize(link_context.clone(), link_node.address.hash)
            .await
            .forward::<MergeError>("deserializing the merged link's state")?;
        let link_mount = Mount::of_link(&link_context, &link_state, link_node.child, shown)
            .await
            .forward::<MergeError>("resolving the path the link exposes")?;
        let nested = Box::pin(select_link_pins_at(
            &link_context,
            &link_state,
            &link_mount,
            relative,
            unresolved_only,
            claimed,
        ))
        .await?;
        if !nested.is_empty() {
            selected.nested.push(NestedLinkPins {
                link_path,
                local_node: entry.local_node,
                link_context,
                branch: entry.base.branch,
                selected: nested,
            });
        }
    }

    for (relative, claimed) in relative.iter().zip(claimed.iter_mut()) {
        let Some(relative) = relative else {
            continue;
        };
        for (index, link_path, shown, unresolved, node_conflicted) in &conflicts {
            if relative.is_empty() || relative.covers_ignore_case(shown) {
                if *unresolved || !unresolved_only {
                    selected.rows.insert(
                        *index,
                        SelectedLinkRow {
                            link_path: link_path.as_str().to_string(),
                            shown: shown.as_str().to_string(),
                        },
                    );
                }
                *claimed |=
                    relative.as_lowercase_str() == shown.as_lowercase_str() && !node_conflicted;
            }
        }
    }
    selected.entries = entries;
    Ok(selected)
}

/// What [`select_link_pin_paths`] found for a resolve.
#[derive(Default)]
struct SelectedLinkPins {
    entries: Vec<LinkMergeEntry>,
    /// The rows to settle, by index in `entries`.
    rows: BTreeMap<usize, SelectedLinkRow>,
    /// The links merged in their own right whose own rows, or those of links nested in them, the
    /// resolve settles.
    nested: Vec<NestedLinkPins>,
    /// The paths left for the file resolve.
    remaining: Vec<LoreString>,
}

impl SelectedLinkPins {
    fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.nested.is_empty()
    }
}

/// A divergent row a resolve settles.
struct SelectedLinkRow {
    /// The link's path in the repository holding it.
    link_path: String,
    /// The link's path in the working tree.
    shown: String,
}

/// A link merged in its own right holding rows a resolve settles.
struct NestedLinkPins {
    /// The link's path in the repository holding it.
    link_path: RelativePath,
    local_node: NodeID,
    link_context: Arc<RepositoryContext>,
    /// The branch the link's row stores.
    branch: BranchId,
    selected: SelectedLinkPins,
}

/// A row settled inside a nested link, realized at its mount once every row is checked.
struct NestedPinChange {
    link_context: Arc<RepositoryContext>,
    target: link::LinkPinRealizeTarget,
}

/// Settle `selected`'s rows in `state_staged` to `resolution`, writing each chosen row with its
/// mount and realizing it as `realize` says. The rows of the links nested in it are settled in
/// their own states, as `state_staged` holds them, each such link restaged, and the rows realized
/// at their own mounts. Every row is checked against the working tree first, so a refused one
/// leaves the working tree as the staged state describes it. The caller owns the write of
/// `state_staged`. Returns the nested rows a `StateOnly` settle leaves to realize.
///
/// A link holding neither recorded side carries work staged inside it since the merge started, or
/// the result of its own merge: mine keeps it, and theirs is refused rather than discard it.
///
/// Only a resolve naming a link row settles one, so its callers box it rather than hold it.
async fn settle_link_pins(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    token: &RepositoryWriteToken,
    selected: SelectedLinkPins,
    resolution: LoreLinkPinResolution,
    realize: link::LinkPinRealize,
) -> Result<Vec<NestedPinChange>, MergeError> {
    let SelectedLinkPins {
        mut entries,
        rows,
        nested,
        ..
    } = selected;
    let settles_rows = !rows.is_empty();
    let mut chosen = Vec::with_capacity(rows.len());
    let mut changes = Vec::new();
    let mut settled = Vec::with_capacity(rows.len());
    for (index, row) in rows {
        let SelectedLinkRow { link_path, shown } = row;
        let entry = entries
            .get_mut(index)
            .ok_or_else(|| MergeError::internal("Link merge state changed during the resolve"))?;
        let current = link::resolve_link_at_path(state_staged, repository.clone(), &link_path)
            .await
            .forward::<MergeError>("resolving the link to settle")?;
        let current_row = current
            .link_reference
            .with_signature(current.link_node.address.hash);
        let holds = |row: &LinkReference, mount: NodeID| {
            current_row.stores_same(row) && current.link_node.child == mount
        };
        let write = match resolution {
            LoreLinkPinResolution::Mine => {
                holds(&entry.source, entry.source_mount)
                    && !holds(&entry.target, entry.target_mount)
            }
            LoreLinkPinResolution::Theirs if holds(&entry.source, entry.source_mount) => false,
            LoreLinkPinResolution::Theirs if holds(&entry.target, entry.target_mount) => true,
            LoreLinkPinResolution::Theirs => {
                return Err(InvalidArguments {
                    reason: format!(
                        "{shown} holds changes staged since the merge started, which taking the \
                         merged branch's row would discard; commit or unstage them first, or \
                         resolve {shown} on its own"
                    ),
                }
                .into());
            }
        };
        entry.set_resolution(LinkMergeResolution::from(resolution));
        if write && let Some((row, mount)) = entry.resolved_reference() {
            if matches!(realize, link::LinkPinRealize::StateOnly) {
                let Ok(shown_path) = RelativePath::from_str(&shown);
                changes.push(NestedPinChange {
                    link_context: current.link_context.clone(),
                    target: link::LinkPinRealizeTarget {
                        link_path: shown_path,
                        old_signature: current.link_node.address.hash,
                        new_signature: row.signature(),
                        current_node: current.link_node.child,
                        mount_source: link::LinkMountSource::Node(mount),
                    },
                });
            }
            chosen.push((link_path, row, mount));
        }
        settled.push(shown);
    }

    let mut restaged = Vec::with_capacity(nested.len());
    for NestedLinkPins {
        link_path,
        local_node,
        link_context,
        branch,
        selected,
    } in nested
    {
        let link_node = state_staged
            .node(repository.clone(), local_node)
            .await
            .forward::<MergeError>("reading the link node")?;
        let link_state = state::State::deserialize(link_context.clone(), link_node.address.hash)
            .await
            .forward::<MergeError>("deserializing the merged link's state")?;
        changes.extend(
            Box::pin(settle_link_pins(
                &link_context,
                &link_state,
                token,
                selected,
                resolution,
                link::LinkPinRealize::StateOnly,
            ))
            .await?,
        );
        let signature = link_state
            .serialize(link_context.clone(), token)
            .await
            .forward::<MergeError>("serializing the merged link's state")?;
        restaged.push((link_path, link_node, link_context, branch, signature));
    }

    let realizes = matches!(realize, link::LinkPinRealize::WorkingTree);
    if realizes {
        with_operation(repository.file_system(), async |operation| {
            for (link_path, row, mount) in &chosen {
                link::verify_resolved_link_pin(
                    &operation,
                    repository,
                    state_staged,
                    link_path,
                    row,
                    *mount,
                )
                .await
                .forward::<MergeError>("checking the resolved link pin against the working tree")?;
            }
            for change in &changes {
                link::verify_link_pin_change(
                    &operation,
                    repository,
                    &change.link_context,
                    change.target.clone(),
                )
                .await
                .forward::<MergeError>("checking the resolved link pin against the working tree")?;
            }
            Ok::<_, MergeError>(())
        })
        .await?;
    }

    for (link_path, row, mount) in chosen {
        link::apply_resolved_link_pin(
            repository.clone(),
            state_staged,
            &link_path,
            row,
            mount,
            realize,
        )
        .await
        .forward::<MergeError>("applying the resolved link pin")?;
    }
    for (link_path, link_node, link_context, branch, signature) in restaged {
        link::stage_link_pin(
            repository.clone(),
            state_staged,
            &link_context,
            link_path,
            link_node,
            link_node.address.hash,
            signature,
            branch,
            link::LinkMountSource::SamePath,
            link::LinkPinRealize::StateOnly,
        )
        .await
        .forward::<MergeError>("staging the merged link's settled rows")?;
    }
    if realizes {
        with_operation(repository.file_system(), async |operation| {
            for change in changes.drain(..) {
                link::realize_link_pin_change_in_operation(
                    &operation,
                    repository.clone(),
                    change.link_context,
                    change.target,
                    false,
                )
                .await
                .forward::<MergeError>("realizing the resolved link pin")?;
            }
            Ok::<_, MergeError>(())
        })
        .await?;
    }
    for link_path in settled {
        event::LoreEvent::BranchMergeLinkPinResolved(LoreBranchMergeLinkPinResolvedEventData {
            link_path: LoreString::from(link_path.as_str()),
            resolution,
        })
        .send();
    }

    if settles_rows {
        state_staged
            .serialize_link_merge_state(repository.clone(), &entries)
            .await
            .forward::<MergeError>("serializing link merge state")?;
    }
    Ok(changes)
}

async fn merge_resolve_side(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
    parent: stage::MergeParent,
    resolution: LoreLinkPinResolution,
) -> Result<(), StageError> {
    validate_merge_type(repository.clone(), None, MergeType::BranchMerge)
        .await
        .forward::<StageError>("Failed to deserialize revision state")?;

    let state_staged = staged_state(&repository)
        .await
        .forward::<StageError>("Failed to deserialize staged state")?;
    // No path names every conflict, files and link rows alike.
    let paths = if paths.is_empty() {
        vec![LoreString::from(".")]
    } else {
        paths.as_slice().to_vec()
    };
    let mut selected = select_link_pin_paths(&repository, &state_staged, &paths, false)
        .await
        .forward::<StageError>("Failed to select link pin conflicts")?;

    // The files go first, so a refused resolve leaves every row it named unsettled.
    if selected.is_empty() || !selected.remaining.is_empty() {
        Box::pin(stage::stage_from_parent_revision(
            repository.clone(),
            token,
            LoreArray::from_vec(std::mem::take(&mut selected.remaining)),
            parent,
        ))
        .await?;
    }
    if selected.is_empty() {
        return Ok(());
    }

    let state_staged = staged_state(&repository)
        .await
        .forward::<StageError>("Failed to deserialize staged state")?;
    Box::pin(settle_link_pins(
        &repository,
        &state_staged,
        token,
        selected,
        resolution,
        link::LinkPinRealize::WorkingTree,
    ))
    .await
    .forward::<StageError>("Failed to resolve link pin conflict")?;
    let signature = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<StageError>("Failed to serialize staged state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<StageError>("Failed to store staged anchor")
}

/// The staged state, or the current one where nothing is staged.
async fn staged_state(repository: &Arc<RepositoryContext>) -> Result<Arc<State>, MergeError> {
    let (state_current, state_staged, _branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<MergeError>("deserializing current and staged state")?;
    Ok(state_staged.unwrap_or(state_current))
}

pub async fn merge_resolve_mine(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), StageError> {
    merge_resolve_side(
        repository,
        token,
        paths,
        stage::MergeParent::Mine,
        LoreLinkPinResolution::Mine,
    )
    .await
}

pub async fn merge_resolve_theirs(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    paths: LoreArray<LoreString>,
) -> Result<(), StageError> {
    merge_resolve_side(
        repository,
        token,
        paths,
        stage::MergeParent::Theirs,
        LoreLinkPinResolution::Theirs,
    )
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
    /// Metadata keys carried from the current branch onto the revision created
    /// on the target branch.
    pub inherit_metadata: MetadataInherit,
}

/// Copies the file metadata `path` has in `state_source` onto `path` in `state_staged`, where
/// both states hold it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn merge_metadata_task(
    repository: Arc<RepositoryContext>,
    path: RelativePath,
    state_source: Arc<State>,
    state_staged: Arc<State>,
) -> impl Future<Output = Result<(), MergeError>> {
    async move {
        let metadata_hash;

        if let Ok(node_link) = state_source
            .find_node_link(repository.clone(), path.as_str())
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
                path
            );
            return Ok(());
        }

        if let Ok(node_link) = state_staged
            .find_node_link(repository.clone(), path.as_str())
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
            }

            lore_trace!("Merged metadata for {}", path);
        } else {
            lore_debug!(
                "Merge metadata skipped due to missing 'staged' node for {}",
                path
            );
            return Ok(());
        }

        Ok(())
    }
}

async fn merge_file_metadata(
    repository: Arc<RepositoryContext>,
    changes: Arc<Vec<NodeChange>>,
    state_source: Arc<State>,
    state_staged: Arc<State>,
) -> Result<(), MergeError> {
    let mut tasks = JoinSet::new();
    for change in changes.iter() {
        lore_spawn!(
            tasks,
            merge_metadata_task(
                repository.clone(),
                change.path().clone(),
                state_source.clone(),
                state_staged.clone(),
            )
        );
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

/// Carry the keys `inherit` permits from the source revision onto the staged
/// state, and no others.
///
/// Runs before the operation writes the keys describing the revision it creates,
/// so one it does not write is left absent rather than inherited.
async fn merge_revision_metadata(
    repository: Arc<RepositoryContext>,
    state_source: Arc<State>,
    state_staged: Arc<State>,
    inherit: &MetadataInherit,
) -> Result<(), MergeError> {
    let metadata_hash = state_source.metadata_hash();
    if inherit.is_empty() || metadata_hash.is_zero() {
        state_staged.set_metadata_hash(Hash::default());
        return Ok(());
    }

    let mut metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .forward::<MergeError>("deserializing source revision metadata")?;

    if metadata.retain_inherited(inherit) == 0 {
        state_staged.set_metadata_hash(metadata_hash);
        return Ok(());
    }

    state_staged.set_metadata_hash(
        metadata
            .serialize(repository.clone())
            .await
            .forward::<MergeError>("serializing inherited revision metadata")?,
    );

    Ok(())
}

pub async fn merge_metadata(
    repository: Arc<RepositoryContext>,
    changes: Arc<Vec<NodeChange>>,
    state_source: Arc<State>,
    state_staged: Arc<State>,
    inherit: &MetadataInherit,
) -> Result<(), MergeError> {
    merge_file_metadata(
        repository.clone(),
        changes,
        state_source.clone(),
        state_staged.clone(),
    )
    .await?;

    merge_revision_metadata(
        repository,
        state_source.clone(),
        state_staged.clone(),
        inherit,
    )
    .await?;

    Ok(())
}

/// Record who performed the merge, leaving the key unset without an identity.
///
/// Called only by the branch-merge paths; a cherry-pick performs no merge.
async fn stamp_merged_by(metadata: &mut Metadata) -> Result<(), MergeError> {
    let merge_user = execution_context().user_id().await;
    if merge_user.is_empty() {
        return Ok(());
    }
    metadata
        .set_string(metadata::MERGED_BY, &merge_user)
        .forward::<MergeError>("setting merged-by metadata")?;
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
    inherit: &MetadataInherit,
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

    let target_link = link::resolve_link_at_path(&state_staged, repository.clone(), link_path)
        .await
        .forward_with::<MergeError, _>(|| format!("link not found: {link_path_owned}"))?;
    let link_path_rel =
        RelativePath::from_str(link_path).internal_with(|| format!("link: {link_path}"))?;
    link::stage_link_pin(
        repository.clone(),
        &state_staged,
        &current_link.link_context,
        link_path_rel,
        target_link.link_node,
        target_link.link_reference.signature,
        current_link.link_reference.signature,
        current_link.link_reference.branch,
        link::LinkMountSource::Node(current_link.link_node.child),
        link::LinkPinRealize::StateOnly,
    )
    .await
    .forward::<MergeError>("updating link pin")?;

    merge_revision_metadata(
        repository.clone(),
        state_current.clone(),
        state_staged.clone(),
        inherit,
    )
    .await?;

    let signature = commit_merge_into(
        &repository,
        token,
        &state_staged,
        target_branch,
        target_branch,
        [branch_latest, state_current.revision()],
        message,
    )
    .await?;

    let (mut revision, mut revision_number) = (signature, state_staged.revision_number());
    // A dry run pushes nothing: the push moves the branch on the remote.
    if !execution_context().globals().dry_run()
        && let Ok(remote) = repository.remote().await
    {
        (revision, revision_number) = push_merge_into(
            &repository,
            remote,
            &state_branch,
            &state_staged,
            signature,
            target_branch,
        )
        .await?;
    }

    LoreEvent::BranchMergeIntoRevision(LoreBranchMergeIntoRevisionEventData {
        revision,
        revision_number,
    })
    .send();

    Ok(())
}

/// Commits `state_staged`, the merge of `parents[1]` into `parents[0]`, on `branch` with
/// `message`, writing nothing to the working tree. `rehash_branch` is the branch the rehash
/// commits linked content for. Returns the revision's signature.
async fn commit_merge_into(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_staged: &Arc<State>,
    branch: BranchId,
    rehash_branch: BranchId,
    parents: [Hash; 2],
    message: String,
) -> Result<Hash, MergeError> {
    let metadata_hash = state_staged.metadata_hash();
    let mut original_metadata = if metadata_hash.is_zero() {
        Metadata::new()
    } else {
        Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .forward::<MergeError>("deserializing metadata")?
    };
    stamp_merged_by(&mut original_metadata).await?;

    let metadata = commit::prepare_commit_metadata(
        repository.clone(),
        original_metadata,
        branch,
        message,
        None,
        None,
        None,
    )
    .await
    .forward::<MergeError>("preparing commit metadata")?;

    commit::rehash_tree_in_operation(
        repository.clone(),
        token.share(),
        state_staged.clone(),
        metadata.clone(),
        rehash_branch,
    )
    .await
    .forward::<MergeError>("rehashing commit")?
    .discard();

    state_staged.reset_merge_conflict_flags();
    state_staged.set_parent_self(parents[0]);
    state_staged.set_parent_other(parents[1]);
    state_staged.set_metadata_hash(
        metadata
            .serialize(repository.clone())
            .await
            .forward::<MergeError>("serializing metadata")?,
    );

    commit::weave_history(
        repository.clone(),
        state_staged.clone(),
        commit::BasedOn::FirstParent,
    )
    .await
    .forward::<MergeError>("weaving history")?;

    state_staged
        .serialize_committed(repository.clone(), token)
        .await
        .forward::<MergeError>("serializing state")
}

/// Pushes `state_new`, committed on `state_branch`, to `branch`. Returns the revision the remote
/// holds and its number.
async fn push_merge_into(
    repository: &Arc<RepositoryContext>,
    remote: Arc<Connection>,
    state_branch: &Arc<State>,
    state_new: &Arc<State>,
    signature: Hash,
    branch: BranchId,
) -> Result<(Hash, u64), MergeError> {
    let fragments = state::collect_new_fragments(
        repository.clone(),
        state_branch.clone(),
        state_new.clone(),
        true,
        Arc::new(StoreRequestTracker::default()),
    )
    .await
    .forward::<MergeError>("collecting new fragments")?;

    let stats = Arc::new(PushProgress::new(execution_context().push_stats().clone()));
    let correlation_id = execution_context().globals().correlation_id.to_string();
    let storage_protocol = remote
        .session(repository.id, &correlation_id)
        .await
        .forward::<MergeError>("opening storage session")?;

    let missing_fragments = push_query(
        storage_protocol.clone(),
        fragments,
        remote.environment.max_query_batch(),
        execution_context().push_stats(),
    )
    .await
    .forward_any::<MergeError>("querying fragments")?;

    push_fragments(
        repository.clone(),
        storage_protocol,
        missing_fragments,
        stats,
    )
    .await
    .forward_any::<MergeError>("pushing fragments")?;

    push_merged_branch(repository, &remote, branch, signature).await
}

/// Moves `branch` on the remote to `signature`, a revision on top of its latest, and returns the
/// revision the remote holds and its number.
async fn push_merged_branch(
    repository: &Arc<RepositoryContext>,
    remote: &Arc<Connection>,
    branch: BranchId,
    signature: Hash,
) -> Result<(Hash, u64), MergeError> {
    let response = remote
        .revision(repository.id)
        .await
        .forward::<MergeError>("acquiring revision protocol")?
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
    Ok((response.revision, response.revision_number))
}

/// The links `merge into` brings in from one linked repository, with where they land.
struct IncomingLinksInto {
    incoming: IncomingLinks,
    /// The revision the links land at.
    pin: Hash,
    /// The merge committed for the links to land at.
    merged: Option<LinkMergedInto>,
}

/// A linked repository's merge `merge into` commits, for it to push ahead of the main
/// repository's.
struct LinkMergedInto {
    link_context: Arc<RepositoryContext>,
    state_branch: Arc<State>,
    state_new: Arc<State>,
    signature: Hash,
}

/// The following links `merge into` brings into `target_branch`, each repository's merged into
/// the branch they land on and committed there, with the revision they land at. A merge that
/// conflicts is refused, as `merge into` has no working tree to resolve it in.
///
/// Each repository's merge is boxed: it holds a diff and a commit, and the path is taken only for
/// a link the merge brings in.
async fn merge_incoming_links_into(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_target: &Arc<State>,
    state_current: &Arc<State>,
    current_branch: BranchId,
    target_branch: BranchId,
    options: &MergeIntoOptions,
) -> Result<Vec<IncomingLinksInto>, MergeError> {
    // The target branch is merged into the current one, so it is the merge's base as well.
    let incoming = plan_incoming_links(
        repository,
        state_target,
        state_target,
        state_current,
        current_branch,
        target_branch,
    )
    .await?;
    link::try_join_links(
        incoming
            .into_iter()
            .map(|incoming| {
                Box::pin(async move {
                    let (pin, merged) = match &incoming.merge {
                        IncomingMerge::Latest(pin) | IncomingMerge::MissingBranch(pin) => {
                            (*pin, None)
                        }
                        IncomingMerge::Merge(source) => {
                            let merge =
                                prepare_merge(incoming.link_context.clone(), *source, None).await?;
                            if !merge.diff.conflicts.is_empty() {
                                let link_path = &incoming.mounts[0].link_path;
                                return Err(NotSupported {
                                    operation: format!(
                                        "merging into a branch whose linked repository at \
                                     {link_path} conflicts with this one. Switch to the target \
                                     branch and merge this one with `lore branch merge start`"
                                    ),
                                }
                                .into());
                            }
                            if merge.diff.is_empty() {
                                (source.current_signature, None)
                            } else {
                                let merged = commit_link_merge_into(
                                    &incoming.link_context,
                                    token,
                                    merge,
                                    options,
                                )
                                .await?;
                                (merged.signature, Some(merged))
                            }
                        }
                    };
                    Ok(IncomingLinksInto {
                        incoming,
                        pin,
                        merged,
                    })
                })
            })
            .collect(),
    )
    .await
}

/// Commits `merge`, a merge of a linked repository free of conflicts, on the branch it merges
/// into, writing nothing to the working tree.
async fn commit_link_merge_into(
    link_context: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    merge: PreparedMerge,
    options: &MergeIntoOptions,
) -> Result<LinkMergedInto, MergeError> {
    let PreparedMerge { source, diff, .. } = merge;
    let (state_branch, state_from, state_new) = tokio::try_join!(
        state::State::deserialize(link_context.clone(), source.current_signature),
        state::State::deserialize(link_context.clone(), source.revision),
        state::State::deserialize(link_context.clone(), source.current_signature),
    )
    .forward::<MergeError>("deserializing the linked merge's states")?;

    apply_diff_grafts(link_context, &state_new, &diff.grafts).await?;
    let changes = Arc::new(diff.changes);
    sync::realize_changes(
        link_context.clone(),
        changes.clone(),
        Some(state_new.clone()),
        true,                       /* No changes on disk */
        Some(state_branch.clone()), /* The merge applies to the branch latest */
        Arc::new(sync::SyncRealizeStats::default()),
    )
    .await
    .forward::<MergeError>("realizing linked changes")?;
    merge_metadata(
        link_context.clone(),
        changes,
        state_from,
        state_new.clone(),
        &options.inherit_metadata,
    )
    .await?;

    // The working tree holds none of the merge, so the commit takes each file from its node.
    let filter = Filter::excluding_all_from_view()
        .forward::<MergeError>("excluding the merge from the working tree")?;
    let signature = commit_merge_into(
        &Arc::new(link_context.to_filter_context(Arc::new(filter))),
        token,
        &state_new,
        source.current_branch,
        source.current_branch,
        [source.current_signature, source.revision],
        options.message.clone(),
    )
    .await?;
    Ok(LinkMergedInto {
        link_context: link_context.clone(),
        state_branch,
        state_new,
        signature,
    })
}

/// Pins each link of `incoming` that `state_staged`, the merge into the target branch, holds at
/// the revision its repository's merge lands at. Returns the linked merges to push, and the
/// repositories that hold no branch for their links to land on.
async fn land_incoming_links_into(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    incoming: Vec<IncomingLinksInto>,
) -> Result<(Vec<LinkMergedInto>, Vec<MissingBranch>), MergeError> {
    let mut merged_into = Vec::new();
    let mut missing_branches = Vec::new();
    for IncomingLinksInto {
        incoming,
        pin,
        merged,
    } in incoming
    {
        let landed = landed_mounts(repository, state_staged, &incoming).await?;
        if landed.is_empty() {
            continue;
        }
        if let IncomingMerge::MissingBranch(pin) = incoming.merge {
            missing_branches.push(missing_branch(&incoming, &landed, pin));
        }
        pin_landed_mounts(
            repository,
            state_staged,
            &incoming.link_context,
            &landed,
            pin,
            link::LinkPinRealize::StateOnly,
        )
        .await?;
        merged_into.extend(merged);
    }
    Ok((merged_into, missing_branches))
}

/// Merges the current branch into `branch` and pushes the result.
///
/// The link-only merge `--link` asks for is boxed. Only that option reaches it, and inline it
/// would make every merge's future as large as its own.
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

    let branch_latest = if let Some(remote) = absent_unless_refused::<_, _, MergeError>(
        repository.remote().await,
        "connecting to the remote",
    )? {
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
    let branch_point = stack.first().map_or_default(|parent| parent.revision);
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
        return Box::pin(merge_into_link(
            repository,
            token,
            branch,
            options.message.clone(),
            state_current,
            state_branch,
            branch_latest,
            link_path,
            &options.inherit_metadata,
        ))
        .await;
    }

    let incoming_links = if options.ignore_links {
        warn_of_unmerged_links(&repository, &state_branch, &state_branch, &state_current).await?;
        Vec::new()
    } else {
        merge_incoming_links_into(
            &repository,
            token,
            &state_branch,
            &state_current,
            current_branch,
            branch,
            &options,
        )
        .await?
    };

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

    // Realizing a linked repository's changes against the parent state attaches
    // its file nodes under the parent's link node and overwrites
    // `link_node.child`. Link contents go through `merge into --link`.
    changes.retain(|c| c.to.mapping.repository.id == repository.id);

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
                    .mapping
                    .state
                    .block(
                        change.from.mapping.repository.clone(),
                        NodeBlock::index(change.from.mapping.node),
                    )
                    .await
                    .forward::<MergeError>("deserializing block")?;
                block.node(Node::index(change.from.mapping.node))
            } else {
                let block = change
                    .to
                    .mapping
                    .state
                    .block(
                        change.to.mapping.repository.clone(),
                        NodeBlock::index(change.to.mapping.node),
                    )
                    .await
                    .forward::<MergeError>("deserializing block")?;
                block.node(Node::index(change.to.mapping.node))
            }
        };

        LoreEvent::BranchMergeIntoFile(LoreBranchMergeIntoFileEventData {
            path: LoreString::from(change.path()),
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
        true,                       /* No changes on disk */
        Some(state_branch.clone()), /* The merge applies to the branch latest */
        stats,
    )
    .await
    .forward::<MergeError>("realizing changes")?;
    lore_debug!("Realized changes on state");

    // The target branch is required to be merged into this one already, so this
    // branch's rows are the newer ones. The working tree stays on the current
    // branch, so nothing is realized on disk.
    if !options.ignore_links {
        link::merge_link_pins(
            repository.clone(),
            &state_staged,
            branch,
            &state_current,
            link::LinkPinResolution::Incoming,
            link::LinkPinRealize::StateOnly,
            &[],
        )
        .await
        .forward::<MergeError>("carrying link pins into the target branch")?;
    }
    let (merged_into_links, missing_branches) =
        land_incoming_links_into(&repository, &state_staged, incoming_links).await?;

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
        &options.inherit_metadata,
    )
    .await?;
    lore_debug!("Merged metadata on state");

    let signature = commit_merge_into(
        &repository,
        token,
        &state_staged,
        branch,
        current_branch,
        [branch_latest, state_current.revision()],
        options.message.clone(),
    )
    .await?;
    let state_new = state_staged;

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
        Arc::new(StoreRequestTracker::default()),
    )
    .await
    .forward::<MergeError>("collecting new fragments")?;

    let mut revision = signature;
    let mut revision_number = state_new.revision_number();

    // A dry run pushes nothing: the push moves the branch on the remote.
    if !execution_context().globals().dry_run()
        && let Ok(remote) = repository.remote().await
    {
        let stats = Arc::new(PushProgress::new(execution_context().push_stats().clone()));

        LoreEvent::BranchMergeIntoFragmentBegin(LoreBranchMergeIntoFragmentBeginEventData {
            fragments: fragments.len() as u64,
        })
        .send();

        let correlation_id = execution_context().globals().correlation_id.to_string();
        let storage_protocol = remote
            .session(repository.id, &correlation_id)
            .await
            .forward::<MergeError>("opening storage session")?;
        let missing_fragments = push_query(
            storage_protocol.clone(),
            fragments,
            remote.environment.max_query_batch(),
            execution_context().push_stats(),
        )
        .await
        .forward_any::<MergeError>("querying fragments")?;

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
                        complete: stats.complete(),
                        count: stats.count(),
                    }).send();
                },
                result = &mut push_task => {
                    break result.map_err(|e| MergeError::internal_with_context(e, "task failure"))?;
                }
            }
        };
        result.forward_any::<MergeError>("pushing fragments")?;

        LoreEvent::BranchMergeIntoFragmentEnd(LoreBranchMergeIntoFragmentEndEventData {
            fragments: stats.complete(),
        })
        .send();

        // Linked repositories change only once nothing but the push can fail, and before it, so
        // the published target never holds a link with no branch to push to, or pinned at a
        // revision its branch does not hold.
        link::try_join_links(
            merged_into_links
                .iter()
                .map(async |merged| {
                    let link_remote = merged
                        .link_context
                        .remote()
                        .await
                        .forward::<MergeError>("connecting to the linked repository")?;
                    push_merge_into(
                        &merged.link_context,
                        link_remote,
                        &merged.state_branch,
                        &merged.state_new,
                        merged.signature,
                        branch,
                    )
                    .await
                })
                .collect(),
        )
        .await?;
        link::create_branches_for_merged_links(
            repository.clone(),
            &missing_branches,
            branch,
            current_branch,
        )
        .await
        .forward::<MergeError>("creating branches for merged links")?;

        (revision, revision_number) =
            push_merged_branch(&repository, &remote, branch, signature).await?;
    }

    LoreEvent::BranchMergeIntoRevision(LoreBranchMergeIntoRevisionEventData {
        revision,
        revision_number,
    })
    .send();

    Ok(())
}
