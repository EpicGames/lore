// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::RevisionTreeRequest;
use lore_proto::lore::thin_client::v1::RevisionTreeResponse;
use lore_proto::lore::thin_client::v1::revision_tree_response::Payload;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::tree;
use lore_revision::state::AttributedRevision;
use lore_revision::state::AttributionFailures;
use lore_revision::state::RepositorySpan;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::TreePath;
use lore_revision::state::TreeWalk;
use lore_revision::util::path::RelativePath;
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

use super::helpers::PartitionTable;
use super::helpers::node_flags_to_node_type;
use super::helpers::resolve_to_identifier;
use super::helpers::revision_record;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

#[lore_macro::test_pub]
type RevisionTreeStream =
    Pin<Box<dyn Stream<Item = Result<RevisionTreeResponse, Status>> + Send + 'static>>;

/// `lore.thin_client.v1.ThinClientService.RevisionTree` handler.
///
/// Streams a `RevisionTreeHeader` echoing the resolved revision, then a `TreeNode` per entry below
/// `path_prefix`, `max_depth` levels deep when set. A `DiffPartition` precedes the first item naming
/// each linked repository. With `include_last_revision`, the header says so and a
/// `TreeRevision` precedes the first `TreeNode` naming it. A revision that fails to resolve fails
/// the call before the stream begins.
#[tracing::instrument(name = "RevisionTree::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionTreeRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Response<RevisionTreeStream>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let can_read = link_read_authorizer(&repository_authorizer, request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let Some(query) = req.query else {
        return Err(Status::invalid_argument(
            "RevisionTreeRequest.query must be set (identifier or signature)",
        ));
    };

    let path = match req.path_prefix.as_deref() {
        Some(s) if !s.is_empty() => RelativePath::new_from_initial_path(s)
            .map_err(|err| Status::invalid_argument(format!("invalid path_prefix: {err}")))?,
        _ => RelativePath::new(),
    };
    let max_depth = req.max_depth.map_or(usize::MAX, |d| d as usize);
    let include_last_revision = req.include_last_revision;

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // Resolve up-front so the unary part of the call can surface
            // NotFound / Internal before the stream opens.
            let (signature, identifier) =
                resolve_to_identifier(&repository, query.into(), history_step_size, acceleration)
                    .await?;

            if signature.is_zero() {
                return Err(Status::invalid_argument(
                    "Cannot get the tree of a zeroed revision",
                ));
            }

            let (tx, rx) = mpsc::channel(64);
            let header = thin_client_v1::RevisionTreeHeader {
                identifier: Some(identifier),
                signature: signature.into(),
                last_revision_included: include_last_revision,
            };

            lore_spawn!(
                stream_tree(
                    repository,
                    signature,
                    path,
                    max_depth,
                    can_read,
                    header,
                    tx,
                    include_last_revision,
                )
                .in_current_span()
            );

            let stream: RevisionTreeStream = Box::pin(ReceiverStream::from(rx));
            Ok(Response::new(stream))
        })
        .await
}

/// Streams `header`, then the tree of `signature` below `path`. What attribution leaves
/// unattributed is reported in one warning.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn stream_tree(
    repository: Arc<RepositoryContext>,
    signature: Hash,
    path: RelativePath,
    max_depth: usize,
    can_read: lore_revision::state::CanReadRepository,
    header: thin_client_v1::RevisionTreeHeader,
    tx: mpsc::Sender<Result<RevisionTreeResponse, Status>>,
    include_last_revision: bool,
) -> impl Future<Output = ()> {
    async move {
        let Ok(permit) = tx.reserve().await else {
            debug!("RevisionTree receiver dropped before header");
            return;
        };
        permit.send(Ok(RevisionTreeResponse {
            payload: Some(Payload::Header(header)),
        }));

        let walk = match tree(
            repository.clone(),
            signature,
            path,
            max_depth,
            can_read,
            include_last_revision,
        )
        .await
        {
            Ok(walk) => walk,
            Err(err) => {
                let status = if err.is_slow_down() {
                    Status::resource_exhausted(err.to_string())
                } else if err.is_invalid_path() {
                    Status::invalid_argument(
                        "Cannot calculate tree for path that is not a directory",
                    )
                } else if err.is_node_not_found() {
                    Status::not_found("A node in the tree could not be found")
                } else {
                    warn!(
                        {REPOSITORY_ID} = %repository.id, {REVISION} = %signature, ?err,
                        "Failed to walk revision tree",
                    );
                    warn_error_to_status(&err, |e| Status::internal(e.to_string()))
                };
                if let Ok(permit) = tx.reserve().await {
                    permit.send(Err(status));
                }
                return;
            }
        };

        let TreeWalk {
            paths,
            repository_spans,
            revisions,
            attribution_failures,
            ..
        } = walk;
        let mut report = AttributionReport {
            failures: attribution_failures,
            missing_branches: 0,
        };
        emit_tree_stream(
            &paths,
            &repository_spans,
            &revisions,
            repository.id,
            load_attributed_revision,
            tx,
            &mut report,
        )
        .await;
        if report.failures.count > 0 || report.missing_branches > 0 {
            warn!(
                {REPOSITORY_ID} = %repository.id, {REVISION} = %signature,
                failures = report.failures.count, missing_branches = report.missing_branches,
                error = ?report.failures.error,
                "RevisionTree attribution incomplete",
            );
        }
    }
}

