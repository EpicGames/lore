// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `RemoteEndpoint` — connection-pool wrapper used by the storage API to drive remote-side
//! ops without depending on `lore_revision::store::remote::RemoteImmutableStore`.
//!
//! Holds only what the storage API actually needs: its connections to the remote and the
//! session pools it holds, per partition. Construction does not open the wire — connections are
//! established lazily by the first call that needs one. Each `lore_storage_open` with a
//! `remote_config` builds its own `RemoteEndpoint` on `StoreInternal`, over connections from the
//! process-wide connection cache that every handle shares.

use std::sync::Arc;

use lore_revision::lore::RepositoryId;
use lore_revision::protocol;
use lore_transport::Admin;
use lore_transport::Connection;
use lore_transport::PartitionSessions;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;

/// Lazily-resolving handle to a peer storage service. One instance per open handle that has a
/// remote configured. Internally caches one `Arc<Connection>` per partition, which every op on
/// that partition shares.
#[lore_macro::test_pub]
pub(crate) struct RemoteEndpoint {
    connections: protocol::RemoteConnections,
    /// The session pool this handle holds for each partition, which its calls under one
    /// correlation id share until a call under another replaces it or the handle closes.
    pub pools: PartitionSessions,
}

impl RemoteEndpoint {
    #[lore_macro::test_pub]
    pub(crate) fn new(remote_url: impl Into<String>, identity: Option<&str>) -> Self {
        Self {
            connections: protocol::RemoteConnections::new(remote_url, identity),
            pools: PartitionSessions::default(),
        }
    }

    /// The connection for ops on `partition`, as [`protocol::RemoteConnections::connection`]
    /// hands it out.
    #[lore_macro::test_pub]
    pub(crate) fn connection(
        &self,
        partition: RepositoryId,
    ) -> impl Future<Output = Result<Arc<Connection>, ProtocolError>> + '_ {
        self.connections.connection(partition)
    }

    /// Resolve a `StorageSession` bound to `partition`, over the pool [`pools`](Self::pools)
    /// holds for the partition under the call's correlation id, started on first use and again
    /// once its connection is marked stale. The pool is not pinned on the connection, so its
    /// server-side sessions end once the handle and every call drop it.
    pub(crate) async fn session(
        &self,
        partition: RepositoryId,
    ) -> Result<Arc<StorageSession>, ProtocolError> {
        let correlation_id = lore_revision::lore::execution_context()
            .globals()
            .correlation_id
            .to_string();
        if let Some(pool) = self.pools.get_live(partition, &correlation_id) {
            return Ok(Arc::new(StorageSession::pooled(pool)));
        }
        let connection = self.connection(partition).await?;
        let pool = connection
            .unpinned_session_pool(partition, &correlation_id)
            .await?;
        self.pools.hold(partition, pool.clone());
        Ok(Arc::new(StorageSession::pooled(pool)))
    }

    /// Ensure `partition` is in the server's `authorized_repos` set on the underlying
    /// connection without leaving a session pinned. Fast-paths via the connector's
    /// `authorized_partitions` cache: if a previous successful session for `partition`
    /// has already registered the authz, no wire call happens.
    pub(crate) async fn ensure_authorized(
        &self,
        partition: RepositoryId,
    ) -> Result<(), ProtocolError> {
        let connection = self.connection(partition).await?;
        let correlation_id = lore_revision::lore::execution_context()
            .globals()
            .correlation_id
            .to_string();
        connection
            .ensure_partition_authorized(partition, &correlation_id)
            .await
    }

    /// Resolve an `Admin` handle for `partition` — used by ops like obliterate that go
    /// through the admin verb rather than a per-session storage verb.
    pub(crate) async fn admin(
        &self,
        partition: RepositoryId,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        let connection = self.connection(partition).await?;
        connection.admin(partition).await
    }
}
