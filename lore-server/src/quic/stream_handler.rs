// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::lore_spawn_core;
use lore_base::lore_spawn_net;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::runtime::execution_context;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::chunking::ChunkingMetric;
use lore_transport::quic::chunking::ParallelChunking;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::frame_queue::FrameChunks;
use lore_transport::quic::frame_queue::FrameQueue;
use lore_transport::quic::slow_response::ReportRateLimit;
use quinn::ReadError;
use quinn::RecvStream;
use quinn::SendStream;
use quinn::VarInt;
use quinn::WriteError;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tracing::Instrument;
use tracing::Span;
use tracing::debug;
use tracing::error;
use tracing::trace;
use tracing::warn;

use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::QuinnConnection;
use crate::quic::ProtocolErrorInfo;
use crate::quic::QuicErrorStatus;
use crate::quic::QuicService;
use crate::quic::StreamDataHandler;
use crate::quic::StreamHandlerError;
use crate::quic::stream_observer::MessageFailure;
use crate::quic::stream_observer::MessageHandling;
use crate::quic::stream_observer::ResponseWrite;
use crate::quic::stream_observer::ServiceMetricEvent;
use crate::quic::stream_observer::StreamMetricEvent;
use crate::quic::stream_observer::StreamMetricSender;
use crate::quic::stream_observer::observe_connection;

const METRIC_EVENTS_BUFFER: usize = 10_000;

const PERMIT_TIMEOUT_LABEL_VALUE: &str = "PermitTimeout";
/// Refused by the per-connection ceiling, before any wait for a stream permit.
const ADMISSION_LIMIT_LABEL_VALUE: &str = "AdmissionLimit";
/// Rejected because the service could not parse the request.
const PARSE_ERROR_LABEL_VALUE: &str = "ParsingError";
const HANDLE_MESSAGE_USER_ERROR_LABEL_VALUE: &str = "User";
const HANDLE_MESSAGE_INTERNAL_ERROR_LABEL_VALUE: &str = "Internal";

/// How long a request may wait for a stream permit before the server answers `SlowDown`, when no
/// `permit_timeout_ms` is configured. Roughly one round trip, since refusing only costs the client
/// a reissue through its `store_retry()` back-off.
const DEFAULT_PERMIT_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(100);

/// Ceiling on a request's total time when no `handler_timeout_seconds` is configured.
const DEFAULT_HANDLER_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The rejections a stream holds queued for its writer. A client that stops reading its responses
/// stalls the writer, and past this many rejections the stream's read loop waits for the writer
/// rather than queuing another, so the client's further requests wait on its flow control and
/// none goes unanswered.
#[lore_macro::test_pub]
const QUEUED_REJECTIONS_PER_STREAM: usize = 64;

/// The responses a stream's writer queue keeps room for once it empties. The queue holds every
/// response waiting to be written, however many, but a server holds many streams, so a queue that
/// grew in a burst gives most of its room back once it empties.
const RESPONSES_KEPT_PER_STREAM: usize = 8;

/// Slow responses logged across every connection the process serves.
static SLOW_RESPONSE_REPORTS: ReportRateLimit = ReportRateLimit::new();

fn is_graceful_close(err: &ReadError) -> bool {
    match err {
        ReadError::ConnectionLost(quinn::ConnectionError::LocallyClosed) => true,
        ReadError::ConnectionLost(quinn::ConnectionError::ApplicationClosed(close)) => {
            close.error_code == VarInt::from_u32(0)
        }
        _ => false,
    }
}

/// Counts a request against its connection's ceiling for as long as the guard is alive.
///
/// The ceiling is only ever tested, never waited on, so a counter suffices and avoids the
/// waiter-queue lock a semaphore takes on every release. The count has to come back down on
/// every exit from a request, which is why this is a guard rather than a pair of calls.
struct AdmissionGuard(Arc<AtomicUsize>);

