// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use lore_base::types::HealResult;
use lore_error_set::prelude::*;
use lore_storage::FlushOptions;
use lore_storage::ReadOptions;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_storage::read::check_receipt;
use lore_storage::read::heal_on_receipt;
use lore_transport::Admin;
use lore_transport::PartitionSessions;
use lore_transport::ProtocolError;
use lore_transport::SessionPool;
use lore_transport::StorageSession;

use super::StoreObliterateStats;
use crate::errors::AddressNotFound;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Fragment;
use crate::lore::Hash;
use crate::lore::Partition;
use crate::lore::execution_context;
use crate::lore_warn;
use crate::protocol;
use crate::store;
use crate::store::KeyType;
use crate::store::KeyValueStream;
use crate::store::StoreError;
use crate::store::StoreGetData;
use crate::store::StoreMatch;
use crate::store::StoreMatchResult;

/// An immutable store served by a remote over the storage protocol. A payload the remote serves is
/// checked against its address on receipt, unless the store trusts the remote
/// ([`verifying`](Self::verifying)).
pub struct RemoteImmutableStore {
    connections: protocol::RemoteConnections,
    pools: Pools,
    /// How a payload the remote serves is checked on receipt.
    receipt: ReadOptions,
}

impl RemoteImmutableStore {
    pub fn new(remote_url: &str, identity: Option<&str>) -> Self {
        RemoteImmutableStore {
            connections: protocol::RemoteConnections::new(remote_url, identity),
            pools: Pools::default(),
            receipt: ReadOptions::default().no_decompress(),
        }
    }

    /// A store whose connection for `partition` is `connection`.
    #[cfg(feature = "test-util")]
    pub fn with_connection(
        partition: Partition,
        connection: Arc<lore_transport::Connection>,
    ) -> Self {
        RemoteImmutableStore {
            connections: protocol::RemoteConnections::with_connection(partition, connection),
            pools: Pools::default(),
            receipt: ReadOptions::default().no_decompress(),
        }
    }

    /// This store, checking each payload the remote serves against its address where `verify`, or
    /// otherwise trusting the remote and checking only the payload's sizes ([`check_receipt`]).
    pub fn verifying(mut self, verify: bool) -> Self {
        self.receipt.trust_remote = !verify;
        self
    }

    /// See [`RemoteConnections::connecting_over`](protocol::RemoteConnections::connecting_over).
    pub fn connecting_over(mut self, storage_connections: usize) -> Self {
        self.connections = self.connections.connecting_over(storage_connections);
        self
    }

    /// A session for ops on `partition`, as [`remote_session`] takes it.
    pub fn session(
        &self,
        partition: Partition,
    ) -> impl Future<Output = Result<Arc<StorageSession>, StoreError>> + '_ {
        remote_session(&self.connections, &self.pools, partition)
    }

    /// The remote's admin service for `partition`, as [`protocol::RemoteConnections::admin`] hands
    /// it out.
    ///
    /// Returns that future mapped, without a future of its own.
    pub fn admin(
        &self,
        partition: Partition,
    ) -> impl Future<Output = Result<Arc<dyn Admin>, StoreError>> + '_ {
        futures::FutureExt::map(self.connections.admin(partition), move |admin| {
            admin.forward_with::<StoreError, _>(|| {
                format!(
                    "connecting to remote store at {}",
                    self.connections.remote_url()
                )
            })
        })
    }
}

/// The session pools a remote store's calls share. Calls under a correlation id a hold names share
/// a pool per partition until the last hold on it drops. Calls under any other share the most
/// recent pool per partition, which a call under another correlation id replaces.
#[lore_macro::test_pub]
#[derive(Default)]
struct Pools {
    held: Arc<DashMap<Arc<str>, HeldPools>>,
    recent: PartitionSessions,
}

/// The pools of one held correlation id, and the holds on it.
#[lore_macro::test_pub]
#[derive(Default)]
struct HeldPools {
    holds: usize,
    pools: Vec<(Partition, Arc<SessionPool>)>,
}

/// A hold on one correlation id's pools, which drops them as the last hold drops.
#[lore_macro::test_pub]
struct PoolsHold {
    held: Arc<DashMap<Arc<str>, HeldPools>>,
    correlation_id: Arc<str>,
}

