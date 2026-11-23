// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use super::QuicErrorStatus;
use super::QuicOpCode;

/// How one response to a command arrived.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObservedResponse {
    pub opcode: QuicOpCode,
    /// The status of an error response, or `None` for a successful one.
    pub error_status: Option<QuicErrorStatus>,
    /// Bytes of the response's payload, which an error response has none of.
    pub payload_bytes: usize,
    /// The index of the connection's stream the response arrived on.
    pub stream_index: usize,
    /// Time from the command's frame being queued for its stream's writer until the writer had
    /// written it in full, which includes the crossing to the runtime the writer runs on and the
    /// frames queued ahead of it.
    pub queue_to_written: Duration,
    /// Time from the command's frame being written until the response's header had arrived, which
    /// includes the peer's time handling the command as well as the round trip. A response that
    /// arrives before the write returns is timed from the command being registered on its stream,
    /// just before the write began.
    pub to_header: Duration,
    /// Time from the response's header having arrived until its last byte had.
    pub header_to_last_byte: Duration,
}

impl ObservedResponse {
    /// The response to a command queued for writing at `queued` and written at `written`, whose
    /// header had arrived at `header` and last byte at `last_byte`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        opcode: QuicOpCode,
        error_status: Option<QuicErrorStatus>,
        payload_bytes: usize,
        stream_index: usize,
        queued: Instant,
        written: Instant,
        header: Instant,
        last_byte: Instant,
    ) -> Self {
        Self {
            opcode,
            error_status,
            payload_bytes,
            stream_index,
            queue_to_written: written.saturating_duration_since(queued),
            to_header: header.saturating_duration_since(written),
            header_to_last_byte: last_byte.saturating_duration_since(header),
        }
    }
}

/// Receives each response a connection reads, once the whole response has arrived.
///
/// Called on the reader task of the response's stream, after the response is handed to its
/// command. The stream's next response is read only once the call returns, so an implementation
/// must not block or wait.
pub trait ResponseObserver: Send + Sync {
    /// `connection` is the connection the response arrived on, for reading its statistics.
    fn observe(&self, response: &ObservedResponse, connection: &quinn::Connection);
}

/// The observer a connection's streams report to, set at most once and shared by every stream,
/// including those a reconnect opens.
pub(crate) type ResponseObserverSlot = Arc<OnceLock<Arc<dyn ResponseObserver>>>;
