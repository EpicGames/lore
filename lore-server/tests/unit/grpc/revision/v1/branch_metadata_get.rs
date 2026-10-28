// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1::BranchMetadataGetRequest;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::revision::v1::branch_metadata_get::*;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Request;

use crate::store::test_support::test_store_create;

fn make_request(repository: RepositoryId, branch: BranchId) -> Request<BranchMetadataGetRequest> {
    let mut request = Request::new(BranchMetadataGetRequest { id: branch.into() });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

#[tokio::test]
async fn returns_metadata_hash_for_existing_branch() {
    let repository_id = random::<RepositoryId>();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository_id,
        ));

        let write_token = get_write_token();
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
        .expect("Failed to create branch");

        let response = handler(
            make_request(repository_id, branch_id),
            immutable_store,
            mutable_store,
        )
        .await
        .expect("Handler failed");

        let hash: lore_storage::Hash = response.into_inner().metadata.into();
        assert!(!hash.is_zero(), "metadata hash should be non-zero");
    }))
    .await;
}

#[tokio::test]
async fn returns_metadata_hash_for_deleted_branch() {
    let repository_id = random::<RepositoryId>();
    let main_id = BranchId::from(uuid::Uuid::now_v7());
    let child_id = BranchId::from(uuid::Uuid::now_v7());

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository_id,
        ));
        let write_token = get_write_token();
        branch::create(
            repository.clone(),
            &write_token,
            main_id,
            "main",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create main");

        // Push a real latest revision on main so the child can fork
        // from it (the parent validator rejects zero-revision parents
        // unless the parent is the repository's default branch, which
        // the test fixture doesn't initialise).
        let state = state::State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        let state_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let main_latest = branch_push::push(
            repository.clone(),
            main_id,
            state_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("seed main latest")
        .revision;

        branch::create(
            repository.clone(),
            &write_token,
            child_id,
            "feature",
            branch::personal_category(),
            "creator",
            1,
            vec![lore_base::types::BranchPoint {
                branch: main_id,
                revision: main_latest,
            }],
            false,
            false,
        )
        .await
        .expect("create child");
        branch::delete(repository.clone(), child_id)
            .await
            .expect("delete child");

        // Metadata blob is preserved through delete, so the
        // pointer is still readable by id.
        let response = handler(
            make_request(repository_id, child_id),
            immutable_store,
            mutable_store,
        )
        .await
        .expect("Handler failed");
        let hash: lore_storage::Hash = response.into_inner().metadata.into();
        assert!(!hash.is_zero());
    }))
    .await;
}

#[tokio::test]
async fn returns_not_found_for_nonexistent_branch() {
    let repository_id = random::<RepositoryId>();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let err = handler(
            make_request(repository_id, branch_id),
            immutable_store,
            mutable_store,
        )
        .await
        .expect_err("nonexistent branch should fail");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

#[tokio::test]
async fn rejects_zero_branch_id() {
    let repository_id = random::<RepositoryId>();
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    let err = handler(
        make_request(repository_id, BranchId::default()),
        immutable_store,
        mutable_store,
    )
    .await
    .expect_err("zero branch id should fail");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn rejects_missing_repository_id() {
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    let request = Request::new(BranchMetadataGetRequest {
        id: branch_id.into(),
    });
    let err = handler(request, immutable_store, mutable_store)
        .await
        .expect_err("missing repository should fail");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
