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

use super::StoreObliterateStats;
use crate::cluster::peer::PeerInfo;
use crate::cluster::topology::Topology;
use crate::errors::AddressNotFound;
use crate::errors::SlowDown;
use crate::fragment::FragmentFlags;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Fragment;
use crate::lore::Partition;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_warn;
use crate::store::ImmutableStore;
use crate::store::StoreError;
use crate::store::StoreMatch;
use crate::store::StoreQueryResult;
use crate::store::composite::replica_factory::ReplicaFactory;
use crate::store::composite::topology_refresh::TopologyRefreshSubscription;
use crate::util::inflight::InflightOutput;
use crate::util::inflight::RequestRole;

const METRICS_REPLICA_TYPE_LABEL: &str = "replica_type";

type InfightGetsKey = (Partition, Address, StoreMatch);

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

    fn map<U, F: FnOnce(T) -> U>(self, op: F) -> CompositeStoreHit<U> {
        match self {
            CompositeStoreHit::Local(v) => CompositeStoreHit::Local(op(v)),
            CompositeStoreHit::Durable(v) => CompositeStoreHit::Durable(op(v)),
            CompositeStoreHit::Replica(v) => CompositeStoreHit::Replica(op(v)),
            CompositeStoreHit::Mixed(v) => CompositeStoreHit::Mixed(op(v)),
            CompositeStoreHit::Miss(v) => CompositeStoreHit::Miss(op(v)),
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
    /// If a `StoreMatch::MatchFull` is made and we didn't have that result to hand in our local store,
    /// should we cache that result?
    should_cache_query_results: bool,
    /// If true, the local store only caches fragment metadata (no payloads).
    /// Payloads are only stored in the durable store and replicas.
    /// Local `get()` calls that hit metadata-only entries fall through to durable/replicas for payloads.
    local_metadata_only: bool,
}

impl CompositeStoreBuilder {
    pub fn with_cache_query_results(mut self, cache_query_results: bool) -> Self {
        self.should_cache_query_results = cache_query_results;
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

        let provider = CompositeStoreInstrumentProvider;
        Ok(CompositeStore {
            local: Arc::new(local),
            read_replicas: self.read_replicas.into(),
            write_replicas: self.write_replicas.into(),
            durable,
            local_durable,
            should_cache_query_results: self.should_cache_query_results,
            local_metadata_only: self.local_metadata_only,
            peers_refreshed_guard: Semaphore::new(1),
            peer_replica_builder: self.peer_replica_builder,
            topology_subscription: None.into(),
            instruments: CompositeStoreInstruments {
                counter_get: provider.counter("get"),
                counter_put: provider.counter("put"),
                counter_query: provider.counter("query"),
                counter_exist: provider.counter("exist"),
                counter_exist_batch: provider.counter("exist_batch"),
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
    /// Flag if local store is durable
    local_durable: bool,
    /// Should Query results be cached in the local store?
    should_cache_query_results: bool,
    /// If true, local store only caches metadata (no payloads)
    local_metadata_only: bool,

    peer_replica_builder: Option<Arc<dyn ReplicaFactory>>,
    peers_refreshed_guard: Semaphore,
    topology_subscription: RwLock<Option<TopologyRefreshSubscription>>,

    instruments: CompositeStoreInstruments,

    inflight_gets: InflightOutput<InfightGetsKey, Result<(Fragment, Bytes), StoreError>>,
}

pub struct ReevaluatePeersSummary {
    pub detected_new_peers: HashSet<PeerInfo>,
    pub num_new_peers_errors: usize,
    pub lost_peers: HashSet<PeerInfo>,
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

    async fn get_from_remotes(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        let mut queries = JoinSet::new();

        if !self.local_durable {
            let durable_store = self.durable.store();
            lore_spawn!(queries, async move {
                let durable_result = durable_store
                    .get(repository, address, match_required)
                    .await
                    .map(CompositeStoreHit::Durable);
                (true, durable_result)
            });
        }
        {
            let read_replicas = self.read_replicas.read().await;
            for replica in read_replicas.iter() {
                let replica_store = replica.store();
                lore_spawn!(queries, async move {
                    let replica_result = replica_store
                        .get(repository, address, match_required)
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
                    if !self.local_durable {
                        // Cache the found result locally
                        let local_store = self.local.store();
                        let (mut fragment, payload) = result.inner().clone();
                        let cache_payload = if self.local_metadata_only {
                            None
                        } else {
                            Some(payload)
                        };
                        let cache_counter = self.instruments.counter_local_caching.clone();
                        lore_spawn!(async move {
                            fragment.flags |= FragmentFlags::PayloadStoredLocal
                                | FragmentFlags::PayloadStoredDurable;
                            let put_result = local_store
                                .put(repository, address, fragment, cache_payload, false)
                                .await;
                            count_result("put_after_get", &cache_counter, &put_result);
                            put_result
                        });
                    }
                    queries.detach_all();
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
                        queries.detach_all();
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

    async fn exist(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        let local_match_made = self
            .local
            .store()
            .exist(repository, address, match_requested)
            .await
            .map(CompositeStoreHit::Local)?;

        if local_match_made.inner() >= &match_requested {
            return local_match_made.into_counted_result(&self.instruments.counter_exist);
        }

        let mut queries = JoinSet::new();

        if !self.local_durable {
            let durable_store = self.durable.store();
            lore_spawn!(queries, async move {
                let durable_result = durable_store
                    .exist(repository, address, match_requested)
                    .await
                    .map(CompositeStoreHit::Durable);
                (true, durable_result)
            });
        }
        {
            let read_replicas = self.read_replicas.read().await;
            for replica in read_replicas.iter() {
                let replica_store = replica.store();
                lore_spawn!(queries, async move {
                    let replica_result = replica_store
                        .exist(repository, address, match_requested)
                        .await
                        .map(CompositeStoreHit::Replica);
                    (false, replica_result)
                });
            }
        }

        let mut best_match = CompositeStoreHit::Miss(*local_match_made.inner());

        while let Some(join_result) = queries.join_next().await {
            let Ok((is_durable, query_result)) = join_result else {
                continue;
            };
            if let Ok(match_made) = query_result {
                if match_made.inner() >= &match_requested {
                    queries.detach_all();
                    return match_made.into_counted_result(&self.instruments.counter_exist);
                }
                if match_made.inner() > best_match.inner() {
                    best_match = match_made;
                }

                if is_durable {
                    // durable is the source of truth - other replicas aren't going to do any better
                    queries.detach_all();
                    break;
                }
            }
        }

        best_match.into_counted_result(&self.instruments.counter_exist)
    }

    async fn exist_batch(
        self: Arc<Self>,
        repository: Partition,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let mut result = self
            .local
            .target
            .clone()
            .exist_batch(repository, addresses, match_requested)
            .await?;

        // The order of results should match the order in which the addresses were originally
        // sent to the `exist_batch` call. The result of this call is a Vec of (position, address)
        // pairs for any addresses that were not found in the local store, where the position still
        // correlates back to the address's original place in the provided input.
        let remaining = result
            .iter()
            .zip(addresses.iter())
            .enumerate()
            .filter_map(|(pos, (m, address))| {
                if *m < match_requested {
                    Some((pos, *address))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if remaining.is_empty() {
            return CompositeStoreHit::Local(result)
                .into_counted_result(&self.instruments.counter_exist_batch);
        }

        let mut queries = JoinSet::new();

        if !self.local_durable {
            let durable_store = self.durable.target.clone();
            let addresses = remaining
                .iter()
                .map(|(_, address)| *address)
                .collect::<Vec<_>>();

            lore_spawn!(queries, async move {
                let durable_result = durable_store
                    .exist_batch(repository, addresses.as_slice(), match_requested)
                    .await
                    .map(CompositeStoreHit::Durable);
                (true, durable_result)
            });
        }

        {
            let read_replicas = self.read_replicas.read().await;
            for replica in read_replicas.iter() {
                let replica_store = replica.target.clone();
                let addresses = remaining
                    .iter()
                    .map(|(_, address)| *address)
                    .collect::<Vec<_>>();

                lore_spawn!(queries, async move {
                    let replica_result = replica_store
                        .exist_batch(repository, addresses.as_slice(), match_requested)
                        .await
                        .map(CompositeStoreHit::Replica);
                    (false, replica_result)
                });
            }
        }

        let mut failure = None;
        while let Some(join_result) = queries.join_next().await {
            let (is_durable, remote_result);
            match join_result {
                Ok(result) => {
                    is_durable = result.0;
                    remote_result = result.1;
                }
                Err(error) => {
                    failure = failure.or(Some(StoreError::internal_with_context(
                        error,
                        "Task failure",
                    )));
                    continue;
                }
            }

            // The result of these calls will be a list of matches, the order of which correlates to
            // the positions in the `remaining` subset (i.e. the addresses which were not found
            // locally). In order to correlate these results back to their original positions in the
            // input we need to map each item back to its position in the `remaining` list. The
            // value at that position is a (pos, address) tuple, where `pos` is the position in the
            // original input. This feels terrible.
            match remote_result {
                Ok(matches) => {
                    for (pos, match_made) in matches.inner().iter().enumerate() {
                        let original_pos = remaining[pos].0;

                        if *match_made > result[original_pos] {
                            result[original_pos] = *match_made;
                        }
                    }
                    // Durable is the source of truth - whatever its results say is the complete
                    // result.
                    // Or, after having gathered all the new replica results, is the result set
                    // complete? If so we can early out
                    if is_durable || result.iter().all(|m| *m >= match_requested) {
                        failure = None;
                        queries.detach_all();
                        break;
                    }
                }
                Err(StoreError::SlowDown(_)) => {
                    // Prioritize slowdown failures
                    failure = Some(StoreError::from(SlowDown));
                }
                Err(err) => {
                    let is_internal_error = err.is_internal();
                    failure = failure.or(Some(err));
                    // durable is the source of truth - if it error'd bubble its error up and forget
                    // about the replicas
                    if is_durable && !is_internal_error {
                        queries.detach_all();
                        break;
                    }
                }
            }
        }

        if let Some(failure) = failure {
            return Err(failure);
        }

        CompositeStoreHit::Mixed(result).into_counted_result(&self.instruments.counter_exist_batch)
    }

    async fn query(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        if let Ok(result) = self
            .local
            .store()
            .query(repository, address, match_requested)
            .await
            .map(CompositeStoreHit::Local)
            && result.inner().match_made >= match_requested
        {
            return result.into_counted_result(&self.instruments.counter_query);
        }

        let mut queries = JoinSet::new();

        if !self.local_durable {
            let durable_store = self.durable.target.clone();
            lore_spawn!(queries, async move {
                let durable_result = durable_store
                    .query(repository, address, match_requested)
                    .await
                    .map(CompositeStoreHit::Durable);
                (true, durable_result)
            });
        }
        {
            let read_replicas = self.read_replicas.read().await;
            for replica in read_replicas.iter() {
                let replica_store = replica.target.clone();
                lore_spawn!(queries, async move {
                    let replica_result = replica_store
                        .query(repository, address, match_requested)
                        .await
                        .map(CompositeStoreHit::Replica);
                    (false, replica_result)
                });
            }
        }

        let mut best_result = CompositeStoreHit::Miss(StoreQueryResult::default());

        while let Some(join_result) = queries.join_next().await {
            let Ok((is_durable, query_result)) = join_result else {
                continue;
            };
            match query_result {
                Ok(result) => {
                    let result_match = result.inner().match_made;
                    if result_match > best_result.inner().match_made {
                        best_result = result;
                        if result_match >= match_requested {
                            // If we found a requested match, the rest of the tasks can elapse
                            queries.detach_all();
                            break;
                        }
                    }
                    if is_durable {
                        // durable is the source of truth - replicas will not be able to do better
                        queries.detach_all();
                        break;
                    }
                }
                Err(error) => {
                    // durable is the source of truth - if it error'd bubble its error up and forget
                    // about the replicas
                    if is_durable && !error.is_slow_down() && !error.is_internal() {
                        queries.detach_all();
                        break;
                    }
                }
            }
        }

        if self.should_cache_query_results
            && best_result.inner().match_made == StoreMatch::MatchFull
            && !self.local_durable
        {
            let local_store = self.local.store();
            let fragment = best_result.inner().fragment;
            lore_spawn!(async move {
                // Cache the address existence only (without payload)
                local_store
                    .put(
                        repository, address, fragment, None,  /* payload */
                        false, /* force */
                    )
                    .await
            });
        }

        best_result.into_counted_result(&self.instruments.counter_query)
    }

    async fn get(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        if let Ok(result) = self
            .local
            .store()
            .get(repository, address, match_required)
            .await
            .map(CompositeStoreHit::Local)
        {
            return result.into_counted_result(&self.instruments.counter_get);
        }

        match self
            .inflight_gets
            .request((repository, address, match_required))
        {
            RequestRole::RequestMaker(guard) => {
                let result = self
                    .clone()
                    .get_from_remotes(repository, address, match_required)
                    .await;
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

    async fn put(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        // Check if address already exists, if so it does not need to be written anywhere as it has
        // already been stored durably at least once
        if !force
            && let Ok(match_made) = self
                .local
                .store()
                .exist(repository, address, StoreMatch::MatchFull)
                .await
                .map(CompositeStoreHit::Local)
            && match_made.inner() == &StoreMatch::MatchFull
        {
            return match_made
                .map(|_| ())
                .into_counted_result(&self.instruments.counter_put);
        }

        let mut fragment = fragment;
        fragment.flags |= if self.local_durable {
            FragmentFlags::PayloadStoredLocal | FragmentFlags::PayloadStoredDurable
        } else {
            FragmentFlags::PayloadStoredDurable
        };
        let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

        // Store durably
        self.durable
            .store()
            .put(repository, address, fragment, payload.clone(), force)
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
                    .put(repository, address, fragment, local_payload, force)
                    .await;
                count_result("put", &cache_counter, &put_result);
                put_result
            });
        }

        // Detached send to all write replicas
        if !behaviour.do_not_replicate {
            let write_replicas = self.write_replicas.read().await;
            for replica in write_replicas.iter() {
                let replica_store = replica.store();
                let payload = payload.clone();
                lore_spawn!(async move {
                    replica_store
                        .put(repository, address, fragment, payload, force)
                        .await
                });
            }
        }

        CompositeStoreHit::Miss(()).into_counted_result(&self.instruments.counter_put)
    }

    async fn obliterate(
        self: Arc<Self>,
        repository: Partition,
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
                match local.obliterate(repository, address, stats.clone()).await {
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
            .obliterate(repository, address, stats)
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

    async fn compact_stop(self: Arc<Self>) {
        self.local.store().compact_stop().await;
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
        source_repository: Partition,
        source_address: Address,
        destination_repository: Partition,
        destination_context: Context,
        durable: bool,
    ) -> Result<(), StoreError> {
        self.durable
            .store()
            .copy(
                source_repository,
                source_address,
                destination_repository,
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
                        source_repository,
                        source_address,
                        destination_repository,
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
    counter_exist: Counter<u64>,
    counter_exist_batch: Counter<u64>,
    counter_query: Counter<u64>,
    gauge_num_replicas: Gauge<u64>,
    topology_refresh_num_changes: Counter<u64>,
    topology_refresh_num_peer_errors: Counter<u64>,
    topology_refresh_duration: Histogram<f64>,
    counter_get_inflight_receiver: Counter<u64>,
    counter_local_caching: Counter<u64>,
}
