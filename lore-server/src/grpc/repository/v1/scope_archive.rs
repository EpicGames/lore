// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::repository::v1::ScopeArchiveRequest;
use lore_proto::lore::repository::v1::ScopeArchiveResponse;
use lore_revision::scope;
use lore_telemetry::InstrumentProvider;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::scope::Verb;
use super::scope::administer;
use super::scope::requested_scope_id;
use super::scope::scope_context;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::forwarded_requests::CallerContext;

/// `lore.repository.v1.RepositoryService.ScopeArchive` handler.
///
/// Archives the scope of the repository the request metadata names, as
/// `lore_revision::scope::archive` does: the scope's name mapping is removed, and its record stays.
/// An archived scope answers itself. Needs repository administration, or to be the scope's creator,
/// and refuses the default scope ([`administer`]).
#[tracing::instrument(name = "ScopeArchive::v1::handle", skip_all)]
pub async fn handler(
    request: Request<ScopeArchiveRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    instrument_provider: &(impl InstrumentProvider + Clone + Send + 'static),
) -> Result<Response<ScopeArchiveResponse>, Status> {
    let caller = CallerContext::from_original_request(&request)?;
    let (_, extensions, req) = request.into_parts();
    let id = requested_scope_id(&req.id)?;
    let (repository, execution) =
        scope_context(&caller, immutable_store, mutable_store, module_path!());

    LORE_CONTEXT
        .scope(execution, async move {
            let scope = administer(
                authorizer,
                &extensions,
                repository.clone(),
                id,
                &caller.user_id,
                Verb::Archive,
                instrument_provider.clone(),
                move || scope::archive(repository, id),
            )
            .await?;
            Ok(Response::new(ScopeArchiveResponse { scope: Some(scope) }))
        })
        .await
}
