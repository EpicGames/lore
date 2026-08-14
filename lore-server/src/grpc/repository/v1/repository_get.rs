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
use crate::grpc::extract_authorization_header;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::handlers::repository_query::check_repository_query_authorization;
use crate::util::setup_execution;

/// `lore.repository.v1.RepositoryService.RepositoryGet` handler.
///
/// Resolves a repository by id or by name and returns the full
/// `Repository` record. Honors auth when the environment configures an
/// auth-service URL. Self-heals stale or missing name → id mappings the
/// same way the legacy `RepositoryQuery` handler does.
#[tracing::instrument(name = "RepositoryGet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryGetRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryGetResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = extract_authorization_header(&request);
    let req = request.into_inner();

    let Some(query) = req.query else {
        return Err(Status::invalid_argument(
            "RepositoryGetRequest.query must be set (id or name)",
        ));
    };

    let execution = setup_execution(module_path!(), correlation_id, user_id);

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
