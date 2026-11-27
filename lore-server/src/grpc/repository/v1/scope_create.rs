// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::repository::v1::ScopeCreateRequest;
use lore_proto::lore::repository::v1::ScopeCreateResponse;
use lore_revision::scope;
use lore_revision::scope::Refused;
use lore_telemetry::InstrumentProvider;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::scope::Verb;
use super::scope::build_scope;
use super::scope::count;
use super::scope::refused_status;
use super::scope::requested_scope_id;
use super::scope::scope_context;
use super::scope::scope_status;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::require_action;
use crate::grpc::run_detached;

/// `lore.repository.v1.RepositoryService.ScopeCreate` handler.
///
/// Creates the scope in the repository the request metadata names, as
/// `lore_revision::scope::create` does, attributed to the caller and created now, and a create of
/// an archived scope's id restores the scope under the requested name, as
/// `lore_revision::scope::restore` does. Needs write access. The writes run to their end in a task
/// of their own ([`run_detached`]), which counts the one that succeeds.
#[tracing::instrument(name = "ScopeCreate::v1::handle", skip_all)]
pub async fn handler(
    request: Request<ScopeCreateRequest>,
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    instrument_provider: &(impl InstrumentProvider + Clone + Send + 'static),
) -> Result<Response<ScopeCreateResponse>, Status> {
    require_action(authorizer, &request, Action::Write).await?;
    let caller = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    let id = requested_scope_id(&req.id)?;
    let name = req.name;
    let (repository, execution) =
        scope_context(&caller, immutable_store, mutable_store, module_path!());
    let created = lore_revision::util::time::timestamp();

    LORE_CONTEXT
        .scope(execution, async move {
            let written = run_detached("scope create", {
                let instrument_provider = instrument_provider.clone();
                async move {
                    let (verb, written) = match scope::create(
                        repository.clone(),
                        id,
                        &name,
                        &caller.user_id,
                        created,
                    )
                    .await
                    {
                        Ok(Err(Refused::Archived)) => {
                            (Verb::Restore, scope::restore(repository, id, &name).await)
                        }
                        written => (Verb::Create, written),
                    };
                    if matches!(written, Ok(Ok(_))) {
                        count(&instrument_provider, verb);
                    }
                    written
                }
            })
            .await?
            .map_err(scope_status)?;
            let scope = written.map(build_scope).map_err(refused_status)?;
            Ok(Response::new(ScopeCreateResponse { scope: Some(scope) }))
        })
        .await
}
