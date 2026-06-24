// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use lore_base::error::SlowDown;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Fragment;
use lore_base::types::Partition;
use lore_proto::PutRequest;
use lore_proto::PutResponse;
use lore_proto::ReplicationPutRequest;
use lore_proto::rpc::replication_service_client::ReplicationServiceClient;
use lore_revision::util;
use lore_revision::util::time::RetryPolicy;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreObliterateStats;
use lore_storage::StoreQueryResult;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::drop_record::DropRecord;
use lore_telemetry::observe::ObserveResult;
#[cfg(test)]
use mockall::automock;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Histogram;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;
use tonic::Streaming;
use tonic::transport::Channel;
use tracing::Instrument;
use tracing::debug;
use tracing::info;
use tracing::instrument;
use tracing::warn;

pub const MESSAGE_DROPPED_REASON: &str = "reason";

#[derive(Clone)]
struct PutRequestPayload {
    repository: Partition,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
}

type PutStreamHandle = StreamHandle<PutRequestPayload, PutResponse>;
type ResponseSender<ResponseType> =
    oneshot::Sender<Result<Arc<ResponseType>, ReplicationClientError>>;

#[async_trait::async_trait]
trait StreamImplementation {
    type InputType;
    type ResponseType;

    async fn stream_requests(
        &self,
        client: ReplicationServiceClient<Channel>,
        inflight: Arc<DashMap<Address, Vec<ResponseSender<Self::ResponseType>>>>,
        rx: Receiver<(Self::InputType, ResponseSender<Self::ResponseType>)>,
    ) -> Result<Streaming<Self::ResponseType>, ReplicationClientError>;

    fn get_inflight_key_from_response(&self, response: &Self::ResponseType) -> Option<Address>;
}

struct PutStreamImplementation;

#[async_trait::async_trait]
impl StreamImplementation for PutStreamImplementation {
    type InputType = PutRequestPayload;
    type ResponseType = PutResponse;

    async fn stream_requests(
        &self,
        mut client: ReplicationServiceClient<Channel>,
        inflight: Arc<DashMap<Address, Vec<ResponseSender<Self::ResponseType>>>>,
        mut rx: Receiver<(Self::InputType, ResponseSender<Self::ResponseType>)>,
    ) -> Result<Streaming<Self::ResponseType>, ReplicationClientError> {
        let request_stream = async_stream::stream! {
            while let Some((request, sender)) = rx.recv().await {
                let inflight_key = request.address;
                let mut send = false;
                // `DashMap::entry` is safe here as it is not held across any awaits and no other locks are acquired while held
                #[allow(clippy::disallowed_methods)]
                inflight.entry(inflight_key).or_insert_with(|| { send = true; vec![] }).push(sender);

                if send {
                    yield ReplicationPutRequest {
                        repository_id: request.repository.into(),
                        put_request: Some(PutRequest {
                            address: Some(request.address.into()),
                            fragment: Some(request.fragment.into()),
                            payload: request.payload,
                        }),
                    };
                }
            }
        };

        let streaming = client
            .put(request_stream)
            .await
            .map_err(Into::<ReplicationClientError>::into)?
            .into_inner();
        Ok(streaming)
    }

    fn get_inflight_key_from_response(&self, response: &Self::ResponseType) -> Option<Address> {
        response.address.as_ref().map(Address::from)
    }
}

#[derive(Clone)]
struct StreamHandle<InputType, ResponseType> {
    sender: mpsc::Sender<(InputType, ResponseSender<ResponseType>)>,
    epoch: u64,
}

struct DropInflightGuard<ResponseType> {
    inflight: Arc<DashMap<Address, Vec<ResponseSender<ResponseType>>>>,
}

impl<ResponseType> Drop for DropInflightGuard<ResponseType> {
    fn drop(&mut self) {
        self.inflight.clear();
    }
}

struct ReplicationClientInstruments {
    instrument_provider: ReplicationClientInstrumentProvider,
    messages_dropped: Counter<u64>,
    operation_latency: Histogram<f64>,
    num_attempts_histogram: Histogram<u64>,
    timeouts: Counter<u64>,
    connect: Counter<u64>,
    disconnect: Counter<u64>,
}

