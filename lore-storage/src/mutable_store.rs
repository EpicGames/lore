// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;

use crate::Hash;
use crate::Partition;
use crate::immutable_store::CorrelationHold;
use crate::immutable_store::StoreError;
use crate::local::store_lock::StoreGuard;
use crate::store_types::KeyType;
use crate::store_types::KeyValueStream;

#[async_trait]
pub trait MutableStore: Send + Sync {
    /// Claims this store for the span it is about to be used — a storage handle from
    /// open to close, or one repository command.
    ///
    /// The claim is what tells other processes this one is using the store, so it
    /// belongs to the span rather than to each call within it. It is also the pair a
    /// repository lock is contained within: store locks are the outer ones, because
    /// they are the pair the storage API can also take, and two orders over one pair
    /// would be a cycle.
    ///
    /// `None` means this store has nothing on disk to claim — a remote store, or an
    /// in-memory one. There is no default answer: every store decides for itself, so a
    /// wrapper around a store with files on disk delegates to it rather than answering
    /// `None`, which would open a handle holding nothing that then reads and writes a
    /// store no process has claimed.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the lock cannot be taken, or if the reload a stale
    /// acquisition demands fails.
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError>;

    /// Joins the claim this process holds on this store, without waiting: a counted guard when
    /// the store's flock is held and this store is current, `None` otherwise and for a store
    /// with nothing on disk to claim. The guard keeps the flock for work spawned to outlive the
    /// caller and covers no operation, as
    /// [`crate::immutable_store::claim_for_task`] describes. A wrapper delegates it as it
    /// delegates [`Self::hold_for_command`].
    fn try_join_claim(&self) -> Option<StoreGuard> {
        None
    }

    /// Keeps what this store holds for calls under `correlation_id`, as
    /// [`crate::ImmutableStore::hold_correlation`] describes.
    fn hold_correlation(&self, _correlation_id: &str) -> Option<CorrelationHold> {
        None
    }

    /// Returns the mutable value stored for `key` with the type `key_type` within `partition`.
    ///
    /// Returns `StoreError::AddressNotFound` if no value of the given type is stored for the given key.
    /// Other errors indicate store inconsistency or inability to perform the load.
    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError>;

    /// Store a new mutable value for `key` of type `key_type` within `partition`.
    ///
    /// Storing a null hash (`Hash::default()`) removes the key.
    /// Errors indicate store inconsistency or inability to perform the store.
    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError>;

    /// Compare and swap a mutable value for `key` with the type `key_type` within `partition`.
    ///
    /// Updates the value for the given key if the current value matches `expected`
    /// or the key does not exist. Returns the previous value of the key (equal to
    /// `expected` if the swap succeeded).
    /// Errors indicate store inconsistency or inability to perform the operation.
    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError>;

    /// List all the key-value pairs of a given type for the given partition.
    /// If the partition is null, it must match all partitions the user have access to
    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError>;

    /// Flush any pending writes to durable storage.
    /// When `sync_data` is true, data is synced to the storage media (fsync).
    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError>;
}

impl Debug for dyn MutableStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MutableStore")
    }
}
