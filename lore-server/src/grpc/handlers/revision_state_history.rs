// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::min;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::RevisionStateHistoryRequest;
use lore_proto::RevisionStateHistoryResponse;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::no_link_traversal;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "RevisionStateHistory::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionStateHistoryRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RevisionStateHistoryResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let request = request.into_inner();
    let mut revision = Hash::from(request.revision);
    // if we encounter a revision state not found in the history when we expect to find it,
    // for debugging purposes it is good to know the last good state
    let mut previous_revision = Hash::default();

    // Cap the request depth to avoid clients triggering very long-running operations on server
    let mut depth = min(request.depth as usize, 100);
    let with_metadata = request.with_metadata;
    let follow_merge = request.follow_merge;

    info!(
        base_revision = %revision,
        depth,
        with_metadata,
        follow_merge,
        "Handling revision state history"
    );

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
        no_link_traversal(),
    ));
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let mut base_revision = true;
            let mut response = RevisionStateHistoryResponse::default();
            while depth > 0 {
                if !base_revision {
                    trace!("Revision {}", revision);
                    response.signature.push(revision.into());
                }

                let state = {
                    match state::State::deserialize(repository.clone(), revision)
                        .await
                        .filter_slow_down()?
                    {
                        Ok(state) => state,
                        Err(ref e) if e.is_not_found() => {
                            if base_revision {
                                return Err(Status::not_found("Base revision not found"));
                            }
                            warn!(
                                %revision,
                                %previous_revision,
                                "Parent revision state not found",
                            );
                            return Err(Status::internal(
                                "Failed reading state data from immutable store".to_string(),
                            ));
                        }
                        Err(err) => {
                            warn!(
                                ?err,
                                %revision,
                                %previous_revision,
                                "Failed to deserialize revision state",
                            );
                            return Err(Status::internal(err.to_string()));
                        }
                    }
                };

                if with_metadata && !base_revision {
                    trace!("Metadata {}", state.metadata_hash());
                    response.metadata.push(state.metadata_hash().into());
                }

                if revision.is_zero() {
                    break;
                }

                previous_revision = revision;
                revision = state.parent_self();

                if follow_merge && !state.parent_other().is_zero() {
                    return Err(Status::unimplemented(
                        "Revision state history follow merge not implemented",
                    ));
                }

                base_revision = false;
                depth -= 1;
            }

            Ok(Response::new(response))
        })
        .await
}
