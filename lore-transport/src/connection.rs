// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Once;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::env::AUTH_PATH_VAR;
use lore_base::error::is_refusal;
use lore_base::lore_debug;
use lore_base::lore_spawn_net;
use lore_base::lore_spawn_net_nocontext;
use lore_base::lore_trace;
use lore_base::lore_warn;
use lore_base::runtime::shutdown_block_on;
use lore_base::types::*;
use lore_error_set::prelude::*;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::task::AbortOnDropHandle;
use url::Url;

use crate::auth;
use crate::auth::AuthEndpoint;
use crate::auth::AuthMode;
use crate::auth::exchange::auth_exchange_for_stored_identity;
use crate::auth::user_directory::UserDirectory;
use crate::error::ProtocolError;
use crate::grpc;
use crate::quic;
use crate::session::PartitionSessions;
use crate::session::SessionPool;
use crate::session::StorageConnector;
use crate::session::StorageSession;
use crate::traits::*;
use crate::types::*;

/// Most storage connections a command opens.
///
/// The server paces each QUIC connection on its own, so one connection to a distant remote
/// delivers a few MiB/s however many requests it carries, and bulk reads scale with the number
/// of connections they are spread over. They connect in parallel, and storage is ready once the
/// slowest of them has.
pub static MAX_STORAGE_CONNECTIONS: usize = 10;
pub static DEFAULT_PROTOCOL: &str = "lores";

/// The storage connections a command opens to a remote when it asks for `requested`, and the
/// connections its network threads are sized for: one in a process that names the machine
/// server's socket, which carries its network traffic, else `requested`, with 0 for the default of
/// one, and at most [`MAX_STORAGE_CONNECTIONS`].
pub fn storage_connection_count(requested: usize) -> usize {
    if crate::uds::route().is_some() {
        1
    } else {
        requested.clamp(1, MAX_STORAGE_CONNECTIONS)
    }
}

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
        let _ = add(crate::uds::SCHEME, Arc::new(UdsProtocol::default()));
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

/// The token store a call reads its credentials from, by the directory naming it: empty for the
/// per-user default. A process carrying out calls for other processes reads the store each caller
/// names, so what is cached from a store is keyed by it.
pub(crate) type CredentialStore = String;

/// What a connection is cached under: the remote URL, the identity, whether the caller supplied
/// its own credentials, the token store the connection authenticated from, and the auth mode it
/// was opened in. The mode is part of it because the lookup runs before the environment is
/// fetched, so the path the mode resolves to is not yet known; a call in another mode opens its
/// own connection rather than being served one that may be on the other path.
#[lore_macro::test_pub]
type ConnectionKey = (
    String,
    String,
    FromSuppliedCredentials,
    CredentialStore,
    AuthMode,
);

/// Connections are keyed by [`ConnectionKey`]. Storage uses per-session auth, and non-storage
/// services (revision, admin, lock) are created lazily per-repository with per-repository authz
/// tokens.
static CONNECTION_MAP: Mutex<Option<HashMap<ConnectionKey, Arc<Connection>>>> = Mutex::new(None);

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
/// deterministic for a given URL and store, so any entry under that URL opened
/// from the same store is the one it would have opened anyway.
///
/// A call that supplied its own credentials is the opposite case: whose
/// connection sits under that URL depends on whose token opened it, and the URL
/// says nothing about that. Reusing it would run this call against the previous
/// caller's connection and be authorized as *their* identity -- refusing to write
/// this call's credentials to it does not help, because the connection is still
/// the one returned. The identity is knowable here, since `connect_impl` resolves
/// it from the supplied token, so such a call goes and resolves it and matches on
/// the exact key instead of guessing.
#[lore_macro::test_pub]
fn may_match_on_url_alone(
    identity: &str,
    from_supplied_credentials: FromSuppliedCredentials,
) -> bool {
    identity.is_empty() && !from_supplied_credentials
}

#[lore_macro::test_pub]
fn matches_url_mode_and_store(
    key: &ConnectionKey,
    remote_url: &str,
    from_supplied_credentials: FromSuppliedCredentials,
    credential_store: &str,
    auth_mode: AuthMode,
) -> bool {
    let (url, _identity, supplied, store, mode) = key;
    url == remote_url
        && *supplied == from_supplied_credentials
        && store == credential_store
        && *mode == auth_mode
}

/// The [`CredentialStore`] this call reads.
pub(crate) fn credential_store() -> CredentialStore {
    lore_base::env::var(AUTH_PATH_VAR).unwrap_or_default()
}

