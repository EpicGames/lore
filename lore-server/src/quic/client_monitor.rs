// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::OnceLock;

use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME;
use lore_transport::quic::client::ConnectionStats;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;

use crate::quic::CWND_BYTES_BUCKETS;
use crate::quic::RTT_MS_BUCKETS;

pub fn default_quic_client_monitor_interval_secs() -> u64 {
    10
}

struct ClientInstruments {
    /// Current smoothed `RTT` estimate from the congestion controller (ms).
    /// This is the single most important metric — lets us see what `BBR` thinks the
    /// `RTT` is and whether it diverges from the expected cross-region latency.
    pub rtt_ms: Histogram<f64>,

    /// Current congestion window in bytes.
    /// During bursts after idle, a small `cwnd` forces pacing delays — this is the
    /// leading indicator of the latency spikes we observed.
    pub cwnd_bytes: Histogram<u64>,

    /// Cumulative congestion events on the path.
    /// Each event means `BBR` detected congestion and reduced its sending rate.
    pub congestion_events: Gauge<u64>,

    /// Cumulative packets lost on the path.
    /// Packet loss directly causes retransmission delays (at least 1 `RTT` per loss).
    pub lost_packets: Gauge<u64>,

    /// Cumulative bytes lost on the path.
    pub lost_bytes: Gauge<u64>,

    /// Cumulative packets sent on the path.
    /// Together with `lost_packets`, gives loss rate.
    pub sent_packets: Gauge<u64>,

    /// Number of black holes detected on the path.
    /// A black hole means packets are being silently dropped (e.g. `MTU` issue).
    pub black_holes_detected: Gauge<u64>,

    /// Current path `MTU` in bytes.
    pub current_mtu: Gauge<u64>,

    // -- UdpStats: volume and I/O efficiency --
    /// Cumulative UDP datagrams sent.
    pub udp_tx_datagrams: Gauge<u64>,

    /// Cumulative bytes sent in UDP datagrams.
    pub udp_tx_bytes: Gauge<u64>,

    /// Cumulative send I/O operations (may be less than datagrams with `GSO`).
    pub udp_tx_ios: Gauge<u64>,

    /// Cumulative UDP datagrams received.
    pub udp_rx_datagrams: Gauge<u64>,

    /// Cumulative bytes received in UDP datagrams.
    pub udp_rx_bytes: Gauge<u64>,

    /// Cumulative receive I/O operations (may be less than datagrams with `GRO`).
    pub udp_rx_ios: Gauge<u64>,

    // -- FrameStats (tx): sender-side flow control signals --
    /// `DATA_BLOCKED` frames sent — connection-level flow control is constraining us.
    pub frame_tx_data_blocked: Gauge<u64>,

    /// `STREAM_DATA_BLOCKED` frames sent — per-stream flow control is constraining us.
    pub frame_tx_stream_data_blocked: Gauge<u64>,

    /// `STREAMS_BLOCKED` (bidi) frames sent — we need more bidirectional streams than the peer allows.
    pub frame_tx_streams_blocked_bidi: Gauge<u64>,

    /// `STREAM` frames sent — each carries application data.
    pub frame_tx_stream: Gauge<u64>,

    /// `RESET_STREAM` frames sent — streams being aborted.
    pub frame_tx_reset_stream: Gauge<u64>,

    // -- FrameStats (rx): peer-side signals received --
    /// `DATA_BLOCKED` frames received — the peer's connection-level flow control hit us.
    pub frame_rx_data_blocked: Gauge<u64>,

    /// `STREAM_DATA_BLOCKED` frames received — the peer's per-stream flow control hit us.
    pub frame_rx_stream_data_blocked: Gauge<u64>,

    /// `MAX_DATA` frames received — the peer is raising our connection-level send allowance.
    pub frame_rx_max_data: Gauge<u64>,

    /// `MAX_STREAM_DATA` frames received — the peer is raising a per-stream send allowance.
    pub frame_rx_max_stream_data: Gauge<u64>,
}

fn instruments() -> &'static ClientInstruments {
    static INSTANCE: OnceLock<ClientInstruments> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let provider = ClientMonitorProvider {};
        ClientInstruments::new(provider)
    })
}

