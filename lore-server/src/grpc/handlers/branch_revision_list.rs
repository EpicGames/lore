// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::BranchRevisionListRequest;
use lore_proto::BranchRevisionListResponse;
use lore_revision::branch::list_revisions;
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

const REVISIONS_LIMIT: u32 = 100;

#[tracing::instrument(name = "BranchRevisionList::handle", skip_all)]
pub async fn handler(
    request: Request<BranchRevisionListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchRevisionListResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let source = req.source.map(Hash::from);
    let target = req.target.map(Hash::from);
    let branch = req.branch.map(BranchId::from);

    if branch.is_none() && source.is_none() {
        return Err(Status::invalid_argument(
            "branch is required when source is not provided",
        ));
    }

    let limit = req.limit.unwrap_or(REVISIONS_LIMIT).min(REVISIONS_LIMIT);

    debug!(
        {BRANCH_ID} = ?branch, limit, target_hash = ?target,
        "Handling branch revision list",
    );

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
        no_link_traversal(),
    ));
    LORE_CONTEXT
        .scope(execution, async move {
            list_revisions(repository, branch, Some(limit as usize), source, target)
                .await
                .filter_slow_down()?
                .map(|result| {
                    debug!("Found {} revisions", result.revisions.len());
                    Response::new(BranchRevisionListResponse {
                        revisions: result
                            .revisions
                            .iter()
                            .map(lore_proto::Revision::from)
                            .collect(),
                        has_more: result.has_more,
                    })
                })
                .map_err(|e| {
                    if e.is_branch_not_found() {
                        debug!("Failed to retrieve list of revisions for branch: {branch:?}");
                        Status::not_found("Branch does not exist")
                    } else if e.is_invalid_arguments() {
                        debug!("Branch is required when source is not provided");
                        Status::invalid_argument(e.to_string())
                    } else {
                        warn!(
                            {BRANCH_ID} = ?branch, error = ?e,
                            "Error retrieving the list of revisions"
                        );
                        Status::internal("Failed to retrieve the branch revision list")
                    }
                })
        })
        .await
}
