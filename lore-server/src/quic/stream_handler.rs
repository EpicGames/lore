// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::runtime::execution_context;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::create_operation_context_attribute;
use lore_telemetry::drop_record::DropRecord;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::command_header::COMMAND_HEADER_SIZE_V4;
use lore_transport::quic::command_header::CommandHeader;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use quinn::ClosedStream;
use quinn::ReadError;
use quinn::RecvStream;
use quinn::SendStream;
use quinn::VarInt;
use quinn::WriteError;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tracing::Instrument;
use tracing::debug;
use tracing::info;
use tracing::info_span;
use tracing::trace;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::protocol::attribute_map::AttributeMap;
use crate::quic::ProtocolErrorInfo;
use crate::quic::QuicErrorStatus;
use crate::quic::QuicService;
use crate::quic::StreamDataHandler;
use crate::quic::StreamHandlerError;

const SERVICE_LABEL_KEY: &str = "quic_service_name";
const OPCODE_LABEL_KEY: &str = "opcode";
const HANDLER_ERROR_LABEL_KEY: &str = "handler_error";
const HANDLER_ERROR_CLASSIFICATION_LABEL_KEY: &str = "error_classification";

const PERMIT_TIMEOUT_LABEL_VALUE: &str = "PermitTimeout";
// todo(plockhart) differentiate this label value and add to alerting rules
const HANDLE_MESSAGE_TIMEOUT_LABEL_VALUE: &str = "SlowDown";
const HANDLE_MESSAGE_USER_ERROR_LABEL_VALUE: &str = "User";
const HANDLE_MESSAGE_INTERNAL_ERROR_LABEL_VALUE: &str = "Internal";

fn is_graceful_close(err: &ReadError) -> bool {
    match err {
        ReadError::ConnectionLost(quinn::ConnectionError::LocallyClosed) => true,
        ReadError::ConnectionLost(quinn::ConnectionError::ApplicationClosed(close)) => {
            close.error_code == VarInt::from_u32(0)
        }
        _ => false,
    }
}

struct StreamHandlerInstrumentProvider;

impl InstrumentProvider for StreamHandlerInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.quic.stream_handler"
    }
}

pub struct StreamHandler<ServiceType>
where
    ServiceType: QuicService,
{
    service: Arc<ServiceType>,
    context: Arc<AttributeMap>,
    limiter: Arc<Semaphore>,
    handler_duration_timeout: Option<Duration>,
    latency_histogram: Histogram<f64>,
    peak_pending_chunks_histogram: Histogram<u64>,
    pending_chunk_stall_histogram: Histogram<u64>,
}

