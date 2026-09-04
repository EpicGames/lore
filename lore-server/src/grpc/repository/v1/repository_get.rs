// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::error::RepositoryNotFound;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_proto::lore::repository::v1::RepositoryGetRequest;
use lore_proto::lore::repository::v1::RepositoryGetResponse;
use lore_proto::lore::repository::v1::repository_get_request::Query;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::RepositoryMetadata;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;
use tracing::warn;

use super::record::build_repository;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_authorization_header;
use crate::grpc::extract_correlation_id;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::get_user_id;
use crate::grpc::handlers::repository_query::check_repository_query_authorization;
use crate::util::setup_execution;

/// `lore.repository.v1.RepositoryService.RepositoryGet` handler.
///
/// Resolves a repository by id or by name and returns the full
/// `Repository` record. Honors auth when the environment configures an
/// auth-service URL. Self-heals stale or missing name → id mappings the
/// same way the legacy `RepositoryQuery` handler does.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRepositoryService`
#[tracing::instrument(name = "RepositoryGet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryGetRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
) -> Result<Response<RepositoryGetResponse>, Status> {
    let caller_context = CallerContext {
        repository_id: RepositoryId::default(), // the queried repository is named by the request
        user_id: get_user_id(request.extensions()),
        correlation_id: extract_correlation_id(&request).unwrap_or_default(),
        authorization: extract_authorization_header(&request),
    };
    let req = request.into_inner();

    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().repository_get
    {
        forward_repository_get(req, caller_context, forwarded_requests).await
    } else {
        repository_get_implementation(
            req,
            caller_context,
            auth_url,
            immutable_store,
            mutable_store,
        )
        .await
    }
}

