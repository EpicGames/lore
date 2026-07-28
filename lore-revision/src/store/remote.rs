// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_error_set::Internal;
use lore_error_set::prelude::*;
use lore_storage::immutable_store::sanitise_fragment_behavior_flags;
use lore_transport::Admin;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;
use tokio::sync::Mutex;

use super::StoreObliterateStats;
use crate::error::LoreResultExt;
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
use crate::store::StoreMatch;
use crate::store::StoreQueryResult;

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

    async fn connection(&self, repository: RepositoryId) -> Result<Arc<Connection>, StoreError> {
        let mut lock = self.connections.lock().await;
        if let Some(connection) = lock.get(&repository) {
            return Ok(connection.clone());
        }
        let connection = protocol::connect(
            self.remote_url.as_str(),
            self.identity.as_deref().unwrap_or_default(),
            repository,
        )
        .await
        .emit_map_err(Internal::msg(format!(
            "Unable to connect to remote store at {}",
            self.remote_url
        )))?;
        lock.insert(repository, connection.clone());
        Ok(connection)
    }

    pub async fn session(
        &self,
        repository: RepositoryId,
    ) -> Result<Arc<StorageSession>, StoreError> {
        let connection = self.connection(repository).await?;
        let correlation_id = execution_context().globals().correlation_id.to_string();
        connection
            .session(repository, &correlation_id)
            .await
            .emit_map_err(Internal::msg(format!(
                "Unable to create session to remote store at {}",
                self.remote_url
            )))
            .map_err(StoreError::from)
    }

    pub async fn admin(&self, repository: RepositoryId) -> Result<Arc<dyn Admin>, StoreError> {
        let mut lock = self.admin.lock().await;
        if let Some(connection) = lock.get(&repository) {
            connection
                .admin(repository)
                .await
                .emit_map_err(Internal::msg(format!(
                    "Unable to connect to remote store at {}",
                    self.remote_url
                )))
                .map_err(StoreError::from)
        } else {
            let connection = protocol::connect(
                self.remote_url.as_str(),
                self.identity.as_deref().unwrap_or_default(),
                repository,
            )
            .await
            .emit_map_err(Internal::msg(format!(
                "Unable to connect to remote store at {}",
                self.remote_url
            )))?;
            lock.insert(repository, connection.clone());
            connection
                .admin(repository)
                .await
                .emit_map_err(Internal::msg(format!(
                    "Unable to connect to remote store at {}",
                    self.remote_url
                )))
                .map_err(StoreError::from)
        }
    }
}

#[async_trait]
impl store::ImmutableStore for RemoteImmutableStore {
    async fn exist(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        _match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;
        let status = session.query(&[address]).await.unwrap_or_default();
        if !status.is_empty() {
            if status[0] == 0 {
                Ok(StoreMatch::MatchFull)
            } else if status[0] == 1 {
                Ok(StoreMatch::MatchHash)
            } else {
                Ok(StoreMatch::MatchNone)
            }
        } else {
            Ok(StoreMatch::MatchNone)
        }
    }

    async fn exist_batch(
        self: Arc<Self>,
        repository: Partition,
        addresses: &[Address],
        _match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;

        let bytes = session
            .query(addresses)
            .await
            .emit_map_err(Internal::msg("Remote store failed"))?;

        if bytes.len() != addresses.len() {
            lore_warn!(
                "Query returned incorrect number of results, expected {}, but got {}",
                addresses.len(),
                bytes.len()
            );
            return Err(StoreError::internal("Remote store failed"));
        }

        Ok(bytes
            .iter()
            .map(|byte| match byte {
                0 => StoreMatch::MatchFull,
                1 => StoreMatch::MatchHash,
                _ => StoreMatch::MatchNone,
            })
            .collect())
    }

    async fn query(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        _match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;
        let status = session.query(&[address]).await.unwrap_or_default();
        if !status.is_empty() && status[0] == 0 {
            let (fragment, _payload) = session
                .get(&address)
                .await
                .forward::<StoreError>("Remote store query failed")?;
            Ok(StoreQueryResult {
                fragment,
                match_made: StoreMatch::MatchFull,
            })
        } else {
            Ok(StoreQueryResult {
                fragment: Fragment::default(),
                match_made: StoreMatch::MatchNone,
            })
        }
    }

    async fn get(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        _match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;
        let (fragment, payload) = session
            .get(&address)
            .await
            .forward::<StoreError>("Remote store get failed")?;
        lore_storage::validate_fragment_payload(&fragment, payload.len())?;
        Ok((fragment, payload))
    }

    async fn put(
        self: Arc<Self>,
        repository: Partition,
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
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;
        session
            .put(address, fragment, payload)
            .await
            .forward("Remote store put failed")
    }

    async fn copy(
        self: Arc<Self>,
        source_repository: Partition,
        source_address: Address,
        destination_repository: Partition,
        destination_context: Context,
        // The remote service tracks durability on its own side; the local-flag bookkeeping that
        // `durable` controls happens in the local-store leg of a composite copy.
        _durable: bool,
    ) -> Result<(), StoreError> {
        let source_repository: RepositoryId = source_repository;
        let destination_repository: RepositoryId = destination_repository;
        let session = self.session(destination_repository).await?;
        session
            .copy(source_repository, source_address, destination_context)
            .await
            .forward("Remote copy failed")
    }

    async fn obliterate(
        self: Arc<Self>,
        repository: Partition,
        address: Address,
        _stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let repository: RepositoryId = repository;
        let admin = self.admin(repository).await?;
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

    async fn compact_stop(self: Arc<Self>) {}

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

    async fn session(&self, repository: RepositoryId) -> Result<Arc<StorageSession>, StoreError> {
        let mut lock = self.connections.lock().await;
        let connection = if let Some(connection) = lock.get(&repository) {
            connection.clone()
        } else {
            let connection = protocol::connect(
                self.remote_url.as_str(),
                self.identity.as_deref().unwrap_or_default(),
                repository,
            )
            .await
            .emit_map_err(Internal::msg(format!(
                "Unable to connect to remote store at {}",
                self.remote_url
            )))?;
            lock.insert(repository, connection.clone());
            connection
        };
        drop(lock);
        let correlation_id = execution_context().globals().correlation_id.to_string();
        connection
            .session(repository, &correlation_id)
            .await
            .emit_map_err(Internal::msg(format!(
                "Unable to create session to remote store at {}",
                self.remote_url
            )))
            .map_err(StoreError::from)
    }
}

#[async_trait]
impl store::MutableStore for RemoteMutableStore {
    async fn load(
        self: Arc<Self>,
        repository: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;
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
        repository: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;
        session
            .mutable_store(key, value, key_type)
            .await
            .forward("Remote mutable store failed")
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        repository: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let repository: RepositoryId = repository;
        let session = self.session(repository).await?;
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
