// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::convert::Infallible;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;

pub mod replica_factory;
mod topology_refresh;

use async_trait::async_trait;
use bytes::Bytes;
use futures::FutureExt;
use lore_base::lore_spawn;
use lore_base::runtime::DetachingJoinSet;
use lore_base::types::HealResult;
use lore_error_set::prelude::*;
use lore_storage::FlushOptions;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::claim_for_task;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME;
use lore_telemetry::METRICS_SUCCESS_ATTRIBUTE_NAME;
use lore_telemetry::observe::ObserveResult;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use pin_project::pin_project;
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::task::Id;
use tokio::task::JoinError;
use tokio::time::Sleep;

use super::StoreObliterateStats;
use crate::cluster::peer::PeerInfo;
use crate::cluster::topology::Topology;
use crate::errors::AddressNotFound;
use crate::errors::SlowDown;
use crate::errors::is_refusal;
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
use crate::util::inflight::InflightGuard;
use crate::util::inflight::InflightOutput;
use crate::util::inflight::RequestRole;

pub const METRICS_REPLICA_TYPE_LABEL: &str = "replica_type";

/// Which of the two replica lists a target belongs to.
///
/// A peer serving both roles is held as two targets with a connection each, so anything recorded
/// per connection carries this to keep the two apart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReplicaType {
    Read,
    Write,
}

impl ReplicaType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReplicaType::Read => "read",
            ReplicaType::Write => "write",
        }
    }
}

/// Which read replicas a read that misses locally is sent to.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReadReplicaSelection {
    /// Every read replica.
    #[default]
    FanOut,
    /// Read replicas one after another, up to the max tries, from the next position in rotation
    /// over the current read replica list, each until it fails to settle the read or passes the
    /// per-try limit.
    RoundRobin,
}

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

/// Default for how many read replicas a round robin read tries.
pub const DEFAULT_ROUND_ROBIN_MAX_TRIES: NonZeroUsize = NonZeroUsize::new(2).unwrap();

/// Default for how long a round robin read waits on one read replica before trying the next.
pub const DEFAULT_ROUND_ROBIN_TRY_LIMIT: Duration = Duration::from_millis(50);

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
    /// Whether a copy is recorded beyond the durable store. `None` enables it — see
    /// [`Self::with_record_copy_out_of_band`].
    record_copy_out_of_band: Option<bool>,
    read_replica_selection: ReadReplicaSelection,
    round_robin_max_tries: Option<NonZeroUsize>,
    round_robin_try_limit: Option<Duration>,
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

    /// Record a copy the durable store accepted in the local store and at the write replicas, as
    /// detached work. `None` enables it.
    ///
    /// Disabling leaves the durable store as the only holder of the destination association, so
    /// every later lookup of it costs a round trip there. It is a release valve for a deployment
    /// where the extra read and write of the content per copy is not worth that, not a correctness
    /// switch.
    pub fn with_record_copy_out_of_band(mut self, enabled: Option<bool>) -> Self {
        self.record_copy_out_of_band = enabled;
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

    /// Which read replicas a read that misses locally is sent to. Defaults to
    /// [`ReadReplicaSelection::FanOut`].
    pub fn with_read_replica_selection(mut self, selection: ReadReplicaSelection) -> Self {
        self.read_replica_selection = selection;
        self
    }

    /// How many read replicas a round robin read tries, one after another, and how long it waits on
    /// each before trying the next. `None` uses [`DEFAULT_ROUND_ROBIN_MAX_TRIES`] and
    /// [`DEFAULT_ROUND_ROBIN_TRY_LIMIT`].
    pub fn with_round_robin_tries(
        mut self,
        max_tries: Option<NonZeroUsize>,
        try_limit: Option<Duration>,
    ) -> Self {
        self.round_robin_max_tries = max_tries;
        self.round_robin_try_limit = try_limit;
        self
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
            read_replicas: RwLock::new(self.read_replicas.into()),
            write_replicas: self.write_replicas.into(),
            durable,
            durable_delay: self.durable_delay,
            local_durable,
            cache_metadata: self.cache_metadata,
            cache_metadata_semaphore,
            local_metadata_only: self.local_metadata_only,
            record_copy_out_of_band: self.record_copy_out_of_band.unwrap_or(true),
            read_replica_selection: self.read_replica_selection,
            round_robin_max_tries: self
                .round_robin_max_tries
                .unwrap_or(DEFAULT_ROUND_ROBIN_MAX_TRIES),
            round_robin_try_limit: self
                .round_robin_try_limit
                .unwrap_or(DEFAULT_ROUND_ROBIN_TRY_LIMIT),
            // A `u32` start keeps the count far from the `usize` wrap, which breaks the even
            // spread.
            read_replica_rotation: AtomicUsize::new(rand::random::<u32>() as usize),
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
                counter_get_metadata_inflight_receiver: provider
                    .counter("get_metadata.inflight.receiver"),
                counter_local_caching: provider.counter("local.caching_total"),
                caching: Arc::new(CachingGauges::new(&provider)),

                provider,
            },
            inflight_gets: Default::default(),
            inflight_get_metadatas: Default::default(),
        })
    }
}

