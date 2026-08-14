// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchListRequest;
use lore_proto::BranchListResponse;
use lore_revision::branch;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::info_span;
use tracing::warn;

use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchList::handle", skip_all)]
pub async fn handler(
    request: Request<BranchListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchListResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let _req = request.into_inner();

    debug!("Handling branch list request for repository");

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
    ));
    LORE_CONTEXT
        .scope(
            execution,
            async move { branch_list_handler(repository).await },
        )
        .await
}

async fn branch_list_handler(
    repository: Arc<RepositoryContext>,
) -> Result<Response<BranchListResponse>, Status> {
    let mut branch_list = match branch::list(repository.clone()).await {
        Ok(branch_list) => branch_list,
        Err(err) if err.is_branch_not_found() => {
            warn!("No branches found for repository: {}", repository.id);
            return Ok(Response::new(BranchListResponse { branches: vec![] }));
        }
        Err(err) => {
            warn!("Failed to retrieve branch list: {err}");
            return Err(Status::internal(err.to_string()));
        }
    };

    // TODO(mjansson): Change this to a streaming response
    let mut branch_meta_tasks = JoinSet::new();
    while let Some(branch) = branch_list.next().await {
        let repository = repository.clone();
        let span = info_span!("retrieve_metadata", %branch);
        lore_spawn!(
            branch_meta_tasks,
            async move {
                let metadata = branch::metadata(repository.clone(), branch)
                    .await
                    .inspect_err(|err| warn!(?err, "Failed to retrieve branch metadata"))?;

                branch::branch_metadata(repository.clone(), branch, &metadata)
                    .await
                    .inspect_err(|err| warn!(?err, "Failed to resolve branch metadata"))
            }
            .instrument(span)
        );
    }

    let mut branches: Vec<lore_proto::Branch> = vec![];
    while let Some(task_result) = branch_meta_tasks.join_next().await {
        if let Ok(Ok(metadata)) = task_result
            .warn_map_err(|err| Status::internal(format!("Failed branch metadata task: {err:?}")))
        {
            branches.push(metadata.into());
        }
    }

    // Ensure the default branch is included in the response. If missing,
    // recreate the branch name-to-id mutable key and include its metadata.
    if let Ok(metadata_hash) = repository::metadata_hash(repository.clone()).await
        && let Ok(repo_metadata) = repository::metadata(repository.clone(), metadata_hash).await
    {
        let default_branch = repo_metadata.default_branch;
        if !default_branch.is_zero()
            && !branches
                .iter()
                .any(|b| b.id.as_ref() == default_branch.data())
        {
            warn!(
                %default_branch,
                name = repo_metadata.default_branch_name,
                "Default branch missing from list, recreating name-to-id mapping"
            );
            if let Err(err) = branch::store_name_to_id(
                repository.clone(),
                default_branch,
                &repo_metadata.default_branch_name,
            )
            .await
            {
                warn!(%err, "Failed to recreate default branch name-to-id mapping");
            }

            if let Ok(metadata) = branch::metadata(repository.clone(), default_branch).await
                && let Ok(branch_meta) =
                    branch::branch_metadata(repository.clone(), default_branch, &metadata).await
            {
                branches.push(branch_meta.into());
            }
        }
    }

    Ok(Response::new(BranchListResponse { branches }))
}

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::branch::BranchLatestStatus;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn test_handle() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository.into(),
                ));
                let write_token = get_write_token();
                // Create the main branch (without parent)
                let no_parent = Context::default(); // Zero, no parent
                let payload_main = random::<[u8; size_of::<Hash>()]>().to_vec();
                let hash_main = Hash::hash_buffer(&payload_main);
                let main = lore_revision::branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    lore_revision::branch::DEFAULT_DEFAULT_NAME,
                    lore_revision::branch::default_category(),
                    "MainCreator",
                    1234,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create main branch");

                let mut request = Request::new(BranchListRequest {});
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Failed BranchList message handle");
                assert_eq!(
                    BranchListResponse {
                        branches: [lore_proto::Branch {
                            id: main.into(),
                            name: lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                            category: lore_revision::branch::default_category().to_string(),
                            parent_deprecated: Some(Context::default().into()),
                            latest: Hash::default().into(),
                            branch_point_deprecated: Some(Hash::default().into()),
                            creator: "MainCreator".to_string(),
                            created: 1234,
                            stack: vec![],
                        }]
                        .to_vec()
                    },
                    response.into_inner()
                );

                // Create another branch1
                let payload_branch1 = random::<[u8; size_of::<Hash>()]>().to_vec();
                let hash_branch1 = Hash::hash_buffer(&payload_branch1);
                let branch1 = lore_revision::branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    "branch1",
                    lore_revision::branch::default_category(),
                    "TestCreator",
                    4321,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create branch1 branch");

                // Update main branch
                branch::store_latest(
                    repository.clone(),
                    main,
                    Hash::default(),
                    hash_main,
                    BranchLatestStatus::Convergent,
                )
                .await
                .expect("Failed to store latest");

                // Update branch1 branch
                branch::store_latest(
                    repository.clone(),
                    branch1,
                    Hash::default(),
                    hash_branch1,
                    BranchLatestStatus::Convergent,
                )
                .await
                .expect("Failed to store latest");

                let mut request = Request::new(BranchListRequest {});
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Failed BranchList message handle")
                    .into_inner();

                assert!(response.branches.contains(&lore_proto::Branch {
                    id: main.into(),
                    name: lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                    category: lore_revision::branch::default_category().to_string(),
                    parent_deprecated: Some(no_parent.into()),
                    latest: hash_main.into(),
                    branch_point_deprecated: Some(Hash::default().into()),
                    creator: "MainCreator".to_string(),
                    created: 1234,
                    stack: vec![]
                }));

                assert!(response.branches.contains(&lore_proto::Branch {
                    id: branch1.into(),
                    name: "branch1".to_string(),
                    category: lore_revision::branch::default_category().to_string(),
                    parent_deprecated: Some(Context::default().into()),
                    latest: hash_branch1.into(),
                    branch_point_deprecated: Some(Hash::default().into()),
                    creator: "TestCreator".to_string(),
                    created: 4321,
                    stack: vec![]
                }));
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_no_branches() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository.into(),
                ));
                let mut request = Request::new(BranchListRequest {});
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("Failed BranchList message handle");
                assert_eq!(
                    BranchListResponse { branches: vec![] },
                    response.into_inner()
                );
            })
            .await;
    }
}
