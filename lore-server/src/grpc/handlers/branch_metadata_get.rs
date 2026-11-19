// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchMetadataGetRequest;
use lore_proto::BranchMetadataGetResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::no_link_traversal;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchMetadataGet::handle", skip_all)]
pub async fn handler(
    request: Request<BranchMetadataGetRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchMetadataGetResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let branch = BranchId::from(req.branch_id);
    if branch == BranchId::default() {
        return Err(Status::invalid_argument("Missing branch ID"));
    }

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
        no_link_traversal(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let metadata_hash = branch::metadata_hash(repository, branch)
                .await
                .filter_slow_down()?
                .map_err(|err| {
                    warn!(%err, "Failed to load branch metadata hash");
                    Status::not_found(err.to_string())
                })?;

            Ok(Response::new(BranchMetadataGetResponse {
                metadata_hash: metadata_hash.into(),
            }))
        })
        .await
}
