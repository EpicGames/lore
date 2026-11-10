// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The socket server end to end: a server over in-memory stores on a socket in a temporary
//! directory, reached by the socket storage client, by raw connections and by gRPC.
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use bytes::Bytes;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::HealResult;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_server::auth::jwk::JWKService;
use lore_server::auth::jwk::JWKServiceError;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::JwtVerifier;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::repository_authorizer::AuthClientAuthorizer;
use lore_server::settings::UdsSettings;
use lore_server::uds::bind_sockets;
use lore_server::uds::launch_uds_server;
use lore_storage::CorrelationHold;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::KeyValueStream;
use lore_storage::MutableStore;
use lore_storage::MutableStoreSettings;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::gc_event::GcEventSinkRef;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::store_lock::StoreGuard;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use lore_transport::ProtocolError;
use lore_transport::auth::AuthEndpoint;
use lore_transport::connection::SuppliedCredentials;
use lore_transport::local_socket;
use lore_transport::local_socket::AsyncStream;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use lore_transport::traits::Storage;
use lore_transport::uds::MAX_MESSAGE;
use lore_transport::uds::SERVER_HELLO_SIZE;
use lore_transport::uds::ServerHello;
use lore_transport::uds::SocketConnector;
use lore_transport::uds::VERSION;
use lore_transport::uds::client::UdsStorageClient;
use lore_transport::uds::client_hello;
use lore_transport::uds::io::FrameReader;
use rand::random;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio::time::timeout;
use zerocopy::IntoBytes;

use crate::store::test_support::test_store_create;
use crate::util::test_support::setup_test_execution;

const PATIENCE: Duration = Duration::from_secs(10);
/// A handshake or stall timeout short enough for a test to wait out.
const SHORT_TIMEOUT: Duration = Duration::from_millis(200);
const SECRET: &[u8] = b"uds-server-test-secret";
const AUDIENCE: &str = "lore";

/// Answers every key lookup with the key tokens are signed with.
struct SharedSecret;

#[async_trait]
impl JWKService for SharedSecret {
    async fn get_key(
        &self,
        _kid: &str,
    ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError> {
        Ok((
            DecodingKey::from_secret(SECRET),
            jsonwebtoken::Algorithm::HS256,
        ))
    }

    fn get_cached_key(&self, _kid: &str) -> Option<(DecodingKey, jsonwebtoken::Algorithm)> {
        Some((
            DecodingKey::from_secret(SECRET),
            jsonwebtoken::Algorithm::HS256,
        ))
    }

    async fn refresh_key(
        &self,
        _kid: &str,
    ) -> Result<Option<(DecodingKey, jsonwebtoken::Algorithm)>, JWKServiceError> {
        Ok(None)
    }
}

/// A token granting read and write on `repositories`, signed with `secret`.
fn token_signed_with(secret: &[u8], repositories: &[Partition]) -> String {
    let claims = AuthorizationToken {
        user_id: "tester".to_string(),
        issuer: "test-issuer".to_string(),
        issued_at: 1,
        audience: vec![AUDIENCE.to_string()],
        expires: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600,
        resources: Some(
            repositories
                .iter()
                .map(|repository| ResourcePermission {
                    resource_id: format!("urc-{repository}"),
                    permission: vec!["read".to_string(), "write".to_string()],
                })
                .collect(),
        ),
        ..Default::default()
    };
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("test".to_string());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(secret)).unwrap()
}

fn token(repositories: &[Partition]) -> String {
    token_signed_with(SECRET, repositories)
}

fn payload_address(payload: &Bytes) -> (Address, Fragment) {
    (
        Address {
            hash: Hash::hash_buffer(payload),
            ..Default::default()
        },
        Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        },
    )
}

