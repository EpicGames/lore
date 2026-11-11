// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use dashmap::DashMap;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_proto::lore::storage::v1::storage_service_server::StorageService as StorageServiceV1;
use lore_proto::lore::storage::v1::storage_service_server::StorageServiceServer;
use lore_transport::auth::AuthEndpoint;
use lore_transport::error::ProtocolError;
use lore_transport::grpc::storage_client::*;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::Streaming;
use tonic::service::Routes;

const TEST_PAYLOAD: &[u8] = b"rotation test payload";

/// How a test server fails: the first `Get` stream it accepts, or every unary request.
///
/// [`CleanEnd`](Self::CleanEnd) and [`TerminalStatus`](Self::TerminalStatus) both leave requests
/// unanswered with no per-request attribution, but they reach the client through different code: a
/// clean end is `Streaming` reporting exhaustion, a status is the terminal `Err` arm. Real
/// connection failures take the latter shape.
#[derive(Clone, Copy)]
enum KillMode {
    /// End the response body with nothing on it.
    CleanEnd,
    /// Terminate with the code an h2 connection failure produces.
    TerminalStatus,
    /// Never open the RPC at all, on any attempt.
    RefuseOpen,
    /// Refuse the first open, then serve normally — a channel that was down and came back.
    RefuseFirstOpen,
    /// Answer with a populated fragment and payload *and* a failure status.
    ErrorBesidePayload,
    /// Answer every unary request with this code.
    RefuseUnary(tonic::Code),
}

/// Kills the first `Get` stream it accepts, then serves every later stream normally. Records the
/// target context of each copy it reads, and answers a copy after [`COPY_DELAY`].
struct StreamKillingServer {
    accepted: Arc<AtomicUsize>,
    requests_read: Arc<AtomicUsize>,
    kill_mode: KillMode,
    copied: Arc<Mutex<Vec<Bytes>>>,
}

/// How long the test server holds a copy before answering it, long enough for a concurrent
/// copy to be requested while the first is on the wire.
const COPY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

impl StreamKillingServer {
    /// The answer to a unary request: the code of [`KillMode::RefuseUnary`], else `Unimplemented`.
    fn unary_answer(&self) -> Status {
        match self.kill_mode {
            KillMode::RefuseUnary(code) => Status::new(code, "refused by the test server"),
            _ => Status::unimplemented("not used by this test"),
        }
    }
}

#[tonic::async_trait]
impl StorageServiceV1 for StreamKillingServer {
    type GetStream = ResponseStream<storage_v1::GetResponse>;

    async fn get(
        &self,
        request: Request<Streaming<model_v1::Address>>,
    ) -> Result<Response<Self::GetStream>, Status> {
        let attempt = self.accepted.fetch_add(1, Ordering::SeqCst);
        let kill_mode = self.kill_mode;
        if let KillMode::RefuseOpen = kill_mode {
            return Err(Status::unavailable("refusing to open"));
        }
        if attempt == 0 && matches!(kill_mode, KillMode::RefuseFirstOpen) {
            return Err(Status::unavailable("refusing the first open"));
        }
        let mut requests = request.into_inner();
        let requests_read = self.requests_read.clone();

        let stream = async_stream::stream! {
            if attempt == 0 {
                if let KillMode::TerminalStatus = kill_mode {
                    yield Err(Status::internal("connection reset"));
                }
                return;
            }
            while let Some(Ok(address)) = requests.next().await {
                requests_read.fetch_add(1, Ordering::SeqCst);
                let status = if matches!(kill_mode, KillMode::ErrorBesidePayload) {
                    lore_proto::lore::model::v1::ItemStatus {
                        code: i32::from(tonic::Code::NotFound) as u32,
                        message: "gone".to_string(),
                    }
                } else {
                    lore_proto::lore::model::v1::ItemStatus::ok()
                };
                yield Ok(storage_v1::GetResponse {
                    address: Some(address),
                    fragment: Some(model_v1::Fragment {
                        flags: 0,
                        size_payload: TEST_PAYLOAD.len() as u32,
                        size_content: TEST_PAYLOAD.len() as u64,
                    }),
                    payload: Bytes::from_static(TEST_PAYLOAD),
                    status: Some(status),
                });
            }
        };

        Ok(Response::new(Box::pin(stream) as Self::GetStream))
    }

