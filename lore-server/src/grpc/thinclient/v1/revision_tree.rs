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
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

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
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Response<RevisionTreeStream>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let authorization = get_authorization(request.extensions()).ok();
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
    let can_read = link_read_authorizer(authorization);

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

    // Resolve every attribution referenced by the walk into `(RepositoryId,
    // Hash) -> Option<Revision>`. Pulled out of the emission loop so the
    // loop stays synchronous and the resolver's I/O is testable in
    // isolation. `None` inside the map means the resolver saw the key but
    // could not load it (revision missing, metadata corrupt), which is
    // distinct from "key not present" - the latter never happens because
    // the walk never puts an attribution the resolver was not asked about.
    let revisions = resolve_attributions(&repository, &result.paths).await;

    emit_tree_stream(result.paths, revisions, tx).await;
}

/// Resolve every `(RepositoryId, Hash)` attribution referenced by the walk.
///
/// Deduplicates by `(RepositoryId, Hash)` so a large listing with a small
/// number of touching revisions costs one resolution per unique revision.
/// Uses the walked context when the revision belongs to the walked repo,
/// building a linked context only when the tree spans a link.
///
/// Failure is reported as `Some(key -> None)` in the map: siblings on the
/// same key must not retry, and the caller distinguishes "unresolvable"
/// (map contains `None`) from "not attributed" (walk produced a zero
/// `last_revision`, key never lands in the map).
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

