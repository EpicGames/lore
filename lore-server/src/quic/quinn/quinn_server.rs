// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use lore_base::lore_spawn_core;
use lore_base::lore_spawn_net;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::runtime::execution_context;
use lore_transport::quic::client::RECEIVE_BUFFER_BYTES;
use lore_transport::quic::client::raise_receive_buffer;
use lore_transport::quic::net_runtime::NetRuntime;
use opentelemetry::KeyValue;
use quinn::AckFrequencyConfig;
use quinn::EndpointConfig;
use quinn::ServerConfig;
use quinn::VarInt;
use quinn::crypto::rustls::HandshakeData;
use quinn::crypto::rustls::QuicServerConfig;
use quinn_proto::ConnectionIdGenerator;
use quinn_proto::HashedConnectionIdGenerator;
use quinn_proto::InvalidCid;
use socket2::Domain;
use socket2::Protocol;
use socket2::Type;
use tokio_metrics::TaskMonitor;
use tracing::debug;
use tracing::info;
use tracing::info_span;
use tracing::trace;
use tracing::warn;

use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::ConnectionId;
use crate::quic::StreamDataHandler;
use crate::quic::StreamHandlerFactory;
use crate::quic::initial_cwnd::InitialCwndBytes;
use crate::quic::quinn::config::QuinnConfig;
use crate::quic::quinn::config::crypto_config;
use crate::quic::quinn::metrics::track_connection_stats;
use crate::telemetry::OtelTokioTaskMetrics;
use crate::util::cert_metrics::parse_certificate_info;
use crate::util::cert_metrics::start_certificate_metrics;

const METRICS_QUINN_SERVER_LABEL: &str = "quinn_server_name";

/// Connection attempts the server holds waiting to be accepted or refused, across its sockets:
/// quinn's default for one endpoint.
const MAX_INCOMING: usize = 1 << 16;

/// Bytes the server buffers for connections not yet accepted, across its sockets: quinn's default
/// for one endpoint.
const INCOMING_BUFFER_SIZE_TOTAL: u64 = 100 << 20;

pub struct QuinnServer {
    /// One per socket, all serving the same port.
    endpoints: Vec<quinn::Endpoint>,
}

impl QuinnServer {
    pub fn start(mut settings: QuinnConfig) -> anyhow::Result<Self> {
        let span = info_span!(
            "quinn_server",
            quinn_server_name = settings.server_metrics_name
        );
        let _guard = span.enter();
        info!("Initializing quinn quic server from {settings:?}");

        // Initialize certificate expiry metrics
        let cert_path = settings
            .cert_chain
            .clone()
            .unwrap_or(settings.cert_file.clone());
        if let Some(cert_info) = parse_certificate_info(&cert_path) {
            info!(
                cert_path = %cert_info.cert_path.display(),
                subject = %cert_info.subject,
                serial = %cert_info.serial,
                expiry_timestamp = cert_info.expiry_timestamp,
                "Parsed certificate for expiry metrics"
            );
            start_certificate_metrics(cert_info, settings.metrics_frequency);
        } else {
            warn!(
                cert_path = %cert_path.display(),
                "Could not parse certificate for expiry metrics, skipping"
            );
        }

        let endpoint_config = EndpointConfig::default();
        let mut server_config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(
            crypto_config(&settings)?,
        )?));

        if let Some(transport_config) = Arc::get_mut(&mut server_config.transport) {
            transport_config.max_concurrent_uni_streams(0_u8.into());
            transport_config
                .max_concurrent_bidi_streams(VarInt::from_u64(settings.max_bidi_streams)?);
            transport_config.max_idle_timeout(Some(settings.idle_timeout.try_into()?));
            transport_config.keep_alive_interval(Some(settings.keep_alive));

            let bandwidth_estimate = settings.transport_bits_per_second / 8;
            let recv_window = (bandwidth_estimate / 1000) * settings.transport_rtt;
            // Any stream can use at most 25% of available recv window
            let stream_recv_window = recv_window / 4;
            let send_window = recv_window;
            info!("QUIC stream recv window: {stream_recv_window}");
            info!("QUIC recv window: {recv_window}");
            info!("QUIC send window: {send_window}");
            transport_config
                .stream_receive_window(VarInt::from_u32(stream_recv_window as u32))
                .receive_window(VarInt::from_u32(recv_window as u32))
                .send_window(send_window as u64)
                .datagram_receive_buffer_size(None)
                .datagram_send_buffer_size(0);
            transport_config.congestion_controller_factory(
                settings.congestion_algorithm.controller_factory(
                    settings.initial_cwnd.map(InitialCwndBytes::get),
                    send_window as u64,
                ),
            );
            transport_config
                .datagram_receive_buffer_size(None)
                .datagram_send_buffer_size(0);

            let mut ack_freq_config = AckFrequencyConfig::default();
            ack_freq_config
                .reordering_threshold(VarInt::from_u32(lore_transport::quic::PACKET_THRESHOLD - 1));
            transport_config
                .packet_threshold(lore_transport::quic::PACKET_THRESHOLD)
                .time_threshold(lore_transport::quic::TIME_THRESHOLD)
                .max_rtt(Duration::from_millis(lore_transport::quic::MAX_RTT_MS))
                .ack_frequency_config(Some(ack_freq_config));
        }

        let sockets = bind_sockets(&mut settings)?;
        split_incoming_limits(&mut server_config, sockets.len());
        let connection_id_keys: Arc<[u64]> = sockets.iter().map(|_| rand::random()).collect();

        // `NetRuntime` places every task quinn spawns on net, including the connection driver
        // created when an `Incoming` is awaited. The guard covers what it cannot: the socket is
        // handed to `tokio::net::UdpSocket::from_std`, which registers with whichever reactor is
        // current here. Removing it would register net's socket on core's reactor.
        let endpoints = {
            let _guard = lore_base::runtime::net_runtime().enter();
            sockets
                .into_iter()
                .zip(connection_id_keys.iter())
                .map(|(socket, &key)| {
                    raise_receive_buffer(&socket, RECEIVE_BUFFER_BYTES);
                    quinn::Endpoint::new(
                        socket_endpoint_config(&endpoint_config, key, &connection_id_keys),
                        Some(server_config.clone()),
                        socket,
                        Arc::new(NetRuntime),
                    )
                })
                .collect::<std::io::Result<Vec<_>>>()?
        };

        run_loop(&endpoints, settings)?;

        Ok(Self { endpoints })
    }

    /// Gracefully close all connections on every endpoint. Sends `CONNECTION_CLOSE`
    /// frames to all peers (causing accept loops to return `None`) and waits for
    /// the frames to be delivered.
    pub async fn close(&self) {
        for endpoint in &self.endpoints {
            endpoint.close(VarInt::from_u32(0), b"server shutting down");
        }
        for endpoint in &self.endpoints {
            endpoint.wait_idle().await;
        }
    }
}

