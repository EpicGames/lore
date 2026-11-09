// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The sessions a storage client hands out, shared by the QUIC client and the socket client: the
//! table every command reads without a lock, where each session is issued on the server, and the
//! authorization a session start presents.
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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
use lore_base::lore_debug;
use lore_base::lore_spawn_net;
use lore_base::types::Partition;
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use zerocopy::IntoBytes;

use crate::auth::AuthEndpoint;
use crate::auth::exchange::unsigned_identity_tokens;
use crate::connection::SuppliedCredentials;
use crate::error::ProtocolError;
use crate::quic::client::BoundSession;
use crate::quic::client::NO_GENERATION;

/// A session a client hands out: what the server issues it for, and the id the server knows it
/// by on a connection.
pub(crate) struct Session {
    pub(crate) id: u32,
    pub(crate) partition: Partition,
    pub(crate) correlation_id: Box<str>,
    /// The session as the server issued it ([`BoundSession::pack`]), so a command reads its id and
    /// connection at once.
    bound: AtomicU64,
    /// The generation of the connection the latest request issuing this session went to, which the
    /// id the server answers belongs to.
    issuing: AtomicU32,
    /// Held by the task issuing the session again, or by a stop while it takes the session from
    /// the table, so a stop never overlaps an issue.
    rebinding: Semaphore,
    /// The task issuing the session again, with the generation of the connection the commands
    /// awaiting it found the session stale on; every command that finds it stale there while the
    /// task runs awaits the same task.
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
#[cold]
pub(crate) fn not_open(session_id: u32) -> ProtocolError {
    ProtocolError::internal(format!("session {session_id} is not open"))
}

impl Session {
    /// The session as the server last issued it.
    pub(crate) fn bound(&self) -> BoundSession {
        BoundSession::unpack(self.bound.load(Ordering::Relaxed))
    }

    /// Records the generation of the connection a request issuing the session goes to.
    pub(crate) fn issue_on(&self, generation: u32) {
        self.issuing.store(generation, Ordering::Relaxed);
    }

    /// Binds the session to `id`, the server's answer to the latest request issuing it, on the
    /// connection that request went to.
    pub(crate) fn bind(&self, id: u32) {
        let generation = self.issuing.load(Ordering::Relaxed);
        self.bound
            .store(BoundSession { id, generation }.pack(), Ordering::Relaxed);
    }

    /// The task issuing the session again for the commands that found it stale on the connection
    /// of `generation`: the one running for that generation already without an outcome yet, or
    /// else the one `spawn` starts. `None` when `spawn` starts none.
    fn reissue_on(
        &self,
        generation: u32,
        spawn: impl FnOnce() -> Option<JoinHandle<Result<(), ProtocolError>>>,
    ) -> Option<Reissue> {
        let mut reissue = self.reissue.lock();
        match &*reissue {
            Some((issuing, running)) if *issuing == generation && running.peek().is_none() => {
                Some(running.clone())
            }
            _ => {
                let running = spawn()?.map(issued as fn(_) -> _).shared();
                *reissue = Some((generation, running.clone()));
                Some(running)
            }
        }
    }

    /// The commands holding the last issue of the session: the handles to it beside the
    /// session's own.
    #[cfg(feature = "test-util")]
    fn awaiting_issue(&self) -> usize {
        let reissue = self.reissue.lock();
        reissue
            .as_ref()
            .and_then(|(_, running)| running.strong_count())
            .map_or(0, |handles| handles - 1)
    }
}

/// The sessions a client handed out, sorted by id. Every command reads the table, and does so
/// without a lock; a session start or stop, rare beside the commands, replaces it whole.
#[derive(Default)]
pub(crate) struct Sessions {
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
    pub(crate) fn find<'g>(&self, id: u32, guard: &'g Guard) -> Option<&'g Arc<Session>> {
        // SAFETY: a table is freed only by `replace`, deferred past every reader pinned when it was
        // unlinked, or by `drop`, which no reader outlives.
        let issued = unsafe { self.issued.load(Ordering::Acquire, guard).as_ref() }?;
        let at = issued
            .binary_search_by_key(&id, |session| session.id)
            .ok()?;
        Some(&issued[at])
    }

    /// The session `id` names.
    fn get(&self, id: u32) -> Option<Arc<Session>> {
        self.find(id, &epoch::pin()).cloned()
    }

    /// The commands holding the last issue of the session `id` names: the handles to it beside
    /// the session's own.
    #[cfg(feature = "test-util")]
    pub(crate) fn awaiting_issue(&self, id: u32) -> usize {
        self.get(id).map_or(0, |session| session.awaiting_issue())
    }

    /// Takes the session `id` names out of the table.
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

