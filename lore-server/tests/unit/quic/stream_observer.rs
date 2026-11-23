// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use async_channel::Receiver;
use async_trait::async_trait;
use bytes::Bytes;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::client_identify::UserAgentValue;
use lore_server::quic::ProtocolErrorInfo;
use lore_server::quic::QuicService;
use lore_server::quic::SERVICE_LABEL_KEY;
use lore_server::quic::stream_observer::*;
use lore_telemetry::USER_AGENT_NONE;
use lore_transport::quic::QuicErrorStatus;
use lore_transport::quic::QuicOpCode;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::command_header::CommandHeader;
use opentelemetry::KeyValue;
use opentelemetry::Value;
use opentelemetry_semantic_conventions::attribute::USER_AGENT_NAME;
use tracing::Span;

const TEST_SERVICE_LABEL: &str = "observer_test";

#[derive(Debug, thiserror::Error)]
#[error("test service error")]
struct TestError;

/// Supplies the observer the only two things it asks of a service: the labels naming the
/// service and an opcode. Request handling is unreachable from the observer, so the rest of
/// the trait panics rather than pretending to serve requests.
struct TestService;

#[async_trait]
impl QuicService for TestService {
    type ParsedRequestType = ();
    type RequestParseErrorType = TestError;
    type RequestHandlerError = TestError;

    fn get_service_name_label(&self) -> &'static str {
        TEST_SERVICE_LABEL
    }

    fn parse_request_bytes(&self, _header: &CommandHeader, _bytes: Bytes) -> Result<(), TestError> {
        unreachable!("the observer never parses requests")
    }

    async fn run_request_handler(
        &self,
        _context: Arc<AttributeMap>,
        _request: (),
    ) -> Result<Vec<Bytes>, TestError> {
        unreachable!("the observer never handles requests")
    }

    fn command_to_metrics_label(&self, _opcode: QuicOpCode) -> &'static str {
        "test_opcode"
    }

    fn transform_protocol_error(&self, _error: &TestError) -> ProtocolErrorInfo {
        ProtocolErrorInfo {
            response_error_code: QuicServiceError::Failed as QuicErrorStatus,
            message_handle_label: "test_error",
            is_internal_error: true,
            is_appropriate_for_logging: false,
        }
    }

    fn max_chunk_size(&self) -> usize {
        unreachable!("the observer never reads from a stream")
    }

    fn build_request_span(
        &self,
        _header: &CommandHeader,
        _message: &(),
        _context: &Arc<AttributeMap>,
    ) -> Span {
        unreachable!("the observer never builds request spans")
    }
}

fn context(user_agent: Option<&str>) -> AttributeMap {
    let context = AttributeMap::default();
    if let Some(user_agent) = user_agent {
        context.insert(UserAgentValue(Arc::from(user_agent)));
    }
    context
}

fn observer() -> ConnectionObserver<TestService> {
    ConnectionObserver::new(Arc::new(TestService), &context(Some("agent/1")))
}

fn label(labels: &[KeyValue], key: &str) -> Value {
    labels
        .iter()
        .find(|label| label.key.as_str() == key)
        .unwrap_or_else(|| panic!("labels carry {key}"))
        .value
        .clone()
}

fn drain(receiver: &Receiver<StreamMetricEvent>) -> Vec<StreamMetricEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

/// What a sender reports for a finished stream, named so an assertion says which peak it
/// is checking.
struct FinishedPeaks {
    pending_chunks: usize,
    stall_ms: u64,
}

/// The peaks a sender reports for a finished stream, panicking if it reported anything else.
fn finished_peaks(event: &StreamMetricEvent) -> FinishedPeaks {
    match event {
        StreamMetricEvent::Finished {
            peak_pending,
            peak_stall_ms,
        } => FinishedPeaks {
            pending_chunks: *peak_pending,
            stall_ms: *peak_stall_ms,
        },
        StreamMetricEvent::ChunkStall(stall) => {
            panic!("expected a finished report, got a stall of {stall:?}")
        }
    }
}

fn stall_duration(event: &StreamMetricEvent) -> Duration {
    match event {
        StreamMetricEvent::ChunkStall(stall) => *stall,
        StreamMetricEvent::Finished { .. } => panic!("expected a stall, got a finished report"),
    }
}

mod message_labels {
    use super::*;

    fn keys(labels: &[KeyValue]) -> Vec<&str> {
        labels.iter().map(|label| label.key.as_str()).collect()
    }

    #[test]
    fn a_success_records_neither_failure_label() {
        let observer = observer();
        let (labels, applicable) = observer.message_labels(1, None);

        assert_eq!(
            keys(&labels[..applicable]),
            vec![
                SERVICE_LABEL_KEY,
                lore_telemetry::METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME,
                USER_AGENT_NAME,
                OPCODE_LABEL_KEY,
                SUCCESS_LABEL_KEY,
            ]
        );
        assert_eq!(label(&labels[..applicable], SUCCESS_LABEL_KEY), true.into());
    }

    #[test]
    fn a_failure_records_its_classification_and_label() {
        let observer = observer();
        let failure = MessageFailure {
            classification: "user_error",
            error_label: "parse_failed",
        };

        let (labels, applicable) = observer.message_labels(1, Some(&failure));

        assert_eq!(applicable, labels.len());
        assert_eq!(label(&labels, SUCCESS_LABEL_KEY), false.into());
        assert_eq!(
            label(&labels, HANDLER_ERROR_CLASSIFICATION_LABEL_KEY),
            Value::String("user_error".into())
        );
        assert_eq!(
            label(&labels, HANDLER_ERROR_LABEL_KEY),
            Value::String("parse_failed".into())
        );
    }
}

