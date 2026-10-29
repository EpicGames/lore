// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::fragment::generate_random;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::replication_store::get::Get;
use lore_server::protocol::replication_store::header::ReplicationHeader;
use lore_server::quic::ProtocolErrorInfo;
use lore_server::quic::QuicService;
use lore_server::quic::StreamDataHandler;
use lore_server::quic::StreamHandlerFactory;
use lore_server::quic::quinn::QuinnConfigBuilder;
use lore_server::quic::quinn::QuinnServer;
use lore_server::quic::quinn::SocketConnectionIdGenerator;
use lore_server::quic::quinn::build_cert_verifier;
use lore_server::quic::quinn::service_store::ServiceStore;
use lore_server::quic::quinn::service_store::StreamDataHandlerBuilder;
use lore_server::quic::replication_store_service::ReplicationServiceErrorCode;
use lore_server::quic::replication_store_service::client::CommandBehavior;
use lore_server::quic::replication_store_service::client::ReplicationStoreClient;
use lore_server::quic::replication_store_service::client::ReplicationStoreClientError;
use lore_server::quic::replication_store_service::client::StoreClient;
use lore_server::quic::stream_handler::*;
use lore_server::quic::tests::TEST_PROTOCOL;
use lore_server::quic::tests::TEST_PROTOCOL_V4;
use lore_server::quic::tests::TestHandlerFactory;
use lore_server::quic::tests::server_certs;
use lore_server::quic::tests::test_data_path;
use lore_transport::quic::QuicErrorStatus;
use lore_transport::quic::QuicOpCode;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::client::CertificateSettings;
use lore_transport::quic::client::ClientCerts;
use lore_transport::quic::client::CongestionAlgorithm;
use lore_transport::quic::client::DEFAULT_EXPECTED_RTT_MS;
use lore_transport::quic::client::STREAM_COUNT;
use lore_transport::quic::client::TransportConfig;
use lore_transport::quic::client::insecure_client_auth;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use quinn::ClientConfig;
use quinn::ConnectionError;
use quinn::Endpoint;
use quinn::ReadError;
use quinn::ReadExactError;
use quinn::RecvStream;
use quinn::SendStream;
use quinn::crypto::rustls::QuicClientConfig;
use quinn_proto::ConnectionIdGenerator;
use quinn_proto::HashedConnectionIdGenerator;
use rand::random;
use tokio::io::AsyncWriteExt;
use zerocopy::IntoBytes;

use crate::store::test_support::test_store_create;

fn untrusted_client_cert_paths() -> anyhow::Result<(PathBuf, PathBuf)> {
    let path = test_data_path();
    let cert = path.join("untrusted_cert.pem");
    let key = path.join("untrusted_key.pem");
    Ok((cert, key))
}

fn trusted_client_cert_paths() -> anyhow::Result<(PathBuf, PathBuf)> {
    let path = test_data_path();
    let cert = path.join("test_client_cert.pem");
    let key = path.join("test_client_key.pem");
    Ok((cert, key))
}

/// A running server and an open client stream to it.
///
/// Quinn's send and receive streams cannot be mocked, so exercising the stream handler needs
/// a real server. The server, endpoint and connection are held because dropping any of them
/// closes the stream.
struct Harness {
    send: SendStream,
    recv: RecvStream,
    connection: quinn::Connection,
    _server: QuinnServer,
    _endpoint: Endpoint,
}

impl Harness {
    /// Open an additional stream on the same connection.
    async fn open_stream(&self) -> (SendStream, RecvStream) {
        self.connection
            .open_bi()
            .await
            .expect("Failed to open additional stream")
    }
}

/// Send a request asking the handler for `behaviour`.
async fn request(send: &mut SendStream, behaviour: MockBehaviour, command_id: u32) {
    send.write(&CommandHeader::new(behaviour.opcode(), command_id, 0).to_bytes())
        .await
        .expect("Failed to write header");
    send.flush().await.expect("Failed flush");
}

/// Read one response header.
async fn response(recv: &mut RecvStream) -> CommandHeader {
    let mut buffer = [0u8; 8];
    recv.read_exact(&mut buffer)
        .await
        .expect("Failed to read response");
    CommandHeader::from_bytes(&buffer)
}

/// Serve `factory` for `protocol` on a loopback endpoint and open a client stream to it.
///
/// The client skips certificate verification and presents none of its own; the mTLS tests
/// build their endpoints by hand because varying exactly that is what they test.
async fn serve_and_connect(
    factory: Box<dyn StreamHandlerFactory>,
    protocol: &'static str,
) -> Harness {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = socket.local_addr().expect("Failed socket setup");
    drop(socket);

    let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");
    let server = QuinnServer::start(
        QuinnConfigBuilder::new()
            .address(server_addr)
            .cert_file(cert_path)
            .pkey_file(key_path)
            .stream_handler_factory(factory)
            .build()
            .unwrap(),
    )
    .expect("Failed Quinn server start");

    let mut crypto_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(insecure_client_auth::SkipServerVerification::new())
        .with_no_client_auth();
    crypto_config.alpn_protocols = vec![protocol.as_bytes().into()];

    let client_config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
    ));

    let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let mut endpoint = Endpoint::client(client_addr).expect("Failed to create client endpoint");
    endpoint.set_default_client_config(client_config);

    let connection = endpoint
        .connect(server_addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (send, recv) = connection
        .open_bi()
        .await
        .expect("Failed to setup bidirectional channel");

    Harness {
        send,
        recv,
        connection,
        _server: server,
        _endpoint: endpoint,
    }
}

const MOCK_PROTOCOL: &str = "mock-test/0.1";
const MOCK_MAX_CHUNK: usize = 4096;

/// How long [`MockBehaviour::Block`] holds a permit: longer than any test runs, and unrelated
/// to the timeouts under test so changing one does not move the other.
const BLOCKING_HANDLER_SLEEP: Duration = Duration::from_secs(3600);

