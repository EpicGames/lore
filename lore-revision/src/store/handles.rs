// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::marker::PhantomData;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_storage::KeyValueStream;
use lore_storage::MutableStore;
use lore_storage::StoreError;

use crate::repository::RepositoryWriteToken;

/// Read-only handle to a [`MutableStore`], scoped to the borrow of a [`RepositoryContext`].
///
/// The handle forwards only `load` and `list` — write methods (`store`,
/// `compare_and_swap`, `flush`) are deliberately not forwarded so calling them
/// on a `ReadHandle` is a compile-time error. The handle holds a reference to
/// the shared `Arc<dyn MutableStore>`; callers cannot extract or clone that
/// Arc, so they cannot outlive the handle's lifetime.
///
/// [`RepositoryContext`]: crate::repository::RepositoryContext
pub struct ReadHandle<'a> {
    store: &'a Arc<dyn MutableStore>,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> ReadHandle<'a> {
    pub(crate) fn new(store: &'a Arc<dyn MutableStore>) -> Self {
        Self {
            store,
            _lifetime: PhantomData,
        }
    }

    pub async fn load(
        &self,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.store.clone().load(partition, key, key_type).await
    }

    pub async fn list(
        &self,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.store.clone().list(partition, key_type).await
    }
}

/// Write-capable handle to a [`MutableStore`], scoped to the borrow of a
/// [`RepositoryWriteToken`].
///
/// The handle forwards every `MutableStore` method. Its lifetime is bounded by
/// the token reference it was constructed from, so it cannot be stored past
/// the token's scope or moved into a `'static` task. The handle holds a
/// reference to the shared `Arc<dyn MutableStore>`; callers cannot extract or
/// clone that Arc.
///
/// [`RepositoryContext`]: crate::repository::RepositoryContext
pub struct WriteHandle<'a> {
    store: &'a Arc<dyn MutableStore>,
    _token: PhantomData<&'a RepositoryWriteToken>,
}

impl<'a> WriteHandle<'a> {
    pub(crate) fn new(store: &'a Arc<dyn MutableStore>, _: &'a RepositoryWriteToken) -> Self {
        Self {
            store,
            _token: PhantomData,
        }
    }

    pub async fn load(
        &self,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.store.clone().load(partition, key, key_type).await
    }

    pub async fn list(
        &self,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.store.clone().list(partition, key_type).await
    }

    pub async fn store(
        &self,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        self.store
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    pub async fn compare_and_swap(
        &self,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.store
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    pub async fn flush(&self, options: lore_storage::FlushOptions) -> Result<(), StoreError> {
        self.store.clone().flush(options).await
    }
}
