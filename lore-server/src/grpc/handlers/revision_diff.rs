// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::RevisionDiffRequest;
use lore_proto::RevisionDiffResponse;
use lore_revision::change;
use lore_revision::diff;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::collect_stream::collect_stream_with_summary;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::warn;

use super::path_diff::link_pin_path_diffs;
use super::path_diff::map_to_path_diff;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::util::setup_execution;

#[tracing::instrument(name = "RevisionDiff::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
) -> Result<Response<RevisionDiffResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let can_read = link_read_authorizer(&repository_authorizer, request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let revision_from = Hash::from(req.revision_from);
    let revision_to = Hash::from(req.revision_to);

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(%revision_from, %revision_to,
        "Handling revision diff",
    );

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
        can_read,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let state_source = State::deserialize(repository.clone(), revision_from)
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::invalid_argument("Invalid from state"))?;
            let state_target = State::deserialize(repository.clone(), revision_to)
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::invalid_argument("Invalid to state"))?;

            let pin_diffs =
                link_pin_path_diffs(&repository, &state_source, &state_target, repository_id)
                    .await?;

            let result = collect_stream_with_summary(|tx| {
                diff::diff_revision_paths(
                    repository.clone(),
                    state_source.clone(),
                    state_target.clone(),
                    None,
                    tx,
                )
            })
            .await;
            match result.filter_slow_down()? {
                Ok((_, mut changes)) => {
                    change::sort_by_path(&mut changes);
                    debug!("Found {} changes", changes.len());
                    let mut diffs = Vec::with_capacity(changes.len() + pin_diffs.len());
                    diffs.extend(pin_diffs);
                    for change in &changes {
                        if let Some(diff) = map_to_path_diff(change, repository_id).await {
                            diffs.push(diff);
                        }
                    }
                    Ok(Response::new(RevisionDiffResponse { diffs }))
                }
                Err(err) => {
                    warn!(?err, %revision_from, %revision_to,
                        "Failed to calculate diff",
                    );
                    Err(Status::internal(err.to_string()))
                }
            }
        })
        .await
}