impl AdmissionGuard {
    /// `None` once `limit` requests are already outstanding.
    fn take(count: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        count
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |outstanding| {
                (outstanding < limit).then_some(outstanding + 1)
            })
            .ok()
            .map(|_| Self(count.clone()))
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Limits governing how much work a connection may have in progress, and how long its requests
/// may take.
#[derive(Copy, Clone)]
pub struct AdmissionLimits {
    /// Concurrent requests allowed per stream.
    pub process_limit: usize,
    /// Ceiling on requests in handling for the whole connection, waiting ones included.
    pub inflight_limit: usize,
    /// Ceiling on a request's total time; [`DEFAULT_HANDLER_TIMEOUT`] when unset.
    pub handler_timeout: Option<Duration>,
    /// Wait for a stream permit; [`DEFAULT_PERMIT_ACQUIRE_TIMEOUT`] when unset.
    pub permit_timeout: Option<Duration>,
    /// Wait for a response's stream writer, or time writing it, above which the response is
    /// logged as slow. Unset, no response is logged.
    pub slow_send_threshold: Option<Duration>,
}

pub struct StreamHandler<ServiceType>
where
    ServiceType: QuicService,
{
    service: Arc<ServiceType>,
    context: Arc<AttributeMap>,
    /// Concurrent requests allowed per stream. [`StreamDataHandler::handle_stream`] builds one
    /// permit pool per stream from it; the handler itself is per connection so that
    /// per-connection service state stays shared.
    process_limit: usize,
    /// Requests in handling for the connection, across every stream and including those only
    /// waiting for a stream permit.
    admission: Arc<AtomicUsize>,
    /// Ceiling on [`StreamHandler::admission`], above which requests are refused outright.
    inflight_limit: usize,
    handler_duration_timeout: Duration,
    /// Clamped at construction to no more than [`StreamHandler::handler_duration_timeout`], so a
    /// short request deadline wins over the permit budget.
    permit_acquire_timeout: Duration,
    slow_send_threshold: Option<Duration>,

    service_metrics: async_channel::Sender<ServiceMetricEvent>,
    stream_metrics: async_channel::Sender<StreamMetricEvent>,
}

impl<ServiceType> StreamHandler<ServiceType>
where
    ServiceType: QuicService,
{
    pub fn new(
        service: Arc<ServiceType>,
        context: Arc<AttributeMap>,
        limits: AdmissionLimits,
    ) -> Self {
        let (service_metrics, service_metrics_receiver) =
            async_channel::bounded(METRIC_EVENTS_BUFFER);
        let (stream_metrics, stream_metrics_receiver) =
            async_channel::bounded(METRIC_EVENTS_BUFFER);
        observe_connection(
            service_metrics_receiver,
            stream_metrics_receiver,
            service.clone(),
            context.clone(),
        );

        let handler_duration_timeout = limits.handler_timeout.unwrap_or(DEFAULT_HANDLER_TIMEOUT);
        Self {
            service,
            context,
            process_limit: limits.process_limit,
            admission: Arc::new(AtomicUsize::new(0)),
            inflight_limit: limits.inflight_limit,
            handler_duration_timeout,
            permit_acquire_timeout: limits
                .permit_timeout
                .unwrap_or(DEFAULT_PERMIT_ACQUIRE_TIMEOUT)
                .min(handler_duration_timeout),
            slow_send_threshold: limits.slow_send_threshold,
            service_metrics,
            stream_metrics,
        }
    }

    /// Logs a failed write to a stream, at a level that depends on whether the peer ended the
    /// stream or connection on purpose.
    fn log_write_error(e: &WriteError) {
        match e {
            WriteError::Stopped(error_code) => {
                warn!(error_code = %error_code, "Peer closed stream during write");
            }
            // Peer closed the connection gracefully (CONNECTION_CLOSE, app code 0) while
            // we were still writing a response. Mirrors is_graceful_close on the read
            // path so a writer racing the close does not warn.
            WriteError::ConnectionLost(quinn::ConnectionError::ApplicationClosed(close))
                if close.error_code == VarInt::from_u32(0) =>
            {
                debug!("Peer closed connection gracefully during write");
            }
            WriteError::ConnectionLost(err) => {
                warn!(error = %e, cause = %err, "Stream write failed");
            }
            _ => {
                warn!(error = %e, "Stream write failed");
            }
        }
    }

    /// Answer a request that arrived at `start` and was rejected before its handler ran,
    /// recording `reason` as the metric label for what turned it away. Waits only while the
    /// stream's pool of rejections queued for its writer is used up.
    async fn reject(
        &self,
        header: CommandHeader,
        stream: &StreamQueues,
        code: QuicServiceError,
        reason: &'static str,
        start: Instant,
    ) {
        // The pool is never closed.
        let Ok(permit) = stream.rejections.clone().acquire_owned().await else {
            return;
        };
        queue_response(
            &stream.responses,
            Err((
                header,
                ProtocolErrorInfo {
                    response_error_code: code as QuicErrorStatus,
                    message_handle_label: reason,
                    is_internal_error: false,
                    is_appropriate_for_logging: false,
                },
            )),
            ResponseHold {
                elapsed: start.elapsed(),
                error_info: Some(MessageFailure {
                    classification: HANDLE_MESSAGE_USER_ERROR_LABEL_VALUE,
                    error_label: reason,
                }),
                permit,
                admission: None,
                span: Span::none(),
            },
        );
    }

    /// Take a permit from `limiter`, waiting up to `permit_acquire_timeout` when none is free.
    ///
    /// `None` once that wait elapses or the pool is closed.
    async fn acquire_permit(&self, limiter: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
        if let Ok(permit) = limiter.clone().try_acquire_owned() {
            return Some(permit);
        }
        match tokio::time::timeout(self.permit_acquire_timeout, limiter.clone().acquire_owned())
            .await
        {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(err)) => {
                warn!(error = %err, "Acquire stream handler permit failed");
                None
            }
            Err(_elapsed) => None,
        }
    }

