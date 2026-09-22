// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::RepositoryListRequest;
use lore_proto::RepositoryListResponse;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tokio::task::JoinSet;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::warn;

use crate::authnz::repository_authorizer::VerifiedTokenOwned;
use crate::authnz::repository_catalog::RepositoryCatalog;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryList::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryListRequest>,
    repository_catalog: Arc<dyn RepositoryCatalog>,
    catalog_budget: Duration,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryListResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let token = get_verified_token(request.extensions()).map(|token| token.owned());
    let _req = request.into_inner();

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        Context::default().into(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let token = token.as_ref().map(VerifiedTokenOwned::as_token);
            let visible = repository_catalog
                .list_all(token.as_ref(), catalog_budget)
                .await?;

            // TODO(mjansson): Change this to a streaming response
            let mut authorized_repositories = JoinSet::new();
            for id in visible {
                let repository = Arc::new(repository.to_server_context(id));
                lore_spawn!(
                    authorized_repositories,
                    LORE_CONTEXT
                        .scope(execution_context(), async move {
                            (id, repository::metadata_hash(repository).await)
                        })
                        .in_current_span(),
                );
            }

            debug!(
                "Repository list found {} entries",
                authorized_repositories.len()
            );

            let mut repositories: Vec<lore_proto::Repository> = vec![];
            while let Some(task_result) = authorized_repositories.join_next().await {
                let (id, result) = task_result.warn_map_err(|err| {
                    warn!("Repository list metadata failed: {err}");
                    Status::internal(format!("Failed repository metadata task: {err:?}"))
                })?;

                match result.filter_slow_down()? {
                    Ok(metadata_hash) => {
                        let repository = Arc::new(repository.to_server_context(id));
                        match repository::metadata(repository, metadata_hash)
                            .await
                            .filter_slow_down()?
                        {
                            Ok(metadata) => {
                                repositories.push(lore_proto::Repository {
                                    id: Context::from(id).into(),
                                    name: metadata.name,
                                    metadata: metadata_hash.into(),
                                });
                            }
                            Err(err) => warn!("Failed to load repository metadata: {err}"),
                        }
                    }
                    Err(err) => warn!("Failed to retrieve repository metadata: {err}"),
                }
            }

            debug!("Repository list with {} entries", repositories.len());

            Ok(Response::new(RepositoryListResponse { repositories }))
        })
        .await
}
