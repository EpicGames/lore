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
use super::response_observer::ObservedResponse;
use super::response_observer::ResponseObserverSlot;
use crate::quic::chunking::ParallelChunking;

pub(crate) type PendingResultSender = oneshot::Sender<Result<Bytes, QuicClientError>>;

/// A command waiting for its answer.
struct PendingCommand {
    sender: PendingResultSender,
    /// When the command's frame was queued for the stream's writer, kept only while the stream's
    /// responses are observed.
    queued: Option<Instant>,
    /// When the command was written, kept only while the stream's responses are observed.
    written: Option<Instant>,
}

/// The commands waiting for their answer on one stream, by the ids it assigns them, and whether
/// the stream's reader has stopped.
///
/// The map's shard locks order `closed`: the reader sets it before it locks every shard to see
/// whether a command still waits, and [`PendingCommands::wait_for`] reads it after the lock that
/// inserts its command, so either the reader finds the command or the command sees the flag.
pub struct PendingCommands {
    commands: DashMap<u32, PendingCommand>,
    closed: AtomicBool,
    counter: AtomicU32,
    observer: ResponseObserverSlot,
}

impl PendingCommands {
    /// An empty set, whose first command takes id 1, whose responses go to the observer `observer`
    /// holds once one is set.
    #[lore_macro::test_pub]
    pub(crate) fn new(observer: ResponseObserverSlot) -> Self {
        Self {
            commands: DashMap::default(),
            closed: AtomicBool::new(false),
            counter: AtomicU32::new(1),
            observer,
        }
    }

    /// Whether the stream's responses are observed, so that a command is timed.
    pub(crate) fn observed(&self) -> bool {
        self.observer.get().is_some()
    }

    /// A new stream-specific id for a command.
    pub(crate) fn next_id(&self) -> u32 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Puts the completion channel of the command `command_id`, whose frame was queued for the
    /// stream's writer at `queued`, in the map, and answers whether it stays there. Once the reader
    /// has stopped, nothing would read the answer, so the command is taken out again and fails with
    /// `Terminated`. A command queued without a time is not observed.
    #[lore_macro::test_pub]
    pub(crate) fn wait_for(
        &self,
        command_id: u32,
        tx: PendingResultSender,
        queued: Option<Instant>,
    ) -> bool {
        let written = queued.map(|_| Instant::now());
        self.commands.insert(
            command_id,
            PendingCommand {
                sender: tx,
                queued,
                written,
            },
        );
        if self.closed.load(Ordering::Relaxed) {
            self.fail(command_id, QuicClientError::Terminated);
            return false;
        }
        true
    }

    /// Takes out the command `command_id`, for which no answer will come.
    pub(crate) fn remove(&self, command_id: u32) {
        self.commands.remove(&command_id);
    }

    /// Takes out the command `command_id`, failing it with `error`.
    pub(crate) fn fail(&self, command_id: u32, error: QuicClientError) {
        if let Some((_, command)) = self.commands.remove(&command_id) {
            let _ = command.sender.send(Err(error));
        }
    }

    /// Records that the frame of the command `command_id` has been written. Costs nothing while
    /// the stream's responses are not observed.
    pub(crate) fn written(&self, command_id: u32) {
        if !self.observed() {
            return;
        }
        if let Some(mut command) = self.commands.get_mut(&command_id) {
            command.written = Some(Instant::now());
        }
    }
}

pub struct ResponseReader {
    pub task: JoinHandle<Result<(), QuicClientError>>,
    /// The commands the reader serves, kept only for the tests to count.
    #[cfg(feature = "test-util")]
    pending: Arc<PendingCommands>,
}