#[error_set]
pub enum PeersRefreshedError {}

struct CompositeCopyBehavior {
    copy_behavior: CopyBehavior,
    /// The destination's content where a put drove the copy, with the fragment that put supplied
    /// to describe it. The pair is kept together because the representation the durable store
    /// copied need not be the one these bytes are in.
    supplied: Option<(Fragment, Bytes)>,
}

/// A write to the local store of a result the durable store or a replica served.
struct LocalFill {
    store: Arc<dyn ImmutableStore>,
    /// The command's claim on the local store, joined in the reading task.
    joined: Option<lore_storage::local::store_lock::StoreGuard>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
    counter: Counter<u64>,
    /// Counts the write, and the payload it holds, until it ends.
    caching: CachingWrite,
}

/// The guard of a read in flight, keyed by its partition and address.
type InflightGetGuard = InflightGuard<InflightGetsKey, Result<CompositeGetOutput, StoreError>>;

impl LocalFill {
    /// Writes the result to the local store, holding `hold` until the write ends.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn run(self, hold: InflightGetGuard) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            // Named, as the block takes only the fields it names: the count ends with the write.
            let caching = self.caching;
            let _claim = claim_for_task(&self.store, self.joined).await;
            let put_result = self
                .store
                .put(
                    self.partition,
                    self.address,
                    self.fragment,
                    self.payload,
                    false,
                )
                .await;
            count_result("put_after_get", &self.counter, &put_result);
            drop(caching);
            drop(hold);
            put_result
        }
    }
}

#[derive(Clone, Debug)]
struct CompositeGetOutput {
    get_data: StoreGetData,
    /// The local store answered the read, or a write of the result to it has been spawned.
    local_cache_will_be_warm: bool,
}

/// `output` without the content a check on receipt expanded, as a read that arrives while `output`
/// is written to the local store gets it: such a read expands the payload itself where it needs the
/// content.
fn without_content(output: &CompositeGetOutput) -> CompositeGetOutput {
    let data = &output.get_data;
    CompositeGetOutput {
        get_data: StoreGetData {
            fragment: data.fragment,
            match_made: data.match_made,
            partition: data.partition,
            payload: data.payload.clone(),
            content: None,
        },
        local_cache_will_be_warm: output.local_cache_will_be_warm,
    }
}

/// A store that is able to propagate read and write operations to local, durable and replica stores.
///
/// Read immutable operations try local store first, if no required match is made it then goes wide
/// and reads from read durable store and the read replicas [`ReadReplicaSelection`] chooses. First
/// successful result will be early returned. A result from either is cached in the local store,
/// whichever answered.
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
    /// Look aside read replicas, swapped whole on change so a read can keep the list it started
    /// with after it releases the lock.
    read_replicas: RwLock<Arc<[ReplicationTarget]>>,
    /// Look aside write replicas
    write_replicas: RwLock<Vec<ReplicationTarget>>,
    /// Durable read store
    durable: DurableTarget,
    /// How long durable store reads wait for the read replicas to answer first, counted from the
    /// start of the read. The wait ends early once every replica query the read sent has ended
    /// without settling it and no round robin try is left to start. It is skipped when there are
    /// no read replicas.
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
    /// Whether a copy the durable store accepted is also recorded locally and at the write
    /// replicas. Off leaves the durable store the only holder of the destination association.
    record_copy_out_of_band: bool,
    read_replica_selection: ReadReplicaSelection,
    /// How many read replicas a round robin read tries, at most.
    round_robin_max_tries: NonZeroUsize,
    /// How long a round robin read waits on one try before starting the next.
    round_robin_try_limit: Duration,
    /// Counts round robin reads. A read takes the entry at this count modulo the length of the
    /// read replica list it holds, so a refresh that shortens the list cannot put it out of range.
    /// It starts at a random count, so the first read replica chosen does not depend on list order.
    read_replica_rotation: AtomicUsize,

    peer_replica_builder: Option<Arc<dyn ReplicaFactory>>,
    peers_refreshed_guard: Semaphore,
    topology_subscription: RwLock<Option<TopologyRefreshSubscription>>,

    instruments: CompositeStoreInstruments,

    inflight_gets: InflightOutput<InflightGetsKey, Result<CompositeGetOutput, StoreError>>,
    inflight_get_metadatas: InflightOutput<InflightGetsKey, Result<StoreGetData, StoreError>>,
}

pub struct ReevaluatePeersSummary {
    pub detected_new_peers: HashSet<PeerInfo>,
    pub num_new_peers_errors: usize,
    pub lost_peers: HashSet<PeerInfo>,
}

/// Container representing a typical composite store operation, where requests are sent to the
/// durable store and to read replicas.
///
/// Fan-out replicas are spawned on the unpinned operation and results drained once it is pinned,
/// so every one is spawned before the first call to [`Self::next`]. Round robin tries start one at
/// a time, as [`Tries`] describes.
#[lore_macro::test_pub]
#[pin_project]
struct CompositeOperation<H, D, F>
where
    H: 'static,
{
    queries: DetachingJoinSet<(bool, Result<H, StoreError>)>,
    /// The durable query. It remains unspawned until `deadline` passes or no remaining replica
    /// query could settle the operation.
    durable: Option<D>,
    /// Started when the operation is built. `None` when there is no durable query.
    #[pin]
    deadline: Option<Sleep>,
    /// Round robin tries, where a second try can start. Boxed so that a read without them does not
    /// carry their state and timer in its future.
    tries: Option<Pin<Box<Tries<F>>>>,
}

