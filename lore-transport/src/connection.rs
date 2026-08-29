// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;
use std::sync::Weak;
use std::sync::atomic::Ordering;

use lore_base::lore_debug;
use lore_base::lore_spawn_net;
use lore_base::lore_trace;
use lore_base::lore_warn;
use lore_base::runtime::shutdown_block_on;
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

/// Whether the connection was opened for a call working from credentials it
/// supplied. Part of the key so the two never share: a call that supplies none
/// is asking to be authorized the usual way, and must not end up presenting
/// another call's credential, nor hand its own store-resolved one to a call that
/// supplied its own. A boolean rather than the credential itself, because
/// rotating a supplied token must not cost a new connection -- the shared
/// [`SuppliedCredentials`] carries a rotation within one mode.
type FromSuppliedCredentials = bool;

#[allow(clippy::type_complexity)]
/// Connections are keyed by `(remote_url, identity, from_supplied_credentials)`. Storage uses per-session auth,
/// and non-storage services (revision, admin, lock) are created lazily per-repository
/// with per-repository authz tokens.
static CONNECTION_MAP: Mutex<
    Option<HashMap<(String, String, FromSuppliedCredentials), Arc<Connection>>>,
> = Mutex::new(None);

/// Whether a cached connection answers for `remote_url` in this mode, for the
/// fallback that serves a caller who has not resolved an identity yet.
///
/// The mode has to match even here: a call working from the token store must not
/// be handed a connection opened for one that supplied its own credentials, and
/// the fallback is the one path that does not compare the whole key.
/// Whether a lookup naming no identity may settle for matching on URL and mode
/// alone, rather than on the full key.
///
/// Only a call working from the token store may. The identity it will resolve is
/// deterministic for a given URL and store, so any entry under that URL is the
/// one it would have opened anyway.
///
/// A call that supplied its own credentials is the opposite case: whose
/// connection sits under that URL depends on whose token opened it, and the URL
/// says nothing about that. Reusing it would run this call against the previous
/// caller's connection and be authorized as *their* identity -- refusing to write
/// this call's credentials to it does not help, because the connection is still
/// the one returned. The identity is knowable here, since `connect_impl` resolves
/// it from the supplied token, so such a call goes and resolves it and matches on
/// the exact key instead of guessing.
fn may_match_on_url_alone(
    identity: &str,
    from_supplied_credentials: FromSuppliedCredentials,
) -> bool {
    identity.is_empty() && !from_supplied_credentials
}

fn matches_url_and_mode(
    key: &(String, String, FromSuppliedCredentials),
    remote_url: &str,
    from_supplied_credentials: FromSuppliedCredentials,
) -> bool {
    let (url, _identity, supplied) = key;
    url == remote_url && *supplied == from_supplied_credentials
}

pub fn find_connection(
    remote_url: &str,
    identity: &str,
    from_supplied_credentials: FromSuppliedCredentials,
) -> Option<Arc<Connection>> {
    let mut map = CONNECTION_MAP.lock();
    let map = map.as_mut()?;

    // An exact key match, which is the hot path once the first auth exchange has
    // cached the resolved entry. An identity-less call takes this too: it only
    // matches an entry that also resolved no identity, such as one opened against
    // a server that does not authenticate.
    let key = (
        remote_url.to_string(),
        identity.to_string(),
        from_supplied_credentials,
    );
    if let Some(connection) = map.get(&key) {
        if !connection.stale.load(Ordering::Relaxed) {
            return Some(connection.clone());
        }
        map.remove(&key);
    }

    if !may_match_on_url_alone(identity, from_supplied_credentials) {
        return None;
    }

    // Caller has no identity yet (config omits it). The resolved identity is
    // deterministic for a given url/credential store, so reuse any non-stale entry
    // keyed under the same URL. Without this, every call that omits an identity
    // re-enters `connect_impl` and re-issues `EnvironmentService/Get` even though
    // the Connection would be reused by the inner lookup after auth_exchange.
    map.iter()
        .find(|(key, c)| {
            matches_url_and_mode(key, remote_url, from_supplied_credentials)
                && !c.stale.load(Ordering::Relaxed)
        })
        .map(|(_, c)| c.clone())
}

