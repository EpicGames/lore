// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The storage socket's client and writer, against a scripted server.
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_transport::ProtocolError;
use lore_transport::auth::AuthEndpoint;
use lore_transport::connection::SuppliedCredentials;
use lore_transport::local_socket;
use lore_transport::local_socket::AsyncStream;
use lore_transport::quic::QuicClientError;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use lore_transport::traits::Storage;
use lore_transport::uds::CLIENT_HELLO_SIZE;
use lore_transport::uds::MAX_MESSAGE;
use lore_transport::uds::SERVER_HELLO_SIZE;
use lore_transport::uds::SOCKET_ENV;
use lore_transport::uds::ServerHello;
use lore_transport::uds::VERSION;
use lore_transport::uds::client::ABANDONED;
use lore_transport::uds::client::Slots;
use lore_transport::uds::client::Ticket;
use lore_transport::uds::client::UdsConnection;
use lore_transport::uds::client::UdsStorageClient;
use lore_transport::uds::client::shared_connection;
use lore_transport::uds::client_hello;
use lore_transport::uds::io::Backlog;
use lore_transport::uds::io::FrameReader;
use lore_transport::uds::io::Message;
use lore_transport::uds::io::SocketWriter;
use lore_transport::uds::io::Wakeup;
use lore_transport::uds::parse_client_hello;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::time::timeout;
use zerocopy::IntoBytes;

use crate::auth::stub::AUTHN_ONE;
use crate::auth::stub::AUTHZ_ONE;
use crate::auth::stub::AUTHZ_TWO;
use crate::auth::stub::StubAuthentication;
use crate::quic::storage_service::test_server::presented_authorization;
use crate::quic::storage_service::test_server::test_partition;
use crate::quic::storage_service::test_server::wait_until;

/// Runs a test-local task on the test's own runtime, so the scripted side never waits on the
/// shared runtimes other tests in the binary keep busy.
#[allow(clippy::disallowed_methods)] // Test-local tasks, outside any Lore context.
fn spawn_local<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}
const PATIENCE: Duration = Duration::from_secs(5);

/// A directory of the test's own for a socket, and the socket's path in it.
fn socket_dir() -> (TempDir, PathBuf) {
    let dir = TempDir::new("lore-uds-test-");
    let path = dir.path().join("server.sock");
    (dir, path)
}

/// Listens at `path`, on the runtime of the test that calls this.
fn listen(path: &Path) -> local_socket::AsyncListener {
    local_socket::Claim::acquire(path)
        .unwrap()
        .expect("the path is unclaimed")
        .listen(0o600)
        .unwrap()
        .into_async()
        .unwrap()
}

/// The two ends of one connection.
async fn pair() -> (AsyncStream, AsyncStream) {
    let (_dir, path) = socket_dir();
    let listener = listen(&path);
    let (near, far) = tokio::join!(local_socket::connect(&path), listener.accept());
    (near.unwrap(), far.unwrap())
}

/// A request as the scripted server read it, with the means to answer it on its connection.
struct Request {
    header: CommandHeader,
    body: Bytes,
    writer: Arc<SocketWriter>,
}

impl Request {
    fn answer(&self, body: &'static [u8]) {
        self.answer_with(Bytes::from_static(body));
    }

    fn answer_with(&self, body: Bytes) {
        self.answer_for(self.header.command_id, body);
    }

    /// Answers on the request's connection as if for the request `command_id` names.
    fn answer_for(&self, command_id: u32, body: Bytes) {
        let mut header = self.header;
        header.command_id = command_id;
        header.size_or_status = body.len() as u32;
        let mut message = Message::new(header);
        message.payload(body);
        assert!(self.writer.send(message));
    }

    /// Refuses the request with `status`.
    fn refuse(&self, status: QuicServiceError) {
        assert!(
            self.writer
                .send(Message::new(self.header.response_error(status as u32)))
        );
    }

    /// Hangs up the connection the request came on.
    fn hang_up(&self) {
        self.writer.close();
    }
}

/// A server that grants `credits` and hands every request it reads to the test.
struct ScriptedServer {
    _dir: TempDir,
    path: PathBuf,
    requests: mpsc::UnboundedReceiver<Request>,
    accepting: tokio::task::JoinHandle<()>,
}

impl ScriptedServer {
    fn start(credits: u32) -> Self {
        Self::stating(credits, 100)
    }

    /// A server that states `handler_timeout_seconds` in its hello.
    fn stating(credits: u32, handler_timeout_seconds: u16) -> Self {
        Self::answering(ServerHello {
            version: VERSION,
            handler_timeout_seconds,
            max_message: MAX_MESSAGE as u32,
            credits,
            credit_bytes: 1 << 24,
        })
    }

    /// A server that answers a client's hello with `hello`.
    fn answering(hello: ServerHello) -> Self {
        let (dir, path) = socket_dir();
        let (requests, accepting) = Self::accept(&path, hello);
        Self {
            _dir: dir,
            path,
            requests,
            accepting,
        }
    }

    fn accept(
        path: &Path,
        hello: ServerHello,
    ) -> (
        mpsc::UnboundedReceiver<Request>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = listen(path);
        let (sender, requests) = mpsc::unbounded_channel();
        let accepting = spawn_local(async move {
            loop {
                let Ok(stream) = listener.accept().await else {
                    return;
                };
                spawn_local(serve(stream, hello, sender.clone()));
            }
        });
        (requests, accepting)
    }

    /// Hands the socket to a server that answers hellos with `hello`, as a restart into another
    /// build would. Connections already open stay with the old server.
    async fn replace(&mut self, hello: ServerHello) {
        self.accepting.abort();
        // The old listener holds the claim on the path until its task has dropped it.
        let _ = (&mut self.accepting).await;
        let _ = std::fs::remove_file(&self.path);
        (self.requests, self.accepting) = Self::accept(&self.path, hello);
    }

    /// Stops taking connections, as a server that went away.
    fn stop_accepting(&self) {
        self.accepting.abort();
        let _ = std::fs::remove_file(&self.path);
    }

