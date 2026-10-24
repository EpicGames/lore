// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

use async_trait::async_trait;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use crossbeam_epoch as epoch;
use crossbeam_epoch::Atomic;
use crossbeam_epoch::Guard;
use crossbeam_epoch::Owned;
use futures::FutureExt;
use futures::future::Map;
use futures::future::Shared;
use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::error::Oversized;
use lore_base::error::SlowDown;
use lore_base::lore_debug;
use lore_base::lore_spawn_net;
use lore_base::lore_trace;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::HealResult;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_base::types::VerifyResult;
use lore_error_set::prelude::*;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use zerocopy::IntoBytes;

use super::super::QuicClientError;
use super::super::QuicOpCode;
use super::super::client::AuthAdapter;
use super::super::client::BoundSession;
use super::super::client::DEFAULT_EXPECTED_RTT_MS;
use super::super::client::EndpointConfig;
use super::super::client::NO_GENERATION;
use super::super::client::QuicConnection;
use super::super::client::SendWithReconnectError;
use super::super::client::ServiceClient;
use super::super::client::TransportConfig;
use super::super::client::connect;
use super::super::client::send_high_priority_with_reconnect;
use super::super::client::send_normal;
use super::super::client::send_normal_with_reconnect;
use super::super::storage_service;
use super::super::storage_service::Command;
use super::super::storage_service::MAX_CHUNK_SIZE;
use super::super::storage_service::auth::StorageClientAuth;
use crate::auth::AuthEndpoint;
use crate::connection::Connection;
use crate::connection::SuppliedCredentials;
use crate::error::ProtocolError;
use crate::quic::client::CongestionAlgorithm;
use crate::traits::Storage;

const INFLIGHT_COMMAND_LIMIT: usize = 10000;

const MAX_BYTES_BANDWIDTH_PER_SEC: u64 = (1024 * 1024 * 1024) / 8;

#[allow(dead_code)]
pub struct StorageClient {
    connection: Weak<Connection>,
    remote_url: String,
    transport_config: TransportConfig,
    auth_adapter: Arc<dyn AuthAdapter<ErrorType = ProtocolError>>,
    auth_endpoint: AuthEndpoint,
    recipient_domain: String,
    identity: String,
    credentials: Arc<SuppliedCredentials>,
    partition: Partition,
    counter: AtomicUsize,
    quic: Arc<QuicConnection>,
    connection_establish: Semaphore,
    command_limit: Semaphore,
    sent: AtomicUsize,
    sessions: Sessions,
    /// This client, for the task that issues a session again ([`StorageClient::rebind`]).
    this: Weak<StorageClient>,
}

/// A session a [`StorageClient`] hands out: what the server issues it for, and the id the server
/// knows it by on a connection.
struct Session {
    id: u32,
    partition: Partition,
    correlation_id: Box<str>,
    /// The session as the server issued it ([`BoundSession::pack`]), so a command reads its id and
    /// connection at once.
    bound: AtomicU64,
    /// The generation of the connection the latest request issuing this session went to, which the
    /// id the server answers belongs to.
    issuing: AtomicU32,
    /// Held by the task issuing the session again, or by a stop while it takes the session from
    /// the table, so a stop never overlaps an issue.
    rebinding: Semaphore,
    /// The task issuing the session again on the connection of the generation it names, which
    /// every command that finds the session stale there while it runs awaits.
    reissue: parking_lot::Mutex<Option<(u32, Reissue)>>,
}

/// The outcome of the task issuing a session again ([`issued`]).
type Reissue = Shared<
    Map<
        JoinHandle<Result<(), ProtocolError>>,
        fn(Result<Result<(), ProtocolError>, JoinError>) -> Result<(), ProtocolError>,
    >,
>;

/// The outcome of the task issuing a session again, or an internal error where it ended without
/// one.
fn issued(joined: Result<Result<(), ProtocolError>, JoinError>) -> Result<(), ProtocolError> {
    joined.unwrap_or_else(|_| {
        Err(ProtocolError::internal(
            "session issue ended without outcome",
        ))
    })
}