/// Time a refusal that involves no waiting is allowed to take, with room for a loaded host.
const SHED_BUDGET: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
#[error("mock error")]
struct MockError;

/// What a request asks the handler to do, carried by the command opcode so that one service
/// instance can behave differently on different streams of a connection.
#[derive(Copy, Clone, Debug)]
enum MockBehaviour {
    /// Hold the permit for [`BLOCKING_HANDLER_SLEEP`].
    Block,
    /// Answer immediately.
    Echo,
    /// Answer with more bytes than [`MOCK_MAX_CHUNK`] allows.
    Oversized,
}

impl MockBehaviour {
    fn opcode(self) -> QuicOpCode {
        match self {
            MockBehaviour::Block => 1,
            MockBehaviour::Echo => 2,
            MockBehaviour::Oversized => 3,
        }
    }

    fn from_opcode(opcode: QuicOpCode) -> Option<Self> {
        [
            MockBehaviour::Block,
            MockBehaviour::Echo,
            MockBehaviour::Oversized,
        ]
        .into_iter()
        .find(|behaviour| behaviour.opcode() == opcode)
    }
}

struct MockService;

#[async_trait]
impl QuicService for MockService {
    type ParsedRequestType = MockBehaviour;
    type RequestParseErrorType = MockError;
    type RequestHandlerError = MockError;

    fn get_service_name_label(&self) -> &'static str {
        "mock_test"
    }

    fn parse_request_bytes(
        &self,
        header: &CommandHeader,
        _bytes: Bytes,
    ) -> Result<MockBehaviour, MockError> {
        MockBehaviour::from_opcode(header.cmd).ok_or(MockError)
    }

    async fn run_request_handler(
        &self,
        _context: Arc<AttributeMap>,
        request: MockBehaviour,
    ) -> Result<Vec<Bytes>, MockError> {
        match request {
            MockBehaviour::Block => {
                tokio::time::sleep(BLOCKING_HANDLER_SLEEP).await;
                Ok(vec![Bytes::new()])
            }
            MockBehaviour::Echo => Ok(vec![Bytes::new()]),
            MockBehaviour::Oversized => Ok(vec![Bytes::from(vec![0xAB; MOCK_MAX_CHUNK + 1])]),
        }
    }

    fn command_to_metrics_label(&self, _opcode: QuicOpCode) -> &'static str {
        "test_cmd"
    }

    fn transform_protocol_error(&self, _error: &MockError) -> ProtocolErrorInfo {
        ProtocolErrorInfo {
            response_error_code: QuicServiceError::Failed as QuicErrorStatus,
            message_handle_label: "mock_error",
            is_internal_error: true,
            is_appropriate_for_logging: true,
        }
    }

    fn max_chunk_size(&self) -> usize {
        MOCK_MAX_CHUNK
    }

    fn build_request_span(
        &self,
        header: &CommandHeader,
        _message: &MockBehaviour,
        _context: &Arc<AttributeMap>,
    ) -> tracing::Span {
        lore_server::quic::storage_service::build_storage_protocol_request_span(
            header.cmd,
            lore_server::telemetry::StorageProtocol::StorageV0,
            lore_server::quic::NO_CONNECTION_ID,
            lore_server::quic::NO_REPOSITORY_ID,
            lore_server::quic::NO_CORRELATION_ID,
            lore_server::quic::NO_USER_ID,
            lore_server::quic::NO_USER_AGENT,
        )
    }
}

/// Limits generous enough not to interfere, with the ceiling derived as the server does.
fn test_limits() -> AdmissionLimits {
    let process_limit = 100;
    AdmissionLimits {
        process_limit,
        inflight_limit: process_limit * STREAM_COUNT as usize,
        handler_timeout: None,
        permit_timeout: None,
    }
}

/// Factory serving one service under one protocol with explicit limits.
struct SingleServiceFactory {
    service_store: ServiceStore,
}

impl SingleServiceFactory {
    fn new<ServiceType: QuicService + 'static>(
        protocol: &'static str,
        make_service: fn() -> ServiceType,
        limits: AdmissionLimits,
    ) -> Self {
        let mut service_store = ServiceStore::default();
        service_store.add_service(
            protocol,
            Box::new(move |context: Arc<AttributeMap>| {
                Box::new(StreamHandler::new(
                    Arc::new(make_service()),
                    context,
                    limits,
                )) as Box<dyn StreamDataHandler>
            }) as StreamDataHandlerBuilder,
        );
        Self { service_store }
    }
}

impl StreamHandlerFactory for SingleServiceFactory {
    fn supported_protocols(&self) -> Vec<String> {
        self.service_store.get_supported_services()
    }

    fn get_stream_handler_builder(
        &self,
        protocol: &str,
    ) -> Option<(&&'static str, &StreamDataHandlerBuilder)> {
        self.service_store.get_stream_builder(protocol)
    }
}