    async fn next(&mut self) -> Request {
        timeout(PATIENCE, self.requests.recv())
            .await
            .expect("the client sent a request")
            .expect("the server is running")
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

async fn serve(
    mut stream: AsyncStream,
    hello: ServerHello,
    requests: mpsc::UnboundedSender<Request>,
) {
    let mut client_hello = [0u8; CLIENT_HELLO_SIZE];
    stream.read_exact(&mut client_hello).await.unwrap();
    stream.write_all(&hello.to_bytes()).await.unwrap();
    let (read, write) = stream.into_split();
    let writer = SocketWriter::new(write, None, None);
    spawn_local(writer.clone().drain());
    let mut reader = FrameReader::new(read, MAX_MESSAGE, false);
    loop {
        let Ok(Some((header, body))) = reader.next().await else {
            return;
        };
        let request = Request {
            header,
            body,
            writer: writer.clone(),
        };
        if requests.send(request).is_err() {
            return;
        }
    }
}

fn identify(body: &'static [u8]) -> Message {
    identify_with(Bytes::from_static(body))
}

fn identify_with(body: Bytes) -> Message {
    let mut message = Message::new(CommandHeader::new_with_session(
        Command::ClientIdentify as u8,
        0,
        0,
        0,
    ));
    message.payload(body);
    message
}

/// A slot reserved on `connection` at once, or `None` where every slot is taken.
fn reserve_now(connection: &UdsConnection) -> Option<Ticket<'_>> {
    let reserving = std::pin::pin!(connection.reserve());
    match reserving.poll(&mut std::task::Context::from_waker(
        futures::task::noop_waker_ref(),
    )) {
        Poll::Ready(Ok(ticket)) => Some(ticket),
        _ => None,
    }
}

/// Records that a waker was woken, for a test to wait on.
#[derive(Default)]
struct Woken(AtomicBool);

impl futures::task::ArcWake for Woken {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.store(true, Ordering::SeqCst);
    }
}

/// A server of another version, or one stating messages larger than the protocol's largest, is
/// refused as it answers, not waited out as a server that is away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_of_another_version_or_with_larger_messages_is_refused() {
    let stated = ScriptedServer::stating(16, 100);
    let hello = ServerHello {
        version: VERSION,
        handler_timeout_seconds: 100,
        max_message: MAX_MESSAGE as u32,
        credits: 16,
        credit_bytes: 1 << 24,
    };
    for hello in [
        ServerHello {
            version: VERSION + 1,
            ..hello
        },
        ServerHello {
            max_message: MAX_MESSAGE as u32 + 1,
            ..hello
        },
    ] {
        let server = ScriptedServer::answering(hello);
        let refused = timeout(PATIENCE, shared_connection(&server.path))
            .await
            .expect("the connect reports");
        assert!(
            matches!(refused, Err(ProtocolError::Internal(_))),
            "{hello:?} is refused"
        );
    }
    assert!(
        timeout(PATIENCE, shared_connection(&stated.path))
            .await
            .expect("the connect reports")
            .is_ok(),
        "the server's own hello is accepted"
    );
}

/// The socket is the process's whole route, so a `lore-unix` remote is not reached through a
/// socket the process never named.
#[test]
fn a_lore_unix_remote_without_the_socket_variable_is_refused() {
    assert!(
        std::env::var_os(SOCKET_ENV).is_none(),
        "the unit tests run without {SOCKET_ENV}"
    );

    let refused = lore_transport::uds::socket();
    let parsed = lore_transport::connection::parse("lore-unix://example.com/r").err();

    assert!(lore_transport::uds::route().is_none());
    assert!(
        refused.as_ref().is_err_and(ProtocolError::is_internal),
        "{refused:?}"
    );
    assert!(
        parsed.as_ref().is_some_and(ProtocolError::is_internal),
        "{parsed:?}"
    );
}

/// A process that names no socket keeps a gRPC channel's URI as it is, TLS and all.
#[test]
fn a_process_without_a_socket_keeps_a_channels_uri() {
    assert!(
        std::env::var_os(SOCKET_ENV).is_none(),
        "the unit tests run without {SOCKET_ENV}"
    );

    let uri = lore_transport::uds::channel_uri("https://h".to_string());

    assert_eq!(uri, "https://h");
}

/// A process that names no socket opens the storage connections it asks for, within the limit.
#[test]
fn a_process_without_a_socket_opens_the_storage_connections_it_asks_for() {
    assert!(
        std::env::var_os(SOCKET_ENV).is_none(),
        "the unit tests run without {SOCKET_ENV}"
    );

    let most = lore_transport::connection::MAX_STORAGE_CONNECTIONS;
    for (requested, opened) in [(0, 1), (1, 1), (4, 4), (most + 1, most)] {
        assert_eq!(
            lore_transport::connection::storage_connection_count(requested),
            opened,
            "{requested} requested"
        );
    }
}

