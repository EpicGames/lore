// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Partition;
use lore_revision::runtime::execution_context;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreObliterateStats;
use lore_storage::StoreQueryResult;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::observe::Observe;
use lore_transport::ProtocolError;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use parking_lot::Mutex;
use smallvec::SmallVec;
use tokio::time::MissedTickBehavior;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::error;
use tracing::instrument;
use tracing::warn;

use crate::protocol::replication_store::exists_batch;
use crate::protocol::replication_store::exists_batch::ExistsBatch;
use crate::protocol::replication_store::exists_batch::ExistsBatchResponse;
use crate::protocol::replication_store::exists_batch::MAX_ADDRESSES;
use crate::protocol::replication_store::get::Get;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::replication_store::obliterate::Obliterate;
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

/// An [`ImmutableStore`] implementation that forwards all operations to a remote Lore Server,
/// rather than e.g. accessing storage resources directly from a suboptimal geographic location.
///
/// Edge-region Lore Servers can use this to delegate immutable store operations to a server in
/// a region where storage resources are co-located. This avoids the compounding cross-region
/// latency that occurs when an edge server would make multiple sequential SDK API calls for a
/// single store operation, each paying a round-trip cost. By forwarding the entire storage
/// request to the
/// co-located server instead, the edge server pays the cross-region cost only once.
pub struct ReplicatedStore<ClientType: StoreClient> {
    instruments: ReplicatedStoreInstruments,
    client_container: ClientContainer<ClientType>,
    refresh_task: Mutex<Option<AbortOnDropHandle<()>>>,
    client_monitor_task: Mutex<Option<AbortOnDropHandle<()>>>,
}

