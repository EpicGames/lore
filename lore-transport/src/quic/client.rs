// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures::TryFutureExt;
use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_base::lore_debug;
use lore_base::lore_error;
use lore_base::lore_info;
use lore_base::lore_trace;
use lore_base::lore_warn;
use lore_error_set::prelude::*;
use quinn::AckFrequencyConfig;
use quinn::IdleTimeout;
use quinn::VarInt;
use quinn::congestion;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::RootCertStore;
use rustls_native_certs::load_native_certs;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::sync::oneshot;
use url::Url;

use super::MAX_RTT_MS;
use super::PACKET_THRESHOLD;
use super::QuicClientError;
use super::QuicOpCode;
use super::TIME_THRESHOLD;
use super::command_header::CommandHeader;
use super::response_reader::ResponseReader;
use crate::connection::RECONNECT_MAX_ATTEMPTS;
use crate::connection::RECONNECT_MAX_DELAY;
use crate::connection::RECONNECT_START_DELAY;
use crate::error::ProtocolError;
use crate::tls::load_certs;
use crate::tls::load_private_key;

pub const STREAM_COUNT: u32 = 8;
pub const PRIORITY_STREAM_COUNT: u32 = 2;

/// Configuration for establishing a QUIC connection to a remote endpoint.
#[derive(Clone, Debug)]
pub struct EndpointConfig {
    pub remote_url: String,
    pub default_port: u16,
    /// When `Some`, used as the `server_name` argument to `quinn::Endpoint::connect`
    /// instead of the URL host. This enables connections to IP-addressed peers (from
    /// topology) to present the correct server name for TLS validation.
    pub sni_override: Option<String>,
}

const IDLE_TIMEOUT_MS: u32 = 30000;
const KEEP_ALIVE_MS: u64 = 500;
pub const DEFAULT_EXPECTED_RTT_MS: u64 = 100;

#[derive(Clone, Debug)]
pub struct ClientCerts {
    pub cert_file: PathBuf,
    pub pkey_file: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CertificateSettings {
    // if the server is using a custom CA, clients can pass the file here
    pub custom_ca: Option<PathBuf>,
    // if clients should send certs, otherwise no certs are sent
    pub client: Option<ClientCerts>,
}

/// Statistics about UDP datagrams transmitted or received on a connection
pub struct UdpStats {
    /// The number of UDP datagrams observed
    pub datagrams: u64,
    /// The total bytes transferred inside UDP datagrams
    pub bytes: u64,
    /// The number of I/O operations executed (may be less than `datagrams` with GSO/GRO)
    pub ios: u64,
}

/// Number of frames transmitted or received, by frame type
pub struct FrameStats {
    pub data_blocked: u64,
    pub stream_data_blocked: u64,
    pub streams_blocked_bidi: u64,
    pub streams_blocked_uni: u64,
    pub max_data: u64,
    pub max_stream_data: u64,
    pub max_streams_bidi: u64,
    pub stream: u64,
    pub reset_stream: u64,
}

/// Statistics related to the current transmission path
pub struct PathStats {
    /// Current best estimate of this connection's latency (round-trip-time)
    pub rtt: Duration,
    /// Current congestion window of the connection in bytes
    pub cwnd: u64,
    /// Cumulative congestion events on the connection
    pub congestion_events: u64,
    /// Cumulative packets lost on this path
    pub lost_packets: u64,
    /// Cumulative bytes lost on this path
    pub lost_bytes: u64,
    /// Cumulative packets sent on this path
    pub sent_packets: u64,
    /// Number of black holes detected on the path
    pub black_holes_detected: u64,
    /// Largest UDP payload size the path currently supports
    pub current_mtu: u16,
}

/// Connection statistics, decoupled from the underlying QUIC implementation.
pub struct ConnectionStats {
    /// Statistics about UDP datagrams transmitted
    pub udp_tx: UdpStats,
    /// Statistics about UDP datagrams received
    pub udp_rx: UdpStats,
    /// Statistics about frames transmitted
    pub frame_tx: FrameStats,
    /// Statistics about frames received
    pub frame_rx: FrameStats,
    /// Statistics related to the current transmission path
    pub path: PathStats,
}

impl From<quinn::ConnectionStats> for ConnectionStats {
    fn from(s: quinn::ConnectionStats) -> Self {
        Self {
            udp_tx: UdpStats {
                datagrams: s.udp_tx.datagrams,
                bytes: s.udp_tx.bytes,
                ios: s.udp_tx.ios,
            },
            udp_rx: UdpStats {
                datagrams: s.udp_rx.datagrams,
                bytes: s.udp_rx.bytes,
                ios: s.udp_rx.ios,
            },
            frame_tx: FrameStats {
                data_blocked: s.frame_tx.data_blocked,
                stream_data_blocked: s.frame_tx.stream_data_blocked,
                streams_blocked_bidi: s.frame_tx.streams_blocked_bidi,
                streams_blocked_uni: s.frame_tx.streams_blocked_uni,
                max_data: s.frame_tx.max_data,
                max_stream_data: s.frame_tx.max_stream_data,
                max_streams_bidi: s.frame_tx.max_streams_bidi,
                stream: s.frame_tx.stream,
                reset_stream: s.frame_tx.reset_stream,
            },
            frame_rx: FrameStats {
                data_blocked: s.frame_rx.data_blocked,
                stream_data_blocked: s.frame_rx.stream_data_blocked,
                streams_blocked_bidi: s.frame_rx.streams_blocked_bidi,
                streams_blocked_uni: s.frame_rx.streams_blocked_uni,
                max_data: s.frame_rx.max_data,
                max_stream_data: s.frame_rx.max_stream_data,
                max_streams_bidi: s.frame_rx.max_streams_bidi,
                stream: s.frame_rx.stream,
                reset_stream: s.frame_rx.reset_stream,
            },
            path: PathStats {
                rtt: s.path.rtt,
                cwnd: s.path.cwnd,
                congestion_events: s.path.congestion_events,
                lost_packets: s.path.lost_packets,
                lost_bytes: s.path.lost_bytes,
                sent_packets: s.path.sent_packets,
                black_holes_detected: s.path.black_holes_detected,
                current_mtu: s.path.current_mtu,
            },
        }
    }
}

#[derive(Clone)]
pub enum CongestionAlgorithm {
    Bbr,
    Cubic,
}

#[derive(Clone)]
pub struct TransportConfig {
    pub max_bytes_bandwidth_per_second: u64,
    pub expected_rtt_ms: u64,
    pub congestion_algorithm: CongestionAlgorithm,
    /// Warm-start hint for Congestion Algorithms: seed the initial congestion window
    pub initial_cwnd: Option<u64>,
}

/// When working within a QUIC connection, these are the opportunities
/// for any authentication/authorization to occur. Errors raised will be mapped to `QuicClientError`
/// by the generic client logic
#[async_trait]
pub trait AuthAdapter: Send + Sync {
    type ErrorType: std::error::Error + Send + Sync;