    type GetMetadataStream = ResponseStream<storage_v1::GetResponse>;

    async fn get_metadata(
        &self,
        _request: Request<Streaming<model_v1::Address>>,
    ) -> Result<Response<Self::GetMetadataStream>, Status> {
        Err(Status::unimplemented("not used by this test"))
    }

    type GetResolvedStream = ResponseStream<storage_v1::GetResolvedResponse>;

    async fn get_resolved(
        &self,
        request: Request<Streaming<storage_v1::GetResolvedRequest>>,
    ) -> Result<Response<Self::GetResolvedStream>, Status> {
        let mut requests = request.into_inner();
        let stream = async_stream::stream! {
            while let Some(Ok(req)) = requests.next().await {
                yield Ok(storage_v1::GetResolvedResponse {
                    request_id: req.request_id,
                    ..Default::default()
                });
            }
        };
        Ok(Response::new(Box::pin(stream) as Self::GetResolvedStream))
    }

    type PutResolvedStream = ResponseStream<storage_v1::PutResolvedResponse>;

    async fn put_resolved(
        &self,
        request: Request<Streaming<storage_v1::PutResolvedRequest>>,
    ) -> Result<Response<Self::PutResolvedStream>, Status> {
        let mut requests = request.into_inner();
        let stream = async_stream::stream! {
            while let Some(Ok(req)) = requests.next().await {
                yield Ok(storage_v1::PutResolvedResponse {
                    request_id: req.request_id,
                    ..Default::default()
                });
            }
        };
        Ok(Response::new(Box::pin(stream) as Self::PutResolvedStream))
    }

    type PutStream = ResponseStream<storage_v1::PutResponse>;

    async fn put(
        &self,
        request: Request<Streaming<storage_v1::PutRequest>>,
    ) -> Result<Response<Self::PutStream>, Status> {
        let mut requests = request.into_inner();
        let stream = async_stream::stream! {
            while let Some(Ok(req)) = requests.next().await {
                yield Ok(storage_v1::PutResponse {
                    address: req.address,
                    status: None,
                });
            }
        };
        Ok(Response::new(Box::pin(stream) as Self::PutStream))
    }

    type CopyStream = ResponseStream<storage_v1::CopyResponse>;

    async fn copy(
        &self,
        request: Request<Streaming<storage_v1::CopyRequest>>,
    ) -> Result<Response<Self::CopyStream>, Status> {
        let copied = self.copied.clone();
        let mut requests = request.into_inner();
        let stream = async_stream::stream! {
            while let Some(Ok(req)) = requests.next().await {
                copied.lock().unwrap().push(req.target_context.clone());
                tokio::time::sleep(COPY_DELAY).await;
                yield Ok(storage_v1::CopyResponse {
                    source_repository_id: req.source_repository_id,
                    source_address: req.source_address,
                    status: None,
                });
            }
        };
        Ok(Response::new(Box::pin(stream) as Self::CopyStream))
    }

    async fn query(
        &self,
        _request: Request<storage_v1::QueryRequest>,
    ) -> Result<Response<storage_v1::QueryResponse>, Status> {
        Err(self.unary_answer())
    }

    async fn verify(
        &self,
        _request: Request<storage_v1::VerifyRequest>,
    ) -> Result<Response<storage_v1::VerifyResponse>, Status> {
        Err(self.unary_answer())
    }

    async fn mutable_load(
        &self,
        _request: Request<storage_v1::MutableLoadRequest>,
    ) -> Result<Response<storage_v1::MutableLoadResponse>, Status> {
        Err(self.unary_answer())
    }

    async fn mutable_store(
        &self,
        _request: Request<storage_v1::MutableStoreRequest>,
    ) -> Result<Response<storage_v1::MutableStoreResponse>, Status> {
        Err(self.unary_answer())
    }

