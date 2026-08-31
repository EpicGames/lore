// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_base::types::BranchPoint;
use lore_error_set::prelude::*;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;
use lore_transport::quic::storage_service::QueryStatus;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinError;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::fragment;
use crate::history;
use crate::immutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::layer;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Fragment;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state;
use crate::state::State;
use crate::store::StoreMatch;
use crate::util::serde::u8_as_bool;

/// Data for the event sent when a branch push starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushEventData {
    /// The remote being pushed to.
    pub remote: LoreString,
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
    /// The name of the branch being pushed.
    pub branch_name: LoreString,
    /// The latest revision of the branch on the remote.
    pub remote_revision: Hash,
    /// The latest revision of the branch in the local repository.
    pub local_revision: Hash,
    /// The number of revisions on the remote that are not present locally.
    pub remote_history: u64,
    /// The number of local revisions to push.
    pub local_history: u64,
    /// Set when the local revision is already present on the remote.
    #[serde(with = "u8_as_bool")]
    pub flag_already_pushed: u8,
    /// Set when the branch is the repository's default branch.
    #[serde(with = "u8_as_bool")]
    pub flag_default: u8,
    /// Set when the repository is a linked repository.
    #[serde(with = "u8_as_bool")]
    pub flag_link: u8,
    /// Set when the repository is a layer.
    #[serde(with = "u8_as_bool")]
    pub flag_layer: u8,
}

/// Data for the event sent before a revision's parent is rewritten during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionUpdateBeginEventData {
    /// The revision being updated.
    pub revision: Hash,
    /// The previous parent revision.
    pub old_parent: Hash,
    /// The new parent revision.
    pub new_parent: Hash,
}

/// Data for the event sent after a revision's parent is rewritten during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionUpdateEndEventData {
    /// The updated revision.
    pub revision: Hash,
}

/// Data for the event sent before fragments are transferred during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushFragmentBeginEventData {
    /// The number of fragments to transfer.
    pub fragments: u64,
    /// The total number of bytes to transfer.
    pub bytes_total: u64,
}

/// Data for the event sent as fragments are transferred during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushFragmentProgressEventData {
    /// The number of fragments transferred so far.
    pub complete: u64,
    /// The total number of fragments to transfer.
    pub count: u64,
    /// The number of bytes transferred so far.
    pub bytes_transferred: u64,
    /// The total number of bytes to transfer.
    pub bytes_total: u64,
}

/// Data for the event sent after fragments are transferred during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushFragmentEndEventData {
    /// The number of fragments transferred.
    pub fragments: u64,
    /// The number of bytes transferred.
    pub bytes_transferred: u64,
}

/// Data for the event reporting what a push cost.
///
/// Emitted once, when the push finishes, at statistics level one and above. A
/// push that failed reports what it had done by then. The counts are cumulative
/// across every revision, link and layer the push registers, where
/// [`LoreBranchPushFragmentProgressEventData`] reports the revision in flight.
///
/// A push stores no payload of its own: a fragment the peer was asked about is
/// deduplicated, copied or put, unless the push ended before it was reached.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushStatsEventData {
    /// Fragments the peer already held, so nothing was registered for them.
    pub deduplicated: u64,
    /// Fragments the peer duplicated an association for, sending no payload.
    pub copied: u64,
    /// Fragments whose payload was uploaded.
    pub put: u64,
}

/// Data for the event sent before a branch is created on the remote.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushBranchCreateBeginEventData {
    /// The repository the branch is created in.
    pub repository: RepositoryId,
    /// The branch being created.
    pub branch: BranchId,
    /// The local revision the branch starts from.
    pub local_revision: Hash,
}

/// Data for the event sent after a branch is created on the remote.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushBranchCreateEndEventData {
    /// The revision the branch points to on the remote.
    pub remote_revision: Hash,
}

/// Data for the event sent before a revision is pushed to the remote.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionPushBeginEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed to.
    pub branch: BranchId,
    /// The latest revision of the branch on the remote.
    pub remote_revision: Hash,
    /// The local revision being pushed.
    pub local_revision: Hash,
}

/// Data for the event sent when the remote assigns a pushed revision a new identity.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionPushUpdateEventData {
    /// The revision before the remote reassigned it.
    pub old_revision: Hash,
    /// The revision the remote assigned.
    pub new_revision: Hash,
    /// The sequential number of the new revision.
    pub new_revision_number: u64,
}

/// Data for the event sent after a revision is pushed to the remote.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionPushEndEventData {
    /// The repository that was pushed.
    pub repository: RepositoryId,
    /// The branch that was pushed to.
    pub branch: BranchId,
    /// The branch revision on the remote before the push.
    pub old_remote_revision: Hash,
    /// The branch revision on the remote after the push.
    pub new_remote_revision: Hash,
    /// The sequential number of the new remote revision.
    pub new_remote_revision_number: u64,
    /// A message returned by the remote for the push.
    pub message: LoreString,
    /// Set when the remote performed a fast-forward merge.
    #[serde(with = "u8_as_bool")]
    pub fast_forward_merged: u8,
}

