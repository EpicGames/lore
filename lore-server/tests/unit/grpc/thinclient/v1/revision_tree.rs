// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::RevisionTreeRequest;
use lore_proto::lore::thin_client::v1::RevisionTreeResponse;
use lore_proto::lore::thin_client::v1::revision_tree_request::Query;
use lore_proto::lore::thin_client::v1::revision_tree_response::Payload;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::link::LinkFlags;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::handlers::branch_push::PushHandler;
use lore_server::grpc::handlers::branch_push::PushReport;
use lore_server::grpc::server::RevisionListAcceleration;
use lore_server::grpc::thinclient::v1::revision_tree::*;
use lore_storage::hash::hash_string;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::store::test_support::test_store_create;

fn allow_all() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(AllowAllRepositoryAuthorizer)
}

/// A Tier 2 deployment reading the legacy `resources` claim shape, so
/// the link-read verdict comes from the token in the request.
fn resource_grants() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(ResourceGrantsAuthorizer::new(
        "resources".to_string(),
        "resource_id".to_string(),
        None,
        "urc-{id}".to_string(),
        "urc-*".to_string(),
    ))
}

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

/// Creates a branch in `repository`, answering its ID.
async fn create_branch(repository: &Arc<RepositoryContext>) -> BranchId {
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository.clone(),
        &get_write_token(),
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
    branch_id
}