impl ClientInstruments {
    pub fn new(p: ClientMonitorProvider) -> Self {
        Self {
            // PathStats
            rtt_ms: p
                .meter()
                .f64_histogram(p.scope_name("connection.path.rtt"))
                .with_unit("milliseconds")
                .with_boundaries(RTT_MS_BUCKETS.to_vec())
                .build(),
            cwnd_bytes: p
                .meter()
                .u64_histogram(p.scope_name("connection.path.cwnd"))
                .with_unit("bytes")
                .with_boundaries(CWND_BYTES_BUCKETS.to_vec())
                .build(),
            congestion_events: p.gauge("connection.path.congestion_events"),
            lost_packets: p.gauge("connection.path.lost_packets"),
            lost_bytes: p.gauge("connection.path.lost_bytes"),
            sent_packets: p.gauge("connection.path.sent_packets"),
            black_holes_detected: p.gauge("connection.path.black_holes_detected"),
            current_mtu: p.gauge("connection.path.current_mtu"),

            // UdpStats
            udp_tx_datagrams: p.gauge("connection.udp.tx.datagrams"),
            udp_tx_bytes: p.gauge("connection.udp.tx.bytes"),
            udp_tx_ios: p.gauge("connection.udp.tx.ios"),
            udp_rx_datagrams: p.gauge("connection.udp.rx.datagrams"),
            udp_rx_bytes: p.gauge("connection.udp.rx.bytes"),
            udp_rx_ios: p.gauge("connection.udp.rx.ios"),

            // FrameStats (tx)
            frame_tx_data_blocked: p.gauge("connection.frame.tx.data_blocked"),
            frame_tx_stream_data_blocked: p.gauge("connection.frame.tx.stream_data_blocked"),
            frame_tx_streams_blocked_bidi: p.gauge("connection.frame.tx.streams_blocked_bidi"),
            frame_tx_stream: p.gauge("connection.frame.tx.stream"),
            frame_tx_reset_stream: p.gauge("connection.frame.tx.reset_stream"),

            // FrameStats (rx)
            frame_rx_data_blocked: p.gauge("connection.frame.rx.data_blocked"),
            frame_rx_stream_data_blocked: p.gauge("connection.frame.rx.stream_data_blocked"),
            frame_rx_max_data: p.gauge("connection.frame.rx.max_data"),
            frame_rx_max_stream_data: p.gauge("connection.frame.rx.max_stream_data"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ClientMetrics {
    labels: Vec<KeyValue>,
}

impl ClientMetrics {
    pub fn new(context: &'static str, mut labels: Vec<KeyValue>) -> Self {
        labels.push(KeyValue::new(
            METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME,
            context,
        ));

        Self { labels }
    }

    pub fn observe(&self, stats: &ConnectionStats) {
        let instruments = instruments();
        // PathStats — point-in-time snapshots
        instruments
            .rtt_ms
            .record(stats.path.rtt.as_secs_f64() * 1000.0, &self.labels);
        instruments.cwnd_bytes.record(stats.path.cwnd, &self.labels);
        instruments
            .current_mtu
            .record(stats.path.current_mtu as u64, &self.labels);

        // PathStats — cumulative values (reset when connection is replaced)
        instruments
            .congestion_events
            .record(stats.path.congestion_events, &self.labels);
        instruments
            .lost_packets
            .record(stats.path.lost_packets, &self.labels);
        instruments
            .lost_bytes
            .record(stats.path.lost_bytes, &self.labels);
        instruments
            .sent_packets
            .record(stats.path.sent_packets, &self.labels);
        instruments
            .black_holes_detected
            .record(stats.path.black_holes_detected, &self.labels);

        // UdpStats
        instruments
            .udp_tx_datagrams
            .record(stats.udp_tx.datagrams, &self.labels);
        instruments
            .udp_tx_bytes
            .record(stats.udp_tx.bytes, &self.labels);
        instruments
            .udp_tx_ios
            .record(stats.udp_tx.ios, &self.labels);
        instruments
            .udp_rx_datagrams
            .record(stats.udp_rx.datagrams, &self.labels);
        instruments
            .udp_rx_bytes
            .record(stats.udp_rx.bytes, &self.labels);
        instruments
            .udp_rx_ios
            .record(stats.udp_rx.ios, &self.labels);

        // FrameStats (tx)
        instruments
            .frame_tx_data_blocked
            .record(stats.frame_tx.data_blocked, &self.labels);
        instruments
            .frame_tx_stream_data_blocked
            .record(stats.frame_tx.stream_data_blocked, &self.labels);
        instruments
            .frame_tx_streams_blocked_bidi
            .record(stats.frame_tx.streams_blocked_bidi, &self.labels);
        instruments
            .frame_tx_stream
            .record(stats.frame_tx.stream, &self.labels);
        instruments
            .frame_tx_reset_stream
            .record(stats.frame_tx.reset_stream, &self.labels);

        // FrameStats (rx)
        instruments
            .frame_rx_data_blocked
            .record(stats.frame_rx.data_blocked, &self.labels);
        instruments
            .frame_rx_stream_data_blocked
            .record(stats.frame_rx.stream_data_blocked, &self.labels);
        instruments
            .frame_rx_max_data
            .record(stats.frame_rx.max_data, &self.labels);
        instruments
            .frame_rx_max_stream_data
            .record(stats.frame_rx.max_stream_data, &self.labels);
    }
}

#[derive(Clone, Default)]
struct ClientMonitorProvider;

impl InstrumentProvider for ClientMonitorProvider {
    fn namespace(&self) -> &'static str {
        "urc.quic.client_monitor"
    }
}
