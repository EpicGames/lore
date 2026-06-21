// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchUnprotectRequest;
use lore_proto::BranchUnprotectResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::warn;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::require_write_permission;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchUnprotect::handle", skip_all)]
pub async fn handler(
    request: Request<BranchUnprotectRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchUnprotectResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    require_write_permission(request.extensions(), repository_id)?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let branch = BranchId::from(req.branch);

    info!("Handling branch unprotect in repository {repository_id}: branch {branch}");

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            match branch::unprotect(repository, branch).await {
                Ok(_) => {
                    info!("Branch unprotected in repository {repository_id}: branch {branch}");
                    Ok(Response::new(BranchUnprotectResponse {}))
                }
                Err(err) => {
                    warn!("Failed to unprotect branch {branch} in repository {repository_id}");
                    Err(Status::internal(err.to_string()))
                }
            }
        })
        .await
}