/// The tries of a round robin read, over the read replica list as it was when the read started.
/// The first starts when the operation is built, and each next one when the current try's result
/// has been returned and the next result is asked for, or when the current try passes its limit.
/// A try past its limit keeps running.
#[lore_macro::test_pub]
#[pin_project]
struct Tries<F> {
    read_replicas: Arc<[ReplicationTarget]>,
    /// Builds the query sent to a read replica.
    query: F,
    /// The list position of the next try, before wrapping on the list length.
    position: usize,
    /// Tries not started yet.
    left: usize,
    /// How long a try runs before the next one starts.
    limit: Duration,
    /// The try started last. Its result moves the read on to the next try.
    current: Option<Id>,
    /// The last result returned came from the current try.
    move_on: bool,
    /// When the current try is passed over for the next. `None` when no try is left to start.
    #[pin]
    deadline: Option<Sleep>,
}

impl<F> Tries<F> {
    /// Starts the next try, if one is left, and sets when it is passed over.
    fn start_next<H, R>(
        self: Pin<&mut Self>,
        queries: &mut DetachingJoinSet<(bool, Result<H, StoreError>)>,
    ) where
        H: Send + 'static,
        F: Fn(Arc<dyn ImmutableStore>) -> R,
        R: Future<Output = Result<H, StoreError>> + Send + 'static,
    {
        let mut this = self.project();
        let position = *this.position % this.read_replicas.len().max(1);
        let Some(replica) = this.read_replicas.get(position).filter(|_| *this.left > 0) else {
            this.deadline.set(None);
            return;
        };
        let query = (this.query)(replica.store());
        *this.current = Some(lore_spawn!(queries, query.map(|result| (false, result))).id());
        *this.position += 1;
        *this.left -= 1;
        let deadline = (*this.left > 0).then(|| tokio::time::sleep(*this.limit));
        this.deadline.set(deadline);
    }

    fn poll_deadline(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        match self.project().deadline.as_pin_mut() {
            Some(deadline) => deadline.poll(cx),
            None => Poll::Pending,
        }
    }
}

/// What a [`CompositeOperation`] waits for.
enum OperationEvent<T> {
    Joined(Option<Result<(Id, T), JoinError>>),
    TryLimit,
    DurableDelay,
}

impl<H, D, F> CompositeOperation<H, D, F>
where
    H: Send + 'static,
    D: Future<Output = Result<H, StoreError>> + Send + 'static,
{
    /// `durable` is the durable query and how long it waits for the replicas to answer first.
    fn new(durable: Option<(Duration, D)>) -> Self {
        let (deadline, durable) = durable
            .map(|(delay, query)| (tokio::time::sleep(delay), query))
            .unzip();
        Self {
            queries: DetachingJoinSet::default(),
            durable,
            deadline,
            tries: None,
        }
    }

    /// An operation that tries up to `count` read replicas of `read_replicas` one after another,
    /// from list position `start`, each for at most `limit` before the next starts, and starts
    /// the first. `count` must not exceed the list length.
    ///
    /// The try state is boxed only for two or more tries; one try is a single replica query, as
    /// the last try has no limit. The box is on a cold path: a round robin operation is built only
    /// when a read misses locally.
    fn with_tries<R>(
        durable: Option<(Duration, D)>,
        read_replicas: &Arc<[ReplicationTarget]>,
        start: usize,
        count: usize,
        limit: Duration,
        query: F,
    ) -> Self
    where
        F: Fn(Arc<dyn ImmutableStore>) -> R,
        R: Future<Output = Result<H, StoreError>> + Send + 'static,
    {
        debug_assert!(count <= read_replicas.len());
        let mut operation = Self::new(durable);
        if count < 2 {
            if count == 1
                && let Some(replica) = read_replicas.get(start)
            {
                operation.spawn_replica(query(replica.store()));
            }
            return operation;
        }
        let mut tries = Box::pin(Tries {
            read_replicas: Arc::clone(read_replicas),
            query,
            position: start,
            left: count,
            limit,
            current: None,
            move_on: false,
            deadline: None,
        });
        tries.as_mut().start_next(&mut operation.queries);
        operation.tries = Some(tries);
        operation
    }

    fn spawn_replica(
        &mut self,
        query: impl Future<Output = Result<H, StoreError>> + Send + 'static,
    ) {
        let queries = &mut self.queries;
        lore_spawn!(queries, query.map(|result| (false, result)));
    }

    /// Waits for the next query to finish, yielding whether it came from the durable store
    /// alongside its result.
    ///
    /// Asking for the next result after one from the current try starts the next try. Asking for
    /// it once every replica result has been returned and no try is left spawns the durable
    /// query. Do not call this again after a result that settles the operation.
    async fn next<R>(
        self: Pin<&mut Self>,
    ) -> Option<Result<(bool, Result<H, StoreError>), JoinError>>
    where
        F: Fn(Arc<dyn ImmutableStore>) -> R,
        R: Future<Output = Result<H, StoreError>> + Send + 'static,
    {
        let mut this = self.project();
        if let Some(tries) = this.tries.as_mut()
            && std::mem::take(tries.as_mut().project().move_on)
        {
            tries.as_mut().start_next(this.queries);
        }
        loop {
            let has_durable = this.durable.is_some();
            // An already available result may settle the operation, so it takes precedence over
            // a deadline that expires at the same time.
            let event = std::future::poll_fn(|cx| {
                if let Poll::Ready(joined) = this.queries.poll_join_next_with_id(cx) {
                    return Poll::Ready(OperationEvent::Joined(joined));
                }
                if let Some(tries) = this.tries.as_mut()
                    && tries.as_mut().poll_deadline(cx).is_ready()
                {
                    return Poll::Ready(OperationEvent::TryLimit);
                }
                if has_durable
                    && let Some(deadline) = this.deadline.as_mut().as_pin_mut()
                    && deadline.poll(cx).is_ready()
                {
                    return Poll::Ready(OperationEvent::DurableDelay);
                }
                Poll::Pending
            })
            .await;
            match event {
                OperationEvent::Joined(Some(joined)) => {
                    if let Some(tries) = this.tries.as_mut() {
                        let id = joined.as_ref().map_or_else(JoinError::id, |(id, _)| *id);
                        let tries = tries.as_mut().project();
                        *tries.move_on = *tries.current == Some(id);
                    }
                    return Some(joined.map(|(_, output)| output));
                }
                OperationEvent::Joined(None) => {
                    // Every spawned query has been returned, and no try is left.
                    let query = this.durable.take()?;
                    let queries = &mut *this.queries;
                    lore_spawn!(queries, query.map(|result| (true, result)));
                }
                OperationEvent::TryLimit => {
                    if let Some(tries) = this.tries.as_mut() {
                        tries.as_mut().start_next(this.queries);
                    }
                }
                OperationEvent::DurableDelay => {
                    if let Some(query) = this.durable.take() {
                        let queries = &mut *this.queries;
                        lore_spawn!(queries, query.map(|result| (true, result)));
                    }
                }
            }
        }
    }
}

