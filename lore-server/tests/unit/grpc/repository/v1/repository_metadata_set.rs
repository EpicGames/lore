// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1::RepositoryMetadataSetRequest;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use tonic::Request;
use tonic::Status;
mod auth_guard {
    use lore_base::types::RepositoryId;
    use lore_revision::repository::RepositoryMetadata;
    use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use lore_server::grpc::repository::v1::repository_metadata_set::*;
    use tonic::Code;

    use super::*;
    use crate::store::test_support::test_store_create;

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

    const REPOSITORY_ID: [u8; 16] = [1u8; 16];

    /// Writes a valid metadata blob to the immutable store and returns its
    /// hash. `metadata_set` can then use that hash as `updated`.
    async fn seed_metadata_blob(
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        mutable: Arc<dyn lore_storage::MutableStore>,
    ) -> lore_base::types::Hash {
        let repo_ctx = Arc::new(RepositoryContext::new_server_context(
            immutable,
            mutable,
            Context::from(REPOSITORY_ID).into(),
        ));
        lore_revision::repository::metadata_store(
            repo_ctx,
            RepositoryMetadata {
                name: "test".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn no_auth_configured_allows_operation() {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                let hash = seed_metadata_blob(immutable.clone(), mutable.clone()).await;
                let request = Request::new(RepositoryMetadataSetRequest {
                    id: REPOSITORY_ID.to_vec().into(),
                    expected: vec![0u8; 32].into(),
                    updated: hash.into(),
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
        let request = Request::new(RepositoryMetadataSetRequest {
            id: REPOSITORY_ID.to_vec().into(),
            expected: vec![0u8; 32].into(),
            updated: vec![1u8; 32].into(),
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
                let hash = seed_metadata_blob(immutable.clone(), mutable.clone()).await;
                let authorizer = Arc::new(RecordingPermitAuthorizer::default());
                let request = Request::new(RepositoryMetadataSetRequest {
                    id: REPOSITORY_ID.to_vec().into(),
                    expected: vec![0u8; 32].into(),
                    updated: hash.into(),
                });
                handler(request, authorizer.clone(), immutable, mutable)
                    .await
                    .unwrap();
                assert_eq!(authorizer.asked.lock().unwrap().as_deref(), Some("write"));
            })
            .await;
    }
}

mod validate_read_only_fields {
    use lore_base::types::Context;
    use lore_revision::repository;
    use lore_server::grpc::repository::v1::repository_metadata_set::validate_read_only_fields;

    use super::*;

    /// Build a metadata blob with every read-only key populated to a
    /// known value so individual tests can mutate exactly one field
    /// and assert the rejection is attributable to that mutation.
    fn baseline() -> Metadata {
        let mut metadata = Metadata::new();
        metadata.set_string(repository::NAME, "repo").unwrap();
        metadata
            .set_context(repository::DEFAULT_BRANCH, Context::default())
            .unwrap();
        metadata
            .set_string(repository::DEFAULT_BRANCH_NAME, "main")
            .unwrap();
        metadata.set_string(repository::CREATOR, "alice").unwrap();
        metadata.set_u64(repository::CREATED, 100).unwrap();
        metadata
    }

    #[test]
    fn accepts_unchanged_read_only_fields_with_writable_change() {
        let current = baseline();
        let mut proposed = baseline();
        proposed
            .set_string(repository::DESCRIPTION, "edited description")
            .unwrap();
        validate_read_only_fields(&current, &proposed)
            .expect("description is writable, all read-only fields unchanged");
    }

    #[test]
    fn rejects_name_modification() {
        let current = baseline();
        let mut proposed = baseline();
        proposed.set_string(repository::NAME, "renamed").unwrap();
        let err = validate_read_only_fields(&current, &proposed)
            .expect_err("mutating name must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(repository::NAME));
    }

    #[test]
    fn rejects_creator_modification() {
        let current = baseline();
        let mut proposed = baseline();
        proposed.set_string(repository::CREATOR, "mallory").unwrap();
        let err = validate_read_only_fields(&current, &proposed)
            .expect_err("mutating creator must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(repository::CREATOR));
    }

    #[test]
    fn rejects_default_branch_modification() {
        let current = baseline();
        let mut proposed = baseline();
        proposed
            .set_context(repository::DEFAULT_BRANCH, Context::from([1u8; 16]))
            .unwrap();
        let err = validate_read_only_fields(&current, &proposed)
            .expect_err("mutating default-branch must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(repository::DEFAULT_BRANCH));
    }

    #[test]
    fn rejects_default_branch_name_modification() {
        let current = baseline();
        let mut proposed = baseline();
        proposed
            .set_string(repository::DEFAULT_BRANCH_NAME, "trunk")
            .unwrap();
        let err = validate_read_only_fields(&current, &proposed)
            .expect_err("mutating default-branch-name must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(repository::DEFAULT_BRANCH_NAME));
    }

    #[test]
    fn rejects_created_modification() {
        let current = baseline();
        let mut proposed = baseline();
        proposed.set_u64(repository::CREATED, 200).unwrap();
        let err = validate_read_only_fields(&current, &proposed)
            .expect_err("mutating created must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(repository::CREATED));
    }

    #[test]
    fn rejects_read_only_key_removal() {
        let current = baseline();
        let mut proposed = baseline();
        assert!(proposed.remove_key(repository::CREATOR));
        let err = validate_read_only_fields(&current, &proposed)
            .expect_err("removing a read-only key must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(repository::CREATOR));
        assert!(err.message().contains("remove"));
    }

    #[test]
    fn accepts_setting_read_only_keys_when_current_is_empty() {
        // CAS-from-zero path: `expected` was Hash::default(), so the
        // server passes an empty `current` Metadata. Every key in
        // proposed is being set for the first time and must be
        // allowed.
        let current = Metadata::new();
        let proposed = baseline();
        validate_read_only_fields(&current, &proposed)
            .expect("first-time write of read-only keys must be allowed");
    }

    #[test]
    fn ignores_read_only_keys_absent_from_both() {
        // No read-only key is present in either blob; the validator
        // must not invent rejections.
        let current = Metadata::new();
        let proposed = Metadata::new();
        validate_read_only_fields(&current, &proposed).expect("absence on both sides is a no-op");
    }

    #[test]
    fn type_mismatch_on_read_only_key_is_rejected() {
        // Same key, same logical value, different MetadataType: the
        // validator compares (bytes, type) and must catch this.
        let mut current = Metadata::new();
        current.set_string(repository::CREATED, "100").unwrap();
        let mut proposed = Metadata::new();
        proposed.set_u64(repository::CREATED, 100).unwrap();
        let err = validate_read_only_fields(&current, &proposed)
            .expect_err("type change on a read-only key must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(repository::CREATED));
    }
}
