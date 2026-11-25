// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use bytes::BytesMut;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_storage::FragmentWriteCounts;
use serde::Serialize;
use smallvec::SmallVec;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::change;
use crate::change::FileAction;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::file::dirty::MovedNode;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::CaseLookup;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::HoldOptions;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::fs::hold;
use crate::immutable;
use crate::infer::ConflictMarkers;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreMetadataType;
use crate::interface::LoreString;
use crate::layer;
use crate::link;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore::TypedBytes;
use crate::lore::TypedBytesMut;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_info;
use crate::lore_trace;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::metadata::MetadataType;
use crate::node;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeDelta;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFileMetadataFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::NodeLink;
use crate::node::NodeNameLock;
use crate::node::ROOT_NODE;
use crate::progress::DEFAULT_WORK_CHANNEL_CAPACITY;
use crate::progress::DiscoveryStats;
use crate::progress::MAX_CONCURRENT_DIRECTORY_TASKS;
use crate::progress::ProgressTicker;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::verify::verify_state_for_commit;
use crate::revision::sync;
use crate::state;
use crate::state::LinkMergeEntry;
use crate::state::LinkPinAgreement;
use crate::state::LinkReference;
use crate::state::Mark;
use crate::state::NodeMapping;
use crate::state::OwnFlags;
use crate::state::RecordedModifiedTimes;
use crate::state::Return;
use crate::state::ReturnOptions;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;
use crate::util::serde::u8_as_bool;

/// Event data reported at the start of a commit.
#[repr(C)]
#[derive(
    Clone, Default, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitBeginEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Progress counters describing how far a commit has advanced.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitCountData {
    /// Number of directories processed so far.
    pub directory_count: u64,
    /// Total number of directories to process.
    pub directory_total: u64,
    /// Number of files processed so far.
    pub file_count: u64,
    /// Total number of files to process.
    pub file_total: u64,
    /// Number of directories deleted.
    pub directory_delete_count: u64,
    /// Number of files modified.
    pub file_modify_count: u64,
    /// Number of files deleted.
    pub file_delete_count: u64,
    /// Number of content bytes transferred so far.
    pub bytes_transferred: u64,
    /// Total number of content bytes to transfer.
    pub bytes_total: u64,
    /// Set when file and directory discovery has finished.
    #[serde(with = "u8_as_bool")]
    pub discovery_complete: u8,
}

impl LoreRevisionCommitCountData {
    /// The counters as they stand. A count is incremented with `Release` once its total holds what
    /// it counts, and loaded here with `Acquire` ahead of its total, so no count reads ahead of its
    /// total.
    #[lore_macro::test_pub]
    fn new(stats: Arc<CommitStats>) -> Self {
        let directory_count = stats.complete.directory_count.load(Ordering::Acquire);
        let file_count = stats.complete.file_count.load(Ordering::Acquire);
        Self {
            directory_count,
            directory_total: stats.complete.directory_total.load(Ordering::Relaxed),
            file_count,
            file_total: stats.complete.file_total.load(Ordering::Relaxed),
            directory_delete_count: stats
                .complete
                .directory_delete_count
                .load(Ordering::Relaxed),
            file_modify_count: stats.complete.file_modify_count.load(Ordering::Relaxed),
            file_delete_count: stats.complete.file_delete_count.load(Ordering::Relaxed),
            bytes_transferred: stats.complete.bytes_transferred.load(Ordering::Relaxed),
            bytes_total: stats.discovery.total_bytes.load(Ordering::Relaxed),
            discovery_complete: stats.discovery.complete.load(Ordering::Relaxed) as u8,
        }
    }
}

/// Event data reporting commit progress.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitProgressEventData {
    /// Current progress counters.
    pub count: LoreRevisionCommitCountData,
}

/// Event data reported at the end of a commit.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitEndEventData {
    /// Final progress counters.
    pub count: LoreRevisionCommitCountData,
}

/// How many files a commit wrote, split by the action each was staged with.
///
/// The actions are exclusive: a file is counted once, under the action its staged
/// node flags name.
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Debug,
    Serialize,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct LoreCommitFileStatsData {
    /// Files staged as new additions.
    pub added: u64,
    /// Files whose content or mode changed.
    pub modified: u64,
    /// Files staged for deletion.
    pub deleted: u64,
    /// Files staged as moves.
    pub moved: u64,
    /// Files staged as copies.
    pub copied: u64,
    /// Directories staged for deletion.
    pub directories_deleted: u64,
    /// Files the commit read off disk and fragmented. A different set from
    /// `files`: a staged file whose content turns out to match the revision it is
    /// committed against is read and committed as nothing, a view-excluded path is
    /// committed from its staged node without being read, and an in-memory commit
    /// reads none at all.
    pub files_read: u64,
    /// Uncompressed content bytes of `files_read`. The same number the progress
    /// event reports as `bytesTransferred`.
    pub bytes_transferred: u64,
    /// Files whose content the commit wrote: `added + modified + moved + copied`.
    pub files: u64,
    /// Uncompressed content size of exactly the `files` above, so the two are a
    /// pair. A delete contributes none.
    ///
    /// Distinct from [`FragmentWriteCounts::data_content_bytes`], which counts
    /// fragments rather than files and excludes every fragment that needed no
    /// payload.
    pub file_bytes: u64,
}

/// Event data reporting what a commit cost.
///
/// Emitted once, when the commit has drained every background write, at
/// statistics level one and above. A commit that failed reports what it had done
/// by then.
#[repr(C)]
#[derive(
    Clone, Copy, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitStatsEventData {
    /// Files committed, by action.
    pub files: LoreCommitFileStatsData,
    /// What the commit's fragment writes cost.
    pub fragments: FragmentWriteCounts,
}

/// Event data describing a revision produced by a commit.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitRevisionEventData {
    /// Identifier of the repository the revision belongs to.
    pub repository: RepositoryId,
    /// Identifier of the branch the revision was committed on.
    pub branch: BranchId,
    /// Signature of the committed revision.
    pub revision: Hash,
    /// Sequential number of the revision.
    pub revision_number: u64,
    /// Signature of the first parent revision.
    pub parent: Hash,
    /// Signature of the second parent revision, set for a merge.
    pub parent_other: Hash,
}

#[error_set]
pub enum CommitError {
    Cancelled,
    // User-actionable
    NothingStaged,
    BranchAdvanced,
    Conflict,
    MissingIdentity,
    // Forwarded from AnchorError, StateErrors
    NotFound,
    NodeNotFound,
    LinkNotFound,
    LinkPinsDiverge,
    // Forwarded from MetadataError
    FileNotFound,
    InvalidArguments,
    InvalidPath,
    InvalidNodeHierarchy,
    // Forwarded from ImmutableError
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    NotConnected,
    Oversized,
    // Forwarded from LayerError
    AlreadyLinked,
    LayerNotFound,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NoRemote,
    NotSupported,
    // Forwarded from BranchError
    BranchNotFound,
    BranchAlreadyExists,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    LocalModifications,
    MaxHistorySearchDepth,
    RevisionNotFound,
    WriteRequired,
    // Link-scoped commit errors
    LinkPathNotFound,
    NotALink,
    // Layer-scoped commit errors
    NotALayer,
    // Repository-scoped (flowed in via BranchError)
    IdenticalMetadata,
    LockNotFound,
    LockNotOwned,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
}

impl EventError for CommitError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[lore_macro::test_pub]
#[derive(Default)]
struct CommitCompleteStats {
    pub directory_count: AtomicU64,
    pub directory_total: AtomicU64,
    pub file_count: AtomicU64,
    pub file_total: AtomicU64,
    pub directory_delete_count: AtomicU64,
    pub file_delete_count: AtomicU64,
    /// Files the commit wrote, whatever action staged them. Reported as
    /// `fileModifyCount` on the progress event.
    pub file_modify_count: AtomicU64,
    /// Content bytes read off disk and fragmented.
    pub bytes_transferred: AtomicU64,
}

/// The counts the statistics event alone reports: which action staged each file
/// the commit wrote, and how many files were read to write them. No progress event
/// reads any of it, so none of it is kept at statistics level zero.
///
/// Deletes are not here; `file_delete_count` and `directory_delete_count` above
/// carry those.
#[derive(Default)]
struct CommitReportStats {
    pub add_count: AtomicU64,
    pub modify_count: AtomicU64,
    pub move_count: AtomicU64,
    pub copy_count: AtomicU64,
    /// Content size of the files the counters above counted, which is a
    /// different set from `bytes_transferred`: that one covers every file the
    /// commit read, including one that turned out to match what it is committed
    /// against.
    pub file_bytes: AtomicU64,
    /// Files read off disk and fragmented. A delete is never read; a
    /// view-excluded path is committed without being read.
    pub file_read_count: AtomicU64,
}

#[lore_macro::test_pub]
pub(crate) struct CommitStats {
    discovery: DiscoveryStats,
    complete: CommitCompleteStats,
    report: CommitReportStats,
    /// Whether to keep [`CommitReportStats`], which nothing reads at statistics
    /// level zero.
    statistics: bool,
}

impl AsRef<DiscoveryStats> for CommitStats {
    fn as_ref(&self) -> &DiscoveryStats {
        &self.discovery
    }
}

impl CommitStats {
    /// Counters for one commit, keeping what the call's statistics level reports.
    #[lore_macro::test_pub]
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            discovery: DiscoveryStats::default(),
            complete: CommitCompleteStats::default(),
            report: CommitReportStats::default(),
            statistics: crate::runtime::try_execution_context()
                .is_some_and(|context| context.globals().stats()),
        })
    }

    /// Sends a progress event with the counts as they stand, in order with the other progress
    /// events of the commit ([`DiscoveryStats::send_progress`]).
    fn send_progress(self: &Arc<Self>) {
        self.discovery.send_progress(|| {
            self.send_progress_event();
        });
    }

    /// Sends a progress event with the counts as they stand, without the order lock
    /// ([`DiscoveryStats::send_progress`] holds it).
    fn send_progress_event(self: &Arc<Self>) {
        event::LoreEvent::RevisionCommitProgress(LoreRevisionCommitProgressEventData {
            count: LoreRevisionCommitCountData::new(self.clone()),
        })
        .send();
    }

    /// Counts one directory in the total and as done, the total first, as
    /// [`LoreRevisionCommitCountData::new`] reads them.
    #[lore_macro::test_pub]
    fn count_directory(&self) {
        self.complete
            .directory_total
            .fetch_add(1, Ordering::Relaxed);
        self.complete
            .directory_count
            .fetch_add(1, Ordering::Release);
    }

    /// Counts one file in the total and as done, the total first, as
    /// [`LoreRevisionCommitCountData::new`] reads them.
    #[lore_macro::test_pub]
    fn count_file(&self) {
        self.complete.file_total.fetch_add(1, Ordering::Relaxed);
        self.complete.file_count.fetch_add(1, Ordering::Release);
    }

    /// One file was read off disk and fragmented. The byte total is what a
    /// progress event reports as transferred, so it is kept whatever the level.
    #[lore_macro::test_pub]
    fn file_read(&self, content_size: u64) {
        if self.statistics {
            self.report.file_read_count.fetch_add(1, Ordering::Relaxed);
        }
        self.complete
            .bytes_transferred
            .fetch_add(content_size, Ordering::Relaxed);
    }

    /// The per-action file counts, as an event payload.
    #[lore_macro::test_pub]
    fn file_stats(&self) -> LoreCommitFileStatsData {
        let added = self.report.add_count.load(Ordering::Relaxed);
        let modified = self.report.modify_count.load(Ordering::Relaxed);
        let moved = self.report.move_count.load(Ordering::Relaxed);
        let copied = self.report.copy_count.load(Ordering::Relaxed);
        LoreCommitFileStatsData {
            files_read: self.report.file_read_count.load(Ordering::Relaxed),
            bytes_transferred: self.complete.bytes_transferred.load(Ordering::Relaxed),
            added,
            modified,
            deleted: self.complete.file_delete_count.load(Ordering::Relaxed),
            moved,
            copied,
            directories_deleted: self.complete.directory_delete_count.load(Ordering::Relaxed),
            files: added + modified + moved + copied,
            file_bytes: self.report.file_bytes.load(Ordering::Relaxed),
        }
    }

    /// Record which action staged one file the commit wrote, and the uncompressed
    /// size of the content it committed.
    ///
    /// A file with no staged action counts as modified: it is here because its
    /// content or mode changed under it.
    #[lore_macro::test_pub]
    fn record_file_action(&self, flags: u16, content_size: u64) {
        if !self.statistics {
            return;
        }
        let counter = match change::FileAction::from_node_flags(flags) {
            change::FileAction::Add => &self.report.add_count,
            change::FileAction::Move => &self.report.move_count,
            change::FileAction::Copy => &self.report.copy_count,
            _ => &self.report.modify_count,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.report
            .file_bytes
            .fetch_add(content_size, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Default)]
pub struct CommitOptions {
    /// Message for the main repository (and default for links/layers without a specific message)
    pub message: String,
    /// Per-link commit messages, keyed by link relative path (forward-slash separated,
    /// e.g., `"vendor/engine"`). Must match the paths returned by `link list --staged`
    /// and shown by `urc link list`. If a link path is not present in this map, the main
    /// `message` is used as fallback.
    pub link_messages: HashMap<String, String>,
    /// If set, commit only changes in this linked repository path
    pub link: Option<String>,
    /// Per-layer commit messages, keyed by layer `target_path` (the mount path in the
    /// parent repository). If a layer path is not present in this map, the main
    /// `message` is used as fallback.
    pub layer_messages: HashMap<String, String>,
    /// If set, commit only changes in this layer path
    pub layer: Option<String>,
}

impl CommitOptions {
    pub fn new(message: String) -> Self {
        Self {
            message,
            ..Default::default()
        }
    }
}

/// How one commit reports on itself, carried through the commit as a unit.
///
/// The counters cover the whole commit, including the sub-commits it performs
/// inside linked and layered repositories, so what is reported is the commit a
/// caller asked for rather than one repository's share of it. The fragment
/// counters it reports alongside live on the execution context, and are shared for
/// the same reason.
#[derive(Clone)]
struct CommitReporting {
    /// Emit the statistics event at all. Off still counts files, the progress
    /// events needing the same counters.
    enabled: bool,
    /// Emit a `FragmentWrite` event per stored fragment.
    per_fragment: bool,
    /// How often to emit progress events.
    interval: std::time::Duration,
    /// Counters for the whole commit.
    stats: Arc<CommitStats>,
}

impl CommitReporting {
    /// Read what to report from the call's global arguments.
    fn from_globals() -> Self {
        let context = execution_context();
        let globals = context.globals();
        Self {
            enabled: globals.stats(),
            per_fragment: globals.stats_full(),
            interval: globals.event_interval(),
            stats: CommitStats::new(),
        }
    }

    /// A guard that emits the statistics event when dropped, so the commit that
    /// fails reports as the one that succeeds does.
    ///
    /// Hold it until after the commit has drained its background writes; the
    /// fragment counters settle only then. Reporting turned off yields `None`.
    fn report(&self) -> Option<CommitStatsReport> {
        self.enabled.then(|| CommitStatsReport {
            reporting: self.clone(),
            baseline: crate::runtime::try_execution_context()
                .map_or_default(|context| context.fragment_stats().snapshot()),
        })
    }
}

/// Emits what the commit cost when dropped.
struct CommitStatsReport {
    reporting: CommitReporting,
    /// The call's fragment counters as the commit found them. They are per call
    /// rather than per commit, so an operation that writes before it commits — a
    /// merge serializing its staged state, say — would otherwise have those
    /// writes reported as the commit's.
    baseline: FragmentWriteCounts,
}

impl Drop for CommitStatsReport {
    fn drop(&mut self) {
        immutable::send_fragment_stats(&self.baseline, |fragments| {
            event::LoreEvent::RevisionCommitStats(LoreRevisionCommitStatsEventData {
                files: self.reporting.stats.file_stats(),
                fragments,
            })
        });
    }
}

/// Boxed version of [`commit_with_metadata`] for cross-crate use, without metadata.
pub fn commit_boxed(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
) -> crate::BoxFuture<'_, Result<Hash, CommitError>> {
    commit_with_metadata_boxed(
        repository,
        token,
        options,
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
    )
}

/// Wraps [`commit_with_metadata_in_operation`] in the one filesystem operation the whole call
/// reads the working tree through, for the parent and for every link and layer mounted in it.
///
/// The working tree is written to only where a commit resolves a staged merge conflict, which
/// removes the copies that merge left beside the file.
///
/// A provider that holds its state is handed each revision the instance's repository commits
/// ([`InstanceOperation::committed`]), and is refused a commit of a link alone or of a layer,
/// which would make a revision current that it is not handed.
///
/// A commit of this instance cut short before its anchors reached disk is finished first, through
/// an operation of its own, or the call fails where something was staged after it
/// ([`complete_interrupted_commit`]).
#[lore_macro::test_pub]
pub(crate) async fn commit_with_metadata(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
) -> Result<Hash, CommitError> {
    complete_interrupted_commit(&repository).await?;
    with_operation(repository.file_system(), async |operation| {
        Box::pin(commit_with_metadata_in_operation(
            &operation, repository, token, options, keys, values, formats,
        ))
        .await
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn commit_with_metadata_in_operation(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
) -> Result<Hash, CommitError> {
    if operation.holds_state() {
        if options.link.is_some() {
            return Err(hold::refused("commit a link alone").into());
        }
        if options.layer.is_some() {
            return Err(hold::refused("commit a layer").into());
        }
    }

    let reporting = CommitReporting::from_globals();
    let _stats_report = reporting.report();

    if !options.link_messages.is_empty() {
        let (state_current, state_staged, _) =
            State::deserialize_current_and_staged(repository.clone())
                .await
                .forward::<CommitError>("Failed to deserialize current and staged state")?;
        link::validate_link_message_paths(
            &state_staged.unwrap_or(state_current),
            &repository,
            options.link_messages.keys(),
        )
        .await
        .forward::<CommitError>("validating link message paths")?;
    }

    if options.layer.is_none() {
        Box::pin(refuse_link_commits_losing_work(
            &repository,
            options.link.as_deref(),
            &options.link_messages,
        ))
        .await?;
    }

    if let Some(ref link_path) = options.link {
        return commit_link_only(
            operation,
            repository,
            token.share(),
            options.message.clone(),
            link_path.clone(),
            keys,
            values,
            formats,
            reporting.clone(),
        )
        .await;
    }

    if let Some(ref layer_path) = options.layer {
        let layer_message = options
            .layer_messages
            .get(layer_path)
            .cloned()
            .unwrap_or_else(|| options.message.clone());
        return commit_layer_only(
            operation,
            repository,
            token.share(),
            layer_message,
            layer_path.clone(),
            keys,
            values,
            formats,
            reporting.clone(),
        )
        .await;
    }

    commit_staged_with_layers(
        operation, repository, token, options, keys, values, formats, &reporting,
    )
    .await
}

/// Commits the staged revision, then each layer staged beside it.
///
/// A staged revision with nothing to commit while a layer has changes, such as one holding only
/// dirty nodes, stays staged as it is, and the layers commit.
///
/// Its own future: inline, what it holds across its awaits would be reserved in the states of
/// [`commit_with_metadata_in_operation`] that commit a link or a layer alone.
#[allow(clippy::too_many_arguments)]
async fn commit_staged_with_layers(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
    reporting: &CommitReporting,
) -> Result<Hash, CommitError> {
    let context = execution_context();
    let globals = context.globals();
    let dry_run = globals.dry_run();

    let layers = layer::list(repository.clone()).await.unwrap_or_default();
    let mut layer_staged = false;
    for layer in &layers {
        lore_debug!("Layer status: {layer:?}");
        if !layer.staged.is_zero() && layer.staged != layer.current {
            layer_staged = true;
        }
    }
    if operation.holds_state()
        && layers.iter().any(|layer| {
            !layer.staged.is_zero() && (globals.force() || layer.staged != layer.current)
        })
    {
        return Err(hold::refused("commit a layer").into());
    }

    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<CommitError>("Failed to deserialize current revision anchor")?;
    let staged_revision = match crate::instance::load_staged_revision(&repository).await {
        Ok(Some(revision)) => revision,
        Ok(None) => {
            if globals.force() || layer_staged {
                current_revision
            } else {
                return Err(NothingStaged.into());
            }
        }
        Err(_err) => {
            if globals.force() || layer_staged {
                current_revision
            } else {
                return Err(NothingStaged.into());
            }
        }
    };

    if !globals.force() && current_revision == staged_revision && !layer_staged {
        return Err(NothingStaged.into());
    }

    lore_debug!("Commit staged revision: {staged_revision}");

    // Early check: has another instance advanced the branch latest pointer?
    // This is placed before expensive commit work (fragmenting, rehashing) so
    // the user gets a fast failure. The mutable store filesystem lock ensures
    // atomicity regardless of check placement.
    let branch_latest = branch::load_latest(repository.clone(), current_branch)
        .await
        .unwrap_or_default();
    if !globals.force() && !branch_latest.is_zero() && branch_latest != current_revision {
        return Err(BranchAdvanced.into());
    }

    let state_staged =
        State::deserialize_staged(repository.clone(), staged_revision, current_revision)
            .await
            .forward_with::<CommitError, _>(|| {
                format!("Failed to deserialize revision state {staged_revision}")
            })?;
    let state_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {current_revision}")
        })?;
    reject_unresolved_link_merge_conflicts(&repository, &state_staged, &link::Mount::root())
        .await?;
    reject_unmovable_link_mounts(&repository, &state_staged, None).await?;

    let metadata = build_commit_metadata(
        repository.clone(),
        &state_staged,
        current_branch,
        options.message.clone(),
        keys,
        values,
        formats,
    )
    .await?;

    let link_messages = Arc::new(options.link_messages);
    let layer_messages = Arc::new(options.layer_messages);

    // Snapshot the dirty-only nodes before pruning: after the commit, the staged
    // anchor is rebuilt from them and the moves the prune returned ([`DirtyOnly`]).
    let mut dirty = DirtyOnly::default();
    state::collect_dirty_only_paths(
        state_staged.clone(),
        repository.clone(),
        ROOT_NODE,
        RelativePathBuf::new(),
        &mut dirty,
    )
    .await
    .forward::<CommitError>("Failed collecting dirty paths from staged state")?;

    lore_debug!(
        "Collected {} dirty paths and {} dirty marks",
        dirty.paths.len(),
        dirty.marks.len()
    );

    // Capture the merge parents now — `finalize_commit` will overwrite
    // `parent_self` with the new revision hash signature, which would prevent us
    // from matching against a `merge_carry` blob below.
    let merge_parent_self = state_staged.parent_self();
    let merge_parent_other = state_staged.parent_other();

    state_staged.hold_base(state_current.clone());
    let pruned =
        prune_dirty_for_commit(state_staged.clone(), repository.clone(), current_revision).await?;
    let left_out: Vec<NodeID> = pruned.left_out().collect();
    let mut left = LeftToMark::default();

    let mut signature = current_revision;
    if current_revision != staged_revision {
        lore_debug!(
            "Committing revision, current revision {}, staged revision {}",
            state_current.revision(),
            state_staged.revision()
        );
        'parent: {
            let link_commits = Arc::default();
            let (committed, modified_times) = match commit_staged_revision(
                operation,
                repository.clone(),
                token.share(),
                state_current.clone(),
                state_staged.clone(),
                state_staged.committed_revision(),
                vec![(RelativePath::new(), ROOT_NODE)],
                metadata.clone(),
                link_messages.clone(),
                current_branch,
                reporting.clone(),
                MissingPaths::Settle,
                left_out,
                Some(&mut left),
                &link_commits,
            )
            .await
            {
                Ok(committed) => committed,
                Err(err) if layer_staged && err.is_nothing_staged() => break 'parent,
                Err(err) => return Err(err),
            };
            signature = committed;

            if let Err(err) = finalize_commit(
                repository.clone(),
                &state_current,
                &state_staged,
                branch_latest,
                signature,
                current_branch,
                token,
                Some((operation, &modified_times)),
            )
            .await
            {
                link_commits
                    .roll_back_unpublished((&repository, current_branch, signature))
                    .await;
                return Err(err);
            }

            modified_times.store(repository.clone()).await;

            event::metadata::send(&metadata);
        }
    }

    let (dirty_paths, carried) = dirty.carried(pruned, left);
    if !dry_run
        && signature != current_revision
        && (!dirty_paths.is_empty() || !carried.is_empty())
        && let Err(error) = crate::file::dirty::dirty_relative_paths_in_operation(
            operation,
            repository.clone(),
            dirty_paths,
            Some(&carried),
        )
        .await
    {
        lore_warn!(
            "Committed revision {signature}, but marking its dirty changes again failed, so they \
             stay unmarked until `lore status --scan` finds them: {error}"
        );
    }

    // Apply any merge dirty-tracking carry. `take_matching` only returns
    // paths when the blob's parents match the merge we just committed, and
    // always clears the blob so a stale carry can't outlive this commit.
    if !dry_run
        && let Err(error) = apply_merge_carry(
            operation,
            &repository,
            merge_parent_self,
            merge_parent_other,
        )
        .await
    {
        lore_warn!(
            "Committed revision {signature}, but marking the merge's dirty changes failed, so \
             they stay unmarked until `lore status --scan` finds them: {error}"
        );
    }

    for layer in layers {
        if layer.staged.is_zero() || (!globals.force() && layer.staged == layer.current) {
            continue;
        }

        let layer_state = layer
            .deserialize_current_and_staged(repository.clone())
            .await
            .forward::<CommitError>("Failed to deserialize layer states")?;
        let layer_repository = layer_state.repository.clone();

        lore_debug!(
            "Committing layer revision, current revision {}, staged revision {}",
            layer_state.state_current.revision(),
            layer_state.state_staged.revision()
        );
        let layer_metadata = if let Some(layer_msg) = layer_messages.get(layer.target_path.as_str())
        {
            let mut overridden = (*metadata).clone();
            overridden
                .set_string(metadata::MESSAGE, layer_msg)
                .forward::<CommitError>("Failed to set layer commit message")?;
            Arc::new(overridden)
        } else {
            metadata.clone()
        };
        let (layer_node, layer_path) = layer_commit_root(
            &layer_repository,
            &layer_state.state_staged,
            layer.source_path.as_str(),
            layer.target_path.as_str(),
        )
        .await?;
        let link_commits = Arc::default();
        let layer_result = commit_staged_revision(
            operation,
            layer_repository.clone(),
            token.share(),
            layer_state.state_current.clone(),
            layer_state.state_staged.clone(),
            layer_state.state_staged.committed_revision(),
            vec![(layer_path, layer_node)],
            layer_metadata.clone(),
            Arc::new(HashMap::new()),
            current_branch,
            reporting.clone(),
            MissingPaths::Settle,
            Vec::new(),
            None,
            &link_commits,
        )
        .await;

        let (layer_signature, layer_modified_times) = match layer_result {
            Ok(committed) => committed,
            Err(err) if err.is_nothing_staged() => {
                // The parent revision is already committed at this point, so a
                // layer with nothing to commit must not fail the whole commit.
                lore_warn!(
                    "Layer at {} had nothing to commit, clearing its staged state",
                    layer.target_path
                );
                if !dry_run {
                    layer::store_layer_staged(
                        repository.clone(),
                        token,
                        layer.target_path.as_str(),
                        layer.repository,
                        Hash::default(),
                    )
                    .await
                    .forward::<CommitError>("Failed to clear layer staged state")?;
                }
                continue;
            }
            Err(err) => return Err(err),
        };
        let layer_branch = layer_state
            .state_current
            .branch(layer_repository.clone())
            .await;

        if !dry_run {
            // Without this `layer::latest_revision` cannot reach the new
            // layer revision.
            let layer_previous = branch::load_latest(layer_repository.clone(), current_branch)
                .await
                .unwrap_or_default();
            if let Err(err) = branch::store_latest(
                layer_repository.clone(),
                current_branch,
                layer_previous,
                layer_signature,
                BranchLatestStatus::Divergent,
            )
            .await
            .forward::<CommitError>("Failed to store layer branch latest")
            {
                link_commits.roll_back().await;
                return Err(err);
            }

            layer::store_layer_current(
                repository.clone(),
                token,
                layer.target_path.as_str(),
                layer.repository,
                layer_signature,
                Some(Hash::default()),
            )
            .await
            .forward::<CommitError>("Failed to store layer configuration")?;
        }

        layer_modified_times.store(layer_repository.clone()).await;

        event::LoreEvent::RevisionCommitRevision(LoreRevisionCommitRevisionEventData {
            repository: layer_repository.id,
            branch: layer_branch,
            revision: layer_signature,
            revision_number: layer_state.state_staged.revision_number(),
            parent: layer_state.state_staged.parent_self(),
            parent_other: layer_state.state_staged.parent_other(),
        })
        .send();
    }

    Ok(signature)
}

/// Boxed version of [`commit_with_metadata`] for cross-crate use.
pub fn commit_with_metadata_boxed(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
) -> crate::BoxFuture<'_, Result<Hash, CommitError>> {
    Box::pin(commit_with_metadata(
        repository, token, options, keys, values, formats,
    ))
}

/// Commits staged changes in a single layer without committing the parent.
///
/// Resolves the layer by `target_path` against the parent's layer config,
/// runs the existing commit pipeline against the layer from the node its
/// source path names, advances the local layer config to point at the new
/// layer revision, and emits a `RevisionCommitRevision` event for the layer.
///
/// The parent's staged anchor and tree state are NOT modified — layer pins
/// live in `.urc/layer.toml`, not in the parent's revision tree.
#[allow(clippy::too_many_arguments)]
async fn commit_layer_only(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    message: String,
    layer_path: String,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
    reporting: CommitReporting,
) -> Result<Hash, CommitError> {
    // Resolve the layer by target_path against the parent's configured layers.
    // Unlike the auto-bundle path (which falls back to "no layers" on error),
    // a scoped commit must surface config-read failures so the user knows
    // their explicit `--layer <path>` request couldn't be evaluated.
    let layers = layer::list(repository.clone())
        .await
        .forward::<CommitError>("Failed to load layer configuration")?;
    let layer = layers
        .into_iter()
        .find(|l| l.target_path == layer_path)
        .ok_or_else(|| -> CommitError {
            NotALayer {
                path: layer_path.clone(),
            }
            .into()
        })?;

    let layer_state = layer
        .deserialize_current_and_staged(repository.clone())
        .await
        .forward::<CommitError>("Failed to deserialize layer states")?;
    let layer_repository_ctx = layer_state.repository.clone();

    let context = execution_context();
    let globals = context.globals();

    // Match auto-bundle semantics: zero-staged unconditionally errors (nothing
    // to commit, force can't help), staged-matches-current errors unless
    // `--force` is set.
    if layer.staged.is_zero() || (!globals.force() && layer.staged == layer.current) {
        return Err(NothingStaged.into());
    }

    let (_parent_current_revision, parent_current_branch) =
        crate::instance::load_current_anchor(&repository)
            .await
            .forward::<CommitError>("Failed to deserialize current revision anchor")?;

    // Detect concurrent advancement of the layer's branch by another instance.
    // Mirrors the auto-bundle parent's branch-advanced check — if the layer's
    // branch latest pointer has moved past `layer.current`, the staged state
    // would create a divergent revision unless `--force` is set.
    let layer_branch = layer_state
        .state_current
        .branch(layer_state.repository.clone())
        .await;
    let layer_branch_latest = branch::load_latest(layer_state.repository.clone(), layer_branch)
        .await
        .unwrap_or_default();
    if !globals.force() && !layer_branch_latest.is_zero() && layer_branch_latest != layer.current {
        return Err(BranchAdvanced.into());
    }

    // Build metadata using the resolved (per-layer or fallback) message and
    // forward any custom metadata key/value/format args from the caller —
    // matches the link-scoped commit's handling.
    let metadata = build_commit_metadata(
        layer_state.repository.clone(),
        &layer_state.state_staged,
        parent_current_branch,
        message,
        keys,
        values,
        formats,
    )
    .await?;

    let (layer_node, layer_path) = layer_commit_root(
        &layer_state.repository,
        &layer_state.state_staged,
        layer.source_path.as_str(),
        layer.target_path.as_str(),
    )
    .await?;
    let link_commits = Arc::default();
    let (layer_signature, layer_modified_times) = commit_staged_revision(
        operation,
        layer_state.repository.clone(),
        token.share(),
        layer_state.state_current.clone(),
        layer_state.state_staged.clone(),
        layer_state.state_staged.committed_revision(),
        vec![(layer_path, layer_node)],
        metadata,
        Arc::new(HashMap::new()),
        parent_current_branch,
        reporting.clone(),
        MissingPaths::Settle,
        Vec::new(),
        None,
        &link_commits,
    )
    .await?;

    let layer_branch = layer_state
        .state_current
        .branch(layer_state.repository.clone())
        .await;

    if !globals.dry_run() {
        let layer_previous =
            branch::load_latest(layer_state.repository.clone(), parent_current_branch)
                .await
                .unwrap_or_default();
        if let Err(err) = branch::store_latest(
            layer_state.repository.clone(),
            parent_current_branch,
            layer_previous,
            layer_signature,
            BranchLatestStatus::Divergent,
        )
        .await
        .forward::<CommitError>("Failed to store layer branch latest")
        {
            link_commits.roll_back().await;
            return Err(err);
        }

        layer::store_layer_current(
            repository.clone(),
            &token,
            layer.target_path.as_str(),
            layer.repository,
            layer_signature,
            Some(Hash::default()),
        )
        .await
        .forward::<CommitError>("Failed to store layer configuration")?;
    }

    layer_modified_times
        .store(layer_state.repository.clone())
        .await;

    event::LoreEvent::RevisionCommitRevision(LoreRevisionCommitRevisionEventData {
        repository: layer_repository_ctx.id,
        branch: layer_branch,
        revision: layer_signature,
        revision_number: layer_state.state_staged.revision_number(),
        parent: layer_state.state_staged.parent_self(),
        parent_other: layer_state.state_staged.parent_other(),
    })
    .send();

    Ok(layer_signature)
}

/// Commits staged changes in a single linked repository without committing the parent.
///
/// The commit walks every mount of the repository pinning the revision staged at the link, and
/// pins every mount of it at the revision committed. Each repository on the way to a nested link
/// is then committed, inner to outer, so the top-level pin references a revision of each, and
/// every mount of it is pinned too. The parent state is staged so the pins are visible in
/// `lore status`; the parent is not committed.
///
/// A commit that fails before it stages the parent moves back every linked branch it moved
/// ([`LinkCommits::roll_back`]).
#[allow(clippy::too_many_arguments)]
async fn commit_link_only(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    message: String,
    link_path: String,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
    reporting: CommitReporting,
) -> Result<Hash, CommitError> {
    let link_commits = Arc::default();
    let committed = commit_link_only_moving(
        operation,
        repository,
        token,
        (message, link_path),
        (keys, values, formats),
        reporting,
        &link_commits,
    )
    .await;
    if committed.is_err() {
        link_commits.roll_back().await;
    }
    committed
}