    async fn mutable_compare_and_swap(
        &self,
        _request: Request<storage_v1::MutableCompareAndSwapRequest>,
    ) -> Result<Response<storage_v1::MutableCompareAndSwapResponse>, Status> {
        Err(self.unary_answer())
    }
}

fn test_context() -> GrpcSessionContext {
    GrpcSessionContext {
        partition: Partition::from(Context::from([0x11u8; 16])),
        correlation_id: "rotation-test".to_string(),
        auth_token: String::new(),
    }
}

/// Stand up a `StorageService` talking to a `StreamKillingServer`, and hand back the
/// counter of streams the server has accepted.
async fn start_test_service(
    kill_mode: KillMode,
) -> (StorageService, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (connection, accepted, requests_read, _) = start_test_connection(kill_mode).await;
    (StorageService::new(connection), accepted, requests_read)
}

/// Stand up a `StorageService` over the test server, and hand back the target contexts of the
/// copies the server reads.
async fn start_copy_service() -> (StorageService, Arc<Mutex<Vec<Bytes>>>) {
    let (connection, _, _, copied) = start_test_connection(KillMode::CleanEnd).await;
    (StorageService::new(connection), copied)
}

/// The same server and a connection over it, for tests that drive the connection directly
/// rather than through a storage client.
async fn start_test_connection(
    kill_mode: KillMode,
) -> (
    Arc<lore_transport::grpc::GRPCConnection>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<Bytes>>>,
) {
    let accepted = Arc::new(AtomicUsize::new(0));
    let requests_read = Arc::new(AtomicUsize::new(0));
    let copied = Arc::new(Mutex::new(vec![]));

    let server = StreamKillingServer {
        accepted: accepted.clone(),
        requests_read: requests_read.clone(),
        kill_mode,
        copied: copied.clone(),
    };
    let (addr, channel) = super::serve_logged(Routes::new(StorageServiceServer::new(server))).await;

    let connection = Arc::new(lore_transport::grpc::GRPCConnection::for_test(
        format!("http://{addr}").parse().expect("test url"),
        channel,
    ));
    (connection, accepted, requests_read, copied)
}

/// Drive one `get` against a server that kills its first stream, and report how many
/// streams the server ended up accepting.
async fn get_against_stream_killing_server(
    kill_mode: KillMode,
) -> (Result<(Fragment, Bytes), ProtocolError>, usize) {
    let (service, accepted, _) = start_test_service(kill_mode).await;
    let ctx = test_context();
    let address = Address::zero_context_hash(Hash::from([0x22u8; 32]));

    let result = service.get(0, &ctx, &address).await;

    (result, accepted.load(Ordering::SeqCst))
}

/// A stream that ends without answering is re-established and the request replayed.
///
/// The caller must see one successful fetch, not a transport error. Before, the dead channel
/// stayed cached and every later request on the session failed against it.
#[tokio::test]
async fn get_rotates_when_the_first_stream_ends_without_answering() {
    let (result, streams) = get_against_stream_killing_server(KillMode::CleanEnd).await;
    let (fragment, payload) = result.expect("get must recover by re-establishing the stream");

    assert_eq!(payload.as_ref(), TEST_PAYLOAD);
    assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
    assert_eq!(
        streams, 2,
        "the server must have seen a second Get stream — the first was killed",
    );
}

/// The same recovery when the stream dies with a terminal status rather than ending.
///
/// This is the shape a real connection failure takes, and the codes it carries — `Internal`,
/// `Cancelled` — are not `Disconnected` on their own. Unless a stream status is treated as a
/// stream death regardless of its code, the request is handed back to the caller as a plain
/// error and the rotation never happens.
#[tokio::test]
async fn get_rotates_when_the_first_stream_dies_with_a_terminal_status() {
    let (result, streams) = get_against_stream_killing_server(KillMode::TerminalStatus).await;
    let (fragment, payload) = result.expect("get must recover by re-establishing the stream");

    assert_eq!(payload.as_ref(), TEST_PAYLOAD);
    assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
    assert_eq!(
        streams, 2,
        "an Internal stream status must rotate onto a new stream, not fail the request",
    );
}