/// What a test changes about the server before it starts.
#[derive(Default)]
struct Setup {
    immutable: Option<Arc<dyn ImmutableStore>>,
    mutable: Option<Arc<dyn MutableStore>>,
    /// Hosts forwarded to, by the host a request names.
    forward: Vec<(&'static str, String)>,
    /// Sockets served besides [`TestServer::socket`].
    extra_sockets: usize,
    max_connections_per_socket: Option<usize>,
    handler_timeout_seconds: Option<u64>,
    credit_bytes: Option<u32>,
    /// The handshake and stall timeouts, in place of the server's own.
    timeouts: Option<(Duration, Duration)>,
}

/// A socket server over in-memory stores, on a socket in a temporary directory. Shut down when
/// dropped.
struct TestServer {
    socket: PathBuf,
    /// The sockets [`Setup::extra_sockets`] asked for.
    extra_sockets: Vec<PathBuf>,
    shutdown: watch::Sender<bool>,
    _dir: TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// Whether a file is at `path`. Not [`Path::exists`], which follows the reparse point a socket
/// file is on Windows, and finds nothing there.
fn file_at(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A directory in `dir` for sockets, which the server creates. On Windows the server refuses a
/// socket directory that lets other accounts change it, as a temporary directory may inherit.
fn socket_dir(dir: &TempDir) -> PathBuf {
    dir.path().join("sockets")
}

impl TestServer {
    async fn start(setup: Setup) -> Self {
        let dir = TempDir::new("lore-uds-server-");
        let socket_dir = socket_dir(&dir);
        let socket = socket_dir.join("lore.sock");
        let extra_sockets: Vec<PathBuf> = (1..=setup.extra_sockets)
            .map(|index| socket_dir.join(format!("lore-{index}.sock")))
            .collect();
        let sockets: Vec<PathBuf> = std::iter::once(socket.clone())
            .chain(extra_sockets.iter().cloned())
            .collect();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (listening, listens) = tokio::sync::oneshot::channel();
        let mut serving = lore_base::lore_spawn!(serve(
            dir.path().to_path_buf(),
            sockets.clone(),
            setup,
            shutdown_rx,
            Some(listening),
        ));
        // Waits until the sockets listen rather than connecting, which would take one of their
        // places; a socket's file exists from its bind, before it listens.
        timeout(PATIENCE, async {
            // A server that fails to start drops the sender as it returns, so a closed channel is
            // answered with what the server returned.
            if listens.await.is_err() {
                panic!("the server stopped starting: {:?}", (&mut serving).await);
            }
        })
        .await
        .expect("the server's sockets listen within PATIENCE");
        Self {
            socket,
            extra_sockets,
            shutdown,
            _dir: dir,
        }
    }

    /// A storage client presenting `token` as its authorization.
    async fn client(&self, token: &str) -> Arc<UdsStorageClient> {
        UdsStorageClient::connect(
            self.socket.clone(),
            "example.com".to_string(),
            &AuthEndpoint {
                url: "https://auth.invalid".to_string(),
                ..AuthEndpoint::none()
            },
            "tester",
            &Arc::new(SuppliedCredentials::new("", token)),
        )
        .await
        .unwrap()
    }

    /// A raw connection that sent `hello`, with the server's hello read when it answered one. A
    /// connection the server closed at once may refuse the hello itself.
    async fn hello(&self, hello: &[u8]) -> (AsyncStream, Option<ServerHello>) {
        let mut stream = local_socket::connect(&self.socket)
            .await
            .expect("connect to the listening socket");
        if stream.write_all(hello).await.is_err() {
            return (stream, None);
        }
        let mut answer = [0u8; SERVER_HELLO_SIZE];
        let answered = timeout(PATIENCE, stream.read_exact(&mut answer))
            .await
            .expect("the server answers or closes");
        let hello = answered.ok().and_then(|_| ServerHello::from_bytes(&answer));
        (stream, hello)
    }
}

/// Serves `sockets` over stores in `dir`, with what `setup` changes, until `shutdown` turns true.
/// Returns at once with the error that stops the server starting. `listening`, where given, is sent
/// once every socket listens.
async fn serve(
    dir: PathBuf,
    sockets: Vec<PathBuf>,
    setup: Setup,
    shutdown: watch::Receiver<bool>,
    listening: Option<tokio::sync::oneshot::Sender<()>>,
) -> anyhow::Result<()> {
    let mut settings: UdsSettings = toml::from_str("enabled = true").unwrap();
    settings.sockets = sockets;
    settings.credits = 16;
    settings.forward.extend(
        setup
            .forward
            .into_iter()
            .map(|(host, target)| (host.to_string(), target)),
    );
    if let Some(max) = setup.max_connections_per_socket {
        settings.max_connections_per_socket = max;
    }
    if let Some(seconds) = setup.handler_timeout_seconds {
        settings.handler_timeout_seconds = seconds;
    }
    if let Some(bytes) = setup.credit_bytes {
        settings.credit_bytes = bytes;
    }
    if let Some((handshake, stall)) = setup.timeouts {
        settings = settings.timing_out(handshake, stall);
    }
    let immutable = lore_storage::local::immutable_store::create(
        Some(dir.join("immutable")),
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .unwrap();
    let mutable = lore_storage::local::mutable_store::create(
        Some(dir.join("mutable")),
        MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .unwrap();
    let bound = bind_sockets(&settings).await?;
    if let Some(listening) = listening {
        let _ = listening.send(());
    }
    launch_uds_server(
        settings,
        bound,
        setup.immutable.unwrap_or_else(|| immutable.clone()),
        immutable,
        setup.mutable.unwrap_or(mutable),
        JwtVerifier {
            jwk_service: Arc::new(SharedSecret),
            jwt_issuer: None,
            jwt_audience: Some(vec![AUDIENCE.to_string()]),
            jwt_typ: None,
            exchange_issuer: None,
        },
        Arc::new(AuthClientAuthorizer::new(
            "https://auth.invalid".to_string(),
        )),
        Arc::new(UserAgentFilter::default()),
        shutdown,
    )
    .await
}

/// Whether the server closes `stream` without sending anything more.
async fn closes(stream: &mut AsyncStream) -> bool {
    let mut byte = [0u8; 1];
    matches!(
        timeout(PATIENCE, stream.read(&mut byte)).await,
        Ok(Ok(0) | Err(_))
    )
}

/// Whether `stream` reaches its end within [`PATIENCE`], past whatever the server wrote before it
/// closed.
async fn drains_to_end(stream: &mut AsyncStream) -> bool {
    let mut buffer = vec![0u8; 64 * 1024];
    timeout(PATIENCE, async {
        while let Ok(read) = stream.read(&mut buffer).await {
            if read == 0 {
                break;
            }
        }
    })
    .await
    .is_ok()
}

/// Whether a connection's hello is answered within [`PATIENCE`], trying again while the socket
/// has no place for one.
async fn a_place_frees(server: &TestServer) -> bool {
    timeout(PATIENCE, async {
        while server.hello(&client_hello()).await.1.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// A client hello stating `version`.
fn hello_of_version(version: u16) -> Vec<u8> {
    let mut hello = client_hello().to_vec();
    hello[8..10].copy_from_slice(&version.to_le_bytes());
    hello
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_storage_command_round_trips_over_the_socket() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;
            let (repository, source) = (random::<Partition>(), random::<Partition>());
            let client = server.client(&token(&[repository, source])).await;
            let session = client
                .session_start(repository, "round-trip")
                .await
                .unwrap();
            let payload = Bytes::from_static(b"a payload over the socket");
            let (address, fragment) = payload_address(&payload);
            let missing = Address {
                hash: Hash::hash_buffer(b"never stored"),
                ..Default::default()
            };

            client
                .put(session, address, fragment, Some(payload.clone()))
                .await
                .unwrap();
            let (got_fragment, got) = client.get(session, &address).await.unwrap();
            let (_, prioritized) = client.get_priority(session, &address).await.unwrap();
            let metadata = client.get_metadata(session, &address).await.unwrap();
            let statuses = client.query(session, &[address, missing]).await.unwrap();
            let verified = client.verify(session, &address, false).await.unwrap();

            assert_eq!(got, payload);
            assert_eq!(prioritized, payload);
            assert_eq!(got_fragment.size_payload, fragment.size_payload);
            assert_eq!(metadata.size_content, fragment.size_content);
            assert_eq!(&statuses[..], &[0, 3], "a full match, then none");
            assert!(!verified.corrupted);

            let key = Hash::hash_buffer(b"resolved key");
            let resolved_payload = Bytes::from_static(b"a payload behind a key");
            let (resolved_address, resolved_fragment) = payload_address(&resolved_payload);
            client
                .put_resolved(
                    session,
                    &key,
                    resolved_address,
                    resolved_fragment,
                    Some(resolved_payload.clone()),
                )
                .await
                .unwrap();
            let (resolved, _, resolved_got) = client
                .get_resolved(session, &key, &resolved_address.context, 0)
                .await
                .unwrap();
            assert_eq!(resolved, resolved_address.hash);
            assert_eq!(resolved_got, resolved_payload);

            let source_session = client.session_start(source, "copy-source").await.unwrap();
            let copied = Bytes::from_static(b"a payload copied across partitions");
            let (copied_address, copied_fragment) = payload_address(&copied);
            client
                .put(
                    source_session,
                    copied_address,
                    copied_fragment,
                    Some(copied.clone()),
                )
                .await
                .unwrap();
            let target_context = random::<Context>();
            client
                .copy(session, source, copied_address, target_context)
                .await
                .unwrap();
            let (_, copied_got) = client
                .get(
                    session,
                    &Address {
                        hash: copied_address.hash,
                        context: target_context,
                    },
                )
                .await
                .unwrap();
            assert_eq!(copied_got, copied);

            let (branch, first, second) = (random::<Hash>(), random::<Hash>(), random::<Hash>());
            client
                .mutable_store(session, branch, first, KeyType::BranchId)
                .await
                .unwrap();
            let loaded = client
                .mutable_load(session, &branch, KeyType::BranchId)
                .await
                .unwrap();
            let swapped = client
                .mutable_compare_and_swap(session, branch, first, second, KeyType::BranchId)
                .await
                .unwrap();
            let after = client
                .mutable_load(session, &branch, KeyType::BranchId)
                .await
                .unwrap();
            assert_eq!(loaded, first);
            assert_eq!(swapped, first);
            assert_eq!(after, second);

            client.session_stop(source_session).await.unwrap();
            client.session_stop(session).await.unwrap();
        })
        .await;
}

/// A session starts only for a partition a token signed with the server's key grants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_start_without_a_valid_grant_is_refused() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;
            let (repository, other) = (random::<Partition>(), random::<Partition>());

            for (case, token) in [
                ("no token", String::new()),
                (
                    "a token signed with another key",
                    token_signed_with(b"another-secret", &[repository]),
                ),
                ("a token granting another partition", token(&[other])),
            ] {
                let refused = server
                    .client(&token)
                    .await
                    .session_start(repository, "refused")
                    .await;
                assert!(
                    matches!(&refused, Err(error) if lore_base::error::is_refusal(error)),
                    "{case}: {refused:?}"
                );
            }
        })
        .await;
}

/// A loopback gRPC target that answers every call with success, echoing the caller's
/// `authorization` header as `x-authorization`, and the count of connections it has accepted.
async fn echo_target() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = accepted.clone();
    lore_base::lore_spawn!(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::Relaxed);
            let service =
                hyper::service::service_fn(|request: http::Request<hyper::body::Incoming>| {
                    let mut response = http::Response::builder()
                        .header("content-type", "application/grpc")
                        .header("grpc-status", "0");
                    if let Some(token) = request.headers().get(http::header::AUTHORIZATION) {
                        response = response.header("x-authorization", token);
                    }
                    let response = response.body(http_body_util::Empty::<Bytes>::new());
                    async move { response }
                });
            lore_base::lore_spawn!(
                hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
            );
        }
    });
    (format!("http://{address}"), accepted)
}

