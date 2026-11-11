// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream;
use futures::stream::FuturesUnordered;
use lore_base::lore_spawn;
use lore_base::types::BranchMetadata;
use lore_error_set::prelude::*;
use lore_storage::CopyOutcome;
use lore_transport::BranchPushResponse;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::Revision;
use lore_transport::StorageSession;
use lore_transport::quic::storage_service::QueryStatus;
use serde::Serialize;
use tokio::sync::Semaphore;
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
use crate::lore_error;
use crate::repository;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state;
use crate::state::Collection;
use crate::state::LinkReference;
use crate::state::State;
use crate::store::StoreMatch;
use crate::util::request_tracker::StoreRequestTracker;
use crate::util::serde::u8_as_bool;

/// Data for the event sent when a branch push starts.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionUpdateBeginEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
    /// The revision being updated.
    pub revision: Hash,
    /// The previous parent revision.
    pub old_parent: Hash,
    /// The new parent revision.
    pub new_parent: Hash,
}

/// Data for the event sent after a revision's parent is rewritten during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionUpdateEndEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
    /// The updated revision.
    pub revision: Hash,
}

/// Data for the event sent before fragments are transferred during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushFragmentBeginEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
    /// The number of fragments to transfer.
    pub fragments: u64,
    /// The total number of bytes to transfer.
    pub bytes_total: u64,
}

/// Data for the event sent as fragments are transferred during push.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushFragmentProgressEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushFragmentEndEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
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
/// `lore_branch_push_fragment_progress_event_data_t` reports one revision's share.
///
/// A push stores no payload of its own: a fragment the peer was asked about is
/// deduplicated, copied or put, unless the push ended before it was reached.
#[repr(C)]
#[derive(
    Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, bitcode::Encode, bitcode::Decode,
)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushBranchCreateEndEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
    /// The revision the branch points to on the remote.
    pub remote_revision: Hash,
}

/// Data for the event sent before a revision is pushed to the remote.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreBranchPushRevisionPushUpdateEventData {
    /// The repository being pushed.
    pub repository: RepositoryId,
    /// The branch being pushed.
    pub branch: BranchId,
    /// The revision before the remote reassigned it.
    pub old_revision: Hash,
    /// The revision the remote assigned.
    pub new_revision: Hash,
    /// The sequential number of the new revision.
    pub new_revision_number: u64,
}

/// Data for the event sent after a revision is pushed to the remote.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
    LinkDivergent,
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
#[lore_macro::test_pub]
pub(crate) struct PushStats {
    deduplicated: AtomicU64,
    copied: AtomicU64,
    put: AtomicU64,
    /// Whether to keep the counts, which only the statistics event reports.
    statistics: bool,
}

impl PushStats {
    /// Counters for one push, keeping what the call's statistics level reports.
    #[lore_macro::test_pub]
    pub(crate) fn new(statistics: bool) -> Self {
        Self {
            deduplicated: AtomicU64::new(0),
            copied: AtomicU64::new(0),
            put: AtomicU64::new(0),
            statistics,
        }
    }

