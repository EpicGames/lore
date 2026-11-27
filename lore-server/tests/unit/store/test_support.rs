// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Partition;
use lore_revision::runtime::execution_context;
use lore_storage::FlushOptions;
use lore_storage::Fragment;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;

/// An `ImmutableStore` that returns `SlowDown` on every operation.
pub struct SlowDownImmutableStore;

#[async_trait]
impl ImmutableStore for SlowDownImmutableStore {
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    async fn query(
        self: Arc<Self>,
        _partition: Partition,
        _addresses: &[Address],
        _results: &mut [lore_storage::StoreMatchResult],
    ) -> Result<(), StoreError> {
        Err(StoreError::from(SlowDown))
    }

    async fn get_metadata(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
    ) -> Result<StoreGetData, StoreError> {
        Err(StoreError::from(SlowDown))
    }

    async fn get(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
    ) -> Result<StoreGetData, StoreError> {
        Err(StoreError::from(SlowDown))
    }

    async fn put(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
        _fragment: Fragment,
        _payload: Option<Bytes>,
        _force: bool,
    ) -> Result<(), StoreError> {
        Err(StoreError::from(SlowDown))
    }

    async fn obliterate(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
        _stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        Err(StoreError::from(SlowDown))
    }

    async fn evict(
        self: Arc<Self>,
        _max_capacity: usize,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        Err(StoreError::from(SlowDown))
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
        Err(StoreError::from(SlowDown))
    }

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Err(StoreError::from(SlowDown))
    }

    async fn copy(
        self: Arc<Self>,
        _source_partition: Partition,
        _source_address: Address,
        _destination_partition: Partition,
        _destination_context: Context,
        _behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        Err(StoreError::from(SlowDown))
    }
}

/// A [`lore_storage::MutableStore`] that fails `load` with a chosen error —
/// for every key, or for one key when `key` is set — while every other
/// operation delegates to the wrapped store.
pub struct FailingLoadStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    key: Option<lore_base::types::Hash>,
    error: StoreError,
}

impl FailingLoadStore {
    /// Fails every `load`.
    pub fn all(
        inner: Arc<dyn lore_storage::MutableStore>,
        error: StoreError,
    ) -> Arc<dyn lore_storage::MutableStore> {
        Arc::new(Self {
            inner,
            key: None,
            error,
        })
    }

    /// Fails `load` for `key` alone, so a single lookup in a larger call
    /// chain can be failed in isolation.
    pub fn for_key(
        inner: Arc<dyn lore_storage::MutableStore>,
        key: lore_base::types::Hash,
        error: StoreError,
    ) -> Arc<dyn lore_storage::MutableStore> {
        Arc::new(Self {
            inner,
            key: Some(key),
            error,
        })
    }
}

#[async_trait::async_trait]
impl lore_storage::MutableStore for FailingLoadStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::MutableStore::hold_for_command(self.inner.clone()).await
    }

    fn stored_typed_key(
        &self,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> lore_base::types::Hash {
        self.inner.stored_typed_key(key, key_type)
    }

    /// Joins the claim of the store this one wraps.
    fn try_join_claim(&self) -> Option<lore_storage::local::store_lock::StoreGuard> {
        self.inner.try_join_claim()
    }

    async fn load(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        if self.key.is_none_or(|failing| failing == key) {
            return Err(self.error.clone());
        }
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        expected: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key_type: lore_base::types::KeyType,
        consistency: lore_storage::ReadConsistency,
    ) -> Result<lore_storage::KeyValueStream, StoreError> {
        self.inner
            .clone()
            .list(partition, key_type, consistency)
            .await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}

/// Holds each load of a gated key, after the first loads it lets through, until the test calls
/// [`LoadGate::open`]. A held load counts as in flight until it returns or is cancelled, so a test
/// can see how many loads run at once and whether cancelled loads are released.
pub struct LoadGate {
    gated: std::collections::HashSet<lore_base::types::Hash>,
    passing: std::sync::atomic::AtomicUsize,
    open: tokio::sync::watch::Sender<bool>,
    in_flight: tokio::sync::watch::Sender<usize>,
    max_in_flight: std::sync::atomic::AtomicUsize,
}

impl LoadGate {
    /// A gate that lets the first `passing` loads of its gated keys through, and holds the loads
    /// after them.
    fn new(gated: impl IntoIterator<Item = lore_base::types::Hash>, passing: usize) -> Self {
        Self {
            gated: gated.into_iter().collect(),
            passing: std::sync::atomic::AtomicUsize::new(passing),
            open: tokio::sync::watch::Sender::new(false),
            in_flight: tokio::sync::watch::Sender::new(0),
            max_in_flight: Default::default(),
        }
    }

    /// Releases every held load, and lets later gated loads through at once.
    pub fn open(&self) {
        self.open.send_replace(true);
    }

    pub fn in_flight(&self) -> usize {
        *self.in_flight.borrow()
    }

    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Waits until the number of gated loads in flight satisfies `predicate`.
    pub async fn wait_for_in_flight(&self, predicate: impl FnMut(&usize) -> bool) {
        self.in_flight
            .subscribe()
            .wait_for(predicate)
            .await
            .expect("the gate owns the sender");
    }

    /// Returns once a load of `key` may go on: at once for a key that is not gated or is let
    /// through, and once the gate opens otherwise.
    async fn pass(&self, key: lore_base::types::Hash) {
        if !self.gated.contains(&key)
            || self
                .passing
                .try_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |left| left.checked_sub(1),
                )
                .is_ok()
        {
            return;
        }
        let _in_flight = InFlight::enter(self);
        self.open
            .subscribe()
            .wait_for(|open| *open)
            .await
            .expect("the gate owns the sender");
    }
}

