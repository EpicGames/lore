// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::RevisionDiffRequest;
use lore_proto::lore::thin_client::v1::RevisionDiffResponse;
use lore_proto::lore::thin_client::v1::revision_diff_request::QueryFrom;
use lore_proto::lore::thin_client::v1::revision_diff_request::QueryTo;
use lore_proto::lore::thin_client::v1::revision_diff_response::Payload;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::link::LinkFlags;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::handlers::branch_push::PushHandler;
use lore_server::grpc::handlers::branch_push::PushReport;
use lore_server::grpc::server::RevisionListAcceleration;
use lore_server::grpc::thinclient::v1::helpers::PartitionTable;
use lore_server::grpc::thinclient::v1::revision_diff::*;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
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
    push_merge_revision(
        repository,
        branch_id,
        parent,
        Hash::default(),
        revision_number,
        files,
    )
    .await
}

/// `push_revision` with a second parent, as a merge revision has.
async fn push_merge_revision(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    parent: Hash,
    parent_other: Hash,
    revision_number: u64,
    files: &[(&str, &[u8])],
) -> Hash {
    push_linked_revision(
        repository,
        branch_id,
        [parent, parent_other],
        revision_number,
        files,
        None,
    )
    .await
}

/// `push_merge_revision` with a link at path `link.0`, at most one directory
/// deep, mounting node `link.3` of repository `link.1` at revision `link.2`.
async fn push_linked_revision(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    [parent, parent_other]: [Hash; 2],
    revision_number: u64,
    files: &[(&str, &[u8])],
    link: Option<(&str, RepositoryId, Hash, NodeID)>,
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
    state.set_parent_other(parent_other);
    state.set_revision_number(revision_number);
    state.set_metadata_hash(metadata_hash);
    for (name, bytes) in files {
        let address = lore_revision::immutable::write(
            repository.clone(),
            lore_storage::Context::default(),
            bytes::Bytes::copy_from_slice(bytes),
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
    if let Some((path, link_repository, link_revision, mount_source)) = link {
        let (parent, name) = match path.split_once('/') {
            Some((directory, name)) => {
                let node = Node {
                    name_hash: hash_string(directory),
                    ..Default::default()
                };
                let directory_id = state
                    .node_add(repository.clone(), ROOT_NODE, node, directory)
                    .await
                    .expect("add directory node");
                (directory_id, name)
            }
            None => (ROOT_NODE, path),
        };
        let node = Node {
            flags: NodeFlags::Link.bits(),
            child: mount_source,
            address: Address {
                hash: link_revision,
                context: link_repository.into(),
            },
            name_hash: hash_string(name),
            ..Default::default()
        };
        let node_id = state
            .node_add(repository.clone(), parent, node, name)
            .await
            .expect("add link node");
        state
            .link_add(
                repository.clone(),
                link_repository,
                BranchId::default(),
                link_revision,
                node_id,
                LinkFlags::NoFlags,
            )
            .await
            .expect("register link");
    }
    let serialized = state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");
    branch_push::push(
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
            allow_all(),
            RevisionDiffConfig::default(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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
                .any(|c| c.path == "a.txt" && c.action == thin_client_v1::Action::Delete as i32),
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
            allow_all(),
            RevisionDiffConfig::default(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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
            allow_all(),
            RevisionDiffConfig::default(),
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

/// Builds main with a change after branch B's branch point, then merges B
/// back into main. Returns B's tip and the merge revision, so B's tip is an
/// ancestor of the merge only through the merge's second parent (UCS-25014).
async fn push_branch_merged_into_main(repository: &Arc<RepositoryContext>) -> (Hash, Hash) {
    let main = create_branch(repository, "main", vec![]).await;
    let main_rev1 = push_revision(
        repository,
        main,
        Hash::default(),
        1,
        &[("shared.txt", b"base".as_slice())],
    )
    .await;
    let branch_b = create_branch(
        repository,
        "branch_b",
        vec![BranchPoint {
            branch: main,
            revision: main_rev1,
        }],
    )
    .await;
    let b_rev = push_revision(
        repository,
        branch_b,
        main_rev1,
        1,
        &[
            ("shared.txt", b"base".as_slice()),
            ("only_b.txt", b"b-content".as_slice()),
        ],
    )
    .await;
    let main_rev2 = push_revision(
        repository,
        main,
        main_rev1,
        2,
        &[
            ("shared.txt", b"base".as_slice()),
            ("only_main.txt", b"main-content".as_slice()),
        ],
    )
    .await;
    let merge_rev = push_merge_revision(
        repository,
        main,
        main_rev2,
        b_rev,
        3,
        &[
            ("shared.txt", b"base".as_slice()),
            ("only_b.txt", b"b-content".as_slice()),
            ("only_main.txt", b"main-content".as_slice()),
        ],
    )
    .await;
    (b_rev, merge_rev)
}

/// Runs `RevisionDiff(from, to)` over the merge history and returns the
/// header and each change as `(path, action)`.
async fn diff_over_merge_history(
    from: fn(Hash, Hash) -> Hash,
    to: fn(Hash, Hash) -> Hash,
) -> (thin_client_v1::RevisionDiffHeader, Vec<(String, i32)>) {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (b_rev, merge_rev) = push_branch_merged_into_main(&repository_context).await;

        let response = handler(
            make_request(
                repository,
                QueryFrom::SignatureFrom(from(b_rev, merge_rev).into()),
                QueryTo::SignatureTo(to(b_rev, merge_rev).into()),
                false,
            ),
            immutable_store,
            mutable_store,
            allow_all(),
            RevisionDiffConfig::default(),
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("handler ok");

        let mut items = collect(response)
            .await
            .into_iter()
            .map(|r| r.expect("stream item"));
        let header = match items.next().and_then(|item| item.payload) {
            Some(Payload::Header(h)) => h,
            other => panic!("expected header first, got {other:?}"),
        };
        let changes = items
            .filter_map(|item| match item.payload {
                Some(Payload::Change(c)) => Some((c.path, c.action)),
                Some(Payload::Conflict(c)) => panic!("unexpected conflict {c:?}"),
                _ => None,
            })
            .collect();
        (header, changes)
    }))
    .await
}

#[tokio::test]
async fn from_merged_into_to_diffs_two_way() {
    let (header, changes) =
        diff_over_merge_history(|b_rev, _merge| b_rev, |_b_rev, merge| merge).await;

    assert!(header.signature_base.is_none(), "expected 2-way");
    assert_eq!(
        changes,
        vec![(
            "only_main.txt".to_string(),
            thin_client_v1::Action::Add as i32
        )],
    );
}

#[tokio::test]
async fn to_merged_into_from_diffs_two_way() {
    let (header, changes) =
        diff_over_merge_history(|_b_rev, merge| merge, |b_rev, _merge| b_rev).await;

    assert!(header.signature_base.is_none(), "expected 2-way");
    assert_eq!(
        changes,
        vec![(
            "only_main.txt".to_string(),
            thin_client_v1::Action::Delete as i32
        )],
    );
}

fn make_partition_id() -> RepositoryId {
    RepositoryId::from(uuid::Uuid::now_v7())
}

/// Drain whatever is already queued on the receiver, returning the
/// payloads in arrival order. Used to inspect what `PartitionTable`
/// announced on the wire.
async fn drain_now(rx: &mut mpsc::Receiver<Result<RevisionDiffResponse, Status>>) -> Vec<Payload> {
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
}

/// Admits the caller to the request's repository and to no repository a link
/// points into.
struct HostOnlyAuthorizer;

#[async_trait]
impl RepositoryAuthorizer for HostOnlyAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        _action: Action,
    ) -> Result<(), Status> {
        Ok(())
    }
}

/// Serializes a revision of a linked repository holding `sub/a.txt`, and
/// returns it with the node of `sub`. Revisions with the same `contents` mount
/// the same tree under different signatures.
async fn write_linked_revision(
    linked: &Arc<RepositoryContext>,
    revision_number: u64,
    contents: &[u8],
) -> (Hash, NodeID) {
    let state = state::State::new();
    state.set_revision_number(revision_number);
    let address = lore_revision::immutable::write(
        linked.clone(),
        lore_storage::Context::default(),
        bytes::Bytes::copy_from_slice(contents),
        lore_storage::WriteOptions::default(),
    )
    .await
    .expect("immutable::write");
    let directory = Node {
        name_hash: hash_string("sub"),
        ..Default::default()
    };
    let sub = state
        .node_add(linked.clone(), ROOT_NODE, directory, "sub")
        .await
        .expect("add directory node");
    let node = Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string("a.txt"),
        address,
        ..Default::default()
    };
    state
        .node_add(linked.clone(), sub, node, "a.txt")
        .await
        .expect("node_add");
    let revision = state
        .serialize(linked.clone(), &get_write_token())
        .await
        .expect("serialize linked state");
    (revision, sub)
}

/// Pushes main with a link into `linked` at `pins[0]`, as its name, pin and
/// mount source,
/// then sibling branches A and B off it with the link at `pins[1]` and
/// `pins[2]`, `None` meaning no link. A and B also each edit a file of their
/// own. Returns main's, A's and B's revision.
async fn push_link_pins(
    repository: &Arc<RepositoryContext>,
    linked: RepositoryId,
    pins: [Option<(&'static str, Hash, NodeID)>; 3],
) -> [Hash; 3] {
    let link = |pin: Option<(&'static str, Hash, NodeID)>| {
        pin.map(|(name, pin, mount_source)| (name, linked, pin, mount_source))
    };
    let main = create_branch(repository, "main", vec![]).await;
    let base = push_linked_revision(
        repository,
        main,
        [Hash::default(); 2],
        1,
        &[],
        link(pins[0]),
    )
    .await;
    let stack = vec![BranchPoint {
        branch: main,
        revision: base,
    }];
    let branch_a = create_branch(repository, "branch_a", stack.clone()).await;
    let a = push_linked_revision(
        repository,
        branch_a,
        [base, Hash::default()],
        1,
        &[("a.txt", b"a".as_slice())],
        link(pins[1]),
    )
    .await;
    let branch_b = create_branch(repository, "branch_b", stack).await;
    let b = push_linked_revision(
        repository,
        branch_b,
        [base, Hash::default()],
        1,
        &[("b.txt", b"b".as_slice())],
        link(pins[2]),
    )
    .await;
    [base, a, b]
}

/// Runs `RevisionDiff(from, to)` for a caller `authorizer` admits and returns
/// the repositories the stream announces as partitions.
async fn announced_partitions(
    repository: RepositoryId,
    stores: (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>),
    authorizer: Arc<dyn RepositoryAuthorizer>,
    from: Hash,
    to: Hash,
) -> Vec<RepositoryId> {
    let response = handler(
        make_request(
            repository,
            QueryFrom::SignatureFrom(from.into()),
            QueryTo::SignatureTo(to.into()),
            false,
        ),
        stores.0,
        stores.1,
        authorizer,
        RevisionDiffConfig::default(),
        DEFAULT_HISTORY_STEP_SIZE,
        RevisionListAcceleration::default(),
    )
    .await
    .expect("handler ok");
    collect(response)
        .await
        .into_iter()
        .filter_map(|item| match item.expect("stream item").payload {
            Some(Payload::Partition(partition)) => {
                Some(RepositoryId::from(partition.link_partition.as_ref()))
            }
            _ => None,
        })
        .collect()
}

/// Every diff that touches a link announces the link's repository, whether or
/// not the caller may read it, so a stream without a partition is the same
/// for every caller.
#[tokio::test]
async fn touched_link_is_announced_to_every_caller() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let linked_id = random::<RepositoryId>();
        let linked = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            linked_id,
        ));
        let (pin, sub) = write_linked_revision(&linked, 1, b"v1").await;
        let (same_content, _) = write_linked_revision(&linked, 2, b"v1").await;
        let (new_content, _) = write_linked_revision(&linked, 3, b"v2").await;

        // `pins` per `push_link_pins`, and the diffs to run over its revisions.
        let at = |pin| Some(("dir/linked", pin, ROOT_NODE));
        let cases = [
            (
                "source re-pins to the same content",
                [at(pin), at(same_content), at(pin)],
                &[(1, 2), (0, 1)][..],
            ),
            (
                "source re-pins to new content",
                [at(pin), at(new_content), at(pin)],
                &[(1, 2), (0, 1)],
            ),
            (
                "source renames the link's directory in case and re-pins to the same content",
                [
                    at(pin),
                    Some(("Dir/linked", same_content, ROOT_NODE)),
                    at(pin),
                ],
                &[(1, 2), (0, 1)],
            ),
            (
                "target re-pins to the same content",
                [at(pin), at(pin), at(same_content)],
                &[(1, 2), (0, 2)],
            ),
            (
                "target remounts the link at the same pin",
                [at(pin), at(pin), Some(("dir/linked", pin, sub))],
                &[(1, 2), (0, 2)],
            ),
            (
                "source adds the link",
                [None, at(pin), None],
                &[(1, 2), (0, 1)],
            ),
            (
                "source removes the link",
                [at(pin), None, at(pin)],
                &[(1, 2), (0, 1)],
            ),
        ];
        for (case, pins, diffs) in cases {
            let repository = random::<RepositoryId>();
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let revisions = push_link_pins(&repository_context, linked_id, pins).await;
            for &(from, to) in diffs {
                for (caller, authorizer) in [
                    ("reader", allow_all()),
                    (
                        "host-only",
                        Arc::new(HostOnlyAuthorizer) as Arc<dyn RepositoryAuthorizer>,
                    ),
                ] {
                    assert_eq!(
                        announced_partitions(
                            repository,
                            (immutable_store.clone(), mutable_store.clone()),
                            authorizer,
                            revisions[from],
                            revisions[to],
                        )
                        .await,
                        vec![linked_id],
                        "{case}: diff {from} -> {to} for the {caller} caller",
                    );
                }
            }
        }
    }))
    .await;
}