impl<ServiceType> StreamHandler<ServiceType>
where
    ServiceType: QuicService,
{
    pub fn new(
        service: Arc<ServiceType>,
        context: Arc<AttributeMap>,
        process_limit: usize,
        handler_duration_timeout: Option<Duration>,
    ) -> Self {
        let provider = StreamHandlerInstrumentProvider;
        let latency_histogram =
            provider.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME);
        let peak_pending_chunks_histogram = provider.length_histogram(
            "stream.peak_pending_chunks",
            vec![
                1., 5., 10., 25., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000., 25_000.,
                50_000., 100_000.,
            ],
        );
        let pending_chunk_stall_histogram = provider.length_histogram(
            "stream.peak_pending_chunk_stall_duration",
            vec![
                1., 5., 10., 25., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000.,
            ],
        );
        Self {
            service,
            context,
            limiter: Arc::new(Semaphore::new(process_limit)),
            handler_duration_timeout,
            latency_histogram,
            peak_pending_chunks_histogram,
            pending_chunk_stall_histogram,
        }
    }

    fn handle_write_error(e: WriteError) -> Result<(), StreamHandlerError> {
        match &e {
            WriteError::Stopped(error_code) => {
                warn!("Peer closed stream during write (error code: {error_code})");
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
                warn!("Stream write failed: {e} {err}");
                Err(StreamHandlerError::WriteFailed(e))
            }
            _ => {
                warn!("Stream write failed: {e}");
                Err(StreamHandlerError::WriteFailed(e))
            }
        }
    }

    async fn handle_message(
        &self,
        header: CommandHeader,
        message: ServiceType::ParsedRequestType,
        send: Arc<Mutex<SendStream>>,
    ) -> Result<(), StreamHandlerError> {
        let context = self.context.clone();
        let handler_duration_timeout = self
            .handler_duration_timeout
            .unwrap_or(Duration::from_secs(60 * 60));

        let start = Instant::now();

        let histogram = self.latency_histogram.clone();

        let permit = match tokio::select! {
            _timeout = tokio::time::sleep(handler_duration_timeout) => {
                debug!("Timeout in getting permit for message handling");
                Err(QuicServiceError::SlowDown)
            },
            permit = self.limiter.clone().acquire_owned() => match permit {
                Ok(permit) => Ok(permit),
                Err(err) => {
                    warn!("Acquire stream handler permit failed: {err}");
                    Err(QuicServiceError::SlowDown)
                }
            }
        } {
            Ok(permit) => permit,
            Err(_err) => {
                if let Err(err) = Self::send_response(
                    send,
                    Err((
                        header,
                        ProtocolErrorInfo {
                            response_error_code: QuicServiceError::SlowDown as QuicErrorStatus,
                            message_handle_label: PERMIT_TIMEOUT_LABEL_VALUE,
                            is_internal_error: false,
                            is_appropriate_for_logging: false,
                        },
                    )),
                )
                .await
                {
                    warn!("Failed sending acquire permit timeout response: {err}");
                }

                histogram.record(
                    start.elapsed().as_millis() as f64,
                    // QUIC is high throughput, and we want to keep memory allocations to a minimum.
                    // We specifically don't use get_labels from the Instrument Provider as that
                    // would involve multiple allocations to combine arrays of labels together
                    &[
                        KeyValue::new(SERVICE_LABEL_KEY, self.service.get_service_name_label()),
                        KeyValue::new(
                            OPCODE_LABEL_KEY,
                            self.service.command_to_metrics_label(header.cmd),
                        ),
                        KeyValue::new("success", false),
                        KeyValue::new(HANDLER_ERROR_LABEL_KEY, PERMIT_TIMEOUT_LABEL_VALUE),
                        KeyValue::new(
                            HANDLER_ERROR_CLASSIFICATION_LABEL_KEY,
                            HANDLE_MESSAGE_USER_ERROR_LABEL_VALUE,
                        ),
                    ],
                );

                return Ok(());
            }
        };

        let service = self.service.clone();
        let request_span = service.build_request_span(&header, &message, &context);
        let fut = Box::pin(async move {
            let result = tokio::select! {
                _timeout = tokio::time::sleep(handler_duration_timeout.saturating_sub(start.elapsed())) => {
                    debug!("Timeout in message handling duration");
                    Err(ProtocolErrorInfo {
                        response_error_code: QuicServiceError::SlowDown as QuicErrorStatus,
                        message_handle_label: HANDLE_MESSAGE_TIMEOUT_LABEL_VALUE,
                        is_internal_error: true,
                        is_appropriate_for_logging: true,
                    })
                },
                result = async {
                    service.run_request_handler(context, message).await.map_err(|protocol_error| {
                        service.transform_protocol_error(&protocol_error)
                    })
                } => result
            };

            let is_handler_successful;
            let handler_error_label_value;
            let error_classification_label;
            if let Err(err) = &result {
                is_handler_successful = false;
                handler_error_label_value = err.message_handle_label;
                error_classification_label = if err.is_internal_error {
                    HANDLE_MESSAGE_INTERNAL_ERROR_LABEL_VALUE
                } else {
                    HANDLE_MESSAGE_USER_ERROR_LABEL_VALUE
                };
            } else {
                is_handler_successful = true;
                handler_error_label_value = "";
                error_classification_label = "";
            }

            // QUIC is high throughput, and we want to keep memory allocations to a minimum.
            // We specifically don't use get_labels from the Instrument Provider as that
            // would involve multiple allocations to combine arrays of labels together
            let labels = [
                KeyValue::new(SERVICE_LABEL_KEY, service.get_service_name_label()),
                create_operation_context_attribute("handle_message"),
                KeyValue::new(
                    OPCODE_LABEL_KEY,
                    service.command_to_metrics_label(header.cmd),
                ),
                KeyValue::new("success", is_handler_successful),
                // keep error labels below this comment and exclude via count
                KeyValue::new(
                    HANDLER_ERROR_CLASSIFICATION_LABEL_KEY,
                    error_classification_label,
                ),
                KeyValue::new(HANDLER_ERROR_LABEL_KEY, handler_error_label_value),
            ];
            let label_count = if is_handler_successful {
                labels.len() - 2
            } else {
                labels.len()
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
                    warn!("Failed to send response after successful message handling: {err}");
                } else {
                    warn!("Failed to send response after failed message handling: {err}");
                }
            }

            // We can't use the `timed!` macro because we're just firing this off as a task without
            // awaiting the result
            histogram.record(elapsed.as_millis() as f64, &labels[..label_count]);

            drop(permit);
        });
        tokio::spawn(LORE_CONTEXT.scope(execution_context(), fut.instrument(request_span)));

        Ok(())
    }

    async fn process_message(
        &self,
        header: CommandHeader,
        payload: Option<bytes::Bytes>,
        send: Arc<Mutex<SendStream>>,
    ) -> Result<(), StreamHandlerError> {
        let parse_result = self
            .service
            .parse_request_bytes(&header, payload.unwrap_or_default());
        trace!("Parse result for command: {header:?} as: {parse_result:?}");

        match parse_result {
            Err(e) => {
                warn!("Message processing failed for request {header:?}: {e}");
                if let Err(send_error) = Self::send_response(
                    send,
                    Err((
                        header,
                        ProtocolErrorInfo {
                            response_error_code: QuicServiceError::InvalidCommand
                                as QuicErrorStatus,
                            message_handle_label: "ParsingError",
                            is_internal_error: false,
                            is_appropriate_for_logging: false,
                        },
                    )),
                )
                .await
                {
                    warn!(
                        "Failed to send response to channel for failed message handling: {send_error}"
                    );
                }
                Ok(())
            }
            Ok(message) => {
                trace!("Parsed message: {message:?}, sending off for processing",);
                self.handle_message(header, message, send).await
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
                if error.is_appropriate_for_logging {
                    if error.is_internal_error {
                        warn!(command_header = ?header, handler_error_label = error.message_handle_label, response_error_code = error.response_error_code, "internal error handling message");
                    } else {
                        info!(command_header = ?header, handler_error_label = error.message_handle_label, response_error_code = error.response_error_code, "non-internal error handling message");
                    }
                } else {
                    if error.is_internal_error {
                        debug!(command_header = ?header, handler_error_label = error.message_handle_label, response_error_code = error.response_error_code, "internal error handling message");
                    } else {
                        debug!(command_header = ?header, handler_error_label = error.message_handle_label, response_error_code = error.response_error_code, "non-internal error handling message");
                    }
                }
                let response_header = header.response_error(error.response_error_code);
                let (header_buf, header_len) = response_header.response_bytes();
                send.lock()
                    .await
                    .write(&header_buf[..header_len])
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
        let mut request = CommandHeader::default();
        let mut payload: Option<bytes::BytesMut> = None;

        let header_size = self.service.header_size();
        let mut request_bytes = [0u8; COMMAND_HEADER_SIZE_V4];
        let mut request_bytes_read = 0;

        let mut current_offset = 0u64;
        let mut next_chunk: Option<quinn::Chunk> = None;
        let mut pending_chunks = vec![];

        let labels = [KeyValue::new(
            SERVICE_LABEL_KEY,
            self.service.get_service_name_label(),
        )];
        let mut peak_pending = DropRecord::new(self.peak_pending_chunks_histogram.clone(), &labels);
        let mut max_pending: usize = 0;
        let mut peak_stall = DropRecord::new(self.pending_chunk_stall_histogram.clone(), &labels);
        let mut stall_start = Instant::now();

        let send = Arc::new(Mutex::new(send));

        loop {
            if next_chunk.is_none() {
                next_chunk = match recv.read_chunk(self.service.max_chunk_size(), false).await {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        if is_graceful_close(&err) {
                            return Ok(());
                        }
                        return Err(StreamHandlerError::StreamReadError(err));
                    }
                };
            }

            let Some(mut chunk) = next_chunk.take() else {
                debug!("Terminating request reader in stream handler");
                break;
            };

            if chunk.offset == current_offset {
                while !chunk.bytes.is_empty() {
                    if request_bytes_read < header_size {
                        // Read the request header
                        if chunk.bytes.len() + request_bytes_read < header_size {
                            let got_count = chunk.bytes.len();
                            request_bytes[request_bytes_read..(request_bytes_read + got_count)]
                                .copy_from_slice(chunk.bytes.as_ref());

                            request_bytes_read += got_count;
                            current_offset += got_count as u64;
                            chunk.bytes.clear();
                        } else {
                            let remain_count = header_size - request_bytes_read;
                            let remain_bytes = chunk.bytes.split_to(remain_count);

                            request_bytes[request_bytes_read..header_size]
                                .copy_from_slice(remain_bytes.as_ref());

                            request = if header_size == COMMAND_HEADER_SIZE_V4 {
                                CommandHeader::from_bytes_v4(request_bytes.as_bytes())
                            } else {
                                CommandHeader::from_bytes(request_bytes.as_bytes())
                            };
                            if request.size_or_status > self.service.max_chunk_size() as u32 {
                                warn!("Bad header {request:?}");
                                return Err(StreamHandlerError::BadHeader(request));
                            }

                            request_bytes_read = header_size;
                            current_offset += remain_count as u64;
                            chunk.offset += remain_count as u64;

                            trace!("QUIC stream read request header {request:?}");

                            if request.size_or_status > 0 {
                                if chunk.bytes.len() >= request.size_or_status as usize {
                                    // Happy path, we can directly use buffer as it contains the full request
                                    let size = request.size_or_status as usize;
                                    let current_payload = chunk.bytes.split_to(size);

                                    current_offset += size as u64;
                                    chunk.offset += size as u64;

                                    trace!(
                                        "QUIC stream read {} bytes complete payload from single chunk",
                                        size
                                    );

                                    request_bytes_read = 0;

                                    self.process_message(
                                        request,
                                        Some(current_payload),
                                        send.clone(),
                                    )
                                    .await?;
                                } else {
                                    // Allocate buffer for request payload
                                    payload = Some(bytes::BytesMut::with_capacity(
                                        request.size_or_status as usize,
                                    ));
                                }
                            } else {
                                request_bytes_read = 0;

                                self.process_message(request, None, send.clone()).await?;
                            }
                        }
                    }
                    if let Some(mut current_payload) = payload.take() {
                        let size = std::cmp::min(
                            current_payload.capacity() - current_payload.len(),
                            chunk.bytes.len(),
                        );

                        let this_chunk = chunk.bytes.split_to(size);
                        current_payload.extend_from_slice(this_chunk.as_bytes());

                        current_offset += size as u64;
                        chunk.offset += size as u64;

                        trace!(
                            "QUIC stream read {} bytes for a total of {} / {} bytes of payload",
                            size,
                            current_payload.len(),
                            current_payload.capacity()
                        );

                        if current_payload.capacity() == current_payload.len() {
                            request_bytes_read = 0;

                            self.process_message(
                                request,
                                Some(current_payload.freeze()),
                                send.clone(),
                            )
                            .await?;
                        } else {
                            payload = Some(current_payload);
                        }
                    }
                }
            } else {
                // Queue for later processing
                trace!(
                    "Got out of order chunk @ offset {}, current offset is {}",
                    chunk.offset, current_offset
                );
                if pending_chunks.is_empty() {
                    stall_start = Instant::now();
                }
                pending_chunks.push(chunk);
                if pending_chunks.len() > max_pending {
                    peak_pending.add((pending_chunks.len() - max_pending) as u64);
                    max_pending = pending_chunks.len();
                }
            }

            for (ichunk, chunk) in pending_chunks.iter().enumerate() {
                if chunk.offset == current_offset {
                    trace!(
                        "Grab out of order chunk @ current offset {current_offset} - {} ooo chunks remaining",
                        pending_chunks.len() - 1
                    );
                    next_chunk = Some(pending_chunks.swap_remove(ichunk));
                    if pending_chunks.is_empty() {
                        let stall_ms = stall_start.elapsed().as_millis() as u64;
                        if stall_ms > peak_stall.get() {
                            peak_stall.set(stall_ms);
                        }
                    }
                    break;
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::net::UdpSocket;
    use std::path::PathBuf;
    use std::time::Duration;

    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::fragment::generate_random;
    use lore_storage::StoreMatch;
    use lore_transport::quic::QuicOpCode;
    use lore_transport::quic::client::CertificateSettings;
    use lore_transport::quic::client::ClientCerts;
    use lore_transport::quic::client::CongestionAlgorithm;
    use lore_transport::quic::client::DEFAULT_EXPECTED_RTT_MS;
    use lore_transport::quic::client::TransportConfig;
    use lore_transport::quic::client::insecure_client_auth;
    use lore_transport::quic::storage_service::Command;
    use quinn::ClientConfig;
    use quinn::ConnectionError;
    use quinn::Endpoint;
    use quinn::ReadExactError;
    use quinn::crypto::rustls::QuicClientConfig;
    use rand::random;
    use tokio::io::AsyncWriteExt;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::protocol::replication_store::get::Get;
    use crate::protocol::replication_store::header::ReplicationHeader;
    use crate::quic::StreamHandlerFactory;
    use crate::quic::quinn::QuinnConfigBuilder;
    use crate::quic::quinn::QuinnServer;
    use crate::quic::quinn::build_cert_verifier;
    use crate::quic::quinn::service_store::ServiceStore;
    use crate::quic::quinn::service_store::StreamDataHandlerBuilder;
    use crate::quic::replication_store_service::ReplicationServiceErrorCode;
    use crate::quic::replication_store_service::client::CommandBehavior;
    use crate::quic::replication_store_service::client::ReplicationStoreClient;
    use crate::quic::replication_store_service::client::ReplicationStoreClientError;
    use crate::quic::replication_store_service::client::StoreClient;
    use crate::quic::tests::TEST_PROTOCOL;
    use crate::quic::tests::TEST_PROTOCOL_V4;
    use crate::quic::tests::TestHandlerFactory;
    use crate::quic::tests::server_certs;
    use crate::quic::tests::test_data_path;
    use crate::store::test_store_create;

    fn untrusted_client_cert_paths() -> anyhow::Result<(PathBuf, PathBuf)> {
        let path = test_data_path();
        let cert = path.join("untrusted_cert.pem");
        let key = path.join("untrusted_key.pem");
        Ok((cert, key))
    }

    fn trusted_client_cert_paths() -> anyhow::Result<(PathBuf, PathBuf)> {
        let path = test_data_path();
        let cert = path.join("test_client_cert.pem");
        let key = path.join("test_client_key.pem");
        Ok((cert, key))
    }

    #[tokio::test]
    async fn test_command() {
        let repository = random::<Context>();

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
                // order to test the stream handler we need to spin up an actual server instance.

                // Find an available port.
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

                let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

                let _server = QuinnServer::start(
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path)
                        .pkey_file(key_path)
                        .stream_handler_factory(Box::new(TestHandlerFactory::new(
                            immutable_store,
                            mutable_store.clone(),
                        )))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                let mut crypto_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(
                        insecure_client_auth::SkipServerVerification::new(),
                    )
                    .with_no_client_auth();

                crypto_config.alpn_protocols = [TEST_PROTOCOL]
                    .iter()
                    .map(|alpn| alpn.as_bytes().into())
                    .collect();

                let client_config = ClientConfig::new(Arc::new(
                    QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
                ));

                let mut endpoint =
                    Endpoint::client(client_addr).expect("Failed to create client endpoint");
                endpoint.set_default_client_config(client_config);

                // connect to server
                let connection = endpoint
                    .connect(server_addr, "localhost")
                    .unwrap()
                    .await
                    .unwrap();

                let (mut send, mut recv) = connection
                    .open_bi()
                    .await
                    .expect("Failed to setup bidirectional channel");

                let token = "some-token";
                let token_bytes = token.as_bytes();

                let header = CommandHeader::new(
                    Command::Authorize as QuicOpCode,
                    random::<u32>(),
                    size_of::<Context>() + token_bytes.len(),
                );

                let header_bytes = header.to_bytes();
                // Send the first 4 bytes to simulate a partial header.
                send.write(&header_bytes[..4])
                    .await
                    .expect("Failed to write header");
                send.flush().await.expect("Failed flush");

                // Wait a tick to let the server receive the message.
                tokio::time::sleep(Duration::from_millis(1)).await;

                let mut data = bytes::BytesMut::new();
                data.extend_from_slice(&header_bytes[4..]);
                data.extend_from_slice(repository.as_bytes());
                data.extend_from_slice(token_bytes);

                // Send the rest of the header and the payload as well.
                send.write(data.to_vec().as_slice())
                    .await
                    .expect("Failed to write data");
                send.flush().await.expect("Failed flush");

                // Now try and read the response.
                let mut response_buffer = [0u8; 8];
                recv.read_exact(&mut response_buffer)
                    .await
                    .expect("Failed to read response");

                let response_header = CommandHeader::from_bytes(&response_buffer);

                assert_eq!(header.response_success(0), response_header);

                // Close the client side of the stream.
                send.finish().expect("Failed to finish stream");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn server_with_mtls_rejects_clients_without_certs() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
                // order to test the stream handler we need to spin up an actual server instance.

                // Find an available port.
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

                let (cert_path, key_path, ca_cert) = server_certs().expect("Bad cert paths");

                let _server = QuinnServer::start(
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path)
                        .pkey_file(key_path)
                        .cert_chain(Some(ca_cert.clone()))
                        .client_cert_verifier(
                            build_cert_verifier(ca_cert).expect("Failed client cert verifier"),
                        )
                        .stream_handler_factory(Box::new(TestHandlerFactory::new(
                            immutable_store,
                            mutable_store.clone(),
                        )))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                let mut crypto_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(
                        insecure_client_auth::SkipServerVerification::new(),
                    )
                    // no certs provided - we should be rejected
                    .with_no_client_auth();

                crypto_config.alpn_protocols = [TEST_PROTOCOL]
                    .iter()
                    .map(|alpn| alpn.as_bytes().into())
                    .collect();

                let client_config = ClientConfig::new(Arc::new(
                    QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
                ));

                let mut endpoint =
                    Endpoint::client(client_addr).expect("Failed to create client endpoint");
                endpoint.set_default_client_config(client_config);

                // it is expected the client can 'connect', as under the hood
                // the server client TLS handshake is still occurring
                let connection = endpoint
                    .connect(server_addr, "localhost")
                    .unwrap()
                    .await
                    .unwrap();
                let (mut send, mut recv) = connection
                    .open_bi()
                    .await
                    .expect("Failed to setup bidirectional channel");

                // but when we try to do something with the connection (like receive data)
                // eventually the TLS handshake will have finished and reject our connection
                let mut response_buffer = [0u8; 8];
                let error = recv
                    .read_exact(&mut response_buffer)
                    .await
                    .expect_err("receive should have failed");
                let ReadExactError::ReadError(read_error) = error else {
                    panic!("Unexpected error type {error:?}");
                };
                let ReadError::ConnectionLost(connection_lost) = read_error else {
                    panic!("Unexpected read error {read_error:?}");
                };
                let ConnectionError::ConnectionClosed(closed_error) = connection_lost else {
                    panic!("Unexpected connection lost error {connection_lost:?}");
                };
                assert_eq!(closed_error.reason, "peer sent no certificates");

                // Close the client side of the stream.
                send.finish().expect("Failed to finish stream");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn server_with_mtls_accepts_clients_with_certs() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
                // order to test the stream handler we need to spin up an actual server instance.

                // Find an available port.
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let (cert_path, key_path, ca_cert) = server_certs().expect("Bad cert paths");
                let (client_cert, client_key) =
                    trusted_client_cert_paths().expect("Bad client cert paths");

                let _server = QuinnServer::start(
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path)
                        .pkey_file(key_path)
                        .cert_chain(Some(ca_cert.clone()))
                        .client_cert_verifier(
                            build_cert_verifier(ca_cert.clone())
                                .expect("Failed client cert verifier"),
                        )
                        .stream_handler_factory(Box::new(TestHandlerFactory::new(
                            immutable_store,
                            mutable_store.clone(),
                        )))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                // `s` suffix so the server's certificate is validated
                let remote_url = format!("quics://{server_addr}");
                let client = ReplicationStoreClient::connect(
                    &remote_url,
                    CertificateSettings {
                        custom_ca: Some(ca_cert),
                        client: Some(ClientCerts {
                            cert_file: client_cert,
                            pkey_file: client_key,
                        }),
                    },
                    None,
                    TransportConfig {
                        max_bytes_bandwidth_per_second: 1_000_000,
                        expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                        congestion_algorithm: CongestionAlgorithm::Bbr,
                        initial_cwnd: None,
                    },
                    CommandBehavior {
                        message_limit: 10,
                        should_await_command_permit: false,
                    },
                    None,
                )
                .await
                .expect("Failed to establish client connection");

                // requests should be handled gracefully
                let (_, address, _) = generate_random();
                let client_error = client
                    .get(Get {
                        header: ReplicationHeader {
                            correlation_id: Default::default(),
                            repository: random(),
                        },
                        address,
                        match_required: StoreMatch::MatchFull,
                    })
                    .await
                    .expect_err("Failed to get request");
                assert!(matches!(
                    client_error,
                    ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::AddressNotFound
                    )
                ));
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn server_with_mtls_rejects_clients_with_invalid_certs() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
                // order to test the stream handler we need to spin up an actual server instance.

                // Find an available port.
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let (cert_path, key_path, ca_cert) = server_certs().expect("Bad cert paths");
                let (untrusted_cert, untrusted_key) =
                    untrusted_client_cert_paths().expect("Bad untrusted cert paths");

                let _server = QuinnServer::start(
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path.clone())
                        .pkey_file(key_path.clone())
                        .cert_chain(Some(ca_cert.clone()))
                        .client_cert_verifier(
                            build_cert_verifier(ca_cert.clone())
                                .expect("Failed client cert verifier"),
                        )
                        .stream_handler_factory(Box::new(TestHandlerFactory::new(
                            immutable_store,
                            mutable_store.clone(),
                        )))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                let remote_url = format!("quic://{server_addr}");
                let client = ReplicationStoreClient::connect(
                    &remote_url,
                    CertificateSettings {
                        custom_ca: None,
                        client: Some(ClientCerts {
                            cert_file: untrusted_cert,
                            pkey_file: untrusted_key,
                        }),
                    },
                    None,
                    TransportConfig {
                        max_bytes_bandwidth_per_second: 1_000_000,
                        expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                        congestion_algorithm: CongestionAlgorithm::Bbr,
                        initial_cwnd: None,
                    },
                    CommandBehavior {
                        message_limit: 10,
                        should_await_command_permit: false,
                    },
                    None,
                )
                .await
                .expect("Failed to establish client connection");

                // requests should be handled gracefully
                let (_, address, _) = generate_random();
                let client_error = client
                    .get(Get {
                        header: ReplicationHeader {
                            correlation_id: Default::default(),
                            repository: random(),
                        },
                        address,
                        match_required: StoreMatch::MatchFull,
                    })
                    .await
                    .expect_err("Failed to get request");
                assert!(matches!(
                    client_error,
                    ReplicationStoreClientError::ConnectionFailed
                ));
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn server_without_mtls_accepts_clients_without_certs() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
                // order to test the stream handler we need to spin up an actual server instance.

                // Find an available port.
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

                let _server = QuinnServer::start(
                    // no client_cert_verifier which defaults to NoClientAuth verifier
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path.clone())
                        .pkey_file(key_path.clone())
                        .stream_handler_factory(Box::new(TestHandlerFactory::new(
                            immutable_store,
                            mutable_store.clone(),
                        )))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                let remote_url = format!("quic://{server_addr}");
                let client = ReplicationStoreClient::connect(
                    &remote_url,
                    CertificateSettings {
                        custom_ca: None,
                        client: None,
                    },
                    None,
                    TransportConfig {
                        max_bytes_bandwidth_per_second: 1_000_000,
                        expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                        congestion_algorithm: CongestionAlgorithm::Bbr,
                        initial_cwnd: None,
                    },
                    CommandBehavior {
                        message_limit: 10,
                        should_await_command_permit: false,
                    },
                    None,
                )
                .await
                .expect("Failed to establish client connection");

                // requests should be handled gracefully
                let (_, address, _) = generate_random();
                let client_error = client
                    .get(Get {
                        header: ReplicationHeader {
                            correlation_id: Default::default(),
                            repository: random(),
                        },
                        address,
                        match_required: StoreMatch::MatchFull,
                    })
                    .await
                    .expect_err("Failed to get request");
                assert!(matches!(
                    client_error,
                    ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::AddressNotFound
                    )
                ));
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn unsupported_protocol_rejects_client() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                // Unfortunately, there's no way to mock or otherwise fake Quinn Send/Recv streams, so in
                // order to test the stream handler we need to spin up an actual server instance.

                // Find an available port.
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

                let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

                let _server = QuinnServer::start(
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path)
                        .pkey_file(key_path)
                        .stream_handler_factory(Box::new(TestHandlerFactory::new(
                            immutable_store,
                            mutable_store.clone(),
                        )))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                let mut crypto_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(
                        insecure_client_auth::SkipServerVerification::new(),
                    )
                    .with_no_client_auth();

                crypto_config.alpn_protocols = ["no-test/0.2"]
                    .iter()
                    .map(|alpn| alpn.as_bytes().into())
                    .collect();

                let client_config = ClientConfig::new(Arc::new(
                    QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
                ));

                let mut endpoint =
                    Endpoint::client(client_addr).expect("Failed to create client endpoint");
                endpoint.set_default_client_config(client_config);

                let connection_error = endpoint
                    .connect(server_addr, "localhost")
                    .unwrap()
                    .await
                    .unwrap_err();
                let ConnectionError::ConnectionClosed(frame) = connection_error else {
                    panic!("Unexpected error type {connection_error:?}");
                };
                assert_eq!(frame.reason, "peer doesn't support any known protocol");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn handler_returns_error_when_response_exceeds_max_chunk_size() {
        use lore_transport::quic::QuicServiceError;

        const OVERSIZED_PROTOCOL: &str = "oversized-test/0.1";
        const OVERSIZED_MAX_CHUNK: usize = 4096;

        struct OversizedResponseService;

        #[derive(Debug)]
        struct SimpleRequest;

        #[derive(Debug, thiserror::Error)]
        #[error("mock error")]
        struct MockError;

        #[async_trait]
        impl QuicService for OversizedResponseService {
            type ParsedRequestType = SimpleRequest;
            type RequestParseErrorType = MockError;
            type RequestHandlerError = MockError;

            fn get_service_name_label(&self) -> &'static str {
                "oversized_test"
            }

            fn parse_request_bytes(
                &self,
                _header: &CommandHeader,
                _bytes: Bytes,
            ) -> Result<SimpleRequest, MockError> {
                Ok(SimpleRequest)
            }

            async fn run_request_handler(
                &self,
                _context: Arc<AttributeMap>,
                _request: SimpleRequest,
            ) -> Result<Vec<Bytes>, MockError> {
                // Return a response larger than OVERSIZED_MAX_CHUNK
                Ok(vec![Bytes::from(vec![0xAB; OVERSIZED_MAX_CHUNK + 1])])
            }

            fn command_to_metrics_label(&self, _opcode: QuicOpCode) -> &'static str {
                "test_cmd"
            }

            fn transform_protocol_error(&self, _error: &MockError) -> ProtocolErrorInfo {
                ProtocolErrorInfo {
                    response_error_code: QuicServiceError::Failed as QuicErrorStatus,
                    message_handle_label: "mock_error",
                    is_internal_error: true,
                    is_appropriate_for_logging: true,
                }
            }

            fn max_chunk_size(&self) -> usize {
                OVERSIZED_MAX_CHUNK
            }

            fn build_request_span(
                &self,
                header: &CommandHeader,
                _message: &SimpleRequest,
                _context: &Arc<AttributeMap>,
            ) -> tracing::Span {
                crate::quic::storage_service::build_storage_protocol_request_span(
                    header.cmd,
                    crate::telemetry::StorageProtocol::StorageV0,
                    crate::quic::NO_CONNECTION_ID,
                    crate::quic::NO_REPOSITORY_ID,
                    crate::quic::NO_CORRELATION_ID,
                    crate::quic::NO_USER_ID,
                )
            }
        }

        struct OversizedHandlerFactory {
            service_store: ServiceStore,
        }

        impl OversizedHandlerFactory {
            fn new() -> Self {
                let mut service_store = ServiceStore::default();
                service_store.add_service(
                    OVERSIZED_PROTOCOL,
                    Box::new(move |context: Arc<AttributeMap>| {
                        Box::new(StreamHandler::new(
                            Arc::new(OversizedResponseService),
                            context,
                            100,
                            None,
                        ))
                    }),
                );
                Self { service_store }
            }
        }

        impl StreamHandlerFactory for OversizedHandlerFactory {
            fn supported_protocols(&self) -> Vec<String> {
                self.service_store.get_supported_services()
            }

            fn get_stream_handler_builder(
                &self,
                protocol: &str,
            ) -> Option<(&&'static str, &StreamDataHandlerBuilder)> {
                self.service_store.get_stream_builder(protocol)
            }
        }

        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
                let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

                let _server = QuinnServer::start(
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path)
                        .pkey_file(key_path)
                        .stream_handler_factory(Box::new(OversizedHandlerFactory::new()))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                let mut crypto_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(
                        insecure_client_auth::SkipServerVerification::new(),
                    )
                    .with_no_client_auth();

                crypto_config.alpn_protocols = [OVERSIZED_PROTOCOL]
                    .iter()
                    .map(|alpn| alpn.as_bytes().into())
                    .collect();

                let client_config = ClientConfig::new(Arc::new(
                    QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
                ));

                let mut endpoint =
                    Endpoint::client(client_addr).expect("Failed to create client endpoint");
                endpoint.set_default_client_config(client_config);

                let connection = endpoint
                    .connect(server_addr, "localhost")
                    .unwrap()
                    .await
                    .unwrap();

                let (mut send, mut recv) = connection
                    .open_bi()
                    .await
                    .expect("Failed to setup bidirectional channel");

                // Send a command with no payload — the mock service will produce an oversized response
                let header = CommandHeader::new(1, random::<u32>(), 0);
                send.write(&header.to_bytes())
                    .await
                    .expect("Failed to write header");
                send.flush().await.expect("Failed flush");

                // The handler should detect the oversized response and send back an error
                let mut response_buffer = [0u8; 8];
                recv.read_exact(&mut response_buffer)
                    .await
                    .expect("Failed to read response");

                let response = CommandHeader::from_bytes(&response_buffer);
                assert!(
                    response.error,
                    "Expected error response for oversized message, got: {response:?}"
                );
                assert_eq!(
                    response.size_or_status,
                    QuicServiceError::Failed as u32,
                    "Expected Failed error status for oversized response"
                );

                send.finish().expect("Failed to finish stream");
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test]
    async fn test_v4_authorize_put_get_query_stop() {
        use lore_base::types::Fragment;
        use lore_transport::quic::command_header::COMMAND_HEADER_SIZE_V4;

        let repository = random::<Context>();

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        let (fragment, address, payload) = generate_random();
        let (_, other_address, _) = generate_random();

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                let server_addr = socket.local_addr().expect("Failed socket setup");
                drop(socket);

                let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
                let (cert_path, key_path, _) = server_certs().expect("Bad cert paths");

                let _server = QuinnServer::start(
                    QuinnConfigBuilder::new()
                        .address(server_addr)
                        .cert_file(cert_path)
                        .pkey_file(key_path)
                        .stream_handler_factory(Box::new(TestHandlerFactory::new(
                            immutable_store,
                            mutable_store,
                        )))
                        .build()
                        .unwrap(),
                )
                .expect("Failed Quinn server start");

                let mut crypto_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(
                        insecure_client_auth::SkipServerVerification::new(),
                    )
                    .with_no_client_auth();

                crypto_config.alpn_protocols = [TEST_PROTOCOL_V4]
                    .iter()
                    .map(|alpn| alpn.as_bytes().into())
                    .collect();

                let client_config = ClientConfig::new(Arc::new(
                    QuicClientConfig::try_from(crypto_config).expect("Failed client config"),
                ));

                let mut endpoint =
                    Endpoint::client(client_addr).expect("Failed to create client endpoint");
                endpoint.set_default_client_config(client_config);

                let connection = endpoint
                    .connect(server_addr, "localhost")
                    .unwrap()
                    .await
                    .unwrap();

                let (mut send, mut recv) = connection
                    .open_bi()
                    .await
                    .expect("Failed to setup bidirectional channel");

                let mut cmd_id: u32 = 0;
                let mut next_cmd_id = || {
                    cmd_id += 1;
                    cmd_id
                };

                // Helper: send a v4 command and read the response header
                async fn send_v4_cmd(
                    send: &mut quinn::SendStream,
                    recv: &mut quinn::RecvStream,
                    header: CommandHeader,
                    payload: &[u8],
                ) -> CommandHeader {
                    send.write(&header.to_bytes_v4())
                        .await
                        .expect("write header");
                    send.write(payload).await.expect("write payload");
                    send.flush().await.expect("flush");

                    let mut buf = [0u8; COMMAND_HEADER_SIZE_V4];
                    recv.read_exact(&mut buf).await.expect("read response");
                    CommandHeader::from_bytes_v4(&buf)
                }

                // Helper: read response payload
                async fn read_payload(recv: &mut quinn::RecvStream, len: usize) -> Vec<u8> {
                    let mut buf = vec![0u8; len];
                    recv.read_exact(&mut buf).await.expect("read payload");
                    buf
                }

                // === Authorize Start ===
                let corr_id = b"test-corr-id";
                let mut auth_payload = Vec::new();
                auth_payload.push(0u8); // action = start
                auth_payload.extend_from_slice(repository.as_bytes());
                auth_payload.push(corr_id.len() as u8);
                auth_payload.extend_from_slice(corr_id);
                auth_payload.extend_from_slice(&0u16.to_le_bytes()); // no token

                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Authorize as QuicOpCode,
                        id,
                        auth_payload.len(),
                        0,
                    ),
                    &auth_payload,
                )
                .await;

                assert!(!resp.error, "Authorize start failed: {resp:?}");
                assert_eq!(resp.size_or_status, 4);
                let session_id_bytes = read_payload(&mut recv, 4).await;
                let session_id = u32::from_le_bytes(session_id_bytes.try_into().unwrap());
                assert!(session_id >= 1);

                // === Put a fragment via the protocol ===
                let mut put_payload = Vec::new();
                put_payload.extend_from_slice(address.as_bytes());
                put_payload.extend_from_slice(fragment.as_bytes());
                put_payload.extend_from_slice(&payload);

                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Put as QuicOpCode,
                        id,
                        put_payload.len(),
                        session_id,
                    ),
                    &put_payload,
                )
                .await;

                assert!(!resp.error, "Put failed: {resp:?}");
                assert_eq!(resp.command_id, id);
                assert_eq!(resp.session_id, session_id);
                assert_eq!(resp.size_or_status, 0); // empty response

                // === Get the fragment we just put ===
                let get_payload = address.as_bytes().to_vec();
                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Get as QuicOpCode,
                        id,
                        get_payload.len(),
                        session_id,
                    ),
                    &get_payload,
                )
                .await;

                assert!(!resp.error, "Get failed: {resp:?}");
                assert_eq!(resp.command_id, id);
                assert_eq!(resp.session_id, session_id);
                assert!(resp.size_or_status > 0);

                let get_data = read_payload(&mut recv, resp.size_or_status as usize).await;
                // Response is Fragment + payload bytes
                assert!(get_data.len() >= size_of::<Fragment>());
                let returned_payload = &get_data[size_of::<Fragment>()..];
                assert_eq!(returned_payload, payload.as_ref());

                // === Get a non-existent address should fail ===
                let other_get_payload = other_address.as_bytes().to_vec();
                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Get as QuicOpCode,
                        id,
                        other_get_payload.len(),
                        session_id,
                    ),
                    &other_get_payload,
                )
                .await;

                assert!(resp.error, "Get non-existent should fail");
                assert_eq!(resp.command_id, id);
                // NotFound = 4
                assert_eq!(resp.size_or_status, 4);

                // === Query: one existing, one non-existent ===
                let mut query_payload = Vec::new();
                query_payload.extend_from_slice(address.as_bytes());
                query_payload.extend_from_slice(other_address.as_bytes());

                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Query as QuicOpCode,
                        id,
                        query_payload.len(),
                        session_id,
                    ),
                    &query_payload,
                )
                .await;

                assert!(!resp.error, "Query failed: {resp:?}");
                assert_eq!(resp.command_id, id);
                assert_eq!(resp.size_or_status, 2); // 2 results, one byte each

                let query_results = read_payload(&mut recv, 2).await;
                assert_eq!(query_results[0], 0); // ExistFullMatch for the put address
                assert_eq!(query_results[1], 3); // NotFound for other address

                // === Second session with different correlation ID ===
                let corr_id_2 = b"test-corr-id-2";
                let mut auth_payload_2 = Vec::new();
                auth_payload_2.push(0u8); // action = start
                auth_payload_2.extend_from_slice(repository.as_bytes());
                auth_payload_2.push(corr_id_2.len() as u8);
                auth_payload_2.extend_from_slice(corr_id_2);
                auth_payload_2.extend_from_slice(&0u16.to_le_bytes()); // no token

                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Authorize as QuicOpCode,
                        id,
                        auth_payload_2.len(),
                        0,
                    ),
                    &auth_payload_2,
                )
                .await;

                assert!(!resp.error, "Authorize start session 2 failed: {resp:?}");
                assert_eq!(resp.size_or_status, 4);
                let session_id_2_bytes = read_payload(&mut recv, 4).await;
                let session_id_2 = u32::from_le_bytes(session_id_2_bytes.try_into().unwrap());
                assert!(session_id_2 >= 1);
                assert_ne!(session_id_2, session_id, "Sessions must have different IDs");

                // === Put a second fragment via session 2 ===
                let (fragment2, address2, payload2) = generate_random();
                let mut put_payload_2 = Vec::new();
                put_payload_2.extend_from_slice(address2.as_bytes());
                put_payload_2.extend_from_slice(fragment2.as_bytes());
                put_payload_2.extend_from_slice(&payload2);

                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Put as QuicOpCode,
                        id,
                        put_payload_2.len(),
                        session_id_2,
                    ),
                    &put_payload_2,
                )
                .await;

                assert!(!resp.error, "Put via session 2 failed: {resp:?}");
                assert_eq!(resp.session_id, session_id_2);

                // === Get fragment via session 2 ===
                let get_payload_2 = address2.as_bytes().to_vec();
                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Get as QuicOpCode,
                        id,
                        get_payload_2.len(),
                        session_id_2,
                    ),
                    &get_payload_2,
                )
                .await;

                assert!(!resp.error, "Get via session 2 failed: {resp:?}");
                assert_eq!(resp.session_id, session_id_2);
                assert!(resp.size_or_status > 0);

                let get_data_2 = read_payload(&mut recv, resp.size_or_status as usize).await;
                let returned_payload_2 = &get_data_2[size_of::<Fragment>()..];
                assert_eq!(returned_payload_2, payload2.as_ref());

                // === Copy via session 1: copy fragment2 within same repo ===
                let mut copy_payload = Vec::new();
                copy_payload.extend_from_slice(repository.as_bytes()); // source_repo (16 bytes)
                copy_payload.extend_from_slice(address2.hash.as_bytes()); // source hash (32 bytes)
                copy_payload.extend_from_slice(address2.context.as_bytes()); // source context (16 bytes)
                // v4 wire bumped Copy to 80 bytes — append target_context. This test preserves
                // the source's context so the destination tuple is the same as the source's
                // (cross-partition copy) — matching the legacy semantics.
                copy_payload.extend_from_slice(address2.context.as_bytes()); // target context (16 bytes)

                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Copy as QuicOpCode,
                        id,
                        copy_payload.len(),
                        session_id,
                    ),
                    &copy_payload,
                )
                .await;

                assert!(!resp.error, "Copy via session 1 failed: {resp:?}");
                assert_eq!(resp.session_id, session_id);

                // === Query via session 2: both addresses should exist ===
                let mut query_payload_2 = Vec::new();
                query_payload_2.extend_from_slice(address.as_bytes());
                query_payload_2.extend_from_slice(address2.as_bytes());

                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Query as QuicOpCode,
                        id,
                        query_payload_2.len(),
                        session_id_2,
                    ),
                    &query_payload_2,
                )
                .await;

                assert!(!resp.error, "Query via session 2 failed: {resp:?}");
                assert_eq!(resp.size_or_status, 2);
                let query_results_2 = read_payload(&mut recv, 2).await;
                assert_eq!(query_results_2[0], 0, "address from session 1 should exist");
                assert_eq!(query_results_2[1], 0, "address from session 2 should exist");

                // === Authorize Stop session 1 ===
                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Authorize as QuicOpCode,
                        id,
                        1,
                        session_id,
                    ),
                    &[1u8], // action = stop
                )
                .await;

                assert!(!resp.error, "Authorize stop session 1 failed: {resp:?}");
                assert_eq!(resp.size_or_status, 0);

                // === Session 2 should still work after session 1 stopped ===
                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Get as QuicOpCode,
                        id,
                        get_payload_2.len(),
                        session_id_2,
                    ),
                    &get_payload_2,
                )
                .await;

                assert!(
                    !resp.error,
                    "Get via session 2 after session 1 stopped should work"
                );
                let _ = read_payload(&mut recv, resp.size_or_status as usize).await;

                // === Get with stopped session 1 should fail ===
                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Get as QuicOpCode,
                        id,
                        get_payload.len(),
                        session_id,
                    ),
                    &get_payload,
                )
                .await;

                assert!(resp.error, "Get on stopped session 1 should fail");

                // === Authorize Stop session 2 ===
                let id = next_cmd_id();
                let resp = send_v4_cmd(
                    &mut send,
                    &mut recv,
                    CommandHeader::new_with_session(
                        Command::Authorize as QuicOpCode,
                        id,
                        1,
                        session_id_2,
                    ),
                    &[1u8], // action = stop
                )
                .await;

                assert!(!resp.error, "Authorize stop session 2 failed: {resp:?}");

                send.finish().expect("Failed to finish stream");
            }))
            .await
            .expect("Test task failed");
    }
}
