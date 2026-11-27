// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::allow_all_repositories;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::handlers::branch_push::PushHandler;
use lore_server::grpc::handlers::branch_push::PushReport;
use lore_server::grpc::revision::v1::branch_list::*;
use lore_storage::ReadConsistency;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::store::test_support::test_store_create;

/// Creates a root-style branch (empty stack); not deletable.
pub async fn create_root_branch(
    repository_context: &Arc<RepositoryContext>,
    name: &str,
    creator: &str,
) -> BranchId {
    let write_token = get_write_token();
    lore_revision::branch::create(
        repository_context.clone(),
        &write_token,
        BranchId::from(uuid::Uuid::now_v7()),
        name,
        branch::default_category(),
        creator,
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("Could not create root branch")
}

/// Pushes a real revision to `branch` so it can serve as a parent in
/// subsequent `BranchPoint` entries (zero-revision parents are
/// rejected unless the parent is the repository's default branch,
/// which the test fixture doesn't initialise).
async fn seed_revision(repository_context: &Arc<RepositoryContext>, branch: BranchId) -> Hash {
    let write_token = get_write_token();
    let state = state::State::new();
    state.set_parent_self(Hash::default());
    state.set_revision_number(1);
    let state_hash = state
        .serialize(repository_context.clone(), &write_token)
        .await
        .expect("Failed to serialize state");
    branch_push::push(
        repository_context.clone(),
        None,
        branch,
        state_hash,
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
        &mut PushReport::new(PushHandler::Legacy, Default::default()),
    )
    .await
    .expect("Failed to push latest revision")
    .revision
}

/// Creates a child branch off `parent@parent_revision` so it has a
/// non-empty stack and can be deleted.
async fn create_child_branch(
    repository_context: &Arc<RepositoryContext>,
    name: &str,
    creator: &str,
    parent: BranchId,
    parent_revision: Hash,
) -> BranchId {
    let write_token = get_write_token();
    lore_revision::branch::create(
        repository_context.clone(),
        &write_token,
        BranchId::from(uuid::Uuid::now_v7()),
        name,
        branch::default_category(),
        creator,
        1,
        vec![BranchPoint {
            branch: parent,
            revision: parent_revision,
        }],
        false,
        false,
    )
    .await
    .expect("Could not create child branch")
}

fn make_request(
    repository: RepositoryId,
    creator: Option<String>,
    include_deleted: bool,
) -> Request<BranchListRequest> {
    let mut request = Request::new(BranchListRequest {
        creator,
        include_deleted,
    });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

async fn collect_response(
    response: Response<BranchListStream>,
) -> Vec<Result<BranchListResponse, Status>> {
    response.into_inner().collect().await
}

/// Reads that calls share for `window` after each starts.
pub fn shared_reads(window: std::time::Duration) -> Arc<BranchListReads> {
    BranchListReads::new(
        window,
        &lore_server::grpc::revision::v1::service::RevisionServiceInstrumentProvider,
    )
    .expect("the window is below the maximum")
}

/// Reads that only calls made while one runs share, so each call made after another has finished
/// reads the store.
pub fn unshared_reads() -> Arc<BranchListReads> {
    shared_reads(std::time::Duration::ZERO)
}

mod direct_handling {
    use super::*;

    #[tokio::test]
    async fn list_streams_all_live_branches() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;

            let response = handler(
                make_request(repository, None, false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");

            let items: Vec<_> = collect_response(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();

            assert_eq!(items.len(), 2);
            let names: Vec<String> = items
                .iter()
                .map(|r| r.branch.as_ref().unwrap().name.clone())
                .collect();
            assert!(names.contains(&"main".to_string()));
            assert!(names.contains(&"feature".to_string()));
            assert!(items.iter().all(|r| !r.branch.as_ref().unwrap().deleted));
        }))
        .await;
    }

    /// The key of the `function` value of `branch_id` in `repository`, in the form `list` of
    /// `mutable_store` reports it.
    fn listed_key(
        mutable_store: &Arc<dyn lore_storage::MutableStore>,
        function: &str,
        repository: RepositoryId,
        branch_id: BranchId,
    ) -> Hash {
        let (key, key_type) = branch::mutable_key(
            lore_revision::repository::SALT_LORE,
            function,
            repository,
            branch_id,
        );
        mutable_store.stored_typed_key(key, key_type)
    }

    /// The items of `response`, each expected to be a branch.
    async fn collect_branches(
        response: Response<BranchListStream>,
    ) -> Vec<lore_proto::lore::model::v1::Branch> {
        collect_response(response)
            .await
            .into_iter()
            .map(|item| {
                item.expect("stream item ok")
                    .branch
                    .expect("branch response")
            })
            .collect()
    }

    /// The status that ends the stream of `response`.
    async fn final_status(response: Response<BranchListStream>) -> Status {
        collect_response(response)
            .await
            .into_iter()
            .find_map(Result::err)
            .expect("stream must carry an error")
    }

    /// A throttled range read must end the stream with `RESOURCE_EXHAUSTED`
    /// rather than completing successfully with branches absent: a client
    /// cannot tell an omitted branch from one that does not exist, so a
    /// silently short listing is worse than an error.
    #[tokio::test]
    async fn throttled_range_read_ends_the_stream() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            create_root_branch(&repository_context, "main", "alice").await;

            for key_type in [
                KeyType::BranchId,
                KeyType::BranchMetadata,
                KeyType::BranchLatestPointer,
            ] {
                let throttled = lore_storage::test_util::HookedStore::wrap(
                    mutable_store.clone(),
                    move |call, _key, called_type| {
                        if call == lore_storage::test_util::StoreCall::List
                            && called_type == key_type
                        {
                            return Err(lore_storage::StoreError::from(lore_base::error::SlowDown));
                        }
                        Ok(())
                    },
                    None,
                );
                let response = handler(
                    make_request(repository, None, false),
                    immutable_store.clone(),
                    throttled,
                    &None, /* no forwarded requests */
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &unshared_reads(),
                )
                .await
                .expect("handler returns a stream");

                let status = final_status(response).await;
                assert_eq!(
                    status.code(),
                    tonic::Code::ResourceExhausted,
                    "{key_type:?}"
                );
                assert_eq!(status.message(), OVERLOADED);
            }
        }))
        .await;
    }

    /// A throttled metadata record read skips that branch and lists the others:
    /// the read layer reports a failed immutable read as not found.
    #[tokio::test]
    async fn throttled_metadata_record_read_skips_the_branch() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            create_root_branch(&repository_context, "main", "alice").await;
            let feature = create_root_branch(&repository_context, "feature", "bob").await;
            let metadata_hash = branch::metadata_hash(repository_context, feature)
                .await
                .expect("metadata hash");
            let throttled = lore_storage::test_util::HookedImmutableStore::wrap(
                immutable_store,
                move |call, _partition, address| {
                    if call == lore_storage::test_util::ImmutableCall::Get
                        && address.hash == metadata_hash
                    {
                        return Err(lore_storage::StoreError::from(lore_base::error::SlowDown));
                    }
                    Ok(())
                },
            );

            let response = handler(
                make_request(repository, None, false),
                throttled,
                mutable_store,
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("handler returns a stream");

            assert_eq!(
                listed_branches(response.into_inner()).await,
                [("main".to_string(), false)]
            );
        }))
        .await;
    }

    /// A branch whose latest entry the range read misses is listed with a
    /// zero latest, and its latest is not read again.
    #[tokio::test]
    async fn branch_whose_latest_entry_is_not_listed_has_a_zero_latest() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let feature =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;
            let feature_latest = seed_revision(&repository_context, feature).await;
            assert_ne!(feature_latest, Hash::default());
            let missing = crate::store::test_support::ListOmittingStore::wrap(
                mutable_store.clone(),
                [listed_key(
                    &mutable_store,
                    branch::LATEST,
                    repository,
                    feature,
                )],
            );

            let response = handler(
                make_request(repository, None, false),
                immutable_store,
                missing,
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("handler returns a stream");

            let branches = collect_branches(response).await;
            assert_eq!(branches.len(), 2);
            let main = branches
                .iter()
                .find(|branch| branch.name == "main")
                .expect("main is listed");
            assert_eq!(Hash::from(main.latest.clone()), main_latest);
            let feature = branches
                .iter()
                .find(|branch| branch.name == "feature")
                .expect("feature is listed without its latest entry");
            assert_eq!(Hash::from(feature.latest.clone()), Hash::default());
        }))
        .await;
    }

    /// A live branch whose metadata entry the range read misses is left out,
    /// with or without deleted branches, and its metadata is not read again.
    #[tokio::test]
    async fn branch_whose_metadata_entry_is_not_listed_is_left_out() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let feature =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;
            let missing = crate::store::test_support::ListOmittingStore::wrap(
                mutable_store.clone(),
                [listed_key(
                    &mutable_store,
                    branch::METADATA,
                    repository,
                    feature,
                )],
            );

            for include_deleted in [false, true] {
                let response = handler(
                    make_request(repository, None, include_deleted),
                    immutable_store.clone(),
                    missing.clone(),
                    &None, /* no forwarded requests */
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &unshared_reads(),
                )
                .await
                .expect("handler returns a stream");

                let branches = collect_branches(response).await;
                assert_eq!(branches.len(), 1);
                assert_eq!(branches[0].name, "main");
                assert_eq!(Hash::from(branches[0].latest.clone()), main_latest);
            }
        }))
        .await;
    }

    /// A branch two names map to is listed once.
    #[tokio::test]
    async fn branch_two_names_map_to_is_listed_once() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            // The key of a second name: listing reads the value, not the name it is the key of.
            mutable_store
                .clone()
                .store(
                    repository,
                    random::<Hash>(),
                    Hash::from_context(main),
                    KeyType::BranchId,
                )
                .await
                .expect("store a second name");

            for include_deleted in [false, true] {
                let response = handler(
                    make_request(repository, None, include_deleted),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None, /* no forwarded requests */
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &unshared_reads(),
                )
                .await
                .expect("handler returns a stream");

                assert_eq!(
                    listed_branches(response.into_inner()).await,
                    [("main".to_string(), false)]
                );
            }
        }))
        .await;
    }

    #[tokio::test]
    async fn unreadable_metadata_payload_omits_only_the_broken_branch() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let feature =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;
            // The pointer resolves, but its metadata payload is unavailable in either listing pass.
            branch::mutable_store_metadata(repository_context, feature, random::<Hash>())
                .await
                .expect("replace metadata pointer");

            for include_deleted in [false, true] {
                let response = handler(
                    make_request(repository, None, include_deleted),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    &None,
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &unshared_reads(),
                )
                .await
                .expect("handler returns a stream");
                let branches: Vec<_> = collect_response(response)
                    .await
                    .into_iter()
                    .map(|item| {
                        item.expect("missing payload must not terminate listing")
                            .branch
                            .expect("branch response")
                    })
                    .collect();
                assert_eq!(branches.len(), 1);
                assert_eq!(branches[0].name, "main");
                assert_eq!(Hash::from(branches[0].latest.clone()), main_latest);
            }
        }))
        .await;
    }

    #[tokio::test]
    async fn list_excludes_deleted_by_default() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let to_delete =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;
            branch::delete(repository_context.clone(), to_delete)
                .await
                .expect("delete should succeed");

            let response = handler(
                make_request(repository, None, false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");

            let items: Vec<_> = collect_response(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();

            assert_eq!(items.len(), 1);
            assert_eq!(items[0].branch.as_ref().unwrap().name, "main");
        }))
        .await;
    }

    #[tokio::test]
    async fn list_includes_deleted_when_flag_set() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let to_delete =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;
            let deleted_latest = seed_revision(&repository_context, to_delete).await;
            branch::delete(repository_context.clone(), to_delete)
                .await
                .expect("delete should succeed");

            let response = handler(
                make_request(repository, None, true),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");

            let items: Vec<_> = collect_response(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();

            assert_eq!(items.len(), 2);
            let deleted_count = items
                .iter()
                .filter(|r| r.branch.as_ref().unwrap().deleted)
                .count();
            assert_eq!(deleted_count, 1);
            let deleted_branch = items
                .iter()
                .find(|r| r.branch.as_ref().unwrap().deleted)
                .unwrap()
                .branch
                .as_ref()
                .unwrap();
            assert_eq!(deleted_branch.name, "feature");
            assert_eq!(deleted_branch.creator, "bob");
            // An archive keeps the latest entry.
            assert_eq!(Hash::from(deleted_branch.latest.clone()), deleted_latest);
        }))
        .await;
    }

    #[tokio::test]
    async fn list_creator_filter_combines_with_include_deleted() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            // Three branches by alice (one deleted), one by bob.
            create_child_branch(
                &repository_context,
                "alice-live",
                "alice",
                main,
                main_latest,
            )
            .await;
            let alice_dead = create_child_branch(
                &repository_context,
                "alice-dead",
                "alice",
                main,
                main_latest,
            )
            .await;
            create_child_branch(&repository_context, "bob-live", "bob", main, main_latest).await;
            branch::delete(repository_context.clone(), alice_dead)
                .await
                .expect("delete should succeed");

            // include_deleted=false → only live alice branches (main + alice-live = 2)
            let live_only = handler(
                make_request(repository, Some("alice".into()), false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");
            let live_items: Vec<_> = collect_response(live_only)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();
            assert_eq!(live_items.len(), 2);
            assert!(
                live_items
                    .iter()
                    .all(|r| r.branch.as_ref().unwrap().creator == "alice")
            );
            assert!(
                live_items
                    .iter()
                    .all(|r| !r.branch.as_ref().unwrap().deleted)
            );

            // include_deleted=true → all alice branches including the deleted one (3)
            let with_deleted = handler(
                make_request(repository, Some("alice".into()), true),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");
            let all_items: Vec<_> = collect_response(with_deleted)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();
            assert_eq!(all_items.len(), 3);
            assert!(
                all_items
                    .iter()
                    .all(|r| r.branch.as_ref().unwrap().creator == "alice")
            );
            let dead_count = all_items
                .iter()
                .filter(|r| r.branch.as_ref().unwrap().deleted)
                .count();
            assert_eq!(dead_count, 1);
        }))
        .await;
    }

    #[tokio::test]
    async fn list_filters_by_creator() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            create_root_branch(&repository_context, "main", "alice").await;
            create_root_branch(&repository_context, "feature", "bob").await;
            create_root_branch(&repository_context, "extra", "alice").await;

            let response = handler(
                make_request(repository, Some("alice".into()), false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");

            let items: Vec<_> = collect_response(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();

            assert_eq!(items.len(), 2);
            assert!(
                items
                    .iter()
                    .all(|r| r.branch.as_ref().unwrap().creator == "alice")
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn list_delivers_items_incrementally() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            create_root_branch(&repository_context, "main", "alice").await;
            create_root_branch(&repository_context, "feature1", "alice").await;
            create_root_branch(&repository_context, "feature2", "bob").await;

            // The handler's Response is returned before the producer
            // task has touched the mutable store; pulling one item at a
            // time proves incremental delivery rather than batched
            // buffering of the full result.
            let response = handler(
                make_request(repository, None, false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");

            let mut stream = response.into_inner();
            let first = stream.next().await.expect("first item ready");
            first.expect("first item ok");

            let second = stream.next().await.expect("second item ready");
            second.expect("second item ok");

            let third = stream.next().await.expect("third item ready");
            third.expect("third item ok");

            assert!(stream.next().await.is_none(), "expected end of stream");
        }))
        .await;
    }

    /// Waits, for a bounded time, until the gated loads in flight satisfy
    /// `predicate`.
    async fn wait_for_in_flight(
        gated: &crate::store::test_support::LoadGate,
        predicate: impl FnMut(&usize) -> bool,
        expected: &str,
    ) {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            gated.wait_for_in_flight(predicate),
        )
        .await
        .unwrap_or_else(|_elapsed| panic!("timed out waiting for {expected}"));
    }

    /// Gives spawned tasks a chance to start. If a load beyond the limit has a
    /// gated first read, it would reach that gate during this time.
    async fn let_spawned_tasks_run() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    /// The listed branches as `(name, deleted)` pairs, sorted by name.
    async fn listed_branches(stream: BranchListStream) -> Vec<(String, bool)> {
        let items: Vec<_> =
            tokio::time::timeout(std::time::Duration::from_secs(30), stream.collect())
                .await
                .expect("listing finishes once the gate is open");
        let mut listed: Vec<(String, bool)> = items
            .into_iter()
            .map(|item| {
                let branch = item.expect("stream item ok").branch.expect("branch");
                (branch.name, branch.deleted)
            })
            .collect();
        listed.sort();
        listed
    }

    /// Creates `branch_count` live branches and starts listing them with the
    /// given concurrency, holding each branch's metadata record read at the gate.
    async fn start_gated_live_listing(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        branch_count: usize,
        concurrency: usize,
    ) -> (
        Arc<crate::store::test_support::GatedGetStore>,
        BranchListStream,
    ) {
        let repository = random::<RepositoryId>();
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let mut metadata_hashes = Vec::with_capacity(branch_count);
        for index in 0..branch_count {
            let id =
                create_root_branch(&repository_context, &format!("branch{index}"), "alice").await;
            let metadata_hash = branch::metadata_hash(repository_context.clone(), id)
                .await
                .expect("metadata hash");
            metadata_hashes.push(metadata_hash);
        }
        let gated =
            crate::store::test_support::GatedGetStore::new(immutable_store, metadata_hashes);
        let response = handler(
            make_request(repository, None, false),
            gated.clone(),
            mutable_store,
            &None, /* no forwarded requests */
            std::num::NonZeroUsize::new(concurrency).expect("nonzero concurrency"),
            &unshared_reads(),
        )
        .await
        .expect("handler returns a stream");
        (gated, response.into_inner())
    }

    /// Every branch is loading at once when the limit covers them all.
    #[tokio::test]
    async fn branch_loads_run_concurrently() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let (gated, stream) =
                start_gated_live_listing(immutable_store, mutable_store, 4, 4).await;

            wait_for_in_flight(&gated, |count| *count >= 4, "all four loads at once").await;
            gated.open();

            let listed = listed_branches(stream).await;
            let names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
            assert_eq!(names, ["branch0", "branch1", "branch2", "branch3"]);
        }))
        .await;
    }

    /// Each branch makes one gated load: its metadata record read. The number of
    /// gated loads in flight therefore equals the number of branches loading at
    /// once.
    #[tokio::test]
    async fn branch_loads_in_flight_never_exceed_the_concurrency_limit() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let (gated, stream) =
                start_gated_live_listing(immutable_store, mutable_store, 6, 2).await;

            wait_for_in_flight(&gated, |count| *count >= 2, "two loads at once").await;
            let_spawned_tasks_run().await;
            assert_eq!(gated.in_flight(), 2);
            gated.open();

            assert_eq!(listed_branches(stream).await.len(), 6);
            assert_eq!(gated.max_in_flight(), 2);
        }))
        .await;
    }

    /// Dropping the response stream cancels the loads in flight, even while
    /// none of them can finish.
    #[tokio::test]
    async fn disconnecting_client_cancels_loads_in_flight() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let (gated, stream) =
                start_gated_live_listing(immutable_store, mutable_store, 6, 2).await;
            wait_for_in_flight(&gated, |count| *count >= 2, "two loads at once").await;

            drop(stream);

            // The gate never opens, so only cancellation releases the loads.
            wait_for_in_flight(
                &gated,
                |count| *count == 0,
                "the held loads to be cancelled",
            )
            .await;
            assert_eq!(gated.max_in_flight(), 2);
        }))
        .await;
    }

    /// A listing reads each of the three branch key types with one eventually consistent range
    /// read, whatever its filter and whether it includes deleted branches, and makes no
    /// single-item mutable read.
    #[tokio::test]
    async fn listing_makes_one_eventual_range_read_per_key_type_and_no_loads() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            for (name, creator) in [("first", "alice"), ("second", "bob"), ("third", "alice")] {
                create_child_branch(&repository_context, name, creator, main, main_latest).await;
            }
            let deleted =
                create_child_branch(&repository_context, "deleted", "bob", main, main_latest).await;
            branch::delete(repository_context, deleted)
                .await
                .expect("delete should succeed");
            let recording = crate::store::test_support::RecordingReadStore::new(mutable_store);

            for (creator, include_deleted, listed_count) in [
                (None, false, 4),
                (None, true, 5),
                (Some("alice"), false, 3),
                (Some("bob"), true, 2),
            ] {
                let response = handler(
                    make_request(repository, creator.map(String::from), include_deleted),
                    immutable_store.clone(),
                    recording.clone(),
                    &None, /* no forwarded requests */
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &unshared_reads(),
                )
                .await
                .expect("handler returns a stream");

                assert_eq!(
                    listed_branches(response.into_inner()).await.len(),
                    listed_count
                );
                let mut listed = recording.take_listed();
                listed.sort_by_key(|(key_type, _consistency)| *key_type as u8);
                assert_eq!(
                    listed,
                    [
                        (KeyType::BranchMetadata, ReadConsistency::Eventual),
                        (KeyType::BranchId, ReadConsistency::Eventual),
                        (KeyType::BranchLatestPointer, ReadConsistency::Eventual),
                    ]
                );
                assert_eq!(recording.take_loaded(), []);
            }
        }))
        .await;
    }

    /// Calls for one repository within the window share one read of its branch entries, whatever
    /// their filter and whether they include deleted branches.
    #[tokio::test]
    async fn calls_with_different_filters_share_one_read() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            create_child_branch(&repository_context, "first", "bob", main, main_latest).await;
            let deleted =
                create_child_branch(&repository_context, "deleted", "bob", main, main_latest).await;
            branch::delete(repository_context, deleted)
                .await
                .expect("delete should succeed");
            let recording = crate::store::test_support::RecordingReadStore::new(mutable_store);
            let reads = shared_reads(std::time::Duration::from_secs(3600));

            for (creator, include_deleted, listed_count) in [
                (None, false, 2),
                (Some("bob"), true, 2),
                (None, true, 3),
                (Some("alice"), false, 1),
            ] {
                let response = handler(
                    make_request(repository, creator.map(String::from), include_deleted),
                    immutable_store.clone(),
                    recording.clone(),
                    &None, /* no forwarded requests */
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &reads,
                )
                .await
                .expect("handler returns a stream");
                assert_eq!(
                    listed_branches(response.into_inner()).await.len(),
                    listed_count
                );
            }

            assert_eq!(
                recording.take_listed().len(),
                3,
                "one read of each key type"
            );
            assert_eq!(recording.take_loaded(), []);
        }))
        .await;
    }

    /// The shared read joins the listed entries once: a call that reuses it computes no keys.
    #[tokio::test]
    async fn a_call_that_reuses_a_read_computes_no_keys() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            create_child_branch(&repository_context, "first", "bob", main, main_latest).await;
            let recording = crate::store::test_support::RecordingReadStore::new(mutable_store);
            let reads = shared_reads(std::time::Duration::from_secs(3600));

            for computes_keys in [true, false] {
                let response = handler(
                    make_request(repository, None, true),
                    immutable_store.clone(),
                    recording.clone(),
                    &None, /* no forwarded requests */
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &reads,
                )
                .await
                .expect("handler returns a stream");
                assert_eq!(listed_branches(response.into_inner()).await.len(), 2);
                assert_eq!(recording.take_stored_key_count() > 0, computes_keys);
            }
        }))
        .await;
    }

    /// The error of a read the store throttled ends every call in the window, though the store
    /// would answer the later calls.
    #[tokio::test]
    async fn a_throttled_read_s_error_ends_the_calls_in_the_window() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            create_root_branch(&repository_context, "main", "alice").await;
            let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let throttled_once = {
                let lists = lists.clone();
                lore_storage::test_util::HookedStore::wrap(
                    mutable_store.clone(),
                    move |call, _key, _key_type| {
                        if call == lore_storage::test_util::StoreCall::List
                            && lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
                        {
                            return Err(lore_storage::StoreError::from(lore_base::error::SlowDown));
                        }
                        Ok(())
                    },
                    None,
                )
            };
            let reads = shared_reads(std::time::Duration::from_secs(3600));

            for include_deleted in [false, true] {
                let response = handler(
                    make_request(repository, None, include_deleted),
                    immutable_store.clone(),
                    throttled_once.clone(),
                    &None, /* no forwarded requests */
                    DEFAULT_BRANCH_LIST_CONCURRENCY,
                    &reads,
                )
                .await
                .expect("handler returns a stream");

                let status = final_status(response).await;
                assert_eq!(status.code(), tonic::Code::ResourceExhausted);
                assert_eq!(status.message(), OVERLOADED);
            }
            assert!(
                lists.load(std::sync::atomic::Ordering::SeqCst) <= 3,
                "only the first call reads"
            );
        }))
        .await;
    }

    /// Deleted branches are loaded concurrently, and each branch is listed once,
    /// with live branches never reported as deleted.
    #[tokio::test]
    async fn deleted_branch_loads_run_concurrently() {
        const DELETED: usize = 6;
        const LIMIT: usize = 2;
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            create_child_branch(&repository_context, "kept", "alice", main, main_latest).await;
            let mut metadata_hashes = Vec::with_capacity(DELETED);
            for index in 0..DELETED {
                let id = create_child_branch(
                    &repository_context,
                    &format!("deleted{index}"),
                    "alice",
                    main,
                    main_latest,
                )
                .await;
                branch::delete(repository_context.clone(), id)
                    .await
                    .expect("delete should succeed");
                let metadata_hash = branch::metadata_hash(repository_context.clone(), id)
                    .await
                    .expect("metadata hash");
                metadata_hashes.push(metadata_hash);
            }
            // Only the deleted pass reads these records.
            let gated =
                crate::store::test_support::GatedGetStore::new(immutable_store, metadata_hashes);
            let response = handler(
                make_request(repository, None, true),
                gated.clone(),
                mutable_store,
                &None, /* no forwarded requests */
                std::num::NonZeroUsize::new(LIMIT).expect("nonzero concurrency"),
                &unshared_reads(),
            )
            .await
            .expect("handler returns a stream");

            wait_for_in_flight(&gated, |count| *count >= LIMIT, "two deleted loads at once").await;
            gated.open();

            assert_eq!(
                listed_branches(response.into_inner()).await,
                [
                    ("deleted0".to_string(), true),
                    ("deleted1".to_string(), true),
                    ("deleted2".to_string(), true),
                    ("deleted3".to_string(), true),
                    ("deleted4".to_string(), true),
                    ("deleted5".to_string(), true),
                    ("kept".to_string(), false),
                    ("main".to_string(), false),
                ]
            );
            assert_eq!(gated.max_in_flight(), LIMIT);
        }))
        .await;
    }

    #[tokio::test]
    async fn list_empty_repository_yields_empty_stream() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let response = handler(
                make_request(repository, None, false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("Request failed");

            let items: Vec<_> = collect_response(response).await;
            assert!(items.is_empty());
        }))
        .await;
    }
}

