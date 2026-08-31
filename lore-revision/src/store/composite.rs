// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;

pub mod replica_factory;
mod topology_refresh;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME;
use lore_telemetry::METRICS_SUCCESS_ATTRIBUTE_NAME;
use lore_telemetry::observe::ObserveResult;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::StoreObliterateStats;
use crate::cluster::peer::PeerInfo;
use crate::cluster::topology::Topology;
use crate::errors::AddressNotFound;
use crate::errors::SlowDown;
use crate::fragment::FragmentFlags;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Fragment;
use crate::lore::Hash;
use crate::lore::Partition;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_warn;
use crate::store::ImmutableStore;
use crate::store::StoreError;
use crate::store::StoreGetData;
use crate::store::StoreMatch;
use crate::store::StoreMatchResult;
use crate::store::composite::replica_factory::ReplicaFactory;
use crate::store::composite::topology_refresh::TopologyRefreshSubscription;
use crate::store::query_one;
use crate::util::inflight::InflightOutput;
use crate::util::inflight::RequestRole;

const METRICS_REPLICA_TYPE_LABEL: &str = "replica_type";

type InflightGetsKey = (Partition, Address);

/// A target for a local store
#[derive(Clone)]
struct LocalTarget {
    target: Arc<dyn ImmutableStore>,
    name: String,
}

impl Debug for LocalTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Local[name={}]", self.name)
    }
}

impl LocalTarget {
    fn store(&self) -> Arc<dyn ImmutableStore> {
        self.target.clone()
    }
}

/// A target for read/write immutable store replication
#[derive(Clone)]
pub struct ReplicationTarget {
    target: Arc<dyn ImmutableStore>,
    name: String,
    peer_info: Option<PeerInfo>,
}

impl Debug for ReplicationTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Replication[name={}, peer_info={:?}]",
            self.name, self.peer_info
        )
    }
}

impl ReplicationTarget {
    pub fn new(peer_info: PeerInfo, target: Arc<dyn ImmutableStore>) -> Self {
        let name = peer_info.to_string();
        Self {
            target,
            name,
            peer_info: Some(peer_info),
        }
    }

    fn store(&self) -> Arc<dyn ImmutableStore> {
        self.target.clone()
    }

    pub fn peer_info(&self) -> &Option<PeerInfo> {
        &self.peer_info
    }
}

/// A target for a durable store
#[derive(Clone)]
struct DurableTarget {
    target: Arc<dyn ImmutableStore>,
    name: String,
}

impl Debug for DurableTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Durable[name={}]", self.name)
    }
}

impl DurableTarget {
    fn store(&self) -> Arc<dyn ImmutableStore> {
        self.target.clone()
    }
}

enum CompositeStoreHit<T> {
    Local(T),
    Durable(T),
    Replica(T),
    Mixed(T),
    Miss(T),
}

impl<T> CompositeStoreHit<T> {
    fn inner(&self) -> &T {
        match self {
            CompositeStoreHit::Local(v)
            | CompositeStoreHit::Durable(v)
            | CompositeStoreHit::Replica(v)
            | CompositeStoreHit::Mixed(v)
            | CompositeStoreHit::Miss(v) => v,
        }
    }

    /// Consume the inner value, wrapping it as a `Result::Ok`, while also counting a metric labeled
    /// with the type of hit.
    fn into_counted_result<E>(self, counter: &Counter<u64>) -> Result<T, E> {
        let (kind, value) = match self {
            CompositeStoreHit::Local(v) => ("local", v),
            CompositeStoreHit::Durable(v) => ("durable", v),
            CompositeStoreHit::Replica(v) => ("replica", v),
            CompositeStoreHit::Mixed(v) => ("mixed", v),
            CompositeStoreHit::Miss(v) => ("miss", v),
        };

        counter.add(1, &[STORE_ATTRIBUTE.clone(), KeyValue::new("found", kind)]);

        Ok(value)
    }
}

/// Fold one store's answer into the running one. The level is the best any store established and
/// durability is whatever any of them knows, since a replica holding the fragment holds what it was
/// told about where the payload lives. `stored_local` is the exception and stays as the local store
/// left it: a replica reporting content on *its* disk says nothing about ours.
fn merge_resolved(into: &mut StoreMatchResult, from: &StoreMatchResult) -> bool {
    let mut is_new_stronger = false;

    // The partition and the context travel with the level that won, never merged on their own: they
    // name where *that* store found the content, and pairing one store's source with another's
    // level would point a copy at somewhere the content was never seen. They move together for the
    // same reason — a context is only meaningful inside the partition it was found in.
    if from.match_made > into.match_made {
        into.match_made = from.match_made;
        into.partition = from.partition;
        into.context = from.context;
        is_new_stronger = true;
    }
    into.stored_durable |= from.stored_durable;

    is_new_stronger
}