#[tokio::test]
async fn test_command() {
    let repository = random::<Context>();

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(TestHandlerFactory::new(
                immutable_store,
                mutable_store.clone(),
            )),
            TEST_PROTOCOL,
        )
        .await;

        let token = "some-token";
        let token_bytes = token.as_bytes();

        let header = CommandHeader::new(
            Command::Authorize as QuicOpCode,
            random::<u32>(),
            size_of::<Context>() + token_bytes.len(),
        );
        let header_bytes = header.to_bytes();

        // Split across two writes, so the server has to buffer a partial header.
        harness
            .send
            .write(&header_bytes[..4])
            .await
            .expect("Failed to write header");
        harness.send.flush().await.expect("Failed flush");
        tokio::time::sleep(Duration::from_millis(1)).await;

        let mut data = bytes::BytesMut::new();
        data.extend_from_slice(&header_bytes[4..]);
        data.extend_from_slice(repository.as_bytes());
        data.extend_from_slice(token_bytes);

        harness
            .send
            .write(data.to_vec().as_slice())
            .await
            .expect("Failed to write data");
        harness.send.flush().await.expect("Failed flush");

        assert_eq!(
            header.response_success(0),
            response(&mut harness.recv).await
        );

        harness.send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn server_with_mtls_rejects_clients_without_certs() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
        // order to test the stream handler we need to spin up an actual server instance.

        // Find an available port.
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().expect("Failed socket setup");
        drop(socket);

        let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

        let (cert_path, key_path, ca_cert) = server_certs().expect("Bad cert paths");

        let _server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .cert_chain(Some(ca_cert.clone()))
                .client_cert_verifier(
                    build_cert_verifier(ca_cert).expect("Failed client cert verifier"),
                )
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    immutable_store,
                    mutable_store.clone(),
                )))
                .build()
                .unwrap(),
        )
        .expect("Failed Quinn server start");

        let mut crypto_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(insecure_client_auth::SkipServerVerification::new())
            // no certs provided - we should be rejected
            .with_no_client_auth();

        crypto_config.alpn_protocols = [TEST_PROTOCOL]
            .iter()
            .map(|alpn| alpn.as_bytes().into())
            .collect();

        let client_config = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
        ));

        let mut endpoint = Endpoint::client(client_addr).expect("Failed to create client endpoint");
        endpoint.set_default_client_config(client_config);

        // it is expected the client can 'connect', as under the hood
        // the server client TLS handshake is still occurring
        let connection = endpoint
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .expect("Failed to setup bidirectional channel");

        // but when we try to do something with the connection (like receive data)
        // eventually the TLS handshake will have finished and reject our connection
        let mut response_buffer = [0u8; 8];
        let error = recv
            .read_exact(&mut response_buffer)
            .await
            .expect_err("receive should have failed");
        let ReadExactError::ReadError(read_error) = error else {
            panic!("Unexpected error type {error:?}");
        };
        let ReadError::ConnectionLost(connection_lost) = read_error else {
            panic!("Unexpected read error {read_error:?}");
        };
        let ConnectionError::ConnectionClosed(closed_error) = connection_lost else {
            panic!("Unexpected connection lost error {connection_lost:?}");
        };
        assert_eq!(closed_error.reason, "peer sent no certificates");

        // Close the client side of the stream.
        send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn server_with_mtls_accepts_clients_with_certs() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
        // order to test the stream handler we need to spin up an actual server instance.

        // Find an available port.
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().expect("Failed socket setup");
        drop(socket);

        let (cert_path, key_path, ca_cert) = server_certs().expect("Bad cert paths");
        let (client_cert, client_key) = trusted_client_cert_paths().expect("Bad client cert paths");

        let _server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .cert_chain(Some(ca_cert.clone()))
                .client_cert_verifier(
                    build_cert_verifier(ca_cert.clone()).expect("Failed client cert verifier"),
                )
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    immutable_store,
                    mutable_store.clone(),
                )))
                .build()
                .unwrap(),
        )
        .expect("Failed Quinn server start");

        // `s` suffix so the server's certificate is validated
        let remote_url = format!("quics://{server_addr}");
        let client = ReplicationStoreClient::connect(
            &remote_url,
            CertificateSettings {
                custom_ca: Some(ca_cert),
                client: Some(ClientCerts {
                    cert_file: client_cert,
                    pkey_file: client_key,
                }),
            },
            None,
            TransportConfig {
                max_bytes_bandwidth_per_second: 1_000_000,
                expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                congestion_algorithm: CongestionAlgorithm::Bbr,
                initial_cwnd: None,
            },
            CommandBehavior {
                message_limit: 10,
                should_await_command_permit: false,
            },
            None,
            None,
        )
        .await
        .expect("Failed to establish client connection");

        // requests should be handled gracefully
        let (_, address, _) = generate_random();
        let client_error = client
            .get(Get {
                header: ReplicationHeader {
                    correlation_id: Default::default(),
                    repository: random(),
                },
                address,
            })
            .await
            .expect_err("Failed to get request");
        assert!(matches!(
            client_error,
            ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::AddressNotFound)
        ));
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn server_with_mtls_rejects_clients_with_invalid_certs() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
        // order to test the stream handler we need to spin up an actual server instance.

        // Find an available port.
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().expect("Failed socket setup");
        drop(socket);

        let (cert_path, key_path, ca_cert) = server_certs().expect("Bad cert paths");
        let (untrusted_cert, untrusted_key) =
            untrusted_client_cert_paths().expect("Bad untrusted cert paths");

        let _server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path.clone())
                .pkey_file(key_path.clone())
                .cert_chain(Some(ca_cert.clone()))
                .client_cert_verifier(
                    build_cert_verifier(ca_cert.clone()).expect("Failed client cert verifier"),
                )
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    immutable_store,
                    mutable_store.clone(),
                )))
                .build()
                .unwrap(),
        )
        .expect("Failed Quinn server start");

        let remote_url = format!("quic://{server_addr}");
        let client = ReplicationStoreClient::connect(
            &remote_url,
            CertificateSettings {
                custom_ca: None,
                client: Some(ClientCerts {
                    cert_file: untrusted_cert,
                    pkey_file: untrusted_key,
                }),
            },
            None,
            TransportConfig {
                max_bytes_bandwidth_per_second: 1_000_000,
                expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                congestion_algorithm: CongestionAlgorithm::Bbr,
                initial_cwnd: None,
            },
            CommandBehavior {
                message_limit: 10,
                should_await_command_permit: false,
            },
            None,
            None,
        )
        .await;

        // The client finishes its side of the handshake once it has validated the
        // server; the server's refusal of our untrusted certificate arrives after
        // that as a connection close. Whether that close lands before `connect`
        // returns is a race, so accept a refusal at either point — both show the
        // certificate was rejected.
        let Ok(client) = client else {
            return;
        };

        let (_, address, _) = generate_random();
        let client_error = client
            .get(Get {
                header: ReplicationHeader {
                    correlation_id: Default::default(),
                    repository: random(),
                },
                address,
            })
            .await
            .expect_err("request over a rejected connection must fail");
        assert!(matches!(
            client_error,
            ReplicationStoreClientError::ConnectionFailed
        ));
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn server_without_mtls_accepts_clients_without_certs() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
        // order to test the stream handler we need to spin up an actual server instance.

        // Find an available port.
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().expect("Failed socket setup");
        drop(socket);

        let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

        let _server = QuinnServer::start(
            // no client_cert_verifier which defaults to NoClientAuth verifier
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path.clone())
                .pkey_file(key_path.clone())
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    immutable_store,
                    mutable_store.clone(),
                )))
                .build()
                .unwrap(),
        )
        .expect("Failed Quinn server start");

        let remote_url = format!("quic://{server_addr}");
        let client = ReplicationStoreClient::connect(
            &remote_url,
            CertificateSettings {
                custom_ca: None,
                client: None,
            },
            None,
            TransportConfig {
                max_bytes_bandwidth_per_second: 1_000_000,
                expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                congestion_algorithm: CongestionAlgorithm::Bbr,
                initial_cwnd: None,
            },
            CommandBehavior {
                message_limit: 10,
                should_await_command_permit: false,
            },
            None,
            None,
        )
        .await
        .expect("Failed to establish client connection");

        // requests should be handled gracefully
        let (_, address, _) = generate_random();
        let client_error = client
            .get(Get {
                header: ReplicationHeader {
                    correlation_id: Default::default(),
                    repository: random(),
                },
                address,
            })
            .await
            .expect_err("Failed to get request");
        assert!(matches!(
            client_error,
            ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::AddressNotFound)
        ));
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn unsupported_protocol_rejects_client() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
        // order to test the stream handler we need to spin up an actual server instance.

        // Find an available port.
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().expect("Failed socket setup");
        drop(socket);

        let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

        let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

        let _server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    immutable_store,
                    mutable_store.clone(),
                )))
                .build()
                .unwrap(),
        )
        .expect("Failed Quinn server start");

        let mut crypto_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(insecure_client_auth::SkipServerVerification::new())
            .with_no_client_auth();

        crypto_config.alpn_protocols = ["no-test/0.2"]
            .iter()
            .map(|alpn| alpn.as_bytes().into())
            .collect();

        let client_config = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
        ));

        let mut endpoint = Endpoint::client(client_addr).expect("Failed to create client endpoint");
        endpoint.set_default_client_config(client_config);

        let connection_error = endpoint
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap_err();
        let ConnectionError::ConnectionClosed(frame) = connection_error else {
            panic!("Unexpected error type {connection_error:?}");
        };
        assert_eq!(frame.reason, "peer doesn't support any known protocol");
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn handler_returns_error_when_response_exceeds_max_chunk_size() {
    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                test_limits(),
            )),
            MOCK_PROTOCOL,
        )
        .await;

        request(&mut harness.send, MockBehaviour::Oversized, random::<u32>()).await;

        let response = response(&mut harness.recv).await;
        assert!(
            response.error,
            "Expected error response for oversized message, got: {response:?}"
        );
        assert_eq!(
            response.size_or_status,
            QuicServiceError::Failed as u32,
            "Expected Failed error status for oversized response"
        );

        harness.send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

