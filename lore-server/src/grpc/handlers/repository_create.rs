// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::RepositoryCreateRequest;
use lore_proto::RepositoryCreateResponse;
use lore_proto::rebac::CreateResourceRequest;
use lore_revision::branch;
use lore_revision::lore::RepositoryId;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_telemetry::InstrumentProvider;
use lore_transport::RepositoryData;
use tonic::Code;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Span;
use tracing::info;
use tracing::warn;

use super::repository_query::repository_query_id;
use super::repository_query::repository_query_name;
use crate::authnz::common::create_request_with_authorization;
use crate::authnz::rebac::RebacApiClient;
use crate::authnz::rebac::grpc_get_rebac_client;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::grpc::warn_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryCreate::handle", skip_all, fields(requested_repo_id))]
pub async fn handler(
    request: Request<RepositoryCreateRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
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

    let id: RepositoryId = Context::from(req.id).into();

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

            let default_branch_id = req.default_branch_id.into();
            let repository = repository_create(
                repository,
                req.name.as_str(),
                req.description.as_str(),
                default_branch_id,
                req.default_branch_name.as_str(),
                req.creator.as_str(),
                req.created,
                auth_url,
                authorization,
            )
            .await
            .inspect_err(|err| warn!(error = ?err, "Repository create failed"))?;

            hook_dispatcher.spawn_post(HookPoint::RepositoryCreate, hook_ctx);

            let num_repositories_created = instrument_provider.counter("num_repositories_created");
            num_repositories_created.add(1, &[]);

            Ok(Response::new(RepositoryCreateResponse {
                repository: Some(lore_proto::Repository {
                    id: repository.id.into(),
                    name: repository.name,
                    metadata: repository.metadata.into(),
                }),
            }))
        })
        .await
}

// Reject oversized string fields early to prevent resource exhaustion.
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

pub(crate) fn ensure_default_branch_name_matches(
    repository_id: RepositoryId,
    existing_default_branch_name: &str,
    requested_default_branch_name: &str,
) -> Result<(), Status> {
    if existing_default_branch_name == requested_default_branch_name {
        return Ok(());
    }

    Err(Status::already_exists(format!(
        "Repository {repository_id} already exist with default branch \
         {existing_default_branch_name} which does not match {requested_default_branch_name}"
    )))
}

#[allow(clippy::too_many_arguments)]
async fn repository_create(
    repository: Arc<RepositoryContext>,
    name: &str,
    description: &str,
    default_branch_id: Context,
    default_branch_name: &str,
    creator: &str,
    created: u64,
    auth_url: Option<String>,
    authorization: Option<String>,
) -> Result<RepositoryData, Status> {
    validate_create_input(name, description, default_branch_name, creator)?;

    if !repository::is_valid_name(name) {
        return Err(Status::invalid_argument("Invalid repository name"));
    }

    // If the name is an ID, make sure it matches the actual ID as we do not want
    // to alias IDs with mismatching names
    if let Ok(name_id) = Context::from_str(name)
        && !name_id.is_zero()
        && RepositoryId::from(name_id) != repository.id
    {
        return Err(Status::invalid_argument("Invalid repository name"));
    }

    // Check if a repository already exist. Skip authz check to also check repositories registered by others
    if let Ok(data) = repository_query_id(
        repository.clone(),
        repository.id,
        None, /* auth url */
        None, /* authorization */
    )
    .await
    {
        return if data.name == name {
            ensure_default_branch_name_matches(
                data.id,
                data.default_branch_name.as_str(),
                default_branch_name,
            )?;

            info!(
                "Repository {} already exist with name {}, early out create successful",
                repository.id, data.name
            );

            // Make sure name -> ID mapping exist
            if repository_query_name(
                repository.clone(),
                name,
                None, /* auth url */
                None, /* authorization */
            )
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

            Ok(data)
        } else {
            Err(Status::already_exists(format!(
                "Repository {} already exist with name {} which does not match {}",
                repository.id, data.name, name
            )))
        };
    }
    if let Ok(data) = repository_query_name(
        repository.clone(),
        name,
        None, /* auth url */
        None, /* authorization */
    )
    .await
    {
        return if data.id == repository.id {
            ensure_default_branch_name_matches(
                data.id,
                data.default_branch_name.as_str(),
                default_branch_name,
            )?;

            info!(
                "Repository {} already exist with id {}, early out create successful",
                name, data.id
            );
            Ok(data)
        } else {
            Err(Status::already_exists(format!(
                "Repository {} already exist with id {} which does not match {}",
                name, data.id, repository.id
            )))
        };
    }

    if let Some(auth_url) = auth_url {
        let client = Box::new(grpc_get_rebac_client(auth_url).await?);
        repository_create_auth_resource(client, authorization, repository.id, name).await?;
    }

    // Set up the repository metadata
    let metadata = RepositoryMetadata {
        name: name.to_string(),
        description: description.to_string(),
        default_branch: default_branch_id,
        default_branch_name: default_branch_name.to_string(),
        creator: if !creator.is_empty() {
            creator.to_string()
        } else {
            execution_context().user_id().await
        },
        created,
    };

    let metadata = repository::metadata_store(repository.clone(), metadata)
        .await
        .warn_map_err(|err| {
            Status::internal(format!("Failed to serialize repository metadata: {err}"))
        })?;

    let stack = vec![];

    // Create the default branch
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

    repository::metadata_store_hash(repository.clone(), metadata)
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

    Ok(RepositoryData {
        id: repository.id,
        name: name.to_string(),
        default_branch_name: default_branch_name.to_string(),
        metadata,
    })
}