impl Drop for PoolsHold {
    fn drop(&mut self) {
        self.held.remove_if_mut(&*self.correlation_id, |_, held| {
            held.holds -= 1;
            held.holds == 0
        });
    }
}

impl Pools {
    /// A hold on the pools of calls under `correlation_id`. A hold on a correlation id already held
    /// allocates nothing.
    #[lore_macro::test_pub]
    #[allow(clippy::disallowed_methods)] // The shard lock is held for the count alone.
    fn hold(&self, correlation_id: &str) -> PoolsHold {
        let mut held = self
            .held
            .get_mut(correlation_id)
            .unwrap_or_else(|| self.held.entry(Arc::from(correlation_id)).or_default());
        held.holds += 1;
        PoolsHold {
            held: self.held.clone(),
            correlation_id: held.key().clone(),
        }
    }

    /// The live pool calls under `correlation_id` share for `partition`.
    #[lore_macro::test_pub]
    fn get_live(&self, partition: Partition, correlation_id: &str) -> Option<Arc<SessionPool>> {
        let Some(held) = self.held.get(correlation_id) else {
            return self.recent.get_live(partition, correlation_id);
        };
        held.pools
            .iter()
            .find(|(kept, pool)| *kept == partition && is_live(pool))
            .map(|(_, pool)| pool.clone())
    }

    /// Keeps `pool`, started for `partition` under `correlation_id`, for later calls to share, and
    /// returns the pool to use: one a concurrent call kept first for a held correlation id, or
    /// `pool`. A held pool that went stale, or whose connection did, is replaced by `pool`.
    #[lore_macro::test_pub]
    fn keep(
        &self,
        partition: Partition,
        correlation_id: &str,
        pool: Arc<SessionPool>,
    ) -> Arc<SessionPool> {
        let Some(mut held) = self.held.get_mut(correlation_id) else {
            self.recent.hold(partition, pool.clone());
            return pool;
        };
        match held.pools.iter_mut().find(|(kept, _)| *kept == partition) {
            Some((_, kept)) if is_live(kept) => kept.clone(),
            Some((_, kept)) => {
                *kept = pool.clone();
                pool
            }
            None => {
                held.pools.push((partition, pool.clone()));
                pool
            }
        }
    }
}

/// Whether `pool` may serve another call: it is not stale and neither is its connection.
fn is_live(pool: &SessionPool) -> bool {
    !pool.is_stale() && !pool.connection_is_stale()
}

/// A session for ops on `partition`, from the pool `pools` keeps under the call's correlation id,
/// started on the connection `connections` holds for the partition.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn remote_session<'a>(
    connections: &'a protocol::RemoteConnections,
    pools: &'a Pools,
    partition: Partition,
) -> impl Future<Output = Result<Arc<StorageSession>, StoreError>> + 'a {
    async move {
        if let Some(pool) = pools.get_live(
            partition,
            execution_context().globals().correlation_id.as_str(),
        ) {
            return Ok(pool.pick());
        }
        let connection = connections
            .connection(partition)
            .await
            .forward_with::<StoreError, _>(|| {
                format!("connecting to remote store at {}", connections.remote_url())
            })?;
        let execution = execution_context();
        let pool = connection
            .unpinned_session_pool(partition, execution.globals().correlation_id.as_str())
            .await
            .forward_with::<StoreError, _>(|| {
                format!(
                    "creating session to remote store at {}",
                    connections.remote_url()
                )
            })?;
        Ok(pools
            .keep(partition, execution.globals().correlation_id.as_str(), pool)
            .pick())
    }
}

/// `err` as an internal failure where the remote refused the connection or the request as
/// unsupported. A read takes an internal failure as a miss, as it takes a remote it cannot reach,
/// and fails on `NotSupported`, which it takes as a payload this build cannot decode.
fn unsupported_as_internal(err: StoreError) -> StoreError {
    if err.is_not_supported() {
        return StoreError::internal_with_context(err, "remote refused the request as unsupported");
    }
    err
}

