// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The machine server's Unix sockets, one per container.
//!
//! A connection opening with the storage magic speaks the `lore-storage/0.4` commands, served by
//! the same [`StorageServiceV4`] as the public QUIC endpoint. One opening with the HTTP/2 client
//! preface is forwarded to the upstream host it names; see [`proxy`]. Any other is closed.
//!
//! The sockets' reads run on the net runtime with the rest of the server's network I/O. Storage
//! messages are read whole and each is handled on the core runtime as soon as it is read. A
//! handler writes its response itself, on the core runtime, unless someone else is writing, in
//! which case the response joins their `writev`; responses go out in completion order. The
//! connection's drain task, on the net runtime, writes what a socket pushes back on and what is
//! still queued after a few rounds.
//!
//! Admission counts requests, as on QUIC: a connection has at most `credits` requests in
//! handling. Bytes are bounded by what is held: request bodies until their handler finishes, the
//! largest response for a read until its handler finishes, and responses the socket has not taken
//! yet. The reader takes a request only while the connection's held bytes stay under
//! `credit_bytes`, and the socket's large bodies, reads and unwritten responses under
//! `socket_bytes`. Past a limit it stops reading, and the socket's own buffer pushes back. A peer
//! that takes no response for QUIC's idle timeout is dropped, as QUIC drops one.
#[cfg(not(feature = "test-util"))]
mod proxy;
#[cfg(feature = "test-util")]
pub mod proxy;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use bytes::Bytes;
use lore_base::lore_spawn_core;
use lore_base::lore_spawn_net;
use lore_base::lore_spawn_net_nocontext;
use lore_base::runtime::LORE_CONTEXT;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use lore_transport::local_socket;
use lore_transport::local_socket::AsyncStream;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use lore_transport::uds::CLIENT_HELLO_SIZE;
use lore_transport::uds::MAGIC;
use lore_transport::uds::MAX_MESSAGE;
use lore_transport::uds::ServerHello;
use lore_transport::uds::VERSION;
use lore_transport::uds::io::Backlog;
use lore_transport::uds::io::FrameReader;
use lore_transport::uds::io::Message;
use lore_transport::uds::io::SocketWriter;
use lore_transport::uds::io::Wakeup;
use lore_transport::uds::parse_client_hello;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::auth::jwt::JwtVerifier;
use crate::auth::jwt::TokenVerification;
use crate::authnz::repository_authorizer::AuthorizerSelection;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::select_repository_authorizer;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::session::SessionMap;
use crate::quic::ProtocolErrorInfo;
use crate::quic::QuicService;
use crate::quic::storage_service_v4::StorageServiceV4;
use crate::settings::AuthSettings;
use crate::settings::UdsSettings;

/// A connection's requests in handling count in the low bits of one word and their bytes above,
/// so a handler returns both with one addition.
const COUNT_BITS: u32 = 20;

/// Most requests a connection may have in handling.
const MAX_CREDITS: usize = (1 << COUNT_BITS) - 1;

/// Put bodies below this are copied out of the read buffer, which they would otherwise keep alive
/// for as long as a store holds them. It is [`SOCKET_COUNTED`], so that every put body that keeps
/// its read buffer alive counts against the socket's budget.
const COPY_BELOW: usize = SOCKET_COUNTED;

/// Connection ids for the socket, kept apart from QUIC's.
static NEXT_CONNECTION: AtomicUsize = AtomicUsize::new(1 << 48);

/// Session ids for every socket connection. Shared, so an id from a closed connection names no
/// session on a later one until the counter wraps, and seeded from the clock, so a restart is
/// unlikely to issue the ids of the process before.
static SESSION_IDS: LazyLock<Arc<AtomicU32>> = LazyLock::new(|| {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    Arc::new(AtomicU32::new(((nanos ^ (nanos >> 32)) as u32) | 1))
});

struct Shared {
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    token_verification: Arc<TokenVerification>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    user_agent_filter: Arc<UserAgentFilter>,
    limits: Limits,
    max_connections: usize,
    handler_timeout: Duration,
    handshake_timeout: Duration,
    stall_timeout: Duration,
}

impl Shared {
    fn storage_service(&self) -> StorageServiceV4 {
        StorageServiceV4::new(
            self.token_verification.clone(),
            self.repository_authorizer.clone(),
            self.immutable_store.clone(),
            self.local_store.clone(),
            self.mutable_store.clone(),
            self.user_agent_filter.clone(),
            SessionMap::sharing_ids(SESSION_IDS.clone()),
        )
    }
}

