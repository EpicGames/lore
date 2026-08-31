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

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tracing::debug;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::protocol::attribute_map::AttributeMap;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn test_handle() {
        let repository = random::<RepositoryId>();

        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(repository);

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository,
                ));
                let write_token = get_write_token();
                // Create the main branch (without parent)
                let _main = lore_revision::branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    lore_revision::branch::DEFAULT_DEFAULT_NAME,
                    lore_revision::branch::default_category(),
                    "CreatorUser",
                    1234,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create main branch");

                let message = RevisionStateHistoryRequest {
                    revision: Hash::default().into(),
                    depth: 5,
                    follow_merge: false,
                    with_metadata: true,
                };
                let mut request = Request::new(message);
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Failed RevisionStateHistoryRequest message handle");
                assert_eq!(
                    RevisionStateHistoryResponse {
                        signature: vec![],
                        metadata: vec![]
                    },
                    response.into_inner()
                );

                let message = RevisionStateHistoryRequest {
                    revision: Hash::default().into(),
                    depth: 5,
                    follow_merge: false,
                    with_metadata: false,
                };
                let mut request = Request::new(message);
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Failed RevisionStateHistoryRequest message handle");
                assert_eq!(
                    RevisionStateHistoryResponse {
                        signature: vec![],
                        metadata: vec![]
                    },
                    response.into_inner()
                );

                let state = Arc::new(state::State::new());
                state.set_parent_self(Hash::default());
                let base_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize base state");
                debug!("Created base revision {}", base_hash);

                state.set_parent_self(base_hash);
                let second_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize second state");
                debug!("Created second revision {}", second_hash);

                state.set_parent_self(second_hash);
                let third_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize third state");
                debug!("Created third revision {}", third_hash);

                let message = RevisionStateHistoryRequest {
                    revision: third_hash.into(),
                    depth: 5,
                    follow_merge: false,
                    with_metadata: false,
                };
                let mut request = Request::new(message);
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Failed RevisionStateHistoryRequest message handle");
                assert_eq!(
                    RevisionStateHistoryResponse {
                        signature: vec![
                            second_hash.into(),
                            base_hash.into(),
                            Hash::default().into()
                        ],
                        metadata: vec![]
                    },
                    response.into_inner()
                );

                let message = RevisionStateHistoryRequest {
                    revision: Hash::default().into(),
                    depth: 10000,
                    follow_merge: false,
                    with_metadata: false,
                };
                let mut request = Request::new(message);
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Failed RevisionStateHistoryRequest message handle");
                assert_eq!(
                    RevisionStateHistoryResponse {
                        signature: vec![],
                        metadata: vec![]
                    },
                    response.into_inner()
                );
            })
            .await;
    }
}
