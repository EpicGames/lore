// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use async_channel::Receiver;
use async_channel::Sender;
use lore_base::lore_spawn_core;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::create_operation_context_attribute;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use tokio::select;
use tracing::error;

use crate::protocol::attribute_map::AttributeMap;
use crate::quic::QuicService;
use crate::quic::RESPONSE_DURATION_MS_BUCKETS;
use crate::quic::SERVICE_LABEL_KEY;

#[lore_macro::test_pub]
const OPCODE_LABEL_KEY: &str = "opcode";
#[lore_macro::test_pub]
const SUCCESS_LABEL_KEY: &str = "success";
#[lore_macro::test_pub]
const HANDLER_ERROR_LABEL_KEY: &str = "handler_error";
#[lore_macro::test_pub]
const HANDLER_ERROR_CLASSIFICATION_LABEL_KEY: &str = "error_classification";

pub struct MessageFailure {
    pub classification: &'static str,
    pub error_label: &'static str,
}

pub struct MessageHandling {
    /// Time from the request's arrival until its response was ready to send.
    pub elapsed: Duration,
    pub opcode: u8,
    pub error_info: Option<MessageFailure>,
    pub response: ResponseWrite,
}

/// How long sending one response took.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResponseWrite {
    /// Time from the response being queued for its stream's writer until the writer takes it.
    pub queue: Duration,
    /// Time from the writer taking the response until it is written in full, including waiting
    /// behind the responses queued before it. The transport takes data only as the peer's
    /// flow-control credit and its own send buffer allow, so this includes waiting for both.
    pub write: Duration,
    /// Bytes of the response, its header included.
    pub bytes: usize,
}

impl ResponseWrite {
    /// Whether waiting for the writer or writing took longer than `threshold`.
    pub fn exceeds(&self, threshold: Duration) -> bool {
        self.queue.max(self.write) > threshold
    }
}

pub enum ServiceMetricEvent {
    MessageHandling(MessageHandling),
}

/// Milliseconds a stream spent blocked on an out of order chunk, shared by the histogram of
/// individual stalls and the histogram of each stream's peak so that one can be read against the
/// other without accounting for different boundaries.
///
/// Only the peak histogram reaches the buckets below [`CHUNK_STALL_REPORT_THRESHOLD`], since a
/// stall has to exceed it to be recorded individually.
const STALL_DURATION_BUCKETS: &[f64] = &[
    1., 5., 10., 25., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000.,
];

/// Stall a stream has to exceed to be recorded individually.
///
/// Recovering from a reordered chunk takes about a round trip, so stalls below this are the
/// expected cost of reordering and say nothing a caller can act on. Only those above it are
/// reported, which is what keeps a histogram of every stall off the chunk read path.
#[lore_macro::test_pub]
const CHUNK_STALL_REPORT_THRESHOLD: Duration = Duration::from_millis(30);

pub enum StreamMetricEvent {
    /// A stall exceeding [`CHUNK_STALL_REPORT_THRESHOLD`]. Reported as it happens, so that a
    /// stream blocking repeatedly is distinguishable from one that blocked once.
    ChunkStall(Duration),
    /// The stream has ended. Carries the peaks reached over its lifetime, which are accumulated by
    /// the sender rather than reported as they change.
    Finished {
        peak_pending: usize,
        peak_stall_ms: u64,
    },
}

/// Accumulates one stream's statistics and reports them over its connection's channel, reporting
/// the peaks reached when dropped.
///
/// Held for the duration of a stream, so that every exit from stream handling - including the
/// error paths - reports the stream as finished.
///
/// One of these per stream is what keeps the peaks per stream: they are measured by whoever owns
/// them and travel as a value, so the labels the histograms carry are free to describe less than
/// one stream without merging what two streams reached.
///
/// Reporting is synchronous throughout. The channel is unbounded, so awaiting a send could never
/// wait, and keeping it out of the caller's future avoids a suspension point on the chunk read
/// path. Should the channel ever gain a bound, dropping a metric is the wanted behaviour there in
/// any case.
pub struct StreamMetricSender {
    sender: Sender<StreamMetricEvent>,
    peak_pending: usize,
    peak_stall: Duration,
}

impl StreamMetricSender {
    pub fn new(sender: Sender<StreamMetricEvent>) -> Self {
        Self {
            sender,
            peak_pending: 0,
            peak_stall: Duration::ZERO,
        }
    }

    /// Records a change in the stream's queue of out of order chunks.
    ///
    /// Only the deepest the queue reached is reported, when the stream finishes: a queue forms and
    /// drains inside a single export interval, so a count published as it changed would be
    /// collected long after the queue it described had gone.
    pub fn pending_chunks(&mut self, pending_chunks: usize) {
        self.peak_pending = self.peak_pending.max(pending_chunks);
    }

