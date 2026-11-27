// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::repository::v1::ScopeGetRequest;
use lore_proto::lore::repository::v1::ScopeGetResponse;
use lore_proto::lore::repository::v1::scope_get_request::Query;
use lore_revision::scope;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::scope::build_scope;
use super::scope::requested_scope_id;
use super::scope::scope_context;
use super::scope::scope_status;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::require_action;

/// `lore.repository.v1.RepositoryService.ScopeGet` handler.
///
/// Answers a scope of the repository the request metadata names, by id, archived or not, or by
/// the name of a live scope, regardless of case. The default scope's id and the name `default`
/// answer the default scope with no store read. Needs read access.
#[tracing::instrument(name = "ScopeGet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<ScopeGetRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<ScopeGetResponse>, Status> {
    require_action(authorizer, &request, Action::Read).await?;
    let caller = CallerContext::from_original_request(&request)?;
    let query = request
        .into_inner()
        .query
        .ok_or_else(|| Status::invalid_argument("Missing scope id or name"))?;
    let (repository, execution) =
        scope_context(&caller, immutable_store, mutable_store, module_path!());

    LORE_CONTEXT
        .scope(execution, async move {
            let scope = match query {
                Query::Id(id) => {
                    let id = requested_scope_id(&id)?;
                    scope::get(repository, id)
                        .await
                        .map_err(scope_status)?
                        .ok_or_else(|| Status::not_found(format!("Scope {id} not found")))?
                }
                Query::Name(name) => scope::get_by_name(repository, &name)
                    .await
                    .map_err(scope_status)?
                    .ok_or_else(|| Status::not_found(format!("Scope {name} not found")))?,
            };
            Ok(Response::new(ScopeGetResponse {
                scope: Some(build_scope(scope)),
            }))
        })
        .await
}
