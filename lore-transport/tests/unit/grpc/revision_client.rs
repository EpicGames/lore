// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::types::RepositoryId;
use lore_proto::lore::model::v1 as model;
use lore_proto::lore::revision::v1 as proto;
use lore_proto::lore::revision::v1::revision_service_server::RevisionService as RevisionServiceV1;
use lore_proto::lore::revision::v1::revision_service_server::RevisionServiceServer;
use lore_transport::grpc::GRPCAuth;
use lore_transport::grpc::revision_client::RevisionService;
use lore_transport::grpc::revision_client::collect_branch_list;
use lore_transport::grpc::revision_client::first_throttle_reason;
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::service::Routes;

use crate::types::full_branch;

/// The first throttled answer of a call gives the remote's reason. A later answer of the same
/// call, an answer with no reason, and an answer with another code give none.
#[test]
fn first_throttle_reason_is_given_once_and_only_with_a_reason() {
    let mut retry = lore_transport::util::retry(1, 1, 3);
    let throttled = Status::resource_exhausted("use branch info");

    assert_eq!(
        first_throttle_reason(&retry, &throttled),
        Some("use branch info")
    );
    assert_eq!(
        first_throttle_reason(&retry, &Status::resource_exhausted("")),
        None
    );
    assert_eq!(
        first_throttle_reason(&retry, &Status::internal("use branch info")),
        None
    );

    retry.next_interval();
    assert_eq!(first_throttle_reason(&retry, &throttled), None);
}

fn branch(index: usize) -> Result<proto::BranchListResponse, tonic::Status> {
    Ok(proto::BranchListResponse {
        branch: Some(model::Branch {
            name: index.to_string(),
            ..Default::default()
        }),
    })
}

#[tokio::test]
async fn exact_limit_preserves_all_branches() {
    let result = collect_branch_list(tokio_stream::iter((0..10_000).map(branch)), Some(10_000))
        .await
        .unwrap()
        .expect("complete listing");
    assert_eq!(result.list.len(), 10_000);
    assert_eq!(result.list.first().unwrap().name, "0");
    assert_eq!(result.list.last().unwrap().name, "9999");
}

#[tokio::test]
async fn over_limit_stops_consuming_the_stream() {
    let consumed = AtomicUsize::new(0);
    let stream = tokio_stream::iter((0..20_000).map(branch)).map(|item| {
        consumed.fetch_add(1, Ordering::Relaxed);
        item
    });
    let result = collect_branch_list(stream, Some(10_000)).await.unwrap();
    assert!(result.is_none(), "must report incomplete enumeration");
    assert_eq!(consumed.load(Ordering::Relaxed), 10_001);
}

#[tokio::test]
async fn empty_and_zero_limit_are_distinguished_from_over_limit() {
    let empty = collect_branch_list(tokio_stream::iter(std::iter::empty()), Some(0))
        .await
        .unwrap()
        .unwrap();
    assert!(empty.list.is_empty());
    assert!(
        collect_branch_list(tokio_stream::iter([branch(0)]), Some(0))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stream_failure_does_not_return_a_partial_listing() {
    let result = collect_branch_list(
        tokio_stream::iter([branch(0), Err(tonic::Status::unavailable("stream failed"))]),
        Some(10_000),
    )
    .await;
    assert_eq!(result.err().unwrap().code(), tonic::Code::Unavailable);
}

#[tokio::test]
async fn unbounded_listing_still_returns_every_branch() {
    let result = collect_branch_list(tokio_stream::iter((0..10_001).map(branch)), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.list.len(), 10_001);
}

#[tokio::test]
async fn absent_branch_messages_do_not_spend_branch_budget() {
    let result = collect_branch_list(
        tokio_stream::iter([Ok(proto::BranchListResponse { branch: None }), branch(0)]),
        Some(1),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.list.len(), 1);
    assert_eq!(result.list[0].name, "0");
}

/// A revision server that answers each branch get and create with [`Self::branch`], and records
/// each branch listing request and answers it with no branches. It refuses every other request.
struct BranchServer {
    branch: model::Branch,
    listed: Arc<Mutex<Vec<proto::BranchListRequest>>>,
    /// The number of branch listing requests still to answer with `RESOURCE_EXHAUSTED`.
    throttles: AtomicUsize,
}

#[tonic::async_trait]
impl RevisionServiceV1 for BranchServer {
    async fn branch_create(
        &self,
        _: Request<proto::BranchCreateRequest>,
    ) -> Result<Response<proto::BranchCreateResponse>, Status> {
        Ok(Response::new(proto::BranchCreateResponse {
            branch: Some(self.branch.clone()),
        }))
    }

    async fn branch_delete(
        &self,
        _: Request<proto::BranchDeleteRequest>,
    ) -> Result<Response<proto::BranchDeleteResponse>, Status> {
        Err(Status::unimplemented("not used by this test"))
    }

    async fn branch_get(
        &self,
        _: Request<proto::BranchGetRequest>,
    ) -> Result<Response<proto::BranchGetResponse>, Status> {
        Ok(Response::new(proto::BranchGetResponse {
            branch: Some(self.branch.clone()),
        }))
    }

    type BranchListStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<proto::BranchListResponse, Status>>>;

    async fn branch_list(
        &self,
        request: Request<proto::BranchListRequest>,
    ) -> Result<Response<Self::BranchListStream>, Status> {
        self.listed
            .lock()
            .expect("the listed requests' lock")
            .push(request.into_inner());
        let throttled = self
            .throttles
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            });
        // As Lore Server does, a throttled listing answers with a stream that carries the status.
        let items = if throttled.is_ok() {
            vec![Err(Status::resource_exhausted("use branch info"))]
        } else {
            Vec::new()
        };
        Ok(Response::new(tokio_stream::iter(items)))
    }

    async fn branch_push(
        &self,
        _: Request<proto::BranchPushRequest>,
    ) -> Result<Response<proto::BranchPushResponse>, Status> {
        Err(Status::unimplemented("not used by this test"))
    }

    async fn branch_metadata_get(
        &self,
        _: Request<proto::BranchMetadataGetRequest>,
    ) -> Result<Response<proto::BranchMetadataGetResponse>, Status> {
        Err(Status::unimplemented("not used by this test"))
    }

    async fn branch_metadata_set(
        &self,
        _: Request<proto::BranchMetadataSetRequest>,
    ) -> Result<Response<proto::BranchMetadataSetResponse>, Status> {
        Err(Status::unimplemented("not used by this test"))
    }

    async fn revision_list(
        &self,
        _: Request<proto::RevisionListRequest>,
    ) -> Result<Response<proto::RevisionListResponse>, Status> {
        Err(Status::unimplemented("not used by this test"))
    }
}

