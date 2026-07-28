// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;
use std::sync::Weak;
use std::sync::atomic::Ordering;

use lore_base::lore_debug;
use lore_base::lore_spawn;
use lore_base::lore_trace;
use lore_base::lore_warn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::runtime::try_lore_context;
use lore_base::types::*;
use lore_error_set::prelude::*;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use url::Url;

use crate::auth;
use crate::auth::exchange::auth_exchange;
use crate::error::ProtocolError;
use crate::grpc;
use crate::quic;
use crate::session::SessionPool;
use crate::session::StorageConnector;
use crate::session::StorageSession;
use crate::traits::*;
use crate::types::*;

pub static MAX_STORAGE_CONNECTIONS: usize = 10;
pub static DEFAULT_PROTOCOL: &str = "lores";

/// Start delay in reconnect loop, in milliseconds
pub static RECONNECT_START_DELAY: u64 = 1_000;
/// Maximum wait time between reconnect attempts, in milliseconds
pub static RECONNECT_MAX_DELAY: u64 = 30_000;
/// Maximum reconnect attempts before giving up
pub static RECONNECT_MAX_ATTEMPTS: usize = 10;

static PROTOCOL_MAP: Mutex<Option<HashMap<String, Arc<dyn Protocol>>>> = Mutex::new(None);

static REGISTER_BUILTIN_PROTOCOLS: Once = Once::new();

pub fn find(scheme: &str) -> Result<Arc<dyn Protocol>, ProtocolError> {
    REGISTER_BUILTIN_PROTOCOLS.call_once(|| {
        let _ = add("lore", Arc::new(LoreProtocol::default()));
        let _ = add("lores", Arc::new(LoreProtocol::default()));
        // Legacy protocol schemes for backwards compatibility
        let _ = add("urc", Arc::new(LoreProtocol::default()));
        let _ = add("urcs", Arc::new(LoreProtocol::default()));
        let _ = add("grpc", Arc::new(GRPCProtocol::default()));
        let _ = add("grpcs", Arc::new(GRPCProtocol::default()));
    });

    let mut map = PROTOCOL_MAP.lock();
    if map.is_none() {
        *map = Some(HashMap::new());
    }
    match map.as_ref().unwrap().get(scheme) {
        Some(protocol) => Ok(protocol.clone()),
        None => Err(ProtocolError::internal(format!(
            "protocol {scheme} was not recognized"
        ))),
    }
}

pub fn add(scheme: &str, protocol: Arc<dyn Protocol>) -> Result<(), ProtocolError> {
    let mut map = PROTOCOL_MAP.lock();
    if map.is_none() {
        *map = Some(HashMap::new());
    }
    map.as_mut().unwrap().insert(scheme.to_string(), protocol);
    Ok(())
}

#[allow(clippy::type_complexity)]
/// Connections are keyed by `(remote_url, identity)`. Storage uses per-session auth,
/// and non-storage services (revision, admin, lock) are created lazily per-repository
/// with per-repository authz tokens.
static CONNECTION_MAP: Mutex<Option<HashMap<(String, String), Arc<Connection>>>> = Mutex::new(None);

pub fn find_connection(remote_url: &str, identity: &str) -> Option<Arc<Connection>> {
    let mut map = CONNECTION_MAP.lock();
    let map = map.as_mut()?;

    // When the caller supplies an identity, require an exact key match. This is the
    // hot path after the first auth exchange has cached the resolved entry.
    if !identity.is_empty() {
        let key = (remote_url.to_string(), identity.to_string());
        if let Some(connection) = map.get(&key) {
            if !connection.stale.load(Ordering::Relaxed) {
                return Some(connection.clone());
            }
            map.remove(&key);
        }
        return None;
    }

    // Caller has no identity yet (config omits it). The resolved identity is
    // deterministic for a given url/credential store, so reuse any non-stale entry
    // keyed under the same URL. Without this, every call that omits an identity
    // re-enters `connect_impl` and re-issues `EnvironmentService/Get` even though
    // the Connection would be reused by the inner lookup after auth_exchange.
    map.iter()
        .find(|((u, _), c)| u == remote_url && !c.stale.load(Ordering::Relaxed))
        .map(|(_, c)| c.clone())
}