/// [`commit_link_only`], recording in `link_commits` each linked branch it moves.
async fn commit_link_only_moving(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    (message, link_path): (String, String),
    (keys, values, formats): (
        LoreArray<LoreString>,
        LoreArray<LoreString>,
        LoreArray<LoreMetadataType>,
    ),
    reporting: CommitReporting,
    link_commits: &Arc<LinkCommits>,
) -> Result<Hash, CommitError> {
    let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<CommitError>("Failed to deserialize current anchor")?;

    // Load the parent's current state
    let state_parent_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {current_revision}")
        })?;

    // Load the parent's staged state (contains the modified link state)
    let parent_staged_revision = match crate::instance::load_staged_revision(&repository).await {
        Ok(Some(revision)) if revision != current_revision => revision,
        _ => return Err(NothingStaged.into()),
    };
    let state_parent_staged =
        State::deserialize_staged(repository.clone(), parent_staged_revision, current_revision)
            .await
            .forward_with::<CommitError, _>(|| {
                format!("Failed to deserialize revision state {parent_staged_revision}")
            })?;

    // Resolve the link in both staged and current parent states
    let resolved_staged =
        link::resolve_link_at_path(&state_parent_staged, repository.clone(), &link_path)
            .await
            .map_err(|err| {
                forward_refusal_or(err, "resolving the staged link at path", |err| {
                    if err.is_not_a_link() {
                        CommitError::NotALink(
                            NotALink {
                                path: link_path.clone(),
                            }
                            .chain_err_from(err, "resolving the staged link at path"),
                        )
                    } else {
                        CommitError::LinkPathNotFound(
                            LinkPathNotFound {
                                path: link_path.clone(),
                            }
                            .chain_err_from(err, "resolving the staged link at path"),
                        )
                    }
                })
            })?;

    // `resolve_link_chain` gives one shared mutable state per level so the pin write and the
    // propagation that follow the link's commit see the same objects.
    let link_path_rel = RelativePath::from_str(&link_path).unwrap_or_default();
    let chain = link::resolve_link_chain(
        NodeMapping::root(repository.clone(), state_parent_staged.clone()),
        state_parent_current.clone(),
        link_path_rel.clone(),
        current_branch,
    )
    .await
    .forward::<CommitError>("Failed to resolve link chain")?;

    let owner_repository = chain.innermost.repository.clone();
    let owner_state = chain.innermost.state.clone();

    // Locate the link node within the owning repo's staged state.
    let (link_local_node, _) = chain
        .innermost_link(&link_path_rel)
        .await
        .forward::<CommitError>("Failed to resolve link node in owning repository")?;
    // The link of the instance's own repository the commit goes through, whose rows it settles,
    // and the repository it links.
    let outermost = chain.levels.first().map_or(
        (link_local_node, resolved_staged.link_context.id),
        |level| (level.link_node_id, level.child_repository_id),
    );

    // The parent is written with its link rows, so only the link committed has to be settled
    // there. Each repository on the way to a nested link is committed whole, so what it holds
    // unresolved is refused before the link's commit.
    reject_unresolved_link_pins(
        &repository,
        &state_parent_staged,
        Some(outermost.0),
        &link::Mount::root(),
    )
    .await?;
    if !chain.levels.is_empty() {
        reject_unresolved_link_holders(&chain).await?;
    }
    reject_unmovable_link_mounts(&repository, &state_parent_staged, Some(outermost.1)).await?;

    let resolved_current =
        link::resolve_link_at_path(&state_parent_current, repository.clone(), &link_path)
            .await
            .map_err(|err| {
                forward_refusal_or(err, "resolving link at path", |err| {
                    CommitError::LinkPathNotFound(
                        LinkPathNotFound {
                            path: link_path.clone(),
                        }
                        .chain_err_from(err, "resolving link at path"),
                    )
                })
            })?;

    let link_staged_revision = resolved_staged.link_node.linked_node().revision;
    let link_current_revision = resolved_current.link_node.linked_node().revision;
    // A link follows the branch of the repository holding it.
    let link_branch = resolved_staged
        .link_reference
        .resolve_branch(chain.owner_branch(current_branch));

    if link_current_revision == link_staged_revision {
        return Err(NothingStaged.into());
    }

    let link_repository = repository
        .to_link_context(resolved_staged.link_context.id)
        .await;
    let link_state_staged = State::deserialize(link_repository.clone(), link_staged_revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {link_staged_revision}")
        })?;
    let based_on = link_based_on(
        &owner_repository,
        &owner_state,
        &resolved_staged.link_reference,
        &link_state_staged,
    )
    .await?;
    let link_base = link_branch_base(&link_state_staged, based_on);
    let link_previous = latest_to_replace(
        &link_repository,
        link_branch,
        &link_state_staged,
        &link_path_rel,
    )
    .await?;
    let link_state_current = State::deserialize(link_repository.clone(), link_base)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {link_base}")
        })?;
    let Ok(link_mount_path) = RelativePath::from_str(&link_path);
    let link_mount = link::Mount::of_link(
        &link_repository,
        &link_state_staged,
        resolved_staged.link_node.child,
        link_mount_path,
    )
    .await
    .forward::<CommitError>("Failed resolving the path the link exposes")?;
    reject_unresolved_link_tree(
        &link_repository,
        &link_state_staged,
        &link_mount,
        resolved_staged.link_node.child,
    )
    .await?;
    let holder_roots = chain_holder_roots(&chain).await?;
    let owner_roots = holder_roots.last().map_or(&[][..], Vec::as_slice);
    let link_roots = mounts_pinning(
        (&owner_repository, &owner_state),
        owner_roots,
        &resolved_staged.link_node,
        (link_local_node, link_mount.path.clone()),
    )
    .await?;

    let link_built_on = link_state_staged.committed_revision();
    let metadata = build_commit_metadata(
        link_repository.clone(),
        &link_state_staged,
        link_branch,
        message.clone(),
        keys,
        values,
        formats,
    )
    .await?;

    lore_debug!(
        "Link-scoped commit: current {} staged {}",
        link_state_current.revision(),
        link_state_staged.revision()
    );

    let (link_signature, link_modified_times) = commit_staged_revision(
        operation,
        link_repository.clone(),
        token.share(),
        link_state_current.clone(),
        link_state_staged.clone(),
        link_built_on,
        link_roots,
        metadata,
        Arc::new(HashMap::new()),
        link_branch,
        reporting.clone(),
        MissingPaths::Settle,
        Vec::new(),
        None,
        link_commits,
    )
    .await?;

    commit_link_finalize(
        link_commits,
        (&link_repository, link_branch),
        (&link_state_current, &link_state_staged),
        (link_previous, link_signature),
        &token,
    )
    .await?;

    link_modified_times.store(link_repository.clone()).await;

    repin_mounts_of_committed(
        operation,
        (&owner_repository, &owner_state),
        owner_roots,
        (
            resolved_staged.link_context.id,
            resolved_staged.link_reference.branch,
        ),
        (Some(link_staged_revision), link_signature),
    )
    .await?;

    for i in (0..chain.levels.len()).rev() {
        let level = &chain.levels[i];
        let (child_state, child_repository) = chain.child_at(i);

        let child_staged = (child_state.revision_number() == 0).then(|| child_state.revision());
        let child_built_on = child_state.committed_revision();
        let level_reference = level
            .state
            .link_find(
                level.repository.clone(),
                level.child_repository_id,
                level.link_node_id,
            )
            .await
            .forward::<CommitError>("Failed to read the row of a link holding the link")?;
        let child_based_on = link_based_on(
            &level.repository,
            &level.state,
            &level_reference,
            &child_state,
        )
        .await?;
        let child_base = link_branch_base(&child_state, child_based_on);
        let child_current = if child_base == level.old_signature {
            level.current_child.clone()
        } else {
            State::deserialize(child_repository.clone(), child_base)
                .await
                .forward_with::<CommitError, _>(|| {
                    format!("Failed to deserialize revision state {child_base}")
                })?
        };
        child_state.reparent_onto(child_base);

        let child_metadata = build_commit_metadata(
            child_repository.clone(),
            &child_state,
            level.branch,
            message.clone(),
            LoreArray::default(),
            LoreArray::default(),
            LoreArray::default(),
        )
        .await?;

        let child_previous =
            latest_to_replace(&child_repository, level.branch, &child_state, &level.mount).await?;
        let (child_signature, child_modified_times) = commit_staged_revision(
            operation,
            child_repository.clone(),
            token.share(),
            child_current.clone(),
            child_state.clone(),
            child_built_on,
            vec![(RelativePath::new(), ROOT_NODE)],
            child_metadata,
            Arc::new(HashMap::new()),
            level.branch,
            reporting.clone(),
            MissingPaths::Refuse,
            Vec::new(),
            None,
            link_commits,
        )
        .await?;

        commit_link_finalize(
            link_commits,
            (&child_repository, level.branch),
            (&child_current, &child_state),
            (child_previous, child_signature),
            &token,
        )
        .await?;

        child_modified_times.store(child_repository.clone()).await;

        repin_mounts_of_committed(
            operation,
            (&level.repository, &level.state),
            &holder_roots[i],
            (level.child_repository_id, level.stored_branch),
            (child_staged, child_signature),
        )
        .await?;
    }

    // The commit holds the merges of the link and of the links on the way to it, so the rows the
    // instance's own repository keeps for the outermost of them, which would roll it back behind
    // the commit, go.
    let dry_run = execution_context().globals().dry_run();
    let ended = !dry_run
        && !state_parent_staged.link_merge_hash().is_zero()
        && settle_committed_link_rows(&repository, &state_parent_staged, outermost).await?;

    if state_parent_staged.holds_promoted_moves() {
        Box::pin(crate::file::stage::stage_promoted_moves_of_call(
            &repository,
            &state_parent_staged,
            None,
        ))
        .await
        .forward::<CommitError>("Failed to stage the move of a directory holding the link")?;
    }
    if !dry_run {
        let parent_signature = state_parent_staged
            .serialize(repository.clone(), &token)
            .await
            .forward::<CommitError>("Failed to serialize parent state")?;

        crate::instance::store_staged_anchor(&repository, parent_signature)
            .await
            .forward::<CommitError>("Failed to store staged anchor")?;
    }
    // Only a commit that ends a merge restores its carry, so that is boxed rather than held by
    // every link's commit.
    if ended {
        Box::pin(branch::merge::drop_merge_carry(&repository)).await;
    }

    Ok(link_signature)
}

/// [`finalize_commit`] of `signature`, a linked repository's revision a link's commit commits on
/// `branch` over `previous`, recorded in `link_commits`.
async fn commit_link_finalize(
    link_commits: &LinkCommits,
    (repository, branch): (&Arc<RepositoryContext>, BranchId),
    (state_current, state_staged): (&Arc<State>, &Arc<State>),
    (previous, signature): (Hash, Hash),
    token: &RepositoryWriteToken,
) -> Result<(), CommitError> {
    let divergent = branch::load_latest_divergent(repository.clone(), branch)
        .await
        .unwrap_or_default();
    let finalized = finalize_commit(
        repository.clone(),
        state_current,
        state_staged,
        previous,
        signature,
        branch,
        token,
        None,
    )
    .await;
    // The branch latest is stored first, so a finalize failing after it has moved the branch.
    if !execution_context().globals().dry_run()
        && (finalized.is_ok()
            || branch::load_latest(repository.clone(), branch).await.ok() == Some(signature))
    {
        link_commits.record(MovedLinkBranch {
            repository: repository.clone(),
            branch,
            previous,
            signature,
            divergent,
        });
    }
    finalized
}

/// Pins every mount of `linked` in `state` at `signature`, which a commit of `linked` alone made on
/// the stored `branch`, and stages each for the commit of `state` that follows. The mounts pinning
/// `committed_from`, the uncommitted revision the commit was made from, already hold what it
/// committed; each other mount is realized where a walk from `walk_roots` materializes it
/// ([`repin_committed_links`]).
async fn repin_mounts_of_committed(
    operation: &Arc<InstanceOperationImpl>,
    (repository, state): (&Arc<RepositoryContext>, &Arc<State>),
    walk_roots: &[(RelativePath, NodeID)],
    (linked, branch): (RepositoryId, BranchId),
    (committed_from, signature): (Option<Hash>, Hash),
) -> Result<(), CommitError> {
    if let Some(committed_from) = committed_from {
        let rows = state
            .link_list(repository.clone())
            .await
            .forward::<CommitError>("Failed reading the link registry")?;
        for row in rows.iter().filter(|row| row.repository() == linked) {
            let pinned = state
                .node(repository.clone(), row.local_node())
                .await
                .forward::<CommitError>("Failed reading a link node")?
                .address
                .hash;
            if pinned == committed_from {
                link::repin_link_node(
                    state,
                    repository.clone(),
                    linked,
                    branch,
                    signature,
                    row.local_node(),
                    NodeFlags::Staged,
                )
                .await
                .forward::<CommitError>("Failed to update link pin")?;
            }
        }
    }
    let committed = Arc::new(parking_lot::Mutex::new(vec![CommittedLink {
        repository: linked,
        branch,
        signature,
        made: true,
    }]));
    repin_committed_links(operation, (repository, state), walk_roots, &committed, None).await
}

/// Settles the link merge rows `state_staged` keeps for the link `local_node`, linking
/// `link_repository`, whose merge a link's commit committed: an abort is to leave the pin the
/// commit made. Where the main repository is merged, whose abort but for the links restages each
/// link with a row, one row rolls the link back to that pin. Otherwise the rows go, and a merge
/// of links alone ends with its last row, which it says.
async fn settle_committed_link_rows(
    repository: &Arc<RepositoryContext>,
    state_staged: &State,
    (local_node, link_repository): (NodeID, RepositoryId),
) -> Result<bool, CommitError> {
    let rows = state_staged
        .deserialize_link_merge_state(repository.clone())
        .await
        .forward::<CommitError>("Failed to deserialize link merge state")?;
    let mut remaining: Vec<_> = rows
        .iter()
        .filter(|row| row.local_node != local_node)
        .copied()
        .collect();
    if remaining.len() == rows.len() {
        return Ok(false);
    }
    if !state_staged.parent_other().is_zero() {
        let committed = state_staged
            .link_find(repository.clone(), link_repository, local_node)
            .await
            .forward::<CommitError>("Failed to read the committed link's row")?;
        remaining.push(LinkMergeEntry::rollback(committed));
    }
    state_staged
        .serialize_link_merge_state(repository.clone(), &remaining)
        .await
        .forward::<CommitError>("Failed to serialize link merge state")?;
    Ok(branch::merge::end_merge_of_links_alone(state_staged))
}

/// Refuses the commit of the link `chain` leads to where a repository on the way to it, which
/// the commit commits whole, holds a conflict or a row awaiting the user's choice.
async fn reject_unresolved_link_holders(
    chain: &link::ResolvedLinkChain,
) -> Result<(), CommitError> {
    for (i, level) in chain.levels.iter().enumerate() {
        let (holder_state, holder) = chain.child_at(i);
        let node = level
            .state
            .node(level.repository.clone(), level.link_node_id)
            .await
            .forward::<CommitError>("Failed to read a link holding the link")?
            .linked_node()
            .node;
        let mount = link::Mount::of_link(&holder, &holder_state, node, level.mount.clone())
            .await
            .forward::<CommitError>("Failed resolving the path a link holding the link exposes")?;
        reject_unresolved_link_tree(&holder, &holder_state, &mount, node).await?;
    }
    Ok(())
}

/// Converts interface metadata args and prepares the commit metadata object.
async fn build_commit_metadata(
    repository: Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    branch: BranchId,
    message: String,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
) -> Result<Arc<Metadata>, CommitError> {
    let metadata_keys = keys
        .as_slice()
        .iter()
        .map(|k| String::from(k.as_str()))
        .collect();
    let metadata_values = values
        .as_slice()
        .iter()
        .map(|v| String::from(v.as_str()))
        .collect();
    let metadata_formats = formats.as_slice().to_vec();

    let metadata_hash = state_staged.metadata_hash();
    let original_metadata = if metadata_hash.is_zero() {
        Metadata::new()
    } else {
        Metadata::deserialize(repository.clone(), metadata_hash)
            .await
            .forward::<CommitError>("Failed to deserialize metadata")?
    };

    prepare_commit_metadata(
        repository,
        original_metadata,
        branch,
        message,
        Some(metadata_keys),
        Some(metadata_values),
        Some(metadata_formats),
    )
    .await
}

/// Stores branch latest, records the sync point of a merge of divergent history, keeps the dirty
/// nodes left over in a staged anchor or deletes it, and emits the revision commit event.
///
/// The sync point is the merge's first parent, the remote latest it merged. A committed state whose
/// second parent is the current revision is such a merge; its merge flag is cleared by then.
///
/// `branch_previous` is what the pointer write compares against, so it must be
/// the tip observed for `branch`, not the revision the caller built on: a prior
/// link commit can advance the branch without committing the parent, which
/// leaves that pin stale.
///
/// `reconciling` is the operation the commit read its files through, with the times it collected,
/// `None` for a link's own commit, which makes no revision of the instance current. Outside a dry
/// run, where its provider holds its state, it owes the provider's settle from before the revision
/// may become current ([`store_branch_latest_and_make_current`]), and, once the revision is current
/// and the times are stored in the instance's records, is handed `state_staged` as stored
/// ([`hand_over_committed`]), before it is reused as the staged anchor.
///
/// The instance's own commit publishes its branch latest under a commit intent, cleared once the
/// anchors are on disk ([`clear_commit_intent`]). A link's commit writes its branch latest and
/// current anchor with no intent: the commit that follows reads a link's revision from its branch
/// latest or its pin, not from its anchor.
#[allow(clippy::too_many_arguments)]
async fn finalize_commit(
    repository: Arc<RepositoryContext>,
    state_current: &Arc<State>,
    state_staged: &Arc<State>,
    branch_previous: Hash,
    signature: Hash,
    branch: BranchId,
    token: &RepositoryWriteToken,
    reconciling: Option<(&InstanceOperationImpl, &RecordedModifiedTimes)>,
) -> Result<(), CommitError> {
    let dry_run = execution_context().globals().dry_run();

    if !dry_run {
        if let Some((operation, _)) = reconciling {
            store_branch_latest_and_make_current(
                repository.clone(),
                branch_previous,
                signature,
                branch,
                Some(operation),
            )
            .await?;
        } else {
            branch::store_latest(
                repository.clone(),
                branch,
                branch_previous,
                signature,
                BranchLatestStatus::Divergent,
            )
            .await
            .forward::<CommitError>("Failed to store current branch latest")?;
            crate::instance::store_current_anchor(&repository, signature)
                .await
                .forward::<CommitError>("Failed to serialize current revision anchor")?;
        }
        if let Some((operation, modified_times)) = reconciling
            && operation.holds_state()
        {
            modified_times.store(repository.clone()).await;
            if let Err(error) = hand_over_committed(&repository, operation, state_staged).await {
                lore_warn!(
                    "The filesystem provider did not take revision {signature} as committed: {error}"
                );
            }
        }

        if state_staged.parent_other() == state_current.revision()
            && !state_current.revision().is_zero()
        {
            branch::store_last_sync(repository.clone(), branch, state_staged.parent_self()).await;
        }

        // Check if any dirty-only nodes remain in the staged state.
        // If so, preserve them in a new staged anchor re-parented to the new revision.
        let has_dirty = state_staged
            .node_has_dirty_children(repository.clone(), crate::node::ROOT_NODE)
            .await
            .forward::<CommitError>("Failed deserializing state node block")?;

        if has_dirty {
            lore_debug!("Dirty nodes remain after commit, preserving in new staged anchor");
            state_staged.reparent_onto(signature);
            state_staged.mark_dirty();

            let staged_signature =
                state_staged
                    .serialize(repository.clone(), token)
                    .await
                    .forward::<CommitError>("Failed to serialize staged revision state")?;
            crate::instance::store_staged_anchor(&repository, staged_signature)
                .await
                .forward::<CommitError>("Failed to serialize staged anchor")?;
        } else {
            let _ = crate::instance::delete_staged_anchor(&repository).await;
        }

        if reconciling.is_some() {
            clear_commit_intent(&repository).await?;
        }
    }

    event::LoreEvent::RevisionCommitRevision(LoreRevisionCommitRevisionEventData {
        repository: repository.id,
        branch,
        revision: signature,
        revision_number: state_staged.revision_number(),
        parent: state_staged.parent_self(),
        parent_other: state_staged.parent_other(),
    })
    .send();

    Ok(())
}

/// Hands `committed` to `operation`, whose provider holds its state, once the stores are flushed
/// and the instance's records written back after them ([`RepositoryContext::flush`]): the provider
/// then serves the files the commit read from the store, and reports the times the records hold
/// for them. The provider may then drop the copies it kept of those files, which only a revision on
/// disk replaces after a crash. A failed flush, or records left unwritten, hand nothing over and
/// drop the blocks kept for the settle ([`InstanceOperationImpl::keep_serialized_blocks`]).
async fn hand_over_committed(
    repository: &Arc<RepositoryContext>,
    operation: &InstanceOperationImpl,
    committed: &Arc<State>,
) -> Result<(), CommitError> {
    if let Err(error) = repository
        .flush(execution_context().globals().flush_options())
        .await
    {
        operation.drop_serialized_blocks();
        return Err(error).forward_any::<CommitError>("flushing the commit");
    }
    if repository
        .instance_records()
        .is_some_and(|records| records.is_dirty())
    {
        operation.drop_serialized_blocks();
        return Err(CommitError::internal(
            "writing the commit's modified-time records back",
        ));
    }
    operation
        .committed(repository.clone(), committed.clone())
        .await
        .forward::<CommitError>("handing the provider the committed revision")
}

/// Where a layer's commit starts: the node its configured source path names in the state being
/// committed, and the path the layer is materialized at in the working tree.
///
/// A layer draws a subtree of the repository it names, and that source path is configuration
/// naming a node of that repository's own tree — resolving it is the one place the drawn-from
/// tree's spelling of a path is read. A layer that draws a whole repository into the root of
/// the working tree starts at the root of both.
async fn layer_commit_root(
    repository: &Arc<RepositoryContext>,
    state_staged: &Arc<State>,
    source_path: &str,
    mount_path: &str,
) -> Result<(NodeID, RelativePath), CommitError> {
    let relative_path = RelativePath::new_from_initial_path(mount_path)
        .forward::<CommitError>("Invalid layer target path")?;
    if source_path.is_empty() {
        return Ok((ROOT_NODE, relative_path));
    }

    let found = state_staged
        .locate_node(repository.clone(), source_path)
        .await
        .unwrap_or_default();
    if !found.is_valid() {
        return Err(CommitError::internal("Invalid subpath"));
    }
    // TODO(mjansson): Layers that specify a path living inside a link need
    //                 special snowflake care here to commit from the link and down
    let node = found
        .own_node()
        .ok_or_else(|| CommitError::internal("Not supported"))?;
    Ok((node, relative_path))
}

/// Commits `state_staged` as the next revision after `state_current`. `built_on` is the revision
/// the state names as its self parent before any caller moved it onto `state_current`: the one it
/// was staged on, but for a merge reconciling divergent history, which is staged on its second
/// parent. `missing` says what the commit makes of a staged path the working tree no longer holds
/// ([`MissingPaths`]), and `left_out` names the directories whose dirty-only moves a prune before
/// this one left out ([`Gone::left_out`]).
#[allow(clippy::too_many_arguments)]
async fn commit_staged_revision(
    operation: &Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    built_on: Hash,
    roots: Vec<(RelativePath, NodeID)>,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    parent_branch: BranchId,
    reporting: CommitReporting,
    missing: MissingPaths,
    left_out: Vec<NodeID>,
    left: Option<&mut LeftToMark>,
    link_commits: &Arc<LinkCommits>,
) -> Result<(Hash, Arc<RecordedModifiedTimes>), CommitError> {
    let context = execution_context();
    let globals = context.globals();

    let revision_current = state_current.revision();
    let reconciles = state_staged.reconciles_divergent_history(revision_current);
    if state_staged.parent_self() != revision_current {
        if reconciles {
            lore_debug!("Merge of divergent branch history, other parent is current revision");
        } else if globals.force() {
            state_staged.set_parent_self(revision_current);
            state_staged.set_parent_other(Hash::default());
            state_staged.set_metadata_hash(Hash::default());
        } else if state_staged.is_cherry_pick() {
            lore_debug!("Cherry-pick, other parent is empty");
        } else {
            return Err(CommitError::internal("Staged state has stale parent"));
        }
    }

    reject_unresolved_link_pins(&repository, &state_staged, None, &link::Mount::root()).await?;

    // A merge reconciling divergent history is staged on its second parent, and a state moved onto
    // another revision keeps the node IDs of the one it was staged on.
    let based_on = if reconciles || state_staged.parent_self() != built_on {
        BasedOn::Other
    } else {
        BasedOn::FirstParent
    };

    state_staged.hold_base(state_current.clone());
    let pruned =
        prune_dirty_for_commit(state_staged.clone(), repository.clone(), revision_current).await?;
    let gone = (missing == MissingPaths::Settle).then(|| {
        Arc::new(Gone {
            added: left.is_some().then(parking_lot::Mutex::default),
            replaced: left.is_some().then(Arc::default),
            ..Gone::left_out(left_out, &pruned)
        })
    });
    let carried = gone.clone().filter(|_| left.is_some());

    // Per-operation tracker for background fragment uploads. Write-producing
    // calls below dispatch leader tasks into it; await_all drains every
    // outstanding task before returning — on success AND on any error path.
    // This is the graceful-drain pattern: if an intermediate step fails, we
    // still wait for spawned leaders to terminate before propagating the
    // error so no task outlives this function holding references to scope-
    // bound resources.
    let tracker = immutable::commit_write_tracker(reporting.per_fragment);

    let work_tracker = tracker.clone();
    let operation = operation.clone();
    let walk_link_commits = link_commits.clone();
    let work_result: Result<(Hash, Arc<RecordedModifiedTimes>), CommitError> = async move {
        let collected = Arc::new(RecordedModifiedTimes::below(
            operation.clock_floor(&repository).await,
        ));
        // For each file in repository, create fragments if they don't already
        // exist. Also rehash directories affected by change and generate new
        // root hash.
        // A clone: the block's own reference serves the serialize below, so no second slot holds
        // one across the walk's await.
        let StagedChanges {
            delta: staged,
            first_parent,
        } = commit_files_and_rehash(
            operation.clone(),
            repository.clone(),
            token.share(),
            state_staged.clone(),
            based_on,
            roots,
            metadata.clone(),
            link_messages,
            parent_branch,
            work_tracker.clone(),
            collected.clone(),
            reporting.stats.clone(),
            reporting.interval,
            gone,
            walk_link_commits,
        )
        .await?;
        let changes = staged.to_aligned::<NodeDelta>();

        let metadata_hash = metadata
            .serialize_with_tracker(repository.clone(), Some(work_tracker.clone()))
            .await
            .forward::<CommitError>("Failed to write commit metadata")?;

        state_staged.set_metadata_hash(metadata_hash);

        let root_staged = state_staged
            .tree(repository.clone())
            .await
            .forward::<CommitError>("Failed to read revision tree data")?
            .hash_root;
        let root_current = state_current
            .tree(repository.clone())
            .await
            .forward::<CommitError>("Failed to read revision tree data")?
            .hash_root;
        if !state_staged.is_merge_or_cherry_pick_or_revert()
            && root_staged == root_current
            && !link::link_registries_differ(repository.clone(), &state_current, &state_staged)
                .await
                .forward::<CommitError>("Failed to compare the link registries")?
        {
            if !globals.force() {
                lore_debug!(
                    "Staged tree {} in revision {} is identical to current tree {} in revision {}",
                    root_staged,
                    state_staged.revision(),
                    root_current,
                    state_current.revision(),
                );
                return Err(NothingStaged.into());
            }
            lore_debug!("Force commit of identical merkle tree");
        }

        weave_changes(
            repository.clone(),
            state_staged.clone(),
            based_on,
            changes.as_type_slice::<NodeDelta>(),
            first_parent,
        )
        .await?;

        // Before the save, with the discards done, as nothing else frees a slot left detached or
        // in flight (C15).
        state_staged
            .node_sweep_detached(&repository)
            .await
            .forward::<CommitError>("Failed to free the detached slots")?;

        // Serialize the new current state, its fresh blocks kept for the provider's settle
        let signature = state_staged
            .serialize_committed_keeping(repository.clone(), &token, |written| {
                operation.keep_serialized_blocks(written);
            })
            .await
            .forward::<CommitError>("Failed to serialize revision state")?;

        Ok((signature, collected))
    }
    .await;
    if let (Some(left), Some(carried)) = (left, carried) {
        if let Some(found) = &carried.added {
            left.added.append(&mut found.lock());
        }
        if let Some(found) = &carried.replaced {
            left.replaced.append(&mut found.lock());
        }
    }

    // ALWAYS drain the tracker, regardless of whether the work above
    // succeeded. Dropping the tracker while leaders are still running would
    // abort them (JoinSet drop); instead we let them finish (or fail) so the
    // caller never observes a half-committed state.
    let drain_result = tracker.await_all().await;

    // Original work errors take precedence; tracker drain errors surface only
    // when the work itself succeeded.
    let committed = match work_result {
        Ok(committed) => drain_result
            .forward::<CommitError>("Background fragment upload task failed during commit")
            .map(|()| committed),
        Err(work_err) => Err(work_err),
    };
    if committed.is_err() {
        link_commits.roll_back().await;
    }
    committed
}

/// Publish `signature` as `branch`'s tip and anchor it as the current revision.
///
/// `previous` is the tip the caller observed when it decided to commit; the
/// pointer write compares against it, so a branch that advanced in the meantime
/// fails with `BranchAdvanced` instead of orphaning the other committer's
/// revision.
///
/// The commit intent naming `signature` is stored and flushed before the branch latest is stored,
/// and the branch latest is flushed before the current anchor is written. A process killed on the
/// way leaves the anchors behind the branch latest, never ahead of it, and the intent on disk
/// wherever the branch latest names `signature`, which is what the next commit needs to finish the
/// work ([`complete_interrupted_commit`]). The caller clears the intent once the anchors it writes
/// are on disk ([`clear_commit_intent`]).
///
/// `reconciling`, where given, is the operation the commit read its files through. It owes the
/// provider's settle from before the revision may become current
/// ([`InstanceOperationImpl::owe_settle`]), released where the branch latest was not stored.
pub(crate) async fn store_branch_latest_and_make_current(
    repository: Arc<RepositoryContext>,
    previous: Hash,
    signature: Hash,
    branch: BranchId,
    reconciling: Option<&InstanceOperationImpl>,
) -> Result<(), CommitError> {
    if let Some(operation) = reconciling {
        operation.owe_settle().await;
    }
    if let Err(error) = publish_branch_latest(&repository, previous, signature, branch).await {
        if let Some(operation) = reconciling {
            operation.release_settle_owed();
        }
        return Err(error);
    }
    repository
        .flush(execution_context().globals().flush_options())
        .await
        .forward_any::<CommitError>("flushing the branch latest")?;

    crate::instance::store_committed_current_anchor(&repository, signature)
        .await
        .forward::<CommitError>("Failed to serialize current revision anchor")
}

/// Stores the commit intent naming `signature` and flushes it, then stores `signature` as
/// `branch`'s latest over `previous` (no flush).
async fn publish_branch_latest(
    repository: &Arc<RepositoryContext>,
    previous: Hash,
    signature: Hash,
    branch: BranchId,
) -> Result<(), CommitError> {
    crate::instance::store_commit_intent(repository, signature)
        .await
        .forward::<CommitError>("storing the commit intent")?;
    repository
        .flush(execution_context().globals().flush_options())
        .await
        .forward_any::<CommitError>("flushing the commit intent")?;

    branch::store_latest(
        repository.clone(),
        branch,
        previous,
        signature,
        BranchLatestStatus::Divergent,
    )
    .await
    .forward::<CommitError>("Failed to store current branch latest")
}

/// Clears the commit intent ([`crate::instance::store_commit_intent`]) once the anchors a commit
/// wrote are on disk: flushes the stores, then stores zero, which the command's closing flush
/// writes. A cleared intent on disk ahead of the anchors would leave them behind a branch latest
/// that no commit completes.
pub(crate) async fn clear_commit_intent(
    repository: &Arc<RepositoryContext>,
) -> Result<(), CommitError> {
    repository
        .flush(execution_context().globals().flush_options())
        .await
        .forward_any::<CommitError>("flushing the anchors of the commit")?;
    crate::instance::store_commit_intent(repository, Hash::default())
        .await
        .forward::<CommitError>("clearing the commit intent")
}

/// Finishes a commit of this instance that was cut short after it published its revision as the
/// branch latest, before the current and staged anchors it writes reached disk, and clears a
/// commit intent that names no such commit.
///
/// Fails with `BranchAdvanced`, keeping the intent and the anchors as they are, where something
/// was staged after the commit was cut short ([`staged_by_the_commit`]): the branch latest holds
/// the commit, and what was staged since is the user's to unstage, or to stage again after a
/// sync takes the revision. A warning says so. A forced command clears the intent and runs over
/// the anchors as they are, as it did before commits recorded an intent.
///
/// A dry run changes nothing. Over a commit cut short it warns that the commit will be finished,
/// or that it can't be, and fails with `BranchAdvanced`, since the anchors stay behind the branch
/// latest; a forced dry run, and one over an intent that names no commit cut short, go on.
///
/// Reads one key where the commit intent is zero, which it is after every commit that ran to its
/// end. The rest is a cold path, boxed: it runs only after a commit was cut short
/// ([`complete_commit_cut_short`]).
pub(crate) async fn complete_interrupted_commit(
    repository: &Arc<RepositoryContext>,
) -> Result<(), CommitError> {
    let intent = crate::instance::load_commit_intent(repository)
        .await
        .forward::<CommitError>("loading the commit intent")?;
    if intent.is_zero() {
        return Ok(());
    }
    Box::pin(complete_commit_cut_short(repository, intent)).await
}

/// What a non-zero commit intent names.
enum CutShort {
    /// No commit of this instance cut short: the intent is left over and is cleared.
    None,
    /// A commit cut short, with something staged after it: it is left as it is.
    StagedSince {
        /// Whether the current anchor reached disk on the interrupted commit's revision
        current_on_intent: bool,
    },
    /// A commit cut short and nothing staged after it: it is finished.
    Completes {
        /// The State of the interrupted commit's revision
        committed: Arc<State>,
        /// The staged anchor as recovery read it, rebased onto `committed` once it is current
        staged: Option<Hash>,
        /// The branch and the revision a merge of divergent history the commit made leaves as
        /// where the last sync points, which the commit stores once its anchors are written
        last_sync: Option<(BranchId, Hash)>,
    },
}

/// [`complete_interrupted_commit`] for `intent`, a non-zero commit intent.
///
/// A commit cut short leaves its intent equal to the branch latest with the anchors behind it
/// ([`anchors_behind_commit`]): the current anchor still on the revision the commit was made
/// over, which reads as a branch advanced by another instance, or the staged anchor still on a
/// state staged over that revision, which reads as a staged state with a stale parent. A provider
/// that holds its state is handed the intent's revision to hold, keeping the local changes it
/// serves, as a sync hands it the revision it moves to. The revision is made current and the
/// staged anchor, as [`cut_short`] read it, rebased onto it, its dirty paths carried over, which
/// leaves both as a commit that runs to its end leaves them. The intent is cleared once they are
/// on disk ([`clear_commit_intent`]).
async fn complete_commit_cut_short(
    repository: &Arc<RepositoryContext>,
    intent: Hash,
) -> Result<(), CommitError> {
    let dry_run = execution_context().globals().dry_run();
    let force = execution_context().globals().force();
    let (committed, staged, last_sync) = match cut_short(repository, intent).await? {
        CutShort::Completes {
            committed,
            staged,
            last_sync,
        } => (committed, staged, last_sync),
        CutShort::StagedSince { current_on_intent } if !force => {
            if current_on_intent {
                lore_warn!(
                    "Revision {intent}, which a commit of this instance published before it was \
                     interrupted, is current, and changes were staged after it over the revision \
                     before it. Unstage them, then stage them again."
                );
            } else {
                lore_warn!(
                    "The branch latest holds revision {intent}, which a commit of this instance \
                     published before it was interrupted, and changes were staged after it. \
                     Unstage them, or run `lore sync` to take the revision, then stage them again."
                );
            }
            return Err(BranchAdvanced.into());
        }
        cut => {
            if matches!(cut, CutShort::StagedSince { .. }) {
                // A forced command runs over the anchors as they are, as it did before the intent
                // was recorded.
                lore_warn!(
                    "Revision {intent}, which a commit of this instance published before it was \
                     interrupted, is left as it is; the forced command replaces it as the branch \
                     latest."
                );
            }
            if dry_run {
                return Ok(());
            }
            return crate::instance::store_commit_intent(repository, Hash::default())
                .await
                .forward::<CommitError>("clearing the commit intent");
        }
    };
    if dry_run {
        // The anchors stay behind the branch latest. A dry run fails as the command run over them
        // fails, rather than reporting an amend of the revision before the intent's.
        lore_warn!(
            "The commit of revision {intent} was interrupted; the command will finish it first. A \
             dry run leaves it as it is."
        );
        return Err(BranchAdvanced.into());
    }

    lore_info!("Completing the interrupted commit of revision {intent}");
    with_operation(repository.file_system(), async |operation| {
        if operation.holds_state() {
            operation
                .hold_state(
                    repository.clone(),
                    committed.clone(),
                    HoldOptions {
                        keep_local: true,
                        ..HoldOptions::default()
                    },
                )
                .await
                .forward_any::<CommitError>("holding the revision of the interrupted commit")?;
        }
        crate::instance::store_committed_current_anchor(repository, intent)
            .await
            .forward::<CommitError>("Failed to serialize current revision anchor")?;
        let Some(staged) = staged else {
            return Ok(());
        };
        let rebased = state::rebase_staged_state_in(
            &operation,
            repository.clone(),
            staged,
            intent,
            false,
            true,
            None,
        )
        .await
        .forward::<CommitError>("rebasing the staged anchor")?;
        crate::instance::store_staged_anchor(repository, rebased.unwrap_or_default())
            .await
            .forward::<CommitError>("Failed to serialize staged anchor")
    })
    .await?;
    if let Some((branch, revision)) = last_sync {
        branch::store_last_sync(repository.clone(), branch, revision).await;
    }

    clear_commit_intent(repository).await
}

/// What `intent`, a non-zero commit intent, names ([`CutShort`]): a commit of this instance cut
/// short before its anchors reached disk where `intent` is the branch latest and the anchors are
/// behind it ([`anchors_behind_commit`]), and whether anything was staged after it
/// ([`staged_by_the_commit`]).
async fn cut_short(
    repository: &Arc<RepositoryContext>,
    intent: Hash,
) -> Result<CutShort, CommitError> {
    let (current_revision, current_branch) = crate::instance::load_own_current_anchor(repository)
        .await
        .forward::<CommitError>("loading the current anchor")?;
    let branch_latest = branch::load_latest(repository.clone(), current_branch)
        .await
        .forward::<CommitError>("loading the branch latest")?;
    if branch_latest != intent {
        return Ok(CutShort::None);
    }
    let staged = match crate::instance::load_own_staged_revision(repository)
        .await
        .forward::<CommitError>("loading the staged anchor")?
    {
        Some(staged_revision) => Some(
            State::deserialize(repository.clone(), staged_revision)
                .await
                .forward_with::<CommitError, _>(|| {
                    format!("Failed to deserialize revision state {staged_revision}")
                })?,
        ),
        None => None,
    };
    let committed = State::deserialize(repository.clone(), intent)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {intent}")
        })?;
    if !anchors_behind_commit(repository, current_revision, &committed, staged.as_ref()).await? {
        return Ok(CutShort::None);
    }
    if !staged_by_the_commit(repository, &committed, current_revision, staged.as_ref()).await? {
        return Ok(CutShort::StagedSince {
            current_on_intent: current_revision == intent,
        });
    }
    // A merge of divergent history leaves the last sync on the revision it merged in, as
    // `finalize_commit` does once its anchors are written: one whose second parent is the current
    // anchor, or, where the current anchor reached disk on the intent, one made over its second
    // parent ([`made_over`]).
    let other = committed.parent_other();
    let merged_divergent = !other.is_zero()
        && (other == current_revision
            || (current_revision == intent && made_over(repository, other, &committed).await?));
    let last_sync = merged_divergent.then(|| (current_branch, committed.parent_self()));
    Ok(CutShort::Completes {
        committed,
        staged: staged.map(|staged| staged.revision()),
        last_sync,
    })
}

/// Whether `staged`, the instance's staged state, is the one the commit of `committed` left,
/// with nothing staged after it: there is none; it is the state of dirty nodes the commit stages
/// over `committed`, with no other parent and nothing staged on it; or it was staged over the
/// parents `committed` has and stages nothing `committed` doesn't hold.
///
/// An amend cut short leaves `current_revision`, the current anchor, on the revision it replaces,
/// and the staged state of dirty nodes over that revision. That state, with no other parent and
/// nothing staged on it, is the amend's own as well.
///
/// A staged node `committed` holds at a path the commit changed over its first parent is one the
/// commit committed, whatever its content: staging pins a path, and the commit read the content.
/// So is a link there that `committed` holds with the same pin, and a staged delete of a path
/// `committed` no longer holds. Anything else staged, a node at a path the commit did not change,
/// an add, a delete of a node `committed` holds, a move, a change in a linked repository or of a
/// link's pin, or other parents, which a merge, cherry-pick or revert stages, was staged after
/// the commit.
#[lore_macro::test_pub]
async fn staged_by_the_commit(
    repository: &Arc<RepositoryContext>,
    committed: &Arc<State>,
    current_revision: Hash,
    staged: Option<&Arc<State>>,
) -> Result<bool, CommitError> {
    let Some(staged) = staged else {
        return Ok(true);
    };
    let over_committed = staged.parent_self() == committed.revision()
        || (staged.parent_self() == current_revision
            && current_revision != committed.parent_self());
    if over_committed {
        if !staged.parent_other().is_zero() {
            return Ok(false);
        }
    } else if (staged.parent_self(), staged.parent_other())
        != (committed.parent_self(), committed.parent_other())
    {
        return Ok(false);
    }
    let changes = state::diff_collect(
        repository.clone(),
        committed.clone(),
        repository.clone(),
        staged.clone(),
        None,
        FilterMode::TRACKED,
    )
    .await
    .forward_any::<CommitError>("comparing the staged state with the interrupted commit")?;
    if over_committed {
        return Ok(!changes.iter().any(|change| change.flags.is_stage()));
    }
    // The paths the commit changed over its first parent: a path staged anywhere else was
    // staged after it, whatever `committed` holds there.
    let parent = State::deserialize(repository.clone(), committed.parent_self())
        .await
        .forward_with::<CommitError, _>(|| {
            format!(
                "Failed to deserialize revision state {}",
                committed.parent_self()
            )
        })?;
    let committed_changes = state::diff_collect(
        repository.clone(),
        parent,
        repository.clone(),
        committed.clone(),
        None,
        FilterMode::TRACKED,
    )
    .await
    .forward_any::<CommitError>("comparing the interrupted commit with its parent")?;
    let changed: HashSet<&str> = committed_changes
        .iter()
        .map(|change| change.path().as_str())
        .collect();
    Ok(changes
        .iter()
        .filter(|change| change.flags.is_stage())
        .all(|change| {
            if change.resolved_side().mapping.repository.id != repository.id
                || !changed.contains(change.path().as_str())
            {
                return false;
            }
            match change.action {
                FileAction::Keep => {
                    !change.to.flags.contains(NodeFlags::Link)
                        || change.from.address == change.to.address
                }
                FileAction::Delete => !change.from.mapping.node.is_valid_node_id(),
                _ => false,
            }
        }))
}