/// Counts a gated load as in flight for as long as it lives.
struct InFlight<'a>(&'a LoadGate);

impl<'a> InFlight<'a> {
    fn enter(gate: &'a LoadGate) -> Self {
        gate.in_flight.send_modify(|count| {
            *count += 1;
            gate.max_in_flight
                .fetch_max(*count, std::sync::atomic::Ordering::SeqCst);
        });
        Self(gate)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.send_modify(|count| *count -= 1);
    }
}

/// A [`lore_storage::MutableStore`] that holds each `load` of a gated key at its [`LoadGate`].
pub struct GatedLoadStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    gate: LoadGate,
}

impl GatedLoadStore {
    /// A store that lets the first `passing` loads of its gated keys through, and holds the loads
    /// after them.
    pub fn passing_first(
        inner: Arc<dyn lore_storage::MutableStore>,
        gated: impl IntoIterator<Item = lore_base::types::Hash>,
        passing: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            gate: LoadGate::new(gated, passing),
        })
    }
}

impl std::ops::Deref for GatedLoadStore {
    type Target = LoadGate;

    fn deref(&self) -> &LoadGate {
        &self.gate
    }
}

#[async_trait::async_trait]
impl lore_storage::MutableStore for GatedLoadStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::MutableStore::hold_for_command(self.inner.clone()).await
    }

    fn stored_typed_key(
        &self,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> lore_base::types::Hash {
        self.inner.stored_typed_key(key, key_type)
    }

    /// Joins the claim of the store this one wraps.
    fn try_join_claim(&self) -> Option<lore_storage::local::store_lock::StoreGuard> {
        self.inner.try_join_claim()
    }

    async fn load(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.gate.pass(key).await;
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        expected: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key_type: lore_base::types::KeyType,
        consistency: lore_storage::ReadConsistency,
    ) -> Result<lore_storage::KeyValueStream, StoreError> {
        self.inner
            .clone()
            .list(partition, key_type, consistency)
            .await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}

/// An [`ImmutableStore`] that holds each payload read of a gated hash at its [`LoadGate`], while
/// every other operation delegates to the wrapped store.
pub struct GatedGetStore {
    inner: Arc<dyn ImmutableStore>,
    gate: LoadGate,
}

impl GatedGetStore {
    pub fn new(
        inner: Arc<dyn ImmutableStore>,
        gated: impl IntoIterator<Item = lore_base::types::Hash>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            gate: LoadGate::new(gated, 0),
        })
    }
}

impl std::ops::Deref for GatedGetStore {
    type Target = LoadGate;

    fn deref(&self) -> &LoadGate {
        &self.gate
    }
}

#[async_trait]
impl ImmutableStore for GatedGetStore {
    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    fn try_join_claim(&self) -> Option<lore_storage::local::store_lock::StoreGuard> {
        self.inner.try_join_claim()
    }

    fn isolates_partitions(&self) -> bool {
        self.inner.isolates_partitions()
    }

    fn read_scope(&self) -> lore_storage::StoreMatch {
        self.inner.read_scope()
    }

    fn query_scope(&self) -> lore_storage::StoreMatch {
        self.inner.query_scope()
    }

    /// Payload reads through `get_into` reach this through its default implementation.
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.gate.pass(address.hash).await;
        self.inner.clone().get(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [lore_storage::StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
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
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

/// A [`lore_storage::MutableStore`] whose `list` leaves out chosen keys, as a range read that misses
/// entries does, while every other operation delegates to the wrapped store.
pub struct ListOmittingStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    omitted: Vec<lore_base::types::Hash>,
}

impl ListOmittingStore {
    /// `inner`, leaving each of `keys`, given as `list` reports them, out of every `list`.
    pub fn wrap(
        inner: Arc<dyn lore_storage::MutableStore>,
        keys: impl IntoIterator<Item = lore_base::types::Hash>,
    ) -> Arc<dyn lore_storage::MutableStore> {
        Arc::new(Self {
            inner,
            omitted: keys.into_iter().collect(),
        })
    }
}

#[async_trait::async_trait]
impl lore_storage::MutableStore for ListOmittingStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::MutableStore::hold_for_command(self.inner.clone()).await
    }

