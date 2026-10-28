// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::revision::v1::branch_list::*;
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
        branch,
        state_hash,
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
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
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;

            let response = handler(
                make_request(repository, None, false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
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

    /// A throttled store must end the stream with `RESOURCE_EXHAUSTED`
    /// rather than completing successfully with that branch absent: a
    /// client cannot tell an omitted branch from one that does not exist,
    /// so a silently short listing is worse than an error.
    #[tokio::test]
    async fn throttled_branch_record_ends_the_stream_rather_than_omitting_it() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let feature =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;

            // Throttle only the branch-latest lookup that emit_branch makes
            // for `feature`. Listing the branch ids and reading its
            // metadata still succeed, so the request reaches emit_branch.
            let (latest_key, _key_type) = branch::mutable_key(
                lore_revision::repository::SALT_LORE,
                branch::LATEST,
                repository,
                feature,
            );
            let throttled = crate::store::test_support::FailingLoadStore::for_key(
                mutable_store,
                latest_key,
                lore_storage::StoreError::from(lore_base::error::SlowDown),
            );

            let response = handler(
                make_request(repository, None, false),
                immutable_store,
                throttled,
                &None, /* no forwarded requests */
            )
            .await
            .expect("handler returns a stream");

            let items = collect_response(response).await;
            let status = items
                .iter()
                .find_map(|item| item.as_ref().err())
                .expect("stream must carry the backpressure signal");
            assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        }))
        .await;
    }

    #[tokio::test]
    async fn unreadable_latest_preserves_other_branches_and_defaults_to_zero() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let feature =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;

            // Fail only the branch-latest lookup that emit_branch makes
            // for `feature`. Listing the branch ids and reading its
            // metadata still succeed, so the request reaches emit_branch.
            let (latest_key, _key_type) = branch::mutable_key(
                lore_revision::repository::SALT_LORE,
                branch::LATEST,
                repository,
                feature,
            );
            let failing = crate::store::test_support::FailingLoadStore::for_key(
                mutable_store,
                latest_key,
                lore_storage::StoreError::internal("injected latest failure"),
            );

            let response = handler(
                make_request(repository, None, false),
                immutable_store,
                failing,
                &None, /* no forwarded requests */
            )
            .await
            .expect("handler returns a stream");

            let items = collect_response(response).await;
            let branches: Vec<_> = items
                .into_iter()
                .map(|item| {
                    item.expect("an unreadable latest must not terminate listing")
                        .branch
                        .expect("branch response")
                })
                .collect();
            assert_eq!(branches.len(), 2);
            let main = branches
                .iter()
                .find(|branch| branch.name == "main")
                .expect("healthy branch remains available");
            assert_eq!(Hash::from(main.latest.clone()), main_latest);
            let feature = branches
                .iter()
                .find(|branch| branch.name == "feature")
                .expect("branch metadata remains available despite unreadable latest");
            assert_eq!(Hash::from(feature.latest.clone()), Hash::default());
        }))
        .await;
    }

    #[tokio::test]
    async fn unreadable_metadata_hash_omits_only_the_broken_branch() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let feature =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;

            // The branch id is listed, but its metadata hash cannot be read.
            let (metadata_key, _key_type) = branch::mutable_key(
                lore_revision::repository::SALT_LORE,
                branch::METADATA,
                repository,
                feature,
            );
            let failing = crate::store::test_support::FailingLoadStore::for_key(
                mutable_store,
                metadata_key,
                lore_storage::StoreError::internal("injected metadata failure"),
            );

            let response = handler(
                make_request(repository, None, false),
                immutable_store,
                failing,
                &None, /* no forwarded requests */
            )
            .await
            .expect("handler returns a stream");

            let items = collect_response(response).await;
            let branches: Vec<_> = items
                .into_iter()
                .map(|item| {
                    item.expect("unreadable metadata must not terminate listing")
                        .branch
                        .expect("branch response")
                })
                .collect();
            assert_eq!(branches.len(), 1);
            assert_eq!(branches[0].name, "main");
            assert_eq!(Hash::from(branches[0].latest.clone()), main_latest);
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
            ));
            let main = create_root_branch(&repository_context, "main", "alice").await;
            let main_latest = seed_revision(&repository_context, main).await;
            let to_delete =
                create_child_branch(&repository_context, "feature", "bob", main, main_latest).await;
            branch::delete(repository_context.clone(), to_delete)
                .await
                .expect("delete should succeed");

            let response = handler(
                make_request(repository, None, true),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
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
            ));
            create_root_branch(&repository_context, "main", "alice").await;
            create_root_branch(&repository_context, "feature", "bob").await;
            create_root_branch(&repository_context, "extra", "alice").await;

            let response = handler(
                make_request(repository, Some("alice".into()), false),
                immutable_store.clone(),
                mutable_store.clone(),
                &None, /* no forwarded requests */
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
            ));
            create_root_branch(&repository_context, "local-branch", "alice").await;

            let response = handler(
                make_request(repository, None, false),
                immutable_store,
                mutable_store,
                &Some(forwarded_requests as Arc<dyn ForwardedRequests>),
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