/// The association a put can be satisfied by copying, or `None` where its payload has to be stored.
///
/// A partial match is what names another association, `stored_durable` on it is what says the durable
/// store has that one rather than only the cache, and a match naming no partition cannot be aimed at.
/// A supplied payload is the remaining condition, checked by the caller: ingress verifies it against
/// the address, so a caller holding one could have stored the content the long way for the same
/// result.
fn can_put_use_copy(resolved: &StoreMatchResult, hash: Hash) -> Option<(Partition, Address)> {
    if !matches!(
        resolved.match_made,
        StoreMatch::MatchPartition | StoreMatch::MatchHash
    ) {
        return None;
    }
    if !resolved.stored_durable || resolved.partition.is_zero() {
        return None;
    }
    Some((resolved.partition, resolved.source_address(hash)))
}

/// Default number of permits for the `cache_metadata` semaphore.
pub const DEFAULT_QUERY_CACHE_SEMAPHORE_SIZE: usize = 1000;

#[error_set]
pub enum CompositeStoreBuilderError {}

/// Used to construct a composite store instance. Takes care of sorting the read and write chains
/// when building the `CompositeStore`. Also enforces some basic sanity checks, namely that there
/// must be exactly one durable store in the write chain.
#[derive(Default, Debug)]
pub struct CompositeStoreBuilder {
    /// Local store
    local: Option<LocalTarget>,
    /// Non-durable read replicas
    read_replicas: Vec<ReplicationTarget>,
    /// Non-durable write replicas
    write_replicas: Vec<ReplicationTarget>,
    /// Durable read upstream (or local)
    durable: Option<DurableTarget>,
    /// Factory called to create a `ReplicationTarget` from a `PeerInfo`
    peer_replica_builder: Option<Arc<dyn ReplicaFactory>>,
    cache_metadata: bool,
    /// Semaphore size for write-backs. `None` uses [`DEFAULT_QUERY_CACHE_SEMAPHORE_SIZE`].
    cache_metadata_semaphore_size: Option<usize>,
    /// If true, the local store only caches fragment metadata (no payloads).
    /// Payloads are only stored in the durable store and replicas.
    /// Local `get()` calls that hit metadata-only entries fall through to durable/replicas for payloads.
    local_metadata_only: bool,
    /// The delay applied to durable store operations so replicas are given a chance
    /// to fulfill a request first. `Duration` default is 0 length (no delay)
    durable_delay: Duration,
}

impl CompositeStoreBuilder {
    /// Cache remote metadata locally so future `query` and `get_metadata` calls can be served
    /// in-process rather than going to the durable store. `semaphore_size` bounds concurrent
    /// write-backs; `None` uses [`DEFAULT_QUERY_CACHE_SEMAPHORE_SIZE`].
    pub fn with_cache_metadata(
        mut self,
        cache_metadata: bool,
        semaphore_size: Option<usize>,
    ) -> Self {
        self.cache_metadata = cache_metadata;
        self.cache_metadata_semaphore_size = semaphore_size;
        self
    }

    /// When enabled, the local store only caches fragment metadata (no payloads).
    /// Payloads are only stored in the durable store and replicas.
    /// Local `get()` calls that need the payload will fall through to durable/replicas.
    pub fn with_local_metadata_only(mut self, local_metadata_only: bool) -> Self {
        self.local_metadata_only = local_metadata_only;
        self
    }

    /// Add a target to the composite store read/write replicas
    pub fn with_replica(
        mut self,
        name: String,
        target: Arc<dyn ImmutableStore>,
        read: bool,
        write: bool,
    ) -> Self {
        let target = ReplicationTarget {
            target: target.clone(),
            name: name.clone(),
            peer_info: None,
        };
        if read {
            lore_debug!("Adding target {name} to read replicas");
            self.read_replicas.push(target.clone());
        }
        if write {
            lore_debug!("Adding target {name} to write replicas");
            self.write_replicas.push(target);
        }
        self
    }

    /// Add a target to the composite store as the local store
    pub fn with_local(
        mut self,
        name: String,
        target: Arc<dyn ImmutableStore>,
    ) -> Result<Self, CompositeStoreBuilderError> {
        if self.local.is_some() {
            return Err(CompositeStoreBuilderError::internal(
                "too many local stores",
            ));
        }
        let target = LocalTarget {
            target: target.clone(),
            name: name.clone(),
        };
        lore_debug!("Adding target {name} as local store");
        self.local = Some(target);
        Ok(self)
    }

    /// Add a target to the composite store as the durable store
    pub fn with_durable(
        mut self,
        name: String,
        target: Arc<dyn ImmutableStore>,
    ) -> Result<Self, CompositeStoreBuilderError> {
        if self.durable.is_some() {
            return Err(CompositeStoreBuilderError::internal(
                "too many durable stores",
            ));
        }
        let target = DurableTarget {
            target: target.clone(),
            name: name.clone(),
        };
        lore_debug!("Adding target {name} as durable store");
        self.durable = Some(target);
        Ok(self)
    }

    pub fn with_durable_delay(mut self, duration: Duration) -> Self {
        self.durable_delay = duration;
        self
    }