/// `shared`, whose stateless reset key every endpoint on the port holds, for the endpoint issuing
/// connection IDs under `key` and recognising those issued under any of `keys`; see
/// [`SocketConnectionIdGenerator`].
fn socket_endpoint_config(shared: &EndpointConfig, key: u64, keys: &Arc<[u64]>) -> EndpointConfig {
    let keys = keys.clone();
    let mut config = shared.clone();
    config.cid_generator(move || Box::new(SocketConnectionIdGenerator::new(key, keys.clone())));
    config
}

/// Issues quinn's hashed connection IDs under its socket's key and recognises those issued under
/// the key of any socket on the port. The keys differ, so sockets, whose endpoints track their IDs
/// apart, do not issue the same ID. Recognising a sibling's ID lets an endpoint answer a datagram
/// that reached the wrong socket, as after the client's address changes, with a stateless reset the
/// client accepts rather than drop it.
#[lore_macro::test_pub]
struct SocketConnectionIdGenerator {
    issuer: HashedConnectionIdGenerator,
    keys: Arc<[u64]>,
}

impl SocketConnectionIdGenerator {
    /// Issues IDs under `key` and recognises those issued under any of `keys`.
    #[lore_macro::test_pub]
    fn new(key: u64, keys: Arc<[u64]>) -> Self {
        Self {
            issuer: HashedConnectionIdGenerator::from_key(key),
            keys,
        }
    }
}

impl ConnectionIdGenerator for SocketConnectionIdGenerator {
    fn generate_cid(&mut self) -> quinn_proto::ConnectionId {
        self.issuer.generate_cid()
    }

    fn validate(&self, cid: &quinn_proto::ConnectionId) -> Result<(), InvalidCid> {
        if self.keys.iter().any(|&key| {
            HashedConnectionIdGenerator::from_key(key)
                .validate(cid)
                .is_ok()
        }) {
            Ok(())
        } else {
            Err(InvalidCid)
        }
    }

    fn cid_len(&self) -> usize {
        self.issuer.cid_len()
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        self.issuer.cid_lifetime()
    }
}

