// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Partition;
use lore_revision::runtime::execution_context;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
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

use crate::protocol::replication_store::get::Get;
use crate::protocol::replication_store::get_metadata::GetMetadata;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::replication_store::query;
use crate::protocol::replication_store::query::Query;
use crate::protocol::replication_store::query::QueryResponse;
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

#[lore_macro::test_pub]
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
                    let stats = store.client_container.connection_stats().await;
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
    #[lore_macro::test_pub]
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

    async fn do_query(
        self: Arc<Self>,
        repository: Partition,
        addresses: Vec<Address>,
    ) -> Result<QueryResponse, StoreError> {
        let (meta, client) = self.client_container.request_client(None);

        let service_result = async move {
            let context = execution_context();
            let request = Query {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: repository.into(),
                },
                addresses,
            };
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

        handle_service_response(service_result, self, meta)
    }
}

#[async_trait]
impl<ClientType> ImmutableStore for Replica<ClientType>
where
    ClientType: StoreClient,
{
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    /// The peer answers exact addresses only, so nothing it returns can have come from another
    /// partition.
    fn isolates_partitions(&self) -> bool {
        true
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::Query", skip_all)]
    async fn query(
        self: Arc<Self>,
        repository: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(addresses.len(), results.len());

        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let batch_futures = addresses
            .chunks(query::MAX_ADDRESSES)
            .map(|chunk| self.clone().do_query(repository, chunk.to_vec()));

        let responses = join_all(batch_futures).await;

        let mut offset = 0;
        for (chunk, response) in addresses.chunks(query::MAX_ADDRESSES).zip(responses) {
            let response = response?;
            if response.results.len() != chunk.len() {
                return Err(StoreError::internal(
                    "read replica resolve response mismatch",
                ));
            }

            for (result_from_peer, result) in response
                .results
                .into_iter()
                .zip(results[offset..].iter_mut())
            {
                *result = result_from_peer;
            }
            offset += chunk.len();
        }

        Ok(())
    }

    /// Answered by this store's own `query`, so it reports whether the payload is durable rather
    /// than what representation is stored. Wiring the remote's metadata operation through is
    /// outstanding; until then a caller wanting a representation must ask the store holding it.
    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::GetMetadata", skip_all)]
    async fn get_metadata(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let (meta, client) = self.client_container.request_client(Some(address));

        let service_result = async move {
            let context = execution_context();
            let request = GetMetadata {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: repository.into(),
                },
                address,
            };
            client.local_get_metadata(request).await
        }
        .observe(
            self.instruments.operation_latency.clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("get_metadata"),
            observe_client_interaction(),
        )
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }

    #[lore_macro::lore_instrument]
    #[instrument(name = "QuicReplica::Get", skip_all)]
    async fn get(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        if !self.client_container.is_healthy() {
            return Err(StoreError::internal("client is unhealthy"));
        }

        let (meta, client) = self.client_container.request_client(Some(address));

        let service_result = async move {
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

        handle_service_response(service_result, self, meta)
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

        let (meta, client) = self.client_container.request_client(Some(address));

        let service_result = async move {
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

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
        Ok(())
    }

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Ok(())
    }

    async fn copy(
        self: Arc<Self>,
        _source_partition: Partition,
        _source_address: Address,
        _destination_partition: Partition,
        _destination_context: Context,
        _behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        Err(StoreError::internal("copy not supported on read replica"))
    }
}

#[lore_macro::test_pub]
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
