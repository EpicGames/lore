// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::RepositoryCreateRequest;
use lore_proto::RepositoryCreateResponse;
use lore_revision::branch;
use lore_revision::lore::RepositoryId;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_telemetry::InstrumentProvider;
use lore_transport::RepositoryData;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Span;
use tracing::info;
use tracing::warn;

use super::repository_query::repository_query_id;
use super::repository_query::repository_query_name;
use crate::authnz::repository_registry::RepositoryRegistry;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_authorization_header;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::grpc::none_or_status;
use crate::grpc::warn_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryCreate::handle", skip_all, fields(requested_repo_id))]
pub async fn handler(
    request: Request<RepositoryCreateRequest>,
    registry: &RepositoryRegistry,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<RepositoryCreateResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = extract_authorization_header(&request);
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
                .await
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
                registry,
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
#[lore_macro::test_pub]
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
async fn repository_create(
    repository: Arc<RepositoryContext>,
    name: &str,
    description: &str,
    default_branch_id: Context,
    default_branch_name: &str,
    creator: &str,
    created: u64,
    registry: &RepositoryRegistry,
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
        None, /* skip authz */
        None, /* token */
    )
    .await
    .filter_slow_down()?
    {
        return if data.name == name {
            info!(
                "Repository {} already exist with name {}, early out create successful",
                repository.id, data.name
            );

            // Make sure name -> ID mapping exist
            if repository_query_name(
                repository.clone(),
                name,
                None, /* skip authz */
                None, /* token */
            )
            .await
            .filter_slow_down()?
            .is_err()
            {
                info!(
                    "Recreating repository name {} -> ID {} mapping",
                    name, repository.id
                );
                // no filter_slow_down()? usage here: the create has already
                // succeeded, so this mapping repair must not fail it.
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
    // Name-collision guard: its absent path lets the create below rebind the
    // name, so an unreadable answer must not be read as absence.
    if let Some(data) = none_or_status(
        repository_query_name(
            repository.clone(),
            name,
            None, /* skip authz */
            None, /* token */
        )
        .await,
        |err| err.is_address_not_found() || err.is_repository_not_found(),
    )? {
        return if data.id == repository.id {
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

    registry.add(authorization, repository.id, name).await?;

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
        .filter_slow_down()?
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
    .filter_slow_down()?
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
        .filter_slow_down()?
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to store metadata hash for {name}/{}: {err}",
                repository.id
            ))
        })?;

    repository::store_name_to_id(repository.clone(), name, repository.id)
        .await
        .filter_slow_down()?
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
        metadata,
    })
}