impl ResponseReader {
    /// A reader of the answers on `stream` to the commands `pending` holds, run as a task without
    /// the context of the command that opens the stream, as it serves every command the stream
    /// carries.
    /// `connection` and `stream_index` identify the stream to the observer of its responses.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: quinn::RecvStream,
        pending: Arc<PendingCommands>,
        max_chunk_size: usize,
        last_recv: Arc<AtomicU64>,
        created: Instant,
        v4: bool,
        connection: quinn::Connection,
        stream_index: usize,
    ) -> Self {
        #[cfg(feature = "test-util")]
        let counted = pending.clone();
        ResponseReader {
            task: lore_spawn_net_nocontext!({
                async move {
                    let result = read_response(
                        stream,
                        max_chunk_size,
                        pending.clone(),
                        last_recv,
                        created,
                        v4,
                        StreamIdentity {
                            connection,
                            index: stream_index,
                        },
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
                                if let Some((_, command)) = pending.commands.remove(&id) {
                                    let _ = command.sender.send(Err(error_to_send.clone()));
                                }
                            }
                        }
                    }
                    result
                }
            }),
            #[cfg(feature = "test-util")]
            pending: counted,
        }
    }

    /// How many commands wait for their answer on the stream.
    #[cfg(feature = "test-util")]
    pub fn pending_commands(&self) -> usize {
        self.pending.commands.len()
    }
}

/// The connection a stream belongs to and its index there, as its response observer sees them.
#[lore_macro::test_pub]
struct StreamIdentity {
    connection: quinn::Connection,
    index: usize,
}

#[lore_macro::test_pub]
async fn read_response(
    mut stream: quinn::RecvStream,
    max_chunk_size: usize,
    pending: Arc<PendingCommands>,
    last_recv: Arc<AtomicU64>,
    created: Instant,
    v4: bool,
    identity: StreamIdentity,
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
        // quinn's reads never use tokio's budget, so while the stream keeps data buffered this
        // loop would not return, holding its net worker and keeping the tasks waiting to run
        // there, such as the connection's stream writers, from running. Taken before the read, so
        // a chunk's arrival is timed from when it is read.
        tokio::task::consume_budget().await;
        let mut next_chunk = match stream.read_chunk(max_chunk_size, false).await {
            Ok(chunk) => chunk,
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

        let arrived = Instant::now();
        last_recv.store(
            arrived.duration_since(created).as_millis() as u64,
            Ordering::Relaxed,
        );

        let Some(chunk) = next_chunk.take() else {
            lore_trace!("Terminating response reader");
            break;
        };

        chunk_resolver
            .resolve_arrived_into(chunk, Some(arrived), &mut responses)
            .map_err(|invalid_header| {
                lore_warn!("Bad header: {:?}", invalid_header);
                QuicClientError::InvalidResponse(invalid_header)
            })?;

        for response in responses.drain(..) {
            if response.header.command_id == 0 {
                return Err(QuicClientError::InvalidResponse(response.header));
            }

            if let Some((_, command)) = pending.commands.remove(&response.header.command_id) {
                let observed = command
                    .queued
                    .zip(command.written)
                    .zip(response.header_at)
                    .map(|((queued, written), header)| {
                        ObservedResponse::new(
                            response.header.cmd,
                            response
                                .header
                                .error
                                .then_some(response.header.size_or_status),
                            response.payload.as_ref().map_or(0, Bytes::len),
                            identity.index,
                            queued,
                            written,
                            header,
                            arrived,
                        )
                    });

                // Send response header and reset so next read is next response
                let result = if !response.header.error {
                    Ok(response.payload.unwrap_or_default())
                } else {
                    Err(handle_error(response.header.size_or_status))
                };
                if command.sender.send(result).is_err() {
                    lore_debug!("Failed to transfer QUIC command result back to reader");
                }

                // Observed once the command has its answer, so recording costs the command nothing.
                if let Some(observed) = observed
                    && let Some(observer) = pending.observer.get()
                {
                    observer.observe(&observed, &identity.connection);
                }

                // A chunk can carry thousands of responses.
                tokio::task::consume_budget().await;
            } else {
                return Err(QuicClientError::UnexpectedCommand(response.header));
            }
        }
    }

    Ok(())
}

/// Whether `result` shows its command's frame written in full: the server's answer, which it sends
/// only once it has read the frame in full, or the `Ok` of a command that waits only for its frame
/// to be written. Every error [`handle_error`] makes is the server's.
#[lore_macro::test_pub]
pub(crate) fn shows_frame_written(result: &Result<Bytes, QuicClientError>) -> bool {
    matches!(
        result,
        Ok(_)
            | Err(QuicClientError::SlowDown
                | QuicClientError::NotAuthorized
                | QuicClientError::NotFound
                | QuicClientError::Oversized
                | QuicClientError::ServerError(_))
    )
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