/// A revision client over a [`BranchServer`] that answers with `branch`, and the listing requests
/// that server records.
async fn start_branch_service(
    branch: model::Branch,
) -> (RevisionService, Arc<Mutex<Vec<proto::BranchListRequest>>>) {
    start_throttled_branch_service(branch, 0).await
}

/// [`start_branch_service`] with a server that answers its first `throttles` branch listing
/// requests with a stream that carries `RESOURCE_EXHAUSTED`.
async fn start_throttled_branch_service(
    branch: model::Branch,
    throttles: usize,
) -> (RevisionService, Arc<Mutex<Vec<proto::BranchListRequest>>>) {
    let server = BranchServer {
        branch,
        listed: Arc::default(),
        throttles: AtomicUsize::new(throttles),
    };
    let listed = server.listed.clone();
    let (_, channel) = super::serve_logged(Routes::new(RevisionServiceServer::new(server))).await;
    let auth = Arc::new(parking_lot::RwLock::new(GRPCAuth::default()));
    (
        RevisionService::new(channel, RepositoryId::default(), auth),
        listed,
    )
}

#[tokio::test]
async fn branch_query_returns_every_field_of_the_answer() {
    let (branch, expected) = full_branch();
    let (service, _) = start_branch_service(branch).await;

    let record = service
        .branch_query(None, Some("feature"))
        .await
        .expect("the branch's record");

    assert_eq!(record, expected);
}

#[tokio::test]
async fn a_query_by_id_keeps_the_queried_id_where_the_answer_has_none() {
    let (mut branch, expected) = full_branch();
    branch.id = Default::default();
    let (service, _) = start_branch_service(branch).await;

    let record = service
        .branch_query(Some(expected.id), None)
        .await
        .expect("the branch's record");

    assert_eq!(record, expected);
}

#[tokio::test]
async fn branch_create_returns_every_field_of_the_answer() {
    let (branch, expected) = full_branch();
    let (service, _) = start_branch_service(branch).await;

    let record = service
        .branch_create(expected.id, "feature", "release", "alice", &expected.stack)
        .await
        .expect("the created branch's record");

    assert_eq!(record, expected);
}

/// A throttled branch listing is sent again, and the call answers with the list of the next
/// answer.
#[tokio::test]
async fn branch_list_sends_a_throttled_listing_again() {
    let (service, listed) = start_throttled_branch_service(model::Branch::default(), 1).await;

    let list = service
        .branch_list(None, false)
        .await
        .expect("the listing after the throttled answer");

    assert!(list.list.is_empty());
    assert_eq!(listed.lock().expect("the listed requests' lock").len(), 2);
}

#[tokio::test]
async fn branch_list_sends_its_creator_and_include_deleted() {
    let (service, listed) = start_branch_service(model::Branch::default()).await;

    service
        .branch_list(Some("alice"), true)
        .await
        .expect("a filtered listing");
    service
        .branch_list(None, false)
        .await
        .expect("an unfiltered listing");

    assert_eq!(
        *listed.lock().expect("the listed requests' lock"),
        [
            proto::BranchListRequest {
                creator: Some("alice".to_string()),
                include_deleted: true,
            },
            proto::BranchListRequest {
                creator: None,
                include_deleted: false,
            },
        ]
    );
}
