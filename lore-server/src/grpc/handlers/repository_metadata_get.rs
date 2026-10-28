// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::RepositoryMetadataGetRequest;
use lore_proto::RepositoryMetadataGetResponse;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::warn;

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
use crate::grpc::no_repository_access_status;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryMetadataGet::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryMetadataGetRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryMetadataGetResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let (_, extensions, req) = request.into_parts();

    let repository_id: Context = req.repository_id.into();
    if repository_id == Context::default() {
        return Err(Status::invalid_argument("Missing repository ID"));
    }

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id.into(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            authorizer
                .check_repository_access(
                    get_verified_token(&extensions).as_ref(),
                    repository_id.into(),
                    Action::Read,
                )
                .await
                .map_err(|_err| no_repository_access_status())?;

            let metadata_hash = repository::metadata_hash(repository)
                .await
                .filter_slow_down()?
                .map_err(|err| {
                    warn!(%err, "Failed to load repository metadata hash");
                    Status::not_found(err.to_string())
                })?;

            Ok(Response::new(RepositoryMetadataGetResponse {
                metadata_hash: metadata_hash.into(),
            }))
        })
        .await
}