/// Many requests losing a stream together must cost exactly one replacement.
///
/// Single-flight rotation rests on `Arc::ptr_eq` against the cached handle under the shard
/// lock. If that check regressed, every waiter would open its own stream — still correct, so
/// nothing else would fail, but the stream count would scale with concurrency.
#[tokio::test]
async fn concurrent_requests_share_one_replacement_stream() {
    let (service, accepted, _) = start_test_service(KillMode::CleanEnd).await;
    let ctx = test_context();

    let addresses: Vec<Address> = (0..16u8)
        .map(|i| Address::zero_context_hash(Hash::from([i; 32])))
        .collect();
    let results = futures::future::join_all(
        addresses
            .iter()
            .map(|address| service.get(0, &ctx, address)),
    )
    .await;

    for result in &results {
        assert!(result.is_ok(), "every request must recover: {result:?}");
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        2,
        "the killed stream must be replaced once for all waiters, not once each",
    );
}

/// Concurrent requests for one address ride a single round trip.
///
/// `register` returns false for later waiters on a key so they attach to the request already
/// on the wire. A regression here costs duplicate wire traffic on duplicate reads without
/// failing anything, so only the server's request count catches it. The warm-up get spends
/// the killed first stream, leaving a live one for the coalescing itself.
#[tokio::test]
async fn concurrent_requests_for_one_address_coalesce() {
    let (service, _, requests_read) = start_test_service(KillMode::CleanEnd).await;
    let ctx = test_context();
    let address = Address::zero_context_hash(Hash::from([0x44u8; 32]));

    service
        .get(
            0,
            &ctx,
            &Address::zero_context_hash(Hash::from([0x45u8; 32])),
        )
        .await
        .expect("warm-up get");
    requests_read.store(0, Ordering::SeqCst);

    let (first, second) = tokio::join!(
        service.get(0, &ctx, &address),
        service.get(0, &ctx, &address)
    );

    assert_eq!(first.expect("first get").1.as_ref(), TEST_PAYLOAD);
    assert_eq!(second.expect("second get").1.as_ref(), TEST_PAYLOAD);
    assert_eq!(
        requests_read.load(Ordering::SeqCst),
        1,
        "duplicate addresses must coalesce onto one round trip",
    );
}

/// Concurrent callers losing one channel must rebuild it once between them.
///
/// This is the real machinery, not a stand-in: `GRPCConnection::reconnect` serialises callers
/// on `reconnector` and then compares the epoch each carries against the current one, so the
/// first through does the connect and the rest adopt its channel. The epoch delta is the
/// observable — it advances once per actual rebuild, so N concurrent callers that each
/// rebuilt would leave it N higher.
#[tokio::test]
async fn concurrent_reconnects_rebuild_the_channel_once() {
    let (connection, _, _, _) = start_test_connection(KillMode::CleanEnd).await;
    let epoch_before = connection.reconnect.load(Ordering::Relaxed);

    let results = futures::future::join_all((0..8).map(|_| {
        let connection = connection.clone();
        async move { connection.reconnect(epoch_before).await.map(|_| ()) }
    }))
    .await;

    for result in &results {
        assert!(
            result.is_ok(),
            "every caller must get a channel: {result:?}"
        );
    }
    assert_eq!(
        connection.reconnect.load(Ordering::Relaxed),
        epoch_before + 1,
        "eight concurrent callers must cost exactly one rebuild",
    );
}

/// A failure status decides the item even when the payload fields are populated.
///
/// The fields kept their original numbers so an older peer still decodes them, which means a
/// response can carry both a fragment and a failure. The status wins: consulting `fragment`
/// first would turn a reported miss into a served fragment.
#[tokio::test]
async fn a_failure_status_wins_over_a_populated_payload() {
    let (service, _, _) = start_test_service(KillMode::ErrorBesidePayload).await;
    let ctx = test_context();
    let address = Address::zero_context_hash(Hash::from([0x77u8; 32]));

    let err = service
        .get(0, &ctx, &address)
        .await
        .expect_err("a failure status must not be overridden by the payload fields");

    assert!(
        err.is_not_found(),
        "the server's code must survive, got {err:?}",
    );
}