#[error_set]
pub enum PushError {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    FileNotFound,
    RevisionNotFound,
    WriteRequired,
    Oversized,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    InvalidArguments,
    AlreadyLinked,
    LayerNotFound,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NotConnected,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

#[derive(Clone, Debug, Default)]
pub struct PushOptions {
    /// Branch to push, default to current branch if not set
    pub branch: Option<String>,
    /// Allow the server to fast-forward merge if the target branch head has moved
    pub fast_forward_merge: bool,
}

impl EventError for PushError {
    fn translated(&self) -> LoreError {
        match self {
            PushError::Disconnected(_) => LoreError::Connection,
            PushError::SlowDown(_) => LoreError::SlowDown,
            PushError::Oversized(_) => LoreError::Oversized,
            PushError::FileNotFound(_) => LoreError::FileNotFound,
            PushError::NotFound(_)
            | PushError::LayerNotFound(_)
            | PushError::RevisionNotFound(_) => LoreError::NotFound,
            PushError::AddressNotFound(_) => LoreError::AddressNotFound,
            PushError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            PushError::InvalidPath(_) | PushError::InvalidArguments(_) => {
                LoreError::InvalidArguments
            }
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// What a push does with the fragments the peer was asked about.
///
/// A push stores no payload of its own: the peer either already holds the
/// fragment, duplicates an association it holds under another context, or is sent
/// the payload. Shared behind an [`Arc`](std::sync::Arc) by every task the push
/// spawns, and cumulative across every revision, link and layer it registers.
pub(crate) struct PushStats {
    deduplicated: AtomicU64,
    copied: AtomicU64,
    put: AtomicU64,
    /// Payload bytes the uploads carried, which the per-revision progress event
    /// reports rather than the statistics one.
    put_bytes: AtomicU64,
    /// Whether to keep the count the statistics event alone reports. `copied` and
    /// `put` sum to the fragments a progress event reports as registered, so those
    /// two are kept whatever the level.
    statistics: bool,
}

impl PushStats {
    /// Counters for one push, keeping what the call's statistics level reports.
    pub(crate) fn new(statistics: bool) -> Self {
        Self {
            deduplicated: AtomicU64::new(0),
            copied: AtomicU64::new(0),
            put: AtomicU64::new(0),
            put_bytes: AtomicU64::new(0),
            statistics,
        }
    }

    /// `count` fragments the peer already held, so nothing was registered.
    fn deduplicated(&self, count: u64) {
        if self.statistics {
            self.deduplicated.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// The peer duplicated an association it already held, sending no payload.
    fn copied(&self) {
        self.copied.fetch_add(1, Ordering::Relaxed);
    }

    /// A payload was uploaded to the peer.
    fn put(&self, payload_bytes: u64) {
        self.put.fetch_add(1, Ordering::Relaxed);
        self.put_bytes.fetch_add(payload_bytes, Ordering::Relaxed);
    }

    /// Fragments registered with the peer, by copy or upload.
    fn registered(&self) -> u64 {
        self.copied.load(Ordering::Relaxed) + self.put.load(Ordering::Relaxed)
    }

    /// Payload bytes uploaded.
    fn put_bytes(&self) -> u64 {
        self.put_bytes.load(Ordering::Relaxed)
    }

    /// The counts, as an event payload.
    fn snapshot(&self) -> LoreBranchPushStatsEventData {
        LoreBranchPushStatsEventData {
            deduplicated: self.deduplicated.load(Ordering::Relaxed),
            copied: self.copied.load(Ordering::Relaxed),
            put: self.put.load(Ordering::Relaxed),
        }
    }
}

/// The in-flight revision's share of the push, for the per-revision progress
/// event.
///
/// Fragments registered and bytes uploaded are read out of the push-wide
/// [`PushStats`] as a delta against the baseline this revision started from, so
/// those facts are recorded in one place whichever event reports them.
/// `fragment_count` and `bytes_queued` are this revision's alone: the fragments it
/// has to register, and the payload bytes it has loaded to send.
pub(crate) struct PushProgress {
    fragment_count: AtomicUsize,
    bytes_queued: AtomicU64,
    registered_baseline: u64,
    put_bytes_baseline: u64,
    stats: Arc<PushStats>,
}

impl PushProgress {
    /// Start a revision's progress from where the push-wide counters stand now.
    pub(crate) fn new(stats: Arc<PushStats>) -> Self {
        Self {
            fragment_count: AtomicUsize::new(0),
            bytes_queued: AtomicU64::new(0),
            registered_baseline: stats.registered(),
            put_bytes_baseline: stats.put_bytes(),
            stats,
        }
    }

    fn set_fragment_count(&self, count: usize) {
        self.fragment_count.store(count, Ordering::Relaxed);
    }

    /// A payload was loaded and is about to be uploaded.
    fn payload_queued(&self, bytes: u64) {
        self.bytes_queued.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Fragments this revision has registered with the peer, by copy or upload.
    pub(crate) fn complete(&self) -> u64 {
        self.stats
            .registered()
            .saturating_sub(self.registered_baseline)
    }

    /// Fragments this revision has to register.
    pub(crate) fn count(&self) -> u64 {
        self.fragment_count.load(Ordering::Relaxed) as u64
    }

    fn event(&self) -> LoreBranchPushFragmentProgressEventData {
        LoreBranchPushFragmentProgressEventData {
            complete: self.complete(),
            count: self.fragment_count.load(Ordering::Relaxed) as u64,
            bytes_transferred: self
                .stats
                .put_bytes()
                .saturating_sub(self.put_bytes_baseline),
            bytes_total: self.bytes_queued.load(Ordering::Relaxed),
        }
    }
}

/// Emits what the push cost when dropped.
struct PushStatsReport;

impl PushStatsReport {
    /// A guard that emits the statistics event when dropped, so the push that
    /// fails reports as the one that succeeds does. Reporting turned off yields
    /// `None`.
    fn start() -> Option<Self> {
        execution_context().globals().stats().then_some(Self)
    }
}

impl Drop for PushStatsReport {
    fn drop(&mut self) {
        if let Some(snapshot) = push_stats_event() {
            event::LoreEvent::BranchPushStats(snapshot).send();
        }
    }
}

/// The push-wide counts, as an event payload, or `None` outside an execution
/// context — which is also no context to send an event through.
fn push_stats_event() -> Option<LoreBranchPushStatsEventData> {
    Some(
        crate::runtime::try_execution_context()?
            .push_stats()
            .snapshot(),
    )
}

pub async fn push(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: PushOptions,
) -> Result<(), PushError> {
    let _stats_report = PushStatsReport::start();

    let branch;
    let local_latest;
    if let Some(branch_identifier) = &options.branch {
        let status = branch::resolve(repository.clone(), branch_identifier.as_str())
            .await
            .forward::<PushError>("resolving branch identifier")?;
        if !status.local {
            return Err(PushError::internal(
                "Unable to push a branch that does not exist in local repository",
            ));
        }
        branch = status.id;
        local_latest = status.latest;
    } else {
        (local_latest, branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<PushError>("loading current anchor")?;
    }

    let state_current = State::deserialize(repository.clone(), local_latest)
        .await
        .forward::<PushError>("deserializing current state")?;

    collect_fragments_and_push(
        repository.clone(),
        token,
        options.clone(),
        state_current,
        branch,
        local_latest,
    )
    .await?;

    for (layer, repository) in layer::list_with_context(repository.clone())
        .await
        .unwrap_or_default()
    {
        let state_current = State::deserialize(repository.clone(), layer.current)
            .await
            .forward::<PushError>("deserializing layer state")?;

        collect_fragments_and_push(
            repository.clone(),
            token,
            options.clone(),
            state_current,
            branch,
            layer.current,
        )
        .await?;
    }

    let state_current = State::deserialize(repository.clone(), local_latest)
        .await
        .forward::<PushError>("re-deserializing current state for links")?;
    if let Ok(link_list) = state_current.link_list(repository.clone()).await {
        for link_reference in link_list.iter() {
            let link_repository =
                Arc::new(repository.to_link_context(link_reference.repository).await);
            let link_branch_id = link_reference.resolve_branch(branch);
            let link_local_latest = branch::load_latest(link_repository.clone(), link_branch_id)
                .await
                .unwrap_or_default();
            if link_local_latest.is_zero() {
                continue;
            }
            let link_state = State::deserialize(link_repository.clone(), link_local_latest)
                .await
                .forward::<PushError>("deserializing link state")?;

            collect_fragments_and_push(
                link_repository,
                token,
                options.clone(),
                link_state,
                link_branch_id,
                link_local_latest,
            )
            .await?;
        }
    }

    Ok(())
}

async fn collect_fragments_and_push(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: PushOptions,
    state: Arc<State>,
    branch: BranchId,
    local_latest: Hash,
) -> Result<(), PushError> {
    let remote = repository
        .remote()
        .await
        .forward::<PushError>("acquiring remote")?;

    let revision_protocol = remote
        .revision(repository.id)
        .await
        .forward::<PushError>("acquiring revision protocol")?;

    let correlation_id = execution_context().globals().correlation_id.to_string();
    let storage_protocol = remote
        .session(repository.id, &correlation_id)
        .await
        .forward::<PushError>("opening storage session")?;

    let repository_metadata = repository::metadata_hash(repository.clone())
        .await
        .forward::<PushError>("loading repository metadata hash")?;
    let repository_metadata = repository::metadata(repository.clone(), repository_metadata)
        .await
        .forward::<PushError>("loading repository metadata")?;
    let default_branch = repository_metadata.default_branch;

    let mut full_local_history = vec![];
    let mut full_remote_history = vec![];
    let mut current_branch_remote_history = vec![];
    let mut remote_revision = None;
    let mut current_branch = branch;
    let mut current_revision = local_latest;

    // Get remote branch info
    let (mut remote_latest, remote_metadata, remote_deleted) = match branch::load_remote(
        remote.clone(),
        repository.id,
        current_branch,
    )
    .await
    {
        Ok(status) => (status.latest, status.metadata, status.deleted),
        Err(err) if err.is_branch_not_found() => (Hash::default(), Hash::default(), false),
        Err(err) => {
            lore_debug!(
                "Failed to load remote branch info, assuming branch does not exist on remote: {err}"
            );
            (Hash::default(), Hash::default(), false)
        }
    };

    while remote_revision.is_none() {
        let current_remote_latest = if current_branch != branch {
            match branch::load_remote(remote.clone(), repository.id, current_branch).await {
                Ok(status) => status.latest,
                Err(err) if err.is_branch_not_found() => Hash::default(),
                Err(err) => {
                    lore_debug!(
                        "Failed to load remote branch info for {current_branch}, assuming branch does not exist on remote: {err}"
                    );
                    Hash::default()
                }
            }
        } else {
            remote_latest
        };

        let branch_metadata = branch::metadata(repository.clone(), current_branch)
            .await
            .forward::<PushError>("loading branch metadata")?;
        let branch_metadata =
            branch::branch_metadata(repository.clone(), current_branch, &branch_metadata)
                .await
                .forward::<PushError>("loading branch metadata")?;

        let default_branch_point = BranchPoint::default();

        lore_debug!("Walking history for branch {current_branch} at revision {current_revision}");

        if current_remote_latest.is_zero() && (current_branch != default_branch) {
            lore_debug!("Remote latest is zero, collect revisions and continue");
            let branch_point = branch_metadata
                .stack
                .first()
                .map_or(&default_branch_point, |parent| parent);
            if branch_point.revision.is_zero() {
                return Err(PushError::internal(
                    "Invalid branch data, unknown branch point",
                ));
            }

            let branch_point_state = State::deserialize(repository.clone(), branch_point.revision)
                .await
                .forward::<PushError>("deserializing branch point state")?;

            let mut local_revision = current_revision;
            while local_revision != branch_point.revision {
                let revision_state = State::deserialize(repository.clone(), local_revision)
                    .await
                    .forward::<PushError>("deserializing revision state")?;

                if revision_state.revision_number() < branch_point_state.revision_number() {
                    return Err(PushError::internal("Local branch metadata is out of date"));
                }

                full_local_history.push(local_revision);
                local_revision = revision_state.parent_self();
            }

            current_revision = branch_point.revision;
            current_branch = branch_point.branch;

            // Early out - if the parent branch latest revision is convergent, it is known
            // to have been pushed and validated at some point. We don't need to iterate further
            // in that case, since there are no potentially missing fragments from this point
            if !branch::load_latest_divergent(repository.clone(), current_branch)
                .await
                .unwrap_or(true)
            {
                lore_debug!(
                    "Parent branch is known to be convergent, stop iterating revisions to push"
                );
                break;
            }
        } else if (current_remote_latest != state.parent_self()
            && current_remote_latest != state.parent_other())
            || current_remote_latest.is_zero()
        {
            lore_debug!("Found remote latest or reached initial branch");

            let (_branch_point, remote_history, local_history) = history::find_branch_point(
                repository.clone(),
                current_remote_latest,
                current_revision,
            )
            .await
            .forward::<PushError>("reconciling branch history")?;

            full_local_history.extend(local_history.clone());
            full_remote_history.extend(remote_history.clone());

            if current_branch == branch {
                current_branch_remote_history = remote_history;
            }

            // Either the remote latest was found or there is none
            if !current_remote_latest.is_zero() {
                remote_revision = Some(current_remote_latest);
                lore_debug!("Found remote latest {remote_latest}");
            } else {
                lore_debug!("Remote latest is zero, reached initial branch");
                break;
            }
        } else {
            lore_debug!("Only single revision to push");

            full_local_history.push(current_revision);
            remote_revision = Some(current_remote_latest);
        }
    }

    // Check if revision is already pushed and there is nothing to do
    let already_pushed = remote_latest == local_latest;

    let branch_metadata = branch::metadata(repository.clone(), branch)
        .await
        .forward::<PushError>("loading branch metadata")?;
    let branch_metadata = branch::branch_metadata(repository.clone(), branch, &branch_metadata)
        .await
        .forward::<PushError>("loading branch metadata")?;

    event::LoreEvent::BranchPush(LoreBranchPushEventData {
        remote: remote.remote_url().into(),
        repository: repository.id,
        branch,
        branch_name: branch_metadata.name.as_str().into(),
        remote_revision: remote_revision.unwrap_or_default(),
        local_revision: local_latest,
        remote_history: full_remote_history.len() as u64,
        local_history: full_local_history.len() as u64,
        flag_already_pushed: already_pushed.into(),
        flag_default: (branch == default_branch).into(),
        flag_link: repository.is_link().into(),
        flag_layer: repository.is_layer().into(),
    })
    .send();

    let dry_run = execution_context().globals().dry_run();

    // If the revision is already pushed and the branch still exists, early out.
    // If the branch was deleted, restore it via branch_create before returning.
    if already_pushed {
        if remote_deleted && !dry_run {
            lore_debug!("Branch deleted on server with same latest, restoring via branch_create");
            revision_protocol
                .branch_create(
                    branch,
                    branch_metadata.name.as_str(),
                    branch_metadata.category.as_str(),
                    branch_metadata.creator.as_str(),
                    &branch_metadata.stack,
                )
                .await
                .forward::<PushError>("creating branch on remote")?;
        }
        return Ok(());
    }

    // If the branch diverged, early out (unless fast-forward merge is enabled,
    // in which case let the server attempt to resolve the divergence)
    let force = execution_context().globals().force();
    if !current_branch_remote_history.is_empty()
        && !force
        && !options.fast_forward_merge
        && !repository.is_link()
    {
        lore_debug!(
            "Branch divergence detected, {} remote changes",
            current_branch_remote_history.len()
        );
        return Err(PushError::internal(
            "Branch has diverged, sync to merge remote changes",
        ));
    }

    // If force pushing a current revision that's already pushed, add it
    if full_local_history.is_empty() && !local_latest.is_zero() && force {
        lore_debug!(
            "Branch push of old revision detected, {} remote changes",
            full_remote_history.len()
        );
        full_local_history.push(local_latest);
    }

    // If the branch was deleted on the server, restore it via branch_create
    if remote_deleted && !dry_run {
        lore_debug!("Branch deleted on server, restoring via branch_create before push");
        revision_protocol
            .branch_create(
                branch,
                branch_metadata.name.as_str(),
                branch_metadata.category.as_str(),
                branch_metadata.creator.as_str(),
                &branch_metadata.stack,
            )
            .await
            .forward::<PushError>("creating branch on remote")?;
    }

    // If this is the initial push of a branch, create it
    if remote_metadata.is_zero() {
        let branch_point = if let Some(parent) = branch_metadata.stack.first() {
            parent.revision
        } else {
            Hash::default()
        };

        event::LoreEvent::BranchPushBranchCreateBegin(LoreBranchPushBranchCreateBeginEventData {
            repository: repository.id,
            branch,
            local_revision: branch_point,
        })
        .send();

        if !dry_run {
            remote_latest = revision_protocol
                .branch_create(
                    branch,
                    branch_metadata.name.as_str(),
                    branch_metadata.category.as_str(),
                    branch_metadata.creator.as_str(),
                    &branch_metadata.stack,
                )
                .await
                .forward::<PushError>("creating branch on remote")?;

            if remote_latest != branch_point {
                return Err(PushError::internal(format!(
                    "Failed to create branch {}, remote latest now at {}",
                    branch_metadata.name.clone(),
                    remote_latest
                )));
            }

            branch::store_last_sync(repository.clone(), branch, branch_point).await;
        } else {
            // Report the revision the branch creation would yield.
            remote_latest = branch_point;
        }

        event::LoreEvent::BranchPushBranchCreateEnd(LoreBranchPushBranchCreateEndEventData {
            remote_revision: remote_latest,
        })
        .send();
    }

    let mut current_latest = Hash::default();
    let mut fast_forward_merged = false;
    for current_revision in full_local_history.iter().rev() {
        let mut current_revision = *current_revision;

        let state = State::deserialize(repository.clone(), current_revision)
            .await
            .forward::<PushError>("deserializing revision state")?;

        // Push links
        if let Ok(link_list) = state.link_list(repository.clone()).await {
            // TODO(vri): UCS-17135 - Push links in individual tasks
            for link_reference in link_list.iter() {
                let link_id = link_reference.repository;
                let link_repository = Arc::new(repository.to_link_context(link_id).await);
                let link_signature = link_reference.signature;
                let link_state = State::deserialize(link_repository.clone(), link_signature)
                    .await
                    .forward::<PushError>("deserializing link state")?;

                let link_branch_id = link_reference.resolve_branch(branch);

                lore_debug!(
                    "Pushing link changes for link ID {link_id} on branch {link_branch_id} at revision {link_signature}"
                );

                if collect_fragments_and_push_recurse(
                    link_repository,
                    token.share(),
                    options.clone(),
                    link_state,
                    link_branch_id,
                    link_reference.signature,
                )
                .await
                .is_err()
                {
                    return Err(PushError::internal(format!(
                        "Failed to push link with ID {link_id}"
                    )));
                }
            }
        }

        if !current_latest.is_zero() && state.parent_self() != current_latest {
            // Rebase on new latest revision
            // TODO(mjansson): This only handles revision number rewrite for now, implement proper
            //                 automatic rebase if the push resulted in a clean rebase
            // ...

            event::LoreEvent::BranchPushRevisionUpdateBegin(
                LoreBranchPushRevisionUpdateBeginEventData {
                    revision: state.revision(),
                    old_parent: state.parent_self(),
                    new_parent: current_latest,
                },
            )
            .send();

            state.set_parent_self(current_latest);
            current_revision = state
                .serialize(repository.clone(), token)
                .await
                .forward::<PushError>("serializing state")?;

            event::LoreEvent::BranchPushRevisionUpdateEnd(
                LoreBranchPushRevisionUpdateEndEventData {
                    revision: current_revision,
                },
            )
            .send();
        }

        // Load parent state
        let state_parent = State::deserialize(repository.clone(), state.parent_self())
            .await
            .forward::<PushError>("deserializing parent state")?;

        // Check missing fragments on server
        lore_debug!(
            "Calculating new fragments from {} to {}",
            state_parent.revision(),
            state.revision()
        );
        let mut fragments = state::collect_new_fragments(
            repository.clone(),
            state_parent.clone(),
            state.clone(),
            true, /* Ignore already durably stored fragments */
        )
        .await
        .forward::<PushError>("collecting new fragments")?;

        if !state.parent_other().is_zero() {
            fragments.push(Address::zero_context_hash(state.parent_other()));
        }

        let push_stats = execution_context().push_stats().clone();
        let progress = Arc::new(PushProgress::new(push_stats.clone()));
        let fragments = push_query(
            storage_protocol.clone(),
            fragments,
            remote.environment.max_query_batch(),
            &push_stats,
        )
        .await?;

        event::LoreEvent::BranchPushFragmentBegin(LoreBranchPushFragmentBeginEventData {
            fragments: fragments.len() as u64,
            bytes_total: 0,
        })
        .send();

        let ticker_progress = progress.clone();
        let progress_interval = execution_context().globals().event_interval();
        let ticker = AbortOnDropHandle::new(lore_spawn!(async move {
            let mut ticker = tokio::time::interval(progress_interval);
            loop {
                ticker.tick().await;
                event::LoreEvent::BranchPushFragmentProgress(ticker_progress.event()).send();
            }
        }));

        if !dry_run {
            push_fragments(
                repository.clone(),
                storage_protocol.clone(),
                fragments,
                progress.clone(),
            )
            .await?;
        }

        drop(ticker);

        // Emit a final progress event with the completed values now that the
        // ticker has been dropped and push_fragments has finished.
        let final_progress = progress.event();
        event::LoreEvent::BranchPushFragmentProgress(final_progress.clone()).send();

        event::LoreEvent::BranchPushFragmentEnd(LoreBranchPushFragmentEndEventData {
            fragments: final_progress.complete,
            bytes_transferred: final_progress.bytes_transferred,
        })
        .send();

        // We don't want to push revisions from any other branch than the current one,
        // so we will early out here
        if state.branch(repository.clone()).await != branch {
            continue;
        };

        event::LoreEvent::BranchPushRevisionPushBegin(LoreBranchPushRevisionPushBeginEventData {
            repository: repository.id,
            branch,
            remote_revision: remote_latest,
            local_revision: current_revision,
        })
        .send();

        // Push new latest to remote
        let current_remote = remote_latest;
        let mut response_message = None;

        if !dry_run && remote_latest != current_revision {
            let push_result = revision_protocol
                .branch_push(branch, current_revision, force, options.fast_forward_merge)
                .await;

            // If the server returns NotFound, the branch was deleted on the server.
            // Recreate it via branch_create and retry the push.
            let response = match push_result {
                Err(ProtocolError::NotFound(_)) => {
                    lore_debug!("Branch push returned NotFound, recreating branch on server");

                    event::LoreEvent::BranchPushBranchCreateBegin(
                        LoreBranchPushBranchCreateBeginEventData {
                            repository: repository.id,
                            branch,
                            local_revision: remote_latest,
                        },
                    )
                    .send();

                    revision_protocol
                        .branch_create(
                            branch,
                            branch_metadata.name.as_str(),
                            branch_metadata.category.as_str(),
                            branch_metadata.creator.as_str(),
                            &branch_metadata.stack,
                        )
                        .await
                        .forward::<PushError>("creating branch on remote")?;

                    event::LoreEvent::BranchPushBranchCreateEnd(
                        LoreBranchPushBranchCreateEndEventData {
                            remote_revision: remote_latest,
                        },
                    )
                    .send();

                    revision_protocol
                        .branch_push(branch, current_revision, force, options.fast_forward_merge)
                        .await
                        .forward::<PushError>("pushing branch to remote")?
                }
                result => result.forward::<PushError>("pushing branch to remote")?,
            };
            if response.fast_forward_merged {
                // Server performed a fast-forward merge — push succeeded with a new revision.
                // Store the server-created revision as local latest (marked divergent since the
                // local working directory still reflects the original merge revision).
                let local_latest = branch::load_latest(repository.clone(), branch)
                    .await
                    .unwrap_or_default();
                branch::store_latest(
                    repository.clone(),
                    branch,
                    local_latest,
                    response.revision,
                    BranchLatestStatus::Divergent,
                )
                .await
                .forward::<PushError>("setting new latest revision for branch")?;
                branch::store_last_sync(repository.clone(), branch, response.revision).await;

                remote_latest = response.revision;
                current_latest = response.revision;

                event::LoreEvent::BranchPushRevisionPushEnd(
                    LoreBranchPushRevisionPushEndEventData {
                        repository: repository.id,
                        branch,
                        old_remote_revision: current_remote,
                        new_remote_revision: current_latest,
                        new_remote_revision_number: response.revision_number,
                        message: response.message.unwrap_or_default().into(),
                        fast_forward_merged: 1,
                    },
                )
                .send();

                // Skip the normal post-push processing — do not update anchor
                // or working directory. A subsequent `urc sync` will handle that.
                fast_forward_merged = true;
                continue;
            }
            if response.revision_number == 0 {
                if options.fast_forward_merge {
                    return Err(PushError::internal(
                        "Fast-forward merge failed due to conflicts, sync and merge locally to resolve",
                    ));
                }
                return Err(PushError::internal(format!(
                    "Remote latest has moved to {} and automatic rebase not possible",
                    response.revision
                )));
            }
            if response.revision != current_revision {
                event::LoreEvent::BranchPushRevisionPushUpdate(
                    LoreBranchPushRevisionPushUpdateEventData {
                        old_revision: current_revision,
                        new_revision: response.revision,
                        new_revision_number: response.revision_number,
                    },
                )
                .send();
            }
            response_message = response.message;

            remote_latest = response.revision;
            current_latest = response.revision;
        } else {
            current_latest = current_revision;
        }

        let current_number = State::deserialize(repository.clone(), current_latest)
            .await
            .forward::<PushError>("deserializing current latest state")?
            .revision_number();

        event::LoreEvent::BranchPushRevisionPushEnd(LoreBranchPushRevisionPushEndEventData {
            repository: repository.id,
            branch,
            old_remote_revision: current_remote,
            new_remote_revision: current_latest,
            new_remote_revision_number: current_number,
            message: response_message.unwrap_or_default().into(),
            fast_forward_merged: 0,
        })
        .send();

        if !dry_run {
            branch::store_last_sync(repository.clone(), branch, current_latest).await;
        }
    }

    lore_debug!(
        "All revisions pushed, updating current local latest to {}",
        current_latest
    );
    if !current_latest.is_zero()
        && !repository.is_layer()
        && !repository.is_link()
        && !fast_forward_merged
        && !dry_run
    {
        let local_latest = branch::load_latest(repository.clone(), branch)
            .await
            .unwrap_or_default();
        branch::store_latest(
            repository.clone(),
            branch,
            local_latest,
            current_latest,
            BranchLatestStatus::Convergent,
        )
        .await
        .forward::<PushError>("setting new latest revision for branch")?;

        branch::store_last_sync(repository.clone(), branch, current_latest).await;
    }

    Ok(())
}

fn collect_fragments_and_push_recurse(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    options: PushOptions,
    state: Arc<State>,
    branch: BranchId,
    local_latest: Hash,
) -> Pin<Box<dyn Future<Output = Result<(), PushError>> + Send>> {
    Box::pin(async move {
        collect_fragments_and_push(repository, &token, options, state, branch, local_latest).await
    })
}

pub const RETRY_START_DURATION: u64 = 100;
pub const RETRY_MAX_DURATION: u64 = 10_000;
pub const RETRY_MAX_ATTEMPTS: usize = 10;

/// What the peer answered about the fragments a push is about to send, split by what it takes to
/// register each one.
///
/// A full match needs no transfer, but it is still an answer worth keeping. The rest divide by
/// whether the peer already holds the bytes: it either has to be sent them, or it has an
/// association for the same hash and can duplicate that instead.
#[derive(Debug, Default)]
pub(crate) struct PushQueryResult {
    /// The peer holds nothing for these, so their payloads have to be transferred.
    pub absent: Vec<Address>,
    /// The partition already holds these hashes under another context, so an association can be
    /// duplicated rather than the payload sent again.
    pub copyable: Vec<Address>,
    /// The peer already holds these exactly. Nothing is transferred, but the local entries still
    /// have to be marked durable — see [`mark_present_durable`].
    pub present: Vec<Address>,
}

impl PushQueryResult {
    /// Counts only what has to be transferred. `present` is deliberately excluded. Drives the
    /// fragment total in the push progress events.
    pub fn len(&self) -> usize {
        self.absent.len() + self.copyable.len()
    }
}

/// Sort one batch's answers into what the peer needs from us: nothing for an association it
/// already holds, a duplicated association where it holds the hash under another context, and the
/// payload where it holds neither.
fn classify_query_batch(batch: &[Address], statuses: &Bytes, queried: &mut PushQueryResult) {
    for (address, status) in batch.iter().zip(statuses.iter()) {
        match QueryStatus::from(*status) {
            QueryStatus::ExistFullMatch => queried.present.push(*address),
            QueryStatus::ExistPartitionMatch => queried.copyable.push(*address),
            QueryStatus::NotFound => queried.absent.push(*address),
        }
    }
}

pub(crate) async fn push_query(
    storage: Arc<StorageSession>,
    addresses: Vec<Address>,
    max_batch_size: Option<usize>,
    stats: &PushStats,
) -> Result<PushQueryResult, PushError> {
    if addresses.is_empty() {
        return Ok(PushQueryResult::default());
    }

    let address_count = addresses.len();

    const MAX_TASK_COUNT: usize = 1000;

    let mut tasks = JoinSet::new();
    let mut remain = addresses;

    let mut failure = None;
    let mut queried = PushQueryResult::default();
    let mut retry =
        crate::util::time::retry(RETRY_START_DURATION, RETRY_MAX_DURATION, RETRY_MAX_ATTEMPTS);
    let max_batch_size = if let Some(max_batch_size) = max_batch_size
        && max_batch_size > 0
    {
        max_batch_size
    } else {
        1000
    };

    fn handle_join_result(
        result: Result<(Vec<Address>, Result<Bytes, ProtocolError>), JoinError>,
        remain: &mut Vec<Address>,
        queried: &mut PushQueryResult,
    ) -> Result<(), PushError> {
        let (mut batch, result) = result.internal("query task panicked")?;
        match result {
            Ok(result) => {
                classify_query_batch(&batch, &result, queried);
                Ok(())
            }
            Err(ProtocolError::SlowDown(_)) => {
                remain.append(&mut batch);
                Ok(())
            }
            err => err
                .map(|_| ())
                .forward::<PushError>("querying server for existing fragments"),
        }
    }

    while !remain.is_empty() {
        while !remain.is_empty() && failure.is_none() {
            let mut batch = remain.split_off(remain.len().saturating_sub(max_batch_size));

            let storage = storage.clone();
            lore_spawn!(tasks, async move {
                batch.sort_unstable();
                batch.dedup();

                let result = storage.query(batch.as_slice()).await;
                (batch, result)
            });

            while failure.is_none()
                && tasks.len() > MAX_TASK_COUNT
                && let Some(result) = tasks.join_next().await
            {
                failure = handle_join_result(result, remain.as_mut(), &mut queried).err();
            }
        }

        while let Some(result) = tasks.join_next().await {
            if failure.is_none() {
                failure = handle_join_result(result, remain.as_mut(), &mut queried).err();
            }
        }

        if let Some(failure) = failure {
            return Err(failure);
        }

        if !remain.is_empty() && !retry.wait().await {
            return Err(PushError::internal(
                "Failed to query server for existing fragments",
            ));
        }
    }

    queried.absent.sort_unstable();
    queried.absent.dedup();
    queried.copyable.sort_unstable();
    queried.copyable.dedup();
    queried.present.sort_unstable();
    queried.present.dedup();

    stats.deduplicated(queried.present.len() as u64);

    lore_debug!(
        "Queried {} fragments, {} to upload, {} the peer can duplicate an association for, {} the peer already holds",
        address_count,
        queried.absent.len(),
        queried.copyable.len(),
        queried.present.len()
    );

    Ok(queried)
}

/// Record locally that the peer holds this address, which is what keeps the next push from
/// offering it again.
///
/// The payload stays where it is, so this writes the header alone.
async fn mark_durable(repository: &Arc<RepositoryContext>, address: Address, fragment: Fragment) {
    let mut fragment = fragment;
    fragment.flags |= fragment::FragmentFlags::PayloadStoredDurable;
    let _ = repository
        .immutable_store()
        .put(repository.id, address, fragment, None, false)
        .await;
}

/// Ask the peer to duplicate an association it already holds for this hash, reporting whether it
/// did.
///
/// The source names no context because the query did not say which one the peer matched under —
/// only that the partition holds the hash — and that is exactly what a zero source context asks
/// for. A refusal costs the round trip and leaves the payload to be uploaded as before.
async fn duplicate_association(
    repository: &Arc<RepositoryContext>,
    storage: &Arc<StorageSession>,
    address: Address,
    stats: &PushStats,
) -> bool {
    if !storage.can_copy_from(repository.id).await {
        return false;
    }
    if let Err(err) = storage
        .copy(
            repository.id,
            Address::zero_context_hash(address.hash),
            address.context,
        )
        .await
    {
        lore_debug!("Copy of {address} refused ({err:?}), uploading instead");
        return false;
    }

    stats.copied();
    mark_stored_durable(repository, address).await;
    true
}

/// Mark the local entry for `address` durable, looking up the fragment it needs to write back.
///
/// Shared by every path that learns the peer holds the address without having uploaded a payload
/// for it in this call — a duplicated association, and a query that answered `ExistFullMatch`.
/// Only metadata is read: the payload is already local and does not need loading to set a flag on
/// its entry.
async fn mark_stored_durable(repository: &Arc<RepositoryContext>, address: Address) {
    if let Ok(data) = repository
        .immutable_store()
        .get_metadata(repository.id, address)
        .await
        && data.match_made == StoreMatch::MatchFull
    {
        mark_durable(repository, address, data.fragment).await;
    }
}

/// Mark the local entries for addresses the peer already holds durable.
///
/// `ExistFullMatch` establishes the same fact an upload does. An entry that never records it is
/// pinned against eviction, excluded from the store's size and capacity totals, and re-queried on
/// every subsequent push.
async fn mark_present_durable(repository: &Arc<RepositoryContext>, present: Vec<Address>) {
    const MAX_PARALLEL_MARK: usize = 1000;

    let mut tasks: JoinSet<()> = JoinSet::new();
    for address in present {
        if address.hash.is_zero() {
            continue;
        }

        while tasks.len() >= MAX_PARALLEL_MARK {
            let _ = tasks.join_next().await;
        }

        let repository = repository.clone();
        lore_spawn!(tasks, async move {
            mark_stored_durable(&repository, address).await;
        });
    }
    while tasks.join_next().await.is_some() {}
}

/// Register every fragment the peer is missing, transferring a payload only where it has no
/// association to duplicate. Addresses the peer already holds are marked durable alongside.
pub(crate) async fn push_fragments(
    repository: Arc<RepositoryContext>,
    storage: Arc<StorageSession>,
    fragments: PushQueryResult,
    progress: Arc<PushProgress>,
) -> Result<(), PushError> {
    let fragment_count = fragments.len();
    let PushQueryResult {
        absent,
        copyable,
        present,
    } = fragments;

    progress.set_fragment_count(fragment_count);

    let stats = execution_context().push_stats().clone();
    let marking = {
        let repository = repository.clone();
        lore_spawn!(async move { mark_present_durable(&repository, present).await })
    };

    const MAX_PARALLEL_PUT: usize = 10000;

    let mut tasks: JoinSet<Result<(), PushError>> = JoinSet::new();
    let mut failure = None;
    for (address, duplicable) in copyable
        .into_iter()
        .map(|address| (address, true))
        .chain(absent.into_iter().map(|address| (address, false)))
    {
        if address.hash.is_zero() {
            debug_assert!(
                !address.hash.is_zero(),
                "Zero hash address in list of fragments to push"
            );
            continue;
        }

        let repository = repository.clone();
        let storage = storage.clone();
        let stats = stats.clone();
        let progress = progress.clone();
        lore_spawn!(tasks, async move {
            if duplicable && duplicate_association(&repository, &storage, address, &stats).await {
                return Ok(());
            }

            let (fragment, payload) = immutable::load_raw_store_retry(
                repository.immutable_store(),
                repository.id,
                address,
            )
            .await
            .forward::<PushError>("loading fragment payload")?;

            let payload_size = payload.len() as u64;
            progress.payload_queued(payload_size);

            immutable::store_raw_remote_retry(storage.clone(), address, fragment, Some(payload))
                .await
                .map_err(|err| {
                    if err.is_disconnected() {
                        PushError::from(Disconnected)
                    } else {
                        PushError::internal_with_context(err, "putting fragment to remote")
                    }
                })?;

            stats.put(payload_size);

            mark_durable(&repository, address, fragment).await;

            Ok(())
        });

        while let Some(result) = tasks.try_join_next() {
            failure = failure.or(result
                .map_err(|e| PushError::internal_with_context(e, "fragment task panicked"))
                .flatten()
                .err());
        }
        while tasks.len() > MAX_PARALLEL_PUT
            && let Some(result) = tasks.join_next().await
        {
            failure = failure.or(result
                .map_err(|e| PushError::internal_with_context(e, "fragment task panicked"))
                .flatten()
                .err());
        }
        if failure.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        failure = failure.or(result
            .map_err(|e| PushError::internal_with_context(e, "fragment task panicked"))
            .flatten()
            .err());
    }

    let _ = marking.await;

    if let Some(err) = failure {
        return Err(err);
    }

    lore_debug!("Pushed {} fragments", fragment_count);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(seed: u8) -> Address {
        Address {
            hash: Hash::from([seed; 32]),
            context: crate::lore::Context::from([seed; 16]),
        }
    }

    /// Statistics level zero reports nothing, so it keeps nothing beyond what a
    /// progress event reads: the fragments registered, which `copied` and `put`
    /// sum to, and the bytes uploaded.
    #[test]
    fn a_count_is_kept_only_where_something_reports_it() {
        for (statistics, deduplicated) in [(false, 0), (true, 3)] {
            let stats = PushStats::new(statistics);
            stats.deduplicated(3);
            stats.copied();
            stats.put(64);

            let counts = stats.snapshot();
            assert_eq!(counts.deduplicated, deduplicated, "statistics {statistics}");
            assert_eq!(counts.copied, 1, "statistics {statistics}");
            assert_eq!(counts.put, 1, "statistics {statistics}");
            assert_eq!(stats.registered(), 2, "statistics {statistics}");
            assert_eq!(stats.put_bytes(), 64, "statistics {statistics}");
        }
    }

    /// What the push does with a fragment is decided entirely by the status byte the peer answered
    /// with, so this is where the copy path is chosen or missed.
    mod classify {
        use super::*;

        fn classify(statuses: &[u8]) -> PushQueryResult {
            let batch: Vec<Address> = (0..statuses.len() as u8).map(address).collect();
            let mut queried = PushQueryResult::default();
            classify_query_batch(&batch, &Bytes::copy_from_slice(statuses), &mut queried);
            queried
        }

        /// A full match transfers nothing, but the answer still has to be kept: it is what tells
        /// the local store the payload is safe elsewhere, and an entry that never learns that is
        /// pinned against eviction and invisible to both store caps for the rest of its life.
        #[test]
        fn an_association_the_peer_holds_transfers_nothing_but_is_recorded() {
            let queried = classify(&[QueryStatus::ExistFullMatch as u8]);
            assert_eq!(queried.len(), 0, "nothing to transfer");
            assert_eq!(queried.present, vec![address(0)]);
        }

        /// The change this path exists for: the partition holds the hash, so the peer is asked to
        /// duplicate the association rather than sent the payload it already has.
        #[test]
        fn a_partition_match_is_copied_rather_than_uploaded() {
            let queried = classify(&[QueryStatus::ExistPartitionMatch as u8]);
            assert_eq!(queried.copyable, vec![address(0)]);
            assert!(queried.absent.is_empty());
        }

        #[test]
        fn a_miss_is_uploaded() {
            let queried = classify(&[QueryStatus::NotFound as u8]);
            assert_eq!(queried.absent, vec![address(0)]);
            assert!(queried.copyable.is_empty());
        }

        /// A status the client does not know must not be read as "the peer has it" — that would
        /// drop the fragment from the push and leave the revision unreadable on the peer.
        #[test]
        fn an_unknown_status_is_uploaded() {
            let queried = classify(&[2, 7, 255]);
            assert_eq!(queried.absent.len(), 3);
            assert!(queried.copyable.is_empty());
        }

        #[test]
        fn a_batch_is_split_by_status_in_order() {
            let queried = classify(&[
                QueryStatus::NotFound as u8,
                QueryStatus::ExistFullMatch as u8,
                QueryStatus::ExistPartitionMatch as u8,
                QueryStatus::NotFound as u8,
                QueryStatus::ExistPartitionMatch as u8,
            ]);
            assert_eq!(queried.absent, vec![address(0), address(3)]);
            assert_eq!(queried.copyable, vec![address(2), address(4)]);
            assert_eq!(queried.present, vec![address(1)]);
            assert_eq!(queried.len(), 4, "len counts only what is transferred");
        }

        /// Each status answers the address at its own position, so a batch where only some entries
        /// are copyable must not shift the rest.
        #[test]
        fn statuses_line_up_with_the_addresses_they_answer() {
            let queried = classify(&[
                QueryStatus::ExistPartitionMatch as u8,
                QueryStatus::ExistFullMatch as u8,
                QueryStatus::NotFound as u8,
            ]);
            assert_eq!(queried.copyable, vec![address(0)]);
            assert_eq!(queried.present, vec![address(1)]);
            assert_eq!(queried.absent, vec![address(2)]);
        }
    }

    /// Recording a full match is what takes the fragment off the local store's protected list, so
    /// this is where a push stops re-offering content the peer already holds.
    mod mark {
        use super::*;

        const DURABLE: u32 = fragment::FragmentFlags::PayloadStoredDurable.bits();

        /// A repository over in-memory stores, holding only what the test puts in it.
        async fn null_repository() -> Arc<RepositoryContext> {
            let immutable_store = lore_storage::local::immutable_store::create(
                None::<&str>,
                lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
                false,
                lore_storage::ImmutableStoreSettings::default(),
            )
            .await
            .expect("in-memory immutable store");
            let mutable_store = lore_storage::local::mutable_store::create(
                None::<&str>,
                lore_storage::MutableStoreSettings::default(),
                immutable_store.clone(),
            )
            .await
            .expect("in-memory mutable store");

            Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
            ))
        }

        /// Store a payload under `address` carrying no durability, as a local commit leaves it.
        async fn store_local(repository: &Arc<RepositoryContext>, address: Address) {
            let payload = Bytes::from_static(b"payload");
            let fragment = Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            };
            repository
                .immutable_store()
                .put(repository.id, address, fragment, Some(payload), false)
                .await
                .expect("storing a local payload");
        }

        /// The flags the store holds for `address`.
        async fn stored_flags(repository: &Arc<RepositoryContext>, address: Address) -> u32 {
            repository
                .immutable_store()
                .get_metadata(repository.id, address)
                .await
                .expect("the store holds the address")
                .fragment
                .flags
        }

        /// The fact a full match establishes: the payload is safe on the peer, so the local entry
        /// is no longer the only copy.
        #[tokio::test]
        async fn a_present_address_becomes_durable() {
            let repository = null_repository().await;
            let present = address(1);
            store_local(&repository, present).await;
            assert_eq!(
                stored_flags(&repository, present).await & DURABLE,
                0,
                "a locally stored payload starts out non-durable"
            );

            mark_present_durable(&repository, vec![present]).await;

            assert_eq!(stored_flags(&repository, present).await & DURABLE, DURABLE);
        }

        /// An address the store cannot describe answers nothing to write back, and must not cost
        /// the addresses it can describe their record.
        #[tokio::test]
        async fn a_batch_marks_what_it_can_and_skips_the_rest() {
            let repository = null_repository().await;
            let present = address(2);
            store_local(&repository, present).await;

            mark_present_durable(&repository, vec![address(0), address(3), present]).await;

            assert_eq!(
                stored_flags(&repository, present).await & DURABLE,
                DURABLE,
                "a zero hash and an address the store never held are skipped, not fatal"
            );
        }
    }
}