    /// Admit a request against the connection ceiling and `limiter`, the calling stream's pool,
    /// then hand it to a task.
    ///
    /// Only the admission checks run on the caller's read loop. Acquiring in the spawned task
    /// instead would let a client park unboundedly many requests, each holding its parsed
    /// payload.
    ///
    /// Both are held until the response has been written, so egress congestion cannot let a
    /// connection admit more work than it can answer.
    async fn handle_message(
        &self,
        header: CommandHeader,
        message: ServiceType::ParsedRequestType,
        stream: &StreamQueues,
        start: Instant,
    ) -> Result<(), StreamHandlerError> {
        let handler_duration_timeout = self.handler_duration_timeout;

        let Some(admission) = AdmissionGuard::take(&self.admission, self.inflight_limit) else {
            self.reject(
                header,
                stream,
                QuicServiceError::SlowDown,
                ADMISSION_LIMIT_LABEL_VALUE,
                start,
            )
            .await;
            return Ok(());
        };

        let Some(permit) = self.acquire_permit(&stream.limiter).await else {
            self.reject(
                header,
                stream,
                QuicServiceError::SlowDown,
                PERMIT_TIMEOUT_LABEL_VALUE,
                start,
            )
            .await;
            return Ok(());
        };

        let handler_context = self.context.clone();
        let responses = stream.responses.clone();
        let service = self.service.clone();
        let request_span = service.build_request_span(&header, &message, &self.context);
        let span = request_span.clone();
        let fut = Box::pin(async move {
            let result = tokio::select! {
                _timeout = tokio::time::sleep(handler_duration_timeout.saturating_sub(start.elapsed())) => {
                    debug!("Timeout in message handling duration");
                    Err(ProtocolErrorInfo::timed_out())
                },
                result = async {
                    service.run_request_handler(handler_context, message).await.map_err(|protocol_error| {
                        service.transform_protocol_error(&protocol_error)
                    })
                } => result
            };

            let error_info = if let Err(err) = &result {
                Some(MessageFailure {
                    classification: if err.is_internal_error {
                        HANDLE_MESSAGE_INTERNAL_ERROR_LABEL_VALUE
                    } else {
                        HANDLE_MESSAGE_USER_ERROR_LABEL_VALUE
                    },
                    error_label: err.message_handle_label,
                })
            } else {
                None
            };

            let result = match result {
                Ok(response_bytes) => {
                    let total_length: usize = response_bytes.iter().map(|chunk| chunk.len()).sum();

                    // the protocol client and server agree on the chunking ahead of time. The clients
                    // have code that will close the connection (and reconnect) if a message is received
                    // that is too big, but we won't have observability over client side code. So catch
                    // it here so we have metrics and alerting
                    if total_length > service.max_chunk_size() {
                        warn!(
                            total_length,
                            opcode = header.cmd,
                            "Message handler produced a message too big for clients"
                        );
                        Err((
                            header,
                            ProtocolErrorInfo {
                                response_error_code: QuicServiceError::Failed as QuicErrorStatus,
                                message_handle_label: "ResponseTooBig",
                                is_internal_error: true,
                                is_appropriate_for_logging: true,
                            },
                        ))
                    } else {
                        Ok((header, response_bytes))
                    }
                }
                Err(error_status) => Err((header, error_status)),
            };

            queue_response(
                &responses,
                result,
                ResponseHold {
                    elapsed: start.elapsed(),
                    error_info,
                    permit,
                    admission: Some(admission),
                    span,
                },
            );
        });
        // The transport-to-handler boundary: everything above this point runs on
        // net, and request processing must not. Pinned rather than `lore_spawn!`,
        // which would inherit net from the caller.
        lore_spawn_core!(LORE_CONTEXT.scope(execution_context(), fut.instrument(request_span)));

        Ok(())
    }