    /// Records how long the stream was blocked waiting for an out of order chunk.
    ///
    /// Reported individually only when it exceeds [`CHUNK_STALL_REPORT_THRESHOLD`]. Every stall
    /// counts towards the peak either way.
    pub fn chunk_stall(&mut self, stall: Duration) {
        self.peak_stall = self.peak_stall.max(stall);

        if stall > CHUNK_STALL_REPORT_THRESHOLD {
            self.send(StreamMetricEvent::ChunkStall(stall));
        }
    }

    fn send(&self, event: StreamMetricEvent) {
        let _ = self
            .sender
            .try_send(event)
            .inspect_err(|err| error!(?err, "failed to send stream metric"));
    }
}

impl Drop for StreamMetricSender {
    fn drop(&mut self) {
        self.send(StreamMetricEvent::Finished {
            peak_pending: self.peak_pending,
            peak_stall_ms: self.peak_stall.as_millis() as u64,
        });
    }
}

struct StreamHandlerInstrumentProvider;

impl InstrumentProvider for StreamHandlerInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.quic.stream_handler"
    }
}

struct StreamHandlerInstruments {
    latency_histogram: Histogram<f64>,
    response_queue_histogram: Histogram<f64>,
    response_write_histogram: Histogram<f64>,
    peak_pending_chunks_histogram: Histogram<u64>,
    peak_pending_chunk_stall_histogram: Histogram<u64>,
    pending_chunk_stall_histogram: Histogram<u64>,
}

impl StreamHandlerInstruments {
    fn instance() -> &'static StreamHandlerInstruments {
        static INSTANCE: OnceLock<StreamHandlerInstruments> = OnceLock::new();
        INSTANCE.get_or_init(StreamHandlerInstruments::new)
    }

    fn new() -> Self {
        let provider = StreamHandlerInstrumentProvider;

        Self {
            latency_histogram: provider.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            response_queue_histogram: provider.latency_histogram_ms_with_boundaries(
                "response.queue_duration",
                RESPONSE_DURATION_MS_BUCKETS.to_vec(),
            ),
            response_write_histogram: provider.latency_histogram_ms_with_boundaries(
                "response.write_duration",
                RESPONSE_DURATION_MS_BUCKETS.to_vec(),
            ),
            peak_pending_chunks_histogram: provider.length_histogram(
                "stream.peak_pending_chunks",
                vec![
                    1., 5., 10., 25., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000.,
                    25_000., 50_000., 100_000.,
                ],
            ),
            peak_pending_chunk_stall_histogram: provider.length_histogram(
                "stream.peak_pending_chunk_stall_duration",
                STALL_DURATION_BUCKETS.to_vec(),
            ),
            pending_chunk_stall_histogram: provider.length_histogram(
                "stream.pending_chunk_stall_duration",
                STALL_DURATION_BUCKETS.to_vec(),
            ),
        }
    }
}

/// Consumes one connection's service and stream metric events, and owns the labels they are
/// recorded under.
#[lore_macro::test_pub]
struct ConnectionObserver<ServiceType>
where
    ServiceType: QuicService,
{
    service: Arc<ServiceType>,

    service_label: KeyValue,
    handle_message_context: KeyValue,
    user_agent_label: KeyValue,
}

