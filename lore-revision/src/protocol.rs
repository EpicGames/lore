// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use lore_base::lore_spawn_net;
use lore_base::retry::Retry;
use lore_base::retry::retry;
use lore_transport::Admin;
use lore_transport::Connection;
use lore_transport::PartitionSessions;
use lore_transport::ProtocolError;
use lore_transport::SessionPool;
use lore_transport::StorageSession;
use lore_transport::connection::RECONNECT_MAX_DELAY;
use lore_transport::connection::RECONNECT_START_DELAY;
use tokio::time::Instant;

use crate::errors::is_refusal;
use crate::lore::RepositoryId;
use crate::lore_debug;

/// A connection to `remote_url` as `identity` for `repository`: one the connection cache holds, or
/// one newly opened that asks for the execution's count of storage connections.
pub fn connect<'a>(
    remote_url: &'a str,
    identity: &'a str,
    repository: crate::lore::RepositoryId,
) -> impl Future<Output = Result<Arc<Connection>, ProtocolError>> + 'a {
    connect_over(remote_url, identity, repository, None)
}

/// [`connect`], asking for `storage_connections` storage connections where it opens a new
/// connection, or for the execution's count where `None`.
async fn connect_over(
    remote_url: &str,
    identity: &str,
    repository: crate::lore::RepositoryId,
    storage_connections: Option<usize>,
) -> Result<Arc<Connection>, ProtocolError> {
    let execution = crate::lore::execution_context();
    let globals = execution.globals();
    let connection = lore_transport::connect(
        remote_url,
        identity,
        repository,
        storage_connections.unwrap_or(globals.max_connections as usize),
        globals.identity_token(),
        globals.access_token(),
        globals.auth_mode,
    )
    .await?;
    take_server_compression_mode(&connection);
    Ok(connection)
}

/// Connections to one remote, opened as one identity and held per repository. One connect serves
/// every caller waiting on a repository. A failed connect, or a connection marked stale within the
/// back-off of its connect, is answered to every caller until that back-off has passed, and the
/// back-off doubles for each connect that fails so. A refusal of the credentials is answered to no
/// caller after those waiting on its connect, as a call with other credentials, or after a login,
/// may be accepted.
pub struct RemoteConnections {
    remote_url: Arc<str>,
    identity: Arc<str>,
    /// The storage connections a connect asks for where it opens a new connection, or the
    /// execution's count where `None`.
    storage_connections: Option<usize>,
    connect: Connect,
    connections: parking_lot::Mutex<HashMap<RepositoryId, Remote>>,
}

/// How [`RemoteConnections`] connects to its remote, as its identity, for a repository, asking for
/// its storage connections.
pub type Connect = fn(
    Arc<str>,
    Arc<str>,
    RepositoryId,
    Option<usize>,
) -> BoxFuture<'static, Result<Arc<Connection>, ProtocolError>>;

/// A connect shared by every caller awaiting it.
#[lore_macro::test_pub]
pub(crate) type RemoteFuture = Shared<BoxFuture<'static, Result<Arc<Connection>, ProtocolError>>>;

/// `connect` run as a task of its own on the network runtime and shared by every caller awaiting
/// it, so it runs to its end whichever of them drop.
pub(crate) fn shared_connect(
    connect: impl Future<Output = Result<Arc<Connection>, ProtocolError>> + Send + 'static,
) -> RemoteFuture {
    let handle = lore_spawn_net!(connect);
    async move {
        handle
            .await
            .unwrap_or_else(|_| Err(ProtocolError::internal("connect task failed")))
    }
    .boxed()
    .shared()
}

/// The connection [`RemoteConnections`] holds for one repository. A connect's back-off is the
/// interval its `retry` gave when it settled, which ends at `retry_at`.
enum Remote {
    Connected {
        connection: Arc<Connection>,
        retry_at: Instant,
        retry: Retry,
    },
    Connecting {
        attempt: RemoteFuture,
        retry: Retry,
    },
    /// A failed connect, or a connection marked stale within its connect's back-off, answered until
    /// `retry_at`.
    Failed {
        answer: Result<Arc<Connection>, ProtocolError>,
        retry_at: Instant,
        retry: Retry,
    },
}

impl RemoteConnections {
    /// Connections to `remote_url` as `identity`, none of them open yet.
    pub fn new(remote_url: impl Into<String>, identity: Option<&str>) -> Self {
        Self {
            remote_url: Arc::from(remote_url.into()),
            identity: Arc::from(identity.unwrap_or_default()),
            storage_connections: None,
            connect: |remote_url, identity, repository, storage_connections| {
                async move {
                    connect_over(&remote_url, &identity, repository, storage_connections).await
                }
                .boxed()
            },
            connections: parking_lot::Mutex::default(),
        }
    }