/// Whether the anchors of this instance are behind `committed`, a revision it committed: the
/// current anchor is on a revision `committed` was made over ([`made_over`]), or the current
/// anchor is on `committed` and `staged`, the staged state, is still staged over its first
/// parent.
///
/// The current anchor is held to the revisions a commit is made over, so that an intent left
/// behind by a commit, which a writer that does not know the intent never clears, does not take a
/// current anchor that writer moved elsewhere for a commit cut short.
async fn anchors_behind_commit(
    repository: &Arc<RepositoryContext>,
    current_revision: Hash,
    committed: &Arc<State>,
    staged: Option<&Arc<State>>,
) -> Result<bool, CommitError> {
    if current_revision != committed.revision() {
        return made_over(repository, current_revision, committed).await;
    }
    Ok(staged.is_some_and(|staged| staged.parent_self() == committed.parent_self()))
}

/// Whether `revision` is one a commit of `committed` was made over: its first parent; its second
/// parent where the commit merged the branch latest into it, which leaves both parents on one
/// branch, as a sync of divergent history does; or a revision with the same parents, which an
/// amend replaces. A merge of another branch is made over its first parent, and so is a merge
/// whose second parent records no branch.
async fn made_over(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    committed: &Arc<State>,
) -> Result<bool, CommitError> {
    let parents = (committed.parent_self(), committed.parent_other());
    if revision == parents.0 {
        return Ok(true);
    }
    if revision.is_zero() {
        return Ok(false);
    }
    let state = State::deserialize(repository.clone(), revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {revision}")
        })?;
    if revision == parents.1 {
        if state.metadata_hash().is_zero() || committed.metadata_hash().is_zero() {
            return Ok(false);
        }
        let branch = state
            .recorded_branch(repository.clone())
            .await
            .forward_any::<CommitError>("reading the branch of the second parent")?;
        let committed_branch = committed
            .recorded_branch(repository.clone())
            .await
            .forward_any::<CommitError>("reading the branch of the interrupted commit")?;
        return Ok(branch == committed_branch);
    }
    Ok((state.parent_self(), state.parent_other()) == parents)
}

/// Fragments every file the state holds and rehashes the directories the changes reach.
///
/// `based_on` says whether the tree was built on its first parent, sharing its node IDs, or on
/// another revision ([`DeletionBase`]). Where it was, the delta's deletions are recorded against
/// the revision the state is staged on or was loaded from, read before the walk, as a caller
/// rebuilding a committed revision's tree sets its parents after; where it was not, the walk
/// records no deletion, and the revision records its diff from the first parent
/// ([`rehash_and_record_delta`]). Answers the changes staged on the tree ([`StagedChanges`]).
///
/// The walk starts at `node_id`, which `relative_path` is the working-tree path of: the root of
/// the repository for a commit that covers the whole of one, and the path a link or layer is
/// materialized at for a commit that covers what it draws in.
///
/// `operation` is the caller's, covering the whole working tree the files are read from, links
/// and layers included: their paths are the parent's and so is their filesystem. A file the
/// state holds as a resolved merge conflict has the copies that merge left beside it removed,
/// so the operation is one finalized as having changed the filesystem. Where its provider holds
/// its state, it is handed each directory the walk adds or moves that the tree still holds once
/// the commit settles what the working tree no longer holds ([`hand_directories`]).
///
/// `progress_interval` is how often the walk reports its progress. `gone` collects the staged
/// paths the working tree no longer holds for [`commit_gone`] to settle, a commit of the instance's
/// staged changes; without it, as for the rehash of a tree a caller assembled and a nested link's
/// intermediate level, such a path is refused as not found ([`MissingPaths`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn commit_files_and_rehash(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    based_on: BasedOn,
    roots: Vec<(RelativePath, NodeID)>,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    parent_branch: BranchId,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    modified_times: Arc<RecordedModifiedTimes>,
    stats: Arc<CommitStats>,
    progress_interval: std::time::Duration,
    gone: Option<Arc<Gone>>,
    link_commits: Arc<LinkCommits>,
) -> Result<StagedChanges, CommitError> {
    lore_info!("Fragmenting files and updating tree hashes");

    event::LoreEvent::RevisionCommitBegin(LoreRevisionCommitBeginEventData::default()).send();

    // Print progress at regular intervals
    let ticker = ProgressTicker::start(
        stats.clone(),
        progress_interval,
        CommitStats::send_progress_event,
    );

    let (walk, file_rx) = CommitWalk::new(
        operation.holds_state(),
        operation.clone(),
        repository.clone(),
        token,
        state.clone(),
        metadata,
        link_messages,
        stats.clone(),
        parent_branch,
        tracker.clone(),
        gone,
    );
    let committed_links = walk.committed_links.clone();
    let delta = commit_walks(
        CommitWalk {
            link_commits,
            ..walk
        },
        file_rx,
        &roots,
        based_on,
        modified_times,
        true,
    )
    .await?;
    if !committed_links.lock().is_empty() {
        Box::pin(repin_committed_links(
            &operation,
            (&repository, &state),
            &roots,
            &committed_links,
            Some(&delta),
        ))
        .await?;
    }

    drop(ticker);

    event::LoreEvent::RevisionCommitEnd(LoreRevisionCommitEndEventData {
        count: LoreRevisionCommitCountData::new(stats),
    })
    .send();

    rehash_and_record_delta(repository, state, based_on, delta, tracker).await
}

/// Commits the staged nodes of `walk`'s state from each of `roots`, a node and its path in the
/// working tree, settles what the working tree no longer holds where the walk collects it
/// ([`commit_gone`]), hands its provider the directories the walk adds or moves that stay
/// ([`hand_directories`]), then frees the nodes staged for deletion, and answers the delta.
///
/// Several roots are the mounts of one linked repository, walked side by side. Two of them
/// exposing one folder are refused ([`reject_shared_roots`]). Where the roots are below
/// the root, as for a link or a layer mounting a subtree, the staged nodes outside them are frozen
/// first ([`freeze_directory`]), so that a refusal of a node the freeze visits comes before the walk
/// of the working tree writes anything. A subtree staged for deletion, or below a directory staged
/// for deletion, is refused: the walk would commit into nodes the deletion then frees. `based_on`
/// is as for [`commit_files_and_rehash`]. Where `reports_discovery`, the walk's end is reported as
/// the end of the discovery of `walk.stats`.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments)]
async fn commit_walks(
    walk: CommitWalk,
    file_rx: mpsc::Receiver<FileToCommit>,
    roots: &[(RelativePath, NodeID)],
    based_on: BasedOn,
    modified_times: Arc<RecordedModifiedTimes>,
    reports_discovery: bool,
) -> Result<Arc<parking_lot::RwLock<BytesMut>>, CommitError> {
    reject_shared_roots(roots)?;
    let Some((first_path, first_node)) = roots.first().cloned() else {
        return Err(CommitError::internal("committing a tree from no node"));
    };
    let operation = walk.operation.clone();
    let repository = walk.repository.clone();
    let state = walk.state.clone();
    let stats = walk.stats.clone();
    let delta = walk.delta.clone();
    let discard = walk.discard.clone();
    let subnodes_to_discard = walk.subnodes_to_discard.clone();
    let handed = walk.handed.clone();
    let tracker = walk.tracker.clone();
    let base = DeletionBase::of(state.committed_revision(), based_on);

    if first_node != ROOT_NODE {
        for (relative_path, node_id) in roots {
            if staged_for_deletion(&repository, &state, *node_id).await? {
                return Err(NotSupported {
                    operation: format!(
                        "committing {relative_path}, whose subtree the commit deletes"
                    ),
                }
                .into());
            }
        }
        freeze_directory(
            repository.clone(),
            state.clone(),
            ROOT_NODE,
            roots,
            delta.clone(),
            discard.clone(),
            subnodes_to_discard.clone(),
            stats.clone(),
        )
        .await?;
    }

    let first = CommitWalk {
        root: first_node,
        roots: roots.into(),
        ..walk
    };
    let mut walks = Vec::with_capacity(roots.len());
    for (relative_path, node_id) in &roots[1..] {
        walks.push((first.rooted(*node_id), relative_path.clone(), *node_id));
    }
    walks.insert(0, (first, first_path.clone(), first_node));
    let gones: Vec<_> = walks
        .iter()
        .map(|(walk, relative_path, node_id)| (walk.gone.clone(), relative_path.clone(), *node_id))
        .collect();

    let consumer = {
        let operation = operation.clone();
        let repository = repository.clone();
        let state = state.clone();
        let delta = delta.clone();
        let gone = gones[0].0.clone();
        let stats = stats.clone();
        lore_spawn!(crate::subtask!(async move {
            commit_execute(
                operation,
                file_rx,
                repository,
                state,
                delta,
                gone,
                stats,
                tracker,
                modified_times,
            )
            .await
        }))
    };

    let producer = {
        let stats = stats.clone();
        lore_spawn!(crate::subtask!(async move {
            let walked = commit_roots(walks).await;
            if reports_discovery {
                stats.discovery.complete.store(true, Ordering::Relaxed);
                // Send a progress event immediately when discovery finishes,
                // ensuring at least one progress event has discoveryComplete=true
                stats.send_progress();
            }
            walked
        }))
    };

    join_commit_tasks(producer, consumer).await?;

    if gones.len() > 1 {
        share_gone_files(&gones);
    }
    let mut leaving = Vec::new();
    for (gone, relative_path, node_id) in &gones {
        if let Some(gone) = gone {
            leaving.extend(
                commit_gone(
                    &operation,
                    &repository,
                    &state,
                    (relative_path, *node_id),
                    &delta,
                    gone,
                    &discard,
                    &subnodes_to_discard,
                    &stats,
                )
                .await?,
            );
        }
    }
    if gones.len() > 1 {
        leaving.sort_unstable();
    }
    hand_directories(&operation, &handed, &leaving);
    commit_discard(
        repository,
        state,
        base,
        roots,
        delta.clone(),
        &discard,
        &subnodes_to_discard,
        stats,
    )
    .await?;

    Ok(delta)
}

/// Refuses `roots` where two of them mount one node: the working tree holds that node's files at
/// both paths, and the walk can read each of them from only one.
#[lore_macro::test_pub]
fn reject_shared_roots(roots: &[(RelativePath, NodeID)]) -> Result<(), CommitError> {
    for (i, (path, node)) in roots.iter().enumerate() {
        if let Some((other, _)) = roots[..i].iter().find(|(_, earlier)| earlier == node) {
            return Err(InvalidArguments {
                reason: format!(
                    "the mounts at {} and {} expose one folder of a linked repository; remove one \
                     of them",
                    link::display_path(other.as_str()),
                    link::display_path(path.as_str()),
                ),
            }
            .into());
        }
    }
    Ok(())
}

/// Hands each walk of `gones` the files the first one's fragmenting found gone below its root: a
/// walk settles only what lies below its own root ([`commit_gone`]).
fn share_gone_files(gones: &[(Option<Arc<Gone>>, RelativePath, NodeID)]) {
    let Some((Some(first), ..)) = gones.first() else {
        return;
    };
    let mut files = std::mem::take(&mut *first.files.write());
    for (gone, root, _) in &gones[1..] {
        let Some(gone) = gone else {
            continue;
        };
        let (below, rest) = files.into_iter().partition(|(_, path)| root.covers(path));
        gone.files.write().extend::<Vec<_>>(below);
        files = rest;
    }
    first.files.write().extend(files);
}

/// Whether `node_id`, or a directory above it, is staged for deletion.
pub(crate) async fn staged_for_deletion(
    repository: &Arc<RepositoryContext>,
    state: &State,
    mut node_id: NodeID,
) -> Result<bool, CommitError> {
    while node_id != ROOT_NODE {
        let block = state
            .block(repository.clone(), NodeBlock::index(node_id))
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
        let node = *block.read().node(Node::index(node_id));
        if node.is_staged_delete() {
            return Ok(true);
        }
        node_id = node.parent;
    }
    Ok(false)
}

/// Rehashes the directories the staged changes of `state` reach and records its node delta,
/// answering `delta`, the changes staged on its tree, for the history weave ([`weave_changes`]).
///
/// A tree [based on](BasedOn) its first parent records `delta`. A tree based on another revision
/// records its change from the first parent ([`self_parent_delta`]), as every reader of a delta
/// takes it: the staged changes are its change from the revision it was built on. The first
/// parent's tree, read for that diff, is answered for the weave to read it once.
async fn rehash_and_record_delta(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    based_on: BasedOn,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
) -> Result<StagedChanges, CommitError> {
    rehash_directory(repository.clone(), state.clone(), ROOT_NODE).await?;

    state
        .update_tree_root_hash(repository.clone())
        .await
        .forward::<CommitError>("Failed to update tree root hash")?;
    lore_debug!("Rehashed merkle tree");

    let staged = delta.write().split().freeze();
    let (recorded, first_parent) = match based_on {
        BasedOn::FirstParent => (staged.clone(), None),
        BasedOn::Other => {
            let parent = State::deserialize(repository.clone(), state.parent_self())
                .await
                .forward_any::<CommitError>("Failed reading the first parent's tree")?;
            let recorded = Box::pin(self_parent_delta(&repository, &state, parent.clone())).await?;
            (recorded, Some(parent))
        }
    };
    generate_delta_block(repository, state, recorded, Some(tracker)).await?;
    Ok(StagedChanges {
        delta: staged,
        first_parent,
    })
}

/// The changes staged on the tree of a revision being committed, for the history weave
/// ([`weave_changes`]), and the first parent's tree where it was read to record the delta.
pub(crate) struct StagedChanges {
    pub(crate) delta: Bytes,
    pub(crate) first_parent: Option<Arc<State>>,
}

/// The node delta of `state`, a tree built on another revision than its first parent
/// ([`BasedOn::Other`]), as its change from `parent`, the first parent's tree: the diff of it
/// against this one ([`state::diff_for_delta`]), with adds and deletes of one file paired into
/// moves. A node the tree holds is named by its ID here and a deleted node by its ID in the first
/// parent, where a reader looks each up. A link whose pin changed is recorded on its node, and each
/// directory the tree holds above a change is recorded as kept ([`directories_above`]), as the
/// delta of a revision committed on its first parent records each directory its staged changes
/// reach. Every change of a revision with a second parent is marked as merged.
///
/// Boxed by its caller: only a revision whose tree was started from another revision reaches it.
async fn self_parent_delta(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    parent: Arc<State>,
) -> Result<Bytes, CommitError> {
    let walk_repository = repository.clone();
    let to = state.clone();
    let mut changes = state::ChangeStream::spawn(async move |changes| {
        state::diff_for_delta(walk_repository, parent, to, &changes).await
    })
    .collect()
    .await
    .forward_any::<CommitError>("Failed diffing the tree against its first parent")?;
    state::detect_and_coalesce_moves(&mut changes);
    let directories = directories_above(repository, state, &changes).await?;

    let merged = if state.parent_other().is_zero() {
        change::Flags::None
    } else {
        change::Flags::Merge
    };
    let mut delta =
        BytesMut::with_capacity((changes.len() + directories.len()) * size_of::<NodeDelta>());
    for change in &changes {
        let node = if change.action == FileAction::Delete {
            change.from.mapping.node
        } else {
            change.to.mapping.node
        };
        let entry = NodeDelta {
            node,
            _unused: 0,
            action: change.action as u16,
            flags: ((change.flags & change::Flags::Modify) | merged).bits(),
        };
        delta.extend_from_slice(entry.as_bytes());
    }
    for &node in &directories {
        let entry = NodeDelta {
            node,
            _unused: 0,
            action: FileAction::Keep as u16,
            flags: merged.bits(),
        };
        delta.extend_from_slice(entry.as_bytes());
    }
    Ok(delta.freeze())
}

/// The directories of `state` above the paths `changes` reach, a move's source included, below the
/// root and each once, that `changes` doesn't name itself. Each is found below the one above it, as
/// the sorted paths list a directory before those below it.
async fn directories_above(
    repository: &Arc<RepositoryContext>,
    state: &State,
    changes: &[change::NodeChange],
) -> Result<Vec<NodeID>, CommitError> {
    let mut paths = BTreeSet::new();
    for change in changes {
        for path in std::iter::once(change.path()).chain(change.move_source()) {
            let mut path = path.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                if !paths.insert(parent) {
                    break;
                }
                path = parent;
            }
        }
    }
    let named: HashSet<NodeID> = changes
        .iter()
        .filter(|change| change.action != FileAction::Delete)
        .map(|change| change.to.mapping.node)
        .collect();
    let mut found = HashMap::with_capacity(paths.len());
    let mut directories = Vec::with_capacity(paths.len());
    for path in paths {
        let (root, name) = match path.rsplit_once('/') {
            Some((parent, name)) => match found.get(parent) {
                Some(&parent) => (parent, name),
                None => continue,
            },
            None => (ROOT_NODE, path),
        };
        let Some(node) = state::node_at_path(repository, state, root, name)
            .await
            .forward::<CommitError>("Failed reading a directory above a change in the tree")?
        else {
            continue;
        };
        found.insert(path, node);
        if !named.contains(&node) {
            directories.push(node);
        }
    }
    Ok(directories)
}

/// A linked repository a commit walk committed through one of its link nodes.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct CommittedLink {
    repository: RepositoryId,
    /// The branch the link's row stores.
    branch: BranchId,
    /// The revision the link node pins once committed.
    signature: Hash,
    /// Whether the commit made `signature`, rather than keeping the revision the link was built on.
    made: bool,
}

#[lore_macro::test_pub]
type CommittedLinks = Arc<parking_lot::Mutex<Vec<CommittedLink>>>;

/// The link of each repository in `committed` whose revision every mount of it takes.
#[lore_macro::test_pub]
fn repins(mut committed: Vec<CommittedLink>) -> Vec<CommittedLink> {
    // A revision the commit made outranks one a link node kept, whatever order the tasks ran in.
    committed.sort_by_key(|link| !link.made);
    let mut repins: Vec<CommittedLink> = Vec::new();
    for link in &committed {
        if repins
            .iter()
            .any(|repin| repin.repository == link.repository)
        {
            continue;
        }
        // Mounts that kept different revisions, as a revision written before every mount shared
        // one can hold, give the commit no revision to move them to.
        if !link.made
            && committed.iter().any(|other| {
                other.repository == link.repository && other.signature != link.signature
            })
        {
            continue;
        }
        repins.push(*link);
    }
    repins
}

/// Pins every mount of each repository in `committed` at the revision committed, so a commit through
/// one mount moves the others with it, and realizes each mount moved through `operation` where a
/// walk from one of `walk_roots`, a node and its working-tree path, materializes it, once every
/// such mount is verified. Within a commit's walk, `delta` records each mount moved and the
/// directories above it are staged for the rehash; outside one, each moved link node is staged
/// for the commit that follows.
///
/// Runs once the walk is done: its tasks have cleared the directories they committed, so a mount
/// moved from inside one would leave a directory above it the rehash doesn't visit.
async fn repin_committed_links(
    operation: &Arc<InstanceOperationImpl>,
    (repository, state): (&Arc<RepositoryContext>, &Arc<State>),
    walk_roots: &[(RelativePath, NodeID)],
    committed: &CommittedLinks,
    delta: Option<&Arc<parking_lot::RwLock<BytesMut>>>,
) -> Result<(), CommitError> {
    let committed = std::mem::take(&mut *committed.lock());
    let mut pins = Vec::new();
    for link in repins(committed) {
        let moved = link::repin_repository(
            state,
            repository.clone(),
            link.repository,
            link.signature,
            link.branch,
        )
        .await
        .forward::<CommitError>("Failed to pin every mount of a committed repository")?;
        let link_context = repository.to_link_context(link.repository).await;
        for mount in &moved {
            let staged = if delta.is_some() {
                mount.parent
            } else {
                mount.link_node
            };
            state
                .node_mark(repository, staged, Mark::staged(NodeFlags::Staged, true))
                .await
                .forward::<CommitError>("Failed to stage the directory holding a mount")?;
            if let Some(delta) = delta {
                delta_add(
                    delta.clone(),
                    mount.link_node,
                    (NodeFlags::Link | NodeFlags::StagedModify).bits(),
                );
            }
            for (walk_path, walk_node) in walk_roots {
                if let Some(below) = state
                    .node_path_below(repository.clone(), mount.link_node, *walk_node)
                    .await
                    .forward::<CommitError>("Failed resolving the path of a mount")?
                {
                    pins.push((
                        link_context.clone(),
                        mount.pin(walk_path.join(below.as_str())),
                    ));
                    break;
                }
            }
        }
    }
    link::realize_resolved_link_pins_in_operation(operation, repository.clone(), &pins)
        .await
        .forward_any::<CommitError>("Failed to realize a mount the commit moved")
}

/// Rehashes the whole of `state` under one filesystem operation, answering with the modified
/// times the fragmenting recorded and the node delta it records, for the history weave
/// ([`weave_changes`]).
///
/// For a caller rebuilding a tree it assembled itself -- a merge, a restore -- rather than one
/// committing what a working tree holds staged. The times are the caller's to store against the
/// revision it publishes or to discard where it publishes none.
///
/// The tracker is drained whether or not the rehash succeeded, so no spawned leader outlives
/// this call holding references to the state it was given. The rehash failure is the one
/// reported, a drain failure surfacing only where the rehash itself succeeded.
pub(crate) async fn rehash_tree_in_operation(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    metadata: Arc<Metadata>,
    branch: BranchId,
) -> Result<(Arc<RecordedModifiedTimes>, Bytes), CommitError> {
    let tracker = Arc::new(lore_storage::write_tracker::WriteTracker::new());
    let modified_times = Arc::new(RecordedModifiedTimes::below(
        crate::state::read_clock_floor(&repository).await,
    ));
    let rehashed = with_operation(repository.file_system(), async |operation| {
        commit_files_and_rehash(
            operation,
            repository.clone(),
            token,
            state,
            BasedOn::FirstParent,
            vec![(RelativePath::new(), ROOT_NODE)],
            metadata,
            Arc::new(HashMap::new()),
            branch,
            tracker.clone(),
            modified_times.clone(),
            CommitStats::new(),
            execution_context().globals().event_interval(),
            None,
            Arc::default(),
        )
        .await
    })
    .await;
    let drained = tracker.await_all().await;
    let delta = rehashed?.delta;
    drained.forward::<CommitError>("Background fragment upload task failed during rehash")?;
    Ok((modified_times, delta))
}

#[lore_macro::test_pub]
struct FileToCommit {
    node_id: NodeID,
    relative_path: RelativePath,
}

/// A commit drops the merge's link rows, so a row still awaiting the user's choice has to be
/// refused before the record of it is gone. Link rows carry no node flags, so the walk that
/// rejects an unresolved file cannot see them. With `only`, just the row of that link node counts.
/// `mount` is where the working tree materializes `state_staged`'s repository.
///
/// Only an open merge records link rows, so their read is boxed rather than held by every commit.
async fn reject_unresolved_link_pins(
    repository: &Arc<RepositoryContext>,
    state_staged: &State,
    only: Option<NodeID>,
    mount: &link::Mount,
) -> Result<(), CommitError> {
    if state_staged.link_merge_hash().is_zero() {
        return Ok(());
    }
    Box::pin(reject_recorded_link_pins(
        repository,
        state_staged,
        only,
        mount,
    ))
    .await
}

async fn reject_recorded_link_pins(
    repository: &Arc<RepositoryContext>,
    state_staged: &State,
    only: Option<NodeID>,
    mount: &link::Mount,
) -> Result<(), CommitError> {
    let entries = state_staged
        .deserialize_link_merge_state(repository.clone())
        .await
        .forward::<CommitError>("Failed to deserialize link merge state")?;

    let Some(entry) = entries.iter().find(|entry| {
        entry.is_unresolved() && only.is_none_or(|local_node| entry.local_node == local_node)
    }) else {
        return Ok(());
    };

    let path = state_staged
        .link_merge_entry_path(repository.clone(), entry)
        .await
        .forward::<CommitError>("Failed resolving link node path")?;
    let Ok(path) = RelativePath::from_str(&path);
    Err(Conflict {
        path: mount.spell(&path).unwrap_or(path).to_string(),
    }
    .into())
}

/// A commit walk clears the flags of every node it freezes, so a conflict has to be refused before
/// the record of it is gone.
fn reject_unresolved_conflict(node: &Node, path: &str) -> Result<(), CommitError> {
    if node.is_staged_merge_unresolved() {
        return Err(Conflict {
            path: path.to_string(),
        }
        .into());
    }
    Ok(())
}

/// Fails where the commit of `state` couldn't move every mount of a repository it commits through
/// a staged link node, in `state` or in a linked state the commit commits: with
/// [`LinkPinsDiverge`] where the mounts disagree on branch or flags, and with
/// [`InvalidArguments`] where the revision staged at the link node lacks the folder another mount
/// exposes. Checked before anything is committed, as the commit publishes a linked revision before
/// it moves the other mounts.
///
/// `only` limits the check in `state` to the repository a commit of one link goes through; each
/// linked state it commits is committed whole.
#[lore_macro::test_pub]
fn reject_unmovable_link_mounts<'a>(
    repository: &'a Arc<RepositoryContext>,
    state: &'a State,
    only: Option<RepositoryId>,
) -> crate::BoxFuture<'a, Result<(), CommitError>> {
    Box::pin(async move {
        let link_list = state
            .link_list(repository.clone())
            .await
            .forward::<CommitError>("Failed reading the link registry")?;
        let mut mounts: HashMap<RepositoryId, usize> = HashMap::new();
        for row in &link_list {
            *mounts.entry(row.repository()).or_default() += 1;
        }
        // Each repository committed through a staged link node, with the revisions staged at them.
        let mut committed: Vec<(RepositoryId, Vec<Hash>)> = Vec::new();
        for row in link_list
            .iter()
            .filter(|row| only.is_none_or(|only| row.repository() == only))
        {
            let node = state
                .node(repository.clone(), row.local_node())
                .await
                .forward::<CommitError>("Failed reading a link node")?;
            if !node.is_staged() {
                continue;
            }
            match committed
                .iter_mut()
                .find(|(linked, _)| *linked == row.repository())
            {
                Some((_, staged)) if !staged.contains(&node.address.hash) => {
                    staged.push(node.address.hash);
                }
                Some(_) => {}
                None => committed.push((row.repository(), vec![node.address.hash])),
            }
        }
        // A repository mounted once has no other mount to move.
        let repositories: Vec<_> = committed
            .iter()
            .map(|(linked, _)| *linked)
            .filter(|linked| mounts[linked] > 1)
            .collect();
        state
            .check_link_rows_agree(
                repository,
                &link_list,
                &repositories,
                LinkPinAgreement::Rows,
            )
            .await
            .forward::<CommitError>("Checking the rows of the repositories the commit links")?;
        for (linked, staged) in &committed {
            // Mounts staged at different revisions are refused once the walk compares them.
            if let [staged] = staged[..]
                && mounts[linked] > 1
                && let Some(missing) =
                    link::mount_missing_at(state, repository, &link_list, *linked, staged)
                        .await
                        .forward::<CommitError>("Checking the mounts the commit moves")?
            {
                let mount = state
                    .node_path(repository.clone(), missing)
                    .await
                    .forward::<CommitError>("Failed resolving the path of a mount")?;
                return Err(InvalidArguments {
                    reason: format!(
                        "the mount at {mount} exposes a folder the revision committed for linked \
                         repository {linked} no longer holds; remove that mount, or restore the \
                         folder"
                    ),
                }
                .into());
            }
            for &staged in staged {
                let link_repository = repository.to_link_context(*linked).await;
                let link_state = State::deserialize(link_repository.clone(), staged)
                    .await
                    .forward::<CommitError>("Failed to deserialize link state")?;
                reject_unmovable_link_mounts(&link_repository, &link_state, None).await?;
            }
        }
        Ok(())
    })
}

/// Refuses the commit of `state`, which the working tree materializes at `mount`, where a link it
/// merges, or a link nested in one that the merge merged too, holds a conflict not resolved
/// anywhere in the linked tree, or a row of a nested link still awaiting the user's choice. The
/// walks refuse each conflict they meet as well, but only once the links committed beside it, and
/// the files met before it, are written.
async fn reject_unresolved_link_merge_conflicts(
    repository: &Arc<RepositoryContext>,
    state: &State,
    mount: &link::Mount,
) -> Result<(), CommitError> {
    let merged = state
        .deserialize_link_merge_state(repository.clone())
        .await
        .forward::<CommitError>("Failed reading link merge state")?;
    for entry in merged.iter().filter(|entry| !entry.is_conflict()) {
        let link_node = {
            let block = state
                .block(repository.clone(), NodeBlock::index(entry.local_node))
                .await
                .forward::<CommitError>("Failed deserializing state block")?;
            *block.read().node(Node::index(entry.local_node))
        };
        if !link_node.is_link() {
            continue;
        }
        let link = link_node.linked_node();
        let (link_repository, link_state) = link
            .open(repository)
            .await
            .forward::<CommitError>("Failed to deserialize link state")?;
        let path = path_for_message(repository, state, entry.local_node).await;
        let Ok(path) = RelativePath::from_str(&path);
        let link_mount = link::Mount::of_link(
            &link_repository,
            &link_state,
            link.node,
            mount.spell(&path).unwrap_or(path),
        )
        .await
        .forward::<CommitError>("Failed resolving the path the link exposes")?;
        reject_unresolved_link_tree(&link_repository, &link_state, &link_mount, link.node).await?;
    }
    Ok(())
}

/// Refuses the commit of `link_state`, a link's merge mounting its `node` at `mount`, where it
/// holds a conflict, a row awaiting the user's choice, or such a link nested in it.
async fn reject_unresolved_link_tree(
    link_repository: &Arc<RepositoryContext>,
    link_state: &Arc<State>,
    mount: &link::Mount,
    node: NodeID,
) -> Result<(), CommitError> {
    if let Some(conflict) = unresolved_link_conflict(link_repository, link_state).await? {
        return Err(
            link_conflict(link_repository, link_state, (&mount.path, node), conflict).await,
        );
    }
    reject_unresolved_link_pins(link_repository, link_state, None, mount).await?;
    Box::pin(reject_unresolved_link_merge_conflicts(
        link_repository,
        link_state,
        mount,
    ))
    .await
}

/// The refusal of `conflict` in `state`, a linked repository's, named as a walk of the link at
/// `mount` names it: the working-tree path of the mount and the node it mounts.
async fn link_conflict(
    repository: &Arc<RepositoryContext>,
    state: &State,
    mount: (&RelativePath, NodeID),
    conflict: NodeID,
) -> CommitError {
    Conflict {
        path: walk_path_for_message(repository, state, mount, conflict).await,
    }
    .into()
}

/// A conflict not resolved anywhere in the tree of `state`, a linked repository's, where it is a
/// merge.
async fn unresolved_link_conflict(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
) -> Result<Option<NodeID>, CommitError> {
    if !state.is_merge() {
        return Ok(None);
    }
    first_staged_node(repository, state, Node::is_staged_merge_unresolved).await
}

/// What every directory of a commit walk reads and writes.
#[lore_macro::test_pub]
struct CommitWalk {
    /// Whether each directory the walk adds or moves goes to `operation`
    /// ([`InstanceOperationImpl::hand_directories`]): a link's walk keeps its own.
    hands_directories: bool,
    /// Each directory the walk adds or moves, with its path, where it hands directories: handed
    /// once the commit settles what the working tree no longer holds ([`hand_directories`]).
    handed: Arc<parking_lot::RwLock<Vec<(NodeID, RelativePath)>>>,
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    /// The nodes staged for deletion, with their flags.
    discard: Arc<parking_lot::RwLock<Vec<(NodeID, u16)>>>,
    /// The nodes below the directories staged for deletion, with their flags.
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<(NodeID, u16)>>>,
    /// What the walk finds for [`commit_gone`], for a commit that takes what the working tree holds
    /// at the staged paths it no longer holds ([`MissingPaths::Settle`]); `None` for the rehash of a
    /// tree a caller assembled and a nested link's intermediate level, which refuse such a path.
    gone: Option<Arc<Gone>>,
    file_tx: mpsc::Sender<FileToCommit>,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    stats: Arc<CommitStats>,
    parent_branch: BranchId,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    dir_semaphore: Arc<Semaphore>,
    /// The node the walk starts at, set by [`commit_walks`].
    root: NodeID,
    /// Every node the commit's walks start at, each with its working-tree path.
    roots: WalkRoots,
    /// Each linked repository the walk committed through a link node, with the stored branch and
    /// the revision committed, for [`repin_committed_links`].
    committed_links: CommittedLinks,
    link_commits: Arc<LinkCommits>,
}

/// The nodes a commit's walks start at, each with its working-tree path.
#[lore_macro::test_pub]
type WalkRoots = Arc<[(RelativePath, NodeID)]>;

/// What one commit does to the linked repositories below it, shared by every walk of it.
#[lore_macro::test_pub]
#[derive(Default)]
pub(crate) struct LinkCommits {
    /// The revision each staged revision of a linked repository commits as on a branch, so that the
    /// links pinning one staged revision commit it once.
    revisions: parking_lot::Mutex<
        HashMap<(RepositoryId, BranchId, Hash), Arc<tokio::sync::OnceCell<Hash>>>,
    >,
    /// Each linked branch the commit moved, in the order it moved them.
    moved: parking_lot::Mutex<Vec<MovedLinkBranch>>,
}

/// A linked branch a commit moved from `previous` to `signature`, with whether its latest was
/// divergent before.
#[lore_macro::test_pub]
pub(crate) struct MovedLinkBranch {
    pub repository: Arc<RepositoryContext>,
    pub branch: BranchId,
    pub previous: Hash,
    pub signature: Hash,
    pub divergent: bool,
}

impl LinkCommits {
    /// The revision `staged`, a staged revision of `repository`, commits as on `branch`, which
    /// every link node pinning it shares, so that it commits once.
    pub(crate) fn revision(
        &self,
        repository: RepositoryId,
        branch: BranchId,
        staged: Hash,
    ) -> Arc<tokio::sync::OnceCell<Hash>> {
        self.revisions
            .lock()
            .entry((repository, branch, staged))
            .or_default()
            .clone()
    }

    #[lore_macro::test_pub]
    pub(crate) fn record(&self, moved: MovedLinkBranch) {
        self.moved.lock().push(moved);
    }

    /// [`Self::roll_back`], unless the failed commit published `published`, its revision, as the
    /// latest of `branch` of `repository`.
    #[lore_macro::test_pub]
    async fn roll_back_unpublished(
        &self,
        (repository, branch, published): (&Arc<RepositoryContext>, BranchId, Hash),
    ) {
        if branch::load_latest(repository.clone(), branch)
            .await
            .is_ok_and(|latest| latest != published)
        {
            self.roll_back().await;
        }
    }

    /// Moves each linked branch the commit moved back to the latest it replaced, newest first,
    /// with the latest status it had, for a commit that fails before it publishes the revision
    /// pinning them: a push would otherwise publish revisions no parent pins. A branch moved since
    /// is left where it is.
    #[lore_macro::test_pub]
    pub(crate) async fn roll_back(&self) {
        let moved = std::mem::take(&mut *self.moved.lock());
        for moved in moved.into_iter().rev() {
            let status = if moved.divergent {
                BranchLatestStatus::Divergent
            } else {
                BranchLatestStatus::Convergent
            };
            match branch::store_latest(
                moved.repository.clone(),
                moved.branch,
                moved.signature,
                moved.previous,
                status,
            )
            .await
            {
                Ok(()) => {}
                Err(error) if error.is_branch_advanced() => lore_debug!(
                    "Left branch {} of repository {}, which another commit moved past revision {}",
                    moved.branch,
                    moved.repository.id,
                    moved.signature
                ),
                Err(error) => lore_warn!(
                    "Left branch {} of repository {} at revision {}, which a failed commit \
                     stored: {error}",
                    moved.branch,
                    moved.repository.id,
                    moved.signature
                ),
            }
        }
    }
}

/// A link, at any depth below a staged state, pinning a revision of its repository that is not
/// committed yet.
#[lore_macro::test_pub]
struct UncommittedPin {
    /// The repository holding the link.
    holder: RepositoryId,
    repository: RepositoryId,
    /// The branch the link resolves to.
    branch: BranchId,
    pin: Hash,
    mount_path: RelativePath,
}

/// Refuses, before it writes anything, a commit of the staged state of `repository`, or of the
/// link at `scope` alone, that would lose work in a repository links held by several repositories
/// reach on one branch.
///
/// The links pinning one staged revision commit it once, through one of them, which reads from the
/// working tree only the mounts the repository holding it has: a file staged from the working tree
/// under a link another repository holds would be committed at its old content. A revision only a
/// merge staged holds the content of each file it changed, so it commits once.
async fn refuse_link_commits_losing_work(
    repository: &Arc<RepositoryContext>,
    scope: Option<&str>,
    link_messages: &HashMap<String, String>,
) -> Result<(), CommitError> {
    let (_, state_staged, branch) = State::deserialize_current_and_staged(repository.clone())
        .await
        .forward::<CommitError>("Failed to deserialize current and staged state")?;
    let Some(state) = state_staged else {
        return Ok(());
    };
    let pins = uncommitted_link_pins(repository, &state, branch).await?;
    if let Some(scope) = scope {
        let Ok(scope) = RelativePath::from_str(scope);
        let pins: Vec<UncommittedPin> = pins.into_iter().map(|(pin, ..)| pin).collect();
        return refuse_scoped_link_commit(&pins, &scope);
    }
    let mut edited = HashSet::new();
    for (pin, mounts) in link::group_by(&pins, |(pin, ..)| pin.pin) {
        let [(first, context, linked), rest @ ..] = mounts.as_slice() else {
            continue;
        };
        if rest.iter().all(|(pin, ..)| pin.holder == first.holder) {
            continue;
        }
        if first_staged_node(context, linked, staged_from_the_working_tree)
            .await?
            .is_some()
        {
            edited.insert(pin);
        }
    }
    let pins: Vec<UncommittedPin> = pins.into_iter().map(|(pin, ..)| pin).collect();
    refuse_link_commits(&pins, &edited, link_messages)
}