/// `err`, the remote's answer that it holds nothing at `address`, as `AddressNotFound` chained to
/// it under `context` at the caller's location.
#[track_caller]
fn missing_on_remote(address: Address, err: ProtocolError, context: &str) -> StoreError {
    StoreError::AddressNotFound(AddressNotFound::from(address).chain_err_from(err, context))
}

#[async_trait]
impl store::ImmutableStore for RemoteImmutableStore {
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    /// Keeps a session pool per partition for calls under `correlation_id`, as [`Pools`] does.
    fn hold_correlation(&self, correlation_id: &str) -> Option<lore_storage::CorrelationHold> {
        Some(Box::new(self.pools.hold(correlation_id)))
    }

    /// The peer answers exact addresses only, so nothing it returns can have come from another
    /// partition.
    fn isolates_partitions(&self) -> bool {
        true
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(addresses.len(), results.len());

        let session = self.session(partition).await?;
        let status = session
            .query(addresses)
            .await
            .forward::<StoreError>("querying remote store")?;

        if status.len() != addresses.len() {
            lore_warn!(
                "Query returned incorrect number of results, expected {}, but got {}",
                addresses.len(),
                status.len()
            );
            return Err(StoreError::internal("Remote store failed"));
        }

        for ((byte, result), address) in status.iter().zip(results.iter_mut()).zip(addresses.iter())
        {
            let match_made = match byte {
                0 => StoreMatch::MatchFull,
                1 => StoreMatch::MatchPartition,
                _ => StoreMatch::MatchNone,
            };

            *result = if match_made == StoreMatch::MatchNone {
                StoreMatchResult::default()
            } else {
                StoreMatchResult {
                    match_made,
                    // Never anything but the partition asked about: the peer resolves within it,
                    // and a match found anywhere else collapses to absence before it is sent.
                    partition,
                    context: if match_made == StoreMatch::MatchFull {
                        address.context
                    } else {
                        Context::default()
                    },
                    stored_local: false,
                    stored_durable: true,
                }
            };
        }

        Ok(())
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let session = self.session(partition).await?;

        match session.get_metadata(&address).await {
            Ok(fragment) => Ok(StoreGetData::metadata(
                fragment,
                StoreMatch::MatchFull,
                partition,
            )),
            Err(ProtocolError::NotFound(_)) => Ok(StoreGetData::default()),
            Err(error) => Err(error).forward::<StoreError>("Remote store metadata query failed"),
        }
    }

    /// Serves the payload as the remote stored it, checked on receipt as [`check_receipt`] does:
    /// against its address, with the content the check expanded, unless the store trusts the
    /// remote. A payload that fails the check is fetched again once where the remote heals it
    /// ([`heal_on_receipt`]). An address the remote lacks is answered `AddressNotFound`. It answers
    /// `NotSupported` only for a payload this build cannot decode ([`unsupported_as_internal`]).
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let session = self
            .session(partition)
            .await
            .map_err(unsupported_as_internal)?;
        let mut heal_attempted = false;
        loop {
            let raw = match session.get(&address).await {
                Err(err @ ProtocolError::NotFound(_)) => {
                    return Err(missing_on_remote(address, err, "address missing on remote"));
                }
                got => got
                    .forward::<StoreError>("Remote store get failed")
                    .map_err(unsupported_as_internal)?,
            };
            match check_receipt(&raw, address, self.receipt) {
                // The peer answers exact addresses only, so anything it served was a full match.
                Ok(content) => {
                    let (fragment, payload) = raw;
                    return Ok(StoreGetData {
                        fragment,
                        match_made: StoreMatch::MatchFull,
                        partition,
                        payload: Some(payload),
                        content,
                    });
                }
                Err(err) => {
                    if !heal_on_receipt(&session, address, &err, heal_attempted).await {
                        return Err(err).forward_any::<StoreError>("checking a remote payload");
                    }
                    heal_attempted = true;
                }
            }
        }
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        mut fragment: Fragment,
        payload: Option<Bytes>,
        _force: bool,
    ) -> Result<(), StoreError> {
        sanitise_fragment_behavior_flags(&mut fragment);

        if let Some(payload) = payload.as_ref() {
            lore_storage::validate_fragment_payload(&fragment, payload.len())?;
        } else {
            lore_storage::validate_fragment_size(&fragment)?;
        }
        let session = self.session(partition).await?;
        session
            .put(address, fragment, payload)
            .await
            .forward("Remote store put failed")
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        // The remote service decides both for itself: durability is tracked on its own side, and
        // it owns which of its peers the copy reaches. The local-flag bookkeeping the caller's
        // behaviour describes happens in the local-store leg of a composite copy.
        _behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        let session = self.session(destination_partition).await?;
        session
            .copy(source_partition, source_address, destination_context)
            .await
            .forward("Remote copy failed")
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        _stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let admin = self.admin(partition).await?;
        match admin.obliterate(address).await {
            Ok(()) => Ok(()),
            Err(err @ ProtocolError::NotFound(_)) => {
                Err(missing_on_remote(address, err, "address missing on remote"))
            }
            Err(other) => Err(other).forward("Remote store obliterate failed"),
        }
    }

    async fn evict(
        self: Arc<Self>,
        _max_capacity: usize,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        // Noop for remote store
        Ok(0)
    }

    async fn compact(
        self: Arc<Self>,
        _max_size: usize,
        _at: Option<usize>,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        // Noop for remote store
        Ok(None)
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        None
    }

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Ok(())
    }

    /// Asks the remote to check its copy of `address` and repair it. A remote that holds no copy of
    /// `address` answers [`HealResult::NotAttempted`].
    async fn heal(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<HealResult, StoreError> {
        let session = self.session(partition).await?;
        match session.verify(&address, true).await {
            Ok(verified) => Ok(verified.healed),
            Err(ProtocolError::NotFound(_)) => Ok(HealResult::NotAttempted),
            Err(err) => Err(err).forward::<StoreError>("Remote store heal failed"),
        }
    }

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
        // Noop for remote store
        Ok(())
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }
}