    /// These connections, each connect asking for `storage_connections` storage connections rather
    /// than the execution's count where it opens a new connection.
    pub fn connecting_over(mut self, storage_connections: usize) -> Self {
        self.storage_connections = Some(storage_connections);
        self
    }

    /// Connections to no remote, each opened by `connect`.
    #[cfg(feature = "test-util")]
    pub fn with_connect(connect: Connect) -> Self {
        Self {
            connect,
            ..Self::new("", None)
        }
    }

    /// Connections to no remote, holding `connection` for `repository`.
    #[cfg(feature = "test-util")]
    pub fn with_connection(repository: RepositoryId, connection: Arc<Connection>) -> Self {
        Self {
            connections: parking_lot::Mutex::new(HashMap::from([(
                repository,
                Remote::Connected {
                    connection,
                    retry_at: Instant::now(),
                    retry: connect_retry(),
                },
            )])),
            ..Self::new("", None)
        }
    }

    /// The URL of the remote.
    pub fn remote_url(&self) -> &str {
        &self.remote_url
    }

    /// The [`Admin`] handle for `partition`, opened on first use and held by its connection
    /// ([`Self::connection`]).
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub(crate) fn admin(
        &self,
        partition: RepositoryId,
    ) -> impl Future<Output = Result<Arc<dyn Admin>, ProtocolError>> + '_ {
        async move { self.connection(partition).await?.admin(partition).await }
    }

    /// The connection held for `repository`, a connection marked stale included until its
    /// connect's back-off has passed unless its storage was refused its credentials, or one newly
    /// opened in place of none, of a stale one past that back-off or refused, or of a failure whose
    /// back-off has passed. A replaced connection drops the session pools it pins, whose sessions
    /// hold it.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn connection(
        &self,
        repository: RepositoryId,
    ) -> impl Future<Output = Result<Arc<Connection>, ProtocolError>> + '_ {
        async move {
            let attempt = {
                let (attempt, replaced) = 'attempt: {
                    let mut connections = self.connections.lock();
                    let now = Instant::now();
                    if let Some(entry) = connections.get_mut(&repository) {
                        match entry {
                            Remote::Connected { connection, .. }
                                if !connection.stale.load(Ordering::Relaxed) =>
                            {
                                return Ok(connection.clone());
                            }
                            Remote::Connected {
                                connection,
                                retry_at,
                                retry,
                            } if now < *retry_at && !connection.storage_refused() => {
                                let connection = connection.clone();
                                let failed = Remote::Failed {
                                    answer: Ok(connection.clone()),
                                    retry_at: *retry_at,
                                    retry: retry.clone(),
                                };
                                *entry = failed;
                                return Ok(connection);
                            }
                            Remote::Connecting { attempt, .. } => {
                                break 'attempt (attempt.clone(), None);
                            }
                            Remote::Failed {
                                answer, retry_at, ..
                            } if now < *retry_at => return answer.clone(),
                            _ => {}
                        }
                    }
                    let (retry, replaced) = match connections.remove(&repository) {
                        Some(Remote::Failed { answer, retry, .. }) => (retry, answer.ok()),
                        Some(Remote::Connected { connection, .. }) => {
                            (connect_retry(), Some(connection))
                        }
                        _ => (connect_retry(), None),
                    };
                    let attempt = shared_connect((self.connect)(
                        self.remote_url.clone(),
                        self.identity.clone(),
                        repository,
                        self.storage_connections,
                    ));
                    connections.insert(
                        repository,
                        Remote::Connecting {
                            attempt: attempt.clone(),
                            retry,
                        },
                    );
                    (attempt, replaced)
                };
                if let Some(replaced) = replaced {
                    replaced.invalidate_all_sessions();
                }
                attempt
            };
            let connected = attempt.clone().await;
            let mut connections = self.connections.lock();
            if let Some(entry) = connections.get_mut(&repository)
                && let Remote::Connecting {
                    attempt: current,
                    retry,
                } = entry
                && current.ptr_eq(&attempt)
            {
                let mut retry = retry.clone();
                let retry_at = Instant::now() + retry.next_interval().unwrap_or_default();
                match &connected {
                    Ok(connection) => {
                        *entry = Remote::Connected {
                            connection: connection.clone(),
                            retry_at,
                            retry,
                        };
                    }
                    Err(error) if is_refusal(error) => {
                        connections.remove(&repository);
                    }
                    Err(error) => {
                        *entry = Remote::Failed {
                            answer: Err(error.clone()),
                            retry_at,
                            retry,
                        };
                    }
                }
            }
            connected
        }
    }
}