    /// `count` fragments the peer already held, so nothing was registered.
    #[lore_macro::test_pub]
    fn deduplicated(&self, count: u64) {
        if self.statistics {
            self.deduplicated.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// The peer duplicated an association it already held, sending no payload.
    #[lore_macro::test_pub]
    fn copied(&self) {
        if self.statistics {
            self.copied.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A payload was uploaded to the peer.
    #[lore_macro::test_pub]
    fn put(&self) {
        if self.statistics {
            self.put.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The counts, as an event payload.
    #[lore_macro::test_pub]
    fn snapshot(&self) -> LoreBranchPushStatsEventData {
        LoreBranchPushStatsEventData {
            deduplicated: self.deduplicated.load(Ordering::Relaxed),
            copied: self.copied.load(Ordering::Relaxed),
            put: self.put.load(Ordering::Relaxed),
        }
    }
}

/// One revision's share of the push, for the per-revision progress event.
///
/// What the revision registers is counted here and added to the push-wide
/// [`PushStats`], so revisions pushed at once each report their own.
#[lore_macro::test_pub]
pub(crate) struct PushProgress {
    fragment_count: AtomicUsize,
    registered: AtomicU64,
    bytes_queued: AtomicU64,
    put_bytes: AtomicU64,
    stats: Arc<PushStats>,
}

impl PushProgress {
    /// A revision's progress, adding to the push-wide `stats`.
    #[lore_macro::test_pub]
    pub(crate) fn new(stats: Arc<PushStats>) -> Self {
        Self {
            fragment_count: AtomicUsize::new(0),
            registered: AtomicU64::new(0),
            bytes_queued: AtomicU64::new(0),
            put_bytes: AtomicU64::new(0),
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

    /// The peer duplicated an association it already held, sending no payload.
    #[lore_macro::test_pub]
    fn copied(&self) {
        self.registered.fetch_add(1, Ordering::Relaxed);
        self.stats.copied();
    }

    /// A payload was uploaded to the peer.
    #[lore_macro::test_pub]
    fn put(&self, payload_bytes: u64) {
        self.registered.fetch_add(1, Ordering::Relaxed);
        self.put_bytes.fetch_add(payload_bytes, Ordering::Relaxed);
        self.stats.put();
    }

    /// Fragments this revision has registered with the peer, by copy or upload.
    #[lore_macro::test_pub]
    pub(crate) fn complete(&self) -> u64 {
        self.registered.load(Ordering::Relaxed)
    }

    /// Fragments this revision has to register.
    pub(crate) fn count(&self) -> u64 {
        self.fragment_count.load(Ordering::Relaxed) as u64
    }

    /// The progress event of this revision of `branch` of `repository`.
    #[lore_macro::test_pub]
    fn event(
        &self,
        repository: RepositoryId,
        branch: BranchId,
    ) -> LoreBranchPushFragmentProgressEventData {
        LoreBranchPushFragmentProgressEventData {
            repository,
            branch,
            complete: self.complete(),
            count: self.count(),
            bytes_transferred: self.put_bytes.load(Ordering::Relaxed),
            bytes_total: self.bytes_queued.load(Ordering::Relaxed),
        }
    }
}

/// Most fragment query batches one call's pushes have in flight at once.
const MAX_QUERY_TASKS: usize = 1000;
/// Most fragment uploads one call's pushes have in flight at once.
const MAX_PARALLEL_PUT: usize = 10000;

/// The query and upload slots of one call's pushes, shared by every target it pushes at once, so
/// that they share one bound rather than each multiplying it.
pub(crate) struct PushBudget {
    queries: Arc<Semaphore>,
    puts: Arc<Semaphore>,
}

impl Default for PushBudget {
    fn default() -> Self {
        Self {
            queries: Arc::new(Semaphore::new(MAX_QUERY_TASKS)),
            puts: Arc::new(Semaphore::new(MAX_PARALLEL_PUT)),
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

pub(crate) async fn push(
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

    let (targets, orders) =
        collect_targets(repository, branch, local_latest, options.fast_forward_merge).await?;
    push_targets(&targets, &orders, token, options.fast_forward_merge).await
}

/// Most targets one push plans or pushes at once. A few at once overlap their round trips, and each
/// one pushing opens a storage session of its own.
#[lore_macro::test_pub]
const MAX_CONCURRENT_TARGETS: usize = 8;

/// One branch of one repository a push reaches: the pushing repository, one of its layers, or a
/// branch a revision of another target links.
#[lore_macro::test_pub]
struct PushTarget {
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    /// The local revision the branch is pushed at.
    latest: Hash,
    /// How the repository is mounted, which names it in a failure. `None` for the pushing
    /// repository.
    mount: Option<Mount>,
    plan: TargetPlan,
    /// The revisions of other targets that this target's revisions link, each with its target.
    linked: Vec<(usize, Hash)>,
    /// Revisions other targets link that are still to be placed against `plan`.
    pending: Vec<Hash>,
    /// How many revisions of `plan` have their links joined to targets.
    scanned: usize,
}

/// A target reached, still to be planned.
#[lore_macro::test_pub]
struct TargetRequest {
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
    mount: Option<Mount>,
    pending: Vec<Hash>,
}

/// How a target's repository is mounted in the repository that reaches it.
#[lore_macro::test_pub]
enum Mount {
    /// The link `link`, which the revision `revision` of the target `target` holds.
    Link {
        target: usize,
        revision: Hash,
        link: LinkReference,
    },
    /// A layer of the pushing repository, placed at this path.
    Layer(String),
}

/// What pushing a target takes: the peer's view of its branch, and the revisions the peer is
/// missing.
#[lore_macro::test_pub]
struct TargetPlan {
    remote: Arc<Connection>,
    default_branch: BranchId,
    branch_metadata: BranchMetadata,
    /// The revision a branch the peer does not hold is created at.
    branch_point: Hash,
    /// The peer's latest revision of the branch, zero when the peer holds no such branch.
    remote_latest: Hash,
    remote_metadata: Hash,
    remote_deleted: bool,
    /// The peer's latest revision the history walk reached, if it reached one.
    remote_revision: Option<Hash>,
    remote_history: usize,
    local_history: usize,
    /// Whether the peer holds revisions on the branch that the local history does not.
    diverged: bool,
    /// The revisions to push, each line a merge names before the local revisions that name it.
    revisions: Vec<TargetRevision>,
    pushed: HashSet<Hash>,
    /// What the peer answered for the latest revision of each branch a line of the plan is on,
    /// so that each branch is asked about once.
    peer_latest: Vec<(BranchId, Hash)>,
}

impl TargetPlan {
    fn pushes(&self, revision: Hash) -> bool {
        self.pushed.contains(&revision)
    }

    fn add(&mut self, revision: TargetRevision) {
        if self.pushed.insert(revision.revision) {
            self.revisions.push(revision);
        }
    }
}

/// A revision a target pushes, with the links it holds.
#[lore_macro::test_pub]
struct TargetRevision {
    revision: Hash,
    /// Whether the revision is on the history of the target's branch, which offers it as the
    /// branch's latest revision when it belongs to the branch. The others are only uploaded.
    local: bool,
    links: Vec<LinkReference>,
}

/// The order a target pushes in.
#[lore_macro::test_pub]
#[derive(Debug)]
pub(crate) struct TargetOrder {
    /// The targets to finish first: those pushing a revision this target's revisions link, and for
    /// a layer the pushing repository, as a failed push leaves its layers alone.
    after: Vec<usize>,
    /// The first target of this target's repository. Targets of one repository push one at a
    /// time, so that they don't upload the same fragments at once.
    group: usize,
}

/// The targets a push has reached while it collects them.
#[lore_macro::test_pub]
struct TargetCollection {
    targets: Vec<PushTarget>,
    /// The targets to plan in the next round, numbered on from `targets`.
    requests: Vec<TargetRequest>,
    index: HashMap<(RepositoryId, BranchId), usize>,
}

/// The targets a push of `branch` of `repository` at `local_latest` reaches, with the order each
/// pushes in: the repository, each of its layers at its current revision, and each branch a
/// revision of theirs links, at any depth, at the newest revision linked.
///
/// Targets are planned a level of links at a time, [`MAX_CONCURRENT_TARGETS`] at once. Nothing is
/// pushed yet, so a push that would fail at a diverged repository or layer, or at a link list it
/// can't read, fails before it starts.
async fn collect_targets(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    local_latest: Hash,
    fast_forward_merge: bool,
) -> Result<(Vec<PushTarget>, Vec<TargetOrder>), PushError> {
    let mut requests = vec![TargetRequest {
        repository: repository.clone(),
        branch,
        latest: local_latest,
        mount: None,
        pending: vec![],
    }];
    let layers = layer::list_with_context(repository)
        .await
        .unwrap_or_default();
    for (layer, layer_repository) in layers {
        requests.push(TargetRequest {
            repository: layer_repository,
            branch,
            latest: layer.current,
            mount: Some(Mount::Layer(layer.target_path)),
            pending: vec![],
        });
    }
    let index = requests
        .iter()
        .enumerate()
        .map(|(target, request)| ((request.repository.id, request.branch), target))
        .collect();
    let mut collection = TargetCollection {
        targets: vec![],
        requests,
        index,
    };

    plan_requests(&mut collection).await?;
    refuse_diverged_targets(&collection.targets, fast_forward_merge).await?;
    join_latest_links(&mut collection).await?;
    loop {
        let placed = place_all_pending(&mut collection.targets).await?;
        let mut scanned = false;
        for target in 0..collection.targets.len() {
            if collection.targets[target].scanned < collection.targets[target].plan.revisions.len()
            {
                scan_target(&mut collection, target).await;
                scanned = true;
            }
        }
        if collection.requests.is_empty() && !placed && !scanned {
            break;
        }
        plan_requests(&mut collection).await?;
    }

    Ok(order_targets(collection.targets))
}

/// Plan every requested target, [`MAX_CONCURRENT_TARGETS`] at once, failing with every request that
/// fails.
async fn plan_requests(collection: &mut TargetCollection) -> Result<(), PushError> {
    let requests = std::mem::take(&mut collection.requests);
    let mut planned: Vec<(usize, Result<TargetPlan, PushError>)> = stream::iter(0..requests.len())
        .map(|position| {
            let request = &requests[position];
            plan_target(&request.repository, request.branch, request.latest)
                .map(move |plan| (position, plan))
        })
        .buffer_unordered(MAX_CONCURRENT_TARGETS)
        .collect()
        .await;
    planned.sort_unstable_by_key(|(position, _)| *position);

    let mut plans = Vec::with_capacity(planned.len());
    let mut failures = vec![];
    for ((_, plan), request) in planned.into_iter().zip(&requests) {
        match plan {
            Ok(plan) => plans.push(plan),
            Err(err) => failures.push(
                failed_mount_push(
                    err,
                    &collection.targets,
                    &request.repository,
                    request.branch,
                    request.mount.as_ref(),
                )
                .await,
            ),
        }
    }
    if !failures.is_empty() {
        return Err(report_failures(failures));
    }

    for (request, plan) in requests.into_iter().zip(plans) {
        collection.targets.push(PushTarget {
            repository: request.repository,
            branch: request.branch,
            latest: request.latest,
            mount: request.mount,
            plan,
            linked: vec![],
            pending: request.pending,
            scanned: 0,
        });
    }
    Ok(())
}

/// Join the links of the pushing repository's current revision at the latest revision of the
/// branch each resolves to, so that what was committed in a linked repository since the last
/// revision naming it is pushed too.
async fn join_latest_links(collection: &mut TargetCollection) -> Result<(), PushError> {
    let Some(root) = collection.targets.first() else {
        return Ok(());
    };
    let repository = root.repository.clone();
    let (branch, latest) = (root.branch, root.latest);
    let state = State::deserialize(repository.clone(), latest)
        .await
        .forward::<PushError>("deserializing current state for links")?;
    for link in revision_links(&repository, &state).await? {
        let link_branch = link.resolve_branch(branch);
        let link_repository = match collection.index.get(&(link.repository, link_branch)) {
            Some(&target) => collection.repository(target).clone(),
            None => repository.to_link_context(link.repository).await,
        };
        let link_latest = branch::load_latest(link_repository.clone(), link_branch)
            .await
            .unwrap_or_default();
        if link_latest.is_zero() {
            continue;
        }
        join_target(
            collection,
            0,
            latest,
            link,
            link_branch,
            link_latest,
            Some(link_repository),
        )
        .await;
    }
    Ok(())
}

/// Place the revisions other targets link in each of `targets`, [`MAX_CONCURRENT_TARGETS`] targets at
/// once, failing with every target that fails. Returns whether any target had revisions to place.
async fn place_all_pending(targets: &mut [PushTarget]) -> Result<bool, PushError> {
    let mut placing = FuturesUnordered::new();
    let mut failures = vec![];
    let pending: Vec<(usize, &mut PushTarget)> = targets
        .iter_mut()
        .enumerate()
        .filter(|(_, target)| !target.pending.is_empty())
        .collect();
    let placed = !pending.is_empty();
    let mut pending = pending.into_iter();
    loop {
        while placing.len() < MAX_CONCURRENT_TARGETS
            && let Some((position, target)) = pending.next()
        {
            placing.push(place_pending(target).map(move |placed| (position, placed)));
        }
        let Some((position, placed)) = placing.next().await else {
            break;
        };
        if let Err(err) = placed {
            failures.push((position, err));
        }
    }
    drop(placing);
    if failures.is_empty() {
        return Ok(placed);
    }
    failures.sort_unstable_by_key(|(position, _)| *position);
    let mut named = Vec::with_capacity(failures.len());
    for (position, err) in failures {
        named.push(failed_push(err, targets, position).await);
    }
    Err(report_failures(named))
}

/// Place the revisions other targets link in `target` against its plan.
///
/// A revision older than the latest one on its line needs nothing more: the plan pushes it, or the
/// peer holds it. A revision of another branch is uploaded with its line back to what the peer
/// holds of that branch, and not offered, as that branch's own push offers it. A newer revision of
/// the target's branch on its line extends the plan. One on another line replaces the plan when it
/// is newer through a merge or the plan pushes nothing of the branch, and fails the push as
/// [`Divergent`] otherwise.
///
/// Uploading a line, extending and planning again are boxed: linked revisions are mostly ones the
/// plan pushes already, and inline those cold paths would set the size of every push.
#[lore_macro::test_pub]
async fn place_pending(target: &mut PushTarget) -> Result<(), PushError> {
    let pending = std::mem::take(&mut target.pending);
    let repository = target.repository.clone();
    let branch = target.branch;
    let mut placed = HashSet::new();
    for revision in pending {
        let latest = target.latest;
        if !placed.insert(revision) || revision == latest || target.plan.pushes(revision) {
            continue;
        }

        let (_branch_point, latest_line, revision_line) =
            history::find_branch_point(repository.clone(), latest, revision)
                .await
                .forward::<PushError>("placing a linked revision against its branch")?;
        if revision_line.is_empty() {
            continue;
        }

        let revision_branch = State::deserialize(repository.clone(), revision)
            .await
            .forward::<PushError>("deserializing linked revision state")?
            .branch(repository.clone())
            .await;
        if revision_branch != branch {
            Box::pin(upload_line(target, revision, revision_branch)).await?;
            continue;
        }
        if latest_line.is_empty() {
            Box::pin(extend_plan(target, revision, &revision_line)).await?;
            continue;
        }

        let candidate = Box::pin(plan_target(&repository, branch, revision)).await?;
        if !candidate.pushes(latest) && target.plan.revisions.iter().any(|known| known.local) {
            return Err(PushError::from(Divergent)).forward_with::<PushError, _>(|| {
                format!("pushing revisions {latest} and {revision}, which are on diverged lines")
            });
        }
        lore_debug!("Revision {revision} supersedes {latest}, planning the target again");
        let replaced = std::mem::replace(&mut target.plan, candidate);
        target.latest = revision;
        target.scanned = 0;
        target.linked.clear();
        // Lines only the replaced plan uploads would otherwise be lost.
        target.pending.extend(
            replaced
                .revisions
                .iter()
                .filter(|known| !known.local && !target.plan.pushes(known.revision))
                .map(|known| known.revision),
        );
    }
    Ok(())
}

/// Extend `target`'s plan to `revision`, which `line` reaches from its latest revision through
/// first parents, newest first.
async fn extend_plan(
    target: &mut PushTarget,
    revision: Hash,
    line: &[Hash],
) -> Result<(), PushError> {
    lore_debug!(
        "Revision {revision} is newer than {}, extending the plan",
        target.latest
    );
    let plan = &mut target.plan;
    let (local, merges) = read_history(&target.repository, line).await?;
    let mut visited = plan.pushed.clone();
    visited.extend(line.iter().copied());
    let divergent = collect_divergent_history(
        &target.repository,
        &plan.remote,
        merges,
        &mut visited,
        &mut plan.peer_latest,
    )
    .await?;
    for added in divergent.into_iter().rev().chain(local.into_iter().rev()) {
        plan.add(added);
    }
    plan.local_history += line.len();
    target.latest = revision;
    Ok(())
}

/// Add the line of `revision`, a revision of `revision_branch`, to `target`'s plan, back to what
/// the peer holds of that branch, or to the target's latest revision when the peer holds none.
async fn upload_line(
    target: &mut PushTarget,
    revision: Hash,
    revision_branch: BranchId,
) -> Result<(), PushError> {
    lore_debug!("Revision {revision} is on branch {revision_branch}, uploading its line");
    let plan = &mut target.plan;
    let peer_latest = peer_latest_for_branch(
        plan.remote.clone(),
        target.repository.id,
        revision_branch,
        &mut plan.peer_latest,
    )
    .await;
    let base = if peer_latest.is_zero() {
        target.latest
    } else {
        peer_latest
    };
    let (_branch_point, _base_line, line) =
        history::find_branch_point(target.repository.clone(), base, revision)
            .await
            .forward::<PushError>("collecting the line of a linked revision")?;
    for &added in line.iter().rev() {
        if !plan.pushes(added) {
            plan.add(
                read_target_revision(&target.repository, added, false)
                    .await?
                    .0,
            );
        }
    }
    Ok(())
}

/// Join the links of the revisions `target`'s plan gained since its last scan, newest first, each
/// to the target of the branch it resolves to.
///
/// Newest first, a target is requested at the newest revision linked, so that the older ones are
/// in its plan rather than placed against it.
async fn scan_target(collection: &mut TargetCollection, target: usize) {
    let scanning = &mut collection.targets[target];
    let branch = scanning.branch;
    let start = scanning.scanned;
    scanning.scanned = scanning.plan.revisions.len();
    let links: Vec<(Hash, LinkReference)> = scanning.plan.revisions[start..]
        .iter_mut()
        .rev()
        .flat_map(|pushed| {
            let revision = pushed.revision;
            std::mem::take(&mut pushed.links)
                .into_iter()
                .map(move |link| (revision, link))
        })
        .filter(|(_, link)| !link.signature.is_zero())
        .collect();

    for (holder, link) in links {
        let link_branch = link.resolve_branch(branch);
        let linked = join_target(
            collection,
            target,
            holder,
            link,
            link_branch,
            link.signature,
            None,
        )
        .await;
        if linked != target {
            collection.targets[target]
                .linked
                .push((linked, link.signature));
        }
    }
}

/// The links `state` holds. A revision pushed without its links names revisions the peer may not
/// hold, so a list that can't be read fails the push.
#[lore_macro::test_pub]
async fn revision_links(
    repository: &Arc<RepositoryContext>,
    state: &State,
) -> Result<Vec<LinkReference>, PushError> {
    state
        .link_list(repository.clone())
        .await
        .forward_with::<PushError, _>(|| {
            format!("listing the links of revision {}", state.revision())
        })
}

/// The revision `revision` of `repository` with the links it holds, and its two parents when it
/// is a merge.
async fn read_target_revision(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    local: bool,
) -> Result<(TargetRevision, Option<(Hash, Hash)>), PushError> {
    let state = State::deserialize(repository.clone(), revision)
        .await
        .forward::<PushError>("deserializing revision state")?;
    let merge =
        (!state.parent_other().is_zero()).then(|| (state.parent_self(), state.parent_other()));
    let revision = TargetRevision {
        revision,
        local,
        links: revision_links(repository, &state).await?,
    };
    Ok((revision, merge))
}

impl TargetCollection {
    fn repository(&self, target: usize) -> &Arc<RepositoryContext> {
        match self.targets.get(target) {
            Some(pushed) => &pushed.repository,
            None => &self.requests[target - self.targets.len()].repository,
        }
    }
}

/// The target pushing `branch` of the repository `link` links, joined to push `revision`. `link`
/// is held by the revision `holder` of the target `parent`, and names the target it requests when
/// no target pushes the branch yet. `repository` is the linked repository's context where the
/// caller has it.
async fn join_target(
    collection: &mut TargetCollection,
    parent: usize,
    holder: Hash,
    link: LinkReference,
    branch: BranchId,
    revision: Hash,
    repository: Option<Arc<RepositoryContext>>,
) -> usize {
    if let Some(&target) = collection.index.get(&(link.repository, branch)) {
        if let Some(pushed) = collection.targets.get_mut(target) {
            if pushed.latest != revision && !pushed.plan.pushes(revision) {
                pushed.pending.push(revision);
            }
        } else {
            let request = &mut collection.requests[target - collection.targets.len()];
            if request.latest != revision {
                request.pending.push(revision);
            }
        }
        return target;
    }

    lore_debug!(
        "Pushing link {} on branch {branch} at revision {revision}",
        link.repository
    );
    let repository = match repository {
        Some(repository) => repository,
        None => {
            collection.targets[parent]
                .repository
                .to_link_context(link.repository)
                .await
        }
    };
    let target = collection.targets.len() + collection.requests.len();
    collection.requests.push(TargetRequest {
        repository,
        branch,
        latest: revision,
        mount: Some(Mount::Link {
            target: parent,
            revision: holder,
            link,
        }),
        pending: vec![],
    });
    collection.index.insert((link.repository, branch), target);
    target
}

/// The order each of `targets` pushes in.
#[lore_macro::test_pub]
fn order_targets(targets: Vec<PushTarget>) -> (Vec<PushTarget>, Vec<TargetOrder>) {
    let mut groups = HashMap::new();
    let orders = targets
        .iter()
        .enumerate()
        .map(|(position, target)| {
            let mut after: Vec<usize> = target
                .linked
                .iter()
                .filter(|&&(linked, revision)| targets[linked].plan.pushes(revision))
                .map(|&(linked, _)| linked)
                .collect();
            if matches!(target.mount, Some(Mount::Layer(_))) {
                after.push(0);
            }
            after.sort_unstable();
            after.dedup();
            TargetOrder {
                after,
                group: *groups.entry(target.repository.id).or_insert(position),
            }
        })
        .collect();
    (targets, orders)
}

/// Fail the push before anything is pushed when the pushing repository or one of its layers has
/// diverged: the peer holds revisions on its branch that the local history does not. A forced push
/// or a fast-forward merge leaves that to the peer, as does a linked repository, whose refusal
/// fails as [`LinkDivergent`].
async fn refuse_diverged_targets(
    targets: &[PushTarget],
    fast_forward_merge: bool,
) -> Result<(), PushError> {
    if fast_forward_merge || execution_context().globals().force() {
        return Ok(());
    }
    let mut failures = vec![];
    for (position, target) in targets.iter().enumerate() {
        if target.repository.is_link() || !target.plan.diverged {
            continue;
        }
        send_branch_push_event(target);
        lore_debug!("Branch divergence detected on branch {}", target.branch);
        let err = PushError::internal("Branch has diverged, sync to merge remote changes");
        failures.push(failed_push(err, targets, position).await);
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(report_failures(failures))
}

/// Push every target, [`MAX_CONCURRENT_TARGETS`] at once. A failed target holds back the targets
/// that link its revisions, and the others go on.
async fn push_targets(
    targets: &[PushTarget],
    orders: &[TargetOrder],
    token: &RepositoryWriteToken,
    fast_forward_merge: bool,
) -> Result<(), PushError> {
    let mut failures = schedule_targets(orders, |target| {
        push_target(&targets[target], token, fast_forward_merge)
    })
    .await;
    if failures.is_empty() {
        return Ok(());
    }
    failures.sort_unstable_by_key(|(target, _)| *target);
    let mut named = Vec::with_capacity(failures.len());
    for (target, err) in failures {
        named.push(failed_push(err, targets, target).await);
    }
    Err(report_failures(named))
}

/// What a target is doing.
#[derive(Clone, Copy, PartialEq)]
enum TargetState {
    Waiting,
    Pushing,
    Pushed,
    Failed,
}

/// A target left waiting on a cycle of targets, rather than behind a failed one.
///
/// A waiting target that no failure holds back waits on another such target, so following those
/// reaches a target twice, which is on the cycle.
fn target_in_cycle(orders: &[TargetOrder], states: &[TargetState]) -> Option<usize> {
    let mut held = states
        .iter()
        .map(|state| *state == TargetState::Failed)
        .collect::<Vec<_>>();
    let mut changed = true;
    while changed {
        changed = false;
        for (target, order) in orders.iter().enumerate() {
            if !held[target]
                && states[target] == TargetState::Waiting
                && order.after.iter().any(|&after| held[after])
            {
                held[target] = true;
                changed = true;
            }
        }
    }

    let mut target = (0..orders.len())
        .find(|&target| states[target] == TargetState::Waiting && !held[target])?;
    let mut seen = vec![false; orders.len()];
    while !seen[target] {
        seen[target] = true;
        target = orders[target]
            .after
            .iter()
            .copied()
            .find(|&after| states[after] == TargetState::Waiting)?;
    }
    Some(target)
}

/// Push every target `orders` holds, [`MAX_CONCURRENT_TARGETS`] at once, each once the targets it
/// comes after have pushed and no other target of its repository is pushing, and return the
/// failures, each with its target.
///
/// Targets start in the order they were reached. A failed target holds back the targets after it,
/// and the others go on. Targets waiting on each other fail with a target of their cycle.
#[lore_macro::test_pub]
pub(crate) async fn schedule_targets<F, Fut>(
    orders: &[TargetOrder],
    mut push: F,
) -> Vec<(usize, PushError)>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<(), PushError>>,
{
    let mut states = vec![TargetState::Waiting; orders.len()];
    let mut busy = vec![false; orders.len()];
    let mut failures = vec![];
    let mut in_flight = FuturesUnordered::new();
    loop {
        for (target, order) in orders.iter().enumerate() {
            if in_flight.len() >= MAX_CONCURRENT_TARGETS {
                break;
            }
            if states[target] != TargetState::Waiting
                || busy[order.group]
                || !order
                    .after
                    .iter()
                    .all(|&after| states[after] == TargetState::Pushed)
            {
                continue;
            }
            states[target] = TargetState::Pushing;
            busy[order.group] = true;
            in_flight.push(push(target).map(move |pushed| (target, pushed)));
        }

        let Some((target, pushed)) = in_flight.next().await else {
            break;
        };
        busy[orders[target].group] = false;
        states[target] = match pushed {
            Ok(()) => TargetState::Pushed,
            Err(err) => {
                failures.push((target, err));
                TargetState::Failed
            }
        };
    }

    if let Some(target) = target_in_cycle(orders, &states) {
        failures.push((
            target,
            PushError::internal("the targets of the push link each other's revisions"),
        ));
    }
    failures
}

/// Boxed version of [`push`] for cross-crate use.
pub fn push_boxed(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: PushOptions,
) -> crate::BoxFuture<'_, Result<(), PushError>> {
    Box::pin(push(repository, token, options))
}

/// The revision the peer holds as latest for `branch`, remembered in `known`, or zero when
/// the peer names none.
///
/// A zero branch is no branch to ask about, which a revision whose metadata cannot be read
/// answers with. A push carries a line or two, so the answers are kept in a list rather
/// than a map.
async fn peer_latest_for_branch(
    remote: Arc<Connection>,
    repository: RepositoryId,
    branch: BranchId,
    known: &mut Vec<(BranchId, Hash)>,
) -> Hash {
    if branch.is_zero() {
        return Hash::default();
    }

    if let Some((_branch, latest)) = known.iter().find(|(known, _)| *known == branch) {
        return *latest;
    }

    let latest = branch::load_remote_latest(remote, repository, branch)
        .await
        .unwrap_or_default();
    known.push((branch, latest));
    latest
}

/// The revisions that `merges`, each a merge's two parents, reach only through the second parent,
/// newest first, skipping those in `visited`, with their links.
///
/// A merge sets `parent_self` to the revision the peer already had and `parent_other` to
/// the line merged into it, so the branch history walk that produced `history` reaches a
/// merge revision and stops. The revisions on that second line still have to be readable
/// on the peer, which names them through the merge, and nothing else uploads them: an
/// online commit uploads what it writes as it writes it, so it is a line committed offline
/// that arrives here with nothing of it on the peer.
///
/// [`history::find_branch_point`] is what says where a line left the history the peer
/// holds, and what it walks against decides how much of the line it reports. The peer's
/// latest for the branch the line belongs to is the bound where the peer names one - the
/// same bound the branch history walk applies to the branch being pushed, so a line the
/// peer carries already is walked no further than the revisions it is missing, and
/// `peer_latest` keeps what the peer answered, starting from the branch being pushed. A branch the peer
/// has never seen it names nothing for, and a walk against nothing runs to the root of the
/// line, so those fall back on the merge's own first parent: a line and the line it was
/// merged into meet at the branch point, which is as far as the line reaches.
///
/// A revision on the line may be a merge in its own right, and its second line is just as
/// unreachable, so those are followed the same way.
///
/// The result is ordered to be walked in reverse, which visits each line oldest revision
/// first. What each revision owns is collected against its own first parent whatever the
/// order, so this is what the order buys: a fragment an older revision registers counts as
/// durably stored before a newer revision holding the same one is collected, and drops out
/// of that collection. Completeness does not rest on the order - every line is walked back
/// to a revision the peer holds, so a fragment is offered by the oldest revision on the
/// line that holds it.
async fn collect_divergent_history(
    repository: &Arc<RepositoryContext>,
    remote: &Arc<Connection>,
    mut merges: Vec<(Hash, Hash)>,
    visited: &mut HashSet<Hash>,
    peer_latest: &mut Vec<(BranchId, Hash)>,
) -> Result<Vec<TargetRevision>, PushError> {
    let mut divergent = vec![];
    while let Some((parent_self, parent_other)) = merges.pop() {
        let line_branch = State::deserialize(repository.clone(), parent_other)
            .await
            .forward::<PushError>("deserializing merged line state")?
            .branch(repository.clone())
            .await;
        let line_base =
            peer_latest_for_branch(remote.clone(), repository.id, line_branch, peer_latest).await;
        let line_base = if line_base.is_zero() {
            parent_self
        } else {
            line_base
        };

        let (_branch_point, _peer_history, line_history) =
            history::find_branch_point(repository.clone(), line_base, parent_other)
                .await
                .forward::<PushError>("reconciling divergent history")?;

        lore_debug!(
            "Found {} revision(s) on the line merged from {parent_other}, against {line_base} on branch {line_branch}",
            line_history.len()
        );

        for revision in line_history {
            if !visited.insert(revision) {
                continue;
            }
            let (revision, merge) = read_target_revision(repository, revision, false).await?;
            merges.extend(merge);
            divergent.push(revision);
        }
    }
    Ok(divergent)
}

/// The revisions of `history`, which are on the target's branch, with their links, and the merges
/// among them, each as its two parents.
async fn read_history(
    repository: &Arc<RepositoryContext>,
    history: &[Hash],
) -> Result<(Vec<TargetRevision>, Vec<(Hash, Hash)>), PushError> {
    let mut revisions = Vec::with_capacity(history.len());
    let mut merges = vec![];
    for &revision in history {
        let (revision, merge) = read_target_revision(repository, revision, true).await?;
        merges.extend(merge);
        revisions.push(revision);
    }
    Ok((revisions, merges))
}

/// The descriptive metadata of `branch`.
///
/// Its own future, so that the key-value metadata it is read from is not held across the awaits
/// of the futures that keep what it returns.
async fn load_branch_metadata(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<BranchMetadata, PushError> {
    let metadata = branch::metadata(repository.clone(), branch)
        .await
        .forward::<PushError>("loading branch metadata")?;
    branch::branch_metadata(repository.clone(), branch, &metadata)
        .await
        .forward::<PushError>("loading branch metadata")
}

/// The history a push of `branch` at `local_latest` reconciles against the peer's.
#[lore_macro::test_pub]
struct UnpushedHistory {
    /// The local revisions the peer is missing, newest first.
    local: Vec<Hash>,
    /// The peer's revisions on the lines walked that the local history does not hold.
    remote: Vec<Hash>,
    /// The peer's revisions on `branch` itself that the local history does not hold: a push onto
    /// them diverges.
    branch_remote: Vec<Hash>,
    /// The peer's latest revision the walk reached, if it reached one.
    remote_revision: Option<Hash>,
    branch_metadata: BranchMetadata,
}

/// Walk the history of `branch` back from `local_latest`, through the branches it was created
/// from, until it reaches a revision the peer holds, a parent branch known to be pushed, or the
/// root of the default branch.
///
/// Its own future: inline, what the walk holds across its awaits would be reserved in every
/// state of [`plan_target`].
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn collect_unpushed_history(
    repository: &Arc<RepositoryContext>,
    remote: &Arc<Connection>,
    state: &State,
    branch: BranchId,
    local_latest: Hash,
    remote_latest: Hash,
    default_branch: BranchId,
) -> impl Future<Output = Result<UnpushedHistory, PushError>> {
    async move {
        let branch_metadata = load_branch_metadata(repository, branch).await?;
        let mut full_local_history = vec![];
        let mut full_remote_history = vec![];
        let mut current_branch_remote_history = vec![];
        let mut remote_revision = None;
        let mut current_branch = branch;
        let mut current_revision = local_latest;

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

            let branch_point = if current_branch == branch {
                branch_metadata.branch_point().cloned()
            } else {
                load_branch_metadata(repository, current_branch)
                    .await?
                    .branch_point()
                    .cloned()
            }
            .unwrap_or_default();

            lore_debug!(
                "Walking history for branch {current_branch} at revision {current_revision}"
            );

            if current_remote_latest.is_zero() && (current_branch != default_branch) {
                lore_debug!("Remote latest is zero, collect revisions and continue");
                if branch_point.revision.is_zero() {
                    return Err(PushError::internal(
                        "Invalid branch data, unknown branch point",
                    ));
                }

                let branch_point_state =
                    State::deserialize(repository.clone(), branch_point.revision)
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

        Ok(UnpushedHistory {
            local: full_local_history,
            remote: full_remote_history,
            branch_remote: current_branch_remote_history,
            remote_revision,
            branch_metadata,
        })
    }
}

/// What pushing `branch` of `repository` at `latest` takes: the peer's view of the branch, and the
/// revisions the peer is missing, with their links. Reads the peer and the local history, and
/// changes neither. A branch the peer holds already at `latest` pushes nothing.
async fn plan_target(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
) -> Result<TargetPlan, PushError> {
    let remote = repository
        .remote()
        .await
        .forward::<PushError>("acquiring remote")?;

    let repository_metadata = repository::metadata_hash(repository.clone())
        .await
        .forward::<PushError>("loading repository metadata hash")?;
    let default_branch = repository::metadata(repository.clone(), repository_metadata)
        .await
        .forward::<PushError>("loading repository metadata")?
        .default_branch;

    let (remote_latest, remote_metadata, remote_deleted) = match branch::load_remote(
        remote.clone(),
        repository.id,
        branch,
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

    let state = State::deserialize(repository.clone(), latest)
        .await
        .forward::<PushError>("deserializing current state")?;
    let UnpushedHistory {
        local: mut local_history,
        remote: remote_history,
        branch_remote,
        remote_revision,
        branch_metadata,
    } = collect_unpushed_history(
        repository,
        &remote,
        &state,
        branch,
        latest,
        remote_latest,
        default_branch,
    )
    .await?;
    drop(state);

    let branch_point = branch_metadata
        .branch_point()
        .map_or_else(Hash::default, |parent| parent.revision);
    let created_latest = if remote_metadata.is_zero() {
        branch_point
    } else {
        remote_latest
    };
    let mut plan = TargetPlan {
        remote,
        default_branch,
        branch_metadata,
        branch_point,
        remote_latest,
        remote_metadata,
        remote_deleted,
        remote_revision,
        remote_history: remote_history.len(),
        local_history: local_history.len(),
        diverged: !branch_remote.is_empty(),
        revisions: vec![],
        pushed: HashSet::new(),
        peer_latest: vec![(branch, created_latest)],
    };
    if remote_latest == latest {
        return Ok(plan);
    }

    if local_history.is_empty() && !latest.is_zero() && execution_context().globals().force() {
        local_history.push(latest);
    }
    let (local, merges) = read_history(repository, &local_history).await?;
    let mut visited = local_history.iter().copied().collect();
    let divergent = collect_divergent_history(
        repository,
        &plan.remote,
        merges,
        &mut visited,
        &mut plan.peer_latest,
    )
    .await?;
    for revision in divergent.into_iter().rev().chain(local.into_iter().rev()) {
        plan.add(revision);
    }
    Ok(plan)
}

/// What a target's push keeps from one revision to the next.
struct TargetRun {
    revision_protocol: Arc<dyn Revision>,
    storage: Arc<StorageSession>,
    /// The peer's latest revision of the branch.
    remote_latest: Hash,
    /// The last revision the peer accepted, which the next revision is rebased on.
    current_latest: Hash,
    fast_forward_merged: bool,
}

fn send_branch_push_event(target: &PushTarget) {
    let plan = &target.plan;
    event::LoreEvent::BranchPush(LoreBranchPushEventData {
        remote: plan.remote.remote_url().into(),
        repository: target.repository.id,
        branch: target.branch,
        branch_name: plan.branch_metadata.name.as_str().into(),
        remote_revision: plan.remote_revision.unwrap_or_default(),
        local_revision: target.latest,
        remote_history: plan.remote_history as u64,
        local_history: plan.local_history as u64,
        flag_already_pushed: (plan.remote_latest == target.latest).into(),
        flag_default: (target.branch == plan.default_branch).into(),
        flag_link: target.repository.is_link().into(),
        flag_layer: target.repository.is_layer().into(),
    })
    .send();
}

/// Create `branch` on the peer from `metadata`, returning the revision it starts at.
async fn create_branch(
    revision_protocol: &dyn Revision,
    branch: BranchId,
    metadata: &BranchMetadata,
) -> Result<Hash, PushError> {
    revision_protocol
        .branch_create(
            branch,
            metadata.name.as_str(),
            metadata.category.as_str(),
            metadata.creator.as_str(),
            &metadata.stack,
        )
        .await
        .map(|record| record.latest)
        .forward::<PushError>("creating branch on remote")
}

/// Push `target`: start its push, push each revision of its plan in order, and finish.
async fn push_target(
    target: &PushTarget,
    token: &RepositoryWriteToken,
    fast_forward_merge: bool,
) -> Result<(), PushError> {
    let Some(mut run) = start_target(target).await? else {
        return Ok(());
    };
    for pushed in &target.plan.revisions {
        if pushed.local {
            push_local_revision(target, &mut run, pushed.revision, token, fast_forward_merge)
                .await?;
        } else {
            let state = State::deserialize(target.repository.clone(), pushed.revision)
                .await
                .forward::<PushError>("deserializing divergent revision state")?;
            upload_revision_fragments(
                &target.repository,
                target.branch,
                &run.storage,
                target.plan.remote.environment.max_query_batch(),
                &state,
                execution_context().globals().dry_run(),
                Collection::New,
            )
            .await?;
        }
    }
    finish_target(target, &run).await
}

/// Start the push of `target`: announce it, and create its branch on the peer where the peer holds
/// none, or holds it deleted. Returns what pushing its revisions keeps, or `None` when it has none.
async fn start_target(target: &PushTarget) -> Result<Option<TargetRun>, PushError> {
    let repository = &target.repository;
    let branch = target.branch;
    let plan = &target.plan;
    let revision_protocol = plan
        .remote
        .revision(repository.id)
        .await
        .forward::<PushError>("acquiring revision protocol")?;

    send_branch_push_event(target);

    let dry_run = execution_context().globals().dry_run();
    if plan.remote_deleted && !dry_run {
        lore_debug!("Branch deleted on server, restoring via branch_create");
        create_branch(revision_protocol.as_ref(), branch, &plan.branch_metadata).await?;
    }
    if plan.remote_latest == target.latest && plan.revisions.is_empty() {
        return Ok(None);
    }

    let mut remote_latest = plan.remote_latest;
    if plan.remote_metadata.is_zero() {
        event::LoreEvent::BranchPushBranchCreateBegin(LoreBranchPushBranchCreateBeginEventData {
            repository: repository.id,
            branch,
            local_revision: plan.branch_point,
        })
        .send();

        if !dry_run {
            remote_latest =
                create_branch(revision_protocol.as_ref(), branch, &plan.branch_metadata).await?;
            if remote_latest != plan.branch_point {
                return Err(PushError::internal(format!(
                    "Failed to create branch {}, remote latest now at {}",
                    plan.branch_metadata.name, remote_latest
                )));
            }

            branch::store_last_sync(repository.clone(), branch, plan.branch_point).await;
        } else {
            remote_latest = plan.branch_point;
        }

        event::LoreEvent::BranchPushBranchCreateEnd(LoreBranchPushBranchCreateEndEventData {
            repository: repository.id,
            branch,
            remote_revision: remote_latest,
        })
        .send();
    }

    if plan.revisions.is_empty() {
        return Ok(None);
    }

    let storage = plan
        .remote
        .session(
            repository.id,
            execution_context().globals().correlation_id.as_ref(),
        )
        .await
        .forward::<PushError>("opening storage session")?;

    Ok(Some(TargetRun {
        revision_protocol,
        storage,
        remote_latest,
        current_latest: Hash::default(),
        fast_forward_merged: false,
    }))
}

/// Push `revision` of the history of `target`'s branch: rebase it on the last revision the peer
/// accepted, upload what it holds, and offer it as the branch's latest revision when it belongs to
/// the branch. A linked repository's offer the peer refuses onto revisions the local history does
/// not hold fails as [`Divergent`].
async fn push_local_revision(
    target: &PushTarget,
    run: &mut TargetRun,
    revision: Hash,
    token: &RepositoryWriteToken,
    fast_forward_merge: bool,
) -> Result<(), PushError> {
    let repository = &target.repository;
    let branch = target.branch;
    let plan = &target.plan;
    let force = execution_context().globals().force();
    let dry_run = execution_context().globals().dry_run();
    let mut current_revision = revision;

    let state = State::deserialize(repository.clone(), current_revision)
        .await
        .forward::<PushError>("deserializing revision state")?;

    if !run.current_latest.is_zero() && state.parent_self() != run.current_latest {
        // TODO(mjansson): This only handles revision number rewrite for now, implement proper
        //                 automatic rebase if the push resulted in a clean rebase

        event::LoreEvent::BranchPushRevisionUpdateBegin(
            LoreBranchPushRevisionUpdateBeginEventData {
                repository: repository.id,
                branch,
                revision: state.revision(),
                old_parent: state.parent_self(),
                new_parent: run.current_latest,
            },
        )
        .send();

        state.set_parent_self(run.current_latest);
        current_revision = state
            .serialize(repository.clone(), token)
            .await
            .forward::<PushError>("serializing state")?;

        event::LoreEvent::BranchPushRevisionUpdateEnd(LoreBranchPushRevisionUpdateEndEventData {
            repository: repository.id,
            branch,
            revision: current_revision,
        })
        .send();
    }

    upload_revision_fragments(
        repository,
        branch,
        &run.storage,
        plan.remote.environment.max_query_batch(),
        &state,
        dry_run,
        Collection::New,
    )
    .await?;

    // A revision from another branch belongs to that branch's own push. Its fragments
    // are what the peer needs of it, and they are now up.
    if state.branch(repository.clone()).await != branch {
        return Ok(());
    };

    event::LoreEvent::BranchPushRevisionPushBegin(LoreBranchPushRevisionPushBeginEventData {
        repository: repository.id,
        branch,
        remote_revision: run.remote_latest,
        local_revision: current_revision,
    })
    .send();

    let current_remote = run.remote_latest;
    let mut response_message = None;

    if !dry_run && run.remote_latest != current_revision {
        let response = offer_revision(
            run.revision_protocol.as_ref(),
            repository.id,
            branch,
            &plan.branch_metadata,
            run.remote_latest,
            current_revision,
            force,
            fast_forward_merge,
            || {
                Box::pin(upload_revision_fragments(
                    repository,
                    branch,
                    &run.storage,
                    plan.remote.environment.max_query_batch(),
                    &state,
                    false,
                    Collection::Recovery,
                ))
            },
        )
        .await
        .map_err(|err| refused_offer(err, repository.is_link() && plan.diverged))?;
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

            run.remote_latest = response.revision;
            run.current_latest = response.revision;

            event::LoreEvent::BranchPushRevisionPushEnd(LoreBranchPushRevisionPushEndEventData {
                repository: repository.id,
                branch,
                old_remote_revision: current_remote,
                new_remote_revision: run.current_latest,
                new_remote_revision_number: response.revision_number,
                message: response.message.unwrap_or_default().into(),
                fast_forward_merged: 1,
            })
            .send();

            // The working directory still holds the merged revision; `lore sync` updates it.
            run.fast_forward_merged = true;
            return Ok(());
        }
        if response.revision_number == 0 {
            if fast_forward_merge {
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
                    repository: repository.id,
                    branch,
                    old_revision: current_revision,
                    new_revision: response.revision,
                    new_revision_number: response.revision_number,
                },
            )
            .send();
        }
        response_message = response.message;

        run.remote_latest = response.revision;
        run.current_latest = response.revision;
    } else {
        run.current_latest = current_revision;
    }

    let current_number = State::deserialize(repository.clone(), run.current_latest)
        .await
        .forward::<PushError>("deserializing current latest state")?
        .revision_number();

    event::LoreEvent::BranchPushRevisionPushEnd(LoreBranchPushRevisionPushEndEventData {
        repository: repository.id,
        branch,
        old_remote_revision: current_remote,
        new_remote_revision: run.current_latest,
        new_remote_revision_number: current_number,
        message: response_message.unwrap_or_default().into(),
        fast_forward_merged: 0,
    })
    .send();

    if !dry_run {
        branch::store_last_sync(repository.clone(), branch, run.current_latest).await;
    }
    Ok(())
}

/// Record the last revision the peer accepted for `target` as its branch's local latest revision.
/// Only the pushing repository keeps one, and a fast-forward merge leaves it to the next sync.
async fn finish_target(target: &PushTarget, run: &TargetRun) -> Result<(), PushError> {
    let repository = &target.repository;
    lore_debug!(
        "All revisions pushed, updating current local latest to {}",
        run.current_latest
    );
    if run.current_latest.is_zero()
        || repository.is_layer()
        || repository.is_link()
        || run.fast_forward_merged
        || execution_context().globals().dry_run()
    {
        return Ok(());
    }

    let local_latest = branch::load_latest(repository.clone(), target.branch)
        .await
        .unwrap_or_default();
    branch::store_latest(
        repository.clone(),
        target.branch,
        local_latest,
        run.current_latest,
        BranchLatestStatus::Convergent,
    )
    .await
    .forward::<PushError>("setting new latest revision for branch")?;

    branch::store_last_sync(repository.clone(), target.branch, run.current_latest).await;
    Ok(())
}

/// The error of a revision offer that failed with `err`. The peer refuses an offer as an internal
/// error, which is [`Divergent`] when `diverged`: the peer holds revisions on the branch that the
/// local history does not.
#[lore_macro::test_pub]
fn refused_offer(err: PushError, diverged: bool) -> PushError {
    if !diverged || !err.is_internal() {
        return err;
    }
    let context = format!("remote refused the revision: {err}");
    PushError::Divergent(Divergent.chain_err_from(err, &context))
}

/// Forward what the peer answered a branch push with, naming the fragment where it refused for
/// a missing one.
#[track_caller]
fn forward_branch_push<T>(result: Result<T, ProtocolError>) -> Result<T, PushError> {
    match result {
        Err(ProtocolError::AddressNotFound(missing)) => {
            let address = Address::from(&missing.address[..]);
            Err(ProtocolError::AddressNotFound(missing)).forward_with::<PushError, _>(|| {
                format!("pushing branch to remote, missing fragment {address}")
            })
        }
        result => result.forward::<PushError>("pushing branch to remote"),
    }
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
#[lore_macro::test_pub]
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

// `len` counts only what has to be transferred, so an `is_empty` beside it would mislead.
#[allow(clippy::len_without_is_empty)]
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
#[lore_macro::test_pub]
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
    let slots = execution_context().push_budget().queries.clone();

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
            let Ok(slot) = slots.clone().acquire_owned().await else {
                failure = Some(PushError::internal("query slots closed"));
                break;
            };
            let mut batch = remain.split_off(remain.len().saturating_sub(max_batch_size));

            let storage = storage.clone();
            lore_spawn!(tasks, async move {
                let _slot = slot;
                batch.sort_unstable();
                batch.dedup();

                let result = storage.query(batch.as_slice()).await;
                (batch, result)
            });

            while failure.is_none()
                && let Some(result) = tasks.try_join_next()
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
/// for. A refusal costs the round trip and leaves the payload to be uploaded.
#[lore_macro::test_pub]
async fn duplicate_association(
    repository: &Arc<RepositoryContext>,
    storage: &StorageSession,
    address: Address,
    progress: &PushProgress,
) -> Result<bool, PushError> {
    match lore_storage::remote_copy_retry(
        storage,
        repository.id,
        Address::zero_context_hash(address.hash),
        address.context,
    )
    .await
    {
        CopyOutcome::Copied => {}
        CopyOutcome::Refused => return Ok(false),
        // The peer is still asking for back-off, and an upload would send it the whole payload.
        CopyOutcome::Throttled => return Err(PushError::from(SlowDown)),
    }

    progress.copied();
    mark_stored_durable(repository, address).await;
    Ok(true)
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
/// pinned against eviction and re-queried on every subsequent push.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
pub(crate) fn push_fragments(
    repository: Arc<RepositoryContext>,
    storage: Arc<StorageSession>,
    fragments: PushQueryResult,
    progress: Arc<PushProgress>,
) -> impl Future<Output = Result<(), PushError>> {
    let slots = execution_context().push_budget().puts.clone();
    push_fragments_within(repository, storage, fragments, progress, slots)
}

/// [`push_fragments`], uploading in the slots `slots` holds, which every upload of the call shares.
#[lore_macro::test_pub]
async fn push_fragments_within(
    repository: Arc<RepositoryContext>,
    storage: Arc<StorageSession>,
    fragments: PushQueryResult,
    progress: Arc<PushProgress>,
    slots: Arc<Semaphore>,
) -> Result<(), PushError> {
    let fragment_count = fragments.len();
    let PushQueryResult {
        absent,
        copyable,
        present,
    } = fragments;

    progress.set_fragment_count(fragment_count);

    let marking = {
        let repository = repository.clone();
        lore_spawn!(async move { mark_present_durable(&repository, present).await })
    };

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

        let Ok(slot) = slots.clone().acquire_owned().await else {
            failure = failure.or(Some(PushError::internal("upload slots closed")));
            break;
        };
        let repository = repository.clone();
        let storage = storage.clone();
        let progress = progress.clone();
        lore_spawn!(tasks, async move {
            let _slot = slot;
            if duplicable
                && duplicate_association(&repository, &storage, address, &progress).await?
            {
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
                    if err.is_disconnected() || is_refusal(&err) {
                        err.forward("putting fragment to remote")
                    } else {
                        PushError::internal_with_context(err, "putting fragment to remote")
                    }
                })?;

            progress.put(payload_size);

            mark_durable(&repository, address, fragment).await;

            Ok(())
        });

        while let Some(result) = tasks.try_join_next() {
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

/// The kind, mount path and branch name of the link or layer a target pushes.
#[lore_macro::test_pub]
#[derive(Debug, PartialEq)]
struct MountName {
    kind: &'static str,
    path: String,
    branch: String,
}

/// Names the link or layer `mount` mounts `repository` as, pushed to `branch`, or `None` for the
/// pushing repository.
#[lore_macro::test_pub]
async fn mount_name(
    targets: &[PushTarget],
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    mount: Option<&Mount>,
) -> Option<MountName> {
    let mount = mount?;
    let kind = match mount {
        Mount::Link { .. } => "link",
        Mount::Layer(_) => "layer",
    };
    let branch = load_branch_metadata(repository, branch)
        .await
        .map_or_else(|_| branch.to_string(), |metadata| metadata.name);
    Some(MountName {
        kind,
        path: mount_path(targets, mount).await,
        branch,
    })
}

/// The path `mount` places its repository at in the pushing repository: the path of each mount on
/// the way to it.
async fn mount_path(targets: &[PushTarget], mount: &Mount) -> String {
    let mut segments = vec![];
    let mut mount = Some(mount);
    // A target is mounted in one reached before it, which keeps the walk finite.
    let mut bound = targets.len();
    while let Some(current) = mount {
        match current {
            Mount::Layer(path) => {
                segments.push(path.trim_end_matches('/').to_string());
                break;
            }
            Mount::Link {
                target,
                revision,
                link,
            } => {
                let Some(parent) = targets.get(*target).filter(|_| *target < bound) else {
                    break;
                };
                segments.push(link_path(&parent.repository, *revision, link).await);
                mount = parent.mount.as_ref();
                bound = *target;
            }
        }
    }
    segments.reverse();
    segments.join("/")
}

/// The path of the node holding `link` in the revision `revision` of `repository`.
async fn link_path(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    link: &LinkReference,
) -> String {
    let local_node = link.local_node();
    let path = match State::deserialize(repository.clone(), revision).await {
        Ok(state) => state.node_path(repository.clone(), local_node).await.ok(),
        Err(_) => None,
    };
    path.filter(|path| !path.is_empty())
        .unwrap_or_else(|| format!("node {local_node}"))
}

/// The push of `target` failing with `err`: see [`failed_mount_push`].
async fn failed_push(err: PushError, targets: &[PushTarget], target: usize) -> (PushError, String) {
    let pushed = &targets[target];
    failed_mount_push(
        err,
        targets,
        &pushed.repository,
        pushed.branch,
        pushed.mount.as_ref(),
    )
    .await
}

/// The error and the line reporting a push of `repository` to `branch`, mounted as `mount`, that
/// failed with `err`, naming the link or layer: its mount path, repository and branch. A link push
/// [`Divergent`] refused fails as [`LinkDivergent`].
async fn failed_mount_push(
    err: PushError,
    targets: &[PushTarget],
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    mount: Option<&Mount>,
) -> (PushError, String) {
    let Some(name) = mount_name(targets, repository, branch, mount).await else {
        let line = err.to_string();
        return (err, line);
    };
    let context = format!(
        "pushing the {} at {} (repository {}) to branch {}",
        name.kind, name.path, repository.id, name.branch
    );
    if repository.is_link() && err.is_divergent() {
        let divergent = LinkDivergent {
            path: name.path,
            branch: name.branch,
        };
        let line = divergent.to_string();
        return (
            PushError::LinkDivergent(divergent.chain_err_from(err, &context)),
            line,
        );
    }
    let line = format!(
        "Failed to push the {} at {} (branch {}): {err}",
        name.kind, name.path, name.branch
    );
    let mut err = err;
    err.push_trace(lore_error_set::Location::with_context(
        file!(),
        line!(),
        column!(),
        Arc::from(context),
    ));
    (err, line)
}

/// The error of a push that failed with `failures`, in the order their targets were reached: the
/// first, once each other one is reported on a line of its own.
#[lore_macro::test_pub]
fn report_failures(failures: Vec<(PushError, String)>) -> PushError {
    let mut failures = failures.into_iter();
    let Some((first, _)) = failures.next() else {
        return PushError::internal("the push failed without a failure");
    };
    let others: Vec<String> = failures.map(|(_, line)| line).collect();
    if !others.is_empty() {
        lore_error!("{}", others.join("\n"));
    }
    first
}

/// Upload what `state` holds that its first parent does not, of the addresses `collection`
/// keeps, and the revision the second parent of a merge names.
///
/// The peer needs this of every revision a push reaches, whether the revision goes on to
/// be offered as a new latest revision or sits on a line that is only named by a merge.
async fn upload_revision_fragments(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    storage: &Arc<StorageSession>,
    max_query_batch: Option<usize>,
    state: &Arc<State>,
    dry_run: bool,
    collection: Collection,
) -> Result<(), PushError> {
    let state_parent = State::deserialize(repository.clone(), state.parent_self())
        .await
        .forward::<PushError>("deserializing parent state")?;

    lore_debug!(
        "Calculating new fragments from {} to {}",
        state_parent.revision(),
        state.revision()
    );
    let mut fragments = state::collect_fragments(
        repository.clone(),
        state_parent,
        state.clone(),
        collection,
        Arc::new(StoreRequestTracker::default()),
    )
    .await
    .forward::<PushError>("collecting new fragments")?;

    if !state.parent_other().is_zero() {
        fragments.push(Address::zero_context_hash(state.parent_other()));
    }

    query_and_push_fragments(
        repository.clone(),
        branch,
        storage.clone(),
        max_query_batch,
        fragments,
        dry_run,
    )
    .await
}

/// Offer `revision` to the peer as the latest revision of `branch`, and return the peer's answer.
///
/// A peer that holds no such branch had it deleted while the push ran: the branch is created on
/// the peer again from `metadata`, and `revision` offered once more. A peer that refuses an offer
/// for a missing fragment has `recover` upload what it is missing, and `revision` is offered once
/// more. A push that stopped part way can leave a list on the peer without every fragment it
/// names, and a list the local store has since evicted hides those fragments from the collection
/// of [`Collection::New`]. `recover` runs at most once, and only after such a refusal.
#[allow(clippy::too_many_arguments)]
#[lore_macro::test_pub]
async fn offer_revision(
    revision_protocol: &dyn Revision,
    repository: RepositoryId,
    branch: BranchId,
    metadata: &BranchMetadata,
    remote_latest: Hash,
    revision: Hash,
    force: bool,
    fast_forward_merge: bool,
    recover: impl AsyncFnOnce() -> Result<(), PushError>,
) -> Result<BranchPushResponse, PushError> {
    let mut answer = revision_protocol
        .branch_push(branch, revision, force, fast_forward_merge)
        .await;
    if let Err(ProtocolError::NotFound(_)) = answer {
        lore_debug!("Branch push returned NotFound, recreating branch on server");
        event::LoreEvent::BranchPushBranchCreateBegin(LoreBranchPushBranchCreateBeginEventData {
            repository,
            branch,
            local_revision: remote_latest,
        })
        .send();
        create_branch(revision_protocol, branch, metadata).await?;
        event::LoreEvent::BranchPushBranchCreateEnd(LoreBranchPushBranchCreateEndEventData {
            repository,
            branch,
            remote_revision: remote_latest,
        })
        .send();
        answer = revision_protocol
            .branch_push(branch, revision, force, fast_forward_merge)
            .await;
    }
    if let Err(ProtocolError::AddressNotFound(missing)) = &answer {
        lore_debug!(
            "Branch push missing fragment {}, collecting through the remote",
            Address::from(&missing.address[..])
        );
        recover().await?;
        answer = revision_protocol
            .branch_push(branch, revision, force, fast_forward_merge)
            .await;
    }
    forward_branch_push(answer)
}

/// Upload those of `fragments` the peer answers that it is missing, reporting progress
/// while the upload runs. A dry run stops once the peer has answered.
async fn query_and_push_fragments(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    storage: Arc<StorageSession>,
    max_query_batch: Option<usize>,
    fragments: Vec<Address>,
    dry_run: bool,
) -> Result<(), PushError> {
    let push_stats = execution_context().push_stats().clone();
    let progress = Arc::new(PushProgress::new(push_stats.clone()));
    let fragments = push_query(storage.clone(), fragments, max_query_batch, &push_stats).await?;

    let repository_id = repository.id;
    event::LoreEvent::BranchPushFragmentBegin(LoreBranchPushFragmentBeginEventData {
        repository: repository_id,
        branch,
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
            event::LoreEvent::BranchPushFragmentProgress(
                ticker_progress.event(repository_id, branch),
            )
            .send();
        }
    }));

    if !dry_run {
        push_fragments(repository, storage, fragments, progress.clone()).await?;
    }

    // The ticker sends no progress event after the end event once it has stopped.
    ticker.abort();
    let _ = ticker.await;

    // Emit a final progress event with the completed values now that the
    // ticker has been dropped and push_fragments has finished.
    let final_progress = progress.event(repository_id, branch);
    event::LoreEvent::BranchPushFragmentProgress(final_progress.clone()).send();

    event::LoreEvent::BranchPushFragmentEnd(LoreBranchPushFragmentEndEventData {
        repository: repository_id,
        branch,
        fragments: final_progress.complete,
        bytes_transferred: final_progress.bytes_transferred,
    })
    .send();

    Ok(())
}
