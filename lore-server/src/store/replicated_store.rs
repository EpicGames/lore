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
use lore_storage::StoreGetData;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
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
use tracing::info_span;
use tracing::warn;

use crate::protocol::replication_store::copy::ImmutableCopy;
use crate::protocol::replication_store::get::Get;
use crate::protocol::replication_store::get_metadata::GetMetadata;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::replication_store::obliterate::Obliterate;
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

/// An [`ImmutableStore`] implementation that forwards all operations to a remote Lore Server,
/// rather than e.g. accessing storage resources directly from a suboptimal geographic location.
///
/// Edge-region Lore Servers can use this to delegate immutable store operations to a server in
/// a region where storage resources are co-located. This avoids the compounding cross-region
/// latency that occurs when an edge server would make multiple sequential SDK API calls for a
/// single store operation, each paying a round-trip cost. By forwarding the entire storage
/// request to the
/// co-located server instead, the edge server pays the cross-region cost only once.
#[lore_macro::test_pub]
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
                    let stats = store.client_container.connection_stats().await;
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
    #[lore_macro::test_pub]
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

    async fn do_query(
        self: Arc<Self>,
        partition: Partition,
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
                    repository: partition.into(),
                },
                addresses,
            };
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
        .instrument(info_span!("ReplicatedStore::Query"))
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
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
        self.client_container.is_healthy()
    }

    /// The peer answers exact addresses only, so nothing it returns can have come from another
    /// partition.
    fn isolates_partitions(&self) -> bool {
        true
    }

    #[lore_macro::lore_instrument]
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(addresses.len(), results.len());

        let quic_futures = addresses
            .chunks(query::MAX_ADDRESSES)
            .map(|address_chunk| self.clone().do_query(partition, address_chunk.to_vec()));

        let responses = join_all(quic_futures).await;

        let mut offset = 0;
        for (chunk, response) in addresses.chunks(query::MAX_ADDRESSES).zip(responses) {
            let response = response?;
            if response.results.len() != chunk.len() {
                warn!(
                    num_response_results = response.results.len(),
                    num_requested = chunk.len(),
                    "resolve mismatch"
                );
                return Err(StoreError::internal(
                    "replication service resolve response mismatch",
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

    #[lore_macro::lore_instrument]
    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let (meta, client) = self.client_container.request_client(Some(address));

        let service_result = async move {
            let context = execution_context();
            let request = GetMetadata {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: partition.into(),
                },
                address,
            };
            client.get_metadata(request).await
        }
        .observe(
            self.instruments
                .immutable_operation_latency_histogram
                .clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("get_metadata"),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::GetMetadata"))
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }

    #[lore_macro::lore_instrument]
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let (meta, client) = self.client_container.request_client(Some(address));

        let service_result = async move {
            let context = execution_context();
            let request = Get {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: partition.into(),
                },
                address,
            };
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
        .instrument(info_span!("ReplicatedStore::Get"))
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }

    #[lore_macro::lore_instrument]
    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        mut fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        let (meta, client) = self.client_container.request_client(Some(address));

        let service_result = async move {
            // The remote server may be running composite store with write replication enabled.
            // We want to avoid scenarios where the recipient server replicates to its peers
            // which may include this region that sent the payload. Our store should control the
            // replication behaviour as we were the first recipient of the payload
            fragment.flags |= FragmentFlags::PayloadDoNotReplicate;
            let request = make_put_message(partition, address, fragment, payload, force)?;
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
        .instrument(info_span!("ReplicatedStore::Put"))
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }

    #[lore_macro::lore_instrument]
    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let (meta, client) = self.client_container.request_client(Some(address));

        let service_result = async move {
            let context = execution_context();
            let request = Obliterate {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: partition.into(),
                },
                address,
            };
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
        .instrument(info_span!("ReplicatedStore::Obliterate"))
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

    fn max_query_batch(&self) -> Option<usize> {
        // todo(UCS-18195) - configure the max query size to be whatever the QUIC Server says is the max_query_batch
        Some(query::MAX_ADDRESSES)
    }

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        Ok(())
    }

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Ok(())
    }

    #[lore_macro::lore_instrument]
    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        let (meta, client) = self.client_container.request_client(Some(source_address));

        let service_result = async move {
            let context = execution_context();
            let request = ImmutableCopy {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::try_parse(
                        context.globals().correlation_id.as_str(),
                    )
                    .unwrap_or_default(),
                    repository: destination_partition.into(),
                },
                source_partition,
                source_address,
                destination_context,
                durable: behavior.durable,
                // The remote server may be running composite store with write replication enabled.
                // We want to avoid scenarios where the recipient server replicates to its peers
                // which may include this region that drove the request. Our store should control
                // the replication behaviour as we were the first recipient of the payload
                do_not_replicate: true,
            };
            client.copy(request).await
        }
        .observe(
            self.instruments
                .immutable_operation_latency_histogram
                .clone(),
            self.instruments
                .provider
                .get_labels_for_operation_context("copy"),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::Copy"))
        .await
        .output;

        handle_service_response(service_result, self, meta)
    }
}

#[lore_macro::test_pub]
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
