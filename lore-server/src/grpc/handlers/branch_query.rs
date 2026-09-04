// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::BranchQueryRequest;
use lore_proto::BranchQueryResponse;
use lore_revision::branch;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::METADATA;
use lore_telemetry::tracing::fields::REVISION;
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

// Branch existence model (consistent with branch::create in urc-core/src/branch.rs):
//
// Query by name:
//   1. Check name→ID mapping. Not found → does not exist.
//   2. Check ID→metadata for the mapped ID. Not found → stale mapping, does not exist.
//   3. Load metadata, check metadata.name matches queried name. Mismatch → does not exist.
//
// Query by ID:
//   1. Check ID→metadata. Not found → does not exist.
//   2. Load metadata, look up name→ID(metadata.name).
//      - Not found or different ID → exists with deleted flag.
//      - Matches → exists.

#[tracing::instrument(name = "BranchQuery::handle", skip_all)]
pub async fn handler(
    request: Request<BranchQueryRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchQueryResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let Some(query) = req.query else {
        return Err(Status::invalid_argument("Invalid query"));
    };

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            match query {
                lore_proto::branch_query_request::Query::Name(name) => {
                    debug!(name, "Handling branch query request - name");
                    query_by_name(repository, &name).await
                }
                lore_proto::branch_query_request::Query::Id(id) => {
                    let id = Context::from(id);
                    debug!({BRANCH_ID} = %id, "Handling branch query request - ID");
                    query_by_id(repository, id).await
                }
            }
        })
        .await
}

async fn query_by_name(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> Result<Response<BranchQueryResponse>, Status> {
    // Step 1: name→ID mapping
    let branch = branch::load_name_to_id_local(repository.clone(), name)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            debug!(name, error = ?err, "Branch name not found");
            Status::not_found("Branch does not exist")
        })?;

    // Step 2: ID→metadata
    let metadata_hash = branch::metadata_hash(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            info!({BRANCH_ID} = %branch, error = ?err, "Stale name mapping, metadata missing");
            Status::not_found("Branch does not exist")
        })?;

    // Step 3: Load metadata and verify name matches
    let branch_metadata = branch::load_metadata(repository.clone(), metadata_hash)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            info!({BRANCH_ID} = %branch, error = ?err, "Failed to load branch metadata");
            Status::not_found("Branch does not exist")
        })?;

    let metadata_name = branch::name(&branch_metadata).unwrap_or_default();
    if metadata_name != name {
        info!({BRANCH_ID} = %branch, metadata_name, name, "Stale name mapping, metadata name mismatch");
        return Err(Status::not_found("Branch does not exist"));
    }

    let revision = branch::load_latest(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .unwrap_or_default();

    debug!({BRANCH_ID} = %branch, {REVISION} = %revision, {METADATA} = %metadata_hash, "Branch query by name response");
    Ok(Response::new(BranchQueryResponse {
        id: branch.into(),
        revision: revision.into(),
        metadata: metadata_hash.into(),
        deleted: false,
    }))
}

async fn query_by_id(
    repository: Arc<RepositoryContext>,
    branch: Context,
) -> Result<Response<BranchQueryResponse>, Status> {
    // Step 1: ID→metadata
    let metadata_hash = branch::metadata_hash(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            info!({BRANCH_ID} = %branch, error = ?err, "Branch metadata not found");
            Status::not_found("Branch does not exist")
        })?;

    // Step 2: Load metadata
    let branch_metadata = branch::load_metadata(repository.clone(), metadata_hash)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            info!({BRANCH_ID} = %branch, error = ?err, "Failed to load branch metadata");
            Status::not_found("Branch does not exist")
        })?;

    let revision = branch::load_latest(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .unwrap_or_default();

    // Step 3: Check name→ID(metadata.name) to determine deleted status
    let deleted = if let Ok(metadata_name) = branch::name(&branch_metadata)
        && !metadata_name.is_empty()
    {
        let name_maps_to_us = branch::load_name_to_id_local(repository.clone(), metadata_name)
            .await
            .filter_slow_down()?
            .is_ok_and(|id| id == branch);
        if !name_maps_to_us {
            info!({BRANCH_ID} = %branch, metadata_name, "Branch deleted (name mapping missing or points elsewhere)");
        }
        !name_maps_to_us
    } else {
        false
    };

    debug!({BRANCH_ID} = %branch, {REVISION} = %revision, {METADATA} = %metadata_hash, deleted, "Branch query by ID response");
    Ok(Response::new(BranchQueryResponse {
        id: branch.into(),
        revision: revision.into(),
        metadata: metadata_hash.into(),
        deleted,
    }))
}