/// Permit exhaustion is answered on the permit timeout rather than the handler timeout.
#[tokio::test]
async fn permit_exhaustion_is_answered_before_the_handler_timeout() {
    const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);
    const PERMIT_TIMEOUT: Duration = Duration::from_millis(200);

    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                AdmissionLimits {
                    process_limit: 1,
                    handler_timeout: Some(HANDLER_TIMEOUT),
                    permit_timeout: Some(PERMIT_TIMEOUT),
                    ..test_limits()
                },
            )),
            MOCK_PROTOCOL,
        )
        .await;

        for command_id in [1, 2] {
            request(&mut harness.send, MockBehaviour::Block, command_id).await;
        }

        let started = Instant::now();
        let response = response(&mut harness.recv).await;
        let elapsed = started.elapsed();
        assert_eq!(
            response.command_id, 2,
            "expected the queued request to be answered first, got: {response:?}"
        );
        assert!(
            response.error,
            "expected an error response, got: {response:?}"
        );
        assert_eq!(
            response.size_or_status,
            QuicServiceError::SlowDown as u32,
            "expected SlowDown when no permit is available"
        );
        assert!(
            (PERMIT_TIMEOUT..PERMIT_TIMEOUT * 5).contains(&elapsed),
            "expected a wait of about {PERMIT_TIMEOUT:?}, took {elapsed:?}"
        );

        harness.send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

/// The connection ceiling refuses outright, capping how many requests can be parked holding
/// a parsed payload -- which the per-stream permit wait alone does not.
#[tokio::test]
async fn connection_inflight_ceiling_sheds_without_waiting() {
    const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);
    const PERMIT_TIMEOUT: Duration = Duration::from_secs(10);

    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                AdmissionLimits {
                    process_limit: 64,
                    inflight_limit: 1,
                    handler_timeout: Some(HANDLER_TIMEOUT),
                    permit_timeout: Some(PERMIT_TIMEOUT),
                },
            )),
            MOCK_PROTOCOL,
        )
        .await;

        for command_id in [1, 2] {
            request(&mut harness.send, MockBehaviour::Block, command_id).await;
        }

        let started = Instant::now();
        let response = response(&mut harness.recv).await;
        let elapsed = started.elapsed();
        assert_eq!(response.command_id, 2, "got: {response:?}");
        assert!(
            response.error,
            "expected an error response, got: {response:?}"
        );
        assert_eq!(
            response.size_or_status,
            QuicServiceError::SlowDown as u32,
            "expected SlowDown at the connection ceiling"
        );
        assert!(
            elapsed < SHED_BUDGET,
            "the ceiling must refuse outright, took {elapsed:?}"
        );

        harness.send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