/// The error of a command on a session the client does not hold.
fn not_open(session_id: u32) -> ProtocolError {
    ProtocolError::internal(format!("session {session_id} is not open"))
}

impl Session {
    fn bound(&self) -> BoundSession {
        BoundSession::unpack(self.bound.load(Ordering::Relaxed))
    }
}

/// The sessions a [`StorageClient`] handed out, sorted by id. Every command reads the table, and
/// does so without a lock; a session start or stop, rare beside the commands, replaces it whole.
#[derive(Default)]
struct Sessions {
    issued: Atomic<Vec<Arc<Session>>>,
    next: AtomicU32,
}

impl Sessions {
    /// A session for `partition` and `correlation_id`, bound to no connection, under a new id.
    fn open(&self, partition: Partition, correlation_id: &str) -> Arc<Session> {
        let session = Arc::new(Session {
            id: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            partition,
            correlation_id: correlation_id.into(),
            bound: AtomicU64::new(
                BoundSession {
                    id: 0,
                    generation: NO_GENERATION,
                }
                .pack(),
            ),
            issuing: AtomicU32::new(NO_GENERATION),
            rebinding: Semaphore::new(1),
            reissue: parking_lot::Mutex::new(None),
        });
        self.replace(|issued| {
            let at = issued.partition_point(|held| held.id < session.id);
            let mut opened = Vec::with_capacity(issued.len() + 1);
            opened.extend_from_slice(&issued[..at]);
            opened.push(session.clone());
            opened.extend_from_slice(&issued[at..]);
            Some((opened, ()))
        });
        session
    }

    /// The session `id` names, held while `guard` is.
    fn find<'g>(&self, id: u32, guard: &'g Guard) -> Option<&'g Arc<Session>> {
        // SAFETY: a table is freed only by `replace`, deferred past every reader pinned when it was
        // unlinked, or by `drop`, which no reader outlives.
        let issued = unsafe { self.issued.load(Ordering::Acquire, guard).as_ref() }?;
        let at = issued
            .binary_search_by_key(&id, |session| session.id)
            .ok()?;
        Some(&issued[at])
    }

    fn get(&self, id: u32) -> Option<Arc<Session>> {
        self.find(id, &epoch::pin()).cloned()
    }

    fn remove(&self, id: u32) -> Option<Arc<Session>> {
        self.replace(|issued| {
            let at = issued
                .binary_search_by_key(&id, |session| session.id)
                .ok()?;
            let mut kept = issued.to_vec();
            let removed = kept.remove(at);
            Some((kept, removed))
        })
    }

    /// Replaces the table with the one `edit` makes of it, making it again from the table another
    /// replacement put in place first; a table `edit` makes none of stays as it is.
    fn replace<R>(
        &self,
        mut edit: impl FnMut(&[Arc<Session>]) -> Option<(Vec<Arc<Session>>, R)>,
    ) -> Option<R> {
        let guard = epoch::pin();
        let mut current = self.issued.load(Ordering::Acquire, &guard);
        loop {
            // SAFETY: as in `find`.
            let issued = unsafe { current.as_ref() }.map_or(&[][..], Vec::as_slice);
            let (replacement, edited) = edit(issued)?;
            match self.issued.compare_exchange(
                current,
                Owned::new(replacement),
                Ordering::AcqRel,
                Ordering::Acquire,
                &guard,
            ) {
                Ok(_) => {
                    if !current.is_null() {
                        // SAFETY: unlinked above; a reader still pinned on it holds it until
                        // its guard drops.
                        unsafe { guard.defer_destroy(current) };
                        guard.flush();
                    }
                    return Some(edited);
                }
                Err(raced) => current = raced.current,
            }
        }
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        // SAFETY: `drop` holds the only reference, so no reader is pinned on the table.
        drop(unsafe { std::mem::take(&mut self.issued).try_into_owned() });
    }
}

impl Drop for StorageClient {
    fn drop(&mut self) {
        // Close the QUIC connection immediately without waiting for streams to
        // drain. Quinn sends a CLOSE frame and RSTs any open streams. The server
        // handles this gracefully (cleans up all sessions on connection close).
        // This avoids blocking the runtime during shutdown which would deadlock
        // with any in-flight session_stop sends on the same connection.
        self.quic.close_immediate();
        lore_trace!("QUIC storage client dropped");
    }
}

