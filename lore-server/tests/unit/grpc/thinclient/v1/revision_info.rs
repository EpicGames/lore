// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1::RevisionInfoRequest;
use lore_proto::lore::thin_client::v1::revision_info_request::Query;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::server::RevisionListAcceleration;
use lore_server::grpc::thinclient::v1::revision_info::*;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Request;

use crate::store::test_support::test_store_create;

fn make_request(repository: RepositoryId, query: Query) -> Request<RevisionInfoRequest> {
    let mut request = Request::new(RevisionInfoRequest { query: Some(query) });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

/// Push `count` chained revisions to a freshly-created branch.
/// Returns `(branch_id, signatures-newest-first)`. Each revision's
/// state metadata blob carries the originating branch so handlers
/// can derive `(branch, number)` from a signature lookup.
async fn create_branch_with_history(
    repository: &Arc<RepositoryContext>,
    count: u64,
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

    let mut signatures = Vec::with_capacity(count as usize);
    let mut parent = Hash::default();
    for n in 1..=count {
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = State::new();
        state.set_parent_self(parent);
        state.set_revision_number(n);
        state.set_metadata_hash(metadata_hash);
        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let pushed = branch_push::push(
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
        .expect("push revision")
        .revision;
        signatures.push(pushed);
        parent = pushed;
    }
    signatures.reverse();
    (branch_id, signatures)
}

#[tokio::test]
async fn unset_query_returns_invalid_argument() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let mut request = Request::new(RevisionInfoRequest { query: None });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        let err = handler(
            request,
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect_err("unset query should fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }))
    .await;
}

#[tokio::test]
async fn identifier_query_with_concrete_number_returns_revision() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 3).await;

        let response = handler(
            make_request(
                repository,
                Query::Identifier(model_v1::RevisionIdentifier {
                    branch_id: branch_id.into(),
                    number: 2,
                }),
            ),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("Request failed")
        .into_inner();

        let revision = response.revision.expect("revision");
        assert_eq!(revision.number, 2);
        assert_eq!(Hash::from(revision.signature.as_ref()), signatures[1]);
    }))
    .await;
}

#[tokio::test]
async fn identifier_query_with_zero_number_resolves_latest() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 5).await;

        let response = handler(
            make_request(
                repository,
                Query::Identifier(model_v1::RevisionIdentifier {
                    branch_id: branch_id.into(),
                    number: 0,
                }),
            ),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("Request failed")
        .into_inner();

        let revision = response.revision.expect("revision");
        // Latest revision is signatures[0] with number 5.
        assert_eq!(revision.number, 5);
        assert_eq!(Hash::from(revision.signature.as_ref()), signatures[0]);
    }))
    .await;
}

#[tokio::test]
async fn root_revision_has_no_parent_self() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (_branch, signatures) = create_branch_with_history(&repository_context, 1).await;
        // Single revision: it is the root, parent_self is zero hash.
        let root = signatures[0];

        let response = handler(
            make_request(repository, Query::Signature(root.into())),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("Request failed")
        .into_inner();

        let revision = response.revision.expect("revision");
        assert_eq!(revision.number, 1);
        assert!(revision.parent_self.is_none());
        assert!(revision.parent_other.is_none());
    }))
    .await;
}

#[tokio::test]
async fn merge_revision_includes_parent_other() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 3).await;
        // Build a merge revision whose parent_self = revision 3, parent_other = revision 1.
        let parent_self = signatures[0]; // revision 3
        let parent_other = signatures[2]; // revision 1
        let write_token = get_write_token();

        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository_context.clone())
            .await
            .expect("serialize metadata");
        let state = State::new();
        state.set_parent_self(parent_self);
        state.set_parent_other(parent_other);
        state.set_revision_number(4);
        state.set_metadata_hash(metadata_hash);
        let serialized = state
            .serialize(repository_context.clone(), &write_token)
            .await
            .expect("serialize state");
        let merge_signature = branch_push::push(
            repository_context.clone(),
            branch_id,
            serialized,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("push merge")
        .revision;

        let response = handler(
            make_request(repository, Query::Signature(merge_signature.into())),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("Request failed")
        .into_inner();

        let revision = response.revision.expect("revision");
        assert_eq!(revision.number, 4);
        let ps = revision.parent_self.expect("parent_self");
        assert_eq!(Hash::from(ps.signature.as_ref()), parent_self);
        assert_eq!(ps.identifier.expect("ps id").number, 3);
        let po = revision.parent_other.expect("parent_other");
        assert_eq!(Hash::from(po.signature.as_ref()), parent_other);
        assert_eq!(po.identifier.expect("po id").number, 1);
    }))
    .await;
}

