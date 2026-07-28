// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::RevisionDiffRequest;
use lore_proto::lore::thin_client::v1::RevisionDiffResponse;
use lore_proto::lore::thin_client::v1::revision_diff_response::Payload;
use lore_revision::branch;
use lore_revision::branch::BranchError;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::diff::diff_revision_paths;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::DiffItem;
use lore_revision::state::State;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::REVISION;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::warn;

use super::helpers::diff_conflict_from_pair;
use super::helpers::identifier_for_signature;
use super::helpers::node_change_to_diff_change;
use super::helpers::resolve_to_identifier;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

type RevisionDiffStream =
    Pin<Box<dyn Stream<Item = Result<RevisionDiffResponse, Status>> + Send + 'static>>;

/// Default maximum number of source-side change items the thin-client
/// `RevisionDiff` handler accepts for a 3-way diff. Diffs whose source
/// side exceeds this count abort with `Status::resource_exhausted`
/// before target's walk runs; callers needing unbounded diffs use the
/// SDK (`lore-capi` or `lore` CLI). Operators can override per
/// deployment via `feature.revision_diff_source_cap` in the server
/// config; this constant is the fallback when no override is set.
///
/// Default ≈ 100k items × ~232 bytes/`NodeChange` + heap paths ≈ ~50 MB
/// worst-case for the source `Vec`. See
/// `docs/specs/streaming-three-way-revision-diff.md` Open Question #3
/// for calibration discussion.
pub const DEFAULT_REVISION_DIFF_SOURCE_CAP: usize = 100_000;

/// Resolved tunables for the v1 thin-client `RevisionDiff` handler.
/// Built once at server start from `FeatureSettings` and threaded
/// through `LoreThinClientV1Service` into the per-request handler.
#[derive(Clone, Copy, Debug)]
pub struct RevisionDiffConfig {
    /// Source-side change-count cap. The handler passes this to
    /// `branch::diff3_with_source_cap` so the producer aborts with
    /// `BranchError::Oversized` before target's walk runs.
    pub source_cap: usize,
    /// Permit count for the parallel history-walk semaphore inside
    /// `revision::diff3_with_source_cap`. `None` falls back to
    /// `lore_revision::revision::DEFAULT_HISTORY_WALK_CONCURRENCY`.
    pub history_walk_concurrency: Option<usize>,
}

impl Default for RevisionDiffConfig {
    fn default() -> Self {
        Self {
            source_cap: DEFAULT_REVISION_DIFF_SOURCE_CAP,
            history_walk_concurrency: None,
        }
    }
}

/// `lore.thin_client.v1.ThinClientService.RevisionDiff` handler.
///
/// Server-streams a `RevisionDiffHeader` first (echoing both resolved
/// revisions and, for 3-way diffs, the resolved common-ancestor base),
/// then `DiffChange` items, then — in 3-way mode only — `DiffConflict`
/// items.
///
/// Mode selection is server-side from revision metadata:
/// * **2-way** when both revisions live on the same branch, or when
///   one is the branch point of the other's branch.
/// * **3-way** otherwise. The common ancestor is found via the
///   branches' stacks (then `find_branch_point` as a fallback). When
///   no common ancestor exists, the call fails with
///   `FAILED_PRECONDITION`.
///
/// Identical revisions short-circuit to an OK header-only stream with
/// `_base` unset.
#[tracing::instrument(name = "RevisionDiff::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    config: RevisionDiffConfig,
) -> Result<Response<RevisionDiffStream>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let Some(query_from) = req.query_from else {
        return Err(Status::invalid_argument(
            "RevisionDiffRequest.query_from must be set",
        ));
    };
    let Some(query_to) = req.query_to else {
        return Err(Status::invalid_argument(
            "RevisionDiffRequest.query_to must be set",
        ));
    };
    let autoresolve = req.autoresolve;

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // Resolve both sides up-front so unary errors surface before
            // the stream opens.
            let (from_sig, from_id) = resolve_to_identifier(&repository, query_from.into()).await?;
            let (to_sig, to_id) = resolve_to_identifier(&repository, query_to.into()).await?;

            let (tx, rx) = mpsc::channel(256);

            lore_spawn!(
                async move {
                    stream_diff(
                        repository,
                        from_sig,
                        from_id,
                        to_sig,
                        to_id,
                        autoresolve,
                        config,
                        tx,
                    )
                    .await;
                }
                .in_current_span()
            );

            let stream: RevisionDiffStream = Box::pin(ReceiverStream::from(rx));
            Ok(Response::new(stream))
        })
        .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_diff(
    repository: Arc<RepositoryContext>,
    from_sig: Hash,
    from_id: model_v1::RevisionIdentifier,
    to_sig: Hash,
    to_id: model_v1::RevisionIdentifier,
    autoresolve: bool,
    config: RevisionDiffConfig,
    tx: mpsc::Sender<Result<RevisionDiffResponse, Status>>,
) {
    // Identical short-circuit: emit a header-only OK stream.
    if from_sig == to_sig {
        let header = thin_client_v1::RevisionDiffHeader {
            identifier_from: Some(from_id),
            signature_from: from_sig.into(),
            identifier_to: Some(to_id),
            signature_to: to_sig.into(),
            identifier_base: None,
            signature_base: None,
        };
        let _ = send_header(&tx, header).await;
        return;
    }

    let from_branch = BranchId::from(&from_id.branch_id);
    let to_branch = BranchId::from(&to_id.branch_id);

    // 2-way mode kicks in when the two revisions share a branch OR when
    // one is the branch point of the other's branch — in both cases
    // there is no divergence to merge.
    let two_way = if from_branch == to_branch {
        debug!(
            {REPOSITORY_ID} = %repository.id,
            {BRANCH_ID} = %from_branch,
            "RevisionDiff: same branch → 2-way",
        );
        true
    } else if from_branch.is_zero() || to_branch.is_zero() {
        debug!(
            {REPOSITORY_ID} = %repository.id,
            from_branch = %from_branch,
            to_branch = %to_branch,
            "RevisionDiff: a branch is zeroed → 2-way",
        );
        true
    } else {
        match is_branch_point_of_other(&repository, from_sig, to_branch, to_sig, from_branch).await
        {
            Ok(true) => {
                debug!(
                    {REPOSITORY_ID} = %repository.id,
                    "RevisionDiff: branch-point-of-other → 2-way",
                );
                true
            }
            Ok(false) => false,
            Err(status) => {
                let _ = tx.send(Err(status)).await;
                return;
            }
        }
    };

    if two_way {
        if let Err(status) = run_two_way(&repository, from_sig, from_id, to_sig, to_id, &tx).await {
            let _ = tx.send(Err(status)).await;
        }
    } else if let Err(status) = run_three_way(
        &repository,
        from_sig,
        from_id,
        from_branch,
        to_sig,
        to_id,
        to_branch,
        autoresolve,
        config,
        &tx,
    )
    .await
    {
        let _ = tx.send(Err(status)).await;
    }
}

