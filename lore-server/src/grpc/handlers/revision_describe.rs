// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::RevisionDescribeRequest;
use lore_proto::RevisionDescribeResponse;
use lore_revision::branch::RevisionListItem;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::no_link_traversal;
use lore_telemetry::tracing::fields::REVISION;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "RevisionDescribe::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionDescribeRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RevisionDescribeResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let revision_id = Hash::from(req.id);

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!({REVISION} = %revision_id, "Handling revision describe");

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
        no_link_traversal(),
    ));
    LORE_CONTEXT
        .scope(execution, async move {
            let state = State::deserialize(repository.clone(), revision_id)
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::invalid_argument("Invalid revision state"))?;

            let metadata = Metadata::deserialize(repository.clone(), state.metadata_hash())
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::invalid_argument("Invalid revision metadata"))?;

            let parent_self_revision_number = if !state.parent_self().is_zero() {
                let parent_state = State::deserialize(repository.clone(), state.parent_self())
                    .await
                    .filter_slow_down()?
                    .map_err(|_err| Status::invalid_argument("Invalid parent revision state"))?;
                Some(parent_state.revision_number())
            } else {
                None
            };

            let parent_other_revision_number = if !state.parent_other().is_zero() {
                let parent_state = State::deserialize(repository.clone(), state.parent_other())
                    .await
                    .filter_slow_down()?
                    .map_err(|_err| {
                        Status::invalid_argument("Invalid parent other revision state")
                    })?;
                Some(parent_state.revision_number())
            } else {
                None
            };

            Ok(Response::new(RevisionDescribeResponse {
                revision: Some(lore_proto::Revision::from(&RevisionListItem {
                    revision: revision_id,
                    revision_number: state.revision_number(),
                    parent_self: state.parent_self(),
                    parent_other: state.parent_other(),
                    parent_self_revision_number,
                    parent_other_revision_number,
                    metadata,
                })),
            }))
        })
        .await
}