/// A request cannot report an error, so a header carrying the error bit tears the stream down
/// rather than being served.
#[tokio::test]
async fn a_request_header_carrying_the_error_bit_closes_the_stream() {
    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                test_limits(),
            )),
            MOCK_PROTOCOL,
        )
        .await;

        let header = CommandHeader::new(MockBehaviour::Echo as QuicOpCode, 1, 0)
            .response_error(QuicServiceError::Failed as u32);
        harness
            .send
            .write(&header.to_bytes())
            .await
            .expect("Failed to write header");
        harness.send.flush().await.expect("Failed flush");

        let outcome = harness.recv.read_chunk(usize::MAX, false).await;
        assert!(
            outcome.is_err() || matches!(outcome, Ok(None)),
            "expected the stream to be closed, got: {outcome:?}"
        );
    }))
    .await
    .expect("Test task failed");
}

/// A request the service cannot parse is answered `InvalidCommand` and recorded, rather than
/// answered without reaching the operation latency metric at all.
#[tokio::test]
async fn unparseable_request_is_rejected() {
    const UNKNOWN_OPCODE: QuicOpCode = 99;

    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                test_limits(),
            )),
            MOCK_PROTOCOL,
        )
        .await;

        harness
            .send
            .write(&CommandHeader::new(UNKNOWN_OPCODE, 1, 0).to_bytes())
            .await
            .expect("Failed to write header");
        harness.send.flush().await.expect("Failed flush");

        let response = response(&mut harness.recv).await;
        assert_eq!(response.command_id, 1, "got: {response:?}");
        assert!(
            response.error,
            "expected an error response, got: {response:?}"
        );
        assert_eq!(
            response.size_or_status,
            QuicServiceError::InvalidCommand as u32,
            "expected InvalidCommand for a request the service cannot parse"
        );

        harness.send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

/// Both the stream permit and the connection admission count come back down when a request
/// completes, so capacity does not degrade as requests are served.
///
/// The limits leave slack of one because the spawned task answers before releasing, so a
/// client can see its response a moment ahead of the release.
#[tokio::test]
async fn permits_are_released_when_a_request_completes() {
    const REQUESTS: u32 = 20;

    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                AdmissionLimits {
                    process_limit: 2,
                    inflight_limit: 2,
                    ..test_limits()
                },
            )),
            MOCK_PROTOCOL,
        )
        .await;

        for command_id in 1..=REQUESTS {
            request(&mut harness.send, MockBehaviour::Echo, command_id).await;
            let served = response(&mut harness.recv).await;
            assert_eq!(served.command_id, command_id, "got: {served:?}");
            assert!(
                !served.error,
                "request {command_id} was refused, so a permit from an earlier request was \
                     never released: {served:?}"
            );
        }

        harness.send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