/// Splits quinn's per-endpoint limits on connections not yet accepted across `endpoints`, so the
/// server as a whole holds no more of them than one endpoint with quinn's defaults.
fn split_incoming_limits(server_config: &mut ServerConfig, endpoints: usize) {
    server_config
        .max_incoming(MAX_INCOMING / endpoints)
        .incoming_buffer_size_total(INCOMING_BUFFER_SIZE_TOTAL / endpoints as u64);
}

/// The UDP sockets to serve on: the one the caller bound, or `num_sockets` bound to `address`
/// with `SO_REUSEPORT`, of which the first settles the port when `address` asks for any free one.
/// Only Linux spreads the datagrams arriving at a port across sockets, so other platforms bind one.
fn bind_sockets(settings: &mut QuinnConfig) -> anyhow::Result<Vec<std::net::UdpSocket>> {
    if let Some(socket) = settings.socket.take() {
        return Ok(vec![socket]);
    }

    let count = if cfg!(any(target_os = "linux", target_os = "android")) {
        settings.num_sockets.get()
    } else {
        1
    };
    if count < settings.num_sockets.get() {
        debug!(
            num_sockets = settings.num_sockets.get(),
            "Binding one socket: this platform does not spread datagrams across sockets"
        );
    }

    let mut address = settings.address;
    let mut sockets = Vec::with_capacity(count);
    for _ in 0..count {
        let socket = bind_socket(address)?;
        address = socket.local_addr()?;
        sockets.push(socket);
    }
    Ok(sockets)
}

