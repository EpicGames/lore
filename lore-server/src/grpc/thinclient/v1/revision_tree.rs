// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::RevisionTreeRequest;
use lore_proto::lore::thin_client::v1::RevisionTreeResponse;
use lore_proto::lore::thin_client::v1::revision_tree_response::Payload;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::tree;
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

use super::helpers::load_revision_attribution;
use super::helpers::node_flags_to_node_type;
use super::helpers::resolve_to_identifier;
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
/// Server-streams a `RevisionTreeHeader` first (echoing the resolved
/// revision identifier + signature), then one `TreeNode` per entry at
/// or under the optional `path_prefix`, bounded by `max_depth` when
/// set. The header is always emitted before the first node — failures
/// during resolution surface as a non-OK `Status` from the unary part
/// of the call, before the stream begins.
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
            };

            lore_spawn!(
                async move {
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
                    .await;
                }
                .in_current_span()
            );

            let stream: RevisionTreeStream = Box::pin(ReceiverStream::from(rx));
            Ok(Response::new(stream))
        })
        .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_tree(
    repository: Arc<RepositoryContext>,
    signature: Hash,
    path: RelativePath,
    max_depth: usize,
    can_read: lore_revision::state::CanReadRepository,
    header: thin_client_v1::RevisionTreeHeader,
    tx: mpsc::Sender<Result<RevisionTreeResponse, Status>>,
    include_last_revision: bool,
) {
    // Emit header first. If the client has already dropped, just bail.
    if tx
        .send(Ok(RevisionTreeResponse {
            payload: Some(Payload::Header(header)),
        }))
        .await
        .is_err()
    {
        debug!("RevisionTree receiver dropped before header");
        return;
    }

    let result = match tree(
        repository.clone(),
        signature,
        path,
        max_depth,
        can_read,
        include_last_revision,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            let status = if err.is_slow_down() {
                Status::resource_exhausted(err.to_string())
            } else if err.is_invalid_path() {
                Status::invalid_argument("Cannot calculate tree for path that is not a directory")
            } else if err.is_node_not_found() {
                Status::not_found("A node in the tree could not be found")
            } else {
                warn!(
                    {REPOSITORY_ID} = %repository.id, {REVISION} = %signature, ?err,
                    "Failed to walk revision tree",
                );
                warn_error_to_status(&err, |e| Status::internal(e.to_string()))
            };
            let _ = tx.send(Err(status)).await;
            return;
        }
    };

    // Resolved up front so the emission loop does no storage I/O and the
    // resolver is testable on its own.
    let revisions = resolve_attributions(&repository, &result.paths).await;

    emit_tree_stream(result.paths, revisions, tx).await;
}

/// Resolve every attribution the walk referenced, one load per distinct
/// revision. A linked context is built only where the tree spans a link.
///
/// A revision that fails to load maps to `None` rather than being absent, so
/// siblings sharing the key do not retry it.
async fn resolve_attributions(
    repository: &Arc<RepositoryContext>,
    paths: &[lore_revision::state::TreePath],
) -> HashMap<(RepositoryId, Hash), Option<thin_client_v1::Revision>> {
    let mut resolved: HashMap<(RepositoryId, Hash), Option<thin_client_v1::Revision>> =
        HashMap::new();
    for tree_path in paths {
        if tree_path.last_revision.is_zero() {
            continue;
        }
        let key = (tree_path.last_revision_repository, tree_path.last_revision);
        if resolved.contains_key(&key) {
            continue;
        }
        let (repository_id, revision) = key;
        let value = if repository_id == repository.id {
            load_revision_attribution(repository, revision).await
        } else {
            let linked = repository.to_link_context(repository_id).await;
            load_revision_attribution(&linked, revision).await
        };
        resolved.insert(key, value);
    }
    resolved
}

/// Streams `TreeRevision` payloads inline with `TreeNode`s, so a client can
/// render rows as they arrive rather than waiting on the full revision set.
/// Each revision is announced on first reference, before the `TreeNode`
/// naming it.
#[lore_macro::test_pub]
async fn emit_tree_stream(
    paths: Vec<lore_revision::state::TreePath>,
    revisions: HashMap<(RepositoryId, Hash), Option<thin_client_v1::Revision>>,
    tx: mpsc::Sender<Result<RevisionTreeResponse, Status>>,
) {
    // `Some(index)`: Already announced. `None`: Unresolvable, do not retry.
    let mut indices: HashMap<(RepositoryId, Hash), Option<u32>> = HashMap::new();
    let mut next_index: u32 = 1;
    let mut emitted_nodes: u64 = 0;
    let mut emitted_revisions: u64 = 0;

    for tree_path in paths {
        let last_revision_index = if tree_path.last_revision.is_zero() {
            None
        } else {
            let key = (tree_path.last_revision_repository, tree_path.last_revision);
            if let Some(cached) = indices.get(&key).copied() {
                cached
            } else {
                let allocated = match revisions.get(&key) {
                    Some(Some(revision_msg)) => {
                        let index = next_index;
                        // A walk with 4 billion distinct revisions would
                        // wrap past `u32::MAX`. Fail the stream instead.
                        let Some(next) = next_index.checked_add(1) else {
                            warn!("RevisionTree attribution index space exhausted");
                            let _ = tx
                                .send(Err(Status::internal(
                                    "RevisionTree attribution index space exhausted",
                                )))
                                .await;
                            return;
                        };
                        next_index = next;
                        // Emit TreeRevision strictly before the referencing TreeNode.
                        if tx
                            .send(Ok(RevisionTreeResponse {
                                payload: Some(Payload::Revision(thin_client_v1::TreeRevision {
                                    index,
                                    revision: Some(revision_msg.clone()),
                                })),
                            }))
                            .await
                            .is_err()
                        {
                            debug!(
                                emitted_nodes,
                                emitted_revisions,
                                "RevisionTree receiver dropped before TreeRevision",
                            );
                            return;
                        }
                        emitted_revisions += 1;
                        Some(index)
                    }
                    // Unresolvable, or absent because the walk attributed an
                    // entry the resolver never saw. Both mean no index.
                    Some(None) | None => None,
                };
                indices.insert(key, allocated);
                allocated
            }
        };

        let node = thin_client_v1::TreeNode {
            path: tree_path.path.to_string(),
            node_type: node_flags_to_node_type(tree_path.flags) as i32,
            address: tree_path.address.map(model_v1::Address::from),
            size: tree_path.size,
            mode: tree_path.mode,
            tracking: tree_path.tracking,
            last_revision_index,
        };
        if tx
            .send(Ok(RevisionTreeResponse {
                payload: Some(Payload::Node(node)),
            }))
            .await
            .is_err()
        {
            debug!(
                emitted_nodes,
                emitted_revisions, "RevisionTree receiver dropped mid-stream",
            );
            return;
        }
        emitted_nodes += 1;
    }

    debug!(emitted_nodes, emitted_revisions, "RevisionTree complete");
}
