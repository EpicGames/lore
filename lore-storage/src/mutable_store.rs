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

/// Values held by keys of a [`KeyType`] within a [`Partition`].
///
/// A key holds zero when it is absent, and a stored zero is an absent key: every method treats it
/// as one, and a write that removes the key removes it.
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
    /// Returns `StoreError::AddressNotFound` when the key holds zero.
    /// Other errors indicate store inconsistency or inability to perform the load.
    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError>;

    /// Store a new mutable value for `key` of type `key_type` within `partition`.
    ///
    /// Storing a null hash (`Hash::default()`) removes the key, and leaves no entry or item for
    /// it, one holding zero included.
    /// Errors indicate store inconsistency or inability to perform the store.
    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError>;

    /// Swaps the value of `key` with the type `key_type` within `partition` from `expected` to
    /// `value`, atomically with respect to every other write of the key:
    ///
    /// - A key holding other than `expected` is left unchanged.
    /// - A key holding `expected` holds `value` after the call, created if absent. For a zero
    ///   `value` the key is removed, and no entry or item is left for it, one holding zero
    ///   included.
    /// - A swap from a non-zero value to the same value leaves the key and its entry or item
    ///   unchanged, and a swap from zero to zero leaves an absent key without an entry or item.
    ///
    /// Returns the value the key held when the store compared it with `expected`, at an instant
    /// while the call ran. It equals `expected` when the swap is made. No interleaving of writers
    /// makes a swap succeed from a value other than `expected`, or answer a value the key did not
    /// hold while the call ran. The swap compares values alone: a key that holds `expected` again,
    /// or is absent again after a removal, swaps as if it never changed, so a caller that has to
    /// tell those apart encodes it in the value. Errors indicate store inconsistency or inability
    /// to perform the operation.
    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError>;

    /// List all the key-value pairs of a given type for the given partition, leaving out the keys
    /// that hold zero.
    /// If the partition is null, it must match all partitions the user have access to
    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError>;

    /// Flush any pending writes to durable storage, as `options` asks.
    async fn flush(self: Arc<Self>, options: crate::FlushOptions) -> Result<(), StoreError>;
}

impl Debug for dyn MutableStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MutableStore")
    }
}
