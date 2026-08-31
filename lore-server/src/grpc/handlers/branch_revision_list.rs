// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::BranchRevisionListRequest;
use lore_proto::BranchRevisionListResponse;
use lore_revision::branch::list_revisions;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::warn;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

const REVISIONS_LIMIT: u32 = 100;

#[tracing::instrument(name = "BranchRevisionList::handle", skip_all)]
pub async fn handler(
    request: Request<BranchRevisionListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchRevisionListResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let source = req.source.map(Hash::from);
    let target = req.target.map(Hash::from);
    let branch = req.branch.map(BranchId::from);

    if branch.is_none() && source.is_none() {
        return Err(Status::invalid_argument(
            "branch is required when source is not provided",
        ));
    }

    let limit = req.limit.unwrap_or(REVISIONS_LIMIT).min(REVISIONS_LIMIT);

    debug!(
        {BRANCH_ID} = ?branch, limit, target_hash = ?target,
        "Handling branch revision list",
    );

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
    ));
    LORE_CONTEXT
        .scope(execution, async move {
            list_revisions(repository, branch, Some(limit as usize), source, target)
                .await
                .map(|result| {
                    debug!("Found {} revisions", result.revisions.len());
                    Response::new(BranchRevisionListResponse {
                        revisions: result
                            .revisions
                            .iter()
                            .map(lore_proto::Revision::from)
                            .collect(),
                        has_more: result.has_more,
                    })
                })
                .map_err(|e| {
                    if e.is_branch_not_found() {
                        debug!("Failed to retrieve list of revisions for branch: {branch:?}");
                        Status::not_found("Branch does not exist")
                    } else if e.is_invalid_arguments() {
                        debug!("Branch is required when source is not provided");
                        Status::invalid_argument(e.to_string())
                    } else {
                        warn!(
                            {BRANCH_ID} = ?branch, error = ?e,
                            "Error retrieving the list of revisions"
                        );
                        Status::internal("Failed to retrieve the branch revision list")
                    }
                })
        })
        .await
}

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::branch::{self};
    use lore_revision::state;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::store::test_store_create;

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
            let state = Arc::new(state::State::new());
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
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
            )
            .await
            .expect("Failed to push head revision")
            .revision;
            assert_eq!(head, first_hash);

            let state = Arc::new(state::State::new());
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
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
            )
            .await
            .expect("Failed to push head revision")
            .revision;
            assert_eq!(head, second_hash);

            let state = Arc::new(state::State::new());
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
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
            )
            .await
            .expect("Failed to push head revision")
            .revision;
            assert_eq!(head, third_hash);

            let state = Arc::new(state::State::new());
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
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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

            let state = Arc::new(state::State::new());
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
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
            )
            .await
            .expect("Failed to push head revision");

            let state = Arc::new(state::State::new());
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
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
            )
            .await
            .expect("Failed to push head revision");

            let state = Arc::new(state::State::new());
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
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
}