/// This `RepositoryGetRequest` should be handled by another server
/// and the response forwarded on to the client
async fn forward_repository_get(
    req: RepositoryGetRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<RepositoryGetResponse>, Status> {
    let mut client = forwarded_requests.forwarded_repository_service();
    let request = context.to_forwarded_request(req)?;

    let repository_get_result = client
        .repository_get(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = repository_get_result?;
    Ok(response)
}

/// This `RepositoryGetRequest` should be fulfilled by this server.
pub async fn repository_get_implementation(
    req: RepositoryGetRequest,
    caller_context: CallerContext,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryGetResponse>, Status> {
    let Some(query) = req.query else {
        return Err(Status::invalid_argument(
            "RepositoryGetRequest.query must be set (id or name)",
        ));
    };

    let authorization = caller_context.authorization;
    let execution = setup_execution(
        module_path!(),
        caller_context.correlation_id,
        caller_context.user_id,
    );

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        RepositoryId::default(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let (id, metadata, metadata_hash) = match query {
                Query::Id(id) => {
                    let id: RepositoryId = Context::from(id).into();
                    debug!(%id, "Get repository by id");
                    let (metadata, metadata_hash) =
                        repository_load_id(repository.clone(), id, auth_url, authorization)
                            .await
                            .filter_slow_down()?
                            .map_err(|_err| {
                                Status::not_found(format!("Repository {id} not found"))
                            })?;
                    (id, metadata, metadata_hash)
                }
                Query::Name(name) => {
                    debug!(name, "Get repository by name");
                    let (id, metadata, metadata_hash) = repository_load_name(
                        repository.clone(),
                        name.as_str(),
                        auth_url,
                        authorization,
                    )
                    .await
                    .filter_slow_down()?
                    .map_err(|_err| Status::not_found(format!("Repository {name} not found")))?;
                    (id, metadata, metadata_hash)
                }
            };
            debug!(%id, "Repository get response");
            Ok(Response::new(RepositoryGetResponse {
                repository: Some(build_repository(id, &metadata, metadata_hash)),
            }))
        })
        .await
}

/// Resolve a repository by id, returning its metadata blob plus the
/// metadata pointer hash. Performs the same authz check + name-mapping
/// repair the legacy v0 handler does.
#[allow(clippy::map_err_ignore)]
pub(super) async fn repository_load_id(
    repository: Arc<RepositoryContext>,
    id: RepositoryId,
    auth_url: Option<String>,
    authorization: Option<String>,
) -> Result<(RepositoryMetadata, Hash), RepositoryError> {
    if let Some(auth_url) = auth_url {
        check_repository_query_authorization(auth_url, authorization, id)
            .await
            .map_err(|status| {
                debug!(%id, "User authorization failed: {status}");
                RepositoryError::from(RepositoryNotFound {
                    repository: id.to_string(),
                })
            })?;
    }

    let repository = Arc::new(repository.to_server_context(id));
    let metadata_hash = repository::metadata_hash(repository.clone())
        .await
        .forward_with::<RepositoryError, _>(|| {
            format!("Repository {id} metadata hash not found")
        })?;
    let metadata = repository::metadata(repository.clone(), metadata_hash)
        .await
        .forward_with::<RepositoryError, _>(|| format!("Repository {id} metadata not found"))?;

    let name_repository = Arc::new(repository.to_server_context(RepositoryId::default()));
    match repository::id_from_name(name_repository, &metadata.name).await {
        Ok(resolved_id) if resolved_id != id => {
            warn!(
                "Repository {} name {} maps to different repository {}, returning not found",
                id, metadata.name, resolved_id
            );
            return Err(RepositoryError::from(RepositoryNotFound {
                repository: id.to_string(),
            }));
        }
        Err(_) => {
            info!(
                "Repairing missing name -> ID mapping: {} -> {}",
                metadata.name, id
            );
            // no filter_slow_down()? usage here: repairing the name mapping is
            // best-effort, and the repository has already been resolved.
            let _ = repository::store_name_to_id(repository.clone(), &metadata.name, id)
                .await
                .inspect_err(|err| warn!("Failed to repair name -> ID mapping: {err}"));
        }
        Ok(_) => {}
    }

    Ok((metadata, metadata_hash))
}

/// Resolve a repository by name. Falls through to id lookup when the
/// caller passed a parseable `RepositoryId` as the name. Self-heals a
/// stale name → id mapping by deleting it when the metadata's name
/// disagrees.
#[allow(clippy::map_err_ignore)]
pub(super) async fn repository_load_name(
    repository: Arc<RepositoryContext>,
    name: &str,
    auth_url: Option<String>,
    authorization: Option<String>,
) -> Result<(RepositoryId, RepositoryMetadata, Hash), RepositoryError> {
    if let Ok(id) = RepositoryId::from_str(name) {
        let (metadata, metadata_hash) =
            repository_load_id(repository, id, auth_url, authorization).await?;
        return Ok((id, metadata, metadata_hash));
    }

    let name_repository = Arc::new(repository.to_server_context(RepositoryId::default()));
    let id = repository::id_from_name(name_repository, name).await?;

    if let Some(auth_url) = auth_url {
        check_repository_query_authorization(auth_url, authorization, id)
            .await
            .map_err(|status| {
                debug!(%id, "User authorization failed: {status}");
                RepositoryError::from(RepositoryNotFound {
                    repository: name.to_string(),
                })
            })?;
    }

    let repository = Arc::new(repository.to_server_context(id));
    let metadata_hash = repository::metadata_hash(repository.clone())
        .await
        .forward_with::<RepositoryError, _>(|| {
            format!("Repository {name} metadata hash not found")
        })?;
    let metadata = repository::metadata(repository.clone(), metadata_hash)
        .await
        .forward_with::<RepositoryError, _>(|| format!("Repository {name} metadata not found"))?;

    if metadata.name != name {
        warn!(
            "Stale name -> ID mapping: {} maps to {} but metadata name is {}, deleting mapping",
            name, id, metadata.name
        );
        let _ = repository::delete_name_to_id(repository.clone(), name)
            .await
            .inspect_err(|err| warn!("Failed to delete stale name -> ID mapping: {err}"));
        return Err(RepositoryError::from(RepositoryNotFound {
            repository: name.to_string(),
        }));
    }

    Ok((id, metadata, metadata_hash))
}

#[cfg(test)]
mod tests {
    mod forwarded_request {
        use std::sync::Mutex;

        use async_trait::async_trait;
        use lore_proto::lore::repository::v1::RepositoryCreateRequest;
        use lore_proto::lore::repository::v1::RepositoryCreateResponse;
        use rand::random;

        use super::super::*;
        use crate::grpc::forwarded_requests::ForwardedRequestResult;
        use crate::grpc::forwarded_requests::InternalClientError;
        use crate::grpc::forwarded_requests::RpcFlags;
        use crate::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
        use crate::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
        use crate::store::test_store_create;

        /// Single-use client that returns a pre-configured result on its one call.
        struct SingleShotClient {
            response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryGetResponse>>>>,
        }

        #[async_trait]
        impl ForwardedRepositoryServiceClient for SingleShotClient {
            async fn repository_create(
                &mut self,
                _request: Request<RepositoryCreateRequest>,
            ) -> ForwardedRequestResult<RepositoryCreateResponse> {
                unreachable!("repository_create should not be called in repository_get tests")
            }

            async fn repository_get(
                &mut self,
                _request: Request<RepositoryGetRequest>,
            ) -> ForwardedRequestResult<RepositoryGetResponse> {
                self.response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("repository_get called more than once")
            }
        }

        struct StubForwardedRequests {
            flags: RpcFlags,
            response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryGetResponse>>>>,
        }

        impl StubForwardedRequests {
            fn new(
                repository_get: bool,
                response: ForwardedRequestResult<RepositoryGetResponse>,
            ) -> Arc<Self> {
                Arc::new(Self {
                    flags: RpcFlags {
                        repository_get,
                        ..Default::default()
                    },
                    response: Arc::new(Mutex::new(Some(response))),
                })
            }

            fn forwarding_enabled(
                response: ForwardedRequestResult<RepositoryGetResponse>,
            ) -> Arc<Self> {
                Self::new(true, response)
            }

            fn forwarding_disabled(
                response: ForwardedRequestResult<RepositoryGetResponse>,
            ) -> Arc<Self> {
                Self::new(false, response)
            }
        }

        impl ForwardedRequests for StubForwardedRequests {
            fn rpc_flags(&self) -> &RpcFlags {
                &self.flags
            }

            fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
                unreachable!(
                    "forwarded_revision_service should not be called in repository_get tests"
                )
            }

            fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient> {
                Box::new(SingleShotClient {
                    response: Arc::clone(&self.response),
                })
            }
        }

        fn make_request(name: &str) -> Request<RepositoryGetRequest> {
            Request::new(RepositoryGetRequest {
                query: Some(Query::Name(name.into())),
            })
        }

        /// Writes the metadata blob, its pointer and the name → id mapping so a
        /// local lookup of `name` resolves.
        async fn seed_repository(
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
            let metadata_hash = repository::metadata_store(
                repository.clone(),
                RepositoryMetadata {
                    name: name.to_string(),
                    creator: "alice".into(),
                    ..Default::default()
                },
            )
            .await
            .expect("Failed to store repository metadata");
            repository::metadata_store_hash(repository.clone(), metadata_hash)
                .await
                .expect("Failed to store repository metadata hash");
            repository::store_name_to_id(repository, name, id)
                .await
                .expect("Failed to store repository name to id mapping");
        }

        #[tokio::test]
        async fn delegates_to_remote_and_returns_response() {
            // When the flag is enabled the other server's response is returned directly;
            // repository_get_implementation is NOT called so the local store is not read.
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let repo_record = lore_proto::lore::model::v1::Repository {
                name: "test-repo".into(),
                ..Default::default()
            };
            let repo_response = Ok(Ok(Response::new(RepositoryGetResponse {
                repository: Some(repo_record),
            })));
            let forwarded_requests = StubForwardedRequests::forwarding_enabled(repo_response);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let response = handler(
                    make_request("test-repo"),
                    None, /* no auth */
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                )
                .await
                .expect("should succeed");

                let repository = response
                    .into_inner()
                    .repository
                    .expect("response should include Repository");
                assert_eq!(repository.name, "test-repo");
            }))
            .await;
        }

        #[tokio::test]
        async fn error_status_returned_to_caller() {
            // An error status from the forwarded server is forwarded directly to the original caller.
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let forwarded_request_result = Ok(Err(Status::not_found("test error forwarded")));
            let forwarded_requests =
                StubForwardedRequests::forwarding_enabled(forwarded_request_result);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let err = handler(
                    make_request("test-repo"),
                    None,
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
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let forwarded_requests = StubForwardedRequests::forwarding_enabled(Err(
                InternalClientError::internal("oops"),
            ));

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let err = handler(
                    make_request("test-repo"),
                    None,
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
            // When repository_get is false the local path runs, even if a
            // ForwardedRequests is present. The stub client is not called.
            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            // response is irrelevant — client must never be called
            let forwarded_result = Ok(Err(Status::internal("should not be called")));
            let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                seed_repository(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    id,
                    "my-repo",
                )
                .await;

                let response = handler(
                    make_request("my-repo"),
                    None, /* no auth */
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                )
                .await
                .expect("local execution should succeed");

                let repository = response
                    .into_inner()
                    .repository
                    .expect("response should include Repository");
                assert_eq!(repository.name, "my-repo");
                assert_eq!(repository.id, bytes::Bytes::from(id));
            }))
            .await;
        }
    }
}