/// What attribution could not do over a stream, in the walk and in the loads.
#[lore_macro::test_pub]
#[derive(Default)]
struct AttributionReport {
    failures: AttributionFailures,
    /// Revisions announced with a zero branch ID, as their metadata names no branch.
    missing_branches: u32,
}

/// Loads `revision` as a `TreeRevision` carries it, answering whether its metadata names no
/// branch, as the record then carries a zero branch ID.
#[lore_macro::test_pub]
async fn load_attributed_revision(
    revision: &AttributedRevision,
) -> Result<(thin_client_v1::Revision, bool), StateError> {
    let state = State::deserialize(revision.repository.clone(), revision.revision).await?;
    let metadata = Metadata::deserialize(revision.repository.clone(), state.metadata_hash())
        .await
        .forward::<StateError>("loading the metadata of an attributed revision")?;
    let branch_id = metadata.get_branch();
    let branch_missing = branch_id.is_err();
    let record = revision_record(
        revision.revision,
        state.revision_number(),
        branch_id.unwrap_or_default(),
        &metadata,
        |_, _, _| {},
    );
    Ok((record, branch_missing))
}

/// How many revisions a stream loads at once, in the order entries first name them.
#[lore_macro::test_pub]
const REVISION_PREFETCH: usize = 8;

/// The announcement state of a revision an entry names that is not yet loaded.
const PENDING: u32 = u32::MAX;

/// The announcement state of each revision of `revisions`, [`PENDING`] for those an entry of
/// `paths` names and 0 for the others, and the revisions entries name, in the order they first
/// name them.
fn announcement_order<'a>(
    paths: &[TreePath],
    revisions: &'a [AttributedRevision],
) -> (Vec<u32>, Vec<&'a AttributedRevision>) {
    if revisions.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut announced = vec![0; revisions.len()];
    let mut order = Vec::new();
    for path in paths {
        if let Some(slot) = (path.last_revision as usize).checked_sub(1)
            && let (Some(announcement), Some(revision)) =
                (announced.get_mut(slot), revisions.get(slot))
            && *announcement == 0
        {
            *announcement = PENDING;
            order.push(revision);
        }
    }
    (announced, order)
}

/// Sends what loading the revision announced as `index`, in the partition `partition` indexes,
/// gave: its `TreeRevision`, or `RESOURCE_EXHAUSTED` for a load refused as `SlowDown`. Any other
/// failure is counted in `report`. Answers the index the entries naming the revision carry, 0 when
/// it is not announced, or `None` when the stream ends.
fn announce(
    permit: mpsc::Permit<'_, Result<RevisionTreeResponse, Status>>,
    loaded: Option<Result<(thin_client_v1::Revision, bool), StateError>>,
    index: u32,
    partition: u32,
    report: &mut AttributionReport,
) -> Option<u32> {
    match loaded {
        Some(Ok((revision, branch_missing))) => {
            report.missing_branches = report
                .missing_branches
                .saturating_add(u32::from(branch_missing));
            permit.send(Ok(RevisionTreeResponse {
                payload: Some(Payload::Revision(thin_client_v1::TreeRevision {
                    index,
                    revision: Some(Box::new(revision)),
                    link_repository_index: partition,
                })),
            }));
            Some(index)
        }
        Some(Err(err)) if err.is_slow_down() => {
            permit.send(Err(Status::resource_exhausted(err.to_string())));
            None
        }
        Some(Err(err)) => {
            report.failures.failed(err);
            Some(0)
        }
        None => Some(0),
    }
}

/// The index of `repository`'s partition on the stream, after sending the `DiffPartition` that
/// announces it on first sight. `None` when the receiver is gone.
async fn partition_index(
    tx: &mpsc::Sender<Result<RevisionTreeResponse, Status>>,
    partitions: &mut PartitionTable,
    repository: RepositoryId,
) -> Option<u32> {
    let permit = tx.reserve().await.ok()?;
    let (index, announcement) = partitions.resolve(repository);
    if let Some(announcement) = announcement {
        permit.send(Ok(RevisionTreeResponse {
            payload: Some(Payload::Partition(announcement)),
        }));
    }
    Some(index)
}