impl<ClientType> ReplicatedStore<ClientType>
where
    ClientType: StoreClient,
{
    pub async fn new(
        client_factory: Arc<dyn ClientFactory<Output = ClientType>>,
        client_container_config: ClientContainerConfig,
        periodic_client_refresh: Duration,
        client_metrics_interval: Duration,
    ) -> Result<Arc<Self>, ProtocolError> {
        let instrument_provider = ReplicatedStoreProvider {};

        let container = ClientContainer::new(client_factory, client_container_config).await?;
        let store = Arc::new(ReplicatedStore {
            instruments: ReplicatedStoreInstruments::new(instrument_provider),
            client_container: container,
            refresh_task: None.into(),
            client_monitor_task: None.into(),
        });
        Self::setup_periodic_refresh(&store, periodic_client_refresh);
        Self::setup_client_stats_monitor(&store, client_metrics_interval);

        Ok(store)
    }

    fn setup_periodic_refresh(
        store: &Arc<ReplicatedStore<ClientType>>,
        periodic_client_refresh: Duration,
    ) {
        let weak = Arc::downgrade(store);
        let task = lore_spawn!({
            async move {
                let mut interval = tokio::time::interval(periodic_client_refresh);
                interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
                interval.tick().await; // skip immediate first tick
                loop {
                    interval.tick().await;
                    let Some(store) = weak.upgrade() else {
                        break;
                    };
                    let epoch = store.client_container.epoch();
                    let regenerate_result = store
                        .regenerate_client(epoch, GenerateClientReason::PeriodicRefresh)
                        .await;
                    if let Err(err) = regenerate_result {
                        warn!(?err, "Error periodic refreshing Replicated Store client");
                    }
                }
            }
        });
        let mut write = store.refresh_task.lock();
        *write = Some(AbortOnDropHandle::new(task));
    }

    fn setup_client_stats_monitor(
        store: &Arc<ReplicatedStore<ClientType>>,
        monitor_interval: Duration,
    ) {
        let quic_instruments = ClientMetrics::new(
            "replicated_store",
            store.instruments.provider.labels().to_vec(),
        );

        let weak = Arc::downgrade(store);
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
        let mut write = store.client_monitor_task.lock();
        *write = Some(AbortOnDropHandle::new(task));
    }

    /// Caution: the concrete QUIC client requires an execution context
    async fn regenerate_client(
        self: &Arc<Self>,
        expected_epoch: u64,
        reason: GenerateClientReason,
    ) -> Result<bool, ProtocolError> {
        let labels = {
            let reason_label = KeyValue::new(
                "refresh_reason",
                match reason {
                    GenerateClientReason::PeriodicRefresh => "periodic_refresh",
                    GenerateClientReason::ConnectionFailed => "connection_failed",
                },
            );
            let mut labels = SmallVec::new();
            labels.extend(self.instruments.provider.labels().iter().cloned());
            labels.push(reason_label);
            labels
        };
        self.client_container
            .regenerate_client(expected_epoch, reason)
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
        repository: Context,
        addresses: Vec<Address>,
        match_requested: StoreMatch,
    ) -> Result<ExistsBatchResponse, StoreError> {
        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: None,
        };

        let store = self.clone();
        let service_result = async move {
            let context = execution_context();
            let request = ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository,
                },
                store_match: match_requested,
                addresses,
            };
            let client = store.client_container.client().read().await;
            client.exists_batch(request).await
        }
        .observe(
            self.instruments
                .immutable_operation_latency_histogram
                .clone(),
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
impl<ClientType> ImmutableStore for ReplicatedStore<ClientType>
where
    ClientType: StoreClient,
{
    async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
        self.client_container.is_healthy()
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "ReplicatedStore::Exist", skip_all)]
    async fn exist(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        let repository: Context = partition.into();
        let quic_response = self
            .do_exists_batch("exist", repository, vec![address], match_requested)
            .await?;
        if quic_response.matches.len() == 1 {
            Ok(quic_response.matches[0])
        } else {
            warn!(
                num_matches = quic_response.matches.len(),
                "exist response was not 1 match"
            );
            Err(StoreError::internal(
                "replication service exist response mismatch",
            ))
        }
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "ReplicatedStore::ExistBatch", skip_all)]
    async fn exist_batch(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let repository: Context = partition.into();
        let quic_futures: Vec<_> = addresses
            .chunks(exists_batch::MAX_ADDRESSES)
            .map(|address_chunk| {
                self.clone().do_exists_batch(
                    "exist_batch",
                    repository,
                    address_chunk.to_vec(),
                    match_requested,
                )
            })
            .collect();
        let store_matches = join_all(quic_futures).await.into_iter().try_fold(
            Vec::with_capacity(addresses.len()),
            |mut acc, response_result| {
                let response = response_result?;
                acc.extend(response.matches);
                Ok::<_, StoreError>(acc)
            },
        )?;

        if store_matches.len() != addresses.len() {
            warn!(
                num_response_matches = store_matches.len(),
                num_requested = addresses.len(),
                "exist_batch mismatch"
            );
            Err(StoreError::internal(
                "replication service exist_batch response mismatch",
            ))
        } else {
            Ok(store_matches)
        }
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "ReplicatedStore::Query", skip_all)]
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        let repository: Context = partition.into();
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
                    repository,
                },
                store_match: match_requested,
                addresses: vec![address],
            });
            let client = store.client_container.client().read().await;
            client.query(request).await
        }
        .observe(
            self.instruments
                .immutable_operation_latency_histogram
                .clone(),
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
    #[instrument(name = "ReplicatedStore::Get", skip_all)]
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        let repository: Context = partition.into();
        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: Some(address),
        };

        let store = self.clone();
        let service_result = async move {
            let context = execution_context();
            let request = Get {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository,
                },
                address,
                match_required,
            };
            let client = store.client_container.client().read().await;
            client.get(request).await
        }
        .observe(
            self.instruments
                .immutable_operation_latency_histogram
                .clone(),
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
    #[instrument(name = "ReplicatedStore::Put", skip_all)]
    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        mut fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: Some(address),
        };

        let store = self.clone();
        let service_result = async move {
            // The remote server may be running composite store with write replication enabled.
            // We want to avoid scenarios where the recipient server replicates to its peers
            // which may include this region that sent the payload. Our store should control the
            // replication behaviour as we were the first recipient of the payload
            fragment.flags |= FragmentFlags::PayloadDoNotReplicate;
            let request = make_put_message(partition, address, fragment, payload, force)?;
            let client = store.client_container.client().read().await;
            client.put(request).await
        }
        .observe(
            self.instruments
                .immutable_operation_latency_histogram
                .clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("put"),
            observe_client_interaction(),
        )
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "ReplicatedStore::Obliterate", skip_all)]
    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let repository: Context = partition.into();
        let meta = ServiceRequestMeta {
            client_epoch: self.client_container.epoch(),
            address: Some(address),
        };

        let store = self.clone();
        let service_result = async move {
            let context = execution_context();
            let request = Obliterate {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository,
                },
                address,
            };
            let client = store.client_container.client().read().await;
            client.obliterate(request).await
        }
        .observe(
            self.instruments
                .immutable_operation_latency_histogram
                .clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("obliterate"),
            observe_client_interaction(),
        )
        .await
        .output;

        let response = handle_service_response(service_result, self, meta)?;
        stats
            .num_fragments
            .fetch_add(response.num_fragments as usize, Ordering::Relaxed);
        stats
            .num_payloads
            .fetch_add(response.num_payloads as usize, Ordering::Relaxed);

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
        // todo(UCS-18195) - configure the max query size to be whatever the QUIC Server says is the max_query_batch
        Some(MAX_ADDRESSES)
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
    store: Arc<ReplicatedStore<ClientType>>,
    meta: ServiceRequestMeta,
) -> Result<ResponseType, StoreError>
where
    ClientType: StoreClient,
{
    match result {
        Ok(output) => Ok(output),
        Err(ReplicationStoreClientError::ConnectionFailed) => {
            lore_spawn!({
                async move {
                    let regen_result = store
                        .regenerate_client(
                            meta.client_epoch,
                            GenerateClientReason::ConnectionFailed,
                        )
                        .await;
                    if let Err(err) = regen_result {
                        error!(
                            ?err,
                            "Failed to regenerate client from ConnectionFailed response"
                        );
                    }
                }
                .in_current_span()
            });
            Err(StoreError::internal("connection failed"))
        }
        Err(error) => Err(map_client_error_to_store_error(error, &meta)),
    }
}