impl StorageClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connection: Weak<Connection>,
        remote_url: &str,
        transport_config: TransportConfig,
        auth_adapter: Arc<dyn AuthAdapter<ErrorType = ProtocolError>>,
        auth_endpoint: &AuthEndpoint,
        recipient_domain: &str,
        identity: &str,
        partition: Partition,
        quinn: quinn::Connection,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Arc<Self> {
        let quic = QuicConnection::with_v4(quinn, MAX_CHUNK_SIZE, true);
        Arc::new_cyclic(|this| StorageClient {
            connection,
            remote_url: remote_url.to_string(),
            transport_config,
            auth_adapter,
            auth_endpoint: auth_endpoint.clone(),
            recipient_domain: recipient_domain.to_string(),
            identity: identity.to_string(),
            credentials: credentials.clone(),
            partition,
            quic: Arc::new(quic),
            connection_establish: Semaphore::new(1),
            counter: AtomicUsize::new(0),
            sent: AtomicUsize::new(0),
            command_limit: Semaphore::new(INFLIGHT_COMMAND_LIMIT),
            sessions: Sessions::default(),
            this: this.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        connection: Weak<Connection>,
        remote_url: &str,
        remote_domain: String,
        auth_endpoint: &AuthEndpoint,
        identity: &str,
        partition: Partition,
        credentials: &Arc<SuppliedCredentials>,
        user_agent: Option<String>,
    ) -> Result<Arc<Self>, ProtocolError> {
        let user_agent = user_agent.unwrap_or_else(|| crate::user_agent().to_string());
        let auth_adapter = Arc::new(StorageClientAuth {
            recipient_domain: remote_domain.clone(),
            auth_url: auth_endpoint.url.clone(),
            identity: identity.to_string(),
            partition,
            user_agent,
        });
        let transport_config = TransportConfig {
            max_bytes_bandwidth_per_second: MAX_BYTES_BANDWIDTH_PER_SEC,
            expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
            congestion_algorithm: CongestionAlgorithm::Bbr,
            initial_cwnd: None,
        };

        lore_trace!("QUIC connecting to {remote_url} for partition {partition}");

        let start = Instant::now();

        let quinn = connect(
            &EndpointConfig {
                remote_url: remote_url.to_string(),
                default_port: Self::DEFAULT_PORT,
                sni_override: None,
            },
            auth_adapter.client_certs(),
            Self::ALPN,
            transport_config.clone(),
        )
        .await?;
        let connection_id = quinn.stable_id();

        let storage = StorageClient::new(
            connection,
            remote_url,
            transport_config,
            auth_adapter.clone(),
            auth_endpoint,
            &remote_domain,
            identity,
            partition,
            quinn,
            credentials,
        );

        lore_trace!(
            "QUIC connected to {remote_url} in {}ms",
            start.elapsed().as_millis()
        );

        storage
            .quic
            .create_initial_stream()
            .await
            .internal_with(|| {
                format!("creating initial QUIC stream to {remote_url} for partition {partition}")
            })?;

        auth_adapter.initial_authorize(storage.quic.clone()).await?;

        storage.quic.stream_count.store(1, Ordering::Relaxed);

        lore_debug!(
            "QUIC connection {connection_id} to {remote_url} for partition {partition} complete in {}ms",
            start.elapsed().as_millis()
        );

        Ok(storage)
    }

    /// The authorization a session start for `partition` presents, read at each start because the
    /// server checks storage authorization there: an access token the newest call supplied, as it
    /// is, or the authorization the newest credentials exchange for, which the cache or the token
    /// store may serve. Empty when there is none, as for a server without authentication or after
    /// a failed exchange.
    async fn authorization(&self, partition: Partition) -> Authorization {
        if self.auth_endpoint.is_none() {
            return Authorization::default();
        }
        let (identity_token, access_token) = self.credentials.tokens();
        let (_, token, _) = crate::auth::exchange::auth_exchange(
            &self.auth_endpoint,
            &self.recipient_domain,
            &self.identity,
            partition,
            &identity_token,
            &access_token,
        )
        .await;
        Authorization {
            token,
            exchanged_with: access_token.is_empty().then_some(identity_token),
        }
    }

    /// Issues `session` on the current connection, authorized for its partition.
    async fn issue(&self, session: &Session) -> Result<(), ProtocolError> {
        let authorization = self.authorization(session.partition).await;
        self.issue_presenting(session, authorization).await
    }

    /// Issues `session` presenting `authorization`, and once more with a new authorization should
    /// the server refuse an exchanged one. Its own future, so that what it holds is not held beside
    /// the exchange that produced `authorization`.
    async fn issue_presenting(
        &self,
        session: &Session,
        authorization: Authorization,
    ) -> Result<(), ProtocolError> {
        let Authorization {
            token,
            exchanged_with,
        } = authorization;
        match (self.send_issue(session, &token).await, exchanged_with) {
            (Err(refusal), Some(identity_token))
                if refusal.is_not_authorized() && !token.is_empty() =>
            {
                Box::pin(self.issue_reauthorized(session, token, identity_token, refusal)).await
            }
            (issued, _) => issued,
        }
    }

    /// Issues the session the server refused once more, with a new authorization in place of
    /// `refused`, which was exchanged with `identity_token`. Cold: only after the server refused an
    /// exchanged authorization.
    ///
    /// The cache would serve `refused` until it expires, so it is forgotten first, and the
    /// exchange then serves an authorization another caller put in its place, or asks the auth
    /// service for another. A refusal of the new authorization is final, and so is `refusal` when
    /// the exchange yields none, or `refused` again.
    async fn issue_reauthorized(
        &self,
        session: &Session,
        refused: String,
        identity_token: String,
        refusal: ProtocolError,
    ) -> Result<(), ProtocolError> {
        let partition = session.partition;
        lore_debug!("Session start for {partition} refused its authorization; exchanging another");
        crate::auth::exchange::forget_refused_authorization(
            &self.auth_endpoint,
            &self.identity,
            partition,
            &self.recipient_domain,
            &identity_token,
            &refused,
        )
        .await;
        let authorization = self.authorization(partition).await;
        if authorization.token.is_empty() || authorization.token == refused {
            return Err(refusal);
        }
        self.send_issue(session, &authorization.token).await
    }

    /// Asks the server to start `session` presenting `token`, and binds it to the id the server
    /// answers on the connection the request went to.
    async fn send_issue(&self, session: &Session, token: &str) -> Result<(), ProtocolError> {
        let payload = session_start_payload(session.partition, &session.correlation_id, token);
        let response = send_normal_with_reconnect(self, Command::Authorize, session.id, || {
            [Bytes::default(), payload.clone()]
        })
        .await?;
        let id = started_session_id(&response)?;
        let generation = session.issuing.load(Ordering::Relaxed);
        session
            .bound
            .store(BoundSession { id, generation }.pack(), Ordering::Relaxed);
        Ok(())
    }

    /// Stops the session `bound` names on the connection it is bound to, and on no other, where
    /// its id names another session or none.
    async fn stop(&self, bound: BoundSession) {
        if !self.quic.has_streams().await {
            return;
        }
        let _ = send_normal(
            self.quic.clone(),
            Command::Authorize as QuicOpCode,
            bound,
            true,
            &mut [Bytes::default(), Bytes::from_static(&[1u8])],
        )
        .await;
    }

    /// Issues the session `session_id` names again on the current connection, unless it is bound
    /// to it already. The issue runs as a task of its own ([`reissue`](Self::reissue)), which every
    /// command that finds the session stale on the same connection while it runs awaits for its
    /// outcome, and which runs to its end when they are dropped.
    async fn rebind(&self, session_id: u32) -> Result<(), ProtocolError> {
        let running = {
            let session = self
                .sessions
                .get(session_id)
                .ok_or_else(|| not_open(session_id))?;
            let generation = self.quic.generation();
            if session.bound().generation == generation {
                return Ok(());
            }
            let mut reissue = session.reissue.lock();
            match &*reissue {
                Some((issuing, running)) if *issuing == generation && running.peek().is_none() => {
                    running.clone()
                }
                _ => {
                    let client = self.this.upgrade().ok_or_else(|| not_open(session_id))?;
                    let running = lore_spawn_net!(client.reissue(session.clone()))
                        .map(issued as fn(_) -> _)
                        .shared();
                    *reissue = Some((generation, running.clone()));
                    running
                }
            }
        };
        running.await
    }

    /// The commands holding the last issue of the session `session_id` names: the handles to it
    /// beside the session's own.
    #[cfg(feature = "test-util")]
    pub fn awaiting_issue(&self, session_id: u32) -> usize {
        let Some(session) = self.sessions.get(session_id) else {
            return 0;
        };
        let reissue = session.reissue.lock();
        reissue
            .as_ref()
            .and_then(|(_, running)| running.strong_count())
            .map_or(0, |handles| handles - 1)
    }

    /// Issues `session` again on the current connection once no stop or other issue of it runs,
    /// unless a stop took it from the table or an issue bound it to the current connection first.
    async fn reissue(self: Arc<Self>, session: Arc<Session>) -> Result<(), ProtocolError> {
        let _rebinding = session.rebinding.acquire().await;
        if self.sessions.find(session.id, &epoch::pin()).is_none() {
            return Err(not_open(session.id));
        }
        if session.bound().generation == self.quic.generation() {
            return Ok(());
        }
        self.issue(&session).await
    }
}

/// Takes the session it names out of its table when dropped, unless the start that opened it
/// succeeded, so a start that fails or is cancelled leaves no entry in the table.
struct Opening<'a> {
    sessions: &'a Sessions,
    id: u32,
    started: bool,
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        if !self.started {
            self.sessions.remove(self.id);
        }
    }
}

