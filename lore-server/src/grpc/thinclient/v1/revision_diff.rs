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
use lore_revision::diff::diff_revision_paths;
use lore_revision::link;
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
use super::helpers::link_pin_change_to_diff_change;
use super::helpers::node_change_to_diff_change;
use super::helpers::resolve_to_identifier;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

#[lore_macro::test_pub]
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
///   `FAILED_PRECONDITION`. When the common ancestor is one of the two
///   revisions, the diff falls back to **2-way**.
///
/// Identical revisions short-circuit to an OK header-only stream with
/// `_base` unset.
#[tracing::instrument(name = "RevisionDiff::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    config: RevisionDiffConfig,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Response<RevisionDiffStream>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let can_read = link_read_authorizer(&repository_authorizer, request.extensions());
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
    let repository = Arc::new(
        RepositoryContext::new_server_context(immutable_store, mutable_store, repository_id)
            .with_link_read(can_read),
    );

    LORE_CONTEXT
        .scope(execution, async move {
            // Resolve both sides up-front so unary errors surface before
            // the stream opens.
            let (from_sig, from_id) = resolve_to_identifier(
                &repository,
                query_from.into(),
                history_step_size,
                acceleration,
            )
            .await?;
            let (to_sig, to_id) = resolve_to_identifier(
                &repository,
                query_to.into(),
                history_step_size,
                acceleration,
            )
            .await?;

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
    let metadata = match branch::metadata(repository.clone(), branch_id)
        .await
        .filter_slow_down()?
    {
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

    // Compared before the header goes out so a failure aborts the call rather
    // than truncating a stream that has already started. Streaming the content
    // changes alone would claim no pin moved, which the consumer cannot
    // distinguish from a pin that genuinely did not move.
    let pin_changes = link::diff_link_pins(repository.clone(), &from_state, &to_state)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                from = %from_sig,
                to = %to_sig,
                ?err,
                "Failed to compare link pins",
            );
            Status::internal(err.to_string())
        })?;

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

    // Emitted ahead of the walk so a consumer sees a link before its contents.
    let mut partitions = PartitionTable::new(repository.id);
    for pin_change in &pin_changes {
        let index = match partitions
            .resolve_or_announce(pin_change.link_repository, tx)
            .await
        {
            Ok(index) => index,
            Err(SendOutcome::ReceiverDropped) => return Ok(()),
            Err(SendOutcome::Sent) => unreachable!("resolve_or_announce returns Sent only via Ok"),
        };
        let payload = Payload::Change(link_pin_change_to_diff_change(pin_change, index));
        match send_payload(tx, payload).await {
            SendOutcome::Sent => {}
            SendOutcome::ReceiverDropped => return Ok(()),
        }
    }

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
    while let Some(item) = producer_rx.recv().await {
        let change = item.filter_slow_down()?.map_err(|err| {
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
            .resolve_or_announce(change.content_repository_id(), tx)
            .await
        {
            Ok(index) => index,
            Err(SendOutcome::ReceiverDropped) => return Ok(()),
            Err(SendOutcome::Sent) => unreachable!("resolve_or_announce returns Sent only via Ok"),
        };
        let payload = Payload::Change(node_change_to_diff_change(&change, index).await);
        match send_payload(tx, payload).await {
            SendOutcome::Sent => {}
            SendOutcome::ReceiverDropped => return Ok(()),
        }
    }

    // Surface any error from the producer task itself.
    let produced = match producer.await {
        Ok(produced) => produced,
        Err(join_err) => {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                ?join_err,
                "2-way revision diff producer task panicked",
            );
            return Err(Status::internal("revision diff producer task failed"));
        }
    };

    match produced.filter_slow_down()? {
        Ok(()) => Ok(()),
        Err(err) => {
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
            .filter_slow_down()?
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
                } else if err.is_invalid_arguments() {
                    Status::invalid_argument(err.to_string())
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

    // One side is an ancestor of the other, for example through a merge's
    // second parent, which the branch checks in `stream_diff` do not see.
    // The 3-way walk only reports target changes at paths the source changed,
    // so it would drop them here; the 2-way diff is the correct answer.
    if base == from_sig || base == to_sig {
        debug!(
            {REPOSITORY_ID} = %repository.id,
            %base,
            "RevisionDiff: base is one side → 2-way",
        );
        return run_two_way(repository, from_sig, from_id, to_sig, to_id, tx).await;
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
            // The display diff reports every changed path individually.
            None,
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
                    .resolve_or_announce(change.content_repository_id(), tx)
                    .await
                {
                    Ok(index) => index,
                    Err(SendOutcome::ReceiverDropped) => return Ok(()),
                    Err(SendOutcome::Sent) => {
                        unreachable!("resolve_or_announce returns Sent only via Ok")
                    }
                };
                Payload::Change(node_change_to_diff_change(&change, index).await)
            }
            DiffItem::Conflict(pair) => {
                let index_from = match partitions
                    .resolve_or_announce(pair.0.content_repository_id(), tx)
                    .await
                {
                    Ok(index) => index,
                    Err(SendOutcome::ReceiverDropped) => return Ok(()),
                    Err(SendOutcome::Sent) => {
                        unreachable!("resolve_or_announce returns Sent only via Ok")
                    }
                };
                let index_to = match partitions
                    .resolve_or_announce(pair.1.content_repository_id(), tx)
                    .await
                {
                    Ok(index) => index,
                    Err(SendOutcome::ReceiverDropped) => return Ok(()),
                    Err(SendOutcome::Sent) => {
                        unreachable!("resolve_or_announce returns Sent only via Ok")
                    }
                };
                Payload::Conflict(diff_conflict_from_pair(&pair, index_from, index_to).await)
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
    if err.is_slow_down() || err.is_oversized() || err.is_max_history_search_depth() {
        Status::resource_exhausted(err.to_string())
    } else if err.is_divergent() {
        Status::failed_precondition(err.to_string())
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
    let from_state = from_res
        .filter_slow_down()?
        .map_err(|err| state_status(repository, from_sig, err))?;
    let to_state = to_res
        .filter_slow_down()?
        .map_err(|err| state_status(repository, to_sig, err))?;
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
struct PartitionTable {
    parent_repository_id: RepositoryId,
    entries: HashMap<RepositoryId, u32>,
}

impl PartitionTable {
    #[lore_macro::test_pub]
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
    #[lore_macro::test_pub]
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