pub fn add_connection(remote_url: &str, identity: &str, connection: Arc<Connection>) {
    let mut map = CONNECTION_MAP.lock();
    if let Some(map) = map.as_mut() {
        map.insert((remote_url.to_string(), identity.to_string()), connection);
    } else {
        let mut hashmap = HashMap::new();
        hashmap.insert((remote_url.to_string(), identity.to_string()), connection);
        map.replace(hashmap);
    }
}

pub fn remove_connection(connection: Arc<Connection>) {
    let mut map = CONNECTION_MAP.lock();
    if let Some(map) = map.as_mut() {
        map.retain(|_, value| !Arc::ptr_eq(value, &connection));
    }
}

pub fn drop_connections() {
    if let Some(map) = CONNECTION_MAP.lock().take() {
        // This is done during library shutdown, setup a dummy context
        // for dropping the remaining connections
        tokio::task::block_in_place(move || {
            runtime().block_on(async {
                for connection in map {
                    let _ = connection.1.cancel_connect().await;
                    // Drain in-flight streams and flush transport close frames to the peer
                    // before the runtime goes away. Without this, the server logs every
                    // outstanding stream read as a transport error on client exit.
                    connection.1.close_transport().await;
                }
            });
        });
    }
}

pub fn parse(remote_url: &str) -> Result<(Url, Arc<dyn Protocol>), ProtocolError> {
    if remote_url.is_empty() {
        return Err(ProtocolError::internal("no remote URL"));
    }

    let mut remote_url = remote_url.to_string();
    if !remote_url.contains("://") {
        let mut full_url = DEFAULT_PROTOCOL.to_string();
        full_url.push_str("://");
        full_url.push_str(remote_url.as_str());
        remote_url = full_url;
    }

    let parsed_url = url::Url::parse(remote_url.as_str())
        .internal_with(|| format!("remote {remote_url} is invalid"))?;

    let protocol = parsed_url.scheme();
    let protocol = find(protocol).internal_with(|| format!("remote {remote_url} is invalid"))?;

    Ok((parsed_url, protocol))
}

pub async fn connect(
    remote_url: &str,
    identity: &str,
    repository: RepositoryId,
    max_connections: usize,
) -> Result<Arc<Connection>, ProtocolError> {
    let (remote_url, protocol) = parse(remote_url)?;

    // Try early out by reusing a known existing connection
    let identity = identity.to_string();
    if let Some(connection) = find_connection(remote_url.as_str(), identity.as_str()) {
        return Ok(connection);
    }

    Box::pin(async move {
        connect_impl(protocol, remote_url, identity, repository, max_connections).await
    })
    .await
}

