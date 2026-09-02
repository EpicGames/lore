// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_proto::lore::repository::v1::RepositoryGetRequest;
use lore_proto::lore::repository::v1::RepositoryGetResponse;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::repository::v1::repository_get::repository_get_implementation;

/// Handler that takes a `RepositoryGet` request forwarded on from peer's `RepositoryService`
/// and executes it, returning the result to the other server for forwarding on to its
/// client
#[tracing::instrument(name = "ForwardedRepository::v1::RepositoryGet::Handler", skip_all)]
pub async fn handler(
    request: Request<RepositoryGetRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryGetResponse>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;

    repository_get_implementation(
        request.into_inner(),
        caller_context,
        auth_url,
        immutable_store,
        mutable_store,
    )
    .await
}

#[cfg(test)]
mod test {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_proto::lore::repository::v1::repository_get_request::Query;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::store::test_store_create;

    async fn store_repository(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        id: RepositoryId,
        name: &str,
    ) {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            id,
        ));
        let metadata = lore_revision::repository::RepositoryMetadata {
            name: name.to_string(),
            description: "a description".into(),
            default_branch: Context::from(uuid::Uuid::now_v7()),
            default_branch_name: "main".into(),
            creator: "alice".into(),
            created: 12345,
        };
        let metadata_hash =
            lore_revision::repository::metadata_store(repository.clone(), metadata.clone())
                .await
                .expect("Failed to store repository metadata");
        lore_revision::repository::metadata_store_hash(repository.clone(), metadata_hash)
            .await
            .expect("Failed to store repository metadata hash");
        lore_revision::repository::store_name_to_id(repository, name, id)
            .await
            .expect("Failed to store repository name to id mapping");
    }

    fn make_forwarded_request(query: Query) -> Request<RepositoryGetRequest> {
        CallerContext {
            repository_id: RepositoryId::default(),
            user_id: "alice".into(),
            correlation_id: String::new(),
            authorization: None,
        }
        .to_forwarded_request(RepositoryGetRequest { query: Some(query) })
        .expect("CallerContext::to_forwarded_request failed in test")
    }

    #[tokio::test]
    async fn missing_user_id_returns_internal_error() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            // Deliberately omit on-behalf-of-user-id to test the missing-field error
            let request = Request::new(RepositoryGetRequest {
                query: Some(Query::Name("my-repo".into())),
            });

            let err = handler(request, None, immutable_store, mutable_store)
                .await
                .expect_err("missing user id should fail");

            assert_eq!(err.code(), tonic::Code::Internal);
            assert!(err.message().contains("on-behalf-of-user-id"));
        }))
        .await;
    }

    // Happy and unhappy paths verify that whatever the underlying
    // `repository_get_implementation` returns is forwarded on correctly.
    mod base_repository_get_handler {
        use super::*;

        #[tokio::test]
        async fn get_by_name_returns_full_repository_record() {
            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                store_repository(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    id,
                    "my-repo",
                )
                .await;

                let response = handler(
                    make_forwarded_request(Query::Name("my-repo".into())),
                    None, /* no auth */
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect("Request failed");

                let repository = response
                    .into_inner()
                    .repository
                    .expect("response should include Repository");
                assert_eq!(repository.name, "my-repo");
                assert_eq!(repository.creator, "alice");
                assert_eq!(repository.id, bytes::Bytes::from(id));
            }))
            .await;
        }

        #[tokio::test]
        async fn get_unknown_id_returns_not_found() {
            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let err = handler(
                    make_forwarded_request(Query::Id(id.into())),
                    None,
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect_err("unknown id should fail");

                assert_eq!(err.code(), tonic::Code::NotFound);
            }))
            .await;
        }
    }
}