/// The authorization a session start presents, with the identity token it was exchanged with,
/// both from one reading of the credentials, so a refusal is judged by what produced the
/// authorization and not by credentials a later call supplied meanwhile.
#[derive(Default)]
struct Authorization {
    token: String,
    /// The identity token `token` was exchanged with, empty where the token store supplied the
    /// credentials. `None` where there is nothing to exchange again, whose refusal is final:
    /// `token` is an access token the caller supplied, or the server has no authentication.
    exchanged_with: Option<String>,
}

/// The payload of a session start for `partition` presenting `token`:
/// `action(1=0) + partition_id(16) + corr_len(1) + corr(N) + token_len(2, u16 LE) + token(M)`.
fn session_start_payload(partition: Partition, correlation_id: &str, token: &str) -> Bytes {
    let token_bytes = token.as_bytes();
    let corr_bytes = correlation_id.as_bytes();
    let mut payload =
        BytesMut::with_capacity(1 + 16 + 1 + corr_bytes.len() + 2 + token_bytes.len());
    payload.put_u8(0);
    payload.extend_from_slice(partition.as_bytes());
    payload.put_u8(corr_bytes.len() as u8);
    payload.extend_from_slice(corr_bytes);
    payload.extend_from_slice(&(token_bytes.len() as u16).to_le_bytes());
    payload.extend_from_slice(token_bytes);
    payload.freeze()
}