impl CompositeStore {
    pub fn local(&self) -> Arc<dyn ImmutableStore> {
        self.local.target.clone()
    }

    pub fn durable(&self) -> Arc<dyn ImmutableStore> {
        self.durable.target.clone()
    }

    /// Which read replicas a read that misses locally is sent to.
    pub fn read_replica_selection(&self) -> ReadReplicaSelection {
        self.read_replica_selection
    }

    /// How many read replicas a round robin read tries, at most.
    pub fn round_robin_max_tries(&self) -> NonZeroUsize {
        self.round_robin_max_tries
    }

    /// How long a round robin read waits on one try before starting the next.
    pub fn round_robin_try_limit(&self) -> Duration {
        self.round_robin_try_limit
    }

    /// The payload writes to the local tier in flight and the bytes they hold, as the gauges
    /// last recorded them.
    #[lore_macro::test_pub]
    #[allow(dead_code)]
    fn caching(&self) -> (u64, u64) {
        let gauges = &self.instruments.caching;
        (
            gauges.writes.load(Ordering::Relaxed),
            gauges.bytes.load(Ordering::Relaxed),
        )
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
                    let mut new_read: Vec<ReplicationTarget> = read
                        .iter()
                        .filter(|replica| retain_peer(replica))
                        .cloned()
                        .collect();

                    // add new targets
                    for result in new_target_results {
                        match result {
                            Ok(targets) => {
                                if let Some(write_target) = targets.write {
                                    write.push(write_target);
                                }
                                if let Some(read_target) = targets.read {
                                    new_read.push(read_target);
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
                    *read = new_read.into();
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
            &[KeyValue::new(
                METRICS_REPLICA_TYPE_LABEL,
                ReplicaType::Write.as_str(),
            )],
        );
        self.instruments.gauge_num_replicas.record(
            self.read_replicas.read().await.len() as u64,
            &[KeyValue::new(
                METRICS_REPLICA_TYPE_LABEL,
                ReplicaType::Read.as_str(),
            )],
        );

        refresh_result
    }

    pub async fn clone_read_replicas(&self) -> Vec<ReplicationTarget> {
        self.read_replicas.read().await.to_vec()
    }

    pub async fn clone_write_replicas(&self) -> Vec<ReplicationTarget> {
        self.write_replicas.read().await.clone()
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

    /// Builds the remote read of one read that missed locally. `durable_query` builds the durable
    /// query from the durable store, and `replica_query` builds the query sent to a read replica:
    /// to each one in fan-out mode, or to each round robin try.
    ///
    /// The read takes the read replica list as it is when it starts. Each round robin read takes
    /// one turn of the rotation, whatever the number of tries it makes.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn remote_read<H, D, F, R>(
        &self,
        durable_query: impl FnOnce(Arc<dyn ImmutableStore>) -> D,
        replica_query: F,
    ) -> impl Future<Output = CompositeOperation<H, D, F>>
    where
        H: Send + 'static,
        D: Future<Output = Result<H, StoreError>> + Send + 'static,
        F: Fn(Arc<dyn ImmutableStore>) -> R,
        R: Future<Output = Result<H, StoreError>> + Send + 'static,
    {
        let durable = (!self.local_durable)
            .then(|| (self.durable_delay, durable_query(self.durable.store())));
        async move {
            let read_replicas = self.read_replicas.read().await;
            match self.read_replica_selection {
                ReadReplicaSelection::FanOut => {
                    let mut operation = CompositeOperation::new(durable);
                    for replica in read_replicas.iter() {
                        operation.spawn_replica(replica_query(replica.store()));
                    }
                    operation
                }
                ReadReplicaSelection::RoundRobin => {
                    let start = self.read_replica_rotation.fetch_add(1, Ordering::Relaxed)
                        % read_replicas.len().max(1);
                    let count = self.round_robin_max_tries.get().min(read_replicas.len());
                    CompositeOperation::with_tries(
                        durable,
                        &read_replicas,
                        start,
                        count,
                        self.round_robin_try_limit,
                        replica_query,
                    )
                }
            }
        }
    }

    /// The result of a read the local store missed, from the durable store or a replica, and the
    /// write of that result to the local store, for the caller to start. `durable_confirmed` is
    /// set by a caller that already knows the durable store holds this address, so the local copy
    /// is recorded durable whichever store answers.
    async fn get_from_remotes(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        durable_confirmed: bool,
    ) -> Result<(CompositeGetOutput, Option<LocalFill>), StoreError> {
        let mut remote = pin!(
            self.remote_read(
                |store| async move {
                    store
                        .get(partition, address)
                        .await
                        .map(CompositeStoreHit::Durable)
                },
                |store| async move {
                    store
                        .get(partition, address)
                        .await
                        .map(CompositeStoreHit::Replica)
                },
            )
            .await
        );

        let mut error_to_return = StoreError::from(AddressNotFound::from(address));
        while let Some(join_result) = remote.as_mut().next().await {
            let Ok((is_durable, query_result)) = join_result else {
                continue;
            };
            match query_result {
                Ok(result) => {
                    let fill = (!self.local_durable).then(|| {
                        let store = self.local.store();
                        let fragment = result.inner().fragment;
                        let mut flags = fragment.flags | FragmentFlags::PayloadStoredLocal;
                        // A replica is not this composite's durable store, so its answer keeps the
                        // durable bit the replica reported. Marking it durable here would let
                        // eviction drop a copy that the durable store may never have held.
                        if durable_confirmed || matches!(result, CompositeStoreHit::Durable(_)) {
                            flags |= FragmentFlags::PayloadStoredDurable;
                        }
                        let payload = if self.local_metadata_only {
                            None
                        } else {
                            result.inner().payload.clone()
                        };
                        LocalFill {
                            joined: store.try_join_claim(),
                            store,
                            partition,
                            address,
                            fragment: Fragment { flags, ..fragment },
                            caching: self
                                .instruments
                                .caching
                                .start(payload.as_ref().map_or(0, Bytes::len)),
                            payload,
                            counter: self.instruments.counter_local_caching.clone(),
                        }
                    });
                    let Ok(get_data) =
                        result.into_counted_result::<Infallible>(&self.instruments.counter_get);
                    return Ok((
                        CompositeGetOutput {
                            get_data,
                            local_cache_will_be_warm: fill.is_some(),
                        },
                        fill,
                    ));
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

    async fn get_metadata_from_remotes(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let mut remote = pin!(
            self.remote_read(
                |store| async move {
                    store
                        .get_metadata(partition, address)
                        .await
                        .map(CompositeStoreHit::Durable)
                },
                |store| async move {
                    store
                        .get_metadata(partition, address)
                        .await
                        .map(CompositeStoreHit::Replica)
                },
            )
            .await
        );

        let mut best_result = CompositeStoreHit::Miss(StoreGetData::default());

        while let Some(join_result) = remote.as_mut().next().await {
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
                Err(error) if is_durable && is_refusal(&error) => return Err(error),
                Err(error) => {
                    if is_durable && !error.is_slow_down() && !error.is_internal() {
                        break;
                    }
                }
            }
        }

        if self.cache_metadata
            && best_result.inner().match_made == StoreMatch::MatchFull
            && !self.local_durable
        {
            let local_store = self.local.store();
            let fragment = best_result.inner().fragment;
            let partition = best_result.inner().partition;
            let cache_counter = self.instruments.counter_local_caching.clone();
            let joined = local_store.try_join_claim();
            lore_spawn!(async move {
                let _claim = claim_for_task(&local_store, joined).await;
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

    async fn copy_implementation(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        composite_behavior: CompositeCopyBehavior,
    ) -> Result<(), StoreError> {
        self.durable
            .store()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                composite_behavior.copy_behavior,
            )
            .await?;

        if !self.local_durable && self.record_copy_out_of_band {
            let store = self.clone();
            let cache_counter = self.instruments.counter_local_caching.clone();
            let destination_address = Address {
                hash: source_address.hash,
                context: destination_context,
            };
            // It is worthwhile caching this new copy so we know of its existence in the
            // other partition, in case we are ever asked for it.
            let joined = self.local.target.try_join_claim();
            lore_spawn!(async move {
                let _claim = claim_for_task(&self.local.target, joined).await;
                let (mut destination_fragment, destination_payload, record_locally) =
                    if let Some((fragment, payload)) = composite_behavior.supplied {
                        // A put already found the local store short of a full match, and the
                        // durable store has just accepted the copy, so a read adds nothing. The
                        // put's own fragment is the one describing its bytes; the durable copy's
                        // representation can differ (for example, compressed there, uncompressed
                        // here).
                        (fragment, Some(payload), true)
                    } else {
                        // Our local store might not have the source content to copy into the
                        // destination, so we may have to go through the durable store; hence
                        // just do `get` on the composite store itself.
                        // If our local store does have it locally, then `get` will resolve faster
                        let get_result = store
                            .clone()
                            .get_implementation(
                                destination_partition,
                                destination_address,
                                composite_behavior.copy_behavior.durable,
                            )
                            .await;
                        count_result("get_after_copy", &cache_counter, &get_result);

                        // the durable store already accepted the copy, so just sanity check
                        // we got it as well
                        let Ok(copied) = get_result else {
                            return;
                        };
                        if matches!(copied.get_data.match_made, StoreMatch::MatchNone) {
                            return;
                        }
                        let record_locally =
                            !matches!(copied.get_data.match_made, StoreMatch::MatchFull)
                                || !copied.local_cache_will_be_warm;
                        (
                            copied.get_data.fragment,
                            copied.get_data.payload,
                            record_locally,
                        )
                    };

                // the durable store did the copy, so this new destination fragment
                // is durably stored
                if composite_behavior.copy_behavior.durable {
                    destination_fragment.flags |= FragmentFlags::PayloadStoredDurable;
                }

                // The durable store confirmed the copy succeeded, so update our local record.
                // Our local record might not even have the source destination+address so `put`
                // the destination instead of attempting to `copy` the source
                let local_payload = if store.local_metadata_only {
                    None // Strip payload — local store only caches metadata
                } else {
                    destination_payload.clone()
                };
                if record_locally {
                    let local_copy_result = self
                        .local
                        .store()
                        .put(
                            destination_partition,
                            destination_address,
                            destination_fragment,
                            local_payload,
                            false, /* do not force */
                        )
                        .await;
                    count_result("local_put_after_copy", &cache_counter, &local_copy_result);
                }

                // fan out to write replicas so they hold the copy under the new partition and
                // can serve it if it is ever asked for
                if !composite_behavior.copy_behavior.do_not_replicate {
                    store.replicate_put(
                        destination_partition,
                        destination_address,
                        destination_fragment,
                        destination_payload,
                        false, /* do not force */
                    );
                }
            });
        }

        Ok(())
    }

    /// A read the local store misses is made once for every caller of its partition and address:
    /// callers arriving while it is in flight wait for its result, and those arriving while its
    /// result is written to the local store take that result [`without_content`]. Such a caller
    /// gets that read's outcome, and the local copy keeps that read's durable bit. See
    /// [`Self::get_from_remotes`] for `durable_confirmed`.
    async fn get_implementation(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        durable_confirmed: bool,
    ) -> Result<CompositeGetOutput, StoreError> {
        if let Ok(result) = self
            .local
            .store()
            .get(partition, address)
            .await
            .map(CompositeStoreHit::Local)
        {
            return Ok(CompositeGetOutput {
                get_data: result
                    .into_counted_result::<StoreError>(&self.instruments.counter_get)?,
                local_cache_will_be_warm: true,
            });
        }

        match self.inflight_gets.request((partition, address)) {
            RequestRole::RequestMaker(guard) => {
                match self
                    .clone()
                    .get_from_remotes(partition, address, durable_confirmed)
                    .await
                {
                    Ok((output, Some(fill))) => {
                        let held = Ok(without_content(&output));
                        let result = Ok(output);
                        lore_spawn!(fill.run(guard.broadcast_and_hold(&result, held)));
                        result
                    }
                    result => {
                        let result = result.map(|(output, _)| output);
                        guard.broadcast(&result);
                        result
                    }
                }
            }
            RequestRole::Ready(result) => {
                self.instruments.counter_get_inflight_receiver.add(1, &[]);
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
}

#[async_trait]
impl ImmutableStore for CompositeStore {
    /// Claims the local store, which is the only one of these with files to claim.
    ///
    /// The replicas and the durable upstream are remote and have no flock of their
    /// own, so the local target's claim is the whole of it. Answering `None` here
    /// would open a handle holding nothing, which then reads and writes a store no
    /// process has claimed.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the local store's claim cannot be taken.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, lore_storage::StoreError> {
        self.local.target.clone().hold_for_command().await
    }

    fn try_join_claim(&self) -> Option<lore_storage::local::store_lock::StoreGuard> {
        self.local.target.try_join_claim()
    }

    /// Heals the local tier's copy, and then the durable store's, which the local tier was filled
    /// from. A read's write of `address` to the local tier ends first, so the local check sees the
    /// copy that write leaves: a payload the durable store served corrupt is dropped, not kept to
    /// be served again. Healed if either store healed its copy.
    async fn heal(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<HealResult, StoreError> {
        self.clone().settle(partition, address).await;
        let local = self
            .local
            .target
            .clone()
            .heal(partition, address)
            .await
            .unwrap_or_else(|err| {
                lore_warn!("Healing the local copy of {address} failed: {err}");
                HealResult::Failed
            });
        let durable = self.durable.target.clone().heal(partition, address).await?;
        Ok(match (local, durable) {
            (HealResult::Healed, _) | (_, HealResult::Healed) => HealResult::Healed,
            (HealResult::Failed, _) | (_, HealResult::Failed) => HealResult::Failed,
            _ => HealResult::NotAttempted,
        })
    }

    /// Waits for a read's write of `address` to the local tier, which runs after the read returns.
    async fn settle(self: Arc<Self>, partition: Partition, address: Address) {
        self.inflight_gets.settled(&(partition, address)).await;
    }

    /// The hold of the durable store, which the composite's misses and writes reach.
    fn hold_correlation(&self, correlation_id: &str) -> Option<lore_storage::CorrelationHold> {
        self.durable.target.hold_correlation(correlation_id)
    }

    fn store_lock(&self) -> Option<Arc<lore_storage::local::store_lock::StoreLock>> {
        self.local.target.store_lock()
    }

    async fn hold_for_task(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, lore_storage::StoreError> {
        self.local.target.clone().hold_for_task().await
    }

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

    /// Local first, then one remote read, to the durable store and the read replicas
    /// [`ReadReplicaSelection`] chooses, for whatever the local store left below a full match. Every
    /// responder is merged rather than raced: a store that establishes a better level than another
    /// keeps it, so a cache in front of a durable store can supply the partition match the durable
    /// store below it declines to establish.
    ///
    /// In both modes every replica query asks about the whole batch the local store left below a
    /// full match, so a later round robin try also asks about addresses an earlier try already
    /// matched fully.
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(addresses.len(), results.len());

        // A local store that cannot answer is not a failed resolve. Papering over one store being
        // unavailable is what a composite is for, so its error leaves every address unresolved and
        // the remote read below answers them.
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

        let mut remote = pin!(
            self.remote_read(
                |store| {
                    let pending = pending.clone();
                    async move {
                        let mut scratch = vec![StoreMatchResult::default(); pending.len()];
                        let outcome = store.query(partition, &pending, &mut scratch).await;
                        outcome.map(|()| CompositeStoreHit::Durable(scratch))
                    }
                },
                |store| {
                    let pending = pending.clone();
                    async move {
                        let mut scratch = vec![StoreMatchResult::default(); pending.len()];
                        let outcome = store.query(partition, &pending, &mut scratch).await;
                        outcome.map(|()| CompositeStoreHit::Replica(scratch))
                    }
                },
            )
            .await
        );

        let mut failure = None;
        while let Some(join_result) = remote.as_mut().next().await {
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

                        if self.cache_metadata
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
                            let joined = self.local.target.try_join_claim();
                            lore_spawn!(async move {
                                let _claim = claim_for_task(&store.local.target, joined).await;
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

    /// Local first, then the read replicas [`ReadReplicaSelection`] chooses, with the durable store
    /// as `durable_delay` describes. Like `query`, read replicas are consulted so that edge-region
    /// replicas can answer without a cross-region round trip to the durable store. Durable is the
    /// source of truth: when it responds the remaining replica queries are detached.
    ///
    /// A representation that had to come from elsewhere is written back to the local store without
    /// its payload, so the next caller finds it in process.
    ///
    /// Callers that miss locally on the same partition and address while a remote read is already
    /// in flight wait on that one rather than starting their own, and every one of them observes the
    /// same outcome, success or failure.
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

        match self.inflight_get_metadatas.request((partition, address)) {
            RequestRole::RequestMaker(guard) => {
                let result = self
                    .clone()
                    .get_metadata_from_remotes(partition, address)
                    .await;
                guard.broadcast(&result);
                result
            }
            RequestRole::Ready(result) => result,
            RequestRole::ResultAwaiter(mut receiver) => {
                self.instruments
                    .counter_get_metadata_inflight_receiver
                    .add(1, &[]);
                receiver.recv().await.unwrap_or_else(|receive_error| {
                    Err(StoreError::internal_with_context(
                        receive_error,
                        "Failed to get_metadata inflight result",
                    ))
                })
            }
        }
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        Ok(self
            .get_implementation(partition, address, false)
            .await?
            .get_data)
    }

    /// A full match is written nowhere. A weaker one the durable store holds is written by
    /// duplicating that association — see [`can_put_use_copy`] — carrying the fragment and payload
    /// so the destination can be recorded without reading the content back. A refused copy is not fallen
    /// back on and fails the put. Recovery is the caller's: it still holds the payload and
    /// retries. The local store and the replicas are sent the destination's content once the
    /// association exists.
    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        let local_resolve = if force {
            StoreMatchResult::default()
        } else {
            query_one(&self.local.store(), partition, address)
                .await
                .unwrap_or_default()
        };
        if local_resolve.match_made == StoreMatch::MatchFull {
            return CompositeStoreHit::Local(()).into_counted_result(&self.instruments.counter_put);
        }

        let mut fragment = fragment;
        fragment.flags |= if self.local_durable {
            FragmentFlags::PayloadStoredLocal | FragmentFlags::PayloadStoredDurable
        } else {
            FragmentFlags::PayloadStoredDurable
        };
        let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

        if let Some(supplied_payload) = &payload
            && let Some((source_partition, source_address)) =
                can_put_use_copy(&local_resolve, address.hash)
        {
            self.clone()
                .copy_implementation(
                    source_partition,
                    source_address,
                    partition,
                    address.context,
                    CompositeCopyBehavior {
                        copy_behavior: CopyBehavior {
                            durable: true,
                            do_not_replicate: behaviour.do_not_replicate,
                        },
                        supplied: Some((fragment, supplied_payload.clone())),
                    },
                )
                .await?;

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
            let caching = self
                .instruments
                .caching
                .start(local_payload.as_ref().map_or(0, Bytes::len));
            let joined = local.try_join_claim();
            lore_spawn!(async move {
                let _claim = claim_for_task(&local, joined).await;
                let put_result = local
                    .put(partition, address, fragment, local_payload, force)
                    .await;
                count_result("put", &cache_counter, &put_result);
                drop(caching);
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
            let joined = local.try_join_claim();

            lore_spawn!(async move {
                let _claim = claim_for_task(&local, joined).await;
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

    async fn load_index(self: Arc<Self>) -> Result<(), StoreError> {
        self.local.store().load_index().await
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

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.local.store().flush(options).await
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
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        self.copy_implementation(
            source_partition,
            source_address,
            destination_partition,
            destination_context,
            CompositeCopyBehavior {
                copy_behavior: behavior,
                supplied: None,
            },
        )
        .await
    }
}

fn count_result<T>(context: &'static str, counter: &Counter<u64>, result: &Result<T, StoreError>) {
    counter.add(
        1,
        &[
            KeyValue::new(METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME, context),
            KeyValue::new(METRICS_SUCCESS_ATTRIBUTE_NAME, result.is_ok()),
            KeyValue::new("outcome", result_outcome(result)),
        ],
    );
}

/// A metric label for a store result: `ok`, or the snake-case name of the error variant.
fn result_outcome<T>(result: &Result<T, StoreError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(StoreError::AddressNotFound(_)) => "address_not_found",
        Err(StoreError::PayloadNotFound(_)) => "payload_not_found",
        Err(StoreError::SlowDown(_)) => "slow_down",
        Err(StoreError::Oversized(_)) => "oversized",
        Err(StoreError::NotFound(_)) => "not_found",
        Err(StoreError::Disconnected(_)) => "disconnected",
        Err(StoreError::NotAuthorized(_)) => "not_authorized",
        Err(StoreError::NotAuthenticated(_)) => "not_authenticated",
        Err(StoreError::Maintenance(_)) => "maintenance",
        Err(StoreError::NoRemote(_)) => "no_remote",
        Err(StoreError::NotSupported(_)) => "not_supported",
        Err(StoreError::Internal(_)) => "internal",
    }
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
    counter_get_metadata_inflight_receiver: Counter<u64>,
    counter_local_caching: Counter<u64>,
    caching: Arc<CachingGauges>,
}

/// The payload writes to the local tier in flight, detached from the reads and puts that started
/// them, and the payload bytes they hold, as the `local.caching_in_progress` and
/// `local.caching_bytes` gauges report them.
#[derive(Debug)]
struct CachingGauges {
    writes: AtomicU64,
    bytes: AtomicU64,
    in_progress: Gauge<u64>,
    held: Gauge<u64>,
}

impl CachingGauges {
    fn new(provider: &CompositeStoreInstrumentProvider) -> Self {
        Self {
            writes: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            in_progress: provider.gauge("local.caching_in_progress"),
            held: provider.gauge("local.caching_bytes"),
        }
    }

    /// Counts a write holding `bytes` of payload until the returned guard drops.
    fn start(self: &Arc<Self>, bytes: usize) -> CachingWrite {
        let bytes = bytes as u64;
        let writes = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
        let held = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.in_progress.record(writes, &[]);
        self.held.record(held, &[]);
        CachingWrite {
            gauges: self.clone(),
            bytes,
        }
    }
}

/// A payload write to the local tier, counted in [`CachingGauges`] while it lives.
#[derive(Debug)]
struct CachingWrite {
    gauges: Arc<CachingGauges>,
    bytes: u64,
}

impl Drop for CachingWrite {
    fn drop(&mut self) {
        let gauges = &self.gauges;
        let writes = gauges.writes.fetch_sub(1, Ordering::Relaxed) - 1;
        let held = gauges.bytes.fetch_sub(self.bytes, Ordering::Relaxed) - self.bytes;
        gauges.in_progress.record(writes, &[]);
        gauges.held.record(held, &[]);
    }
}
