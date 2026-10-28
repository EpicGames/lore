// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::BranchMetadataSetRequest;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_metadata_set::*;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Request;

use crate::store::test_support::test_store_create;

fn make_request(
    repository: RepositoryId,
    branch: BranchId,
    expected_hash: Hash,
    new_hash: Hash,
) -> Request<BranchMetadataSetRequest> {
    let mut request = Request::new(BranchMetadataSetRequest {
        branch_id: branch.into(),
        expected_hash: expected_hash.into(),
        new_hash: new_hash.into(),
    });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

/// Create a branch and return its metadata hash.
async fn create_branch(repository: Arc<RepositoryContext>, branch_id: BranchId) -> Hash {
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

    branch::metadata_hash(repository, branch_id)
        .await
        .expect("Failed to load metadata hash")
}

/// Serialize a metadata blob and return its hash.
async fn serialize_metadata(repository: Arc<RepositoryContext>, metadata: &Metadata) -> Hash {
    metadata
        .serialize(repository)
        .await
        .expect("Failed to serialize metadata")
}

#[tokio::test]
async fn set_custom_key_succeeds() {
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

        let current_hash = create_branch(repository.clone(), branch_id).await;

        // Build proposed metadata with the same read-only fields plus a custom key
        let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
            .await
            .expect("deserialize");
        proposed
            .set_string("custom-key", "custom-value")
            .expect("set custom key");
        let new_hash = serialize_metadata(repository.clone(), &proposed).await;

        let request = make_request(repository_id, branch_id, current_hash, new_hash);
        let response = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store,
            mutable_store,
        )
        .await
        .expect("Handler failed");

        let inner = response.into_inner();
        assert!(inner.success);
        assert_eq!(Hash::from(inner.current_hash), new_hash);
    }))
    .await;
}

#[tokio::test]
async fn rejects_modification_of_read_only_name() {
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

        let current_hash = create_branch(repository.clone(), branch_id).await;

        let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
            .await
            .expect("deserialize");
        proposed
            .set_string(branch::NAME, "renamed-branch")
            .expect("set name");
        let new_hash = serialize_metadata(repository.clone(), &proposed).await;

        let request = make_request(repository_id, branch_id, current_hash, new_hash);
        let result = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store,
            mutable_store,
        )
        .await;

        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("name"));
    }))
    .await;
}

#[tokio::test]
async fn rejects_removal_of_read_only_creator() {
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

        let current_hash = create_branch(repository.clone(), branch_id).await;

        let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
            .await
            .expect("deserialize");
        proposed.remove_key(branch::CREATOR);
        let new_hash = serialize_metadata(repository.clone(), &proposed).await;

        let request = make_request(repository_id, branch_id, current_hash, new_hash);
        let result = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store,
            mutable_store,
        )
        .await;

        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("creator"));
    }))
    .await;
}

#[tokio::test]
async fn allows_modification_of_protect_field() {
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

        let current_hash = create_branch(repository.clone(), branch_id).await;

        let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
            .await
            .expect("deserialize");
        proposed
            .set_bool(branch::PROTECT, true)
            .expect("set protect");
        let new_hash = serialize_metadata(repository.clone(), &proposed).await;

        let request = make_request(repository_id, branch_id, current_hash, new_hash);
        let response = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store,
            mutable_store,
        )
        .await
        .expect("Handler failed — protect should be writable");

        assert!(response.into_inner().success);
    }))
    .await;
}

#[tokio::test]
async fn cas_fails_on_stale_expected_hash() {
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

        let current_hash = create_branch(repository.clone(), branch_id).await;

        // Build a valid proposed metadata with a custom key
        let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
            .await
            .expect("deserialize");
        proposed.set_string("key", "value").expect("set custom key");
        let new_hash = serialize_metadata(repository.clone(), &proposed).await;

        // Use a bogus expected hash
        let stale_hash = Hash::from(random::<[u8; 32]>());
        let request = make_request(repository_id, branch_id, stale_hash, new_hash);
        let result = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store,
            mutable_store,
        )
        .await;

        // CAS should fail because expected_hash doesn't match (it can't be deserialized)
        assert!(result.is_err());
    }))
    .await;
}

#[tokio::test]
async fn rejects_missing_branch_id() {
    let repository_id = random::<RepositoryId>();

    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    let request = make_request(
        repository_id,
        BranchId::default(),
        Hash::default(),
        Hash::default(),
    );
    let result = handler(
        request,
        &crate::util::test_support::allow_all_authorizer(),
        immutable_store,
        mutable_store,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn rejects_modification_of_read_only_category() {
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

        let current_hash = create_branch(repository.clone(), branch_id).await;

        let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
            .await
            .expect("deserialize");
        proposed
            .set_string(branch::CATEGORY, "changed-category")
            .expect("set category");
        let new_hash = serialize_metadata(repository.clone(), &proposed).await;

        let request = make_request(repository_id, branch_id, current_hash, new_hash);
        let result = handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            immutable_store,
            mutable_store,
        )
        .await;

        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("category"));
    }))
    .await;
}
