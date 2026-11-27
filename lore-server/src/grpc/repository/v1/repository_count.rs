// SPDX-FileCopyrightText: 2026 LoreLab.io
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::repository::v1::RepositoryCountRequest;
use lore_proto::lore::repository::v1::RepositoryCountResponse;
use lore_revision::lore::RepositoryId;
use lore_revision::lore::execution_context;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;

use crate::grpc::ServerResultExt;
use crate::grpc::extract_authorization_header;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::handlers::repository_list::lookup_authorized_repositories;
use crate::util::setup_execution;

/// `lore.repository.v1.RepositoryService.RepositoryCount` handler.
///
/// Returns the number of repositories the caller is authorised to see,
/// applying the same filter semantics as `RepositoryList`. When no
/// filter is set the count is answered directly from the id set with no
/// per-repository metadata reads; a `creator` filter forces per-repo
/// metadata loads so the exact same predicate can be evaluated.
#[tracing::instrument(name = "RepositoryCount::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryCountRequest>,
    auth_url: Option<String>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryCountResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = extract_authorization_header(&request);
    let req = request.into_inner();
    let creator_filter = req.creator;

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let candidate_ids = candidate_ids(
                immutable_store.clone(),
                mutable_store.clone(),
                auth_url,
                authorization,
            )
            .await?;

            debug!(count = candidate_ids.len(), "Repository count candidates");

            let count = if let Some(filter) = creator_filter {
                count_matching_creator(immutable_store, mutable_store, candidate_ids, filter).await
            } else {
                candidate_ids.len() as u64
            };

            Ok(Response::new(RepositoryCountResponse { count }))
        })
        .await
}

/// Resolve the caller's authorised repository id set. When an auth URL is
/// configured the ids come from the auth service; otherwise every
/// locally-known repository id is returned. Mirrors the same-named helper
/// inside `repository_list.rs` (separated to avoid widening visibility).
async fn candidate_ids(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    auth_url: Option<String>,
    authorization: Option<String>,
) -> Result<Vec<RepositoryId>, Status> {
    if let Some(auth_url) = auth_url {
        let ids = lookup_authorized_repositories(auth_url, authorization).await?;
        Ok(ids.into_iter().map(RepositoryId::from).collect())
    } else {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            Context::default().into(),
        ));
        let mut stream = repository::list_local(repository)
            .await
            .warn_map_err(|err| Status::internal(format!("Failed to list repositories: {err}")))?;
        let mut out = Vec::new();
        while let Some(id) = stream.next().await {
            out.push(id.into());
        }
        Ok(out)
    }
}

/// Fan out per-repo metadata loads under a `JoinSet`, counting only those
/// whose `creator` matches `filter`. Per-repo load failures are logged
/// and skipped, matching `RepositoryList`'s tolerance for missing or
/// corrupt metadata blobs.
async fn count_matching_creator(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    candidate_ids: Vec<RepositoryId>,
    filter: String,
) -> u64 {
    let mut tasks: JoinSet<bool> = JoinSet::new();
    for id in candidate_ids {
        let immutable_store = immutable_store.clone();
        let mutable_store = mutable_store.clone();
        let filter = filter.clone();
        lore_spawn!(
            tasks,
            LORE_CONTEXT
                .scope(execution_context(), async move {
                    creator_matches(immutable_store, mutable_store, id, &filter).await
                })
                .in_current_span(),
        );
    }

    let mut count: u64 = 0;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(true) => count += 1,
            Ok(false) => {}
            Err(err) => debug!(%err, "Repository count: metadata task panicked, skipping"),
        }
    }
    count
}

async fn creator_matches(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    id: RepositoryId,
    filter: &str,
) -> bool {
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        id,
    ));

    let metadata_hash = match repository::metadata_hash(repository.clone()).await {
        Ok(hash) => hash,
        Err(err) => {
            debug!(%id, %err, "Repository count: metadata hash unavailable, skipping");
            return false;
        }
    };
    let metadata = match repository::metadata(repository, metadata_hash).await {
        Ok(metadata) => metadata,
        Err(err) => {
            debug!(%id, %err, "Repository count: metadata blob unavailable, skipping");
            return false;
        }
    };

    metadata.creator.as_str() == filter
}

#[cfg(test)]
mod tests {
    use lore_revision::repository::RepositoryMetadata;