mod forwarded_request {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lore_server::grpc::forwarded_requests::ForwardedRequestResult;
    use lore_server::grpc::forwarded_requests::ForwardedRequests;
    use lore_server::grpc::forwarded_requests::InternalClientError;
    use lore_server::grpc::forwarded_requests::RpcFlags;
    use lore_server::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
    use tonic::Response;
    use tonic::Status;

    use super::*;

    /// Single-use client that returns a pre-configured result on its one call.
    struct SingleShotClient {
        response: Arc<Mutex<Option<ForwardedRequestResult<BranchListStream>>>>,
    }

    #[async_trait]
    impl ForwardedRevisionServiceClient for SingleShotClient {
        async fn branch_create(
            &mut self,
            _request: Request<lore_proto::lore::revision::v1::BranchCreateRequest>,
        ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchCreateResponse> {
            unreachable!("branch_create should not be called in branch_list tests")
        }

        async fn branch_delete(
            &mut self,
            _request: Request<lore_proto::lore::revision::v1::BranchDeleteRequest>,
        ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchDeleteResponse> {
            unreachable!("branch_delete should not be called in branch_list tests")
        }

        async fn branch_get(
            &mut self,
            _request: Request<lore_proto::lore::revision::v1::BranchGetRequest>,
        ) -> ForwardedRequestResult<lore_proto::lore::revision::v1::BranchGetResponse> {
            unreachable!("branch_get should not be called in branch_list tests")
        }

        async fn branch_list(
            &mut self,
            _request: Request<lore_proto::lore::revision::v1::BranchListRequest>,
        ) -> ForwardedRequestResult<BranchListStream> {
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("branch_list called more than once")
        }
    }

    struct StubForwardedRequests {
        flags: RpcFlags,
        response: Arc<Mutex<Option<ForwardedRequestResult<BranchListStream>>>>,
    }

    impl StubForwardedRequests {
        fn forwarding_enabled(response: ForwardedRequestResult<BranchListStream>) -> Arc<Self> {
            Arc::new(Self {
                flags: RpcFlags {
                    revision_branch_list: true,
                    ..Default::default()
                },
                response: Arc::new(Mutex::new(Some(response))),
            })
        }

        fn forwarding_disabled(response: ForwardedRequestResult<BranchListStream>) -> Arc<Self> {
            Arc::new(Self {
                flags: RpcFlags {
                    revision_branch_list: false,
                    ..Default::default()
                },
                response: Arc::new(Mutex::new(Some(response))),
            })
        }
    }

    impl ForwardedRequests for StubForwardedRequests {
        fn rpc_flags(&self) -> &RpcFlags {
            &self.flags
        }

        fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
            Box::new(SingleShotClient {
                response: Arc::clone(&self.response),
            })
        }

        fn forwarded_repository_service(
            &self,
        ) -> Box<dyn lore_server::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient>
{
            unreachable!("forwarded_repository_service should not be called in branch_list tests")
        }

        fn forwarded_lock_service(
            &self,
        ) -> Box<dyn lore_server::grpc::forwarded_requests::lock_service::ForwardedLockServiceClient>
        {
            unreachable!("forwarded_lock_service should not be called in branch_list tests")
        }
    }

