// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_proto::lore::revision::v1::BranchListRequest;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::revision::v1::branch_list::BranchListStream;
use crate::grpc::revision::v1::branch_list::branch_list_implementation;

/// Handler that takes a `BranchList` request forwarded on from peer's `RevisionService`
/// and executes it, streaming the response back to the other server for forwarding on
/// to its client.
#[tracing::instrument(name = "ForwardedRevision::v1::BranchList::Handler", skip_all)]
pub async fn handler(
    request: Request<BranchListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchListStream>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;
    let req = request.into_inner();

    branch_list_implementation(req, caller_context, immutable_store, mutable_store).await
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_proto::lore::revision::v1::BranchListRequest;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tokio_stream::StreamExt;
    use tonic::Request;

    use super::*;
    use crate::grpc::forwarded_requests::CallerContext;
    use crate::grpc::revision::v1::branch_list::BranchListStream;
    use crate::store::test_store_create;

    fn make_forwarded_request(repository: RepositoryId) -> Request<BranchListRequest> {
        CallerContext {
            repository_id: repository,
            user_id: "lily".into(),
            correlation_id: String::new(),
            authorization: None,
        }
        .to_forwarded_request(BranchListRequest {
            creator: None,
            include_deleted: false,
        })
        .expect("CallerContext::to_forwarded_request failed in test")
    }

    #[tokio::test]
    async fn missing_user_id_returns_internal_error() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            // No on-behalf-of-user-id in metadata
            let mut request = Request::new(BranchListRequest {
                creator: None,
                include_deleted: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let err = handler(request, immutable_store, mutable_store)
                .await
                .map(|_| ())
                .expect_err("missing user id should fail");

            assert_eq!(err.code(), tonic::Code::Internal);
            assert!(err.message().contains("on-behalf-of-user-id"));
        }))
        .await;
    }

    // Happy and unhappy paths verify that whatever the underlying
    // `branch_list_implementation` returns is forwarded on correctly.
    mod base_branch_list_handler {
        use super::*;
        use crate::grpc::revision::v1::branch_list::test::create_root_branch;

        async fn collect(response: Response<BranchListStream>) -> Vec<String> {
            response
                .into_inner()
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .map(|item| item.branch.unwrap().name)
                .collect()
        }

        #[tokio::test]
        async fn list_returns_branches() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                create_root_branch(&repository_context, "main", "lily").await;
                create_root_branch(&repository_context, "feature", "james").await;

                let response = handler(
                    make_forwarded_request(repository),
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect("Request failed");

                let names = collect(response).await;
                assert_eq!(names.len(), 2);
                assert!(names.contains(&"main".to_string()));
                assert!(names.contains(&"feature".to_string()));
            }))
            .await;
        }

        #[tokio::test]
        async fn empty_repository_yields_empty_stream() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let response = handler(
                    make_forwarded_request(repository),
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect("Request failed");

                let names = collect(response).await;
                assert!(names.is_empty());
            }))
            .await;
        }
    }
}