/// A hello with a flag or reserved byte set is not one this version reads, so a server takes it
/// for no storage client at all.
#[test]
fn a_client_hello_with_flags_or_reserved_bytes_set_is_not_read() {
    let hello = client_hello();
    let set = |at: usize| {
        let mut hello = hello;
        hello[at] = 1;
        parse_client_hello(&hello)
    };

    assert_eq!(parse_client_hello(&hello), Some(VERSION));
    for at in 10..CLIENT_HELLO_SIZE {
        assert_eq!(set(at), None, "byte {at} set");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_complete_in_the_order_the_server_answers() {
    let mut server = ScriptedServer::start(16);
    let connection = shared_connection(&server.path).await.unwrap();

    let first = connection.reserve().await.unwrap().send(identify(b"first"));
    let second = connection
        .reserve()
        .await
        .unwrap()
        .send(identify(b"second"));
    let mut first = std::pin::pin!(first);
    let a = server.next().await;
    let b = server.next().await;
    let (request_first, request_second) = if &a.body[..] == b"first" {
        (a, b)
    } else {
        (b, a)
    };

    request_second.answer(b"two");
    let answer = timeout(PATIENCE, second).await.unwrap().unwrap();
    assert_eq!(&answer[..], b"two");
    assert!(futures::poll!(first.as_mut()).is_pending());

    request_first.answer(b"one");
    let answer = timeout(PATIENCE, first).await.unwrap().unwrap();
    assert_eq!(&answer[..], b"one");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hang_up_fails_outstanding_requests_and_the_next_connect_reconnects() {
    let mut server = ScriptedServer::start(16);
    let connection = shared_connection(&server.path).await.unwrap();

    let outstanding = connection.reserve().await.unwrap().send(identify(b"lost"));
    server.next().await.hang_up();

    let outcome = timeout(PATIENCE, outstanding).await.unwrap();
    assert!(matches!(outcome, Err(QuicClientError::Terminated)));
    assert!(connection.is_closed());
    assert!(connection.reserve().await.is_err());

    let again = shared_connection(&server.path).await.unwrap();
    assert!(!Arc::ptr_eq(&again, &connection));
    let answered = again.reserve().await.unwrap().send(identify(b"again"));
    server.next().await.answer(b"back");
    assert_eq!(
        &timeout(PATIENCE, answered).await.unwrap().unwrap()[..],
        b"back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_request_frees_its_slot_when_answered() {
    let mut server = ScriptedServer::start(1);
    let connection = shared_connection(&server.path).await.unwrap();

    drop(
        connection
            .reserve()
            .await
            .unwrap()
            .send(identify(b"abandoned")),
    );
    let abandoned = server.next().await;

    // The only slot is held until the response lands.
    let waiting = connection.reserve();
    let mut waiting = std::pin::pin!(waiting);
    assert!(futures::poll!(waiting.as_mut()).is_pending());

    abandoned.answer(b"late");
    let ticket = timeout(PATIENCE, waiting).await.unwrap().unwrap();
    let response = ticket.send(identify(b"next"));
    server.next().await.answer(b"next");
    assert_eq!(
        &timeout(PATIENCE, response).await.unwrap().unwrap()[..],
        b"next"
    );
}

/// Many times more requests than the server grants credits for, from tasks on several threads at
/// once, each take their own answer, and leave every slot free and no more than the credits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_far_past_the_credits_from_many_threads_complete_and_free_their_slots() {
    const CREDITS: u32 = 4;
    const TASKS: usize = 64;
    const EACH: usize = 200;
    let mut server = ScriptedServer::start(CREDITS);
    let connection = shared_connection(&server.path).await.unwrap();
    let answering = spawn_local(async move {
        for _ in 0..TASKS * EACH {
            let request = server.next().await;
            request.answer_with(request.body.clone());
        }
        server
    });

    let requesting: Vec<_> = (0..TASKS)
        .map(|task| {
            let connection = connection.clone();
            spawn_local(async move {
                let mut answered = 0;
                for sequence in 0..EACH {
                    let body = Bytes::from(format!("{task}:{sequence}"));
                    let answer = connection
                        .reserve()
                        .await?
                        .send(identify_with(body.clone()))
                        .await?;
                    answered += usize::from(answer == body);
                }
                Ok::<_, QuicClientError>(answered)
            })
        })
        .collect();
    let mut answered = Vec::new();
    for task in requesting {
        answered.push(
            timeout(PATIENCE, task)
                .await
                .expect("the requests complete")
                .unwrap(),
        );
    }
    let _server = timeout(PATIENCE, answering).await.unwrap().unwrap();
    let free: Vec<_> = (0..CREDITS)
        .map_while(|_| reserve_now(&connection))
        .collect();
    let past_the_credits = reserve_now(&connection);

    assert!(
        answered
            .iter()
            .all(|outcome| matches!(outcome, Ok(count) if *count == EACH)),
        "every request takes its own answer: {answered:?}"
    );
    assert_eq!(free.len(), CREDITS as usize, "every slot is free");
    assert!(past_the_credits.is_none(), "no slot past the credits");
}

/// A writer on `stream`, with its drain task running, counted against a budget it shares.
fn counted_writer(
    stream: AsyncStream,
    stall: Option<Duration>,
) -> (Arc<SocketWriter>, Arc<Backlog>, Arc<AtomicUsize>) {
    let shared = Arc::new(AtomicUsize::new(0));
    let backlog = Arc::new(Backlog {
        queued: AtomicUsize::new(0),
        shared: shared.clone(),
        drained: Arc::new(Wakeup::default()),
        shared_drained: Arc::new(Wakeup::default()),
    });
    let (_read, write) = stream.into_split();
    let writer = SocketWriter::new(write, Some(backlog.clone()), stall);
    spawn_local(writer.clone().drain());
    (writer, backlog, shared)
}

/// Sends large messages until the socket refuses some of them.
fn fill(writer: &SocketWriter, backlog: &Backlog) {
    let payload = Bytes::from(vec![7u8; 64 * 1024]);
    for id in 0..1024 {
        let mut message = Message::new(CommandHeader::new_with_session(
            Command::Get as u8,
            id,
            payload.len(),
            0,
        ));
        message.payload(payload.clone());
        assert!(writer.send(message));
        if backlog.queued.load(Ordering::SeqCst) > 0 {
            return;
        }
    }
    panic!("the socket took 64 MiB without pushing back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_backlog_counts_what_the_socket_refused_until_it_is_written() {
    let (near, mut far) = pair().await;
    let (writer, backlog, shared) = counted_writer(near, None);

    fill(&writer, &backlog);
    let queued = backlog.queued.load(Ordering::SeqCst);
    assert_eq!(shared.load(Ordering::SeqCst), queued);

    let reading = spawn_local(async move {
        let mut sink = vec![0u8; 1 << 20];
        loop {
            if !far.read(&mut sink).await.is_ok_and(|read| read > 0) {
                return;
            }
        }
    });
    timeout(
        PATIENCE,
        backlog
            .drained
            .until(|| backlog.queued.load(Ordering::SeqCst) == 0),
    )
    .await
    .expect("the backlog drains once the peer reads");
    assert_eq!(shared.load(Ordering::SeqCst), 0);
    writer.close();
    reading.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_returns_the_whole_backlog() {
    let (near, _far) = pair().await;
    let (writer, backlog, shared) = counted_writer(near, None);

    fill(&writer, &backlog);
    writer.close();
    assert_eq!(backlog.queued.load(Ordering::SeqCst), 0);
    assert_eq!(shared.load(Ordering::SeqCst), 0);
    assert!(!writer.send(Message::new(CommandHeader::new_with_session(
        Command::Get as u8,
        0,
        0,
        0
    ))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_takes_nothing_is_dropped_after_the_stall_deadline() {
    let (near, _far) = pair().await;
    let (writer, backlog, shared) = counted_writer(near, Some(Duration::from_millis(100)));

    // The kernel may take more after refusing some, so responses keep coming until it stops for
    // good, as they would from a server whose client stopped running.
    let payload = Bytes::from(vec![7u8; 64 * 1024]);
    timeout(PATIENCE, async {
        loop {
            let mut message = Message::new(CommandHeader::new_with_session(
                Command::Get as u8,
                0,
                payload.len(),
                0,
            ));
            message.payload(payload.clone());
            if !writer.send(message) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the writer gives up on a peer that takes nothing");
    assert!(writer.is_closed());
    // The close that turned the send away may still be dropping what was unwritten.
    timeout(
        PATIENCE,
        backlog
            .drained
            .until(|| backlog.queued.load(Ordering::SeqCst) == 0),
    )
    .await
    .expect("closing returns the backlog");
    assert_eq!(shared.load(Ordering::SeqCst), 0);
}

#[test]
fn an_abandoned_slot_completed_twice_at_once_is_freed_once() {
    let slots = Slots::new(2);
    for round in 0..5_000 {
        let index = slots.free.pop().expect("a free slot");
        let slot = &slots.slots[index as usize];
        let tag = slot.tag.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        slot.state.store(ABANDONED, Ordering::SeqCst);
        let id = (tag as u32) << 16 | index as u32;
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    barrier.wait();
                    slots.complete(id, Err(QuicClientError::Terminated));
                });
            }
        });
        let first = slots.free.pop().expect("the other slot");
        let second = slots.free.pop().expect("the abandoned slot, freed");
        assert_ne!(first, second, "round {round}: a slot was freed twice");
        assert_eq!(
            slots.free.pop(),
            None,
            "round {round}: a slot was freed twice"
        );
        slots.free.push(first);
        slots.free.push(second);
    }
}

/// A store client over the scripted server, without token exchange.
async fn storage_client(server: &ScriptedServer) -> Arc<UdsStorageClient> {
    UdsStorageClient::connect(
        server.path.clone(),
        "example.com".to_string(),
        &AuthEndpoint::none(),
        "",
        &Arc::new(SuppliedCredentials::default()),
    )
    .await
    .unwrap()
}

/// A storage client for alice on `server` that trades `AUTHN_ONE` through a stub at `auth_url`,
/// which gives `AUTHZ_ONE` and then `AUTHZ_TWO`.
async fn exchanging_client(
    server: &ScriptedServer,
    auth_url: &str,
) -> (Arc<UdsStorageClient>, Arc<StubAuthentication>) {
    let stub = StubAuthentication::new(|_, served| if served == 0 { AUTHZ_ONE } else { AUTHZ_TWO });
    let client = UdsStorageClient::connect(
        server.path.clone(),
        "example.com".to_string(),
        &AuthEndpoint::new(auth_url.to_string(), stub.clone()),
        "alice",
        &Arc::new(SuppliedCredentials::new(AUTHN_ONE, "")),
    )
    .await
    .unwrap();
    (client, stub)
}

/// A session start on `client` under way, for a repository the exchange authorizes.
fn starting(client: &Arc<UdsStorageClient>) -> tokio::task::JoinHandle<Result<u32, ProtocolError>> {
    let client = client.clone();
    spawn_local(async move { client.session_start(test_partition(), "c").await })
}

/// A session start the server refuses forgets the authorization it presented and starts the
/// session with a newly exchanged one, as on QUIC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_authorization_is_exchanged_again() {
    let mut server = ScriptedServer::start(16);
    let (client, stub) =
        exchanging_client(&server, "stub-auth://auth.socket-refused.test.invalid").await;
    let session = starting(&client);

    let refused = server.next().await;
    refused.refuse(QuicServiceError::NotAuthorized);
    let accepted = server.next().await;
    accepted.answer_with(Bytes::copy_from_slice(&7u32.to_le_bytes()));
    let started = timeout(PATIENCE, session).await.unwrap().unwrap();

    assert!(started.is_ok(), "the session must start: {started:?}");
    assert_eq!(
        [
            presented_authorization(&refused.body),
            presented_authorization(&accepted.body)
        ],
        [AUTHZ_ONE, AUTHZ_TWO]
    );
    assert_eq!(stub.exchanges(), 2);
}

/// A refusal of the newly exchanged authorization is final.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_new_authorization_is_final() {
    let mut server = ScriptedServer::start(16);
    let (client, stub) = exchanging_client(
        &server,
        "stub-auth://auth.socket-refused-twice.test.invalid",
    )
    .await;
    let session = starting(&client);

    server.next().await.refuse(QuicServiceError::NotAuthorized);
    server.next().await.refuse(QuicServiceError::NotAuthorized);
    let started = timeout(PATIENCE, session).await.unwrap().unwrap();

    assert!(started.is_err_and(|err| err.is_not_authorized()));
    assert_eq!(stub.exchanges(), 2);
}

/// Answers a session start with the server's id `server_id`.
async fn open_session(server: &mut ScriptedServer, server_id: u32) {
    let authorize = server.next().await;
    assert_eq!(authorize.header.cmd, Command::Authorize as u8);
    authorize.answer_with(Bytes::copy_from_slice(&server_id.to_le_bytes()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_the_closed_connection_lost_goes_out_again_on_a_new_one() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;

    let starting = {
        let client = client.clone();
        spawn_local(async move {
            client
                .session_start(Partition::default(), "correlation")
                .await
        })
    };
    open_session(&mut server, 7).await;
    let session = timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap();

    let storing = {
        let client = client.clone();
        spawn_local(async move {
            client
                .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
                .await
        })
    };
    let lost = server.next().await;
    assert_eq!(lost.header.cmd, Command::MutableStore as u8);
    assert_eq!(lost.header.session_id, 7);
    lost.hang_up();

    // The session opens again on the new connection, and the command follows it there.
    open_session(&mut server, 9).await;
    let again = server.next().await;
    assert_eq!(again.header.cmd, Command::MutableStore as u8);
    assert_eq!(again.header.session_id, 9);
    again.answer(b"");
    timeout(PATIENCE, storing).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_lost_twice_reports_a_disconnect() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;

    let starting = {
        let client = client.clone();
        spawn_local(async move {
            client
                .session_start(Partition::default(), "correlation")
                .await
        })
    };
    open_session(&mut server, 7).await;
    let session = timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap();

    let storing = {
        let client = client.clone();
        spawn_local(async move {
            client
                .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
                .await
        })
    };
    server.next().await.hang_up();
    open_session(&mut server, 9).await;
    server.next().await.hang_up();
    let outcome = timeout(PATIENCE, storing).await.unwrap().unwrap();
    assert!(matches!(outcome, Err(ProtocolError::Disconnected(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_keeps_answering_is_not_given_up_on() {
    let mut server = ScriptedServer::start(16);
    let connection = shared_connection(&server.path).await.unwrap();

    let waiting = connection.reserve().await.unwrap().send(identify(b"slow"));
    let _slow = server.next().await;
    assert!(
        connection.stalled(connection.advance(250)),
        "nothing answered for 250 s"
    );

    // A request waiting its turn behind others the server keeps answering is not a lost one.
    let answered = connection.reserve().await.unwrap().send(identify(b"quick"));
    server.next().await.answer(b"done");
    timeout(PATIENCE, answered).await.unwrap().unwrap();
    assert!(!connection.stalled(connection.advance(0)));
    assert!(
        connection.stalled(connection.advance(250)),
        "nothing answered for 250 s since"
    );
    drop(waiting);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_reissues_give_up_together_when_the_server_stays_away() {
    let mut server = ScriptedServer::start(16);
    let client = UdsStorageClient::connect_with_backoff(
        server.path.clone(),
        "example.com".to_string(),
        &AuthEndpoint::none(),
        "",
        &Arc::new(SuppliedCredentials::default()),
        (100, 100, 3),
    )
    .await
    .unwrap();
    let starting = {
        let client = client.clone();
        spawn_local(async move { client.session_start(Partition::default(), "c").await })
    };
    open_session(&mut server, 7).await;
    let session = timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap();

    let commands: Vec<_> = (0..3)
        .map(|_| {
            let client = client.clone();
            spawn_local(async move {
                client
                    .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
                    .await
            })
        })
        .collect();
    let mut sent = Vec::new();
    for _ in 0..3 {
        sent.push(server.next().await);
    }
    server.stop_accepting();
    sent[0].hang_up();

    for command in commands {
        let outcome = timeout(PATIENCE, command).await.unwrap().unwrap();
        assert!(matches!(outcome, Err(ProtocolError::Disconnected(_))));
    }
    assert!(client.given_up());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_stopped_during_its_reopen_is_stopped_on_the_server() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;
    let starting = {
        let client = client.clone();
        spawn_local(async move { client.session_start(Partition::default(), "c").await })
    };
    open_session(&mut server, 7).await;
    let session = timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap();

    let storing = {
        let client = client.clone();
        spawn_local(async move {
            client
                .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
                .await
        })
    };
    server.next().await.hang_up();
    let reopening = server.next().await;
    assert_eq!(reopening.header.cmd, Command::Authorize as u8);
    let stopping = {
        let client = client.clone();
        spawn_local(async move { client.session_stop(session).await })
    };
    reopening.answer_with(Bytes::copy_from_slice(&9u32.to_le_bytes()));

    // The command the reopen served may reach the server before the stop does.
    let stop = loop {
        let request = server.next().await;
        if request.header.cmd == Command::Authorize as u8 {
            break request;
        }
        assert_eq!(request.header.session_id, 9);
        request.answer(b"");
    };
    assert_eq!(stop.header.session_id, 9);
    assert_eq!(&stop.body[..], &[1]);
    stop.answer(b"");
    timeout(PATIENCE, stopping).await.unwrap().unwrap().unwrap();
    timeout(PATIENCE, storing).await.unwrap().unwrap().ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_start_the_closed_connection_lost_goes_out_again() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;
    let starting = {
        let client = client.clone();
        spawn_local(async move { client.session_start(Partition::default(), "c").await })
    };
    let lost = server.next().await;
    assert_eq!(lost.header.cmd, Command::Authorize as u8);
    lost.hang_up();
    open_session(&mut server, 9).await;
    let session = timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap();

    let storing = {
        let client = client.clone();
        spawn_local(async move {
            client
                .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
                .await
        })
    };
    let stored = server.next().await;
    assert_eq!(stored.header.session_id, 9);
    stored.answer(b"");
    timeout(PATIENCE, storing).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deadline_stays_past_the_handler_timeout_the_server_states() {
    let mut server = ScriptedServer::start(16);
    let connection = shared_connection(&server.path).await.unwrap();
    let waiting = connection.reserve().await.unwrap().send(identify(b"slow"));
    let _slow = server.next().await;
    // The scripted server states 100 s, so the deadline is 200 s rather than the 120 s floor.
    assert!(!connection.stalled(connection.advance(150)));
    assert!(connection.stalled(connection.advance(60)));
    drop(waiting);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_states_no_handler_timeout_gets_the_floor_deadline() {
    let mut server = ScriptedServer::stating(16, 0);
    let connection = shared_connection(&server.path).await.unwrap();
    let waiting = connection.reserve().await.unwrap().send(identify(b"slow"));
    let _slow = server.next().await;
    assert!(!connection.stalled(connection.advance(110)));
    assert!(connection.stalled(connection.advance(20)));
    drop(waiting);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_start_lost_twice_reports_a_disconnect() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;
    let starting = {
        let client = client.clone();
        spawn_local(async move { client.session_start(Partition::default(), "c").await })
    };
    server.next().await.hang_up();
    server.next().await.hang_up();
    let outcome = timeout(PATIENCE, starting).await.unwrap().unwrap();
    assert!(matches!(outcome, Err(ProtocolError::Disconnected(_))));
}

/// Starts a session the server knows as `server_id`.
async fn started(
    client: &Arc<UdsStorageClient>,
    server: &mut ScriptedServer,
    server_id: u32,
) -> u32 {
    let starting = {
        let client = client.clone();
        spawn_local(async move { client.session_start(Partition::default(), "c").await })
    };
    open_session(server, server_id).await;
    timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap()
}

fn store(
    client: &Arc<UdsStorageClient>,
    session: u32,
) -> tokio::task::JoinHandle<Result<(), ProtocolError>> {
    let client = client.clone();
    spawn_local(async move {
        client
            .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
            .await
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_reissues_proceed_once_the_reconnect_they_waited_on_succeeds() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;
    let reopened = started(&client, &mut server, 7).await;
    let elsewhere = started(&client, &mut server, 8).await;

    // The first command's reconnect issues its session again, and the server sits on that.
    let first = store(&client, reopened);
    server.next().await.hang_up();
    let held = server.next().await;
    assert_eq!(held.header.cmd, Command::Authorize as u8);

    // Two more wait on issues: one on the session's, held, one on another session the closed
    // connection carried. A give-up lands while they wait.
    let second = store(&client, reopened);
    let third = store(&client, elsewhere);
    wait_until(
        || client.awaiting_issue(reopened) == 2 && client.awaiting_issue(elsewhere) == 1,
        "the commands await their sessions' issues",
    )
    .await;
    client.record_give_up();

    held.answer_with(Bytes::copy_from_slice(&9u32.to_le_bytes()));
    for _ in 0..4 {
        let request = server.next().await;
        if request.header.cmd == Command::Authorize as u8 {
            request.answer_with(Bytes::copy_from_slice(&10u32.to_le_bytes()));
        } else {
            request.answer(b"");
        }
    }
    for command in [first, second, third] {
        timeout(PATIENCE, command).await.unwrap().unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconnect_that_connects_clears_an_earlier_give_up() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;
    let session = started(&client, &mut server, 7).await;
    client.record_give_up();

    let storing = store(&client, session);
    server.next().await.hang_up();
    open_session(&mut server, 9).await;
    server.next().await.answer(b"");
    timeout(PATIENCE, storing).await.unwrap().unwrap().unwrap();
    assert!(!client.given_up());
}

/// A response polled again once it has completed takes nothing from its slot, which by then
/// serves the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completed_response_polled_again_takes_nothing_from_the_next_request() {
    let mut server = ScriptedServer::start(1);
    let connection = shared_connection(&server.path).await.unwrap();
    let mut first = connection.reserve().await.unwrap().send(identify(b"first"));
    server.next().await.answer(b"one");
    let answered = timeout(PATIENCE, &mut first).await.unwrap().unwrap();
    let mut second = connection
        .reserve()
        .await
        .unwrap()
        .send(identify(b"second"));
    let woken = Arc::new(Woken::default());
    let waker = futures::task::waker(woken.clone());
    let waiting = Pin::new(&mut second).poll(&mut std::task::Context::from_waker(&waker));

    // The answer is in the slot once the second request is woken, so `first` finds it there.
    server.next().await.answer(b"two");
    wait_until(
        || woken.0.load(Ordering::SeqCst),
        "the second request is answered",
    )
    .await;
    let polled_again = futures::poll!(&mut first);

    assert_eq!(&answered[..], b"one");
    assert!(waiting.is_pending());
    assert!(
        polled_again.is_pending(),
        "the completed response stays pending"
    );
    assert_eq!(
        &timeout(PATIENCE, second).await.unwrap().unwrap()[..],
        b"two"
    );
}

/// Writes `bytes` into one end of a connection, then ends it, and reads the other end as a
/// server reads requests, or as a client reads responses with `responses`.
async fn first_message(
    bytes: &[u8],
    responses: bool,
) -> std::io::Result<Option<(CommandHeader, Bytes)>> {
    let (mut near, far) = pair().await;
    near.write_all(bytes).await.unwrap();
    drop(near);
    let mut reader = FrameReader::new(far, 1024, responses);
    timeout(PATIENCE, reader.next())
        .await
        .expect("the reader returns")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_larger_than_the_largest_is_invalid() {
    let header = CommandHeader::new_with_session(Command::Get as u8, 1, 1025, 0);

    let read = first_message(&header.to_bytes_v4(), false).await;

    assert!(
        matches!(&read, Err(error) if error.kind() == std::io::ErrorKind::InvalidData),
        "{read:?}"
    );
}

/// Only a response carries a status; a request that sets the error bit is invalid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_error_bit_is_read_on_a_response_and_refused_on_a_request() {
    let failed = CommandHeader::new_with_session(Command::Get as u8, 1, 0, 0)
        .response_error(QuicServiceError::NotFound as u32);

    let as_request = first_message(&failed.to_bytes_v4(), false).await;
    let as_response = first_message(&failed.to_bytes_v4(), true).await;

    assert!(
        matches!(&as_request, Err(error) if error.kind() == std::io::ErrorKind::InvalidData),
        "{as_request:?}"
    );
    let (header, body) = as_response.unwrap().expect("a response");
    assert!(header.error);
    assert_eq!(header.size_or_status, QuicServiceError::NotFound as u32);
    assert!(body.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_ending_inside_a_message_is_an_unexpected_end() {
    let header = CommandHeader::new_with_session(Command::Get as u8, 1, 10, 0);
    let mut cut = header.to_bytes_v4().to_vec();
    cut.extend_from_slice(b"half");

    let inside = first_message(&cut, false).await;
    let between = first_message(&[], false).await;

    assert!(
        matches!(&inside, Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof),
        "{inside:?}"
    );
    assert!(matches!(between, Ok(None)), "{between:?}");
}

/// Bodies near the largest a message carries, sent back to back, start and end anywhere in the
/// reader's buffer, and each is still read whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bodies_near_the_largest_sent_back_to_back_are_each_read_whole() {
    const BODIES: u32 = 24;
    const BODY: usize = 256 * 1024;
    let (near, far) = pair().await;
    let (_read, write) = near.into_split();
    let writer = SocketWriter::new(write, None, None);
    spawn_local(writer.clone().drain());
    let mut reader = FrameReader::new(far, MAX_MESSAGE, false);

    for index in 0..BODIES {
        let mut message = Message::new(CommandHeader::new_with_session(
            Command::Put as u8,
            index,
            BODY,
            0,
        ));
        message.payload(Bytes::from(vec![index as u8; BODY]));
        assert!(writer.send(message));
    }
    let mut read = Vec::new();
    for _ in 0..BODIES {
        let (header, body) = timeout(PATIENCE, reader.next())
            .await
            .expect("every message arrives")
            .unwrap()
            .expect("a message");
        let whole = body.iter().all(|&byte| byte == header.command_id as u8);
        read.push((header.command_id, body.len(), whole));
    }

    let sent: Vec<_> = (0..BODIES).map(|index| (index, BODY, true)).collect();
    assert_eq!(read, sent);
}

/// Messages from many senders arrive whole, and each sender's in the order it sent them, though
/// the socket pushes back before the peer reads any, leaving the drain task to write the rest
/// once the senders are done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_from_many_senders_arrive_whole_and_in_each_senders_order() {
    const SENDERS: u32 = 8;
    const EACH: u32 = 400;
    let size = |sequence: u32| 1 + (sequence as usize * 37) % 9000;
    let (near, far) = pair().await;
    let (writer, backlog, shared) = counted_writer(near, None);

    let sending: Vec<_> = (0..SENDERS)
        .map(|sender| {
            let writer = writer.clone();
            spawn_local(async move {
                for sequence in 0..EACH {
                    let body = Bytes::from(vec![sender as u8; size(sequence)]);
                    let mut message = Message::new(CommandHeader::new_with_session(
                        Command::Get as u8,
                        sender << 16 | sequence,
                        body.len(),
                        0,
                    ));
                    message.payload(body);
                    assert!(writer.send(message));
                    if sequence % 32 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        })
        .collect();
    for sender in sending {
        timeout(PATIENCE, sender)
            .await
            .expect("a sender never waits")
            .unwrap();
    }
    let queued = backlog.queued.load(Ordering::SeqCst);
    let mut reader = FrameReader::new(far, MAX_MESSAGE, false);
    let mut next = [0u32; SENDERS as usize];
    for _ in 0..SENDERS * EACH {
        let (header, body) = timeout(PATIENCE, reader.next())
            .await
            .expect("every message arrives")
            .unwrap()
            .expect("a message");
        let (sender, sequence) = (header.command_id >> 16, header.command_id & 0xffff);
        assert_eq!(sequence, next[sender as usize], "sender {sender} in order");
        next[sender as usize] += 1;
        assert_eq!(body.len(), size(sequence));
        assert!(body.iter().all(|&byte| byte == sender as u8), "whole");
    }
    timeout(
        PATIENCE,
        backlog
            .drained
            .until(|| backlog.queued.load(Ordering::SeqCst) == 0),
    )
    .await
    .expect("the drain task writes what was queued");

    assert!(
        queued > 0,
        "the socket pushed back, leaving the rest queued"
    );
    assert_eq!(shared.load(Ordering::SeqCst), 0);
}

/// The deadline sweep closes a connection whose request went unanswered past the deadline while
/// nothing else was answered, failing the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweep_closes_a_connection_that_lost_a_request() {
    let mut server = ScriptedServer::start(16);
    let connection = shared_connection(&server.path).await.unwrap();
    let waiting = connection.reserve().await.unwrap().send(identify(b"lost"));
    let _lost = server.next().await;

    // Past the 200 s deadline the scripted server's hello implies, a second before a sweep looks.
    let now = connection.advance(0);
    connection.advance(209 + 9 - (now + 209) % 10);
    wait_until(|| connection.is_closed(), "the sweep closes the connection").await;

    let failed = timeout(PATIENCE, waiting).await.expect("the request fails");
    assert!(
        matches!(failed, Err(QuicClientError::Terminated)),
        "{failed:?}"
    );
}

/// A socket that answers a client's hello with anything but a Lore server's hello is no storage
/// server, refused as such rather than waited out as one that is away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_that_answers_no_lore_hello_is_refused() {
    let (_dir, path) = socket_dir();
    let listener = listen(&path);
    let answering = spawn_local(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut hello = [0u8; CLIENT_HELLO_SIZE];
        stream.read_exact(&mut hello).await.unwrap();
        stream.write_all(&[b'x'; SERVER_HELLO_SIZE]).await.unwrap();
        stream
    });

    let refused = timeout(PATIENCE, shared_connection(&path))
        .await
        .expect("the connect reports");

    let Err(refusal) = refused else {
        panic!("a socket without a Lore hello is refused");
    };
    assert!(refusal.is_internal(), "{refusal:?}");
    drop(answering.await.unwrap());
}

/// A caller dropped while its connect waits on the server's hello leaves the connection it was
/// opening to the next caller, rather than a second connection to the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connect_dropped_during_its_handshake_leaves_its_connection_to_the_next_caller() {
    let (_dir, path) = socket_dir();
    let listener = listen(&path);
    let accepted = Arc::new(AtomicUsize::new(0));
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (requests, _kept) = mpsc::unbounded_channel();
    let hello = ServerHello {
        version: VERSION,
        handler_timeout_seconds: 100,
        max_message: MAX_MESSAGE as u32,
        credits: 16,
        credit_bytes: 1 << 24,
    };
    let accepting = spawn_local({
        let accepted = accepted.clone();
        async move {
            let mut released = Some(released);
            loop {
                let Ok(stream) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                // The first connection gets its hello only once the test releases it.
                if let Some(released) = released.take() {
                    let _ = released.await;
                }
                spawn_local(serve(stream, hello, requests.clone()));
            }
        }
    });

    let dropped = spawn_local({
        let path = path.clone();
        async move { shared_connection(&path).await }
    });
    wait_until(
        || accepted.load(Ordering::SeqCst) == 1,
        "the server accepts the first connect",
    )
    .await;
    dropped.abort();
    assert!(matches!(dropped.await, Err(join) if join.is_cancelled()));
    release.send(()).unwrap();

    let connection = timeout(PATIENCE, shared_connection(&path))
        .await
        .expect("the connect reports")
        .expect("the connection opens");
    assert!(!connection.is_closed());
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the next caller takes the connection the dropped one opened"
    );
    accepting.abort();
}

/// A response whose id names no slot, or a slot since given to another request, completes
/// nothing: the request waiting in that slot takes only its own answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_response_for_no_waiting_request_is_ignored() {
    let mut server = ScriptedServer::start(16);
    let connection = shared_connection(&server.path).await.unwrap();

    let waiting = connection
        .reserve()
        .await
        .unwrap()
        .send(identify(b"waiting"));
    let request = server.next().await;
    let id = request.header.command_id;
    request.answer_for(id | 0xFFFF, Bytes::from_static(b"no such slot"));
    request.answer_for(id ^ (1 << 16), Bytes::from_static(b"an earlier request"));
    request.answer(b"its own");

    assert_eq!(
        &timeout(PATIENCE, waiting).await.unwrap().unwrap()[..],
        b"its own"
    );
}

/// A request larger than the server states it takes fails without being sent, and the
/// connection keeps serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_larger_than_the_server_takes_fails_before_it_is_sent() {
    let mut server = ScriptedServer::answering(ServerHello {
        version: VERSION,
        handler_timeout_seconds: 100,
        max_message: 64,
        credits: 16,
        credit_bytes: 1 << 24,
    });
    let connection = shared_connection(&server.path).await.unwrap();

    let refused = connection
        .reserve()
        .await
        .unwrap()
        .send(identify(&[7u8; 65]))
        .await;
    let fitting = connection.reserve().await.unwrap().send(identify(b"fits"));
    let request = server.next().await;
    request.answer(b"answered");

    assert!(
        matches!(refused, Err(QuicClientError::ClientMessageTooBig)),
        "{refused:?}"
    );
    assert_eq!(
        &request.body[..],
        b"fits",
        "the larger request never went out"
    );
    assert_eq!(
        &timeout(PATIENCE, fitting).await.unwrap().unwrap()[..],
        b"answered"
    );
}

/// A frame the client cannot read leaves the stream unframed, so the connection closes and its
/// requests fail as on a hang-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_the_client_cannot_read_closes_the_connection() {
    let mut server = ScriptedServer::start(16);
    let connection = shared_connection(&server.path).await.unwrap();

    let outstanding = connection.reserve().await.unwrap().send(identify(b"lost"));
    let request = server.next().await;
    let mut oversized = request.header;
    oversized.size_or_status = MAX_MESSAGE as u32 + 1;
    assert!(request.writer.send(Message::new(oversized)));

    let outcome = timeout(PATIENCE, outstanding).await.unwrap();
    assert!(
        matches!(outcome, Err(QuicClientError::Terminated)),
        "{outcome:?}"
    );
    assert!(connection.is_closed());
}

/// Sends what `call` sends to the scripted server, answers it with `body`, and returns what the
/// call made of the answer.
async fn answered<T: Send + 'static>(
    server: &mut ScriptedServer,
    call: impl Future<Output = T> + Send + 'static,
    body: &[u8],
) -> T {
    let calling = spawn_local(call);
    server
        .next()
        .await
        .answer_with(Bytes::copy_from_slice(body));
    timeout(PATIENCE, calling).await.unwrap().unwrap()
}

/// An answer too short for what a command reads, or whose fragment disagrees with its payload,
/// fails that command as the server's fault and leaves the client serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_answer_fails_its_command_and_nothing_else() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;
    let short = b"abc";
    let fragment = Fragment {
        flags: 0,
        size_payload: 10,
        size_content: 10,
    };
    let mut mismatched = fragment.as_bytes().to_vec();
    mismatched.extend_from_slice(short);

    let started = {
        let client = client.clone();
        answered(
            &mut server,
            async move { client.session_start(Partition::default(), "short").await },
            short,
        )
        .await
    };
    let starting = {
        let client = client.clone();
        spawn_local(async move { client.session_start(Partition::default(), "valid").await })
    };
    open_session(&mut server, 7).await;
    let session = timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap();
    let mut failures = vec![("session_start", started.map(drop))];
    for (operation, body) in [("get", &short[..]), ("get", &mismatched[..])] {
        let client = client.clone();
        let read = answered(
            &mut server,
            async move { client.get(session, &Address::default()).await },
            body,
        )
        .await;
        failures.push((operation, read.map(drop)));
    }
    let client_for_metadata = client.clone();
    let metadata = answered(
        &mut server,
        async move {
            client_for_metadata
                .get_metadata(session, &Address::default())
                .await
        },
        short,
    )
    .await;
    failures.push(("get_metadata", metadata.map(drop)));
    let client_for_load = client.clone();
    let loaded = answered(
        &mut server,
        async move {
            client_for_load
                .mutable_load(session, &Hash::default(), KeyType::Untyped)
                .await
        },
        short,
    )
    .await;
    failures.push(("mutable_load", loaded.map(drop)));
    let client_for_store = client.clone();
    let stored = answered(
        &mut server,
        async move {
            client_for_store
                .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
                .await
        },
        b"",
    )
    .await;

    for (operation, failure) in failures {
        assert!(
            matches!(&failure, Err(ProtocolError::Internal(_))),
            "{operation}: {failure:?}"
        );
    }
    assert!(stored.is_ok(), "the client still serves: {stored:?}");
}

/// A server replaced by one of another version fails the commands it finds at once, as a
/// mismatch, rather than waiting it out as a server that is away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_replaced_by_another_version_fails_commands_at_once() {
    let mut server = ScriptedServer::start(16);
    let client = storage_client(&server).await;
    let starting = {
        let client = client.clone();
        spawn_local(async move { client.session_start(Partition::default(), "replaced").await })
    };
    open_session(&mut server, 7).await;
    let session = timeout(PATIENCE, starting).await.unwrap().unwrap().unwrap();

    let storing = {
        let client = client.clone();
        spawn_local(async move {
            client
                .mutable_store(session, Hash::default(), Hash::default(), KeyType::Untyped)
                .await
        })
    };
    let lost = server.next().await;
    server
        .replace(ServerHello {
            version: VERSION + 1,
            handler_timeout_seconds: 100,
            max_message: MAX_MESSAGE as u32,
            credits: 16,
            credit_bytes: 1 << 24,
        })
        .await;
    lost.hang_up();

    let outcome = timeout(PATIENCE, storing).await.unwrap().unwrap();
    assert!(
        matches!(&outcome, Err(ProtocolError::Internal(_))),
        "{outcome:?}"
    );
}
