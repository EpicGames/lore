// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use crossbeam::epoch;
use crossbeam::epoch::Atomic;
use crossbeam::epoch::Owned;
use crossbeam::epoch::Shared;
use futures::FutureExt;
use futures::future::BoxFuture;
use lore_base::lore_drain_tasks;
use lore_base::lore_spawn_net;
use lore_base::types::*;
use parking_lot::Mutex;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinSet;

use crate::connection::Connection;
use crate::error::ProtocolError;
use crate::traits::Storage;

/// A live session on a `Storage` connection. Provides all storage operations
/// scoped to a specific partition and correlation ID. Sends `session_stop`
/// to the server when the last reference is dropped.
///
/// A session may be constructed in one of three states:
/// - `Resolved`: the caller has already established the server-side session.
/// - `Pooled`: each operation runs on the next session of a [`SessionPool`].
/// - `Pending`: the caller has everything needed to establish a session but
///   hasn't done so yet. The session is started lazily on the first operation
///   and cached for subsequent ones. This is how local-only command paths avoid
///   forcing the background connect to resolve.
pub struct StorageSession {
    inner: SessionInner,
}

struct ResolvedFields {
    storage: Arc<dyn Storage>,
    /// Keeps the connection alive while this session exists, and is what a source partition is
    /// authorized on — authorization is per connection, not per session.
    connection: Arc<Connection>,
    session_id: u32,
    /// The partition this session was started for, so a copy naming it as its source needs no
    /// authorization beyond the session itself.
    partition: Partition,
    /// The correlation id the session was started under, so authorizing a further partition on this
    /// connection is attributed to the same command.
    correlation_id: Arc<str>,
}

/// Closure signature for a pending session's resolver. Returns an eager or a pooled
/// `Arc<StorageSession>`, typically the pooled one `Connection::session` returns once the
/// caller's pending connection resolves. Runs once per resolution, not once per session: a
/// throttled or invalidated resolution is asked again.
type PendingResolver =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Arc<StorageSession>, ProtocolError>> + Send + Sync>;

enum SessionInner {
    Resolved(ResolvedFields),
    Pooled(Arc<SessionPool>),
    Pending {
        resolver: PendingResolver,
        /// The session the resolver produced, read by every operation without a lock. Null
        /// before the first resolution and after an [`invalidate`](StorageSession::invalidate),
        /// which forces a fresh `session_start` on the next operation — needed when a QUIC
        /// reconnect has invalidated the server-side session map (the same connection-id is
        /// gone, so our `session_id` is unknown on the new connection). Written only under
        /// `resolution`.
        current: Atomic<Arc<StorageSession>>,
        /// Serialises resolutions and invalidations, and holds a failed resolution's error.
        ///
        /// A throttled `session_start` is not held. `SlowDown` is the one failure the retrying
        /// callers answer with back-off instead of `invalidate`, so a held one would be served
        /// to every later attempt and the retry would spend its whole schedule without ever
        /// reaching the server. Every other failure is held, so the rest of a batch sharing the
        /// session fails without repeating a `session_start` that cannot succeed.
        resolution: TokioMutex<Option<ProtocolError>>,
    },
}

impl StorageSession {
    /// Construct an already-resolved session. Used by the connection internals
    /// after a successful `session_start` RPC.
    #[lore_macro::test_pub]
    pub(crate) fn resolved(
        storage: Arc<dyn Storage>,
        connection: Arc<Connection>,
        session_id: u32,
        partition: Partition,
        correlation_id: Arc<str>,
    ) -> Self {
        Self {
            inner: SessionInner::Resolved(ResolvedFields {
                storage,
                connection,
                session_id,
                partition,
                correlation_id,
            }),
        }
    }