impl ReplicationClientInstruments {
    fn new(instrument_provider: ReplicationClientInstrumentProvider) -> Self {
        let messages_dropped = instrument_provider.counter("replication.message_dropped");
        let operation_latency =
            instrument_provider.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME);
        let num_attempts_histogram = instrument_provider
            .length_histogram("replication.num_attempts", vec![1., 2., 3., 4., 5., 10.]);
        let timeouts = instrument_provider.counter("replication.timeout");
        let connect = instrument_provider.counter("replication.connect");
        let disconnect = instrument_provider.counter("replication.disconnect");

        ReplicationClientInstruments {
            instrument_provider,
            messages_dropped,
            operation_latency,
            num_attempts_histogram,
            timeouts,
            connect,
            disconnect,
        }
    }
}

#[derive(Clone, Debug, Error)]
pub enum GetStreamError {
    #[error("Some other process is creating the stream")]
    CreationInProgress,
    #[error("Failed to create stream: {0}")]
    ClientError(#[from] ReplicationClientError),
}

#[derive(Clone, Debug, Error)]
pub enum ReplicationClientError {
    #[error("Request failed {0}")]
    RequestFailed(tonic::Status),
    #[error("Failed to receive message {error}")]
    ReceiveError {
        error: oneshot::error::RecvError,
        observed_epoch: u64,
    },
    #[error("Failed to send message")]
    SendError { observed_epoch: u64 },
    #[error("Upstream overloaded, slow down")]
    SlowDown,
}

impl From<tonic::Status> for ReplicationClientError {
    fn from(status: tonic::Status) -> Self {
        info!(?status, "Replication gRPC request failed");
        match status.code() {
            tonic::Code::Unavailable | tonic::Code::ResourceExhausted => {
                ReplicationClientError::SlowDown
            }
            _ => ReplicationClientError::RequestFailed(status),
        }
    }
}

pub struct ReplicationClientImpl {
    client: ReplicationServiceClient<Channel>,
    buffer: usize,
    retry_policy: RetryPolicy,
    put_sender: RwLock<Option<PutStreamHandle>>,
    stream_creation_semaphore: Semaphore,
    current_epoch: AtomicU64,
    instruments: Arc<ReplicationClientInstruments>,
}

#[cfg(test)]
pub use MockReplicationClientImpl as ReplicationClient;
#[cfg(not(test))]
pub use ReplicationClientImpl as ReplicationClient;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::PARTITION_ID;

#[cfg_attr(test, automock)]
impl ReplicationClientImpl {
    pub fn new(
        client: ReplicationServiceClient<Channel>,
        buffer: usize,
        retry_policy: RetryPolicy,
    ) -> Self {
        Self {
            client,
            buffer,
            retry_policy,
            put_sender: RwLock::default(),
            stream_creation_semaphore: Semaphore::new(1),
            current_epoch: AtomicU64::default(),
            instruments: Arc::new(ReplicationClientInstruments::new(
                ReplicationClientInstrumentProvider {},
            )),
        }
    }