/// A remote's storage: connections per partition, each taken from the process-wide connection cache
/// by the first call that needs it, and the session pool held per partition.
#[lore_macro::test_pub]
pub struct RemoteEndpoint {
    connections: RemoteConnections,
    /// The session pool held for each partition, which calls under one correlation id share. A
    /// call under another id replaces it, as does any call once it or its connection is stale, and
    /// [`Self::clear_pools`] drops it.
    pools: PartitionSessions,
}

impl RemoteEndpoint {
    /// An endpoint for the remote at `remote_url`, connecting as `identity`, with nothing open.
    pub fn new(remote_url: impl Into<String>, identity: Option<&str>) -> Self {
        Self {
            connections: RemoteConnections::new(remote_url, identity),
            pools: PartitionSessions::default(),
        }
    }

    /// An endpoint over `connections`, holding no session pool.
    #[cfg(feature = "test-util")]
    pub fn over(connections: RemoteConnections) -> Self {
        Self {
            connections,
            pools: PartitionSessions::default(),
        }
    }

    /// The connection for calls on `partition`, as [`RemoteConnections::connection`] hands it out.
    pub fn connection(
        &self,
        partition: RepositoryId,
    ) -> impl Future<Output = Result<Arc<Connection>, ProtocolError>> + '_ {
        self.connections.connection(partition)
    }

    /// The session pool for `partition` that serves `correlation_id`: the pool held for the
    /// partition, while it serves that id over a connection that is not stale, or else the
    /// partition's connection's pool for that id ([`Connection::unpinned_session_pool`]), held in
    /// its place.
    ///
    /// The correlation id is a parameter because a lazy session resolves wherever its first
    /// operation runs, which need not be under the execution context of the call that built it.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[lore_macro::test_pub]
    #[allow(clippy::manual_async_fn)]
    pub(crate) fn session_pool<'a>(
        &'a self,
        partition: &'a RepositoryId,
        correlation_id: &'a str,
    ) -> impl Future<Output = Result<Arc<SessionPool>, ProtocolError>> + 'a {
        async move {
            if let Some(pool) = self.pools.get_live(*partition, correlation_id) {
                return Ok(pool);
            }
            let pool = self
                .connection(*partition)
                .await?
                .unpinned_session_pool(*partition, correlation_id)
                .await?;
            self.pools.hold(*partition, pool.clone());
            Ok(pool)
        }
    }

    /// A session for `partition` over [`Self::session_pool`] under the call's correlation id.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn session(
        &self,
        partition: RepositoryId,
    ) -> impl Future<Output = Result<Arc<StorageSession>, ProtocolError>> + '_ {
        async move {
            let execution = crate::lore::execution_context();
            let correlation_id = execution.globals().correlation_id.as_str();
            Ok(Arc::new(StorageSession::pooled(
                self.session_pool(&partition, correlation_id).await?,
            )))
        }
    }

    /// Has the server authorize `partition` on the partition's connection without leaving a
    /// session pinned. A partition the connection has already registered makes no wire call
    /// ([`Connection::ensure_partition_authorized`]).
    pub async fn ensure_authorized(&self, partition: RepositoryId) -> Result<(), ProtocolError> {
        let connection = self.connection(partition).await?;
        let execution = crate::lore::execution_context();
        connection
            .ensure_partition_authorized(partition, execution.globals().correlation_id.as_str())
            .await
    }

    /// The [`Admin`] handle for `partition`, as [`RemoteConnections::admin`] hands it out.
    pub fn admin(
        &self,
        partition: RepositoryId,
    ) -> impl Future<Output = Result<Arc<dyn Admin>, ProtocolError>> + '_ {
        self.connections.admin(partition)
    }

    /// Drops every session pool this endpoint holds, so their sessions stop once no call holds
    /// them.
    pub fn clear_pools(&self) {
        self.pools.clear();
    }
}

/// The back-off of a repository's connects, from the transport's reconnect delays.
fn connect_retry() -> Retry {
    retry(RECONNECT_START_DELAY, RECONNECT_MAX_DELAY, usize::MAX)
}

/// Takes the compression mode the server stated in the environment it answered the connection
/// with, unless a mode is already selected.
///
/// Applied here because connecting is what reads that environment, and because the mode lives in
/// `lore_storage`, which `lore_transport` cannot reach: it depends on the transport rather than the
/// other way around. A mode the server names that this client cannot write under is passed over,
/// the preference being advice and a mode `compress` refuses failing the first write instead.
fn take_server_compression_mode(connection: &lore_transport::Connection) {
    let Some(mode) = connection
        .environment
        .compression_mode()
        .and_then(lore_storage::writable_compression_mode)
    else {
        return;
    };
    if lore_storage::suggest_compression_mode(mode) {
        lore_debug!("Compressing payloads as the server states it prefers: {mode:?}");
    }
}