    /// Construct a session whose server-side session will be started on the
    /// first operation. Subsequent operations use the resolved session; the
    /// resolver runs again only after an [`invalidate`](Self::invalidate) or a
    /// throttled `session_start`. Typical use: defer the underlying remote
    /// connect and session creation until actually needed.
    ///
    /// The resolver returns an eager or a pooled `Arc<StorageSession>`. Callers obtain a pooled
    /// one by awaiting their pending connection and invoking `Connection::session`, which makes
    /// the lazy session share the connection's session dedup cache.
    pub fn pending<F, Fut>(resolver: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Arc<StorageSession>, ProtocolError>>
            + Send
            + 'static,
    {
        Self {
            inner: SessionInner::Pending {
                resolver: Arc::new(move || resolver().boxed()),
                current: Atomic::null(),
                resolution: TokioMutex::new(None),
            },
        }
    }

    /// A session over every session in `pool`: each operation runs on the next one in turn, so
    /// the operations of one holder spread over every connection the pool spans.
    pub fn pooled(pool: Arc<SessionPool>) -> Self {
        Self {
            inner: SessionInner::Pooled(pool),
        }
    }

    /// Whether the server-side session is established on first use rather than
    /// held already.
    ///
    /// Only a lazy session survives an [`invalidate`](Self::invalidate): the next
    /// operation on it re-runs the resolver and obtains a `session_id` the server
    /// knows about. An eager one keeps the id it was built with, so a caller that
    /// invalidates and retries the same session has to hold a lazy one.
    pub fn is_lazy(&self) -> bool {
        matches!(self.inner, SessionInner::Pending { .. })
    }

    /// Drop any cached server-side session. The next operation re-runs the
    /// resolver, triggering a fresh `session_start` against the current
    /// connection. Also marks the pool the session runs over stale and clears
    /// the parent `Connection`'s session pool cache, so no resolution hands out
    /// a pool holding session ids the server may not know. Call this after the
    /// transport surfaces a `NotConnected`/`Failed` server response indicating
    /// the session-id is no longer known server-side.
    pub async fn invalidate(&self) {
        match &self.inner {
            SessionInner::Pending {
                current,
                resolution,
                ..
            } => {
                let mut failure = resolution.lock().await;
                let guard = epoch::pin();
                let resolved = current.swap(Shared::null(), Ordering::AcqRel, &guard);
                // SAFETY: published by `resolve_pending` and freed only here, deferred past every
                // reader pinned when it was unlinked.
                if let Some(inner) = unsafe { resolved.as_ref() } {
                    inner.invalidate_connection_sessions();
                    unsafe { guard.defer_destroy(resolved) };
                    guard.flush();
                }
                *failure = None;
            }
            _ => self.invalidate_connection_sessions(),
        }
    }

    /// Holds a pending session's resolution lock until the guard drops; `None` for any other.
    #[cfg(feature = "test-util")]
    pub async fn hold_resolution(
        &self,
    ) -> Option<tokio::sync::MutexGuard<'_, Option<ProtocolError>>> {
        match &self.inner {
            SessionInner::Pending { resolution, .. } => Some(resolution.lock().await),
            _ => None,
        }
    }

    /// Clears the session pool cache of the `Connection` each session this one runs on belongs
    /// to, and marks a pool this session runs over stale, so no resolution serves it again.
    fn invalidate_connection_sessions(&self) {
        match &self.inner {
            SessionInner::Resolved(r) => r.connection.invalidate_all_sessions(),
            SessionInner::Pooled(pool) => {
                pool.mark_stale();
                for session in &pool.sessions {
                    session.invalidate_connection_sessions();
                }
            }
            SessionInner::Pending { .. } => {}
        }
    }

    /// Read from the resolved session, driving the pending resolver on first call. Every method
    /// needing the server-side session goes through here, so one resolution serves whatever is
    /// asked of a pending session. A pooled session reads from the next session of its pool when
    /// `turn`, and from its first otherwise; a query that only reads a session's fields takes no
    /// turn, so it leaves the operations visiting every session in turn.
    ///
    /// A pending session once resolved is read without a lock ([`project_current`]).
    async fn with_resolved<T>(
        &self,
        turn: bool,
        project: impl FnOnce(&ResolvedFields) -> T,
    ) -> Result<T, ProtocolError> {
        let SessionInner::Pending {
            resolver,
            current,
            resolution,
        } = &self.inner
        else {
            return self.project_member(turn, project).await;
        };
        let project = match project_current(current, turn, project) {
            Ok(answer) => return Ok(answer),
            Err(project) => project,
        };
        resolve_pending(resolver, current, resolution)
            .await?
            .project_member(turn, project)
            .await
    }

    /// `project` applied to the fields of this session or of its pool's
    /// [`member`](SessionPool::member) for `turn`. Only a pool built outside the connector holds
    /// pending sessions, so resolving one is boxed and stays out of every operation's future.
    async fn project_member<T>(
        &self,
        turn: bool,
        project: impl FnOnce(&ResolvedFields) -> T,
    ) -> Result<T, ProtocolError> {
        let member = match &self.inner {
            SessionInner::Resolved(r) => return Ok(project(r)),
            SessionInner::Pooled(pool) => pool.member(turn),
            SessionInner::Pending { .. } => {
                return Err(ProtocolError::internal("nested pending session"));
            }
        };
        match &member.inner {
            SessionInner::Resolved(r) => Ok(project(r)),
            SessionInner::Pending {
                resolver,
                current,
                resolution,
            } => {
                let inner = Box::pin(resolve_pending(resolver, current, resolution)).await?;
                match &inner.inner {
                    SessionInner::Resolved(r) => Ok(project(r)),
                    _ => Err(ProtocolError::internal("nested session")),
                }
            }
            SessionInner::Pooled(_) => Err(ProtocolError::internal("nested pooled session")),
        }
    }