/// A unary storage request the server answers with `Unauthenticated` or `PermissionDenied` fails
/// with that refusal of the credentials.
#[tokio::test]
async fn a_refused_unary_request_fails_with_the_refusal() {
    let ctx = test_context();
    let key = Hash::from([0x88u8; 32]);
    let address = Address::zero_context_hash(key);
    type Refused = fn(&ProtocolError) -> bool;
    let cases: [(tonic::Code, Refused); 2] = [
        (
            tonic::Code::Unauthenticated,
            ProtocolError::is_not_authenticated,
        ),
        (
            tonic::Code::PermissionDenied,
            ProtocolError::is_not_authorized,
        ),
    ];
    for (code, refused) in cases {
        let (service, _, _) = start_test_service(KillMode::RefuseUnary(code)).await;
        let failures = [
            ("query", service.query(&ctx, &[address]).await.err()),
            ("verify", service.verify(&ctx, &address, false).await.err()),
            (
                "mutable_load",
                service
                    .mutable_load(&ctx, &key, KeyType::Resolve)
                    .await
                    .err(),
            ),
            (
                "mutable_store",
                service
                    .mutable_store(&ctx, key, key, KeyType::Resolve)
                    .await
                    .err(),
            ),
            (
                "mutable_compare_and_swap",
                service
                    .mutable_compare_and_swap(&ctx, key, key, key, KeyType::Resolve)
                    .await
                    .err(),
            ),
        ];
        for (request, failure) in failures {
            assert!(
                failure.as_ref().is_some_and(refused),
                "{request} answered {code:?} failed with {failure:?}",
            );
        }
    }
}

/// A refused open must not poison the cache for later requests.
///
/// Giving up leaves a handle whose stream never established. A later request that adopts it
/// from the cache would fail on its send, see `opened` still false, and give up again without
/// ever trying a fresh stream — so a channel that came back would never be used.
#[tokio::test]
async fn a_refused_open_does_not_poison_the_cached_stream() {
    let (service, accepted, _) = start_test_service(KillMode::RefuseFirstOpen).await;
    let ctx = test_context();
    let address = Address::zero_context_hash(Hash::from([0x55u8; 32]));

    let refused = service.get(0, &ctx, &address).await;
    assert!(
        refused.is_err_and(|err| err.is_disconnected()),
        "the refused open must surface as a disconnect",
    );

    let (fragment, payload) = service
        .get(0, &ctx, &address)
        .await
        .expect("a later request must open a fresh stream, not inherit the dead one");

    assert_eq!(payload.as_ref(), TEST_PAYLOAD);
    assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        2,
        "exactly one refused open and one that served",
    );
}

/// A storage operation recovers across a connection-level reconnect.
///
/// This is the seam the two layers meet at, and the one place neither covers alone: the stream
/// cache reports a refused open as `Disconnected`, `GRPCStorage::with_reconnect` rebuilds the
/// channel, and the retried operation must open a stream on the new one and succeed. The
/// caller sees a single successful fetch. Asserting the epoch advanced proves a real reconnect
/// happened rather than the first attempt merely being retried.
#[tokio::test]
async fn a_storage_operation_recovers_across_a_reconnect() {
    use lore_transport::traits::Storage;

    let (connection, accepted, _, _) = start_test_connection(KillMode::RefuseFirstOpen).await;
    let epoch_before = connection.reconnect.load(Ordering::SeqCst);

    let storage = lore_transport::grpc::GRPCStorage {
        connection: connection.clone(),
        client: StorageService::new(connection.clone()),
        auth_endpoint: AuthEndpoint::none(),
        identity: String::new(),
        credentials: Arc::new(lore_transport::connection::SuppliedCredentials::default()),
        session_counter: std::sync::atomic::AtomicU32::new(1),
        sessions: DashMap::new(),
    };
    storage.sessions.insert(0, Arc::new(test_context()));

    let address = Address::zero_context_hash(Hash::from([0x66u8; 32]));
    let (fragment, payload) = storage
        .get(0, &address)
        .await
        .expect("the operation must recover once the channel is rebuilt");

    assert_eq!(payload.as_ref(), TEST_PAYLOAD);
    assert_eq!(fragment.size_payload, TEST_PAYLOAD.len() as u32);
    assert_eq!(
        connection.reconnect.load(Ordering::SeqCst),
        epoch_before + 1,
        "recovery must have gone through a real reconnect",
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        2,
        "one refused open, then one on the rebuilt channel",
    );
}