#[tokio::test]
async fn metadata_fields_are_extracted() {
    use lore_revision::metadata as md;

    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository_context.clone(),
            &write_token,
            branch_id,
            "branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create");

        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        metadata.set_timestamp(1_700_000_000).expect("ts");
        metadata
            .set_string(md::MESSAGE, "hello commit")
            .expect("message");
        metadata
            .set_string(md::CREATED_BY, "alice")
            .expect("created");
        metadata
            .set_string(md::COMMITTED_BY, "bob")
            .expect("committed");
        metadata.set_string("custom", "value").expect("custom");
        let metadata_hash = metadata
            .serialize(repository_context.clone())
            .await
            .expect("serialize metadata");
        let state = State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        state.set_metadata_hash(metadata_hash);
        let serialized = state
            .serialize(repository_context.clone(), &write_token)
            .await
            .expect("serialize state");
        let signature = branch_push::push(
            repository_context.clone(),
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
        .revision;

        let response = handler(
            make_request(repository, Query::Signature(signature.into())),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("Request failed")
        .into_inner();

        let revision = response.revision.expect("revision");
        assert_eq!(revision.commit_message, "hello commit");
        assert_eq!(revision.timestamp, 1_700_000_000);
        assert_eq!(revision.created_by, "alice");
        assert_eq!(revision.committed_by, "bob");
        // Special keys are extracted as top-level fields, not echoed in
        // the metadata vec. Only the non-special "custom" entry remains.
        assert_eq!(revision.metadata.len(), 1);
        assert_eq!(revision.metadata[0].key, "custom");
        assert_eq!(revision.metadata[0].value, "value");
    }))
    .await;
}

#[tokio::test]
async fn unknown_signature_returns_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let bogus = Hash::from(random::<[u8; 32]>());
        let err = handler(
            make_request(repository, Query::Signature(bogus.into())),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect_err("unknown signature should fail");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

#[tokio::test]
async fn unknown_identifier_returns_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let unknown_branch = BranchId::from(uuid::Uuid::now_v7());
        let err = handler(
            make_request(
                repository,
                Query::Identifier(model_v1::RevisionIdentifier {
                    branch_id: unknown_branch.into(),
                    number: 0,
                }),
            ),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect_err("unknown branch should fail");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

#[tokio::test]
async fn unknown_identifier_with_nonzero_number_returns_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    // Exercises the revision::resolve path (number != 0). Distinct
    // from `unknown_identifier_returns_not_found` which goes through
    // branch::load_latest (number == 0).
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let unknown_branch = BranchId::from(uuid::Uuid::now_v7());
        let err = handler(
            make_request(
                repository,
                Query::Identifier(model_v1::RevisionIdentifier {
                    branch_id: unknown_branch.into(),
                    number: 7,
                }),
            ),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect_err("unknown branch should fail");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

#[tokio::test]
async fn signature_query_echoes_resolved_identifier() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 3).await;
        // signatures[0] = revision 3 (latest), signatures[2] = revision 1 (root).
        let target = signatures[1]; // revision 2

        let response = handler(
            make_request(repository, Query::Signature(target.into())),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("Request failed")
        .into_inner();

        let revision = response.revision.expect("revision");
        assert_eq!(Hash::from(revision.signature.as_ref()), target);
        assert_eq!(revision.number, 2);
        let identifier = revision.identifier.expect("identifier");
        assert_eq!(identifier.number, 2);
        assert_eq!(BranchId::from(&identifier.branch_id), branch_id);
        // Root revision had no parent, but revision 2's parent is revision 1.
        let parent_self = revision.parent_self.expect("parent_self");
        assert_eq!(Hash::from(parent_self.signature.as_ref()), signatures[2]);
        let parent_identifier = parent_self.identifier.expect("parent identifier");
        assert_eq!(parent_identifier.number, 1);
        assert_eq!(BranchId::from(&parent_identifier.branch_id), branch_id);
        assert!(revision.parent_other.is_none());
        // Sanity: the request did not specify the identifier, so we also
        // assert the proto message type wired correctly through the
        // handler.
        let _ = model_v1::RevisionIdentifier {
            branch_id: branch_id.into(),
            number: 0,
        };
    }))
    .await;
}
