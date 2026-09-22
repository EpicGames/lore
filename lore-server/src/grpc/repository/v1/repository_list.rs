// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::repository::v1::RepositoryListRequest;
use lore_proto::lore::repository::v1::RepositoryListResponse;
use lore_revision::lore::RepositoryId;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;

use super::record::build_repository;
use crate::authnz::repository_authorizer::VerifiedTokenOwned;
use crate::authnz::repository_catalog::RepositoryCatalog;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
use crate::util::setup_execution;

type ListStream =
    Pin<Box<dyn Stream<Item = Result<RepositoryListResponse, Status>> + Send + 'static>>;

/// `lore.repository.v1.RepositoryService.RepositoryList` handler.
///
/// Streams `Repository` records for the repositories the configured
/// [`RepositoryCatalog`] lists for the caller.
///
/// `RepositoryListRequest.creator`, when set, filters the stream to
/// repositories whose `creator` exactly matches.
///
/// The response streams, so the RPC runs under no request timeout;
/// `catalog_budget` bounds the catalog walk that precedes the stream.
#[tracing::instrument(name = "RepositoryList::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryListRequest>,
    repository_catalog: Arc<dyn RepositoryCatalog>,
    catalog_budget: Duration,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<ListStream>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let token = get_verified_token(request.extensions()).map(|token| token.owned());
    let req = request.into_inner();
    let creator_filter = req.creator;

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let candidate_ids = LORE_CONTEXT
        .scope(execution.clone(), async {
            let token = token.as_ref().map(VerifiedTokenOwned::as_token);
            repository_catalog
                .list_all(token.as_ref(), catalog_budget)
                .await
        })
        .await?;

    debug!(count = candidate_ids.len(), "Repository list candidates");

    let (tx, rx) = mpsc::channel::<Result<RepositoryListResponse, Status>>(16);

    lore_spawn!(
        LORE_CONTEXT
            .scope(execution, async move {
                let mut tasks: JoinSet<()> = JoinSet::new();
                for id in candidate_ids {
                    let immutable_store = immutable_store.clone();
                    let mutable_store = mutable_store.clone();
                    let creator_filter = creator_filter.clone();
                    let tx = tx.clone();
                    lore_spawn!(
                        tasks,
                        LORE_CONTEXT
                            .scope(execution_context(), async move {
                                let item = load_and_filter_repository(
                                    immutable_store,
                                    mutable_store,
                                    id,
                                    creator_filter,
                                )
                                .await;
                                if let Some(item) = item
                                    && tx.send(item).await.is_err()
                                {
                                    debug!("Repository list receiver dropped");
                                }
                            })
                            .in_current_span(),
                    );
                }
                while let Some(_done) = tasks.join_next().await {}
            })
            .in_current_span(),
    );

    let recv_stream = ReceiverStream::from(rx);
    Ok(Response::new(Box::pin(recv_stream) as ListStream))
}

/// Build one repository record. A repository whose metadata cannot be read is
/// skipped with `None`, since a partially-written repository should not fail
/// the whole listing. A store asking the caller to back off is emitted onto
/// the stream instead, so the client is told to retry rather than handed a
/// listing with repositories silently missing from it.
async fn load_and_filter_repository(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    id: RepositoryId,
    creator_filter: Option<String>,
) -> Option<Result<RepositoryListResponse, Status>> {
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        id,
    ));

    let metadata_hash = match repository::metadata_hash(repository.clone())
        .await
        .filter_slow_down()
    {
        Err(status) => return Some(Err(status)),
        Ok(Ok(hash)) => hash,
        Ok(Err(err)) => {
            debug!(%id, %err, "Repository list: metadata hash unavailable, skipping");
            return None;
        }
    };
    let metadata = match repository::metadata(repository.clone(), metadata_hash)
        .await
        .filter_slow_down()
    {
        Err(status) => return Some(Err(status)),
        Ok(Ok(metadata)) => metadata,
        Ok(Err(err)) => {
            debug!(%id, %err, "Repository list: metadata blob unavailable, skipping");
            return None;
        }
    };

    if let Some(filter) = creator_filter.as_ref()
        && metadata.creator.as_str() != filter.as_str()
    {
        return None;
    }

    Some(Ok(RepositoryListResponse {
        repository: Some(build_repository(id, &metadata, metadata_hash)),
    }))
}