impl<ServiceType> ConnectionObserver<ServiceType>
where
    ServiceType: QuicService,
{
    #[lore_macro::test_pub]
    fn new(service: Arc<ServiceType>, context: &AttributeMap) -> Self {
        Self {
            service_label: KeyValue::new(SERVICE_LABEL_KEY, service.get_service_name_label()),
            handle_message_context: create_operation_context_attribute("handle_message"),
            user_agent_label: context.user_agent_label(),
            service,
        }
    }

    /// Labels one handled message is recorded under, and how many of them apply.
    ///
    /// The two describing a failure occupy the end of the array so that a success can drop them by
    /// recording a prefix, rather than building a second array a label shorter. A label added
    /// anywhere but the end therefore changes what a success reports
    #[lore_macro::test_pub]
    fn message_labels(
        &self,
        opcode: u8,
        error_info: Option<&MessageFailure>,
    ) -> ([KeyValue; 7], usize) {
        let labels = [
            self.service_label.clone(),
            self.handle_message_context.clone(),
            self.user_agent_label.clone(),
            KeyValue::new(
                OPCODE_LABEL_KEY,
                self.service.command_to_metrics_label(opcode),
            ),
            KeyValue::new(SUCCESS_LABEL_KEY, error_info.is_none()),
            // failure labels after this point
            KeyValue::new(
                HANDLER_ERROR_CLASSIFICATION_LABEL_KEY,
                error_info.map_or("", |info| info.classification),
            ),
            KeyValue::new(
                HANDLER_ERROR_LABEL_KEY,
                error_info.map_or("", |info| info.error_label),
            ),
        ];

        let applicable = if error_info.is_none() {
            labels.len() - 2
        } else {
            labels.len()
        };

        (labels, applicable)
    }

    fn consume_message(&self, event: MessageHandling) {
        let instruments = StreamHandlerInstruments::instance();
        let (labels, applicable) = self.message_labels(event.opcode, event.error_info.as_ref());
        instruments
            .latency_histogram
            .record(event.elapsed.as_millis() as f64, &labels[..applicable]);
        instruments.response_queue_histogram.record(
            event.response.queue.as_secs_f64() * 1_000.,
            &labels[..applicable],
        );
        instruments.response_write_histogram.record(
            event.response.write.as_secs_f64() * 1_000.,
            &labels[..applicable],
        );
    }

    /// Labels a stream's distributions are recorded under.
    ///
    /// They name neither the stream nor its connection. A distribution aggregates across whoever
    /// contributes to it, so the observations of several streams under one series are simply more
    /// samples, and each stream's own peak is preserved by the sender that measured it. Attributing
    /// a sample to the stream or connection it came from is what spans are for, and they carry the
    /// real identifiers rather than anything reduced to bound a series count.
    #[lore_macro::test_pub]
    fn stream_labels(&self) -> [KeyValue; 2] {
        [self.service_label.clone(), self.user_agent_label.clone()]
    }

    fn consume_stream(&self, event: StreamMetricEvent) {
        let instruments = StreamHandlerInstruments::instance();
        let labels = self.stream_labels();

        match event {
            StreamMetricEvent::ChunkStall(stall) => instruments
                .pending_chunk_stall_histogram
                .record(stall.as_millis() as u64, &labels),
            StreamMetricEvent::Finished {
                peak_pending,
                peak_stall_ms,
            } => {
                instruments
                    .peak_pending_chunks_histogram
                    .record(peak_pending as u64, &labels);
                instruments
                    .peak_pending_chunk_stall_histogram
                    .record(peak_stall_ms, &labels);
            }
        }
    }

    /// Rebuilds the labels subsequent events are recorded under.
    #[lore_macro::test_pub]
    fn context_updated(&mut self, context: &AttributeMap) {
        self.user_agent_label = context.user_agent_label();
    }
}

/// Consumes a connection's service and stream metric events on a single task.
///
/// Both event kinds share the labels derived from the connection's context, so one task owning
/// them keeps the two consistent without synchronisation.
pub fn observe_connection<ServiceType>(
    service_metrics: Receiver<ServiceMetricEvent>,
    stream_metrics: Receiver<StreamMetricEvent>,
    service: Arc<ServiceType>,
    context: Arc<AttributeMap>,
) where
    ServiceType: QuicService,
{
    let mut context_changed = context.subscribe();
    let mut observer = ConnectionObserver::new(service, &context);
    let context = Arc::downgrade(&context);

    // Pinned to core rather than following the caller, which is a net task: there is one of these
    // per live connection, recording metrics for the whole of its traffic, and net stays for
    // driving sockets.
    lore_spawn_core!(async move {
        let mut service_events = true;
        let mut stream_events = true;
        // The context goes with the connection, which may happen while queued events remain. The
        // labels are final from that point on.
        let mut labels_can_change = true;

        while service_events || stream_events {
            select! {
                // A context change is taken before any queued event, so that events are recorded
                // under the labels current when they are consumed. The channels are unbounded, so
                // without this an arbitrary backlog could be attributed to labels the connection
                // has already moved on from. A change is only ready once per notification, so
                // taking it first cannot starve the events.
                biased;

                changed = context_changed.changed(), if labels_can_change => {
                    match context.upgrade() {
                        Some(context) if changed.is_ok() => observer.context_updated(&context),
                        _ => labels_can_change = false,
                    }
                }
                event = service_metrics.recv(), if service_events => {
                    match event {
                        Ok(ServiceMetricEvent::MessageHandling(event)) => {
                            observer.consume_message(event);
                        }
                        Err(_) => service_events = false,
                    }
                }
                event = stream_metrics.recv(), if stream_events => {
                    match event {
                        Ok(event) => observer.consume_stream(event),
                        Err(_) => stream_events = false,
                    }
                }
            }
        }
    });
}