    async fn process_message(
        &self,
        header: CommandHeader,
        payload: Option<bytes::Bytes>,
        stream: &StreamQueues,
    ) -> Result<(), StreamHandlerError> {
        let start = Instant::now();
        let parse_result = self
            .service
            .parse_request_bytes(&header, payload.unwrap_or_default());
        trace!("Parse result for command: {header:?} as: {parse_result:?}");

        match parse_result {
            Err(e) => {
                warn!(command_header = ?header, error = %e, "Request parse failed");
                self.reject(
                    header,
                    stream,
                    QuicServiceError::InvalidCommand,
                    PARSE_ERROR_LABEL_VALUE,
                    start,
                )
                .await;
                Ok(())
            }
            Ok(message) => {
                trace!("Parsed message: {message:?}, sending off for processing",);
                self.handle_message(header, message, stream, start).await
            }
        }
    }

    /// What the writer of a stream of this connection reports its responses with.
    fn response_reporter(&self) -> ResponseReporter<ServiceType> {
        ResponseReporter {
            service: self.service.clone(),
            context: self.context.clone(),
            slow_send_threshold: self.slow_send_threshold,
            service_metrics: self.service_metrics.clone(),
        }
    }
}

/// The channel a stream's responses go to its writer on ([`write_responses`]).
type ResponseSender = mpsc::UnboundedSender<QueuedResponse>;

struct StreamQueues {
    /// The stream's permits for requests in handling.
    limiter: Arc<Semaphore>,
    responses: ResponseSender,
    /// The stream's permits for rejections queued for its writer, of which there are
    /// [`QUEUED_REJECTIONS_PER_STREAM`].
    rejections: Arc<Semaphore>,
}

/// A response on its way to its stream's writer.
struct QueuedResponse {
    chunks: FrameChunks,
    report: ResponseReport,
}

/// What a response holds until its writer has written it or its write has failed.
struct ResponseHold {
    /// Time from the request's arrival until its response was ready to send.
    elapsed: Duration,
    error_info: Option<MessageFailure>,
    /// The stream permit of a handled request, or the rejection permit of one turned away.
    permit: OwnedSemaphorePermit,
    admission: Option<AdmissionGuard>,
    /// The request's span, kept open until its response is written or fails, so the response is
    /// reported in it. Disabled when the request has none, as for a rejection, and the response
    /// is then reported in its stream's span.
    span: Span,
}

struct ResponseReport {
    opcode: u8,
    success: bool,
    /// Bytes of the response, its header included.
    bytes: usize,
    queued: tokio::time::Instant,
    hold: ResponseHold,
}

/// A response a stream's writer has taken from its channel.
struct TakenResponse {
    report: ResponseReport,
    taken: tokio::time::Instant,
}

/// Queues the response to `result` for its stream's writer on `responses`, holding `hold` until
/// it is written.
fn queue_response(responses: &ResponseSender, result: MessageProcessingResult, hold: ResponseHold) {
    let (header, success, chunks, bytes) = match result {
        Ok((header, mut data)) => {
            let total_length: usize = data.iter().map(|chunk| chunk.len()).sum();
            debug!(command_header = ?header, total_length, "Request handled");
            let (header_buf, header_len) = header
                .response_success(total_length as u32)
                .response_bytes();
            let chunks =
                FrameChunks::new(Bytes::copy_from_slice(&header_buf[..header_len]), &mut data);
            (header, true, chunks, header_len + total_length)
        }
        Err((header, error)) => {
            error.log(&header);
            let (header_buf, header_len) = header
                .response_error(error.response_error_code)
                .response_bytes();
            let chunks =
                FrameChunks::new(Bytes::copy_from_slice(&header_buf[..header_len]), &mut []);
            (header, false, chunks, header_len)
        }
    };
    let report = ResponseReport {
        opcode: header.cmd,
        success,
        bytes,
        queued: tokio::time::Instant::now(),
        hold,
    };
    // The writer stops only once every sender is gone, or with its runtime.
    if responses.send(QueuedResponse { chunks, report }).is_err() {
        debug!(command_header = ?header, "Stream writer has stopped, response dropped");
    }
}