/// Returns `Ok(true)` when either `from_sig` appears as a branch point
/// in `to_branch`'s stack, or `to_sig` appears in `from_branch`'s
/// stack. Used to fold the "branch point of other's branch" case into
/// 2-way mode.
async fn is_branch_point_of_other(
    repository: &Arc<RepositoryContext>,
    from_sig: Hash,
    to_branch: BranchId,
    to_sig: Hash,
    from_branch: BranchId,
) -> Result<bool, Status> {
    if branch_stack_contains(repository, to_branch, from_sig).await? {
        return Ok(true);
    }
    if branch_stack_contains(repository, from_branch, to_sig).await? {
        return Ok(true);
    }
    Ok(false)
}

async fn branch_stack_contains(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    revision: Hash,
) -> Result<bool, Status> {
    let metadata = match branch::metadata(repository.clone(), branch_id).await {
        Ok(metadata) => metadata,
        Err(err) if err.is_branch_not_found() => return Ok(false),
        Err(err) => {
            warn!(
                {REPOSITORY_ID} = %repository.id, {BRANCH_ID} = %branch_id, ?err,
                "Failed to load branch metadata for branch-point check",
            );
            return Err(warn_error_to_status(&err, |e| {
                Status::internal(e.to_string())
            }));
        }
    };
    Ok(branch::stack(&metadata)
        .iter()
        .any(|point| point.revision == revision))
}

