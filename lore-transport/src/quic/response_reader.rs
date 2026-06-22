// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use bytes::Bytes;
use bytes::BytesMut;
use dashmap::DashMap;
use lore_base::lore_debug;
use lore_base::lore_spawn;
use lore_base::lore_trace;
use quinn::ConnectionError;
use quinn::ReadError;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use zerocopy::IntoBytes;

use super::QuicClientError;
use super::QuicErrorStatus;
use super::QuicServiceError;
use super::command_header::COMMAND_HEADER_SIZE;
use super::command_header::COMMAND_HEADER_SIZE_V4;
use super::command_header::CommandHeader;

type PendingResultSender = oneshot::Sender<Result<Bytes, QuicClientError>>;
type PendingCommandMap = DashMap<u32, PendingResultSender>;

pub struct ResponseReader {
    pub task: JoinHandle<Result<(), QuicClientError>>,
    pending: Arc<PendingCommandMap>,
    counter: AtomicU32,
}

impl ResponseReader {
    pub fn new(
        index: u32,
        stream: quinn::RecvStream,
        max_chunk_size: usize,
        last_recv: Arc<AtomicU64>,
        created: Instant,
        v4: bool,
    ) -> Self {
        let pending = Arc::new(PendingCommandMap::new());
        let pending_clone = pending.clone();
        Self {
            task: lore_spawn!({
                async move {
                    let pending = pending_clone;
                    let result = read_response(
                        index,
                        stream,
                        max_chunk_size,
                        pending.clone(),
                        last_recv,
                        created,
                        v4,
                    )
                    .await;
                    {
                        let error_to_send = match result {
                            Err(QuicClientError::CrytpoError) => QuicClientError::CrytpoError,
                            _ => QuicClientError::Terminated,
                        };
                        while !pending.is_empty() {
                            let ids: Vec<u32> = pending.iter().map(|entry| *entry.key()).collect();
                            for id in ids {
                                if let Some((_, reader)) = pending.remove(&id) {
                                    let _ = reader.send(Err(error_to_send.clone()));
                                }
                            }
                        }
                    }
                    result
                }
            }),
            pending,
            counter: AtomicU32::new(1),
        }
    }

    /// Assign a new stream specific command ID and put the completion channel in map
    pub fn wait_for(
        &self,
        tx: oneshot::Sender<Result<Bytes, QuicClientError>>,
    ) -> Result<u32, QuicClientError> {
        let command_id = self.counter.fetch_add(1, Ordering::Relaxed);
        self.pending.insert(command_id, tx);
        Ok(command_id)
    }
}

