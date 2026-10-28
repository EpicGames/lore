// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::BranchRevisionListRequest;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::branch::{self};
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::handlers::branch_revision_list::*;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Request;
use zerocopy::IntoBytes;

use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_handle() {
    let repository = random::<Context>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository.into(),
        ));
        let write_token = get_write_token();
        // Create the main branch (without parent)
        let main = lore_revision::branch::create(
            repository.clone(),
            &write_token,
            BranchId::from(uuid::Uuid::now_v7()),
            branch::DEFAULT_DEFAULT_NAME,
            branch::default_category(),
            "BranchCreator",
            12345,
            vec![],
            false,
            false,
        )
        .await
        .expect("Could not create main branch");

        // Create a few revisions
        let state = state::State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        let first_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");
        let head = branch_push::push(
            repository.clone(),
            main,
            first_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision")
        .revision;
        assert_eq!(head, first_hash);

        let state = state::State::new();
        state.set_parent_self(first_hash);
        state.set_revision_number(2);
        let second_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");
        let head = branch_push::push(
            repository.clone(),
            main,
            second_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision")
        .revision;
        assert_eq!(head, second_hash);

        let state = state::State::new();
        state.set_parent_self(second_hash);
        state.set_revision_number(3);
        let third_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");
        let head = branch_push::push(
            repository.clone(),
            main,
            third_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision")
        .revision;
        assert_eq!(head, third_hash);

        let state = state::State::new();
        state.set_parent_self(third_hash);
        state.set_revision_number(4);
        let fourth_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");
        let head = branch_push::push(
            repository.clone(),
            main,
            fourth_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision")
        .revision;
        assert_eq!(head, fourth_hash);

        // Try getting branch revision list unbounded
        let mut request = Request::new(BranchRevisionListRequest {
            branch: Some(main.into()),
            limit: None,
            source: None,
            target: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let list = response.into_inner().revisions;
        assert_eq!(list.len(), 4);
        assert_eq!(list[3].id, first_hash.as_bytes());
        assert_eq!(list[2].id, second_hash.as_bytes());
        assert_eq!(list[1].id, third_hash.as_bytes());
        assert_eq!(list[0].id, fourth_hash.as_bytes());
        assert_eq!(list[3].parent_self, None);
        assert_eq!(list[2].parent_self.clone().unwrap(), first_hash.as_bytes());
        assert_eq!(list[1].parent_self.clone().unwrap(), second_hash.as_bytes());
        assert_eq!(list[0].parent_self.clone().unwrap(), third_hash.as_bytes());
        assert_eq!(list[3].parent_other, None);
        assert_eq!(list[2].parent_other, None);
        assert_eq!(list[1].parent_other, None);
        assert_eq!(list[0].parent_other, None);
        assert_eq!(list[3].number, 1);
        assert_eq!(list[2].number, 2);
        assert_eq!(list[1].number, 3);
        assert_eq!(list[0].number, 4);
        assert_eq!(list[3].parent_self_number, None);
        assert_eq!(list[2].parent_self_number, Some(1));
        assert_eq!(list[1].parent_self_number, Some(2));
        assert_eq!(list[0].parent_self_number, Some(3));
        assert_eq!(list[3].parent_other_number, None);
        assert_eq!(list[2].parent_other_number, None);
        assert_eq!(list[1].parent_other_number, None);
        assert_eq!(list[0].parent_other_number, None);

        // Try getting branch revision list bounded
        let mut request = Request::new(BranchRevisionListRequest {
            branch: Some(main.into()),
            limit: Some(2),
            source: None,
            target: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let response = response.into_inner();
        let list = response.revisions;
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].id, third_hash.as_bytes());
        assert_eq!(list[0].id, fourth_hash.as_bytes());
        assert!(response.has_more);

        // Try getting branch revision with source
        let mut request = Request::new(BranchRevisionListRequest {
            branch: Some(main.into()),
            limit: None,
            source: Some(second_hash.into()),
            target: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let response = response.into_inner();
        let list = response.revisions;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, second_hash.as_bytes());
        assert_eq!(list[1].id, first_hash.as_bytes());
        assert!(!response.has_more);

        // Try getting branch revision with target
        let mut request = Request::new(BranchRevisionListRequest {
            branch: Some(main.into()),
            limit: None,
            source: None,
            target: Some(second_hash.into()),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let response = response.into_inner();
        let list = response.revisions;
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].id, third_hash.as_bytes());
        assert_eq!(list[0].id, fourth_hash.as_bytes());
        assert!(!response.has_more);

        // Try getting branch revision with target and limit where limit is hit first
        let mut request = Request::new(BranchRevisionListRequest {
            branch: Some(main.into()),
            limit: Some(1),
            source: None,
            target: Some(second_hash.into()),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let response = response.into_inner();
        let list = response.revisions;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, fourth_hash.as_bytes());
        assert_eq!(list[0].number, 4);
        assert!(response.has_more);

        // Try getting branch revision with target and limit where target is hit first
        let mut request = Request::new(BranchRevisionListRequest {
            branch: Some(main.into()),
            limit: Some(4),
            source: None,
            target: Some(first_hash.into()),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let response = response.into_inner();
        let list = response.revisions;
        assert_eq!(list.len(), 3);
        assert_eq!(list[2].id, second_hash.as_bytes());
        assert_eq!(list[1].id, third_hash.as_bytes());
        assert_eq!(list[0].id, fourth_hash.as_bytes());
        assert!(!response.has_more);
    }))
    .await;
}

#[tokio::test]
async fn test_handle_optional_branch() {
    let repository = random::<Context>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository.into(),
        ));
        let write_token = get_write_token();
        // No branch and no source should fail with invalid_argument
        let mut request = Request::new(BranchRevisionListRequest {
            branch: None,
            limit: None,
            source: None,
            target: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect_err("Request should have failed");
        assert_eq!(response.code(), tonic::Code::InvalidArgument);

        // No branch with only target should also fail with invalid_argument
        let mut request = Request::new(BranchRevisionListRequest {
            branch: None,
            limit: None,
            source: None,
            target: Some(random::<Hash>().into()),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect_err("Request should have failed");
        assert_eq!(response.code(), tonic::Code::InvalidArgument);

        // Set up a branch with 3 revisions that have branch metadata
        let main = lore_revision::branch::create(
            repository.clone(),
            &write_token,
            BranchId::from(uuid::Uuid::now_v7()),
            branch::DEFAULT_DEFAULT_NAME,
            branch::default_category(),
            "BranchCreator",
            12345,
            vec![],
            false,
            false,
        )
        .await
        .expect("Could not create main branch");

        let mut rev_metadata = lore_revision::metadata::Metadata::new();
        rev_metadata.set_branch(main).unwrap();
        let metadata_hash = rev_metadata.serialize(repository.clone()).await.unwrap();

        let state = state::State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        state.set_metadata_hash(metadata_hash);
        let first_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");
        branch_push::push(
            repository.clone(),
            main,
            first_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision");

        let state = state::State::new();
        state.set_parent_self(first_hash);
        state.set_revision_number(2);
        state.set_metadata_hash(metadata_hash);
        let second_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");
        branch_push::push(
            repository.clone(),
            main,
            second_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision");

        let state = state::State::new();
        state.set_parent_self(second_hash);
        state.set_revision_number(3);
        state.set_metadata_hash(metadata_hash);
        let third_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");
        branch_push::push(
            repository.clone(),
            main,
            third_hash,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision");

        // Source and target without branch should succeed
        let mut request = Request::new(BranchRevisionListRequest {
            branch: None,
            limit: None,
            source: Some(third_hash.into()),
            target: Some(first_hash.into()),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let list = response.into_inner().revisions;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, third_hash.as_bytes());
        assert_eq!(list[1].id, second_hash.as_bytes());

        // Source only without branch should derive branch from source revision
        let mut request = Request::new(BranchRevisionListRequest {
            branch: None,
            limit: None,
            source: Some(third_hash.into()),
            target: None,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Request failed");
        let list = response.into_inner().revisions;
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, third_hash.as_bytes());
        assert_eq!(list[1].id, second_hash.as_bytes());
        assert_eq!(list[2].id, first_hash.as_bytes());
    }))
    .await;
}

#[tokio::test]
async fn test_handle_not_exist() {
    let repository = random::<Context>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));
            // Try getting branch revision list unbounded
            let mut request = Request::new(BranchRevisionListRequest {
                branch: Some(random::<Context>().into()),
                limit: None,
                source: None,
                target: None,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect_err("Request should have failed");

            assert_eq!(response.code(), tonic::Code::NotFound);
        })
        .await;
}
