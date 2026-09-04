// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchMetadataGetRequest;
use lore_proto::BranchMetadataGetResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchMetadataGet::handle", skip_all)]
pub async fn handler(
    request: Request<BranchMetadataGetRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchMetadataGetResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let branch = BranchId::from(req.branch_id);
    if branch == BranchId::default() {
        return Err(Status::invalid_argument("Missing branch ID"));
    }

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let metadata_hash = branch::metadata_hash(repository, branch)
                .await
                .filter_slow_down()?
                .map_err(|err| {
                    warn!(%err, "Failed to load branch metadata hash");
                    Status::not_found(err.to_string())
                })?;

            Ok(Response::new(BranchMetadataGetResponse {
                metadata_hash: metadata_hash.into(),
            }))
        })
        .await
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_proto::BranchMetadataGetRequest;
    use lore_revision::branch;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::store::test_store_create;

    fn make_request(
        repository: RepositoryId,
        branch: BranchId,
    ) -> Request<BranchMetadataGetRequest> {
        let mut request = Request::new(BranchMetadataGetRequest {
            branch_id: branch.into(),
        });
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

            let request = make_request(repository_id, branch_id);
            let response = handler(request, immutable_store, mutable_store)
                .await
                .expect("Handler failed");

            let hash: lore_storage::Hash = response.into_inner().metadata_hash.into();
            assert!(!hash.is_zero(), "metadata hash should be non-zero");
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
            let request = make_request(repository_id, branch_id);
            let result = handler(request, immutable_store, mutable_store).await;

            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_missing_branch_id() {
        let repository_id = random::<RepositoryId>();

        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        let request = make_request(repository_id, BranchId::default());
        let result = handler(request, immutable_store, mutable_store).await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn rejects_missing_repository_id() {
        let branch_id = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        let request = Request::new(BranchMetadataGetRequest {
            branch_id: branch_id.into(),
        });
        let result = handler(request, immutable_store, mutable_store).await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }
}