    fn stored_typed_key(
        &self,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> lore_base::types::Hash {
        self.inner.stored_typed_key(key, key_type)
    }

    /// Joins the claim of the store this one wraps.
    fn try_join_claim(&self) -> Option<lore_storage::local::store_lock::StoreGuard> {
        self.inner.try_join_claim()
    }

    async fn load(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        expected: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key_type: lore_base::types::KeyType,
        consistency: lore_storage::ReadConsistency,
    ) -> Result<lore_storage::KeyValueStream, StoreError> {
        let listed: Vec<_> = futures::StreamExt::collect(
            self.inner
                .clone()
                .list(partition, key_type, consistency)
                .await?,
        )
        .await;
        let (stream, sender) = lore_storage::KeyValueStream::new();
        for (key, value) in listed {
            if !self.omitted.contains(&key) {
                let _ = sender.send((key, value));
            }
        }
        Ok(stream)
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}

/// A [`lore_storage::MutableStore`] that records the key of every `load`, the key type and
/// consistency of every `list`, and counts the keys it puts in stored form, while delegating every
/// operation to the wrapped store.
pub struct RecordingReadStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    loaded: parking_lot::Mutex<Vec<lore_base::types::Hash>>,
    listed: parking_lot::Mutex<Vec<(lore_base::types::KeyType, lore_storage::ReadConsistency)>>,
    stored_keys: std::sync::atomic::AtomicUsize,
}

impl RecordingReadStore {
    pub fn new(inner: Arc<dyn lore_storage::MutableStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            loaded: Default::default(),
            listed: Default::default(),
            stored_keys: Default::default(),
        })
    }

    /// The number of keys put in stored form since the last call.
    pub fn take_stored_key_count(&self) -> usize {
        self.stored_keys
            .swap(0, std::sync::atomic::Ordering::SeqCst)
    }

    /// The keys loaded since the last call, in the order they were loaded.
    pub fn take_loaded(&self) -> Vec<lore_base::types::Hash> {
        std::mem::take(&mut *self.loaded.lock())
    }

    /// The key type and consistency of each list since the last call, in the order the lists
    /// were made.
    pub fn take_listed(&self) -> Vec<(lore_base::types::KeyType, lore_storage::ReadConsistency)> {
        std::mem::take(&mut *self.listed.lock())
    }
}

#[async_trait::async_trait]
impl lore_storage::MutableStore for RecordingReadStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::MutableStore::hold_for_command(self.inner.clone()).await
    }

    fn stored_typed_key(
        &self,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> lore_base::types::Hash {
        self.stored_keys
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.stored_typed_key(key, key_type)
    }

    /// Joins the claim of the store this one wraps.
    fn try_join_claim(&self) -> Option<lore_storage::local::store_lock::StoreGuard> {
        self.inner.try_join_claim()
    }

    async fn load(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.loaded.lock().push(key);
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        expected: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key_type: lore_base::types::KeyType,
        consistency: lore_storage::ReadConsistency,
    ) -> Result<lore_storage::KeyValueStream, StoreError> {
        self.listed.lock().push((key_type, consistency));
        self.inner
            .clone()
            .list(partition, key_type, consistency)
            .await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}

pub async fn test_store_create() -> Result<
    (
        Arc<dyn ImmutableStore>,
        Arc<dyn lore_storage::MutableStore>,
        Arc<lore_revision::interface::ExecutionContext>,
    ),
    StoreError,
> {
    let execution =
        lore_server::util::setup_execution("test", String::default(), String::default());

    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let immutable = lore_storage::local::immutable_store::create(
                None::<&str>, /* No on disk path, in-memory only */
                /* No max capacity, eviction, max size, or compaction */
                ImmutableStoreCreateOptions::none(),
                false, /* Do not deserialize buckets */
                lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
            )
            .await?;
            let mutable: Arc<dyn lore_storage::MutableStore> =
                lore_storage::local::mutable_store::create(
                    None::<&str>, /* No on disk path, in-memory only */
                    lore_storage::MutableStoreSettings::default(),
                    immutable.clone(),
                )
                .await?;
            Ok((immutable, mutable, execution_context()))
        })
        .await
}

