// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchListRequest;
use lore_proto::BranchListResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
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

use crate::grpc::FilterSlowDownExt;
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
    let mut branch_list = match branch::list(repository.clone()).await.filter_slow_down()? {
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
        if let Ok(branch_metadata) = task_result
            .warn_map_err(|err| Status::internal(format!("Failed branch metadata task: {err:?}")))
            && let Ok(metadata) = branch_metadata.filter_slow_down()?
        {
            branches.push(metadata.into());
        }
    }

    // Ensure the default branch is included in the response. If missing,
    // recreate the branch name-to-id mutable key and include its metadata.
    if let Ok(metadata_hash) = repository::metadata_hash(repository.clone())
        .await
        .filter_slow_down()?
        && let Ok(repo_metadata) = repository::metadata(repository.clone(), metadata_hash)
            .await
            .filter_slow_down()?
    {
        let default_branch = repo_metadata.default_branch;
        if !default_branch.is_zero()
            && !branches
                .iter()
                .any(|b| b.id.as_ref() == default_branch.data())
        {
            repair_default_branch_mapping(
                repository.clone(),
                default_branch,
                &repo_metadata.default_branch_name,
            )
            .await;

            if let Ok(metadata) = branch::metadata(repository.clone(), default_branch)
                .await
                .filter_slow_down()?
                && let Ok(branch_meta) =
                    branch::branch_metadata(repository.clone(), default_branch, &metadata)
                        .await
                        .filter_slow_down()?
            {
                branches.push(branch_meta.into());
            }
        }
    }

    Ok(Response::new(BranchListResponse { branches }))
}

/// Claims `name` for `default_branch`, the default branch the listing found missing, unless another
/// branch holds the name ([`branch::claim_name_unless_held`]), and logs one warning naming the
/// outcome. The repair is best effort, so it answers nothing, and a retryable failure doesn't fail
/// the listing. A name another branch holds stays with it, so each listing warns again.
fn repair_default_branch_mapping(
    repository: Arc<RepositoryContext>,
    default_branch: BranchId,
    name: &str,
) -> impl Future<Output = ()> {
    let claim = branch::claim_name_unless_held(repository, name, default_branch);
    async move {
        match claim.await {
            Ok(true) => warn!(
                %default_branch,
                %name,
                "The default branch is missing from the list. Its name maps to it."
            ),
            Ok(false) => warn!(
                %default_branch,
                %name,
                "The default branch is missing from the list. Another branch holds its name, which stays with that branch."
            ),
            Err(err) => warn!(
                %default_branch,
                %name,
                %err,
                "The default branch is missing from the list. Claiming its name failed."
            ),
        }
    }
}
