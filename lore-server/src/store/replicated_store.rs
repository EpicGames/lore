// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use futures::future::try_join_all;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
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
use lore_telemetry::observe::Observe;
use lore_transport::ProtocolError;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;
use smallvec::SmallVec;
use tokio::sync::Notify;
use tokio::time::Instant;
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
use crate::quic::replication_store_service::client_container::InflightGuard;
use crate::quic::replication_store_service::client_container::RequestKind;
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
    /// One client per slot, each with its own connection to the remote server. A request goes
    /// to the healthy slot with the fewest requests of its kind in flight. Ties are broken by the
    /// total number of requests in flight. Requests stuck on a stalled connection therefore
    /// steer new ones of every kind to the other slots.
    client_containers: Box<[Arc<ClientContainer<ClientType>>]>,
    /// Wakes each slot's supervisor when a request reports that the slot's connection failed.
    connection_failures: Box<[Arc<Notify>]>,
    /// Where the next slot search starts. It advances on every request so that slots with equal
    /// counts take turns. Counts are only equal when few requests are in flight. Under light or
    /// sequential load, the first slot would otherwise take every request. Keeping every slot in
    /// use helps detect and replace degraded connections before their slots have to take over
    /// from stalled ones.
    next_slot: AtomicUsize,
    /// One per slot. Each owns every reconnect of its slot.
    _supervisor_tasks: Box<[AbortOnDropHandle<()>]>,
    _client_monitor_task: AbortOnDropHandle<()>,
}

struct Lease<ClientType: StoreClient> {
    slot: usize,
    meta: ServiceRequestMeta,
    client: Arc<ClientType>,
    /// Keeps the request counted as in flight until the lease drops.
    _inflight: InflightGuard,
}