async fn connect_impl(
    protocol: Arc<dyn Protocol>,
    remote_url: Url,
    identity: String,
    repository: RepositoryId,
    max_connections: usize,
) -> Result<Arc<Connection>, ProtocolError> {
    let remote_domain = lore_credential::domain_from_url_or_url(&remote_url);

    // Forward rather than `internal`-wrap so a classified failure (such as
    // `Disconnected`) survives instead of collapsing to `Internal`.
    let environment_client = protocol
        .environment(Weak::default(), remote_url.as_str())
        .await
        .forward_with::<ProtocolError, _>(|| format!("connect: {remote_url}"))?;
    let environment = environment_client
        .get()
        .await
        .forward::<ProtocolError>("failed to get environment config")?;

    let auth_url = environment
        .endpoint
        .as_ref()
        .and_then(|endpoint| endpoint.auth_url.clone())
        .unwrap_or_default();

    let mut identity = identity;
    if !auth_url.is_empty() {
        // Ensure we are authenticated if there is an auth url defined in the environment
        if identity.is_empty() {
            let (_, _, resolved) = auth_exchange(&auth_url, &remote_domain, "", repository).await;
            identity = resolved;

            if identity.is_empty() {
                let has_identities =
                    lore_credential::token_store::load_identities(auth_url.as_str())
                        .await
                        .is_ok_and(|ids| !ids.is_empty());
                if has_identities {
                    return Err(ProtocolError::from(lore_base::error::NotAuthorized));
                }
                return Err(ProtocolError::from(lore_base::error::NotAuthenticated));
            }
        } else {
            lore_credential::token_store::load_user_token(
                &auth_url,
                &identity,
                lore_credential::token_store::tokens_only_for_recipient_domain(
                    remote_domain.clone(),
                ),
            )
            .await
            .internal("loading user token")?;
        }
    }

    if let Some(connection) = find_connection(remote_url.as_str(), identity.as_str()) {
        return Ok(connection);
    }

    let connection = Arc::new(Connection {
        remote_url: remote_url.clone(),
        auth_url: auth_url.clone(),
        identity: identity.clone(),
        protocol: protocol.clone(),
        environment,
        storage_ready: ServiceReady::new(),
        revision_ready: ServiceReady::new(),
        lock_ready: ServiceReady::new(),
        repository_ready: ServiceReady::new(),
        storage_building: tokio::sync::Mutex::new(None),
        storage: parking_lot::RwLock::new(None),
        revision: dashmap::DashMap::new(),
        admin: dashmap::DashMap::new(),
        lock: dashmap::DashMap::new(),
        repository: tokio::sync::Mutex::new(None),
        session_cache: dashmap::DashMap::new(),
        connector: tokio::sync::Mutex::new(None),
        stale: std::sync::atomic::AtomicBool::new(false),
    });

    let subtask_aborts: Arc<parking_lot::Mutex<Vec<tokio::task::AbortHandle>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));

    let connect_task = lore_spawn!({
        let environment_client = environment_client.clone();
        let connection = connection.clone();
        let remote_url = remote_url.clone();
        let identity = identity.clone();
        let subtask_aborts = subtask_aborts.clone();
        async move {
            let endpoint_description = if repository.is_zero() {
                format!("{remote_url} repository service")
            } else {
                format!("{remote_url} for repository {repository}")
            };
            lore_trace!("Connecting to {endpoint_description}");

            if !repository.is_zero() {
                if !auth_url.is_empty() {
                    lore_trace!("Token exchange for identity {identity} for {auth_url}");
                    if let Err(err) =
                        auth::exchange::exchange(&auth_url, &identity, repository, remote_domain)
                            .await
                            .inspect_err(|err| lore_debug!("Auth exchange failed: {err}"))
                            .forward::<ProtocolError>("authorization failure")
                    {
                        connection.storage_ready.complete(Err(err.clone()));
                        connection.revision_ready.complete(Err(err.clone()));
                        connection.lock_ready.complete(Err(err.clone()));
                        connection.repository_ready.complete(Err(err));
                        return;
                    }
                } else {
                    lore_debug!("Unauthenticated server, no token exchange");
                }
            }

            let remote_url_str = remote_url.as_str();
            let storage_url: String = connection
                .environment
                .storage_url(remote_url_str)
                .to_string();
            let revision_url: String = connection
                .environment
                .revision_url(remote_url_str)
                .to_string();
            let lock_url: String = connection.environment.lock_url(remote_url_str).to_string();
            let repository_service_url: String = connection
                .environment
                .repository_url(remote_url_str)
                .to_string();

            // Storage connections are always created -- they're repository-agnostic.
            // Per-repository auth is handled by session_start().
            let max_connections = max_connections.clamp(1, MAX_STORAGE_CONNECTIONS);
            lore_trace!(
                "Connecting storage service to {storage_url} using {max_connections} connections"
            );
            let storage_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(max_connections));
            let storage_error: Arc<parking_lot::Mutex<Option<ProtocolError>>> =
                Arc::new(parking_lot::Mutex::new(None));
            for index in 0..max_connections {
                let storage_url = storage_url.clone();
                let auth_url = auth_url.clone();
                let connection = connection.clone();
                let identity = identity.clone();
                let environment_client = environment_client.clone();
                let storage_remaining = storage_remaining.clone();
                let storage_error = storage_error.clone();
                let handle = lore_spawn!(async move {
                    let _environment_client = environment_client;
                    let result = connection
                        .protocol
                        .storage(
                            Arc::downgrade(&connection),
                            storage_url.as_str(),
                            auth_url.as_str(),
                            identity.as_str(),
                            repository,
                            index,
                        )
                        .await;
                    match result {
                        Ok(storage) => {
                            let mut building = connection.storage_building.lock().await;
                            if let Some(vec) = building.as_mut() {
                                vec.push(storage);
                            } else {
                                *building = Some(vec![storage]);
                            }
                        }
                        Err(err) => {
                            let mut slot = storage_error.lock();
                            if slot.is_none() {
                                *slot = Some(err);
                            }
                        }
                    }
                    if storage_remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                        let err = storage_error.lock().take();
                        if let Some(err) = err {
                            connection.storage_ready.complete(Err(err));
                        } else {
                            let connections = connection
                                .storage_building
                                .lock()
                                .await
                                .take()
                                .unwrap_or_default();
                            if !connections.is_empty() {
                                *connection.storage.write() =
                                    Some(Arc::new(StorageConnector::new(connections)));
                            }
                            connection.storage_ready.complete(Ok(()));
                        }
                    }
                });
                subtask_aborts.lock().push(handle.abort_handle());
            }

            // Admin services are created lazily per-repository via Connection::admin().

            if !repository.is_zero() {
                {
                    lore_trace!("Connecting revision service to {revision_url}");
                    let revision_url = revision_url.clone();
                    let auth_url = auth_url.clone();
                    let connection = connection.clone();
                    let identity = identity.clone();
                    let environment_client = environment_client.clone();
                    let handle = lore_spawn!(async move {
                        let _environment_client = environment_client;
                        let result = connection
                            .protocol
                            .revision(
                                Arc::downgrade(&connection),
                                revision_url.as_str(),
                                auth_url.as_str(),
                                identity.as_str(),
                                repository,
                            )
                            .await;
                        match result {
                            Ok(revision) => {
                                connection.revision.insert(repository, revision);
                                connection.revision_ready.complete(Ok(()));
                            }
                            Err(err) => connection.revision_ready.complete(Err(err)),
                        }
                    });
                    subtask_aborts.lock().push(handle.abort_handle());
                }

                {
                    lore_trace!("Connecting lock service to {lock_url}");
                    let lock_url = lock_url.clone();
                    let auth_url = auth_url.clone();
                    let connection = connection.clone();
                    let identity = identity.clone();
                    let environment_client = environment_client.clone();
                    let handle = lore_spawn!(async move {
                        let _environment_client = environment_client;
                        let result = connection
                            .protocol
                            .lock(
                                Arc::downgrade(&connection),
                                lock_url.as_str(),
                                auth_url.as_str(),
                                identity.as_str(),
                                repository,
                            )
                            .await;
                        match result {
                            Ok(lock) => {
                                connection.lock.insert(repository, lock);
                                connection.lock_ready.complete(Ok(()));
                            }
                            Err(err) => connection.lock_ready.complete(Err(err)),
                        }
                    });
                    subtask_aborts.lock().push(handle.abort_handle());
                }
            } else {
                connection.revision_ready.complete(Ok(()));
                connection.lock_ready.complete(Ok(()));
            }

            {
                let repository_service_url = repository_service_url.clone();
                let auth_url = auth_url.clone();
                let connection = connection.clone();
                let identity = identity.clone();
                let environment_client = environment_client.clone();
                let handle = lore_spawn!(async move {
                    let _environment_client = environment_client;
                    let result = connection
                        .protocol
                        .repository(
                            Arc::downgrade(&connection),
                            // see URC_GREP_TOKEN_AUTH_NOTE regarding token warming and security
                            repository_service_url.as_str(),
                            auth_url.as_str(),
                            identity.as_str(),
                        )
                        .await;
                    match result {
                        Ok(repository) => {
                            let mut conn_lock = connection.repository.lock().await;
                            *conn_lock = Some(repository);
                            drop(conn_lock);
                            connection.repository_ready.complete(Ok(()));
                        }
                        Err(err) => connection.repository_ready.complete(Err(err)),
                    }
                });
                subtask_aborts.lock().push(handle.abort_handle());
            }
        }
    });

    {
        let mut lock = connection.connector.lock().await;
        *lock = Some(Connector {
            setup_handle: connect_task,
            subtask_aborts,
        });
    }

    add_connection(remote_url.as_str(), identity.as_str(), connection.clone());

    Ok(connection)
}

