// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::BytesMut;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_storage::FragmentWriteCounts;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::branch;
use crate::branch::BranchLatestStatus;
use crate::change;
use crate::change::FileAction;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::immutable;
use crate::infer;
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
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeDelta;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeFileMetadataFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::progress::DEFAULT_WORK_CHANNEL_CAPACITY;
use crate::progress::DiscoveryStats;
use crate::progress::MAX_CONCURRENT_DIRECTORY_TASKS;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::verify::verify_state_for_commit;
use crate::revision::sync;
use crate::state;
use crate::state::RecordedModifiedTimes;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;
use crate::state::StateNodeChildrenWithNameIterator;
use crate::util;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;
use crate::util::serde::u8_as_bool;

/// Event data reported at the start of a commit.
#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitBeginEventData {
    /// Unused placeholder field.
    pub _unused: u32,
}

/// Progress counters describing how far a commit has advanced.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    fn new(stats: Arc<CommitStats>) -> Self {
        Self {
            directory_count: stats.complete.directory_count.load(Ordering::Relaxed),
            directory_total: stats.complete.directory_total.load(Ordering::Relaxed),
            file_count: stats.complete.file_count.load(Ordering::Relaxed),
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
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitProgressEventData {
    /// Current progress counters.
    pub count: LoreRevisionCommitCountData,
}

/// Event data reported at the end of a commit.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionCommitStatsEventData {
    /// Files committed, by action.
    pub files: LoreCommitFileStatsData,
    /// What the commit's fragment writes cost.
    pub fragments: FragmentWriteCounts,
}

/// Event data describing a revision produced by a commit.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    // User-actionable
    NothingStaged,
    BranchAdvanced,
    Conflict,
    MissingIdentity,
    // Forwarded from AnchorError, StateErrors
    NotFound,
    NodeNotFound,
    LinkNotFound,
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

pub(crate) struct CommitStats {
    discovery: DiscoveryStats,
    complete: CommitCompleteStats,
    report: CommitReportStats,
    /// Whether to keep [`CommitReportStats`], which nothing reads at statistics
    /// level zero.
    statistics: bool,
}

impl CommitStats {
    /// Counters for one commit, keeping what the call's statistics level reports.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            discovery: DiscoveryStats::default(),
            complete: CommitCompleteStats::default(),
            report: CommitReportStats::default(),
            statistics: crate::runtime::try_execution_context()
                .is_some_and(|context| context.globals().stats()),
        })
    }

    /// One file was read off disk and fragmented. The byte total is what a
    /// progress event reports as transferred, so it is kept whatever the level.
    fn file_read(&self, content_size: u64) {
        if self.statistics {
            self.report.file_read_count.fetch_add(1, Ordering::Relaxed);
        }
        self.complete
            .bytes_transferred
            .fetch_add(content_size, Ordering::Relaxed);
    }

    /// The per-action file counts, as an event payload.
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

    /// Everything this commit has counted since `baseline`, or `None` outside an
    /// execution context — which is also no context to send an event through.
    fn snapshot(&self, baseline: &FragmentWriteCounts) -> Option<LoreRevisionCommitStatsEventData> {
        let context = crate::runtime::try_execution_context()?;
        Some(LoreRevisionCommitStatsEventData {
            files: self.stats.file_stats(),
            fragments: context.fragment_stats().snapshot().since(baseline),
        })
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
                .map(|context| context.fragment_stats().snapshot())
                .unwrap_or_default(),
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
        if let Some(snapshot) = self.reporting.snapshot(&self.baseline) {
            event::LoreEvent::RevisionCommitStats(snapshot).send();
        }
    }
}

pub async fn commit(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
) -> Result<Hash, CommitError> {
    Box::pin(commit_impl(
        repository,
        token,
        options,
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
    ))
    .await
}

pub async fn commit_with_metadata(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
) -> Result<Hash, CommitError> {
    Box::pin(commit_impl(
        repository, token, options, keys, values, formats,
    ))
    .await
}