/// The live connection cached for the key, or for the URL alone where the call has no identity
/// yet, recorded as taken for the idle checks. A stale connection cached under the key is taken out
/// and [retired](retire).
pub fn find_connection(
    remote_url: &str,
    identity: &str,
    from_supplied_credentials: FromSuppliedCredentials,
    auth_mode: AuthMode,
) -> Option<Arc<Connection>> {
    let mut map = CONNECTION_MAP.lock();
    let map = map.as_mut()?;

    // An exact key match, which is the hot path once the first auth exchange has
    // cached the resolved entry. An identity-less call takes this too: it only
    // matches an entry that also resolved no identity, such as one opened against
    // a server that does not authenticate.
    let credential_store = credential_store();
    let key = (
        remote_url.to_string(),
        identity.to_string(),
        from_supplied_credentials,
        credential_store,
        auth_mode,
    );
    if let Some(connection) = map.get(&key) {
        if !connection.stale.load(Ordering::Relaxed) {
            connection.record_taken();
            return Some(connection.clone());
        }
        if let Some(stale) = map.remove(&key) {
            retire(&stale);
        }
    }

    if !may_match_on_url_alone(identity, from_supplied_credentials) {
        return None;
    }

    // Caller has no identity yet (config omits it). The resolved identity is
    // deterministic for a given url/credential store, so reuse any non-stale entry
    // keyed under the same URL and store. Without this, every call that omits an
    // identity re-enters `connect_impl` and re-issues `EnvironmentService/Get` even
    // though the Connection would be reused by the inner lookup after auth_exchange.
    map.iter()
        .find(|(candidate, c)| {
            matches_url_mode_and_store(
                candidate,
                remote_url,
                from_supplied_credentials,
                &key.3,
                auth_mode,
            ) && !c.stale.load(Ordering::Relaxed)
        })
        .map(|(_, c)| {
            c.record_taken();
            c.clone()
        })
}

/// Caches `connection` and returns it, unless a live connection is cached under its key already,
/// which is returned instead: two calls that connect at once share the first. A stale connection
/// cached under the key is replaced and [retired](retire).
pub fn add_connection(
    remote_url: &str,
    identity: &str,
    from_supplied_credentials: FromSuppliedCredentials,
    auth_mode: AuthMode,
    connection: Arc<Connection>,
) -> Arc<Connection> {
    let key = (
        remote_url.to_string(),
        identity.to_string(),
        from_supplied_credentials,
        credential_store(),
        auth_mode,
    );
    let mut map = CONNECTION_MAP.lock();
    match map.get_or_insert_with(HashMap::new).entry(key) {
        Entry::Occupied(cached) if !cached.get().stale.load(Ordering::Relaxed) => {
            cached.get().record_taken();
            cached.get().clone()
        }
        Entry::Occupied(mut cached) => {
            retire(&cached.insert(connection.clone()));
            connection
        }
        Entry::Vacant(vacant) => vacant.insert(connection).clone(),
    }
}

/// Takes `connection` out of the cache and [retires](retire) it.
pub fn remove_connection(connection: Arc<Connection>) {
    if let Some(map) = CONNECTION_MAP.lock().as_mut() {
        map.retain(|_, value| !Arc::ptr_eq(value, &connection));
    }
    retire(&connection);
}

/// Frees a connection taken out of the cache once no call holds it, which closes its transport. The
/// sessions of the pools it pins hold it, so this drops the pins and stops it pinning again. The
/// pools stay valid for the calls that hold them.
fn retire(connection: &Connection) {
    connection.retired.store(true, Ordering::Relaxed);
    connection.session_cache.clear();
}

/// Time allowed for closing connections to flush their close frames to the peer. Expiring leaves
/// the server's session to its idle timeout rather than losing data, so this is shorter than the
/// runtime shutdown timeout. A connection whose close expires is closed without the wait when it
/// is dropped.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

pub fn drop_connections() {
    if let Some(map) = CONNECTION_MAP.lock().take() {
        // Called from library shutdown, which is synchronous and may be on any runtime or
        // none, so `shutdown_block_on` picks how to drive this and bounds it. Calling
        // `block_in_place` here would panic on a `current_thread` runtime.
        let completed = shutdown_block_on(
            async move {
                for connection in map {
                    // Flush the transport close frames to the peer before the runtime goes away.
                    // Without this, the server logs every outstanding stream read as a transport
                    // error on client exit. Retired after the close, so the session stops that
                    // dropping its pins sends find no streams to hold the close back.
                    connection.1.close().await;
                    retire(&connection.1);
                }
            },
            CLOSE_WAIT,
        );
        if !completed {
            lore_base::lore_warn!(
                "Timed out closing connections during shutdown; peers may log outstanding \
                 streams as transport errors"
            );
        }
    }
}