/// The headers a gRPC call naming `host`, sent over `socket` with `token`, is answered with.
async fn grpc_call(socket: &Path, host: &str, token: &str) -> http::HeaderMap {
    let socket: &'static Path = Box::leak(socket.to_path_buf().into_boxed_path());
    let mut channel = tonic::transport::Endpoint::from_shared(format!("http://{host}"))
        .unwrap()
        .connect_with_connector(SocketConnector::new(socket))
        .await
        .unwrap();
    let request = http::Request::builder()
        .uri(format!("http://{host}/lore.test.Echo/Call"))
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(tonic::body::Body::empty())
        .unwrap();
    std::future::poll_fn(|cx| tower::Service::poll_ready(&mut channel, cx))
        .await
        .unwrap();
    let response = timeout(PATIENCE, tower::Service::call(&mut channel, request))
        .await
        .expect("the call is answered")
        .unwrap();
    response.headers().clone()
}

/// The forwarder authorizes nothing itself: the caller's token reaches the upstream unchanged,
/// for the upstream to authorize, and a host not configured is reached by no one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_reaches_a_forwarded_host_with_the_callers_token_and_no_other_host() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (target, _) = echo_target().await;
            let server = TestServer::start(Setup {
                forward: vec![("forward.test", target)],
                ..Default::default()
            })
            .await;

            let forwarded = grpc_call(&server.socket, "Forward.Test", "the-token").await;
            let refused = grpc_call(&server.socket, "elsewhere.test", "the-token").await;

            assert_eq!(forwarded["grpc-status"], "0");
            assert_eq!(forwarded["x-authorization"], "Bearer the-token");
            assert_eq!(refused["grpc-status"], "7", "PERMISSION_DENIED");
            assert!(refused.get("x-authorization").is_none());
        })
        .await;
}

