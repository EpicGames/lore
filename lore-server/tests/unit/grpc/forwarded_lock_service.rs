// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod query_handler {
    use lore_base::types::LockData;
    use lore_base::types::LockResource;
    use lore_proto::lock::QueryRequest;
    use lore_revision::lock::LockQuery;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_server::grpc::forwarded_lock_service::query_handler;
    use lore_server::grpc::forwarded_requests::CallerContext;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Code;
    use tonic::Request;

    use crate::grpc::lock_service::store::MockMockLockStore;

    fn forwarded(repository: RepositoryId, body: QueryRequest) -> Request<QueryRequest> {
        CallerContext {
            repository_id: repository,
            user_id: "alice".into(),
            is_service_account: false,
            correlation_id: String::new(),
            authorization: None,
        }
        .to_forwarded_request(body)
        .expect("CallerContext::to_forwarded_request failed in test")
    }

    #[tokio::test]
    async fn missing_user_id_returns_internal_error() {
        let repository = random::<RepositoryId>();
        let mut request = Request::new(QueryRequest::default());
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );

        let err = query_handler(request, &MockMockLockStore::new())
            .await
            .expect_err("a request without the caller should fail");

        assert_eq!(err.code(), Code::Internal);
        assert!(err.message().contains("on-behalf-of-user-id"));
    }

    /// The repository comes from the forwarded metadata, which the origin
    /// stamped from the partition it authorized the caller for.
    #[tokio::test]
    async fn queries_the_repository_named_in_the_forwarded_metadata() {
        let repository = random::<RepositoryId>();
        let branch = random::<BranchId>();
        let mut lock_store = MockMockLockStore::new();
        lock_store
            .expect_query_locks()
            .withf(move |query| {
                *query == LockQuery::OwnerRepositoryBranch("bob".into(), repository, branch)
            })
            .return_once(|_| {
                Ok(vec![LockData {
                    resource: LockResource::default(),
                    owner: "bob".into(),
                    locked_at: 0,
                }])
            });

        let response = query_handler(
            forwarded(
                repository,
                QueryRequest {
                    branch: Some(branch.into()),
                    owner: Some("bob".into()),
                    description: None,
                },
            ),
            &lock_store,
        )
        .await
        .expect("forwarded query should succeed");

        let owners: Vec<String> = response
            .into_inner()
            .result
            .into_iter()
            .map(|lock| lock.owner)
            .collect();
        assert_eq!(owners, vec!["bob".to_string()]);
    }

    #[tokio::test]
    async fn unsupported_combination_is_invalid_argument() {
        let repository = random::<RepositoryId>();

        let err = query_handler(
            forwarded(
                repository,
                QueryRequest {
                    branch: None,
                    owner: Some("bob".into()),
                    description: Some("a/file.uasset".into()),
                },
            ),
            &MockMockLockStore::new(),
        )
        .await
        .expect_err("owner with description is not a supported query");

        assert_eq!(err.code(), Code::InvalidArgument);
    }
}