async fn run_two_way(
    repository: &Arc<RepositoryContext>,
    from_sig: Hash,
    from_id: model_v1::RevisionIdentifier,
    to_sig: Hash,
    to_id: model_v1::RevisionIdentifier,
    tx: &mpsc::Sender<Result<RevisionDiffResponse, Status>>,
) -> Result<(), Status> {
    let (from_state, to_state) = load_state_pair(repository, from_sig, to_sig).await?;

    // Header first, before opening the producer's sender so a failure in
    // the producer setup surfaces before any header is emitted.
    let header = thin_client_v1::RevisionDiffHeader {
        identifier_from: Some(from_id),
        signature_from: from_sig.into(),
        identifier_to: Some(to_id),
        signature_to: to_sig.into(),
        identifier_base: None,
        signature_base: None,
    };
    send_header(tx, header).await?;

    // End-to-end streaming: bounded channel between the diff producer and
    // an adaptor loop that forwards each NodeChange onto the gRPC wire
    // sender.
    let (producer_tx, mut producer_rx) = mpsc::channel::<
        Result<lore_revision::change::NodeChange, lore_revision::diff::DiffError>,
    >(256);
    let repo_clone = repository.clone();
    let from_sig_clone = from_sig;
    let to_sig_clone = to_sig;
    let producer = lore_spawn!(async move {
        diff_revision_paths(repo_clone, from_state, to_state, None, producer_tx).await
    });

    let mut partitions = PartitionTable::new(repository.id);
    while let Some(item) = producer_rx.recv().await {
        let change = item.map_err(|err| {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                from = %from_sig_clone,
                to = %to_sig_clone,
                ?err,
                "Failed to calculate 2-way revision diff",
            );
            warn_error_to_status(&err, |e| Status::internal(e.to_string()))
        })?;
        let index = match partitions
            .resolve_or_announce(surviving_repository_id(&change), tx)
            .await
        {
            Ok(index) => index,
            Err(SendOutcome::ReceiverDropped) => return Ok(()),
            Err(SendOutcome::Sent) => unreachable!("resolve_or_announce returns Sent only via Ok"),
        };
        let payload = Payload::Change(node_change_to_diff_change(&change, index));
        match send_payload(tx, payload).await {
            SendOutcome::Sent => {}
            SendOutcome::ReceiverDropped => return Ok(()),
        }
    }

    // Surface any error from the producer task itself.
    match producer.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                from = %from_sig,
                to = %to_sig,
                ?err,
                "2-way revision diff producer returned error",
            );
            Err(warn_error_to_status(&err, |e| {
                Status::internal(e.to_string())
            }))
        }
        Err(join_err) => {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                ?join_err,
                "2-way revision diff producer task panicked",
            );
            Err(Status::internal("revision diff producer task failed"))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_three_way(
    repository: &Arc<RepositoryContext>,
    from_sig: Hash,
    from_id: model_v1::RevisionIdentifier,
    from_branch: BranchId,
    to_sig: Hash,
    to_id: model_v1::RevisionIdentifier,
    to_branch: BranchId,
    autoresolve: bool,
    config: RevisionDiffConfig,
    tx: &mpsc::Sender<Result<RevisionDiffResponse, Status>>,
) -> Result<(), Status> {
    // Resolve the common-ancestor base up front so we can emit the header
    // (which carries `_base`) before any DiffItem is produced. This lets
    // the handler stream items directly from the producer onto the wire —
    // no handler-side buffering, no second copy of the diff.
    //
    // The producer itself (branch::diff3 → revision::diff3) still buffers
    // internally because the 3-way merge step needs both intermediate
    // sets present (see `docs/specs/streaming-revision-diff.md`
    // "Limitations"). We can't change that here, but we can keep the
    // *handler* a pure passthrough.
    let base =
        branch::resolve_diff3_base(repository.clone(), from_branch, from_sig, to_branch, to_sig)
            .await
            .map_err(|err| {
                warn!(
                    {REPOSITORY_ID} = %repository.id,
                    from_branch = %from_branch,
                    to_branch = %to_branch,
                    ?err,
                    "Failed to resolve 3-way base revision",
                );
                if err.is_divergent() {
                    Status::failed_precondition(err.to_string())
                } else if err.is_max_history_search_depth() {
                    Status::resource_exhausted(err.to_string())
                } else {
                    warn_error_to_status(&err, |e| Status::internal(e.to_string()))
                }
            })?;

    if base.is_zero() {
        // No common ancestor — disjoint histories.
        return Err(Status::failed_precondition(
            "RevisionDiff: no common ancestor between revisions",
        ));
    }

    let base_id = identifier_for_signature(repository, base).await?;
    let header = thin_client_v1::RevisionDiffHeader {
        identifier_from: Some(from_id),
        signature_from: from_sig.into(),
        identifier_to: Some(to_id),
        signature_to: to_sig.into(),
        identifier_base: Some(base_id),
        signature_base: Some(base.into()),
    };
    send_header(tx, header).await?;

    // Spawn the producer and stream items straight to the wire. The
    // producer re-resolves the base internally (cheap — metadata
    // lookups), runs the 3-way merge, and emits each finalized DiffItem
    // on its channel.
    let (producer_tx, mut producer_rx) = mpsc::channel::<Result<DiffItem, BranchError>>(256);
    let repo_clone = repository.clone();
    let producer = lore_spawn!(async move {
        Box::pin(branch::diff3_with_source_cap(
            repo_clone,
            from_branch,
            from_sig,
            to_branch,
            to_sig,
            None,
            false,
            autoresolve,
            Some(config.source_cap),
            config.history_walk_concurrency,
            producer_tx,
        ))
        .await
    });

    let mut partitions = PartitionTable::new(repository.id);
    while let Some(item) = producer_rx.recv().await {
        let item = item.map_err(|err| {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                from_branch = %from_branch,
                to_branch = %to_branch,
                ?err,
                "Failed to calculate 3-way revision diff",
            );
            map_branch_error_to_status(err)
        })?;
        let payload = match item {
            DiffItem::Change(change) => {
                let index = match partitions
                    .resolve_or_announce(surviving_repository_id(&change), tx)
                    .await
                {
                    Ok(index) => index,
                    Err(SendOutcome::ReceiverDropped) => return Ok(()),
                    Err(SendOutcome::Sent) => {
                        unreachable!("resolve_or_announce returns Sent only via Ok")
                    }
                };
                Payload::Change(node_change_to_diff_change(&change, index))
            }
            DiffItem::Conflict(pair) => {
                let index_from = match partitions
                    .resolve_or_announce(surviving_repository_id(&pair.0), tx)
                    .await
                {
                    Ok(index) => index,
                    Err(SendOutcome::ReceiverDropped) => return Ok(()),
                    Err(SendOutcome::Sent) => {
                        unreachable!("resolve_or_announce returns Sent only via Ok")
                    }
                };
                let index_to = match partitions
                    .resolve_or_announce(surviving_repository_id(&pair.1), tx)
                    .await
                {
                    Ok(index) => index,
                    Err(SendOutcome::ReceiverDropped) => return Ok(()),
                    Err(SendOutcome::Sent) => {
                        unreachable!("resolve_or_announce returns Sent only via Ok")
                    }
                };
                Payload::Conflict(diff_conflict_from_pair(&pair, index_from, index_to))
            }
        };
        match send_payload(tx, payload).await {
            SendOutcome::Sent => {}
            SendOutcome::ReceiverDropped => return Ok(()),
        }
    }

    // Surface any error from the producer task itself. The summary is
    // unused on the wire — the header already carries `base`, `source`,
    // `target`.
    match producer.await {
        Ok(Ok(_summary)) => Ok(()),
        Ok(Err(err)) => {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                from_branch = %from_branch,
                to_branch = %to_branch,
                ?err,
                "3-way revision diff producer returned error",
            );
            Err(map_branch_error_to_status(err))
        }
        Err(join_err) => {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                ?join_err,
                "3-way revision diff producer task panicked",
            );
            Err(Status::internal("revision diff producer task failed"))
        }
    }
}