#[derive(Clone)]
struct ReplicatedStoreProvider;

impl InstrumentProvider for ReplicatedStoreProvider {
    fn namespace(&self) -> &'static str {
        "urc.store.replicated"
    }
}

#[derive(Clone)]
struct ReplicatedStoreInstruments {
    regenerate_latency_histogram: Histogram<f64>,
    immutable_operation_latency_histogram: Histogram<f64>,
    provider: ReplicatedStoreProvider,
}

impl ReplicatedStoreInstruments {
    fn new(instrument_provider: ReplicatedStoreProvider) -> Self {
        Self {
            regenerate_latency_histogram: instrument_provider
                .latency_histogram_ms("client.regenerate.duration"),
            immutable_operation_latency_histogram: instrument_provider
                .latency_histogram_ms("immutable.operation_duration"),
            provider: instrument_provider,
        }
    }
}

#[cfg(test)]
mod tests {
    use lore_revision::util::time::RetryPolicy;
    use lore_transport::quic::client::ConnectionStats;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::Receiver;

    use super::*;
    use crate::protocol::replication_store::exists_batch::ExistsBatch;
    use crate::protocol::replication_store::exists_batch::ExistsBatchResponse;
    use crate::protocol::replication_store::get::GetResponse;
    use crate::protocol::replication_store::obliterate::ObliterateResponse;
    use crate::protocol::replication_store::put::Put;
    use crate::protocol::replication_store::query::Query;
    use crate::protocol::replication_store::query::QueryResponse;
    use crate::quic::replication_store_service::ReplicationServiceErrorCode;
    use crate::quic::replication_store_service::client_container::ClientContainerConfig;

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