    async fn get_or_initialize_stream<Impl, IType, RType>(
        &self,
        handle_lock: &RwLock<Option<StreamHandle<IType, RType>>>,
        stream_creation_semaphore: &Semaphore,
        implementation: Impl,
    ) -> Result<StreamHandle<IType, RType>, GetStreamError>
    where
        Impl: StreamImplementation<InputType = IType, ResponseType = RType> + Send + Sync + 'static,
        IType: Clone + Send + 'static,
        RType: Clone + Debug + Send + Sync + 'static,
    {
        let sender = handle_lock.read().await;
        if let Some(stream) = sender.as_ref() {
            debug!(stream.epoch, "Found existing stream handle with epoch");
            Ok(stream.clone())
        } else {
            debug!("Creating new stream handle");
            drop(sender);
            // only 1 caller to acquire the write guard
            let _creation_guard = stream_creation_semaphore
                .try_acquire()
                .map_err(|_error| GetStreamError::CreationInProgress)?;
            let mut sender_lock = handle_lock.write().await;

            if let Some(stream) = sender_lock.as_ref() {
                debug!(
                    stream.epoch,
                    "After acquiring write lock, found existing stream handle with epoch"
                );
                return Ok(stream.clone());
            }

            let (tx, rx) = mpsc::channel::<(IType, ResponseSender<RType>)>(self.buffer);

            let client = self.client.clone();

            lore_spawn!(
                async move {
                    // ensure that whatever happens to this task that pumps messages into the GRPC
                    // stream and routes messages back, whenever the task is dropped we clear the
                    // inflight requests so subscribers tasks get recv errors and don't hang
                    let inflight_guard = DropInflightGuard {
                        inflight: Arc::new(DashMap::<Address, Vec<ResponseSender<RType>>>::new()),
                    };

                    let mut response = implementation
                        .stream_requests(client, inflight_guard.inflight.clone(), rx)
                        .await
                        .inspect_err(|error| {
                            warn!(?error, "stream_requests error");
                        })?;

                    while let Some(message) = response
                        .message()
                        .await
                        .inspect_err(|error| {
                            warn!(?error, "response.message error");
                        })
                        .map_err(Into::<ReplicationClientError>::into)?
                    {
                        if let Some(key) = implementation.get_inflight_key_from_response(&message) {
                            if let Some((_, subscribers)) = inflight_guard.inflight.remove(&key) {
                                let response = Arc::new(message);
                                for subscriber in subscribers {
                                    let _ =
                                        subscriber.send(Ok(response.clone())).map_err(|error| {
                                            warn!(?error, "Failed to send replication response");
                                        });
                                }
                            } else {
                                warn!(
                                    ?key,
                                    "No subscribers found for address in replication response"
                                );
                            }
                        }
                    }

                    Ok::<(), ReplicationClientError>(())
                }
                .in_current_span()
            );

            let epoch = self.current_epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let handle = StreamHandle { sender: tx, epoch };
            *sender_lock = Some(handle.clone());

            self.instruments.connect.add(1, &[]);

            debug!("Created new stream handle with epoch {epoch}");
            Ok(handle)
        }
    }

    async fn get_or_initialize_put_stream(&self) -> Result<PutStreamHandle, GetStreamError> {
        let span = tracing::info_span!("ReplicationClient::PutStreamImplementation");
        let implementation = PutStreamImplementation {};
        self.get_or_initialize_stream(
            &self.put_sender,
            &self.stream_creation_semaphore,
            implementation,
        )
        .instrument(span)
        .await
    }

    async fn clear_stream_handle<IType, RType>(
        &self,
        observed_epoch: u64,
        handle_lock: &RwLock<Option<StreamHandle<IType, RType>>>,
    ) where
        IType: 'static,
        RType: 'static,
    {
        let mut lock = handle_lock.write().await;
        if let Some(handle) = lock.as_ref()
            && handle.epoch == observed_epoch
        {
            lock.take();
            debug!("clear_stream_handle - attempting to reconnect.");
            self.instruments.disconnect.add(1, &[]);
        } else {
            debug!("clear_stream_handle - reconnect already in progress.");
        }
    }

