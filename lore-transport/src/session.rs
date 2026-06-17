// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::FutureExt;
use futures::future::BoxFuture;
use lore_base::lore_drain_tasks;
use lore_base::lore_spawn;
use lore_base::types::*;
use parking_lot::Mutex;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinSet;

use crate::connection::Connection;
use crate::error::ProtocolError;
use crate::traits::Storage;

/// A live session on a `Storage` connection. Provides all storage operations
/// scoped to a specific repository and correlation ID. Sends `session_stop`
/// to the server when the last reference is dropped.
///
/// A session may be constructed in one of two states:
/// - `Resolved`: the caller has already established the server-side session.
/// - `Pending`: the caller has everything needed to establish a session but
///   hasn't done so yet. The session is started lazily on the first operation
///   and cached for subsequent ones. This is how local-only command paths avoid
///   forcing the background connect to resolve.
pub struct StorageSession {
    inner: SessionInner,
}

struct ResolvedFields {
    storage: Arc<dyn Storage>,
    /// Keeps the connection alive while this session exists.
    #[allow(dead_code)]
    connection: Arc<Connection>,
    session_id: u32,
}

/// Closure signature for a pending session's resolver. The resolver runs at most
/// once and returns an eager `Arc<StorageSession>` (typically obtained by calling
/// `Connection::session` after awaiting the caller's pending connection).
type PendingResolver =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Arc<StorageSession>, ProtocolError>> + Send + Sync>;

type ResolvedSlot = Arc<TokioMutex<Option<Result<Arc<StorageSession>, ProtocolError>>>>;

enum SessionInner {
    Resolved(ResolvedFields),
    Pending {
        resolver: PendingResolver,
        /// Resolved session, lazily populated by the resolver. A `Mutex<Option<_>>`
        /// rather than a `OnceCell` so that `StorageSession::invalidate` can drop
        /// the cached resolution and force a fresh `session_start` on the next
        /// operation — needed when a QUIC reconnect has invalidated the
        /// server-side session map (the same connection-id is gone, so our
        /// `session_id` is unknown on the new connection).
        resolved: ResolvedSlot,
    },
}

impl StorageSession {
    /// Construct an already-resolved session. Used by the connection internals
    /// after a successful `session_start` RPC.
    pub(crate) fn resolved(
        storage: Arc<dyn Storage>,
        connection: Arc<Connection>,
        session_id: u32,
    ) -> Self {
        Self {
            inner: SessionInner::Resolved(ResolvedFields {
                storage,
                connection,
                session_id,
            }),
        }
    }

    /// Construct a session whose server-side session will be started on the
    /// first operation. The resolver is called at most once; subsequent
    /// operations use the cached resolved session. Typical use: defer the
    /// underlying remote connect and session creation until actually needed.
    ///
    /// The resolver returns an eager `Arc<StorageSession>` — callers obtain
    /// this by awaiting their pending connection and invoking
    /// `Connection::session`. That makes the lazy session transparently share
    /// the connection's session dedup cache.
    pub fn pending<F, Fut>(resolver: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Arc<Self>, ProtocolError>> + Send + 'static,
    {
        Self {
            inner: SessionInner::Pending {
                resolver: Arc::new(move || resolver().boxed()),
                resolved: Arc::new(TokioMutex::new(None)),
            },
        }
    }

    /// Drop any cached server-side session. The next operation re-runs the
    /// resolver, triggering a fresh `session_start` against the current
    /// connection. Also clears the parent `Connection`'s session pool cache
    /// so the rebuild observes a clean slate (no stale `Arc<SessionPool>`
    /// keeping dead session-ids alive). Call this after the transport
    /// surfaces a `NotConnected`/`Failed` server response indicating the
    /// session-id is no longer known server-side.
    pub async fn invalidate(&self) {
        match &self.inner {
            SessionInner::Resolved(r) => {
                r.connection.invalidate_all_sessions();
            }
            SessionInner::Pending { resolved, .. } => {
                let mut guard = resolved.lock().await;
                // Bubble the connection invalidation through the cached
                // eager session if one resolved, so the next resolver call
                // is the only thing the parent `Connection` has on file.
                if let Some(Ok(inner)) = guard.as_ref()
                    && let SessionInner::Resolved(r) = &inner.inner
                {
                    r.connection.invalidate_all_sessions();
                }
                *guard = None;
            }
        }
    }

    /// Get the resolved `(storage, session_id)` pair, driving the pending
    /// resolver on first call. All operation methods go through here.
    async fn ensure(&self) -> Result<(Arc<dyn Storage>, u32), ProtocolError> {
        match &self.inner {
            SessionInner::Resolved(r) => Ok((r.storage.clone(), r.session_id)),
            SessionInner::Pending { resolver, resolved } => {
                // Single-writer initialization: the lock both serialises
                // resolver calls and gates the slot against concurrent
                // `invalidate()` resetting it back to `None`.
                let inner = {
                    let mut guard = resolved.lock().await;
                    if guard.is_none() {
                        *guard = Some(resolver().await);
                    }
                    match guard.as_ref().expect("just populated") {
                        Ok(session) => session.clone(),
                        Err(err) => return Err(err.clone()),
                    }
                };
                // The resolver always produces an eager session, so reach
                // directly into its fields without recursing.
                match &inner.inner {
                    SessionInner::Resolved(r) => Ok((r.storage.clone(), r.session_id)),
                    SessionInner::Pending { .. } => {
                        Err(ProtocolError::internal("nested pending session"))
                    }
                }
            }
        }
    }

    pub async fn get(&self, address: &Address) -> Result<(Fragment, Bytes), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.get(session_id, address).await
    }