/// The id of the session a session start's `response` names.
fn started_session_id(response: &[u8]) -> Result<u32, ProtocolError> {
    let Ok(session_id) = <[u8; 4]>::try_from(response) else {
        return Err(ProtocolError::internal(format!(
            "session_start: expected 4-byte response, got {} bytes",
            response.len()
        )));
    };
    Ok(u32::from_le_bytes(session_id))
}

impl ServiceClient for StorageClient {
    type RequestType = Command;
    type ErrorType = ProtocolError;

    const ALPN: &'static str = "lore-storage/0.4";
    const DEFAULT_PORT: u16 = 41337;

    async fn acquire_command_permit(&self) -> Option<SemaphorePermit<'_>> {
        self.command_limit.acquire().await.ok()
    }

    fn quic(&self) -> &Arc<QuicConnection> {
        &self.quic
    }

    fn endpoint_config(&self) -> EndpointConfig {
        EndpointConfig {
            remote_url: self.remote_url.clone(),
            default_port: Self::DEFAULT_PORT,
            sni_override: None,
        }
    }

    fn alpn(&self) -> &str {
        Self::ALPN
    }

    fn map_send_error(
        &self,
        failed_request: Self::RequestType,
        error: SendWithReconnectError,
    ) -> Self::ErrorType {
        match error {
            SendWithReconnectError::PermitAcquire => ProtocolError::internal("permit acquire"),
            SendWithReconnectError::Disconnected => ProtocolError::from(Disconnected),
            SendWithReconnectError::ReconnectFailed => {
                if let Some(connection) = self.connection.upgrade() {
                    connection.stale.store(true, Ordering::Relaxed);
                }
                ProtocolError::from(Disconnected)
            }
            SendWithReconnectError::ClientError(client_error) => match client_error {
                QuicClientError::SlowDown => ProtocolError::from(SlowDown),
                QuicClientError::NotAuthorized => ProtocolError::from(NotAuthorized),
                QuicClientError::NotFound => ProtocolError::from(NotFound),
                QuicClientError::Oversized => ProtocolError::from(Oversized {
                    context: format!(
                        "{}: server rejected oversized fragment",
                        storage_service::command_name(&failed_request)
                    ),
                }),
                _ => {
                    let name = storage_service::command_name(&failed_request);
                    ProtocolError::internal(format!(
                        "{name}: Failed sending command: {client_error}"
                    ))
                }
            },
        }
    }

    fn auth_adapter(&self) -> &Arc<dyn AuthAdapter<ErrorType = Self::ErrorType>> {
        &self.auth_adapter
    }

    fn transport_config(&self) -> TransportConfig {
        self.transport_config.clone()
    }

    fn v4_protocol(&self) -> bool {
        true
    }

    /// An `Authorize` request issues the session it names on the current connection, and records
    /// that connection as the one the server's answer belongs to. Any other request carries the
    /// session as the server last issued it, or binds to no connection once it is stopped.
    fn bind_session(&self, request_type: Command, session_id: u32) -> BoundSession {
        let guard = epoch::pin();
        let session = self.sessions.find(session_id, &guard);
        if request_type == Command::Authorize {
            let generation = self.quic.generation();
            if let Some(session) = session {
                session.issuing.store(generation, Ordering::Relaxed);
            }
            return BoundSession { id: 0, generation };
        }
        session.map_or(
            BoundSession {
                id: 0,
                generation: NO_GENERATION,
            },
            |session| session.bound(),
        )
    }

    /// An `Authorize` request is bound afresh on its next attempt, as it issues the session.
    async fn rebind_session(
        &self,
        request_type: Command,
        session_id: u32,
    ) -> Result<(), ProtocolError> {
        if request_type == Command::Authorize {
            return Ok(());
        }
        self.rebind(session_id).await
    }
}