    pub fn with_replica_builder(mut self, builder: Arc<dyn ReplicaFactory>) -> Self {
        self.peer_replica_builder = Some(builder);
        self
    }

    pub fn build(self) -> Result<CompositeStore, CompositeStoreBuilderError> {
        let Some(durable) = self.durable else {
            return Err(CompositeStoreBuilderError::internal(
                "no durable store found",
            ));
        };

        let mut local_durable = false;
        let local = self.local.unwrap_or_else(|| {
            local_durable = true;
            LocalTarget {
                target: durable.target.clone(),
                name: "durable".to_string(),
            }
        });

        let cache_metadata_semaphore = Arc::new(Semaphore::new(
            self.cache_metadata_semaphore_size
                .unwrap_or(DEFAULT_QUERY_CACHE_SEMAPHORE_SIZE),
        ));

        let provider = CompositeStoreInstrumentProvider;
        Ok(CompositeStore {
            local: Arc::new(local),
            read_replicas: self.read_replicas.into(),
            write_replicas: self.write_replicas.into(),
            durable,
            durable_delay: self.durable_delay,
            local_durable,
            cache_metadata: self.cache_metadata,
            cache_metadata_semaphore,
            local_metadata_only: self.local_metadata_only,
            peers_refreshed_guard: Semaphore::new(1),
            peer_replica_builder: self.peer_replica_builder,
            topology_subscription: None.into(),
            instruments: CompositeStoreInstruments {
                counter_get: provider.counter("get"),
                counter_put: provider.counter("put"),
                counter_query: provider.counter("query"),
                counter_get_metadata: provider.counter("get_metadata"),
                gauge_num_replicas: provider.gauge("topology.refresh.num_peers"),
                topology_refresh_num_changes: provider.counter("topology.refresh.num_changes"),
                topology_refresh_num_peer_errors: provider
                    .counter("topology.refresh.num_peer_errors"),
                topology_refresh_duration: provider
                    .latency_histogram_ms("topology.refresh.iteration.duration"),
                counter_get_inflight_receiver: provider.counter("get.inflight.receiver"),
                counter_local_caching: provider.counter("local.caching_total"),

                provider,
            },
            inflight_gets: Default::default(),
        })
    }
}

#[error_set]
pub enum PeersRefreshedError {}

/// A store that is able to propagate read and write operations to local, durable and replica stores.
///
/// Read immutable operations try local store first, if no required match is made it then goes wide
/// and reads from read durable store and read replicas. First successful result will be early returned.
///
/// Write immutable operations will block on writing to the write durable store.
/// It will then detach spawn tasks to cache data in local store and write replicas.
///
/// Query immutable operations will try local store first, if no suitable match is made it then goes wide
/// and queries read durable store and read replicas. The first required match will be early returned.
/// If no required match is made the best match is returned after all queries complete.
///
/// Read/write/compare-and-swap mutable operations are always deferred to read/write durable store.
#[derive(Debug)]
pub struct CompositeStore {
    /// Local store, always exist
    local: Arc<LocalTarget>,
    /// Look aside read replicas
    read_replicas: RwLock<Vec<ReplicationTarget>>,
    /// Look aside write replicas
    write_replicas: RwLock<Vec<ReplicationTarget>>,
    /// Durable read store
    durable: DurableTarget,
    /// The delay applied to durable store operations so replicas are given a chance
    /// to fulfill a request first
    durable_delay: Duration,
    /// Flag if local store is durable
    local_durable: bool,
    /// Caching remote metadata locally means `query` and `get_metadata` can be served in-process
    /// rather than going to the durable store on repeated lookups.
    cache_metadata: bool,
    /// Caps concurrent background write-backs so cache activity cannot grow unboundedly.
    cache_metadata_semaphore: Arc<Semaphore>,
    /// If true, local store only caches metadata (no payloads)
    local_metadata_only: bool,

    peer_replica_builder: Option<Arc<dyn ReplicaFactory>>,
    peers_refreshed_guard: Semaphore,
    topology_subscription: RwLock<Option<TopologyRefreshSubscription>>,

    instruments: CompositeStoreInstruments,

    inflight_gets: InflightOutput<InflightGetsKey, Result<StoreGetData, StoreError>>,
}

pub struct ReevaluatePeersSummary {
    pub detected_new_peers: HashSet<PeerInfo>,
    pub num_new_peers_errors: usize,
    pub lost_peers: HashSet<PeerInfo>,
}

/// Container representing a typical composite store operation,
/// where many requests are fanned out to durable and replica targets.
struct CompositeOperation<T>
where
    T: 'static,
{
    cancellation_token: CancellationToken,
    queries: JoinSet<T>,
}

impl<T> CompositeOperation<T> {
    fn new() -> Self {
        Self {
            queries: JoinSet::new(),
            cancellation_token: CancellationToken::new(),
        }
    }
}

