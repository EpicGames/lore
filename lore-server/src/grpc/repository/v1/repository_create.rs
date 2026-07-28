// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1::RepositoryCreateRequest;
use lore_proto::lore::repository::v1::RepositoryCreateResponse;
use lore_revision::branch;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_telemetry::InstrumentProvider;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Span;
use tracing::info;
use tracing::warn;

use super::record::build_repository;
use super::repository_get::repository_load_id;
use super::repository_get::repository_load_name;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::handlers::repository_create::repository_create_auth_resource;
use crate::grpc::hook_error_to_status;
use crate::grpc::warn_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

/// `lore.repository.v1.RepositoryService.RepositoryCreate` handler.
///
/// The caller pre-generates `id` and `default_branch_id` for retry
/// idempotency. The server assigns `created`. `creator` is hybrid:
/// caller-set if permitted, otherwise the authenticated JWT identity.
///
/// Depending on server configuration, this request may get completely delegated to another server
/// via `ForwardedRepositoryService`
#[tracing::instrument(
    name = "RepositoryCreate::v1::handle",
    skip_all,
    fields(requested_repo_id)
)]
pub async fn handler(
    request: Request<RepositoryCreateRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    forwarded_requests: &Option<Arc<dyn ForwardedRequests>>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<RepositoryCreateResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|s| s.to_string());
    let req = request.into_inner();
    let caller_context = CallerContext {
        repository_id: RepositoryId::default(), // RepositoryCreate has no pre-existing repository
        user_id,
        correlation_id,
        authorization,
    };

    if let Some(forwarded_requests) = forwarded_requests
        && forwarded_requests.rpc_flags().repository_create
    {
        forward_repository_create(req, caller_context, forwarded_requests).await
    } else {
        repository_create_implementation(
            req,
            caller_context,
            auth_url,
            immutable_store,
            mutable_store,
            hook_dispatcher,
            instrument_provider,
        )
        .await
    }
}

/// This `RepositoryCreateRequest` should be handled by another server
/// and the response forwarded on to the client
async fn forward_repository_create(
    req: RepositoryCreateRequest,
    context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<RepositoryCreateResponse>, Status> {
    let mut client = forwarded_requests.forwarded_repository_service();
    let request = context.to_forwarded_request(req)?;

    let repository_create_result = client
        .repository_create(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = repository_create_result?;
    Ok(response)
}

/// This `RepositoryCreateRequest` should be fulfilled by this server.
pub async fn repository_create_implementation(
    req: RepositoryCreateRequest,
    caller_context: CallerContext,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<RepositoryCreateResponse>, Status> {
    let user_id = caller_context.user_id;
    let correlation_id = caller_context.correlation_id;
    let authorization = caller_context.authorization;

    let id: RepositoryId = Context::from(req.id).into();
    let name = req.name;
    let description = req.description;
    let default_branch_id: Context = req.default_branch_id.into();
    let default_branch_name = req.default_branch_name;
    let creator = req
        .creator
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| user_id.clone());

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        id,
    ));
    Span::current().record("requested_repo_id", id.to_string());

    LORE_CONTEXT
        .scope(execution, async move {
            let hook_ctx = HookContext::builder()
                .correlation_id(correlation_id)
                .hook_point(HookPoint::RepositoryCreate)
                .repository(id)
                .user(user_id)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::RepositoryCreate, &hook_ctx)
                .map_err(hook_error_to_status)?;

            let (created_metadata, metadata_hash) = repository_create_inner(
                repository.clone(),
                &name,
                &description,
                default_branch_id,
                &default_branch_name,
                &creator,
                created,
                auth_url,
                authorization,
            )
            .await
            .inspect_err(|err| warn!(error = ?err, "Repository create failed"))?;

            hook_dispatcher.spawn_post(HookPoint::RepositoryCreate, hook_ctx);

            instrument_provider
                .counter("num_repositories_created")
                .add(1, &[]);

            Ok(Response::new(RepositoryCreateResponse {
                repository: Some(build_repository(id, &created_metadata, metadata_hash)),
            }))
        })
        .await
}