    #[tokio::test]
    async fn delegates_to_remote_and_returns_stream() {
        // When the flag is enabled the remote stream is relayed directly;
        // branch_list_implementation is NOT called so the local store is not read.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let make_branch = |name: &str| lore_proto::lore::model::v1::Branch {
            name: name.into(),
            ..Default::default()
        };
        let stream: BranchListStream = Box::pin(tokio_stream::iter(vec![
            Ok(BranchListResponse {
                branch: Some(make_branch("remote-alpha")),
            }),
            Ok(BranchListResponse {
                branch: Some(make_branch("remote-beta")),
            }),
        ]));
        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(Ok(Ok(Response::new(stream))));

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let response = handler(
                make_request(repository, None, false),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("should succeed");

            let items: Vec<_> = collect_response(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();
            let names: Vec<&str> = items
                .iter()
                .map(|r| r.branch.as_ref().unwrap().name.as_str())
                .collect();
            assert_eq!(items.len(), 2);
            assert!(names.contains(&"remote-alpha"));
            assert!(names.contains(&"remote-beta"));
        }))
        .await;
    }

    #[tokio::test]
    async fn error_status_returned_to_caller() {
        // An error status from the forwarded server is forwarded directly to the original caller.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let forwarded_requests = StubForwardedRequests::forwarding_enabled(Ok(Err(
            Status::not_found("test error forwarded"),
        )));

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let err = handler(
                make_request(repository, None, false),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .map(|_| ())
            .expect_err("forwarded error should propagate");

            assert_eq!(err.code(), tonic::Code::NotFound);
            assert!(err.message().contains("test error forwarded"));
        }))
        .await;
    }

    #[tokio::test]
    async fn internal_client_error_maps_to_internal_status() {
        // A transport-level failure (InternalClientError) is mapped to Status::internal.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let forwarded_requests =
            StubForwardedRequests::forwarding_enabled(Err(InternalClientError::internal("oops")));

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let err = handler(
                make_request(repository, None, false),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .map(|_| ())
            .expect_err("transport error should become internal status");

            assert_eq!(err.code(), tonic::Code::Internal);
            assert!(err.message().contains("Error making forwarded request"));
        }))
        .await;
    }

    #[tokio::test]
    async fn flag_disabled_falls_through_to_local_execution() {
        // When revision_branch_list is false the local path runs, even if a
        // ForwardedRequests is present. The stub client is not called.
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        // response is irrelevant — client must never be called
        let forwarded_result: ForwardedRequestResult<BranchListStream> =
            Ok(Err(Status::internal("should not be called")));
        let forwarded_requests = StubForwardedRequests::forwarding_disabled(forwarded_result);

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            create_root_branch(&repository_context, "local-branch", "alice").await;

            let response = handler(
                make_request(repository, None, false),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
                DEFAULT_BRANCH_LIST_CONCURRENCY,
                &unshared_reads(),
            )
            .await
            .expect("local execution should succeed");

            let items: Vec<_> = collect_response(response)
                .await
                .into_iter()
                .map(|r| r.expect("stream item ok"))
                .collect();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].branch.as_ref().unwrap().name, "local-branch");
        }))
        .await;
    }
}