#[async_trait]
impl Storage for StorageClient {
    async fn session_start(
        &self,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        let session = self.sessions.open(partition, correlation_id);
        let mut opening = Opening {
            sessions: &self.sessions,
            id: session.id,
            started: false,
        };
        self.issue(&session).await?;
        opening.started = true;
        Ok(session.id)
    }

    async fn session_stop(&self, session_id: u32) -> Result<(), ProtocolError> {
        let bound = {
            let Some(session) = self.sessions.get(session_id) else {
                return Ok(());
            };
            let _rebinding = session.rebinding.acquire().await;
            self.sessions.remove(session_id).map(|_| session.bound())
        };
        if let Some(bound) = bound {
            self.stop(bound).await;
        }
        Ok(())
    }

    async fn get(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let mut payload = send_normal_with_reconnect(self, Command::Get, session_id, || {
            [Bytes::default(), Bytes::from_owner(*address)]
        })
        .await?;

        if payload.len() < size_of::<Fragment>() {
            return Err(ProtocolError::internal("get: Invalid server response"));
        }

        let fragment_bytes = payload.split_to(size_of::<Fragment>());
        let fragment = unsafe { fragment_bytes.as_ptr().cast::<Fragment>().read_unaligned() };

        if let Err(reason) = lore_base::types::validate_fragment_response(&fragment) {
            return Err(ProtocolError::internal(format!(
                "get: invalid fragment {fragment:?}: {reason}"
            )));
        }
        if payload.len() != fragment.size_payload as usize {
            return Err(ProtocolError::internal(format!(
                "get: Invalid server payload for fragment {fragment:?}, got {} bytes",
                payload.len()
            )));
        }

        Ok((fragment, payload))
    }