/// Each stream has its own permit pool, so saturating one does not refuse work on another
/// stream of the same connection.
#[tokio::test]
async fn stream_permit_pools_are_independent() {
    const PERMIT_TIMEOUT: Duration = Duration::from_millis(200);

    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let mut harness = serve_and_connect(
            Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                AdmissionLimits {
                    process_limit: 1,
                    inflight_limit: 8,
                    handler_timeout: Some(Duration::from_secs(30)),
                    permit_timeout: Some(PERMIT_TIMEOUT),
                },
            )),
            MOCK_PROTOCOL,
        )
        .await;
        let (mut send_b, mut recv_b) = harness.open_stream().await;

        // Occupy the first stream's only permit, then have it refuse a second request.
        for command_id in [1, 2] {
            request(&mut harness.send, MockBehaviour::Block, command_id).await;
        }
        let refused = response(&mut harness.recv).await;
        assert_eq!(refused.command_id, 2, "got: {refused:?}");
        assert_eq!(
            refused.size_or_status,
            QuicServiceError::SlowDown as u32,
            "the saturated stream should refuse its second request"
        );

        request(&mut send_b, MockBehaviour::Echo, 3).await;
        let served = response(&mut recv_b).await;
        assert_eq!(served.command_id, 3, "got: {served:?}");
        assert!(
            !served.error,
            "a second stream was refused while the first was saturated, so the permit pool \
                 is shared across the connection rather than per stream: {served:?}"
        );

        harness.send.finish().expect("Failed to finish stream");
        send_b.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn test_v4_authorize_put_get_query_stop() {
    use lore_base::types::Fragment;
    use lore_transport::quic::command_header::COMMAND_HEADER_SIZE_V4;

    let repository = random::<Context>();

    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");

    let (fragment, address, payload) = generate_random();
    let (_, other_address, _) = generate_random();

    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().expect("Failed socket setup");
        drop(socket);

        let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

        let _server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    immutable_store,
                    mutable_store,
                )))
                .build()
                .unwrap(),
        )
        .expect("Failed Quinn server start");

        let mut crypto_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(insecure_client_auth::SkipServerVerification::new())
            .with_no_client_auth();

        crypto_config.alpn_protocols = [TEST_PROTOCOL_V4]
            .iter()
            .map(|alpn| alpn.as_bytes().into())
            .collect();

        let client_config = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
        ));

        let mut endpoint = Endpoint::client(client_addr).expect("Failed to create client endpoint");
        endpoint.set_default_client_config(client_config);

        let connection = endpoint
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();

        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .expect("Failed to setup bidirectional channel");

        let mut cmd_id: u32 = 0;
        let mut next_cmd_id = || {
            cmd_id += 1;
            cmd_id
        };

        // Helper: send a v4 command and read the response header
        async fn send_v4_cmd(
            send: &mut quinn::SendStream,
            recv: &mut quinn::RecvStream,
            header: CommandHeader,
            payload: &[u8],
        ) -> CommandHeader {
            send.write(&header.to_bytes_v4())
                .await
                .expect("write header");
            send.write(payload).await.expect("write payload");
            send.flush().await.expect("flush");

            let mut buf = [0u8; COMMAND_HEADER_SIZE_V4];
            recv.read_exact(&mut buf).await.expect("read response");
            CommandHeader::from_bytes_v4(&buf)
        }

        // Helper: read response payload
        async fn read_payload(recv: &mut quinn::RecvStream, len: usize) -> Vec<u8> {
            let mut buf = vec![0u8; len];
            recv.read_exact(&mut buf).await.expect("read payload");
            buf
        }

        // === Authorize Start ===
        let corr_id = b"test-corr-id";
        let mut auth_payload = Vec::new();
        auth_payload.push(0u8); // action = start
        auth_payload.extend_from_slice(repository.as_bytes());
        auth_payload.push(corr_id.len() as u8);
        auth_payload.extend_from_slice(corr_id);
        auth_payload.extend_from_slice(&0u16.to_le_bytes()); // no token

        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Authorize as QuicOpCode,
                id,
                auth_payload.len(),
                0,
            ),
            &auth_payload,
        )
        .await;

        assert!(!resp.error, "Authorize start failed: {resp:?}");
        assert_eq!(resp.size_or_status, 4);
        let session_id_bytes = read_payload(&mut recv, 4).await;
        let session_id = u32::from_le_bytes(session_id_bytes.try_into().unwrap());
        assert!(session_id >= 1);

        // === Put a fragment via the protocol ===
        let mut put_payload = Vec::new();
        put_payload.extend_from_slice(address.as_bytes());
        put_payload.extend_from_slice(fragment.as_bytes());
        put_payload.extend_from_slice(&payload);

        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Put as QuicOpCode,
                id,
                put_payload.len(),
                session_id,
            ),
            &put_payload,
        )
        .await;

        assert!(!resp.error, "Put failed: {resp:?}");
        assert_eq!(resp.command_id, id);
        assert_eq!(resp.session_id, session_id);
        assert_eq!(resp.size_or_status, 0); // empty response

        // === Get the fragment we just put ===
        let get_payload = address.as_bytes().to_vec();
        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Get as QuicOpCode,
                id,
                get_payload.len(),
                session_id,
            ),
            &get_payload,
        )
        .await;

        assert!(!resp.error, "Get failed: {resp:?}");
        assert_eq!(resp.command_id, id);
        assert_eq!(resp.session_id, session_id);
        assert!(resp.size_or_status > 0);

        let get_data = read_payload(&mut recv, resp.size_or_status as usize).await;
        // Response is Fragment + payload bytes
        assert!(get_data.len() >= size_of::<Fragment>());
        let returned_payload = &get_data[size_of::<Fragment>()..];
        assert_eq!(returned_payload, payload.as_ref());

        // === Get a non-existent address should fail ===
        let other_get_payload = other_address.as_bytes().to_vec();
        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Get as QuicOpCode,
                id,
                other_get_payload.len(),
                session_id,
            ),
            &other_get_payload,
        )
        .await;

        assert!(resp.error, "Get non-existent should fail");
        assert_eq!(resp.command_id, id);
        // NotFound = 4
        assert_eq!(resp.size_or_status, 4);

        // === Query: one existing, one non-existent ===
        let mut query_payload = Vec::new();
        query_payload.extend_from_slice(address.as_bytes());
        query_payload.extend_from_slice(other_address.as_bytes());

        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Query as QuicOpCode,
                id,
                query_payload.len(),
                session_id,
            ),
            &query_payload,
        )
        .await;

        assert!(!resp.error, "Query failed: {resp:?}");
        assert_eq!(resp.command_id, id);
        assert_eq!(resp.size_or_status, 2); // 2 results, one byte each

        let query_results = read_payload(&mut recv, 2).await;
        assert_eq!(query_results[0], 0); // ExistFullMatch for the put address
        assert_eq!(query_results[1], 3); // NotFound for other address

        // === Second session with different correlation ID ===
        let corr_id_2 = b"test-corr-id-2";
        let mut auth_payload_2 = Vec::new();
        auth_payload_2.push(0u8); // action = start
        auth_payload_2.extend_from_slice(repository.as_bytes());
        auth_payload_2.push(corr_id_2.len() as u8);
        auth_payload_2.extend_from_slice(corr_id_2);
        auth_payload_2.extend_from_slice(&0u16.to_le_bytes()); // no token

        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Authorize as QuicOpCode,
                id,
                auth_payload_2.len(),
                0,
            ),
            &auth_payload_2,
        )
        .await;

        assert!(!resp.error, "Authorize start session 2 failed: {resp:?}");
        assert_eq!(resp.size_or_status, 4);
        let session_id_2_bytes = read_payload(&mut recv, 4).await;
        let session_id_2 = u32::from_le_bytes(session_id_2_bytes.try_into().unwrap());
        assert!(session_id_2 >= 1);
        assert_ne!(session_id_2, session_id, "Sessions must have different IDs");

        // === Put a second fragment via session 2 ===
        let (fragment2, address2, payload2) = generate_random();
        let mut put_payload_2 = Vec::new();
        put_payload_2.extend_from_slice(address2.as_bytes());
        put_payload_2.extend_from_slice(fragment2.as_bytes());
        put_payload_2.extend_from_slice(&payload2);

        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Put as QuicOpCode,
                id,
                put_payload_2.len(),
                session_id_2,
            ),
            &put_payload_2,
        )
        .await;

        assert!(!resp.error, "Put via session 2 failed: {resp:?}");
        assert_eq!(resp.session_id, session_id_2);

        // === Get fragment via session 2 ===
        let get_payload_2 = address2.as_bytes().to_vec();
        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Get as QuicOpCode,
                id,
                get_payload_2.len(),
                session_id_2,
            ),
            &get_payload_2,
        )
        .await;

        assert!(!resp.error, "Get via session 2 failed: {resp:?}");
        assert_eq!(resp.session_id, session_id_2);
        assert!(resp.size_or_status > 0);

        let get_data_2 = read_payload(&mut recv, resp.size_or_status as usize).await;
        let returned_payload_2 = &get_data_2[size_of::<Fragment>()..];
        assert_eq!(returned_payload_2, payload2.as_ref());

        // === Copy via session 1: copy fragment2 within same repo ===
        let mut copy_payload = Vec::new();
        copy_payload.extend_from_slice(repository.as_bytes()); // source_repo (16 bytes)
        copy_payload.extend_from_slice(address2.hash.as_bytes()); // source hash (32 bytes)
        copy_payload.extend_from_slice(address2.context.as_bytes()); // source context (16 bytes)
        // v4 wire bumped Copy to 80 bytes — append target_context. This test preserves
        // the source's context so the destination tuple is the same as the source's
        // (cross-partition copy) — matching the legacy semantics.
        copy_payload.extend_from_slice(address2.context.as_bytes()); // target context (16 bytes)

        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Copy as QuicOpCode,
                id,
                copy_payload.len(),
                session_id,
            ),
            &copy_payload,
        )
        .await;

        assert!(!resp.error, "Copy via session 1 failed: {resp:?}");
        assert_eq!(resp.session_id, session_id);

        // === GetMetadata via session 2: both addresses should exist ===
        let mut query_payload_2 = Vec::new();
        query_payload_2.extend_from_slice(address.as_bytes());
        query_payload_2.extend_from_slice(address2.as_bytes());

        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Query as QuicOpCode,
                id,
                query_payload_2.len(),
                session_id_2,
            ),
            &query_payload_2,
        )
        .await;

        assert!(!resp.error, "GetMetadata via session 2 failed: {resp:?}");
        assert_eq!(resp.size_or_status, 2);
        let query_results_2 = read_payload(&mut recv, 2).await;
        assert_eq!(query_results_2[0], 0, "address from session 1 should exist");
        assert_eq!(query_results_2[1], 0, "address from session 2 should exist");

        // === Authorize Stop session 1 ===
        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(Command::Authorize as QuicOpCode, id, 1, session_id),
            &[1u8], // action = stop
        )
        .await;

        assert!(!resp.error, "Authorize stop session 1 failed: {resp:?}");
        assert_eq!(resp.size_or_status, 0);

        // === Session 2 should still work after session 1 stopped ===
        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Get as QuicOpCode,
                id,
                get_payload_2.len(),
                session_id_2,
            ),
            &get_payload_2,
        )
        .await;

        assert!(
            !resp.error,
            "Get via session 2 after session 1 stopped should work"
        );
        let _ = read_payload(&mut recv, resp.size_or_status as usize).await;

        // === Get with stopped session 1 should fail ===
        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(
                Command::Get as QuicOpCode,
                id,
                get_payload.len(),
                session_id,
            ),
            &get_payload,
        )
        .await;

        assert!(resp.error, "Get on stopped session 1 should fail");

        // === Authorize Stop session 2 ===
        let id = next_cmd_id();
        let resp = send_v4_cmd(
            &mut send,
            &mut recv,
            CommandHeader::new_with_session(Command::Authorize as QuicOpCode, id, 1, session_id_2),
            &[1u8], // action = stop
        )
        .await;

        assert!(!resp.error, "Authorize stop session 2 failed: {resp:?}");

        send.finish().expect("Failed to finish stream");
    }))
    .await
    .expect("Test task failed");
}

