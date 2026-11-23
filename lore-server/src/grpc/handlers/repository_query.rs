// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;

use lore_base::error::RepositoryNotFound;
use lore_base::error::SlowDown;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_error_set::prelude::*;
use lore_proto::RepositoryQueryRequest;
use lore_proto::RepositoryQueryResponse;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryError;
use lore_revision::state::no_link_traversal;
use lore_transport::RepositoryData;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::authnz::repository_authorizer::AuthorizationOutcome;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryQuery::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryQueryRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryQueryResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let (_, extensions, req) = request.into_parts();

    let Some(query) = req.query else {
        return Err(Status::invalid_argument("Invalid query"));
    };

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        RepositoryId::default(),
        no_link_traversal(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let token = get_verified_token(&extensions);
            let repository = match query {
                lore_proto::repository_query_request::Query::Id(id) => {
                    let id: RepositoryId = Context::from(id).into();
                    repository_query_id(
                        repository.clone(),
                        id,
                        Some(authorizer.as_ref()),
                        token.as_ref(),
                    )
                    .await
                    .filter_slow_down()?
                    .map_err(|err| {
                        lookup_error_status(&err, || {
                            warn!("Repository ID {id} not known: {err}");
                            Status::not_found(format!("Repository {id} not found"))
                        })
                    })?
                }
                lore_proto::repository_query_request::Query::Name(name) => repository_query_name(
                    repository.clone(),
                    name.as_str(),
                    Some(authorizer.as_ref()),
                    token.as_ref(),
                )
                .await
                .filter_slow_down()?
                .map_err(|err| {
                    lookup_error_status(&err, || {
                        warn!("Repository name {name} not known: {err}");
                        Status::not_found(format!("Repository {name} not found"))
                    })
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

pub async fn repository_query_id(
    repository: Arc<RepositoryContext>,
    id: RepositoryId,
    authorizer: Option<&dyn RepositoryAuthorizer>,
    token: Option<&VerifiedToken<'_>>,
) -> Result<RepositoryData, RepositoryError> {
    if let Some(authorizer) = authorizer {
        check_repository_query_authorization(authorizer, token, id, id).await?;
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
            // no filter_slow_down()? usage here: repairing the name mapping is
            // best-effort, and the repository has already been resolved.
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
        metadata: metadata_hash,
    })
}

pub async fn repository_query_name(
    repository: Arc<RepositoryContext>,
    name: &str,
    authorizer: Option<&dyn RepositoryAuthorizer>,
    token: Option<&VerifiedToken<'_>>,
) -> Result<RepositoryData, RepositoryError> {
    // If the name is a parseable context ID, use the query-by-ID path directly
    if let Ok(id) = RepositoryId::from_str(name) {
        return repository_query_id(repository, id, authorizer, token).await;
    }

    let name_repository = Arc::new(repository.to_server_context(RepositoryId::default()));
    let id = repository::id_from_name(name_repository, name).await?;

    if let Some(authorizer) = authorizer {
        check_repository_query_authorization(authorizer, token, id, name).await?;
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
        metadata: metadata_hash,
    })
}

/// The repository-query lookup check, on the configured authorizer, for the
/// partition the caller named as `repository`.
///
/// A denial is `RepositoryNotFound` rather than `permission_denied`,
/// deliberately: answering the two cases apart would let an unauthorized
/// caller distinguish a partition that exists from one that does not. A check
/// that failed with `RESOURCE_EXHAUSTED` is `SlowDown`, which `filter_slow_down`
/// answers so the client retries. Any other failed check is `Internal`, which
/// [`lookup_error_status`] answers as `INTERNAL`. A lookup by name reaches this
/// check only for a name that exists, so while checks fail, a caller can tell
/// an existing name from a missing one.
pub(crate) async fn check_repository_query_authorization(
    authorizer: &dyn RepositoryAuthorizer,
    token: Option<&VerifiedToken<'_>>,
    repository_id: RepositoryId,
    repository: impl Display,
) -> Result<(), RepositoryError> {
    let Err(status) = authorizer
        .check_repository_lookup(token, repository_id)
        .await
    else {
        return Ok(());
    };
    match AuthorizationOutcome::of(&status) {
        AuthorizationOutcome::Denied => {
            debug!(%repository_id, "User authorization failed: {status}");
            Err(RepositoryNotFound {
                repository: repository.to_string(),
            }
            .into())
        }
        AuthorizationOutcome::Retryable => Err(SlowDown.into()),
        AuthorizationOutcome::Failed => Err(RepositoryError::internal_with_context(
            status,
            "checking repository authorization",
        )),
    }
}

/// The status a repository lookup that failed with `err` answers: `INTERNAL`
/// for an internal failure, such as an authorization check that could not be
/// made or a store fault, and `not_found` for every other failure.
pub(crate) fn lookup_error_status(
    err: &RepositoryError,
    not_found: impl FnOnce() -> Status,
) -> Status {
    if err.is_internal() {
        warn_error_to_status(err, |_| Status::internal("Repository lookup failed"))
    } else {
        not_found()
    }
}
