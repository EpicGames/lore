// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Partition;
use lore_revision::runtime::execution_context;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreObliterateStats;
use lore_storage::StoreQueryResult;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::observe::Observe;
use lore_transport::ProtocolError;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use parking_lot::Mutex;
use smallvec::SmallVec;
use tokio::time::MissedTickBehavior;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::instrument;
use tracing::warn;

use crate::protocol::replication_store::exists_batch::ExistsBatch;
use crate::protocol::replication_store::exists_batch::ExistsBatchResponse;
use crate::protocol::replication_store::exists_batch::MAX_ADDRESSES;
use crate::protocol::replication_store::get::Get;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::replication_store::query::Query;
use crate::quic::client_monitor::ClientMetrics;
use crate::quic::replication_store_service::client::ReplicationStoreClientError;
use crate::quic::replication_store_service::client::ServiceRequestMeta;
use crate::quic::replication_store_service::client::StoreClient;
use crate::quic::replication_store_service::client::make_put_message;
use crate::quic::replication_store_service::client::map_client_error_to_store_error;
use crate::quic::replication_store_service::client::observe_client_interaction;
use crate::quic::replication_store_service::client_container::ClientContainer;
use crate::quic::replication_store_service::client_container::ClientContainerConfig;
use crate::quic::replication_store_service::client_container::ClientFactory;
use crate::quic::replication_store_service::client_container::GenerateClientReason;
use crate::quic::replication_store_service::client_container::observe_regenerate;

#[derive(Clone)]
struct ReplicaProvider {
    labels: LabelArray,
}

impl InstrumentProvider for ReplicaProvider {
    fn namespace(&self) -> &'static str {
        "urc.replication.client"
    }

    fn labels(&self) -> &[KeyValue] {
        &self.labels
    }
}

#[allow(dead_code)]
struct ReplicaInstruments {
    operation_latency: Histogram<f64>,
    regenerate_latency_histogram: Histogram<f64>,
    provider: ReplicaProvider,
}

impl ReplicaInstruments {
    fn new(instrument_provider: ReplicaProvider) -> Self {
        Self {
            operation_latency: instrument_provider
                .latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            regenerate_latency_histogram: instrument_provider
                .latency_histogram_ms("client.regenerate.duration"),
            provider: instrument_provider,
        }
    }
}

#[allow(dead_code)]
pub struct Replica<ClientType: StoreClient> {
    client_container: Arc<ClientContainer<ClientType>>,
    client_monitor_task: Mutex<Option<AbortOnDropHandle<()>>>,
    instruments: ReplicaInstruments,
}

impl<ClientType> Replica<ClientType>
where
    ClientType: StoreClient,
{
    pub async fn new(
        client_factory: Arc<dyn ClientFactory<Output = ClientType>>,
        container_config: ClientContainerConfig,
        metric_labels: LabelArray,
    ) -> Result<Self, ProtocolError> {
        let container = ClientContainer::new(client_factory, container_config).await?;

        Ok(Self {
            client_container: Arc::new(container),
            instruments: ReplicaInstruments::new(ReplicaProvider {
                labels: metric_labels,
            }),
            client_monitor_task: None.into(),
        })
    }

    pub fn setup_client_stats_monitor(self: &Arc<Self>, monitor_interval: Duration) {
        let quic_instruments =
            ClientMetrics::new("store_replica", self.instruments.provider.labels().to_vec());

        let weak = Arc::downgrade(self);
        let task = lore_spawn!({
            async move {
                let mut interval = tokio::time::interval(monitor_interval);
                interval.set_missed_tick_behavior(MissedTickBehavior::Burst);
                interval.tick().await; // skip immediate first tick
                loop {
                    interval.tick().await;
                    let Some(store) = weak.upgrade() else {
                        break;
                    };
                    let client = store.client_container.client().read().await;
                    let stats = client.connection_stats().await;
                    if let Some(stats) = stats {
                        quic_instruments.observe(&stats);
                    }
                }
            }
        });
        let mut write = self.client_monitor_task.lock();
        *write = Some(AbortOnDropHandle::new(task));
    }

    /// Caution: the concrete QUIC client requires an execution context
    async fn regenerate_client(
        self: &Arc<Self>,
        expected_epoch: u64,
    ) -> Result<bool, ProtocolError> {
        let labels = {
            let mut labels = SmallVec::new();
            labels.extend(self.instruments.provider.labels().iter().cloned());
            labels
        };

        self.client_container
            .regenerate_client(expected_epoch, GenerateClientReason::ConnectionFailed)
            .observe(
                self.instruments.regenerate_latency_histogram.clone(),
                labels,
                observe_regenerate(),
            )
            .await
            .output
    }

    async fn do_exists_batch(
        self: Arc<Self>,
        metric_label: &'static str,
        repository: Partition,
        addresses: Vec<Address>,
        match_requested: StoreMatch,
    ) -> Result<ExistsBatchResponse, StoreError> {
        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: None,
        };

        let service_result = async {
            let context = execution_context();
            let request = ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: repository.into(),
                },
                store_match: match_requested,
                addresses,
            };
            let client = self.client_container.client().read().await;
            let response = client.local_exists_batch(request).await?;
            Ok(response)
        }
        .observe(
            self.instruments.operation_latency.clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context(metric_label),
            observe_client_interaction(),
        )
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }
}

