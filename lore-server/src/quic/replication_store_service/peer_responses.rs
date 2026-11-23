// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::OnceLock;
use std::time::Duration;

use lore_telemetry::InstrumentProvider;
use lore_transport::quic::QuicErrorStatus;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::response_observer::ObservedResponse;
use lore_transport::quic::response_observer::ResponseObserver;
use lore_transport::quic::slow_response::ReportRateLimit;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use tracing::warn;

use crate::quic::RESPONSE_DURATION_MS_BUCKETS;
use crate::quic::replication_store_service::Command;
use crate::quic::replication_store_service::ReplicationServiceErrorCode;
use crate::quic::replication_store_service::server::command_name;

#[lore_macro::test_pub]
const OPCODE_LABEL_KEY: &str = "opcode";
#[lore_macro::test_pub]
const RESPONSE_STATUS_LABEL_KEY: &str = "response_status";

/// Slow peer responses logged across every peer this process reads from.
static SLOW_PEER_RESPONSE_REPORTS: ReportRateLimit = ReportRateLimit::new();

struct PeerResponseInstrumentProvider;

impl InstrumentProvider for PeerResponseInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.replication.client"
    }
}

struct PeerResponseInstruments {
    queue_to_written: Histogram<f64>,
    to_header: Histogram<f64>,
    header_to_last_byte: Histogram<f64>,
}

impl PeerResponseInstruments {
    fn instance() -> &'static PeerResponseInstruments {
        static INSTANCE: OnceLock<PeerResponseInstruments> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let provider = PeerResponseInstrumentProvider;
            PeerResponseInstruments {
                queue_to_written: provider.latency_histogram_ms_with_boundaries(
                    "request.queue_to_written",
                    RESPONSE_DURATION_MS_BUCKETS.to_vec(),
                ),
                to_header: provider.latency_histogram_ms_with_boundaries(
                    "response.time_to_header",
                    RESPONSE_DURATION_MS_BUCKETS.to_vec(),
                ),
                header_to_last_byte: provider.latency_histogram_ms_with_boundaries(
                    "response.header_to_last_byte",
                    RESPONSE_DURATION_MS_BUCKETS.to_vec(),
                ),
            }
        })
    }
}

/// Records how the responses from one peer's connection arrive, and logs those slow to arrive.
pub struct PeerResponseObserver {
    /// The labels naming the peer and the role of the connection, as the peer's other client
    /// metrics carry them.
    peer: [KeyValue; 3],
    /// Wait for a request to be written, for its response's header, or for the rest of the
    /// response, above which the response is logged. Unset, no response is logged.
    slow_response_threshold: Option<Duration>,
}

impl PeerResponseObserver {
    pub fn new(peer: [KeyValue; 3], slow_response_threshold: Option<Duration>) -> Self {
        Self {
            peer,
            slow_response_threshold,
        }
    }

    /// The labels a response is recorded under: the peer's, then its opcode and status.
    #[lore_macro::test_pub]
    fn response_labels(&self, response: &ObservedResponse) -> [KeyValue; 5] {
        let [peer_id, locality, replica_type] = self.peer.clone();
        [
            peer_id,
            locality,
            replica_type,
            KeyValue::new(OPCODE_LABEL_KEY, opcode_label(response.opcode)),
            KeyValue::new(
                RESPONSE_STATUS_LABEL_KEY,
                response_status_label(response.error_status),
            ),
        ]
    }

    /// Whether waiting for the request to be written, for the header, or for the rest of the
    /// response, took longer than the threshold. Without a threshold no response is slow.
    #[lore_macro::test_pub]
    fn is_slow(&self, response: &ObservedResponse) -> bool {
        self.slow_response_threshold.is_some_and(|threshold| {
            response
                .queue_to_written
                .max(response.to_header)
                .max(response.header_to_last_byte)
                > threshold
        })
    }
}

impl ResponseObserver for PeerResponseObserver {
    fn observe(&self, response: &ObservedResponse, connection: &quinn::Connection) {
        let instruments = PeerResponseInstruments::instance();
        let labels = self.response_labels(response);
        instruments
            .queue_to_written
            .record(response.queue_to_written.as_secs_f64() * 1_000., &labels);
        instruments
            .to_header
            .record(response.to_header.as_secs_f64() * 1_000., &labels);
        instruments
            .header_to_last_byte
            .record(response.header_to_last_byte.as_secs_f64() * 1_000., &labels);

        if !self.is_slow(response) || !SLOW_PEER_RESPONSE_REPORTS.admit() {
            return;
        }
        let stats = connection.stats();
        warn!(
            peer_id = %self.peer[0].value,
            peer_locality = %self.peer[1].value,
            replica_type = %self.peer[2].value,
            opcode = opcode_label(response.opcode),
            response_status = response_status_label(response.error_status),
            payload_bytes = response.payload_bytes,
            stream_index = response.stream_index,
            queue_to_written_ms = response.queue_to_written.as_secs_f64() * 1_000.,
            to_header_ms = response.to_header.as_secs_f64() * 1_000.,
            header_to_last_byte_ms = response.header_to_last_byte.as_secs_f64() * 1_000.,
            cwnd = stats.path.cwnd,
            rtt_ms = stats.path.rtt.as_secs_f64() * 1_000.,
            data_blocked_received = stats.frame_rx.data_blocked,
            stream_data_blocked_received = stats.frame_rx.stream_data_blocked,
            "Slow peer response"
        );
    }
}

fn opcode_label(opcode: u8) -> &'static str {
    Command::try_from(opcode).map_or("unknown", |command| command_name(&command))
}

/// The label of each error status a peer answers with: the replication service's own errors, and
/// those any QUIC service answers, such as a refusal for want of a permit.
const ERROR_STATUS_LABELS: [(QuicErrorStatus, &str); 11] = [
    (
        ReplicationServiceErrorCode::Internal as QuicErrorStatus,
        "internal",
    ),
    (
        ReplicationServiceErrorCode::AddressNotFound as QuicErrorStatus,
        "address_not_found",
    ),
    (
        ReplicationServiceErrorCode::SlowDown as QuicErrorStatus,
        "replication_slow_down",
    ),
    (
        ReplicationServiceErrorCode::PayloadNotFound as QuicErrorStatus,
        "payload_not_found",
    ),
    (
        ReplicationServiceErrorCode::Oversized as QuicErrorStatus,
        "replication_oversized",
    ),
    (
        QuicServiceError::InvalidCommand as QuicErrorStatus,
        "invalid_command",
    ),
    (
        QuicServiceError::NotAuthorized as QuicErrorStatus,
        "not_authorized",
    ),
    (QuicServiceError::Failed as QuicErrorStatus, "failed"),
    (QuicServiceError::NotFound as QuicErrorStatus, "not_found"),
    (QuicServiceError::Oversized as QuicErrorStatus, "oversized"),
    (QuicServiceError::SlowDown as QuicErrorStatus, "slow_down"),
];

/// `ok` for a successful response, otherwise the error its status names.
#[lore_macro::test_pub]
fn response_status_label(error_status: Option<QuicErrorStatus>) -> &'static str {
    let Some(status) = error_status else {
        return "ok";
    };
    ERROR_STATUS_LABELS
        .iter()
        .find(|(code, _)| *code == status)
        .map_or("other_error", |(_, label)| label)
}