/// Pushes a fresh branch and one revision for each of `revisions`; each
/// revision's state contains `revisions[i]` as File nodes at the root.
/// Returns the branch id and the revision signatures in push order.
async fn push_branch_with_revisions(
    repository: &Arc<RepositoryContext>,
    revisions: &[&[&str]],
) -> (BranchId, Vec<Hash>) {
    let write_token = get_write_token();
    let branch_id = create_branch(repository).await;

    let mut signatures = Vec::with_capacity(revisions.len());
    let mut parent = Hash::default();
    for (idx, files) in revisions.iter().enumerate() {
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = state::State::new();
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
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
    let branch_id = create_branch(repository).await;

    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = state::State::new();
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
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
        &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
    let branch_id = create_branch(repository).await;

    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = state::State::new();
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
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
        &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
        let (_branch, signature) = push_branch_with_files(&repository_context, &["file.txt"]).await;

        let response = handler(
            make_request(
                repository,
                Query::Signature(signature.into()),
                Some("file.txt".into()),
                None,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
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
            allow_all(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let payloads: Vec<Payload> = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item").payload.expect("a payload"))
            .collect();
        let partitions: Vec<(usize, &thin_client_v1::DiffPartition)> = payloads
            .iter()
            .enumerate()
            .filter_map(|(at, payload)| match payload {
                Payload::Partition(partition) => Some((at, partition)),
                _ => None,
            })
            .collect();
        let nodes: Vec<thin_client_v1::TreeNode> = payloads
            .iter()
            .filter_map(|payload| match payload {
                Payload::Node(n) => Some(n.clone()),
                _ => None,
            })
            .collect();

        // Expect exactly two entries: the link mount, then the file inside.
        assert_eq!(nodes.len(), 2, "expected link + child, got {nodes:?}");
        assert_eq!(partitions.len(), 1, "{payloads:?}");
        let (announced_at, partition) = partitions[0];
        assert_eq!(partition.index, 1);
        assert_eq!(
            RepositoryId::from(partition.link_partition.as_ref()),
            linked
        );
        assert!(
            matches!(&payloads[announced_at + 1], Payload::Node(link) if link.path == "linked"),
            "the partition is announced just before the link naming it: {payloads:?}",
        );
        assert_eq!(
            nodes[0].link_repository_index, 1,
            "the link names the linked repository"
        );
        assert_eq!(
            nodes[1].link_repository_index, 1,
            "the entry under it is in the linked one"
        );

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
        allow_all(),
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
fn token_authorized_for(repos: &[RepositoryId]) -> lore_server::auth::jwt::AuthorizationToken {
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::auth::jwt::ResourcePermission;

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

        let mut request = make_request(originating, Query::Signature(signature.into()), None, None);
        request
            .extensions_mut()
            .insert(token_authorized_for(&[originating]));
        request
            .extensions_mut()
            .insert(RawToken("raw.jwt".to_string()));

        let response = handler(
            request,
            immutable_store,
            mutable_store,
            resource_grants(),
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
            allow_all(),
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
        let branch_id = create_branch(&a_context).await;

        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(a_context.clone())
            .await
            .expect("serialize metadata");

        let state = state::State::new();
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
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &mut PushReport::new(PushHandler::Legacy, Default::default()),
        )
        .await
        .expect("push")
        .revision;

        let response = handler(
            make_request(a, Query::Signature(a_sig.into()), None, None),
            immutable_store,
            mutable_store,
            allow_all(),
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
        request
            .extensions_mut()
            .insert(RawToken("raw.jwt".to_string()));

        let response = handler(
            request,
            immutable_store,
            mutable_store,
            resource_grants(),
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

/// Commits `names` as files at the root of the first revision of a new branch, through the commit
/// path that records each entry's history, and answers the revision.
async fn commit_files(repository: &Arc<RepositoryContext>, names: &[&str]) -> Hash {
    use lore_base::types::Address;
    use lore_base::types::Context;

    let branch_id = create_branch(repository).await;
    let state = state::State::new();
    for name in names {
        let node = Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 1,
            address: Address {
                hash: Hash::from_u64(1),
                context: Context::from(uuid::Uuid::now_v7()),
            },
            name_hash: hash_string(name),
            ..Default::default()
        };
        state
            .node_add_staged(repository.clone(), ROOT_NODE, node, name)
            .await
            .expect("node_add_staged");
    }
    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    lore_revision::commit::commit_in_memory_revision(
        repository.clone(),
        &get_write_token(),
        state,
        metadata,
        Hash::default(),
        branch_id,
    )
    .await
    .expect("commit")
}

/// Attribution follows the request flag, which the header echoes. With it, each entry names the
/// revision that added it, announced before the first entry naming it. Without it, no entry names
/// one and no revision is sent.
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
        let revision = commit_files(&repository_context, &["a.txt", "b.txt"]).await;

        let items = async |include_last_revision| {
            let mut request =
                make_request(repository, Query::Signature(revision.into()), None, None);
            request.get_mut().include_last_revision = include_last_revision;
            collect(
                handler(
                    request,
                    immutable_store.clone(),
                    mutable_store.clone(),
                    allow_all(),
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

        let mut nodes = 0;
        for item in items(false).await {
            match item.payload {
                Some(Payload::Header(header)) => assert!(!header.last_revision_included),
                Some(Payload::Node(node)) => {
                    nodes += 1;
                    assert_eq!(node.last_revision_index, 0, "{}", node.path);
                }
                Some(Payload::Revision(_)) => panic!("a TreeRevision without the flag"),
                Some(Payload::Partition(_)) => panic!("a DiffPartition without a link"),
                None => panic!("a response without a payload"),
            }
        }
        assert_eq!(nodes, 2);

        let mut announced = Vec::new();
        let mut nodes = 0;
        for item in items(true).await {
            match item.payload {
                Some(Payload::Header(header)) => assert!(header.last_revision_included),
                Some(Payload::Revision(tree_revision)) => {
                    let named = tree_revision.revision.expect("the announced revision");
                    assert_eq!(Hash::from(named.signature.as_ref()), revision);
                    announced.push(tree_revision.index);
                }
                Some(Payload::Node(node)) => {
                    nodes += 1;
                    assert!(
                        announced.contains(&node.last_revision_index),
                        "{}",
                        node.path
                    );
                }
                Some(Payload::Partition(_)) => panic!("a DiffPartition without a link"),
                None => panic!("a response without a payload"),
            }
        }
        assert_eq!(announced, [1]);
        assert_eq!(nodes, 2);
    }))
    .await;
}

/// `emit_tree_stream` over a hand-made walk, with a loader that answers from the test.
mod emission {
    use std::pin::pin;

    use lore_base::error::NodeNotFound;
    use lore_base::error::SlowDown;
    use lore_revision::state::AttributedRevision;
    use lore_revision::state::RepositorySpan;
    use lore_revision::state::StateError;
    use lore_revision::state::TreePath;

    use super::*;

    /// What a load answers.
    type Loaded = Result<(thin_client_v1::Revision, bool), StateError>;

    /// A file entry at `path` naming the revision at `last_revision` from 1, or none for 0.
    fn entry(path: &str, last_revision: u32) -> TreePath {
        TreePath {
            path: lore_revision::util::path::RelativePath::new_from_initial_path(path)
                .expect("valid path"),
            address: None,
            flags: NodeFlags::File,
            size: 0,
            mode: 0,
            tracking: false,
            last_revision,
        }
    }

    /// Runs `test` in the execution context of the test stores, with a repository over them.
    async fn with_repository(test: impl AsyncFnOnce(Arc<RepositoryContext>)) {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            random::<RepositoryId>(),
        ));
        Box::pin(LORE_CONTEXT.scope(execution, test(repository))).await;
    }

    /// Revisions `signatures` of `repository`.
    fn revisions(
        repository: &Arc<RepositoryContext>,
        signatures: &[u64],
    ) -> Vec<AttributedRevision> {
        signatures
            .iter()
            .map(|&signature| AttributedRevision {
                repository: repository.clone(),
                revision: Hash::from_u64(signature),
            })
            .collect()
    }

    /// The record a load answers for `revision`, a branch named.
    fn record(revision: &AttributedRevision) -> Loaded {
        Ok((
            thin_client_v1::Revision {
                signature: revision.revision.into(),
                ..Default::default()
            },
            false,
        ))
    }

    /// What `emit_tree_stream` sends for `paths` of a walk of `repository` naming `revisions`,
    /// each loaded as `outcome` answers, with `report` as the walk left it; and the revisions
    /// loaded, in order.
    async fn emit(
        repository: &Arc<RepositoryContext>,
        paths: &[TreePath],
        revisions: &[AttributedRevision],
        outcome: impl Fn(&AttributedRevision) -> Loaded,
        report: &mut AttributionReport,
    ) -> (Vec<Result<Payload, Status>>, Vec<Hash>) {
        let (tx, mut rx) = mpsc::channel(1024);
        let mut loaded = Vec::new();
        emit_tree_stream(
            paths,
            &[],
            revisions,
            repository.id,
            |revision| {
                loaded.push(revision.revision);
                std::future::ready(outcome(revision))
            },
            tx,
            report,
        )
        .await;
        let mut sent = Vec::new();
        while let Some(item) = rx.recv().await {
            sent.push(item.map(|response| response.payload.expect("a payload")));
        }
        (sent, loaded)
    }

    /// The signature of each revision `sent` announces, in order, and the index each entry
    /// carries, after checking that announcements number from 1 and that each index an entry
    /// carries was announced before it.
    fn read(sent: Vec<Result<Payload, Status>>) -> (Vec<Hash>, Vec<u32>) {
        let (mut announced, mut indices) = (Vec::new(), Vec::new());
        for payload in sent {
            match payload.expect("a stream item") {
                Payload::Revision(tree_revision) => {
                    assert_eq!(tree_revision.index as usize, announced.len() + 1);
                    let revision = tree_revision.revision.expect("the announced revision");
                    announced.push(Hash::from(revision.signature.as_ref()));
                }
                Payload::Node(node) => {
                    assert!(node.last_revision_index as usize <= announced.len());
                    indices.push(node.last_revision_index);
                }
                Payload::Header(_) => panic!("a header from emission"),
                Payload::Partition(_) => panic!("a partition without a span"),
            }
        }
        (announced, indices)
    }

    /// Each revision is loaded once, in the order entries first name it, and announced before the
    /// first entry naming it, numbered in that order whatever its place in the walk's table. An
    /// unattributed entry loads nothing.
    #[tokio::test]
    async fn each_revision_is_loaded_once_and_announced_before_its_first_entry() {
        with_repository(async |repository| {
            let revisions = revisions(&repository, &[0x11, 0x22]);
            let paths = [
                entry("a.txt", 2),
                entry("b.txt", 1),
                entry("c.txt", 2),
                entry("d.txt", 0),
            ];
            let mut report = AttributionReport::default();
            let (sent, loaded) = emit(&repository, &paths, &revisions, record, &mut report).await;

            let (first, second) = (Hash::from_u64(0x22), Hash::from_u64(0x11));
            assert_eq!(loaded, [first, second]);
            assert_eq!(read(sent), (vec![first, second], vec![1, 2, 1, 0]));
            assert_eq!(report.failures.count, 0);
        })
        .await;
    }

    /// A revision that fails to load is loaded once, announced never, and leaves the entries
    /// naming it unattributed. The next revision takes the next index, and the failure is
    /// counted.
    #[tokio::test]
    async fn a_revision_that_fails_to_load_leaves_its_entries_unattributed() {
        with_repository(async |repository| {
            let revisions = revisions(&repository, &[0xbad, 0x22]);
            let paths = [entry("a.txt", 1), entry("b.txt", 2), entry("c.txt", 1)];
            let mut report = AttributionReport::default();
            let outcome = |revision: &AttributedRevision| {
                if revision.revision == Hash::from_u64(0xbad) {
                    Err(StateError::from(NodeNotFound))
                } else {
                    record(revision)
                }
            };
            let (sent, loaded) = emit(&repository, &paths, &revisions, outcome, &mut report).await;

            assert_eq!(loaded, [Hash::from_u64(0xbad), Hash::from_u64(0x22)]);
            assert_eq!(read(sent), (vec![Hash::from_u64(0x22)], vec![0, 1, 0]));
            assert_eq!(report.failures.count, 1);
            assert!(report.failures.error.is_some());
        })
        .await;
    }

    /// A load refused as `SlowDown` ends the stream with `RESOURCE_EXHAUSTED`, after what was
    /// sent before it, and is not counted as a failure.
    #[tokio::test]
    async fn a_load_refused_as_slow_down_ends_the_stream() {
        with_repository(async |repository| {
            let revisions = revisions(&repository, &[0x11, 0x22]);
            let paths = [entry("a.txt", 1), entry("b.txt", 2), entry("c.txt", 1)];
            let mut report = AttributionReport::default();
            let outcome = |revision: &AttributedRevision| {
                if revision.revision == Hash::from_u64(0x22) {
                    Err(StateError::from(SlowDown))
                } else {
                    record(revision)
                }
            };
            let (mut sent, _) = emit(&repository, &paths, &revisions, outcome, &mut report).await;

            let refusal = sent.pop().expect("the refusal").expect_err("an error");
            assert_eq!(refusal.code(), tonic::Code::ResourceExhausted);
            assert_eq!(read(sent), (vec![Hash::from_u64(0x11)], vec![1]));
            assert_eq!(report.failures.count, 0);
        })
        .await;
    }

    /// A revision whose metadata names no branch is announced, and counted.
    #[tokio::test]
    async fn a_revision_naming_no_branch_is_announced_and_counted() {
        with_repository(async |repository| {
            let revisions = revisions(&repository, &[0x11]);
            let paths = [entry("a.txt", 1)];
            let mut report = AttributionReport::default();
            let outcome = |revision: &AttributedRevision| {
                record(revision).map(|(revision, _)| (revision, true))
            };
            let (sent, _) = emit(&repository, &paths, &revisions, outcome, &mut report).await;

            assert_eq!(read(sent), (vec![Hash::from_u64(0x11)], vec![1]));
            assert_eq!(report.missing_branches, 1);
            assert_eq!(report.failures.count, 0);
        })
        .await;
    }

    /// A stream loads at most `REVISION_PREFETCH` revisions ahead of what it has sent.
    #[tokio::test]
    async fn a_stream_loads_at_most_the_prefetch_ahead() {
        with_repository(async |repository| {
            let count = REVISION_PREFETCH as u64 * 3;
            let signatures: Vec<u64> = (1..=count).collect();
            let revisions = revisions(&repository, &signatures);
            let paths: Vec<TreePath> = (1..=count as u32)
                .map(|slot| entry(&format!("{slot}.txt"), slot))
                .collect();
            let (tx, mut rx) = mpsc::channel(1);
            let mut report = AttributionReport::default();
            let loads = std::sync::atomic::AtomicUsize::new(0);
            let mut emission = pin!(emit_tree_stream(
                &paths,
                &[],
                &revisions,
                repository.id,
                |revision| {
                    loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::future::ready(record(revision))
                },
                tx,
                &mut report,
            ));

            assert!(futures::poll!(emission.as_mut()).is_pending());
            assert_eq!(
                loads.load(std::sync::atomic::Ordering::Relaxed),
                REVISION_PREFETCH
            );

            let mut sent = Vec::new();
            let ((), ()) = tokio::join!(emission, async {
                while let Some(item) = rx.recv().await {
                    sent.push(item.map(|response| response.payload.expect("a payload")));
                }
            });
            assert_eq!(
                loads.load(std::sync::atomic::Ordering::Relaxed),
                count as usize
            );
            let (announced, indices) = read(sent);
            assert_eq!(announced.len(), count as usize);
            assert_eq!(indices, (1..=count as u32).collect::<Vec<_>>());
        })
        .await;
    }

    /// Each repository other than the requested one is announced once, before the first entry in
    /// it, numbered from 1 in that order, and each entry carries the index of the repository its
    /// span places it in, 0 for the requested one, which is never announced.
    #[tokio::test]
    async fn each_linked_repository_is_announced_once_before_its_first_entry() {
        with_repository(async |repository| {
            let (outer, inner) = (random::<RepositoryId>(), random::<RepositoryId>());
            let paths = [
                entry("a.txt", 0),
                entry("l/b.txt", 0),
                entry("l/m/c.txt", 0),
                entry("l/d.txt", 0),
                entry("e.txt", 0),
                entry("k/f.txt", 0),
            ];
            let span = |first, repository| RepositorySpan { first, repository };
            let spans = [
                span(1, outer),
                span(2, inner),
                span(3, outer),
                span(4, repository.id),
                span(5, inner),
                span(6, repository.id),
            ];
            let (tx, mut rx) = mpsc::channel(1024);
            let mut report = AttributionReport::default();
            emit_tree_stream(
                &paths,
                &spans,
                &[],
                repository.id,
                |revision: &AttributedRevision| std::future::ready(record(revision)),
                tx,
                &mut report,
            )
            .await;

            let mut sent = Vec::new();
            while let Some(item) = rx.recv().await {
                sent.push(match item.expect("a stream item").payload {
                    Some(Payload::Partition(partition)) => (
                        format!("partition {}", partition.index),
                        RepositoryId::from(partition.link_partition.as_ref()),
                    ),
                    Some(Payload::Node(node)) => (
                        format!("{} in {}", node.path, node.link_repository_index),
                        RepositoryId::default(),
                    ),
                    payload => panic!("{payload:?} from a walk with no revisions"),
                });
            }
            let none = RepositoryId::default();
            let expected = [
                ("a.txt in 0", none),
                ("partition 1", outer),
                ("l/b.txt in 1", none),
                ("partition 2", inner),
                ("l/m/c.txt in 2", none),
                ("l/d.txt in 1", none),
                ("e.txt in 0", none),
                ("k/f.txt in 2", none),
            ]
            .map(|(item, repository)| (item.to_string(), repository));
            assert_eq!(sent, expected);
        })
        .await;
    }

    /// A link carries the index of the repository it links, announced before it. A revision is
    /// announced with the index of the repository the entry naming it is placed in: for a link,
    /// the repository holding it.
    #[tokio::test]
    async fn a_link_names_its_target_and_a_revision_its_repository() {
        with_repository(async |repository| {
            let linked = repository.to_link_context(random::<RepositoryId>()).await;
            let revisions = [revisions(&repository, &[0x11]), revisions(&linked, &[0x22])].concat();
            let link = TreePath {
                flags: NodeFlags::Link,
                address: Some(lore_base::types::Address {
                    hash: Hash::from_u64(0x22),
                    context: linked.id.into(),
                }),
                ..entry("l", 1)
            };
            let paths = [link, entry("l/a.txt", 2)];
            let spans = [RepositorySpan {
                first: 1,
                repository: linked.id,
            }];
            let (tx, mut rx) = mpsc::channel(1024);
            let mut report = AttributionReport::default();
            emit_tree_stream(
                &paths,
                &spans,
                &revisions,
                repository.id,
                |revision| std::future::ready(record(revision)),
                tx,
                &mut report,
            )
            .await;

            let mut sent = Vec::new();
            while let Some(item) = rx.recv().await {
                sent.push(match item.expect("a stream item").payload {
                    Some(Payload::Partition(partition)) => format!(
                        "partition {} {}",
                        partition.index,
                        RepositoryId::from(partition.link_partition.as_ref()) == linked.id
                    ),
                    Some(Payload::Revision(revision)) => format!(
                        "revision {} in {}",
                        revision.index, revision.link_repository_index
                    ),
                    Some(Payload::Node(node)) => format!(
                        "{} in {} named {}",
                        node.path, node.link_repository_index, node.last_revision_index
                    ),
                    payload => panic!("{payload:?} from emission"),
                });
            }
            assert_eq!(
                sent,
                [
                    "partition 1 true",
                    "revision 1 in 0",
                    "l in 1 named 1",
                    "revision 2 in 1",
                    "l/a.txt in 1 named 2",
                ]
            );
        })
        .await;
    }
}

/// `load_attributed_revision` reads the record of a pushed revision, with the identifier that
/// names its branch, and fails for an unknown revision.
#[tokio::test]
async fn load_attributed_revision_reads_the_revision_record() {
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
        let attributed = |revision| lore_revision::state::AttributedRevision {
            repository: repository_context.clone(),
            revision,
        };

        let (revision, branch_missing) = load_attributed_revision(&attributed(signatures[1]))
            .await
            .expect("a pushed revision loads");
        assert!(!branch_missing);
        assert_eq!(Hash::from(revision.signature.as_ref()), signatures[1]);
        let identifier = revision.identifier.expect("the identifier");
        assert_eq!(identifier.number, 2);
        assert_eq!(identifier.branch_id, Into::<bytes::Bytes>::into(branch));
        assert_eq!(revision.number, 2);

        let unknown = load_attributed_revision(&attributed(Hash::from_u64(0xdead)))
            .await
            .expect_err("an unknown revision fails");
        assert!(!unknown.is_slow_down());
    }))
    .await;
}
