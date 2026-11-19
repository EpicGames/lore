// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::revision::v1::BranchMetadataGetRequest;
use lore_proto::lore::revision::v1::BranchMetadataGetResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::no_link_traversal;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::METADATA;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

/// `lore.revision.v1.RevisionService.BranchMetadataGet` handler.
///
/// Hash-only read of a branch's metadata pointer. Deleted branches
/// still resolve here — the metadata blob is preserved past delete and
/// is the canonical record of branch identity.
#[tracing::instrument(name = "BranchMetadataGet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchMetadataGetRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchMetadataGetResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let branch_id = BranchId::from(req.id);
    if branch_id == BranchId::default() {
        return Err(Status::invalid_argument("Branch id must be non-zero"));
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
            debug!({BRANCH_ID} = %branch_id, "Reading branch metadata pointer");

            let metadata_hash = branch::metadata_hash(repository, branch_id)
                .await
                .filter_slow_down()?
                .map_err(|err| {
                    info!({BRANCH_ID} = %branch_id, ?err, "Failed to load branch metadata pointer");
                    Status::not_found(format!("Branch {branch_id} not found"))
                })?;

            debug!(
                {BRANCH_ID} = %branch_id,
                {METADATA} = %metadata_hash,
                "Branch metadata get response",
            );

            Ok(Response::new(BranchMetadataGetResponse {
                metadata: metadata_hash.into(),
            }))
        })
        .await
}