/// How often [`close_idle_connections`] checks for connections that no call has used for `idle`: a
/// tenth of `idle`, and from one to thirty seconds.
#[lore_macro::test_pub]
fn idle_check_interval(idle: Duration) -> Duration {
    (idle / 10).clamp(Duration::from_secs(1), Duration::from_secs(30))
}

/// Takes out of `connections` each connection that no call has used for `idle` at `now`, counting
/// the reference `connections` holds as none.
#[lore_macro::test_pub]
fn take_unused(
    connections: &mut HashMap<ConnectionKey, Arc<Connection>>,
    idle: Duration,
    now: Instant,
) -> Vec<Arc<Connection>> {
    let mut unused = Vec::new();
    connections.retain(|_, connection| {
        let keep = !connection.unused_for(idle, now);
        if !keep {
            unused.push(connection.clone());
        }
        keep
    });
    unused
}

/// Closes each cached connection that no call has used for `idle`, on a task of the network
/// runtime that runs until the returned handle is dropped. A connection starts to close no sooner
/// than `idle` after no call holds it, and at most two [`idle_check_interval`]s later. A call after
/// that opens a new one. The checks and the closes they start belong to no call, so they run
/// without the `LORE_CONTEXT` of the call that starts them, which would report them as its own.
///
/// Until then the connection stays open, kept alive on both ends by its keep-alives. Closing it
/// takes it out of the cache, drops the session pools it pins and closes its storage connections,
/// so the server releases them at once. Its gRPC connection closes once no other connection to the
/// same host uses it.
pub fn close_idle_connections(idle: Duration) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(lore_spawn_net_nocontext!(check_idle_connections(idle)))
}

/// Checks for connections that no call has used for `idle` every [`idle_check_interval`], and
/// closes each on a task of its own, so a slow close holds back no check.
async fn check_idle_connections(idle: Duration) {
    let mut checks = tokio::time::interval(idle_check_interval(idle));
    checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        checks.tick().await;
        let unused = match CONNECTION_MAP.lock().as_mut() {
            Some(connections) => take_unused(connections, idle, Instant::now()),
            None => continue,
        };
        for connection in unused {
            lore_spawn_net_nocontext!(close_idle(connection, idle));
        }
    }
}

/// Closes `connection`, which no call has used for `idle`, allowing it [`CLOSE_WAIT`].
async fn close_idle(connection: Arc<Connection>, idle: Duration) {
    lore_debug!(
        "Closing the connection to {}, which no command has used for {} s",
        connection.remote_url,
        idle.as_secs()
    );
    if tokio::time::timeout(CLOSE_WAIT, connection.close_unused())
        .await
        .is_err()
    {
        lore_warn!(
            "Timed out closing the idle connection to {}",
            connection.remote_url
        );
    }
}