    #[instrument(name = "ReplicationClient::do_put", skip_all)]
    async fn do_put(
        &self,
        repository: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ReplicationClientError> {
        let (tx, rx) = oneshot::channel();

        let (send_result, sender_epoch) = {
            let put_sender = match self.get_or_initialize_put_stream().await {
                Ok(sender) => sender,
                Err(error) => {
                    return match error {
                        GetStreamError::CreationInProgress => {
                            // creation in progress, avoid back pressure by just forgetting about it
                            self.instruments.messages_dropped.add(
                                1,
                                &[KeyValue::new(
                                    MESSAGE_DROPPED_REASON,
                                    "stream_creation_in_progress",
                                )],
                            );
                            Err(ReplicationClientError::SlowDown)
                        }
                        GetStreamError::ClientError(client_error) => {
                            self.instruments.messages_dropped.add(
                                1,
                                &[KeyValue::new(
                                    MESSAGE_DROPPED_REASON,
                                    "error_creating_stream",
                                )],
                            );
                            Err(client_error)
                        }
                    };
                }
            };

            let send_result = put_sender.sender.try_send((
                PutRequestPayload {
                    repository,
                    address,
                    fragment,
                    payload,
                },
                tx,
            ));
            (send_result, put_sender.epoch)
        };

        match send_result
        {
            Ok(_) => rx.await.unwrap_or_else(|e| {
                info!(error = ?e, "Error receiving put result from channel");
                Err(ReplicationClientError::ReceiveError { error: e, observed_epoch: sender_epoch })
            }).map(|response| {
                debug!(address = ?response.address.as_ref().map(Address::from), "Received successful replication put response");
            }),
            Err(TrySendError::Full(_)) => {
                debug!(address = %address, "Could not send replication put request, stream is full");
                self.instruments.messages_dropped.add(1, &[KeyValue::new(MESSAGE_DROPPED_REASON, "stream_full")]);
                Err(ReplicationClientError::SlowDown)
            },
            Err(e) => {
                warn!(error = ?e, "Failed to send put request to channel");
                Err(ReplicationClientError::SendError { observed_epoch: sender_epoch })
            }
        }
    }

    #[instrument(name = "ReplicationClient::put", skip_all)]
    pub async fn put(
        &self,
        repository: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ReplicationClientError> {
        let labels = self
            .instruments
            .instrument_provider
            .get_labels_for_operation_context("replication_put");

        let put_op = {
            let labels = labels.clone();
            async move {
                let mut num_attempts =
                    DropRecord::new(self.instruments.num_attempts_histogram.clone(), &labels);
                let mut retry = util::time::retry_with_policy(self.retry_policy);

                loop {
                    num_attempts.add(1);
                    match self
                        .do_put(repository, address, fragment, payload.clone())
                        .await
                    {
                        Ok(_) => return Ok(()),
                        Err(
                            ReplicationClientError::SendError { observed_epoch }
                            | ReplicationClientError::ReceiveError {
                                error: _,
                                observed_epoch,
                            },
                        ) => {
                            self.clear_stream_handle(observed_epoch, &self.put_sender)
                                .await;

                            if !retry.wait().await {
                                debug!("Replication put retries exceeded.");
                                self.instruments.timeouts.add(1, &[]);
                                return Err(ReplicationClientError::RequestFailed(
                                    tonic::Status::deadline_exceeded(
                                        "Client timeout exceeded sending replication put",
                                    ),
                                ));
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        };

        put_op
            .in_current_span()
            .observe_result(self.instruments.operation_latency.clone(), labels)
            .await
            .output
    }
}

struct ReplicationClientInstrumentProvider;

impl InstrumentProvider for ReplicationClientInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.replication.client"
    }

    fn labels(&self) -> &[KeyValue] {
        &[]
    }
}

pub struct GrpcReplica {
    client: ReplicationClient,
}

impl GrpcReplica {
    pub fn new(client: ReplicationClient) -> Self {
        GrpcReplica { client }
    }
}

#[async_trait]
impl ImmutableStore for GrpcReplica {
    async fn exist(
        self: Arc<Self>,
        _repository: Partition,
        _address: Address,
        _match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        Err(StoreError::internal("Store does not support operation"))
    }

    async fn exist_batch(
        self: Arc<Self>,
        _repository: Partition,
        _addresses: &[Address],
        _match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        Err(StoreError::internal("Store does not support operation"))
    }

    async fn query(
        self: Arc<Self>,
        _repository: Partition,
        _address: Address,
        _match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        Err(StoreError::internal("Store does not support operation"))
    }

    async fn get(
        self: Arc<Self>,
        _repository: Partition,
        _address: Address,
        _match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        Err(StoreError::internal("Store does not support operation"))
    }

    #[lore_macro::lore_instrument]
    async fn put(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        _force: bool,
    ) -> Result<(), StoreError> {
        if let Some(payload) = &payload
            && payload.len() > FRAGMENT_SIZE_THRESHOLD
        {
            // gRPC replication won't break with increased payload size so log just for investigation
            warn!({PARTITION_ID} = %repository, {ADDRESS} = %address, ?fragment, payload_length = payload.len(), "gRPC Large replicated put detected");
        }

        self.client
            .put(repository, address, fragment, payload)
            .await
            .map_err(|e| {
                if let ReplicationClientError::SlowDown = e {
                    StoreError::from(SlowDown)
                } else {
                    warn!(error = ?e, "Replication put failed");
                    StoreError::internal_with_context(e, "Replication put failed")
                }
            })
    }

    async fn obliterate(
        self: Arc<Self>,
        _repository: Partition,
        _address: Address,
        _stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn evict(
        self: Arc<Self>,
        _max_capacity: usize,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        Ok(0)
    }

    async fn compact(
        self: Arc<Self>,
        _max_size: usize,
        _at: Option<usize>,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        Ok(None)
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        None
    }

    async fn compact_stop(self: Arc<Self>) {}

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        Ok(())
    }

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lore_base::types::Partition;
    use lore_revision::fragment::generate_random;
    use lore_storage::ImmutableStore;
    use mockall::predicate::eq;

    use super::*;

    #[tokio::test]
    async fn test_put() -> Result<(), Box<dyn std::error::Error>> {
        let mut client = MockReplicationClientImpl::default();

        let repository = rand::random::<Partition>();
        let (fragment, address, payload) = generate_random();

        client
            .expect_put()
            .with(
                eq(repository),
                eq(address),
                eq(fragment),
                eq(Some(payload.clone())),
            )
            .return_once(|_, _, _, _| Ok(()));

        let store = GrpcReplica::new(client);

        Arc::new(store)
            .put(
                repository,
                address,
                fragment,
                Some(payload),
                false, /* force */
            )
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_put_fails_with_slowdown() -> Result<(), Box<dyn std::error::Error>> {
        let mut client = MockReplicationClientImpl::default();

        let repository = rand::random::<Partition>();
        let (fragment, address, payload) = generate_random();

        client
            .expect_put()
            .with(
                eq(repository),
                eq(address),
                eq(fragment),
                eq(Some(payload.clone())),
            )
            .return_once(|_, _, _, _| Err(ReplicationClientError::SlowDown));

        let store = GrpcReplica::new(client);

        let err = Arc::new(store)
            .put(
                repository,
                address,
                fragment,
                Some(payload),
                false, /* force */
            )
            .await
            .expect_err("should have failed");

        assert!(matches!(err, StoreError::SlowDown(_)));

        Ok(())
    }

    #[tokio::test]
    async fn test_put_fails_with_other_error() -> Result<(), Box<dyn std::error::Error>> {
        let mut client = MockReplicationClientImpl::default();

        let repository = rand::random::<Partition>();
        let (fragment, address, payload) = generate_random();

        client
            .expect_put()
            .with(
                eq(repository),
                eq(address),
                eq(fragment),
                eq(Some(payload.clone())),
            )
            .return_once(|_, _, _, _| {
                Err(ReplicationClientError::RequestFailed(
                    tonic::Status::internal("Oh noes"),
                ))
            });

        let store = GrpcReplica::new(client);

        let err = Arc::new(store)
            .put(
                repository,
                address,
                fragment,
                Some(payload),
                false, /* force */
            )
            .await
            .expect_err("should have failed");

        assert!(matches!(err, StoreError::Internal(_)));

        Ok(())
    }
}

#[cfg(test)]
mod stream_error_tests {
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use lore_base::types::Partition;
    use lore_proto::rpc::replication_service_server::ReplicationService as ReplicationServiceTrait;
    use lore_proto::rpc::replication_service_server::ReplicationServiceServer;
    use lore_revision::fragment::generate_random;
    use lore_revision::util;
    use tokio_stream::Stream;
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;
    use tonic::Response;
    use tonic::Status;

    use super::*;

    /// A test gRPC server that responds normally for the first `n` requests
    /// per stream, then sends an error status that closes the response stream.
    struct ErrorAfterNService {
        n: usize,
    }

    #[tonic::async_trait]
    impl ReplicationServiceTrait for ErrorAfterNService {
        type PutStream = Pin<Box<dyn Stream<Item = Result<PutResponse, Status>> + Send>>;

        async fn put(
            &self,
            request: Request<Streaming<ReplicationPutRequest>>,
        ) -> Result<Response<Self::PutStream>, Status> {
            let n = self.n;
            let mut stream = request.into_inner();
            let (tx, rx) = mpsc::channel(100);

            tokio::spawn(async move {
                let mut count = 0;
                while let Some(req) = stream.next().await {
                    let Ok(req) = req else { break };
                    count += 1;

                    if count > n {
                        let _ = tx
                            .send(Err(Status::internal("test: intentional stream error")))
                            .await;
                        break;
                    }

                    // Delay so requests accumulate in the client's inflight map
                    tokio::time::sleep(Duration::from_millis(50)).await;

                    let address = req.put_request.and_then(|p| p.address);
                    let _ = tx.send(Ok(PutResponse { address })).await;
                }
            });

            Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
        }
    }

    /// Starts a test gRPC server on a random port and returns the port number.
    async fn start_test_server(service: ErrorAfterNService) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let addr: SocketAddr = ([127, 0, 0, 1], port).into();

        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ReplicationServiceServer::new(service))
                .serve(addr)
                .await
                .unwrap();
        });

        // Allow the server to start listening
        tokio::time::sleep(Duration::from_millis(100)).await;

        port
    }

    async fn connect_client(port: u16) -> ReplicationClientImpl {
        let channel = tonic::transport::Channel::from_shared(format!("http://127.0.0.1:{port}"))
            .unwrap()
            .connect()
            .await
            .unwrap();

        ReplicationClientImpl::new(
            ReplicationServiceClient::new(channel),
            100, /* buffer */
            util::time::RetryPolicy::builder()
                .with_initial_backoff_millis(10)
                .with_max_backoff_millis(100)
                .with_limit(0)
                .build(),
        )
    }

    /// Verifies that in-flight requests resolve (don't hang) when the server
    /// errors the response stream. The server responds to the first 3 requests
    /// with a delay, then errors on request 4+. Because the client
    /// sends all 20 concurrently, requests 5-20 are inflight
    /// map when the error arrives.
    #[tokio::test]
    async fn test_inflight_requests_resolve_on_stream_error() {
        let port = start_test_server(ErrorAfterNService { n: 3 }).await;
        let client = Arc::new(connect_client(port).await);

        let mut join_set = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let client = client.clone();
            join_set.spawn(async move {
                let repository = rand::random::<Partition>();
                let (fragment, address, payload) = generate_random();
                client
                    .put(repository, address, fragment, Some(payload))
                    .await
            });
        }

        // All 20 puts must resolve
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(_result) = join_set.join_next().await {}
        })
        .await;