/// Whether `node` is a file staged from the working tree, whose content a commit reads from it,
/// rather than one only a merge staged.
fn staged_from_the_working_tree(node: &Node) -> bool {
    node.is_file()
        && (node.flags & NodeFlags::MergeBits.bits() == 0
            || node.flags & NodeFlags::Dirty.bits() != 0)
}

/// The links below `state`, a staged state of `repository` on `branch`, at any depth, that pin a
/// revision not committed yet, each with that revision. Only a staged link pins one, so only staged
/// links are read, and a link the working tree holds none of is left out.
async fn uncommitted_link_pins(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    branch: BranchId,
) -> Result<Vec<(UncommittedPin, Arc<RepositoryContext>, Arc<State>)>, CommitError> {
    let mut found = Vec::new();
    let mut pending = vec![(
        repository.clone(),
        state.clone(),
        branch,
        link::Mount::root(),
        0,
    )];
    while let Some((holder, holder_state, holder_branch, mount, depth)) = pending.pop() {
        if depth >= state::MAX_LINK_DEPTH {
            continue;
        }
        let links = holder_state
            .link_list(holder.clone())
            .await
            .forward::<CommitError>("Failed listing links")?;
        let reads = links
            .into_iter()
            .map(|link| {
                let (holder, holder_state, mount) = (&holder, &holder_state, &mount);
                async move {
                    let Some((node, path)) = link::staged_link_node(holder_state, holder, &link)
                        .await
                        .forward::<CommitError>("Failed reading a link")?
                    else {
                        return Ok(None);
                    };
                    let (context, linked) = node
                        .linked_node()
                        .open(holder)
                        .await
                        .forward::<CommitError>("Failed to deserialize link state")?;
                    if linked.revision_number() != 0 {
                        return Ok(None);
                    }
                    let Some(mount_path) = mount.spell_str(&path) else {
                        return Ok(None);
                    };
                    let nested = link::Mount::of_link(&context, &linked, node.child, mount_path)
                        .await
                        .forward::<CommitError>("Failed resolving the path a link exposes")?;
                    Ok::<_, CommitError>(Some((link, context, linked, nested)))
                }
            })
            .collect();
        for (link, context, linked, nested) in
            link::try_join_links(reads).await?.into_iter().flatten()
        {
            let branch = link.resolve_branch(holder_branch);
            found.push((
                UncommittedPin {
                    holder: holder.id,
                    repository: link.repository,
                    branch,
                    pin: link.signature,
                    mount_path: nested.path.clone(),
                },
                context.clone(),
                linked.clone(),
            ));
            pending.push((context, linked, branch, nested, depth + 1));
        }
    }
    Ok(found)
}

/// Refuses a commit of `pins`, the links pinning uncommitted revisions, where two links to one
/// repository on one branch pin different revisions, which commit apart on one branch, or, held by
/// different repositories, pin one revision that holds a file staged from the working tree, as
/// `edited` names them, or are given different per-link messages at or below them: the commit
/// takes the messages of one of them.
#[lore_macro::test_pub]
fn refuse_link_commits(
    pins: &[UncommittedPin],
    edited: &HashSet<Hash>,
    link_messages: &HashMap<String, String>,
) -> Result<(), CommitError> {
    for ((repository, _), mounts) in link::group_by(pins, |pin| (pin.repository, pin.branch)) {
        let [first, rest @ ..] = mounts.as_slice() else {
            continue;
        };
        let first_path = link::display_path(first.mount_path.as_str());
        for pin in rest {
            let path = link::display_path(pin.mount_path.as_str());
            if first.pin != pin.pin {
                return Err(NotSupported {
                    operation: format!(
                        "committing two revisions of repository {repository} on one branch, \
                         staged under the links at {first_path} and {path}"
                    ),
                }
                .into());
            }
            if pin.holder != first.holder && edited.contains(&pin.pin) {
                return Err(NotSupported {
                    operation: format!(
                        "committing changes made in the working tree inside repository \
                         {repository} under the links at {first_path} and {path}, which pin one \
                         staged revision of it"
                    ),
                }
                .into());
            }
            if messages_below(link_messages, &first.mount_path)
                != messages_below(link_messages, &pin.mount_path)
            {
                return Err(InvalidArguments {
                    reason: format!(
                        "the links at {first_path} and {path} commit one revision of a \
                         repository, so they take the same per-link messages, at them and below \
                         them"
                    ),
                }
                .into());
            }
        }
    }
    Ok(())
}

/// Refuses a commit of the link at `scope` alone, and the links below it, where a link outside it
/// to one of their repositories on the same branch pins an uncommitted revision too, other than
/// one pinning the same revision in a repository holding one of them, which commits with it: the
/// commit would leave that link's revision to commit apart on the same branch.
#[lore_macro::test_pub]
fn refuse_scoped_link_commit(
    pins: &[UncommittedPin],
    scope: &RelativePath,
) -> Result<(), CommitError> {
    for ((repository, _), mounts) in link::group_by(pins, |pin| (pin.repository, pin.branch)) {
        let inside = |pin: &&UncommittedPin| scope.covers(&pin.mount_path);
        let commits_with = |outside: &&UncommittedPin| {
            mounts
                .iter()
                .any(|pin| inside(pin) && pin.holder == outside.holder && pin.pin == outside.pin)
        };
        if mounts.iter().any(inside)
            && let Some(outside) = mounts.iter().find(|pin| !inside(pin) && !commits_with(pin))
        {
            return Err(NotSupported {
                operation: format!(
                    "committing the link at {} alone, while the link at {} pins an uncommitted \
                     revision of repository {repository} too. Commit them together",
                    link::display_path(scope.as_str()),
                    link::display_path(outside.mount_path.as_str())
                ),
            }
            .into());
        }
    }
    Ok(())
}

/// The messages of `link_messages` at `mount` or below it, each by its path below `mount`.
fn messages_below<'a>(
    link_messages: &'a HashMap<String, String>,
    mount: &RelativePath,
) -> BTreeMap<&'a str, &'a str> {
    link_messages
        .iter()
        .filter(|(path, _)| mount.covers(path))
        .map(|(path, message)| {
            let below = path[mount.as_str().len()..].trim_start_matches('/');
            (below, message.as_str())
        })
        .collect()
}

impl CommitWalk {
    /// A walk handing each directory it adds or moves to `operation` where `hands_directories`,
    /// collecting for [`commit_gone`] what the working tree no longer holds where `gone` is given,
    /// and the receiving end of the files it sends.
    #[lore_macro::test_pub]
    #[allow(clippy::too_many_arguments)]
    fn new(
        hands_directories: bool,
        operation: Arc<InstanceOperationImpl>,
        repository: Arc<RepositoryContext>,
        token: RepositoryWriteToken,
        state: Arc<State>,
        metadata: Arc<Metadata>,
        link_messages: Arc<HashMap<String, String>>,
        stats: Arc<CommitStats>,
        parent_branch: BranchId,
        tracker: Arc<lore_storage::write_tracker::WriteTracker>,
        gone: Option<Arc<Gone>>,
    ) -> (Self, mpsc::Receiver<FileToCommit>) {
        let (file_tx, file_rx) = mpsc::channel(DEFAULT_WORK_CHANNEL_CAPACITY);
        let walk = Self {
            hands_directories,
            handed: Arc::default(),
            operation,
            repository,
            token,
            state,
            delta: Arc::default(),
            discard: Arc::default(),
            subnodes_to_discard: Arc::default(),
            gone,
            file_tx,
            metadata,
            link_messages,
            stats,
            parent_branch,
            tracker,
            dir_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_DIRECTORY_TASKS)),
            root: ROOT_NODE,
            roots: Arc::new([(RelativePath::new(), ROOT_NODE)]),
            committed_links: CommittedLinks::default(),
            link_commits: Arc::default(),
        };
        (walk, file_rx)
    }

    /// Where the prune of a link below this walk's tree records the paths of the nodes it staged
    /// for deletion ([`Gone::replaced`]).
    fn replaced(&self) -> Option<Arc<parking_lot::Mutex<Vec<RelativePath>>>> {
        self.gone.as_ref().and_then(|gone| gone.replaced.clone())
    }

    /// What the walk makes of a staged path the working tree no longer holds
    /// ([`MissingPaths::of`]).
    fn missing(&self) -> MissingPaths {
        MissingPaths::of(self.gone.as_deref())
    }

    /// A walk of the same commit from `root`, beside this one, which collects for [`commit_gone`]
    /// apart from it.
    fn rooted(&self, root: NodeID) -> Self {
        Self {
            hands_directories: self.hands_directories,
            handed: self.handed.clone(),
            operation: self.operation.clone(),
            repository: self.repository.clone(),
            token: self.token.share(),
            state: self.state.clone(),
            delta: self.delta.clone(),
            discard: self.discard.clone(),
            subnodes_to_discard: self.subnodes_to_discard.clone(),
            gone: self.gone.as_deref().map(|gone| Arc::new(gone.sibling())),
            file_tx: self.file_tx.clone(),
            metadata: self.metadata.clone(),
            link_messages: self.link_messages.clone(),
            stats: self.stats.clone(),
            parent_branch: self.parent_branch,
            tracker: self.tracker.clone(),
            dir_semaphore: self.dir_semaphore.clone(),
            root,
            roots: self.roots.clone(),
            committed_links: self.committed_links.clone(),
            link_commits: self.link_commits.clone(),
        }
    }
}

/// The tasks a commit walk spawns.
type CommitTasks = JoinSet<Result<(), CommitError>>;

/// Commits the root directories of a walk over several mounts. A root after the first goes to a
/// task of its own while the directory budget lasts, as a staged subdirectory does, and is walked
/// here once it is spent, so that the mounts' walks share the one budget. The first failure stops
/// the walks here, and is returned once every task has finished.
async fn commit_roots(walks: Vec<(CommitWalk, RelativePath, NodeID)>) -> Result<(), CommitError> {
    let mut tasks = CommitTasks::new();
    let mut here = Vec::with_capacity(1);
    for (walk, relative_path, node_id) in walks {
        let walk = Arc::new(walk);
        // Inline fallback rather than a blocking acquire, as for subdirectories.
        let permit = if here.is_empty() {
            None
        } else {
            walk.dir_semaphore.clone().try_acquire_owned().ok()
        };
        if let Some(permit) = permit {
            lore_spawn!(
                tasks,
                crate::subtask!(commit_directory(walk, relative_path, node_id, Some(permit)))
            );
        } else {
            here.push((walk, relative_path, node_id));
        }
    }
    let mut failure = None;
    for (walk, relative_path, node_id) in here {
        if let Err(err) = commit_directory(walk, relative_path, node_id, None).await {
            failure = Some(err);
            break;
        }
    }
    while let Some(task) = tasks.join_next().await {
        collect_commit_task(task, &mut failure);
    }
    failure.map_or(Ok(()), Err)
}

/// Commits the staged directory `node_id` at `relative_path`, and every staged directory below it
/// that no task takes.
///
/// The walk is a loop over a stack of the directories it is inside rather than a recursion: a
/// staged subdirectory goes to a task of its own while the directory budget lasts, and is entered at
/// once otherwise, its parent resuming once it is left. So no directory is boxed, the stack grows
/// with the tree's depth rather than with the subdirectories found, and it is allocated only once
/// the budget is spent. Between directories the walk collects its finished tasks, and waits for one
/// while more than [`MAX_CONCURRENT_TASKS`] run, so that the tasks of the directories it enters stay
/// bounded. The first failure stops the walk, and is returned once every task has finished. The
/// file channel closes once every walk sharing `walk` has finished.
///
/// `permit` is the budget a spawned walk holds until it has drained its tasks. Not an `async fn`,
/// which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn commit_directory(
    walk: Arc<CommitWalk>,
    relative_path: RelativePath,
    node_id: NodeID,
    permit: Option<OwnedSemaphorePermit>,
) -> impl Future<Output = Result<(), CommitError>> {
    async move {
        let mut frame = CommitFrame::enter(&walk, relative_path, node_id).await?;
        let mut ancestors = Vec::new();
        let mut left = Left::default();
        let mut tasks = CommitTasks::new();
        let mut failure = None;
        loop {
            match commit_directory_entries(&walk, &mut frame, &mut tasks).await {
                Ok(Some(child)) => ancestors.push(std::mem::replace(&mut frame, child)),
                Ok(None) => {
                    let Some(parent) = ancestors.pop() else {
                        frame.leave(&walk, &mut left);
                        break;
                    };
                    std::mem::replace(&mut frame, parent).leave(&walk, &mut left);
                }
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
            while let Some(task) = tasks.try_join_next() {
                collect_commit_task(task, &mut failure);
            }
            while tasks.len() > MAX_CONCURRENT_TASKS
                && let Some(task) = tasks.join_next().await
            {
                collect_commit_task(task, &mut failure);
            }
            if failure.is_some() {
                break;
            }
        }
        while let Some(task) = tasks.join_next().await {
            collect_commit_task(task, &mut failure);
        }
        if let Some(gone) = &walk.gone {
            gone.take(left);
        }
        drop(permit);
        failure.map_or(Ok(()), Err)
    }
}

/// What a walk task leaves for [`commit_gone`] as it leaves each directory, handed to [`Gone`]
/// once the task ends ([`Gone::take`]). Inline for a task that counts two directories at most and
/// leaves one `Holder` to the climb, as most do, so a task allocates nothing for it.
#[derive(Default)]
struct Left {
    counted: smallvec::SmallVec<[NodeID; 2]>,
    directories: smallvec::SmallVec<[Holder; 1]>,
}

/// A directory a commit walk has entered and not yet left.
struct CommitFrame {
    relative_path: RelativePath,
    node_id: NodeID,
    children: StateNodeChildrenWithNameIterator,
    updated: bool,
    /// Whether a child of the directory is staged for deletion.
    holds_deletion: bool,
    /// Whether the directory sent a file to [`commit_file`].
    sent_file: bool,
    /// Whether the directory is staged only for what lies below it.
    bare: bool,
    /// Whether a staged child of the directory was met.
    holds_staged: bool,
}

impl CommitFrame {
    /// Enters the staged directory `node_id` at `relative_path`, adding the directory to the delta
    /// where it is staged, and, where the walk hands directories and the directory is staged as
    /// added or moved, noting it in `walk.handed` for [`hand_directories`].
    async fn enter(
        walk: &CommitWalk,
        relative_path: RelativePath,
        node_id: NodeID,
    ) -> Result<Self, CommitError> {
        let bare = {
            let block = walk
                .state
                .block_with_nametable(walk.repository.clone(), NodeBlock::index(node_id))
                .await
                .forward::<CommitError>("Failed deserializing state block")?;
            let node = block.node(Node::index(node_id));
            debug_assert!(node.is_directory());
            lore_trace!(
                "Committing directory node {} {} flags 0x{:x}",
                node_id,
                relative_path.as_str(),
                node.flags
            );
            if node.is_staged() {
                delta_add(walk.delta.clone(), node_id, node.flags);
                if walk.hands_directories && (node.is_staged_add() || node.is_staged_move()) {
                    walk.handed.write().push((node_id, relative_path.clone()));
                }
            }
            state::is_staged_for_descendants_only(&node)
        };
        let children = StateNodeChildrenWithNameIterator::new(
            walk.state.clone(),
            walk.repository.clone(),
            node_id,
        )
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
        Ok(Self {
            relative_path,
            node_id,
            children,
            updated: false,
            holds_deletion: false,
            sent_file: false,
            bare,
            holds_staged: false,
        })
    }

    /// Leaves the directory, counting it when a child of it was committed or discarded. A
    /// directory holding a staged deletion, and one holding no staged child, staged as added or
    /// moved or for its rehash, go to the climb of [`commit_gone`], which reads whether the
    /// working tree still holds them, through the task's `left`.
    fn leave(self, walk: &CommitWalk, left: &mut Left) {
        if self.updated {
            walk.stats.count_directory();
        }
        if walk.gone.is_none() {
            return;
        }
        if self.updated {
            left.counted.push(self.node_id);
        }
        if self.holds_deletion || !self.holds_staged {
            left.directories.push(Holder {
                node: self.node_id,
                path: self.relative_path,
                sent_file: self.sent_file,
                bare: self.bare && !self.holds_staged,
            });
        }
    }
}

/// Commits the staged children of `frame`'s directory from where it left off, and returns the first
/// subdirectory past the walk's directory budget, entered, or `None` once every child is committed.
async fn commit_directory_entries(
    walk: &Arc<CommitWalk>,
    frame: &mut CommitFrame,
    tasks: &mut CommitTasks,
) -> Result<Option<CommitFrame>, CommitError> {
    loop {
        let rest = {
            let Some((child_node_id, child_node, node_name)) =
                frame
                    .children
                    .next()
                    .await
                    .forward::<CommitError>("Failed deserializing state block")?
            else {
                return Ok(None);
            };
            commit_child(walk, frame, child_node_id, &child_node, node_name, tasks)?
        };
        match rest {
            None => {}
            Some(ChildRest::File(node_id, relative_path, size)) => {
                collect_file(node_id, relative_path, &walk.file_tx, &walk.stats, size).await?;
                frame.updated = true;
                frame.sent_file = true;
            }
            Some(ChildRest::Directory(node_id, relative_path)) => {
                return CommitFrame::enter(walk, relative_path, node_id)
                    .await
                    .map(Some);
            }
        }
    }
}

/// Whether [`commit_file`] commits the staged child `node`: a file not staged for deletion. For a
/// conflict in such a file, `commit_file` refuses it unresolved before reading the file, refuses
/// it resolved where the working tree's file holds conflict markers, unless it takes the staged
/// node's content without reading the file ([`takes_staged_content`]), and removes the copies a
/// merge wrote beside it outside a dry run.
fn committed_by_commit_file(node: &Node) -> bool {
    node.is_file() && !node.is_staged_delete()
}

/// What committing a child leaves for the walk to await.
enum ChildRest {
    /// A file to send to the fragmenting, and its size.
    File(NodeID, RelativePath, u64),
    /// A subdirectory past the walk's directory budget, to enter.
    Directory(NodeID, RelativePath),
}

/// Commits the child `child_node_id` of `frame`'s directory as far as it can without awaiting:
/// rejects an unresolved conflict in a child [`commit_file`] does not commit, and removes the
/// copies a merge wrote beside a directory or a deleted file whose conflict is resolved; discards
/// a deleted child; and spawns a task per link, per deleted subdirectory, and per subdirectory
/// while the walk's directory budget lasts. Returns what is left to await, or `None` for a child
/// that is not staged or needs no more.
///
/// A plain function, so that the child's node is not held across the walk's awaits, and so that it
/// can spawn [`commit_directory`], whose future holds that of [`commit_directory_entries`]: spawned
/// there, it would need that future to be `Send` before its type is known. A subdirectory takes a
/// permit only when one is free: a parent waiting on a permit a descendant needs would deadlock the
/// bounded fan-out. A link's commit takes the walk's tracker only for its observer: `commit_link`
/// builds a tracker of its own, so that it can drain before emitting the linked repository's
/// `RevisionCommitRevision` event.
fn commit_child(
    walk: &Arc<CommitWalk>,
    frame: &mut CommitFrame,
    child_node_id: NodeID,
    child_node: &Node,
    node_name: NodeNameLock,
    tasks: &mut CommitTasks,
) -> Result<Option<ChildRest>, CommitError> {
    if !child_node.is_staged() {
        return Ok(None);
    }
    frame.holds_staged = true;

    // Takes the name by value so its block read lock ends here, rather than reaching the
    // commit of the child below (see NodeNameLock docs).
    let relative_path = frame.relative_path.push_into_buf(node_name).freeze();

    lore_trace!(
        "Committing directory node {} child {child_node_id}",
        frame.node_id
    );

    if !committed_by_commit_file(child_node) {
        reject_unresolved_conflict(child_node, relative_path.as_str())?;
        if !child_node.is_link()
            && child_node.is_staged_merge_conflict()
            && !execution_context().globals().dry_run()
        {
            let operation = walk.operation.clone();
            let path = relative_path.clone();
            lore_spawn!(tasks, async move {
                sync::unlink_merge_artifacts(&operation, &path).await;
                Ok(())
            });
        }
    }

    if child_node.is_staged_delete() {
        if child_node.is_directory() {
            lore_spawn!(
                tasks,
                crate::subtask!(collect_discard_subnodes(
                    walk.repository.clone(),
                    walk.state.clone(),
                    child_node_id,
                    walk.subnodes_to_discard.clone(),
                    walk.stats.clone(),
                ))
            );
            frame.updated = true;
        }
        walk.discard.write().push((child_node_id, child_node.flags));
        frame.holds_deletion = true;
        if child_node.flags & NodeFlags::File != 0 {
            walk.operation.committed_removal(&relative_path);
            walk.stats
                .complete
                .file_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // A deleted directory, or a link's mount path, which is one on the mount: named only
            // where the mount holds it deleted, as an entry naming one it does not makes it an
            // empty file.
            walk.operation.committed_tree_removal(&relative_path);
            walk.stats
                .complete
                .directory_total
                .fetch_add(1, Ordering::Relaxed);
        }
    } else if child_node.is_directory() {
        let Ok(permit) = walk.dir_semaphore.clone().try_acquire_owned() else {
            return Ok(Some(ChildRest::Directory(child_node_id, relative_path)));
        };
        lore_spawn!(
            tasks,
            crate::subtask!(commit_directory(
                walk.clone(),
                relative_path,
                child_node_id,
                Some(permit)
            ))
        );
    } else if child_node.is_link() {
        lore_debug!("Before committing link node, parent node {}", frame.node_id);
        lore_spawn!(
            tasks,
            crate::subtask!(commit_link_node(
                walk.operation.clone(),
                walk.repository.clone(),
                walk.token.share(),
                walk.state.clone(),
                ((walk.roots.clone(), walk.root), child_node_id),
                relative_path,
                walk.delta.clone(),
                walk.metadata.clone(),
                walk.link_messages.clone(),
                walk.stats.clone(),
                walk.parent_branch,
                walk.tracker.clone(),
                (walk.missing(), walk.replaced()),
                walk.committed_links.clone(),
                walk.link_commits.clone(),
            ))
        );
    } else if child_node.is_file() {
        return Ok(Some(ChildRest::File(
            child_node_id,
            relative_path,
            child_node.size,
        )));
    }
    Ok(None)
}

/// Collects the nodes below `node_id`, a directory staged for deletion, with their flags into
/// `subnodes_to_discard`, for [`commit_discard`] to record and free.
async fn collect_discard_subnodes(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<(NodeID, u16)>>>,
    stats: Arc<CommitStats>,
) -> Result<(), CommitError> {
    lore_trace!("Committing raw deletion of node {node_id} children");
    state::node_discard_walk(
        state,
        repository,
        node_id,
        move |discarded_node_id, flags| {
            lore_trace!("Committing raw deletion of node {discarded_node_id}");
            subnodes_to_discard.write().push((discarded_node_id, flags));
            count_totals(&stats, std::iter::once(flags));
        },
    )
    .await
    .forward::<CommitError>("Failed discarding node staged for delete")
}

/// What a commit makes of a staged path the working tree no longer holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MissingPaths {
    /// Takes what the working tree holds there ([`commit_gone`]): a commit of the instance's staged
    /// changes, of a layer's and of a link's walked from its mount.
    Settle,
    /// Refuses the commit as not found: the rehash of a tree a caller assembled, and a nested
    /// link's intermediate level, walked from its root rather than from its mount.
    Refuse,
}

impl MissingPaths {
    /// [`Self::Settle`] for a walk that collects in `gone` what [`commit_gone`] settles, and
    /// [`Self::Refuse`] for one that collects nothing.
    fn of(gone: Option<&Gone>) -> Self {
        if gone.is_some() {
            Self::Settle
        } else {
            Self::Refuse
        }
    }
}

/// What a commit walk leaves for [`commit_gone`] to settle against the working tree.
#[lore_macro::test_pub]
#[derive(Default)]
pub(crate) struct Gone {
    /// Each staged file [`commit_file`] found the working tree no longer holds, with its path.
    files: parking_lot::RwLock<Vec<(NodeID, RelativePath)>>,
    /// Each directory the walk leaves to the climb: one holding a staged deletion, and one staged
    /// that holds no staged child, as one staged as added or moved, or one a staged move left,
    /// staged for its rehash.
    directories: parking_lot::RwLock<Vec<Holder>>,
    /// Each directory the walk counted as it left it, in the total and as done, which a gone
    /// directory may take with it to count again ([`commit_gone`]).
    counted: parking_lot::RwLock<Vec<NodeID>>,
    /// The directories, sorted, whose moves marked dirty alone the commit left out
    /// ([`prune_dirty_for_commit`]): the working tree holds them, and what they hold, elsewhere
    /// than the walk spells.
    left_out: Vec<NodeID>,
    /// Each staged add the commit leaves out below a directory that went back to its committed
    /// place, with its path there, for a commit of the instance's own tree to add again
    /// past it, marked dirty alone ([`crate::file::dirty::CarriedMarks::added`]); `None` for a
    /// walk whose tree another commit marks again, as a link's or a layer's.
    added: Option<parking_lot::Mutex<Vec<crate::file::dirty::AddedNode>>>,
    /// The path from the instance's root of each node a link's prune staged for deletion as a
    /// staged node of its name replaced it ([`Pruned::deleted`]), for a commit of the instance's
    /// own tree to mark nothing again at ([`LeftToMark::replaced`]): shared with the walks of the
    /// links below, `None` for a walk whose tree another commit marks again.
    replaced: Option<Arc<parking_lot::Mutex<Vec<RelativePath>>>>,
}

/// What a commit of the instance's own tree leaves to the dirty marks it makes again past it,
/// beside the dirty-only nodes it found: the staged adds [`Gone::added`] collects, and the paths of
/// the replaced nodes [`Gone::replaced`] does, what they hold left out.
#[derive(Default)]
pub(crate) struct LeftToMark {
    pub added: Vec<crate::file::dirty::AddedNode>,
    pub replaced: Vec<RelativePath>,
}

impl Gone {
    /// Takes what a walk task left as it left its directories.
    fn take(&self, left: Left) {
        if !left.counted.is_empty() {
            self.counted.write().extend(left.counted);
        }
        if !left.directories.is_empty() {
            self.directories.write().extend(left.directories);
        }
    }

    /// What a walk from another root of the same tree leaves to settle. Only the walk of the
    /// instance's own tree collects adds ([`Self::added`]), and it has one root.
    fn sibling(&self) -> Self {
        Self {
            left_out: self.left_out.clone(),
            replaced: self.replaced.clone(),
            ..Self::default()
        }
    }

    /// What a walk leaves to settle where the commit left out the moves `left_out`, found by an
    /// earlier prune, and those of `pruned` ([`Self::left_out`]).
    fn left_out(mut left_out: Vec<NodeID>, pruned: &Pruned) -> Self {
        left_out.extend(pruned.left_out());
        left_out.sort_unstable();
        left_out.dedup();
        Self {
            left_out,
            ..Self::default()
        }
    }
}

/// Commits a staged node the working tree no longer holds as deleted, as `gone` finds them, reading
/// the working tree through `operation`. A gone file the working tree holds under another case
/// refuses the commit as not found ([`refuse_case_variants`]). A file staged as moved goes back to
/// its committed path where the working tree holds a file there, the move left out
/// ([`return_moved_files`]), and so does a directory staged as moved where the working tree holds a
/// directory there ([`return_moved_directories`]), with what moved with it, which stays as
/// committed. A staged directory the working tree no longer holds leaves with everything below it,
/// the topmost one above each gone file and each directory the walk leaves to the climb, but for
/// one staged only for what lies below it that holds a child not staged, which stays, with each
/// gone directory above it staged so too ([`Gone::directories`], [`gone_directories`]), and the
/// delta drops what the walk recorded of what leaves with it or goes back. What leaves goes to
/// `discard` and `subnodes_to_discard`, staged for deletion, in place of what they listed below
/// such a directory, for [`commit_discard`] to record and free. `walk_root` is the node the walk
/// starts at, which never leaves, and its working-tree path. Answers the sorted nodes the revision
/// holds nothing at the walk's path of: those that leave and the directories that go back.
///
/// `operation` is told the path of each gone file and of the topmost directory of those that leave
/// together, as it is those of what was staged as deleted
/// ([`InstanceOperationImpl::committed_removal`],
/// [`InstanceOperationImpl::committed_tree_removal`]): the revision holds nothing at a gone
/// file's path whether the file leaves or goes back to its committed one.
#[allow(clippy::too_many_arguments)]
async fn commit_gone(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    walk_root: (&RelativePath, NodeID),
    delta: &parking_lot::RwLock<BytesMut>,
    gone: &Gone,
    discard: &parking_lot::RwLock<Vec<(NodeID, u16)>>,
    subnodes_to_discard: &parking_lot::RwLock<Vec<(NodeID, u16)>>,
    stats: &CommitStats,
) -> Result<Vec<NodeID>, CommitError> {
    let files = std::mem::take(&mut *gone.files.write());
    let directories = std::mem::take(&mut *gone.directories.write());
    if files.is_empty() && directories.is_empty() {
        return Ok(Vec::new());
    }
    if lists_missed_files(operation) && !files.is_empty() {
        Box::pin(refuse_case_variants(operation, &files)).await?;
    }
    lore_debug!(
        "Committing {} staged files the working tree no longer holds, and {} directories it may \
         not hold",
        files.len(),
        directories.len()
    );
    let base = state.committed_revision();
    let mut starts = if gone.left_out.is_empty() {
        directories
    } else {
        Box::pin(outside_left_out(
            repository,
            state,
            &gone.left_out,
            directories,
        ))
        .await?
    };
    starts.reserve(files.len());
    let mut roots = Vec::with_capacity(files.len());
    let mut moved = Vec::new();
    for (node, path) in files {
        operation.committed_removal(&path);
        let record = state
            .node(repository.clone(), node)
            .await
            .forward::<CommitError>("Failed reading a staged file gone from the working tree")?;
        starts.push(Holder {
            node: record.parent,
            path: path.parent_path(),
            sent_file: false,
            bare: false,
        });
        let candidate = if record.is_staged_move() && !record.is_staged_add() {
            Box::pin(moved_back_source(
                operation,
                repository,
                state,
                (base, walk_root),
                (node, &record),
            ))
            .await?
        } else {
            None
        };
        match candidate {
            Some(candidate) => moved.push(candidate),
            None => roots.push((node, record.parent, deleted_flags(record.flags))),
        }
    }
    if !moved.is_empty() {
        Box::pin(return_moved_files(
            operation, repository, state, moved, &mut roots, stats,
        ))
        .await?;
    }
    let (mut on_disk, gone_paths) = Box::pin(gone_directories(
        operation,
        repository,
        state,
        walk_root.1,
        starts,
    ))
    .await?;
    let (returned, added_tops) = if on_disk.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        Box::pin(return_moved_directories(
            operation,
            repository,
            state,
            (base, walk_root),
            &mut on_disk,
        ))
        .await?
    };
    let mut topmost: Vec<NodeID> = on_disk
        .values()
        .filter_map(|known| match known {
            OnDisk::Below(top) => Some(*top),
            _ => None,
        })
        .collect();
    topmost.sort_unstable();
    topmost.dedup();
    for top in &topmost {
        let path = gone_paths.get(top);
        debug_assert!(path.is_some(), "a directory that leaves has its path");
        if let Some(path) = path {
            operation.committed_tree_removal(path);
        }
    }
    drop(gone_paths);
    let (gone_below, kept) = if returned.is_empty() {
        drop(added_tops);
        let mut gone_below = Vec::new();
        let mut kept = Vec::with_capacity(roots.len());
        for (node, parent, flags) in roots {
            match on_disk.get(&parent) {
                Some(OnDisk::Below(_)) => gone_below.push(node),
                _ => kept.push((node, flags)),
            }
        }
        (gone_below, kept)
    } else {
        Box::pin(settle_returned_roots(
            repository,
            state,
            (base, gone),
            roots,
            (&on_disk, added_tops),
            stats,
        ))
        .await?
    };
    if !returned.is_empty() {
        let mut delta = delta.write();
        delta_retain(&mut delta, |node| returned.binary_search(&node).is_err());
        for &directory in &returned {
            delta_push(&mut delta, directory, NodeFlags::Staged.bits());
        }
    }
    if topmost.is_empty() {
        discard.write().extend(kept);
        return Ok(returned);
    }
    let below = Arc::new(parking_lot::RwLock::new(Vec::new()));
    let mut tops = Vec::with_capacity(topmost.len());
    for &top in &topmost {
        let record = state
            .node(repository.clone(), top)
            .await
            .forward::<CommitError>("Failed reading a directory gone from the working tree")?;
        tops.push((top, deleted_flags(record.flags), record.parent));
        let below = below.clone();
        state::node_discard_walk(
            state.clone(),
            repository.clone(),
            top,
            move |node, flags| {
                below.write().push((node, flags));
            },
        )
        .await
        .forward::<CommitError>("Failed collecting a directory gone from the working tree")?;
    }
    let mut below = std::mem::take(&mut *below.write());
    let mut leaving: Vec<NodeID> = below
        .iter()
        .map(|&(node, _)| node)
        .chain(topmost.iter().copied())
        .collect();
    leaving.sort_unstable();
    let stays = |&(node, _): &(NodeID, u16)| leaving.binary_search(&node).is_err();
    delta_retain(&mut delta.write(), |node| {
        leaving.binary_search(&node).is_err()
    });
    let mut counted = gone_below;
    {
        let mut subnodes = subnodes_to_discard.write();
        counted.extend(
            subnodes
                .iter()
                .filter(|entry| !stays(entry))
                .map(|&(node, _)| node),
        );
        subnodes.retain(stays);
    }
    {
        let mut discard = discard.write();
        counted.extend(
            discard
                .iter()
                .filter(|entry| !stays(entry))
                .map(|&(node, _)| node),
        );
        discard.retain(stays);
        discard.extend(kept);
        discard.extend(tops.iter().map(|&(top, flags, _)| (top, flags)));
    }
    counted.sort_unstable();
    count_totals(
        stats,
        below
            .iter()
            .filter(|&&(node, _)| counted.binary_search(&node).is_err())
            .map(|&(_, flags)| flags),
    );
    for entry in &mut below {
        entry.1 = deleted_flags(entry.1);
    }
    subnodes_to_discard.write().extend(below);
    let above = tops.into_iter().map(|(_, _, parent)| parent).collect();
    count_gone_directories(stats, &leaving, &mut gone.counted.write(), &topmost, above);
    if !returned.is_empty() {
        leaving.extend(returned);
        leaving.sort_unstable();
    }
    Ok(leaving)
}

/// `holders` without each bare one ([`Holder::bare`]) inside a directory of `left_out`, sorted,
/// whose dirty-only move the commit left out: the working tree holds it elsewhere than the walk
/// spells. Each bare holder's ancestors are read once, each looked up in `left_out`.
async fn outside_left_out(
    repository: &Arc<RepositoryContext>,
    state: &State,
    left_out: &[NodeID],
    holders: Vec<Holder>,
) -> Result<Vec<Holder>, CommitError> {
    let mut kept = Vec::with_capacity(holders.len());
    'holders: for holder in holders {
        if holder.bare {
            let mut at = holder.node;
            while at.is_valid_node_id() {
                if left_out.binary_search(&at).is_ok() {
                    continue 'holders;
                }
                at = state
                    .node(repository.clone(), at)
                    .await
                    .forward::<CommitError>("Failed reading a directory's place")?
                    .parent;
            }
        }
        kept.push(holder);
    }
    Ok(kept)
}

/// What a staged file gone from the walk's path below a directory that went back to its committed
/// place ([`return_moved_directories`]) is to the commit ([`went_back_with`]).
enum WentBack {
    /// It stands where `base` holds it, having moved with that directory alone, and stays as
    /// committed, as [`return_moved_file`] leaves a file.
    Committed,
    /// An add, a copy staged as one among them, which `base` holds nowhere: it leaves the
    /// revision, and stays below the directory as an add marked dirty alone ([`Gone::added`]).
    Added,
    /// A node a move took there from elsewhere: it leaves.
    Elsewhere,
}

/// Splits `roots`, the gone files [`commit_gone`] deletes, by what `on_disk` found of their parents:
/// those that leave with their directory, and those deleted where they stand. One below a directory
/// that went back stays as committed, is left out as added, or is deleted, as [`went_back_with`]
/// answers, and each add left out, with `added_tops`, the added directories that leave so, goes to
/// `gone` to carry past the commit ([`added_at_returned`]). Boxed by its caller, which takes it only
/// where a directory went back.
async fn settle_returned_roots(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    (base, gone): (Hash, &Gone),
    roots: Vec<(NodeID, NodeID, u16)>,
    (on_disk, added_tops): (&HashMap<NodeID, OnDisk>, Vec<NodeID>),
    stats: &CommitStats,
) -> Result<(Vec<NodeID>, Vec<(NodeID, u16)>), CommitError> {
    let mut gone_below = Vec::new();
    let mut kept = Vec::with_capacity(roots.len());
    let mut added = Vec::new();
    for (node, parent, flags) in roots {
        match on_disk.get(&parent) {
            Some(OnDisk::Below(_)) => gone_below.push(node),
            Some(OnDisk::Returned) => {
                match Box::pin(went_back_with(repository, state, base, node)).await? {
                    WentBack::Committed => {
                        stats.complete.file_count.fetch_add(1, Ordering::Release);
                    }
                    WentBack::Added => {
                        added.push(node);
                        kept.push((node, flags));
                    }
                    WentBack::Elsewhere => kept.push((node, flags)),
                }
            }
            _ => kept.push((node, flags)),
        }
    }
    if let Some(carried) = &gone.added
        && (!added.is_empty() || !added_tops.is_empty())
    {
        let mut found = Vec::new();
        for node in added.into_iter().chain(added_tops) {
            Box::pin(added_at_returned(repository, state, node, &mut found)).await?;
        }
        carried.lock().extend(found);
    }
    Ok((gone_below, kept))
}

/// What `node`, a staged file gone from the walk's path below a directory that went back, is to
/// the commit ([`WentBack`]), its staged change cleared where it stays as committed.
async fn went_back_with(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    base: Hash,
    node: NodeID,
) -> Result<WentBack, CommitError> {
    let record = state
        .node(repository.clone(), node)
        .await
        .forward::<CommitError>("Failed reading a staged file gone from the working tree")?;
    match state
        .at_base_place(repository, base, (node, &record), &mut String::new())
        .await
        .forward::<CommitError>("Failed reading where the revision holds a file")?
    {
        Some(true) => {}
        Some(false) => return Ok(WentBack::Elsewhere),
        None => return Ok(WentBack::Added),
    }
    let block = state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    state.node_clear_own(&block, node, OwnFlags::Change);
    Ok(WentBack::Committed)
}