/// What the writer of a stream needs to report the responses it writes.
struct ResponseReporter<ServiceType> {
    service: Arc<ServiceType>,
    context: Arc<AttributeMap>,
    slow_send_threshold: Option<Duration>,
    service_metrics: async_channel::Sender<ServiceMetricEvent>,
}

impl<ServiceType: QuicService> ResponseReporter<ServiceType> {
    /// Releases what `response` holds, logs it in its request's span, and records its metrics, as
    /// written in full at `now` when `written`, or as failed.
    fn report(&self, response: TakenResponse, now: tokio::time::Instant, written: bool) {
        let TakenResponse { report, taken } = response;
        let ResponseReport {
            opcode,
            success,
            bytes,
            queued,
            hold,
        } = report;
        let ResponseHold {
            elapsed,
            error_info,
            permit,
            admission,
            span,
        } = hold;
        drop(permit);
        drop(admission);
        let _entered = span.enter();

        let timing = ResponseWrite {
            queue: taken - queued,
            write: now - taken,
            bytes,
        };
        if written {
            report_slow_response(
                self.slow_send_threshold,
                &self.context,
                self.service.command_to_metrics_label(opcode),
                success,
                timing,
            );
        }
        let _ = self
            .service_metrics
            .try_send(ServiceMetricEvent::MessageHandling(MessageHandling {
                elapsed,
                opcode,
                error_info,
                response: timing,
            }))
            .inspect_err(|err| error!(?err, "failed to send service handle metric"));
    }
}

