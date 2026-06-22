// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::slice;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;
use quinn::Connection;
use tokio::select;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::debug;
use tracing::warn;

// Connection duration buckets from 1 second to 30 days in a 1-2-5 pattern.
const DURATION_BUCKETS: &[f64] = &[
    1., 2., 5., 10., 20., 30., 60., 120., 300., 600., 1_200., 1_800., 3_600., 7_200., 14_400.,
    28_800., 43_200., 86_400., 172_800., 259_200., 604_800., 1_209_600., 1_814_400., 2_592_000.,
];

struct ConnectionMetricsInstrumentProvider;

impl InstrumentProvider for ConnectionMetricsInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.quinn"
    }
}

struct QuinnConnectionInstruments {
    data_blocked: Gauge<u64>,
    max_bidi_streams: Gauge<u64>,
    stream_data_blocked: Gauge<u64>,
    streams_blocked_bidi: Gauge<u64>,
    duration: Histogram<u64>,
}

impl QuinnConnectionInstruments {
    fn new() -> Self {
        let instrument_provider = ConnectionMetricsInstrumentProvider;

        Self {
            data_blocked: instrument_provider.gauge("connection.data_blocked"),
            max_bidi_streams: instrument_provider.gauge("connection.max_bidi_streams"),
            stream_data_blocked: instrument_provider.gauge("connection.stream_data_blocked"),
            streams_blocked_bidi: instrument_provider.gauge("connection.streams_blocked_bidi"),
            duration: instrument_provider
                .length_histogram("connection.duration_seconds", DURATION_BUCKETS.to_vec()),
        }
    }

    pub fn instance() -> &'static Self {
        static INSTANCE: OnceLock<QuinnConnectionInstruments> = OnceLock::new();
        INSTANCE.get_or_init(Self::new)
    }
}

pub(crate) fn track_connection_stats<'a>(
    service_name: &'static str,
    connection: &'a Connection,
    interval: Duration,
) -> ConnectionMetricsGuard<'a> {
    let mut guard = ConnectionMetricsGuard {
        service_name,
        connection,
        task_handle: None,
        established_at: Instant::now(),
    };

    guard.start(interval);

    guard
}

pub(crate) struct ConnectionMetricsGuard<'a> {
    service_name: &'static str,
    connection: &'a Connection,
    task_handle: Option<JoinHandle<()>>,
    established_at: Instant,
}

fn record_stats(service_name: &'static str, connection: &Connection, elapsed: Duration) {
    let stats = connection.stats();

    debug!(
        "Emitting metrics for connection stats: {stats:?} for connection: {}",
        connection.stable_id()
    );

    let instruments = QuinnConnectionInstruments::instance();
    let protocol_label = KeyValue::new("quic_service_name", service_name);

    instruments
        .duration
        .record(elapsed.as_secs(), slice::from_ref(&protocol_label));

    let pairs = [
        (stats.frame_tx, KeyValue::new("direction", "tx")),
        (stats.frame_rx, KeyValue::new("direction", "rx")),
    ];

    for (stats, label) in pairs {
        let labels = [label, protocol_label.clone()];
        instruments.data_blocked.record(stats.data_blocked, &labels);
        instruments
            .stream_data_blocked
            .record(stats.stream_data_blocked, &labels);
        instruments
            .streams_blocked_bidi
            .record(stats.streams_blocked_bidi, &labels);
        instruments
            .max_bidi_streams
            .record(stats.max_streams_bidi, &labels);
    }
}

impl ConnectionMetricsGuard<'_> {
    fn start(&mut self, interval: Duration) {
        if self.task_handle.is_some() {
            warn!("Attempted to start connection stats tracking, but task handle was already set");
            return;
        }

        let connection = self.connection.clone();
        let service_name = self.service_name;
        let established_at = self.established_at; // Force copy before moving into the task

        self.task_handle = Some(lore_spawn!(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                select! {
                    _ = ticker.tick() => {
                        record_stats(
                            service_name,
                            &connection,
                            established_at.elapsed(),
                        );
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

            record_stats(
                self.service_name,
                self.connection,
                self.established_at.elapsed(),
            );
        }
    }
}