async fn read_response(
    stream_index: u32,
    mut stream: quinn::RecvStream,
    max_chunk_size: usize,
    pending: Arc<PendingCommandMap>,
    last_recv: Arc<AtomicU64>,
    created: Instant,
    v4: bool,
) -> Result<(), QuicClientError> {
    let header_size = if v4 {
        COMMAND_HEADER_SIZE_V4
    } else {
        COMMAND_HEADER_SIZE
    };

    let mut current_offset = 0u64;
    let mut response = CommandHeader::default();
    let mut payload: Option<BytesMut> = None;

    let mut response_bytes = [0u8; COMMAND_HEADER_SIZE_V4];
    let mut response_bytes_read = 0;

    let mut pending_chunks = vec![];

    let mut next_chunk: Option<quinn::Chunk> = None;
    loop {
        if next_chunk.is_none() {
            next_chunk = match stream.read_chunk(max_chunk_size, false).await {
                Ok(chunk) => {
                    last_recv.store(created.elapsed().as_millis() as u64, Ordering::Relaxed);
                    chunk
                }
                Err(err) => {
                    if err != ReadError::ConnectionLost(quinn::ConnectionError::LocallyClosed) {
                        lore_debug!("Error reading chunk: {err}");

                        if let ReadError::ConnectionLost(lost_error) = err
                            && let ConnectionError::ConnectionClosed(closed_error) = lost_error
                            && closed_error.error_code.is_crypto()
                        {
                            return Err(QuicClientError::CrytpoError);
                        }

                        return Err(QuicClientError::Read);
                    }
                    return Ok(());
                }
            };
        }

        let Some(mut chunk) = next_chunk.take() else {
            lore_trace!("Terminating response reader");
            break;
        };

        lore_trace!(
            "QUIC stream {stream_index} got {} bytes @ offset {}",
            chunk.bytes.len(),
            chunk.offset
        );

        if chunk.offset == current_offset {
            while !chunk.bytes.is_empty() {
                if response_bytes_read < header_size {
                    // Read the response header
                    if chunk.bytes.len() + response_bytes_read < header_size {
                        let got_count = chunk.bytes.len();
                        response_bytes[response_bytes_read..(response_bytes_read + got_count)]
                            .copy_from_slice(chunk.bytes.as_ref());

                        response_bytes_read += got_count;
                        current_offset += got_count as u64;
                        chunk.bytes.clear();
                    } else {
                        let remain_count = header_size - response_bytes_read;
                        let remain_bytes = chunk.bytes.split_to(remain_count);

                        response_bytes[response_bytes_read..header_size]
                            .copy_from_slice(remain_bytes.as_ref());

                        response = if v4 {
                            CommandHeader::from_bytes_v4(&response_bytes[..header_size])
                        } else {
                            CommandHeader::from_bytes(response_bytes[..header_size].as_bytes())
                        };
                        if response.command_id == 0
                            || response.size_or_status > max_chunk_size as u32
                        {
                            return Err(QuicClientError::InvalidResponse(response));
                        }

                        response_bytes_read = header_size;
                        current_offset += remain_count as u64;
                        chunk.offset += remain_count as u64;

                        lore_trace!(
                            "QUIC stream {stream_index} read response header {:?}",
                            response
                        );

                        // If error there is no more data, otherwise allocate buffer for response payload
                        if response.error || response.size_or_status == 0 {
                            response_bytes_read = 0;

                            if let Some((_, reader)) = pending.remove(&response.command_id) {
                                // Send response header and reset so next read is next response
                                let result = if !response.error {
                                    Ok(Bytes::default())
                                } else {
                                    Err(handle_error(response.size_or_status))
                                };
                                if reader.send(result).is_err() {
                                    lore_debug!(
                                        "Failed to transfer QUIC command result back to reader"
                                    );
                                }
                            } else {
                                return Err(QuicClientError::UnexpectedCommand(response));
                            }
                        } else if chunk.bytes.len() >= response.size_or_status as usize {
                            // Happy path, we can directly use buffer as it contains the full response
                            let size = response.size_or_status as usize;
                            let payload = chunk.bytes.split_to(size);

                            current_offset += size as u64;
                            chunk.offset += size as u64;

                            lore_trace!(
                                "QUIC stream {stream_index} read {size} bytes complete payload from single chunk",
                            );

                            response_bytes_read = 0;

                            if let Some((_, reader)) = pending.remove(&response.command_id) {
                                // Send response header and payload and reset so next read is next response
                                let result = if !response.error {
                                    Ok(payload)
                                } else {
                                    Err(handle_error(response.size_or_status))
                                };
                                if reader.send(result).is_err() {
                                    lore_debug!(
                                        "Failed to transfer QUIC command result back to reader"
                                    );
                                }
                            } else {
                                return Err(QuicClientError::UnexpectedCommand(response));
                            }
                        } else {
                            // Allocate buffer for response payload concatenated from multiple chunks
                            payload =
                                Some(BytesMut::with_capacity(response.size_or_status as usize));
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

                    lore_trace!(
                        "QUIC stream {stream_index} read {} bytes for a total of {} / {} bytes of payload",
                        size,
                        current_payload.len(),
                        current_payload.capacity()
                    );

                    if current_payload.capacity() == current_payload.len() {
                        response_bytes_read = 0;

                        if let Some((_, reader)) = pending.remove(&response.command_id) {
                            // Send response header and payload and reset so next read is next response
                            let payload = current_payload.freeze();
                            let result = if !response.error {
                                Ok(payload)
                            } else {
                                Err(handle_error(response.size_or_status))
                            };
                            if reader.send(result).is_err() {
                                lore_debug!(
                                    "Failed to transfer QUIC command result back to reader"
                                );
                            }
                        } else {
                            return Err(QuicClientError::UnexpectedCommand(response));
                        }
                    } else {
                        payload = Some(current_payload);
                    }
                }
            }
        } else {
            // Queue for later processing
            lore_trace!(
                "Got out of order chunk @ offset {}, current offset is {}",
                chunk.offset,
                current_offset
            );
            pending_chunks.push(chunk);
        }

        for (ichunk, chunk) in pending_chunks.iter().enumerate() {
            if chunk.offset == current_offset {
                lore_trace!(
                    "Grab out of order chunk @ current offset {current_offset} - {} ooo chunks remaining",
                    pending_chunks.len() - 1
                );
                next_chunk = Some(pending_chunks.swap_remove(ichunk));
                break;
            }
        }
    }

    Ok(())
}

fn handle_error(status: QuicErrorStatus) -> QuicClientError {
    match status {
        x if x == QuicServiceError::SlowDown as u32 => QuicClientError::SlowDown,
        x if x == QuicServiceError::NotAuthorized as u32 => QuicClientError::NotAuthorized,
        x if x == QuicServiceError::NotFound as u32 => QuicClientError::NotFound,
        x if x == QuicServiceError::Oversized as u32 => QuicClientError::Oversized,
        _ => QuicClientError::ServerError(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_service_errors_to_typed_variants() {
        assert!(matches!(
            handle_error(QuicServiceError::NotAuthorized as u32),
            QuicClientError::NotAuthorized
        ));
        assert!(matches!(
            handle_error(QuicServiceError::SlowDown as u32),
            QuicClientError::SlowDown
        ));
        assert!(matches!(
            handle_error(QuicServiceError::NotFound as u32),
            QuicClientError::NotFound
        ));
        assert!(matches!(
            handle_error(QuicServiceError::Oversized as u32),
            QuicClientError::Oversized
        ));
    }

    #[test]
    fn unknown_status_falls_back_to_server_error() {
        assert!(matches!(handle_error(42), QuicClientError::ServerError(42)));
    }
}