/// Reject oversized string fields early to prevent resource exhaustion.
fn validate_create_input(
    name: &str,
    description: &str,
    default_branch_name: &str,
    creator: &str,
) -> Result<(), Status> {
    if name.len() > repository::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Repository name exceeds maximum length of {} bytes",
            repository::MAX_NAME_LEN,
        )));
    }
    if description.len() > repository::MAX_DESCRIPTION_LEN {
        return Err(Status::invalid_argument(format!(
            "Repository description exceeds maximum length of {} bytes",
            repository::MAX_DESCRIPTION_LEN,
        )));
    }
    if default_branch_name.len() > repository::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Branch name exceeds maximum length of {} bytes",
            repository::MAX_NAME_LEN,
        )));
    }
    if creator.len() > repository::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Creator exceeds maximum length of {} bytes",
            repository::MAX_NAME_LEN,
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn repository_create_inner(
    repository: Arc<RepositoryContext>,
    name: &str,
    description: &str,
    default_branch_id: Context,
    default_branch_name: &str,
    creator: &str,
    created: u64,
    auth_url: Option<String>,
    authorization: Option<String>,
) -> Result<(RepositoryMetadata, lore_storage::Hash), Status> {
    validate_create_input(name, description, default_branch_name, creator)?;

    if !repository::is_valid_name(name) {
        return Err(Status::invalid_argument("Invalid repository name"));
    }

    if let Ok(name_id) = Context::from_str(name)
        && !name_id.is_zero()
        && RepositoryId::from(name_id) != repository.id
    {
        return Err(Status::invalid_argument("Invalid repository name"));
    }

    if let Ok((metadata, metadata_hash)) =
        repository_load_id(repository.clone(), repository.id, None, None).await
    {
        return if metadata.name == name {
            info!(
                "Repository {} already exist with name {}, early out create successful",
                repository.id, metadata.name
            );

            if repository_load_name(repository.clone(), name, None, None)
                .await
                .is_err()
            {
                info!(
                    "Recreating repository name {} -> ID {} mapping",
                    name, repository.id
                );
                let _ = repository::store_name_to_id(repository.clone(), name, repository.id)
                    .await
                    .inspect_err(|err| info!("Recreate name -> ID mapping failed: {err}"));
            }

            Ok((metadata, metadata_hash))
        } else {
            Err(Status::already_exists(format!(
                "Repository {} already exist with name {} which does not match {}",
                repository.id, metadata.name, name
            )))
        };
    }
    if let Ok((id, metadata, metadata_hash)) =
        repository_load_name(repository.clone(), name, None, None).await
    {
        return if id == repository.id {
            info!(
                "Repository {} already exist with id {}, early out create successful",
                name, id
            );
            Ok((metadata, metadata_hash))
        } else {
            Err(Status::already_exists(format!(
                "Repository {} already exist with id {} which does not match {}",
                name, id, repository.id
            )))
        };
    }

    if let Some(auth_url) = auth_url {
        let client = Box::new(crate::authnz::rebac::grpc_get_rebac_client(auth_url).await?);
        repository_create_auth_resource(client, authorization, repository.id, name).await?;
    }

    let metadata = RepositoryMetadata {
        name: name.to_string(),
        description: description.to_string(),
        default_branch: default_branch_id,
        default_branch_name: default_branch_name.to_string(),
        creator: creator.to_string(),
        created,
    };

    let metadata_hash = repository::metadata_store(repository.clone(), metadata.clone())
        .await
        .warn_map_err(|err| {
            Status::internal(format!("Failed to serialize repository metadata: {err}"))
        })?;

    let stack = vec![];
    let write_token = get_write_token();
    match branch::create(
        repository.clone(),
        &write_token,
        default_branch_id,
        default_branch_name,
        branch::default_category(),
        creator,
        created,
        stack,
        false,
        false,
    )
    .await
    {
        Ok(_) => {}
        Err(err) if err.is_branch_already_exists() => {}
        Err(err) => {
            let response = warn_error_to_status(&err, |err| {
                Status::internal(format!("Failed to create default branch: {err}"))
            });
            return Err(response);
        }
    }

    repository::metadata_store_hash(repository.clone(), metadata_hash)
        .await
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to store metadata hash for {name}/{}: {err}",
                repository.id
            ))
        })?;

    repository::store_name_to_id(repository.clone(), name, repository.id)
        .await
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to store name to ID lookup for {name} -> {}: {err}",
                repository.id
            ))
        })?;

    info!("Created repository {} with ID {}", name, repository.id);

    Ok((metadata, metadata_hash))
}