pub fn add_connection(
    remote_url: &str,
    identity: &str,
    from_supplied_credentials: FromSuppliedCredentials,
    connection: Arc<Connection>,
) {
    let key = (
        remote_url.to_string(),
        identity.to_string(),
        from_supplied_credentials,
    );
    let mut map = CONNECTION_MAP.lock();
    if let Some(map) = map.as_mut() {
        map.insert(key, connection);
    } else {
        let mut hashmap = HashMap::new();
        hashmap.insert(key, connection);
        map.replace(hashmap);
    }
}

pub fn remove_connection(connection: Arc<Connection>) {
    let mut map = CONNECTION_MAP.lock();
    if let Some(map) = map.as_mut() {
        map.retain(|_, value| !Arc::ptr_eq(value, &connection));
    }
}

/// Time allowed for the close frames to reach the peer before shutdown proceeds without
/// them. The cost of expiring is a server-side session lingering to its idle timeout, not
/// lost data, so this is shorter than the runtime shutdown timeout it runs ahead of.
const DROP_CONNECTIONS_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn drop_connections() {
    if let Some(map) = CONNECTION_MAP.lock().take() {
        // Called from library shutdown, which is synchronous and may be on any runtime or
        // none, so `shutdown_block_on` picks how to drive this and bounds it. Calling
        // `block_in_place` here would panic on a `current_thread` runtime.
        let completed = shutdown_block_on(
            async move {
                for connection in map {
                    let _ = connection.1.cancel_connect().await;
                    // Drain in-flight streams and flush transport close frames to the peer
                    // before the runtime goes away. Without this, the server logs every
                    // outstanding stream read as a transport error on client exit.
                    connection.1.close_transport().await;
                }
            },
            DROP_CONNECTIONS_WAIT,
        );
        if !completed {
            lore_base::lore_warn!(
                "Timed out closing connections during shutdown; peers may log outstanding \
                 streams as transport errors"
            );
        }
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
    let protocol = find(protocol)
        .forward_with::<ProtocolError, _>(|| format!("remote {remote_url} is invalid"))?;

    Ok((parsed_url, protocol))
}

/// Whether a failed `session_start` settles the question for this connection's identity, so asking
/// again before the identity changes is a round trip that can only fail the same way.
///
/// Only a refusal does. Everything else — a disconnect, backpressure, an internal fault — says the
/// answer was not obtained rather than that it is no.
fn refusal_is_final(err: &ProtocolError) -> bool {
    err.is_not_authorized() || err.is_not_authenticated()
}

/// `identity_token` and `access_token` are the credentials the caller supplied
/// for this call, empty when they supplied none. A reused connection adopts
/// them, so the services it already built -- and the background token refreshers
/// -- present what this call supplied rather than a credential a previous call
/// left behind. See [`SuppliedCredentials`].
pub async fn connect(
    remote_url: &str,
    identity: &str,
    repository: RepositoryId,
    max_connections: usize,
    identity_token: &str,
    access_token: &str,
) -> Result<Arc<Connection>, ProtocolError> {
    let (remote_url, protocol) = parse(remote_url)?;

    // Try early out by reusing a known existing connection
    let identity = identity.to_string();
    let from_supplied_credentials = !identity_token.is_empty() || !access_token.is_empty();
    if let Some(connection) = find_connection(
        remote_url.as_str(),
        identity.as_str(),
        from_supplied_credentials,
    ) {
        // A match with credentials to adopt is a match on the full key -- see
        // `may_match_on_url_alone` -- so this writes to the connection belonging
        // to the identity the call acts as. The check restates that at the point
        // of the write rather than relying on the lookup for it.
        if !identity.is_empty() {
            connection.credentials.update(identity_token, access_token);
        }
        return Ok(connection);
    }

    let identity_token = identity_token.to_string();
    let access_token = access_token.to_string();
    Box::pin(async move {
        connect_impl(
            protocol,
            remote_url,
            identity,
            repository,
            max_connections,
            identity_token,
            access_token,
        )
        .await
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn connect_impl(
    protocol: Arc<dyn Protocol>,
    remote_url: Url,
    identity: String,
    repository: RepositoryId,
    max_connections: usize,
    identity_token: String,
    access_token: String,
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
    lore_debug!("Server environment config from {remote_url}: {environment:?}");

    let auth_url = environment
        .endpoint
        .as_ref()
        .and_then(|endpoint| endpoint.auth_url.clone())
        .unwrap_or_default();

    let mut identity = identity;
    if !auth_url.is_empty() {
        // Ensure we are authenticated if there is an auth url defined in the environment
        if identity.is_empty() {
            let (_, _, resolved) = auth_exchange(
                &auth_url,
                &remote_domain,
                "",
                repository,
                &identity_token,
                &access_token,
            )
            .await;
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
        } else if access_token.is_empty() {
            // With an access token there is no authentication token to find:
            // the caller authorizes with what they supplied, and the services
            // that need an authentication token fail where they use it rather
            // than failing the whole connection here.
            lore_credential::token_store::load_user_token(
                &auth_url,
                &identity,
                lore_credential::token_store::tokens_only_for_recipient_domain(
                    remote_domain.clone(),
                ),
                &identity_token,
                &access_token,
            )
            .await
            .map_err(|err| {
                // A configured identity with no stored token is the normal
                // logged-out state (login saves the identity to the repo
                // config; logout removes only the tokens) — report it as
                // NotAuthenticated rather than an internal failure.
                if err.is_token_not_found() {
                    lore_debug!("No token stored for identity {identity} at {auth_url}");
                    ProtocolError::from(lore_base::error::NotAuthenticated)
                } else {
                    ProtocolError::internal_with_context(err, "loading user token")
                }
            })?;
        }
    }

    let from_supplied_credentials = !identity_token.is_empty() || !access_token.is_empty();
    if let Some(connection) = find_connection(
        remote_url.as_str(),
        identity.as_str(),
        from_supplied_credentials,
    ) {
        // As above. An unauthenticated server leaves the identity unresolved even
        // here, and that entry is shared with every other call that resolved none.
        if !identity.is_empty() {
            connection
                .credentials
                .update(&identity_token, &access_token);
        }
        return Ok(connection);
    }

    let credentials = Arc::new(SuppliedCredentials::new(&identity_token, &access_token));

    let connection = Arc::new(Connection {
        remote_url: remote_url.clone(),
        auth_url: auth_url.clone(),
        identity: identity.clone(),
        credentials: credentials.clone(),
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

    let connect_task = lore_spawn_net!({
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
                    let (identity_token, access_token) = connection.credentials().tokens();
                    if let Err(err) = auth::exchange::exchange(
                        &auth_url,
                        &identity,
                        repository,
                        remote_domain,
                        &identity_token,
                        &access_token,
                    )
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
                let handle = lore_spawn_net!(async move {
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
                            connection.credentials(),
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
                    let handle = lore_spawn_net!(async move {
                        let _environment_client = environment_client;
                        let result = connection
                            .protocol
                            .revision(
                                Arc::downgrade(&connection),
                                revision_url.as_str(),
                                auth_url.as_str(),
                                identity.as_str(),
                                repository,
                                connection.credentials(),
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
                    let handle = lore_spawn_net!(async move {
                        let _environment_client = environment_client;
                        let result = connection
                            .protocol
                            .lock(
                                Arc::downgrade(&connection),
                                lock_url.as_str(),
                                auth_url.as_str(),
                                identity.as_str(),
                                repository,
                                connection.credentials(),
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
                let handle = lore_spawn_net!(async move {
                    let _environment_client = environment_client;
                    let result = connection
                        .protocol
                        .repository(
                            Arc::downgrade(&connection),
                            // see URC_GREP_TOKEN_AUTH_NOTE regarding token warming and security
                            repository_service_url.as_str(),
                            auth_url.as_str(),
                            identity.as_str(),
                            connection.credentials(),
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

    add_connection(
        remote_url.as_str(),
        identity.as_str(),
        from_supplied_credentials,
        connection.clone(),
    );

    Ok(connection)
}

/// The credentials a call supplied, shared by everything a connection builds.
///
/// Read at the moment of use rather than captured when the connection opened,
/// because a long-lived process rotates tokens while the connection outlives any
/// one of them, and both transports check authorization against whatever the
/// client presents at the time: gRPC verifies the header on every request, and
/// storage verifies at `session_start`. A credential snapshot taken at open
/// would go stale and could not be replaced without dropping the connection.
///
/// Empty strings mean the caller supplied nothing and credentials are resolved
/// the usual way.
#[derive(Debug)]
pub struct SuppliedCredentials {
    tokens: parking_lot::RwLock<(String, String)>,
    /// Bumped whenever the pair changes, so the background refreshers can act on
    /// a rotation instead of waiting out their interval. A `watch` rather than a
    /// `Notify` because a connection has one refresher per authorization it
    /// holds -- the repository service, each repository, each custom resource --
    /// and every one of them has to see the change. `notify_one` would wake a
    /// single refresher, and `notify_waiters` would miss any that happened to be
    /// mid-exchange; a `watch` receiver keeps the pending change until it is read.
    generation: tokio::sync::watch::Sender<u64>,
}

impl Default for SuppliedCredentials {
    /// No credentials supplied: everything is resolved the usual way.
    fn default() -> Self {
        Self::new("", "")
    }
}

impl SuppliedCredentials {
    pub fn new(identity_token: &str, access_token: &str) -> Self {
        Self {
            tokens: parking_lot::RwLock::new((
                identity_token.to_string(),
                access_token.to_string(),
            )),
            generation: tokio::sync::watch::Sender::new(0),
        }
    }

    /// The credentials to derive a token from, and a handle reporting when they
    /// are replaced.
    ///
    /// Taken together on purpose. Deriving a token is slow -- it can take an
    /// exchange with the auth service -- and the connection is already
    /// discoverable while that runs, so a caller can rotate the credentials
    /// in between. A `watch` receiver counts the value present at subscription as
    /// seen, so subscribing after reading would mark that rotation seen and leave
    /// the token derived from the stale pair standing until the next scheduled
    /// refresh. Subscribing under the read lock closes the window: `update` takes
    /// the write lock, so it cannot land between the two.
    pub fn tokens_and_signal(&self) -> ((String, String), tokio::sync::watch::Receiver<u64>) {
        let tokens = self.tokens.read();
        let rotated = self.generation.subscribe();
        (tokens.clone(), rotated)
    }

    /// Whether a caller supplied these credentials, as opposed to leaving them
    /// to be resolved from the token store. Stable for the life of a connection:
    /// a call that supplies nothing never reaches a connection opened for one
    /// that did, and `update` ignores an empty pair.
    pub fn from_supplied_credentials(&self) -> bool {
        let tokens = self.tokens.read();
        !tokens.0.is_empty() || !tokens.1.is_empty()
    }

    /// The credentials to use now, as `(identity_token, access_token)`.
    pub fn tokens(&self) -> (String, String) {
        self.tokens.read().clone()
    }

    /// Adopts the credentials a later call supplied, so the services this
    /// connection already built stop presenting a credential the caller has
    /// replaced. The most recent call to supply any wins; a call that supplies
    /// none leaves what is there, since it is asking for the usual resolution
    /// rather than for the connection to forget what it was given.
    pub fn update(&self, identity_token: &str, access_token: &str) {
        if identity_token.is_empty() && access_token.is_empty() {
            return;
        }
        let mut tokens = self.tokens.write();
        if tokens.0 != identity_token || tokens.1 != access_token {
            lore_debug!("Adopting the credentials supplied for this call");
            *tokens = (identity_token.to_string(), access_token.to_string());
            drop(tokens);
            // Only on a real change: a call repeating the credentials it already
            // supplied costs nothing, while a rotation reaches every refresher.
            self.generation.send_modify(|generation| *generation += 1);
        }
    }
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
    /// The credentials supplied for the call in progress, shared with every
    /// service this connection built so a later call's fresher ones take effect
    /// without reconnecting.
    credentials: Arc<SuppliedCredentials>,
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
    session_cache: dashmap::DashMap<(Partition, String), Arc<SessionPool>>,
    connector: tokio::sync::Mutex<Option<Connector>>,
    pub stale: std::sync::atomic::AtomicBool,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // We hold `&mut self`, so take the connector without locking. Awaiting the
        // aborted setup task inline would block whichever worker runs this Drop, and it
        // can run on a net worker, so hand the await to a task instead. Pinned to net
        // rather than following the dropper: the handle belongs to a net task, and the
        // placement should not depend on who happens to drop the connection.
        let Some(connector) = self.connector.get_mut().take() else {
            return;
        };
        let setup_handle = self.abort_connector(connector);
        lore_spawn_net!(async move {
            let _ = setup_handle.await;
        });
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

    /// The credentials supplied for the call in progress. Hand this to anything
    /// that authorizes later, so it reads them at the time it needs them.
    pub fn credentials(&self) -> &Arc<SuppliedCredentials> {
        &self.credentials
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
        let Some(connector) = self.connector.lock().await.take() else {
            return Ok(());
        };
        let setup_handle = self.abort_connector(connector);
        let _ = setup_handle.await;
        Ok(())
    }

    /// Aborts the connector's setup and subtasks and fails every readiness gate.
    /// Returns the setup task handle so the caller can await it (or, from `Drop`,
    /// await it off the blocking path). Everything here is synchronous.
    fn abort_connector(&self, connector: Connector) -> JoinHandle<()> {
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
        connector.setup_handle
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

    /// Creates or reuses a `SessionPool` for the given partition and correlation
    /// ID, returning a round-robin-picked session from it. The pool is pinned in
    /// the connection's session cache so the `Weak` in `StorageConnector` stays
    /// upgradeable for subsequent calls within the same command, keeping every
    /// session in the pool alive without start/stop churn. Call
    /// `release_session()` when the API call completes to release the pool.
    pub async fn session(
        self: &Arc<Self>,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<Arc<StorageSession>, ProtocolError> {
        Ok(self.session_pool(partition, correlation_id).await?.pick())
    }

    /// The pool of sessions for `(partition, correlation_id)`, pinned here so the
    /// connector's `Weak` to it stays upgradeable.
    ///
    /// A caller that needs one session per unit of work — a commit needs one per
    /// file — should hold the pool and [`pick`](SessionPool::pick) from it rather
    /// than calling [`session`](Self::session) each time. Every call owns a key to
    /// look the pool up by, twice over, so a caller in a loop allocates two strings
    /// an iteration and every iteration hashes to the same shard of both maps.
    pub async fn session_pool(
        self: &Arc<Self>,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<Arc<SessionPool>, ProtocolError> {
        self.ensure_storage_connected().await?;
        let connector = self.storage_connector()?;
        let pool = connector
            .session_pool(partition, correlation_id, self.clone())
            .await?;
        self.pin_session_pool(partition, correlation_id, &pool);
        Ok(pool)
    }

    /// Pin `pool` for `(partition, correlation_id)`, unless that is where it is
    /// pinned already, so a caller in a loop takes the shard's read lock rather
    /// than its write lock.
    ///
    /// The read guard is released before the insert: holding one across a write to
    /// the same map deadlocks.
    fn pin_session_pool(
        &self,
        partition: Partition,
        correlation_id: &str,
        pool: &Arc<SessionPool>,
    ) {
        let key = (partition, correlation_id.to_string());
        let pinned = self
            .session_cache
            .get(&key)
            .is_some_and(|entry| Arc::ptr_eq(entry.value(), pool));
        if !pinned {
            self.session_cache.insert(key, pool.clone());
        }
    }

    /// Unpin a cached session pool so its `Weak` in `StorageConnector` can
    /// expire. The pool's `Drop` releases every `Arc<StorageSession>` it owns,
    /// each of which sends `session_stop` to the server.
    pub fn release_session(&self, partition: Partition, correlation_id: &str) {
        self.session_cache
            .remove(&(partition, correlation_id.to_string()));
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

    /// Ensure the server's per-connection `authorized_repos` set contains `partition`,
    /// without leaving a pool pinned. Fast-paths via the connector's
    /// `authorized_partitions` cache: if a previous `session_start` already registered
    /// `partition` on every underlying connection, no wire calls happen. Otherwise a
    /// fresh pool is started (which fans `session_start` across all connections in
    /// parallel) and immediately released; the server keeps `authorized_repos` permanent
    /// for the connection's lifetime, so the registration outlives the sessions.
    pub async fn ensure_partition_authorized(
        self: &Arc<Self>,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<(), ProtocolError> {
        self.ensure_storage_connected().await?;
        let connector = self.storage_connector()?;
        if connector.is_partition_authorized(partition) {
            return Ok(());
        }
        if connector.is_partition_refused(partition) {
            return Err(ProtocolError::from(lore_base::error::NotAuthorized));
        }
        // Drive the slow path through `session_pool()` so the `authorized_partitions`
        // insert and the standard race-resolution / pool bookkeeping all run. We
        // immediately drop the returned pool and release the cache entry — the call's
        // only purpose was to register authz, not to keep a live session.
        match self.session_pool(partition, correlation_id).await {
            Ok(_pool) => {
                self.release_session(partition, correlation_id);
                Ok(())
            }
            Err(err) => {
                if refusal_is_final(&err) {
                    connector.mark_partition_refused(partition);
                }
                Err(err)
            }
        }
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
                &self.credentials,
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
                &self.credentials,
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
                &self.credentials,
            )
            .await?;
        self.lock.insert(repository, lock.clone());
        Ok(lock)
    }

    pub async fn connect_module(&self, module: RepositoryId) -> Result<Arc<Self>, ProtocolError> {
        // TODO(vri): UCS-19226 - Links: Connection reuse for already connected links
        let (identity_token, access_token) = self.credentials.tokens();
        connect(
            self.remote_url.as_str(),
            self.identity.as_str(),
            module,
            MAX_STORAGE_CONNECTIONS,
            &identity_token,
            &access_token,
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
        partition: Partition,
        _index: usize,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        quic::storage(
            connection,
            remote_url,
            auth_url,
            identity,
            partition,
            credentials,
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
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Revision>, ProtocolError> {
        grpc::revision(
            connection,
            remote_url,
            auth_url,
            identity,
            repository,
            credentials,
        )
        .await
    }

    async fn repository(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Repository>, ProtocolError> {
        grpc::repository(connection, remote_url, auth_url, identity, credentials).await
    }

    async fn admin(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        grpc::admin(
            connection,
            remote_url,
            auth_url,
            identity,
            repository,
            credentials,
        )
        .await
    }

    async fn lock(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Lock>, ProtocolError> {
        grpc::lock(
            connection,
            remote_url,
            auth_url,
            identity,
            repository,
            credentials,
        )
        .await
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
        partition: Partition,
        index: usize,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        grpc::storage(
            connection,
            remote_url,
            auth_url,
            identity,
            partition,
            index,
            credentials,
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
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Revision>, ProtocolError> {
        grpc::revision(
            connection,
            remote_url,
            auth_url,
            identity,
            repository,
            credentials,
        )
        .await
    }

    async fn repository(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Repository>, ProtocolError> {
        grpc::repository(connection, remote_url, auth_url, identity, credentials).await
    }

    async fn admin(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        grpc::admin(
            connection,
            remote_url,
            auth_url,
            identity,
            repository,
            credentials,
        )
        .await
    }

    async fn lock(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Lock>, ProtocolError> {
        grpc::lock(
            connection,
            remote_url,
            auth_url,
            identity,
            repository,
            credentials,
        )
        .await
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

    /// A long-lived process rotates the credentials it supplies. The connection
    /// outlives any one of them, so the services it already built have to see
    /// the newest ones -- the server checks gRPC authorization on every request
    /// and storage authorization at each session start, so what matters is what
    /// the client presents at the time, not what it was given when it connected.
    /// The fallback for a caller with no identity yet must not cross modes: a
    /// call working from the token store would otherwise be handed a connection
    /// opened for one that supplied its own credentials, and be authorized by
    /// them. Same user either way, but each asked to be authorized a particular
    /// way.
    #[test]
    fn the_no_identity_fallback_does_not_cross_credential_modes() {
        let remote = "lores://mode-isolation.test.invalid";
        let stored = (remote.to_string(), "alice".to_string(), false);
        let supplied = (remote.to_string(), "alice".to_string(), true);

        assert!(matches_url_and_mode(&stored, remote, false));
        assert!(matches_url_and_mode(&supplied, remote, true));

        assert!(
            !matches_url_and_mode(&supplied, remote, false),
            "a store-mode call must not be given a supplied-credential connection"
        );
        assert!(
            !matches_url_and_mode(&stored, remote, true),
            "a supplied-credential call must not be given a store-mode connection"
        );

        assert!(!matches_url_and_mode(
            &stored,
            "lores://elsewhere.invalid",
            false
        ));
    }

    /// A call that supplied its own credentials must never be matched on URL
    /// alone.
    ///
    /// Which connection sits under a URL in that mode depends on whose token
    /// opened it. If Alice's supplied-credential connection is cached and a
    /// caller then supplies Bob's token, matching on the URL would run Bob's call
    /// against Alice's connection and authorize it as Alice -- declining to write
    /// Bob's credentials onto it does not help, since it is still the connection
    /// the call gets. Such a call resolves its identity from the token it
    /// supplied and matches the full key instead.
    #[test]
    fn a_call_supplying_credentials_is_never_matched_on_url_alone() {
        assert!(
            !may_match_on_url_alone("", true),
            "an identity-less supplied-credential call must resolve its identity, \
             not borrow whichever connection shares the URL"
        );
        assert!(!may_match_on_url_alone("bob", true));

        // The store-mode shortcut stands: the identity such a call resolves is
        // fixed by the URL and the store, so the entry there is its own.
        assert!(may_match_on_url_alone("", false));
        assert!(
            !may_match_on_url_alone("alice", false),
            "a named identity is matched on the full key either way"
        );
    }

    /// The mode a connection was opened in, which keys it apart from the other.
    #[test]
    fn credentials_report_whether_a_caller_supplied_them() {
        assert!(!SuppliedCredentials::default().from_supplied_credentials());
        assert!(SuppliedCredentials::new("an-identity-token", "").from_supplied_credentials());
        assert!(SuppliedCredentials::new("", "an-access-token").from_supplied_credentials());
    }

    #[test]
    fn a_later_call_replaces_the_credentials_the_services_read() {
        let credentials = SuppliedCredentials::new("first-identity", "first-access");
        assert_eq!(
            credentials.tokens(),
            ("first-identity".to_string(), "first-access".to_string())
        );

        credentials.update("second-identity", "second-access");

        assert_eq!(
            credentials.tokens(),
            ("second-identity".to_string(), "second-access".to_string()),
            "the newest credentials are the ones handed out"
        );
    }

    /// A rotation has to reach the refreshers, or the tokens a service client
    /// already presents would go on being the replaced ones until the next
    /// scheduled refresh.
    ///
    /// It has to reach a refresher whose signal was taken alongside the earlier
    /// credentials too: deriving a token from them can take an exchange with the
    /// auth service, and the connection is discoverable while that runs, so this
    /// is the rotation most in need of reporting.
    #[test]
    fn replacing_the_credentials_signals_the_refreshers() {
        let credentials = SuppliedCredentials::new("first-identity", "");
        let ((identity_token, _), rotated) = credentials.tokens_and_signal();
        assert_eq!(identity_token, "first-identity");
        assert!(
            !rotated.has_changed().expect("the sender outlives this"),
            "nothing to report before a rotation"
        );

        credentials.update("second-identity", "");

        assert!(
            rotated.has_changed().expect("the sender outlives this"),
            "the refreshers must be told the credentials were replaced"
        );
    }

    /// A call repeating the credentials already in place is not a rotation, and
    /// must not send every refresher off to re-derive tokens that have not
    /// changed.
    #[test]
    fn repeating_the_same_credentials_signals_nothing() {
        let credentials = SuppliedCredentials::new("an-identity-token", "an-access-token");
        let (_, rotated) = credentials.tokens_and_signal();

        credentials.update("an-identity-token", "an-access-token");
        assert!(!rotated.has_changed().expect("the sender outlives this"));

        // Nor does a call that supplies nothing at all.
        credentials.update("", "");
        assert!(!rotated.has_changed().expect("the sender outlives this"));
    }

    /// A call that supplies nothing is asking for the usual resolution, not for
    /// the connection to forget what it was given. Clearing here would strip the
    /// credential from every service the connection already built.
    #[test]
    fn a_call_supplying_nothing_leaves_the_credentials_alone() {
        let credentials = SuppliedCredentials::new("an-identity-token", "");

        credentials.update("", "");

        assert_eq!(
            credentials.tokens(),
            ("an-identity-token".to_string(), String::new())
        );
    }

    #[test]
    fn no_credentials_supplied_reads_as_empty() {
        let credentials = SuppliedCredentials::default();
        assert_eq!(credentials.tokens(), (String::new(), String::new()));
    }
    use crate::MatchedProtocolError;

    /// A copy naming a source partition asks whether it may before it tries, and a `false` costs a
    /// `session_start`. Latching the answer bounds that at one per partition — but only for the
    /// failure that is actually about the claim, or a disconnect would disable a legitimate source
    /// for as long as the connection lives.
    mod refusal_is_final {
        use super::*;

        #[test]
        fn a_refusal_settles_it() {
            assert!(super::super::refusal_is_final(&ProtocolError::from(
                NotAuthorized
            )));
            assert!(super::super::refusal_is_final(&ProtocolError::from(
                NotAuthenticated
            )));
        }

        #[test]
        fn anything_else_is_worth_asking_again() {
            for err in [
                ProtocolError::from(Disconnected),
                ProtocolError::from(SlowDown),
                ProtocolError::from(Maintenance),
                ProtocolError::from(NotFound),
                ProtocolError::internal("transport blew up"),
            ] {
                assert!(
                    !super::super::refusal_is_final(&err),
                    "{err:?} says the answer was not obtained, not that it is no"
                );
            }
        }
    }

    /// The refusal is remembered so the round trip is paid once, and retired by the success that
    /// proves it no longer holds.
    #[test]
    fn a_refusal_lasts_until_a_session_start_succeeds() {
        let connector = crate::session::StorageConnector::new(Vec::new());
        let partition = Partition::from([0x7au8; 16]);

        assert!(!connector.is_partition_refused(partition));

        connector.mark_partition_refused(partition);
        assert!(connector.is_partition_refused(partition));
        assert!(!connector.is_partition_authorized(partition));

        connector.mark_partition_authorized(partition);
        assert!(!connector.is_partition_refused(partition));
        assert!(connector.is_partition_authorized(partition));
    }

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