/// Each socket reaches a forwarded host over an upstream connection of its own, which all of the
/// socket's connections share, so a container that stops reading a response holds up no other
/// container's calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_socket_forwards_over_an_upstream_connection_of_its_own() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (target, accepted) = echo_target().await;
            let server = TestServer::start(Setup {
                forward: vec![("forward.test", target)],
                extra_sockets: 1,
                ..Default::default()
            })
            .await;

            for socket in [&server.socket, &server.socket, &server.extra_sockets[0]] {
                let answered = grpc_call(socket, "forward.test", "the-token").await;
                assert_eq!(answered["grpc-status"], "0");
            }

            assert_eq!(
                accepted.load(Ordering::Relaxed),
                2,
                "one upstream connection per socket"
            );
        })
        .await;
}

/// A client of another version learns the server's version from its hello before the server
/// closes, so it reports a mismatch rather than a server that is away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_of_another_version_learns_the_servers_before_the_close() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;

            let (mut stream, hello) = server.hello(&hello_of_version(VERSION + 1)).await;

            assert_eq!(hello.map(|hello| hello.version), Some(VERSION));
            assert!(closes(&mut stream).await);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_hello_with_a_flag_set_is_closed_unanswered() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;
            let mut flagged = client_hello().to_vec();
            flagged[10] = 1;

            let (_stream, hello) = server.hello(&flagged).await;

            assert!(
                hello.is_none(),
                "a hello with a flag set is closed unanswered, not answered with {hello:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_larger_than_the_largest_closes_the_connection() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;
            let (mut stream, hello) = server.hello(&client_hello()).await;
            assert!(hello.is_some());

            let oversized =
                CommandHeader::new_with_session(Command::Get as u8, 1, MAX_MESSAGE + 1, 0);
            stream.write_all(&oversized.to_bytes_v4()).await.unwrap();

            assert!(closes(&mut stream).await);
        })
        .await;
}

/// A connection over the socket's limit is closed at once, and a place frees once a connection
/// ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_over_the_socket_limit_is_closed_until_one_ends() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup {
                max_connections_per_socket: Some(1),
                ..Default::default()
            })
            .await;
            let (first, first_hello) = server.hello(&client_hello()).await;

            let (_over, over_hello) = server.hello(&client_hello()).await;
            drop(first);
            let freed = timeout(PATIENCE, async {
                loop {
                    if let (stream, Some(_)) = server.hello(&client_hello()).await {
                        return stream;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;

            assert!(first_hello.is_some());
            assert!(
                over_hello.is_none(),
                "the second connection is over the limit"
            );
            assert!(freed.is_ok(), "a connection ending frees its place");
        })
        .await;
}

/// A mutable store whose calls reach `store`, but whose loads never finish where it `hangs`, as a
/// store that hangs. It counts in `holds` the correlation holds it gave out and has not had back.
struct MutableDouble {
    store: Arc<dyn MutableStore>,
    hangs: bool,
    holds: Arc<AtomicUsize>,
}

/// A hold [`MutableDouble`] gave out, counted until it drops.
struct CountedHold(Arc<AtomicUsize>);

impl Drop for CountedHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl MutableStore for MutableDouble {
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.store.clone().hold_for_command().await
    }

    fn hold_correlation(&self, _correlation_id: &str) -> Option<CorrelationHold> {
        self.holds.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(CountedHold(self.holds.clone())))
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        if self.hangs {
            return std::future::pending().await;
        }
        self.store.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        self.store
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.store
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.store.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.store.clone().flush(options).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handler_past_its_timeout_answers_slow_down() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (_, mutable, _execution) = test_store_create().await.unwrap();
            let server = TestServer::start(Setup {
                mutable: Some(Arc::new(MutableDouble {
                    store: mutable,
                    hangs: true,
                    holds: Arc::default(),
                })),
                handler_timeout_seconds: Some(1),
                ..Default::default()
            })
            .await;
            let repository = random::<Partition>();
            let client = server.client(&token(&[repository])).await;
            let session = client.session_start(repository, "slow").await.unwrap();

            let loaded = timeout(
                PATIENCE,
                client.mutable_load(session, &random::<Hash>(), KeyType::BranchId),
            )
            .await
            .expect("the server answers within its handler timeout");

            assert!(
                matches!(&loaded, Err(error) if error.is_slow_down()),
                "{loaded:?}"
            );
        })
        .await;
}