/// A put response without a status is read as success, for a peer that predates the field.
///
/// The status field is additive: a server that does not set it reports a per-item failure by
/// ending the stream, exactly as it did before the field existed. Treating silence as a
/// failure would break every put against such a server.
#[tokio::test]
async fn put_treats_a_missing_status_as_success() {
    let (service, _accepted, _) = start_test_service(KillMode::CleanEnd).await;
    let ctx = test_context();
    let address = Address::zero_context_hash(Hash::from([0x33u8; 32]));
    let fragment = Fragment {
        flags: 0,
        size_payload: TEST_PAYLOAD.len() as u32,
        size_content: TEST_PAYLOAD.len() as u64,
    };

    service
        .put(
            0,
            &ctx,
            address,
            fragment,
            Some(Bytes::from_static(TEST_PAYLOAD)),
        )
        .await
        .expect("a status-less put response must be taken as success");
}

/// A stream that will not open hands straight off rather than reissuing.
///
/// Reissuing only covers a stream dying on a healthy channel. A refused open means the
/// channel is suspect, which is `GRPCConnection::reconnect`'s job, so this must report
/// `Disconnected` after a single attempt instead of spending the reissue budget.
#[tokio::test]
async fn get_gives_up_once_the_stream_will_not_open() {
    let (result, opens) = get_against_stream_killing_server(KillMode::RefuseOpen).await;

    let err = result.expect_err("a remote refusing every open must fail the request");
    assert!(
        err.is_disconnected(),
        "giving up must report a disconnect, got {err:?}",
    );
    assert_eq!(
        opens, 1,
        "a refused open must hand off immediately, not reissue",
    );
}

/// Concurrent copies of one source to two target contexts each reach the server. A copy response
/// names only the source, so the second waits for the first to be answered rather than riding
/// on its round trip and reporting a copy the server never made.
#[tokio::test]
async fn copies_of_one_source_to_two_targets_each_reach_the_server() {
    let (service, copied) = start_copy_service().await;
    let ctx = test_context();
    let source = Address::zero_context_hash(Hash::from([0x55u8; 32]));
    let first = Context::from([0x01u8; 16]);
    let second = Context::from([0x02u8; 16]);

    let (first_copy, second_copy) = tokio::join!(
        service.copy(0, &ctx, ctx.partition, source, first),
        service.copy(0, &ctx, ctx.partition, source, second)
    );

    first_copy.expect("first copy");
    second_copy.expect("second copy");
    let mut copied = copied.lock().unwrap().clone();
    copied.sort();
    assert_eq!(
        copied,
        vec![
            Bytes::copy_from_slice(first.as_ref()),
            Bytes::copy_from_slice(second.as_ref())
        ]
    );
}

/// Concurrent copies to one target ride a single round trip.
#[tokio::test]
async fn copies_to_one_target_coalesce() {
    let (service, copied) = start_copy_service().await;
    let ctx = test_context();
    let source = Address::zero_context_hash(Hash::from([0x56u8; 32]));
    let target = Context::from([0x03u8; 16]);

    let (first_copy, second_copy) = tokio::join!(
        service.copy(0, &ctx, ctx.partition, source, target),
        service.copy(0, &ctx, ctx.partition, source, target)
    );

    first_copy.expect("first copy");
    second_copy.expect("second copy");
    assert_eq!(copied.lock().unwrap().len(), 1);
}