    /// Called when first establishing the QUIC connection. See this as an opportunity
    /// to fail early and vocally if authentication/authorization is not correct
    async fn initial_authorize(
        &self,
        connection: Arc<QuicConnection>,
    ) -> Result<(), Self::ErrorType>;

    /// After previously successfully establishing a connection, this is the
    /// authentication/authorization logic that runs if we need to reestablish a connection.
    /// Since it was proven previously that a connection is possible, and we have correct
    /// credentials, this logic and its failures  should be seen as more background/benign.
    async fn reconnect_authorize(
        &self,
        connection: Arc<QuicConnection>,
    ) -> Result<(), QuicClientError>;

    /// The certs to provide when establishing the QUIC connection
    fn client_certs(&self) -> CertificateSettings;
}

/// When interacting with a QUIC server, these are the required functionality
/// a QUIC client should provide to use the QUIC client scaffolding
pub trait ServiceClient: Send + Sync {
    const ALPN: &'static str;
    const DEFAULT_PORT: u16;

    /// Concrete type that represents what opcodes can be sent
    type RequestType: Into<QuicOpCode> + Copy + Send;
    /// The concrete error types that `QuicClientError` can be converted into, when receiving
    /// error responses from the Server
    type ErrorType: std::error::Error + Send + Sync;

    /// Rate limiting message throughput
    fn acquire_command_permit(&self) -> impl Future<Output = Option<SemaphorePermit<'_>>> + Send;

    /// The underlying QUIC connection being used by this client
    fn quic(&self) -> &Arc<QuicConnection>;

    /// The endpoint configuration for connecting to the remote server
    fn endpoint_config(&self) -> EndpointConfig;

    /// The ALPN to use when connecting to the server
    fn alpn(&self) -> &str;

    /// Given a failure to send request bytes under the given `RequestType`,
    /// convert this generic error into the `ServiceClient` error space.
    /// This logic is done within the send function rather than outside it
    /// to the keep the size of the future as small as possible
    fn map_send_error(
        &self,
        failed_request: Self::RequestType,
        error: SendWithReconnectError,
    ) -> Self::ErrorType;

    /// The implementation of how authentication/authorization is done by this client
    fn auth_adapter(&self) -> &Arc<dyn AuthAdapter<ErrorType = Self::ErrorType>>;