/// Refuses to run a server whose process names a machine server's socket in `LORE_UDS_SOCKET`,
/// given as `route`: the transport would send the server's own gRPC, to its upstream and to the
/// authorization service, through that socket.
pub fn refuse_socket_route(route: Option<&Path>) -> Result<()> {
    match route {
        Some(socket) => anyhow::bail!(
            "{} names {}: unset it for the server, whose own gRPC would go through that socket",
            lore_transport::uds::SOCKET_ENV,
            socket.display()
        ),
        None => Ok(()),
    }
}

/// Refuses `settings` the sockets cannot run under: no socket, a forward key that is more than a
/// host name, which no request's host would match, a handler timeout the server's hello cannot
/// state in its 16 bits of seconds, or an authorizer, chosen from `auth` and `auth_url`, that does
/// not tell partitions apart, as the allow-all one of a server without `[server.auth]` does not.
/// Warns of a `forward` that names no host, under which every gRPC request is refused.
pub fn check_settings(
    settings: &UdsSettings,
    auth: Option<&AuthSettings>,
    auth_url: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(
        !settings.sockets.is_empty(),
        "[server.uds] sockets names no socket; list one path per container"
    );
    for host in settings.forward.keys() {
        anyhow::ensure!(
            is_bare_host(host),
            "[server.uds] forward key {host:?} must be a host name alone, without a scheme, port \
             or user: requests are forwarded by their host"
        );
    }
    if settings.forward.is_empty() {
        warn!(
            "[server.uds] forward names no host, so every gRPC request on the sockets is refused"
        );
    }
    anyhow::ensure!(
        (1..=u16::MAX as u64).contains(&settings.handler_timeout_seconds),
        "[server.uds] handler_timeout_seconds must be between 1 and {}",
        u16::MAX
    );
    match select_repository_authorizer(auth, auth_url)? {
        AuthorizerSelection::AuthClient | AuthorizerSelection::ResourceGrants => Ok(()),
        selection => anyhow::bail!(
            "[server.uds] needs an authorizer that grants per partition, not {selection:?}"
        ),
    }
}

/// Whether `key` is a host name alone, as the forwarder matches a request's host against it.
fn is_bare_host(key: &str) -> bool {
    key.parse::<http::uri::Authority>()
        .is_ok_and(|authority| authority.port().is_none() && authority.host() == key)
}

/// The sockets a server serves and its forward targets, ready before it serves anything.
pub struct Bound {
    listeners: Vec<local_socket::AsyncListener>,
    forward: proxy::ForwardTargets,
}

/// Claims, binds and registers every socket `settings` names and checks the forward targets, so
/// that a socket the server cannot have, or a forward target that is no valid URL, fails startup
/// before any endpoint serves. A failure gives up the sockets bound before it.
pub async fn bind_sockets(settings: &UdsSettings) -> Result<Bound> {
    let forward = proxy::ForwardTargets::new(
        settings.forward.clone(),
        crate::util::setup_execution("uds-forward", String::new(), String::new()),
    )?;
    let mut listeners = Vec::with_capacity(settings.sockets.len());
    for path in &settings.sockets {
        let listener = bind(path, settings.socket_mode)?;
        listeners.push(
            register(listener)
                .await
                .with_context(|| format!("registering socket {}", path.display()))?,
        );
    }
    Ok(Bound { listeners, forward })
}