/// Multi-waiter "set once" completion signal. Each per-service connect
/// subtask owns one; consumers await readiness via `wait`. Calling
/// `complete` twice is a no-op — the first value wins, which matters during
/// cancellation when a subtask may already have raced to a success.
#[derive(Default)]
struct ServiceReady {
    notify: tokio::sync::Notify,
    result: parking_lot::Mutex<Option<Result<(), ProtocolError>>>,
}

impl ServiceReady {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn complete(&self, result: Result<(), ProtocolError>) {
        let mut guard = self.result.lock();
        if guard.is_some() {
            return;
        }
        *guard = Some(result);
        drop(guard);
        self.notify.notify_waiters();
    }

    async fn wait(&self) -> Result<(), ProtocolError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.result.lock().as_ref() {
                return result.clone();
            }
            notified.await;
        }
    }
}

struct Connector {
    /// Outer setup task: runs token exchange and spawns per-service
    /// subtasks. Each subtask signals its own `ServiceReady` independently;
    /// this handle exists so `cancel_connect` can abort the setup phase.
    setup_handle: JoinHandle<()>,
    /// Abort handles for every subtask the setup task has spawned. Pushed by
    /// the setup task as it spawns them; consumed by `cancel_connect`.
    subtask_aborts: Arc<parking_lot::Mutex<Vec<tokio::task::AbortHandle>>>,
}

