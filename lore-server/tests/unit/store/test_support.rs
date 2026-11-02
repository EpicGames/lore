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

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
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
    ) -> Result<lore_storage::KeyValueStream, StoreError> {
        self.inner.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
        self.inner.clone().flush(sync_data).await
    }
}

/// A [`lore_storage::MutableStore`] that records the key of every `load` while
/// delegating every operation to the wrapped store.
pub struct RecordingLoadStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    loaded: parking_lot::Mutex<Vec<lore_base::types::Hash>>,
}

impl RecordingLoadStore {
    pub fn new(inner: Arc<dyn lore_storage::MutableStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            loaded: Default::default(),
        })
    }

    /// The keys loaded since the last call, in the order they were loaded.
    pub fn take_loaded(&self) -> Vec<lore_base::types::Hash> {
        std::mem::take(&mut *self.loaded.lock())
    }
}

#[async_trait::async_trait]
impl lore_storage::MutableStore for RecordingLoadStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::MutableStore::hold_for_command(self.inner.clone()).await
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
    ) -> Result<lore_storage::KeyValueStream, StoreError> {
        self.inner.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
        self.inner.clone().flush(sync_data).await
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