/// Takes the session it names out of its table when dropped, unless the start that opened it
/// succeeded, so a start that fails or is cancelled leaves no entry in the table.
struct Opening<'a> {
    sessions: &'a Sessions,
    id: u32,
    started: bool,
}

impl<'a> Opening<'a> {
    fn new(sessions: &'a Sessions, id: u32) -> Self {
        Self {
            sessions,
            id,
            started: false,
        }
    }

    /// Keeps the session in the table: its start succeeded.
    fn started(mut self) {
        self.started = true;
    }
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        if !self.started {
            self.sessions.remove(self.id);
        }
    }
}

/// What a client presents to have a session issued: the authorization its credentials exchange
/// for at the auth service, or, for a server without authentication, the unsigned token naming
/// its identity.
pub(crate) struct Authorizer {
    auth_endpoint: AuthEndpoint,
    recipient_domain: String,
    identity: String,
    credentials: Arc<SuppliedCredentials>,
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

impl Authorizer {
    pub(crate) fn new(
        auth_endpoint: &AuthEndpoint,
        recipient_domain: &str,
        identity: &str,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Self {
        Self {
            auth_endpoint: auth_endpoint.clone(),
            recipient_domain: recipient_domain.to_string(),
            identity: identity.to_string(),
            credentials: credentials.clone(),
        }
    }

    /// The authorization a session start for `partition` presents, read at each start because the
    /// server checks storage authorization there: an access token the newest call supplied, as it
    /// is, or the authorization the newest credentials exchange for, which the cache or the token
    /// store may serve. For a server without authentication, the unsigned token naming the
    /// identity that [`unsigned_identity_tokens`] gives. Empty when there is none, or after a failed
    /// exchange.
    async fn authorization(&self, partition: Partition) -> Authorization {
        let (identity_token, access_token) = self.credentials.tokens();
        if self.auth_endpoint.is_none() {
            return Authorization {
                token: unsigned_identity_tokens(
                    &self.auth_endpoint,
                    &self.identity,
                    &identity_token,
                    &access_token,
                )
                .map(|(_, token, _)| token)
                .unwrap_or_default(),
                exchanged_with: None,
            };
        }
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

    /// Forgets `refused`, exchanged with `identity_token` for `partition`, which the cache would
    /// otherwise serve until it expires.
    async fn forget(&self, partition: Partition, identity_token: &str, refused: &str) {
        crate::auth::exchange::forget_refused_authorization(
            &self.auth_endpoint,
            &self.identity,
            partition,
            &self.recipient_domain,
            identity_token,
            refused,
        )
        .await;
    }
}

/// A client that asks its server to issue a session.
pub(crate) trait IssueSession: Sync {
    /// What the client presents to have a session issued.
    fn authorizer(&self) -> &Authorizer;

    /// The sessions the client hands out.
    fn sessions(&self) -> &Sessions;

    /// The client, while anything holds it, for a task that outlives the call that starts it.
    fn upgrade(&self) -> Option<Arc<Self>>;

    /// Whether `bound` names a session on the connection the client's commands go out on now.
    fn is_live(&self, bound: BoundSession) -> bool;

    /// Asks the server to start `session` presenting `token`, and binds the session to the id the
    /// server answers on the connection the request went to.
    fn send_issue(
        &self,
        session: &Session,
        token: &str,
    ) -> impl Future<Output = Result<(), ProtocolError>> + Send;

    /// Stops the session `bound` names on the connection it is bound to, and on no other, where
    /// its id names another session or none.
    fn stop(&self, bound: BoundSession) -> impl Future<Output = ()> + Send;
}

/// Starts a session for `partition` under `correlation_id` through `client`, and hands out its id
/// once the server issued it.
pub(crate) async fn start(
    client: &impl IssueSession,
    partition: Partition,
    correlation_id: &str,
) -> Result<u32, ProtocolError> {
    let sessions = client.sessions();
    let session = sessions.open(partition, correlation_id);
    let opening = Opening::new(sessions, session.id);
    issue(client, &session).await?;
    opening.started();
    Ok(session.id)
}

/// Stops the session `session_id` names, once no issue of it runs, on the connection it is bound
/// to. A session no longer handed out is stopped already.
pub(crate) async fn stop(client: &impl IssueSession, session_id: u32) -> Result<(), ProtocolError> {
    let bound = {
        let Some(session) = client.sessions().get(session_id) else {
            return Ok(());
        };
        let _rebinding = session.rebinding.acquire().await;
        client
            .sessions()
            .remove(session_id)
            .map(|_| session.bound())
    };
    if let Some(bound) = bound {
        client.stop(bound).await;
    }
    Ok(())
}

/// Issues the session `session_id` names again on the live connection, unless it is bound to the
/// connection of `generation`, the one the caller found it stale on, already. The issue runs as a
/// task of its own ([`reissue`]), which every command that finds the session stale on the same
/// generation while it runs awaits for its outcome, and which runs to its end when they are
/// dropped.
pub(crate) async fn rebind<T: IssueSession + Send + 'static>(
    client: &T,
    session_id: u32,
    generation: u32,
) -> Result<(), ProtocolError> {
    let running = {
        let session = client
            .sessions()
            .get(session_id)
            .ok_or_else(|| not_open(session_id))?;
        if session.bound().generation == generation {
            return Ok(());
        }
        session
            .reissue_on(generation, || {
                let client = client.upgrade()?;
                Some(lore_spawn_net!(reissue(client, session.clone())))
            })
            .ok_or_else(|| not_open(session_id))?
    };
    running.await
}

/// Issues `session` again once no stop or other issue of it runs, unless a stop took it from the
/// table or an issue bound it to the live connection first.
async fn reissue<T: IssueSession>(
    client: Arc<T>,
    session: Arc<Session>,
) -> Result<(), ProtocolError> {
    let _rebinding = session.rebinding.acquire().await;
    if client.sessions().find(session.id, &epoch::pin()).is_none() {
        return Err(not_open(session.id));
    }
    if client.is_live(session.bound()) {
        return Ok(());
    }
    issue(&*client, &session).await
}

/// Issues `session` through `client`, authorized for its partition.
async fn issue(client: &impl IssueSession, session: &Session) -> Result<(), ProtocolError> {
    let authorization = client.authorizer().authorization(session.partition).await;
    issue_presenting(client, session, authorization).await
}

/// Issues `session` presenting `authorization`, and once more with a new authorization should the
/// server refuse an exchanged one. Its own future, so that what it holds is not held beside the
/// exchange that produced `authorization`.
async fn issue_presenting(
    client: &impl IssueSession,
    session: &Session,
    authorization: Authorization,
) -> Result<(), ProtocolError> {
    let Authorization {
        token,
        exchanged_with,
    } = authorization;
    match (client.send_issue(session, &token).await, exchanged_with) {
        (Err(refusal), Some(identity_token))
            if refusal.is_not_authorized() && !token.is_empty() =>
        {
            Box::pin(issue_reauthorized(
                client,
                session,
                token,
                identity_token,
                refusal,
            ))
            .await
        }
        (issued, _) => issued,
    }
}

/// Issues the session the server refused once more, with a new authorization in place of
/// `refused`, which was exchanged with `identity_token`. Cold: only after the server refused an
/// exchanged authorization.
///
/// The cache would serve `refused` until it expires, so it is forgotten first, and the exchange
/// then serves an authorization another caller put in its place, or asks the auth service for
/// another. A refusal of the new authorization is final, and so is `refusal` when the exchange
/// yields none, or `refused` again.
async fn issue_reauthorized(
    client: &impl IssueSession,
    session: &Session,
    refused: String,
    identity_token: String,
    refusal: ProtocolError,
) -> Result<(), ProtocolError> {
    let partition = session.partition;
    lore_debug!("Session start for {partition} refused its authorization; exchanging another");
    let authorizer = client.authorizer();
    authorizer
        .forget(partition, &identity_token, &refused)
        .await;
    let authorization = authorizer.authorization(partition).await;
    if authorization.token.is_empty() || authorization.token == refused {
        return Err(refusal);
    }
    client.send_issue(session, &authorization.token).await
}

/// The payload of a session start for `partition` presenting `token`:
/// `action(1=0) + partition_id(16) + corr_len(1) + corr(N) + token_len(2, u16 LE) + token(M)`.
pub(crate) fn session_start_payload(
    partition: Partition,
    correlation_id: &str,
    token: &str,
) -> Bytes {
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
pub(crate) fn started_session_id(response: &[u8]) -> Result<u32, ProtocolError> {
    let Ok(session_id) = <[u8; 4]>::try_from(response) else {
        return Err(ProtocolError::internal(format!(
            "session_start: expected 4-byte response, got {} bytes",
            response.len()
        )));
    };
    Ok(u32::from_le_bytes(session_id))
}