/// Connection over a protocol
pub struct Connection {
    pub remote_url: Url,
    pub auth_url: String,
    pub identity: String,
    pub environment: EnvironmentConfig,
    protocol: Arc<dyn Protocol>,
    /// Per-service readiness. Signalled by each subtask on completion (or
    /// by `cancel_connect` on shutdown). Outlives `connector` so waiters
    /// resolve to a cancelled error after cleanup rather than hanging.
    storage_ready: Arc<ServiceReady>,
    revision_ready: Arc<ServiceReady>,
    lock_ready: Arc<ServiceReady>,
    repository_ready: Arc<ServiceReady>,
    /// Temporary collection point for storage subtasks. The last subtask to
    /// finish freezes the Vec into `storage` and signals `storage_ready`.
    storage_building: tokio::sync::Mutex<Option<Vec<Arc<dyn Storage>>>>,
    /// Frozen storage connector, set once all storage subtasks complete.
    storage: parking_lot::RwLock<Option<Arc<StorageConnector>>>,
    /// Per-repository services, created lazily on first access.
    revision: dashmap::DashMap<RepositoryId, Arc<dyn Revision>>,
    admin: dashmap::DashMap<RepositoryId, Arc<dyn Admin>>,
    lock: dashmap::DashMap<RepositoryId, Arc<dyn Lock>>,
    /// Repository service -- not per-repository (uses default `RepositoryId`).
    repository: tokio::sync::Mutex<Option<Arc<dyn Repository>>>,
    /// Pins `Arc<SessionPool>` to keep the `Weak` in `StorageConnector` upgradeable.
    /// Pinning the pool keeps every session it owns alive across operations
    /// within a command, avoiding session start/stop churn between calls.
    /// Cleared by the caller (e.g. `repository_call`) when the API call completes.
    session_cache: dashmap::DashMap<(RepositoryId, String), Arc<SessionPool>>,
    connector: tokio::sync::Mutex<Option<Connector>>,
    pub stale: std::sync::atomic::AtomicBool,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let runtime = runtime();
        if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
            // Only in tests, here we cannot block in place to call the async complete
        } else {
            // Connection may be dropped from a fire-and-forget task (e.g. StorageSession::drop)
            // that has no context. Use try_lore_context to avoid panicking.
            #[allow(clippy::disallowed_methods)]
            tokio::task::block_in_place(move || {
                let future = async move { self.cancel_connect().await };
                if let Some(ctx) = try_lore_context() {
                    let _ = runtime.block_on(LORE_CONTEXT.scope(ctx, future));
                } else {
                    let _ = runtime.block_on(future);
                }
            });
        }
    }
}

impl Connection {
    pub fn remote_url(&self) -> &str {
        self.remote_url.as_str()
    }

    pub fn auth_url(&self) -> &str {
        self.auth_url.as_str()
    }

    pub fn identity(&self) -> &str {
        self.identity.as_str()
    }

    /// Mark the connection failed and unregister it from the connection cache.
    /// Idempotent — additional callers find no entry and no-op.
    fn mark_failed(self: &Arc<Self>) {
        if !self.stale.swap(true, Ordering::Relaxed) {
            remove_connection(self.clone());
            lore_warn!("Connection to {} failed", self.remote_url);
        }
    }

    pub async fn ensure_repository_connected(self: &Arc<Self>) -> Result<(), ProtocolError> {
        let result = self.repository_ready.wait().await;
        if result.is_err() {
            self.mark_failed();
        }
        result
    }

    pub async fn ensure_revision_connected(self: &Arc<Self>) -> Result<(), ProtocolError> {
        let result = self.revision_ready.wait().await;
        if result.is_err() {
            self.mark_failed();
        }
        result
    }

    pub async fn ensure_lock_connected(self: &Arc<Self>) -> Result<(), ProtocolError> {
        let result = self.lock_ready.wait().await;
        if result.is_err() {
            self.mark_failed();
        }
        result
    }

