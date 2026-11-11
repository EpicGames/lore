// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A mutable store that forwards every call to the store it wraps, and shows each load, store,
//! compare-and-swap and list to a hook first, so a test can fail or record them. A second hook,
//! where given, sees each load, store and compare-and-swap the store answers without an error, so a
//! test can write between calls or fail a call that landed.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_storage::FlushOptions;
use lore_storage::KeyType;
use lore_storage::KeyValueStream;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::local::store_lock::StoreGuard;

/// A call a [`HookedStore`] shows its hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreCall {
    Load,
    Store,
    CompareAndSwap,
    List,
}

/// Runs before a [`HookedStore`] forwards a call, with the key and key type the call names, and a
/// zero key for a list. An error fails the call, which then reaches no store.
type Hook = Box<dyn Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync>;

/// Awaited after a [`HookedStore`] forwards a load, store or compare-and-swap that the store
/// answers without an error, with the store it wraps, the partition, and the key and key type the
/// call names. An error fails the call in place of the store's answer.
pub(crate) type Answered = Box<
    dyn Fn(
            Arc<dyn MutableStore>,
            Partition,
            StoreCall,
            Hash,
            KeyType,
        ) -> BoxFuture<'static, Result<(), StoreError>>
        + Send
        + Sync,
>;

/// A mutable store that forwards every call to `inner`, after its hook sees each load, store,
/// compare-and-swap and list, and before its `answered` hook, if any, sees each load, store and
/// compare-and-swap the store answers without an error.
pub(crate) struct HookedStore {
    inner: Arc<dyn MutableStore>,
    hook: Hook,
    answered: Option<Answered>,
}

impl HookedStore {
    /// `inner`, with `hook` run before each load, store, compare-and-swap and list it forwards, and
    /// `answered`, if any, after each load, store and compare-and-swap the store answers without an
    /// error.
    pub(crate) fn wrap(
        inner: Arc<dyn MutableStore>,
        hook: impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static,
        answered: Option<Answered>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            hook: Box::new(hook),
            answered,
        })
    }

    /// Runs the `answered` hook, if any, on a call the store answered without an error.
    async fn answered(
        &self,
        partition: Partition,
        call: StoreCall,
        key: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        match &self.answered {
            Some(answered) => answered(self.inner.clone(), partition, call, key, key_type).await,
            None => Ok(()),
        }
    }
}

/// An `answered` hook for [`HookedStore::wrap`] that awaits the future `hook` returns.
pub(crate) fn answered<Answer>(
    hook: impl Fn(Arc<dyn MutableStore>, Partition, StoreCall, Hash, KeyType) -> Answer
    + Send
    + Sync
    + 'static,
) -> Option<Answered>
where
    Answer: Future<Output = Result<(), StoreError>> + Send + 'static,
{
    Some(Box::new(
        move |inner: Arc<dyn MutableStore>,
              partition: Partition,
              call: StoreCall,
              key: Hash,
              key_type: KeyType|
              -> BoxFuture<'static, Result<(), StoreError>> {
            Box::pin(hook(inner, partition, call, key, key_type))
        },
    ))
}

#[async_trait]
impl MutableStore for HookedStore {
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    fn try_join_claim(&self) -> Option<StoreGuard> {
        self.inner.try_join_claim()
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        (self.hook)(StoreCall::Load, key, key_type)?;
        let value = self.inner.clone().load(partition, key, key_type).await?;
        self.answered(partition, StoreCall::Load, key, key_type)
            .await?;
        Ok(value)
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        (self.hook)(StoreCall::Store, key, key_type)?;
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await?;
        self.answered(partition, StoreCall::Store, key, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        (self.hook)(StoreCall::CompareAndSwap, key, key_type)?;
        let held = self
            .inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await?;
        self.answered(partition, StoreCall::CompareAndSwap, key, key_type)
            .await?;
        Ok(held)
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        (self.hook)(StoreCall::List, Hash::default(), key_type)?;
        self.inner.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}