impl<ClientType> ReplicatedStore<ClientType>
where
    ClientType: StoreClient,
{
    /// Connects all `num_clients` clients before returning, and fails if any of them cannot
    /// connect.
    pub async fn new(
        client_factory: Arc<dyn ClientFactory<Output = ClientType>>,
        client_container_config: ClientContainerConfig,
        num_clients: NonZeroUsize,
        periodic_client_refresh: Duration,
        client_metrics_interval: Duration,
    ) -> Result<Arc<Self>, ProtocolError> {
        let instruments = ReplicatedStoreInstruments::new(ReplicatedStoreProvider {});

        let containers: Box<[_]> = try_join_all((0..num_clients.get()).map(|_| async {
            ClientContainer::new(client_factory.clone(), client_container_config.clone())
                .await
                .map(Arc::new)
        }))
        .await?
        .into();
        let connection_failures: Box<[_]> =
            containers.iter().map(|_| Arc::new(Notify::new())).collect();

        let num_slots = containers.len();
        let supervisor_tasks = containers
            .iter()
            .zip(&connection_failures)
            .enumerate()
            .map(|(slot, (container, connection_failed))| {
                let first_refresh =
                    first_refresh_delay(slot, num_slots, periodic_client_refresh, rand::random());
                let task = lore_spawn!(supervise_slot(
                    slot,
                    container.clone(),
                    connection_failed.clone(),
                    instruments.clone(),
                    first_refresh,
                    periodic_client_refresh,
                ));
                AbortOnDropHandle::new(task)
            })
            .collect();
        let client_monitor_task = AbortOnDropHandle::new(lore_spawn!(monitor_clients(
            containers.to_vec(),
            instruments.clone(),
            client_metrics_interval,
        )));

        Ok(Arc::new(ReplicatedStore {
            instruments,
            client_containers: containers,
            connection_failures,
            next_slot: AtomicUsize::new(0),
            _supervisor_tasks: supervisor_tasks,
            _client_monitor_task: client_monitor_task,
        }))
    }

    /// Picks the slot for a request of `kind`: the healthy slot with the fewest requests of that
    /// kind in flight, then the fewest in total, or any slot when none is healthy.
    #[lore_macro::test_pub]
    fn select_slot(&self, kind: RequestKind) -> usize {
        let num_slots = self.client_containers.len();
        let start = self.next_slot.fetch_add(1, Ordering::Relaxed) % num_slots;
        let mut least: Option<(usize, (usize, usize))> = None;
        for offset in 0..num_slots {
            let slot = (start + offset) % num_slots;
            let container = &self.client_containers[slot];
            // An unhealthy slot is waiting to replace a client whose connection failed. Its
            // requests fail fast, so its count stays low and would otherwise attract more of them.
            if !container.is_healthy() {
                continue;
            }
            let inflight = (container.inflight(kind), container.inflight_total());
            if least.is_none_or(|(_, least_inflight)| inflight < least_inflight) {
                least = Some((slot, inflight));
            }
        }
        // Every slot is reconnecting, and a request fails on any of them until one is back.
        least.map_or(start, |(slot, _)| slot)
    }

    fn lease(&self, kind: RequestKind, address: Option<Address>) -> Lease<ClientType> {
        let slot = self.select_slot(kind);
        let (meta, client, inflight) = self.client_containers[slot].lease(kind, address);
        Lease {
            slot,
            meta,
            client,
            _inflight: inflight,
        }
    }

    async fn do_query(
        self: Arc<Self>,
        partition: Partition,
        addresses: Vec<Address>,
    ) -> Result<QueryResponse, StoreError> {
        let lease = self.lease(RequestKind::Query, None);
        let client = lease.client;

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
            operation_labels(RequestKind::Query, lease.slot),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::Query"))
        .await
        .output;

        handle_service_response(service_result, &self, lease.slot, lease.meta)
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

    /// Available while any slot is, since requests go only to healthy slots.
    async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
        self.client_containers
            .iter()
            .any(|container| container.is_healthy())
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
        let lease = self.lease(RequestKind::GetMetadata, Some(address));
        let client = lease.client;

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
            operation_labels(RequestKind::GetMetadata, lease.slot),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::GetMetadata"))
        .await
        .output;

        handle_service_response(service_result, &self, lease.slot, lease.meta)
    }

    #[lore_macro::lore_instrument]
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let lease = self.lease(RequestKind::Get, Some(address));
        let client = lease.client;

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
            operation_labels(RequestKind::Get, lease.slot),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::Get"))
        .await
        .output;

        handle_service_response(service_result, &self, lease.slot, lease.meta)
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
        let lease = self.lease(RequestKind::Put, Some(address));
        let client = lease.client;

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
            operation_labels(RequestKind::Put, lease.slot),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::Put"))
        .await
        .output;

        handle_service_response(service_result, &self, lease.slot, lease.meta)
    }

    #[lore_macro::lore_instrument]
    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let lease = self.lease(RequestKind::Obliterate, Some(address));
        let client = lease.client;

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
            operation_labels(RequestKind::Obliterate, lease.slot),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::Obliterate"))
        .await
        .output;

        let response = handle_service_response(service_result, &self, lease.slot, lease.meta)?;
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

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
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
        let lease = self.lease(RequestKind::Copy, Some(source_address));
        let client = lease.client;

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
            operation_labels(RequestKind::Copy, lease.slot),
            observe_client_interaction(),
        )
        .instrument(info_span!("ReplicatedStore::Copy"))
        .await
        .output;

        handle_service_response(service_result, &self, lease.slot, lease.meta)
    }
}

#[lore_macro::test_pub]
fn handle_service_response<ResponseType, ClientType>(
    result: Result<ResponseType, ReplicationStoreClientError>,
    store: &ReplicatedStore<ClientType>,
    slot: usize,
    meta: ServiceRequestMeta,
) -> Result<ResponseType, StoreError>
where
    ClientType: StoreClient,
{
    match result {
        Ok(output) => Ok(output),
        Err(ReplicationStoreClientError::ConnectionFailed) => {
            if store.client_containers[slot].report_connection_failed(meta.client_epoch) {
                store.connection_failures[slot].notify_one();
            }
            Err(StoreError::internal("connection failed"))
        }
        Err(error) => Err(map_client_error_to_store_error(error, &meta)),
    }
}

/// Owns every reconnect of one slot: the periodic refresh, and reconnecting after a connection
/// failure until the slot is healthy again.
async fn supervise_slot<ClientType: StoreClient>(
    slot: usize,
    container: Arc<ClientContainer<ClientType>>,
    connection_failed: Arc<Notify>,
    instruments: ReplicatedStoreInstruments,
    first_refresh: Duration,
    refresh_period: Duration,
) {
    let mut interval = tokio::time::interval_at(Instant::now() + first_refresh, refresh_period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        if !container.is_healthy() {
            // Requests skip an unhealthy slot, so no later failure would prompt another attempt.
            // Each attempt starts by waiting out the container's connection-lost sleep, which
            // paces these retries.
            let result = regenerate_slot(
                slot,
                &container,
                &instruments,
                GenerateClientReason::ConnectionFailed,
            )
            .await;
            match result {
                Ok(true) => {
                    // A refresh due during the outage would otherwise fire at once and replace
                    // the client just connected.
                    interval.reset();
                    continue;
                }
                // Something else holds the slot's regeneration. Retrying straight away would spin
                // on it, so wait for the next refresh or failure report instead.
                Ok(false) => {}
                Err(err) => {
                    error!(
                        ?err,
                        client_slot = slot,
                        "Failed to regenerate client after a connection failure"
                    );
                    continue;
                }
            }
        }
        tokio::select! {
            _ = interval.tick() => {
                let result = regenerate_slot(
                    slot,
                    &container,
                    &instruments,
                    GenerateClientReason::PeriodicRefresh,
                )
                .await;
                if let Err(err) = result {
                    warn!(
                        ?err,
                        client_slot = slot,
                        "Error periodic refreshing Replicated Store client"
                    );
                }
            }
            () = connection_failed.notified() => {}
        }
    }
}