    pub async fn get_priority(
        &self,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.get_priority(session_id, address).await
    }

    pub async fn put(
        &self,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.put(session_id, address, fragment, payload).await
    }

    pub async fn query(&self, address: &[Address]) -> Result<Bytes, ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.query(session_id, address).await
    }

    pub async fn verify(
        &self,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.verify(session_id, address, heal).await
    }

    pub async fn copy(
        &self,
        source_repository: RepositoryId,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage
            .copy(
                session_id,
                source_repository,
                source_address,
                target_context,
            )
            .await
    }

    /// Fetch only fragment metadata (`flags`, `size_payload`, `size_content`) for `address`.
    /// The wire request is identical to `get`; the server's response carries no payload bytes.
    /// Use this when the caller needs metadata without paying the payload transfer cost — e.g.
    /// the storage API's `query` op for remote-hit metadata lookups.
    pub async fn get_metadata(&self, address: &Address) -> Result<Fragment, ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.get_metadata(session_id, address).await
    }

    pub async fn mutable_load(&self, key: &Hash, key_type: KeyType) -> Result<Hash, ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.mutable_load(session_id, key, key_type).await
    }

    pub async fn mutable_store(
        &self,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage
            .mutable_store(session_id, key, value, key_type)
            .await
    }

    pub async fn mutable_compare_and_swap(
        &self,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage
            .mutable_compare_and_swap(session_id, key, expected, value, key_type)
            .await
    }
}

impl Drop for StorageSession {
    fn drop(&mut self) {
        // Only the Resolved variant owns a server-side session directly. A
        // Pending variant that never resolved has nothing to stop. A Pending
        // variant that did resolve delegates: the inner Arc<StorageSession>
        // in the OnceCell has its own Drop that fires session_stop when its
        // refcount reaches zero.
        if let SessionInner::Resolved(r) = &self.inner {
            let storage = r.storage.clone();
            let session_id = r.session_id;
            lore_base::lore_spawn!(async move {
                let _ = storage.session_stop(session_id).await;
            });
        }
    }
}

/// A pool of `StorageSession`s for a single `(repository, correlation_id)`
/// tuple. Holds one session per underlying `Storage` connection, plus a
/// round-robin counter so successive `pick()` calls spread load across all
/// connections in the pool.
pub struct SessionPool {
    sessions: Vec<Arc<StorageSession>>,
    next: AtomicUsize,
}

impl SessionPool {
    /// Returns the next session in the pool via round-robin.
    pub fn pick(&self) -> Arc<StorageSession> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.sessions.len();
        self.sessions[index].clone()
    }
}