impl<T> Drop for CompositeOperation<T>
where
    T: 'static,
{
    fn drop(&mut self) {
        self.cancellation_token.cancel();
        // not every target of a composite store operation is cancel
        // safe, e.g. QUIC futures writing to a connection.
        // Therefore, it is safest to detach all
        self.queries.detach_all();
    }
}

impl CompositeStore {
    pub fn local(&self) -> Arc<dyn ImmutableStore> {
        self.local.target.clone()
    }

    pub fn durable(&self) -> Arc<dyn ImmutableStore> {
        self.durable.target.clone()
    }

    pub async fn set_topology_subscription(
        self: Arc<Self>,
        topology: Arc<dyn Topology + Send + Sync>,
    ) {
        let subscription = TopologyRefreshSubscription::new(topology, self.clone());
        let mut write = self.topology_subscription.write().await;
        *write = Some(subscription);
    }

    pub async fn topology_peers_refreshed(
        &self,
        refreshed_peers: HashSet<PeerInfo>,
    ) -> Result<ReevaluatePeersSummary, PeersRefreshedError> {
        let _guard = self.peers_refreshed_guard.acquire().await;

        let refresh = async move {
            let Some(builder) = &self.peer_replica_builder else {
                return Err(PeersRefreshedError::internal("no builder set"));
            };

            let current_peers: HashSet<PeerInfo> = {
                let write_replicas = self.write_replicas.read().await;
                let read_replicas = self.read_replicas.read().await;
                write_replicas
                    .iter()
                    .chain(read_replicas.iter())
                    .filter_map(|replica| replica.peer_info.clone())
                    .collect()
            };

            lore_debug!("current_peers '{current_peers:?}'");

            let detected_new_peers: HashSet<PeerInfo> = refreshed_peers
                .difference(&current_peers)
                .cloned()
                .collect();
            lore_debug!("detected_new_peers '{detected_new_peers:?}'");

            let lost_peers: HashSet<PeerInfo> = current_peers
                .difference(&refreshed_peers)
                .cloned()
                .collect();
            lore_debug!("lost_peers '{lost_peers:?}'");

            let num_detected_new_peers = detected_new_peers.len();
            let num_lost_peers = lost_peers.len();
            let num_refreshed_peers = refreshed_peers.len();
            let mut num_new_peers_errors = 0;

            if num_detected_new_peers > 0 || num_lost_peers > 0 {
                let new_target_results = builder
                    .clone()
                    .make_replica_targets(&detected_new_peers)
                    .await
                    .internal("joining replica build tasks")?;

                {
                    let mut write = self.write_replicas.write().await;
                    let mut read = self.read_replicas.write().await;

                    // remove lost peers from both read and write replica lists
                    let retain_peer = |replica: &ReplicationTarget| {
                        if let Some(info) = &replica.peer_info
                            && lost_peers.contains(info)
                        {
                            false
                        } else {
                            true
                        }
                    };
                    write.retain(retain_peer);
                    read.retain(retain_peer);

                    // add new targets
                    for result in new_target_results {
                        match result {
                            Ok(targets) => {
                                if let Some(write_target) = targets.write {
                                    write.push(write_target);
                                }
                                if let Some(read_target) = targets.read {
                                    read.push(read_target);
                                }
                            }
                            // don't let one bad apple ruin the bunch. There is still value in having the other
                            // replicas hooked up
                            Err(error) => {
                                lore_warn!("failed to make peer - ignoring: {error}");
                                num_new_peers_errors += 1;
                            }
                        }
                    }
                }
            }
            lore_debug!(
                "composite store refresh end: num_new_peers_errors '{num_new_peers_errors}' num_refreshed_peers '{num_refreshed_peers}'"
            );
            Ok(ReevaluatePeersSummary {
                detected_new_peers,
                num_new_peers_errors,
                lost_peers,
            })
        };

        let refresh_result = refresh
            .observe_result(
                self.instruments.topology_refresh_duration.clone(),
                self.instruments.provider.labels().into(),
            )
            .await
            .output;
        if let Ok(output) = &refresh_result {
            self.instruments.topology_refresh_num_changes.add(
                (output.lost_peers.len() + output.detected_new_peers.len()) as u64,
                &[],
            );
            self.instruments
                .topology_refresh_num_peer_errors
                .add(output.num_new_peers_errors as u64, &[]);
        }
        self.instruments.gauge_num_replicas.record(
            self.write_replicas.read().await.len() as u64,
            &[KeyValue::new(METRICS_REPLICA_TYPE_LABEL, "write")],
        );
        self.instruments.gauge_num_replicas.record(
            self.read_replicas.read().await.len() as u64,
            &[KeyValue::new(METRICS_REPLICA_TYPE_LABEL, "read")],
        );

        refresh_result
    }

    pub async fn clone_read_replicas(&self) -> Vec<ReplicationTarget> {
        self.read_replicas.read().await.clone()
    }

    pub async fn clone_write_replicas(&self) -> Vec<ReplicationTarget> {
        self.write_replicas.read().await.clone()
    }