    /// Defines how the underlying quic connection is configured
    fn transport_config(&self) -> TransportConfig;

    /// Whether this client uses the v4 protocol (12-byte headers with `session_id`).
    /// Default is false (v2, 8-byte headers).
    fn v4_protocol(&self) -> bool {
        false
    }
}

struct QuicQuinnConnection {
    connection: quinn::Connection,
    writer: Vec<Arc<Mutex<quinn::SendStream>>>,
    reader: Vec<ResponseReader>,
}

impl QuicQuinnConnection {
    async fn close(&mut self) {
        let connection_id = self.connection.stable_id();
        lore_debug!(
            "QUIC connection {connection_id} stats: {:?}",
            self.connection.stats()
        );
        // Skip per-stream finish() + stopped() + reader.task.await. The server treats
        // CONNECTION_CLOSE(app, 0) as a graceful close (see is_graceful_close in
        // lore-server/src/quic/stream_handler.rs), so the stream-level FIN handshake
        // (1 RTT per stream) is unnecessary. Dropping the reader/writer vecs detaches
        // the reader tasks; they exit promptly once the connection's close() below
        // terminates their recv streams.
        self.writer.clear();
        self.reader.clear();
        self.connection
            .close(quinn::VarInt::from(0u32), b"terminate");
        // Drive I/O until the close frame has been flushed to the peer. Without this,
        // the process can exit before the CONNECTION_CLOSE reaches the server, causing
        // server-side stream reads to surface as transport errors rather than normal close.
        self.connection.closed().await;
    }
}

pub struct QuicConnection {
    connection: RwLock<QuicQuinnConnection>,
    created: Instant,
    last_send: AtomicU64,
    last_recv: Arc<AtomicU64>,
    pub epoch: AtomicU32,
    max_reconnects: Option<u32>,
    reconnect_guard: Semaphore,
    counter: AtomicU32,
    non_priority_counter: AtomicU32,
    pub stream_count: AtomicU32,
    stream_inflight: Arc<[AtomicU64; STREAM_COUNT as usize]>,
    max_chunk_size: usize,
    v4: bool,
}

impl QuicConnection {
    pub fn new(connection: quinn::Connection, max_chunk_size: usize) -> Self {
        Self::with_v4(connection, max_chunk_size, false)
    }

    pub fn with_v4(connection: quinn::Connection, max_chunk_size: usize, v4: bool) -> Self {
        QuicConnection {
            connection: RwLock::new(QuicQuinnConnection {
                connection,
                writer: vec![],
                reader: vec![],
            }),
            created: Instant::now(),
            last_send: AtomicU64::new(0),
            last_recv: Arc::new(AtomicU64::new(0)),
            epoch: AtomicU32::new(1),
            max_reconnects: None,
            reconnect_guard: Semaphore::new(1),
            counter: AtomicU32::new(0),
            non_priority_counter: AtomicU32::new(0),
            stream_count: AtomicU32::new(0),
            stream_inflight: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            max_chunk_size,
            v4,
        }
    }

    pub async fn create_initial_stream(&self) -> Result<(), QuicClientError> {
        let last_recv = self.last_recv.clone();
        let created = self.created;

        {
            let mut connection = self.connection.write().await;

            let (send, recv) = connection
                .connection
                .open_bi()
                .await
                .map_err(|_err| QuicClientError::StreamOpen)?;
            connection.writer.push(Arc::new(Mutex::new(send)));
            connection.reader.push(ResponseReader::new(
                0,
                recv,
                self.max_chunk_size,
                last_recv,
                created,
                self.v4,
            ));
            lore_trace!("Created initial connect bidirectional stream");
        }

        Ok(())
    }

    pub async fn has_streams(&self) -> bool {
        !self.connection.read().await.reader.is_empty()
    }

    pub async fn close(&self) {
        let mut connection = self.connection.write().await;
        connection.close().await;
    }

    /// Close the QUIC connection immediately without waiting for streams to drain.
    /// Used in Drop to avoid blocking the runtime during shutdown.
    pub fn close_immediate(&self) {
        if let Ok(connection) = self.connection.try_write() {
            connection
                .connection
                .close(quinn::VarInt::from(0u32), b"terminate");
        }
    }

    pub fn set_max_reconnects(&mut self, max_reconnects: Option<u32>) {
        self.max_reconnects = max_reconnects;
    }