    /// Get the resolved `(storage, session_id)` pair, driving the pending
    /// resolver on first call. All operation methods go through here.
    async fn ensure(&self) -> Result<(Arc<dyn Storage>, u32), ProtocolError> {
        self.with_resolved(true, |r| (r.storage.clone(), r.session_id))
            .await
    }

    /// The partition this session is scoped to, driving the pending resolver on first call.
    pub async fn partition(&self) -> Result<Partition, ProtocolError> {
        self.with_resolved(false, |r| r.partition).await
    }

    /// Whether a [`StorageSession::copy`] on this session may name `partition` as its source.
    ///
    /// The session's own partition always may. Any other has to be authorized on the connection,
    /// which is the scope the server checks a copy's source against; that costs one `session_start`
    /// the first time and nothing afterwards. Answers `false` rather than erroring because a caller
    /// asking this is choosing whether to name a source at all, and trades a copy the server would
    /// refuse for a cached lookup.
    pub async fn can_copy_from(&self, partition: Partition) -> bool {
        let Ok((connection, own, correlation_id)) = self
            .with_resolved(false, |r| {
                (r.connection.clone(), r.partition, r.correlation_id.clone())
            })
            .await
        else {
            return false;
        };
        if own == partition {
            return true;
        }
        connection
            .ensure_partition_authorized(partition, &correlation_id)
            .await
            .is_ok()
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
        source_partition: Partition,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage
            .copy(session_id, source_partition, source_address, target_context)
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

    /// `mutable_load` + `get` in one round trip, always reading the key as
    /// [`KeyType::Resolve`]. Returns `(resolved_hash, fragment, payload)`.
    /// `flags` is a `get_resolved_flags` bitmask; 0 for default behaviour.
    pub async fn get_resolved(
        &self,
        key: &Hash,
        context: &Context,
        flags: u32,
    ) -> Result<(Hash, Fragment, Bytes), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage.get_resolved(session_id, key, context, flags).await
    }

