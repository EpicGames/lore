// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod auth;
pub mod authnz;
pub mod cache;
pub mod execution_state;
pub mod grpc;
pub mod hooks;
pub mod http;
pub mod legacy;
pub mod lock;
pub mod notification;
pub mod plugins;
pub mod protocol;
pub mod quic;
pub mod server;
pub mod server_config;
pub mod settings;
pub mod store;
pub mod telemetry;
pub mod tls;
pub mod topology;
pub mod util;

mod correlation;

/// Ensures that when we run our test suite for `lore-server` it is using
/// the same store policies that lore-server will run in production
#[cfg(test)]
#[ctor::ctor]
fn init_test_policies() {
    lore_storage::assume_server_policies();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use lore_base::error::SlowDown;
    use lore_base::types::Address;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_storage::Fragment;
    use lore_storage::ImmutableStore;
    use lore_storage::StoreError;
    use lore_storage::StoreMatch;
    use lore_storage::StoreObliterateStats;
    use lore_storage::StoreQueryResult;

    /// An `ImmutableStore` that returns `SlowDown` on every operation.
    struct SlowDownImmutableStore;

    #[async_trait]
    impl ImmutableStore for SlowDownImmutableStore {
        async fn exist(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreMatch, StoreError> {
            Err(StoreError::from(SlowDown))
        }

        async fn exist_batch(
            self: Arc<Self>,
            _partition: Partition,
            _addresses: &[Address],
            _match_requested: StoreMatch,
        ) -> Result<Vec<StoreMatch>, StoreError> {
            Err(StoreError::from(SlowDown))
        }

        async fn query(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreQueryResult, StoreError> {
            Err(StoreError::from(SlowDown))
        }

        async fn get(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_required: StoreMatch,
        ) -> Result<(Fragment, Bytes), StoreError> {
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

        async fn compact_stop(self: Arc<Self>) {}

        fn max_query_batch(&self) -> Option<usize> {
            None
        }

        async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
            Err(StoreError::from(SlowDown))
        }

        async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
            Err(StoreError::from(SlowDown))
        }
    }

    /// Sanity check test to ensure that the `lore-server` test suites are running
    /// using the server store policies, and not the library (i.e. client focused)
    /// defaults, which have many retry attempts.
    /// The server cannot attempt an operation over many minutes and should bail out
    /// early. Without the server policies, this test would time out as the Lore library
    /// retries up to 60 times by default.
    #[tokio::test]
    async fn slow_down_store_exhausts_retries_with_server_policy() {
        let store: Arc<dyn ImmutableStore> = Arc::new(SlowDownImmutableStore);
        // Non-zero so read_raw's debug_assert passes and the store is actually called.
        let address = Address::zero_context_hash(Hash::from([1u8; 32]));

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            lore_storage::read_raw(store, Partition::default(), address, StoreMatch::MatchFull),
        )
        .await
        .expect("read_raw did not return within 10s — server retry policy may not be active");

        assert!(
            result.unwrap_err().is_slow_down(),
            "Expected SlowDown after retry exhaustion"
        );
    }
}