/// Starts a session for `repository` under `correlation_id`, presenting `token`, on a connection
/// of its own, which the returned stream keeps open. Returns the stream and the session's id.
async fn session_on_its_own_connection(
    server: &TestServer,
    repository: Partition,
    correlation_id: &str,
    token: &str,
) -> (AsyncStream, u32) {
    let (mut stream, hello) = server.hello(&client_hello()).await;
    assert!(hello.is_some());
    let mut start = vec![0u8];
    start.extend_from_slice(repository.as_bytes());
    start.push(correlation_id.len() as u8);
    start.extend_from_slice(correlation_id.as_bytes());
    start.extend_from_slice(&(token.len() as u16).to_le_bytes());
    start.extend_from_slice(token.as_bytes());
    let header = CommandHeader::new_with_session(Command::Authorize as u8, 1, start.len(), 0);
    stream.write_all(&header.to_bytes_v4()).await.unwrap();
    stream.write_all(&start).await.unwrap();
    let mut response = [0u8; 16];
    timeout(PATIENCE, stream.read_exact(&mut response))
        .await
        .expect("the session start is answered")
        .unwrap();
    assert!(!CommandHeader::from_bytes_v4(&response[..12]).error);
    (
        stream,
        u32::from_le_bytes(response[12..].try_into().unwrap()),
    )
}

/// A session holds what the stores keep for its correlation id until it stops or its connection
/// closes, so a correlation id two sessions name stays held until both have ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_holds_its_correlation_id_until_it_stops_or_its_connection_closes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (_, mutable, _execution) = test_store_create().await.unwrap();
            let holds = Arc::new(AtomicUsize::new(0));
            let server = TestServer::start(Setup {
                mutable: Some(Arc::new(MutableDouble {
                    store: mutable,
                    hangs: false,
                    holds: holds.clone(),
                })),
                ..Default::default()
            })
            .await;
            let repository = random::<Partition>();
            let token = token(&[repository]);
            let client = server.client(&token).await;

            let stopped = client
                .session_start(repository, "one-command")
                .await
                .unwrap();
            let (open, _) =
                session_on_its_own_connection(&server, repository, "one-command", &token).await;
            assert_eq!(holds.load(Ordering::SeqCst), 2);

            client.session_stop(stopped).await.unwrap();
            assert_eq!(
                holds.load(Ordering::SeqCst),
                1,
                "the stopped session's hold"
            );

            drop(open);
            let released = timeout(PATIENCE, async {
                while holds.load(Ordering::SeqCst) != 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            assert!(
                released.is_ok(),
                "the closed connection's session keeps its hold"
            );
        })
        .await;
}

/// A session that names no correlation id runs under one the server makes up for it, and holds
/// what the stores keep for that id as a session that names one does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_names_no_correlation_id_holds_the_one_it_is_given() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (_, mutable, _execution) = test_store_create().await.unwrap();
            let holds = Arc::new(AtomicUsize::new(0));
            let server = TestServer::start(Setup {
                mutable: Some(Arc::new(MutableDouble {
                    store: mutable,
                    hangs: false,
                    holds: holds.clone(),
                })),
                ..Default::default()
            })
            .await;
            let repository = random::<Partition>();
            let client = server.client(&token(&[repository])).await;

            let session = client.session_start(repository, "").await.unwrap();
            assert_eq!(holds.load(Ordering::SeqCst), 1, "the session's hold");

            client.session_stop(session).await.unwrap();
            assert_eq!(
                holds.load(Ordering::SeqCst),
                0,
                "the stopped session's hold"
            );
        })
        .await;
}

type Read = futures::future::BoxFuture<'static, Result<StoreGetData, StoreError>>;

/// An immutable store whose reads `read` answers, which counts in `healed` the heals asked of it
/// and answers each as made, and whose every other call reaches `store`.
pub(crate) struct Reads {
    store: Arc<dyn ImmutableStore>,
    read: Box<dyn Fn(Partition, Address) -> Read + Send + Sync>,
    healed: Arc<AtomicUsize>,
}

