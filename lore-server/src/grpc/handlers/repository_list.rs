// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::RepositoryListRequest;
use lore_proto::RepositoryListResponse;
use lore_proto::auth::LookupUserPermissionsRequest;
use lore_revision::lore::execution_context;
use lore_revision::lore_debug;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tonic::Code;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::warn;

use crate::authnz::auth::grpc_get_auth_client;
use crate::authnz::common::create_request_with_authorization;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_authorization_header;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

#[tracing::instrument(name = "RepositoryList::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryListRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryListResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = extract_authorization_header(&request);
    let _req = request.into_inner();

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        Context::default().into(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // TODO(mjansson): Change this to a streaming response
            let mut authorized_repositories = if let Some(auth_url) = auth_url {
                let authorized_repositories =
                    lookup_authorized_repositories(auth_url, authorization).await?;

                let mut meta_tasks = JoinSet::new();
                for id in authorized_repositories {
                    let repository = Arc::new(repository.to_server_context(id.into()));
                    lore_spawn!(
                        meta_tasks,
                        LORE_CONTEXT
                            .scope(execution_context(), async move {
                                (id, repository::metadata_hash(repository).await)
                            })
                            .in_current_span(),
                    );
                }

                meta_tasks
            } else {
                let mut repository_list = repository::list_local(repository.clone())
                    .await
                    .warn_map_err(|err| {
                        Status::internal(format!("Failed to list repositories: {err}"))
                    })?;
                let mut meta_tasks = JoinSet::new();
                while let Some(id) = repository_list.next().await {
                    let repository = Arc::new(repository.to_server_context(id.into()));
                    lore_spawn!(
                        meta_tasks,
                        LORE_CONTEXT
                            .scope(execution_context(), async move {
                                (id, repository::metadata_hash(repository).await)
                            })
                            .in_current_span(),
                    );
                }

                meta_tasks
            };

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

                match result {
                    Ok(metadata_hash) => {
                        let repository = Arc::new(repository.to_server_context(id.into()));
                        match repository::metadata(repository, metadata_hash).await {
                            Ok(metadata) => {
                                repositories.push(lore_proto::Repository {
                                    id: id.into(),
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

pub(crate) async fn lookup_authorized_repositories(
    auth_url: String,
    authorization: Option<String>,
) -> Result<Vec<Context>, Status> {
    lore_debug!("Repository fetch authorized repositories");

    let mut client = grpc_get_auth_client(auth_url).await?;
    let request = create_request_with_authorization(
        LookupUserPermissionsRequest {
            resource_filter: "urc".to_string(),
            ..Default::default()
        },
        authorization,
    )?;

    let permissions = client
        .lookup_user_permissions(request)
        .await
        .warn_map_err(|err| {
            if err.code() == Code::PermissionDenied {
                return Status::permission_denied("List resources denied");
            } else if err.code() == Code::Unauthenticated {
                return Status::unauthenticated("list resource failed - unauthenticated");
            }
            Status::internal(format!(
                "Failed to call auth lookup_user_permissions: {err}"
            ))
        })?;

    Ok(permissions
        .into_inner()
        .resource_permission
        .iter()
        .filter_map(|permission| {
            permission
                .resource_id
                .strip_prefix("urc-")
                .and_then(|repository_id| Context::from_str(repository_id).ok())
        })
        .collect())
}