/// Binds a UDP socket to `address` with `SO_REUSEPORT`, so that sockets bound to its port later
/// share it. Port 0 is bound before the option is set, as Linux otherwise may pick a port another
/// socket of the same user holds with the option and join it.
fn bind_socket(address: SocketAddr) -> anyhow::Result<std::net::UdpSocket> {
    let socket = socket2::Socket::new(
        Domain::for_address(address),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;

    // set_reuse_port is not implemented on Windows
    #[cfg(target_family = "unix")]
    if address.port() != 0 {
        socket.set_reuse_port(true)?;
    }
    socket.bind(&socket2::SockAddr::from(address))?;
    #[cfg(target_family = "unix")]
    socket.set_reuse_port(true)?;

    Ok(socket.into())
}

fn run_loop(endpoints: &[quinn::Endpoint], settings: QuinnConfig) -> anyhow::Result<()> {
    let stream_handler_factory = Arc::new(settings.stream_handler_factory);
    let metrics_interval = settings.metrics_frequency;

    let monitor = TaskMonitor::new();
    let otel_bridge = OtelTokioTaskMetrics::new(
        &lore_telemetry::meter("quicsrv_tokio_task"),
        vec![KeyValue::new(
            METRICS_QUINN_SERVER_LABEL,
            settings.server_metrics_name,
        )],
    );

    {
        // The `instrument` function from `tracing` conflicts with the `instrument` function from
        // the `TaskMonitor` so we need to use a scoped import.
        use tracing::instrument::Instrument;
        let monitor = monitor.clone();
        lore_spawn_core!(
            LORE_CONTEXT.scope(
                execution_context(),
                async move {
                    for task_metrics in monitor.intervals() {
                        otel_bridge.record(task_metrics);
                        tokio::time::sleep(metrics_interval).await;
                    }
                }
                .in_current_span(),
            )
        );
    }

    info!(
        sockets = endpoints.len(),
        listeners_per_socket = settings.num_listeners.get(),
        "Creating listeners"
    );
    for (socket, endpoint) in endpoints.iter().enumerate() {
        for listener in 0..settings.num_listeners.get() {
            spawn_listener(
                endpoint.clone(),
                socket,
                listener,
                monitor.clone(),
                metrics_interval,
                stream_handler_factory.clone(),
            );
        }
    }

    Ok(())
}

/// Accepts connections on `endpoint` until it closes, each handled by a task of its own.
fn spawn_listener(
    endpoint: quinn::Endpoint,
    socket: usize,
    listener: u8,
    monitor: TaskMonitor,
    metrics_interval: Duration,
    stream_handler_factory: Arc<Box<dyn StreamHandlerFactory>>,
) {
    // The `instrument` function from `tracing` conflicts with the `instrument` function from
    // the `TaskMonitor` so we need to use a scoped import.
    use tracing::instrument::Instrument;

    lore_spawn_net!(
        LORE_CONTEXT.scope(
            execution_context(),
            async move {
                debug!(socket, listener, "Waiting to accept connections");
                while let Some(connection) = endpoint.accept().await {
                    debug!(socket, listener, "Received a connection: {connection:?}");

                    let monitor = monitor.clone();
                    let stream_handler_factory = stream_handler_factory.clone();

                    lore_spawn_net!(
                        LORE_CONTEXT.scope(
                            execution_context(),
                            async move {
                                if let Err(error) = handle_conn(
                                    connection,
                                    monitor,
                                    metrics_interval,
                                    stream_handler_factory,
                                )
                                .await
                                {
                                    warn!("Error handling connection: {error}");
                                }
                            }
                            .in_current_span(),
                        )
                    );
                }
            }
            .in_current_span(),
        )
    );
}

fn get_protocol(connection: &quinn::Connection) -> Result<String, anyhow::Error> {
    let handshake_data = connection
        .handshake_data()
        .and_then(|h| h.downcast::<HandshakeData>().ok())
        .ok_or(anyhow!("No handshake data"))?;

    handshake_data
        .protocol
        .map(String::from_utf8)
        .transpose()
        .map_err(|e| anyhow!("Failed to decode protocol: {e:?}"))?
        .ok_or(anyhow!("No protocol found on request"))
}

#[tracing::instrument(
    name = "urc-quic",
    skip_all,
    fields(connection_id, protocol, correlation_id, repository_id)
)]
async fn handle_conn(
    conn: quinn::Incoming,
    monitor: TaskMonitor,
    connection_metrics_interval: Duration,
    stream_handler_factory: Arc<Box<dyn StreamHandlerFactory>>,
) -> anyhow::Result<()> {
    let connection = conn.await?;

    let protocol = get_protocol(&connection)?;

    let connection_id = connection.stable_id();

    let conn_span = tracing::Span::current();
    conn_span.record("connection_id", connection_id);
    conn_span.record("protocol", &protocol);

    info!(
        remote_address = %connection.remote_address(),
        "Established connection",
    );

    let Some((service_name, service_builder)) =
        stream_handler_factory.get_stream_handler_builder(&protocol)
    else {
        warn!("Protocol {protocol} is not supported by the stream handler factory");
        return Err(anyhow!(
            "Received connection for unsupported protocol: {protocol:?}"
        ));
    };

    let context = Arc::new(AttributeMap::default());
    context.insert(conn_span.clone());
    context.insert(ConnectionId(connection_id));

    let _stats_guard = track_connection_stats(
        service_name,
        &connection,
        context.clone(),
        connection_metrics_interval,
    );

    // Create the stream handler once per connection so per-connection state
    // (e.g. SessionMap in StorageServiceV4) is shared across all streams.
    let connection_handler: Arc<Box<dyn StreamDataHandler>> =
        Arc::new(service_builder(context.clone()));

    // Keeps accepting requests until the connection closes or errors
    loop {
        trace!("Waiting for stream on connection id: {connection_id}");
        let stream = connection.accept_bi().await;

        let (send, mut recv) = match stream {
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                info!("Connection closed");
                return Ok(());
            }
            Err(e) => {
                return Err(anyhow!(e));
            }
            Ok(s) => {
                debug!(
                    stream_id = s.0.id().to_string(),
                    "Stream opened for connection"
                );
                s
            }
        };

        trace!("Spawning task to handle request");

        let execution = execution_context();

        // The `instrument` function from `tracing` conflicts with the `instrument` function from
        // the `TaskMonitor` so we need to use a scoped import.
        let handle_future = {
            use tracing::instrument::Instrument;

            let handler = connection_handler.clone();

            async move {
                // Get the request off the transport
                match handler.handle_stream(&mut recv, send).await {
                    Ok(_) => {
                        debug!("Successfully handled request for connection");
                    }
                    Err(e) => {
                        warn!("Error handling request: {e:?}");

                        if let Err(e) = handler.close(&mut recv, Some(u32::MAX)).await {
                            warn!("Error closing transport: {}", e);
                        }
                    }
                }
            }
            .instrument(info_span!("handle_stream"))
        };

        // Transport read only: `handle_stream` lifts the request off the wire and
        // hands processing to core from inside the stream handler.
        lore_spawn_net!(monitor.instrument(LORE_CONTEXT.scope(execution.clone(), handle_future)));
    }
}

#[cfg(feature = "test-util")]
pub mod test_util {
    use std::net::SocketAddr;

    use super::QuinnServer;

    impl QuinnServer {
        /// The local address of each socket the server serves on.
        pub fn socket_addrs(&self) -> std::io::Result<Vec<SocketAddr>> {
            self.endpoints
                .iter()
                .map(|endpoint| endpoint.local_addr())
                .collect()
        }

        /// The handshakes accepted on each socket the server serves on.
        pub fn socket_handshakes(&self) -> Vec<u64> {
            self.endpoints
                .iter()
                .map(|endpoint| endpoint.stats().accepted_handshakes)
                .collect()
        }
    }
}
