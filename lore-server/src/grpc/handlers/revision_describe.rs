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
    ));
    LORE_CONTEXT
        .scope(execution, async move {
            let state = State::deserialize(repository.clone(), revision_id)
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::invalid_argument("Invalid revision state"))?;

            let metadata = Metadata::deserialize(repository.clone(), state.metadata_hash())
                .await
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

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::state;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn test_handle() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository.into(),
                ));
                let write_token = get_write_token();
                // Create the main branch (without parent)
                let main = lore_revision::branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    lore_revision::branch::DEFAULT_DEFAULT_NAME,
                    lore_revision::branch::default_category(),
                    "BranchCreator",
                    12345,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create main branch");

                // Create a few revisions
                let state = Arc::new(state::State::new());
                state.set_parent_self(Hash::default());
                state.set_revision_number(1);
                let first_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize state");
                let head = branch_push::push(
                    repository.clone(),
                    main,
                    first_hash,
                    true,
                    true,
                    false,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .expect("Failed to push head revision")
                .revision;
                assert_eq!(head, first_hash);

                let state = Arc::new(state::State::new());
                state.set_parent_self(first_hash);
                state.set_revision_number(2);
                let second_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize state");
                let head = branch_push::push(
                    repository.clone(),
                    main,
                    second_hash,
                    true,
                    true,
                    false,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .expect("Failed to push head revision")
                .revision;
                assert_eq!(head, second_hash);

                let state = Arc::new(state::State::new());
                state.set_parent_self(second_hash);
                state.set_revision_number(3);
                let third_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize state");
                let head = branch_push::push(
                    repository.clone(),
                    main,
                    third_hash,
                    true,
                    true,
                    false,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .expect("Failed to push head revision")
                .revision;
                assert_eq!(head, third_hash);

                // Try to describe the initial revision
                let mut request = Request::new(RevisionDescribeRequest {
                    id: first_hash.into(),
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Request failed");
                let revision = response
                    .into_inner()
                    .revision
                    .expect("Did not get a revision as expected");
                assert_eq!(revision.id, first_hash.as_bytes());
                assert_eq!(revision.parent_self, None);
                assert_eq!(revision.parent_other, None);
                assert_eq!(revision.number, 1);
                assert_eq!(revision.parent_self_number, None);
                assert_eq!(revision.parent_other_number, None);

                // Try to describe a revision with a parent
                let mut request = Request::new(RevisionDescribeRequest {
                    id: third_hash.into(),
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Request failed");
                let revision = response
                    .into_inner()
                    .revision
                    .expect("Did not get a revision as expected");
                assert_eq!(revision.id, third_hash.as_bytes());
                assert_eq!(
                    revision.parent_self.clone().unwrap(),
                    second_hash.as_bytes()
                );
                assert_eq!(revision.parent_other, None);
                assert_eq!(revision.number, 3);
                assert_eq!(revision.parent_self_number, Some(2));
                assert_eq!(revision.parent_other_number, None);

                // Create a merge revision (parent_self = third, parent_other = first)
                let state = Arc::new(state::State::new());
                state.set_parent_self(third_hash);
                state.set_parent_other(first_hash);
                state.set_revision_number(4);
                let merge_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize state");
                let head = branch_push::push(
                    repository.clone(),
                    main,
                    merge_hash,
                    true,
                    true,
                    false,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .expect("Failed to push head revision")
                .revision;
                assert_eq!(head, merge_hash);

                // Describe the merge revision
                let mut request = Request::new(RevisionDescribeRequest {
                    id: merge_hash.into(),
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Request failed");
                let revision = response
                    .into_inner()
                    .revision
                    .expect("Did not get a revision as expected");
                assert_eq!(revision.id, merge_hash.as_bytes());
                assert_eq!(revision.parent_self.clone().unwrap(), third_hash.as_bytes());
                assert_eq!(
                    revision.parent_other.clone().unwrap(),
                    first_hash.as_bytes()
                );
                assert_eq!(revision.number, 4);
                assert_eq!(revision.parent_self_number, Some(3));
                assert_eq!(revision.parent_other_number, Some(1));

                // Try to describe a revision that does not exist
                let mut request = Request::new(RevisionDescribeRequest {
                    id: random::<Hash>().into(),
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect_err("Request should have failed");
            })
            .await;
    }
}