    /// `put` + `mutable_store` in one round trip: store the fragment, then map `key` to
    /// `address.hash` under [`KeyType::Resolve`]. The write side of [`Self::get_resolved`].
    pub async fn put_resolved(
        &self,
        key: &Hash,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        let (storage, session_id) = self.ensure().await?;
        storage
            .put_resolved(session_id, key, address, fragment, payload)
            .await
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

/// `project` applied without a lock to the fields of the session `current` holds resolved, or of
/// its pool's [`member`](SessionPool::member) for `turn` where every session of the pool is
/// resolved. Hands `project` back otherwise.
fn project_current<T, F: FnOnce(&ResolvedFields) -> T>(
    current: &Atomic<Arc<StorageSession>>,
    turn: bool,
    project: F,
) -> Result<T, F> {
    let guard = epoch::pin();
    // SAFETY: published by `resolve_pending` and freed only by `invalidate`, deferred past every
    // reader pinned when it was unlinked.
    let Some(session) = (unsafe { current.load(Ordering::Acquire, &guard).as_ref() }) else {
        return Err(project);
    };
    let member = match &session.inner {
        SessionInner::Pooled(pool) if pool.resolved => pool.member(turn),
        _ => session,
    };
    match &member.inner {
        SessionInner::Resolved(r) => Ok(project(r)),
        _ => Err(project),
    }
}

/// The session a pending session resolves to: the one another call published while this one
/// waited for `resolution`, the failure it holds, or what the resolver answers now, published to
/// `current` on success.
async fn resolve_pending(
    resolver: &PendingResolver,
    current: &Atomic<Arc<StorageSession>>,
    resolution: &TokioMutex<Option<ProtocolError>>,
) -> Result<Arc<StorageSession>, ProtocolError> {
    let mut failure = resolution.lock().await;
    {
        let guard = epoch::pin();
        // SAFETY: as in `project_current`.
        if let Some(session) = unsafe { current.load(Ordering::Acquire, &guard).as_ref() } {
            return Ok(session.clone());
        }
    }
    if let Some(err) = failure.as_ref() {
        return Err(err.clone());
    }
    match resolver().await {
        Ok(session) => {
            current.store(Owned::new(session.clone()), Ordering::Release);
            Ok(session)
        }
        Err(err) => {
            if !err.is_slow_down() {
                *failure = Some(err.clone());
            }
            Err(err)
        }
    }
}

impl Drop for StorageSession {
    fn drop(&mut self) {
        // Only the Resolved variant owns a server-side session directly. A
        // Pooled variant's sessions and a resolved Pending variant's session
        // each stop their own when their last reference drops.
        match &mut self.inner {
            SessionInner::Resolved(r) => {
                let storage = r.storage.clone();
                let session_id = r.session_id;
                lore_base::lore_spawn_net!(async move {
                    let _ = storage.session_stop(session_id).await;
                });
            }
            SessionInner::Pending { current, .. } => {
                // SAFETY: `drop` holds the only reference, so no reader is pinned on it.
                drop(unsafe { std::mem::take(current).try_into_owned() });
            }
            SessionInner::Pooled(_) => {}
        }
    }
}

/// A pool of `StorageSession`s for a single `(partition, correlation_id)`
/// tuple. Holds one session per underlying `Storage` connection, plus a
/// round-robin counter: a [`StorageSession::pooled`] session runs each operation
/// on the next session, and [`pick`](Self::pick) hands out the next one for a
/// single unit of work.
pub struct SessionPool {
    sessions: Vec<Arc<StorageSession>>,
    /// Whether every session is resolved, so an operation reads its turn's fields without
    /// awaiting a resolution.
    resolved: bool,
    next: AtomicUsize,
    /// Set when a session over this pool is invalidated: its session ids may be unknown to the
    /// server, so no resolution hands the pool out again, though its holders keep it.
    stale: AtomicBool,
}

impl SessionPool {
    /// A pool over `sessions`, taken in turn.
    ///
    /// The connector builds one session per underlying `Storage` connection, so taking them in
    /// turn spreads a command's operations over every connection the connect phase established.
    pub fn new(sessions: Vec<Arc<StorageSession>>) -> Self {
        Self {
            resolved: sessions
                .iter()
                .all(|session| matches!(session.inner, SessionInner::Resolved(_))),
            sessions,
            next: AtomicUsize::new(0),
            stale: AtomicBool::new(false),
        }
    }

    /// Whether a session over this pool was invalidated since it was built.
    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_stale(&self) {
        self.stale.store(true, Ordering::Relaxed);
    }

    /// Returns the next session in the pool via round-robin.
    ///
    /// Every call advances the cursor, so only a caller that is going to use the
    /// session picks. One that discards what it picked makes every other caller
    /// stride over the connections rather than visit each in turn.
    pub fn pick(&self) -> Arc<StorageSession> {
        self.sessions[self.next_index()].clone()
    }

    /// The session an operation runs on: the next via round-robin when `turn`, and the first
    /// otherwise.
    fn member(&self, turn: bool) -> &StorageSession {
        if turn {
            &self.sessions[self.next_index()]
        } else {
            &self.sessions[0]
        }
    }

    fn next_index(&self) -> usize {
        self.next.fetch_add(1, Ordering::Relaxed) % self.sessions.len()
    }
}

/// Owns a pool of Storage connections and manages session lifecycle with
/// deduplication, round-robin connection assignment, and automatic cleanup.
///
/// Each `(partition, correlation_id)` maps to a `SessionPool` containing one
/// `StorageSession` per underlying `Storage` connection. Operations on a
/// returned session round-robin across the pool so a single command spreads
/// load over every connection set up in the connect phase.
pub struct StorageConnector {
    connections: Vec<Arc<dyn Storage>>,
    counter: AtomicUsize,
    pools: dashmap::DashMap<(Partition, String), Weak<SessionPool>>,
    /// Partitions for which `session_start` has already succeeded on every underlying
    /// `Storage`. Tracks the server-side `authorized_repos` state — once a partition is
    /// registered here, the server keeps it in `authorized_repos` for the connection's
    /// lifetime regardless of `session_stop`, so subsequent ops for the same partition can
    /// skip the `session_start` round-trip purely for authorization.
    ///
    /// The set is per-`StorageConnector`, which matches the server scoping: one
    /// `StorageServiceV4` instance (and its `SessionMap`) per accepted connection. When the
    /// owning `Connection` drops, the connector goes with it and the set resets.
    authorized_partitions: dashmap::DashSet<Partition>,
    /// Partitions this connector's identity has been refused. Only a refusal is recorded — a
    /// transport failure says nothing about the claim and stays retryable — so the entry means the
    /// answer will not change until the identity does, and asking again is a round trip that can
    /// only fail. Cleared by a `session_start` that later succeeds, which is proof it has.
    refused_partitions: dashmap::DashSet<Partition>,
}

impl StorageConnector {
    pub fn new(connections: Vec<Arc<dyn Storage>>) -> Self {
        Self {
            connections,
            counter: AtomicUsize::new(0),
            pools: dashmap::DashMap::new(),
            authorized_partitions: dashmap::DashSet::new(),
            refused_partitions: dashmap::DashSet::new(),
        }
    }

    /// Whether the given partition has previously had `session_start` succeed on every
    /// underlying `Storage` for this connector. A `true` answer means the server's
    /// `authorized_repos` set already contains the partition and a fresh `session_start`
    /// purely for authorization is unnecessary.
    pub fn is_partition_authorized(&self, partition: Partition) -> bool {
        self.authorized_partitions.contains(&partition)
    }

    /// Whether `session_start` for this partition has already been refused on this connector.
    pub fn is_partition_refused(&self, partition: Partition) -> bool {
        self.refused_partitions.contains(&partition)
    }

    /// Record that this connector's identity holds no claim to `partition`.
    pub fn mark_partition_refused(&self, partition: Partition) {
        self.refused_partitions.insert(partition);
    }

    /// Record that `session_start` has succeeded, which retires any earlier refusal: the claim was
    /// just exercised, so whatever the refusal was about no longer holds.
    #[lore_macro::test_pub]
    pub(crate) fn mark_partition_authorized(&self, partition: Partition) {
        self.authorized_partitions.insert(partition);
        self.refused_partitions.remove(&partition);
    }

    /// Get or create the `SessionPool` for the given partition and correlation ID.
    /// The caller pins the pool to keep every session it owns alive across the
    /// operations of one command. A stale pool is not handed out again.
    ///
    /// Nothing is taken from the pool here. Taking a session advances the pool's
    /// round-robin cursor, so a caller that only wanted the pool would leave every
    /// other caller striding over the connections instead of visiting each in turn.
    ///
    /// On a miss, one server-side session is started per underlying connection, in
    /// parallel. The first writer wins the key, vacant, expired or stale entry alike. No
    /// session is stopped here: each stops its own when its last reference drops, a losing
    /// racer's as its pool drops. A replaced entry's ids may name sessions this one started,
    /// since a reconnect restarts the server's session ids.
    pub async fn session_pool(
        &self,
        partition: Partition,
        correlation_id: &str,
        connection: Arc<Connection>,
    ) -> Result<Arc<SessionPool>, ProtocolError> {
        let key = (partition, correlation_id.to_string());

        // Fast path: live pool exists.
        if let Some(entry) = self.pools.get(&key)
            && let Some(pool) = entry.upgrade()
            && !pool.is_stale()
        {
            return Ok(pool);
        }

        // Slow path: start one session per connection in parallel. No lock held.
        let started = Arc::new(Mutex::new(Vec::with_capacity(self.connections.len())));
        let mut tasks = JoinSet::new();
        for storage in self.connections.iter().cloned() {
            let correlation_id = correlation_id.to_string();
            let started = started.clone();
            lore_spawn_net!(tasks, async move {
                let session_id = storage.session_start(partition, &correlation_id).await?;
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
        self.mark_partition_authorized(partition);

        // Build the pool with strong refs to every session.
        let correlation: Arc<str> = Arc::from(correlation_id);
        let sessions: Vec<Arc<StorageSession>> = started
            .into_iter()
            .map(|(storage, session_id)| {
                Arc::new(StorageSession::resolved(
                    storage,
                    connection.clone(),
                    session_id,
                    partition,
                    correlation.clone(),
                ))
            })
            .collect();
        let pool = Arc::new(SessionPool::new(sessions));

        #[allow(clippy::disallowed_methods)]
        // Synchronous entry check; no await while lock is held.
        let winner = match self.pools.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if let Some(alive) = entry.get().upgrade().filter(|alive| !alive.is_stale()) {
                    alive
                } else {
                    entry.insert(Arc::downgrade(&pool));
                    pool
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(Arc::downgrade(&pool));
                pool
            }
        };
        Ok(winner)
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