    /// Response framing: resolved `Hash` (32) ++ `Fragment` (16) ++ payload.
    async fn get_resolved(
        &self,
        session_id: u32,
        key: &Hash,
        context: &Context,
        flags: u32,
    ) -> Result<(Hash, Fragment, Bytes), ProtocolError> {
        let tail = flags.to_le_bytes();

        let mut payload =
            send_normal_with_reconnect(self, Command::GetResolved, session_id, || {
                [
                    Bytes::default(),
                    Bytes::from_owner(*key),
                    Bytes::from_owner(*context),
                    Bytes::copy_from_slice(&tail),
                ]
            })
            .await?;

        let prefix = size_of::<Hash>() + size_of::<Fragment>();
        if payload.len() < prefix {
            return Err(ProtocolError::internal(format!(
                "get_resolved: Invalid server response, expected at least {prefix} bytes got {}",
                payload.len()
            )));
        }

        let resolved_bytes = payload.split_to(size_of::<Hash>());
        let resolved = Hash::from(&resolved_bytes[..]);

        let fragment_bytes = payload.split_to(size_of::<Fragment>());
        let fragment = unsafe { fragment_bytes.as_ptr().cast::<Fragment>().read_unaligned() };

        if let Err(reason) = lore_base::types::validate_fragment_response(&fragment) {
            return Err(ProtocolError::internal(format!(
                "get_resolved: invalid fragment {fragment:?}: {reason}"
            )));
        }
        if payload.len() != fragment.size_payload as usize {
            return Err(ProtocolError::internal(format!(
                "get_resolved: Invalid server payload for fragment {fragment:?}, got {} bytes",
                payload.len()
            )));
        }

        Ok((resolved, fragment, payload))
    }

    async fn get_metadata(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        let payload = send_normal_with_reconnect(self, Command::GetMetadata, session_id, || {
            [Bytes::default(), Bytes::from_owner(*address)]
        })
        .await?;

        if payload.len() != size_of::<Fragment>() {
            return Err(ProtocolError::internal(format!(
                "get_metadata: expected {} bytes for Fragment, got {}",
                size_of::<Fragment>(),
                payload.len()
            )));
        }

        let fragment = unsafe { payload.as_ptr().cast::<Fragment>().read_unaligned() };
        Ok(fragment)
    }

    async fn get_priority(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let mut payload = send_high_priority_with_reconnect(self, Command::Get, session_id, || {
            [Bytes::default(), Bytes::from_owner(*address)]
        })
        .await?;

        if payload.len() < size_of::<Fragment>() {
            return Err(ProtocolError::internal("get: Invalid server response"));
        }

        let fragment_bytes = payload.split_to(size_of::<Fragment>());
        let fragment = unsafe { fragment_bytes.as_ptr().cast::<Fragment>().read_unaligned() };

        if let Err(reason) = lore_base::types::validate_fragment_response(&fragment) {
            return Err(ProtocolError::internal(format!(
                "get: invalid fragment {fragment:?}: {reason}"
            )));
        }
        if payload.len() != fragment.size_payload as usize {
            return Err(ProtocolError::internal(format!(
                "get: Invalid server payload for fragment {fragment:?}, got {} bytes",
                payload.len()
            )));
        }

        Ok((fragment, payload))
    }

