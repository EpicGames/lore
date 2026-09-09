// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1::RepositoryMetadataGetRequest;
use lore_proto::lore::repository::v1::RepositoryMetadataGetResponse;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
use crate::grpc::no_repository_access_status;
use crate::util::setup_execution;

/// `lore.repository.v1.RepositoryService.RepositoryMetadataGet` handler.
///
/// Cheap hash-only read of the repository's metadata pointer. Returns the
/// hash unchanged from `repository::metadata_hash`; callers wanting the
/// deserialised metadata blob fetch the addressed content separately.
#[tracing::instrument(name = "RepositoryMetadataGet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryMetadataGetRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryMetadataGetResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let (_, extensions, req) = request.into_parts();

    let repository_id: Context = req.id.into();
    if repository_id == Context::default() {
        return Err(Status::invalid_argument("Missing repository id"));
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
                .check_repository_access(
                    get_verified_token(&extensions).as_ref(),
                    repository_id.into(),
                    None,
                )
                .await
                .map_err(|_err| no_repository_access_status())?;

            let metadata_hash = repository::metadata_hash(repository)
                .await
                .filter_slow_down()?
                .map_err(|err| Status::not_found(err.to_string()))?;

            Ok(Response::new(RepositoryMetadataGetResponse {
                metadata: metadata_hash.into(),
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

    /// Denies every request.
    struct DenyAllRepositoryAuthorizer;

    #[async_trait::async_trait]
    impl RepositoryAuthorizer for DenyAllRepositoryAuthorizer {
        async fn check_repository_access(
            &self,
            _token: Option<&crate::authnz::repository_authorizer::VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            _action: Option<&str>,
        ) -> Result<(), Status> {
            Err(Status::permission_denied("denied"))
        }
    }

    /// Permits, recording that the handler asked with `action: None`.
    #[derive(Default)]
    struct RecordingPermitAuthorizer {
        called_with_action_none: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl RepositoryAuthorizer for RecordingPermitAuthorizer {
        async fn check_repository_access(
            &self,
            _token: Option<&crate::authnz::repository_authorizer::VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            action: Option<&str>,
        ) -> Result<(), Status> {
            self.called_with_action_none
                .store(action.is_none(), std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Writes a metadata blob to both the immutable and mutable stores so
    /// that `metadata_get` can return it successfully.
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
                    id: REPOSITORY_ID.to_vec().into(),
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
        let request = Request::new(RepositoryMetadataGetRequest {
            id: REPOSITORY_ID.to_vec().into(),
        });
        let err = handler(
            request,
            Arc::new(DenyAllRepositoryAuthorizer),
            immutable,
            mutable,
        )
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
                let authorizer = Arc::new(RecordingPermitAuthorizer::default());
                let request = Request::new(RepositoryMetadataGetRequest {
                    id: REPOSITORY_ID.to_vec().into(),
                });
                handler(request, authorizer.clone(), immutable, mutable)
                    .await
                    .unwrap();
                assert!(
                    authorizer
                        .called_with_action_none
                        .load(std::sync::atomic::Ordering::SeqCst)
                );
            })
            .await;
    }
}