/// Tracks a pool entry in the connector's `DashMap`. Mirrors the previous
/// per-session bookkeeping but at pool granularity: when the pool's strong
/// refcount reaches zero (caller releases the pin in `Connection::session_cache`),
/// the `Weak` becomes unupgradeable and the next access rebuilds the pool.
struct PoolEntry {
    /// Weak ref to the live pool. If upgradeable, every session it owns is in use.
    pool: Weak<SessionPool>,
    /// Server-assigned session IDs aligned with `storages`, for sending
    /// `session_stop` if this entry is replaced.
    session_ids: Vec<u32>,
    /// Weak refs to the storages each session was started on, aligned with
    /// `session_ids`. If a `Weak` no longer upgrades, the connection is gone
    /// and the server already cleaned up -- no stop needed.
    storages: Vec<Weak<dyn Storage>>,
}

/// Result of the synchronous `DashMap` entry check in `StorageConnector::session()`.
enum PoolOutcome {
    /// We inserted into a vacant slot -- we own this pool.
    Inserted { pool: Arc<SessionPool> },
    /// We replaced an expired entry -- we own the new pool, must stop the old sessions.
    Replaced {
        pool: Arc<SessionPool>,
        old_session_ids: Vec<u32>,
        old_storages: Vec<Weak<dyn Storage>>,
    },
    /// Another task won the race -- use the winner, stop our server-side sessions.
    RaceLost { winner: Arc<SessionPool> },
}

/// Owns a pool of Storage connections and manages session lifecycle with
/// deduplication, round-robin connection assignment, and automatic cleanup.
///
/// Each `(repository, correlation_id)` maps to a `SessionPool` containing one
/// `StorageSession` per underlying `Storage` connection. Operations on a
/// returned session round-robin across the pool so a single command spreads
/// load over every connection set up in the connect phase.
pub struct StorageConnector {
    connections: Vec<Arc<dyn Storage>>,
    counter: AtomicUsize,
    pools: dashmap::DashMap<(RepositoryId, String), PoolEntry>,
    /// Partitions for which `session_start` has already succeeded on every underlying
    /// `Storage`. Tracks the server-side `authorized_repos` state — once a partition is
    /// registered here, the server keeps it in `authorized_repos` for the connection's
    /// lifetime regardless of `session_stop`, so subsequent ops for the same partition can
    /// skip the `session_start` round-trip purely for authorization.
    ///
    /// The set is per-`StorageConnector`, which matches the server scoping: one
    /// `StorageServiceV4` instance (and its `SessionMap`) per accepted connection. When the
    /// owning `Connection` drops, the connector goes with it and the set resets.
    authorized_partitions: dashmap::DashSet<RepositoryId>,
}

impl StorageConnector {
    pub fn new(connections: Vec<Arc<dyn Storage>>) -> Self {
        Self {
            connections,
            counter: AtomicUsize::new(0),
            pools: dashmap::DashMap::new(),
            authorized_partitions: dashmap::DashSet::new(),
        }
    }

    /// Whether the given partition has previously had `session_start` succeed on every
    /// underlying `Storage` for this connector. A `true` answer means the server's
    /// `authorized_repos` set already contains the partition and a fresh `session_start`
    /// purely for authorization is unnecessary.
    pub fn is_partition_authorized(&self, partition: RepositoryId) -> bool {
        self.authorized_partitions.contains(&partition)
    }

