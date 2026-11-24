// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::BranchDiffRequest;
use lore_proto::BranchDiffResponse;
use lore_proto::PathDiff;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterCancelledExt;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::handlers::path_diff::link_pin_path_diffs;
use crate::grpc::handlers::path_diff::map_to_conflict;
use crate::grpc::handlers::path_diff::map_to_path_diff;
use crate::grpc::link_read_authorizer;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchDiff::handle", skip_all)]
pub async fn handler(
    request: Request<BranchDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
) -> Result<Response<BranchDiffResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let can_read = link_read_authorizer(&repository_authorizer, request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner().clone();
    let branch_source = BranchId::from(req.branch_source);
    let branch_target = BranchId::from(req.branch_target);
    let revision_source = req.revision_source.map(Hash::from);
    let revision_target = req.revision_target.map(Hash::from);
    let auto_resolve = req.autoresolve;

    info!(
        "Handling branch diff in repository {repository_id} source: {branch_source} target {branch_target}"
    );

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
        can_read,
    ));
    LORE_CONTEXT
        .scope(execution, async move {
            branch_diff_handler(
                repository,
                branch_source,
                revision_source,
                branch_target,
                revision_target,
                auto_resolve,
            )
            .await
        })
        .await
}

/// Loads the two states a pin comparison needs. A failure fails the diff for
/// the same reason [`link_pin_path_diffs`] does.
async fn link_pin_diffs(
    repository: &Arc<RepositoryContext>,
    from: Hash,
    to: Hash,
    parent_repository_id: RepositoryId,
) -> Result<Vec<PathDiff>, Status> {
    if from.is_zero() || to.is_zero() || from == to {
        return Ok(Vec::new());
    }
    let states = async {
        let state_from = State::deserialize(repository.clone(), from).await?;
        let state_to = State::deserialize(repository.clone(), to).await?;
        Ok::<_, StateError>((state_from, state_to))
    }
    .await
    .filter_slow_down()?;
    let (state_from, state_to) = states.map_err(|err| {
        warn!(
            {REPOSITORY_ID} = %repository.id, %from, %to, ?err,
            "Failed to load states for link pin comparison",
        );
        Status::internal(err.to_string())
    })?;
    link_pin_path_diffs(repository, &state_from, &state_to, parent_repository_id).await
}

async fn branch_diff_handler(
    repository: Arc<RepositoryContext>,
    branch_source: BranchId,
    revision_source: Option<Hash>,
    branch_target: BranchId,
    revision_target: Option<Hash>,
    auto_resolve: bool,
) -> Result<Response<BranchDiffResponse>, Status> {
    let metadata = branch::metadata(repository.clone(), branch_source)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            warn!("Failed to get source branch metadata: {branch_source}");
            Status::not_found(err.to_string())
        })?;
    let source = branch::branch_metadata(repository.clone(), branch_source, &metadata)
        .await
        .filter_slow_down()?
        .map_err(|e| {
            warn!("Failed to resolve source branch: {branch_source}");
            Status::not_found(e.to_string())
        })?;

    let metadata = branch::metadata(repository.clone(), branch_target)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            warn!("Failed to get target branch metadata: {branch_target}");
            Status::not_found(err.to_string())
        })?;
    let target = branch::branch_metadata(repository.clone(), branch_target, &metadata)
        .await
        .filter_slow_down()?
        .map_err(|e| {
            warn!("Failed to resolve target branch: {branch_target}");
            Status::not_found(e.to_string())
        })?;

    let repository_id = repository.id;
    let link_repository = repository.clone();
    let result = branch::diff3_collect(
        repository,
        branch_source,
        revision_source.unwrap_or(source.latest),
        branch_target,
        revision_target.unwrap_or(target.latest),
        None,  /* No path */
        false, /* Do not include identical changes */
        auto_resolve,
    )
    .await;
    match result.filter_slow_down()?.filter_cancelled()? {
        Ok(result) => {
            debug!("Found {} changes", result.changes.len());
            // The changes are base -> source; compare the registries over the
            // same pair.
            let pin_diffs =
                link_pin_diffs(&link_repository, result.base, result.source, repository_id).await?;

            let mut diffs = Vec::with_capacity(result.changes.len() + pin_diffs.len());
            diffs.extend(pin_diffs);
            for change in &result.changes {
                if let Some(diff) = map_to_path_diff(change, repository_id).await {
                    diffs.push(diff);
                }
            }
            let mut conflicts = Vec::with_capacity(result.conflicts.len());
            for conflict in &result.conflicts {
                if let Some(conflict) = map_to_conflict(conflict, repository_id).await {
                    conflicts.push(conflict);
                }
            }
            Ok(Response::new(BranchDiffResponse {
                diffs,
                conflicts,
                branch_source: Some(source.into()),
                branch_target: Some(target.into()),
                revision_source: result.source.into(),
                revision_target: result.target.into(),
                revision_base: result.base.into(),
            }))
        }
        Err(err) => {
            warn!({REPOSITORY_ID} = %repository_id, %branch_source, %branch_target, ?err, "Failed to calculate diff");
            if err.is_divergent() || err.is_invalid_arguments() {
                Err(Status::invalid_argument(err.to_string()))
            } else if err.is_max_history_search_depth() {
                Err(Status::resource_exhausted(err.to_string()))
            } else {
                Err(Status::internal(err.to_string()))
            }
        }
    }
}
