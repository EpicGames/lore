// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A branch listing the remote throttles logs one warning with the remote's reason, and then
//! answers with the list of a later answer.
//!
//! In its own binary: it installs the process-wide log callback.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::log::LoreLogLevel;
use lore_base::types::RepositoryId;
use lore_proto::lore::revision::v1 as proto;
use lore_proto::lore::revision::v1::revision_service_server::RevisionService as RevisionServiceV1;
use lore_proto::lore::revision::v1::revision_service_server::RevisionServiceServer;
use lore_transport::grpc::GRPCAuth;
use lore_transport::grpc::RequestLoggerLayer;
use lore_transport::grpc::revision_client::RevisionService;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tower::ServiceBuilder;

const REASON: &str = "list one branch with branch info";

static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Keeps each warning.
fn capture(level: LoreLogLevel, _location: &str, message: &str) {
    if level == LoreLogLevel::Warn {
        WARNINGS
            .lock()
            .expect("the warnings' lock")
            .push(message.to_string());
    }
}

/// A revision server that answers its first [`Self::throttles`] branch listings as Lore Server
/// answers a throttled one, with a stream that carries `RESOURCE_EXHAUSTED` and [`REASON`], and
/// each later listing with no branches. It refuses every other request.
struct ThrottlingServer {
    throttles: AtomicUsize,
    listings: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl RevisionServiceV1 for ThrottlingServer {
    async fn branch_create(
        &self,
        _: Request<proto::BranchCreateRequest>,
    ) -> Result<Response<proto::BranchCreateResponse>, Status> {
        Err(Status::unimplemented("not used by this test"))
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
        Err(Status::unimplemented("not used by this test"))
    }

    type BranchListStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<proto::BranchListResponse, Status>>>;

    async fn branch_list(
        &self,
        _: Request<proto::BranchListRequest>,
    ) -> Result<Response<Self::BranchListStream>, Status> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        let throttled = self
            .throttles
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            });
        let items = if throttled.is_ok() {
            vec![Err(Status::resource_exhausted(REASON))]
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

/// A revision client over a [`ThrottlingServer`] that throttles its first `throttles` branch
/// listings, and the count of the listings that server gets.
async fn start_throttling_service(throttles: usize) -> (RevisionService, Arc<AtomicUsize>) {
    let listings = Arc::new(AtomicUsize::new(0));
    let server = ThrottlingServer {
        throttles: AtomicUsize::new(throttles),
        listings: listings.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener");
    let address = listener.local_addr().expect("the listener's address");
    lore_base::lore_spawn!(
        tonic::transport::Server::builder()
            .add_service(RevisionServiceServer::new(server))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let channel = tonic::transport::Channel::from_shared(format!("http://{address}"))
        .expect("the server's URL")
        .connect()
        .await
        .expect("connect to the test server");
    let channel = ServiceBuilder::new()
        .layer(RequestLoggerLayer {})
        .service(channel);
    let auth = Arc::new(parking_lot::RwLock::new(GRPCAuth::default()));
    (
        RevisionService::new(channel, RepositoryId::default(), auth),
        listings,
    )
}

/// **A listing throttled twice logs one warning, with the remote's reason, and answers with the
/// third answer's list.** The second throttled answer of the same call adds no warning.
#[tokio::test]
async fn a_throttled_listing_warns_once_with_the_remotes_reason() {
    lore_base::log::set_log_level(LoreLogLevel::Warn);
    lore_base::log::set_log_callback(Some(capture));
    let (service, listings) = start_throttling_service(2).await;

    let list = service
        .branch_list(None, false)
        .await
        .expect("the listing after the throttled answers");

    assert!(list.list.is_empty());
    assert_eq!(listings.load(Ordering::SeqCst), 3);
    let warnings = WARNINGS.lock().expect("the warnings' lock");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(REASON), "{warnings:?}");
}