/// Records in `added` `node`, a staged add that went back with a directory, and for an added
/// directory each add below it, with the path the staged tree holds each at, the place it went back
/// to, parents first, for the commit to mark them dirty again past it ([`Gone::added`]). A node a
/// move brought in below an added directory leaves with it.
async fn added_at_returned(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    node: NodeID,
    added: &mut Vec<crate::file::dirty::AddedNode>,
) -> Result<(), CommitError> {
    let Some(path) = state
        .node_path_below(repository.clone(), node, ROOT_NODE)
        .await
        .forward::<CommitError>("Failed reading the path of an add that went back")?
    else {
        return Ok(());
    };
    let record = state
        .node(repository.clone(), node)
        .await
        .forward::<CommitError>("Failed reading an add that went back")?;
    added.push(crate::file::dirty::AddedNode {
        path: path.clone(),
        directory: record.is_directory(),
    });
    if !record.is_directory() {
        return Ok(());
    }
    let mut pending = vec![(node, path)];
    while let Some((directory, path)) = pending.pop() {
        let mut children =
            StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), directory)
                .await
                .forward::<CommitError>("Failed reading the children of an add that went back")?;
        while let Some((child, record, name)) = children
            .next()
            .await
            .forward::<CommitError>("Failed reading a child of an add that went back")?
        {
            if !record.is_added() {
                continue;
            }
            let child_path = RelativePath::new_from_clean_parts(path.as_str(), &name);
            drop(name);
            added.push(crate::file::dirty::AddedNode {
                path: child_path.clone(),
                directory: record.is_directory(),
            });
            if record.is_directory() {
                pending.push((child, child_path));
            }
        }
    }
    Ok(())
}

/// Counts the directories a gone directory takes with it, `leaving`, its `topmost` among them,
/// each once in the total and once as done, as a commit of the topmost's staged deletion counts
/// them: the topmost in the total, as [`commit_discard`] counts them done, each the walk counted
/// as it left it (`counted`) no more, and each directory `above` a topmost one as changed, where
/// the walk did not count it.
fn count_gone_directories(
    stats: &CommitStats,
    leaving: &[NodeID],
    counted: &mut [NodeID],
    topmost: &[NodeID],
    mut above: Vec<NodeID>,
) {
    counted.sort_unstable();
    let walked = counted
        .iter()
        .filter(|node| leaving.binary_search(node).is_ok())
        .count() as u64;
    let complete = &stats.complete;
    complete
        .directory_total
        .fetch_add(topmost.len() as u64, Ordering::Relaxed);
    complete
        .directory_total
        .fetch_sub(walked, Ordering::Relaxed);
    complete
        .directory_count
        .fetch_sub(walked, Ordering::Release);
    above.sort_unstable();
    above.dedup();
    for parent in above {
        if leaving.binary_search(&parent).is_err() && counted.binary_search(&parent).is_err() {
            stats.count_directory();
        }
    }
}

/// Counts each node of `flags` in the totals of the work `stats` reports, a file or a directory,
/// each total that takes one with a single add.
fn count_totals(stats: &CommitStats, flags: impl Iterator<Item = u16>) {
    let (mut files, mut directories) = (0, 0);
    for flags in flags {
        if flags & NodeFlags::File != 0 {
            files += 1;
        } else {
            directories += 1;
        }
    }
    let complete = &stats.complete;
    if files != 0 {
        complete.file_total.fetch_add(files, Ordering::Relaxed);
    }
    if directories != 0 {
        complete
            .directory_total
            .fetch_add(directories, Ordering::Relaxed);
    }
}

/// `flags`, of a node the commit removes from the revision, staged for deletion, whatever change
/// it carried. The deletion of a node staged as added is recorded as [`unrecorded_deletions`]
/// answers: as nothing where the revision the tree was built on lacks it.
fn deleted_flags(flags: u16) -> u16 {
    (flags & !NodeFlags::ActionBits.bits()) | NodeFlags::StagedDelete.bits()
}

/// A staged file the working tree no longer holds at its staged path that may go back to the place
/// the base revision holds it at ([`moved_back_source`]): the node and its record, that place, its
/// name and its path there, and whether the working tree holds a file there, `None` where a stat
/// missed it, for [`listed_files`] to answer.
struct MovedBack {
    node: NodeID,
    record: Node,
    place: (NodeID, u64),
    name: String,
    source: RelativePath,
    held: Option<bool>,
}

/// Where `node`, with its record `record`, a staged file the working tree no longer holds, may go
/// back to: staged as moved, it goes back to the place `base` holds it at where the working tree
/// holds a file at that path and nothing else holds that place, as committed ([`return_moved_file`]).
/// `walk_root` is the node the walk starts at and its working-tree path, below which `base` holds the
/// place. A file standing under its committed parent by its committed name moved only with a
/// directory, and is none, as is one whose committed place is staged for deletion
/// ([`place_staged_for_deletion`]): the walk has collected that directory's subtree to discard
/// already, so the file leaves as one gone from the working tree. One file-info read of that path.
/// Boxed by its caller, which takes it for a staged move alone.
async fn moved_back_source(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    (base, walk_root): (Hash, (&RelativePath, NodeID)),
    (node, record): (NodeID, &Node),
) -> Result<Option<MovedBack>, CommitError> {
    let (root_path, root) = walk_root;
    let flags = NodeFlags::from_bits_retain(record.flags);
    let mut name = String::new();
    let Some((place, below)) = state
        .base_place_below(
            repository,
            (base, node, root),
            (flags, record.address.context),
            &mut name,
        )
        .await
        .forward::<CommitError>("Failed reading where the revision holds a moved file")?
    else {
        return Ok(None);
    };
    if place_staged_for_deletion(state, repository, place.0).await? {
        return Ok(None);
    }
    if record.parent == place.0
        && record.name_hash == place.1
        && state
            .node_name_is(repository.clone(), node, &name)
            .await
            .forward::<CommitError>("Failed reading a moved file's name")?
    {
        return Ok(None);
    }
    let source = RelativePath::new_from_clean_parts(root_path.as_str(), below.as_str());
    let info = operation
        .file_info(&source)
        .await
        .forward_with::<CommitError, _>(|| format!("Failed to get metadata for file {source}"))?;
    Ok(Some(MovedBack {
        node,
        record: *record,
        place,
        name,
        source,
        held: info.exists().then(|| !info.is_dir()),
    }))
}

/// Returns `moved`, a staged file whose committed path the working tree holds a file at, there,
/// the move left out, its staged change cleared, and answers whether it went back. Its content
/// there is not read, and the commit keeps the committed content: a later scan marks an edit as a
/// modification.
async fn return_moved_file(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    moved: &MovedBack,
) -> Result<bool, CommitError> {
    let MovedBack {
        node,
        record,
        place,
        name,
        ..
    } = moved;
    let flags = NodeFlags::from_bits_retain(record.flags);
    let (returned, _) = state
        .node_return_into(
            repository,
            (*node, record.parent, record.name_hash, flags),
            (*place, name),
            ReturnOptions::default(),
        )
        .await
        .forward::<CommitError>("Failed returning a moved file to its committed path")?;
    let returned = returned == Return::Back;
    if returned {
        let block = state
            .block(repository.clone(), NodeBlock::index(*node))
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
        state.node_clear_own(&block, *node, OwnFlags::Change);
    }
    Ok(returned)
}

/// Returns each of `moved` whose committed path the working tree holds a file at
/// ([`return_moved_file`]), a stat's miss answered from its parent's listing where a file system
/// may miss it so ([`listed_files`]), each distinct parent listed once; one that does not go back is
/// pushed to `roots`, gone. Counts each one that goes back done in `stats`.
async fn return_moved_files(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    moved: Vec<MovedBack>,
    roots: &mut Vec<(NodeID, NodeID, u16)>,
    stats: &CommitStats,
) -> Result<(), CommitError> {
    let missed: Vec<RelativePath> = moved
        .iter()
        .filter(|moved| moved.held.is_none())
        .map(|moved| moved.source.clone())
        .collect();
    let listed = if missed.is_empty() || !lists_missed_files(operation) {
        Vec::new()
    } else {
        listed_files(operation, &missed).await?
    };
    let mut listed = listed.into_iter();
    for candidate in moved {
        let held = match candidate.held {
            Some(held) => held,
            None => listed.next().unwrap_or(false),
        };
        if held && return_moved_file(repository, state, &candidate).await? {
            stats.complete.file_count.fetch_add(1, Ordering::Release);
        } else {
            let record = candidate.record;
            roots.push((candidate.node, record.parent, deleted_flags(record.flags)));
        }
    }
    Ok(())
}

/// Whether a stat of `operation`'s working tree may miss a file it lists: on a file system that
/// folds no case, one held under another case ([`CaseLookup::OnMiss`]), and on Windows one a stat is
/// refused permission for, as of a path mid-deletion or one the user may not read.
fn lists_missed_files(operation: &InstanceOperationImpl) -> bool {
    operation.case_lookup() == CaseLookup::OnMiss || cfg!(target_family = "windows")
}

/// Which of `paths`, files a stat missed, the working tree lists, as a file of a name folding to
/// theirs below their parent, in the spelling the working tree holds the parent in
/// ([`HeldSpellings`]). Each distinct parent is listed once, each name hashed once.
async fn listed_files(
    operation: &InstanceOperationImpl,
    paths: &[RelativePath],
) -> Result<Vec<bool>, CommitError> {
    let mut listed = vec![false; paths.len()];
    let mut by_parent = by_parent_and_name(paths, |_| true);
    by_parent.sort_unstable_by(by_path_then_hash);
    let mut spellings = HeldSpellings::default();
    for group in by_parent.chunk_by(|left, right| left.0.as_str() == right.0.as_str()) {
        let parent = &group[0].0;
        let held = if parent.is_empty() {
            Some(RelativePath::new())
        } else {
            spellings.of(operation, parent).await?.map(|(held, _)| held)
        };
        let Some(held) = held else {
            continue;
        };
        let mut listing = match operation.read_directory(&held).await {
            Ok(listing) => listing,
            Err(error) if error.is_file_not_found() => continue,
            Err(error) => {
                return Err(error)
                    .forward_with::<CommitError, _>(|| format!("Failed to list {held}"));
            }
        };
        while let Some(entry) = listing.next().await {
            let entry =
                entry.forward_with::<CommitError, _>(|| format!("Failed to list {held}"))?;
            if entry.info.is_dir() {
                continue;
            }
            for &(_, _, index) in carrying(group, entry.name_hash) {
                listed[index] = true;
            }
        }
    }
    Ok(listed)
}

/// Each of `paths` `keeps` answers true for, by its index, with its parent and the hash of its
/// name, for a listing of each parent to find them by.
fn by_parent_and_name(
    paths: &[RelativePath],
    keeps: impl Fn(usize) -> bool,
) -> Vec<(RelativePath, u64, usize)> {
    paths
        .iter()
        .enumerate()
        .filter(|&(index, path)| keeps(index) && !path.is_empty())
        .map(|(index, path)| {
            (
                path.parent_path(),
                crate::hash::hash_string(path.name()),
                index,
            )
        })
        .collect()
}

/// The order [`by_parent_and_name`]'s entries are grouped in: by parent, then by name hash.
fn by_path_then_hash(
    left: &(RelativePath, u64, usize),
    right: &(RelativePath, u64, usize),
) -> std::cmp::Ordering {
    left.0
        .as_str()
        .cmp(right.0.as_str())
        .then(left.1.cmp(&right.1))
}

/// The entries of `group`, one parent's sorted by name hash, carrying `name_hash`.
fn carrying(group: &[(RelativePath, u64, usize)], name_hash: u64) -> &[(RelativePath, u64, usize)] {
    let start = group.partition_point(|&(_, hash, _)| hash < name_hash);
    let end = start + group[start..].partition_point(|&(_, hash, _)| hash == name_hash);
    &group[start..end]
}

/// What [`gone_directories`] finds the working tree holds of a directory.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnDisk {
    /// The working tree holds it, or it does not leave: a climb stops at it.
    Held,
    /// The working tree no longer holds it, and the directory it leaves with is not yet known.
    Gone,
    /// It leaves with the directory named, the topmost gone one above it, or itself.
    Below(NodeID),
    /// It went back to its committed place with a directory staged as moved, itself or one above
    /// it ([`return_moved_directories`]), and stays as committed.
    Returned,
    /// A directory staged as moved that the working tree no longer holds, which does not leave
    /// only as it holds a link's mount or what the view excludes: it goes back where the working
    /// tree holds a directory at its committed path ([`return_moved_directories`]), and is held
    /// otherwise.
    Kept,
}

/// A directory the walk leaves to [`commit_gone`]'s climb, with its path, and whether its frame
/// sent a file to [`commit_file`]: one whose read succeeded shows the directory held, unless it is
/// also the parent of a gone file. So does a merged file past the view, which the commit reads
/// from the node rather than the disk ([`takes_staged_content`]): the working tree lacks what the
/// view excludes without its having been deleted. A bare one inside a directory whose dirty move
/// the commit left out is no start: the working tree holds it elsewhere ([`Gone::left_out`]).
struct Holder {
    node: NodeID,
    path: RelativePath,
    sent_file: bool,
    /// Whether the directory is staged only for what lies below it, as for its rehash after a
    /// staged move left it, and holds no staged child.
    bare: bool,
}

/// Finds what the working tree holds of each directory of `starts`, and of those above it: the
/// directories of `starts` read side by side ([`directories_held`]), but for one a file it sent
/// showed held ([`Holder`]), then the parents of those it no longer holds, a level at a time, up
/// to the first held one. Answers what it found of each directory, with the path of each that may
/// leave, and nothing where the working tree holds every one of `starts`. `walk_root`, the node
/// the walk starts at, and the root are held.
/// Boxed by [`commit_gone`], whose future it would outgrow.
///
/// A directory the working tree no longer holds leaves only where it is staged, if only for a node
/// below it, the view takes in everything below it
/// ([`crate::filter::Filter::view_covers_subtree`]), and it holds no link's mount
/// ([`linked_directories`]): a working tree lacks what the view excludes without its having been
/// deleted, and a link leaves only by its removal. Elsewhere it counts as held, but for one staged
/// as moved, which may go back ([`OnDisk::Kept`]). One staged only for what lies below it
/// ([`state::staged_for_below`]) that holds a child not staged counts as held, with each gone
/// directory above it, where each of those is staged only for what lies below it too
/// ([`hold_unstaged`]): the commit deletes only what was staged. A file at its path counts as gone.
async fn gone_directories(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &State,
    walk_root: NodeID,
    mut starts: Vec<Holder>,
) -> Result<(HashMap<NodeID, OnDisk>, HashMap<NodeID, RelativePath>), CommitError> {
    starts.sort_unstable_by_key(|holder| holder.node);
    starts.dedup_by(|later, kept| {
        let same = later.node == kept.node;
        if same {
            kept.sent_file &= later.sent_file;
        }
        same
    });
    starts.retain(|holder| holder.node != walk_root && holder.node != ROOT_NODE);
    let mut on_disk = HashMap::new();
    let mut frontier: Vec<(NodeID, RelativePath)> = Vec::with_capacity(starts.len());
    for holder in starts {
        if holder.sent_file {
            on_disk.insert(holder.node, OnDisk::Held);
        } else {
            frontier.push((holder.node, holder.path));
        }
    }
    let mut frontier = read_held(operation, frontier, &mut on_disk).await?;
    if frontier.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    let mut up = HashMap::new();
    let mut paths = HashMap::new();
    let mut holding = Vec::new();
    let mut linked = None;
    let filter = &repository.filter;
    while !frontier.is_empty() {
        let mut parents = Vec::new();
        for (directory, path) in frontier {
            let (parent, staged, bare, kept) = {
                let node = state
                    .node(repository.clone(), directory)
                    .await
                    .forward::<CommitError>(
                        "Failed reading a directory gone from the working tree",
                    )?;
                let staged = node.is_directory() && node.is_staged();
                let kept = staged && node.is_staged_move() && !node.is_staged_add();
                (node.parent, staged, state::staged_for_below(&node), kept)
            };
            let leaves = staged
                && filter.view_covers_subtree(filter.exclusion_states(&path), &path)
                && !holds_link(repository, state, &mut linked, directory).await?;
            if !leaves {
                on_disk.insert(directory, if kept { OnDisk::Kept } else { OnDisk::Held });
                continue;
            }
            if bare
                && state
                    .node_has_unstaged_children(repository.clone(), directory)
                    .await
                    .forward::<CommitError>("Failed reading a directory's children")?
            {
                holding.push(directory);
            }
            up.insert(directory, (parent, bare));
            if parent != walk_root && parent != ROOT_NODE && !on_disk.contains_key(&parent) {
                parents.push((parent, path.parent_path()));
            }
            paths.insert(directory, path);
        }
        parents.sort_unstable_by_key(|&(node, _)| node);
        parents.dedup_by_key(|(node, _)| *node);
        frontier = read_held(operation, parents, &mut on_disk).await?;
    }
    hold_unstaged(&holding, &mut up, &mut on_disk);
    let mut chain = Vec::new();
    let gone: Vec<NodeID> = up.keys().copied().collect();
    for directory in gone {
        if matches!(on_disk.get(&directory), Some(OnDisk::Below(_))) {
            continue;
        }
        chain.clear();
        let mut at = directory;
        let top = loop {
            chain.push(at);
            let Some(&(parent, _)) = up.get(&at) else {
                break at;
            };
            match on_disk.get(&parent) {
                Some(OnDisk::Below(top)) => break *top,
                Some(OnDisk::Gone) => at = parent,
                _ => break at,
            }
        };
        for &below in &chain {
            on_disk.insert(below, OnDisk::Below(top));
        }
    }
    Ok((on_disk, paths))
}

/// Holds each gone directory of `holding`, staged only for what lies below it and holding a child
/// not staged, with each gone directory above it up to the first held one, where every one of those
/// is staged only for what lies below it too: `up` maps each gone directory to its parent and
/// whether it is staged so ([`state::staged_for_below`]). One below a gone directory with a staged
/// change of its own leaves or goes back with that directory instead, as its change says, and each
/// gone directory its climb passed is then marked as not staged so, for a later climb to stop at.
/// Reads nothing, and passes each gone directory at most twice.
fn hold_unstaged(
    holding: &[NodeID],
    up: &mut HashMap<NodeID, (NodeID, bool)>,
    on_disk: &mut HashMap<NodeID, OnDisk>,
) {
    for &directory in holding {
        let mut at = directory;
        while let Some(&(parent, true)) = up.get(&at) {
            at = parent;
        }
        let held = !up.contains_key(&at);
        let mut at = directory;
        if held {
            while let Some((parent, _)) = up.remove(&at) {
                on_disk.insert(at, OnDisk::Held);
                at = parent;
            }
        } else {
            while let Some(entry) = up.get_mut(&at).filter(|entry| entry.1) {
                entry.1 = false;
                at = entry.0;
            }
        }
    }
}

/// Reads whether the working tree holds each directory of `directories` ([`directories_held`]),
/// recording each in `on_disk`, and answers those it no longer holds.
async fn read_held(
    operation: &InstanceOperationImpl,
    directories: Vec<(NodeID, RelativePath)>,
    on_disk: &mut HashMap<NodeID, OnDisk>,
) -> Result<Vec<(NodeID, RelativePath)>, CommitError> {
    if directories.is_empty() {
        return Ok(directories);
    }
    let paths: Arc<[RelativePath]> = directories.iter().map(|(_, path)| path.clone()).collect();
    let held = directories_held(operation, paths).await?;
    let mut gone = Vec::new();
    for ((directory, path), held) in directories.into_iter().zip(held) {
        if held {
            on_disk.insert(directory, OnDisk::Held);
        } else {
            on_disk.insert(directory, OnDisk::Gone);
            gone.push((directory, path));
        }
    }
    Ok(gone)
}

/// Whether the directory `directory` holds a link's mount below it, from `linked`, the
/// directories that do, sorted, read once from the tree's links the first time it is asked
/// ([`linked_directories`], boxed, as one read serves the climb).
async fn holds_link(
    repository: &Arc<RepositoryContext>,
    state: &State,
    linked: &mut Option<Vec<NodeID>>,
    directory: NodeID,
) -> Result<bool, CommitError> {
    if linked.is_none() {
        *linked = Some(Box::pin(linked_directories(repository, state)).await?);
    }
    Ok(linked
        .as_ref()
        .is_some_and(|linked| linked.binary_search(&directory).is_ok()))
}

/// The directories that hold a link's mount below them, sorted: the ancestors of each link node
/// the tree registers, but for one staged for deletion, which leaves by its removal.
async fn linked_directories(
    repository: &Arc<RepositoryContext>,
    state: &State,
) -> Result<Vec<NodeID>, CommitError> {
    let links = state
        .link_list(repository.clone())
        .await
        .forward::<CommitError>("Failed to read link registry")?;
    let mut linked = Vec::new();
    for link in links {
        let mut at = link.local_node();
        if !at.is_valid_node_id() {
            continue;
        }
        let node = state
            .node(repository.clone(), at)
            .await
            .forward::<CommitError>("Failed reading a link node")?;
        if node.is_discarded() || node.is_staged_delete() {
            continue;
        }
        at = node.parent;
        while at.is_valid_node_id() && at != ROOT_NODE {
            linked.push(at);
            at = state
                .node(repository.clone(), at)
                .await
                .forward::<CommitError>("Failed reading a directory above a link")?
                .parent;
        }
    }
    linked.sort_unstable();
    linked.dedup();
    Ok(linked)
}

/// Returns each directory staged as moved that `on_disk` finds gone, leaving or kept
/// ([`OnDisk::Kept`]), to its committed place, where the working tree holds a directory there
/// ([`moved_directory_source`]), as [`return_moved_file`] returns a file, the shallowest
/// committed place first and then by node, so the outcome follows no map's order: one
/// file-information read of the committed path per such directory, side by side
/// ([`directories_held`]). One inside a directory that went back goes back to its own place where
/// that is held. A directory standing under its committed parent by its committed name moved only
/// with one above it. Each directory that went back is recorded [`OnDisk::Returned`] in `on_disk`,
/// as is each gone one below it that stands where `base` holds it, having moved with it alone,
/// each with its own staged change cleared; any other gone one below, as one added or moved there
/// from elsewhere, leaves with what it holds ([`settle_below_returned`]). The gone files below them
/// the caller settles. Answers what [`settle_below_returned`] answers.
async fn return_moved_directories(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    (base, walk_root): (Hash, (&RelativePath, NodeID)),
    on_disk: &mut HashMap<NodeID, OnDisk>,
) -> Result<(Vec<NodeID>, Vec<NodeID>), CommitError> {
    let mut moved = Vec::new();
    let mut name = String::new();
    for (&directory, known) in on_disk.iter() {
        if !matches!(known, OnDisk::Below(_) | OnDisk::Kept) {
            continue;
        }
        let record = state
            .node(repository.clone(), directory)
            .await
            .forward::<CommitError>("Failed reading a directory gone from the working tree")?;
        if let Some((place, source)) = moved_directory_source(
            repository,
            state,
            (base, (walk_root.0.as_str(), walk_root.1)),
            (directory, &record),
            &mut name,
        )
        .await?
        {
            moved.push((directory, record, place, name.clone(), source));
        }
    }
    if moved.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    moved.sort_unstable_by_key(|(directory, _, _, _, source)| {
        (source.as_str().matches('/').count(), *directory)
    });
    let sources: Arc<[RelativePath]> = moved.iter().map(|moved| moved.4.clone()).collect();
    let held = directories_held(operation, sources).await?;
    let mut returned: Vec<NodeID> = Vec::new();
    for ((directory, record, place, name, _), held) in moved.into_iter().zip(held) {
        if !held {
            continue;
        }
        let flags = NodeFlags::from_bits_retain(record.flags);
        let (went, _) = state
            .node_return_into(
                repository,
                (directory, record.parent, record.name_hash, flags),
                (place, &name),
                ReturnOptions::default(),
            )
            .await
            .forward::<CommitError>("Failed returning a moved directory to its committed path")?;
        if went == Return::Back {
            returned.push(directory);
        }
    }
    if returned.is_empty() {
        return Ok((returned, Vec::new()));
    }
    returned.sort_unstable();
    Box::pin(settle_below_returned(
        repository, state, base, &returned, on_disk,
    ))
    .await
}

/// Where the directory `directory`, staged as moved, goes back to ([`return_moved_directories`]):
/// its committed place, its name there copied into `name`, and its committed path below
/// `walk_root`'s path. `None` for one not staged as moved, one `base` holds nowhere below
/// `walk_root`'s node, one standing at its committed place by its committed name, which moved
/// only with a directory above it, and one whose committed place is staged for deletion
/// ([`place_staged_for_deletion`]), which leaves with what it holds, as the walk has collected that
/// directory's subtree to discard already.
async fn moved_directory_source(
    repository: &Arc<RepositoryContext>,
    state: &State,
    (base, (root_path, root)): (Hash, (&str, NodeID)),
    (directory, record): (NodeID, &Node),
    name: &mut String,
) -> Result<Option<((NodeID, u64), RelativePath)>, CommitError> {
    if !record.is_staged_move() || record.is_staged_add() {
        return Ok(None);
    }
    let flags = NodeFlags::from_bits_retain(record.flags);
    let Some((place, below)) = state
        .base_place_below(
            repository,
            (base, directory, root),
            (flags, record.address.context),
            name,
        )
        .await
        .forward::<CommitError>("Failed reading where the revision holds a moved directory")?
    else {
        return Ok(None);
    };
    if place_staged_for_deletion(state, repository, place.0).await? {
        return Ok(None);
    }
    if record.parent == place.0
        && record.name_hash == place.1
        && state
            .node_name_is(repository.clone(), directory, name)
            .await
            .forward::<CommitError>("Failed reading a moved directory's name")?
    {
        return Ok(None);
    }
    Ok(Some((
        place,
        RelativePath::new_from_clean_parts(root_path, below.as_str()),
    )))
}

/// Settles in `on_disk` each gone directory at or below one of `returned`, the directories that
/// went back, sorted: one of them, and one that stands where `base` holds it below one that did,
/// is [`OnDisk::Returned`], its own staged change cleared and left staged for its rehash, as what
/// it holds may have changed; any other below is the topmost gone directory it leaves with, or
/// below that, and its subtree leaves with it, but for one [`OnDisk::Kept`], which stays so with
/// what it holds. Answers the directories recorded returned, sorted,
/// and the added directories directly below them that leave so, which `base` holds nowhere.
async fn settle_below_returned(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    base: Hash,
    returned: &[NodeID],
    on_disk: &mut HashMap<NodeID, OnDisk>,
) -> Result<(Vec<NodeID>, Vec<NodeID>), CommitError> {
    let mut gone: Vec<NodeID> = on_disk
        .iter()
        .filter(|(_, known)| matches!(known, OnDisk::Below(_) | OnDisk::Kept))
        .map(|(&directory, _)| directory)
        .collect();
    gone.sort_unstable();
    let mut at_base: Vec<NodeID> = Vec::new();
    let mut added: Vec<NodeID> = Vec::new();
    let mut parents: HashMap<NodeID, NodeID> = HashMap::new();
    let mut name = String::new();
    for &directory in &gone {
        let record = state
            .node(repository.clone(), directory)
            .await
            .forward::<CommitError>("Failed reading a directory gone from the working tree")?;
        parents.insert(directory, record.parent);
        if returned.binary_search(&directory).is_ok() {
            continue;
        }
        match state
            .at_base_place(repository, base, (directory, &record), &mut name)
            .await
            .forward::<CommitError>("Failed reading where the revision holds a directory")?
        {
            Some(true) => at_base.push(directory),
            None if record.is_added() => added.push(directory),
            _ => {}
        }
    }
    let mut added_tops = Vec::new();
    let mut chain = Vec::new();
    let mut back = Vec::new();
    for &directory in &gone {
        chain.clear();
        let mut at = directory;
        let mut above = None;
        loop {
            if returned.binary_search(&at).is_ok() {
                above = Some(OnDisk::Returned);
                break;
            }
            chain.push(at);
            match parents.get(&at) {
                Some(&parent) => at = parent,
                None => break,
            }
        }
        let Some(mut known) = above else {
            continue;
        };
        for &below in chain.iter().rev() {
            known = match known {
                OnDisk::Returned if at_base.binary_search(&below).is_ok() => OnDisk::Returned,
                OnDisk::Returned if on_disk.get(&below) == Some(&OnDisk::Kept) => break,
                OnDisk::Returned => {
                    if added.binary_search(&below).is_ok() {
                        added_tops.push(below);
                    }
                    OnDisk::Below(below)
                }
                other => other,
            };
            on_disk.insert(below, known);
        }
    }
    for &directory in returned {
        on_disk.insert(directory, OnDisk::Returned);
    }
    for (&directory, known) in on_disk.iter() {
        if *known == OnDisk::Returned {
            back.push(directory);
        }
    }
    back.sort_unstable();
    for &directory in &back {
        let block = state
            .block(repository.clone(), NodeBlock::index(directory))
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
        state.node_clear_own(&block, directory, OwnFlags::ChangeToBare);
    }
    added_tops.sort_unstable();
    added_tops.dedup();
    Ok((back, added_tops))
}

/// Whether the working tree holds a directory at each of `paths`, in order, read side by side
/// ([`InstanceOperation::directories_held`]). On a file system that folds no case, a path missed
/// in its own spelling is held where its parent, in the spelling the working tree holds it in,
/// lists a directory of its name in another ([`held_in_other_cases`]).
async fn directories_held(
    operation: &InstanceOperationImpl,
    paths: Arc<[RelativePath]>,
) -> Result<Vec<bool>, CommitError> {
    let mut held = Vec::with_capacity(paths.len());
    operation
        .directories_held(paths.clone(), &mut held)
        .await
        .forward::<CommitError>("Failed to read whether directories are held")?;
    if operation.case_lookup() == CaseLookup::OnMiss && held.iter().any(|&held| !held) {
        Box::pin(held_in_other_cases(operation, &paths, &mut held)).await?;
    }
    Ok(held)
}

/// Marks held each path of `paths` that `held` misses, where its parent, in the spelling the
/// working tree holds it in ([`HeldSpellings`]), lists a directory of its name in another
/// spelling, or in its own where the parent's differs. Each distinct parent is listed once, each
/// name hashed once.
async fn held_in_other_cases(
    operation: &InstanceOperationImpl,
    paths: &[RelativePath],
    held: &mut [bool],
) -> Result<(), CommitError> {
    let mut missed = by_parent_and_name(paths, |index| !held[index]);
    missed.sort_unstable_by(by_path_then_hash);
    let mut spellings = HeldSpellings::default();
    for group in missed.chunk_by(|left, right| left.0.as_str() == right.0.as_str()) {
        let parent = &group[0].0;
        let spelled = if parent.is_empty() {
            Some((parent.clone(), false))
        } else {
            spellings.of(operation, parent).await?
        };
        let Some((spelled, varied)) = spelled else {
            continue;
        };
        let mut listing = match operation.read_directory(&spelled).await {
            Ok(listing) => listing,
            Err(error) if error.is_file_not_found() => continue,
            Err(error) => {
                return Err(error)
                    .forward_with::<CommitError, _>(|| format!("Failed to list {spelled}"));
            }
        };
        while let Some(entry) = listing.next().await {
            let entry =
                entry.forward_with::<CommitError, _>(|| format!("Failed to list {spelled}"))?;
            if !entry.info.is_dir() {
                continue;
            }
            for &(_, _, index) in carrying(group, entry.name_hash) {
                if varied || entry.name != paths[index].name() {
                    held[index] = true;
                }
            }
        }
    }
    Ok(())
}

/// Every spelling of `name` the directory at `directory` holds that folds to the same name
/// ([`InstanceOperation::names_folding_to`]); none where the working tree holds no directory there.
async fn names_folding(
    operation: &InstanceOperationImpl,
    directory: &RelativePath,
    name: &str,
) -> Result<Vec<String>, CommitError> {
    match operation.names_folding_to(directory, name).await {
        Ok(names) => Ok(names),
        Err(error) if error.is_file_not_found() => Ok(Vec::new()),
        Err(error) => {
            Err(error).forward_with::<CommitError, _>(|| format!("Failed to list {directory}"))
        }
    }
}

/// The spellings the working tree holds paths in on a file system that folds no case, asked one
/// after another in sorted order: the spelling of each, not empty, with whether it differs from
/// its own, each component a stat misses looked up by its fold, and `None` where some component is
/// held in no spelling. It keeps what it found of each component of the last path asked, so a
/// path that shares a prefix with it reads none of the prefix again, and one below a prefix held
/// in no spelling reads nothing; and it keeps the folded names of the last directory it listed, so
/// siblings a stat missed list their parent once.
#[derive(Default)]
struct HeldSpellings {
    /// The last path asked.
    last: RelativePath,
    /// What the working tree holds the last path in up to each of its components, as far as it
    /// was resolved: the spelling and whether it differs, `None` from the first held in no
    /// spelling.
    held: Vec<Option<(RelativePath, bool)>>,
    /// The directory last listed for a component a stat missed, as held, with the hashes of the
    /// folded names it holds, sorted.
    listed: Option<(RelativePath, Vec<u64>)>,
}

impl HeldSpellings {
    /// The spelling the working tree holds `path`, not empty, in, with whether it differs from
    /// `path`'s; `None` where some component is held in no spelling.
    async fn of(
        &mut self,
        operation: &InstanceOperationImpl,
        path: &RelativePath,
    ) -> Result<Option<(RelativePath, bool)>, CommitError> {
        let shared = path
            .as_str()
            .split('/')
            .zip(self.last.as_str().split('/'))
            .take_while(|(asked, last)| asked == last)
            .count()
            .min(self.held.len());
        self.held.truncate(shared);
        self.last = path.clone();
        let (mut held, mut varied) = match self.held.last() {
            Some(None) => return Ok(None),
            Some(Some((held, varied))) => (held.clone(), *varied),
            None => (RelativePath::new(), false),
        };
        for component in path.as_str().split('/').skip(shared) {
            let exact = RelativePath::new_from_clean_parts(held.as_str(), component);
            let info = operation
                .file_info(&exact)
                .await
                .forward_with::<CommitError, _>(|| format!("Failed to get metadata for {exact}"))?;
            if info.exists() {
                held = exact;
            } else {
                let Some(variant) = self.variant(operation, &held, component).await? else {
                    self.held.push(None);
                    return Ok(None);
                };
                held = RelativePath::new_from_clean_parts(held.as_str(), &variant);
                varied = true;
            }
            self.held.push(Some((held.clone(), varied)));
        }
        Ok(Some((held, varied)))
    }

    /// A spelling other than `component` that the directory held at `directory` holds a name
    /// folding to it in: answered from its folded names, listed once for the siblings asked one
    /// after another, and its names read by their fold only where it holds one.
    async fn variant(
        &mut self,
        operation: &InstanceOperationImpl,
        directory: &RelativePath,
        component: &str,
    ) -> Result<Option<String>, CommitError> {
        if self
            .listed
            .as_ref()
            .is_none_or(|(listed, _)| listed.as_str() != directory.as_str())
        {
            let mut folded = Vec::new();
            match operation.read_directory(directory).await {
                Ok(mut listing) => {
                    while let Some(entry) = listing.next().await {
                        let entry = entry.forward_with::<CommitError, _>(|| {
                            format!("Failed to list {directory}")
                        })?;
                        folded.push(entry.name_hash);
                    }
                }
                Err(error) if error.is_file_not_found() => {}
                Err(error) => {
                    return Err(error)
                        .forward_with::<CommitError, _>(|| format!("Failed to list {directory}"));
                }
            }
            folded.sort_unstable();
            self.listed = Some((directory.clone(), folded));
        }
        let name_hash = crate::hash::hash_string(component);
        if self
            .listed
            .as_ref()
            .is_none_or(|(_, folded)| folded.binary_search(&name_hash).is_err())
        {
            return Ok(None);
        }
        let names = names_folding(operation, directory, component).await?;
        Ok(names.into_iter().find(|name| name != component))
    }
}

/// Refuses as not found the first of `files`, staged files a stat missed, that the working tree
/// still lists as a file ([`listed_files`]): under another case, on a file system that folds no
/// case, or on Windows one a stat was refused permission for, as such a file is not gone. Each
/// distinct directory is listed once. Boxed by its caller, as only such a file system takes it
/// ([`lists_missed_files`]).
async fn refuse_case_variants(
    operation: &InstanceOperationImpl,
    files: &[(NodeID, RelativePath)],
) -> Result<(), CommitError> {
    let paths: Vec<RelativePath> = files.iter().map(|(_, path)| path.clone()).collect();
    let listed = listed_files(operation, &paths).await?;
    if let Some(index) = listed.iter().position(|&listed| listed) {
        return Err(FileNotFound {
            resource: paths[index].as_str().to_string(),
        }
        .into());
    }
    Ok(())
}

/// Hands `operation` the directories of `handed`, those the walk adds or moves, that the tree still
/// holds once the commit has settled what the working tree no longer holds
/// ([`InstanceOperationImpl::hand_directories`]): one of `leaving`, the sorted nodes that left with
/// a gone directory and the directories that went back to their committed paths, is not handed,
/// as the revision holds nothing at the path the walk spelled for it.
fn hand_directories(
    operation: &InstanceOperationImpl,
    handed: &parking_lot::RwLock<Vec<(NodeID, RelativePath)>>,
    leaving: &[NodeID],
) {
    let handed = std::mem::take(&mut *handed.write());
    if handed.is_empty() {
        return;
    }
    operation.hand_directories(
        handed
            .into_iter()
            .filter(|(node, _)| leaving.binary_search(node).is_err())
            .map(|(_, path)| path)
            .collect(),
    );
}