    use super::*;
    use crate::store::test_store_create;

    async fn seed_repository(
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        mutable: Arc<dyn lore_storage::MutableStore>,
        id_byte: u8,
        creator: &str,
    ) {
        let id_bytes = [id_byte; 16];
        let name = format!("repo-{id_byte}");
        let repo_id = RepositoryId::from(Context::from(id_bytes));
        let repo_ctx = Arc::new(RepositoryContext::new_server_context(
            immutable, mutable, repo_id,
        ));
        let hash = repository::metadata_store(
            repo_ctx.clone(),
            RepositoryMetadata {
                name: name.clone(),
                creator: creator.to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        repository::metadata_store_hash(repo_ctx.clone(), hash)
            .await
            .unwrap();
        // Register the id in the local repository index so `list_local`
        // returns it - otherwise the handler's unfiltered count sees zero
        // even after metadata is written.
        repository::store_name_to_id(repo_ctx, name, repo_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn empty_store_counts_zero() {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                let response = handler(
                    Request::new(RepositoryCountRequest { creator: None }),
                    None,
                    immutable,
                    mutable,
                )
                .await
                .unwrap();
                assert_eq!(response.into_inner().count, 0);
            })
            .await;
    }

    #[tokio::test]
    async fn unfiltered_returns_total_count() {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                seed_repository(immutable.clone(), mutable.clone(), 1, "alice").await;
                seed_repository(immutable.clone(), mutable.clone(), 2, "bob").await;
                seed_repository(immutable.clone(), mutable.clone(), 3, "alice").await;

                let response = handler(
                    Request::new(RepositoryCountRequest { creator: None }),
                    None,
                    immutable,
                    mutable,
                )
                .await
                .unwrap();
                assert_eq!(response.into_inner().count, 3);
            })
            .await;
    }

    #[tokio::test]
    async fn creator_filter_counts_matches_only() {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                seed_repository(immutable.clone(), mutable.clone(), 1, "alice").await;
                seed_repository(immutable.clone(), mutable.clone(), 2, "bob").await;
                seed_repository(immutable.clone(), mutable.clone(), 3, "alice").await;

                let response = handler(
                    Request::new(RepositoryCountRequest {
                        creator: Some("alice".to_string()),
                    }),
                    None,
                    immutable,
                    mutable,
                )
                .await
                .unwrap();
                assert_eq!(response.into_inner().count, 2);
            })
            .await;
    }

    #[tokio::test]
    async fn creator_filter_with_no_matches_counts_zero() {
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                seed_repository(immutable.clone(), mutable.clone(), 1, "alice").await;
                seed_repository(immutable.clone(), mutable.clone(), 2, "bob").await;

                let response = handler(
                    Request::new(RepositoryCountRequest {
                        creator: Some("carol".to_string()),
                    }),
                    None,
                    immutable,
                    mutable,
                )
                .await
                .unwrap();
                assert_eq!(response.into_inner().count, 0);
            })
            .await;
    }

    #[tokio::test]
    async fn corrupt_metadata_is_skipped_under_filter() {
        // Two healthy repos plus one whose id is registered locally but
        // whose metadata is missing - the count must reflect only the
        // matching healthy repos, not error out.
        let (immutable, mutable, execution) = test_store_create().await.unwrap();
        LORE_CONTEXT
            .scope(execution, async move {
                seed_repository(immutable.clone(), mutable.clone(), 1, "alice").await;
                seed_repository(immutable.clone(), mutable.clone(), 2, "alice").await;

                // Register a bare id in the repository index without
                // seeding any metadata for it - `metadata_hash` will
                // fail, exercising the skip-on-error path.
                let bare_id = RepositoryId::from(Context::from([9u8; 16]));
                let bare_ctx = Arc::new(RepositoryContext::new_server_context(
                    immutable.clone(),
                    mutable.clone(),
                    bare_id,
                ));
                repository::store_name_to_id(bare_ctx, "bare-repo", bare_id)
                    .await
                    .unwrap();

                let response = handler(
                    Request::new(RepositoryCountRequest {
                        creator: Some("alice".to_string()),
                    }),
                    None,
                    immutable,
                    mutable,
                )
                .await
                .unwrap();
                assert_eq!(response.into_inner().count, 2);
            })
            .await;
    }
}
