// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_revision::runtime::execution_context;
use opentelemetry::KeyValue;
use quinn::AckFrequencyConfig;
use quinn::EndpointConfig;
use quinn::ServerConfig;
use quinn::TokioRuntime;
use quinn::VarInt;
use quinn::congestion;
use quinn::crypto::rustls::HandshakeData;
use quinn::crypto::rustls::QuicServerConfig;
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
use crate::quic::quinn::config::QuinnConfig;
use crate::quic::quinn::config::crypto_config;
use crate::quic::quinn::metrics::track_connection_stats;
use crate::telemetry::OtelTokioTaskMetrics;
use crate::util::cert_metrics::parse_certificate_info;
use crate::util::cert_metrics::start_certificate_metrics;

const METRICS_QUINN_SERVER_LABEL: &str = "quinn_server_name";

pub struct QuinnServer {
    endpoint: quinn::Endpoint,
}

impl QuinnServer {
    pub fn start(settings: QuinnConfig) -> anyhow::Result<Self> {
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
            transport_config
                .congestion_controller_factory(Arc::new(congestion::BbrConfig::default()));
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

        let socket = socket2::Socket::new(
            Domain::for_address(settings.address),
            Type::DGRAM,
            Some(Protocol::UDP),
        )?;

        // Reuse must be configured before bind.
        socket.reuse_address()?;

        // set_reuse_port is not implemented on Windows
        #[cfg(target_family = "unix")]
        socket.set_reuse_port(true)?;

        socket.bind(&socket2::SockAddr::from(settings.address))?;

        let endpoint = quinn::Endpoint::new(
            endpoint_config,
            Some(server_config),
            socket.into(),
            Arc::new(TokioRuntime),
        )?;

        run_loop(endpoint.clone(), settings)?;

        Ok(Self { endpoint })
    }

    /// Gracefully close all connections on this endpoint. Sends `CONNECTION_CLOSE`
    /// frames to all peers (causing accept loops to return `None`) and waits for
    /// the frames to be delivered.
    pub async fn close(&self) {
        self.endpoint
            .close(VarInt::from_u32(0), b"server shutting down");
        self.endpoint.wait_idle().await;
    }
}

fn run_loop(endpoint: quinn::Endpoint, settings: QuinnConfig) -> anyhow::Result<()> {
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
        runtime().spawn(
            LORE_CONTEXT.scope(
                execution_context(),
                async move {
                    for task_metrics in monitor.intervals() {
                        otel_bridge.record(task_metrics);
                        tokio::time::sleep(metrics_interval).await;
                    }
                }
                .in_current_span(),
            ),
        );
    }

    info!("Creating {} listener(s)", &settings.num_listeners);
    for i in 0..settings.num_listeners {
        // The `instrument` function from `tracing` conflicts with the `instrument` function from
        // the `TaskMonitor` so we need to use a scoped import.
        use tracing::instrument::Instrument;
        let endpoint = endpoint.clone();
        let monitor = monitor.clone();
        let stream_handler_factory = stream_handler_factory.clone();

        runtime().spawn(
            LORE_CONTEXT.scope(
                execution_context(),
                async move {
                    debug!("Spawned task {i} is waiting to accept connections");
                    while let Some(connection) = endpoint.accept().await {
                        debug!("Spawned task {i} received a connection: {connection:?}");

                        let monitor = monitor.clone();
                        let stream_handler_factory = stream_handler_factory.clone();

                        runtime().spawn(
                            LORE_CONTEXT.scope(
                                execution_context(),
                                async move {
                                    if let Err(error) = handle_conn(
                                        connection,
                                        monitor.clone(),
                                        metrics_interval,
                                        stream_handler_factory.clone(),
                                    )
                                    .await
                                    {
                                        warn!("Error handling connection: {error}");
                                    }
                                }
                                .in_current_span(),
                            ),
                        );
                    }
                }
                .in_current_span(),
            ),
        );
    }

    Ok(())
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

    let _stats_guard =
        track_connection_stats(service_name, &connection, connection_metrics_interval);

    let context = Arc::new(AttributeMap::default());
    context.insert(conn_span.clone());
    context.insert(ConnectionId(connection_id));

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

        runtime().spawn(monitor.instrument(LORE_CONTEXT.scope(execution.clone(), handle_future)));
    }
}