    async fn put(
        &self,
        session_id: u32,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        send_normal_with_reconnect(self, Command::Put, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(address),
                Bytes::from_owner(fragment),
                payload.clone().unwrap_or_default(),
            ]
        })
        .await
        .map(|_| ())
    }

    /// Request framing: `put`'s, with the mutable key prepended — key (32) ++ `Address` (48) ++
    /// `Fragment` (16) ++ payload, keeping the 96-byte header a multiple of four.
    async fn put_resolved(
        &self,
        session_id: u32,
        key: &Hash,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        send_normal_with_reconnect(self, Command::PutResolved, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(*key),
                Bytes::from_owner(address),
                Bytes::from_owner(fragment),
                payload.clone().unwrap_or_default(),
            ]
        })
        .await
        .map(|_| ())
    }

    async fn query(&self, session_id: u32, address: &[Address]) -> Result<Bytes, ProtocolError> {
        const MAX_BATCH: usize = lore_base::types::FRAGMENT_SIZE_EXPECTED / size_of::<Address>();
        let address_count = address.len();
        if address_count > MAX_BATCH {
            return Err(ProtocolError::internal(format!(
                "query: Invalid address batch count: {address_count}"
            )));
        }

        // Allocate a copy on the heap to avoid lifetime issues
        let address = Bytes::copy_from_slice(address.as_bytes());
        let payload = send_normal_with_reconnect(self, Command::Query, session_id, || {
            [Bytes::default(), address.clone()]
        })
        .await?;

        if payload.len() != address_count {
            return Err(ProtocolError::internal(format!(
                "query: Server returned query payload of wrong size, expected {} got {}",
                address_count,
                payload.len()
            )));
        }

        Ok(payload)
    }

    async fn verify(
        &self,
        session_id: u32,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        let heal_byte = if heal { 1u8 } else { 0u8 };
        let mut request_bytes = BytesMut::with_capacity(size_of::<Address>() + 1);
        request_bytes.extend_from_slice(address.as_bytes());
        request_bytes.put_u8(heal_byte);
        let request_bytes = request_bytes.freeze();

        let payload = send_normal_with_reconnect(self, Command::Verify, session_id, || {
            [Bytes::default(), request_bytes.clone()]
        })
        .await?;

        // Response should be 2 bytes: corrupted + healed
        if payload.len() != 2 {
            return Err(ProtocolError::internal(format!(
                "verify: Invalid server payload for address {address}, got {} bytes",
                payload.len()
            )));
        }

        Ok(VerifyResult {
            corrupted: payload[0] != 0,
            healed: HealResult::from(payload[1]),
        })
    }

    async fn copy(
        &self,
        session_id: u32,
        source_partition: Partition,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        // Wire payload layout for Copy is: source_partition (16) + source_address (32+16) +
        // target_context (16) = 80 bytes. The target_context tail allows the destination's
        // dedup tag to differ from the source's without transferring the payload.
        send_normal_with_reconnect(self, Command::Copy, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(source_partition),
                Bytes::from_owner(source_address),
                Bytes::from_owner(target_context),
            ]
        })
        .await
        .map(|_| ())
    }

    async fn mutable_load(
        &self,
        session_id: u32,
        key: &Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let payload = send_normal_with_reconnect(self, Command::MutableLoad, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(*key),
                Bytes::copy_from_slice(&[key_type as u8]),
            ]
        })
        .await?;

        if payload.len() != size_of::<Hash>() {
            return Err(ProtocolError::internal(format!(
                "mutable_load: Invalid server response, expected {} bytes got {}",
                size_of::<Hash>(),
                payload.len()
            )));
        }

        Ok(Hash::from(&payload[..]))
    }

    async fn mutable_store(
        &self,
        session_id: u32,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        send_normal_with_reconnect(self, Command::MutableStore, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(key),
                Bytes::from_owner(value),
                Bytes::copy_from_slice(&[key_type as u8]),
            ]
        })
        .await
        .map(|_| ())
    }

    async fn mutable_compare_and_swap(
        &self,
        session_id: u32,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let payload = send_normal_with_reconnect(self, Command::MutableCas, session_id, || {
            [
                Bytes::default(),
                Bytes::from_owner(key),
                Bytes::from_owner(expected),
                Bytes::from_owner(value),
                Bytes::copy_from_slice(&[key_type as u8]),
            ]
        })
        .await?;

        if payload.len() != size_of::<Hash>() {
            return Err(ProtocolError::internal(format!(
                "mutable_cas: Invalid server response, expected {} bytes got {}",
                size_of::<Hash>(),
                payload.len()
            )));
        }

        Ok(Hash::from(&payload[..]))
    }

    async fn close(&self) {
        self.quic.close().await;
    }
}