/// Frees the nodes staged for deletion, `discard`, and the nodes below them a walk collected,
/// `subnodes_to_discard`, each with its flags, recording each deletion in the delta as `base` says
/// ([`unrecorded_deletions`]): a root under the flags it is listed with, and a node below one as a
/// deletion whatever change it is listed with ([`deleted_flags`]). Each root is detached before
/// anything below it is freed (H6).
///
/// Refuses an unresolved conflict among the nodes below them, which no walk visits, named by its
/// working-tree path below one of `walk_roots`, the nodes the walks start at, each with its
/// working-tree path ([`roots_path_for_message`]).
#[allow(clippy::too_many_arguments)]
async fn commit_discard(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    base: DeletionBase,
    walk_roots: &[(RelativePath, NodeID)],
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    discard: &parking_lot::RwLock<Vec<(NodeID, u16)>>,
    subnodes_to_discard: &parking_lot::RwLock<Vec<(NodeID, u16)>>,
    stats: Arc<CommitStats>,
) -> Result<(), CommitError> {
    let (roots, below, unrecorded, records, mut dropped) = {
        let roots = std::mem::take(&mut *discard.write());
        if roots.is_empty() {
            return Ok(());
        }
        let collected = std::mem::take(&mut *subnodes_to_discard.write());

        lore_debug!(
            "Committing deletions of {} nodes and {} below them",
            roots.len(),
            collected.len()
        );

        if let Some(&(node, _)) = collected
            .iter()
            .find(|&&(_, flags)| NodeFlags::from_bits_retain(flags).is_staged_merge_unresolved())
        {
            return Err(Conflict {
                path: roots_path_for_message(&repository, &state, walk_roots, node).await,
            }
            .into());
        }

        let records = !matches!(base, DeletionBase::Unrecorded);
        let unrecorded =
            unrecorded_deletions(&state, &repository, base, &roots, &collected).await?;
        let mut dropped = Dropped::default();
        if records {
            let mut delta = delta.write();
            delta.reserve(collected.len() * size_of::<NodeDelta>());
            for &(node, flags) in &collected {
                if unrecorded.binary_search(&node).is_err() {
                    delta_push(&mut delta, node, deleted_flags(flags));
                } else {
                    dropped.count(flags);
                }
            }
        }
        let below: Vec<NodeID> = collected.into_iter().map(|(node, _)| node).collect();
        (roots, below, unrecorded, records, dropped)
    };

    let counts = state::node_discard_free(&state, &repository, &roots, below, |root, flags| {
        lore_trace!("Committing deletion of node {root}");
        if records {
            if unrecorded.binary_search(&root).is_err() {
                delta_add(delta.clone(), root, flags);
            } else {
                dropped.count(flags);
            }
        }
    })
    .await
    .forward::<CommitError>("Failed discarding nodes staged for delete")?;
    let complete = &stats.complete;
    complete
        .file_count
        .fetch_add(counts.file_count, Ordering::Release);
    complete.file_delete_count.fetch_add(
        counts.file_count.saturating_sub(u64::from(dropped.files)),
        Ordering::Relaxed,
    );
    complete
        .directory_count
        .fetch_add(counts.directory_count, Ordering::Release);
    complete.directory_delete_count.fetch_add(
        counts
            .directory_count
            .saturating_sub(u64::from(dropped.directories)),
        Ordering::Relaxed,
    );
    Ok(())
}

/// The nodes a commit discards that it records no deletion of, as the revision it was built on
/// lacks them ([`unrecorded_deletions`]): an add dropped, which the counts of deletions leave out.
#[derive(Default)]
struct Dropped {
    files: u32,
    directories: u32,
}

impl Dropped {
    /// Counts a node of `flags`, a file or a directory.
    fn count(&mut self, flags: u16) {
        if flags & NodeFlags::File != 0 {
            self.files += 1;
        } else {
            self.directories += 1;
        }
    }
}

/// What a commit records the deletions of the nodes it discards against.
#[derive(Clone, Copy, Debug)]
enum DeletionBase {
    /// The revision the tree was built on, sharing its node IDs: a deletion is recorded where that
    /// revision holds the node.
    Shared(Hash),
    /// None: no deletion is recorded, for a tree with no revision before it, and for a tree built
    /// on another revision than its first parent, whose delta is its diff from the first parent
    /// ([`self_parent_delta`]).
    Unrecorded,
}

impl DeletionBase {
    /// The base of a tree `committed`, the revision it was staged on or loaded from, as `based_on`
    /// says.
    fn of(committed: Hash, based_on: BasedOn) -> Self {
        match based_on {
            BasedOn::FirstParent if !committed.is_zero() => DeletionBase::Shared(committed),
            _ => DeletionBase::Unrecorded,
        }
    }
}

/// The nodes among `roots`, the nodes of `state` a commit staged for deletion, and `collected`,
/// the nodes below them, each with its flags, whose deletion the commit records no entry for,
/// sorted. Every other node records its deletion under its own ID, unless `base` is
/// [`DeletionBase::Unrecorded`].
///
/// [`DeletionBase::Shared`] looks up only a node carrying a staged or dirty mark, in the revision
/// the tree was built on. The tree shares its node IDs with that revision and frees no slot it
/// holds before the commit: it stages the deletion of such a node rather than detaching it. So a
/// node with neither mark is held there as it is here, and a node added since takes a slot that
/// revision holds no live node in, which [`State::base_lacks`] answers lacking.
async fn unrecorded_deletions(
    state: &State,
    repository: &Arc<RepositoryContext>,
    base: DeletionBase,
    roots: &[(NodeID, u16)],
    collected: &[(NodeID, u16)],
) -> Result<Vec<NodeID>, CommitError> {
    match base {
        DeletionBase::Unrecorded => Ok(Vec::new()),
        DeletionBase::Shared(base) => {
            let nodes = roots
                .iter()
                .copied()
                .chain(collected.iter().copied())
                .filter(|&(_, flags)| {
                    flags & (NodeFlags::Staged.bits() | NodeFlags::Dirty.bits()) != 0
                })
                .map(|(node, flags)| (node, NodeFlags::from_bits_retain(flags)));
            state
                .base_lacks(repository, base, nodes)
                .await
                .forward::<CommitError>("Failed reading the tree the revision was built on")
        }
    }
}

async fn collect_file(
    node_id: NodeID,
    relative_path: RelativePath,
    file_tx: &mpsc::Sender<FileToCommit>,
    stats: &CommitStats,
    node_size: u64,
) -> Result<(), CommitError> {
    stats.discovery.total_files.fetch_add(1, Ordering::Relaxed);
    stats
        .discovery
        .total_bytes
        .fetch_add(node_size, Ordering::Relaxed);
    stats.complete.file_total.fetch_add(1, Ordering::Relaxed);
    let Ok(permit) = file_tx.reserve().await else {
        return Err(CommitError::internal("Recursion task failed"));
    };
    permit.send(FileToCommit {
        node_id,
        relative_path,
    });
    Ok(())
}

/// The most tasks a commit's walk or file stage keeps running before it waits for one to finish.
const MAX_CONCURRENT_TASKS: usize = 10000;

/// Folds a finished task of a commit into the first failure seen, answering what it answered where
/// it succeeded.
fn collect_commit_task<T>(
    result: Result<Result<T, CommitError>, tokio::task::JoinError>,
    failure: &mut Option<CommitError>,
) -> Option<T> {
    match result
        .internal("Recursion task failed")
        .map_err(CommitError::from)
        .flatten()
    {
        Ok(answer) => Some(answer),
        Err(err) => {
            failure.get_or_insert(err);
            None
        }
    }
}

/// Waits for a commit's discovery task and the task fragmenting the files it finds, and reports a
/// failure to fragment a file ahead of a failure in discovery. Discovery fails once fragmenting
/// stops taking files, so its error is then a result of the failure to fragment.
#[lore_macro::test_pub]
async fn join_commit_tasks(
    discovery: JoinHandle<Result<(), CommitError>>,
    fragmenting: JoinHandle<Result<(), CommitError>>,
) -> Result<(), CommitError> {
    let (discovery, fragmenting) = tokio::join!(discovery, fragmenting);
    let discovery = discovery
        .internal("Recursion task failed")
        .map_err(CommitError::from)?;
    let fragmenting = fragmenting
        .internal("Recursion task failed")
        .map_err(CommitError::from)?;
    fragmenting?;
    discovery
}

#[allow(clippy::too_many_arguments)]
async fn commit_execute(
    operation: Arc<InstanceOperationImpl>,
    mut file_rx: mpsc::Receiver<FileToCommit>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    gone: Option<Arc<Gone>>,
    stats: Arc<CommitStats>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    modified_times: Arc<RecordedModifiedTimes>,
) -> Result<(), CommitError> {
    let mut tasks = JoinSet::new();
    let mut commit_failure = None;
    let mut found = Vec::new();
    let missing = MissingPaths::of(gone.as_deref());

    while let Some(file_to_commit) = file_rx.recv().await {
        lore_spawn!(
            tasks,
            crate::subtask!(commit_file(
                operation.clone(),
                repository.clone(),
                state.clone(),
                file_to_commit.node_id,
                file_to_commit.relative_path,
                delta.clone(),
                missing,
                stats.clone(),
                tracker.clone(),
                modified_times.clone(),
            ))
        );

        while let Some(result) = tasks.try_join_next() {
            found.extend(collect_commit_task(result, &mut commit_failure).flatten());
        }
        while tasks.len() > MAX_CONCURRENT_TASKS
            && let Some(result) = tasks.join_next().await
        {
            found.extend(collect_commit_task(result, &mut commit_failure).flatten());
        }

        if commit_failure.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        found.extend(collect_commit_task(result, &mut commit_failure).flatten());
    }
    if let Some(gone) = gone.filter(|_| !found.is_empty()) {
        gone.files.write().extend(found);
    }

    if let Some(err) = commit_failure {
        Err(err)
    } else {
        Ok(())
    }
}

/// What the working tree holds at `path`, read before its content rather than after.
///
/// A file written to while it is being fragmented then yields a time older than the content
/// that was committed, costing that one file a hash on the next scan. A time read afterwards
/// would describe content that was never committed, and the next scan would trust it.
///
/// `None` where the path holds nothing or a directory, which the commit takes alike
/// ([`gone_file`]), ahead of the read that would fail on it.
async fn info_before_fragmenting(
    operation: &InstanceOperationImpl,
    path: &RelativePath,
) -> Result<Option<FileInfo>, CommitError> {
    let info = operation
        .file_info(path)
        .await
        .forward_with::<CommitError, _>(|| format!("Failed to get metadata for file {path}"))?;
    Ok((info.exists() && !info.is_dir()).then_some(info))
}

/// What [`commit_file`] answers for the staged file `node` the working tree no longer holds at
/// `path`, nothing or a directory standing there: the path, for [`commit_gone`], which refuses one
/// the working tree still lists ([`refuse_case_variants`]), where `missing` settles such paths, and
/// not found elsewhere.
fn gone_file(
    node: NodeID,
    path: RelativePath,
    missing: MissingPaths,
) -> Result<Option<(NodeID, RelativePath)>, CommitError> {
    if missing == MissingPaths::Refuse {
        return Err(FileNotFound {
            resource: path.as_str().to_string(),
        }
        .into());
    }
    lore_trace!("Staged file gone from the working tree: {path}");
    Ok(Some((node, path)))
}

/// Whether a commit takes the content of the staged file `node` at `path` from the node rather
/// than from the working tree: where a merge staged the file and the view excludes the path,
/// unless a forced stage took the file from the working tree since. A sparse working tree holds no
/// file at such a path, or a stale one, so the commit neither reads it nor looks for conflict
/// markers in it.
///
/// A forced stage of such a file resolves the merge with the working tree's file: it drops the
/// node's content hash, taking the working tree's size, keeping the file ID and the merge bits
/// ([`crate::stage::stage_node_from_metadata`]). A zero hash with a nonzero size is content not
/// yet stored, the state a newly staged add has, which the commit reads from the working tree. A
/// zero hash with size 0 is an empty file, taken from the staged node as it stands: a merge stages
/// one that way, and so does a forced stage of an empty working-tree file.
pub(crate) fn takes_staged_content(
    repository: &RepositoryContext,
    path: &RelativePath,
    node: &Node,
) -> bool {
    !(node.address.is_zero() && node.size != 0) && merged_past_the_view(repository, path, node)
}

/// Whether a merge staged the file `node` at `path` and the view excludes the path, whatever
/// content the node holds.
///
/// The view test must stay identical to the one realize gates disk writes on (`fs/realize.rs`),
/// because that is what decided whether the file was written.
pub(crate) fn merged_past_the_view(
    repository: &RepositoryContext,
    path: &RelativePath,
    node: &Node,
) -> bool {
    node.is_staged_merge()
        && repository
            .filter
            .excludes_tree(path, false, FilterMode::View)
}

/// Commits one file node, recording the modified time it read the file at and telling the
/// operation what it stored ([`InstanceOperation::stored`]) and that the revision now holds what
/// the working tree holds at the path ([`InstanceOperationImpl::committed_path`]). A file whose
/// content is taken from the staged node ([`takes_staged_content`]) records and tells nothing. A
/// file the working tree no longer holds at its spelling, a directory standing there or nothing,
/// is answered as gone ([`gone_file`]), for [`commit_gone`], which refuses one held under another
/// case ([`refuse_case_variants`]).
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn commit_file(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    relative_path: RelativePath,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    missing: MissingPaths,
    stats: Arc<CommitStats>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    modified_times: Arc<RecordedModifiedTimes>,
) -> impl Future<Output = Result<Option<(NodeID, RelativePath)>, CommitError>> {
    async move {
        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
        let mut node = { *block.read().node(node_index) };

        debug_assert!(node.is_file());

        let staged_content = takes_staged_content(&repository, &relative_path, &node);

        if node.is_staged_merge_conflict() {
            reject_unresolved_conflict(&node, relative_path.as_str())?;
            if !staged_content
                && operation
                    .infer_is_conflicted(&relative_path)
                    .await
                    .forward_with::<CommitError, _>(|| {
                        format!("checking {relative_path} for conflict markers")
                    })?
                    == ConflictMarkers::Present
            {
                return Err(Conflict {
                    path: relative_path.as_str().to_string(),
                }
                .into());
            }
            if !execution_context().globals().dry_run() {
                sync::unlink_merge_artifacts(&operation, &relative_path).await;
            }
        }

        lore_trace!(
            "Committing file node {} {} flags 0x{:x}",
            node_id,
            relative_path.as_str(),
            node.flags
        );

        let (address, content_size, mode, modified_time) = if staged_content {
            (node.address, node.size, node.mode, None)
        } else {
            if node.address.context.is_zero() {
                // TODO(mjansson): Optionally find previous identical file content and deduplicate by using same context
                node.address.context = uuid::Uuid::now_v7().into();
                lore_trace!(
                    "Generate file ID for file: {} {}",
                    relative_path.as_str(),
                    node.address.context
                );
            }

            let Some(info) = info_before_fragmenting(&operation, &relative_path).await? else {
                return gone_file(node_id, relative_path, missing);
            };

            let source = operation
                .content_source(&relative_path)
                .await
                .forward_with::<CommitError, _>(|| {
                    format!("naming the content of {relative_path}")
                })?;
            // Content a virtual filesystem serves from the immutable store is stored already,
            // under the address it names, so it is taken as it is rather than read back.
            let (address, size_content) =
                if let Some(stored) = immutable::virtual_immutable(&source) {
                    stored
                } else {
                    let written = immutable::write_from_file_with_tracker(
                        repository.clone(),
                        &source,
                        node.address.context,
                        immutable::write_options_from_repository(repository.clone()),
                        Some(tracker),
                    )
                    .await
                    .forward_with::<CommitError, _>(|| {
                        format!(
                            "Failed writing file {} to immutable store",
                            relative_path.as_str()
                        )
                    })?;
                    stats.file_read(written.1);
                    written
                };
            let mode = info.mode(node.mode);
            operation.stored(&relative_path, address, size_content, mode, info.mtime());
            operation.committed_path(&relative_path);

            (address, size_content, mode, Some(info.mtime()))
        };

        let modified = util::fs::mode_changed(node.mode, mode)
            || node.size != content_size
            || node.address.hash != address.hash;

        if modified
            || node.is_staged_add()
            || node.is_staged_move()
            || node.is_staged_copy()
            || node.is_staged_merge()
        {
            lore_trace!(
                "Committed modified file node {} {} with address {} (was {}) mode 0o{:o} (was 0o{:o}) flags {:x}",
                node_id,
                relative_path.as_str(),
                address,
                node.address.hash,
                mode,
                node.mode,
                node.flags
            );
            stats
                .complete
                .file_modify_count
                .fetch_add(1, Ordering::Relaxed);
            stats.record_file_action(node.flags, content_size);

            let flags = node.flags;
            delta_add(delta, node_id, flags);

            state.node_change_owned(&block, node_id, |held| {
                if !held.clear_change_flags() {
                    return false;
                }
                held.overwrite_content(address, content_size, mode);
                held.clear_legacy_mtime();
                true
            });
        } else if node.is_staged() {
            lore_trace!("Reset staged flag on node {} {}", node_id, relative_path);
            state.node_clear_own(&block, node_id, OwnFlags::Change);
        }

        if let Some(modified_time) = modified_time {
            modified_times.record(&repository, &relative_path, modified_time);
        }

        stats.complete.file_count.fetch_add(1, Ordering::Release);

        Ok(None)
    }
}