mod pending_chunks {
    use super::*;

    /// A queue forms and drains well inside an export interval, so the count is only ever
    /// reduced to the deepest it reached, and nothing is reported as it changes.
    #[test]
    fn counts_are_reported_only_as_a_peak_when_the_stream_finishes() {
        let (sender, receiver) = async_channel::unbounded();
        let mut metrics = StreamMetricSender::new(sender);

        for count in [1, 9, 4, 7, 0] {
            metrics.pending_chunks(count);
        }

        assert_eq!(drain(&receiver).len(), 0);

        drop(metrics);
        let events = drain(&receiver);
        assert_eq!(events.len(), 1);
        assert_eq!(finished_peaks(&events[0]).pending_chunks, 9);
    }

    #[test]
    fn a_lower_count_does_not_lower_the_peak() {
        let (sender, receiver) = async_channel::unbounded();
        let mut metrics = StreamMetricSender::new(sender);

        metrics.pending_chunks(6);
        metrics.pending_chunks(1);
        drop(metrics);

        let events = drain(&receiver);
        assert_eq!(finished_peaks(&events[0]).pending_chunks, 6);
    }
}

mod chunk_stall {
    use super::*;

    /// A stall at the threshold is not over it, so the boundary is not reported.
    #[test]
    fn stalls_up_to_the_threshold_are_not_reported_individually() {
        let (sender, receiver) = async_channel::unbounded();
        let mut metrics = StreamMetricSender::new(sender);

        metrics.chunk_stall(Duration::from_millis(1));
        metrics.chunk_stall(CHUNK_STALL_REPORT_THRESHOLD);

        assert_eq!(drain(&receiver).len(), 0);
    }

    /// Each stall over the threshold is reported as it happens, so a stream blocking
    /// repeatedly is distinguishable from one that blocked once for as long.
    #[test]
    fn every_stall_over_the_threshold_is_reported() {
        let (sender, receiver) = async_channel::unbounded();
        let mut metrics = StreamMetricSender::new(sender);

        metrics.chunk_stall(CHUNK_STALL_REPORT_THRESHOLD + Duration::from_millis(1));
        metrics.chunk_stall(Duration::from_millis(120));

        let stalls: Vec<Duration> = drain(&receiver).iter().map(stall_duration).collect();
        assert_eq!(
            stalls,
            vec![
                CHUNK_STALL_REPORT_THRESHOLD + Duration::from_millis(1),
                Duration::from_millis(120),
            ]
        );
    }

    /// The threshold governs what is reported individually, never what the peak covers, so a
    /// stream whose every stall was short still reports the longest of them.
    #[test]
    fn stalls_below_the_threshold_still_reach_the_peak() {
        let (sender, receiver) = async_channel::unbounded();
        let mut metrics = StreamMetricSender::new(sender);

        metrics.chunk_stall(Duration::from_millis(5));
        metrics.chunk_stall(Duration::from_millis(20));
        metrics.chunk_stall(Duration::from_millis(11));
        drop(metrics);

        let events = drain(&receiver);
        assert_eq!(events.len(), 1);
        let peaks = finished_peaks(&events[0]);
        assert_eq!(peaks.pending_chunks, 0);
        assert_eq!(peaks.stall_ms, 20);
    }
}

mod drop {
    use super::*;

    /// Dropping is the only thing that reports a stream finished, and it happens however
    /// stream handling ended, so a stream's peaks are always recorded.
    #[test]
    fn dropping_reports_the_stream_finished_even_with_nothing_measured() {
        let (sender, receiver) = async_channel::unbounded();
        let metrics = StreamMetricSender::new(sender);

        drop(metrics);

        let events = drain(&receiver);
        assert_eq!(events.len(), 1);
        let peaks = finished_peaks(&events[0]);
        assert_eq!(peaks.pending_chunks, 0);
        assert_eq!(peaks.stall_ms, 0);
    }

    #[test]
    fn a_closed_channel_does_not_fail_the_drop() {
        let (sender, receiver) = async_channel::unbounded::<StreamMetricEvent>();
        let metrics = StreamMetricSender::new(sender);
        receiver.close();
        drop(receiver);

        drop(metrics);
    }
}

mod context_updated {
    use super::*;

    /// Labels follow the connection's context, so events consumed after a change are recorded
    /// under the user agent the connection has since announced.
    #[test]
    fn the_user_agent_label_follows_the_context() {
        let mut observer = observer();

        observer.context_updated(&context(Some("agent/2")));

        assert_eq!(
            label(&observer.stream_labels(), USER_AGENT_NAME),
            Value::String("agent/2".into())
        );
    }

    #[test]
    fn a_context_without_a_user_agent_reports_the_absent_value() {
        let mut observer = observer();

        observer.context_updated(&context(None));

        assert_eq!(
            label(&observer.stream_labels(), USER_AGENT_NAME),
            Value::String(USER_AGENT_NONE.as_ref().into())
        );
    }
}

mod exceeds {
    use super::*;

    #[test]
    fn exceeds_when_either_part_passes_the_threshold() {
        let threshold = Duration::from_millis(20);
        let waited = ResponseWrite {
            lock_wait: Duration::from_millis(21),
            ..ResponseWrite::default()
        };
        let wrote = ResponseWrite {
            write: Duration::from_millis(21),
            ..ResponseWrite::default()
        };

        assert!(waited.exceeds(threshold));
        assert!(wrote.exceeds(threshold));
    }

    #[test]
    fn does_not_exceed_at_the_threshold() {
        let response = ResponseWrite {
            lock_wait: Duration::from_millis(20),
            write: Duration::from_millis(20),
            bytes: 0,
        };

        assert!(!response.exceeds(Duration::from_millis(20)));
    }
}