/// Whether this platform spreads the datagrams arriving at one port across sockets bound to it.
const SPREADS_ACROSS_SOCKETS: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// Sockets the multi-socket server binds.
const SOCKETS: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// Clients, or rebinds of one client, a test tries for the hash to reach every socket, or another
/// one: enough that it fails to by chance one time in ten million.
const MAX_ATTEMPTS: u32 = 64;

/// Time a loopback exchange is allowed to take, with room for a loaded host.
const LOOPBACK_BUDGET: Duration = Duration::from_secs(5);

/// Serve [`MockService`] as `builder` configures, with the test certificate.
fn serve_mock(builder: QuinnConfigBuilder) -> QuinnServer {
    let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");
    QuinnServer::start(
        builder
            .cert_file(cert_path)
            .pkey_file(key_path)
            .stream_handler_factory(Box::new(SingleServiceFactory::new(
                MOCK_PROTOCOL,
                || MockService,
                test_limits(),
            )))
            .build()
            .unwrap(),
    )
    .expect("Failed Quinn server start")
}

/// Serve [`MockService`] on [`SOCKETS`] sockets sharing one free loopback port.
fn serve_on_sockets() -> QuinnServer {
    serve_mock(
        QuinnConfigBuilder::new()
            .address("127.0.0.1:0".parse().unwrap())
            .num_sockets(SOCKETS),
    )
}

/// Connect to `server_addr` from a client endpoint of its own, so from a source port of its own.
async fn connect_new_client(server_addr: SocketAddr) -> (Endpoint, quinn::Connection) {
    let mut crypto_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(insecure_client_auth::SkipServerVerification::new())
        .with_no_client_auth();
    crypto_config.alpn_protocols = vec![MOCK_PROTOCOL.as_bytes().into()];
    let client_config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
    ));

    let mut endpoint =
        Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("Failed to create client endpoint");
    endpoint.set_default_client_config(client_config);
    let connection = endpoint
        .connect(server_addr, "localhost")
        .unwrap()
        .await
        .expect("Failed to connect");
    (endpoint, connection)
}