pub fn parse(remote_url: &str) -> Result<(Url, Arc<dyn Protocol>), ProtocolError> {
    // A repository created without a remote URL has nothing to reach, which is a
    // configuration state rather than a fault: report it as `NoRemote` so callers can
    // tell it apart from a remote that is configured but unreachable (`Disconnected`).
    if remote_url.is_empty() {
        return Err(ProtocolError::from(lore_base::error::NoRemote));
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
    if parsed_url.scheme() == crate::uds::SCHEME {
        crate::uds::socket()?;
    }

    Ok((parsed_url, protocol))
}

/// Whether a connection to `url` authenticates the server, which it does when the scheme ends in
/// `s`: QUIC then validates the server certificate and gRPC uses TLS.
pub(crate) fn authenticates_server(url: &Url) -> bool {
    url.scheme().ends_with('s')
}

/// Refuses an `environment` that advertises a storage, revision, lock, repository or notification
/// endpoint that does not authenticate the server when `remote_url` does.
#[lore_macro::test_pub]
fn check_advertised_endpoints(
    remote_url: &Url,
    environment: &EnvironmentConfig,
) -> Result<(), ProtocolError> {
    if !authenticates_server(remote_url) {
        return Ok(());
    }
    let remote = remote_url.as_str();
    for (service, url) in [
        ("storage", environment.storage_url(remote)),
        ("revision", environment.revision_url(remote)),
        ("lock", environment.lock_url(remote)),
        ("repository", environment.repository_url(remote)),
        ("notification", environment.notification_url(remote)),
    ] {
        if url != remote && !Url::parse(url).is_ok_and(|url| authenticates_server(&url)) {
            return Err(ProtocolError::from(lore_base::error::NotSupported {
                operation: format!(
                    "connecting to the {service} endpoint {url} advertised by {remote_url}, which \
                     does not authenticate the server"
                ),
            }));
        }
    }
    Ok(())
}

/// Whether a failed `session_start` settles the question for this connection's identity, so asking
/// again before the identity changes is a round trip that can only fail the same way.
///
#[lore_macro::test_pub]
/// Only a refusal does. Everything else — a disconnect, backpressure, an internal fault — says the
/// answer was not obtained rather than that it is no.
fn refusal_is_final(err: &ProtocolError) -> bool {
    is_refusal(err)
}

/// `identity_token` and `access_token` are the credentials the caller supplied
/// for this call, empty when they supplied none. A reused connection adopts
/// them, so the services it already built -- and the background token refreshers
/// -- present what this call supplied rather than a credential a previous call
/// left behind. See [`SuppliedCredentials`].
///
/// `auth_mode` picks the authentication path if the server advertises both
/// its gRPC auth service and an OIDC issuer. See
/// [`auth::authentication::select`]. Connections are cached per mode, so a
/// call in another mode opens its own rather than reusing one that may be on
/// the other path.
pub async fn connect(
    remote_url: &str,
    identity: &str,
    repository: RepositoryId,
    max_connections: usize,
    identity_token: &str,
    access_token: &str,
    auth_mode: AuthMode,
) -> Result<Arc<Connection>, ProtocolError> {
    let (remote_url, protocol) = parse(remote_url)?;

    // Try early out by reusing a known existing connection
    let identity = identity.to_string();
    let from_supplied_credentials = !identity_token.is_empty() || !access_token.is_empty();
    if let Some(connection) = find_connection(
        remote_url.as_str(),
        identity.as_str(),
        from_supplied_credentials,
        auth_mode,
    ) {
        return Ok(shared_with_call(
            connection,
            &identity,
            identity_token,
            access_token,
        ));
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
            auth_mode,
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
    auth_mode: AuthMode,
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
    check_advertised_endpoints(&remote_url, &environment)?;

    let auth_endpoint = auth::authentication::select(auth_mode, &environment)
        .forward::<ProtocolError>("selecting the authentication path")?
        .map_or_else(AuthEndpoint::none, |selected| {
            lore_debug!(
                "Authenticating over the {:?} path at {}",
                selected.path,
                selected.endpoint
            );
            selected.endpoint
        });

    let mut identity = identity;
    if !auth_endpoint.is_none() {
        // Ensure we are authenticated if there is an auth url defined in the environment
        if identity.is_empty() {
            let (_, _, resolved) =
                auth_exchange_for_stored_identity(&auth_endpoint, &remote_domain, "", repository)
                    .await
                    .forward::<ProtocolError>("resolving a stored identity")?;
            identity = resolved;
        } else if identity_token.is_empty() && access_token.is_empty() {
            // Only authentication is checked here. The repository's authorization is
            // exchanged where it is used, so its refusal is handled there.
            auth_exchange_for_stored_identity(
                &auth_endpoint,
                &remote_domain,
                &identity,
                RepositoryId::default(),
            )
            .await
            .forward::<ProtocolError>("authenticating the given identity")?;
        }
    }

    let from_supplied_credentials = !identity_token.is_empty() || !access_token.is_empty();
    if let Some(connection) = find_connection(
        remote_url.as_str(),
        identity.as_str(),
        from_supplied_credentials,
        auth_mode,
    ) {
        return Ok(shared_with_call(
            connection,
            &identity,
            &identity_token,
            &access_token,
        ));
    }

    let credentials = Arc::new(SuppliedCredentials::new(&identity_token, &access_token));

    let connection = Arc::new(Connection::unconnected(
        remote_url.clone(),
        auth_endpoint.clone(),
        identity.clone(),
        credentials.clone(),
        auth_mode,
        environment,
        protocol.clone(),
    ));

    let subtask_aborts: SubtaskAborts = Arc::new(parking_lot::Mutex::new(Some(Vec::new())));

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
                if !auth_endpoint.is_none() {
                    lore_trace!("Token exchange for identity {identity} for {auth_endpoint}");
                    let (identity_token, access_token) = connection.credentials().tokens();
                    if let Err(err) = auth::exchange::exchange(
                        &auth_endpoint,
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
            let max_connections = storage_connection_count(max_connections);
            lore_trace!(
                "Connecting storage service to {storage_url} using {max_connections} connections"
            );
            let storage_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(max_connections));
            let storage_error: Arc<parking_lot::Mutex<Option<ProtocolError>>> =
                Arc::new(parking_lot::Mutex::new(None));
            for index in 0..max_connections {
                let storage_url = storage_url.clone();
                let auth_endpoint = auth_endpoint.clone();
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
                            &auth_endpoint,
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
                hold_subtask(&subtask_aborts, handle.abort_handle());
            }

            // Admin services are created lazily per-repository via Connection::admin().

            if !repository.is_zero() {
                {
                    lore_trace!("Connecting revision service to {revision_url}");
                    let revision_url = revision_url.clone();
                    let auth_endpoint = auth_endpoint.clone();
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
                                &auth_endpoint,
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
                    hold_subtask(&subtask_aborts, handle.abort_handle());
                }

                {
                    lore_trace!("Connecting lock service to {lock_url}");
                    let lock_url = lock_url.clone();
                    let auth_endpoint = auth_endpoint.clone();
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
                                &auth_endpoint,
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
                    hold_subtask(&subtask_aborts, handle.abort_handle());
                }
            } else {
                connection.revision_ready.complete(Ok(()));
                connection.lock_ready.complete(Ok(()));
            }

            {
                let repository_service_url = repository_service_url.clone();
                let auth_endpoint = auth_endpoint.clone();
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
                            &auth_endpoint,
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
                hold_subtask(&subtask_aborts, handle.abort_handle());
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

    let cached = add_connection(
        remote_url.as_str(),
        identity.as_str(),
        from_supplied_credentials,
        auth_mode,
        connection.clone(),
    );
    if !Arc::ptr_eq(&cached, &connection) {
        // Another call cached a connection under this key since the lookup above. This call shares
        // it, as it would had the lookup found it, and stops connecting its own before anything
        // holds it. Nothing else locks the connector of a connection no call was handed, so this
        // takes it without an await, which would hold this future's locals across it.
        if let Ok(mut connector) = connection.connector.try_lock()
            && let Some(connector) = connector.take()
        {
            drop(connection.abort_connector(connector));
        }
        return Ok(shared_with_call(
            cached,
            &identity,
            &identity_token,
            &access_token,
        ));
    }

    Ok(connection)
}

/// `connection`, cached for another call, adopting the credentials this call supplied when it acts
/// as an identity, so the services the connection built present them. A lookup naming an identity
/// matches on the full key (see [`may_match_on_url_alone`]), so the credentials reach the
/// connection of the identity the call acts as. An unauthenticated server leaves the identity
/// unresolved, and that entry is shared with every call that resolved none, so it adopts nothing.
fn shared_with_call(
    connection: Arc<Connection>,
    identity: &str,
    identity_token: &str,
    access_token: &str,
) -> Arc<Connection> {
    if !identity.is_empty() {
        connection.credentials.update(identity_token, access_token);
    }
    connection
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

    /// Whether the service failed to connect with a refusal of the connection's credentials.
    fn refused(&self) -> bool {
        self.result
            .lock()
            .as_ref()
            .is_some_and(|result| result.as_ref().is_err_and(is_refusal))
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
    subtask_aborts: SubtaskAborts,
}

/// The abort handles of a connector's subtasks, `None` once the connector is cancelled.
#[lore_macro::test_pub]
type SubtaskAborts = Arc<parking_lot::Mutex<Option<Vec<tokio::task::AbortHandle>>>>;

/// Holds `subtask` for a cancel to abort, or aborts it at once if the connector is cancelled: the
/// setup task spawns its subtasks while a cancel may run, and one spawned after the cancel would
/// otherwise run to its end.
#[lore_macro::test_pub]
fn hold_subtask(subtask_aborts: &SubtaskAborts, subtask: tokio::task::AbortHandle) {
    match subtask_aborts.lock().as_mut() {
        Some(held) => held.push(subtask),
        None => subtask.abort(),
    }
}

/// A connection's use, as [`Connection::unused_for`] records it.
struct Usage {
    /// The time of the last check that found the connection in use or taken since the check before
    /// it, or of the first check after that.
    last: Instant,
    /// Whether the connection was in use at the last check, or has been taken since.
    in_use: bool,
}

/// Connection over a protocol
pub struct Connection {
    pub remote_url: Url,
    /// The authentication path selected for this connection.
    auth_endpoint: AuthEndpoint,
    pub identity: String,
    /// The credentials supplied for the call in progress, shared with every
    /// service this connection built so a later call's fresher ones take effect
    /// without reconnecting.
    credentials: Arc<SuppliedCredentials>,
    /// The mode `auth_endpoint` was selected with, so a module connected through
    /// this connection takes the same path to the same server.
    auth_mode: AuthMode,
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
    /// The session pool pinned for each partition, which calls under one correlation id share
    /// for as long as the connection lives, and a call under another replaces.
    session_cache: PartitionSessions,
    connector: tokio::sync::Mutex<Option<Connector>>,
    pub stale: std::sync::atomic::AtomicBool,
    /// Permits for the gets callers run over this connection to cache fragments ahead of a read,
    /// made by the first caller to ask ([`Self::cache_get_permits`]); `None` for no cap.
    cache_gets: std::sync::OnceLock<Option<Arc<tokio::sync::Semaphore>>>,
    /// What [`Self::unused_for`] has recorded of the connection's use.
    usage: Mutex<Usage>,
    /// Set once the connection is out of the cache, after which it pins no session pool.
    retired: std::sync::atomic::AtomicBool,
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
    /// A connection to `remote_url` that has connected to nothing yet: every service pending and
    /// every cache empty.
    fn unconnected(
        remote_url: Url,
        auth_endpoint: AuthEndpoint,
        identity: String,
        credentials: Arc<SuppliedCredentials>,
        auth_mode: AuthMode,
        environment: EnvironmentConfig,
        protocol: Arc<dyn Protocol>,
    ) -> Self {
        Connection {
            remote_url,
            auth_endpoint,
            identity,
            credentials,
            auth_mode,
            protocol,
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
            session_cache: PartitionSessions::default(),
            connector: tokio::sync::Mutex::new(None),
            stale: std::sync::atomic::AtomicBool::new(false),
            cache_gets: std::sync::OnceLock::new(),
            // In use by the call that opens it.
            usage: Mutex::new(Usage {
                last: Instant::now(),
                in_use: true,
            }),
            retired: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A connection over `storage` that reaches no other service, for tests that connect
    /// storage themselves.
    #[cfg(feature = "test-util")]
    pub fn detached(storage: Vec<Arc<dyn Storage>>) -> Arc<Self> {
        Self::detached_ready(storage, Ok(()))
    }

    /// A [`detached`](Self::detached) connection whose storage connect answered `ready`.
    #[cfg(feature = "test-util")]
    pub fn detached_ready(
        storage: Vec<Arc<dyn Storage>>,
        ready: Result<(), ProtocolError>,
    ) -> Arc<Self> {
        let connection = Self::detached_over(Arc::new(LoreProtocol::default()));
        if !storage.is_empty() {
            *connection.storage.write() = Some(Arc::new(StorageConnector::new(storage)));
        }
        connection.storage_ready.complete(ready);
        connection
    }

    /// A [`detached`](Self::detached) connection reaching no storage, which opens its revision,
    /// admin and lock services through `protocol`.
    #[cfg(feature = "test-util")]
    pub fn detached_protocol(protocol: Arc<dyn Protocol>) -> Arc<Self> {
        let connection = Self::detached_over(protocol);
        connection.storage_ready.complete(Ok(()));
        connection.revision_ready.complete(Ok(()));
        connection.lock_ready.complete(Ok(()));
        connection
    }

    /// A connection to no remote over `protocol`, with every service pending.
    #[cfg(feature = "test-util")]
    fn detached_over(protocol: Arc<dyn Protocol>) -> Arc<Self> {
        Arc::new(Connection::unconnected(
            Url::parse("lore://detached.invalid").expect("valid URL"),
            AuthEndpoint::none(),
            String::new(),
            Arc::default(),
            AuthMode::Auto,
            EnvironmentConfig::default(),
            protocol,
        ))
    }

    /// A [`detached`](Self::detached) connection over `storage`, whose storage connect answered
    /// `ready` and whose revision service for `repository` is `revision`.
    #[cfg(feature = "test-util")]
    pub fn detached_revision(
        storage: Vec<Arc<dyn Storage>>,
        ready: Result<(), ProtocolError>,
        repository: RepositoryId,
        revision: Arc<dyn Revision>,
    ) -> Arc<Self> {
        let connection = Self::detached_ready(storage, ready);
        connection.revision.insert(repository, revision);
        connection.revision_ready.complete(Ok(()));
        connection
    }

    pub fn remote_url(&self) -> &str {
        self.remote_url.as_str()
    }

    /// The permits gets that cache fragments over this connection take: `cap` of them, at most
    /// [`tokio::sync::Semaphore::MAX_PERMITS`], fixed by the first call; `None` for a cap of zero.
    /// Each connection has its own, so a stalled remote holds back only the cache gets over its
    /// connections, and a connection that replaces a stale one has permits of its own.
    pub fn cache_get_permits(&self, cap: usize) -> Option<Arc<tokio::sync::Semaphore>> {
        self.cache_gets
            .get_or_init(|| {
                (cap > 0).then(|| {
                    Arc::new(tokio::sync::Semaphore::new(
                        cap.min(tokio::sync::Semaphore::MAX_PERMITS),
                    ))
                })
            })
            .clone()
    }

    /// The endpoint of the authentication path selected for this connection,
    /// empty for a server that advertises no authentication.
    pub fn auth_url(&self) -> &str {
        self.auth_endpoint.url.as_str()
    }

    pub fn auth_endpoint(&self) -> &AuthEndpoint {
        &self.auth_endpoint
    }

    pub fn user_directory(&self) -> UserDirectory {
        UserDirectory::of(&self.environment)
    }

    pub fn identity(&self) -> &str {
        self.identity.as_str()
    }

    /// The credentials supplied for the call in progress. Hand this to anything
    /// that authorizes later, so it reads them at the time it needs them.
    pub fn credentials(&self) -> &Arc<SuppliedCredentials> {
        &self.credentials
    }

    /// Mark the connection failed and unregister it from the connection cache, which retires it.
    /// Idempotent — additional callers find no entry and no-op.
    #[lore_macro::test_pub]
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

    /// Whether connecting this connection's storage was refused its credentials.
    pub fn storage_refused(&self) -> bool {
        self.storage_ready.refused()
    }

    pub async fn ensure_storage_connected(self: &Arc<Self>) -> Result<(), ProtocolError> {
        let result = self.storage_ready.wait().await;
        if result.is_err() {
            self.mark_failed();
        }
        result
    }

    #[lore_macro::test_pub]
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
        let subtasks = connector.subtask_aborts.lock().take();
        for handle in subtasks.unwrap_or_default() {
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

    /// Closes the storage transport connections held by this `Connection`, waiting until the close
    /// frames reach the peer.
    pub async fn close_transport(&self) {
        let storage = self.storage.read().clone();
        if let Some(storage) = storage {
            storage.close_all().await;
        }
    }

    /// Records that a call has taken this connection from the cache, for the next check to count as
    /// use.
    fn record_taken(&self) {
        self.usage.lock().in_use = true;
    }

    /// Whether anything holds this connection beyond the cache's one reference and the session
    /// pools it pins. A pinned pool's sessions hold the connection they were started on, so a
    /// caller holds it through them only when it holds the pool or one of its sessions.
    fn held_beyond_the_cache(self: &Arc<Self>) -> bool {
        Arc::strong_count(self) > 1 + self.session_cache.references_held_only_here(self)
    }

    /// Whether no call has used this cached connection for `idle` at `now`, recording what this
    /// check finds. Anything holding it beyond the cache and the pools it pins is a call using it,
    /// and so is a call that has taken it since the last check. The time runs from the first check
    /// after that, so it never starts while a call holds the connection. Only a check after one
    /// that found the connection unused answers yes, so no single check's counts close it.
    fn unused_for(self: &Arc<Self>, idle: Duration, now: Instant) -> bool {
        let held = self.held_beyond_the_cache();
        let mut usage = self.usage.lock();
        let was_in_use = usage.in_use;
        if held || was_in_use {
            usage.last = now;
        }
        usage.in_use = held;
        !held && !was_in_use && now.saturating_duration_since(usage.last) >= idle
    }

    /// Closes this connection, which no call holds: marks it stale, so nothing takes it again, drops
    /// the session pools it pins, whose sessions hold it, and closes its storage connections, so the
    /// server releases them at once.
    #[lore_macro::test_pub]
    async fn close_unused(&self) {
        // Before the first await, so a close cut short by its timeout still frees the connection.
        self.stale.store(true, Ordering::Relaxed);
        retire(self);
        self.close().await;
    }

    /// Cancels this connection's connect and closes its storage transport, flushing the close
    /// frames to the peer.
    async fn close(&self) {
        let _ = self.cancel_connect().await;
        self.close_transport().await;
    }

    /// Returns the frozen storage connector, or error if not connected.
    fn storage_connector(&self) -> Result<Arc<StorageConnector>, ProtocolError> {
        self.storage
            .read()
            .clone()
            .ok_or_else(|| ProtocolError::internal("not connected"))
    }

    /// The `session_start` fan-outs this connection's storage connector has begun, as
    /// [`StorageConnector::session_starts`] counts them. 0 while storage isn't connected.
    #[cfg(feature = "test-util")]
    pub fn storage_session_starts(&self) -> usize {
        self.storage_connector()
            .map_or(0, |connector| connector.session_starts())
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

    /// A session over the pool [`session_pool`](Self::session_pool) pins for `partition`,
    /// which runs each operation on the next session of the pool, so a caller's operations
    /// spread over every storage connection.
    pub async fn session(
        self: &Arc<Self>,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<Arc<StorageSession>, ProtocolError> {
        Ok(Arc::new(StorageSession::pooled(
            self.session_pool(partition, correlation_id).await?,
        )))
    }

    /// The pool of sessions pinned here for `partition`, as long as it serves `correlation_id`.
    /// Otherwise the pool the connector has for `(partition, correlation_id)`, or a new one, is
    /// pinned in its place, and the one it replaces stops its sessions once no call holds it. A
    /// connection out of the cache pins none.
    ///
    /// A caller that needs one session per unit of work — a commit needs one per
    /// file — should hold the pool and [`pick`](SessionPool::pick) from it rather
    /// than calling [`session`](Self::session) each time.
    pub async fn session_pool(
        self: &Arc<Self>,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<Arc<SessionPool>, ProtocolError> {
        self.ensure_storage_connected().await?;
        if let Some(pool) = self.session_cache.get(partition, correlation_id) {
            return Ok(pool);
        }
        let connector = self.storage_connector()?;
        let pool = connector
            .session_pool(partition, correlation_id, self.clone())
            .await?;
        self.session_cache.hold(partition, pool.clone());
        // Retiring marks the connection before it drops the pins, so this drops a pool pinned
        // while the connection was being retired, since a pin would hold it.
        if self.retired.load(Ordering::Relaxed) {
            self.session_cache.clear();
        }
        Ok(pool)
    }

    /// The pool of sessions the connector has for `(partition, correlation_id)`, or a new one,
    /// pinned nowhere: its server-side sessions stop once its last holder drops it.
    ///
    /// Shares no helper with [`session_pool`](Self::session_pool): awaiting one from both would
    /// hold its arguments twice in that future.
    pub async fn unpinned_session_pool(
        self: &Arc<Self>,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<Arc<SessionPool>, ProtocolError> {
        self.ensure_storage_connected().await?;
        let connector = self.storage_connector()?;
        connector
            .session_pool(partition, correlation_id, self.clone())
            .await
    }

    /// Mark every pinned `SessionPool` stale and drop it, so the next
    /// `Connection::session` call rebuilds the pool even while another holder
    /// keeps the old one — re-running `session_start` against the current
    /// connection to obtain a fresh `session_id` the server actually knows
    /// about.
    pub fn invalidate_all_sessions(&self) {
        self.session_cache.invalidate();
    }

    /// Ensure the server's per-connection `authorized_repos` set contains `partition`,
    /// without leaving a pool pinned. Fast-paths via the connector's
    /// `authorized_partitions` cache: if a previous `session_start` already registered
    /// `partition` on every underlying connection, no wire calls happen. Otherwise a
    /// fresh pool is started (which fans `session_start` across all connections in
    /// parallel) and dropped unpinned; the server keeps `authorized_repos` permanent
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
        match connector
            .session_pool(partition, correlation_id, self.clone())
            .await
        {
            Ok(_pool) => Ok(()),
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
                &self.auth_endpoint,
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
                &self.auth_endpoint,
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
                &self.auth_endpoint,
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
            self.auth_mode,
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
        auth_endpoint: &AuthEndpoint,
        identity: &str,
        partition: Partition,
        _index: usize,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        quic::storage(
            connection,
            remote_url,
            auth_endpoint,
            identity,
            partition,
            credentials,
        )
        .await
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
        auth_endpoint: &AuthEndpoint,
        identity: &str,
        partition: Partition,
        index: usize,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        grpc::storage(
            connection,
            remote_url,
            auth_endpoint,
            identity,
            partition,
            index,
            credentials,
        )
        .await
    }
}

/// The machine server's socket: the storage protocol for storage, gRPC over the same socket for
/// everything else. See [`crate::uds`].
#[derive(Default)]
struct UdsProtocol {}

#[async_trait::async_trait]
impl Protocol for UdsProtocol {
    async fn storage(
        &self,
        _connection: Weak<Connection>,
        remote_url: &str,
        auth_endpoint: &AuthEndpoint,
        identity: &str,
        _partition: Partition,
        _index: usize,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        crate::uds::storage(remote_url, auth_endpoint, identity, credentials).await
    }
}