/// Reads that reach `store` unchanged, with the heals asked counted in `healed`.
pub(crate) fn counted_heals(store: Arc<dyn ImmutableStore>, healed: Arc<AtomicUsize>) -> Reads {
    Reads {
        store: store.clone(),
        read: Box::new(move |partition, address| Box::pin(store.clone().get(partition, address))),
        healed,
    }
}

/// Reads answered with a payload of the largest message body, which leaves no room for the
/// response's fragment.
fn oversized_reads(store: Arc<dyn ImmutableStore>) -> Reads {
    Reads {
        store,
        healed: Arc::default(),
        read: Box::new(|partition, _address| {
            Box::pin(async move {
                Ok(StoreGetData {
                    fragment: Fragment {
                        flags: 0,
                        size_payload: MAX_MESSAGE as u32,
                        size_content: MAX_MESSAGE as u64,
                    },
                    match_made: StoreMatch::MatchFull,
                    partition,
                    payload: Some(Bytes::from(vec![0; MAX_MESSAGE])),
                    content: None,
                })
            })
        }),
    }
}

/// Where reads wait until the test lets them through.
struct ReadGate {
    /// Reads that reached the store.
    reached: AtomicUsize,
    /// Reads the store finished.
    finished: AtomicUsize,
    /// For each read in the order it reached the store, how many had finished by then.
    finished_on_reaching: std::sync::Mutex<Vec<usize>>,
    open: Semaphore,
}

impl ReadGate {
    /// A gate that holds every read until it opens.
    fn shut() -> Self {
        Self {
            reached: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            finished_on_reaching: std::sync::Mutex::default(),
            open: Semaphore::new(0),
        }
    }
}

/// Reads of `store` held at `gate` until it opens.
fn gated_reads(store: Arc<dyn ImmutableStore>, gate: Arc<ReadGate>) -> Reads {
    Reads {
        store: store.clone(),
        healed: Arc::default(),
        read: Box::new(move |partition, address| {
            let (store, gate) = (store.clone(), gate.clone());
            Box::pin(async move {
                gate.finished_on_reaching
                    .lock()
                    .unwrap()
                    .push(gate.finished.load(Ordering::SeqCst));
                gate.reached.fetch_add(1, Ordering::SeqCst);
                let _passed = gate.open.acquire().await.unwrap();
                let read = store.get(partition, address).await;
                gate.finished.fetch_add(1, Ordering::SeqCst);
                read
            })
        }),
    }
}

#[async_trait]
impl ImmutableStore for Reads {
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.store.clone().hold_for_command().await
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        (self.read)(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.store
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.store.clone().get_metadata(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        self.store
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.store
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.store
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.store
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.store.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.store.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.store.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.store.clone().verify(heal).await
    }

    async fn heal(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
    ) -> Result<HealResult, StoreError> {
        self.healed.fetch_add(1, Ordering::SeqCst);
        Ok(HealResult::Healed)
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        self.store
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

/// A container's heal of a payload the server holds no local copy of reaches the store beyond the
/// local tier, whose heal the container learns of; a check without a heal asks nothing of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heal_of_a_payload_held_nowhere_locally_reaches_the_store_beyond() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, _, _execution) = test_store_create().await.unwrap();
            let healed = Arc::new(AtomicUsize::new(0));
            let server = TestServer::start(Setup {
                immutable: Some(Arc::new(counted_heals(immutable, healed.clone()))),
                ..Default::default()
            })
            .await;
            let repository = random::<Partition>();
            let client = server.client(&token(&[repository])).await;
            let session = client.session_start(repository, "heal").await.unwrap();
            let missing = Address {
                hash: Hash::hash_buffer(b"held nowhere locally"),
                ..Default::default()
            };

            let checked = client.verify(session, &missing, false).await;
            assert_eq!(healed.load(Ordering::SeqCst), 0, "a check asks no heal");
            let healing = client.verify(session, &missing, true).await.unwrap();

            assert!(checked.is_err(), "{checked:?}");
            assert_eq!(healed.load(Ordering::SeqCst), 1);
            assert!(healing.corrupted);
            assert_eq!(healing.healed, HealResult::Healed);
        })
        .await;
}

/// A response the socket cannot carry is answered as a failure, which leaves the connection
/// serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_response_larger_than_a_message_answers_a_failure() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, _, _execution) = test_store_create().await.unwrap();
            let server = TestServer::start(Setup {
                immutable: Some(Arc::new(oversized_reads(immutable))),
                ..Default::default()
            })
            .await;
            let repository = random::<Partition>();
            let client = server.client(&token(&[repository])).await;
            let session = client.session_start(repository, "oversized").await.unwrap();
            let branch = random::<Hash>();

            let read = client.get(session, &Address::default()).await;
            let stored = client
                .mutable_store(session, branch, random::<Hash>(), KeyType::BranchId)
                .await;

            assert!(matches!(&read, Err(ProtocolError::Internal(_))), "{read:?}");
            assert!(stored.is_ok(), "the connection still serves: {stored:?}");
        })
        .await;
}