pub struct RemoteMutableStore {
    connections: protocol::RemoteConnections,
    pools: Pools,
}

impl RemoteMutableStore {
    pub fn new(remote_url: &str, identity: Option<&str>) -> Self {
        RemoteMutableStore {
            connections: protocol::RemoteConnections::new(remote_url, identity),
            pools: Pools::default(),
        }
    }

    /// A store whose connection for `partition` is `connection`.
    #[cfg(feature = "test-util")]
    pub fn with_connection(
        partition: Partition,
        connection: Arc<lore_transport::Connection>,
    ) -> Self {
        RemoteMutableStore {
            connections: protocol::RemoteConnections::with_connection(partition, connection),
            pools: Pools::default(),
        }
    }

    /// See [`RemoteConnections::connecting_over`](protocol::RemoteConnections::connecting_over).
    pub fn connecting_over(mut self, storage_connections: usize) -> Self {
        self.connections = self.connections.connecting_over(storage_connections);
        self
    }

    /// A session for ops on `partition`, as [`remote_session`] takes it.
    fn session(
        &self,
        partition: Partition,
    ) -> impl Future<Output = Result<Arc<StorageSession>, StoreError>> + '_ {
        remote_session(&self.connections, &self.pools, partition)
    }
}

#[async_trait]
impl store::MutableStore for RemoteMutableStore {
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    /// Keeps a session pool per partition for calls under `correlation_id`, as [`Pools`] does.
    fn hold_correlation(&self, correlation_id: &str) -> Option<lore_storage::CorrelationHold> {
        Some(Box::new(self.pools.hold(correlation_id)))
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let session = self.session(partition).await?;
        match session.mutable_load(&key, key_type).await {
            Err(err @ ProtocolError::NotFound(_)) => Err(missing_on_remote(
                Address::zero_context_hash(key),
                err,
                "key missing on remote",
            )),
            loaded => loaded.forward("Remote mutable load failed"),
        }
    }

    async fn list(
        self: Arc<Self>,
        _repository: Partition,
        _key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        Err(StoreError::internal("Store does not support operation"))
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        let session = self.session(partition).await?;
        session
            .mutable_store(key, value, key_type)
            .await
            .forward("Remote mutable store failed")
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let session = self.session(partition).await?;
        session
            .mutable_compare_and_swap(key, expected, value, key_type)
            .await
            .forward("Remote mutable CAS failed")
    }

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
        // Noop for remote store
        Ok(())
    }
}