/// Interleaved emission: Streams `TreeRevision` payloads inline with
/// `TreeNode`s so a client can render rows as they arrive rather than
/// waiting on the full revision set up front.
///
/// - First reference to a `(repository, revision)` allocates the next index
///   and emits a `TreeRevision` on the stream strictly before the
///   referencing `TreeNode`.
/// - Repeat references reuse the allocated index and emit only the
///   `TreeNode`.
/// - Zero `last_revision` (walk did not attribute the entry) emits the
///   `TreeNode` with `last_revision_index = None`; the map is not touched.
/// - An unresolvable key (present in `revisions` with a `None` value) also
///   emits `last_revision_index = None`; the map keeps the miss so siblings
///   do not retry.
async fn emit_tree_stream(
    paths: Vec<lore_revision::state::TreePath>,
    revisions: HashMap<(RepositoryId, Hash), Option<thin_client_v1::Revision>>,
    tx: mpsc::Sender<Result<RevisionTreeResponse, Status>>,
) {
    // `Some(index)` = TreeRevision already announced under this key;
    // `None` = key was unresolvable, siblings should not retry.
    let mut indices: HashMap<(RepositoryId, Hash), Option<u32>> = HashMap::new();
    let mut next_index: u32 = 1;
    let mut emitted_nodes: u64 = 0;
    let mut emitted_revisions: u64 = 0;

    for tree_path in paths {
        let last_revision_index = if tree_path.last_revision.is_zero() {
            None
        } else {
            let key = (tree_path.last_revision_repository, tree_path.last_revision);
            match indices.get(&key).copied() {
                Some(cached) => cached,
                None => {
                    let allocated = match revisions.get(&key) {
                        Some(Some(revision_msg)) => {
                            let index = next_index;
                            // Guards a 4-billion-distinct-revision walk from silently
                            // wrapping past `u32::MAX`. On overflow, the stream reports
                            // `Status::internal` and terminates.
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
                                    payload: Some(Payload::Revision(
                                        thin_client_v1::TreeRevision {
                                            index,
                                            revision: Some(revision_msg.clone()),
                                        },
                                    )),
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
                        // Key resolved to `None`, or key not in the map at all (the
                        // walk emitted an attribution the resolver did not see; treat
                        // as unresolvable so the loop defends against a producer bug).
                        Some(None) | None => None,
                    };
                    indices.insert(key, allocated);
                    allocated
                }
            }
        };

        let node = thin_client_v1::TreeNode {
            path: tree_path.path.to_string(),
            node_type: node_flags_to_node_type(tree_path.flags) as i32,
            address: tree_path.address.map(|address| model_v1::Address {
                hash: address.hash.into(),
                context: address.context.into(),
            }),
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

#[cfg(test)]
mod test {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_proto::lore::thin_client::v1::revision_tree_request::Query;
    use lore_revision::branch;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::link::LinkFlags;
    use lore_revision::lore::BranchId;
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
    use crate::grpc::server::RevisionListAcceleration;
    use crate::store::test_store_create;

    fn make_request(
        repository: RepositoryId,
        query: Query,
        path_prefix: Option<String>,
        max_depth: Option<u32>,
    ) -> Request<RevisionTreeRequest> {
        let mut request = Request::new(RevisionTreeRequest {
            query: Some(query),
            path_prefix,
            max_depth,
            include_last_revision: false,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    /// `make_request` with attribution asked for.
    fn make_attributed_request(
        repository: RepositoryId,
        query: Query,
        max_depth: Option<u32>,
    ) -> Request<RevisionTreeRequest> {
        let mut request = make_request(repository, query, None, max_depth);
        request.get_mut().include_last_revision = true;
        request
    }

    /// Pushes a fresh branch and one revision for each of `revisions`; each
    /// revision's state contains `revisions[i]` as File nodes at the root.
    /// Returns the branch id and the revision signatures in push order.
    async fn push_branch_with_revisions(
        repository: &Arc<RepositoryContext>,
        revisions: &[&[&str]],
    ) -> (BranchId, Vec<Hash>) {
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch_id,
            "test-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let mut signatures = Vec::with_capacity(revisions.len());
        let mut parent = Hash::default();
        for (idx, files) in revisions.iter().enumerate() {
            let mut metadata = Metadata::new();
            metadata.set_branch(branch_id).expect("set branch");
            let metadata_hash = metadata
                .serialize(repository.clone())
                .await
                .expect("serialize metadata");

            let state = Arc::new(state::State::new());
            state.set_parent_self(parent);
            state.set_revision_number((idx + 1) as u64);
            state.set_metadata_hash(metadata_hash);
            for name in *files {
                let node = Node {
                    flags: NodeFlags::File.bits(),
                    name_hash: hash_string(name),
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
            let signature = branch_push::push(
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
            .revision;
            signatures.push(signature);
            parent = signature;
        }
        (branch_id, signatures)
    }

    /// Push a branch with one revision laying out a small tree:
    /// `top.txt` and `subdir/inner.txt`.
    async fn push_branch_with_subdir(repository: &Arc<RepositoryContext>) -> (BranchId, Hash) {
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch_id,
            "test-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = Arc::new(state::State::new());
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        state.set_metadata_hash(metadata_hash);

        // Top-level file.
        let top = Node {
            flags: NodeFlags::File.bits(),
            name_hash: hash_string("top.txt"),
            ..Default::default()
        };
        state
            .node_add(repository.clone(), ROOT_NODE, top, "top.txt")
            .await
            .expect("node_add top.txt");

        // Top-level directory (no File/Link bits = directory).
        let subdir_node = Node {
            flags: NodeFlags::NoFlags.bits(),
            name_hash: hash_string("subdir"),
            ..Default::default()
        };
        let subdir_id = state
            .node_add(repository.clone(), ROOT_NODE, subdir_node, "subdir")
            .await
            .expect("node_add subdir");

        // File inside the subdirectory.
        let inner = Node {
            flags: NodeFlags::File.bits(),
            name_hash: hash_string("inner.txt"),
            ..Default::default()
        };
        state
            .node_add(repository.clone(), subdir_id, inner, "inner.txt")
            .await
            .expect("node_add inner.txt");

        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let signature = branch_push::push(
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
        .revision;
        (branch_id, signature)
    }

    /// Pushes a fresh branch and one revision with `file_names` as File
    /// nodes at the root. Returns the branch id and the revision
    /// signature. Convenience wrapper around `push_branch_with_revisions`
    /// for tests that don't care about history depth.
    async fn push_branch_with_files(
        repository: &Arc<RepositoryContext>,
        file_names: &[&str],
    ) -> (BranchId, Hash) {
        let (branch_id, signatures) = push_branch_with_revisions(repository, &[file_names]).await;
        (branch_id, signatures[0])
    }

    /// `link_branch` registers a link reference for the mount node: zero for a
    /// link following its parent's branch, an explicit id for a pinned one.
    /// `None` leaves the reference unregistered, as an unresolvable link.
    async fn push_branch_with_link(
        repository: &Arc<RepositoryContext>,
        link_name: &str,
        target_repo: RepositoryId,
        target_revision: Hash,
        target_node: lore_revision::node::NodeID,
        link_branch: Option<BranchId>,
    ) -> (BranchId, Hash) {
        use lore_base::types::Address;

        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch_id,
            "test-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = Arc::new(state::State::new());
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        state.set_metadata_hash(metadata_hash);

        let link_node = Node {
            flags: NodeFlags::Link.bits(),
            child: target_node,
            address: Address {
                hash: target_revision,
                context: target_repo.into(),
            },
            name_hash: hash_string(link_name),
            ..Default::default()
        };
        let link_node_id = state
            .node_add(repository.clone(), ROOT_NODE, link_node, link_name)
            .await
            .expect("node_add link");

        if let Some(link_branch) = link_branch {
            state
                .link_add(
                    repository.clone(),
                    target_repo,
                    link_branch,
                    target_revision,
                    link_node_id,
                    LinkFlags::NoFlags,
                )
                .await
                .expect("link_add reference");
        }

        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let signature = branch_push::push(
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
        .revision;
        (branch_id, signature)
    }

    async fn collect(
        response: Response<RevisionTreeStream>,
    ) -> Vec<Result<RevisionTreeResponse, Status>> {
        response.into_inner().collect().await
    }

    #[tokio::test]
    async fn unset_query_returns_invalid_argument() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let mut request = Request::new(RevisionTreeRequest {
                query: None,
                path_prefix: None,
                max_depth: None,
                include_last_revision: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            let err = match handler(
                request,
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            {
                Ok(_) => panic!("unset query should fail"),
                Err(err) => err,
            };
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn signature_query_emits_header_then_nodes() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signature) =
                push_branch_with_files(&repository_context, &["a.txt", "b.txt"]).await;

            let response = handler(
                make_request(repository, Query::Signature(signature.into()), None, None),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();

            // First message is the header echoing the resolved
            // identifier and signature.
            let header = match &items[0].payload {
                Some(Payload::Header(h)) => h,
                other => panic!("expected header first, got {other:?}"),
            };
            assert_eq!(Hash::from(header.signature.as_ref()), signature);
            let identifier = header.identifier.as_ref().expect("identifier");
            assert_eq!(identifier.number, 1);
            assert_eq!(BranchId::from(&identifier.branch_id), branch_id);

            // Remaining messages are TreeNode payloads.
            let nodes: Vec<&thin_client_v1::TreeNode> = items[1..]
                .iter()
                .map(|item| match &item.payload {
                    Some(Payload::Node(n)) => n,
                    other => panic!("expected node payload, got {other:?}"),
                })
                .collect();
            // The walk emits both files (in server-natural order).
            assert!(nodes.iter().any(|n| n.path == "a.txt"));
            assert!(nodes.iter().any(|n| n.path == "b.txt"));
            assert!(
                nodes
                    .iter()
                    .all(|n| n.node_type == thin_client_v1::NodeType::File as i32)
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn identifier_query_with_zero_resolves_latest() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signature) =
                push_branch_with_files(&repository_context, &["only.txt"]).await;

            let response = handler(
                make_request(
                    repository,
                    Query::Identifier(model_v1::RevisionIdentifier {
                        branch_id: branch_id.into(),
                        number: 0,
                    }),
                    None,
                    None,
                ),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
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
            assert_eq!(Hash::from(header.signature.as_ref()), signature);
            assert_eq!(header.identifier.as_ref().unwrap().number, 1);
        }))
        .await;
    }

    #[tokio::test]
    async fn empty_revision_emits_header_only() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signature) = push_branch_with_files(&repository_context, &[]).await;

            let response = handler(
                make_request(repository, Query::Signature(signature.into()), None, None),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let items: Vec<_> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect();
            assert_eq!(items.len(), 1);
            assert!(matches!(items[0].payload, Some(Payload::Header(_))));
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
            let err = match handler(
                make_request(repository, Query::Signature(bogus.into()), None, None),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            {
                Ok(_) => panic!("unknown signature should fail"),
                Err(err) => err,
            };
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn zero_signature_returns_invalid_argument() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let err = match handler(
                make_request(
                    repository,
                    Query::Signature(Hash::default().into()),
                    None,
                    None,
                ),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            {
                Ok(_) => panic!("zeroed revision should fail"),
                Err(err) => err,
            };

            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert_eq!(err.message(), "Cannot get the tree of a zeroed revision");
        }))
        .await;
    }

    #[tokio::test]
    async fn path_prefix_pointing_at_file_returns_invalid_argument_on_stream() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signature) =
                push_branch_with_files(&repository_context, &["file.txt"]).await;

            let response = handler(
                make_request(
                    repository,
                    Query::Signature(signature.into()),
                    Some("file.txt".into()),
                    None,
                ),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("unary part succeeds");

            // The header is emitted first; the error follows on the stream.
            let items: Vec<_> = collect(response).await;
            assert!(matches!(
                items[0].as_ref().unwrap().payload,
                Some(Payload::Header(_))
            ));
            let err = items[1].as_ref().expect_err("expected error item");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn path_prefix_pointing_at_missing_returns_not_found_on_stream() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signature) = push_branch_with_files(&repository_context, &[]).await;

            let response = handler(
                make_request(
                    repository,
                    Query::Signature(signature.into()),
                    Some("missing".into()),
                    None,
                ),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("unary part succeeds");

            let items: Vec<_> = collect(response).await;
            assert!(matches!(
                items[0].as_ref().unwrap().payload,
                Some(Payload::Header(_))
            ));
            let err = items[1].as_ref().expect_err("expected error item");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn identifier_query_with_concrete_number_returns_that_revision() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // Two revisions: rev 1 with "first.txt", rev 2 with both files.
            let (branch_id, signatures) = push_branch_with_revisions(
                &repository_context,
                &[&["first.txt"], &["first.txt", "second.txt"]],
            )
            .await;

            // Querying (branch, 1) MUST resolve to revision 1 — the
            // earlier signature, not the latest. Exercises the
            // revision::resolve(`branch@N`) path.
            let response = handler(
                make_request(
                    repository,
                    Query::Identifier(model_v1::RevisionIdentifier {
                        branch_id: branch_id.into(),
                        number: 1,
                    }),
                    None,
                    None,
                ),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
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
            assert_eq!(header.identifier.as_ref().unwrap().number, 1);
            assert_eq!(Hash::from(header.signature.as_ref()), signatures[0]);
        }))
        .await;
    }

    #[tokio::test]
    async fn path_prefix_at_subdirectory_emits_descendants() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signature) = push_branch_with_subdir(&repository_context).await;

            // Walking from `subdir` should not see `top.txt`.
            let response = handler(
                make_request(
                    repository,
                    Query::Signature(signature.into()),
                    Some("subdir".into()),
                    None,
                ),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .filter_map(|item| match item.payload {
                    Some(Payload::Node(n)) => Some(n),
                    _ => None,
                })
                .collect();

            let paths: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();
            assert!(
                paths.contains(&"subdir/inner.txt"),
                "expected subdir/inner.txt in {paths:?}",
            );
            assert!(
                !paths.contains(&"top.txt"),
                "top.txt should be filtered out, got {paths:?}",
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn max_depth_one_excludes_grandchildren() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signature) = push_branch_with_subdir(&repository_context).await;

            // max_depth = 1 from root: direct children only. The subdir
            // entry is emitted, but `subdir/inner.txt` (a grandchild)
            // must not be.
            let response = handler(
                make_request(
                    repository,
                    Query::Signature(signature.into()),
                    None,
                    Some(1),
                ),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let paths: Vec<String> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .filter_map(|item| match item.payload {
                    Some(Payload::Node(n)) => Some(n.path),
                    _ => None,
                })
                .collect();
            assert!(
                paths.iter().any(|p| p == "top.txt"),
                "expected top.txt in {paths:?}",
            );
            assert!(
                paths.iter().any(|p| p == "subdir"),
                "expected subdir in {paths:?}",
            );
            assert!(
                paths.iter().all(|p| p != "subdir/inner.txt"),
                "max_depth=1 must exclude grandchildren, got {paths:?}",
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn unreachable_link_emits_link_node_then_skips_subtree() {
        // The link points at (target_repo, target_revision), but no state has
        // been pushed for that pair. The walker must still emit the link node
        // and continue (silently skipping the absent subtree), per the spec's
        // failure-mode contract.
        let repository = random::<RepositoryId>();
        let target_repo = random::<RepositoryId>();
        let target_revision = Hash::from(random::<[u8; 32]>());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signature) = push_branch_with_link(
                &repository_context,
                "linked",
                target_repo,
                target_revision,
                ROOT_NODE,
                None,
            )
            .await;

            let response = handler(
                make_request(repository, Query::Signature(signature.into()), None, None),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .filter_map(|item| match item.payload {
                    Some(Payload::Node(n)) => Some(n),
                    _ => None,
                })
                .collect();

            assert_eq!(
                nodes.len(),
                1,
                "expected exactly the link entry, got {nodes:?}"
            );
            let link_node = &nodes[0];
            assert_eq!(link_node.path, "linked");
            assert_eq!(link_node.node_type, thin_client_v1::NodeType::Link as i32);
            let address = link_node.address.as_ref().expect("link address");
            assert_eq!(
                Hash::from(address.hash.as_ref()),
                target_revision,
                "link.address.hash should be the linked revision signature",
            );
            let emitted_context = lore_base::types::Context::from(address.context.as_ref());
            assert_eq!(
                emitted_context,
                target_repo.into(),
                "link.address.context should be the target repository id",
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn link_emits_linked_repository_contents() {
        // Build a real linked repo, push one revision with `inner.txt` at its
        // root, then build the originating repo and push a revision whose
        // root contains a link node pointing at the linked revision. The
        // walker should emit: link entry, then the entry under it.
        let originating = random::<RepositoryId>();
        let linked = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let linked_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                linked,
            ));
            let (_branch, linked_signature) =
                push_branch_with_files(&linked_context, &["inner.txt"]).await;

            let originating_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                originating,
            ));
            let (_branch, signature) = push_branch_with_link(
                &originating_context,
                "linked",
                linked,
                linked_signature,
                ROOT_NODE,
                None,
            )
            .await;

            let response = handler(
                make_request(originating, Query::Signature(signature.into()), None, None),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .filter_map(|item| match item.payload {
                    Some(Payload::Node(n)) => Some(n),
                    _ => None,
                })
                .collect();

            // Expect exactly two entries: the link mount, then the file inside.
            assert_eq!(nodes.len(), 2, "expected link + child, got {nodes:?}");

            let link = &nodes[0];
            assert_eq!(link.path, "linked");
            assert_eq!(link.node_type, thin_client_v1::NodeType::Link as i32);
            let link_address = link.address.as_ref().expect("link address");
            assert_eq!(
                lore_base::types::Context::from(link_address.context.as_ref()),
                linked.into(),
                "link node's address.context is the target repository id, so clients \
                 can resolve where to fetch linked content from",
            );
            assert_eq!(
                Hash::from(link_address.hash.as_ref()),
                linked_signature,
                "link node's address.hash is the linked revision signature",
            );

            let child = &nodes[1];
            assert_eq!(child.path, "linked/inner.txt");
            assert_eq!(child.node_type, thin_client_v1::NodeType::File as i32);
        }))
        .await;
    }

    /// Push a revision holding a single link node whose reference is registered
    /// on `link_branch`, then return the `TreeNode` emitted for the mount path.
    /// The link target is never pushed, so the walk stops at the mount.
    async fn link_tree_node(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        repository: RepositoryId,
        link_branch: BranchId,
    ) -> thin_client_v1::TreeNode {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signature) = push_branch_with_link(
            &repository_context,
            "linked",
            random::<RepositoryId>(),
            Hash::from(random::<[u8; 32]>()),
            ROOT_NODE,
            Some(link_branch),
        )
        .await;

        let response = handler(
            make_request(repository, Query::Signature(signature.into()), None, None),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let mut nodes: Vec<thin_client_v1::TreeNode> = collect(response)
            .await
            .into_iter()
            .map(|item| item.expect("stream item"))
            .filter_map(|item| match item.payload {
                Some(Payload::Node(node)) => Some(node),
                _ => None,
            })
            .collect();

        assert_eq!(
            nodes.len(),
            1,
            "expected exactly the link entry, got {nodes:?}"
        );
        let link_node = nodes.pop().expect("one node");
        assert_eq!(link_node.path, "linked");
        assert_eq!(link_node.node_type, thin_client_v1::NodeType::Link as i32);
        link_node
    }

    /// A zero-branch link reports `tracking = true`.
    #[tokio::test]
    async fn tree_marks_zero_branch_link_as_tracking() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let link_node = link_tree_node(
                immutable_store,
                mutable_store,
                repository,
                BranchId::default(),
            )
            .await;
            assert!(
                link_node.tracking,
                "a zero-branch link must be reported as tracking",
            );
        }))
        .await;
    }

    /// A link pinned to an explicit branch reports `tracking = false`.
    #[tokio::test]
    async fn tree_marks_pinned_link_as_not_tracking() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let link_node = link_tree_node(
                immutable_store,
                mutable_store,
                repository,
                BranchId::from(uuid::Uuid::now_v7()),
            )
            .await;
            assert!(
                !link_node.tracking,
                "a pinned (non-zero branch) link must not be reported as tracking",
            );
        }))
        .await;
    }

    /// Build an `AuthorizationToken` whose `resources` list authorizes only
    /// the given repository ids. Anything else is unauthorized.
    fn token_authorized_for(repos: &[RepositoryId]) -> crate::auth::jwt::AuthorizationToken {
        use crate::auth::jwt::AuthorizationToken;
        use crate::auth::jwt::ResourcePermission;

        AuthorizationToken {
            resources: Some(
                repos
                    .iter()
                    .map(|id| ResourcePermission {
                        permission: vec![],
                        resource_id: format!("urc-{id}"),
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn unauthorized_link_emits_link_only() {
        // Same setup as link_emits_linked_repository_contents, but the
        // request carries an authorization token that only grants access to
        // the originating repo. The walker emits the link node but does
        // not descend into the linked repo.
        let originating = random::<RepositoryId>();
        let linked = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let linked_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                linked,
            ));
            let (_branch, linked_signature) =
                push_branch_with_files(&linked_context, &["inner.txt"]).await;

            let originating_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                originating,
            ));
            let (_branch, signature) = push_branch_with_link(
                &originating_context,
                "linked",
                linked,
                linked_signature,
                ROOT_NODE,
                None,
            )
            .await;

            let mut request =
                make_request(originating, Query::Signature(signature.into()), None, None);
            request
                .extensions_mut()
                .insert(token_authorized_for(&[originating]));

            let response = handler(
                request,
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .filter_map(|item| match item.payload {
                    Some(Payload::Node(n)) => Some(n),
                    _ => None,
                })
                .collect();

            assert_eq!(
                nodes.len(),
                1,
                "expected only the link entry, got {nodes:?}"
            );
            assert_eq!(nodes[0].node_type, thin_client_v1::NodeType::Link as i32);
            assert_eq!(nodes[0].path, "linked");
        }))
        .await;
    }

    #[tokio::test]
    async fn nested_links_emit_contents_from_every_repository() {
        // A -> B -> C. Walking A emits A's link to B, B's link to C, then
        // C's file. The two link entries carry their target repo id in
        // address.context (where clients should fetch linked content from).
        let a = random::<RepositoryId>();
        let b = random::<RepositoryId>();
        let c = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let c_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                c,
            ));
            let (_branch_c, c_sig) = push_branch_with_files(&c_context, &["c_file.txt"]).await;

            // B's revision: one link to C.
            let b_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                b,
            ));
            let (_branch_b, b_sig) =
                push_branch_with_link(&b_context, "link_to_c", c, c_sig, ROOT_NODE, None).await;

            // A's revision: one link to B.
            let a_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                a,
            ));
            let (_branch_a, a_sig) =
                push_branch_with_link(&a_context, "link_to_b", b, b_sig, ROOT_NODE, None).await;

            let response = handler(
                make_request(a, Query::Signature(a_sig.into()), None, None),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .filter_map(|item| match item.payload {
                    Some(Payload::Node(n)) => Some(n),
                    _ => None,
                })
                .collect();

            let paths_and_types: Vec<(&str, i32)> = nodes
                .iter()
                .map(|n| (n.path.as_str(), n.node_type))
                .collect();
            assert_eq!(
                paths_and_types,
                vec![
                    ("link_to_b", thin_client_v1::NodeType::Link as i32),
                    ("link_to_b/link_to_c", thin_client_v1::NodeType::Link as i32),
                    (
                        "link_to_b/link_to_c/c_file.txt",
                        thin_client_v1::NodeType::File as i32,
                    ),
                ],
                "expected A's link to B, B's link to C, then C's file",
            );

            // Each LINK node's address.context is the *target* repo id —
            // i.e. where clients fetch the linked content from.
            let target_context = |idx: usize| {
                let addr = nodes[idx].address.as_ref().expect("link address");
                lore_base::types::Context::from(addr.context.as_ref())
            };
            assert_eq!(target_context(0), b.into(), "link_to_b targets repo B");
            assert_eq!(target_context(1), c.into(), "link_to_c targets repo C");
        }))
        .await;
    }

    #[tokio::test]
    async fn distinct_links_to_same_repository_both_walked() {
        // Two siblings in repo A both link to repo B at the same signature.
        // Both linked subtrees must stream — neither sibling is "inside" the
        // other, so the depth bound applies independently.
        let a = random::<RepositoryId>();
        let b = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let b_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                b,
            ));
            let (_branch_b, b_sig) = push_branch_with_files(&b_context, &["shared.txt"]).await;

            // A's revision: two link nodes pointing at the same B revision.
            let a_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                a,
            ));
            let write_token = get_write_token();
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            branch::create(
                a_context.clone(),
                &write_token,
                branch_id,
                "test-branch",
                branch::default_category(),
                "creator",
                1,
                vec![],
                false,
                false,
            )
            .await
            .expect("create branch");

            let mut metadata = Metadata::new();
            metadata.set_branch(branch_id).expect("set branch");
            let metadata_hash = metadata
                .serialize(a_context.clone())
                .await
                .expect("serialize metadata");

            let state = Arc::new(state::State::new());
            state.set_parent_self(Hash::default());
            state.set_revision_number(1);
            state.set_metadata_hash(metadata_hash);

            for name in ["first", "second"] {
                let link_node = Node {
                    flags: NodeFlags::Link.bits(),
                    child: ROOT_NODE,
                    address: lore_base::types::Address {
                        hash: b_sig,
                        context: b.into(),
                    },
                    name_hash: hash_string(name),
                    ..Default::default()
                };
                state
                    .node_add(a_context.clone(), ROOT_NODE, link_node, name)
                    .await
                    .expect("node_add");
            }

            let serialized = state
                .serialize(a_context.clone(), &write_token)
                .await
                .expect("serialize state");
            let a_sig = branch_push::push(
                a_context.clone(),
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
            .revision;

            let response = handler(
                make_request(a, Query::Signature(a_sig.into()), None, None),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("handler ok");

            let nodes: Vec<thin_client_v1::TreeNode> = collect(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .filter_map(|item| match item.payload {
                    Some(Payload::Node(n)) => Some(n),
                    _ => None,
                })
                .collect();

            let paths: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();
            assert!(paths.contains(&"first"), "missing first link in {paths:?}");
            assert!(
                paths.contains(&"first/shared.txt"),
                "missing first/shared.txt in {paths:?} — sibling links to the \
                 same repo must each walk",
            );
            assert!(
                paths.contains(&"second"),
                "missing second link in {paths:?}"
            );
            assert!(
                paths.contains(&"second/shared.txt"),
                "missing second/shared.txt in {paths:?} — siblings must each \
                 walk independently of each other",
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn path_prefix_into_unauthorized_link_returns_not_found() {
        // path_prefix points inside the linked repo, but the caller is not
        // authorized for it. The handler must return Status::not_found (the
        // same shape as a path that does not exist) — never PermissionDenied.
        let originating = random::<RepositoryId>();
        let linked = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let linked_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                linked,
            ));
            let (_branch, linked_signature) =
                push_branch_with_files(&linked_context, &["inner.txt"]).await;

            let originating_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                originating,
            ));
            let (_branch, signature) = push_branch_with_link(
                &originating_context,
                "linked",
                linked,
                linked_signature,
                ROOT_NODE,
                None,
            )
            .await;

            let mut request = make_request(
                originating,
                Query::Signature(signature.into()),
                Some("linked/inner.txt".into()),
                None,
            );
            request
                .extensions_mut()
                .insert(token_authorized_for(&[originating]));

            let response = handler(
                request,
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            .expect("unary part succeeds");

            let items: Vec<_> = collect(response).await;
            // Header is emitted before the error.
            assert!(matches!(
                items[0].as_ref().unwrap().payload,
                Some(Payload::Header(_))
            ));
            let err = items[1].as_ref().expect_err("expected error item");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    /// The existing fixtures push through `branch_push::push`, which does
    /// not run the delta-block / file-metadata write path that populates
    /// per-entry attribution, so even an attributed request sees no
    /// `TreeRevision` on the wire.
    ///
    /// Real attribution correctness is proved in
    /// `lore-revision/tests/file_revision_history.rs`. Here, we just ensure
    /// the request flag is honoured and the stream still emits the same nodes.
    #[tokio::test]
    async fn last_revision_is_gated_by_the_request_flag() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signatures) =
                push_branch_with_revisions(&repository_context, &[&["top.txt"]]).await;
            let tip = *signatures.last().expect("one revision");

            let items = async |request| {
                collect(
                    handler(
                        request,
                        immutable_store.clone(),
                        mutable_store.clone(),
                        DEFAULT_HISTORY_STEP_SIZE,
                        RevisionListAcceleration::default(),
                    )
                    .await
                    .expect("handler ok"),
                )
                .await
                .into_iter()
                .map(|r| r.expect("stream item"))
                .collect::<Vec<_>>()
            };

            let split = |items: Vec<RevisionTreeResponse>| {
                let mut nodes: Vec<thin_client_v1::TreeNode> = Vec::new();
                let mut revisions: Vec<thin_client_v1::TreeRevision> = Vec::new();
                for item in items {
                    match item.payload {
                        Some(Payload::Node(n)) => nodes.push(n),
                        Some(Payload::Revision(r)) => revisions.push(r),
                        _ => {}
                    }
                }
                (nodes, revisions)
            };

            let (unattributed_nodes, unattributed_revisions) = split(
                items(make_request(
                    repository,
                    Query::Signature(tip.into()),
                    None,
                    None,
                ))
                .await,
            );
            assert!(!unattributed_nodes.is_empty(), "the walk must emit nodes");
            assert!(
                unattributed_nodes
                    .iter()
                    .all(|node| node.last_revision_index.is_none()),
                "last_revision_index must be absent when not requested",
            );
            assert!(
                unattributed_revisions.is_empty(),
                "no TreeRevision payloads without the flag",
            );

            let (attributed_nodes, attributed_revisions) = split(
                items(make_attributed_request(
                    repository,
                    Query::Signature(tip.into()),
                    None,
                ))
                .await,
            );
            assert_eq!(
                attributed_nodes.len(),
                unattributed_nodes.len(),
                "attribution must not change which nodes are emitted",
            );
            // The fixture does not populate delta-block back-pointers, so
            // every entry attributes to zero and no TreeRevision goes out.
            // Real attribution is proved in `file_revision_history.rs`.
            assert!(
                attributed_revisions.is_empty(),
                "hand-serialised-state fixture: no TreeRevision payloads even attributed",
            );
            assert!(
                attributed_nodes
                    .iter()
                    .all(|node| node.last_revision_index.is_none()),
                "hand-serialised-state fixture: no last_revision_index populated",
            );
        }))
        .await;
    }

    /// The `emit_tree_stream` mechanics, driven by a hand-crafted walk result
    /// and a hand-crafted revisions map. No fixture is needed because emission
    /// is a pure state machine over its two inputs.
    ///
    /// The wire-level counterpart lives in `scripts/test/`: a real committed
    /// fixture, end-to-end, proving the same mechanics against a running server.
    mod emission {
        use bytes::Bytes;

        use super::*;

        fn tree_path(
            path: &str,
            last_revision_repository: RepositoryId,
            last_revision: Hash,
        ) -> lore_revision::state::TreePath {
            lore_revision::state::TreePath {
                path: lore_revision::util::path::RelativePath::new_from_initial_path(path)
                    .expect("valid path"),
                address: None,
                flags: NodeFlags::File,
                size: 0,
                mode: 0,
                tracking: false,
                last_revision,
                last_revision_repository,
            }
        }

        fn attribution(signature: Hash, message: &str) -> thin_client_v1::Revision {
            thin_client_v1::Revision {
                signature: Bytes::from(signature),
                commit_message: message.into(),
                ..Default::default()
            }
        }

        async fn drain(
            paths: Vec<lore_revision::state::TreePath>,
            revisions: HashMap<(RepositoryId, Hash), Option<thin_client_v1::Revision>>,
        ) -> (
            Vec<thin_client_v1::TreeRevision>,
            Vec<thin_client_v1::TreeNode>,
            Vec<u32>,
        ) {
            // Channel capacity comfortably exceeds any test's emission volume,
            // so `emit_tree_stream` never blocks on the sender side. We can
            // therefore run emission inline, drop `tx`, and drain the receiver
            // - no spawn needed. Avoiding `tokio::spawn` here is deliberate:
            // production spawns propagate LORE_CONTEXT via lore_base::lore_spawn,
            // and a test using bare `tokio::spawn` would violate the
            // clippy::disallowed_methods gate.
            let (tx, mut rx) = mpsc::channel(1024);
            emit_tree_stream(paths, revisions, tx).await;
            let mut in_order_revisions: Vec<thin_client_v1::TreeRevision> = Vec::new();
            let mut in_order_nodes: Vec<thin_client_v1::TreeNode> = Vec::new();
            // Track the *stream* order of TreeRevision indices as seen by a
            // client; a duplicate here would falsify the "announce once"
            // requirement, and a missing referenced index would falsify the
            // "before the referencing node" requirement.
            let mut announced: Vec<u32> = Vec::new();
            while let Some(item) = rx.recv().await {
                match item.expect("stream item").payload {
                    Some(Payload::Revision(r)) => {
                        announced.push(r.index);
                        in_order_revisions.push(r);
                    }
                    Some(Payload::Node(n)) => {
                        if let Some(idx) = n.last_revision_index {
                            assert!(
                                announced.contains(&idx),
                                "TreeNode referenced index {idx} that has not been announced yet: {announced:?}",
                            );
                        }
                        in_order_nodes.push(n);
                    }
                    other => panic!("unexpected payload {other:?}"),
                }
            }
            (in_order_revisions, in_order_nodes, announced)
        }

        /// Two entries sharing one revision: exactly one `TreeRevision` at
        /// index 1, both `TreeNode`s point to it.
        #[tokio::test]
        async fn dedupes_by_repository_hash() {
            let repo = random::<RepositoryId>();
            let rev = Hash::from_u64(0x11);
            let paths = vec![tree_path("a.txt", repo, rev), tree_path("b.txt", repo, rev)];
            let revisions = HashMap::from([((repo, rev), Some(attribution(rev, "shared")))]);
            let (announced, nodes, _order) = drain(paths, revisions).await;

            assert_eq!(announced.len(), 1, "one shared revision announced once");
            assert_eq!(announced[0].index, 1, "indexing starts at 1");
            assert_eq!(nodes.len(), 2);
            assert_eq!(nodes[0].last_revision_index, Some(1));
            assert_eq!(nodes[1].last_revision_index, Some(1));
        }

        /// Distinct revisions on the same repo: two `TreeRevision`s at
        /// indices 1 and 2, in the order they are first referenced.
        #[tokio::test]
        async fn distinct_revisions_get_sequential_indices() {
            let repo = random::<RepositoryId>();
            let older = Hash::from_u64(0x11);
            let newer = Hash::from_u64(0x22);
            let paths = vec![
                tree_path("old.txt", repo, older),
                tree_path("new.txt", repo, newer),
            ];
            let revisions = HashMap::from([
                ((repo, older), Some(attribution(older, "r1"))),
                ((repo, newer), Some(attribution(newer, "r2"))),
            ]);
            let (announced, nodes, order) = drain(paths, revisions).await;

            assert_eq!(order, vec![1, 2], "sequential in first-seen order");
            assert_eq!(announced.len(), 2);
            assert_eq!(nodes[0].last_revision_index, Some(1));
            assert_eq!(nodes[1].last_revision_index, Some(2));
        }

        /// An unresolvable key (map contains `None`) never announces a
        /// `TreeRevision` and leaves `TreeNode.last_revision_index` unset.
        /// Siblings sharing the same key must not retry the resolver -
        /// there is no resolver on this side of the boundary.
        #[tokio::test]
        async fn unresolvable_key_leaves_index_none() {
            let repo = random::<RepositoryId>();
            let rev = Hash::from_u64(0xdead);
            let paths = vec![tree_path("a.txt", repo, rev), tree_path("b.txt", repo, rev)];
            let revisions = HashMap::from([((repo, rev), None)]);
            let (announced, nodes, _order) = drain(paths, revisions).await;

            assert!(
                announced.is_empty(),
                "no TreeRevision for an unresolvable key"
            );
            assert_eq!(nodes.len(), 2);
            assert!(nodes.iter().all(|n| n.last_revision_index.is_none()));
        }

        /// Zero `last_revision` (walk did not attribute the entry) emits
        /// the `TreeNode` with `last_revision_index = None` and touches
        /// neither the map nor the resolver.
        #[tokio::test]
        async fn zero_last_revision_skips_the_map() {
            let paths = vec![tree_path(
                "root.txt",
                RepositoryId::default(),
                Hash::default(),
            )];
            let (announced, nodes, _order) = drain(paths, HashMap::new()).await;

            assert!(announced.is_empty());
            assert_eq!(nodes.len(), 1);
            assert!(nodes[0].last_revision_index.is_none());
        }

        /// Same signature under two different repositories (a link
        /// crossing) - keys are distinct, indices must be distinct too.
        /// Otherwise the walked repo's revision would silently attribute
        /// the linked side's entry.
        #[tokio::test]
        async fn cross_repo_same_signature_gets_distinct_indices() {
            let walked = random::<RepositoryId>();
            let linked = random::<RepositoryId>();
            let shared_sig = Hash::from_u64(0xabc);
            let paths = vec![
                tree_path("top.txt", walked, shared_sig),
                tree_path("linked/inner.txt", linked, shared_sig),
            ];
            let revisions = HashMap::from([
                (
                    (walked, shared_sig),
                    Some(attribution(shared_sig, "walked")),
                ),
                (
                    (linked, shared_sig),
                    Some(attribution(shared_sig, "linked")),
                ),
            ]);
            let (announced, nodes, order) = drain(paths, revisions).await;

            assert_eq!(order, vec![1, 2], "two distinct keys, two distinct indices");
            assert_eq!(announced.len(), 2);
            assert_eq!(
                announced[0].revision.as_ref().unwrap().commit_message,
                "walked"
            );
            assert_eq!(
                announced[1].revision.as_ref().unwrap().commit_message,
                "linked"
            );
            assert_eq!(nodes[0].last_revision_index, Some(1));
            assert_eq!(nodes[1].last_revision_index, Some(2));
        }
    }

    /// The signature-to-`Revision` resolution the handler applies to every
    /// attributed entry. Uses a pushed revision because
    /// `load_revision_attribution` reads real state + metadata.
    #[tokio::test]
    async fn load_revision_attribution_reads_the_revision_record() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                repository,
            ));
            let (branch, signatures) =
                push_branch_with_revisions(&repository_context, &[&["a.txt"], &["b.txt"]]).await;
            let second = signatures[1];

            let revision = super::load_revision_attribution(&repository_context, second)
                .await
                .expect("a pushed revision must resolve");
            assert_eq!(
                revision.signature,
                Into::<bytes::Bytes>::into(second),
                "the record must describe the revision it was asked for",
            );

            // The identifier is what makes attribution usable as provenance:
            // the number alone is per-branch and cannot say which branch it
            // counts on.
            let identifier = revision
                .identifier
                .as_ref()
                .expect("a resolved commit must carry its identifier");
            assert_eq!(identifier.number, 2, "second revision on the branch");
            assert_eq!(
                identifier.branch_id,
                Into::<bytes::Bytes>::into(branch),
                "the identifier must name the branch the revision was committed to",
            );
            assert_eq!(
                revision.number, 2,
                "the top-level number field mirrors identifier.number",
            );

            // An unknown revision resolves to None, not an error - a walk
            // must not fail because a metadata pointer is stale.
            assert!(
                super::load_revision_attribution(&repository_context, Hash::from_u64(0xdead))
                    .await
                    .is_none(),
                "an unknown revision must be skipped, not fatal",
            );
        }))
        .await;
    }
}