#[cfg(test)]
mod tests {
    mod input_length_validation {
        use lore_revision::repository;

        use super::super::*;

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
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
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
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("description exceeds maximum length"));
        }

        #[test]
        fn rejects_oversized_branch_name() {
            let long_branch = "a".repeat(repository::MAX_NAME_LEN + 1);
            let err = validate_create_input("my-repo", "desc", &long_branch, "alice")
                .expect_err("should reject oversized branch name");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("Branch name exceeds maximum length"));
        }

        #[test]
        fn rejects_oversized_creator() {
            let long_creator = "a".repeat(repository::MAX_NAME_LEN + 1);
            let err = validate_create_input("my-repo", "desc", "main", &long_creator)
                .expect_err("should reject oversized creator");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("Creator exceeds maximum length"));
        }
    }

    mod forwarded_request {
        use std::sync::Arc;
        use std::sync::Mutex;

        use async_trait::async_trait;
        use lore_base::runtime::LORE_CONTEXT;
        use lore_proto::lore::repository::v1::RepositoryCreateRequest;
        use lore_proto::lore::repository::v1::RepositoryCreateResponse;
        use lore_revision::lore::RepositoryId;
        use rand::random;
        use tonic::Request;
        use tonic::Response;
        use tonic::Status;

        use super::super::*;
        use crate::grpc::forwarded_requests::ForwardedRequestResult;
        use crate::grpc::forwarded_requests::ForwardedRequests;
        use crate::grpc::forwarded_requests::InternalClientError;
        use crate::grpc::forwarded_requests::RpcFlags;
        use crate::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
        use crate::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
        use crate::hooks::HookDispatcher;
        use crate::store::test_store_create;

        struct TestInstrumentProvider;

        impl lore_telemetry::InstrumentProvider for TestInstrumentProvider {
            fn namespace(&self) -> &'static str {
                "test"
            }
        }

        /// Single-use client that returns a pre-configured result on its one call.
        struct SingleShotClient {
            response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryCreateResponse>>>>,
        }

        #[async_trait]
        impl ForwardedRepositoryServiceClient for SingleShotClient {
            async fn repository_create(
                &mut self,
                _request: Request<RepositoryCreateRequest>,
            ) -> ForwardedRequestResult<RepositoryCreateResponse> {
                self.response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("repository_create called more than once")
            }
        }

        struct StubForwardedRequests {
            flags: RpcFlags,
            response: Arc<Mutex<Option<ForwardedRequestResult<RepositoryCreateResponse>>>>,
        }

        impl StubForwardedRequests {
            fn forwarding_enabled(
                response: ForwardedRequestResult<RepositoryCreateResponse>,
            ) -> Arc<Self> {
                Arc::new(Self {
                    flags: RpcFlags {
                        repository_create: true,
                        ..Default::default()
                    },
                    response: Arc::new(Mutex::new(Some(response))),
                })
            }

            fn forwarding_disabled(
                response: ForwardedRequestResult<RepositoryCreateResponse>,
            ) -> Arc<Self> {
                Arc::new(Self {
                    flags: RpcFlags {
                        repository_create: false,
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
                unreachable!(
                    "forwarded_revision_service should not be called in repository_create tests"
                )
            }

            fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient> {
                Box::new(SingleShotClient {
                    response: Arc::clone(&self.response),
                })
            }
        }

        fn make_request(
            repository_id: RepositoryId,
            name: &str,
        ) -> Request<RepositoryCreateRequest> {
            let id_bytes: lore_base::types::Context = repository_id.into();
            Request::new(RepositoryCreateRequest {
                id: bytes::Bytes::from(id_bytes),
                name: name.into(),
                description: String::new(),
                default_branch_id: bytes::Bytes::from(lore_base::types::Context::from(
                    uuid::Uuid::now_v7(),
                )),
                default_branch_name: "main".into(),
                creator: Some("alice".into()),
            })
        }

        #[tokio::test]
        async fn delegates_to_remote_and_returns_response() {
            // When the flag is enabled the other server's response is returned directly;
            // repository_create_implementation is NOT called so the local store stays empty.
            let repository_id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let repo_record = lore_proto::lore::model::v1::Repository {
                name: "test-repo".into(),
                ..Default::default()
            };
            let repo_response = Ok(Ok(Response::new(RepositoryCreateResponse {
                repository: Some(repo_record),
            })));
            let forwarded_requests = StubForwardedRequests::forwarding_enabled(repo_response);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let hook_dispatcher = HookDispatcher::empty();

                let response = handler(
                    make_request(repository_id, "test-repo"),
                    None, /* no auth */
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                    &hook_dispatcher,
                    &TestInstrumentProvider,
                )
                .await
                .expect("should succeed");

                let repo = response
                    .into_inner()
                    .repository
                    .expect("response should include Repository");
                assert_eq!(repo.name, "test-repo");
            }))
            .await;
        }

        #[tokio::test]
        async fn error_status_returned_to_caller() {
            // An error status from the forwarded server is forwarded directly to the original caller.
            let repository_id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let forwarded_request_result = Ok(Err(Status::already_exists("test error forwarded")));
            let forwarded_requests =
                StubForwardedRequests::forwarding_enabled(forwarded_request_result);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let hook_dispatcher = HookDispatcher::empty();

                let err = handler(
                    make_request(repository_id, "test-repo"),
                    None,
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                    &hook_dispatcher,
                    &TestInstrumentProvider,
                )
                .await
                .expect_err("forwarded error should propagate");

                assert_eq!(err.code(), tonic::Code::AlreadyExists);
                assert!(err.message().contains("test error forwarded"));
            }))
            .await;
        }

        #[tokio::test]
        async fn internal_client_error_maps_to_internal_status() {
            // A transport-level failure (InternalClientError) is mapped to Status::internal.
            let repository_id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            let forwarded_requests = StubForwardedRequests::forwarding_enabled(Err(
                InternalClientError::internal("oops"),
            ));

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let hook_dispatcher = HookDispatcher::empty();

                let err = handler(
                    make_request(repository_id, "test-repo"),
                    None,
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                    &hook_dispatcher,
                    &TestInstrumentProvider,
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
            // When repository_create is false the local path runs, even if a
            // ForwardedRequests is present. The stub client is not called.
            let repository_id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            // response is irrelevant — client must never be called
            let forwarded_result = Ok(Err(Status::internal("should not be called")));
            let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let hook_dispatcher = HookDispatcher::empty();

                let response = handler(
                    make_request(repository_id, "my-repo"),
                    None, /* no auth */
                    immutable_store,
                    mutable_store,
                    &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                    &hook_dispatcher,
                    &TestInstrumentProvider,
                )
                .await
                .expect("local execution should succeed");

                let repo = response
                    .into_inner()
                    .repository
                    .expect("response should include Repository");
                assert_eq!(repo.name, "my-repo");
            }))
            .await;
        }
    }
}