/// A second server cannot take a socket a running server holds: its startup fails and the
/// running server keeps the socket and goes on serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_server_on_a_held_socket_fails_to_start() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;
            let other = TempDir::new("lore-uds-server-");
            let (_shutdown, shutdown_rx) = watch::channel(false);

            let second = timeout(
                PATIENCE,
                serve(
                    other.path().to_path_buf(),
                    vec![server.socket.clone()],
                    Setup::default(),
                    shutdown_rx,
                    None,
                ),
            )
            .await
            .expect("the second server's startup ends");

            assert!(
                matches!(&second, Err(error) if error.downcast_ref::<std::io::Error>().map(std::io::Error::kind) == Some(std::io::ErrorKind::AddrInUse)),
                "{second:?}"
            );
            assert!(file_at(&server.socket));
            let repository = random::<Partition>();
            let client = server.client(&token(&[repository])).await;
            assert!(client.session_start(repository, "first").await.is_ok());
        })
        .await;
}

/// A socket that cannot be bound fails startup, and the sockets bound before it are given up
/// rather than left without a server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_that_cannot_be_bound_fails_startup_and_leaves_no_socket() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = TempDir::new("lore-uds-server-");
            let bound = socket_dir(&dir).join("bound.sock");
            let not_a_directory = dir.path().join("file");
            std::fs::write(&not_a_directory, b"").unwrap();
            let (_shutdown, shutdown_rx) = watch::channel(false);

            let started = timeout(
                PATIENCE,
                serve(
                    dir.path().to_path_buf(),
                    vec![bound.clone(), not_a_directory.join("lore.sock")],
                    Setup::default(),
                    shutdown_rx,
                    None,
                ),
            )
            .await
            .expect("the startup ends");

            assert!(started.is_err());
            assert!(!file_at(&bound), "the socket bound first is removed");
        })
        .await;
}

/// A connection that ends before its hello gives its place back, as one that ends later does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_ends_before_its_hello_frees_its_place() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup {
                max_connections_per_socket: Some(1),
                ..Default::default()
            })
            .await;

            drop(local_socket::connect(&server.socket).await.unwrap());
            let freed = timeout(PATIENCE, async {
                loop {
                    if let (stream, Some(_)) = server.hello(&client_hello()).await {
                        return stream;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;

            assert!(freed.is_ok(), "the place is free again");
        })
        .await;
}

/// A connection that sends nothing, or stops partway through its hello, is closed once the
/// handshake timeout passes, and gives its place back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_never_finishes_its_hello_loses_its_place() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup {
                max_connections_per_socket: Some(2),
                timeouts: Some((SHORT_TIMEOUT, PATIENCE)),
                ..Default::default()
            })
            .await;

            let mut silent = local_socket::connect(&server.socket).await.unwrap();
            let mut partial = local_socket::connect(&server.socket).await.unwrap();
            partial.write_all(&client_hello()[..4]).await.unwrap();

            assert!(
                closes(&mut silent).await,
                "a connection that sends nothing is closed"
            );
            assert!(
                closes(&mut partial).await,
                "a connection that stops in its hello is closed"
            );
            assert!(a_place_frees(&server).await, "their places are free again");
        })
        .await;
}

/// A connection that stops after the bytes that tell the protocols apart, or partway through its
/// HTTP/2 client preface, is closed once the handshake timeout passes, and gives its place back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_never_finishes_its_http2_preface_loses_its_place() {
    const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup {
                max_connections_per_socket: Some(2),
                timeouts: Some((SHORT_TIMEOUT, PATIENCE)),
                ..Default::default()
            })
            .await;

            let mut opened = local_socket::connect(&server.socket).await.unwrap();
            opened.write_all(&PREFACE[..4]).await.unwrap();
            let mut partial = local_socket::connect(&server.socket).await.unwrap();
            partial.write_all(&PREFACE[..20]).await.unwrap();

            assert!(
                closes(&mut opened).await,
                "a connection that stops after the bytes that tell the protocols apart is closed"
            );
            assert!(
                closes(&mut partial).await,
                "a connection that stops in its preface is closed"
            );
            assert!(a_place_frees(&server).await, "their places are free again");
        })
        .await;
}

/// Whether the server closes `stream`, after whatever it sends first.
async fn ends(stream: &mut AsyncStream) -> bool {
    let mut buffer = [0u8; 1024];
    timeout(PATIENCE, async {
        while let Ok(read) = stream.read(&mut buffer).await {
            if read == 0 {
                return;
            }
        }
    })
    .await
    .is_ok()
}

/// At shutdown the server closes its storage and forwarded connections, as a QUIC endpoint closes
/// its own, so that clients see the close and reconnect rather than wait on a connection nothing
/// serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_storage_and_forwarded_connections() {
    const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;
            let (mut storage, hello) = server.hello(&client_hello()).await;
            assert!(hello.is_some(), "the storage connection is served");
            let mut forwarded = local_socket::connect(&server.socket).await.unwrap();
            forwarded.write_all(PREFACE).await.unwrap();
            // The server's HTTP/2 settings frame header, once it serves the connection.
            let mut settings = [0u8; 9];
            timeout(PATIENCE, forwarded.read_exact(&mut settings))
                .await
                .expect("the server answers the preface")
                .expect("the forwarded connection is served");

            drop(server);

            assert!(ends(&mut storage).await, "the storage connection is closed");
            assert!(
                ends(&mut forwarded).await,
                "the forwarded connection is closed"
            );
        })
        .await;
}