pub async fn commit_impl(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
) -> Result<Hash, CommitError> {
    let reporting = CommitReporting::from_globals();
    let _stats_report = reporting.report();

    if let Some(ref link_path) = options.link {
        return commit_link_only(
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

    let state_staged = State::deserialize(repository.clone(), staged_revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {staged_revision}")
        })?;
    let state_current = State::deserialize(repository.clone(), current_revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {current_revision}")
        })?;

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

    // Snapshot dirty paths before pruning — after the commit, the staged
    // anchor is rebuilt by replaying these against the new revision, same
    // pattern as `state::rebase_staged_anchor`.
    let mut dirty_paths: Vec<RelativePath> = Vec::new();
    state::collect_dirty_only_paths(
        state_staged.clone(),
        repository.clone(),
        ROOT_NODE,
        RelativePathBuf::new(),
        &mut dirty_paths,
    )
    .await
    .forward::<CommitError>("Failed collecting dirty paths from staged state")?;

    lore_debug!("Collected {} dirty paths", dirty_paths.len());

    // Capture the merge parents now — `finalize_commit` will overwrite
    // `parent_self` with the new commit signature, which would prevent us
    // from matching against a `merge_carry` blob below.
    let merge_parent_self = state_staged.parent_self();
    let merge_parent_other = state_staged.parent_other();

    prune_dirty_for_commit(state_staged.clone(), repository.clone()).await?;

    let mut signature = staged_revision;
    if current_revision != staged_revision {
        lore_debug!(
            "Committing revision, current revision {}, staged revision {}",
            state_current.revision(),
            state_staged.revision()
        );
        let (committed, modified_times) = commit_staged_revision(
            repository.clone(),
            token.share(),
            state_current.clone(),
            state_staged.clone(),
            metadata.clone(),
            None,
            link_messages.clone(),
            current_branch,
            reporting.clone(),
        )
        .await?;
        signature = committed;

        finalize_commit(
            repository.clone(),
            &state_current,
            &state_staged,
            branch_latest,
            signature,
            current_branch,
            token,
        )
        .await?;

        modified_times.store(repository.clone()).await;

        event::metadata::send(&metadata);
    }

    if !dry_run && !dirty_paths.is_empty() {
        crate::file::dirty::dirty_relative_paths(repository.clone(), dirty_paths)
            .await
            .forward::<CommitError>("Failed to re-apply dirty paths after commit")?;
    }

    // Apply any merge dirty-tracking carry. `take_matching` only returns
    // paths when the blob's parents match the merge we just committed, and
    // always clears the blob so a stale carry can't outlive this commit.
    if !dry_run {
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
            crate::file::dirty::dirty_relative_paths(repository.clone(), paths)
                .await
                .forward::<CommitError>("Failed to apply merge dirty-tracking carry")?;
        }
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
        let layer_result = commit_staged_revision(
            layer_repository.clone(),
            token.share(),
            layer_state.state_current.clone(),
            layer_state.state_staged.clone(),
            layer_metadata.clone(),
            if !layer.source_path.is_empty() || !layer.target_path.is_empty() {
                Some((layer.source_path.clone(), layer.target_path.clone()))
            } else {
                None
            },
            Arc::new(HashMap::new()),
            current_branch,
            reporting.clone(),
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
            branch::store_latest(
                layer_repository.clone(),
                current_branch,
                layer_previous,
                layer_signature,
                BranchLatestStatus::Divergent,
            )
            .await
            .forward::<CommitError>("Failed to store layer branch latest")?;

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

/// Commits staged changes in a single layer without committing the parent.
///
/// Resolves the layer by `target_path` against the parent's layer config,
/// runs the existing commit pipeline against the layer with proper path
/// remapping, advances the local layer config to point at the new layer
/// revision, and emits a `RevisionCommitRevision` event for the layer.
///
/// The parent's staged anchor and tree state are NOT modified — layer pins
/// live in `.urc/layer.toml`, not in the parent's revision tree.
#[allow(clippy::too_many_arguments)]
async fn commit_layer_only(
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

    let (layer_signature, layer_modified_times) = commit_staged_revision(
        layer_state.repository.clone(),
        token.share(),
        layer_state.state_current.clone(),
        layer_state.state_staged.clone(),
        metadata,
        // Same path-remap guard as the auto-bundle layer commit.
        if !layer.source_path.is_empty() || !layer.target_path.is_empty() {
            Some((layer.source_path.clone(), layer.target_path.clone()))
        } else {
            None
        },
        Arc::new(HashMap::new()),
        parent_current_branch,
        reporting.clone(),
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
        branch::store_latest(
            layer_state.repository.clone(),
            parent_current_branch,
            layer_previous,
            layer_signature,
            BranchLatestStatus::Divergent,
        )
        .await
        .forward::<CommitError>("Failed to store layer branch latest")?;

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
/// After committing the link, updates the parent's link pin and stages the parent
/// state so the updated pin is visible in `lore status`. The parent is not committed.
#[allow(clippy::too_many_arguments)]
async fn commit_link_only(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    message: String,
    link_path: String,
    keys: LoreArray<LoreString>,
    values: LoreArray<LoreString>,
    formats: LoreArray<LoreMetadataType>,
    reporting: CommitReporting,
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
    let state_parent_staged = State::deserialize(repository.clone(), parent_staged_revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {parent_staged_revision}")
        })?;

    // Resolve the link in both staged and current parent states
    let resolved_staged = match link::resolve_link_at_path(
        &state_parent_staged,
        repository.clone(),
        &link_path,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(err) if err.is_not_a_link() => {
            return Err(NotALink { path: link_path }.into());
        }
        Err(_) => {
            return Err(LinkPathNotFound { path: link_path }.into());
        }
    };

    let resolved_current =
        link::resolve_link_at_path(&state_parent_current, repository.clone(), &link_path)
            .await
            .map_err(|err| {
                CommitError::LinkPathNotFound(
                    LinkPathNotFound {
                        path: link_path.clone(),
                    }
                    .chain_err_from(err, "resolving link at path"),
                )
            })?;

    let link_staged_revision = resolved_staged.link_node.linked_node().revision;
    let link_current_revision = resolved_current.link_node.linked_node().revision;
    let link_branch = resolved_staged
        .link_reference
        .resolve_branch(current_branch);

    if link_current_revision == link_staged_revision {
        return Err(NothingStaged.into());
    }

    // Create link repository context with the correct filesystem path
    let mut link_context = repository
        .to_link_context(resolved_staged.link_context.id)
        .await;
    link_context.path = Some(repository.require_path()?.join(&link_path));
    let link_repository = Arc::new(link_context);

    // Determine the effective current revision for the link. The parent's
    // committed pin (`link_current_revision`) may be stale if a prior --link
    // commit advanced the branch without committing the parent. In that case
    // the branch latest IS the correct current revision — the subsequent
    // `commit_staged_revision` call will reject truly stale states via
    // `StagedStaleParent`.
    let link_branch_latest = branch::load_latest(link_repository.clone(), link_branch)
        .await
        .unwrap_or_default();

    let link_effective_current = if !link_branch_latest.is_zero() {
        link_branch_latest
    } else {
        link_current_revision
    };

    let link_state_current = State::deserialize(link_repository.clone(), link_effective_current)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {link_effective_current}")
        })?;
    let link_state_staged = State::deserialize(link_repository.clone(), link_staged_revision)
        .await
        .forward_with::<CommitError, _>(|| {
            format!("Failed to deserialize revision state {link_staged_revision}")
        })?;

    // If a prior --link commit advanced the branch, the staged state's parent
    // still points to the parent's committed pin (stale). Update it to the
    // effective current so commit_staged_revision accepts it.
    if link_state_staged.parent_self() != link_effective_current
        && link_effective_current == link_branch_latest
        && !link_branch_latest.is_zero()
    {
        link_state_staged.set_parent_self(link_effective_current);
    }

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

    let source_path = link_state_staged
        .node_path(link_repository.clone(), resolved_staged.link_node.child)
        .await
        .forward::<CommitError>("Failed to resolve link source path")?;
    let path_remap = if source_path.is_empty() {
        None
    } else {
        Some((source_path, String::new()))
    };

    let (link_signature, link_modified_times) = commit_staged_revision(
        link_repository.clone(),
        token.share(),
        link_state_current.clone(),
        link_state_staged.clone(),
        metadata,
        path_remap,
        Arc::new(HashMap::new()),
        link_branch,
        reporting.clone(),
    )
    .await?;

    finalize_commit(
        link_repository.clone(),
        &link_state_current,
        &link_state_staged,
        link_branch_latest,
        link_signature,
        link_branch,
        &token,
    )
    .await?;

    link_modified_times.store(link_repository.clone()).await;

    // Update the link pin in the OWNING repository's registry (the innermost
    // containing repo for a nested link, the top-level repo otherwise), then
    // fold any intermediate linked revisions up into the top-level staged
    // state. `resolve_link_chain` gives one shared mutable state per level so
    // the pin write and the propagation see the same objects.
    let chain = link::resolve_link_chain(
        repository.clone(),
        state_parent_staged.clone(),
        state_parent_current.clone(),
        RelativePath::from_str(&link_path).unwrap_or_default(),
        current_branch,
    )
    .await
    .forward::<CommitError>("Failed to resolve link chain")?;

    let owner_repository = chain.innermost_repository.clone();
    let owner_state = chain.innermost_state.clone();

    // Locate the link node within the owning repo's staged state.
    let link_local_node = owner_state
        .find_relative_node_link(
            owner_repository.clone(),
            chain.innermost_base_node,
            chain.remainder_path.as_str(),
        )
        .await
        .forward::<CommitError>("Failed to resolve link node in owning repository")?
        .node;

    link::update_link_pin_by_node(
        &owner_state,
        owner_repository.clone(),
        resolved_staged.link_context.id,
        resolved_staged.link_reference.branch,
        link_signature,
        link_local_node,
    )
    .await
    .forward::<CommitError>("Failed to update link pin")?;

    owner_state
        .node_mark(
            owner_repository.clone(),
            link_local_node,
            NodeFlags::Staged,
            true,
        )
        .await
        .forward::<CommitError>("Failed to mark link node as staged")?;

    // For a nested link, each intermediate link between the committed target
    // and the top-level repo must itself be COMMITTED (not just reserialized),
    // so the top-level pin references a real revision on each intermediate
    // branch. Walk the chain inner -> outer: commit each level's child state
    // (which now carries the updated pin), then repin its parent.
    for i in (0..chain.levels.len()).rev() {
        let level = &chain.levels[i];
        let (child_state, child_repository) = chain.child_at(i);

        let child_current = State::deserialize(child_repository.clone(), level.old_signature)
            .await
            .forward::<CommitError>("Failed to deserialize intermediate link current state")?;
        child_state.set_parent_self(level.old_signature);

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

        let (child_signature, child_modified_times) = commit_staged_revision(
            child_repository.clone(),
            token.share(),
            child_current.clone(),
            child_state.clone(),
            child_metadata,
            None,
            Arc::new(HashMap::new()),
            level.branch,
            reporting.clone(),
        )
        .await?;

        let child_branch_latest = branch::load_latest(child_repository.clone(), level.branch)
            .await
            .unwrap_or_default();

        finalize_commit(
            child_repository.clone(),
            &child_current,
            &child_state,
            child_branch_latest,
            child_signature,
            level.branch,
            &token,
        )
        .await?;

        child_modified_times.store(child_repository.clone()).await;

        link::update_link_pin_by_node(
            &level.state,
            level.repository.clone(),
            level.child_repository_id,
            level.branch,
            child_signature,
            level.link_node_id,
        )
        .await
        .forward::<CommitError>("Failed to update intermediate link pin")?;

        level
            .state
            .node_mark(
                level.repository.clone(),
                level.link_node_id,
                NodeFlags::Staged,
                true,
            )
            .await
            .forward::<CommitError>("Failed to mark intermediate link node staged")?;
    }

    if !execution_context().globals().dry_run() {
        let parent_signature = state_parent_staged
            .serialize(repository.clone(), &token)
            .await
            .forward::<CommitError>("Failed to serialize parent state")?;

        crate::instance::store_staged_anchor(&repository, parent_signature)
            .await
            .forward::<CommitError>("Failed to store staged anchor")?;
    }

    Ok(link_signature)
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

/// Stores branch latest, deletes staged anchor, updates last sync if merge,
/// and emits the revision commit event.
///
/// `branch_previous` is what the pointer write compares against, so it must be
/// the tip observed for `branch`, not the revision the caller built on: a prior
/// link commit can advance the branch without committing the parent, which
/// leaves that pin stale.
async fn finalize_commit(
    repository: Arc<RepositoryContext>,
    state_current: &Arc<State>,
    state_staged: &Arc<State>,
    branch_previous: Hash,
    signature: Hash,
    branch: BranchId,
    token: &RepositoryWriteToken,
) -> Result<(), CommitError> {
    let dry_run = execution_context().globals().dry_run();

    if !dry_run {
        store_branch_latest_and_make_current(
            repository.clone(),
            branch_previous,
            signature,
            branch,
        )
        .await?;

        // Check if any dirty-only nodes remain in the staged state.
        // If so, preserve them in a new staged anchor re-parented to the new revision.
        let has_dirty = state_staged
            .node_has_dirty_children(repository.clone(), crate::node::ROOT_NODE)
            .await
            .forward::<CommitError>("Failed deserializing state node block")?;

        if has_dirty {
            lore_debug!("Dirty nodes remain after commit, preserving in new staged anchor");
            state_staged.set_parent_self(signature);
            state_staged.set_revision_number(0);
            state_staged.set_parent_other(Hash::default());
            state_staged.set_metadata_hash(Hash::default());
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

        if state_staged.parent_other() == state_current.revision() && state_staged.is_merge() {
            branch::store_last_sync(repository.clone(), branch, state_staged.parent_self()).await;
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

#[allow(clippy::too_many_arguments)]
async fn commit_staged_revision(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state_current: Arc<State>,
    state_staged: Arc<State>,
    metadata: Arc<Metadata>,
    path_remap: Option<(String, String)>,
    link_messages: Arc<HashMap<String, String>>,
    parent_branch: BranchId,
    reporting: CommitReporting,
) -> Result<(Hash, Arc<RecordedModifiedTimes>), CommitError> {
    let context = execution_context();
    let globals = context.globals();

    let revision_current = state_current.revision();
    if state_staged.parent_self() != revision_current {
        if globals.force() && revision_current == state_current.revision() {
            // Force commit same revision, reset metadata
            state_staged.set_parent_self(revision_current);
            state_staged.set_parent_other(Hash::default());
            state_staged.set_metadata_hash(Hash::default());
        } else if state_staged.parent_other() == revision_current && state_staged.is_merge() {
            // Merge of a divergent branch history
            lore_debug!("Merge of divergent branch history, other parent is current revision");
        } else if state_staged.is_cherry_pick() {
            lore_debug!("Cherry-pick, other parent is empty");
        } else {
            return Err(CommitError::internal("Staged state has stale parent"));
        }
    }

    prune_dirty_for_commit(state_staged.clone(), repository.clone()).await?;

    // Per-operation tracker for background fragment uploads. Write-producing
    // calls below dispatch leader tasks into it; await_all drains every
    // outstanding task before returning — on success AND on any error path.
    // This is the graceful-drain pattern: if an intermediate step fails, we
    // still wait for spawned leaders to terminate before propagating the
    // error so no task outlives this function holding references to scope-
    // bound resources.
    let tracker = immutable::commit_write_tracker(reporting.per_fragment);

    let work_tracker = tracker.clone();
    let modified_times = Arc::new(RecordedModifiedTimes::default());
    let collected = modified_times.clone();
    let work_result: Result<(Hash, Arc<RecordedModifiedTimes>), CommitError> = async move {
        // For each file in repository, create fragments if they don't already
        // exist. Also rehash directories affected by change and generate new
        // root hash.
        commit_files_and_rehash(
            repository.clone(),
            token.share(),
            state_staged.clone(),
            repository.require_path()?,
            metadata.clone(),
            path_remap,
            link_messages,
            parent_branch,
            work_tracker.clone(),
            collected.clone(),
            reporting.stats.clone(),
            reporting.interval,
        )
        .await?;

        let metadata_hash = metadata
            .serialize_with_tracker(repository.clone(), Some(work_tracker.clone()))
            .await
            .forward::<CommitError>("Failed to write commit metadata")?;

        state_staged.set_metadata_hash(metadata_hash);

        let tree_staged = state_staged
            .tree(repository.clone())
            .await
            .forward::<CommitError>("Failed to read revision tree data")?;
        let tree_current = state_current
            .tree(repository.clone())
            .await
            .forward::<CommitError>("Failed to read revision tree data")?;
        if !state_staged.is_merge_or_cherry_pick_or_revert()
            && tree_staged.hash_root == tree_current.hash_root
        {
            if !globals.force() {
                lore_debug!(
                    "Staged tree {} in revision {} is identical to current tree {} in revision {}",
                    tree_staged.hash_root,
                    state_staged.revision(),
                    tree_current.hash_root,
                    state_current.revision(),
                );
                return Err(NothingStaged.into());
            }
            lore_debug!("Force commit of identical merkle tree");
        }

        weave_history(repository.clone(), state_staged.clone()).await?;

        // Reset conflict/merge flags and transient link merge state
        state_staged.reset_merge_conflict_flags();
        state_staged.clear_link_merge_state();

        // Serialize the new current state
        let signature = state_staged
            .serialize(repository.clone(), &token)
            .await
            .forward::<CommitError>("Failed to serialize revision state")?;

        Ok((signature, collected))
    }
    .await;

    // ALWAYS drain the tracker, regardless of whether the work above
    // succeeded. Dropping the tracker while leaders are still running would
    // abort them (JoinSet drop); instead we let them finish (or fail) so the
    // caller never observes a half-committed state.
    let drain_result = tracker.await_all().await;

    // Original work errors take precedence; tracker drain errors surface only
    // when the work itself succeeded.
    match work_result {
        Ok(committed) => {
            drain_result
                .forward::<CommitError>("Background fragment upload task failed during commit")?;
            Ok(committed)
        }
        Err(work_err) => Err(work_err),
    }
}

/// Publish `signature` as `branch`'s tip and anchor it as the current revision.
///
/// `previous` is the tip the caller observed when it decided to commit; the
/// pointer write compares against it, so a branch that advanced in the meantime
/// fails with `BranchAdvanced` instead of orphaning the other committer's
/// revision.
pub async fn store_branch_latest_and_make_current(
    repository: Arc<RepositoryContext>,
    previous: Hash,
    signature: Hash,
    branch: BranchId,
) -> Result<(), CommitError> {
    branch::store_latest(
        repository.clone(),
        branch,
        previous,
        signature,
        BranchLatestStatus::Divergent,
    )
    .await
    .forward::<CommitError>("Failed to store current branch latest")?;

    crate::instance::store_current_anchor(&repository, signature)
        .await
        .forward::<CommitError>("Failed to serialize current revision anchor")?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn commit_files_and_rehash(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    repository_root_path: &Path,
    metadata: Arc<Metadata>,
    path_remap: Option<(String, String)>,
    link_messages: Arc<HashMap<String, String>>,
    parent_branch: BranchId,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    modified_times: Arc<RecordedModifiedTimes>,
    stats: Arc<CommitStats>,
    progress_interval: std::time::Duration,
) -> Result<(), CommitError> {
    lore_info!("Fragmenting files and updating tree hashes");

    let mut relative_path = RelativePath::new();

    let delta = Arc::new(parking_lot::RwLock::new(BytesMut::new()));
    let discard = Arc::new(parking_lot::RwLock::new(vec![]));
    let subnodes_to_discard = Arc::new(parking_lot::RwLock::new(vec![]));

    event::LoreEvent::RevisionCommitBegin(LoreRevisionCommitBeginEventData::default()).send();

    let mut root_path = repository_root_path.to_path_buf();
    let mut root_node = ROOT_NODE;

    if let Some((source_path, target_path)) = path_remap {
        if !target_path.is_empty() {
            root_path.push(target_path);
        }
        if !source_path.is_empty() {
            let node_link = state
                .find_node_link(repository.clone(), source_path.as_str())
                .await
                .unwrap_or_default();
            if !node_link.is_valid() {
                return Err(CommitError::internal("Invalid subpath"));
            }
            if node_link.repository != repository.id {
                // TODO(mjansson): Layers that specify a path living inside a link need
                //                 special snowflake care here to commit from the link and down
                return Err(CommitError::internal("Not supported"));
            }
            root_node = node_link.node;
            relative_path = RelativePath::new_from_initial_path(source_path.as_str())
                .forward::<CommitError>("Invalid subpath")?;
        }
    }

    let (file_tx, file_rx) = mpsc::channel(DEFAULT_WORK_CHANNEL_CAPACITY);

    // Print progress at regular intervals
    let ticker_stats = stats.clone();
    let ticker = AbortOnDropHandle::new(lore_spawn!(async move {
        let mut ticker = tokio::time::interval(progress_interval);
        loop {
            ticker.tick().await;
            event::LoreEvent::RevisionCommitProgress(LoreRevisionCommitProgressEventData {
                count: LoreRevisionCommitCountData::new(ticker_stats.clone()),
            })
            .send();
        }
    }));

    // Producer: discover staged files and directories
    let discover_stats = stats.clone();
    let producer = {
        let repository = repository.clone();
        let token = token.share();
        let state = state.clone();
        let delta = delta.clone();
        let discard = discard.clone();
        let subnodes_to_discard = subnodes_to_discard.clone();
        let stats = stats.clone();
        let tracker = tracker.clone();
        let dir_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DIRECTORY_TASKS));
        lore_spawn!(async move {
            let result = commit_directory(
                repository,
                token,
                state,
                root_path,
                relative_path,
                root_node,
                delta,
                discard,
                subnodes_to_discard,
                file_tx,
                metadata,
                link_messages,
                stats,
                parent_branch,
                tracker,
                dir_semaphore,
            )
            .await;
            discover_stats
                .discovery
                .complete
                .store(true, Ordering::Relaxed);
            // Send a progress event immediately when discovery finishes,
            // ensuring at least one progress event has discoveryComplete=true
            event::LoreEvent::RevisionCommitProgress(LoreRevisionCommitProgressEventData {
                count: LoreRevisionCommitCountData::new(discover_stats),
            })
            .send();
            result
        })
    };

    // Consumer: fragment files as they are discovered
    let consumer = {
        let repository = repository.clone();
        let state = state.clone();
        let delta = delta.clone();
        let stats = stats.clone();
        let tracker = tracker.clone();
        let modified_times = modified_times.clone();
        lore_spawn!(async move {
            commit_execute(
                file_rx,
                repository,
                state,
                delta,
                stats,
                tracker,
                modified_times,
            )
            .await
        })
    };

    let (producer_result, consumer_result) = tokio::join!(producer, consumer);
    producer_result
        .internal("Recursion task failed")
        .map_err(CommitError::from)??;
    consumer_result
        .internal("Recursion task failed")
        .map_err(CommitError::from)??;

    commit_discard_subnodes(
        repository.clone(),
        state.clone(),
        subnodes_to_discard.clone(),
        stats.clone(),
    )
    .await?;

    commit_discard(
        repository.clone(),
        state.clone(),
        delta.clone(),
        discard,
        stats.clone(),
    )
    .await?;

    drop(ticker);

    event::LoreEvent::RevisionCommitEnd(LoreRevisionCommitEndEventData {
        count: LoreRevisionCommitCountData::new(stats.clone()),
    })
    .send();

    rehash_directory(repository.clone(), state.clone(), ROOT_NODE).await?;

    state
        .update_tree_root_hash(repository.clone())
        .await
        .forward::<CommitError>("Failed to update tree root hash")?;
    lore_debug!("Rehashed merkle tree");

    generate_delta_block(repository.clone(), state.clone(), delta, tracker).await?;

    Ok(())
}

struct FileToCommit {
    node_id: NodeID,
    absolute_path: PathBuf,
    relative_path: RelativePath,
}

#[allow(clippy::too_many_arguments)]
async fn commit_directory(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    node_id: NodeID,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    discard: Arc<parking_lot::RwLock<Vec<u32>>>,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    file_tx: mpsc::Sender<FileToCommit>,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    stats: Arc<CommitStats>,
    parent_branch: BranchId,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    dir_semaphore: Arc<Semaphore>,
) -> Result<(), CommitError> {
    let node_index = Node::index(node_id);
    let block = state
        .block_with_nametable(repository.clone(), NodeBlock::index(node_id))
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    let node = block.node(node_index);

    debug_assert!(node.is_directory());
    lore_trace!(
        "Committing directory node {} {} flags 0x{:x}",
        node_id,
        relative_path.as_str(),
        node.flags
    );

    if node.is_staged() {
        delta_add(delta.clone(), node_id, node.flags);
    }

    let mut updated = false;
    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), node_id)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;

    let mut tasks = JoinSet::new();
    let mut walk_failure = None;

    loop {
        let child = match children
            .next()
            .await
            .forward::<CommitError>("Failed deserializing state block")
        {
            Ok(Some(child)) => child,
            Ok(None) => break,
            Err(err) => {
                walk_failure = Some(err);
                break;
            }
        };
        let (child_node_id, child_node, node_name) = child;

        if !child_node.is_staged() {
            continue;
        }

        debug_assert!(node.is_directory());
        lore_trace!("Committing directory node {node_id} child {child_node_id}");

        let relative_path = relative_path.push_into_buf(&node_name).freeze();
        let absolute_path = absolute_path.join(node_name);

        if child_node.is_staged_delete() {
            if child_node.is_directory() {
                lore_spawn!(tasks, {
                    let repository = repository.clone();
                    let state = state.clone();
                    let delta = delta.clone();
                    let subnodes_to_discard = subnodes_to_discard.clone();
                    let stats = stats.clone();
                    async move {
                        collect_discard_subnodes(
                            repository,
                            state,
                            delta,
                            child_node_id,
                            subnodes_to_discard,
                            stats,
                        )
                        .await
                    }
                });
                updated = true;
            }
            discard.write().push(child_node_id);
            if child_node.flags & NodeFlags::File != 0 {
                stats.complete.file_total.fetch_add(1, Ordering::Relaxed);
            } else {
                stats
                    .complete
                    .directory_total
                    .fetch_add(1, Ordering::Relaxed);
            }
        } else if child_node.is_directory() {
            // Inline fallback rather than a blocking acquire: a parent awaiting
            // its children must never block on a permit a descendant needs, or
            // the bounded fan-out would deadlock.
            match dir_semaphore.clone().try_acquire_owned() {
                Ok(permit) => {
                    lore_spawn!(tasks, {
                        let repository = repository.clone();
                        let token = token.share();
                        let state = state.clone();
                        let delta = delta.clone();
                        let discard = discard.clone();
                        let subnodes_to_discard = subnodes_to_discard.clone();
                        let file_tx = file_tx.clone();
                        let metadata = metadata.clone();
                        let link_messages = link_messages.clone();
                        let stats = stats.clone();
                        let tracker = tracker.clone();
                        let dir_semaphore = dir_semaphore.clone();
                        async move {
                            let _permit = permit;
                            commit_directory_recurse(
                                repository,
                                token,
                                state,
                                absolute_path,
                                relative_path,
                                child_node_id,
                                delta,
                                discard,
                                subnodes_to_discard,
                                file_tx,
                                metadata,
                                link_messages,
                                stats,
                                parent_branch,
                                tracker,
                                dir_semaphore,
                            )
                            .await
                        }
                    });
                }
                Err(_) => {
                    if let Err(err) = commit_directory_recurse(
                        repository.clone(),
                        token.share(),
                        state.clone(),
                        absolute_path,
                        relative_path,
                        child_node_id,
                        delta.clone(),
                        discard.clone(),
                        subnodes_to_discard.clone(),
                        file_tx.clone(),
                        metadata.clone(),
                        link_messages.clone(),
                        stats.clone(),
                        parent_branch,
                        tracker.clone(),
                        dir_semaphore.clone(),
                    )
                    .await
                    {
                        walk_failure = Some(err);
                        break;
                    }
                }
            }
        } else if child_node.is_link() {
            lore_debug!(
                "Before committing link node, parent node {} address {}",
                node_id,
                node.address
            );

            lore_spawn!(tasks, {
                let repository = repository.clone();
                let token = token.share();
                let state = state.clone();
                let delta = delta.clone();
                let metadata = metadata.clone();
                let link_messages = link_messages.clone();
                let stats = stats.clone();
                // commit_link builds its own per-link tracker so it can drain
                // before emitting the sub-repo's RevisionCommitRevision event;
                // the parent tracker is passed only to inherit its observer.
                let parent_tracker = tracker.clone();
                async move {
                    commit_link_node(
                        repository,
                        token,
                        state,
                        child_node_id,
                        absolute_path,
                        relative_path,
                        delta,
                        metadata,
                        link_messages,
                        stats,
                        parent_branch,
                        parent_tracker,
                    )
                    .await
                }
            });
        } else if child_node.is_file() {
            if let Err(err) = collect_file(
                child_node_id,
                absolute_path,
                relative_path,
                &file_tx,
                &stats,
                child_node.size,
            )
            .await
            {
                walk_failure = Some(err);
                break;
            }
            updated = true;
        }
    }

    // Wait for all tasks to finish and gather any errors
    let mut task_failure = Ok(());
    let mut commit_failure = Ok(());
    while let Some(task) = tasks.join_next().await {
        if let Ok(result) = task {
            if result.is_err() {
                commit_failure = result;
            }
        } else {
            task_failure = Err(task.unwrap_err());
        }
    }
    if let Some(err) = walk_failure {
        return Err(err);
    }
    commit_failure?;
    task_failure.internal("Recursion task failed")?;

    if updated {
        stats
            .complete
            .directory_count
            .fetch_add(1, Ordering::Relaxed);
        stats
            .complete
            .directory_total
            .fetch_add(1, Ordering::Relaxed);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_directory_recurse(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    node_id: NodeID,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    discard: Arc<parking_lot::RwLock<Vec<u32>>>,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    file_tx: mpsc::Sender<FileToCommit>,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    stats: Arc<CommitStats>,
    parent_branch: BranchId,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    dir_semaphore: Arc<Semaphore>,
) -> Pin<Box<dyn Future<Output = Result<(), CommitError>> + Send>> {
    Box::pin(commit_directory(
        repository,
        token,
        state,
        absolute_path,
        relative_path,
        node_id,
        delta,
        discard,
        subnodes_to_discard,
        file_tx,
        metadata,
        link_messages,
        stats,
        parent_branch,
        tracker,
        dir_semaphore,
    ))
}

async fn collect_discard_subnodes(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    node_id: NodeID,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    stats: Arc<CommitStats>,
) -> Result<(), CommitError> {
    lore_trace!("Committing raw deletion of node {node_id} children");

    debug_assert!({
        let node = state
            .node(repository.clone(), node_id)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
        node.is_directory()
    });
    let mut iter = StateNodeChildrenIterator::new(state.clone(), repository.clone(), node_id)
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    while let Some((child_node_id, _child_node)) = iter
        .next()
        .await
        .forward::<CommitError>("Failed deserializing state block")?
    {
        lore_trace!("Committing raw deletion of node {child_node_id}");
        let delta = delta.clone();
        let subnodes_to_discard = subnodes_to_discard.clone();
        let stats = stats.clone();
        state::node_discard_nopatch(
            state.clone(),
            repository.clone(),
            child_node_id,
            true,
            false,
            move |discarded_node_id, flags| {
                delta_add(delta.clone(), discarded_node_id, flags);
                subnodes_to_discard.write().push(discarded_node_id);
                if flags & NodeFlags::File != 0 {
                    stats.complete.file_total.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats
                        .complete
                        .directory_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            },
        )
        .await
        .forward::<CommitError>("Failed discarding node staged for delete")?;
    }

    Ok(())
}

async fn commit_discard_subnodes(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    stats: Arc<CommitStats>,
) -> Result<(), CommitError> {
    let subnodes_to_discard = {
        let lock = subnodes_to_discard.read();
        lock.clone()
    };

    let mut tasks = JoinSet::new();

    for subnode_to_discard in subnodes_to_discard.iter() {
        lore_spawn!(tasks, {
            let state = state.clone();
            let repository = repository.clone();
            let node_to_discard = *subnode_to_discard;
            let stats = stats.clone();
            async move {
                state::node_discard_nopatch(
                    state,
                    repository,
                    node_to_discard,
                    false,
                    true,
                    move |_discarded_node_id, flags| {
                        if flags & NodeFlags::File != 0 {
                            stats.complete.file_count.fetch_add(1, Ordering::Relaxed);
                            stats
                                .complete
                                .file_delete_count
                                .fetch_add(1, Ordering::Relaxed);
                        } else {
                            stats
                                .complete
                                .directory_count
                                .fetch_add(1, Ordering::Relaxed);
                            stats
                                .complete
                                .directory_delete_count
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    },
                )
                .await
                .forward::<CommitError>("Failed discarding node staged for delete")
            }
        });
    }

    // Wait for all tasks to finish and gather any errors
    let mut task_failure = Ok(());
    let mut commit_failure = Ok(state::DiscardCounts::default());
    while let Some(task) = tasks.join_next().await {
        if let Ok(result) = task {
            if result.is_err() {
                commit_failure = result;
            }
        } else {
            task_failure = Err(task.unwrap_err());
        }
    }
    commit_failure?;
    task_failure.internal("Recursion task failed")?;

    Ok(())
}

async fn commit_discard(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    discard: Arc<parking_lot::RwLock<Vec<u32>>>,
    stats: Arc<CommitStats>,
) -> Result<(), CommitError> {
    let nodes = {
        let lock = discard.read();
        lock.clone()
    };
    if nodes.is_empty() {
        return Ok(());
    }

    lore_debug!("Committing patched deletions of {} nodes", nodes.len());

    for node_id in nodes.iter() {
        lore_trace!("Committing patched deletion of node {}", *node_id);
        let delta = delta.clone();
        let stats = stats.clone();
        state::node_discard_patch(
            state.clone(),
            repository.clone(),
            *node_id,
            move |discarded_node_id, flags| {
                delta_add(delta.clone(), discarded_node_id, flags);
                if flags & NodeFlags::File != 0 {
                    stats.complete.file_count.fetch_add(1, Ordering::Relaxed);
                    stats
                        .complete
                        .file_delete_count
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    stats
                        .complete
                        .directory_count
                        .fetch_add(1, Ordering::Relaxed);
                    stats
                        .complete
                        .directory_delete_count
                        .fetch_add(1, Ordering::Relaxed);
                }
            },
        )
        .await
        .forward::<CommitError>("Failed discarding node staged for delete")?;
    }
    Ok(())
}

async fn collect_file(
    node_id: NodeID,
    absolute_path: PathBuf,
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
    if file_tx
        .send(FileToCommit {
            node_id,
            absolute_path,
            relative_path,
        })
        .await
        .is_err()
    {
        return Err(CommitError::internal("Recursion task failed"));
    }
    Ok(())
}

/// Folds a finished [`commit_file`] task into the first failure seen.
fn collect_committed_file(
    result: Result<Result<(), CommitError>, tokio::task::JoinError>,
    failure: &mut Option<CommitError>,
) {
    if let Err(err) = result
        .internal("Recursion task failed")
        .map_err(CommitError::from)
        .flatten()
        && failure.is_none()
    {
        *failure = Some(err);
    }
}

async fn commit_execute(
    mut file_rx: mpsc::Receiver<FileToCommit>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    stats: Arc<CommitStats>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    modified_times: Arc<RecordedModifiedTimes>,
) -> Result<(), CommitError> {
    const MAX_CONCURRENT_TASKS: usize = 10000;
    let mut tasks = JoinSet::new();
    let mut commit_failure = None;

    while let Some(file_to_commit) = file_rx.recv().await {
        lore_spawn!(tasks, {
            let repository = repository.clone();
            let state = state.clone();
            let delta = delta.clone();
            let stats = stats.clone();
            let tracker = tracker.clone();
            let modified_times = modified_times.clone();
            async move {
                let block_index = NodeBlock::index(file_to_commit.node_id);
                let node_index = Node::index(file_to_commit.node_id);
                let block = state
                    .block(repository.clone(), block_index)
                    .await
                    .forward::<CommitError>("Failed deserializing state block")?;
                commit_file(
                    repository,
                    state,
                    file_to_commit.node_id,
                    block,
                    block_index,
                    node_index,
                    file_to_commit.absolute_path,
                    file_to_commit.relative_path,
                    delta,
                    stats,
                    tracker,
                    modified_times,
                )
                .await
            }
        });

        while let Some(result) = tasks.try_join_next() {
            collect_committed_file(result, &mut commit_failure);
        }
        while tasks.len() > MAX_CONCURRENT_TASKS
            && let Some(result) = tasks.join_next().await
        {
            collect_committed_file(result, &mut commit_failure);
        }

        if commit_failure.is_some() {
            break;
        }
    }

    while let Some(result) = tasks.join_next().await {
        collect_committed_file(result, &mut commit_failure);
    }

    if let Some(err) = commit_failure {
        Err(err)
    } else {
        Ok(())
    }
}

/// Metadata of the file about to be fragmented, read before its content rather than after.
///
/// A file written to while it is being fragmented then yields a time older than the content
/// that was committed, costing that one file a hash on the next scan. A time read afterwards
/// would describe content that was never committed, and the next scan would trust it.
async fn metadata_before_fragmenting(path: &Path) -> Result<std::fs::Metadata, CommitError> {
    Ok(lore_io::IoDriver::global()
        .metadata(path)
        .await
        .internal_with(|| format!("Failed to get metadata for file {}", path.display()))?)
}

/// Commits one file node, recording the modified time it read the file at. A view-excluded
/// path records nothing: its content is taken from the staged node rather than disk.
#[allow(clippy::too_many_arguments)]
async fn commit_file(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    block: Arc<NodeBlock>,
    block_index: usize,
    node_index: usize,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    stats: Arc<CommitStats>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
    modified_times: Arc<RecordedModifiedTimes>,
) -> Result<(), CommitError> {
    let mut node = { *block.read().node(node_index) };

    debug_assert!(node.is_file());

    if node.is_staged_merge_conflict() {
        // Check if it's resolved on the node level before going to disk
        if !node.is_staged_merge_resolved() {
            return Err(Conflict {
                path: relative_path.as_str().to_string(),
            }
            .into());
        }
        // Check if file has conflict markers remaining
        if infer::infer_is_conflicted_by_path(absolute_path.as_path())
            .await
            .internal_with(|| format!("Failed reading file {}", relative_path.as_str()))?
        {
            return Err(Conflict {
                path: relative_path.as_str().to_string(),
            }
            .into());
        }
        // Clean up theirs/base files
        if !execution_context().globals().dry_run() {
            sync::unlink_merge_mine_theirs_base(absolute_path.as_path()).await;
        }
    }

    lore_trace!(
        "Committing file node {} {} flags 0x{:x}",
        node_id,
        relative_path.as_str(),
        node.flags
    );

    // A sparse checkout has no working-tree file for a view-excluded path, so
    // the staged content hash is the only source. Re-fragmenting would read a
    // file that is absent or stale.
    //
    // This test must stay identical to the one realize gates disk writes on
    // (`fs/realize.rs`), because that is what decided whether the file was
    // written.
    let (address, content_size, mode, modified_time) =
        if repository
            .filter
            .excludes(&relative_path, false, FilterMode::View)
        {
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

            let metadata = metadata_before_fragmenting(absolute_path.as_path()).await?;

            let (address, size_content) = immutable::write_from_file_with_tracker(
                repository.clone(),
                absolute_path.as_path(),
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

            stats.file_read(size_content);

            (
                address,
                size_content,
                util::fs::metadata_to_mode(&metadata, node.mode),
                Some(util::fs::file_mtime(&metadata)),
            )
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

        let block_dirtied = {
            let mut block_writer = block.write();
            let node = block_writer.node(node_index);

            node.address = address;
            node.size = content_size;
            node.mode = mode;
            node.child = 0;

            node.clear_all_change_flags();

            block_writer.mark_dirty()
        };
        if block_dirtied {
            state.block_modified(block.clone(), block_index);
            state.mark_dirty();
        }
    } else if node.is_staged() {
        lore_trace!("Reset staged flag on node {} {}", node_id, relative_path);

        let block_dirtied = {
            let mut block_writer = block.write();
            let node = block_writer.node(node_index);
            node.clear_all_change_flags();

            block_writer.mark_dirty()
        };
        if block_dirtied {
            state.block_modified(block.clone(), block_index);
            state.mark_dirty();
        }
    }

    if let Some(modified_time) = modified_time {
        modified_times.record(&repository, &relative_path, modified_time);
    }

    stats.complete.file_count.fetch_add(1, Ordering::Relaxed);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn commit_link_node(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    node_id: NodeID,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    stats: Arc<CommitStats>,
    parent_branch: BranchId,
    parent_tracker: Arc<lore_storage::write_tracker::WriteTracker>,
) -> Result<(), CommitError> {
    // First check if this was an initial add of the link
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);

    let block = state
        .block(repository.clone(), block_index)
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    let node = block.node(node_index);

    if node.is_staged_add() {
        lore_trace!("Reset staged add flag on link node {relative_path}");

        delta_add(delta, node_id, node.flags);

        let block_dirtied = {
            let mut block_writer = block.write();
            let node = block_writer.node(node_index);
            node.clear_all_change_flags();

            block_writer.mark_dirty()
        };
        if block_dirtied {
            state.block_modified(block.clone(), block_index);
            state.mark_dirty();
        }
        return Ok(());
    }

    // Link, recurse and check if there are changes inside the link to commit, and if so update the
    // target repository hash and potentially target node index in this node data

    let link = node.linked_node();
    let link_repository = Arc::new(repository.to_link_context(link.repository).await);
    let signature = link.revision;
    let link_node = link.node;
    let link_state = State::deserialize(link_repository.clone(), signature)
        .await
        .forward::<CommitError>("Failed to deserialize link state")?;

    let link_reference = state
        .link_find(repository.clone(), link.repository, node_id)
        .await
        .forward::<CommitError>("Commit of link node failed")?;

    let branch_id = link_reference.resolve_branch(parent_branch);

    let link_metadata = if let Some(link_msg) = link_messages.get(relative_path.as_str()) {
        let mut overridden = (*metadata).clone();
        overridden
            .set_string(metadata::MESSAGE, link_msg)
            .forward::<CommitError>("Failed to set metadata")?;
        Arc::new(overridden)
    } else {
        metadata.clone()
    };

    let (link_latest, link_node_id) = commit_link(
        link_repository.clone(),
        token,
        link_state,
        absolute_path,
        relative_path,
        link_node,
        branch_id,
        signature,
        link_metadata,
        link_messages,
        stats,
        parent_tracker,
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

    delta_add(delta, node_id, node.flags);

    // Clear staged bits before updating the link pin
    let block_dirtied = {
        let mut block_writer = block.write();
        let node = block_writer.node(node_index);
        node.flags &= !NodeFlags::StagedBits;
        block_writer.mark_dirty()
    };

    if block_dirtied {
        state.block_modified(block.clone(), block_index);
        state.mark_dirty();
    }

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

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn commit_link(
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    state: Arc<State>,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    node_id: NodeID,
    branch: BranchId,
    current_revision: Hash,
    metadata: Arc<Metadata>,
    link_messages: Arc<HashMap<String, String>>,
    stats: Arc<CommitStats>,
    parent_tracker: Arc<lore_storage::write_tracker::WriteTracker>,
) -> Result<(Hash, NodeID), CommitError> {
    // Per-link tracker: a linked sub-repo is a logically independent commit
    // and must carry its own durability gate. If we reused the parent's
    // tracker, the sub-repo's RevisionCommitRevision would fire before the
    // parent drained — RevisionCommitRevision must only be emitted after
    // tracker.await_all succeeds AND the branch pointer is updated.
    let link_tracker = Arc::new(parent_tracker.new_like());

    let node_path = state
        .node_path(repository.clone(), node_id)
        .await
        .forward::<CommitError>("Failed to get link node path")?;

    lore_debug!(
        "Committing link node_id {node_id} at relpath {}, abspath {}, nodepath {}",
        relative_path.to_string(),
        absolute_path.to_str().unwrap_or_default(),
        node_path
    );

    prune_dirty_for_commit(state.clone(), repository.clone()).await?;

    // Observed before any write, so the pointer write below compares against the
    // tip this commit was built on rather than one a racing committer published.
    let link_previous = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();

    // Graceful-drain pattern mirroring commit_staged_revision: run all
    // write-producing work under `work_tracker`, then ALWAYS drain so leaders
    // never outlive this function. Only on drain success AND branch-pointer
    // success do we emit the event. `Ok(None)` signals the "unchanged link"
    // case — no new revision, no event.
    let work_tracker = link_tracker.clone();
    let link_modified_times = Arc::new(RecordedModifiedTimes::default());
    let work_result: Result<Option<Hash>, CommitError> = {
        let repository = repository.clone();
        let state = state.clone();
        let metadata = metadata.clone();
        let link_messages = link_messages.clone();
        let stats = stats.clone();
        let link_modified_times = link_modified_times.clone();
        async move {
            let delta = Arc::new(parking_lot::RwLock::new(BytesMut::new()));
            let discard = Arc::new(parking_lot::RwLock::new(vec![]));
            let subnodes_to_discard = Arc::new(parking_lot::RwLock::new(vec![]));
            let (file_tx, file_rx) = mpsc::channel(DEFAULT_WORK_CHANNEL_CAPACITY);

            // A linked sub-repo becomes its own `parent_branch` for everything
            // it recurses into — nested operations must anchor against the
            // link's branch, not the outer repo's.
            let link_branch = branch;

            let producer = {
                let repository = repository.clone();
                let token = token.share();
                let state = state.clone();
                let delta = delta.clone();
                let discard = discard.clone();
                let subnodes_to_discard = subnodes_to_discard.clone();
                let metadata = metadata.clone();
                let link_messages = link_messages.clone();
                let stats = stats.clone();
                let tracker = work_tracker.clone();
                // Own budget per linked sub-repo, isolated from the parent's.
                let dir_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DIRECTORY_TASKS));
                lore_spawn!(async move {
                    commit_directory_recurse(
                        repository,
                        token,
                        state,
                        absolute_path,
                        relative_path,
                        node_id,
                        delta,
                        discard,
                        subnodes_to_discard,
                        file_tx,
                        metadata,
                        // Per-link messages are keyed by full mount path, so a
                        // nested link (e.g. `vendor/b/vendor/c`) still receives
                        // its `--link-message`; it falls back to the main
                        // message when no entry matches.
                        link_messages,
                        stats,
                        link_branch,
                        tracker,
                        dir_semaphore,
                    )
                    .await
                })
            };

            let consumer = {
                let repository = repository.clone();
                let state = state.clone();
                let delta = delta.clone();
                let stats = stats.clone();
                let tracker = work_tracker.clone();
                let modified_times = link_modified_times.clone();
                lore_spawn!(async move {
                    commit_execute(
                        file_rx,
                        repository,
                        state,
                        delta,
                        stats,
                        tracker,
                        modified_times,
                    )
                    .await
                })
            };

            let (producer_result, consumer_result) = tokio::join!(producer, consumer);
            producer_result
                .internal("Recursion task failed")
                .map_err(CommitError::from)??;
            consumer_result
                .internal("Recursion task failed")
                .map_err(CommitError::from)??;

            commit_discard_subnodes(
                repository.clone(),
                state.clone(),
                subnodes_to_discard.clone(),
                stats.clone(),
            )
            .await?;

            commit_discard(
                repository.clone(),
                state.clone(),
                delta.clone(),
                discard.clone(),
                stats.clone(),
            )
            .await?;

            {
                let read = delta.read();
                let delta_count = read.count::<NodeDelta>();

                if delta_count == 0 {
                    lore_debug!("Link is unchanged, skipping rehash");
                    return Ok(None);
                }
            };

            rehash_directory(repository.clone(), state.clone(), ROOT_NODE).await?;

            state
                .update_tree_root_hash(repository.clone())
                .await
                .forward::<CommitError>("Failed to update tree root hash")?;

            generate_delta_block(
                repository.clone(),
                state.clone(),
                delta,
                work_tracker.clone(),
            )
            .await?;

            state.set_metadata_hash(
                metadata
                    .serialize_with_tracker(repository.clone(), Some(work_tracker))
                    .await
                    .forward::<CommitError>("Failed to write commit metadata")?,
            );

            weave_history(repository.clone(), state.clone()).await?;

            state.reset_merge_conflict_flags();
            state.clear_link_merge_state();

            let signature = state
                .serialize(repository.clone(), &token)
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

    // Unchanged link: no new revision, no event to emit. Return early with
    // the pre-existing revision/node so the parent keeps its current pin.
    let signature = match signature_opt {
        Some(sig) => sig,
        None => return Ok((current_revision, node_id)),
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

    let node_link = state
        .find_node_link(repository.clone(), &node_path)
        .await
        .forward::<CommitError>("Failed to find link node")?;

    link_modified_times.store(repository).await;

    Ok((signature, node_link.node))
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable)]
struct NodeHashData {
    name_hash: u64,
    mode: u32,
    address: Address,
    padding: u32,
    size: u64,
}

impl NodeHashData {
    fn from_node(node: &Node) -> Self {
        NodeHashData {
            name_hash: node.name_hash,
            mode: node.mode as u32,
            address: node.address,
            padding: 0,
            size: node.size,
        }
    }
}

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

    let mut child_data: Vec<NodeHashData> = vec![];

    let mut total_size = 0;
    let mut children = StateNodeChildrenIterator::new(state.clone(), repository.clone(), node_id)
        .await
        .forward::<CommitError>("Failed deserializing state block")?;

    let mut tasks = JoinSet::new();
    let mut walk_failure = None;

    loop {
        let child = match children
            .next()
            .await
            .forward::<CommitError>("Failed deserializing state block")
        {
            Ok(Some(child)) => child,
            Ok(None) => break,
            Err(err) => {
                walk_failure = Some(err);
                break;
            }
        };
        let (child_node_id, child_node) = child;

        if child_node.is_staged_delete() {
            let node_path = state
                .node_path(repository.clone(), child_node_id)
                .await
                .unwrap_or_default();
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
                state
                    .node_path(repository.clone(), child_node_id)
                    .await
                    .unwrap_or_default(),
                child_node.flags,
                child_node.address
            );
            walk_failure = Some(CommitError::internal(
                "Dirty node remain after nodes were committed",
            ));
            break;
        }

        if child_node.is_directory() {
            lore_trace!("Spawning rehash of directory node {}", child_node_id);
            lore_spawn!(tasks, {
                let repository = repository.clone();
                let state = state.clone();
                async move { rehash_directory_recurse(repository, state, child_node_id).await }
            });
        } else {
            if child_node.is_staged() {
                lore_debug!(
                    "Node {} {} staged flags not cleared 0x{:x} address {}",
                    child_node_id,
                    state
                        .node_path(repository.clone(), child_node_id)
                        .await
                        .unwrap_or_default(),
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
                "File node {} mode 0o{:o} size {} flags 0x{:x} address {}",
                child_node_id,
                child_node.mode,
                child_node.size,
                child_node.flags,
                child_node.address
            );

            total_size += child_node.size;

            child_data.push(NodeHashData::from_node(&child_node));
        }
    }

    let mut task_failure = Ok(());
    while let Some(task) = tasks.join_next().await {
        match task {
            Ok(Ok(result)) => {
                total_size += result.size;
                child_data.push(result);
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

    child_data.sort_unstable_by_key(|lhs| lhs.name_hash);

    let mut hasher = blake3::Hasher::new();
    for data in child_data.iter() {
        // Assumes little endian
        hasher.update(data.as_bytes());
    }
    let blake3_hash = hasher.finalize();

    let block_dirtied = {
        let mut block_writer = block.write();
        let node = block_writer.node(node_index);
        let prev_hash = node.address.hash;
        node.address.hash = Hash::from(*blake3_hash.as_bytes());
        node.size = total_size;
        node.flags &= !NodeFlags::StagedBits;

        lore_trace!(
            "Node {} rehashed, mode 0o{:o} size {} flags 0x{:x} hash {} (previous {})",
            node_id,
            node.mode,
            node.size,
            node.flags,
            node.address.hash,
            prev_hash
        );

        block_writer.mark_dirty()
    };
    if block_dirtied {
        state.block_modified(block.clone(), block_index);
        state.mark_dirty();
    }

    Ok(())
}

fn rehash_directory_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> Pin<Box<dyn Future<Output = Result<NodeHashData, CommitError>> + Send>> {
    Box::pin(async move {
        rehash_directory(repository.clone(), state.clone(), node_id).await?;

        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
        let node = block.node(node_index);

        Ok(NodeHashData::from_node(&node))
    })
}

fn delta_add(delta: Arc<parking_lot::RwLock<BytesMut>>, node_id: NodeID, flags: u16) {
    let node_delta = NodeDelta::from_node_and_flags(node_id, flags);
    lore_trace!("Record node delta {node_delta:?}");
    delta.write().extend_from_slice(node_delta.as_bytes());
}

async fn generate_delta_block(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
) -> Result<(), CommitError> {
    let delta = {
        let mut lock = delta.write();
        lock.split().freeze()
    };
    let delta_count = delta.count::<NodeDelta>();

    let address = if !delta.is_empty() {
        immutable::write_with_tracker(
            repository.clone(),
            Context::default(),
            delta,
            immutable::write_options_from_repository(repository.clone())
                .with_local_cache_priority()
                .with_max_size_chunk(),
            Some(tracker),
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

pub(crate) async fn weave_history(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
) -> Result<(), CommitError> {
    let [parent_self, parent_other] = state.parents();
    let mut history_count = 0;
    let mut revision_number = 0;

    if !parent_self.is_zero() {
        lore_debug!("Weave branch history");
        let state_parent = State::deserialize(repository.clone(), parent_self)
            .await
            .forward_with::<CommitError, _>(|| {
                format!("Failed deserializing parent state {parent_self} when weaving history")
            })?;

        revision_number = std::cmp::max(state_parent.revision_number(), revision_number);

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

                node.revision[0] = parent_self;
                node.revision[1] = Hash::default();
                node.node[0] = node_id;
                node.node[1] = 0;
                node.action[0] = delta.action;
                node.flags[0] = delta.flags;
                node.action[1] = change::FileAction::Keep as u16;
                node.flags[1] = NodeFileMetadataFlags::NoFlag.bits();
                history_count += 1;

                lore_trace!(
                    "History {}/{}: node {} {:?}",
                    idelta,
                    delta_slice.len(),
                    node_id,
                    node
                );

                block_writer.mark_dirty()
            };

            if dirtied {
                state.block_file_metadata_modified(file_metadata_block, block_index);
                state.mark_dirty();
            }

            idelta += 1;
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

        // First get the file history as seen by the branch being merged in (the other parent) state
        // for all nodes affected by the merge (which is the set of nodes in this revision delta block)
        let delta_buffer = state
            .delta_block(repository.clone())
            .await
            .forward::<CommitError>(
                "Failed deserializing parent state delta block when weaving history",
            )?
            .to_aligned::<NodeDelta>();
        let delta_slice = delta_buffer.as_type_slice::<NodeDelta>();
        for (idelta, delta) in delta_slice.iter().enumerate() {
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

                let node_parent = state_parent
                    .find_node_link(repository.clone(), node_path.as_str())
                    .await
                    .unwrap_or_default();
                if !node_parent.is_valid() {
                    // Node does not exist in the branch being merged in
                    continue;
                }

                let metadata_node = node::node_to_file_metadata(node_parent.node);
                let block_index = NodeFileMetadataBlock::index(metadata_node);
                let node_index = NodeFileMetadata::index(metadata_node);

                let file_metadata_block = state_parent
                    .block_file_metadata(repository.clone(), block_index)
                    .await
                    .forward::<CommitError>("Failed deserializing file metadata block")?;

                let merge_metadata = *file_metadata_block.read().node(node_index);

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
                    node.node[1] = node_parent.node;
                    node.flags[1] |= merge_metadata.flags[0] | change::Flags::Merge.bits();
                    node.action[1] = merge_metadata.action[0];

                    lore_trace!(
                        "History {}/{}: node {} {:?}",
                        idelta,
                        delta_slice.len(),
                        node_id,
                        node
                    );

                    block_writer.mark_dirty()
                };

                if dirtied {
                    state.block_file_metadata_modified(file_metadata_block, block_index);
                    state.mark_dirty();
                }
            }
        }

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

            let node_link = state
                .find_node_link(repository.clone(), node_path.as_str())
                .await
                .unwrap_or_default();
            if !node_link.node.is_valid_node_id() {
                // This node does not exist in the current state, it was deleted as part of the
                // merge conflict resolution, or deleted in the incoming branch revision.
                // TODO(mjansson): For the rename case where the node as renamed/moved as part
                //                 of the merge and now exist in a new node in current state
                //                 with a new name, we need to find the correct target node
                lore_debug!(
                    "Ignore weaving history for path that does not exist in current state: {node_path}"
                );
                continue;
            }

            let current_node_id = node_link.node;
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
                state.mark_dirty();
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
            Ok(conn) => !conn.auth_url.is_empty(),
            Err(lore_transport::ProtocolError::NoRemote(_)) => false,
            Err(_) => true,
        };
        if require_identity {
            return Err(MissingIdentity.into());
        }
    } else {
        metadata
            .set_string(metadata::CREATED_BY, &commit_user)
            .forward::<CommitError>("Failed setting revision metadata")?;
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

/// Discard dirty-only-add subtrees and clear dirty flags on every other
/// dirty-only node before the commit pipeline builds the new revision.
///
/// `state_staged` doubles as the source tree for the new committed revision
/// and the carrier of staged-anchor tracking. Dirty-only nodes belong only to
/// the latter — they were marked by `lore dirty <path>` (or `lore status
/// --scan`) but never staged, so the committed merkle tree must not reference
/// them. After pruning, the staged tracking is rebuilt against the new
/// committed revision by re-running `file::dirty::dirty()` on the dirty paths
/// captured beforehand (mirroring `state::rebase_staged_anchor`).
pub(crate) async fn prune_dirty_for_commit(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
) -> Result<(), CommitError> {
    let _ = prune_dirty_recurse(state, repository, ROOT_NODE).await?;
    Ok(())
}

/// Post-order walk: returns `true` when the caller should patch-discard this
/// node from its parent's child chain. Post-order is required so empty
/// intermediate directories can collapse upward after their dirty-add
/// children have already been removed.
///
/// Only dirty children are descended into: `node_mark_dirty` propagates the
/// base Dirty bit up to the root, so a clean child cannot have dirty
/// descendants (the same invariant `collect_dirty_paths` walks by). Clean
/// subtrees are untouched — in particular a clean committed empty directory
/// is no longer visited, so the empty-directory collapse below only applies
/// to directories on dirty paths.
///
/// `clear_dirty_flags` preserves Staged and the action bits when Staged is set,
/// so clearing both restores dirty-only nodes and strips a stale Dirty
/// propagation bit that `rehash_directory` would otherwise leave on a staged
/// parent. An intermediate directory created solely to host dirty-add children
/// is discarded once emptied, so the parent's rehash does not pull in an
/// orphaned dirty-only directory.
fn prune_dirty_recurse(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    node_id: NodeID,
) -> Pin<Box<dyn Future<Output = Result<bool, CommitError>> + Send>> {
    Box::pin(async move {
        let block_index = NodeBlock::index(node_id);
        let node_index = Node::index(node_id);
        let block = state
            .block(repository.clone(), block_index)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;

        let (is_directory, child_initial) = {
            let node = block.node(node_index);
            (node.is_directory(), node.child)
        };

        if is_directory {
            let mut child_id = child_initial;
            let mut to_discard: Vec<NodeID> = Vec::new();
            while child_id.is_valid_node_id() {
                let child_block = state
                    .block(repository.clone(), NodeBlock::index(child_id))
                    .await
                    .forward::<CommitError>("Failed deserializing state block")?;
                let (next_sibling, child_is_dirty) = {
                    let child = child_block.node(Node::index(child_id));
                    (child.sibling, child.is_dirty())
                };

                if child_is_dirty
                    && prune_dirty_recurse(state.clone(), repository.clone(), child_id).await?
                {
                    to_discard.push(child_id);
                }

                child_id = next_sibling;
            }

            for discard_id in to_discard {
                state::node_discard_patch(state.clone(), repository.clone(), discard_id, |_, _| {})
                    .await
                    .forward::<CommitError>("Failed patch-discarding dirty node")?;
            }
        }

        let is_root = node_id == ROOT_NODE;
        let (is_staged, is_dirty_add, is_dirty, is_dir_now, child_now) = {
            let node = block.node(node_index);
            (
                node.is_staged(),
                node.is_dirty_add(),
                node.is_dirty(),
                node.is_directory(),
                node.child,
            )
        };

        if !is_staged && is_dirty_add && !is_root {
            return Ok(true);
        }

        if is_dirty {
            let dirtied = {
                let mut block_writer = block.write();
                let node = block_writer.node(node_index);
                node.clear_dirty_flags();
                block_writer.mark_dirty()
            };
            if dirtied {
                state.block_modified(block.clone(), block_index);
            }
        }

        if is_staged {
            return Ok(false);
        }

        if is_dir_now && !child_now.is_valid_node_id() && !is_root {
            return Ok(true);
        }

        Ok(false)
    })
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
            Ok(connection) => !connection.auth_url.is_empty(),
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
/// `current_revision` becomes the new revision's parent, and the branch tip must
/// still be on it: the check runs before any work so a caller fails fast, and the
/// pointer write compares against the same observed value so a tip that advances
/// during the freeze fails too.
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
    current_revision: Hash,
    branch: BranchId,
) -> Result<Hash, InMemoryCommitFailure> {
    let observed_tip = branch::load_latest(repository.clone(), branch)
        .await
        .unwrap_or_default();
    if !observed_tip.is_zero() && observed_tip != current_revision {
        lore_debug!(
            "Branch {branch} is at {observed_tip}, not at the loaded revision {current_revision}"
        );
        return Err(InMemoryCommitFailure::rejected(BranchAdvanced));
    }

    let has_staged = has_staged_children(repository.clone(), &state_staged)
        .await
        .map_err(InMemoryCommitFailure::rejected)?;
    if !has_staged && metadata_is_empty(&metadata) {
        return Err(InMemoryCommitFailure::rejected(NothingStaged));
    }

    verify_state_for_commit(repository.clone(), state_staged.clone())
        .await
        .forward::<CommitError>("Revision state is not valid to commit")
        .map_err(InMemoryCommitFailure::rejected)?;

    let mut metadata = metadata;
    stamp_commit_metadata(repository.clone(), &mut metadata, branch)
        .await
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

    state_staged.set_parent_self(current_revision);
    state_staged.set_parent_other(Hash::default());

    let stats = CommitStats::new();
    let tracker = immutable::commit_write_tracker(false);
    let work_tracker = tracker.clone();
    let work_repository = repository.clone();
    let work_result: Result<Hash, CommitError> = async move {
        freeze_in_memory_tree(
            work_repository.clone(),
            state_staged.clone(),
            stats.clone(),
            work_tracker.clone(),
        )
        .await?;

        state_staged.set_metadata_hash(
            metadata
                .serialize_with_tracker(work_repository.clone(), Some(work_tracker.clone()))
                .await
                .forward::<CommitError>("Failed to write commit metadata")?,
        );

        weave_history(work_repository.clone(), state_staged.clone()).await?;

        state_staged.reset_merge_conflict_flags();
        state_staged.clear_link_merge_state();

        state_staged
            .serialize(work_repository, token)
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
async fn has_staged_children(
    repository: Arc<RepositoryContext>,
    state: &Arc<State>,
) -> Result<bool, CommitError> {
    let mut children = StateNodeChildrenIterator::new(state.clone(), repository, ROOT_NODE)
        .await
        .forward::<CommitError>("Failed deserializing state node block")?;
    while let Some((_child_node_id, child_node)) = children
        .next()
        .await
        .forward::<CommitError>("Failed deserializing state node block")?
    {
        if child_node.is_staged() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Freeze the staged tree into the shape a revision records: dirty tracking
/// pruned, staged deletions discarded, staged files and links cleared of their
/// change flags, every affected directory rehashed, and the node delta written.
async fn freeze_in_memory_tree(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    stats: Arc<CommitStats>,
    tracker: Arc<lore_storage::write_tracker::WriteTracker>,
) -> Result<(), CommitError> {
    prune_dirty_for_commit(state.clone(), repository.clone()).await?;

    let delta = Arc::new(parking_lot::RwLock::new(BytesMut::new()));
    let discard = Arc::new(parking_lot::RwLock::new(vec![]));
    let subnodes_to_discard = Arc::new(parking_lot::RwLock::new(vec![]));

    event::LoreEvent::RevisionCommitBegin(LoreRevisionCommitBeginEventData::default()).send();

    freeze_directory(
        repository.clone(),
        state.clone(),
        ROOT_NODE,
        delta.clone(),
        discard.clone(),
        subnodes_to_discard.clone(),
        stats.clone(),
    )
    .await?;

    stats.discovery.complete.store(true, Ordering::Relaxed);
    event::LoreEvent::RevisionCommitProgress(LoreRevisionCommitProgressEventData {
        count: LoreRevisionCommitCountData::new(stats.clone()),
    })
    .send();

    commit_discard_subnodes(
        repository.clone(),
        state.clone(),
        subnodes_to_discard,
        stats.clone(),
    )
    .await?;

    commit_discard(
        repository.clone(),
        state.clone(),
        delta.clone(),
        discard,
        stats.clone(),
    )
    .await?;

    event::LoreEvent::RevisionCommitEnd(LoreRevisionCommitEndEventData {
        count: LoreRevisionCommitCountData::new(stats),
    })
    .send();

    rehash_directory(repository.clone(), state.clone(), ROOT_NODE).await?;

    state
        .update_tree_root_hash(repository.clone())
        .await
        .forward::<CommitError>("Failed to update tree root hash")?;

    generate_delta_block(repository, state, delta, tracker).await
}

/// Walk one directory's staged children, recursing into staged subdirectories.
///
/// Sequential on purpose: the walk only flips flags and appends to one shared
/// delta buffer, so fanning out would contend on that buffer for no gain — the
/// parallelism that matters is in [`rehash_directory`], which hashes.
///
/// A staged file or link already carries the address the caller supplied, so
/// there is nothing to fragment — only the change flags to clear.
async fn freeze_directory(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    stats: Arc<CommitStats>,
) -> Result<(), CommitError> {
    let parent = state
        .node(repository.clone(), node_id)
        .await
        .forward::<CommitError>("Failed deserializing state block")?;
    if parent.is_staged() {
        delta_add(delta.clone(), node_id, parent.flags);
    }

    let mut subdirectories: Vec<NodeID> = Vec::new();
    let mut updated = false;
    let mut children =
        StateNodeChildrenIterator::from_parent(state.clone(), repository.clone(), node_id, &parent)
            .await
            .forward::<CommitError>("Failed deserializing state block")?;
    while let Some((child_node_id, child_node)) = children
        .next()
        .await
        .forward::<CommitError>("Failed deserializing state block")?
    {
        if !child_node.is_staged() {
            continue;
        }
        updated = true;

        if child_node.is_staged_delete() {
            if child_node.is_directory() {
                collect_discard_subnodes(
                    repository.clone(),
                    state.clone(),
                    delta.clone(),
                    child_node_id,
                    subnodes_to_discard.clone(),
                    stats.clone(),
                )
                .await?;
            }
            discard.write().push(child_node_id);
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
            subdirectories.push(child_node_id);
            continue;
        }

        delta_add(delta.clone(), child_node_id, child_node.flags);
        freeze_node(repository.clone(), state.clone(), child_node_id).await?;
        stats.complete.file_count.fetch_add(1, Ordering::Relaxed);
        stats.complete.file_total.fetch_add(1, Ordering::Relaxed);
        stats
            .complete
            .file_modify_count
            .fetch_add(1, Ordering::Relaxed);
        stats.record_file_action(child_node.flags, child_node.size);
        stats.discovery.total_files.fetch_add(1, Ordering::Relaxed);
    }

    for subdirectory in subdirectories {
        freeze_directory_recurse(
            repository.clone(),
            state.clone(),
            subdirectory,
            delta.clone(),
            discard.clone(),
            subnodes_to_discard.clone(),
            stats.clone(),
        )
        .await?;
    }

    if updated {
        stats
            .complete
            .directory_count
            .fetch_add(1, Ordering::Relaxed);
        stats
            .complete
            .directory_total
            .fetch_add(1, Ordering::Relaxed);
    }

    Ok(())
}

fn freeze_directory_recurse(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
    delta: Arc<parking_lot::RwLock<BytesMut>>,
    discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    subnodes_to_discard: Arc<parking_lot::RwLock<Vec<NodeID>>>,
    stats: Arc<CommitStats>,
) -> Pin<Box<dyn Future<Output = Result<(), CommitError>> + Send>> {
    Box::pin(freeze_directory(
        repository,
        state,
        node_id,
        delta,
        discard,
        subnodes_to_discard,
        stats,
    ))
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
    let dirtied = {
        let mut writer = block.write();
        writer.node(Node::index(node_id)).clear_all_change_flags();
        writer.mark_dirty()
    };
    if dirtied {
        state.block_modified(block, block_index);
        state.mark_dirty();
    }
    Ok(())
}

#[cfg(test)]
mod statistics_level_tests {
    use lore_base::runtime::LORE_CONTEXT;

    use super::*;
    use crate::interface::ExecutionContext;
    use crate::interface::LoreGlobalArgs;
    use crate::relay::EventDispatcher;

    /// Run `body` under an execution context asking for `stats`.
    async fn at_statistics_level<T>(stats: u32, body: impl Future<Output = T>) -> T {
        let globals = LoreGlobalArgs {
            stats,
            ..Default::default()
        };
        let execution = Arc::new(ExecutionContext::new_client(
            globals,
            EventDispatcher::no_dispatch(),
        ));
        LORE_CONTEXT.scope(execution, body).await
    }

    /// Statistics level zero reports nothing, so it keeps nothing beyond what a
    /// progress event reads: the content read off disk, which it reports as the
    /// bytes transferred.
    #[tokio::test]
    async fn a_count_is_kept_only_where_something_reports_it() {
        for (level, kept) in [(0, 0), (1, 1)] {
            let files = at_statistics_level(level, async {
                let stats = CommitStats::new();
                stats.file_read(4096);
                stats.record_file_action(0, 4096);
                stats.file_stats()
            })
            .await;

            assert_eq!(files.files, kept, "level {level}");
            assert_eq!(files.files_read, kept, "level {level}");
            assert_eq!(files.file_bytes, kept * 4096, "level {level}");
            assert_eq!(files.bytes_transferred, 4096, "level {level}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::NotALayer;

    #[test]
    fn commit_options_new_has_empty_layer_defaults() {
        let opts = CommitOptions::new("msg".into());
        assert!(opts.layer_messages.is_empty());
        assert!(opts.layer.is_none());
        assert!(opts.link_messages.is_empty());
        assert!(opts.link.is_none());
    }

    #[test]
    fn commit_error_carries_not_a_layer() {
        let err: CommitError = NotALayer {
            path: "external/lib".into(),
        }
        .into();
        assert!(matches!(err, CommitError::NotALayer { .. }));
        assert!(err.to_string().contains("external/lib"));
    }
}
