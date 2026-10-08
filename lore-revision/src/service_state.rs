// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Service state for the Lore background service.
//!
//! This module provides [`ServiceStateImpl`], which holds service-level state:
//! uptime tracking, connection counting, and buffered log messages. The service
//! process shares one instance, reached through [`ServiceStateImpl::global`] and
//! passed to whatever needs it. Code that can take it as an argument should,
//! because an injected instance is one a test can create for itself.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use lore_base::log::LoreLogLevel;
use parking_lot::Mutex;

/// Maximum number of log messages to buffer before dropping oldest.
pub const MAX_BUFFER_SIZE: usize = 30;

/// A log message captured by the service.
#[derive(Debug, Clone)]
pub struct LogMessage {
    /// The severity level of the log message.
    pub level: LoreLogLevel,
    /// The log message text.
    pub message: String,
}

/// The state the service process shares, created on first access.
static SERVICE_STATE: LazyLock<Arc<ServiceStateImpl>> =
    LazyLock::new(|| Arc::new(ServiceStateImpl::new()));

/// Service-level state: when the service started, how many clients are
/// connected, and the log messages it has buffered for the next status report.
///
/// Every field carries its own synchronization, so callers share one instance
/// behind an [`Arc`] rather than a lock.
pub struct ServiceStateImpl {
    /// When the service was initialized, unset until [`Self::initialize`] runs.
    start_time: OnceLock<Instant>,
    /// Number of active client connections.
    connection_count: AtomicU32,
    /// Number of log messages dropped due to buffer overflow.
    dropped_count: AtomicU64,
    /// Buffered service-level log messages.
    log_buffer: Mutex<VecDeque<LogMessage>>,
}

/// RAII guard holding one unit of the active connection count.
///
/// [`ServiceStateImpl::increment_connections`] returns one, and dropping it
/// decrements the count. The count therefore cannot drift when a connection ends
/// on an early return, an error, or a cancelled task.
#[must_use = "the connection count drops back as soon as this guard is dropped"]
pub struct ConnectionGuard {
    state: Arc<ServiceStateImpl>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.decrement_connections();
    }
}

impl ServiceStateImpl {
    /// Creates state for a service that has not started yet.
    pub fn new() -> Self {
        Self {
            start_time: OnceLock::new(),
            connection_count: AtomicU32::new(0),
            dropped_count: AtomicU64::new(0),
            log_buffer: Mutex::new(VecDeque::new()),
        }
    }

    /// Returns the state the service process shares.
    ///
    /// For the paths that cannot be handed an instance, which are the SWFS C
    /// callbacks and the status command running inside the service. Anything
    /// else takes the instance its caller was given.
    pub fn global() -> &'static Arc<Self> {
        &SERVICE_STATE
    }

    /// Starts tracking uptime. The first call sets the start time and later
    /// calls leave it where it is, so a second initialization cannot make a
    /// running service look freshly started.
    pub fn initialize(&self) {
        let _ = self.start_time.set(Instant::now());
    }

    /// Returns whether the service state has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.start_time.get().is_some()
    }

    /// Pushes a log message to the buffer, dropping the oldest if full.
    pub fn push_log(&self, level: LoreLogLevel, message: String) {
        let mut buffer = self.log_buffer.lock();
        if buffer.len() >= MAX_BUFFER_SIZE {
            buffer.pop_front();
            self.dropped_count.fetch_add(1, Ordering::Relaxed);
        }
        buffer.push_back(LogMessage { level, message });
    }

    /// Drains all buffered log messages and returns them along with the drop count.
    /// The drop count is reset to zero after draining.
    pub fn drain_logs(&self) -> (Vec<LogMessage>, u64) {
        let mut buffer = self.log_buffer.lock();
        let messages: Vec<LogMessage> = buffer.drain(..).collect();
        let dropped = self.dropped_count.swap(0, Ordering::Relaxed);
        (messages, dropped)
    }

    /// Increments the active connection count, returning a [`ConnectionGuard`]
    /// that decrements it again when dropped. Hold the guard for as long as the
    /// connection is active.
    pub fn increment_connections(self: &Arc<Self>) -> ConnectionGuard {
        self.connection_count.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            state: Arc::clone(self),
        }
    }

    /// Decrements the active connection count.
    ///
    /// Private so that the count is only ever released by dropping the
    /// [`ConnectionGuard`] that claimed it.
    fn decrement_connections(&self) {
        self.connection_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Returns the number of milliseconds since the service was initialized.
    /// Returns 0 if not yet initialized.
    pub fn uptime_ms(&self) -> u64 {
        self.start_time
            .get()
            .map_or(0, |t| t.elapsed().as_millis() as u64)
    }

    /// Returns the current active connection count.
    pub fn connection_count(&self) -> u32 {
        self.connection_count.load(Ordering::Relaxed)
    }
}

impl Default for ServiceStateImpl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test owns its own `ServiceStateImpl`, so none of them can see a
    // count or a buffered message another test put there.

    #[test]
    fn draining_returns_what_was_pushed_and_empties_the_buffer() {
        let state = ServiceStateImpl::new();

        state.push_log(LoreLogLevel::Info, "first".to_string());
        state.push_log(LoreLogLevel::Error, "second".to_string());

        let (messages, dropped) = state.drain_logs();
        assert_eq!(dropped, 0);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].level, LoreLogLevel::Info);
        assert_eq!(messages[0].message, "first");
        assert_eq!(messages[1].level, LoreLogLevel::Error);
        assert_eq!(messages[1].message, "second");

        let (messages, dropped) = state.drain_logs();
        assert!(messages.is_empty(), "The buffer should have been drained");
        assert_eq!(dropped, 0, "The drop count resets when it is reported");
    }

    #[test]
    fn a_full_buffer_drops_the_oldest_messages_and_counts_them() {
        let state = ServiceStateImpl::new();

        for index in 0..MAX_BUFFER_SIZE + 2 {
            state.push_log(LoreLogLevel::Info, index.to_string());
        }

        let (messages, dropped) = state.drain_logs();
        assert_eq!(messages.len(), MAX_BUFFER_SIZE, "The buffer stays bounded");
        assert_eq!(dropped, 2);
        assert_eq!(
            messages[0].message, "2",
            "The two oldest messages are the ones dropped"
        );
    }

    #[test]
    fn a_connection_guard_holds_one_unit_of_the_count() {
        let state = Arc::new(ServiceStateImpl::new());
        assert_eq!(state.connection_count(), 0);

        let guard = state.increment_connections();
        assert_eq!(state.connection_count(), 1);

        drop(guard);
        assert_eq!(state.connection_count(), 0);
    }

    #[test]
    fn uptime_reads_zero_until_the_state_is_initialized() {
        let state = ServiceStateImpl::new();
        assert!(!state.is_initialized());
        assert_eq!(state.uptime_ms(), 0);

        state.initialize();
        assert!(state.is_initialized());
    }

    #[test]
    fn the_global_state_is_a_single_shared_instance() {
        assert!(
            Arc::ptr_eq(ServiceStateImpl::global(), ServiceStateImpl::global()),
            "Every caller of global() should reach the same state"
        );
    }
}
