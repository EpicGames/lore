// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::RepositoryId;
use lore_proto::lore::repository::v1::RepositoryMetadataGetRequest;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::grpc::repository::v1::repository_metadata_get::*;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::store::test_support::test_store_create;

const REPOSITORY_ID: [u8; 16] = [1u8; 16];

/// Denies every request.
struct DenyAllRepositoryAuthorizer;

#[async_trait::async_trait]
impl RepositoryAuthorizer for DenyAllRepositoryAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&lore_server::authnz::repository_authorizer::VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        _action: Action,
    ) -> Result<(), Status> {
        Err(Status::permission_denied("denied"))
    }
}

/// Permits, recording the action the handler asked.
#[derive(Default)]
struct RecordingPermitAuthorizer {
    asked: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl RepositoryAuthorizer for RecordingPermitAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&lore_server::authnz::repository_authorizer::VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        self.asked.lock().unwrap().replace(action.to_string());
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
            assert_eq!(authorizer.asked.lock().unwrap().as_deref(), Some("read"));
        })
        .await;
}
