// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_error_set::prelude::*;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_transport::Admin;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;
use tokio::sync::Mutex;

use super::StoreObliterateStats;
use crate::errors::AddressNotFound;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Fragment;
use crate::lore::Hash;
use crate::lore::Partition;
use crate::lore::RepositoryId;
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

pub struct RemoteImmutableStore {
    /// Remote address
    remote_url: String,
    /// Identity
    identity: Option<String>,
    /// Cached connections
    connections: Mutex<HashMap<RepositoryId, Arc<Connection>>>,
    /// Cached admin connections
    admin: Mutex<HashMap<RepositoryId, Arc<Connection>>>,
}

impl RemoteImmutableStore {
    pub fn new(remote_url: &str, identity: Option<&str>) -> Self {
        RemoteImmutableStore {
            remote_url: remote_url.to_string(),
            identity: identity.map(|url| url.to_string()),
            connections: Mutex::new(HashMap::new()),
            admin: Mutex::new(HashMap::new()),
        }
    }

    async fn connection(&self, partition: Partition) -> Result<Arc<Connection>, StoreError> {
        let mut lock = self.connections.lock().await;
        if let Some(connection) = lock.get(&partition) {
            return Ok(connection.clone());
        }
        let connection = protocol::connect(
            self.remote_url.as_str(),
            self.identity.as_deref().unwrap_or_default(),
            partition,
        )
        .await
        .forward_with::<StoreError, _>(|| {
            format!("connecting to remote store at {}", self.remote_url)
        })?;
        lock.insert(partition, connection.clone());
        Ok(connection)
    }

    pub async fn session(&self, partition: Partition) -> Result<Arc<StorageSession>, StoreError> {
        let connection = self.connection(partition).await?;
        let correlation_id = execution_context().globals().correlation_id.to_string();
        connection
            .session(partition, &correlation_id)
            .await
            .forward_with(|| format!("creating session to remote store at {}", self.remote_url))
    }

    pub async fn admin(&self, partition: Partition) -> Result<Arc<dyn Admin>, StoreError> {
        let mut lock = self.admin.lock().await;
        if let Some(connection) = lock.get(&partition) {
            connection
                .admin(partition)
                .await
                .forward_with(|| format!("connecting to remote store at {}", self.remote_url))
        } else {
            let connection = protocol::connect(
                self.remote_url.as_str(),
                self.identity.as_deref().unwrap_or_default(),
                partition,
            )
            .await
            .forward_with::<StoreError, _>(|| {
                format!("connecting to remote store at {}", self.remote_url)
            })?;
            lock.insert(partition, connection.clone());
            connection
                .admin(partition)
                .await
                .forward_with(|| format!("connecting to remote store at {}", self.remote_url))
        }
    }
}

#[async_trait]
impl store::ImmutableStore for RemoteImmutableStore {
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

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let session = self.session(partition).await?;
        let (fragment, payload) = session
            .get(&address)
            .await
            .forward::<StoreError>("Remote store get failed")?;
        lore_storage::validate_fragment_payload(&fragment, payload.len())?;

        // The peer answers exact addresses only, so anything it served was a full match.
        Ok(StoreGetData {
            fragment,
            match_made: StoreMatch::MatchFull,
            partition,
            payload: Some(payload),
        })
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
        // The remote service tracks durability on its own side; the local-flag bookkeeping that
        // `durable` controls happens in the local-store leg of a composite copy.
        _durable: bool,
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
    /// Remote address
    remote_url: String,
    /// Identity
    identity: Option<String>,
    /// Cached connections
    connections: Mutex<HashMap<RepositoryId, Arc<Connection>>>,
}

impl RemoteMutableStore {
    pub fn new(remote_url: &str, identity: Option<&str>) -> Self {
        RemoteMutableStore {
            remote_url: remote_url.to_string(),
            identity: identity.map(|identity| identity.to_string()),
            connections: Mutex::new(HashMap::new()),
        }
    }

    async fn session(&self, partition: Partition) -> Result<Arc<StorageSession>, StoreError> {
        let mut lock = self.connections.lock().await;
        let connection = if let Some(connection) = lock.get(&partition) {
            connection.clone()
        } else {
            let connection = protocol::connect(
                self.remote_url.as_str(),
                self.identity.as_deref().unwrap_or_default(),
                partition,
            )
            .await
            .forward_with::<StoreError, _>(|| {
                format!("connecting to remote store at {}", self.remote_url)
            })?;
            lock.insert(partition, connection.clone());
            connection
        };
        drop(lock);
        let correlation_id = execution_context().globals().correlation_id.to_string();
        connection
            .session(partition, &correlation_id)
            .await
            .forward_with(|| format!("creating session to remote store at {}", self.remote_url))
    }
}

#[async_trait]
impl store::MutableStore for RemoteMutableStore {
    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let session = self.session(partition).await?;
        session
            .mutable_load(&key, key_type)
            .await
            .map_err(|err| match err {
                ProtocolError::NotFound(_) => {
                    StoreError::from(AddressNotFound::from(Address::zero_context_hash(key)))
                }
                other => StoreError::internal_with_context(other, "Remote mutable load failed"),
            })
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
