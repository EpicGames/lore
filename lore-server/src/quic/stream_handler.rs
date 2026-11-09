// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::lore_spawn_core;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::runtime::execution_context;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::chunking::ChunkingMetric;
use lore_transport::quic::chunking::ParallelChunking;
use lore_transport::quic::command_header::CommandHeader;
use quinn::ClosedStream;
use quinn::ReadError;
use quinn::RecvStream;
use quinn::SendStream;
use quinn::VarInt;
use quinn::WriteError;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info_span;
use tracing::trace;
use tracing::warn;

use crate::protocol::attribute_map::AttributeMap;
use crate::quic::ProtocolErrorInfo;
use crate::quic::QuicErrorStatus;
use crate::quic::QuicService;
use crate::quic::StreamDataHandler;
use crate::quic::StreamHandlerError;
use crate::quic::stream_observer::MessageFailure;
use crate::quic::stream_observer::MessageHandling;
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

/// Limits governing how much work a connection may have in progress.
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
            service_metrics,
            stream_metrics,
        }
    }

    fn handle_write_error(e: WriteError) -> Result<(), StreamHandlerError> {
        match &e {
            WriteError::Stopped(error_code) => {
                warn!(error_code = %error_code, "Peer closed stream during write");
                Ok(())
            }
            // Peer closed the connection gracefully (CONNECTION_CLOSE, app code 0) while
            // we were still writing a response. Mirrors is_graceful_close on the read
            // path so in-flight handle_message tasks don't warn per racing response.
            WriteError::ConnectionLost(quinn::ConnectionError::ApplicationClosed(close))
                if close.error_code == VarInt::from_u32(0) =>
            {
                debug!("Peer closed connection gracefully during write");
                Ok(())
            }
            WriteError::ConnectionLost(err) => {
                warn!(error = %e, cause = %err, "Stream write failed");
                Err(StreamHandlerError::WriteFailed(e))
            }
            _ => {
                warn!(error = %e, "Stream write failed");
                Err(StreamHandlerError::WriteFailed(e))
            }
        }
    }

    /// Answer a request rejected before its handler ran, recording `reason` as the metric label
    /// for what turned it away.
    async fn reject(
        &self,
        header: CommandHeader,
        send: Arc<Mutex<SendStream>>,
        code: QuicServiceError,
        reason: &'static str,
        waited: Duration,
    ) {
        if let Err(err) = Self::send_response(
            send,
            Err((
                header,
                ProtocolErrorInfo {
                    response_error_code: code as QuicErrorStatus,
                    message_handle_label: reason,
                    is_internal_error: false,
                    is_appropriate_for_logging: false,
                },
            )),
        )
        .await
        {
            warn!(reason, error = %err, "Failed sending rejection response");
        }

        let _ = self
            .service_metrics
            .try_send(ServiceMetricEvent::MessageHandling(MessageHandling {
                elapsed: waited,
                opcode: header.cmd,
                error_info: Some(MessageFailure {
                    classification: HANDLE_MESSAGE_USER_ERROR_LABEL_VALUE,
                    error_label: reason,
                }),
            }))
            .inspect_err(|err| error!(?err, "failed to send service reject metric"));
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
        send: Arc<Mutex<SendStream>>,
        limiter: &Arc<Semaphore>,
        start: Instant,
    ) -> Result<(), StreamHandlerError> {
        let handler_duration_timeout = self.handler_duration_timeout;

        let Some(admission) = AdmissionGuard::take(&self.admission, self.inflight_limit) else {
            self.reject(
                header,
                send,
                QuicServiceError::SlowDown,
                ADMISSION_LIMIT_LABEL_VALUE,
                start.elapsed(),
            )
            .await;
            return Ok(());
        };

        let Some(permit) = self.acquire_permit(limiter).await else {
            self.reject(
                header,
                send,
                QuicServiceError::SlowDown,
                PERMIT_TIMEOUT_LABEL_VALUE,
                start.elapsed(),
            )
            .await;
            return Ok(());
        };

        let context = self.context.clone();
        let metrics_sender = self.service_metrics.clone();
        let service = self.service.clone();
        let request_span = service.build_request_span(&header, &message, &context);
        let fut = Box::pin(async move {
            let result = tokio::select! {
                _timeout = tokio::time::sleep(handler_duration_timeout.saturating_sub(start.elapsed())) => {
                    debug!("Timeout in message handling duration");
                    Err(ProtocolErrorInfo::timed_out())
                },
                result = async {
                    service.run_request_handler(context, message).await.map_err(|protocol_error| {
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

            let elapsed = start.elapsed();
            let success = result.is_ok();
            if let Err(err) = Self::send_response(send, result).await {
                if success {
                    warn!(error = %err, "Failed to send response after successful message handling");
                } else {
                    warn!(error = %err, "Failed to send response after failed message handling");
                }
            }

            drop(permit);
            drop(admission);

            let _ = metrics_sender
                .try_send(ServiceMetricEvent::MessageHandling(MessageHandling {
                    elapsed,
                    opcode: header.cmd,
                    error_info,
                }))
                .inspect_err(|err| error!(?err, "failed to send service handle metric"));
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
        send: Arc<Mutex<SendStream>>,
        limiter: &Arc<Semaphore>,
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
                    send,
                    QuicServiceError::InvalidCommand,
                    PARSE_ERROR_LABEL_VALUE,
                    start.elapsed(),
                )
                .await;
                Ok(())
            }
            Ok(message) => {
                trace!("Parsed message: {message:?}, sending off for processing",);
                self.handle_message(header, message, send, limiter, start)
                    .await
            }
        }
    }

    async fn send_response(
        send: Arc<Mutex<SendStream>>,
        result: MessageProcessingResult,
    ) -> Result<(), StreamHandlerError> {
        match result {
            Ok((header, mut data)) => {
                let total_length: usize = data.iter().map(|chunk| chunk.len()).sum();
                debug!(
                    "Successfully handled request for {header:?}. response has {} bytes",
                    total_length
                );

                let response_header = header.response_success(total_length as u32);
                let (header_buf, header_len) = response_header.response_bytes();

                let mut chunks = Vec::with_capacity(1 + data.len());
                chunks.push(Bytes::copy_from_slice(&header_buf[..header_len]));
                chunks.append(&mut data);

                send.lock()
                    .await
                    .write_all_chunks(chunks.as_mut_slice())
                    .instrument(info_span!("send_response"))
                    .await
                    .map(|_| ())
                    .or_else(Self::handle_write_error)?;

                trace!(
                    "Message for command: {header:?} was handled successfully, sent {} bytes of data in response",
                    total_length,
                );
            }
            Err((header, error)) => {
                error.log(&header);
                let response_header = header.response_error(error.response_error_code);
                let (header_buf, header_len) = response_header.response_bytes();
                send.lock()
                    .await
                    .write_all(&header_buf[..header_len])
                    .await
                    .map_err(StreamHandlerError::WriteFailed)?;
                trace!("Wrote error response header: {response_header:?}");
            }
        }

        Ok(())
    }
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

        let send = Arc::new(Mutex::new(send));
        let limiter = Arc::new(Semaphore::new(self.process_limit));
        // Kept across the loop so a chunk carrying several requests costs no allocation.
        let mut messages = Vec::new();

        loop {
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

                self.process_message(message.header, message.payload, send.clone(), &limiter)
                    .await?;
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

        debug!("Sending finish");
        send.lock().await.finish().or_else(|e: ClosedStream| {
            debug!("Received closed stream error when sending finish: {e:?}",);
            Self::handle_write_error(WriteError::ClosedStream)
        })?;

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