    pub async fn connection_stats(&self) -> ConnectionStats {
        self.connection.read().await.connection.stats().into()
    }
}

#[derive(Debug, Error)]
pub enum SendWithReconnectError {
    #[error("Failed to acquire permit to run command")]
    PermitAcquire,
    #[error("QUIC Client Error: {0}")]
    ClientError(#[from] QuicClientError),
    #[error("Disconnected from server")]
    Disconnected,
    #[error("Reconnect to server failed")]
    ReconnectFailed,
}

#[error_set]
pub enum ReconnectError {
    Disconnected,
    NotAuthorized,
}

/// Send a command to the QUIC server, automatically reconnecting on transient failures.
///
/// Acquires a rate-limiting permit, sends the command via [`send_command`], and handles
/// the response. On transient errors (`Terminated`, `StreamOpen`), triggers a reconnect
/// and retries. On ambiguous errors, checks whether a concurrent reconnect already
/// occurred (via the epoch counter) and retries if so, otherwise propagates the error.
///
/// `HIGH_PRIORITY` is a const generic rather than a runtime parameter to keep it out of
/// the async future state. Because this function contains a retry loop with multiple await
/// points, any runtime parameter would be captured in the compiler-generated future struct
/// for the lifetime of the loop. The `Storage::get` future is heap-allocated via
/// `async_trait` boxing, so every byte in the future state is a per-request allocation
/// cost. Using a const generic resolves the priority value at compile time through
/// monomorphization, adding zero bytes to the future. This is validated by the
/// `test_futures_size` test which enforces a strict upper bound on the get future size.
pub async fn send_with_reconnect<ServiceClientType, const LEN: usize, const HIGH_PRIORITY: bool>(
    service_client: &ServiceClientType,
    request_type: ServiceClientType::RequestType,
    session_id: u32,
    chunks: impl Fn() -> [Bytes; LEN],
) -> Result<Bytes, ServiceClientType::ErrorType>
where
    ServiceClientType: ServiceClient,
{
    loop {
        let Some(permit) = service_client.acquire_command_permit().await else {
            return Err(
                service_client.map_send_error(request_type, SendWithReconnectError::PermitAcquire)
            );
        };

        let epoch = service_client.quic().epoch.load(Ordering::Relaxed);
        match send_command::<HIGH_PRIORITY>(
            service_client.quic().clone(),
            request_type.into(),
            session_id,
            service_client.v4_protocol(),
            &mut chunks(),
        )
        .await
        {
            Ok(payload) => return Ok(payload),
            // error handling for things that cannot be recovered by reconnecting
            // and should be bubbled up to the caller immediately
            Err(err)
                if matches!(
                    err,
                    QuicClientError::SlowDown
                        | QuicClientError::NotAuthorized
                        | QuicClientError::NotFound
                        | QuicClientError::ClientMessageTooBig
                ) =>
            {
                return Err(service_client
                    .map_send_error(request_type, SendWithReconnectError::ClientError(err)));
            }
            // error handling for things that should trigger a reconnect
            Err(QuicClientError::Terminated | QuicClientError::StreamOpen) => {
                // Fall through to reconnect
                drop(permit);
            }
            // a non retryable connection error - so just mark as disconnected immediately
            Err(QuicClientError::CrytpoError) => {
                return Err(service_client
                    .map_send_error(request_type, SendWithReconnectError::Disconnected));
            }
            // error handling for things that have indicated an error, but the error could
            // be related to something else that triggered a reconnect. We should see if we are
            // reconnecting, and if we are then retry the message again otherwise bubble it up
            Err(err) => {
                drop(permit);

                let epoch_current = service_client.quic().epoch.load(Ordering::Relaxed);
                if epoch_current == 0 {
                    return Err(service_client
                        .map_send_error(request_type, SendWithReconnectError::Disconnected));
                }
                if epoch >= epoch_current {
                    // Not reconnected, return failure
                    return Err(service_client
                        .map_send_error(request_type, SendWithReconnectError::ClientError(err)));
                }

                // Reconnected, loop and retry
                continue;
            }
        };

        if Box::pin(reconnect(
            service_client.endpoint_config(),
            service_client.alpn(),
            service_client.auth_adapter().clone(),
            service_client.transport_config(),
            service_client.quic().clone(),
            epoch,
        ))
        .await
        .is_err()
        {
            return Err(service_client
                .map_send_error(request_type, SendWithReconnectError::ReconnectFailed));
        }
    }
}

fn strip_ipv6_brackets(host: &str) -> &str {
    if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    }
}

pub mod insecure_client_auth {
    use std::sync::Arc;

    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::ServerName;
    use rustls::pki_types::UnixTime;

    #[derive(Debug)]
    pub struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

    impl SkipServerVerification {
        pub fn new() -> Arc<Self> {
            Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
        }
    }

    impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}

fn client_crypto_config(
    alpn: &str,
    certificate_settings: CertificateSettings,
    validate_server_certificate: bool,
) -> Result<rustls::ClientConfig, ProtocolError> {
    let client_builder = if !validate_server_certificate {
        rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .dangerous()
            .with_custom_certificate_verifier(insecure_client_auth::SkipServerVerification::new())
    } else {
        let mut cert_store = RootCertStore::empty();

        // load native certs
        let native_certs = load_native_certs();
        if native_certs.certs.is_empty() {
            return Err(ProtocolError::internal(
                "failed to load native certificates",
            ));
        }
        for cert in native_certs.certs {
            let _ = cert_store.add(cert);
        }

        // load custom ca
        if let Some(ca_path) = &certificate_settings.custom_ca {
            let ca_certs = load_certs(ca_path)
                .internal_with(|| format!("loading CA certificate from {}", ca_path.display()))?;
            for cert in ca_certs {
                let _ = cert_store.add(cert);
            }
        }

        rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_root_certificates(cert_store)
    };

    let mut cfg = if let Some(client_certs) = certificate_settings.client {
        // Load client certificate(s)
        let mut certs = load_certs(&client_certs.cert_file).internal_with(|| {
            format!(
                "loading client certificate from {}",
                client_certs.cert_file.display()
            )
        })?;

        // Append chain if provided
        if let Some(chain_path) = &certificate_settings.custom_ca {
            let chain_certs = load_certs(chain_path).internal_with(|| {
                format!("loading certificate chain from {}", chain_path.display())
            })?;
            certs.extend(chain_certs);
        }

        // Load private key
        let key = load_private_key(&client_certs.pkey_file).internal_with(|| {
            format!(
                "loading private key from {}",
                client_certs.pkey_file.display()
            )
        })?;

        client_builder
            .with_client_auth_cert(certs, key)
            .internal("building client auth certificate chain")?
    } else {
        client_builder.with_no_client_auth()
    };

    cfg.enable_early_data = true;
    cfg.alpn_protocols = vec![alpn.into()];

    Ok(cfg)
}

pub async fn connect(
    config: &EndpointConfig,
    certificate_settings: CertificateSettings,
    alpn: &str,
    transport: TransportConfig,
) -> Result<quinn::Connection, ProtocolError> {
    let remote_url = config.remote_url.as_str();
    let url = Url::parse(remote_url).internal_with(|| format!("remote {remote_url} is invalid"))?;
    let host = url.host_str().unwrap_or_default().to_string();
    let remote_addrs = (
        strip_ipv6_brackets(host.as_str()),
        url.port().unwrap_or(config.default_port),
    )
        .to_socket_addrs()
        .internal_with(|| format!("remote {remote_url} is invalid"))?;
    let server_name = config.sni_override.as_deref().unwrap_or(host.as_str());

    let validate_certificate = url.scheme().ends_with("s");
    let crypto_config = client_crypto_config(alpn, certificate_settings, validate_certificate)?;

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto_config).internal("configuring QUIC client crypto")?,
    ));

    let mut transport_config = quinn::TransportConfig::default();
    transport_config
        .max_concurrent_uni_streams(0_u8.into())
        .max_concurrent_bidi_streams(STREAM_COUNT.into());

    transport_config
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(IDLE_TIMEOUT_MS))))
        .keep_alive_interval(Some(Duration::from_millis(KEEP_ALIVE_MS)));

    let recv_window = (transport.max_bytes_bandwidth_per_second / 1000) * transport.expected_rtt_ms;
    let send_window = recv_window;
    // Any stream can use at most 3 times the average stream recv window
    let stream_recv_window = (recv_window / STREAM_COUNT as u64) * 3;

    transport_config
        .send_window(send_window)
        .receive_window(VarInt::from_u64(recv_window).map_err(|_err| {
            lore_warn!("recv_window {recv_window} exceeds VarInt max");
            ProtocolError::internal("client initialization failure")
        })?)
        .stream_receive_window(VarInt::from_u64(stream_recv_window).map_err(|_err| {
            lore_warn!("stream_recv_window {stream_recv_window} exceeds VarInt max");
            ProtocolError::internal("client initialization failure")
        })?)
        .datagram_receive_buffer_size(None)
        .datagram_send_buffer_size(0);

    let mut ack_freq_config = AckFrequencyConfig::default();
    ack_freq_config.reordering_threshold(VarInt::from_u32(PACKET_THRESHOLD - 1));

    transport_config
        .send_fairness(false)
        .packet_threshold(PACKET_THRESHOLD)
        .time_threshold(TIME_THRESHOLD)
        .max_rtt(Duration::from_millis(MAX_RTT_MS))
        .ack_frequency_config(Some(ack_freq_config));

    let congestion_controller: Arc<dyn congestion::ControllerFactory + Send + Sync + 'static> =
        match transport.congestion_algorithm {
            CongestionAlgorithm::Bbr => {
                let mut bbr = congestion::BbrConfig::default();
                if let Some(cwnd) = transport.initial_cwnd {
                    bbr.initial_window(cwnd);
                }
                Arc::new(bbr)
            }
            CongestionAlgorithm::Cubic => {
                let mut cubic = congestion::CubicConfig::default();
                if let Some(cwnd) = transport.initial_cwnd {
                    cubic.initial_window(cwnd);
                }

                Arc::new(cubic)
            }
        };
    transport_config.congestion_controller_factory(congestion_controller);

    lore_debug!("QUIC transport config: {transport_config:?}");

    client_config.transport_config(Arc::new(transport_config));

    for remote_addr in remote_addrs {
        lore_debug!("QUIC connecting to {host} at {remote_addr}");
        let bind = if remote_addr.is_ipv6() {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
        } else {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        };
        match quinn::Endpoint::client(bind) {
            Ok(mut endpoint) => {
                endpoint.set_default_client_config(client_config.clone());
                match endpoint.connect(remote_addr, server_name) {
                    Ok(connecting) => match connecting.await {
                        Ok(connection) => {
                            lore_debug!("Success QUIC connecting to {remote_addr}");
                            return Ok(connection);
                        }
                        Err(err) => {
                            lore_debug!("Failed QUIC connecting to {remote_addr}: {err}");
                        }
                    },
                    Err(err) => {
                        lore_debug!("Failed QUIC connect to {remote_addr}: {err}");
                    }
                }
            }
            Err(err) => {
                lore_debug!("QUIC failed binding socket to {bind} for {remote_addr}: {err}");
            }
        }
    }

    // Silent propagation of connection errors
    lore_debug!("QUIC connect failed {remote_url}");
    Err(ProtocolError::internal(format!("connect: {remote_url}")))
}

