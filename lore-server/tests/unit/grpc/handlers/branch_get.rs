// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::BranchGetRequest;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::allow_all_repositories;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_get::*;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Request;

use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_handle() {
    let repository = random::<Context>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
                allow_all_repositories(),
            ));
            let write_token = get_write_token();
            // Create the main branch (without parent)
            let main = lore_revision::branch::create(
                repository.clone(),
                &write_token,
                BranchId::from(uuid::Uuid::now_v7()),
                lore_revision::branch::DEFAULT_DEFAULT_NAME,
                lore_revision::branch::default_category(),
                "BranchCreator",
                12345,
                vec![],
                false,
                false,
            )
            .await
            .expect("Could not create main branch");

            // Create another branch1
            let payload_branch1 = random::<[u8; size_of::<Hash>()]>().to_vec();
            let hash_branch1 = Hash::hash_buffer(&payload_branch1);
            let branch1 = lore_revision::branch::create(
                repository.clone(),
                &write_token,
                BranchId::from(uuid::Uuid::now_v7()),
                "branch1",
                lore_revision::branch::default_category(),
                "BranchCreator",
                12345,
                vec![],
                false,
                false,
            )
            .await
            .expect("Could not create branch1 branch");

            // Try getting main
            let mut request = Request::new(BranchGetRequest {
                branch: main.into(),
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Request failed");
            let response_branch = response
                .into_inner()
                .branch
                .expect("Did not get branch metadata as expected");
            let expected = lore_proto::Branch {
                id: main.into(),
                name: lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                category: lore_revision::branch::default_category().to_string(),
                latest: Hash::default().into(),
                parent_deprecated: Some(Context::default().into()),
                branch_point_deprecated: Some(Hash::default().into()),
                creator: "BranchCreator".to_string(),
                created: 12345,
                stack: vec![],
            };
            assert_eq!(response_branch, expected);

            // Update branch1 branch
            branch::store_latest(
                repository.clone(),
                branch1,
                Hash::default(),
                hash_branch1,
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("Failed to store head");

            // Try getting branch1
            let mut request = Request::new(BranchGetRequest {
                branch: branch1.into(),
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Request failed");
            let response_branch = response
                .into_inner()
                .branch
                .expect("Did not get branch metadata as expected");
            let expected = lore_proto::Branch {
                id: branch1.into(),
                name: "branch1".to_string(),
                category: lore_revision::branch::default_category().to_string(),
                latest: hash_branch1.into(),
                parent_deprecated: Some(Context::default().into()),
                branch_point_deprecated: Some(Hash::default().into()),
                creator: "BranchCreator".to_string(),
                created: 12345,
                stack: vec![],
            };
            assert_eq!(response_branch, expected);
        })
        .await;
}