    async fn make_store() -> Arc<ReplicatedStore<MockClient>> {
        let (tx, rx) = mpsc::channel(1);
        let factory = ChannelFactory { rx: rx.into() };

        // allow 1 creation for initialization
        tx.send(Ok(make_mock_client())).await.unwrap();
        ReplicatedStore::new(
            Arc::new(factory),
            make_client_container_config(),
            Duration::from_secs(60),
            Duration::from_secs(10),
        )
        .await
        .expect("Creation should work")
    }

    mod regenerate_client {
        use lore_base::runtime::LORE_CONTEXT;
        use tokio::join;
        use tokio::select;
        use tokio::sync::mpsc;

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

                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");
                    let original_epoch = store.client_container.epoch();
                    assert_eq!(original_epoch, 0);

                    let regen_1 = store
                        .regenerate_client(original_epoch, GenerateClientReason::PeriodicRefresh);
                    let regen_2 = store
                        .regenerate_client(original_epoch, GenerateClientReason::PeriodicRefresh);

                    let (output_1, output_2) = join!(regen_1, regen_2);
                    // 1 regenerated and the other didn't
                    assert_ne!(
                        output_1.expect("future 1 failed"),
                        output_2.expect("future 2 failed")
                    );
                    assert_eq!(store.client_container.epoch(), original_epoch + 1);
                })
                .await;
        }

        #[tokio::test]
        async fn regenerate_client_doesnt_block_client() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    // allow 1 creation for initialization
                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let regen = store.regenerate_client(
                        store.client_container.epoch(),
                        GenerateClientReason::PeriodicRefresh,
                    );

                    tokio::pin!(regen);
                    select! {
                        _ = &mut regen => {panic!("regen should not finish at this point");},
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }

                    // we have proven that the client regeneration is hanging,
                    // now prove we can still read the client for use in a potential store operation
                    assert!(store.client_container.is_healthy());
                    let _ = store.client_container.client().read().await;

                    // unblock the regen future
                    tx.send(Ok(make_mock_client())).await.unwrap();
                    let did_regen = regen.await.expect("regen should work");
                    assert!(did_regen);
                    assert!(store.client_container.is_healthy());
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
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");
                    assert!(store.client_container.is_healthy());

                    for _n in 1..10 {
                        tx.send(Err(ProtocolError::internal("test-error")))
                            .await
                            .expect("send error");
                    }

                    let regen = store.regenerate_client(
                        store.client_container.epoch(),
                        // ConnectionFailed should mark the store's client as unhealthy
                        GenerateClientReason::ConnectionFailed,
                    );

                    tokio::pin!(regen);
                    select! {
                        _ = &mut regen => {panic!("regen should not finish at this point");},
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                    // client should be still marked as unhealthy as regen is not finished
                    assert!(!store.client_container.is_healthy());

                    tx.send(Ok(make_mock_client()))
                        .await
                        .expect("send success error");

                    let did_regen = regen.await.expect("regen should work");
                    assert!(did_regen);
                    assert!(store.client_container.is_healthy());
                })
                .await;
        }

        #[tokio::test]
        async fn periodic_refresh_regenerates_client() {
            let execution =
                crate::util::setup_execution("test", String::default(), String::default());
            LORE_CONTEXT
                .scope(execution, async move {
                    let num_clients_to_mock = 15;
                    let (tx, rx) = mpsc::channel(num_clients_to_mock);
                    let factory = ChannelFactory { rx: rx.into() };

                    // 1 for initialization + plenty for periodic refreshes
                    for _ in 0..num_clients_to_mock {
                        tx.send(Ok(make_mock_client())).await.unwrap();
                    }

                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_millis(100),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    assert_eq!(store.client_container.epoch(), 0);

                    tokio::time::sleep(Duration::from_secs(1)).await;

                    let epoch = store.client_container.epoch();
                    assert!(
                        // we can't reliably guess how many times the task will get to execute,
                        // but we should expect at least a few times
                        epoch >= 5,
                        "expected several refreshes, got {epoch}"
                    );
                    assert!(store.client_container.is_healthy());
                })
                .await;
        }
    }

    mod handle_service_response {
        use lore_base::runtime::LORE_CONTEXT;
        use tokio::sync::mpsc;

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
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let start_epoch = store.client_container.epoch();
                    let error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ConnectionFailed),
                        store.clone(),
                        ServiceRequestMeta {
                            client_epoch: start_epoch,
                            address: None,
                        },
                    )
                    .unwrap_err();
                    assert!(matches!(error, StoreError::Internal(_)));

                    // we should be hanging in regenerate
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    assert!(!store.client_container.is_healthy());

                    // unblock and observe new client regenerated
                    tx.send(Ok(make_mock_client())).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    assert!(store.client_container.is_healthy());
                    assert_eq!(store.client_container.epoch(), start_epoch + 1);
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
                    let store = make_store().await;

                    let context = ServiceRequestMeta {
                        client_epoch: 0,
                        address: None,
                    };

                    let error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::AddressNotFound,
                        )),
                        store.clone(),
                        context.clone(),
                    )
                    .unwrap_err();
                    assert!(matches!(error, StoreError::AddressNotFound(_)));

                    let error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        )),
                        store.clone(),
                        context.clone(),
                    )
                    .unwrap_err();
                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }
    }

    mod put {
        use lore_base::runtime::LORE_CONTEXT;
        use lore_base::types::FragmentFlags;
        use lore_revision::fragment;
        use mockall::predicate::eq;
        use rand::random;

        use super::*;

        #[tokio::test]
        async fn successful_put_request_transformation_works() {
            let correlation_id = uuid::Uuid::new_v4();
            let repository: Context = random();
            let (fragment, address, payload) = fragment::generate_random();

            // The put method sets PayloadDoNotReplicate on the fragment before
            // sending to the remote peer, so the expected fragment must include it.
            let mut expected_fragment = fragment;
            expected_fragment.flags |= FragmentFlags::PayloadDoNotReplicate;

            let (tx, rx) = mpsc::channel(1);
            let factory = ChannelFactory { rx: rx.into() };

            let mut client = make_mock_client();
            client
                .expect_put()
                .with(eq(Put {
                    header: ReplicationHeader {
                        correlation_id,
                        repository,
                    },
                    address,
                    fragment: expected_fragment,
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
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    store
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
                    client.expect_put().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let error = store
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
    }

    mod exists_batch {
        use std::collections::HashMap;

        use lore_base::runtime::LORE_CONTEXT;
        use lore_revision::fragment;
        use mockall::predicate::eq;
        use rand::random;

        use super::*;
        use crate::protocol::replication_store::exists_batch::MAX_ADDRESSES;

        // Creates a large volume addresses to check, which all have a pre-assigned
        // random store match. The mocks return the store match that was pre-assigned
        // and the final output from store.exists_batch() call goes through the addresses in
        // original order and ensures they match the original pre-assigned store match
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

                    // create random addresses and decide randomly what store match they will be
                    // for the duration of the test
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
                            .expect_exists_batch()
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
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let store_matches = store
                        .exist_batch(repository.into(), &addresses, StoreMatch::MatchHash)
                        .await
                        .expect("exists_batch should work");

                    // sanity check the addresses are what we expect
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
                    client.expect_exists_batch().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });
                    client.expect_exists_batch().returning(|request| {
                        Ok(ExistsBatchResponse {
                            matches: request
                                .addresses
                                .into_iter()
                                .map(|_| StoreMatch::MatchHash)
                                .collect(),
                        })
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let error = store
                        .exist_batch(repository.into(), &addresses, StoreMatch::MatchHash)
                        .await
                        .expect_err("exists_error should fail");
                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }
    }

    mod exists {
        use lore_base::runtime::LORE_CONTEXT;
        use lore_revision::fragment;
        use mockall::predicate::eq;
        use rand::random;

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
                        .expect_exists_batch()
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
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let exist_output = store
                        .exist(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect("exists_batch should work");

                    assert_eq!(exist_output, StoreMatch::MatchPartition);
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
                    let (_, address, _) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client.expect_exists_batch().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let exist_error = store
                        .exist(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("exists_batch should fail");

                    assert!(matches!(exist_error, StoreError::SlowDown(_)));
                })
                .await;
        }
    }

    mod obliterate {
        use lore_base::runtime::LORE_CONTEXT;
        use lore_revision::fragment;
        use mockall::predicate::eq;
        use rand::random;

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
                    let (_, address, _) = fragment::generate_random();
                    // don't generate too large a random value to overflow when
                    // we add pre-existing stats to the obliterate() call below
                    let expected_num_fragments = random::<u32>() as usize;
                    let expected_num_payloads = random::<u32>() as usize;

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    {
                        let num_fragments = expected_num_fragments as u64;
                        let num_payloads = expected_num_payloads as u64;
                        client
                            .expect_obliterate()
                            .with(eq(Obliterate {
                                header: ReplicationHeader {
                                    correlation_id,
                                    repository,
                                },
                                address,
                            }))
                            .returning(move |_| {
                                Ok(ObliterateResponse {
                                    num_fragments,
                                    num_payloads,
                                })
                            });
                    }

                    tx.send(Ok(client)).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let base_num_fragments = 0usize;
                    let base_num_payloads = random::<u8>() as usize;
                    let stats = Arc::new(StoreObliterateStats {
                        num_fragments: base_num_fragments.into(),
                        num_payloads: base_num_payloads.into(),
                    });
                    store
                        .obliterate(repository.into(), address, stats.clone())
                        .await
                        .expect("obliterate should work");
                    assert_eq!(
                        stats.num_fragments.load(Ordering::Relaxed),
                        base_num_fragments + expected_num_fragments
                    );
                    assert_eq!(
                        stats.num_payloads.load(Ordering::Relaxed),
                        base_num_payloads + expected_num_payloads
                    );
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
                    client.expect_obliterate().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let stats = Arc::new(StoreObliterateStats {
                        num_fragments: 0.into(),
                        num_payloads: 0.into(),
                    });
                    let error = store
                        .obliterate(repository.into(), address, stats.clone())
                        .await
                        .expect_err("obliterate should fail");
                    assert!(matches!(error, StoreError::SlowDown(_)));
                    assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
                    assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
                })
                .await;
        }
    }

    mod get {
        use lore_base::runtime::LORE_CONTEXT;
        use lore_revision::fragment;
        use mockall::predicate::eq;
        use rand::random;

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
                            .expect_get()
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
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let get_output = store
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
                    client.expect_get().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let error = store
                        .get(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("get should fail");
                    assert!(matches!(error, StoreError::SlowDown(_)));
                })
                .await;
        }
    }

    mod query {
        use lore_base::runtime::LORE_CONTEXT;
        use lore_revision::fragment;
        use mockall::predicate::eq;
        use rand::random;

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
                        .expect_query()
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
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let exist_output = store
                        .query(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect("query should work");

                    assert_eq!(exist_output.match_made, StoreMatch::MatchPartition);
                    assert_eq!(exist_output.fragment, fragment);
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
                    let (_, address, _) = fragment::generate_random();

                    let (tx, rx) = mpsc::channel(1);
                    let factory = ChannelFactory { rx: rx.into() };

                    let mut client = make_mock_client();
                    client.expect_query().returning(|_| {
                        Err(ReplicationStoreClientError::ServiceError(
                            ReplicationServiceErrorCode::SlowDown,
                        ))
                    });

                    tx.send(Ok(client)).await.unwrap();
                    let store = ReplicatedStore::new(
                        Arc::new(factory),
                        make_client_container_config(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await
                    .expect("Creation should work");

                    let exist_error = store
                        .query(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect_err("query should fail");

                    assert!(matches!(exist_error, StoreError::SlowDown(_)));
                })
                .await;
        }
    }
}