pub async fn reconnect<AuthErrorType>(
    config: EndpointConfig,
    alpn: &str,
    auth_adapter: Arc<dyn AuthAdapter<ErrorType = AuthErrorType>>,
    transport_config: TransportConfig,
    connection: Arc<QuicConnection>,
    epoch: u32,
) -> Result<(), ReconnectError>
where
    AuthErrorType: std::error::Error + Send + Sync,
{
    let remote_url = &config.remote_url;
    let Ok(_permit) = connection.reconnect_guard.acquire().await else {
        return Err(ReconnectError::from(Disconnected));
    };

    let epoch_current = connection.epoch.load(Ordering::Relaxed);
    if epoch_current == 0 {
        // Reconnection failed, give up
        return Err(ReconnectError::from(Disconnected));
    }
    if epoch < epoch_current {
        // Something else reconnected
        return Ok(());
    }

    let elapsed = connection.created.elapsed().as_millis() as u64;
    {
        let quinn = connection.connection.read().await;
        let connection_id = quinn.connection.stable_id();
        lore_warn!(
            "QUIC lost connection {connection_id} to {remote_url} after {:.2}s: {:?} (last send: {:.2}s, last recv: {:.2}s)",
            elapsed as f64 / 1000.0,
            quinn.connection.close_reason(),
            (elapsed - connection.last_send.load(Ordering::Relaxed)) as f64 / 1000.0,
            (elapsed - connection.last_recv.load(Ordering::Relaxed)) as f64 / 1000.0
        );

        lore_debug!(
            "QUIC connection {connection_id} stats: {:?}",
            quinn.connection.stats()
        );

        quinn.connection.close(0u32.into(), b"lost connection");
    }

    if let Some(max_reconnects) = connection.max_reconnects
        // will be 1 for the initial successful initial connect, therefore if greater
        // than the max is when we have reached the limit
        && epoch_current > max_reconnects
    {
        lore_info!("Total reconnects to {remote_url} exhausted - not reconnecting");
        // Indicate that any pending commands entering their retry flow should give up
        connection.epoch.store(0, Ordering::Relaxed);
        return Err(ReconnectError::from(Disconnected));
    }

    let mut retry_count = 1;
    let mut retry = crate::util::retry(
        RECONNECT_START_DELAY,
        RECONNECT_MAX_DELAY,
        RECONNECT_MAX_ATTEMPTS,
    );

    loop {
        lore_info!(
            "Reconnecting to {} attempt {retry_count} / {RECONNECT_MAX_ATTEMPTS}",
            remote_url
        );

        let start = Instant::now();

        match connect(
            &config,
            auth_adapter.client_certs(),
            alpn,
            transport_config.clone(),
        )
        .await
        {
            Ok(quic_connection) => {
                lore_debug!(
                    "QUIC reconnected to {remote_url} in {}ms",
                    start.elapsed().as_millis()
                );

                let connection_id = quic_connection.stable_id();

                {
                    let mut connection = connection.connection.write().await;
                    for reader in connection.reader.drain(..) {
                        let _ = reader.task.await;
                    }

                    connection.reader = vec![];
                    connection.writer = vec![];
                    connection.connection = quic_connection;
                }

                let restart_flow = connection
                    .create_initial_stream()
                    .and_then(|_| auth_adapter.reconnect_authorize(connection.clone()));
                match restart_flow.await {
                    Ok(_) => {
                        connection.stream_count.store(1, Ordering::Relaxed);
                        // Indicate that the reconnect attempt was successful and let any
                        // pending commands see that they can just early out and resend
                        let epoch_current = 1 + connection.epoch.fetch_add(1, Ordering::Relaxed);

                        lore_debug!(
                            "QUIC reconnection {connection_id} to {remote_url} complete in {}ms ({epoch} -> {epoch_current})",
                            start.elapsed().as_millis()
                        );

                        break;
                    }
                    Err(
                        QuicClientError::StreamOpen
                        | QuicClientError::Terminated
                        | QuicClientError::SlowDown,
                    ) => {
                        lore_debug!("Reconnect authorization failed, retry");
                        if !retry.wait().await {
                            lore_debug!("Reconnect attempts exhausted, giving up");
                            {
                                let connection = connection.connection.write().await;
                                connection.connection.close(0u32.into(), b"failed connect");
                            }
                            // Indicate that any pending commands entering this flow should give up
                            connection.epoch.store(0, Ordering::Relaxed);
                            return Err(ReconnectError::from(Disconnected));
                        }
                    }
                    Err(err) => {
                        {
                            let connection = connection.connection.read().await;
                            connection
                                .connection
                                .close(0u32.into(), b"failed authorization");
                        }
                        // Indicate that any pending commands entering this flow should give up
                        connection.epoch.store(0, Ordering::Relaxed);

                        lore_error!("Failed to reconnect, authorization failed: {err}");
                        return Err(ReconnectError::from(NotAuthorized));
                    }
                }
            }
            Err(err) => {
                lore_debug!("Reconnect attempt failed: {err}");
                if !retry.wait().await {
                    lore_debug!("Reconnect attempts exhausted, giving up");
                    {
                        let connection = connection.connection.write().await;
                        connection.connection.close(0u32.into(), b"failed connect");
                    }
                    // Indicate that any pending commands entering this flow should give up
                    connection.epoch.store(0, Ordering::Relaxed);
                    return Err(ReconnectError::from(Disconnected));
                }
            }
        }

        retry_count += 1;
    }

    lore_info!("Reconnected to {}", remote_url);

    Ok(())
}

