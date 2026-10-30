// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::grpc::handlers::repository_create::*;
use tonic::Code;

mod input_length_validation {
    use lore_revision::repository;

    use super::*;

    #[test]
    fn accepts_valid_input() {
        validate_create_input("my-repo", "a description", "main", "alice")
            .expect("valid input should pass");
    }

    #[test]
    fn accepts_name_at_max_length() {
        let name = "a".repeat(repository::MAX_NAME_LEN);
        validate_create_input(&name, "desc", "main", "alice")
            .expect("name at exactly MAX_NAME_LEN should pass");
    }

    #[test]
    fn rejects_oversized_repository_name() {
        let long_name = "a".repeat(repository::MAX_NAME_LEN + 1);
        let err = validate_create_input(&long_name, "desc", "main", "alice")
            .expect_err("should reject oversized name");
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(
            err.message()
                .contains("Repository name exceeds maximum length")
        );
    }

    #[test]
    fn rejects_oversized_description() {
        let long_desc = "a".repeat(repository::MAX_DESCRIPTION_LEN + 1);
        let err = validate_create_input("my-repo", &long_desc, "main", "alice")
            .expect_err("should reject oversized description");
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("description exceeds maximum length"));
    }

    #[test]
    fn rejects_oversized_branch_name() {
        let long_branch = "a".repeat(repository::MAX_NAME_LEN + 1);
        let err = validate_create_input("my-repo", "desc", &long_branch, "alice")
            .expect_err("should reject oversized branch name");
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Branch name exceeds maximum length"));
    }

    #[test]
    fn rejects_oversized_creator() {
        let long_creator = "a".repeat(repository::MAX_NAME_LEN + 1);
        let err = validate_create_input("my-repo", "desc", "main", &long_creator)
            .expect_err("should reject oversized creator");
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Creator exceeds maximum length"));
    }
}

mod zero_repository_id {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_proto::RepositoryCreateRequest;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_server::authnz::repository_registry::RepositoryRegistry;
    use lore_server::grpc::handlers::repository_create::*;
    use lore_server::hooks::HookDispatcher;
    use tonic::Request;

    use crate::store::test_support::test_store_create;

    struct TestInstrumentProvider;

    impl lore_telemetry::InstrumentProvider for TestInstrumentProvider {
        fn namespace(&self) -> &'static str {
            "test"
        }
    }

    /// The all-zero id names the namespace-root partition: the create is
    /// refused and no repository metadata is stored there.
    #[tokio::test]
    async fn is_refused_and_stores_nothing() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let request = Request::new(RepositoryCreateRequest {
                id: bytes::Bytes::from_static(&[0u8; 16]),
                name: "claimed-name".into(),
                default_branch_id: bytes::Bytes::from(lore_base::types::Context::from(
                    uuid::Uuid::now_v7(),
                )),
                default_branch_name: "main".into(),
                ..Default::default()
            });
            let status = handler(
                request,
                &RepositoryRegistry::Unregistered,
                immutable_store.clone(),
                mutable_store.clone(),
                &HookDispatcher::empty(),
                &TestInstrumentProvider,
            )
            .await
            .expect_err("the zero id must be refused");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);

            let namespace_root = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                RepositoryId::default(),
            ));
            let err = repository::metadata_hash(namespace_root)
                .await
                .expect_err("no repository metadata may be stored");
            assert!(err.is_address_not_found(), "unexpected error: {err:?}");
        }))
        .await;
    }
}