/// Ask `connection` for an echo of `command_id` and wait for it.
async fn echo(connection: &quinn::Connection, command_id: u32) -> anyhow::Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&CommandHeader::new(MockBehaviour::Echo.opcode(), command_id, 0).to_bytes())
        .await?;
    let mut buffer = [0u8; 8];
    recv.read_exact(&mut buffer).await?;
    let served = CommandHeader::from_bytes(&buffer);
    anyhow::ensure!(
        served.command_id == command_id && !served.error,
        "got: {served:?}"
    );
    Ok(())
}

/// Connect clients, each from a source port of its own and each served an echo, until every
/// socket of `server` has accepted one or [`MAX_ATTEMPTS`] have connected.
async fn connect_to_every_socket(server: &QuinnServer) -> Vec<(Endpoint, quinn::Connection)> {
    let server_addr = server
        .socket_addrs()
        .expect("Failed to read the socket addresses")[0];
    let mut clients = Vec::new();
    for command_id in 0..MAX_ATTEMPTS {
        let (endpoint, connection) = connect_new_client(server_addr).await;
        echo(&connection, command_id)
            .await
            .expect("Client was not served");
        clients.push((endpoint, connection));
        if server.socket_handshakes().iter().all(|count| *count > 0) {
            break;
        }
    }
    clients
}

/// Clients the kernel spreads across the sockets are each served, whichever socket's endpoint
/// their datagrams reach.
#[tokio::test]
async fn clients_spread_across_the_sockets_are_all_served() {
    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let server = serve_on_sockets();
        let clients = connect_to_every_socket(&server).await;

        let handshakes = server.socket_handshakes();
        let expected = if SPREADS_ACROSS_SOCKETS {
            SOCKETS.get()
        } else {
            1
        };
        assert_eq!(handshakes.len(), expected, "got: {handshakes:?}");
        assert_eq!(
            handshakes.iter().sum::<u64>(),
            clients.len() as u64,
            "got: {handshakes:?}"
        );
        assert!(
            handshakes.iter().all(|count| *count > 0),
            "a socket served no client, so the kernel did not spread them: {handshakes:?}"
        );
    }))
    .await
    .expect("Test task failed");
}

#[tokio::test]
async fn closing_the_server_closes_the_connections_on_every_socket() {
    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let server = serve_on_sockets();
        let clients = connect_to_every_socket(&server).await;

        tokio::time::timeout(LOOPBACK_BUDGET, server.close())
            .await
            .expect("The server did not close in time");
        for (_, connection) in &clients {
            let closed = tokio::time::timeout(LOOPBACK_BUDGET, connection.closed())
                .await
                .expect("A connection was not closed in time");
            assert!(
                matches!(closed, ConnectionError::ApplicationClosed(_)),
                "got: {closed:?}"
            );
        }
    }))
    .await
    .expect("Test task failed");
}

/// A client whose address changes reaches another socket's endpoint, which resets the connection
/// rather than leaving it to time out.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::test]
async fn a_client_reaching_another_socket_is_reset() {
    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let server = serve_on_sockets();
        let server_addr = server
            .socket_addrs()
            .expect("Failed to read the socket addresses")[0];
        let (endpoint, connection) = connect_new_client(server_addr).await;

        for command_id in 0..MAX_ATTEMPTS {
            endpoint
                .rebind(UdpSocket::bind("127.0.0.1:0").unwrap())
                .expect("Failed to rebind");
            let served = tokio::time::timeout(LOOPBACK_BUDGET, echo(&connection, command_id))
                .await
                .expect("The connection was neither served nor reset in time");
            if served.is_err() {
                let reason = connection.close_reason();
                assert!(
                    matches!(reason, Some(ConnectionError::Reset)),
                    "got: {reason:?}"
                );
                return;
            }
        }
        panic!("No rebind reached another socket");
    }))
    .await
    .expect("Test task failed");
}

#[test]
fn a_socket_recognises_the_connection_ids_of_its_siblings_and_no_others() {
    const IDS: u32 = 1000;

    let keys: Arc<[u64]> = Arc::new([random(), random()]);
    let own = SocketConnectionIdGenerator::new(keys[0], keys.clone());
    let mut sibling = SocketConnectionIdGenerator::new(keys[1], keys.clone());
    let mut stranger = HashedConnectionIdGenerator::from_key(random());
    for _ in 0..IDS {
        let issued = sibling.generate_cid();
        assert!(own.validate(&issued).is_ok(), "rejected {issued:?}");
        let foreign = stranger.generate_cid();
        assert!(own.validate(&foreign).is_err(), "accepted {foreign:?}");
    }
}

#[tokio::test]
async fn a_socket_the_caller_bound_is_served_alone() {
    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().expect("Failed socket setup");

        let server = serve_mock(
            QuinnConfigBuilder::new()
                .socket(socket)
                .num_sockets(SOCKETS),
        );

        let addrs = server
            .socket_addrs()
            .expect("Failed to read the socket addresses");
        assert_eq!(addrs, vec![addr]);
    }))
    .await
    .expect("Test task failed");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::test]
async fn the_default_is_16_sockets_or_one_per_processor_if_fewer() {
    let (_immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
        let server = serve_mock(QuinnConfigBuilder::new().address("127.0.0.1:0".parse().unwrap()));

        let addrs = server
            .socket_addrs()
            .expect("Failed to read the socket addresses");
        assert_eq!(
            addrs.len(),
            lore_base::runtime::processor_count().min(16),
            "got: {addrs:?}"
        );
    }))
    .await
    .expect("Test task failed");
}
