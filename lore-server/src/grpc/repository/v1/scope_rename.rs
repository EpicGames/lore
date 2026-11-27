// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::repository::v1::ScopeRenameRequest;
use lore_proto::lore::repository::v1::ScopeRenameResponse;
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

/// `lore.repository.v1.RepositoryService.ScopeRename` handler.
///
/// Renames the live scope of the repository the request metadata names, as
/// `lore_revision::scope::rename` does: the name mapping and the name in the scope's metadata both
/// change. An archived scope is refused. Needs repository administration, or to be the scope's
/// creator, and refuses the default scope ([`administer`]).
#[tracing::instrument(name = "ScopeRename::v1::handle", skip_all)]
pub async fn handler(
    request: Request<ScopeRenameRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    instrument_provider: &(impl InstrumentProvider + Clone + Send + 'static),
) -> Result<Response<ScopeRenameResponse>, Status> {
    let caller = CallerContext::from_original_request(&request)?;
    let (_, extensions, req) = request.into_parts();
    let id = requested_scope_id(&req.id)?;
    let name = req.name;
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
                Verb::Rename,
                instrument_provider.clone(),
                move || async move { scope::rename(repository, id, &name).await },
            )
            .await?;
            Ok(Response::new(ScopeRenameResponse { scope: Some(scope) }))
        })
        .await
}
