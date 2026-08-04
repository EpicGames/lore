// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::error::RepositoryNotFound;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_error_set::prelude::*;
use lore_proto::RepositoryQueryRequest;
use lore_proto::RepositoryQueryResponse;
use lore_proto::auth::CheckUserPermissionRequest;
use lore_revision::lore::RepositoryId;
use lore_revision::lore_debug;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryError;
use lore_transport::RepositoryData;
use tonic::Code;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::warn;

use crate::authnz::auth::grpc_get_auth_client;
use crate::authnz::common::create_request_with_authorization;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryQuery::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryQueryRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryQueryResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|s| s.to_string());
    let req = request.into_inner();

    let Some(query) = req.query else {
        return Err(Status::invalid_argument("Invalid query"));
    };

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        RepositoryId::default(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let repository = match query {
                lore_proto::repository_query_request::Query::Id(id) => {
                    let id: RepositoryId = Context::from(id).into();
                    repository_query_id(repository.clone(), id, auth_url, authorization)
                        .await
                        .map_err(|err| {
                            warn!("Repository ID {id} not known: {err}",);
                            Status::not_found(err.to_string())
                        })?
                }
                lore_proto::repository_query_request::Query::Name(name) => repository_query_name(
                    repository.clone(),
                    name.as_str(),
                    auth_url,
                    authorization,
                )
                .await
                .map_err(|err| {
                    warn!("Repository name {name} not known: {err}");
                    Status::not_found(err.to_string())
                })?,
            };
            Ok(Response::new(RepositoryQueryResponse {
                repository: Some(lore_proto::Repository {
                    id: repository.id.into(),
                    name: repository.name,
                    metadata: repository.metadata.into(),
                }),
            }))
        })
        .await
}

#[allow(clippy::map_err_ignore)]
pub async fn repository_query_id(
    repository: Arc<RepositoryContext>,
    id: RepositoryId,
    auth_url: Option<String>,
    authorization: Option<String>,
) -> Result<RepositoryData, RepositoryError> {
    if let Some(auth_url) = auth_url {
        check_repository_query_authorization(auth_url, authorization, id)
            .await
            .map_err(|status| {
                warn!("User authorization failed: {status}");
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

    // Verify the name -> ID mapping resolves back to the same ID, repair if missing
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
            let _ = repository::store_name_to_id(repository.clone(), &metadata.name, id)
                .await
                .inspect_err(|err| warn!("Failed to repair name -> ID mapping: {err}"));
        }
        Ok(_) => {}
    }

    info!("Repository query ID {id} found {metadata:?}");
    Ok(RepositoryData {
        id,
        name: metadata.name,
        default_branch_name: metadata.default_branch_name,
        metadata: metadata_hash,
    })
}

#[allow(clippy::map_err_ignore)]
pub async fn repository_query_name(
    repository: Arc<RepositoryContext>,
    name: &str,
    auth_url: Option<String>,
    authorization: Option<String>,
) -> Result<RepositoryData, RepositoryError> {
    // If the name is a parseable context ID, use the query-by-ID path directly
    if let Ok(id) = RepositoryId::from_str(name) {
        return repository_query_id(repository, id, auth_url, authorization).await;
    }

    let name_repository = Arc::new(repository.to_server_context(RepositoryId::default()));
    let id = repository::id_from_name(name_repository, name).await?;

    if let Some(auth_url) = auth_url {
        check_repository_query_authorization(auth_url, authorization, id)
            .await
            .map_err(|status| {
                warn!("User authorization failed: {status}");
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

    // Verify the metadata name matches the queried name — if not, the name -> ID mapping is stale
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

    info!("Repository query name {name} found {metadata:?}");
    Ok(RepositoryData {
        id,
        name: metadata.name,
        default_branch_name: metadata.default_branch_name,
        metadata: metadata_hash,
    })
}

pub(crate) async fn check_repository_query_authorization(
    auth_url: String,
    authorization: Option<String>,
    repository_id: RepositoryId,
) -> Result<(), Status> {
    lore_debug!("Repository query authorization check for {}", repository_id,);

    let mut client = grpc_get_auth_client(auth_url).await?;
    let resource_id = format!("urc-{repository_id}");
    let request = create_request_with_authorization(
        CheckUserPermissionRequest {
            resource_id: vec![resource_id.clone()],
            target_user: None,
        },
        authorization,
    )?;

    let permissions = client
        .check_user_permission(request)
        .await
        .warn_map_err(|err| {
            if err.code() == Code::PermissionDenied {
                return Status::permission_denied("Query resource denied");
            } else if err.code() == Code::Unauthenticated {
                return Status::unauthenticated("Query resource failed - unauthenticated");
            }
            Status::internal(format!("Failed to call auth check_user_permission: {err}"))
        })?;

    if permissions
        .into_inner()
        .allowed_resource_permission
        .first()
        .ok_or(Status::internal("No permissions for resource"))?
        .resource_id
        == resource_id
    {
        Ok(())
    } else {
        Err(Status::internal("Unexpected resource_id"))
    }
}
