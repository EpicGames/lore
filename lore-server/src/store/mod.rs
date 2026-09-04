// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod configuration;

pub mod grpc_replica;
pub mod replica;
pub mod replica_factory;
pub mod replicated_store; // Mutable and Immutable store facade, where store operations are forwarded via QUIC to a remote Lore Server

// Re-export configuration items for convenience
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub use configuration::StoreConfigError;
pub use configuration::create_immutable_store_with_registry;
pub use configuration::create_lock_store_with_registry;
pub use configuration::create_mutable_store_with_registry;
pub use configuration::empty_plugin_config;
pub use configuration::has_plugin_config;
pub use configuration::resolve_plugin_config;
pub use configuration::resolve_plugin_config_with_fallback;
use lore_base::lore_spawn;
#[cfg(test)]
use lore_revision::runtime::execution_context;
use lore_storage::ImmutableStore;
#[cfg(test)]
use lore_storage::StoreError;
#[cfg(test)]
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use tracing::info;

use crate::http::server::ServerHealth;

#[derive(Default)]
struct StoreInstrumentProvider {}

impl InstrumentProvider for StoreInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.store.immutable.local"
    }

    fn labels(&self) -> &[KeyValue] {
        &[]
    }
}

struct StoreInstruments {
    instrument_provider: StoreInstrumentProvider,
    fragment_count: Gauge<u64>,
    used_bytes: Gauge<u64>,
    available: Gauge<u64>,
}

impl InstrumentProvider for StoreInstruments {
    fn namespace(&self) -> &'static str {
        self.instrument_provider.namespace()
    }

    fn labels(&self) -> &[KeyValue] {
        self.instrument_provider.labels()
    }
}

static STORE_INSTRUMENTS: OnceLock<StoreInstruments> = OnceLock::new();

static METRICS_FRAGMENT_COUNT_METRIC_NAME: &str = "fragment-count";
static METRICS_USED_BYTES_METRIC_NAME: &str = "used-bytes";
static METRICS_AVAILABLE_METRIC_NAME: &str = "available";

impl Default for StoreInstruments {
    fn default() -> Self {
        let instrument_provider = StoreInstrumentProvider::default();
        let fragment_count = instrument_provider.gauge(METRICS_FRAGMENT_COUNT_METRIC_NAME);
        let used_bytes = instrument_provider.gauge(METRICS_USED_BYTES_METRIC_NAME);
        let available = instrument_provider.gauge(METRICS_AVAILABLE_METRIC_NAME);

        Self {
            instrument_provider,
            fragment_count,
            used_bytes,
            available,
        }
    }
}

pub async fn memory_stats_reporter(store: Weak<dyn ImmutableStore>, interval: Option<Duration>) {
    let instruments = STORE_INSTRUMENTS.get_or_init(Default::default);
    while let Some(store) = store.upgrade() {
        tokio::time::sleep(interval.unwrap_or(Duration::from_secs(10))).await;

        let fragment_count = store.fragment_count().await;

        let stats = lore_base::allocator::memory_stats();

        info!(
            "Server local store memory stats: {} fragments, {} growvec bytes",
            fragment_count.unwrap_or_default(),
            stats.used_bytes
        );

        instruments
            .fragment_count
            .record(fragment_count.unwrap_or_default() as u64, &[]);
        instruments.used_bytes.record(stats.used_bytes as u64, &[]);
    }
}

pub fn spawn_immutable_store_availability_monitor(health: Arc<ServerHealth>) {
    let instruments = STORE_INSTRUMENTS.get_or_init(Default::default);
    if let Some((interval, timeout)) = health.interval_timeout {
        lore_spawn!(async move {
            // Check if the store is in good condition every given interval
            loop {
                tokio::time::sleep(interval.max(Duration::from_secs(10))).await;

                if let Some(store) = health.immutable_store.upgrade() {
                    let is_available = store.is_available(timeout).await;
                    health.available.store(is_available, Ordering::Release);

                    instruments
                        .available
                        .record(if is_available { 1 } else { 0 }, &[]);
                } else {
                    break;
                }
            }
        });
    }
}

/// A [`lore_storage::MutableStore`] that fails `load` with a chosen error —
/// for every key, or for one key when `key` is set — while every other
/// operation delegates to the wrapped store.
#[cfg(test)]
pub struct FailingLoadStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    key: Option<lore_base::types::Hash>,
    error: StoreError,
}

#[cfg(test)]
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

#[cfg(test)]
#[async_trait::async_trait]
impl lore_storage::MutableStore for FailingLoadStore {
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

#[cfg(test)]
pub async fn test_store_create() -> Result<
    (
        Arc<dyn ImmutableStore>,
        Arc<dyn lore_storage::MutableStore>,
        Arc<lore_revision::interface::ExecutionContext>,
    ),
    StoreError,
> {
    let execution = crate::util::setup_execution("test", String::default(), String::default());

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