    /// Get or create a `SessionPool` for the given repository and correlation ID,
    /// returning a round-robin-picked session from it along with the pool itself
    /// so the caller can pin the pool to keep all its sessions alive across
    /// multiple operations within a command.
    ///
    /// On a miss, one server-side session is started per underlying connection,
    /// in parallel. Race resolution mirrors the previous single-session path:
    /// the first writer wins (vacant or expired entry); a losing racer stops
    /// every server-side session it just started.
    pub async fn session(
        &self,
        repository: RepositoryId,
        correlation_id: &str,
        connection: Arc<Connection>,
    ) -> Result<(Arc<StorageSession>, Arc<SessionPool>), ProtocolError> {
        let key = (repository, correlation_id.to_string());

        // Fast path: live pool exists.
        if let Some(entry) = self.pools.get(&key)
            && let Some(pool) = entry.pool.upgrade()
        {
            let picked = pool.pick();
            return Ok((picked, pool));
        }

        // Slow path: start one session per connection in parallel. No lock held.
        let started = Arc::new(Mutex::new(Vec::with_capacity(self.connections.len())));
        let mut tasks = JoinSet::new();
        for storage in self.connections.iter().cloned() {
            let correlation_id = correlation_id.to_string();
            let started = started.clone();
            lore_spawn!(tasks, async move {
                let session_id = storage.session_start(repository, &correlation_id).await?;
                started.lock().push((storage, session_id));
                Ok::<_, ProtocolError>(())
            });
        }
        lore_drain_tasks!(
            tasks,
            ProtocolError::internal("session_start task join failure")
        )?;
        let Ok(started) = Arc::try_unwrap(started) else {
            unreachable!("session_start tasks dropped their Arc<Mutex<_>> clones");
        };
        let started: Vec<(Arc<dyn Storage>, u32)> = started.into_inner();

        // session_start succeeded on every connection in parallel above; the partition is now
        // in `authorized_repos` of every server-side `SessionMap` for the pool. Even on the
        // race-loser path below (which stops these sessions to defer to the winner), the
        // server keeps the partition in `authorized_repos` permanently — `session_stop` only
        // touches the per-session map, not the authorization set. So this is the right point
        // to mark the partition as authorized for any future fast-path query.
        self.authorized_partitions.insert(repository);

        // Build the pool with strong refs to every session.
        let sessions: Vec<Arc<StorageSession>> = started
            .iter()
            .map(|(storage, session_id)| {
                Arc::new(StorageSession::resolved(
                    storage.clone(),
                    connection.clone(),
                    *session_id,
                ))
            })
            .collect();
        let pool = Arc::new(SessionPool {
            sessions,
            next: AtomicUsize::new(0),
        });
        let session_ids: Vec<u32> = started.iter().map(|(_, id)| *id).collect();
        let storages: Vec<Weak<dyn Storage>> =
            started.iter().map(|(s, _)| Arc::downgrade(s)).collect();

        // Try to insert under the entry lock (synchronous only -- no .await).
        let outcome = {
            #[allow(clippy::disallowed_methods)]
            // Synchronous entry check; no await while lock is held.
            let entry = self.pools.entry(key);
            match entry {
                dashmap::mapref::entry::Entry::Occupied(mut e) => {
                    if let Some(alive) = e.get().pool.upgrade() {
                        // Race loser -- another task won while we were starting sessions.
                        PoolOutcome::RaceLost { winner: alive }
                    } else {
                        // Expired entry -- take old info for cleanup, replace with ours.
                        let old_session_ids = std::mem::take(&mut e.get_mut().session_ids);
                        let old_storages = std::mem::take(&mut e.get_mut().storages);
                        e.insert(PoolEntry {
                            pool: Arc::downgrade(&pool),
                            session_ids: session_ids.clone(),
                            storages: storages.clone(),
                        });
                        PoolOutcome::Replaced {
                            pool: pool.clone(),
                            old_session_ids,
                            old_storages,
                        }
                    }
                }
                dashmap::mapref::entry::Entry::Vacant(v) => {
                    v.insert(PoolEntry {
                        pool: Arc::downgrade(&pool),
                        session_ids: session_ids.clone(),
                        storages: storages.clone(),
                    });
                    PoolOutcome::Inserted { pool: pool.clone() }
                }
            }
        }; // entry lock released here

        match outcome {
            PoolOutcome::Inserted { pool } => {
                let picked = pool.pick();
                Ok((picked, pool))
            }
            PoolOutcome::Replaced {
                pool,
                old_session_ids,
                old_storages,
            } => {
                // Stop expired sessions outside the lock.
                for (id, storage) in old_session_ids.into_iter().zip(old_storages) {
                    if let Some(storage) = storage.upgrade() {
                        let _ = storage.session_stop(id).await;
                    }
                }
                let picked = pool.pick();
                Ok((picked, pool))
            }
            PoolOutcome::RaceLost { winner } => {
                // Stop every server-side session we just started -- the winner owns this key.
                for (id, storage) in session_ids.into_iter().zip(storages) {
                    if let Some(storage) = storage.upgrade() {
                        let _ = storage.session_stop(id).await;
                    }
                }
                let picked = winner.pick();
                Ok((picked, winner))
            }
        }
    }

    /// Direct access to the underlying connections.
    pub fn connections(&self) -> &[Arc<dyn Storage>] {
        &self.connections
    }

    /// Returns the next connection index via round-robin.
    pub fn next_connection_index(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed) % self.connections.len()
    }

    /// Gracefully close every underlying storage connection, draining in-flight
    /// streams before sending the transport close frame.
    pub async fn close_all(&self) {
        for storage in &self.connections {
            storage.close().await;
        }
    }
}
