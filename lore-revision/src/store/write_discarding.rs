// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use lore_storage::local::store_lock::StoreGuard;

use crate::lore::Hash;
use crate::lore::Partition;
use crate::store;
use crate::store::KeyType;
use crate::store::KeyValueStream;
use crate::store::StoreError;

/// A mutable store that reads from `base` and discards every write, reporting it as made. A
/// read after a write answers what `base` holds.
pub struct WriteDiscardingMutableStore {
    base: Arc<dyn store::MutableStore>,
}

impl WriteDiscardingMutableStore {
    pub fn new(base: Arc<dyn store::MutableStore>) -> Self {
        Self { base }
    }
}

#[async_trait]
impl store::MutableStore for WriteDiscardingMutableStore {
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.base.clone().hold_for_command().await
    }

    fn try_join_claim(&self) -> Option<StoreGuard> {
        self.base.try_join_claim()
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.base.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        _partition: Partition,
        _key: Hash,
        _value: Hash,
        _key_type: KeyType,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// Answers the value `base` holds, as a swap does, so the caller reads the swap as made
    /// where that value is `expected`.
    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        _expected: Hash,
        _value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        match self.base.clone().load(partition, key, key_type).await {
            Ok(value) => Ok(value),
            Err(StoreError::AddressNotFound(_)) => Ok(Hash::default()),
            Err(err) => Err(err),
        }
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.base.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        Ok(())
    }
}