        assert!(result.is_ok(), "Timed out — inflight requests are hanging");
    }

    /// Verifies that the client recovers after a stream error: the first
    /// puts succeed, then the stream errors, and a subsequent put succeeds
    /// after the client reconnects on a new stream.
    #[tokio::test]
    async fn test_client_recovers_after_stream_error() {
        let port = start_test_server(ErrorAfterNService { n: 3 }).await;
        let client = connect_client(port).await;

        assert_eq!(client.current_epoch.load(Ordering::SeqCst), 0);

        // Send 3 sequential puts — these all succeed on the first stream
        for _ in 0..3 {
            let repository = rand::random::<Partition>();
            let (fragment, address, payload) = generate_random();
            client
                .put(repository, address, fragment, Some(payload))
                .await
                .expect("put should succeed within server's limit");
        }

        // First stream was created during put 1
        assert_eq!(client.current_epoch.load(Ordering::SeqCst), 1);

        // 4th put triggers stream error;
        let repository = rand::random::<Partition>();
        let (fragment, address, payload) = generate_random();
        client
            .put(repository, address, fragment, Some(payload.clone()))
            .await
            .expect_err("4th should cause error");

        // 5th put creates a new stream (epoch 2) and succeeds
        client
            .put(repository, address, fragment, Some(payload.clone()))
            .await
            .expect("5th request should work");

        assert_eq!(client.current_epoch.load(Ordering::SeqCst), 2);
    }
}