    /// At this given moment in time, how much should the durable store be delayed by?
    /// The delay is pointless if there are no read replicas to take advantage of
    async fn get_durable_delay_for_operation(&self) -> Duration {
        if self.read_replicas.read().await.is_empty() {
            Duration::ZERO
        } else {
            self.durable_delay
        }
    }

    /// Detached fan-out of a put to every write replica, skipped where a topology refresh holds the
    /// list: replicas accelerate reads rather than own content, so waiting on it is not worth a put.
    fn replicate_put(
        &self,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) {
        let Ok(write_replicas) = self.write_replicas.try_read() else {
            return;
        };
        for replica in write_replicas.iter() {
            let replica_store = replica.store();
            let payload = payload.clone();
            lore_spawn!(async move {
                replica_store
                    .put(partition, address, fragment, payload, force)
                    .await
            });
        }
    }

    /// Detached fan-out of a copy to every write replica, skipped for the reason on
    /// [`Self::replicate_put`].
    fn replicate_copy(
        &self,
        source_partition: Partition,
        source_address: Address,
        partition: Partition,
        context: Context,
    ) {
        let Ok(write_replicas) = self.write_replicas.try_read() else {
            return;
        };
        for replica in write_replicas.iter() {
            let replica_store = replica.store();
            lore_spawn!(async move {
                replica_store
                    .copy(source_partition, source_address, partition, context, true)
                    .await
            });
        }
    }

    async fn get_from_remotes(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let mut fan_out = CompositeOperation::new();
        let queries = &mut fan_out.queries;

        if !self.local_durable {
            let cancel_token = fan_out.cancellation_token.clone();
            let delay = self.get_durable_delay_for_operation().await;
            let durable_store = self.durable.store();
            lore_spawn!(queries, async move {
                tokio::time::sleep(delay).await;
                if cancel_token.is_cancelled() {
                    (true, Err(StoreError::internal("cancelled")))
                } else {
                    let durable_result = durable_store
                        .get(partition, address)
                        .await
                        .map(CompositeStoreHit::Durable);
                    (true, durable_result)
                }
            });
        }
        {
            let read_replicas = self.read_replicas.read().await;
            for replica in read_replicas.iter() {
                let replica_store = replica.store();
                lore_spawn!(queries, async move {
                    let replica_result = replica_store
                        .get(partition, address)
                        .await
                        .map(CompositeStoreHit::Replica);
                    (false, replica_result)
                });
            }
        }

        let mut error_to_return = StoreError::from(AddressNotFound::from(address));
        while let Some(join_result) = queries.join_next().await {
            let Ok((is_durable, query_result)) = join_result else {
                continue;
            };
            match query_result {
                Ok(result) => {
                    // If the durable store was the first to answer, then either the
                    // replicas are too slow or don't have the fragment, so we should build up
                    // our own local cache
                    if !self.local_durable && matches!(result, CompositeStoreHit::Durable(_)) {
                        // Cache the found result locally
                        let local_store = self.local.store();
                        let mut fragment = result.inner().fragment;
                        let cache_payload = if self.local_metadata_only {
                            None
                        } else {
                            result.inner().payload.clone()
                        };
                        let cache_counter = self.instruments.counter_local_caching.clone();
                        lore_spawn!(async move {
                            fragment.flags |= FragmentFlags::PayloadStoredLocal
                                | FragmentFlags::PayloadStoredDurable;
                            let put_result = local_store
                                .put(partition, address, fragment, cache_payload, false)
                                .await;
                            count_result("put_after_get", &cache_counter, &put_result);
                            put_result
                        });
                    }
                    return result.into_counted_result(&self.instruments.counter_get);
                }
                Err(StoreError::SlowDown(_)) => {
                    error_to_return = StoreError::from(SlowDown);
                }
                Err(err) => {
                    let is_internal_error = err.is_internal();
                    // durable is the source of truth - if it error'd bubble its error up and forget
                    // about the replicas
                    if is_durable && !is_internal_error {
                        error_to_return = err;
                        break;
                    }
                }
            }
        }

        Err(error_to_return)
    }
}

#[async_trait]
impl ImmutableStore for CompositeStore {
    /// The local store is the one a read is served from without leaving the process, so its setting
    /// is the one that decides whether bytes can cross a partition here.
    fn isolates_partitions(&self) -> bool {
        self.local.target.isolates_partitions()
    }

    async fn is_available(self: Arc<Self>, timeout: Duration) -> bool {
        let (local_available, durable_available) = tokio::join!(
            self.local.target.clone().is_available(timeout),
            self.durable.target.clone().is_available(timeout)
        );

        if !local_available {
            lore_error!("local store is unavailable");
        }
        if !durable_available {
            lore_error!("durable store is unavailable");
        };

        local_available && durable_available
    }

    /// Local first, then one fan-out for whatever the local store left below a full match. Every
    /// responder is merged rather than raced: a store that establishes a better level than another
    /// keeps it, so a cache in front of a durable store can supply the partition match the durable
    /// store below it declines to establish.
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(addresses.len(), results.len());