/// Map a `BranchError` from the 3-way diff producer to a gRPC `Status`.
/// `Oversized` surfaces as `resource_exhausted` so clients can
/// distinguish "diff too large" from generic internal failures — the
/// typed variant lets us avoid string-matching the inner `StateError`
/// across crate boundaries.
fn map_branch_error_to_status(err: BranchError) -> Status {
    if err.is_oversized() {
        Status::resource_exhausted(err.to_string())
    } else if err.is_divergent() {
        Status::failed_precondition(err.to_string())
    } else if err.is_max_history_search_depth() {
        Status::resource_exhausted(err.to_string())
    } else {
        warn_error_to_status(&err, |e| Status::internal(e.to_string()))
    }
}

async fn load_state_pair(
    repository: &Arc<RepositoryContext>,
    from_sig: Hash,
    to_sig: Hash,
) -> Result<(Arc<State>, Arc<State>), Status> {
    let from_fut = State::deserialize(repository.clone(), from_sig);
    let to_fut = State::deserialize(repository.clone(), to_sig);
    let (from_res, to_res) = tokio::join!(from_fut, to_fut);
    let from_state = from_res.map_err(|err| state_status(repository, from_sig, err))?;
    let to_state = to_res.map_err(|err| state_status(repository, to_sig, err))?;
    Ok((from_state, to_state))
}

fn state_status(
    repository: &Arc<RepositoryContext>,
    signature: Hash,
    err: lore_revision::state::StateError,
) -> Status {
    if err.is_slow_down() {
        return Status::resource_exhausted(err.to_string());
    }
    if err.is_not_found() {
        Status::not_found(format!("Revision {signature} not found"))
    } else {
        warn!(
            {REPOSITORY_ID} = %repository.id, {REVISION} = %signature, ?err,
            "Failed to deserialize revision state",
        );
        warn_error_to_status(&err, |e| Status::internal(e.to_string()))
    }
}

async fn send_header(
    tx: &mpsc::Sender<Result<RevisionDiffResponse, Status>>,
    header: thin_client_v1::RevisionDiffHeader,
) -> Result<(), Status> {
    if tx
        .send(Ok(RevisionDiffResponse {
            payload: Some(Payload::Header(header)),
        }))
        .await
        .is_err()
    {
        warn!("RevisionDiff receiver dropped before header — client cancelled or disconnected");
    }
    Ok(())
}

/// Outcome of attempting to forward one payload to the gRPC wire sender.
///
/// `ReceiverDropped` is **not** a server-side failure: it means the gRPC
/// client cancelled the stream or disconnected, the wire-side receiver is
/// gone, and `tonic` cannot deliver any further messages (including a
/// `Status::cancelled`) to that peer. The handler unwinds cleanly and the
/// spawned producer task observes the same drop on its own channel send.
///
/// The enum exists so the caller's bail path reads as deliberate
/// cancellation (`SendOutcome::ReceiverDropped => return Ok(())`) rather
/// than a generic ignored error.
#[derive(Debug)]
enum SendOutcome {
    Sent,
    ReceiverDropped,
}

/// Send a single non-header payload to the wire sender. On
/// receiver-drop, logs a `warn!` line (cancellation is rare and useful
/// to surface in operator logs) and returns `ReceiverDropped`.
async fn send_payload(
    tx: &mpsc::Sender<Result<RevisionDiffResponse, Status>>,
    payload: Payload,
) -> SendOutcome {
    if tx
        .send(Ok(RevisionDiffResponse {
            payload: Some(payload),
        }))
        .await
        .is_err()
    {
        warn!("RevisionDiff receiver dropped mid-stream — client cancelled or disconnected");
        return SendOutcome::ReceiverDropped;
    }
    SendOutcome::Sent
}

/// Per-stream map of linked-repository `RepositoryId` to its assigned
/// index. The parent repository is index 0 and never stored here.
struct PartitionTable {
    parent_repository_id: RepositoryId,
    entries: HashMap<RepositoryId, u32>,
}

impl PartitionTable {
    fn new(parent_repository_id: RepositoryId) -> Self {
        Self {
            parent_repository_id,
            entries: HashMap::new(),
        }
    }

    /// Resolve `partition` to its index, sending a `DiffPartition` on
    /// first sighting (before the index is returned, so the announcement
    /// always precedes the change that references it). `Err` only when
    /// the receiver is gone; the table is left unchanged in that case.
    async fn resolve_or_announce(
        &mut self,
        partition: RepositoryId,
        tx: &mpsc::Sender<Result<RevisionDiffResponse, Status>>,
    ) -> Result<u32, SendOutcome> {
        if partition == self.parent_repository_id {
            return Ok(0);
        }
        if let Some(&index) = self.entries.get(&partition) {
            return Ok(index);
        }
        let index = (self.entries.len() as u32) + 1;
        let announcement = Payload::Partition(thin_client_v1::DiffPartition {
            index,
            link_partition: Bytes::from(partition),
        });
        match send_payload(tx, announcement).await {
            SendOutcome::Sent => {
                self.entries.insert(partition, index);
                Ok(index)
            }
            SendOutcome::ReceiverDropped => Err(SendOutcome::ReceiverDropped),
        }
    }
}