/// Streams a `TreeNode` per entry of `paths`, with the index of the partition its content is in:
/// for a link the linked repository, for any other entry the repository `spans` places it in. Each
/// revision of `revisions` an entry names is announced as a `TreeRevision` before the first
/// `TreeNode` naming it, numbered from 1 in that order, with the index of the repository `spans`
/// places that entry in, which holds the revision. A repository other than `repository`, the one
/// the request names, is announced as a `DiffPartition` before the first item naming it, numbered
/// from 1 in that order.
///
/// `load` loads each revision named once, up to [`REVISION_PREFETCH`] at once, in the order
/// entries first name them, and the record is dropped once sent. A revision that fails to load
/// leaves its entries unattributed, and a load refused as `SlowDown` ends the stream with
/// `RESOURCE_EXHAUSTED`.
#[lore_macro::test_pub]
async fn emit_tree_stream<'a, F>(
    paths: &[TreePath],
    spans: &[RepositorySpan],
    revisions: &'a [AttributedRevision],
    repository: RepositoryId,
    load: impl FnMut(&'a AttributedRevision) -> F,
    tx: mpsc::Sender<Result<RevisionTreeResponse, Status>>,
    report: &mut AttributionReport,
) where
    F: Future<Output = Result<(thin_client_v1::Revision, bool), StateError>>,
{
    let (mut announced, order) = announcement_order(paths, revisions);
    let mut loads = pin!(
        (!order.is_empty()).then(|| stream::iter(order).map(load).buffered(REVISION_PREFETCH))
    );
    let mut partitions = PartitionTable::new(repository);
    let mut next_span = 0;
    let mut span_index = 0;
    let mut next_index: u32 = 1;
    let mut emitted_nodes: u64 = 0;
    let mut emitted_revisions: u64 = 0;

    for (entry, tree_path) in paths.iter().enumerate() {
        let mut entered = None;
        while let Some(span) = spans.get(next_span).filter(|span| span.first <= entry) {
            entered = Some(span.repository);
            next_span += 1;
        }
        if let Some(entered) = entered {
            let Some(index) = partition_index(&tx, &mut partitions, entered).await else {
                debug!(
                    emitted_nodes,
                    emitted_revisions, "RevisionTree receiver dropped before DiffPartition",
                );
                return;
            };
            span_index = index;
        }
        let link_repository_index = match tree_path
            .address
            .filter(|_| tree_path.flags.contains(NodeFlags::Link))
        {
            Some(link) => {
                let Some(index) = partition_index(&tx, &mut partitions, link.context.into()).await
                else {
                    debug!(
                        emitted_nodes,
                        emitted_revisions, "RevisionTree receiver dropped before DiffPartition",
                    );
                    return;
                };
                index
            }
            None => span_index,
        };

        let announcement = (tree_path.last_revision as usize)
            .checked_sub(1)
            .and_then(|slot| announced.get_mut(slot));
        let last_revision_index = match announcement {
            Some(announcement) if *announcement == PENDING => {
                let Ok(permit) = tx.reserve().await else {
                    debug!(
                        emitted_nodes,
                        emitted_revisions, "RevisionTree receiver dropped before TreeRevision",
                    );
                    return;
                };
                let loaded = match loads.as_mut().as_pin_mut() {
                    Some(mut pending) => pending.next().await,
                    None => None,
                };
                let Some(index) = announce(permit, loaded, next_index, span_index, report) else {
                    return;
                };
                if index != 0 {
                    next_index = next_index.saturating_add(1);
                    emitted_revisions += 1;
                }
                *announcement = index;
                index
            }
            Some(announcement) => *announcement,
            None => 0,
        };

        let Ok(permit) = tx.reserve().await else {
            debug!(
                emitted_nodes,
                emitted_revisions, "RevisionTree receiver dropped mid-stream",
            );
            return;
        };
        permit.send(Ok(RevisionTreeResponse {
            payload: Some(Payload::Node(thin_client_v1::TreeNode {
                path: tree_path.path.to_string(),
                node_type: node_flags_to_node_type(tree_path.flags) as i32,
                address: tree_path.address.map(model_v1::Address::from),
                size: tree_path.size,
                mode: tree_path.mode,
                tracking: tree_path.tracking,
                last_revision_index,
                link_repository_index,
            })),
        }));
        emitted_nodes += 1;
    }

    debug!(emitted_nodes, emitted_revisions, "RevisionTree complete");
}
