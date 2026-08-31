// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::RepositoryMetadataGetRequest;
use lore_proto::RepositoryMetadataGetResponse;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::warn;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::extract_authorization_header;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::no_repository_access_status;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryMetadataGet::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryMetadataGetRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryMetadataGetResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = extract_authorization_header(&request);
    let req = request.into_inner();

    let repository_id: Context = req.repository_id.into();
    if repository_id == Context::default() {
        return Err(Status::invalid_argument("Missing repository ID"));
    }

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id.into(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            authorizer
                .check_repository_access(authorization, repository_id.into())
                .await
                .map_err(|_err| no_repository_access_status())?;

            let metadata_hash = repository::metadata_hash(repository).await.map_err(|err| {
                warn!(%err, "Failed to load repository metadata hash");
                Status::not_found(err.to_string())
            })?;

            Ok(Response::new(RepositoryMetadataGetResponse {
                metadata_hash: metadata_hash.into(),
            }))
        })
        .await
}

#[cfg(test)]
mod tests {
    use lore_base::types::RepositoryId;
    use lore_revision::repository::RepositoryMetadata;
    use tonic::Code;

    use super::*;
    use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use crate::store::test_store_create;

    const REPOSITORY_ID: [u8; 16] = [1u8; 16];

    mockall::mock! {
        pub Authorizer {}

        #[async_trait::async_trait]
        impl RepositoryAuthorizer for Authorizer {
            async fn check_repository_access(
                &self,
                authorization: Option<String>,
                repository_id: RepositoryId,
            ) -> Result<(), Status>;
        }
    }

    async fn seed_metadata(
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        mutable: Arc<dyn lore_storage::MutableStore>,
    ) {
        let repo_ctx = Arc::new(RepositoryContext::new_server_context(
            immutable,
            mutable,
            Context::from(REPOSITORY_ID).into(),
        ));
        let hash = lore_revision::repository::metadata_store(
            repo_ctx.clone(),
            RepositoryMetadata {
                name: "test".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        lore_revision::repository::metadata_store_hash(repo_ctx, hash)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn no_auth_configured_allows_operation() {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                seed_metadata(immutable.clone(), mutable.clone()).await;
                let request = Request::new(RepositoryMetadataGetRequest {
                    repository_id: REPOSITORY_ID.to_vec().into(),
                });
                handler(
                    request,
                    Arc::new(AllowAllRepositoryAuthorizer),
                    immutable,
                    mutable,
                )
                .await
                .unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn auth_configured_no_access_returns_permission_denied() {
        let (immutable, mutable, _) = test_store_create().await.unwrap();
        let mut mock = MockAuthorizer::new();
        mock.expect_check_repository_access()
            .returning(|_, _| Err(Status::permission_denied("denied")));
        let request = Request::new(RepositoryMetadataGetRequest {
            repository_id: REPOSITORY_ID.to_vec().into(),
        });
        let err = handler(request, Arc::new(mock), immutable, mutable)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), "Unauthorized");
    }

    #[tokio::test]
    async fn auth_configured_with_access_allows_operation() {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                seed_metadata(immutable.clone(), mutable.clone()).await;
                let mut mock = MockAuthorizer::new();
                mock.expect_check_repository_access()
                    .returning(|_, _| Ok(()));
                let request = Request::new(RepositoryMetadataGetRequest {
                    repository_id: REPOSITORY_ID.to_vec().into(),
                });
                handler(request, Arc::new(mock), immutable, mutable)
                    .await
                    .unwrap();
            })
            .await;
    }
}