        // A local store that cannot answer is not a failed resolve. Papering over one store being
        // unavailable is what a composite is for, so its error leaves every address unresolved and
        // the fan-out below answers them.
        if let Err(error) = self
            .local
            .store()
            .query(partition, addresses, results)
            .await
        {
            lore_debug!("local store failed to resolve, falling through: {error:?}");
            results.fill(StoreMatchResult::default());
        }

        let remaining = results
            .iter()
            .zip(addresses.iter())
            .enumerate()
            .filter_map(|(pos, (result, address))| {
                result.match_made.is_partial().then_some((pos, *address))
            })
            .collect::<Vec<_>>();

        if remaining.is_empty() {
            return CompositeStoreHit::Local(())
                .into_counted_result(&self.instruments.counter_query);
        }

        let pending = Arc::new(
            remaining
                .iter()
                .map(|(_, address)| *address)
                .collect::<Vec<_>>(),
        );

        let mut fan_out = CompositeOperation::new();
        let queries = &mut fan_out.queries;

        if !self.local_durable {
            let cancel_token = fan_out.cancellation_token.clone();
            let delay = self.get_durable_delay_for_operation().await;
            let durable_store = self.durable.store();
            let pending = pending.clone();
            lore_spawn!(queries, async move {
                tokio::time::sleep(delay).await;
                if cancel_token.is_cancelled() {
                    (true, Err(StoreError::internal("cancelled")))
                } else {
                    let mut scratch = vec![StoreMatchResult::default(); pending.len()];
                    let outcome = durable_store.query(partition, &pending, &mut scratch).await;
                    (true, outcome.map(|()| CompositeStoreHit::Durable(scratch)))
                }
            });
        }
        {
            let read_replicas = self.read_replicas.read().await;
            for replica in read_replicas.iter() {
                let replica_store = replica.store();
                let pending = pending.clone();
                lore_spawn!(queries, async move {
                    let mut scratch = vec![StoreMatchResult::default(); pending.len()];
                    let outcome = replica_store.query(partition, &pending, &mut scratch).await;
                    (false, outcome.map(|()| CompositeStoreHit::Replica(scratch)))
                });
            }
        }

        let mut failure = None;
        while let Some(join_result) = queries.join_next().await {
            let (is_durable_result, resolved) = match join_result {
                Ok(joined) => joined,
                Err(error) => {
                    failure = failure.or(Some(StoreError::internal_with_context(
                        error,
                        "Task failure",
                    )));
                    continue;
                }
            };

            match resolved {
                Ok(hit) => {
                    for (resolved, (pos, address)) in hit.inner().iter().zip(remaining.iter()) {
                        let is_stronger_match = merge_resolved(&mut results[*pos], resolved);

                        // If the results came from the durable store,
                        // and answered before the replicas could (i.e. they are too slow to be
                        // worth relying upon, or they couldn't answer),
                        // then cache the fragment metadata locally to build up our own local cache
                        if is_durable_result
                            && self.cache_metadata
                            && is_stronger_match
                            && resolved.match_made == StoreMatch::MatchFull
                            && !self.local_durable
                            && let Ok(permit) =
                                self.cache_metadata_semaphore.clone().try_acquire_owned()
                        {
                            let store = self.clone();
                            let match_partition = resolved.partition;
                            let address = *address;
                            let cache_counter = self.instruments.counter_local_caching.clone();
                            lore_spawn!(async move {
                                let _permit = permit;
                                let result = store.get_metadata(match_partition, address).await;
                                count_result("get_metadata_after_query", &cache_counter, &result);
                            });
                        }
                    }
                    // Durable is the source of truth, so its answers complete the set. Short of
                    // that, replicas are only worth waiting on while something is still partial.
                    if is_durable_result
                        || results.iter().all(|result| !result.match_made.is_partial())
                    {
                        failure = None;
                        break;
                    }
                }
                Err(StoreError::SlowDown(_)) => {
                    failure = Some(StoreError::from(SlowDown));
                }
                Err(error) => {
                    let is_internal_error = error.is_internal();
                    failure = failure.or(Some(error));
                    if is_durable_result && !is_internal_error {
                        break;
                    }
                }
            }
        }

        if let Some(failure) = failure {
            return Err(failure);
        }