/// Caution: the concrete QUIC client requires an execution context
async fn regenerate_slot<ClientType: StoreClient>(
    slot: usize,
    container: &ClientContainer<ClientType>,
    instruments: &ReplicatedStoreInstruments,
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
        labels.extend(instruments.provider.labels().iter().cloned());
        labels.push(reason_label);
        labels.push(slot_label(slot));
        labels
    };
    container
        .regenerate_client(container.epoch(), reason)
        .observe(
            instruments.regenerate_latency_histogram.clone(),
            labels,
            observe_regenerate(),
        )
        .await
        .output
}

async fn monitor_clients<ClientType: StoreClient>(
    containers: Vec<Arc<ClientContainer<ClientType>>>,
    instruments: ReplicatedStoreInstruments,
    monitor_interval: Duration,
) {
    let slot_metrics: Vec<ClientMetrics> = (0..containers.len())
        .map(|slot| {
            let mut labels = instruments.provider.labels().to_vec();
            labels.push(slot_label(slot));
            ClientMetrics::new("replicated_store", labels)
        })
        .collect();

    let mut interval = tokio::time::interval(monitor_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Burst);
    interval.tick().await; // skip immediate first tick
    loop {
        interval.tick().await;
        for (slot, container) in containers.iter().enumerate() {
            for kind in RequestKind::ALL {
                instruments.inflight_requests_gauge.record(
                    container.inflight(kind) as u64,
                    &operation_labels(kind, slot),
                );
            }
        }
        join_all(
            containers
                .iter()
                .zip(&slot_metrics)
                .map(|(container, metrics)| async move {
                    // A stalled connection can be slow to report its stats. Each slot reports as
                    // soon as its stats arrive. If they take longer than one reporting interval,
                    // that slot misses the tick.
                    let stats =
                        tokio::time::timeout(monitor_interval, container.connection_stats());
                    if let Ok(Some(stats)) = stats.await {
                        metrics.observe(&stats);
                    }
                }),
        )
        .await;
    }
}

fn operation_labels(kind: RequestKind, slot: usize) -> LabelArray {
    let mut labels = ReplicatedStoreProvider.get_labels_for_operation_context(kind.as_str());
    labels.push(slot_label(slot));
    labels
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
    /// Sampled per slot and request kind, counting only requests on each slot's current client.
    inflight_requests_gauge: Gauge<u64>,
    provider: ReplicatedStoreProvider,
}

impl ReplicatedStoreInstruments {
    fn new(instrument_provider: ReplicatedStoreProvider) -> Self {
        Self {
            regenerate_latency_histogram: instrument_provider
                .latency_histogram_ms("client.regenerate.duration"),
            immutable_operation_latency_histogram: instrument_provider
                .latency_histogram_ms("immutable.operation_duration"),
            inflight_requests_gauge: instrument_provider.gauge("client.inflight_requests"),
            provider: instrument_provider,
        }
    }
}

fn slot_label(slot: usize) -> KeyValue {
    KeyValue::new("client_slot", slot as i64)
}

/// How long after startup a slot first refreshes its client. The slots are spaced evenly across
/// one refresh period, so that normally at most one is reconnecting at a time. Each is then
/// delayed by up to half that spacing, scaled by `jitter` between 0 and 1, so that servers started
/// together don't all reconnect to the remote together.
#[lore_macro::test_pub]
fn first_refresh_delay(slot: usize, num_slots: usize, period: Duration, jitter: f64) -> Duration {
    let spacing = period.div_f64(num_slots as f64);
    spacing.mul_f64((slot + 1) as f64 + jitter / 2.0)
}
