// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use lore_base::lore_spawn_core;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use quinn::Connection;
use tokio::select;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::debug;
use tracing::warn;

use crate::protocol::attribute_map::AttributeMap;
use crate::quic::CWND_BYTES_BUCKETS;
use crate::quic::RTT_MS_BUCKETS;
use crate::quic::SERVICE_LABEL_KEY;

const DIRECTION_LABEL_KEY: &str = "direction";

// Connection duration buckets from 1 second to 30 days in a 1-2-5 pattern.
const DURATION_BUCKETS: &[f64] = &[
    1., 2., 5., 10., 20., 30., 60., 120., 300., 600., 1_200., 1_800., 3_600., 7_200., 14_400.,
    28_800., 43_200., 86_400., 172_800., 259_200., 604_800., 1_209_600., 1_814_400., 2_592_000.,
];

// Packets a single connection carried, from one to a hundred million.
const PACKET_BUCKETS: &[f64] = &[
    1.,
    10.,
    100.,
    1_000.,
    10_000.,
    100_000.,
    1_000_000.,
    10_000_000.,
    100_000_000.,
];

// Bytes a single connection carried, from one kilobyte to a hundred gigabytes.
const BYTE_BUCKETS: &[f64] = &[
    1_024.,
    65_536.,
    1_048_576.,
    16_777_216.,
    268_435_456.,
    4_294_967_296.,
    107_374_182_400.,
];

// Frames of one type a single connection carried, from one to a million.
const FRAME_BUCKETS: &[f64] = &[1., 10., 100., 1_000., 10_000., 100_000., 1_000_000.];

struct ConnectionMetricsInstrumentProvider;

impl InstrumentProvider for ConnectionMetricsInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.quinn"
    }
}

/// What a connection reports about itself.
///
/// Distributions, labelled by the service and the client rather than by the connection. The
/// statistics describe one connection each, so reporting them as a last value under labels several
/// connections share means a collection sees whichever reported most recently. A distribution
/// answers what is asked of these instead — how far away the clients are, how much they lose, how
/// much they move — for one client or for all of them, and needs nothing naming a connection to
/// tell connections apart.
struct QuinnConnectionInstruments {
    // sampled while a connection is open, so the distribution covers its life
    rtt: Histogram<f64>,
    congestion_window: Histogram<u64>,

    // observed once, when a connection ends, being totals it reached rather than a current value
    duration: Histogram<u64>,
    lost_packets: Histogram<u64>,
    sent_packets: Histogram<u64>,
    udp_bytes: Histogram<u64>,
    data_blocked: Histogram<u64>,
    max_streams_bidi: Histogram<u64>,
    max_data: Histogram<u64>,
    stream_data_blocked: Histogram<u64>,
    streams_blocked_bidi: Histogram<u64>,
}

impl QuinnConnectionInstruments {
    fn new() -> Self {
        let provider = ConnectionMetricsInstrumentProvider;

        Self {
            rtt: provider
                .meter()
                .f64_histogram(provider.scope_name("connection.path.rtt"))
                .with_unit("milliseconds")
                .with_boundaries(RTT_MS_BUCKETS.to_vec())
                .build(),
            congestion_window: provider
                .meter()
                .u64_histogram(provider.scope_name("connection.path.cwnd"))
                .with_unit("bytes")
                .with_boundaries(CWND_BYTES_BUCKETS.to_vec())
                .build(),

            duration: provider
                .length_histogram("connection.duration_seconds", DURATION_BUCKETS.to_vec()),
            lost_packets: provider
                .length_histogram("connection.path.lost_packets", PACKET_BUCKETS.to_vec()),
            sent_packets: provider
                .length_histogram("connection.path.sent_packets", PACKET_BUCKETS.to_vec()),
            udp_bytes: provider.length_histogram("connection.udp.bytes", BYTE_BUCKETS.to_vec()),
            data_blocked: provider
                .length_histogram("connection.frame.data_blocked", FRAME_BUCKETS.to_vec()),
            max_streams_bidi: provider
                .length_histogram("connection.frame.max_streams_bidi", FRAME_BUCKETS.to_vec()),
            max_data: provider
                .length_histogram("connection.frame.max_data", FRAME_BUCKETS.to_vec()),
            stream_data_blocked: provider.length_histogram(
                "connection.frame.stream_data_blocked",
                FRAME_BUCKETS.to_vec(),
            ),
            streams_blocked_bidi: provider.length_histogram(
                "connection.frame.streams_blocked_bidi",
                FRAME_BUCKETS.to_vec(),
            ),
        }
    }

    pub fn instance() -> &'static QuinnConnectionInstruments {
        static INSTANCE: OnceLock<QuinnConnectionInstruments> = OnceLock::new();
        INSTANCE.get_or_init(QuinnConnectionInstruments::new)
    }
}

/// Labels one connection's statistics are recorded under.
///
/// The service is fixed for the connection's lifetime. The client is not: it announces itself in a
/// message, so it arrives after the connection is established, which is why these are built from
/// the connection's attributes each time rather than held.
struct ConnectionLabels {
    service: KeyValue,
    user_agent: KeyValue,
}