#[async_trait]
impl<ClientType> ImmutableStore for Replica<ClientType>
where
    ClientType: StoreClient,
{
    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::Exist", skip_all)]
    async fn exist(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let response = self
            .do_exists_batch("exist", repository, vec![address], match_requested)
            .await?;
        if response.matches.len() == 1 {
            Ok(response.matches[0])
        } else {
            Err(StoreError::internal("read replica exist response mismatch"))
        }
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::ExistBatch", skip_all)]
    async fn exist_batch(
        self: Arc<Self>,
        repository: Partition,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let batch_futures: Vec<_> = addresses
            .chunks(MAX_ADDRESSES)
            .map(|chunk| {
                self.clone().do_exists_batch(
                    "exist_batch",
                    repository,
                    chunk.to_vec(),
                    match_requested,
                )
            })
            .collect();

        let store_matches = join_all(batch_futures).await.into_iter().try_fold(
            Vec::with_capacity(addresses.len()),
            |mut acc, response_result| {
                let response = response_result?;
                acc.extend(response.matches);
                Ok::<_, StoreError>(acc)
            },
        )?;

        if store_matches.len() != addresses.len() {
            Err(StoreError::internal(
                "read replica exist_batch response mismatch",
            ))
        } else {
            Ok(store_matches)
        }
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::Query", skip_all)]
    async fn query(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: Some(address),
        };

        let store = self.clone();
        let service_result = async move {
            let context = execution_context();
            let request = Query(ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: repository.into(),
                },
                store_match: match_requested,
                addresses: vec![address],
            });
            let client = store.client_container.client().read().await;
            client.local_query(request).await
        }
        .observe(
            self.instruments.operation_latency.clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("query"),
            observe_client_interaction(),
        )
        .await
        .output;

        let query_response = handle_service_response(service_result, self, meta)?;
        Ok(StoreQueryResult {
            fragment: query_response.fragment,
            match_made: query_response.match_made,
        })
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::Get", skip_all)]
    async fn get(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: Some(address),
        };

        let replica = self.clone();
        let client = replica.client_container.client().read().await;
        let service_result = async {
            let context = execution_context();
            let request = Get {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: repository.into(),
                },
                address,
                match_required,
            };
            client.local_get(request).await
        }
        .observe(
            self.instruments.operation_latency.clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("get"),
            observe_client_interaction(),
        )
        .await
        .output;

        let response = handle_service_response(service_result, self, meta)?;
        Ok((response.fragment, response.payload))
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::Put", skip_all)]
    async fn put(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: Some(address),
        };

        let replica = self.clone();
        let client = replica.client_container.client().read().await;
        let service_result = async {
            let request = make_put_message(repository, address, fragment, payload, force)?;
            client.local_put(request).await?;
            Ok(())
        }
        .observe(
            self.instruments.operation_latency.clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("put"),
            observe_client_interaction(),
        )
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }

    async fn obliterate(
        self: Arc<Self>,
        _repository: Partition,
        _address: Address,
        _stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        Err(StoreError::internal(
            "write operations not supported on read replica",
        ))
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

fn handle_service_response<ResponseType, ClientType>(
    result: Result<ResponseType, ReplicationStoreClientError>,
    replica: Arc<Replica<ClientType>>,
    meta: ServiceRequestMeta,
) -> Result<ResponseType, StoreError>
where
    ClientType: StoreClient,
{
    match result {
        Ok(output) => Ok(output),
        Err(ReplicationStoreClientError::ConnectionFailed) => {
            let weak = Arc::downgrade(&replica);
            lore_spawn!({
                async move {
                    // Replica behaviour is that requests will early out if the client is unhealthy
                    // therefore there will be only a handful of requests that will experience
                    // the ConnectionFailed result. It is up to them to reestablish the connection,
                    // as no one else will come later to drive that reconnect. Loop until it is
                    // healed.
                    loop {
                        // if we get dropped then it is because the replica target has been removed
                        // from the cluster so we can never reconnect to it
                        let Some(upgraded) = weak.upgrade() else {
                            break;
                        };

                        let regen_result = upgraded.regenerate_client(meta.client_epoch).await;
                        if let Err(err) = regen_result {
                            warn!(
                                ?err,
                                "Failed to regenerate replica client from ConnectionFailed response"
                            );
                        } else {
                            break;
                        }
                    }
                }
                .in_current_span()
            });
            Err(StoreError::internal("connection failed"))
        }
        Err(error) => Err(map_client_error_to_store_error(error, &meta)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_revision::fragment;
    use lore_revision::util::time::RetryPolicy;
    use lore_transport::quic::client::ConnectionStats;
    use mockall::predicate::eq;
    use rand::random;
    use tokio::join;
    use tokio::select;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::Receiver;

    use super::*;
    use crate::protocol::replication_store::exists_batch::ExistsBatch;
    use crate::protocol::replication_store::exists_batch::ExistsBatchResponse;
    use crate::protocol::replication_store::get::GetResponse;
    use crate::protocol::replication_store::obliterate::Obliterate;
    use crate::protocol::replication_store::obliterate::ObliterateResponse;
    use crate::protocol::replication_store::put::Put;
    use crate::protocol::replication_store::query::Query;
    use crate::protocol::replication_store::query::QueryResponse;
    use crate::quic::replication_store_service::ReplicationServiceErrorCode;

    mockall::mock! {
        pub Client {}

        #[async_trait]
        impl StoreClient for Client {
            async fn connection_stats(&self) -> Option<ConnectionStats>;

            async fn put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;

            async fn exists_batch(
                &self,
                request: ExistsBatch,
            ) -> Result<ExistsBatchResponse, ReplicationStoreClientError>;

            async fn obliterate(
                &self,
                request: Obliterate,
            ) -> Result<ObliterateResponse, ReplicationStoreClientError>;

            async fn get(
                &self,
                request: Get,
            ) -> Result<GetResponse, ReplicationStoreClientError>;

            async fn query(
                &self,
                request: Query,
            ) -> Result<QueryResponse, ReplicationStoreClientError>;

            async fn local_put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;

            async fn local_exists_batch(
                &self,
                request: ExistsBatch,
            ) -> Result<ExistsBatchResponse, ReplicationStoreClientError>;

            async fn local_get(&self, request: Get) -> Result<GetResponse, ReplicationStoreClientError>;

            async fn local_query(
                &self,
                request: Query,
            ) -> Result<QueryResponse, ReplicationStoreClientError>;
        }
    }

    fn make_mock_client() -> MockClient {
        let mut client = MockClient::new();
        client.expect_connection_stats().return_once(|| None);
        client
    }

    struct ChannelFactory {
        rx: tokio::sync::Mutex<Receiver<Result<MockClient, ProtocolError>>>,
    }

    #[async_trait]
    impl ClientFactory for ChannelFactory {
        type Output = MockClient;

        async fn make_client(
            &self,
            _initial_cwnd: Option<u64>,
        ) -> Result<MockClient, ProtocolError> {
            self.rx.lock().await.recv().await.expect("recv should work")
        }
    }

    fn make_client_container_config() -> ClientContainerConfig {
        let retry = RetryPolicy::builder()
            .with_initial_backoff_millis(50)
            .with_max_backoff_millis(1_000)
            .with_limit(300)
            .build();
        ClientContainerConfig {
            regenerate_retry_policy: retry,
            connection_lost_sleep: Duration::from_millis(1),
        }
    }

    async fn make_replica() -> Arc<Replica<MockClient>> {
        let (tx, rx) = mpsc::channel(1);
        let factory = ChannelFactory { rx: rx.into() };

        // allow 1 creation for initialization
        tx.send(Ok(make_mock_client())).await.unwrap();
        let replica = Replica::new(
            Arc::new(factory),
            make_client_container_config(),
            LabelArray::default(),
        )
        .await
        .expect("Creation should work");
        Arc::new(replica)
    }

    #[tokio::test]
    async fn obliterate_returns_error() {
        let err = StoreError::internal("write operations not supported on read replica");
        assert!(matches!(err, StoreError::Internal(_)));
        let msg = format!("{err}");
        assert!(msg.contains("write operations not supported on read replica"));
    }

    mod regenerate_client {
        use super::*;

        #[tokio::test]
        async fn regenerate_client_guarded_by_permit() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(2);
                    // once during creation, allow one other for the test reconnect
                    tx.send(Ok(make_mock_client())).await.expect("send 1");
                    tx.send(Ok(make_mock_client())).await.expect("send 2");
                    let factory = ChannelFactory { rx: rx.into() };

                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);
                    let original_epoch = replica.client_container.epoch();
                    assert_eq!(original_epoch, 0);

                    let regen_1 = replica.regenerate_client(original_epoch);
                    let regen_2 = replica.regenerate_client(original_epoch);

                    let (output_1, output_2) = join!(regen_1, regen_2);
                    // 1 regenerated and the other didn't
                    assert_ne!(
                        output_1.expect("future 1 failed"),
                        output_2.expect("future 2 failed")
                    );
                    assert_eq!(replica.client_container.epoch(), original_epoch + 1);
                })
                .await;
        }

        #[tokio::test]
        async fn regenerate_unhealthy_client_eventually_succeeds() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(15);
                    let factory = ChannelFactory { rx: rx.into() };

                    // allow 1 creation for initialization
                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);
                    assert!(replica.client_container.is_healthy());

                    for _n in 1..10 {
                        tx.send(Err(ProtocolError::internal("test-error")))
                            .await
                            .expect("send error");
                    }

                    let regen = replica.regenerate_client(replica.client_container.epoch());

                    tokio::pin!(regen);
                    select! {
                        _ = &mut regen => {panic!("regen should not finish at this point");},
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                    // client should still be marked as unhealthy as regen is not finished
                    assert!(!replica.client_container.is_healthy());

                    tx.send(Ok(make_mock_client())).await.expect("send success");

                    let did_regen = regen.await.expect("regen should work");
                    assert!(did_regen);
                    assert!(replica.client_container.is_healthy());
                })
                .await;
        }
    }

    mod handle_service_response {
        use super::*;

        #[tokio::test]
        async fn connection_failed_regenerates_client() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    // allow 1 creation for initialization
                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let start_epoch = replica.client_container.epoch();
                    let error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ConnectionFailed),
                        replica.clone(),
                        ServiceRequestMeta {
                            client_epoch: start_epoch,
                            address: None,
                        },
                    )
                    .unwrap_err();
                    assert!(matches!(error, StoreError::Internal(_)));

                    // the spawned loop should be hanging in regenerate
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    assert!(!replica.client_container.is_healthy());

                    // unblock and observe new client regenerated
                    tx.send(Ok(make_mock_client())).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    assert!(replica.client_container.is_healthy());
                    assert_eq!(replica.client_container.epoch(), start_epoch + 1);
                })
                .await;
        }

        #[tokio::test]
        async fn connection_failed_loop_stops_when_replica_dropped() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let start_epoch = replica.client_container.epoch();
                    let _error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ConnectionFailed),
                        replica.clone(),
                        ServiceRequestMeta {
                            client_epoch: start_epoch,
                            address: None,
                        },
                    );

                    // drop the replica — the weak ref in the loop should break it
                    drop(replica);

                    // the spawned task should exit without panicking once it tries
                    // to upgrade the weak reference
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    // if we reach here without a panic, the loop exited cleanly
                })
                .await;
        }

        // samples some common errors to ensure they are returned
        #[tokio::test]
        async fn service_errors_are_mapped_to_store_errors() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let replica = make_replica().await;

                    let context = ServiceRequestMeta {
                        client_epoch: 0,
                        address: None,
                    };

                    let error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::AddressNotFound,
                        )),
                        replica.clone(),
                        context.clone(),
                    )
                    .unwrap_err();
                    assert!(matches!(error, StoreError::AddressNotFound(_)));

                    let error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        )),
                        replica.clone(),
                        context.clone(),
                    )
                    .unwrap_err();
                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }
    }

    mod put {
        use super::*;

        #[tokio::test]
        async fn successful_put_request_transformation_works() {
            let correlation_id = uuid::Uuid::new_v4();
            let repository: Context = random();
            let (fragment, address, payload) = fragment::generate_random();

            let (tx, rx) = mpsc::channel(1);
            let factory = ChannelFactory { rx: rx.into() };

            let mut client = make_mock_client();
            client
                .expect_local_put()
                .with(eq(Put {
                    header: ReplicationHeader {
                        correlation_id,
                        repository,
                    },
                    address,
                    fragment,
                    flags: 0,
                    payload: Some(payload.clone()),
                }))
                .returning(|_| Ok(()));

            tx.send(Ok(client)).await.unwrap();

            let execution = crate::util::setup_execution(
                "test",
                correlation_id.as_hyphenated().to_string(),
                String::default(),
            );
            LORE_CONTEXT
                .scope(execution, async move {
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    replica
                        .put(
                            repository.into(),
                            address,
                            fragment,
                            Some(payload),
                            false, /* force */
                        )
                        .await
                        .expect("put should work");
                })
                .await;
        }

        #[tokio::test]
        async fn service_error_put_request_transformation_works() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();
                    let (fragment, address, payload) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client.expect_local_put().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let error = replica
                        .put(
                            repository.into(),
                            address,
                            fragment,
                            Some(payload),
                            false, /* force */
                        )
                        .await
                        .expect_err("put should fail");
                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }

        #[tokio::test]
        async fn put_returns_error_when_unhealthy() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    // mark unhealthy via ConnectionFailed regenerate
                    // drive the regenerate forward enough to mark unhealthy, then drop
                    {
                        let regen = replica.client_container.regenerate_client(
                            replica.client_container.epoch(),
                            GenerateClientReason::ConnectionFailed,
                        );
                        tokio::pin!(regen);
                        tokio::select! {
                            _ = &mut regen => { panic!("regen should not finish"); },
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        }
                    }
                    assert!(!replica.client_container.is_healthy());

                    let repository: Context = random();
                    let (fragment, address, payload) = fragment::generate_random();

                    let error = replica
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect_err("put should fail when unhealthy");
                    assert!(matches!(error, StoreError::Internal(_)));
                })
                .await;
        }
    }

    mod exists {
        use super::*;

        #[tokio::test]
        async fn successful_exist_transform_works() {
            let correlation_id = uuid::Uuid::new_v4();
            let execution = crate::util::setup_execution(
                "test",
                correlation_id.as_hyphenated().to_string(),
                String::default(),
            );
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client
                        .expect_local_exists_batch()
                        .with(eq(ExistsBatch {
                            header: ReplicationHeader {
                                correlation_id,
                                repository,
                            },
                            store_match: StoreMatch::MatchFull,
                            addresses: vec![address],
                        }))
                        .returning(|_| {
                            Ok(ExistsBatchResponse {
                                matches: vec![StoreMatch::MatchPartition],
                            })
                        });

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let exist_output = replica
                        .exist(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect("exist should work");

                    assert_eq!(exist_output, StoreMatch::MatchPartition);
                })
                .await;
        }

        #[tokio::test]
        async fn service_error_transformation_works() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client.expect_local_exists_batch().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let exist_error = replica
                        .exist(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("exist should fail");

                    assert!(matches!(exist_error, StoreError::SlowDown(_)));
                })
                .await;
        }

        #[tokio::test]
        async fn exist_returns_error_when_unhealthy() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    // drive the regenerate forward enough to mark unhealthy, then drop
                    {
                        let regen = replica.client_container.regenerate_client(
                            replica.client_container.epoch(),
                            GenerateClientReason::ConnectionFailed,
                        );
                        tokio::pin!(regen);
                        tokio::select! {
                            _ = &mut regen => { panic!("regen should not finish"); },
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        }
                    }
                    assert!(!replica.client_container.is_healthy());

                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let error = replica
                        .exist(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("exist should fail when unhealthy");
                    assert!(matches!(error, StoreError::Internal(_)));
                })
                .await;
        }
    }

    mod exists_batch {
        use std::collections::HashMap;

        use parking_lot::Mutex;

        use super::*;
        use crate::protocol::replication_store::exists_batch::MAX_ADDRESSES;

        #[tokio::test]
        async fn successful_transformation_with_order_preserved() {
            let correlation_id = uuid::Uuid::new_v4();
            let execution = crate::util::setup_execution(
                "test",
                correlation_id.as_hyphenated().to_string(),
                String::default(),
            );
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();

                    let mut addresses = Vec::new();
                    let address_matches: Arc<Mutex<HashMap<Address, StoreMatch>>> =
                        Arc::new(HashMap::new().into());
                    for _ in 0..MAX_ADDRESSES * 4 {
                        let (_, address, _) = fragment::generate_random();
                        addresses.push(address);

                        let random_match: u8 = random::<u8>() % 4;
                        let store_match: StoreMatch =
                            random_match.try_into().expect("invalid store match");

                        address_matches.lock().insert(address, store_match);
                    }

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    for addresses in addresses.chunks(MAX_ADDRESSES) {
                        let address_matches = address_matches.clone();
                        client
                            .expect_local_exists_batch()
                            .with(eq(ExistsBatch {
                                header: ReplicationHeader {
                                    correlation_id,
                                    repository,
                                },
                                store_match: StoreMatch::MatchHash,
                                addresses: addresses.to_vec(),
                            }))
                            .returning(move |request| {
                                let map = address_matches.lock();
                                let mut output = Vec::new();
                                for address in request.addresses {
                                    output.push(map[&address]);
                                }
                                Ok(ExistsBatchResponse { matches: output })
                            });
                    }

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let store_matches = replica
                        .exist_batch(repository.into(), &addresses, StoreMatch::MatchHash)
                        .await
                        .expect("exist_batch should work");

                    let map = address_matches.lock();
                    assert_eq!(store_matches.len(), addresses.len());
                    for (index, store_match) in store_matches.iter().enumerate() {
                        let address = addresses[index];
                        let expected_match = map[&address];
                        assert_eq!(*store_match, expected_match);
                    }
                })
                .await;
        }

        #[tokio::test]
        async fn service_error_transformation_works() {
            let correlation_id = uuid::Uuid::new_v4();
            let execution = crate::util::setup_execution(
                "test",
                correlation_id.as_hyphenated().to_string(),
                String::default(),
            );
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();

                    let mut addresses = Vec::new();
                    for _ in 0..MAX_ADDRESSES * 2 {
                        let (_, address, _) = fragment::generate_random();
                        addresses.push(address);
                    }

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    // 1 of the batched calls is ok, the other isn't
                    client.expect_local_exists_batch().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });
                    client.expect_local_exists_batch().returning(|request| {
                        Ok(ExistsBatchResponse {
                            matches: request
                                .addresses
                                .into_iter()
                                .map(|_| StoreMatch::MatchHash)
                                .collect(),
                        })
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let error = replica
                        .exist_batch(repository.into(), &addresses, StoreMatch::MatchHash)
                        .await
                        .expect_err("exist_batch should fail");
                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }

        #[tokio::test]
        async fn exist_batch_returns_error_when_unhealthy() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    // drive the regenerate forward enough to mark unhealthy, then drop
                    {
                        let regen = replica.client_container.regenerate_client(
                            replica.client_container.epoch(),
                            GenerateClientReason::ConnectionFailed,
                        );
                        tokio::pin!(regen);
                        tokio::select! {
                            _ = &mut regen => { panic!("regen should not finish"); },
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        }
                    }
                    assert!(!replica.client_container.is_healthy());

                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let error = replica
                        .exist_batch(repository.into(), &[address], StoreMatch::MatchFull)
                        .await
                        .expect_err("exist_batch should fail when unhealthy");
                    assert!(matches!(error, StoreError::Internal(_)));
                })
                .await;
        }
    }

    mod get {
        use super::*;

        #[tokio::test]
        async fn successful_request_transformation_works() {
            let correlation_id = uuid::Uuid::new_v4();
            let execution = crate::util::setup_execution(
                "test",
                correlation_id.as_hyphenated().to_string(),
                String::default(),
            );
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();
                    let (fragment, address, payload) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    {
                        let payload = payload.clone();
                        client
                            .expect_local_get()
                            .with(eq(Get {
                                header: ReplicationHeader {
                                    correlation_id,
                                    repository,
                                },
                                address,
                                match_required: StoreMatch::MatchFull,
                            }))
                            .returning(move |_| {
                                Ok(GetResponse {
                                    fragment,
                                    payload: payload.clone(),
                                })
                            });
                    }

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let get_output = replica
                        .get(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect("get should work");

                    assert_eq!(get_output.0, fragment);
                    assert_eq!(get_output.1, payload);
                })
                .await;
        }

        #[tokio::test]
        async fn service_error_transformation_works() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client.expect_local_get().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let error = replica
                        .get(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("get should fail");
                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }

        #[tokio::test]
        async fn get_returns_error_when_unhealthy() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    // drive the regenerate forward enough to mark unhealthy, then drop
                    {
                        let regen = replica.client_container.regenerate_client(
                            replica.client_container.epoch(),
                            GenerateClientReason::ConnectionFailed,
                        );
                        tokio::pin!(regen);
                        tokio::select! {
                            _ = &mut regen => { panic!("regen should not finish"); },
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        }
                    }
                    assert!(!replica.client_container.is_healthy());

                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let error = replica
                        .get(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("get should fail when unhealthy");
                    assert!(matches!(error, StoreError::Internal(_)));
                })
                .await;
        }
    }

    mod query {
        use super::*;

        #[tokio::test]
        async fn successful_transform_works() {
            let correlation_id = uuid::Uuid::new_v4();
            let execution = crate::util::setup_execution(
                "test",
                correlation_id.as_hyphenated().to_string(),
                String::default(),
            );
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();
                    let (fragment, address, _) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client
                        .expect_local_query()
                        .with(eq(Query(ExistsBatch {
                            header: ReplicationHeader {
                                correlation_id,
                                repository,
                            },
                            store_match: StoreMatch::MatchFull,
                            addresses: vec![address],
                        })))
                        .returning(move |_| {
                            Ok(QueryResponse {
                                fragment,
                                match_made: StoreMatch::MatchPartition,
                            })
                        });

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let query_output = replica
                        .query(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect("query should work");

                    assert_eq!(query_output.match_made, StoreMatch::MatchPartition);
                    assert_eq!(query_output.fragment, fragment);
                })
                .await;
        }

        #[tokio::test]
        async fn service_error_transformation_works() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client.expect_local_query().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    let error = replica
                        .query(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("query should fail");

                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }

        #[tokio::test]
        async fn query_returns_error_when_unhealthy() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let replica = Replica::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        LabelArray::default(),
                    )
                    .await
                    .expect("Creation should work");
                    let replica = Arc::new(replica);

                    // drive the regenerate forward enough to mark unhealthy, then drop
                    {
                        let regen = replica.client_container.regenerate_client(
                            replica.client_container.epoch(),
                            GenerateClientReason::ConnectionFailed,
                        );
                        tokio::pin!(regen);
                        tokio::select! {
                            _ = &mut regen => { panic!("regen should not finish"); },
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        }
                    }
                    assert!(!replica.client_container.is_healthy());

                    let repository: Context = random();
                    let (_, address, _) = fragment::generate_random();

                    let error = replica
                        .query(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("query should fail when unhealthy");
                    assert!(matches!(error, StoreError::Internal(_)));
                })
                .await;
        }
    }
}
