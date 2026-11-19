// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchGetRequest;
use lore_proto::BranchGetResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::no_link_traversal;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchGet::handle", skip_all)]
pub async fn handler(
    request: Request<BranchGetRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchGetResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let branch = BranchId::from(req.branch);

    debug!({BRANCH_ID} = %branch, "Handling branch get request");

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
        no_link_traversal(),
    ));
    LORE_CONTEXT
        .scope(execution, async move {
            branch_get_handler(repository, branch).await
        })
        .await
}

async fn branch_get_handler(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Response<BranchGetResponse>, Status> {
    let metadata = branch::metadata(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            warn!("Failed to get branch metadata: {err}");
            Status::not_found(err.to_string())
        })?;

    let branch = branch::branch_metadata(repository.clone(), branch, &metadata)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            warn!("Failed to resolve branch metadata: {err}");
            Status::not_found(err.to_string())
        })?;

    Ok(Response::new(BranchGetResponse {
        branch: Some(branch.into()),
    }))
}