/// Serves `bound`, from [`bind_sockets`], until shutdown.
#[allow(clippy::too_many_arguments)]
pub async fn launch_uds_server(
    settings: UdsSettings,
    bound: Bound,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    jwt_verifier: JwtVerifier,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    user_agent_filter: Arc<UserAgentFilter>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let Bound { listeners, forward } = bound;
    let shared = Arc::new(Shared {
        immutable_store,
        local_store,
        mutable_store,
        token_verification: Arc::new(TokenVerification::Verify(jwt_verifier)),
        repository_authorizer,
        user_agent_filter,
        limits: Limits {
            credits: (settings.credits as usize).clamp(1, MAX_CREDITS),
            credit_bytes: (settings.credit_bytes as usize).max(2 * MAX_MESSAGE),
            socket_bytes: settings.socket_bytes.max(2 * MAX_MESSAGE),
        },
        max_connections: settings.max_connections_per_socket.max(1),
        handler_timeout: Duration::from_secs(settings.handler_timeout_seconds),
        handshake_timeout: settings.handshake_timeout,
        stall_timeout: settings.stall_timeout,
    });

    info!(
        sockets = listeners.len(),
        forwarded_hosts = settings.forward.len(),
        "Lore socket server listening"
    );

    let mut accept_tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let forwarder = Arc::new(forward.forwarder());
        lore_spawn_net!(
            accept_tasks,
            accept_loop(listener, shared.clone(), forwarder, shutdown_rx.clone())
        );
    }

    let _ = shutdown_rx.wait_for(|&shutdown| shutdown).await;
    accept_tasks.abort_all();
    // Each listener removes its socket file and gives up its claim as its task drops it. Each
    // connection closes itself at the same signal.
    while accept_tasks.join_next().await.is_some() {}
    Ok(())
}

/// Completes at the shutdown signal, or once nothing can send it.
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|&shutdown| shutdown).await;
}

/// Claims and binds the socket at `path`, refusing one another server has claimed or answers on.
/// On Windows it also refuses a directory another account could place a socket in; on Unix the
/// directory's permissions are the operator's.
fn bind(path: &Path, mode: u32) -> Result<local_socket::Listener> {
    let path = &std::path::absolute(path)
        .with_context(|| format!("resolving socket {}", path.display()))?;
    if let Some(directory) = path.parent() {
        local_socket::prepare_directory(directory)?;
    }
    let claim = local_socket::Claim::acquire(path)
        .with_context(|| format!("claiming socket {}", path.display()))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "another server has claimed socket {}, or answers on it",
                    path.display()
                ),
            )
        })?;
    claim
        .listen(mode)
        .with_context(|| format!("binding socket {}", path.display()))
}

/// What all of a socket's storage connections hold together: large request bodies and reads in
/// handling, and unwritten responses.
#[lore_macro::test_pub]
#[derive(Default)]
struct SocketBudget {
    bodies: AtomicUsize,
    queued: Arc<AtomicUsize>,
    /// Woken when a large body is returned, a backlog shrinks or a connection closes.
    room: Arc<Wakeup>,
}

impl SocketBudget {
    /// A connection's backlog, counted against this budget.
    #[lore_macro::test_pub]
    fn backlog(&self, room: Arc<Wakeup>) -> Backlog {
        Backlog {
            queued: AtomicUsize::new(0),
            shared: self.queued.clone(),
            drained: room,
            shared_drained: self.room.clone(),
        }
    }

    /// Whether the socket has room for one more request of any size.
    #[lore_macro::test_pub]
    fn fits(&self, limits: &Limits) -> bool {
        self.bodies.load(Ordering::Relaxed) + self.queued.load(Ordering::Relaxed) + MAX_MESSAGE
            <= limits.socket_bytes
    }
}

/// One of a socket's open connections, counted until its reader and every request it took have
/// finished.
#[lore_macro::test_pub]
struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Registers `listener` with the net runtime, so that its readiness comes from there with the rest
/// of the socket's I/O.
async fn register(listener: local_socket::Listener) -> Result<local_socket::AsyncListener> {
    Ok(lore_spawn_net!(async move { listener.into_async() }).await??)
}

