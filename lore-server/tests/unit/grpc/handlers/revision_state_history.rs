// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::RevisionStateHistoryRequest;
use lore_proto::RevisionStateHistoryResponse;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::allow_all_repositories;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::revision_state_history::*;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Request;
use tracing::debug;

use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_handle() {
    let repository = random::<RepositoryId>();

    let context_map = Arc::new(AttributeMap::default());
    context_map.insert(repository);

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let write_token = get_write_token();
            // Create the main branch (without parent)
            let _main = lore_revision::branch::create(
                repository.clone(),
                &write_token,
                Context::from(uuid::Uuid::now_v7()),
                lore_revision::branch::DEFAULT_DEFAULT_NAME,
                lore_revision::branch::default_category(),
                "CreatorUser",
                1234,
                vec![],
                false,
                false,
            )
            .await
            .expect("Could not create main branch");

            let message = RevisionStateHistoryRequest {
                revision: Hash::default().into(),
                depth: 5,
                follow_merge: false,
                with_metadata: true,
            };
            let mut request = Request::new(message);
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Failed RevisionStateHistoryRequest message handle");
            assert_eq!(
                RevisionStateHistoryResponse {
                    signature: vec![],
                    metadata: vec![]
                },
                response.into_inner()
            );

            let message = RevisionStateHistoryRequest {
                revision: Hash::default().into(),
                depth: 5,
                follow_merge: false,
                with_metadata: false,
            };
            let mut request = Request::new(message);
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Failed RevisionStateHistoryRequest message handle");
            assert_eq!(
                RevisionStateHistoryResponse {
                    signature: vec![],
                    metadata: vec![]
                },
                response.into_inner()
            );

            let state = state::State::new();
            state.set_parent_self(Hash::default());
            let base_hash = state
                .serialize(repository.clone(), &write_token)
                .await
                .expect("Failed to serialize base state");
            debug!("Created base revision {}", base_hash);

            state.set_parent_self(base_hash);
            let second_hash = state
                .serialize(repository.clone(), &write_token)
                .await
                .expect("Failed to serialize second state");
            debug!("Created second revision {}", second_hash);

            state.set_parent_self(second_hash);
            let third_hash = state
                .serialize(repository.clone(), &write_token)
                .await
                .expect("Failed to serialize third state");
            debug!("Created third revision {}", third_hash);

            let message = RevisionStateHistoryRequest {
                revision: third_hash.into(),
                depth: 5,
                follow_merge: false,
                with_metadata: false,
            };
            let mut request = Request::new(message);
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Failed RevisionStateHistoryRequest message handle");
            assert_eq!(
                RevisionStateHistoryResponse {
                    signature: vec![second_hash.into(), base_hash.into(), Hash::default().into()],
                    metadata: vec![]
                },
                response.into_inner()
            );

            let message = RevisionStateHistoryRequest {
                revision: Hash::default().into(),
                depth: 10000,
                follow_merge: false,
                with_metadata: false,
            };
            let mut request = Request::new(message);
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Failed RevisionStateHistoryRequest message handle");
            assert_eq!(
                RevisionStateHistoryResponse {
                    signature: vec![],
                    metadata: vec![]
                },
                response.into_inner()
            );
        })
        .await;
}