/// A [`lore_storage::MutableStore`] in which another writer stores `racing` under the key of the
/// first repository name mapping a caller writes, just before that write, as a write that races
/// the caller's would. Every operation delegates to the wrapped store.
pub struct RacedNameWriteStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    racing: std::sync::Mutex<Option<lore_base::types::Hash>>,
}

impl RacedNameWriteStore {
    /// A store whose first name mapping write is preceded by a write that binds the name to
    /// `racing`.
    pub fn binding(
        inner: Arc<dyn lore_storage::MutableStore>,
        racing: lore_revision::lore::RepositoryId,
    ) -> Arc<dyn lore_storage::MutableStore> {
        Arc::new(Self {
            inner,
            racing: std::sync::Mutex::new(Some(lore_base::types::Hash::from_context(
                racing.into(),
            ))),
        })
    }

    async fn race(
        &self,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) {
        if key_type != lore_base::types::KeyType::RepositoryId {
            return;
        }
        let racing = self.racing.lock().expect("the racing value").take();
        if let Some(racing) = racing {
            self.inner
                .clone()
                .store(partition, key, racing, key_type)
                .await
                .expect("the racing write");
        }
    }
}

#[async_trait::async_trait]
impl lore_storage::MutableStore for RacedNameWriteStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::MutableStore::hold_for_command(self.inner.clone()).await
    }

    fn stored_typed_key(
        &self,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> lore_base::types::Hash {
        self.inner.stored_typed_key(key, key_type)
    }

    /// Joins the claim of the store this one wraps.
    fn try_join_claim(&self) -> Option<lore_storage::local::store_lock::StoreGuard> {
        self.inner.try_join_claim()
    }

    async fn load(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<(), StoreError> {
        self.race(partition, key, key_type).await;
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key: lore_base::types::Hash,
        expected: lore_base::types::Hash,
        value: lore_base::types::Hash,
        key_type: lore_base::types::KeyType,
    ) -> Result<lore_base::types::Hash, StoreError> {
        self.race(partition, key, key_type).await;
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: lore_base::types::Partition,
        key_type: lore_base::types::KeyType,
        consistency: lore_storage::ReadConsistency,
    ) -> Result<lore_storage::KeyValueStream, StoreError> {
        self.inner
            .clone()
            .list(partition, key_type, consistency)
            .await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}

/// Writes the metadata blob of a repository `id` named `name`, created by `alice`, and its
/// pointer, without a name mapping, and answers a server context for `id`.
pub async fn seed_metadata(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    id: lore_revision::lore::RepositoryId,
    name: &str,
) -> Arc<lore_revision::repository::RepositoryContext> {
    let repository = Arc::new(
        lore_revision::repository::RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            id,
            lore_revision::state::allow_all_repositories(),
        ),
    );
    let metadata_hash = lore_revision::repository::metadata_store(
        repository.clone(),
        lore_revision::repository::RepositoryMetadata {
            name: name.to_string(),
            creator: "alice".into(),
            ..Default::default()
        },
    )
    .await
    .expect("Failed to store repository metadata");
    lore_revision::repository::metadata_store_hash(repository.clone(), metadata_hash)
        .await
        .expect("Failed to store repository metadata hash");
    repository
}

/// The repository `name` maps to in the name index of `mutable_store`. An unbound name panics.
pub async fn bound_id(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    name: &str,
) -> lore_revision::lore::RepositoryId {
    let repository = Arc::new(
        lore_revision::repository::RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            lore_revision::lore::RepositoryId::default(),
            lore_revision::state::allow_all_repositories(),
        ),
    );
    lore_revision::repository::id_from_name(repository, name)
        .await
        .expect("the name is bound")
}

pub struct TestInstrumentProvider;

impl lore_telemetry::InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
}

/// Binds `name` to `id` in the name index of `mutable_store`, without metadata for `id`.
pub async fn bind_without_metadata(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    name: &str,
    id: lore_revision::lore::RepositoryId,
) {
    let repository = Arc::new(
        lore_revision::repository::RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            id,
            lore_revision::state::allow_all_repositories(),
        ),
    );
    lore_revision::repository::store_name_to_id(repository, name, id)
        .await
        .expect("Failed to store the name mapping");
}

/// Whether `name` is unbound in the name index of `mutable_store`. Any other failure panics.
pub async fn unbound(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    name: &str,
) -> bool {
    let repository = Arc::new(
        lore_revision::repository::RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            lore_revision::lore::RepositoryId::default(),
            lore_revision::state::allow_all_repositories(),
        ),
    );
    match lore_revision::repository::id_from_name(repository, name).await {
        Ok(_) => false,
        Err(err) if err.is_address_not_found() => true,
        Err(err) => panic!("reading the mapping of {name}: {err}"),
    }
}