async fn accept_loop(
    listener: local_socket::AsyncListener,
    shared: Arc<Shared>,
    forwarder: Arc<proxy::GrpcProxy>,
    shutdown: watch::Receiver<bool>,
) {
    let path = listener.path();
    let open = Arc::new(AtomicUsize::new(0));
    let budget = Arc::new(SocketBudget::default());
    loop {
        match listener.accept().await {
            Ok(stream) => {
                if open.fetch_add(1, Ordering::Relaxed) >= shared.max_connections {
                    open.fetch_sub(1, Ordering::Relaxed);
                    warn!(socket = %path.display(), "Refusing connection over the socket's limit");
                    continue;
                }
                let slot = ConnectionSlot(open.clone());
                lore_spawn_net_nocontext!(serve_connection(
                    stream,
                    shared.clone(),
                    forwarder.clone(),
                    slot,
                    budget.clone(),
                    shutdown.clone()
                ));
            }
            Err(err) => {
                warn!(socket = %path.display(), error = %err, "Socket accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Serves one connection until it ends or the server shuts down, which closes it, as a QUIC
/// endpoint closes its connections. Requests still in handling then go unanswered, and clients
/// reconnect.
async fn serve_connection(
    mut stream: AsyncStream,
    shared: Arc<Shared>,
    forwarder: Arc<proxy::GrpcProxy>,
    slot: ConnectionSlot,
    budget: Arc<SocketBudget>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut prefix = [0u8; 4];
    match tokio::time::timeout(shared.handshake_timeout, stream.read_exact(&mut prefix)).await {
        Ok(Ok(_)) => {}
        _ => return,
    }
    if prefix != MAGIC[..4] {
        // An HTTP/2 connection sends its whole preface within the timeout a storage hello has, so
        // one that stops partway gives back its place.
        if prefix != proxy::PREFACE[..4] {
            return;
        }
        let mut rest = [0u8; proxy::PREFACE.len() - 4];
        match tokio::time::timeout(shared.handshake_timeout, stream.read_exact(&mut rest)).await {
            Ok(Ok(_)) if rest == proxy::PREFACE[4..] => {}
            _ => return,
        }
        // A task of its own, so the shutdown signal costs the connection's I/O nothing; dropping
        // it closes the connection.
        let mut serving = lore_spawn_net_nocontext!(forwarder.serve(proxy::Prefixed::new(stream)));
        tokio::select! {
            _ = &mut serving => {}
            () = stopped(&mut shutdown) => serving.abort(),
        }
        return;
    }
    let connection_id = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
    serve_storage(
        stream,
        prefix,
        shared,
        connection_id,
        slot,
        budget,
        shutdown,
    )
    .await;
}

/// Closes `writer`, and with it its connection, at the shutdown signal. Ends as the connection
/// closes otherwise. A task of its own, so the signal costs the connection's reads nothing.
async fn close_at_shutdown(mut shutdown: watch::Receiver<bool>, writer: Arc<SocketWriter>) {
    tokio::select! {
        () = stopped(&mut shutdown) => writer.close(),
        () = writer.closed() => {}
    }
}

/// A connection's limits, as admission applies them.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct Limits {
    /// Requests in handling.
    credits: usize,
    /// Request bodies and payload responses in handling, and unwritten responses.
    credit_bytes: usize,
    /// Large request bodies and payload responses in handling, and unwritten responses, for all
    /// of a socket's storage connections together.
    socket_bytes: usize,
}

/// What one connection's handlers hand back as they finish.
#[lore_macro::test_pub]
struct Admission {
    /// Requests and their body bytes returned, summed as [`held`] adds them up.
    returned: AtomicU64,
    backlog: Arc<Backlog>,
    socket: Arc<SocketBudget>,
    /// Woken when a handler returns its request, the backlog shrinks or the connection closes.
    room: Arc<Wakeup>,
    /// Held by the reader and by every request in handling, through their references.
    _slot: ConnectionSlot,
}

impl Admission {
    #[lore_macro::test_pub]
    fn new(
        backlog: Arc<Backlog>,
        socket: Arc<SocketBudget>,
        room: Arc<Wakeup>,
        slot: ConnectionSlot,
    ) -> Self {
        Self {
            returned: AtomicU64::new(0),
            backlog,
            socket,
            room,
            _slot: slot,
        }
    }

    #[lore_macro::test_pub]
    fn finish(&self, bytes: usize) {
        if bytes >= SOCKET_COUNTED {
            self.socket.bodies.fetch_sub(bytes, Ordering::SeqCst);
            self.socket.room.wake();
        }
        self.returned.fetch_add(held(bytes), Ordering::SeqCst);
        self.room.wake();
    }
}

/// A request holding at least this many bytes counts against the socket's budget as well as its
/// connection's. Smaller ones count by number alone and touch nothing the socket's connections
/// share.
#[lore_macro::test_pub]
const SOCKET_COUNTED: usize = 4096;

/// The bytes a request holds while in handling: its body, or the largest response for a command
/// that answers with a payload, so that the response is counted before it exists.
#[lore_macro::test_pub]
fn admitted_bytes(header: &CommandHeader, body: usize) -> usize {
    if header.cmd == Command::Get as u8 || header.cmd == Command::GetResolved as u8 {
        MAX_MESSAGE
    } else {
        body
    }
}

/// One request holding `bytes` of body, as the reader and handlers count it.
#[lore_macro::test_pub]
fn held(bytes: usize) -> u64 {
    ((bytes as u64) << COUNT_BITS) + 1
}

/// What stops the reader taking another request.
#[lore_macro::test_pub]
#[derive(Debug)]
enum Blocked {
    Connection,
    Socket,
}

/// The reader's side of admission: what it has issued, against what handlers have returned. Only
/// the handlers' returns are shared, so taking a small request costs no atomic write.
#[lore_macro::test_pub]
struct Gate {
    admission: Arc<Admission>,
    issued: u64,
}

impl Gate {
    #[lore_macro::test_pub]
    fn new(admission: Arc<Admission>) -> Self {
        Self {
            admission,
            issued: 0,
        }
    }

    /// Requests, and their body bytes, in handling.
    #[lore_macro::test_pub]
    fn outstanding(&self) -> (usize, usize) {
        let held = self
            .issued
            .wrapping_sub(self.admission.returned.load(Ordering::Relaxed));
        (
            (held & MAX_CREDITS as u64) as usize,
            (held >> COUNT_BITS) as usize,
        )
    }

    fn connection_room(&self, limits: &Limits) -> bool {
        let (requests, bytes) = self.outstanding();
        requests < limits.credits
            && bytes + self.admission.backlog.queued.load(Ordering::Relaxed) + MAX_MESSAGE
                <= limits.credit_bytes
    }

    /// Whether one more request of any size fits.
    #[lore_macro::test_pub]
    fn try_admit(&self, limits: &Limits) -> Result<(), Blocked> {
        if !self.connection_room(limits) {
            Err(Blocked::Connection)
        } else if !self.admission.socket.fits(limits) {
            Err(Blocked::Socket)
        } else {
            Ok(())
        }
    }

    /// Waits until another request fits, or `stop` holds. Every wake looks at both limits again.
    #[lore_macro::test_pub]
    async fn admit(&self, limits: &Limits, stop: impl Fn() -> bool) {
        loop {
            match self.try_admit(limits) {
                Ok(()) => return,
                Err(Blocked::Connection) => {
                    self.admission
                        .room
                        .until(|| stop() || self.connection_room(limits))
                        .await;
                }
                Err(Blocked::Socket) => {
                    let socket = &self.admission.socket;
                    socket.room.until(|| stop() || socket.fits(limits)).await;
                }
            }
            if stop() {
                return;
            }
        }
    }

    #[lore_macro::test_pub]
    fn issue(&mut self, bytes: usize) {
        self.issued = self.issued.wrapping_add(held(bytes));
        if bytes >= SOCKET_COUNTED {
            self.admission
                .socket
                .bodies
                .fetch_add(bytes, Ordering::Relaxed);
        }
    }
}

/// What a connection's handlers share, behind one reference per request.
struct ConnectionState {
    service: StorageServiceV4,
    context: Arc<AttributeMap>,
    admission: Arc<Admission>,
    /// Closed once the reader is done or a write fails; requests still waiting then skip their
    /// work.
    writer: Arc<SocketWriter>,
    handler_timeout: Duration,
}

/// A request in handling. However the handler ends, the request is answered and the bytes it
/// holds returned.
struct InFlight {
    connection: Arc<ConnectionState>,
    header: CommandHeader,
    bytes: usize,
    answered: bool,
}

impl InFlight {
    fn answer(mut self, message: Message) {
        self.connection.writer.send(message);
        self.answered = true;
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if !self.answered {
            self.connection.writer.send(Message::new(
                self.header.response_error(QuicServiceError::Failed as u32),
            ));
        }
        self.connection.admission.finish(self.bytes);
    }
}

async fn serve_storage(
    mut stream: AsyncStream,
    prefix: [u8; 4],
    shared: Arc<Shared>,
    connection_id: usize,
    slot: ConnectionSlot,
    budget: Arc<SocketBudget>,
    shutdown: watch::Receiver<bool>,
) {
    let handshake = async {
        let mut hello = [0u8; CLIENT_HELLO_SIZE];
        hello[..4].copy_from_slice(&prefix);
        tokio::time::timeout(shared.handshake_timeout, stream.read_exact(&mut hello[4..]))
            .await??;
        let Some(version) = parse_client_hello(&hello) else {
            anyhow::bail!("not a storage client hello");
        };
        // Before the version check, so that a client of another version learns this one.
        stream
            .write_all(
                &ServerHello {
                    version: VERSION,
                    handler_timeout_seconds: shared.handler_timeout.as_secs() as u16,
                    max_message: MAX_MESSAGE as u32,
                    credits: shared.limits.credits as u32,
                    credit_bytes: shared.limits.credit_bytes as u32,
                }
                .to_bytes(),
            )
            .await?;
        if version != VERSION {
            anyhow::bail!("unsupported storage socket version {version}");
        }
        Ok(())
    };
    if let Err(err) = handshake.await {
        debug!(connection_id, error = %err, "Storage handshake failed");
        return;
    }

    let context = Arc::new(AttributeMap::default());
    let execution = crate::util::setup_execution("uds", String::new(), String::new());
    let room = Arc::new(Wakeup::default());
    let backlog = Arc::new(budget.backlog(room.clone()));
    let admission = Arc::new(Admission::new(backlog.clone(), budget, room, slot));
    let mut gate = Gate::new(admission.clone());

    let (read, write) = stream.into_split();
    let writer = SocketWriter::new(write, Some(backlog), Some(shared.stall_timeout));
    lore_spawn_net_nocontext!(writer.clone().drain());
    lore_spawn_net_nocontext!(close_at_shutdown(shutdown, writer.clone()));
    let connection = Arc::new(ConnectionState {
        service: shared.storage_service(),
        context,
        admission,
        writer,
        handler_timeout: shared.handler_timeout,
    });
    let mut reader = FrameReader::new(read, MAX_MESSAGE, false);
    let mut closed = std::pin::pin!(connection.writer.closed());
    debug!(connection_id, "Storage connection established");

    loop {
        if gate.try_admit(&shared.limits).is_err() {
            let writer = &connection.writer;
            gate.admit(&shared.limits, || writer.is_closed()).await;
        }
        if connection.writer.is_closed() {
            break;
        }
        let message = tokio::select! {
            biased;
            message = reader.next() => message,
            () = &mut closed => break,
        };
        let (header, mut body) = match message {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(err) => {
                debug!(connection_id, error = %err, "Storage connection read failed");
                break;
            }
        };
        if (header.cmd == Command::Put as u8 || header.cmd == Command::PutResolved as u8)
            && body.len() < COPY_BELOW
        {
            body = Bytes::copy_from_slice(&body);
        }
        let size = admitted_bytes(&header, body.len());
        let request = match connection.service.parse_request_bytes(&header, body) {
            Ok(request) => request,
            Err(err) => {
                debug!(command_header = ?header, error = %err, "Request parse failed");
                connection.writer.send(Message::new(
                    header.response_error(QuicServiceError::InvalidCommand as u32),
                ));
                continue;
            }
        };
        gate.issue(size);
        let in_flight = InFlight {
            connection: connection.clone(),
            header,
            bytes: size,
            answered: false,
        };
        lore_spawn_core!(LORE_CONTEXT.scope(execution.clone(), handle_request(request, in_flight)));
    }

    connection.writer.close();
    debug!(connection_id, "Storage connection closed");
}

/// Handles one request and answers it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn handle_request(
    request: <StorageServiceV4 as QuicService>::ParsedRequestType,
    in_flight: InFlight,
) -> impl Future<Output = ()> + Send + 'static {
    async move {
        let connection = &in_flight.connection;
        if connection.writer.is_closed() {
            return;
        }
        let handling = connection
            .service
            .run_request_handler(connection.context.clone(), request);
        let result = match tokio::time::timeout(connection.handler_timeout, handling).await {
            Ok(result) => result.map_err(|err| connection.service.transform_protocol_error(&err)),
            Err(_) => Err(ProtocolErrorInfo::timed_out()),
        };
        let header = in_flight.header;
        let message = match result {
            Ok(parts) => {
                let mut message = Message::new(header);
                message.parts(parts);
                let size = message.size();
                if size > MAX_MESSAGE {
                    warn!(
                        size,
                        opcode = header.cmd,
                        "Response too large for the socket"
                    );
                    Message::new(header.response_error(QuicServiceError::Failed as u32))
                } else {
                    message.header = header.response_success(size as u32);
                    message
                }
            }
            Err(error) => {
                error.log(&header);
                Message::new(header.response_error(error.response_error_code))
            }
        };
        in_flight.answer(message);
    }
}