async fn add_stream(connection: Arc<QuicConnection>) -> Result<u32, QuicClientError> {
    let last_recv = connection.last_recv.clone();
    let created = connection.created;

    let mut connection_lock = connection.connection.write().await;

    let stream_index = connection_lock.writer.len() as u32;

    if stream_index < STREAM_COUNT {
        let (send, recv) = connection_lock
            .connection
            .open_bi()
            .await
            .inspect_err(|err| {
                if stream_index == 0 {
                    lore_debug!("Unable to open base stream: {err}");
                }
            })
            .map_err(|_err| QuicClientError::StreamOpen)?;
        connection_lock.writer.push(Arc::new(Mutex::new(send)));
        connection_lock.reader.push(ResponseReader::new(
            stream_index,
            recv,
            connection.max_chunk_size,
            last_recv.clone(),
            created,
            connection.v4,
        ));

        connection
            .stream_count
            .store(stream_index, Ordering::Relaxed);

        Ok(stream_index)
    } else {
        Ok(stream_index - 1)
    }
}

/// Select stream index based on priority scheduling.
fn select_stream(connection: &QuicConnection, reader_count: u32, high_priority: bool) -> u32 {
    if high_priority {
        // Pick the stream with fewest outstanding requests
        let mut min_inflight = u64::MAX;
        let mut min_stream = 0u32;
        for i in 0..reader_count {
            let inflight = connection.stream_inflight[i as usize].load(Ordering::Relaxed);
            if inflight < min_inflight {
                min_inflight = inflight;
                min_stream = i;
            }
        }
        min_stream
    } else {
        // Round-robin across streams PRIORITY_STREAM_COUNT..STREAM_COUNT
        let index = connection
            .non_priority_counter
            .fetch_add(1, Ordering::Relaxed);
        if reader_count > PRIORITY_STREAM_COUNT {
            PRIORITY_STREAM_COUNT + (index % (reader_count - PRIORITY_STREAM_COUNT))
        } else {
            0
        }
    }
}

