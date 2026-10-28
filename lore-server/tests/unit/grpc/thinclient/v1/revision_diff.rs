// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
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
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::server::RevisionListAcceleration;
use lore_server::grpc::thinclient::v1::revision_diff::*;
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
    assert!(
        !table.entries.contains_key(&linked),
        "failed announcement must not poison the table",
    );
}