pub(crate) async fn repository_create_auth_resource(
    mut client: Box<dyn RebacApiClient + Send + Sync>,
    authorization: Option<String>,
    repository_id: RepositoryId,
    name: &str,
) -> Result<(), Status> {
    info!(
        "Repository create auth resource for {} with name {}",
        repository_id, name
    );

    let request = create_request_with_authorization(
        CreateResourceRequest {
            resource_id: format!("urc-{repository_id}"),
            resource_name: String::from(name),
        },
        authorization,
    )?;

    match client.create_resource(request).await {
        Ok(_) => Ok(()),
        Err(err) if err.code() == Code::AlreadyExists => {
            info!(auth_error = ?err, requested_repo_id = %repository_id, "Auth resource for already exists, continuing");
            Ok(())
        }
        Err(err) if err.code() == Code::PermissionDenied => {
            info!(?err, "Create resource in auth failed - permission denied");
            Err(Status::permission_denied(
                "Failed to create repository, permission denied",
            ))
        }
        Err(err) if err.code() == Code::Unauthenticated => {
            info!(?err, "Create resource in auth failed - unauthenticated");
            Err(Status::unauthenticated(
                "Failed to create repository, reauthenticate",
            ))
        }
        Err(err) if err.code() == Code::NotFound => {
            // there is an issue with misbehaving clients who create external Auth resources but don't check
            // for a success response before calling RepositoryCreate (which in turn depends on those external
            // resources). Doing so results in Auth Service returning a NotFound error that should effectively be bubbled up
            // to the client
            info!(auth_error = ?err, "Repository Create create_resource failed because of Auth 'NotFound'");
            Err(Status::failed_precondition(
                "A required Auth entity was not found",
            ))
            // todo(plockhart): Once auth service supports Richer Error Model, change to look for an error code
        }
        Err(err)
            if err.code() == Code::InvalidArgument
                && err
                    .message()
                    .contains("Missing resource context in resourceName") =>
        {
            info!(auth_error = ?err, requested_name = name, "Repository Create create_resource failed - invalid name was provided");
            Err(Status::invalid_argument(
                "Invalid repository name - missing Organization context",
            ))
        }
        Err(err) => Err(warn_error_to_status(&err, |err| {
            Status::internal(format!("Failed to call auth create_resource: {err}"))
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    mod retry_idempotency {
        use super::*;

        #[test]
        fn accepts_matching_default_branch_name() {
            ensure_default_branch_name_matches(RepositoryId::default(), "main", "main")
                .expect("matching branch names should pass");
        }

        #[test]
        fn rejects_mismatched_default_branch_name() {
            let err = ensure_default_branch_name_matches(RepositoryId::default(), "main", "trunk")
                .expect_err("mismatched branch names should reject retry");

            assert_eq!(err.code(), Code::AlreadyExists);
            assert!(err.message().contains("default branch main"));
            assert!(err.message().contains("does not match trunk"));
        }
    }

    mod repository_create_auth_resource_tests {
        use lore_proto::rebac::CreateResourceResponse;
        use lore_proto::rebac::DeleteResourceRequest;
        use lore_proto::rebac::DeleteResourceResponse;

        use super::*;
        use crate::authnz::rebac::RebacApiResult;

        mockall::mock! {

            pub MockRebacApiClient {}

            #[async_trait::async_trait]
            impl RebacApiClient for MockRebacApiClient {
                async fn create_resource(
                    &mut self,
                    request: Request<CreateResourceRequest>,
                ) -> RebacApiResult<CreateResourceResponse>;

                async fn delete_resource(
                    &mut self,
                    request: Request<DeleteResourceRequest>,
                ) -> RebacApiResult<DeleteResourceResponse>;
            }
        }

        #[tokio::test]
        async fn permission_denied_propagated_to_client() {
            let repo_name = "2fc8bf934117e250152eba9a1fc78e71";
            let repository: RepositoryId = Context::from_str(repo_name)
                .expect("Failed to create repository")
                .into();

            let mut client = MockMockRebacApiClient::new();
            client
                .expect_create_resource()
                .return_once(|_| Err(Status::permission_denied("")));

            let error =
                repository_create_auth_resource(Box::new(client), None, repository, repo_name)
                    .await
                    .expect_err("Should have errored");
            assert_eq!(error.code(), Code::PermissionDenied);
            assert_eq!(
                error.message(),
                "Failed to create repository, permission denied"
            );
        }

        #[tokio::test]
        async fn missing_auth_dependencies_returns_failed_precondition() {
            let repo_name = "2fc8bf934117e250152eba9a1fc78e71";
            let repository: RepositoryId = Context::from_str(repo_name)
                .expect("Failed to create repository")
                .into();

            let mut client = MockMockRebacApiClient::new();
            client
                .expect_create_resource()
                .return_once(|_| Err(Status::not_found("")));

            let error =
                repository_create_auth_resource(Box::new(client), None, repository, repo_name)
                    .await
                    .expect_err("Should have errored");
            assert_eq!(error.code(), Code::FailedPrecondition);
            assert_eq!(error.message(), "A required Auth entity was not found");
        }

        #[tokio::test]
        async fn invalid_repository_name_returns_invalid_argument() {
            let repo_name = "2fc8bf934117e250152eba9a1fc78e71";
            let repository: RepositoryId = Context::from_str(repo_name)
                .expect("Failed to create repository")
                .into();

            let mut client = MockMockRebacApiClient::new();
            client.expect_create_resource().return_once(|_| {
                Err(Status::invalid_argument(
                    "Missing resource context in resourceName",
                ))
            });

            let error =
                repository_create_auth_resource(Box::new(client), None, repository, repo_name)
                    .await
                    .expect_err("Should have errored");
            assert_eq!(error.code(), Code::InvalidArgument);
            assert_eq!(
                error.message(),
                "Invalid repository name - missing Organization context"
            );
        }

        #[tokio::test]
        async fn already_exists_treated_as_success() {
            let repo_name = "2fc8bf934117e250152eba9a1fc78e71";
            let repository: RepositoryId = Context::from_str(repo_name)
                .expect("Failed to create repository")
                .into();

            let mut client = MockMockRebacApiClient::new();
            client
                .expect_create_resource()
                .return_once(|_| Err(Status::already_exists("")));

            repository_create_auth_resource(Box::new(client), None, repository, repo_name)
                .await
                .expect("AlreadyExists should be treated as success");
        }

        // the default case for errors that aren't specially handled
        #[tokio::test]
        async fn other_errors_return_internal_error() {
            let repo_name = "2fc8bf934117e250152eba9a1fc78e71";
            let repository: RepositoryId = Context::from_str(repo_name)
                .expect("Failed to create repository")
                .into();

            let mut client = MockMockRebacApiClient::new();
            client
                .expect_create_resource()
                .return_once(|_| Err(Status::invalid_argument("You used my api wrong!")));

            let error =
                repository_create_auth_resource(Box::new(client), None, repository, repo_name)
                    .await
                    .expect_err("Should have errored");
            assert_eq!(error.code(), Code::Internal);
            assert!(
                error
                    .message()
                    .contains("Failed to call auth create_resource"),
            );
            assert!(error.message().contains("You used my api wrong!"),);
        }
    }
}