pub async fn send_normal(
    connection: Arc<QuicConnection>,
    command: QuicOpCode,
    session_id: u32,
    v4: bool,
    chunks: &mut [Bytes],
) -> Result<Bytes, QuicClientError> {
    send_command::<false>(connection, command, session_id, v4, chunks).await
}

pub async fn send_high_priority(
    connection: Arc<QuicConnection>,
    command: QuicOpCode,
    session_id: u32,
    v4: bool,
    chunks: &mut [Bytes],
) -> Result<Bytes, QuicClientError> {
    send_command::<true>(connection, command, session_id, v4, chunks).await
}

pub fn send_normal_with_reconnect<'a, ServiceClientType, const LEN: usize>(
    service_client: &'a ServiceClientType,
    request_type: ServiceClientType::RequestType,
    session_id: u32,
    chunks: impl Fn() -> [Bytes; LEN] + Send + 'a,
) -> impl Future<Output = Result<Bytes, ServiceClientType::ErrorType>> + Send + 'a
where
    ServiceClientType: ServiceClient,
{
    send_with_reconnect::<ServiceClientType, LEN, false>(
        service_client,
        request_type,
        session_id,
        chunks,
    )
}

pub fn send_high_priority_with_reconnect<'a, ServiceClientType, const LEN: usize>(
    service_client: &'a ServiceClientType,
    request_type: ServiceClientType::RequestType,
    session_id: u32,
    chunks: impl Fn() -> [Bytes; LEN] + Send + 'a,
) -> impl Future<Output = Result<Bytes, ServiceClientType::ErrorType>> + Send + 'a
where
    ServiceClientType: ServiceClient,
{
    send_with_reconnect::<ServiceClientType, LEN, true>(
        service_client,
        request_type,
        session_id,
        chunks,
    )
}

pub async fn send_command<const HIGH_PRIORITY: bool>(
    connection: Arc<QuicConnection>,
    command: QuicOpCode,
    session_id: u32,
    v4: bool,
    chunks: &mut [Bytes],
) -> Result<Bytes, QuicClientError> {
    {
        let stream_index = connection.counter.fetch_add(1, Ordering::Relaxed) % STREAM_COUNT;
        let stream_count = connection.stream_count.load(Ordering::Relaxed);

        if stream_count != 0 && stream_index >= stream_count {
            // Box the rare path to avoid increasing send_command future size
            let connection = connection.clone();
            Box::pin(async move { add_stream(connection).await }).await?;
        }
    }

    connection.last_send.store(
        connection.created.elapsed().as_millis() as u64,
        Ordering::Relaxed,
    );

    let (command_id, writer, rx) = {
        let connection_lock = connection.connection.read().await;
        if connection_lock.reader.is_empty() {
            lore_debug!("No quic stream available when sending command");
            return Err(QuicClientError::StreamOpen);
        }

        // Select stream based on priority, computed inside lock to avoid living across await points
        let reader_count = connection_lock.reader.len() as u32;
        let stream_index = select_stream(&connection, reader_count, HIGH_PRIORITY) as usize
            % connection_lock.reader.len();
        connection.stream_inflight[stream_index].fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = oneshot::channel();
        let command_id = connection_lock.reader[stream_index].wait_for(tx)?;
        let writer = connection_lock.writer[stream_index].clone();
        (command_id, writer, rx)
    };

    {
        // Skip any previous header in case this is a resend
        let total_size: usize = chunks.iter().skip(1).map(|buffer| buffer.len()).sum();
        if total_size > connection.max_chunk_size {
            lore_debug!(
                "Client '{command}' message too big - message size '{total_size}' exceeds {}",
                connection.max_chunk_size
            );
            return Err(QuicClientError::ClientMessageTooBig);
        }
        if v4 {
            let header =
                CommandHeader::new_with_session(command, command_id, total_size, session_id);
            chunks[0] = Bytes::from_owner(header.to_bytes_v4());
        } else {
            let header = CommandHeader::new(command, command_id, total_size);
            chunks[0] = Bytes::from_owner(header.to_bytes());
        }
    }

    {
        let mut stream = writer.lock().await;
        stream.write_all_chunks(chunks).await.map_err(|err| {
            if let quinn::WriteError::ConnectionLost(_) = err {
                QuicClientError::Terminated
            } else {
                lore_warn!("{}: {err}", QuicClientError::WriteChunks);
                QuicClientError::WriteChunks
            }
        })?;
    }

    rx.await.map_err(|err| {
        lore_warn!("{}: {err}", QuicClientError::Read);
        QuicClientError::Read
    })?
}
