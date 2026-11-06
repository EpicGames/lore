// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_error_set::prelude::*;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_storage::read::heal_on_receipt;
use lore_storage::read::verify_receipt;
use lore_transport::Admin;
use lore_transport::PartitionSessions;
use lore_transport::ProtocolError;
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

/// An immutable store served by a remote over the storage protocol. The remote is an untrusted
/// source: a payload it serves is checked against its address on receipt.
pub struct RemoteImmutableStore {
    connections: protocol::RemoteConnections,
    /// The session pool this store holds for each partition, which its ops under one correlation
    /// id share.
    pools: PartitionSessions,
}

impl RemoteImmutableStore {
    pub fn new(remote_url: &str, identity: Option<&str>) -> Self {
        RemoteImmutableStore {
            connections: protocol::RemoteConnections::new(remote_url, identity),
            pools: PartitionSessions::default(),
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
            pools: PartitionSessions::default(),
        }
    }

    /// A session for ops on `partition`, as [`remote_session`] takes it.
    pub fn session(
        &self,
        partition: Partition,
    ) -> impl Future<Output = Result<Arc<StorageSession>, StoreError>> + '_ {
        remote_session(&self.connections, &self.pools, partition)
    }

    /// The remote's admin service for `partition`.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn admin(
        &self,
        partition: Partition,
    ) -> impl Future<Output = Result<Arc<dyn Admin>, StoreError>> + '_ {
        async move {
            self.connections
                .connection(partition)
                .await
                .forward_with::<StoreError, _>(|| {
                    format!(
                        "connecting to remote store at {}",
                        self.connections.remote_url()
                    )
                })?
                .admin(partition)
                .await
                .forward_with(|| {
                    format!(
                        "connecting to remote store at {}",
                        self.connections.remote_url()
                    )
                })
        }
    }
}

/// A session for ops on `partition`, from the pool `pools` holds under the call's correlation id,
/// started on the connection `connections` holds for the partition.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn remote_session<'a>(
    connections: &'a protocol::RemoteConnections,
    pools: &'a PartitionSessions,
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
        pools.hold(partition, pool.clone());
        Ok(pool.pick())
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

    /// Serves the payload as the remote stored it, with the content its check on receipt expanded
    /// ([`verify_receipt`]). A payload that fails the check is fetched again once where the remote
    /// heals it ([`heal_on_receipt`]). It answers `NotSupported` only for a payload this build
    /// cannot decode ([`unsupported_as_internal`]).
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
            let (fragment, payload) = session
                .get(&address)
                .await
                .forward::<StoreError>("Remote store get failed")
                .map_err(unsupported_as_internal)?;
            match verify_receipt(fragment, payload.clone(), address) {
                // The peer answers exact addresses only, so anything it served was a full match.
                Ok(content) => {
                    return Ok(StoreGetData {
                        fragment,
                        match_made: StoreMatch::MatchFull,
                        partition,
                        payload: Some(payload),
                        content: Some(content),
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
            Err(ProtocolError::NotFound(_)) => Err(AddressNotFound::from(address).into()),
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

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        // Noop for remote store
        Ok(())
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }
}

pub struct RemoteMutableStore {
    connections: protocol::RemoteConnections,
    /// The session pool this store holds for each partition, which its ops under one correlation
    /// id share.
    pools: PartitionSessions,
}

impl RemoteMutableStore {
    pub fn new(remote_url: &str, identity: Option<&str>) -> Self {
        RemoteMutableStore {
            connections: protocol::RemoteConnections::new(remote_url, identity),
            pools: PartitionSessions::default(),
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
            pools: PartitionSessions::default(),
        }
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

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let session = self.session(partition).await?;
        match session.mutable_load(&key, key_type).await {
            Err(ProtocolError::NotFound(_)) => Err(StoreError::from(AddressNotFound::from(
                Address::zero_context_hash(key),
            ))),
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

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        // Noop for remote store
        Ok(())
    }
}
