// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use bytes::Bytes;
use dashmap::DashMap;
use lore_base::lore_debug;
use lore_base::lore_spawn_net_nocontext;
use lore_base::lore_trace;
use lore_base::lore_warn;
use quinn::ConnectionError;
use quinn::ReadError;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::QuicClientError;
use super::QuicErrorStatus;
use super::QuicServiceError;
use super::command_header::COMMAND_HEADER_SIZE;
use super::command_header::COMMAND_HEADER_SIZE_V4;
use crate::quic::chunking::ParallelChunking;

type PendingResultSender = oneshot::Sender<Result<Bytes, QuicClientError>>;

/// The commands waiting for their answer on one stream, and whether its reader has stopped.
///
/// The map's shard locks order `closed`: the reader sets it before it locks every shard to see
/// whether a command still waits, and [`ResponseReader::wait_for`] reads it after the lock that
/// inserts its command, so either the reader finds the command or the command sees the flag.
#[derive(Default)]
struct PendingCommands {
    commands: DashMap<u32, PendingResultSender>,
    closed: AtomicBool,
}

pub struct ResponseReader {
    pub task: JoinHandle<Result<(), QuicClientError>>,
    pending: Arc<PendingCommands>,
    counter: AtomicU32,
}

impl ResponseReader {
    /// A reader of the answers on `stream`, run as a task without the context of the command that
    /// opens the stream, as it serves every command the stream carries.
    pub fn new(
        stream: quinn::RecvStream,
        max_chunk_size: usize,
        last_recv: Arc<AtomicU64>,
        created: Instant,
        v4: bool,
    ) -> Self {
        let pending = Arc::new(PendingCommands::default());
        let pending_clone = pending.clone();
        ResponseReader {
            task: lore_spawn_net_nocontext!({
                async move {
                    let pending = pending_clone;
                    let result = read_response(
                        stream,
                        max_chunk_size,
                        pending.clone(),
                        last_recv,
                        created,
                        v4,
                    )
                    .await;
                    pending.closed.store(true, Ordering::Relaxed);
                    {
                        let error_to_send = match result {
                            Err(QuicClientError::CrytpoError) => QuicClientError::CrytpoError,
                            _ => QuicClientError::Terminated,
                        };
                        while !pending.commands.is_empty() {
                            let ids: Vec<u32> =
                                pending.commands.iter().map(|entry| *entry.key()).collect();
                            for id in ids {
                                if let Some((_, reader)) = pending.commands.remove(&id) {
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

    /// Assigns the command a new stream-specific id and puts its completion channel in the map.
    ///
    /// Fails with `Terminated` once the reader has stopped, since nothing would read the answer.
    pub fn wait_for(
        &self,
        tx: oneshot::Sender<Result<Bytes, QuicClientError>>,
    ) -> Result<u32, QuicClientError> {
        let command_id = self.counter.fetch_add(1, Ordering::Relaxed);
        self.pending.commands.insert(command_id, tx);
        if self.pending.closed.load(Ordering::Relaxed) {
            self.pending.commands.remove(&command_id);
            return Err(QuicClientError::Terminated);
        }
        Ok(command_id)
    }
}

async fn read_response(
    mut stream: quinn::RecvStream,
    max_chunk_size: usize,
    pending: Arc<PendingCommands>,
    last_recv: Arc<AtomicU64>,
    created: Instant,
    v4: bool,
) -> Result<(), QuicClientError> {
    let header_size = if v4 {
        COMMAND_HEADER_SIZE_V4
    } else {
        COMMAND_HEADER_SIZE
    };

    let mut chunk_resolver =
        ParallelChunking::new(header_size, max_chunk_size, None /* no metrics */);
    // Kept across the loop so a chunk carrying several responses costs no allocation.
    let mut responses = Vec::new();

    loop {
        let mut next_chunk = match stream.read_chunk(max_chunk_size, false).await {
            Ok(chunk) => {
                last_recv.store(created.elapsed().as_millis() as u64, Ordering::Relaxed);
                chunk
            }
            Err(err) => {
                if err != ReadError::ConnectionLost(ConnectionError::LocallyClosed) {
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

        let Some(chunk) = next_chunk.take() else {
            lore_trace!("Terminating response reader");
            break;
        };

        chunk_resolver
            .resolve_into(chunk, &mut responses)
            .map_err(|invalid_header| {
                lore_warn!("Bad header: {:?}", invalid_header);
                QuicClientError::InvalidResponse(invalid_header)
            })?;

        for response in responses.drain(..) {
            if response.header.command_id == 0 {
                return Err(QuicClientError::InvalidResponse(response.header));
            }

            if let Some((_, reader)) = pending.commands.remove(&response.header.command_id) {
                // Send response header and reset so next read is next response
                let result = if !response.header.error {
                    Ok(response.payload.unwrap_or_default())
                } else {
                    Err(handle_error(response.header.size_or_status))
                };
                if reader.send(result).is_err() {
                    lore_debug!("Failed to transfer QUIC command result back to reader");
                }
            } else {
                return Err(QuicClientError::UnexpectedCommand(response.header));
            }
        }
    }

    Ok(())
}

#[lore_macro::test_pub]
pub(crate) fn handle_error(status: QuicErrorStatus) -> QuicClientError {
    match status {
        x if x == QuicServiceError::SlowDown as u32 => QuicClientError::SlowDown,
        x if x == QuicServiceError::NotAuthorized as u32 => QuicClientError::NotAuthorized,
        x if x == QuicServiceError::NotFound as u32 => QuicClientError::NotFound,
        x if x == QuicServiceError::Oversized as u32 => QuicClientError::Oversized,
        _ => QuicClientError::ServerError(status),
    }
}