impl ConnectionLabels {
    fn new(service_name: &'static str, context: &AttributeMap) -> Self {
        Self {
            service: KeyValue::new(SERVICE_LABEL_KEY, service_name),
            user_agent: context.user_agent_label(),
        }
    }

    /// Labels for what describes the connection as a whole.
    fn undirected(&self) -> [KeyValue; 2] {
        [self.service.clone(), self.user_agent.clone()]
    }

    /// Labels for what is reported separately for what was sent and what was received.
    fn directed(&self, direction: &'static str) -> [KeyValue; 3] {
        [
            self.service.clone(),
            self.user_agent.clone(),
            KeyValue::new(DIRECTION_LABEL_KEY, direction),
        ]
    }
}

pub(crate) fn track_connection_stats<'a>(
    service_name: &'static str,
    connection: &'a Connection,
    context: Arc<AttributeMap>,
    interval: Duration,
) -> ConnectionMetricsGuard<'a> {
    let mut guard = ConnectionMetricsGuard {
        service_name,
        connection,
        context,
        task_handle: None,
        established_at: Instant::now(),
    };

    guard.start(interval);

    guard
}

pub(crate) struct ConnectionMetricsGuard<'a> {
    service_name: &'static str,
    connection: &'a Connection,
    context: Arc<AttributeMap>,
    task_handle: Option<JoinHandle<()>>,
    established_at: Instant,
}

/// Samples what the connection's path looks like at this moment.
fn sample_path(connection: &Connection, labels: &ConnectionLabels) {
    let path = connection.stats().path;
    let instruments = QuinnConnectionInstruments::instance();
    let labels = labels.undirected();

    instruments
        .rtt
        .record(path.rtt.as_secs_f64() * 1000.0, &labels);
    instruments.congestion_window.record(path.cwnd, &labels);
}

/// Records what a connection reached over its life, once it has ended.
///
/// Quinn holds these as totals for the connection, so one observation of each describes that
/// connection and the distribution describes the connections a service served.
fn record_totals(connection: &Connection, labels: &ConnectionLabels, elapsed: Duration) {
    let stats = connection.stats();

    debug!(
        "Emitting metrics for connection stats: {stats:?} for connection: {}",
        connection.stable_id()
    );

    let instruments = QuinnConnectionInstruments::instance();
    let undirected = labels.undirected();

    instruments.duration.record(elapsed.as_secs(), &undirected);
    instruments
        .lost_packets
        .record(stats.path.lost_packets, &undirected);
    // Recorded alongside the packets lost, which is only interpretable against the packets sent.
    instruments
        .sent_packets
        .record(stats.path.sent_packets, &undirected);

    let directions = [
        (stats.frame_tx, stats.udp_tx, "tx"),
        (stats.frame_rx, stats.udp_rx, "rx"),
    ];

    // The direction says who sent a frame, not whose flow control it describes. DATA_BLOCKED is
    // sent by whoever is blocked, so tx means this server was; MAX_DATA is sent by whoever grants
    // credit, so tx means this server granted it to the client. Pairing a stall with the credit
    // that relieves it therefore reads blocked frames sent against credit frames received. The
    // stream counts invert the same way.
    for (frames, udp, direction) in directions {
        let labels = labels.directed(direction);
        instruments.udp_bytes.record(udp.bytes, &labels);
        instruments
            .data_blocked
            .record(frames.data_blocked, &labels);
        instruments.max_data.record(frames.max_data, &labels);
        instruments
            .stream_data_blocked
            .record(frames.stream_data_blocked, &labels);
        instruments
            .streams_blocked_bidi
            .record(frames.streams_blocked_bidi, &labels);
        instruments
            .max_streams_bidi
            .record(frames.max_streams_bidi, &labels);
    }
}

impl ConnectionMetricsGuard<'_> {
    /// Starts the per-connection path sampler on core.
    ///
    /// Pinned rather than following the caller, which is a net task: this is telemetry on a timer,
    /// and there is one of these per live connection, so net stays for driving sockets.
    fn start(&mut self, interval: Duration) {
        if self.task_handle.is_some() {
            warn!("Attempted to start connection stats tracking, but task handle was already set");
            return;
        }

        let connection = self.connection.clone();
        let service_name = self.service_name;
        let context = self.context.clone();

        self.task_handle = Some(lore_spawn_core!(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                select! {
                    _ = ticker.tick() => {
                        let labels = ConnectionLabels::new(service_name, &context);
                        sample_path(&connection, &labels);
                    }
                    e = connection.closed() => {
                        debug!("Connection closed with: {e:?}, exiting metrics loop");
                        break;
                    }
                }
            }
        }));
    }
}

impl Drop for ConnectionMetricsGuard<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            debug!(
                "Shutting down metrics task for connection: {}",
                self.connection.stable_id()
            );
            handle.abort();

            // Built here rather than shared with the sampler: this runs once, when the client is
            // whatever the connection settled on.
            let labels = ConnectionLabels::new(self.service_name, &self.context);
            record_totals(self.connection, &labels, self.established_at.elapsed());
        }
    }
}