/// A peer that reads none of its responses is dropped once the socket has taken nothing for the
/// stall timeout, and gives its place back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_reads_no_responses_is_dropped_after_the_stall_timeout() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup {
                // The storage client's connection and the stalled one.
                max_connections_per_socket: Some(2),
                timeouts: Some((PATIENCE, SHORT_TIMEOUT)),
                ..Default::default()
            })
            .await;
            let repository = random::<Partition>();
            let token = token(&[repository]);
            let client = server.client(&token).await;
            let session = client.session_start(repository, "stall").await.unwrap();
            let payload = Bytes::from(vec![7u8; 256 * 1024]);
            let (address, fragment) = payload_address(&payload);
            client
                .put(session, address, fragment, Some(payload))
                .await
                .unwrap();

            let (mut stalled, stalled_session) =
                session_on_its_own_connection(&server, repository, "stall", &token).await;
            // 32 MiB of responses, far more than a socket buffers.
            for command_id in 2..130 {
                let get = CommandHeader::new_with_session(
                    Command::Get as u8,
                    command_id,
                    size_of::<Address>(),
                    stalled_session,
                );
                stalled.write_all(&get.to_bytes_v4()).await.unwrap();
                stalled.write_all(address.as_bytes()).await.unwrap();
            }

            assert!(
                a_place_frees(&server).await,
                "the stalled connection gives its place back"
            );
            assert!(
                drains_to_end(&mut stalled).await,
                "the stalled connection is closed"
            );
        })
        .await;
}

/// A request the server cannot read is answered as an invalid command, and the connection goes on
/// reading the requests after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_the_server_cannot_read_is_refused_and_the_connection_kept() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let server = TestServer::start(Setup::default()).await;
            let (mut stream, hello) = server.hello(&client_hello()).await;
            assert!(hello.is_some());

            // An opcode no command has, then an authorize naming no action it knows.
            let unknown = CommandHeader::new_with_session(0x30, 1, 0, 0);
            let short = CommandHeader::new_with_session(Command::Authorize as u8, 2, 3, 0);
            stream.write_all(&unknown.to_bytes_v4()).await.unwrap();
            stream.write_all(&short.to_bytes_v4()).await.unwrap();
            stream.write_all(b"abc").await.unwrap();
            let mut responses = FrameReader::new(stream, MAX_MESSAGE, true);
            let mut answered = Vec::new();
            for _ in 0..2 {
                let (header, _) = timeout(PATIENCE, responses.next())
                    .await
                    .expect("the server answers")
                    .unwrap()
                    .expect("a response");
                answered.push((header.command_id, header.error, header.size_or_status));
            }

            let invalid = QuicServiceError::InvalidCommand as u32;
            assert_eq!(answered, [(1, true, invalid), (2, true, invalid)]);
        })
        .await;
}

/// A connection takes no more reads than its byte credit has room for, each holding the largest
/// response while in handling, and takes the rest as reads finish: with the store holding the
/// first two, the third reaches it only once one of them has finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_past_the_byte_credit_wait_for_room() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, _, _execution) = test_store_create().await.unwrap();
            let gate = Arc::new(ReadGate::shut());
            let server = TestServer::start(Setup {
                immutable: Some(Arc::new(gated_reads(immutable, gate.clone()))),
                // The floor, which has room for two reads.
                credit_bytes: Some(0),
                ..Default::default()
            })
            .await;
            let repository = random::<Partition>();
            let client = server.client(&token(&[repository])).await;
            let session = client.session_start(repository, "credit").await.unwrap();
            let payload = Bytes::from_static(b"read three times at once");
            let (address, fragment) = payload_address(&payload);
            client
                .put(session, address, fragment, Some(payload.clone()))
                .await
                .unwrap();

            let reads = futures::future::join3(
                client.get(session, &address),
                client.get(session, &address),
                client.get(session, &address),
            );
            let mut reads = std::pin::pin!(reads);
            let two_reached = timeout(PATIENCE, async {
                while gate.reached.load(Ordering::SeqCst) < 2 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            tokio::select! {
                _ = &mut reads => panic!("the reads finished with the gate shut"),
                reached = two_reached => reached.expect("two reads reach the store"),
            };
            gate.open.add_permits(3);
            let (first, second, third) = timeout(PATIENCE, reads)
                .await
                .expect("every read is answered");

            let finished_on_reaching = gate.finished_on_reaching.lock().unwrap().clone();
            assert_eq!(finished_on_reaching.len(), 3);
            assert!(
                finished_on_reaching[2] >= 1,
                "the third read waits for room: {finished_on_reaching:?}"
            );
            for read in [first, second, third] {
                assert_eq!(read.unwrap().1, payload);
            }
        })
        .await;
}

/// A forwarded host that cannot be reached answers the caller as unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_to_a_forwarded_host_that_is_down_is_unavailable() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let target = format!("http://{}", closed.local_addr().unwrap());
            drop(closed);
            let server = TestServer::start(Setup {
                forward: vec![("down.test", target)],
                ..Default::default()
            })
            .await;

            let answered = grpc_call(&server.socket, "down.test", "the-token").await;

            assert_eq!(answered["grpc-status"], "14", "UNAVAILABLE");
        })
        .await;
}