    pub async fn ensure_storage_connected(self: &Arc<Self>) -> Result<(), ProtocolError> {
        let result = self.storage_ready.wait().await;
        if result.is_err() {
            self.mark_failed();
        }
        result
    }

    async fn cancel_connect(&self) -> Result<(), ProtocolError> {
        let mut connector_lock = self.connector.lock().await;
        let Some(connector) = connector_lock.take() else {
            return Ok(());
        };
        self.stale.store(true, Ordering::Relaxed);
        lore_trace!("Connection to {} cancelled", self.remote_url);
        connector.setup_handle.abort();
        for handle in std::mem::take(&mut *connector.subtask_aborts.lock()) {
            handle.abort();
        }
        let cancelled = || ProtocolError::internal("connection cancelled");
        // Unblock anyone awaiting readiness — `complete` is a no-op if a real
        // result already won the race.
        self.storage_ready.complete(Err(cancelled()));
        self.revision_ready.complete(Err(cancelled()));
        self.lock_ready.complete(Err(cancelled()));
        self.repository_ready.complete(Err(cancelled()));
        let _ = connector.setup_handle.await;
        Ok(())
    }

    /// Gracefully drain the transport connections held by this `Connection`.
    /// Intended to run during library shutdown so that in-flight streams finish
    /// and close frames reach the peer before the process exits.
    pub async fn close_transport(&self) {
        let storage = self.storage.read().clone();
        if let Some(storage) = storage {
            storage.close_all().await;
        }
    }

    /// Returns the frozen storage connector, or error if not connected.
    fn storage_connector(&self) -> Result<Arc<StorageConnector>, ProtocolError> {
        self.storage
            .read()
            .clone()
            .ok_or_else(|| ProtocolError::internal("not connected"))
    }

    /// Returns a raw storage connection from the pool via round-robin.
    pub async fn storage(self: &Arc<Self>) -> Result<Arc<dyn Storage>, ProtocolError> {
        self.ensure_storage_connected().await?;
        let connector = self.storage_connector()?;
        let connections = connector.connections();
        if connections.is_empty() {
            return Err(ProtocolError::internal("not connected"));
        }
        let counter = connector.next_connection_index();
        Ok(connections[counter].clone())
    }

    /// Creates or reuses a `SessionPool` for the given repository and correlation
    /// ID, returning a round-robin-picked session from it. The pool is pinned in
    /// the connection's session cache so the `Weak` in `StorageConnector` stays
    /// upgradeable for subsequent calls within the same command, keeping every
    /// session in the pool alive without start/stop churn. Call
    /// `release_session()` when the API call completes to release the pool.
    pub async fn session(
        self: &Arc<Self>,
        repository: RepositoryId,
        correlation_id: &str,
    ) -> Result<Arc<StorageSession>, ProtocolError> {
        self.ensure_storage_connected().await?;
        let connector = self.storage_connector()?;
        let (session, pool) = connector
            .session(repository, correlation_id, self.clone())
            .await?;
        self.session_cache
            .insert((repository, correlation_id.to_string()), pool);
        Ok(session)
    }

    /// Unpin a cached session pool so its `Weak` in `StorageConnector` can
    /// expire. The pool's `Drop` releases every `Arc<StorageSession>` it owns,
    /// each of which sends `session_stop` to the server.
    pub fn release_session(&self, repository: RepositoryId, correlation_id: &str) {
        self.session_cache
            .remove(&(repository, correlation_id.to_string()));
    }

    /// Drop every pinned `SessionPool`. Once no other strong refs hold the
    /// pools alive (typically true between operations, or after callers
    /// re-resolve their `StorageSession`), the `Weak`s in `StorageConnector`
    /// fall out of scope and the next `Connection::session` call rebuilds
    /// the pool — re-running `session_start` against the current connection
    /// to obtain a fresh `session_id` the server actually knows about.
    /// Called from `StorageSession::invalidate` when a server response
    /// indicates the session-id is stale (e.g. after a QUIC reconnect that
    /// rotated the server's `SessionMap`).
    pub fn invalidate_all_sessions(&self) {
        self.session_cache.clear();
    }