/// Refuses the commit of the link `node_id`, at `relative_path`, whose linked tree holds a staged
/// change, inside a directory staged as moved below `walk_root` that went back: the working tree
/// holds a directory at its committed path and none at its staged one, so
/// [`return_moved_directories`] takes it back. The link's commit runs in the walk, before
/// [`commit_gone`]'s climb, and would read the linked tree at the staged path. One
/// file-information read of each such directory's two paths, side by side.
async fn refuse_link_moved_back(
    operation: &InstanceOperationImpl,
    repository: &Arc<RepositoryContext>,
    state: &State,
    (walk_root, node_id): (NodeID, NodeID),
    relative_path: &RelativePath,
) -> Result<(), CommitError> {
    let mut moved: Vec<(NodeID, Node, usize)> = Vec::new();
    let mut path = relative_path.as_str();
    let mut at = state
        .node(repository.clone(), node_id)
        .await
        .forward::<CommitError>("Failed reading a link node")?
        .parent;
    while at != walk_root && at != ROOT_NODE {
        path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let record = state
            .node(repository.clone(), at)
            .await
            .forward::<CommitError>("Failed reading a directory above a link")?;
        if record.is_staged_move() && !record.is_staged_add() {
            moved.push((at, record, path.len()));
        }
        at = record.parent;
    }
    if moved.is_empty() {
        return Ok(());
    }
    let root_path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
    let base = state.committed_revision();
    let mut name = String::new();
    let mut paths = Vec::with_capacity(moved.len() * 2);
    for (directory, record, length) in moved {
        if let Some((_, source)) = moved_directory_source(
            repository,
            state,
            (base, (root_path, walk_root)),
            (directory, &record),
            &mut name,
        )
        .await?
        {
            paths.push(RelativePath::new_from_clean_parts(
                &relative_path.as_str()[..length],
                "",
            ));
            paths.push(source);
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let held = directories_held(operation, paths.into()).await?;
    if held
        .as_chunks::<2>()
        .0
        .iter()
        .any(|&[staged, committed]| !staged && committed)
    {
        return Err(CommitError::internal(
            "a staged link lies inside a directory moved back to its committed path; stage the \
             move back or unstage it",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn commit_link_node(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    ((walk_roots, walk_root), node_id): ((WalkRoots, NodeID), NodeID),
    relative_path: RelativePath,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    stats: Arc<CommitStats>,
    parent_branch: BranchId,
    parent_tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    (missing, replaced): (
        MissingPaths,
        Option<Arc<parking_lot::Mutex<Vec<RelativePath>>>>,
    ),
    committed_links: CommittedLinks,
    link_commits: Arc<LinkCommits>,
) -> Result<(), CommitError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    let node = block.node(node_index);

    let link = node.linked_node();
    let (link_repository, link_state) = link
        .open(&repository)
        .await
        .forward::<CommitError>("Failed to deserialize link state")?;

    // An added link pinned at a committed revision has nothing inside it to commit; one pinned at
    // an uncommitted state, as dirtying or staging inside it leaves, goes through `commit_link`.
    if node.is_staged_add() && link_state.revision_number() != 0 {
        lore_trace!("Reset staged add flag on link node {relative_path}");

        delta_add(delta, node_id, node.flags);
        state.node_clear_own(&block, node_id, OwnFlags::Change);
        return Ok(());
    }

    if missing == MissingPaths::Settle
        && link_state.revision_number() == 0
        && first_staged_node(&link_repository, &link_state, |_| true)
            .await?
            .is_some()
    {
        Box::pin(refuse_link_moved_back(
            &operation,
            &repository,
            &state,
            (walk_root, node_id),
            &relative_path,
        ))
        .await?;
    }

    // Link, recurse and check if there are changes inside the link to commit, and if so update the
    // target repository hash and potentially target node index in this node data

    let link_reference = state
        .link_find(repository.clone(), link.repository, node_id)
        .await
        .forward::<CommitError>("Commit of link node failed")?;

    let branch_id = link_reference.resolve_branch(parent_branch);
    let built_on = link_state.committed_revision();
    let committed = link_commits.revision(link.repository, branch_id, node.address.hash);
    // Boxed, so a link whose revision another link commits holds none of the commit.
    let link_latest = *committed
        .get_or_try_init(|| {
            Box::pin(async {
                let based_on =
                    link_based_on(&repository, &state, &link_reference, &link_state).await?;
                let roots = mounts_pinning(
                    (&repository, &state),
                    &walk_roots,
                    &node,
                    (node_id, relative_path.clone()),
                )
                .await?;
                let link_metadata =
                    if let Some(link_msg) = link_messages.get(relative_path.as_str()) {
                        let mut overridden = (*metadata).clone();
                        overridden
                            .set_string(metadata::MESSAGE, link_msg)
                            .forward::<CommitError>("Failed to set metadata")?;
                        Arc::new(overridden)
                    } else {
                        metadata.clone()
                    };
                let (link_latest, link_node_id) = commit_link(
                    operation,
                    link_repository.clone(),
                    token,
                    link_state,
                    roots,
                    branch_id,
                    based_on,
                    link_metadata,
                    link_messages,
                    stats,
                    parent_tracker,
                    (missing, replaced),
                    link_commits.clone(),
                )
                .await?;

                if link_latest.is_zero() {
                    lore_error!("Link head is zero");
                    return Err(CommitError::internal("Commit of link node failed"));
                }

                lore_debug!(
                    "Link node revision was {} nodeId {}, new revision {} nodeId {}",
                    node.address.hash,
                    node.child,
                    link_latest,
                    link_node_id
                );

                if node.address.hash == link_latest {
                    lore_debug!("Link hash is unchanged");
                }

                // TODO(vri): Implement support for changed link nodeId
                if node.child != link_node_id {
                    lore_error!("Link nodeId has changed, which is not supported yet");
                    return Err(CommitError::internal("Commit of link node failed"));
                }
                Ok(link_latest)
            })
        })
        .await?;

    delta_add(delta, node_id, node.flags);

    // Clear staged bits before updating the link pin
    state.node_change_owned(&block, node_id, |held| {
        held.clear_staged_and_action_bits();
        true
    });

    link::update_link_pin_by_node(
        &state,
        repository,
        link.repository,
        link_reference.branch,
        link_latest,
        node_id,
    )
    .await
    .forward::<CommitError>("Commit of link node failed")?;

    committed_links.lock().push(CommittedLink {
        repository: link.repository,
        branch: link_reference.branch,
        signature: link_latest,
        made: link_latest != built_on,
    });

    Ok(())
}

/// The revision the tree of the linked state `link_state` was built from. A merge of the link's
/// divergent history is staged on the pin the link held before the merge, which the parent `state`
/// keeps in its link merge state, and takes that pin as its second parent.
async fn link_based_on(
    repository: &Arc<RepositoryContext>,
    state: &State,
    link_reference: &LinkReference,
    link_state: &State,
) -> Result<BasedOn, CommitError> {
    if !link_state.is_merge() {
        return Ok(BasedOn::FirstParent);
    }
    let pinned = state
        .deserialize_link_merge_state(repository.clone())
        .await
        .forward::<CommitError>("Failed reading link merge state")?
        .into_iter()
        .find(|entry| !entry.is_conflict() && entry.local_node == link_reference.local_node)
        .map(|entry| entry.base.signature);
    Ok(if pinned == Some(link_state.parent_other()) {
        BasedOn::Other
    } else {
        BasedOn::FirstParent
    })
}

/// Adds to `replaced` the path from the instance's root of each of `deleted`, the nodes of the link
/// state `state` whose deletion its prune staged, below one of `mounts`, each a path the instance
/// mounts the linked node at and that node: the paths the instance's dirty-only walk spells for
/// them. Boxed by its caller, which takes it only where the prune staged one.
async fn link_replaced_paths(
    repository: &Arc<RepositoryContext>,
    state: &State,
    mounts: &[(RelativePath, NodeID)],
    deleted: &[NodeID],
    replaced: &parking_lot::Mutex<Vec<RelativePath>>,
) -> Result<(), CommitError> {
    for &node in deleted {
        for (mount, link_node) in mounts {
            let below = state
                .node_path_below(repository.clone(), node, *link_node)
                .await
                .forward::<CommitError>(
                    "Failed to read the path of a deletion a link's prune staged",
                )?;
            if let Some(below) = below {
                replaced.lock().push(mount.join(below.as_str()));
                break;
            }
        }
    }
    Ok(())
}

/// The roots a walk of each repository `chain` crosses starts at, outer to inner: the instance's
/// own root, then for each level every mount of its linked repository pinning the revision the
/// level's link pins ([`mounts_pinning`]), each with its working-tree path.
async fn chain_holder_roots(
    chain: &link::ResolvedLinkChain,
) -> Result<Vec<Vec<(RelativePath, NodeID)>>, CommitError> {
    let mut roots = Vec::with_capacity(chain.levels.len() + 1);
    roots.push(vec![(RelativePath::new(), ROOT_NODE)]);
    for level in &chain.levels {
        let link = level
            .state
            .node(level.repository.clone(), level.link_node_id)
            .await
            .forward::<CommitError>("Failed to read a link holding the link")?;
        let holder = mounts_pinning(
            (&level.repository, &level.state),
            &roots[roots.len() - 1],
            &link,
            (level.link_node_id, level.mount.clone()),
        )
        .await?;
        roots.push(holder);
    }
    Ok(roots)
}

/// The mounts in `state` of the linked repository the link node `link` pins, each with its
/// working-tree path and the node it mounts: `first`, a link node and its path, then each other
/// mount pinning the same revision that a walk from one of `walk_roots`, a node and its
/// working-tree path, materializes. They hold one revision of the repository, so its commit walks
/// every one of them.
async fn mounts_pinning(
    (repository, state): (&Arc<RepositoryContext>, &State),
    walk_roots: &[(RelativePath, NodeID)],
    link: &Node,
    (first, first_path): (NodeID, RelativePath),
) -> Result<Vec<(RelativePath, NodeID)>, CommitError> {
    let linked = link.linked_node();
    let mut roots = vec![(first_path, linked.node)];
    let rows = state
        .link_list(repository.clone())
        .await
        .forward::<CommitError>("Failed reading the link registry")?;
    for row in rows
        .iter()
        .filter(|row| row.repository() == linked.repository && row.local_node() != first)
    {
        let mount = state
            .node(repository.clone(), row.local_node())
            .await
            .forward::<CommitError>("Failed reading a link node")?;
        if mount.address.hash != link.address.hash {
            continue;
        }
        for (walk_path, walk_root) in walk_roots {
            if let Some(below) = state
                .node_path_below(repository.clone(), row.local_node(), *walk_root)
                .await
                .forward::<CommitError>("Failed resolving the path of a mount")?
            {
                roots.push((walk_path.join(below.as_str()), mount.child));
                break;
            }
        }
    }
    Ok(roots)
}

/// The revision a commit of `state`, a linked repository's staged state built as `based_on` says,
/// commits over: the revision it was staged on, or, for a merge of the link's divergent history,
/// the pin it merged into, its second parent.
fn link_branch_base(state: &State, based_on: BasedOn) -> Hash {
    match based_on {
        BasedOn::FirstParent => state.committed_revision(),
        BasedOn::Other => state.parent_other(),
    }
}

/// The latest of `branch` a commit of `state`, a linked repository's staged state, replaces,
/// checked before anything is written: a branch moved on from the revision the state was staged
/// on since the parent pinned it fails the commit as advanced. A merge holds either of its
/// parents, so either may be the latest. A branch with no latest here, and a forced commit, take
/// the latest as it is. The refusal names `mount`, a working-tree path of the link, and the merge
/// that brings the newer revisions in.
async fn latest_to_replace(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    state: &State,
    mount: &RelativePath,
) -> Result<Hash, CommitError> {
    let latest = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();
    let holds = if state.is_merge() {
        latest == state.parent_self() || latest == state.parent_other()
    } else {
        latest == state.committed_revision()
    };
    if !latest.is_zero() && !holds && !execution_context().globals().force() {
        let mount = link::display_path(mount.as_str());
        let mut advanced = CommitError::from(BranchAdvanced);
        advanced.push_context(format!(
            "the branch of the link at {mount} has newer revisions; merge them in with \
             `lore branch merge start <branch> --link {mount}`"
        ));
        return Err(advanced);
    }
    Ok(latest)
}

#[allow(clippy::too_many_arguments)]
async fn commit_link(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    roots: Vec<(RelativePath, NodeID)>,
    branch: BranchId,
    based_on: BasedOn,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    stats: Arc<CommitStats>,
    parent_tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    (missing, replaced): (
        MissingPaths,
        Option<Arc<parking_lot::Mutex<Vec<RelativePath>>>>,
    ),
    link_commits: Arc<LinkCommits>,
) -> Result<(Hash, NodeID), CommitError> {
    // Per-link tracker: a linked sub-repo is a logically independent commit
    // and must carry its own durability gate. If we reused the parent's
    // tracker, the sub-repo's RevisionCommitRevision would fire before the
    // parent drained — RevisionCommitRevision must only be emitted after
    // tracker.await_all succeeds AND the branch pointer is updated.
    let link_tracker = Arc::new(parent_tracker.new_like());

    let Some(&(_, node_id)) = roots.first() else {
        return Err(CommitError::internal("committing a link with no mount"));
    };
    lore_debug!(
        "Committing linked repository {} from {node_id}",
        repository.id
    );

    let base = state.committed_revision();
    let pruned = prune_dirty_for_commit(state.clone(), repository.clone(), base).await?;
    if let Some(replaced) = replaced.as_ref().filter(|_| !pruned.deleted.is_empty()) {
        Box::pin(link_replaced_paths(
            &repository,
            &state,
            &roots,
            &pruned.deleted,
            replaced,
        ))
        .await?;
    }
    let gone = (missing == MissingPaths::Settle).then(|| {
        Arc::new(Gone {
            replaced,
            ..Gone::left_out(Vec::new(), &pruned)
        })
    });
    let link_modified_times = Arc::new(RecordedModifiedTimes::below(
        operation.clock_floor(&repository).await,
    ));

    // A committed state, as a pin `lore link update` moved leaves, commits nothing, so its branch
    // may have moved on.
    let link_previous = if state.revision_number() == 0 {
        latest_to_replace(&repository, branch, &state, &roots[0].0).await?
    } else {
        branch::load_latest(repository.clone(), branch)
            .await
            .unwrap_or_default()
    };
    let link_divergent = branch::load_latest_divergent(repository.clone(), branch)
        .await
        .unwrap_or_default();

    // Graceful-drain pattern mirroring commit_staged_revision: run all
    // write-producing work under `work_tracker`, then ALWAYS drain so leaders
    // never outlive this function. Only on drain success AND branch-pointer
    // success do we emit the event. `Ok(None)` signals the "unchanged link"
    // case — no new revision, no event.
    let work_tracker = link_tracker.clone();
    let work_result: Result<Option<Hash>, CommitError> = {
        let moves = link_commits.clone();
        let repository = repository.clone();
        let state = state.clone();
        let metadata = metadata.clone();
        let link_messages = link_messages.clone();
        let stats = stats.clone();
        let link_modified_times = link_modified_times.clone();
        async move {
            // A linked sub-repo becomes its own `parent_branch` for everything
            // it recurses into — nested operations must anchor against the
            // link's branch, not the outer repo's. Per-link messages are keyed
            // by full mount path, so a nested link (e.g. `vendor/b/vendor/c`)
            // still receives its `--link-message`; it falls back to the main
            // message when no entry matches.
            let (walk, file_rx) = CommitWalk::new(
                false,
                operation.clone(),
                repository.clone(),
                token.share(),
                state.clone(),
                metadata.clone(),
                link_messages,
                stats,
                branch,
                work_tracker.clone(),
                gone,
            );
            let committed_links = walk.committed_links.clone();
            let delta = commit_walks(
                CommitWalk {
                    link_commits,
                    ..walk
                },
                file_rx,
                &roots,
                based_on,
                link_modified_times,
                false,
            )
            .await?;
            if !committed_links.lock().is_empty() {
                Box::pin(repin_committed_links(
                    &operation,
                    (&repository, &state),
                    &roots,
                    &committed_links,
                    Some(&delta),
                ))
                .await?;
            }

            // A merge commits as a revision of its own even where it changed nothing, so that
            // the merged branch is recorded as its second parent.
            if delta.read().count::<NodeDelta>() == 0 && !state.is_merge() {
                lore_debug!("Link is unchanged, skipping rehash");
                return Ok(None);
            }

            let StagedChanges {
                delta: staged,
                first_parent,
            } = rehash_and_record_delta(
                repository.clone(),
                state.clone(),
                based_on,
                delta,
                work_tracker.clone(),
            )
            .await?;
            let changes = staged.to_aligned::<NodeDelta>();

            state.set_metadata_hash(
                metadata
                    .serialize_with_tracker(repository.clone(), Some(work_tracker))
                    .await
                    .forward::<CommitError>("Failed to write commit metadata")?,
            );

            weave_changes(
                repository.clone(),
                state.clone(),
                based_on,
                changes.as_type_slice::<NodeDelta>(),
                first_parent,
            )
            .await?;

            let signature = state
                .serialize_committed(repository.clone(), &token)
                .await
                .forward::<CommitError>("Failed to serialize revision state")?;

            if !execution_context().globals().dry_run() {
                branch::store_latest(
                    repository.clone(),
                    branch,
                    link_previous,
                    signature,
                    BranchLatestStatus::Divergent,
                )
                .await
                .forward::<CommitError>("Failed to store current branch latest")?;
                moves.record(MovedLinkBranch {
                    repository: repository.clone(),
                    branch,
                    previous: link_previous,
                    signature,
                    divergent: link_divergent,
                });
            }

            Ok(Some(signature))
        }
        .await
    };

    // ALWAYS drain. Dropping the tracker with live leaders would abort them
    // via JoinSet::drop; we let them terminate first so callers never observe
    // a half-committed link state.
    let drain_result = link_tracker.await_all().await;

    let signature_opt = match work_result {
        Ok(sig_opt) => {
            // Work succeeded; drain errors now take priority — they mean a
            // fragment the work path already acknowledged failed to land.
            drain_result
                .forward::<CommitError>("Background fragment upload task failed during commit")?;
            sig_opt
        }
        Err(e) => return Err(e),
    };

    // Unchanged link: no new revision, no event to emit. Return early with the committed revision
    // the pinned state is based on, which is the pin itself unless the state holds dirty markers
    // alone.
    let signature = match signature_opt {
        Some(sig) => sig,
        None => return Ok((state.committed_revision(), node_id)),
    };

    // For linked repos, RevisionCommitRevision emission is strictly after
    // (a) the per-link tracker drain succeeded and (b) branch::store_latest
    // succeeded — both gated by the `?` above.
    event::LoreEvent::RevisionCommitRevision(LoreRevisionCommitRevisionEventData {
        repository: repository.id,
        branch,
        revision: signature,
        revision_number: state.revision_number(),
        parent: state.parent_self(),
        parent_other: state.parent_other(),
    })
    .send();

    link_modified_times.store(repository).await;

    Ok((signature, node_id))
}

/// Rehash the directory `node_id`, when it is the root or staged, and every staged directory below
/// it, clearing their staged flags.
///
/// A directory's address is the keyed hash of every child in the order of their name hashes
/// ([`state::directory_hash`]): for each, its [`state::NodeHashData`], then the length and the
/// bytes of the name it stores, unchecked against the name rules. A rename that changes only case
/// therefore changes the address. The names are read from the name table of each block holding a
/// child of a rehashed directory.
pub async fn rehash_directory(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> Result<(), CommitError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    let node = block.node(node_index);

    if node_id != ROOT_NODE && !node.is_staged() {
        return Ok(());
    }

    lore_trace!("Rehash directory node {} address {}", node_id, node.address);
    debug_assert!(node.is_directory());

    let mut child_data: Vec<state::ChildHashData> = vec![];
    let mut names: Vec<u8> = vec![];

    let mut total_size = 0;
    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;

    let mut tasks = JoinSet::new();
    let mut walk_failure = None;

    loop {
        let (child_node_id, child_node) = match children
            .next_with_name_bytes(&mut names)
            .await
            .forward::<CommitError>(
            "Failed deserializing state block",
        ) {
            Ok(Some(child)) => child,
            Ok(None) => break,
            Err(err) => {
                walk_failure = Some(err);
                break;
            }
        };
        let Ok(name_end) = u32::try_from(names.len()) else {
            walk_failure = Some(CommitError::internal(
                "Names of a directory's children exceed 4 GiB",
            ));
            break;
        };

        if child_node.is_staged_delete() {
            let node_path = path_for_message(&repository, &state, child_node_id).await;
            lore_warn!(
                "Encountered deleted node {child_node_id} when rehashing directory node {node_id}: {node_path}"
            );
            walk_failure = Some(CommitError::internal(
                "Deleted node remains after nodes were committed",
            ));
            break;
        }

        if child_node.is_dirty() {
            lore_debug!(
                "Node {} {} dirty flags not cleared 0x{:x} address {}",
                child_node_id,
                path_for_message(&repository, &state, child_node_id).await,
                child_node.flags,
                child_node.address
            );
            walk_failure = Some(CommitError::internal(
                "Dirty node remain after nodes were committed",
            ));
            break;
        }

        let child = state::ChildHashData {
            data: state::NodeHashData::from_node(&child_node),
            name: name_end - child_node.name_length..name_end,
        };

        if child_node.is_directory() && child_node.is_staged() {
            lore_trace!("Spawning rehash of directory node {}", child_node_id);
            let index = child_data.len() as u32;
            child_data.push(child);
            lore_spawn!(
                tasks,
                crate::subtask!(rehash_directory_recurse(
                    repository.clone(),
                    state.clone(),
                    child_node_id,
                    index
                ))
            );
        } else {
            if child_node.is_staged() {
                lore_debug!(
                    "Node {} {} staged flags not cleared 0x{:x} address {}",
                    child_node_id,
                    path_for_message(&repository, &state, child_node_id).await,
                    child_node.flags,
                    child_node.address
                );
                walk_failure = Some(CommitError::internal(
                    "Staged node remain after nodes were committed",
                ));
                break;
            } else if child_node.is_link() {
                // Links are handled explicitly before hashing directories
                lore_debug!(
                    "Rehashing encountered link node {child_node_id}, address {}",
                    child_node.address.hash
                );
            } else if child_node.is_file()
                && child_node.address.hash.is_zero()
                && child_node.size > 0
            {
                walk_failure = Some(CommitError::internal(
                    "Node with zero hash remain after nodes were committed",
                ));
                break;
            }

            lore_trace!(
                "Child node {} mode 0o{:o} size {} flags 0x{:x} address {}",
                child_node_id,
                child_node.mode,
                child_node.size,
                child_node.flags,
                child_node.address
            );

            total_size += child_node.size;

            child_data.push(child);
        }
    }

    let mut task_failure = Ok(());
    while let Some(task) = tasks.join_next().await {
        match task {
            Ok(Ok((index, result))) => {
                total_size += result.size();
                child_data[index as usize].data = result;
            }
            Ok(Err(err)) => {
                if walk_failure.is_none() {
                    walk_failure = Some(err);
                }
            }
            Err(join_error) => task_failure = Err(join_error),
        }
    }
    if let Some(err) = walk_failure {
        return Err(err);
    }
    task_failure.internal("Recursion task failed")?;

    let directory_hash = state::directory_hash(&mut child_data, &names);

    state.node_change_owned(&block, node_id, |held| {
        let prev_hash = held.address.hash;
        held.set_hash_and_size(directory_hash, total_size);
        held.clear_staged_and_action_bits();

        lore_trace!(
            "Node {} rehashed, mode 0o{:o} size {} flags 0x{:x} hash {} (previous {})",
            node_id,
            held.mode,
            held.size,
            held.flags,
            held.address.hash,
            prev_hash
        );
        true
    });

    Ok(())
}

/// What the rehash of a subdirectory returns: its index among the children of the directory
/// being rehashed, and what it contributes to that directory's address.
type ChildRehash = Result<(u32, state::NodeHashData), CommitError>;

/// Rehash the directory `node_id`, the child at `index` of the directory being rehashed, and
/// return what it contributes to that directory's address with the index.
fn rehash_directory_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    index: u32,
) -> Pin<Box<dyn Future<Output = ChildRehash> + Send>> {
    Box::pin(async move {
        rehash_directory(repository.clone(), state.clone(), node_id).await?;

        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
        let node = block.node(node_index);

        Ok((index, state::NodeHashData::from_node(&node)))
    })
}

/// The path of `node_id` for a message, or an empty one where it cannot be read. The read is boxed
/// so a rehash's future holds only the box while it runs, on paths that report a failure.
async fn path_for_message(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node_id: NodeID,
) -> String {
    Box::pin(state.node_path(repository.clone(), node_id))
        .await
        .unwrap_or_default()
}

/// [`walk_path_for_message`] from the first of `walk_roots`, each a node a commit walk starts at and
/// its working-tree path, that holds `node_id`, else from the first of them, or from the root
/// where there are none.
async fn roots_path_for_message(
    repository: &Arc<RepositoryContext>,
    state: &State,
    walk_roots: &[(RelativePath, NodeID)],
    node_id: NodeID,
) -> String {
    let root = RelativePath::new();
    let mut walk_root = walk_roots
        .first()
        .map_or((&root, ROOT_NODE), |(path, node)| (path, *node));
    for (path, node) in walk_roots {
        if let Ok(Some(_)) = state
            .node_path_below(repository.clone(), node_id, *node)
            .await
        {
            walk_root = (path, *node);
            break;
        }
    }
    walk_path_for_message(repository, state, walk_root, node_id).await
}

/// The working-tree path of `node_id` for a message, below `walk_root`, the node a commit walk
/// starts at and its working-tree path: the tree of a link or a layer holds its paths from its own
/// root. A node outside the walk's root, which a link mounting a subtree has, has no working-tree
/// path, and is named by its path in the linked tree.
async fn walk_path_for_message(
    repository: &Arc<RepositoryContext>,
    state: &State,
    walk_root: (&RelativePath, NodeID),
    node_id: NodeID,
) -> String {
    let (root_path, root) = walk_root;
    let path = path_for_message(repository, state, node_id).await;
    let root_in_tree = if root == ROOT_NODE {
        String::new()
    } else {
        path_for_message(repository, state, root).await
    };
    let below = match path.strip_prefix(root_in_tree.as_str()) {
        Some(below) if root_in_tree.is_empty() || below.is_empty() || below.starts_with('/') => {
            below.trim_start_matches('/')
        }
        _ if root_path.is_empty() => return format!("{path} (outside /{root_in_tree})"),
        _ => return format!("{root_path}: {path} (outside /{root_in_tree})"),
    };
    match (root_path.is_empty(), below.is_empty()) {
        (true, _) => below.to_string(),
        (false, true) => root_path.as_str().to_string(),
        (false, false) => format!("{root_path}/{below}"),
    }
}

fn delta_add(delta: Arc<parking_lot::RwLock<BytesMut>>, node_id: NodeID, flags: u16) {
    delta_push(&mut delta.write(), node_id, flags);
}

/// Appends to `delta` the entry of node `node_id`, staged with `flags`.
fn delta_push(delta: &mut BytesMut, node_id: NodeID, flags: u16) {
    let node_delta = NodeDelta::from_node_and_flags(node_id, flags);
    lore_trace!("Record node delta {node_delta:?}");
    delta.extend_from_slice(node_delta.as_bytes());
}

/// Keeps in `delta` only the entries of the nodes `keeps` answers true for, in their order.
fn delta_retain(delta: &mut BytesMut, keeps: impl Fn(NodeID) -> bool) {
    let size = size_of::<NodeDelta>();
    let mut kept = 0;
    let mut start = 0;
    while let Ok((entry, _)) = NodeDelta::read_from_prefix(&delta[start..]) {
        if keeps(entry.node) {
            delta.copy_within(start..start + size, kept);
            kept += size;
        }
        start += size;
    }
    delta.truncate(kept);
}

/// Writes `delta` as the node delta of `state`, through `tracker` where given.
async fn generate_delta_block(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    delta: Bytes,
    tracker: Option<Arc<lore_storage::write_tracker::WriteTracker>>,
) -> Result<(), CommitError> {
    let delta_count = delta.count::<NodeDelta>();

    let address = if !delta.is_empty() {
        immutable::write_with_tracker(
            repository.clone(),
            Context::default(),
            delta,
            immutable::write_options_from_repository(repository.clone())
                .with_local_cache_priority()
                .with_max_size_chunk(),
            tracker,
        )
        .await
        .forward::<CommitError>("Failed writing delta block to immutable store")?
    } else {
        Address::default()
    };

    state
        .set_delta_block(address.hash, delta_count)
        .forward::<CommitError>("Failed setting delta block in state")?;

    if delta_count > 0 {
        lore_debug!("Generated delta block with {delta_count} nodes");
    }

    Ok(())
}

/// The revision the tree of a revision was built from, which the tree shares node IDs with.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BasedOn {
    /// The first parent, as for a revision committed on top of it.
    FirstParent,
    /// Another revision than the first parent: the second parent of a merge reconciling divergent
    /// history, which is staged on the current revision and takes it as the second parent, or the
    /// revision a stage was staged on before a commit moved it onto another, as a forced commit of
    /// a stale stage and a commit of a link do. The nodes the first parent added or changed since
    /// can have other IDs in the tree.
    Other,
}

/// Records in the file metadata of `state` what its parents changed. A node the first parent's
/// own delta changed records the first parent as its last change on the first-parent line. A
/// node the merge changed records the second parent's history of it, and a node the second
/// parent's own delta changed records the second parent on the second-parent line. Every other
/// node keeps the history of the tree `state` was built from.
///
/// `changes` are the changes staged on the tree of `state`, in its node IDs, which
/// name the nodes merging changed. They are the delta `state` records where its tree is
/// [based on](BasedOn) the first parent; a tree based on another revision records its change from
/// the first parent instead ([`self_parent_delta`]). They are read only for a tree based on
/// another revision and for a revision with a second parent. `first_parent` is the first parent's
/// tree where it was read to record the delta ([`StagedChanges`]); the weave reads it otherwise.
///
/// The first parent's delta is recorded by node ID when the tree of `state` is based on the first
/// parent. Otherwise it is recorded on the [counterpart](state::find_counterpart_node) of each
/// node, and a node merging changed first takes the first parent's history of its counterpart.
/// The second parent's history is recorded on the node at the same path.
#[lore_macro::test_pub]
pub(crate) async fn weave_changes(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    based_on: BasedOn,
    changes: &[NodeDelta],
    first_parent: Option<Arc<State>>,
) -> Result<(), CommitError> {
    let [parent_self, parent_other] = state.parents();
    let mut history_count = 0;
    let mut revision_number = 0;

    if !parent_self.is_zero() {
        lore_debug!("Weave branch history");
        let state_parent = match first_parent.filter(|parent| parent.revision() == parent_self) {
            Some(parent) => parent,
            None => State::deserialize(repository.clone(), parent_self)
                .await
                .forward_with::<CommitError, _>(|| {
                    format!("Failed deserializing parent state {parent_self} when weaving history")
                })?,
        };

        revision_number = std::cmp::max(state_parent.revision_number(), revision_number);

        let delta_buffer = state_parent
            .delta_block(repository.clone())
            .await
            .forward::<CommitError>(
                "Failed deserializing parent state delta block when weaving history",
            )?
            .to_aligned::<NodeDelta>();

        let delta_slice = delta_buffer.as_type_slice::<NodeDelta>();

        lore_debug!("Weaving {} delta entries", delta_slice.len());
        if based_on == BasedOn::Other {
            history_count += Box::pin(weave_first_parent_counterparts(
                &repository,
                &state,
                &state_parent,
                changes,
                delta_slice,
            ))
            .await?;
        } else {
            for delta in delta_slice.iter() {
                if delta.action == FileAction::Delete as u16 {
                    // Deleted files no longer exist in the tree node
                    continue;
                }

                let metadata_node = node::node_to_file_metadata(delta.node);
                let block_index = NodeFileMetadataBlock::index(metadata_node);
                let file_metadata_block = state
                    .block_file_metadata(repository.clone(), block_index)
                    .await
                    .forward::<CommitError>("Failed deserializing file metadata block")?;
                record_first_parent_change(
                    &state,
                    file_metadata_block,
                    block_index,
                    NodeFileMetadata::index(metadata_node),
                    parent_self,
                    delta,
                );
                history_count += 1;
            }
        }
    }

    if !parent_other.is_zero() {
        lore_debug!("Weave merge history");
        let state_parent = State::deserialize(repository.clone(), parent_other)
            .await
            .forward_with::<CommitError, _>(|| {
                format!("Failed deserializing parent state {parent_other} when weaving history")
            })?;

        revision_number = std::cmp::max(state_parent.revision_number(), revision_number);

        // The last block read is held into the next entry, so that entries sharing a block of the
        // parent read it once: nothing else holds the parent's blocks.
        let mut held = None;
        for (idelta, delta) in changes.iter().enumerate() {
            if delta.flags & change::Flags::Merge != 0 {
                if delta.action == FileAction::Delete as u16 {
                    // Deleted files no longer exist in the tree node
                    continue;
                }

                let node_id = delta.node;
                let node_path = state
                    .node_path(repository.clone(), node_id)
                    .await
                    .forward::<CommitError>("Failed constructing node path when weaving history")?;

                let Some(node_parent) =
                    state::node_at_path(&repository, &state_parent, ROOT_NODE, node_path.as_str())
                        .await
                        .unwrap_or_default()
                else {
                    continue;
                };

                let metadata_node = node::node_to_file_metadata(node_parent);
                let block_index = NodeFileMetadataBlock::index(metadata_node);
                let node_index = NodeFileMetadata::index(metadata_node);

                let file_metadata_block = state_parent
                    .block_file_metadata(repository.clone(), block_index)
                    .await
                    .forward::<CommitError>("Failed deserializing file metadata block")?;

                let merge_metadata = *file_metadata_block.read().node(node_index);
                held = Some(file_metadata_block);

                let metadata_node = node::node_to_file_metadata(node_id);
                let block_index = NodeFileMetadataBlock::index(metadata_node);
                let node_index = NodeFileMetadata::index(metadata_node);

                let file_metadata_block = state
                    .block_file_metadata(repository.clone(), block_index)
                    .await
                    .forward::<CommitError>("Failed deserializing file metadata block")?;

                let dirtied = {
                    let mut block_writer = file_metadata_block.write();
                    let node = block_writer.node(node_index);

                    node.revision[1] = merge_metadata.revision[0];
                    node.node[1] = merge_metadata.node[0];
                    node.flags[1] |= merge_metadata.flags[0] | change::Flags::Merge.bits();
                    node.action[1] = merge_metadata.action[0];

                    lore_trace!(
                        "History {}/{}: node {} {:?}",
                        idelta,
                        changes.len(),
                        node_id,
                        node
                    );

                    block_writer.mark_dirty()
                };

                if dirtied {
                    state.block_file_metadata_modified(file_metadata_block, block_index);
                }
            }
        }

        drop(held);

        // Then weave in the node delta of the revision being merged in (again, other parent)
        let delta_buffer = state_parent
            .delta_block(repository.clone())
            .await
            .forward::<CommitError>(
                "Failed deserializing parent state delta block when weaving history",
            )?
            .to_aligned::<NodeDelta>();
        let delta_slice = delta_buffer.as_type_slice::<NodeDelta>();
        let mut idelta = 1;

        lore_debug!("Weaving {} delta entries", delta_slice.len());
        for delta in delta_slice.iter() {
            if delta.action == FileAction::Delete as u16 {
                // Deleted files no longer exist in the tree node
                continue;
            }

            let node_id = delta.node;
            let Ok(node_path) =
                state_parent
                    .node_path(repository.clone(), node_id)
                    .await
                    .forward::<CommitError>("Failed constructing node path when weaving history")
            else {
                continue;
            };

            let Some(current_node_id) =
                state::node_at_path(&repository, &state, ROOT_NODE, node_path.as_str())
                    .await
                    .unwrap_or_default()
            else {
                // This node does not exist in the current state, it was deleted as part of the
                // merge conflict resolution, or deleted in the incoming branch revision.
                // TODO(mjansson): For the rename case where the node as renamed/moved as part
                //                 of the merge and now exist in a new node in current state
                //                 with a new name, we need to find the correct target node
                lore_debug!(
                    "Ignore weaving history for path that does not exist in current state: {node_path}"
                );
                continue;
            };

            let metadata_node = node::node_to_file_metadata(current_node_id);
            let block_index = NodeFileMetadataBlock::index(metadata_node);
            let node_index = NodeFileMetadata::index(metadata_node);

            let Ok(file_metadata_block) = state
                .block_file_metadata(repository.clone(), block_index)
                .await
                .forward::<CommitError>("Failed deserializing file metadata block")
            else {
                continue;
            };

            let dirtied = {
                let mut block_writer = file_metadata_block.write();
                let node = block_writer.node(node_index);

                node.revision[1] = parent_other;
                node.node[1] = node_id;
                node.action[1] = delta.action;
                node.flags[1] = delta.flags | change::Flags::Merge.bits();

                lore_trace!(
                    "History {}/{}: node {} {:?}",
                    idelta,
                    delta_slice.len(),
                    current_node_id,
                    node
                );

                block_writer.mark_dirty()
            };

            if dirtied {
                state.block_file_metadata_modified(file_metadata_block, block_index);
            }

            idelta += 1;
            history_count += 1;
        }
    }

    if history_count > 0 && !execution_context().globals().dry_run() {
        lore_info!("Stored history for {history_count} nodes");
    }

    state.set_revision_number(revision_number + 1);

    Ok(())
}

/// Records the first parent's history on a tree of `state` built from another revision. A node the
/// merging changed, among `changes`, takes the history the first parent `state_parent` holds for its
/// [counterpart](state::find_counterpart_node), and a node `delta_slice`, the first parent's own
/// delta, changed records the first parent on its counterpart. A node either deletes is skipped.
/// Returns the count of nodes recorded.
///
/// Boxed by the caller: a tree is built from another revision than its first parent only for a
/// merge reconciling divergent history and a stage a commit moved onto another revision
/// ([`BasedOn::Other`]).
async fn weave_first_parent_counterparts(
    repository: &Arc<RepositoryContext>,
    state: &State,
    state_parent: &State,
    changes: &[NodeDelta],
    delta_slice: &[NodeDelta],
) -> Result<u32, CommitError> {
    let block_count = state
        .tree(repository.clone())
        .await
        .forward::<CommitError>("Failed reading tree when weaving history")?
        .block_count;
    let parent_block_count = state_parent
        .tree(repository.clone())
        .await
        .forward::<CommitError>("Failed reading parent tree when weaving history")?
        .block_count;
    let mut history_count = 0;

    for delta in changes {
        if delta.flags & change::Flags::Merge == 0 || delta.action == FileAction::Delete as u16 {
            continue;
        }

        let Some(parent_node) = state::find_counterpart_node(
            repository,
            state,
            delta.node,
            state_parent,
            parent_block_count,
        )
        .await
        .forward::<CommitError>("Failed finding a node's counterpart in the first parent")?
        else {
            continue;
        };
        copy_first_parent_history(repository, state, delta.node, state_parent, parent_node).await?;
        history_count += 1;
    }

    for delta in delta_slice {
        if delta.action == FileAction::Delete as u16 {
            continue;
        }

        let Some(node_id) =
            state::find_counterpart_node(repository, state_parent, delta.node, state, block_count)
                .await
                .forward::<CommitError>("Failed finding a node's counterpart in the tree")?
        else {
            continue;
        };
        let metadata_node = node::node_to_file_metadata(node_id);
        let block_index = NodeFileMetadataBlock::index(metadata_node);
        let file_metadata_block = state
            .block_file_metadata(repository.clone(), block_index)
            .await
            .forward::<CommitError>("Failed deserializing file metadata block")?;
        record_first_parent_change(
            state,
            file_metadata_block,
            block_index,
            NodeFileMetadata::index(metadata_node),
            state_parent.revision(),
            delta,
        );
        history_count += 1;
    }
    Ok(history_count)
}

/// Sets the first-parent history of node `node_id` of `state` to the first-parent history `parent`
/// holds for its node `parent_node`.
async fn copy_first_parent_history(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node_id: NodeID,
    parent: &State,
    parent_node: NodeID,
) -> Result<(), CommitError> {
    let metadata_node = node::node_to_file_metadata(parent_node);
    let history = *parent
        .block_file_metadata(
            repository.clone(),
            NodeFileMetadataBlock::index(metadata_node),
        )
        .await
        .forward::<CommitError>("Failed deserializing parent file metadata block")?
        .read()
        .node(NodeFileMetadata::index(metadata_node));

    let metadata_node = node::node_to_file_metadata(node_id);
    let block_index = NodeFileMetadataBlock::index(metadata_node);
    let block = state
        .block_file_metadata(repository.clone(), block_index)
        .await
        .forward::<CommitError>("Failed deserializing file metadata block")?;
    let dirtied = {
        let mut block_writer = block.write();
        let node = block_writer.node(NodeFileMetadata::index(metadata_node));
        node.revision[0] = history.revision[0];
        node.node[0] = history.node[0];
        node.action[0] = history.action[0];
        node.flags[0] = history.flags[0];
        block_writer.mark_dirty()
    };
    if dirtied {
        state.block_file_metadata_modified(block, block_index);
    }
    Ok(())
}

/// Records in the node at `node_index` of `block`, block `block_index` of `state`, that the node
/// last changed in `parent_self` as `delta` describes, which names the node by its ID there, and
/// clears its second-parent history.
fn record_first_parent_change(
    state: &State,
    block: Arc<NodeFileMetadataBlock>,
    block_index: usize,
    node_index: usize,
    parent_self: Hash,
    delta: &NodeDelta,
) {
    let dirtied = {
        let mut block_writer = block.write();
        let node = block_writer.node(node_index);

        node.revision[0] = parent_self;
        node.revision[1] = Hash::default();
        node.node[0] = delta.node;
        node.node[1] = 0;
        node.action[0] = delta.action;
        node.flags[0] = delta.flags;
        node.action[1] = change::FileAction::Keep as u16;
        node.flags[1] = NodeFileMetadataFlags::NoFlag.bits();

        lore_trace!("History of block {block_index} node {node_index}: {node:?}");

        block_writer.mark_dirty()
    };

    if dirtied {
        state.block_file_metadata_modified(block, block_index);
    }
}

pub async fn prepare_commit_metadata(
    repository: Arc<RepositoryContext>,
    metadata: Metadata,
    branch: BranchId,
    message: String,
    keys: Option<Vec<String>>,
    values: Option<Vec<String>>,
    formats: Option<Vec<MetadataType>>,
) -> Result<Arc<Metadata>, CommitError> {
    let mut metadata = metadata;

    // Set metadata for revision, overwriting any existing value
    let commit_timestamp = util::time::timestamp();
    let commit_user = execution_context().user_id().await;
    let commit_changelist = std::env::var("LORE_P4_CHANGELIST").unwrap_or_default();

    metadata
        .set_branch(branch)
        .forward::<CommitError>("Failed setting revision metadata")?;
    metadata
        .set_u64(metadata::TIMESTAMP, commit_timestamp)
        .forward::<CommitError>("Failed setting revision metadata")?;
    metadata
        .set_string(metadata::MESSAGE, &message)
        .forward::<CommitError>("Failed setting revision metadata")?;
    // TODO: Extend this with UCS-Auth information
    if commit_user.is_empty() {
        // Authorship is required only when the active remote authenticates.
        let require_identity = match repository.remote().await {
            Ok(conn) => !conn.auth_url().is_empty(),
            Err(lore_transport::ProtocolError::NoRemote(_)) => false,
            Err(_) => true,
        };
        if require_identity {
            return Err(MissingIdentity.into());
        }
    } else {
        if string_key_is_unset(&metadata, metadata::CREATED_BY) {
            metadata
                .set_string(metadata::CREATED_BY, &commit_user)
                .forward::<CommitError>("Failed setting revision metadata")?;
        }
        metadata
            .set_string(metadata::COMMITTED_BY, &commit_user)
            .forward::<CommitError>("Failed setting revision metadata")?;
    }
    // TODO: Move this to external metadata
    if !commit_changelist.is_empty() {
        metadata
            .set_string(metadata::P4_CHANGELIST, &commit_changelist)
            .forward::<CommitError>("Failed setting revision metadata")?;
    }

    if let (Some(keys), Some(values), Some(formats)) = (keys, values, formats) {
        // Set any additional metadata
        for index in 0..keys.len() {
            let key = &keys.as_slice()[index];
            let value = &values.as_slice()[index];
            let format = formats.as_slice()[index];
            match format {
                MetadataType::Binary => {
                    return Err(CommitError::internal("Unsupported metadata type"));
                }
                MetadataType::Numeric => {
                    let number = value
                        .as_str()
                        .parse::<u64>()
                        .internal("Failed setting revision metadata")?;
                    metadata
                        .set_u64(key.as_str(), number)
                        .forward::<CommitError>("Failed setting revision metadata")?;
                }
                MetadataType::String => {
                    metadata
                        .set_string(key.as_str(), value.as_str())
                        .forward::<CommitError>("Failed setting revision metadata")?;
                }
                MetadataType::Address => {
                    let address = Address::from_str(value.as_str())
                        .internal("Failed setting revision metadata")?;
                    metadata
                        .set_address(key.as_str(), address)
                        .forward::<CommitError>("Failed setting revision metadata")?;
                }
                MetadataType::Boolean => {
                    let bool = value == "1" || value.to_lowercase() == "true";
                    metadata
                        .set_bool(key.as_str(), bool)
                        .forward::<CommitError>("Failed setting revision metadata")?;
                }
                MetadataType::Context => {
                    let context = Context::from_str(value.as_str())
                        .internal("Failed setting revision metadata")?;
                    metadata
                        .set_context(key.as_str(), context)
                        .forward::<CommitError>("Failed setting revision metadata")?;
                }
                MetadataType::Hash => {
                    let hash = Hash::from_str(value.as_str())
                        .internal("Failed setting revision metadata")?;
                    metadata
                        .set_hash(key.as_str(), hash)
                        .forward::<CommitError>("Failed setting revision metadata")?;
                }
            }
        }
    }

    Ok(Arc::new(metadata))
}

/// Marks the paths a merge's dirty-tracking carry holds dirty, where its parents are
/// `merge_parent_self` and `merge_parent_other`; the carry is cleared either way.
async fn apply_merge_carry(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    merge_parent_self: Hash,
    merge_parent_other: Hash,
) -> Result<(), CommitError> {
    let carry = crate::merge_carry::take_matching(
        repository.clone(),
        merge_parent_self,
        merge_parent_other,
    )
    .await
    .forward::<CommitError>("Failed reading merge dirty-tracking carry")?;
    if let Some(paths) = carry
        && !paths.is_empty()
    {
        crate::file::dirty::dirty_relative_paths_in_operation(
            operation,
            repository.clone(),
            paths,
            None,
        )
        .await
        .forward::<CommitError>("Failed to apply merge dirty-tracking carry")?;
    }
    Ok(())
}

/// Discard dirty-only-add and dirty-only-copy subtrees, return dirty-only moves to the place `base`
/// holds them at, and clear dirty flags on every other dirty-only node before the commit pipeline
/// builds the new revision, so it holds `base` with the staged changes and nothing else. A
/// dirty-only move a staged change displaces leaves the tree, as it cannot go back. `base` is the
/// revision the staged tree shares its node IDs with, the one it was built on or loaded from.
/// Answers each dirty-only move, gone back or left out, with the path the staged tree held its node
/// at, in the order the walk left them: each before the moves of the directories above it.
///
/// A dirty-only directory that holds a staged node is staged with it, as staging the node there
/// would have staged it, so the revision takes the directory where the staged tree holds it, a
/// move or an add with it, and none of its staged nodes is moved or discarded. A directory a
/// staged node left, which a dirty move takes it from without staging the directory, is staged
/// once the moves settle, so the commit rehashes it without that node ([`stage_left`]).
///
/// A node only marked dirty as deleted that shares its name with a staged node of its directory
/// is staged for deletion, with what it holds, as a revision holds one node of a name
/// ([`take_replaced_deletions`]); [`Pruned::deleted`] answers each, and the commit marks nothing
/// again at its path, which the staged node holds.
///
/// `state_staged` doubles as the source tree for the new committed revision
/// and the carrier of staged-anchor tracking. Dirty-only nodes belong only to
/// the latter — they were marked by `lore dirty <path>` (or `lore status
/// --scan`) but never staged, so the committed merkle tree must not reference
/// them. After pruning, the staged tracking is rebuilt against the new
/// committed revision by marking the returned moves again, marking each other
/// node with the dirty marks it carried, and re-running `file::dirty::dirty()`
/// on the paths of the adds, copies and deletions captured beforehand ([`DirtyOnly::carried`]).
///
/// The walk is post-order, so empty intermediate directories can collapse
/// upward after their dirty-add children have already been removed, and a move
/// below a moved directory returns before the directory does. Moves that wait
/// for another to go first settle after the walk ([`settle_waiting`]). It is a
/// loop over a stack of the nodes entered and not yet left rather than a
/// recursion, so no node is boxed and the thread's stack does not grow with the
/// tree's depth.
///
/// Only dirty children are descended into: `node_mark` propagates the base Dirty bit to every
/// directory above the node but the root, so a clean child cannot have dirty descendants (the
/// same invariant `collect_dirty_paths` walks by). Clean subtrees are untouched, so the
/// empty-directory collapse only applies to directories on dirty paths. A staged node is seen
/// among the children read whether or not it is dirty.
pub(crate) async fn prune_dirty_for_commit(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    base: Hash,
) -> Result<Pruned, CommitError> {
    let root = state
        .block(repository.clone(), NodeBlock::index(ROOT_NODE))
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    let mut levels = vec![PruneLevel::new(ROOT_NODE, root, 0)];
    let mut context = PruneContext {
        base,
        returned: Vec::new(),
        waiting: Vec::new(),
        left: Vec::new(),
        replaced: Vec::new(),
        deleted: Vec::new(),
        name: String::new(),
        deleted_above: 0,
    };
    while let Some(child_id) = levels.last().map(|level| level.next_child) {
        if child_id.is_valid_node_id() {
            let child_block = state
                .block(repository.clone(), NodeBlock::index(child_id))
                .await
                .forward::<CommitError>("Failed deserializing state block")?;
            let (next_sibling, child_is_dirty, child_is_staged, child_content) = {
                let child = child_block.node(Node::index(child_id));
                let staged = child.is_staged();
                let content =
                    staged && (!child.is_dirty() || !state::is_staged_for_descendants_only(&child));
                (child.sibling, child.is_dirty(), staged, content)
            };
            if let Some(level) = levels.last_mut() {
                level.next_child = next_sibling;
                level.holds_staged |= child_is_staged;
                level.staged_child |= child_is_staged;
                level.holds_content |= child_content;
            }
            if child_is_dirty {
                let level = PruneLevel::new(child_id, child_block, context.replaced.len());
                context.deleted_above += usize::from(level.entered_deleted);
                levels.push(level);
            }
            continue;
        }

        let Some(level) = levels.pop() else { break };
        context.deleted_above -= usize::from(level.entered_deleted);
        let (node_id, holds_staged, holds_content) =
            (level.node_id, level.holds_staged, level.holds_content);
        let leave = prune_dirty_leave(&state, &repository, level, &mut context).await?;
        if let Some(parent) = levels.last_mut() {
            parent.holds_staged |= holds_staged;
            parent.staged_child |= holds_staged;
            parent.holds_content |= holds_content;
            match leave {
                Leave::Keep => {}
                Leave::Discard => parent.to_discard.push(node_id),
                Leave::Replaced(name_hash) => context.replaced.push((node_id, name_hash)),
            }
        }
    }
    settle_waiting(&state, &repository, &mut context).await?;
    stage_left(&state, &repository, &mut context.left).await?;
    if state.holds_promoted_moves() {
        Box::pin(crate::file::stage::stage_promoted_moves_of_call(
            &repository,
            &state,
            None,
        ))
        .await
        .forward::<CommitError>("Failed to stage the move of a directory the commit stages")?;
    }
    context.deleted.sort_unstable();
    Ok(Pruned {
        moves: context.returned,
        deleted: context.deleted,
    })
}

/// The dirty-only nodes a commit marks again past it, as [`state::collect_dirty_only_paths`] finds
/// them.
#[derive(Default)]
struct DirtyOnly {
    /// The path of each node marked dirty as added, copied or deleted, which the commit marks again
    /// as the working tree holds it, or found through a link.
    paths: Vec<RelativePath>,
    /// Each node of [`Self::paths`] marked dirty as added, copied or deleted, and each new directory
    /// only marked dirty whose children carry it, which the commit marks again by their paths
    /// ([`state::DirtyPathSink::carried_by_children`]), not found through a link.
    named: Vec<NamedPath>,
    /// Each other node of the tree, with its flags, which the revision holds where the tree does or
    /// a move left out takes back, and which keeps the dirty marks it carries.
    marks: Vec<(NodeID, NodeFlags)>,
}

/// A node [`DirtyOnly`] holds the path of, marked dirty as added, copied or deleted: its directory
/// and name hash, which a node of its name shares, its kind, and its path's index in
/// [`DirtyOnly::paths`].
/// A directory its children carry has no path there: it shares one with the node marked deleted
/// that it pairs with, as both hold one name in one directory.
struct NamedPath {
    node: NodeID,
    parent: NodeID,
    name_hash: u64,
    at: u32,
    deleted: bool,
    directory: bool,
    by_children: bool,
}

const _: () = assert!(std::mem::size_of::<NamedPath>() == 24);

impl NamedPath {
    /// The name this node holds in its directory.
    fn name(&self) -> (NodeID, u64) {
        (self.parent, self.name_hash)
    }
}

impl state::DirtyPathSink for DirtyOnly {
    const THROUGH_MOVES: bool = true;

    fn record(&mut self, path: &RelativePathBuf, id: NodeID, node: &Node, linked: bool) {
        if linked || node.is_dirty_add() || node.is_dirty_copy() || node.is_dirty_delete() {
            if !linked {
                self.named.push(NamedPath {
                    node: id,
                    parent: node.parent,
                    name_hash: node.name_hash,
                    at: self.paths.len() as u32,
                    deleted: node.is_dirty_delete(),
                    directory: node.is_directory(),
                    by_children: false,
                });
            }
            self.paths.push(path.clone().freeze());
        } else {
            self.marks
                .push((id, NodeFlags::from_bits_retain(node.flags)));
        }
    }

    fn carried_by_children(&mut self, id: NodeID, node: &Node, linked: bool) {
        if linked {
            return;
        }
        self.named.push(NamedPath {
            node: id,
            parent: node.parent,
            name_hash: node.name_hash,
            at: u32::MAX,
            deleted: false,
            directory: true,
            by_children: true,
        });
    }
}

impl DirtyOnly {
    /// The paths to mark again past the commit, and the marks carried beside them, once the prune
    /// left out the moves of `pruned` and the commit the staged adds of `added`.
    ///
    /// The path of each node whose deletion the prune staged, or a link's prune did
    /// ([`LeftToMark::replaced`]), is dropped: the commit took the deletion, and what the working
    /// tree holds there now is the node that replaced it. A node marked deleted beside one marked
    /// added under its name is carried by node, with that one, rather than by path: the working
    /// tree holds the added node there, which a replay of the path would read as a change of the
    /// deleted one. An added directory whose children carry it is added again so, and its children
    /// by their paths below it.
    fn carried(
        self,
        pruned: Pruned,
        left: LeftToMark,
    ) -> (Vec<RelativePath>, crate::file::dirty::CarriedMarks) {
        let DirtyOnly {
            mut paths,
            mut named,
            marks,
        } = self;
        let LeftToMark {
            mut added,
            mut replaced,
        } = left;
        let mut dropped = Vec::new();
        if !pruned.deleted.is_empty() {
            named.retain(|path| {
                let staged = path.deleted && pruned.deleted.binary_search(&path.node).is_ok();
                if staged {
                    dropped.push(path.at as usize);
                }
                !staged
            });
        }
        let deletions = named.iter().filter(|path| path.deleted).count();
        let mut deleted = Vec::new();
        if deletions > 0 && deletions < named.len() {
            named.sort_unstable_by_key(NamedPath::name);
            for group in named.chunk_by(|one, other| one.name() == other.name()) {
                let Some(shared) = group.iter().find(|path| path.deleted) else {
                    continue;
                };
                if group.iter().all(|path| path.deleted) {
                    continue;
                }
                let shared = shared.at as usize;
                for path in group {
                    let at = if path.by_children {
                        paths[shared].clone()
                    } else {
                        dropped.push(path.at as usize);
                        paths[path.at as usize].clone()
                    };
                    if path.deleted {
                        deleted.push(crate::file::dirty::DeletedNode {
                            node: path.node,
                            directory: path.directory,
                            path: at,
                        });
                    } else {
                        added.push(crate::file::dirty::AddedNode {
                            path: at,
                            directory: path.directory,
                        });
                    }
                }
            }
        }
        if !dropped.is_empty() {
            dropped.sort_unstable();
            let mut at = 0;
            paths.retain(|_| {
                at += 1;
                dropped.binary_search(&(at - 1)).is_err()
            });
        }
        if !replaced.is_empty() {
            replaced.sort_unstable_by(|one, other| one.as_str().cmp(other.as_str()));
            paths.retain(|path| {
                replaced
                    .binary_search_by(|held| held.as_str().cmp(path.as_str()))
                    .is_err()
            });
        }
        let carried = crate::file::dirty::CarriedMarks {
            moves: pruned.moves,
            marks,
            deleted,
            added,
        };
        (paths, carried)
    }
}

/// What [`prune_dirty_for_commit`] answers.
pub(crate) struct Pruned {
    /// Each dirty-only move, gone back or left out, with the path the staged tree held its node at,
    /// in the order the walk left them.
    pub moves: Vec<MovedNode>,
    /// Each node whose deletion the prune staged, with what it holds, as a staged node of its name
    /// replaced it, sorted ([`take_replaced_deletions`]); empty where no node takes it. What it
    /// holds is not listed: the commit marks no path below it again.
    pub deleted: Vec<NodeID>,
}

impl Pruned {
    /// The directories of [`Self::moves`] ([`Gone::left_out`]).
    fn left_out(&self) -> impl Iterator<Item = NodeID> + '_ {
        self.moves
            .iter()
            .filter(|moved| moved.node_type == crate::interface::LoreNodeType::Directory)
            .map(|moved| moved.node)
    }
}

/// A dirty node [`prune_dirty_for_commit`] has entered and not yet left.
struct PruneLevel {
    node_id: NodeID,
    block: Arc<NodeBlock>,
    /// The next child to visit, invalid once every child has been, and from the start for a node
    /// that is not a directory.
    next_child: NodeID,
    /// The children found to be discarded, in the order visited.
    to_discard: Vec<NodeID>,
    /// Where its children only marked dirty as deleted start in [`PruneContext::replaced`].
    replaced_from: usize,
    /// Whether a staged node lies below it.
    holds_staged: bool,
    /// Whether a child of it is staged, as entered or once left, which is what a child only marked
    /// dirty as deleted can share its name with ([`take_replaced_deletions`]).
    staged_child: bool,
    /// Whether staged content lies below it: a node staged in its own right, or a staged node the
    /// walk does not enter, which may hold one.
    holds_content: bool,
    /// Whether it was staged when entered.
    entered_staged: bool,
    /// Whether it was marked dirty as deleted and not staged when entered.
    entered_deleted: bool,
}

impl PruneLevel {
    fn new(node_id: NodeID, block: Arc<NodeBlock>, replaced_from: usize) -> Self {
        let node = block.node(Node::index(node_id));
        let next_child = if node.is_directory() {
            node.child
        } else {
            INVALID_NODE
        };
        let entered_staged = node.is_staged();
        let entered_deleted = node.is_dirty_delete() && !entered_staged;
        Self {
            node_id,
            block,
            next_child,
            to_discard: Vec::new(),
            replaced_from,
            holds_staged: false,
            staged_child: false,
            holds_content: false,
            entered_staged,
            entered_deleted,
        }
    }
}

/// What [`prune_dirty_for_commit`] carries from node to node.
struct PruneContext {
    /// The revision the staged tree shares its node IDs with.
    base: Hash,
    /// Each dirty-only move, with the path the staged tree held its node at, in the order the walk
    /// left them: gone back, waiting to, or left out of the tree.
    returned: Vec<MovedNode>,
    /// The node of each dirty-only move waiting to go back ([`Return::Parked`]), in the order it
    /// came to wait.
    waiting: Vec<NodeID>,
    /// The directory the base revision holds each staged node in that this tree holds in another.
    left: Vec<NodeID>,
    /// Each node only marked dirty as deleted whose directory the walk has not left yet, with its
    /// name hash. The entries from a level's [`PruneLevel::replaced_from`] on are its children's;
    /// its leave settles and drops them ([`take_replaced_deletions`]). One list for the walk, so
    /// no directory allocates for it.
    replaced: Vec<(NodeID, u64)>,
    /// Each node whose deletion the walk staged as a staged node of its name replaced it, without
    /// what it holds ([`Pruned::deleted`]).
    deleted: Vec<NodeID>,
    /// The buffer each committed name is read into.
    name: String,
    /// How many nodes above the one the walk leaves were marked dirty as deleted and not staged
    /// when entered ([`MovedNode::below_deleted`]).
    deleted_above: usize,
}

/// Leaves a node of [`prune_dirty_for_commit`] once its children have been: discards the
/// children found to be discarded, each with its subtree, stages the deletion of those only marked
/// dirty as deleted that a staged child of their name replaces ([`take_replaced_deletions`]),
/// returns it to the place the base revision holds it at where it is an unstaged dirty move
/// ([`State::node_return_unstaged_move`]), recording it in `context`, clears its dirty flags unless
/// it is an unstaged dirty add or copy to be discarded, and answers what its parent does with it,
/// one only marked dirty as deleted named to its parent for that check. A node that was not staged
/// and holds staged content, or a dirty add that holds a staged node, is staged first, its
/// ancestors with it, as staging that node would have staged it ([`stage_holder`]), its dirty move
/// or add with it. One that holds only staged directories that no staged content lies below, as a
/// staged move out of them leaves them for the rehash, keeps its dirty move out and is staged
/// once its dirty flags are cleared, so the commit reaches them.
///
/// `clear_dirty_flags` preserves Staged and the action bits when Staged is set,
/// so clearing both restores dirty-only nodes and strips a stale Dirty
/// propagation bit that `rehash_directory` would otherwise leave on a staged
/// parent. An intermediate directory created solely to host dirty-add children
/// is discarded once emptied, so the parent's rehash does not pull in an
/// orphaned dirty-only directory. An empty directory the base revision holds stays: a commit
/// takes no deletion that was not staged. Only such a directory reads the base revision for it,
/// so that read is boxed.
async fn prune_dirty_leave(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    level: PruneLevel,
    context: &mut PruneContext,
) -> Result<Leave, CommitError> {
    let PruneLevel {
        node_id,
        block,
        to_discard,
        replaced_from,
        holds_staged,
        staged_child,
        holds_content,
        entered_staged,
        ..
    } = level;
    for discard_id in to_discard {
        state::node_discard_fused(state.clone(), repository.clone(), discard_id)
            .await
            .forward::<CommitError>("Failed discarding dirty node")?;
    }
    if staged_child && context.replaced.len() > replaced_from {
        let PruneContext {
            replaced, deleted, ..
        } = &mut *context;
        Box::pin(take_replaced_deletions(
            state,
            repository,
            node_id,
            (&mut replaced[replaced_from..], deleted),
        ))
        .await?;
    }
    context.replaced.truncate(replaced_from);

    let is_root = node_id == ROOT_NODE;
    let (flags, child_now, link, parent) = {
        let node = block.node(Node::index(node_id));
        (
            NodeFlags::from_bits_retain(node.flags),
            node.child,
            node.is_link().then(|| node.linked_node()),
            node.parent,
        )
    };
    let replaced = !is_root
        && link.is_none()
        && flags.contains(NodeFlags::DirtyDelete)
        && !flags.contains(NodeFlags::Staged);
    let is_staged = flags.contains(NodeFlags::Staged) || holds_staged;
    let takes_move = flags.contains(NodeFlags::Staged) || holds_content;
    let promotes = holds_content || (holds_staged && flags.contains(NodeFlags::DirtyAdd));
    if is_staged
        && !is_root
        && let Some(left) = state
            .base_parent(repository, context.base, node_id)
            .await
            .forward::<CommitError>("Failed reading the revision the staged tree was built on")?
        && left != parent
        && left != ROOT_NODE
    {
        context.left.push(left);
    }
    if promotes && !entered_staged && !is_root {
        Box::pin(stage_holder(state, repository, node_id)).await?;
    }

    if !is_staged
        && (flags.contains(NodeFlags::DirtyAdd) || flags.contains(NodeFlags::DirtyCopy))
        && !is_root
    {
        return Ok(Leave::Discard);
    }

    if !takes_move
        && flags.contains(NodeFlags::DirtyMove)
        && !is_root
        && return_unstaged_move(state, repository, context, node_id, flags).await?
    {
        return Ok(Leave::Discard);
    }

    if flags.contains(NodeFlags::Dirty) {
        if let Some(link) = link.filter(|_| !is_staged) {
            restore_committed_link_pin(state, repository, node_id, link).await?;
        }
        state.node_clear_own(&block, node_id, OwnFlags::Dirty);
    }
    if holds_staged && !promotes && !entered_staged && !is_root {
        Box::pin(stage_holder(state, repository, node_id)).await?;
    }

    if is_staged
        || flags.intersects(NodeFlags::File | NodeFlags::Link)
        || child_now.is_valid_node_id()
        || is_root
    {
        return Ok(kept(&block, node_id, replaced));
    }

    let tracked = Box::pin(state.base_holds(repository, context.base, node_id, flags.node_type()))
        .await
        .forward::<CommitError>("Failed reading the revision the staged tree was built on")?;
    Ok(if tracked {
        kept(&block, node_id, replaced)
    } else {
        Leave::Discard
    })
}

/// What [`prune_dirty_leave`] answers for `node`, which `block` holds, kept in the tree:
/// [`Leave::Replaced`] with its name hash where it was only marked dirty as deleted (`replaced`),
/// and [`Leave::Keep`] otherwise.
fn kept(block: &NodeBlock, node: NodeID, replaced: bool) -> Leave {
    if replaced {
        Leave::Replaced(block.node(Node::index(node)).name_hash)
    } else {
        Leave::Keep
    }
}

/// What [`prune_dirty_leave`] leaves its node's parent to do with it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leave {
    /// Keep it as it stands.
    Keep,
    /// Discard it with its subtree.
    Discard,
    /// Keep it, a node only marked dirty as deleted carrying this name hash, whose deletion its
    /// parent stages where a staged child of the name replaces it ([`take_replaced_deletions`]).
    Replaced(u64),
}

/// Stages the deletion of each of `replaced`, the children of `directory` only marked dirty as
/// deleted, each with its name hash, that a staged child not marked deleted stands beside under its
/// name, as the working tree holds that child at the name and a revision holds one node of a name
/// ([`stage_replaced_subtree`]), recording each of them in `deleted`. One pass over the
/// directory's children, which its caller takes only where a child of it is staged and boxes.
async fn take_replaced_deletions(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    directory: NodeID,
    (replaced, deleted): (&mut [(NodeID, u64)], &mut Vec<NodeID>),
) -> Result<(), CommitError> {
    replaced.sort_unstable_by_key(|&(_, name_hash)| name_hash);
    let mut held: SmallVec<[u64; 1]> = SmallVec::new();
    let mut children = StateNodeChildrenIterator::new(state.clone(), repository.clone(), directory)
        .await
        .forward::<CommitError>("Failed reading a directory holding a deletion marked dirty")?;
    while let Some((_, child)) = children
        .next()
        .await
        .forward::<CommitError>("Failed reading a child beside a deletion marked dirty")?
    {
        if child.is_staged()
            && !child.is_deleted()
            && replaced
                .binary_search_by_key(&child.name_hash, |&(_, name_hash)| name_hash)
                .is_ok()
        {
            held.push(child.name_hash);
        }
    }
    held.sort_unstable();
    for &(node, name_hash) in replaced.iter() {
        if held.binary_search(&name_hash).is_ok() {
            stage_replaced_subtree(state, repository, node, deleted).await?;
        }
    }
    Ok(())
}

/// Stages the deletion of `node`, a node a staged node of its name replaces, with its subtree, as a
/// staged move onto a held name stages the deletion of what it replaces
/// ([`stage_subtree_deletion`]); records `node` in `deleted`, as only its path is one the commit
/// marks again.
async fn stage_replaced_subtree(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    deleted: &mut Vec<NodeID>,
) -> Result<(), CommitError> {
    stage_subtree_deletion(state, repository, node).await?;
    deleted.push(node);
    Ok(())
}

/// Stages the deletion of `node` and of each node below it not staged for deletion already, so the
/// commit records each ([`State::node_mark_subtree`]).
async fn stage_subtree_deletion(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
) -> Result<(), CommitError> {
    const DELETED: Mark = Mark::staged(NodeFlags::StagedDelete, true);
    let is_directory = state
        .node(repository.clone(), node)
        .await
        .forward::<CommitError>("Failed reading a node whose deletion the commit stages")?
        .is_directory();
    state
        .node_mark(repository, node, DELETED)
        .await
        .forward::<CommitError>("Failed staging the deletion of a node")?;
    if !is_directory {
        return Ok(());
    }
    state
        .node_mark_subtree(repository, node, |_, record| {
            (!record.is_staged_delete()).then_some(DELETED)
        })
        .await
        .forward::<CommitError>("Failed staging the deletion of a directory")
}

/// Stages `node`, which holds a staged node, its ancestors with it. Boxed by its caller, as only
/// such a node takes it.
async fn stage_holder(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
) -> Result<(), CommitError> {
    state
        .node_mark(repository, node, Mark::staged(NodeFlags::Staged, false))
        .await
        .forward::<CommitError>("Failed staging a directory that holds a staged node")
}

/// Returns `node_id`, carrying `flags`, a move marked dirty and not staged, to the place the base
/// revision holds it at, as [`State::node_return_unstaged_move`] does, recording it in `context`,
/// and as waiting where it waits; answers `true` where its parent is to discard it, as it cannot
/// go back. A node put back in a directory staged for deletion is staged for deletion with its
/// subtree, as the stage of the directory's deletion marked each node it held then
/// ([`stage_subtree_deletion`]).
async fn return_unstaged_move(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    context: &mut PruneContext,
    node_id: NodeID,
    flags: NodeFlags,
) -> Result<bool, CommitError> {
    let (returned, path) = state
        .node_return_unstaged_move(
            repository,
            context.base,
            node_id,
            &mut context.name,
            ReturnOptions {
                want_path: true,
                park: true,
                beside: false,
            },
        )
        .await
        .forward::<CommitError>("Failed returning an unstaged move")?;
    let moved = path.map(|path| MovedNode {
        node: node_id,
        node_type: flags.node_type(),
        path,
        below_deleted: context.deleted_above > 0,
    });
    if returned == Return::Parked {
        context.waiting.push(node_id);
    }
    context.returned.extend(moved);
    if matches!(returned, Return::Back | Return::Parked)
        && returned_into_deletion(state, repository, node_id).await?
    {
        Box::pin(stage_subtree_deletion(state, repository, node_id)).await?;
    }
    Ok(returned == Return::Displaced)
}

/// Whether the parent of `node` is staged for deletion ([`place_staged_for_deletion`]).
async fn returned_into_deletion(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
) -> Result<bool, CommitError> {
    let parent = state
        .node(repository.clone(), node)
        .await
        .forward::<CommitError>("Failed reading a returned move")?
        .parent;
    place_staged_for_deletion(state, repository, parent).await
}

/// Whether `directory`, the committed place of a moved node, is staged for deletion. The stage of a
/// directory's deletion marks each node it holds, so its own mark answers for each one above it.
async fn place_staged_for_deletion(
    state: &State,
    repository: &Arc<RepositoryContext>,
    directory: NodeID,
) -> Result<bool, CommitError> {
    Ok(state
        .node(repository.clone(), directory)
        .await
        .forward::<CommitError>("Failed reading the committed place of a moved node")?
        .is_staged_delete())
}

/// Settles the moves [`prune_dirty_for_commit`] left waiting, once its walk has discarded every
/// dirty add it does not stage: each goes back where nothing keeps it now, in the reverse of the
/// order they came to wait, so a chain the walk met from its first move settles in one pass, and
/// passes repeat while one settles. One that a staged node displaces leaves the tree with its
/// subtree, as it cannot go back, a directory marked dirty as added and staged for a staged node it
/// holds among them; so does one gone with a directory the walk discarded. Where a pass settles
/// none, the last to wait leaves the tree, so what is left settles.
async fn settle_waiting(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    context: &mut PruneContext,
) -> Result<(), CommitError> {
    while !context.waiting.is_empty() {
        let mut settled = false;
        let mut index = context.waiting.len();
        while index > 0 {
            index -= 1;
            let node = context.waiting[index];
            let (returned, _) = state
                .node_return_unstaged_move(
                    repository,
                    context.base,
                    node,
                    &mut context.name,
                    ReturnOptions {
                        park: true,
                        ..ReturnOptions::default()
                    },
                )
                .await
                .forward::<CommitError>("Failed returning an unstaged move")?;
            match returned {
                Return::Parked | Return::Held => continue,
                Return::Displaced => discard_displaced(state, repository, node).await?,
                Return::Stays | Return::Back | Return::Gone => {}
            }
            context.waiting.remove(index);
            settled = true;
        }
        if !settled && let Some(node) = context.waiting.pop() {
            discard_displaced(state, repository, node).await?;
        }
    }
    Ok(())
}

/// Stages each of `left`, a directory the base revision holds a staged node in that the tree holds
/// in another, where the tree still holds it, with its ancestors: its children in the revision
/// differ from the base revision's, so the commit rehashes it. Runs once the moves settle, so the
/// mark climbs from where each directory stands in the revision.
async fn stage_left(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    left: &mut Vec<NodeID>,
) -> Result<(), CommitError> {
    left.sort_unstable();
    left.dedup();
    for directory in left.iter() {
        let live = !state
            .node(repository.clone(), *directory)
            .await
            .forward::<CommitError>("Failed reading a directory a staged node left")?
            .is_discarded();
        if live {
            state
                .node_mark(
                    repository,
                    *directory,
                    Mark::staged(NodeFlags::Staged, false),
                )
                .await
                .forward::<CommitError>("Failed staging a directory a staged node left")?;
        }
    }
    Ok(())
}

/// Discards `node`, a move that cannot go back, with its subtree.
async fn discard_displaced(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
) -> Result<(), CommitError> {
    state::node_discard_fused(state.clone(), repository.clone(), node)
        .await
        .forward::<CommitError>("Failed discarding an unstaged move")?;
    Ok(())
}

async fn restore_committed_link_pin(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    link: NodeLink,
) -> Result<(), CommitError> {
    let (_, linked_state) = link
        .open(repository)
        .await
        .forward::<CommitError>("Failed to deserialize dirty linked state")?;
    let committed = linked_state.committed_revision();
    if committed == link.revision {
        return Ok(());
    }
    let branch = state
        .link_find(repository.clone(), link.repository, node_id)
        .await
        .forward::<CommitError>("Failed to find link")?
        .branch;
    link::update_link_pin_by_node(
        state,
        repository.clone(),
        link.repository,
        branch,
        committed,
        node_id,
    )
    .await
    .forward::<CommitError>("Failed to restore committed link pin")
}

/// Resolve which branch a revision built on `current_revision` publishes to.
///
/// The revision's own `branch` metadata key decides, so a caller starting a new
/// branch's history sets it before committing. A key that is set must name either
/// the parent revision's branch — continuing it — or a branch whose branch point
/// is exactly `(the parent's branch, current_revision)`, which is what the first
/// revision on a branch created from that revision looks like. Unset resolves to
/// the parent's branch.
///
/// "Still at the branch point" needs no separate check: the tip
/// compare-and-swap compares against `current_revision`, so a branch that has
/// moved past its branch point fails there.
///
/// An initial revision has no parent to read a branch from and must carry the key.
///
/// `parent` is the state of `current_revision` — for a handle that is the state it
/// already holds, whose `metadata_hash` is still the loaded revision's until a
/// commit replaces it. A caller without it in hand deserializes it first, so the
/// read is visible where it is paid for rather than hidden here.
pub async fn resolve_commit_branch(
    repository: Arc<RepositoryContext>,
    parent: Arc<State>,
    metadata: &Metadata,
    current_revision: Hash,
) -> Result<BranchId, CommitError> {
    let requested = metadata
        .get_branch()
        .ok()
        .filter(|branch| !branch.is_zero());

    if current_revision.is_zero() {
        return requested.ok_or_else(|| {
            CommitError::from(InvalidArguments {
                reason: "an initial revision must record the branch it starts".into(),
            })
        });
    }

    let parent_branch = parent.branch(repository.clone()).await;

    let Some(requested) = requested else {
        if parent_branch.is_zero() {
            return Err(InvalidArguments {
                reason: format!(
                    "parent revision {current_revision} records no branch, so the commit must record one"
                ),
            }
            .into());
        }
        return Ok(parent_branch);
    };

    if requested == parent_branch {
        return Ok(requested);
    }

    let branch_metadata = branch::metadata(repository.clone(), requested)
        .await
        .forward::<CommitError>("Failed to load branch metadata")?;
    let stack = branch::stack(&branch_metadata);
    let Some(branch_point) = stack.first() else {
        return Err(InvalidArguments {
            reason: format!("branch {requested} has no branch point to start from"),
        }
        .into());
    };
    if branch_point.branch != parent_branch || branch_point.revision != current_revision {
        return Err(InvalidArguments {
            reason: format!(
                "branch {requested} branches from {} on {}, not from {current_revision} on {parent_branch}",
                branch_point.revision, branch_point.branch
            ),
        }
        .into());
    }
    Ok(requested)
}

/// Stamp the facts about the commit act the caller did not supply — which branch
/// it publishes to, when it happened, and who made it — leaving every key the
/// caller did set alone. Authorship is required only when the active remote
/// authenticates.
async fn stamp_commit_metadata(
    repository: Arc<RepositoryContext>,
    metadata: &mut Metadata,
    branch: BranchId,
) -> Result<(), CommitError> {
    metadata
        .set_branch(branch)
        .forward::<CommitError>("Failed setting revision metadata")?;

    if metadata.get_timestamp().unwrap_or_default() == 0 {
        metadata
            .set_timestamp(util::time::timestamp())
            .forward::<CommitError>("Failed setting revision metadata")?;
    }

    let identity = execution_context().user_id().await;
    if identity.is_empty() {
        let require_identity = match repository.remote().await {
            Ok(connection) => !connection.auth_url().is_empty(),
            Err(lore_transport::ProtocolError::NoRemote(_)) => false,
            Err(_) => true,
        };
        if require_identity && string_key_is_unset(metadata, metadata::CREATED_BY) {
            return Err(MissingIdentity.into());
        }
        return Ok(());
    }

    for key in [metadata::CREATED_BY, metadata::COMMITTED_BY] {
        if string_key_is_unset(metadata, key) {
            metadata
                .set_string(key, &identity)
                .forward::<CommitError>("Failed setting revision metadata")?;
        }
    }
    Ok(())
}

/// Whether `key` holds no string value, so writing one fills a gap rather than
/// overwriting a choice.
///
/// Absent counts as unset, and so does an empty string — matching how
/// [`crate::revision::amend`] reads the same keys. A value of any other kind counts
/// as **set**, because the caller picked that kind deliberately. Asking `get_string`
/// alone conflated the two: it fails for every non-string kind, so a numeric or
/// binary value read as absent and was overwritten.
fn string_key_is_unset(metadata: &Metadata, key: &str) -> bool {
    match metadata.get_typed(key) {
        Ok((value, MetadataType::String)) => Metadata::to_string(value).is_ok_and(str::is_empty),
        Ok(_) => false,
        Err(_) => true,
    }
}

/// Commit the tree a path-less handle holds as a new revision on `branch`.
///
/// The in-memory counterpart to [`commit_staged_revision`]. Every leaf already
/// carries the address the caller put there, so there is no working tree to walk
/// and nothing to fragment — but the tree still has to be frozen: staged
/// deletions discarded, change flags cleared, directories rehashed, and the node
/// delta recorded for the next revision to weave history against.
///
/// `parent_self`, the revision the tree was loaded from, becomes the new revision's first
/// parent, and the branch tip must still be on it: the check runs before anything is written
/// so a caller fails fast, and the pointer write compares against the same observed value so a
/// tip that advances during the freeze fails too.
///
/// A non-zero `parent_other` becomes the second parent of a merge revision, whose tree holds the
/// result of merging ([`reject_unusable_second_parent`]). Every change the delta records is then
/// marked as merged, as `lore branch merge` marks the changes it brings in, and the revision is
/// committed even with nothing staged and no metadata, as a merge revision that changes no file
/// still records its second parent.
///
/// The freeze cannot be undone, so it is snapshotted instead: the staged state is
/// serialized just before it begins, and an error from there on returns that
/// signature as [`InMemoryCommitFailure::restore_from`] for the caller to rebuild
/// the tree from with [`State::deserialize`]. A restored state is a replacement, not
/// a repair — the part-frozen one is dropped whole. Blocks written before the failure
/// are content-addressed and idempotent on a retry, and the snapshot itself is
/// unreferenced once the commit succeeds.
///
/// The snapshot is taken after the rejections, which leave the tree untouched and need
/// none, and before `set_parent_self`, which already belongs to the commit rather than
/// to the state the caller built. The state is marked dirty on either side of it: a
/// clean state serializes to its existing signature without writing, so a
/// metadata-only commit on a handle at the zero revision would snapshot to a zero hash
/// — indistinguishable from "the freeze never started" — and the freeze's own writes
/// re-dirty the state only when it has something to do.
///
/// The write tracker is drained on every path, including failure, since dropping it
/// with leaders still running aborts them. The tip is published last, and only once
/// every payload is durable: an unreferenced revision is an orphan the
/// content-addressed store tolerates and a retry reproduces, where a tip pointing at
/// payloads that never landed is not.
pub async fn commit_in_memory_revision(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    state_staged: Arc<State>,
    metadata: Metadata,
    parent_self: Hash,
    parent_other: Hash,
    branch: BranchId,
) -> Result<Hash, InMemoryCommitFailure> {
    reject_unusable_second_parent(&repository, parent_self, parent_other)
        .await
        .map_err(InMemoryCommitFailure::rejected)?;

    let observed_tip = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();
    if !observed_tip.is_zero() && observed_tip != parent_self {
        lore_debug!(
            "Branch {branch} is at {observed_tip}, not at the loaded revision {parent_self}"
        );
        return Err(InMemoryCommitFailure::rejected(BranchAdvanced));
    }

    let has_staged = has_staged_children(repository.clone(), &state_staged)
        .await
        .map_err(InMemoryCommitFailure::rejected)?;
    if !has_staged && metadata_is_empty(&metadata) && parent_other.is_zero() {
        return Err(InMemoryCommitFailure::rejected(NothingStaged));
    }

    verify_state_for_commit(repository.clone(), state_staged.clone())
        .await
        .forward::<CommitError>("Revision state is not valid to commit")
        .map_err(InMemoryCommitFailure::rejected)?;

    reject_unresolved_link_pins(&repository, &state_staged, None, &link::Mount::root())
        .await
        .map_err(InMemoryCommitFailure::rejected)?;

    let mut metadata = metadata;
    stamp_commit_metadata(repository.clone(), &mut metadata, branch)
        .await
        .map_err(InMemoryCommitFailure::rejected)?;

    // The subtrees C API deletes detached, and every slot a dropped or failed call left detached
    // or in flight, freed after the last refusal and before the snapshot (C15): a commit refused
    // frees nothing, and the snapshot holds no slot left to free. A drain that fails before its
    // frees frees nothing, and its frees find every block its walk held; a snapshot that fails
    // after it refuses the commit with them freed, which takes nothing a caller reaches, as a
    // pending node reads as a freed one already, and no caller holds a detached one.
    state_staged
        .node_sweep_detached(&repository)
        .await
        .forward::<CommitError>("Failed to free the deleted subtrees")
        .map_err(InMemoryCommitFailure::rejected)?;

    state_staged.mark_dirty();
    let restore_from = state_staged
        .serialize(repository.clone(), token)
        .await
        .forward::<CommitError>("Failed to snapshot the staged state")
        .map_err(InMemoryCommitFailure::rejected)?;
    debug_assert!(
        !restore_from.is_zero(),
        "the snapshot must be a real state, or a post-freeze failure has nothing to restore from"
    );
    state_staged.mark_dirty();

    state_staged.set_parent_self(parent_self);
    state_staged.set_parent_other(parent_other);

    let stats = CommitStats::new();
    let tracker = immutable::commit_write_tracker(false);
    let work_tracker = tracker.clone();
    let work_repository = repository.clone();
    let work_result: Result<Hash, CommitError> = async move {
        let delta = freeze_in_memory_tree(
            work_repository.clone(),
            state_staged.clone(),
            parent_self,
            stats.clone(),
            work_tracker.clone(),
        )
        .await?
        .to_aligned::<NodeDelta>();

        state_staged.set_metadata_hash(
            metadata
                .serialize_with_tracker(work_repository.clone(), Some(work_tracker.clone()))
                .await
                .forward::<CommitError>("Failed to write commit metadata")?,
        );

        weave_changes(
            work_repository.clone(),
            state_staged.clone(),
            BasedOn::FirstParent,
            delta.as_type_slice::<NodeDelta>(),
            None,
        )
        .await?;

        state_staged
            .serialize_committed(work_repository, token)
            .await
            .forward::<CommitError>("Failed to serialize revision state")
    }
    .await;

    let drain_result = tracker.await_all().await;

    let signature =
        work_result.map_err(|error| InMemoryCommitFailure::while_freezing(error, restore_from))?;
    drain_result
        .forward::<CommitError>("Background fragment upload task failed during commit")
        .map_err(|error| InMemoryCommitFailure::while_freezing(error, restore_from))?;

    branch::store_latest(
        repository,
        branch,
        observed_tip,
        signature,
        BranchLatestStatus::Divergent,
    )
    .await
    .forward::<CommitError>("Failed to store current branch latest")
    .map_err(|error| InMemoryCommitFailure::while_freezing(error, restore_from))?;

    Ok(signature)
}

/// Why an in-memory commit failed, and whether it had begun freezing the tree.
///
/// A failure before the freeze wrote nothing to the state, so it is still exactly
/// what the caller built — a batch with nothing staged or a branch that already
/// moved on costs the caller nothing. From the freeze onwards the tree is
/// part-frozen and cannot be trusted, and [`Self::restore_from`] carries the
/// pre-commit snapshot to rebuild it from.
#[derive(Debug)]
pub struct InMemoryCommitFailure {
    /// What went wrong.
    pub error: CommitError,
    /// Signature of the pre-commit state, zero until the freeze has begun. The two
    /// facts are one: a tree nothing has mutated needs no snapshot to go back to.
    pub restore_from: Hash,
}

impl InMemoryCommitFailure {
    /// Whether the freeze had begun, so `state_staged` can no longer be trusted.
    pub fn tree_mutated(&self) -> bool {
        !self.restore_from.is_zero()
    }

    fn rejected(error: impl Into<CommitError>) -> Self {
        Self {
            error: error.into(),
            restore_from: Hash::default(),
        }
    }

    fn while_freezing(error: impl Into<CommitError>, restore_from: Hash) -> Self {
        Self {
            error: error.into(),
            restore_from,
        }
    }
}

impl std::fmt::Display for InMemoryCommitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

fn metadata_is_empty(metadata: &Metadata) -> bool {
    let mut count = 0usize;
    metadata.walk(|_, _, _| count += 1);
    count == 0
}

/// Whether anything under the root is staged, stopping at the first one found.
///
/// `State::node_has_staged_children` answers the same question through
/// `node_children`, which collects every child into a vector and follows links —
/// and link following is unbounded, so a cyclic link would hang here before the
/// commit had looked at anything. The iterator neither allocates nor descends into
/// a link, and carries the node so there is no second read per child.
///
/// Where a C API change failed or was dropped after its climb began
/// ([`State::has_unfinished_change`]), a staged directory may hold no change, so only a node
/// carrying an action or a merge's state counts: the walk descends staged directories until it
/// meets one. Without an unfinished change, any staged child of the root answers, so an add then
/// a delete below a tracked directory, which leaves the directory staged, still commits.
async fn has_staged_children(
    repository: Arc<RepositoryContext>,
    state: &Arc<State>,
) -> Result<bool, CommitError> {
    let actions_only = state.has_unfinished_change();
    let found = first_staged_node(&repository, state, |node| {
        !actions_only || node.flags & (NodeFlags::ActionBits | NodeFlags::MergeBits).bits() != 0
    })
    .await?;
    Ok(found.is_some())
}

/// The first staged node below the root of `state` that `matches`, found descending only staged
/// directories that don't match: a node a commit records is staged, and so are its ancestors.
async fn first_staged_node(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    matches: impl Fn(&Node) -> bool,
) -> Result<Option<NodeID>, CommitError> {
    let mut pending = Vec::new();
    let mut parent = ROOT_NODE;
    loop {
        let mut children =
            StateNodeChildrenIterator::new(state.clone(), repository.clone(), parent)
                .await
                .forward::<CommitError>("Failed deserializing state node block")?;
        while let Some((child_node_id, child_node)) = children
            .next()
            .await
            .forward::<CommitError>("Failed deserializing state node block")?
        {
            if !child_node.is_staged() {
                continue;
            }
            if matches(&child_node) {
                return Ok(Some(child_node_id));
            }
            if child_node.is_directory() {
                pending.push(child_node_id);
            }
        }
        match pending.pop() {
            Some(next) => parent = next,
            None => return Ok(None),
        }
    }
}

/// Freeze the staged tree into the shape a revision records, as [`freeze_tree`] does against
/// `base`, the revision the tree was loaded from, and write the node delta, each change marked as
/// merged where the state has a second parent. It answers the delta it wrote for the history weave
/// ([`weave_changes`]), whose second-parent pass reads those marks.
async fn freeze_in_memory_tree(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    base: Hash,
    stats: Arc<CommitStats>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
) -> Result<Bytes, CommitError> {
    let delta = freeze_tree(repository.clone(), state.clone(), base, stats).await?;
    let delta = {
        let mut changes = delta.write();
        if !state.parent_other().is_zero() {
            mark_merged(&mut changes);
        }
        changes.split().freeze()
    };
    generate_delta_block(repository, state, delta.clone(), Some(tracker)).await?;
    Ok(delta)
}

/// Marks every entry of `delta` as a change merging made.
fn mark_merged(delta: &mut BytesMut) {
    for entry in delta.as_chunks_mut::<{ size_of::<NodeDelta>() }>().0 {
        if let Ok(mut change) = NodeDelta::read_from_bytes(entry.as_slice()) {
            change.flags |= change::Flags::Merge.bits();
            entry.copy_from_slice(change.as_bytes());
        }
    }
}

/// Refuses `parent_other`, the second parent of a tree loaded from `parent_self`, naming it: where
/// it is `parent_self`, where `parent_self` is zero, as a merge revision needs a first parent, and
/// where `repository` doesn't hold it. Any other failure to read it is the read's own. Zero is no
/// second parent.
async fn reject_unusable_second_parent(
    repository: &Arc<RepositoryContext>,
    parent_self: Hash,
    parent_other: Hash,
) -> Result<(), CommitError> {
    if parent_other.is_zero() {
        return Ok(());
    }
    if parent_other == parent_self {
        return Err(InvalidArguments {
            reason: format!("the second parent {parent_other} is the loaded revision"),
        }
        .into());
    }
    if parent_self.is_zero() {
        return Err(InvalidArguments {
            reason: format!(
                "a tree loaded from no revision takes no second parent, given {parent_other}"
            ),
        }
        .into());
    }
    match State::deserialize(repository.clone(), parent_other).await {
        Ok(_) => Ok(()),
        Err(error) if error.is_not_found() => Err(CommitError::InvalidArguments(
            InvalidArguments {
                reason: format!("the store doesn't hold the second parent {parent_other}"),
            }
            .chain_err_from(error, "reading the second parent"),
        )),
        Err(error) => Err(error).forward_any("reading the second parent"),
    }
}

/// Applies `changes`, the conflict-free changes of a 3-way diff, to `state`, the tree of the
/// revision they merge onto, and commits the result as `lore branch merge` commits it
/// ([`state::apply_tree_changes`]): the revision `state` was loaded from its first parent and
/// `source`, the revision merged in, its second, the tree frozen against the first parent, the node
/// delta of the merge written as the state's own, the file history of both parent lines woven
/// from it, and the revision numbered one above the higher of its parents.
pub async fn merge_tree_changes(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    source: &Arc<State>,
    changes: &mut [change::NodeChange],
) -> Result<(), CommitError> {
    state.set_parent_self(state.revision());
    state.set_parent_other(source.revision());
    state::apply_tree_changes(repository.clone(), state.clone(), source, changes)
        .await
        .forward_any::<CommitError>("applying the merged changes")?;
    let delta = freeze_tree(
        repository.clone(),
        state.clone(),
        state.parent_self(),
        CommitStats::new(),
    )
    .await?;
    let delta = delta.write().split().freeze();
    generate_delta_block(repository.clone(), state.clone(), delta.clone(), None).await?;
    let delta = delta.to_aligned::<NodeDelta>();
    weave_changes(
        repository,
        state,
        BasedOn::FirstParent,
        delta.as_type_slice::<NodeDelta>(),
        None,
    )
    .await
}

/// Freeze a staged tree whose every leaf carries its address into the shape a revision records:
/// dirty tracking pruned against `base`, the revision the tree shares its node IDs with, staged
/// deletions discarded, staged files and links cleared of their change flags and every affected
/// directory rehashed. Answers the node delta of the changes.
///
/// Only a tree holding a dirty mark takes the prune, boxed: every dirty mark marks a child of the
/// root, and no mark the root itself. The tree a merge applies its changes to holds none.
async fn freeze_tree(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    base: Hash,
    stats: Arc<CommitStats>,
) -> Result<Arc<parking_lot::RwLock<BytesMut>>, CommitError> {
    let dirty = state
        .node_has_dirty_children(repository.clone(), ROOT_NODE)
        .await
        .forward::<CommitError>("Failed reading the children of the staged tree's root")?;
    if dirty {
        Box::pin(prune_dirty_for_commit(
            state.clone(),
            repository.clone(),
            base,
        ))
        .await?;
    }

    let delta = Arc::new(parking_lot::RwLock::new(BytesMut::new()));
    let discard = Arc::new(parking_lot::RwLock::new(vec![]));
    let subnodes_to_discard = Arc::new(parking_lot::RwLock::new(vec![]));

    event::LoreEvent::RevisionCommitBegin(LoreRevisionCommitBeginEventData::default()).send();

    freeze_directory(
        repository.clone(),
        state.clone(),
        ROOT_NODE,
        &[],
        delta.clone(),
        discard.clone(),
        subnodes_to_discard.clone(),
        stats.clone(),
    )
    .await?;

    stats.discovery.complete.store(true, Ordering::Relaxed);
    stats.send_progress();

    commit_discard(
        repository.clone(),
        state.clone(),
        DeletionBase::of(base, BasedOn::FirstParent),
        &[(RelativePath::new(), ROOT_NODE)],
        delta.clone(),
        &discard,
        &subnodes_to_discard,
        stats.clone(),
    )
    .await?;

    event::LoreEvent::RevisionCommitEnd(LoreRevisionCommitEndEventData {
        count: LoreRevisionCommitCountData::new(stats),
    })
    .send();

    rehash_directory(repository.clone(), state.clone(), ROOT_NODE).await?;

    state
        .update_tree_root_hash(repository)
        .await
        .forward::<CommitError>("Failed to update tree root hash")?;

    Ok(delta)
}

/// Walk the staged directories under `node_id`, depth first in child order, each directory's files
/// before its subdirectories.
///
/// The walk is a loop over a stack of directories rather than a recursion, so no directory is
/// boxed and the thread's stack does not grow with the tree's depth.
///
/// Sequential on purpose: the walk only flips flags and appends to one shared
/// delta buffer, so fanning out would contend on that buffer for no gain — the
/// parallelism that matters is in [`rehash_directory`], which hashes.
///
/// A staged file or link already carries the address the caller supplied, so
/// there is nothing to fragment — only the change flags to clear.
///
/// Given `mounts`, the working-tree paths of the mounts of a link and the nodes of the subtrees they
/// mount, the walk freezes the linked tree outside those subtrees, which no working tree holds: it
/// skips the subtrees, names what it refuses as [`walk_path_for_message`] does from the first mount,
/// and refuses a file with content but no address and a link pinned at an uncommitted state, as
/// only a working tree could supply either.
#[allow(clippy::too_many_arguments)]
async fn freeze_directory(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    mounts: &[(RelativePath, NodeID)],
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    discard: Arc<parking_lot::RwLock<Vec<(NodeID, u16)>>>,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<(NodeID, u16)>>>,
    stats: Arc<CommitStats>,
) -> Result<(), CommitError> {
    let mut pending = vec![node_id];
    while let Some(node_id) = pending.pop() {
        let (parent, mut children) =
            StateNodeChildrenIterator::with_parent(state.clone(), repository.clone(), node_id)
                .await
                .forward::<CommitError>("Failed deserializing state block")?;
        if parent.is_staged() {
            delta_add(delta.clone(), node_id, parent.flags);
        }

        let first_subdirectory = pending.len();
        let mut updated = false;
        while let Some((child_node_id, child_node)) = children
            .next()
            .await
            .forward::<CommitError>("Failed deserializing state block")?
        {
            if !child_node.is_staged()
                || mounts.iter().any(|(_, mounted)| *mounted == child_node_id)
            {
                continue;
            }
            updated = true;

            // No working tree holds this one, so a conflict on any node has nowhere to be resolved and
            // no markers to read.
            if child_node.is_staged_merge_unresolved() {
                return Err(Conflict {
                    path: freeze_path_for_message(&repository, &state, mounts, child_node_id).await,
                }
                .into());
            }

            if child_node.is_staged_delete() {
                if child_node.is_directory() {
                    collect_discard_subnodes(
                        repository.clone(),
                        state.clone(),
                        child_node_id,
                        subnodes_to_discard.clone(),
                        stats.clone(),
                    )
                    .await?;
                }
                discard.write().push((child_node_id, child_node.flags));
                if child_node.is_file() {
                    stats.complete.file_total.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats
                        .complete
                        .directory_total
                        .fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }

            if child_node.is_directory() {
                pending.push(child_node_id);
                continue;
            }

            if !mounts.is_empty() && !holds_stored_content(&repository, &child_node).await? {
                return Err(NotSupported {
                    operation: format!(
                        "committing {}, which is not stored",
                        freeze_path_for_message(&repository, &state, mounts, child_node_id).await
                    ),
                }
                .into());
            }

            delta_add(delta.clone(), child_node_id, child_node.flags);
            freeze_node(repository.clone(), state.clone(), child_node_id).await?;
            stats.count_file();
            stats
                .complete
                .file_modify_count
                .fetch_add(1, Ordering::Relaxed);
            stats.record_file_action(child_node.flags, child_node.size);
            stats.discovery.total_files.fetch_add(1, Ordering::Relaxed);
        }
        pending[first_subdirectory..].reverse();

        if updated {
            stats.count_directory();
        }
    }

    Ok(())
}

/// The path of `node_id` for a message of [`freeze_directory`] given `mounts`. Boxed, so the walk's
/// future holds only the box on the paths that report a refusal.
async fn freeze_path_for_message(
    repository: &Arc<RepositoryContext>,
    state: &State,
    mounts: &[(RelativePath, NodeID)],
    node_id: NodeID,
) -> String {
    Box::pin(roots_path_for_message(repository, state, mounts, node_id)).await
}

/// Whether the staged file or link `node` holds content stored already: a file with an address,
/// or with neither an address nor content, and a link pinned at a committed revision. The read of
/// a link's revision is boxed, as few nodes are links.
async fn holds_stored_content(
    repository: &Arc<RepositoryContext>,
    node: &Node,
) -> Result<bool, CommitError> {
    if !node.is_link() {
        return Ok(!(node.address.is_zero() && node.size != 0));
    }
    let (_, link_state) = Box::pin(node.linked_node().open(repository))
        .await
        .forward::<CommitError>("Failed to deserialize link state")?;
    Ok(link_state.revision_number() != 0)
}

/// Clear the staged and dirty flags a committed node no longer carries.
async fn freeze_node(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> Result<(), CommitError> {
    let block_index = NodeBlock::index(node_id);
    let block = state
        .block(repository, block_index)
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    state.node_clear_own(&block, node_id, OwnFlags::Change);
    Ok(())
}