/// Writes the responses queued on `responses` to `send`, the send half of one stream, each whole,
/// in the order they are taken, and every response queued at once in as few writes as the
/// stream's flow control allows ([`FrameQueue`]). Each response's permits are released once it is
/// written, so a client that stops reading cannot make the stream admit more work. Runs as a task
/// of its own on the net runtime until the channel is closed, which is once the stream's read loop
/// and every handler it spawned are done, and every response queued is written or failed. Then it
/// finishes the stream.
///
/// A write that fails fails every response queued, and every response queued after it. The stream
/// is then left to `SendStream`'s drop, which finishes it, or resets it if the peer stopped it.
async fn write_responses<ServiceType: QuicService>(
    mut send: SendStream,
    mut responses: mpsc::UnboundedReceiver<QueuedResponse>,
    reporter: ResponseReporter<ServiceType>,
) {
    let mut queue = FrameQueue::new(RESPONSES_KEPT_PER_STREAM);
    let mut failed = false;
    std::future::poll_fn(|cx| {
        let mut now = None;
        let closed = loop {
            match responses.poll_recv(cx) {
                Poll::Ready(Some(QueuedResponse { chunks, report })) => {
                    let taken = *now.get_or_insert_with(tokio::time::Instant::now);
                    let response = TakenResponse { report, taken };
                    if failed {
                        reporter.report(response, taken, false);
                    } else {
                        queue.push(chunks, response);
                    }
                }
                Poll::Ready(None) => break true,
                Poll::Pending => break false,
            }
        };
        while !failed && !queue.is_empty() {
            let written = std::pin::pin!(send.write_chunks(queue.unwritten())).poll(cx);
            match written {
                Poll::Ready(Ok(_)) => {
                    let now = tokio::time::Instant::now();
                    queue.pop_written(|response| reporter.report(response, now, true));
                }
                Poll::Ready(Err(err)) => {
                    let now = tokio::time::Instant::now();
                    queue.fail(|response| reporter.report(response, now, false));
                    failed = true;
                    StreamHandler::<ServiceType>::log_write_error(&err);
                }
                Poll::Pending => break,
            }
        }
        if closed && queue.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;

    if failed {
        return;
    }
    debug!("Sending finish");
    if let Err(err) = send.finish() {
        warn!(error = %err, "Stream finish failed");
    }
}

/// Logs a response that waited for its stream's writer, or took to write, longer than
/// `threshold`, with the state of the connection's path at that moment. Rate limited across the
/// process, so a burst of slow responses is logged in part. Without a threshold nothing is logged.
fn report_slow_response(
    threshold: Option<Duration>,
    context: &AttributeMap,
    opcode: &'static str,
    success: bool,
    response: ResponseWrite,
) {
    let Some(threshold) = threshold else {
        return;
    };
    if !response.exceeds(threshold) || !SLOW_RESPONSE_REPORTS.admit() {
        return;
    }

    let stats = context
        .get::<QuinnConnection>()
        .map(|connection| connection.0.stats());
    warn!(
        opcode,
        success,
        response_bytes = response.bytes,
        queue_ms = response.queue.as_secs_f64() * 1_000.,
        write_ms = response.write.as_secs_f64() * 1_000.,
        cwnd = stats.as_ref().map(|stats| stats.path.cwnd),
        rtt_ms = stats
            .as_ref()
            .map(|stats| stats.path.rtt.as_secs_f64() * 1_000.),
        data_blocked_sent = stats.as_ref().map(|stats| stats.frame_tx.data_blocked),
        stream_data_blocked_sent = stats
            .as_ref()
            .map(|stats| stats.frame_tx.stream_data_blocked),
        "Slow QUIC response"
    );
}

type MessageProcessingResult =
    Result<(CommandHeader, Vec<Bytes>), (CommandHeader, ProtocolErrorInfo)>;

#[async_trait]
impl<ServiceType> StreamDataHandler for StreamHandler<ServiceType>
where
    ServiceType: QuicService,
{
    async fn handle_stream(
        &self,
        recv: &mut RecvStream,
        send: SendStream,
    ) -> Result<(), StreamHandlerError> {
        debug!("Handling stream");

        let mut stream_metrics = StreamMetricSender::new(self.stream_metrics.clone());
        let (metrics_sender, metrics_receiver) = std::sync::mpsc::channel();

        let mut chunk_resolver = ParallelChunking::new(
            self.service.header_size(),
            self.service.max_chunk_size(),
            Some(metrics_sender),
        );

        let (responses, queued) = mpsc::unbounded_channel();
        lore_spawn_net!(write_responses(send, queued, self.response_reporter()).in_current_span());
        let stream = StreamQueues {
            limiter: Arc::new(Semaphore::new(self.process_limit)),
            responses,
            rejections: Arc::new(Semaphore::new(QUEUED_REJECTIONS_PER_STREAM)),
        };
        // Kept across the loop so a chunk carrying several requests costs no allocation.
        let mut messages = Vec::new();

        loop {
            // quinn's reads never use tokio's budget, and neither does handing a request to its
            // handler while a permit is free, so while the stream keeps data buffered this loop
            // would not return, holding its net worker and keeping the tasks waiting to run there,
            // such as the connection's stream writers, from running.
            tokio::task::consume_budget().await;
            let mut next_chunk = match recv.read_chunk(self.service.max_chunk_size(), false).await {
                Ok(chunk) => chunk,
                Err(err) => {
                    if is_graceful_close(&err) {
                        return Ok(());
                    }
                    return Err(StreamHandlerError::StreamReadError(err));
                }
            };

            let Some(chunk) = next_chunk.take() else {
                debug!("Terminating request reader in stream handler");
                break;
            };

            chunk_resolver
                .resolve_into(chunk, &mut messages)
                .map_err(|command_header| {
                    warn!(?command_header, "Bad header");
                    StreamHandlerError::BadHeader(command_header)
                })?;

            for message in messages.drain(..) {
                // A request cannot report an error.
                if message.header.error {
                    debug!(command_header = ?message.header, "Request header carries the error bit");
                    return Err(StreamHandlerError::BadHeader(message.header));
                }

                self.process_message(message.header, message.payload, &stream)
                    .await?;
                // A chunk can carry thousands of requests.
                tokio::task::consume_budget().await;
            }

            while let Ok(metric) = metrics_receiver.try_recv() {
                match metric {
                    ChunkingMetric::Stall(duration) => {
                        stream_metrics.chunk_stall(duration);
                    }
                    ChunkingMetric::PendingChunks(num) => {
                        stream_metrics.pending_chunks(num);
                    }
                }
            }
        }

        Ok(())
    }

    async fn close(
        &self,
        recv: &mut RecvStream,
        error_code: Option<u32>,
    ) -> Result<(), StreamHandlerError> {
        // Close the recv stream to tell the sender to stop.
        recv.stop(VarInt::from_u32(error_code.unwrap_or(0)))
            .map_err(StreamHandlerError::UnknownStream)?;
        Ok(())
    }
}