        CompositeStoreHit::Mixed(()).into_counted_result(&self.instruments.counter_query)
    }

    /// Local first, then durable and read replicas in parallel. Like `query`, read replicas are
    /// consulted so that edge-region replicas can answer without a cross-region round trip to the
    /// durable store. Durable is the source of truth: when it responds the remaining replica
    /// futures are dropped.
    ///
    /// A representation that had to come from elsewhere is written back to the local store without
    /// its payload, so the next caller finds it in process.
    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        // Anything the local store matched is an answer: a level below `MatchFull` describes the
        // same bytes reached under another context, which is what it was asked to describe.
        if let Ok(result) = self
            .local
            .store()
            .get_metadata(partition, address)
            .await
            .map(CompositeStoreHit::Local)
            && result.inner().match_made != StoreMatch::MatchNone
        {
            return result.into_counted_result(&self.instruments.counter_get_metadata);
        }

        let mut fan_out = CompositeOperation::new();
        let queries = &mut fan_out.queries;

        if !self.local_durable {
            let cancel_token = fan_out.cancellation_token.clone();
            let delay = self.get_durable_delay_for_operation().await;
            let durable_store = self.durable.target.clone();
            lore_spawn!(queries, async move {
                tokio::time::sleep(delay).await;
                if cancel_token.is_cancelled() {
                    (true, Err(StoreError::internal("cancelled")))
                } else {
                    let durable_result = durable_store
                        .get_metadata(partition, address)
                        .await
                        .map(CompositeStoreHit::Durable);
                    (true, durable_result)
                }
            });
        }
        {
            let read_replicas = self.read_replicas.read().await;
            for replica in read_replicas.iter() {
                let replica_store = replica.target.clone();
                lore_spawn!(queries, async move {
                    let replica_result = replica_store
                        .get_metadata(partition, address)
                        .await
                        .map(CompositeStoreHit::Replica);
                    (false, replica_result)
                });
            }
        }

        let mut best_result = CompositeStoreHit::Miss(StoreGetData::default());

        while let Some(join_result) = queries.join_next().await {
            let Ok((is_durable, query_result)) = join_result else {
                continue;
            };
            match query_result {
                Ok(result) => {
                    let result_match = result.inner().match_made;
                    if result_match > best_result.inner().match_made {
                        best_result = result;
                        if result_match >= StoreMatch::MatchFull {
                            break;
                        }
                    }
                    if is_durable {
                        // durable is the source of truth — replicas will not be able to do better
                        break;
                    }
                }
                Err(error) => {
                    if is_durable && !error.is_slow_down() && !error.is_internal() {
                        break;
                    }
                }
            }
        }

        if self.cache_metadata
            && best_result.inner().match_made == StoreMatch::MatchFull
            // If the durable store was the first to answer, then either the
            // replicas are too slow or don't have the fragment, so we should build up
            // our own local cache
            && matches!(best_result, CompositeStoreHit::Durable(_))
            && !self.local_durable
        {
            let local_store = self.local.store();
            let fragment = best_result.inner().fragment;
            let partition = best_result.inner().partition;
            let cache_counter = self.instruments.counter_local_caching.clone();
            lore_spawn!(async move {
                let put_result = local_store
                    .put(
                        partition, address, fragment, None,  /* payload */
                        false, /* force */
                    )
                    .await;
                count_result("put_after_get_metadata", &cache_counter, &put_result);
                put_result
            });
        }

        best_result.into_counted_result(&self.instruments.counter_get_metadata)
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        if let Ok(result) = self
            .local
            .store()
            .get(partition, address)
            .await
            .map(CompositeStoreHit::Local)
        {
            return result.into_counted_result(&self.instruments.counter_get);
        }

        match self.inflight_gets.request((partition, address)) {
            RequestRole::RequestMaker(guard) => {
                let result = self.clone().get_from_remotes(partition, address).await;
                guard.broadcast(&result);
                result
            }
            RequestRole::ResultAwaiter(mut receiver) => {
                self.instruments.counter_get_inflight_receiver.add(1, &[]);
                receiver.recv().await.unwrap_or_else(|receive_error| {
                    Err(StoreError::internal_with_context(
                        receive_error,
                        "Failed to get inflight result",
                    ))
                })
            }
        }
    }

    /// A full match is written nowhere. A weaker one the durable store holds is written by
    /// duplicating that association — see [`can_put_use_copy`] — and the payload is released before
    /// the round trip, so a refused copy has nothing to fall back on and fails the put. Recovery is
    /// the caller's: it still holds the payload and retries. Replicas are issued the same copy, and
    /// one that cannot answer it simply does not hold the association.
    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        let resolved = if force {
            StoreMatchResult::default()
        } else {
            query_one(&self.local.store(), partition, address)
                .await
                .unwrap_or_default()
        };
        if resolved.match_made == StoreMatch::MatchFull {
            return CompositeStoreHit::Local(()).into_counted_result(&self.instruments.counter_put);
        }

        let mut fragment = fragment;
        fragment.flags |= if self.local_durable {
            FragmentFlags::PayloadStoredLocal | FragmentFlags::PayloadStoredDurable
        } else {
            FragmentFlags::PayloadStoredDurable
        };
        let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

        if payload.is_some()
            && let Some((source_partition, source_address)) =
                can_put_use_copy(&resolved, address.hash)
        {
            drop(payload);

            self.clone()
                .copy(
                    source_partition,
                    source_address,
                    partition,
                    address.context,
                    true,
                )
                .await?;
            if !behaviour.do_not_replicate {
                self.replicate_copy(source_partition, source_address, partition, address.context);
            }

            return CompositeStoreHit::Durable(())
                .into_counted_result(&self.instruments.counter_put);
        }

        // Store durably
        self.durable
            .store()
            .put(partition, address, fragment, payload.clone(), force)
            .await?;

        // Durable store succeeded, safe to cache and replicate
        if !self.local_durable {
            // Cache detached in local store if it is not the durable store
            let local = self.local.store();
            let local_payload = if self.local_metadata_only {
                None // Strip payload — local store only caches metadata
            } else {
                payload.clone()
            };
            fragment.flags |= FragmentFlags::PayloadStoredLocal;
            let cache_counter = self.instruments.counter_local_caching.clone();
            lore_spawn!(async move {
                let put_result = local
                    .put(partition, address, fragment, local_payload, force)
                    .await;
                count_result("put", &cache_counter, &put_result);
                put_result
            });
        }

        if !behaviour.do_not_replicate {
            self.replicate_put(partition, address, fragment, payload, force);
        }

        CompositeStoreHit::Miss(()).into_counted_result(&self.instruments.counter_put)
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        if !self.local_durable {
            // Detached obliterate local store if it is not the durable store
            let local = self.local.target.clone();

            lore_spawn!(async move {
                // The overall stats we care about returning are those from the durable store, so we
                // construct a stats instance dedicated to the local obliteration.
                let stats = Arc::new(StoreObliterateStats::default());
                match local.obliterate(partition, address, stats.clone()).await {
                    Ok(_) => {
                        lore_debug!(
                            "Successfully obliterated from local store for address: {address}, stats: {stats:?}"
                        );
                    }
                    // It's "ok" if the local obliterate fails, because we'll receive an event that
                    // we can/will retry until successful.
                    Err(e) => {
                        lore_error!(
                            "Failed to obliterate from local store for address: {address}: {e:?}"
                        );
                    }
                }
            });
        }

        // There's no need to explicitly send the message on to replicas, as that will be handled by
        // notification-driven replication.

        // Obliterate from durable store
        self.durable
            .store()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.local
            .store()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.local
            .store()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.local.store().compact_resume_at().await
    }

    async fn stop_gc(self: Arc<Self>, terminate: bool) {
        // Collected before awaiting so no replica lock is held across the drain, then driven
        // together so every target is asked to stop on the first poll rather than each
        // waiting out the one before it.
        let mut stores = vec![self.local.store(), self.durable.store()];
        stores.extend(
            self.read_replicas
                .read()
                .await
                .iter()
                .map(ReplicationTarget::store),
        );
        stores.extend(
            self.write_replicas
                .read()
                .await
                .iter()
                .map(ReplicationTarget::store),
        );

        futures::future::join_all(stores.into_iter().map(|store| store.stop_gc(terminate))).await;
    }

    fn max_query_batch(&self) -> Option<usize> {
        let mut max_query_batch = self.local.store().max_query_batch();
        if let Some(durable_max) = self.durable.store().max_query_batch()
            && durable_max > 0
        {
            if let Some(current_max) = max_query_batch
                && current_max > 0
            {
                max_query_batch = max_query_batch.min(Some(durable_max));
            } else {
                max_query_batch = Some(durable_max);
            }
        }
        max_query_batch
    }

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
        self.local.store().flush(sync_data).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.local.store().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        durable: bool,
    ) -> Result<(), StoreError> {
        self.durable
            .store()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                durable,
            )
            .await?;

        if !self.local_durable {
            // The local mirror reflects whatever durability the durable side just confirmed.
            let local = self.local.store();
            let cache_counter = self.instruments.counter_local_caching.clone();
            lore_spawn!(async move {
                let copy_result = local
                    .copy(
                        source_partition,
                        source_address,
                        destination_partition,
                        destination_context,
                        durable,
                    )
                    .await;
                count_result("copy", &cache_counter, &copy_result);
                copy_result
            });
        }

        Ok(())
    }
}

fn count_result<T, E>(context: &'static str, counter: &Counter<u64>, result: &Result<T, E>) {
    counter.add(
        1,
        &[
            KeyValue::new(METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME, context),
            KeyValue::new(METRICS_SUCCESS_ATTRIBUTE_NAME, result.is_ok()),
        ],
    );
}

static STORE_ATTRIBUTE: LazyLock<KeyValue> = LazyLock::new(|| KeyValue::new("store", "composite"));

#[derive(Debug)]
struct CompositeStoreInstrumentProvider;

impl InstrumentProvider for CompositeStoreInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.store.immutable.composite"
    }
}

#[derive(Debug)]
struct CompositeStoreInstruments {
    provider: CompositeStoreInstrumentProvider,

    counter_put: Counter<u64>,
    counter_get: Counter<u64>,
    counter_query: Counter<u64>,
    counter_get_metadata: Counter<u64>,
    gauge_num_replicas: Gauge<u64>,
    topology_refresh_num_changes: Counter<u64>,
    topology_refresh_num_peer_errors: Counter<u64>,
    topology_refresh_duration: Histogram<f64>,
    counter_get_inflight_receiver: Counter<u64>,
    counter_local_caching: Counter<u64>,
}
