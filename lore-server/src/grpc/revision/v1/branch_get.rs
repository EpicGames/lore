// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::revision::v1::BranchGetRequest;
use lore_proto::lore::revision::v1::BranchGetResponse;
use lore_proto::lore::revision::v1::branch_get_request::Query as BranchGetQuery;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::METADATA;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use super::branch_record::build_branch;
use crate::grpc::ServerResultExt;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::util::setup_execution;

/// `lore.revision.v1.RevisionService.BranchGet` handler.
///
/// Lookup by id resolves live or deleted branches; lookup by name
/// resolves live branches only — deleted-branch names are erased
/// and may have been recycled.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRevisionService`
#[tracing::instrument(name = "BranchGet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchGetRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
) -> Result<Response<BranchGetResponse>, Status> {
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().revision_branch_get
    {
        forward_branch_get(req, caller_context, forwarded_requests).await
    } else {
        branch_get_implementation(req, caller_context, immutable_store, mutable_store).await
    }
}

/// This `BranchGetRequest` should be handled by another server
/// and the response forwarded on to the client
async fn forward_branch_get(
    req: BranchGetRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<BranchGetResponse>, Status> {
    let mut client = forwarded_requests.forwarded_revision_service();
    let request = context.to_forwarded_request(req)?;

    let branch_get_result = client
        .branch_get(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = branch_get_result?;
    Ok(response)
}

/// This `BranchGetRequest` should be fulfilled by this server.
pub async fn branch_get_implementation(
    req: BranchGetRequest,
    caller_context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchGetResponse>, Status> {
    let Some(query) = req.query else {
        return Err(Status::invalid_argument(
            "BranchGetRequest.query must be set (id or name)",
        ));
    };

    let execution = setup_execution(
        module_path!(),
        caller_context.correlation_id,
        caller_context.user_id,
    );
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        caller_context.repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            match query {
                BranchGetQuery::Id(id) => {
                    let branch_id = BranchId::from(id);
                    debug!({BRANCH_ID} = %branch_id, "Get branch by id");
                    get_by_id(repository, branch_id).await
                }
                BranchGetQuery::Name(name) => {
                    debug!(name, "Get branch by name");
                    get_by_name(repository, &name).await
                }
            }
        })
        .await
}

async fn get_by_id(
    repository: Arc<RepositoryContext>,
    branch_id: BranchId,
) -> Result<Response<BranchGetResponse>, Status> {
    let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
        .await
        .map_err(|_err| Status::not_found(format!("Branch {branch_id} not found")))?;
    let metadata = branch::load_metadata(repository.clone(), metadata_hash)
        .await
        .warn_map_err(|err| Status::internal(err.to_string()))?;

    // Delete leaves metadata intact but clears the name → id mapping.
    let deleted = match branch::name(&metadata) {
        Ok(name) if !name.is_empty() => !branch::load_name_to_id_local(repository.clone(), name)
            .await
            .is_ok_and(|id| id == branch_id),
        _ => false,
    };

    let response_branch =
        build_branch(repository, branch_id, &metadata, metadata_hash, deleted).await?;
    debug!({BRANCH_ID} = %branch_id, {METADATA} = %metadata_hash, deleted, "Branch get by id response");
    Ok(Response::new(BranchGetResponse {
        branch: Some(response_branch),
    }))
}

async fn get_by_name(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> Result<Response<BranchGetResponse>, Status> {
    let branch_id_ctx = branch::load_name_to_id_local(repository.clone(), name)
        .await
        .map_err(|_err| Status::not_found(format!("Branch named '{name}' not found")))?;
    let branch_id = BranchId::from(branch_id_ctx);

    let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
        .await
        .map_err(|_err| Status::not_found(format!("Branch named '{name}' not found")))?;
    let metadata = branch::load_metadata(repository.clone(), metadata_hash)
        .await
        .warn_map_err(|err| Status::internal(err.to_string()))?;

    let response_branch =
        build_branch(repository, branch_id, &metadata, metadata_hash, false).await?;
    debug!({BRANCH_ID} = %branch_id, {METADATA} = %metadata_hash, "Branch get by name response");
    Ok(Response::new(BranchGetResponse {
        branch: Some(response_branch),
    }))
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchPoint;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::store::test_store_create;

    /// Returns the latest revision the test branch was forked at.
    async fn create_test_branch(
        repository_context: Arc<RepositoryContext>,
        branch: BranchId,
    ) -> Hash {
        let write_token = get_write_token();
        let main = lore_revision::branch::create(
            repository_context.clone(),
            &write_token,
            BranchId::from(uuid::Uuid::now_v7()),
            branch::DEFAULT_DEFAULT_NAME,
            branch::default_category(),
            "test-creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("Could not create main branch");

        let state = state::State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);
        let state_hash = state
            .serialize(repository_context.clone(), &write_token)
            .await
            .expect("Failed to serialize state");

        let latest = branch_push::push(
            repository_context.clone(),
            main,
            state_hash,
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push latest revision")
        .revision;

        lore_revision::branch::create(
            repository_context.clone(),
            &write_token,
            branch,
            "test-name",
            branch::personal_category(),
            "BranchCreator",
            12345,
            vec![BranchPoint {
                branch: main,
                revision: latest,
            }],
            false,
            false,
        )
        .await
        .expect("Could not create test branch");

        latest
    }

    fn make_request_id(repository: RepositoryId, branch: BranchId) -> Request<BranchGetRequest> {
        let mut request = Request::new(BranchGetRequest {
            query: Some(BranchGetQuery::Id(branch.into())),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    fn make_request_name(repository: RepositoryId, name: &str) -> Request<BranchGetRequest> {
        let mut request = Request::new(BranchGetRequest {
            query: Some(BranchGetQuery::Name(name.into())),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    mod direct_handling {
        use super::*;

        #[tokio::test]
        async fn get_by_id_returns_branch_record() {
            let repository = random::<RepositoryId>();
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                let latest = create_test_branch(repository_context, branch_id).await;

                let response = handler(
                    make_request_id(repository, branch_id),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                )
                .await
                .expect("Request failed");

                let branch = response
                    .into_inner()
                    .branch
                    .expect("response should include Branch");
                assert!(!branch.deleted);
                assert_eq!(branch.name, "test-name");
                assert_eq!(branch.creator, "BranchCreator");
                assert_eq!(branch.category, branch::personal_category());
                assert_eq!(branch.created, 12345);
                assert_eq!(branch.latest, bytes::Bytes::from(latest));
                assert!(!branch.metadata.is_empty());
                assert_eq!(branch.stack.len(), 1);
            }))
            .await;
        }

        #[tokio::test]
        async fn get_by_name_returns_branch_record() {
            let repository = random::<RepositoryId>();
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                let latest = create_test_branch(repository_context, branch_id).await;

                let response = handler(
                    make_request_name(repository, "test-name"),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                )
                .await
                .expect("Request failed");

                let branch = response
                    .into_inner()
                    .branch
                    .expect("response should include Branch");
                assert!(!branch.deleted);
                assert_eq!(branch.name, "test-name");
                assert_eq!(branch.latest, bytes::Bytes::from(latest));
            }))
            .await;
        }

        #[tokio::test]
        async fn get_by_id_returns_deleted_record_after_delete() {
            let repository = random::<RepositoryId>();
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                create_test_branch(repository_context.clone(), branch_id).await;
                branch::delete(repository_context, branch_id)
                    .await
                    .expect("delete should succeed");

                let response = handler(
                    make_request_id(repository, branch_id),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                )
                .await
                .expect("Request failed");

                let branch = response.into_inner().branch.expect("Branch present");
                assert!(branch.deleted);
                assert_eq!(branch.name, "test-name");
            }))
            .await;
        }

        #[tokio::test]
        async fn get_by_name_after_delete_returns_not_found() {
            let repository = random::<RepositoryId>();
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                create_test_branch(repository_context.clone(), branch_id).await;
                branch::delete(repository_context, branch_id)
                    .await
                    .expect("delete should succeed");

                let err = handler(
                    make_request_name(repository, "test-name"),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                )
                .await
                .expect_err("name lookup of deleted branch should fail");
                assert_eq!(err.code(), tonic::Code::NotFound);
            }))
            .await;
        }

        #[tokio::test]
        async fn get_unknown_id_returns_not_found() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let unknown = BranchId::from(uuid::Uuid::now_v7());
                let err = handler(
                    make_request_id(repository, unknown),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                )
                .await
                .expect_err("unknown id should fail");
                assert_eq!(err.code(), tonic::Code::NotFound);
            }))
            .await;
        }

        #[tokio::test]
        async fn get_unknown_name_returns_not_found() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let err = handler(
                    make_request_name(repository, "no-such-branch"),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                )
                .await
                .expect_err("unknown name should fail");
                assert_eq!(err.code(), tonic::Code::NotFound);
            }))
            .await;
        }

        #[tokio::test]
        async fn get_with_unset_query_returns_invalid_argument() {
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
                let mut request = Request::new(BranchGetRequest { query: None });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
                );

                let err = handler(
                    request,
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                )
                .await
                .expect_err("unset query should fail");
                assert_eq!(err.code(), tonic::Code::InvalidArgument);
            }))
            .await;
        }
    }

    mod forwarded_request {
        use std::sync::Mutex;

        use async_trait::async_trait;
        use tonic::Response;
        use tonic::Status;

        use super::*;
        use crate::grpc::forwarded_requests::ForwardedRequestResult;
        use crate::grpc::forwarded_requests::ForwardedRequests;
        use crate::grpc::forwarded_requests::InternalClientError;
        use crate::grpc::forwarded_requests::RpcFlags;
        use crate::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;

        /// Single-use client that returns a pre-configured result on its one call.
        struct SingleShotClient {
            response: Arc<Mutex<Option<ForwardedRequestResult<BranchGetResponse>>>>,
        }

        #[async_trait]
        impl ForwardedRevisionServiceClient for SingleShotClient {
            async fn branch_create(
                &mut self,
                _request: Request<lore_proto::lore::revision::v1::BranchCreateRequest>,
            ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchCreateResponse>
            {
                unreachable!("branch_create should not be called in branch_get tests")
            }

            async fn branch_delete(
                &mut self,
                _request: Request<lore_proto::lore::revision::v1::BranchDeleteRequest>,
            ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchDeleteResponse>
            {
                unreachable!("branch_delete should not be called in branch_get tests")
            }

            async fn branch_get(
                &mut self,
                _request: Request<BranchGetRequest>,
            ) -> ForwardedRequestResult<BranchGetResponse> {
                self.response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("branch_get called more than once")
            }
        }

        struct StubForwardedRequests {
            flags: RpcFlags,
            response: Arc<Mutex<Option<ForwardedRequestResult<BranchGetResponse>>>>,
        }

        impl StubForwardedRequests {
            fn forwarding_enabled(
                response: ForwardedRequestResult<BranchGetResponse>,
            ) -> Arc<Self> {
                Arc::new(Self {
                    flags: RpcFlags {
                        revision_branch_get: true,
                        ..Default::default()
                    },
                    response: Arc::new(Mutex::new(Some(response))),
                })
            }

            fn forwarding_disabled(
                response: ForwardedRequestResult<BranchGetResponse>,
            ) -> Arc<Self> {
                Arc::new(Self {
                    flags: RpcFlags {
                        revision_branch_get: false,
                        ..Default::default()
                    },
                    response: Arc::new(Mutex::new(Some(response))),
                })
            }
        }

        impl ForwardedRequests for StubForwardedRequests {
            fn rpc_flags(&self) -> &RpcFlags {
                &self.flags
            }

            fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
                Box::new(SingleShotClient {
                    response: Arc::clone(&self.response),
                })
            }
        }

        fn make_request(
            repository: RepositoryId,
            branch_id: BranchId,
        ) -> Request<BranchGetRequest> {
            let mut request = Request::new(BranchGetRequest {
                query: Some(BranchGetQuery::Id(branch_id.into())),
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            request
        }

        #[tokio::test]
        async fn delegates_to_remote_and_returns_response() {
            // When the flag is enabled the other server's response is returned directly;
            // branch_get_implementation is NOT called so the local store is not read.
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let branch = lore_proto::lore::model::v1::Branch {
                name: "test-name".into(),
                ..Default::default()
            };
            let branch_response = Ok(Ok(Response::new(BranchGetResponse {
                branch: Some(branch),
            })));
            let forwarded_requests = StubForwardedRequests::forwarding_enabled(branch_response);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());

                let response = handler(
                    make_request(repository, branch_id),
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                )
                .await
                .expect("should succeed");

                let branch = response
                    .into_inner()
                    .branch
                    .expect("response should include Branch");
                assert_eq!(branch.name, "test-name");
            }))
            .await;
        }

        #[tokio::test]
        async fn error_status_returned_to_caller() {
            // An error status from the forwarded server is forwarded directly to the original caller.
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let forwarded_request_result = Ok(Err(Status::not_found("test error forwarded")));
            let forwarded_requests =
                StubForwardedRequests::forwarding_enabled(forwarded_request_result);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());

                let err = handler(
                    make_request(repository, branch_id),
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                )
                .await
                .expect_err("forwarded error should propagate");

                assert_eq!(err.code(), tonic::Code::NotFound);
                assert!(err.message().contains("test error forwarded"));
            }))
            .await;
        }

        #[tokio::test]
        async fn internal_client_error_maps_to_internal_status() {
            // A transport-level failure (InternalClientError) is mapped to Status::internal.
            let repository = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let forwarded_requests = StubForwardedRequests::forwarding_enabled(Err(
                InternalClientError::internal("oops"),
            ));

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let branch_id = BranchId::from(uuid::Uuid::now_v7());

                let err = handler(
                    make_request(repository, branch_id),
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                )
                .await
                .expect_err("transport error should become internal status");

                assert_eq!(err.code(), tonic::Code::Internal);
                assert!(err.message().contains("Error making forwarded request"));
            }))
            .await;
        }

        #[tokio::test]
        async fn flag_disabled_falls_through_to_local_execution() {
            // When revision_branch_get is false the local path runs, even if a
            // ForwardedRequests is present. The stub client is not called.
            let repository = random::<RepositoryId>();
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            // response is irrelevant — client must never be called
            let forwarded_result = Ok(Err(Status::internal("should not be called")));
            let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository_context = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                create_test_branch(repository_context, branch_id).await;

                let response = handler(
                    make_request(repository, branch_id),
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                )
                .await
                .expect("local execution should succeed");

                let branch = response
                    .into_inner()
                    .branch
                    .expect("response should include Branch");
                assert_eq!(branch.name, "test-name");
                assert!(!branch.deleted);
            }))
            .await;
        }
    }
}