    /// Ensure the server's per-connection `authorized_repos` set contains `repository`,
    /// without leaving a session pinned. Fast-paths via the connector's
    /// `authorized_partitions` cache: if a previous `session_start` already registered
    /// `repository` on every underlying connection, no wire calls happen. Otherwise a
    /// fresh session is started (which fans `session_start` across all connections in
    /// parallel) and immediately released; the server keeps `authorized_repos` permanent
    /// for the connection's lifetime, so the registration outlives the session.
    pub async fn ensure_partition_authorized(
        self: &Arc<Self>,
        repository: RepositoryId,
        correlation_id: &str,
    ) -> Result<(), ProtocolError> {
        self.ensure_storage_connected().await?;
        let connector = self.storage_connector()?;
        if connector.is_partition_authorized(repository) {
            return Ok(());
        }
        // Drive the slow path through `session()` so the `authorized_partitions` insert
        // and the standard race-resolution / pool bookkeeping all run. We immediately
        // drop the returned `StorageSession` and release the cache entry — the call's
        // only purpose was to register authz, not to keep a live session.
        let _session = self.session(repository, correlation_id).await?;
        self.release_session(repository, correlation_id);
        Ok(())
    }

    pub async fn revision(
        self: &Arc<Self>,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Revision>, ProtocolError> {
        self.ensure_revision_connected().await?;
        if let Some(entry) = self.revision.get(&repository) {
            return Ok(entry.value().clone());
        }
        let revision = self
            .protocol
            .revision(
                Arc::downgrade(self),
                self.remote_url.as_str(),
                self.auth_url.as_str(),
                self.identity.as_str(),
                repository,
            )
            .await?;
        self.revision.insert(repository, revision.clone());
        Ok(revision)
    }

    pub async fn repository(self: &Arc<Self>) -> Result<Arc<dyn Repository>, ProtocolError> {
        self.ensure_repository_connected().await?;

        let lock = self.repository.lock().await;
        if let Some(repository) = lock.as_ref() {
            return Ok(repository.clone());
        }

        Err(ProtocolError::internal("not connected"))
    }

    pub async fn admin(
        self: &Arc<Self>,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        if let Some(entry) = self.admin.get(&repository) {
            return Ok(entry.value().clone());
        }
        let admin = self
            .protocol
            .admin(
                Arc::downgrade(self),
                self.remote_url.as_str(),
                self.auth_url.as_str(),
                self.identity.as_str(),
                repository,
            )
            .await?;
        self.admin.insert(repository, admin.clone());
        Ok(admin)
    }

    pub async fn lock(
        self: &Arc<Self>,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Lock>, ProtocolError> {
        self.ensure_lock_connected().await?;
        if let Some(entry) = self.lock.get(&repository) {
            return Ok(entry.value().clone());
        }
        let lock = self
            .protocol
            .lock(
                Arc::downgrade(self),
                self.remote_url.as_str(),
                self.auth_url.as_str(),
                self.identity.as_str(),
                repository,
            )
            .await?;
        self.lock.insert(repository, lock.clone());
        Ok(lock)
    }

    pub async fn connect_module(&self, module: RepositoryId) -> Result<Arc<Self>, ProtocolError> {
        // TODO(vri): UCS-19226 - Links: Connection reuse for already connected links
        connect(
            self.remote_url.as_str(),
            self.identity.as_str(),
            module,
            MAX_STORAGE_CONNECTIONS,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Protocol implementations
// ---------------------------------------------------------------------------

/// URC protocol, using QUIC for storage and gRPC for revision
#[derive(Default)]
struct LoreProtocol {}

#[async_trait::async_trait]
impl Protocol for LoreProtocol {
    async fn storage(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
        _index: usize,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        quic::storage(connection, remote_url, auth_url, identity, repository).await
    }

    async fn revision(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Revision>, ProtocolError> {
        grpc::revision(connection, remote_url, auth_url, identity, repository).await
    }

    async fn repository(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
    ) -> Result<Arc<dyn Repository>, ProtocolError> {
        grpc::repository(connection, remote_url, auth_url, identity).await
    }

    async fn admin(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        grpc::admin(connection, remote_url, auth_url, identity, repository).await
    }

    async fn lock(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Lock>, ProtocolError> {
        grpc::lock(connection, remote_url, auth_url, identity, repository).await
    }

    async fn environment(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
    ) -> Result<Arc<dyn Environment>, ProtocolError> {
        grpc::environment(connection, remote_url).await
    }
}

/// gRPC protocol, using gRPC for both storage and revision
#[derive(Default)]
struct GRPCProtocol {}

#[async_trait::async_trait]
impl Protocol for GRPCProtocol {
    async fn storage(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
        index: usize,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        grpc::storage(
            connection, remote_url, auth_url, identity, repository, index,
        )
        .await
    }

    async fn revision(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Revision>, ProtocolError> {
        grpc::revision(connection, remote_url, auth_url, identity, repository).await
    }

    async fn repository(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
    ) -> Result<Arc<dyn Repository>, ProtocolError> {
        grpc::repository(connection, remote_url, auth_url, identity).await
    }

    async fn admin(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        grpc::admin(connection, remote_url, auth_url, identity, repository).await
    }

    async fn lock(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Lock>, ProtocolError> {
        grpc::lock(connection, remote_url, auth_url, identity, repository).await
    }

    async fn environment(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
    ) -> Result<Arc<dyn Environment>, ProtocolError> {
        grpc::environment(connection, remote_url).await
    }
}

#[cfg(test)]
mod tests {
    use lore_base::error::*;

    use super::*;
    use crate::MatchedProtocolError;

    #[test]
    fn not_supported_to_tonic_status() {
        let err = ProtocolError::from(NotSupported {
            operation: "refresh".into(),
        });
        let status: tonic::Status = err.into();
        assert_eq!(status.code(), tonic::Code::Unimplemented);
    }

    #[test]
    fn tonic_unimplemented_to_not_supported() {
        let status = tonic::Status::new(tonic::Code::Unimplemented, "not implemented");
        let err = ProtocolError::from(status);
        assert!(err.is_not_supported());
    }

    #[test]
    fn not_supported_try_match() {
        let result: Result<(), ProtocolError> = Err(ProtocolError::from(NotSupported {
            operation: "refresh".into(),
        }));
        let matched = result.try_match("testing not supported");
        // try_match returns Result<Result<T, Matched>, Internal>
        // NotSupported is a handleable variant, not Internal, so outer should be Ok
        let inner = matched.expect("should not propagate as Internal");
        assert!(inner.is_err());
        match inner.unwrap_err() {
            MatchedProtocolError::NotSupported(e) => {
                assert_eq!(e.operation, "refresh");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Protocol-agnostic type tests
    // -----------------------------------------------------------------------

    #[test]
    fn auth_session_fields() {
        let session = AuthSession {
            session_code: "sess-123".into(),
            login_url: "https://auth.example.com/login?code=abc".into(),
        };
        assert_eq!(session.session_code, "sess-123");
        assert_eq!(session.login_url, "https://auth.example.com/login?code=abc");
    }

    #[test]
    fn authentication_token_with_refresh() {
        let token = AuthenticationToken {
            token: "jwt-token".into(),
            user_id: "user-1".into(),
            user_name: "Alice".into(),
            expires_ms: 1700000000000,
            acceptable_root_domains: vec!["example.com".into()],
            refresh_token: Some("refresh-abc".into()),
        };
        assert_eq!(token.token, "jwt-token");
        assert_eq!(token.user_id, "user-1");
        assert_eq!(token.user_name, "Alice");
        assert_eq!(token.expires_ms, 1700000000000);
        assert_eq!(token.acceptable_root_domains, vec!["example.com"]);
        assert_eq!(token.refresh_token.as_deref(), Some("refresh-abc"));
    }

    #[test]
    fn authentication_token_without_refresh() {
        let token = AuthenticationToken {
            token: "jwt-token".into(),
            user_id: "user-1".into(),
            user_name: "Alice".into(),
            expires_ms: 1700000000000,
            acceptable_root_domains: vec![],
            refresh_token: None,
        };
        assert!(token.refresh_token.is_none());
    }

    #[test]
    fn authorization_token_fields() {
        let token = AuthorizationToken {
            token: "authz-jwt".into(),
            expires_ms: 1700000060000,
            acceptable_root_domains: vec!["repo.example.com".into(), "cdn.example.com".into()],
        };
        assert_eq!(token.token, "authz-jwt");
        assert_eq!(token.expires_ms, 1700000060000);
        assert_eq!(token.acceptable_root_domains.len(), 2);
    }

    #[test]
    fn resolved_user_fields() {
        let user = ResolvedUser {
            user_id: "uid-42".into(),
            user_name: "Bob".into(),
        };
        assert_eq!(user.user_id, "uid-42");
        assert_eq!(user.user_name, "Bob");
    }
}