/// Repository of the side that survives the change: `from` for a
/// delete, `to` otherwise. This is the partition its content lives in.
fn surviving_repository_id(change: &NodeChange) -> RepositoryId {
    match change.action {
        FileAction::Delete => change.from.repository.id,
        _ => change.to.repository.id,
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchPoint;
    use lore_proto::lore::thin_client::v1::revision_diff_request::QueryFrom;
    use lore_proto::lore::thin_client::v1::revision_diff_request::QueryTo;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::state;
    use lore_storage::hash::hash_string;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tokio_stream::StreamExt;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::store::test_store_create;

    fn make_request(
        repository: RepositoryId,
        from: QueryFrom,
        to: QueryTo,
        autoresolve: bool,
    ) -> Request<RevisionDiffRequest> {
        let mut request = Request::new(RevisionDiffRequest {
            query_from: Some(from),
            query_to: Some(to),
            autoresolve,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    /// Push a revision on `branch_id` with `files` as direct File nodes
    /// under root. Each file's `bytes` is written into the immutable
    /// store and its CAS address attached to the node; revisions sharing
    /// the same bytes share the same address (so the diff layer sees an
    /// unchanged file as Keep, not Modify).
    async fn push_revision(
        repository: &Arc<RepositoryContext>,
        branch_id: BranchId,
        parent: Hash,
        revision_number: u64,
        files: &[(&str, &[u8])],
    ) -> Hash {
        let write_token = get_write_token();
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");
        let state = state::State::new();
        state.set_parent_self(parent);
        state.set_revision_number(revision_number);
        state.set_metadata_hash(metadata_hash);
        for (name, bytes) in files {
            let buffer = bytes::Bytes::copy_from_slice(bytes);
            let (address, _) = lore_revision::immutable::write(
                repository.clone(),
                lore_storage::Context::default(),
                buffer,
                lore_storage::WriteOptions::default(),
            )
            .await
            .expect("immutable::write");
            let node = Node {
                flags: NodeFlags::File.bits(),
                name_hash: hash_string(name),
                address,
                ..Default::default()
            };
            state
                .node_add(repository.clone(), ROOT_NODE, node, name)
                .await
                .expect("node_add");
        }
        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        branch_push::push(
            repository.clone(),
            branch_id,
            serialized,
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("push")
        .revision
    }

    async fn create_branch(
        repository: &Arc<RepositoryContext>,
        name: &str,
        stack: Vec<BranchPoint>,
    ) -> BranchId {
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch_id,
            name,
            if stack.is_empty() {
                branch::default_category()
            } else {
                branch::personal_category()
            },
            "creator",
            1,
            stack,
            false,
            false,
        )
        .await
        .expect("create branch");
        branch_id
    }

    async fn collect(
        response: Response<RevisionDiffStream>,
    ) -> Vec<Result<RevisionDiffResponse, Status>> {
        response.into_inner().collect().await
    }

    #[tokio::test]
    async fn unset_query_from_returns_invalid_argument() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let mut request = Request::new(RevisionDiffRequest {
                query_from: None,
                query_to: Some(QueryTo::SignatureTo(Hash::default().into())),
                autoresolve: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            let err = match handler(
                request,
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            {
                Ok(_) => panic!("missing query_from must fail"),
                Err(err) => err,
            };
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn identical_revisions_return_header_only() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_branch(&repository_context, "main", vec![]).await;
            let rev = push_revision(
                &repository_context,
                main,
                Hash::default(),
                1,
                &[("a.txt", b"hello".as_slice())],
            )
            .await;

            let response = handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(rev.into()),
                    QueryTo::SignatureTo(rev.into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();
            assert_eq!(items.len(), 1);
            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header, got {other:?}"),
            };
            assert_eq!(Hash::from(header.signature_from.as_ref()), rev);
            assert_eq!(Hash::from(header.signature_to.as_ref()), rev);
            assert!(header.identifier_base.is_none());
            assert!(header.signature_base.is_none());
        }))
        .await;
    }

    #[tokio::test]
    async fn same_branch_two_way_diff_streams_changes_no_base() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_branch(&repository_context, "main", vec![]).await;
            let rev1 = push_revision(
                &repository_context,
                main,
                Hash::default(),
                1,
                &[("a.txt", b"v1".as_slice())],
            )
            .await;
            let rev2 = push_revision(
                &repository_context,
                main,
                rev1,
                2,
                &[("a.txt", b"v1".as_slice()), ("b.txt", b"new".as_slice())],
            )
            .await;

            let response = handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(rev1.into()),
                    QueryTo::SignatureTo(rev2.into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();
            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header first, got {other:?}"),
            };
            // 2-way: no base.
            assert!(header.identifier_base.is_none());
            assert!(header.signature_base.is_none());

            let changes: Vec<&thin_client_v1::DiffChange> = items[1..]
                .iter()
                .filter_map(|item| match &item.payload {
                    Some(Payload::Change(c)) => Some(c),
                    Some(Payload::Conflict(_)) => panic!("no conflicts expected in 2-way"),
                    _ => None,
                })
                .collect();
            // b.txt was added between rev1 and rev2.
            assert!(
                changes
                    .iter()
                    .any(|c| c.path == "b.txt" && c.action == thin_client_v1::Action::Add as i32),
                "expected b.txt ADD, got {:?}",
                changes.iter().map(|c| &c.path).collect::<Vec<_>>(),
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn branch_point_of_other_is_two_way() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_branch(&repository_context, "main", vec![]).await;
            let main_rev = push_revision(
                &repository_context,
                main,
                Hash::default(),
                1,
                &[("a.txt", b"base".as_slice())],
            )
            .await;
            let feature = create_branch(
                &repository_context,
                "feature",
                vec![BranchPoint {
                    branch: main,
                    revision: main_rev,
                }],
            )
            .await;
            let feature_rev = push_revision(
                &repository_context,
                feature,
                main_rev,
                1,
                &[
                    ("a.txt", b"base".as_slice()),
                    ("feature.txt", b"hi".as_slice()),
                ],
            )
            .await;

            // Diff main_rev (which is feature's branch point) against
            // feature_rev. Same-branch logic doesn't apply (different
            // branches), but main_rev appears in feature.stack so this
            // is the "branch-point-of-other" 2-way path.
            let response = handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(main_rev.into()),
                    QueryTo::SignatureTo(feature_rev.into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();
            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header, got {other:?}"),
            };
            // No base because this collapses to 2-way mode.
            assert!(header.identifier_base.is_none());
            assert!(header.signature_base.is_none());
            // No conflicts in 2-way mode.
            assert!(
                items[1..]
                    .iter()
                    .all(|item| !matches!(item.payload, Some(Payload::Conflict(_))))
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn three_way_diff_populates_base_and_emits_conflict() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_branch(&repository_context, "main", vec![]).await;
            let main_rev = push_revision(
                &repository_context,
                main,
                Hash::default(),
                1,
                &[("conflict.txt", b"base".as_slice())],
            )
            .await;
            // Two sibling branches that both modify conflict.txt
            // differently — the 3-way merge must report a conflict.
            let branch_a = create_branch(
                &repository_context,
                "branch_a",
                vec![BranchPoint {
                    branch: main,
                    revision: main_rev,
                }],
            )
            .await;
            let a_rev = push_revision(
                &repository_context,
                branch_a,
                main_rev,
                1,
                &[("conflict.txt", b"changed-by-a".as_slice())],
            )
            .await;
            let branch_b = create_branch(
                &repository_context,
                "branch_b",
                vec![BranchPoint {
                    branch: main,
                    revision: main_rev,
                }],
            )
            .await;
            let b_rev = push_revision(
                &repository_context,
                branch_b,
                main_rev,
                1,
                &[("conflict.txt", b"changed-by-b".as_slice())],
            )
            .await;

            let response = handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(a_rev.into()),
                    QueryTo::SignatureTo(b_rev.into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();
            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header, got {other:?}"),
            };
            // 3-way: base populated.
            let base_id = header
                .identifier_base
                .as_ref()
                .expect("identifier_base set");
            assert_eq!(BranchId::from(&base_id.branch_id), main);
            let base_sig = header.signature_base.as_ref().expect("signature_base set");
            assert_eq!(Hash::from(base_sig.as_ref()), main_rev);
            // At least one conflict reported.
            assert!(
                items[1..]
                    .iter()
                    .any(|item| matches!(item.payload, Some(Payload::Conflict(_)))),
                "expected at least one conflict",
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn unknown_signature_returns_not_found() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let bogus = Hash::from(random::<[u8; 32]>());
            let real = {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                let main = create_branch(&repository_context, "main", vec![]).await;
                push_revision(&repository_context, main, Hash::default(), 1, &[]).await
            };
            let err = match handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(bogus.into()),
                    QueryTo::SignatureTo(real.into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            {
                Ok(_) => panic!("unknown signature must fail"),
                Err(err) => err,
            };
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn zero_from_signature_diffs_as_two_way_showing_all_adds() {
        // State::deserialize returns an empty state for the zero hash,
        // so every file in rev1 appears as an ADD in the diff.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let rev1 = {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                let main = create_branch(&repository_context, "main", vec![]).await;
                push_revision(
                    &repository_context,
                    main,
                    Hash::default(),
                    1,
                    &[("a.txt", b"hello".as_slice())],
                )
                .await
            };

            let response = handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(Hash::default().into()),
                    QueryTo::SignatureTo(rev1.into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();

            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header first, got {other:?}"),
            };
            assert_eq!(Hash::from(header.signature_from.as_ref()), Hash::default());
            assert_eq!(Hash::from(header.signature_to.as_ref()), rev1);
            assert!(header.identifier_base.is_none(), "2-way: no base");

            let changes: Vec<&thin_client_v1::DiffChange> = items[1..]
                .iter()
                .filter_map(|item| match &item.payload {
                    Some(Payload::Change(c)) => Some(c),
                    _ => None,
                })
                .collect();
            assert!(
                changes
                    .iter()
                    .any(|c| c.path == "a.txt" && c.action == thin_client_v1::Action::Add as i32),
                "expected a.txt ADD, got {changes:?}",
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn zero_to_signature_diffs_as_two_way_showing_all_adds() {
        // State::deserialize returns an empty state for the zero hash,
        // so every file in rev1 appears as an DELETE in the diff.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let rev1 = {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                let main = create_branch(&repository_context, "main", vec![]).await;
                push_revision(
                    &repository_context,
                    main,
                    Hash::default(),
                    1,
                    &[("a.txt", b"hello".as_slice())],
                )
                .await
            };

            let response = handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(rev1.into()),
                    QueryTo::SignatureTo(Hash::default().into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();

            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header first, got {other:?}"),
            };
            assert_eq!(Hash::from(header.signature_from.as_ref()), rev1);
            assert_eq!(Hash::from(header.signature_to.as_ref()), Hash::default());
            assert!(header.identifier_base.is_none(), "2-way: no base");

            let changes: Vec<&thin_client_v1::DiffChange> = items[1..]
                .iter()
                .filter_map(|item| match &item.payload {
                    Some(Payload::Change(c)) => Some(c),
                    _ => None,
                })
                .collect();
            assert!(
                    changes
                        .iter()
                        .any(|c| c.path == "a.txt"
                            && c.action == thin_client_v1::Action::Delete as i32),
                    "expected a.txt DELETE, got {changes:?}",
                );
        }))
        .await;
    }

    #[tokio::test]
    async fn unset_query_to_returns_invalid_argument() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let mut request = Request::new(RevisionDiffRequest {
                query_from: Some(QueryFrom::SignatureFrom(Hash::default().into())),
                query_to: None,
                autoresolve: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            let err = match handler(
                request,
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            {
                Ok(_) => panic!("missing query_to must fail"),
                Err(err) => err,
            };
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn identifier_query_resolves_through_diff() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_branch(&repository_context, "main", vec![]).await;
            let rev1 = push_revision(
                &repository_context,
                main,
                Hash::default(),
                1,
                &[("a.txt", b"v1".as_slice())],
            )
            .await;
            let rev2 = push_revision(
                &repository_context,
                main,
                rev1,
                2,
                &[("a.txt", b"v1".as_slice()), ("b.txt", b"new".as_slice())],
            )
            .await;

            // Query by identifier on both sides: (main, 1) vs (main, 0) — the
            // latter resolves to latest (rev2). Same-branch 2-way diff.
            let response = handler(
                make_request(
                    repository,
                    QueryFrom::IdentifierFrom(model_v1::RevisionIdentifier {
                        branch_id: main.into(),
                        number: 1,
                    }),
                    QueryTo::IdentifierTo(model_v1::RevisionIdentifier {
                        branch_id: main.into(),
                        number: 0,
                    }),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();
            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header, got {other:?}"),
            };
            assert_eq!(Hash::from(header.signature_from.as_ref()), rev1);
            assert_eq!(Hash::from(header.signature_to.as_ref()), rev2);
            assert_eq!(header.identifier_from.as_ref().unwrap().number, 1);
            assert_eq!(header.identifier_to.as_ref().unwrap().number, 2);
            assert!(header.identifier_base.is_none());
        }))
        .await;
    }

    #[tokio::test]
    async fn three_way_clean_merge_has_base_but_no_conflicts() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // Common ancestor on main; two sibling branches change
            // disjoint files (no overlap → no conflict).
            let main = create_branch(&repository_context, "main", vec![]).await;
            let main_rev = push_revision(
                &repository_context,
                main,
                Hash::default(),
                1,
                &[("shared.txt", b"base".as_slice())],
            )
            .await;
            let branch_a = create_branch(
                &repository_context,
                "branch_a",
                vec![BranchPoint {
                    branch: main,
                    revision: main_rev,
                }],
            )
            .await;
            let a_rev = push_revision(
                &repository_context,
                branch_a,
                main_rev,
                1,
                &[
                    ("shared.txt", b"base".as_slice()),
                    ("only_a.txt", b"a-content".as_slice()),
                ],
            )
            .await;
            let branch_b = create_branch(
                &repository_context,
                "branch_b",
                vec![BranchPoint {
                    branch: main,
                    revision: main_rev,
                }],
            )
            .await;
            let b_rev = push_revision(
                &repository_context,
                branch_b,
                main_rev,
                1,
                &[
                    ("shared.txt", b"base".as_slice()),
                    ("only_b.txt", b"b-content".as_slice()),
                ],
            )
            .await;

            let response = handler(
                make_request(
                    repository,
                    QueryFrom::SignatureFrom(a_rev.into()),
                    QueryTo::SignatureTo(b_rev.into()),
                    false,
                ),
                immutable_store,
                mutable_store,
                RevisionDiffConfig::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();
            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header, got {other:?}"),
            };
            // 3-way: base populated.
            assert!(header.identifier_base.is_some());
            assert!(header.signature_base.is_some());
            // No conflicts: the two branches touched disjoint files.
            assert!(
                items[1..]
                    .iter()
                    .all(|item| !matches!(item.payload, Some(Payload::Conflict(_)))),
                "expected zero conflicts for disjoint changes",
            );
        }))
        .await;
    }

    fn make_partition_id() -> RepositoryId {
        RepositoryId::from(uuid::Uuid::now_v7())
    }

    /// Drain whatever is already queued on the receiver, returning the
    /// payloads in arrival order. Used to inspect what `PartitionTable`
    /// announced on the wire.
    async fn drain_now(
        rx: &mut mpsc::Receiver<Result<RevisionDiffResponse, Status>>,
    ) -> Vec<Payload> {
        let mut out = Vec::new();
        while let Ok(Some(Ok(RevisionDiffResponse { payload: Some(p) }))) =
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await
        {
            out.push(p);
        }
        out
    }

    #[tokio::test]
    async fn partition_table_parent_id_returns_zero_no_announcement() {
        let parent = make_partition_id();
        let (tx, mut rx) = mpsc::channel::<Result<RevisionDiffResponse, Status>>(8);
        let mut table = PartitionTable::new(parent);

        let index = table
            .resolve_or_announce(parent, &tx)
            .await
            .expect("parent partition must resolve");
        assert_eq!(index, 0);

        let drained = drain_now(&mut rx).await;
        assert!(
            drained.is_empty(),
            "parent partition must not be announced, got {drained:?}",
        );
    }

    #[tokio::test]
    async fn partition_table_first_link_emits_partition_then_returns_one() {
        let parent = make_partition_id();
        let linked = make_partition_id();
        let (tx, mut rx) = mpsc::channel::<Result<RevisionDiffResponse, Status>>(8);
        let mut table = PartitionTable::new(parent);

        let index = table
            .resolve_or_announce(linked, &tx)
            .await
            .expect("linked partition resolves");
        assert_eq!(index, 1);

        let drained = drain_now(&mut rx).await;
        assert_eq!(drained.len(), 1, "exactly one partition announcement");
        let Payload::Partition(p) = &drained[0] else {
            panic!("expected Partition payload, got {drained:?}");
        };
        assert_eq!(p.index, 1);
        assert_eq!(RepositoryId::from(p.link_partition.as_ref()), linked);
    }

    #[tokio::test]
    async fn partition_table_repeated_lookup_does_not_reannounce() {
        let parent = make_partition_id();
        let linked = make_partition_id();
        let (tx, mut rx) = mpsc::channel::<Result<RevisionDiffResponse, Status>>(8);
        let mut table = PartitionTable::new(parent);

        let first = table.resolve_or_announce(linked, &tx).await.unwrap();
        let _ = drain_now(&mut rx).await;
        let second = table.resolve_or_announce(linked, &tx).await.unwrap();
        let drained = drain_now(&mut rx).await;
        assert_eq!(first, second);
        assert!(
            drained.is_empty(),
            "repeated lookup must not announce again, got {drained:?}",
        );
    }

    #[tokio::test]
    async fn partition_table_two_distinct_ids_get_one_and_two_in_order() {
        let parent = make_partition_id();
        let a = make_partition_id();
        let b = make_partition_id();
        let (tx, mut rx) = mpsc::channel::<Result<RevisionDiffResponse, Status>>(8);
        let mut table = PartitionTable::new(parent);

        assert_eq!(table.resolve_or_announce(a, &tx).await.unwrap(), 1);
        assert_eq!(table.resolve_or_announce(b, &tx).await.unwrap(), 2);
        assert_eq!(
            table.resolve_or_announce(a, &tx).await.unwrap(),
            1,
            "lookup of A after B reuses index 1",
        );

        let drained = drain_now(&mut rx).await;
        assert_eq!(drained.len(), 2, "only two announcements total");
        let Payload::Partition(p1) = &drained[0] else {
            panic!("first must be Partition, got {drained:?}");
        };
        let Payload::Partition(p2) = &drained[1] else {
            panic!("second must be Partition, got {drained:?}");
        };
        assert_eq!(p1.index, 1);
        assert_eq!(RepositoryId::from(p1.link_partition.as_ref()), a);
        assert_eq!(p2.index, 2);
        assert_eq!(RepositoryId::from(p2.link_partition.as_ref()), b);
    }

    #[tokio::test]
    async fn partition_table_receiver_dropped_propagates() {
        let parent = make_partition_id();
        let linked = make_partition_id();
        let (tx, rx) = mpsc::channel::<Result<RevisionDiffResponse, Status>>(8);
        let mut table = PartitionTable::new(parent);

        drop(rx);
        let outcome = table.resolve_or_announce(linked, &tx).await;
        assert!(
            matches!(outcome, Err(SendOutcome::ReceiverDropped)),
            "got {outcome:?}",
        );
        assert!(
            !table.entries.contains_key(&linked),
            "failed announcement must not poison the table",
        );
    }

    #[tokio::test]
    async fn surviving_repository_id_picks_from_for_delete_to_otherwise() {
        // Construct two contexts with distinct ids; place `from` and `to`
        // in different repositories and verify the helper picks the
        // correct side based on FileAction.
        let parent_id = RepositoryId::from(uuid::Uuid::now_v7());
        let linked_id = RepositoryId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, _) = test_store_create().await.expect("test stores");
        let parent_ctx = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            parent_id,
        ));
        let linked_ctx = Arc::new(RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            linked_id,
        ));
        let state = Arc::new(state::State::new());
        let address = lore_storage::Address::default();

        let make = |action: lore_revision::change::FileAction| NodeChange {
            action,
            path: lore_revision::util::path::RelativePath::from_str("p").unwrap(),
            from_path: None,
            flags: lore_revision::change::Flags::None,
            from: lore_revision::change::NodeChangeState {
                node: 1,
                repository: linked_ctx.clone(),
                state: state.clone(),
                address,
                flags: NodeFlags::File,
            },
            to: lore_revision::change::NodeChangeState {
                node: 2,
                repository: parent_ctx.clone(),
                state: state.clone(),
                address,
                flags: NodeFlags::File,
            },
        };

        assert_eq!(
            surviving_repository_id(&make(lore_revision::change::FileAction::Delete)),
            linked_id,
            "Delete surfaces the from side",
        );
        assert_eq!(
            surviving_repository_id(&make(lore_revision::change::FileAction::Add)),
            parent_id,
            "Add surfaces the to side",
        );
        assert_eq!(
            surviving_repository_id(&make(lore_revision::change::FileAction::Keep)),
            parent_id,
            "Keep surfaces the to side",
        );
    }
}
