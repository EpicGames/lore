// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The transport boundary: how a session's workers read requests from and write replies to
//! the kernel, abstracted so tests can substitute a `MockTransport` (task 12) for the real
//! `/dev/fuse` device (task 10's [`device::DeviceTransport`]).

pub mod device;
#[cfg(feature = "test-util")]
pub mod mock;

use std::future::Future;
use std::io::IoSlice;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use crate::buffer::RequestBuffer;
use crate::error::FuseError;

/// A session's transport: how its workers are created, and how out-of-band notifications
/// (task 14) are sent.
pub trait Transport: Send + Sync + 'static {
    type Worker: TransportWorker;

    /// Creates one worker's connection to the kernel. Called once per session worker.
    fn worker(&self) -> Result<Self::Worker, FuseError>;

    /// A handle for sending unsolicited notifications (invalidations), independent of any
    /// single worker.
    fn notifier(&self) -> Result<Notifier, FuseError>;
}

/// One worker's half of a [`Transport`]: pulls requests, pushes replies.
pub trait TransportWorker: Send + 'static {
    /// Reads the next request into `buffer`, returning the number of bytes filled.
    fn receive(
        &mut self,
        buffer: &mut RequestBuffer,
    ) -> impl Future<Output = Result<usize, TransportError>> + Send;

    /// Writes a reply. `Aborted` means the kernel already gave up on this request (e.g. it was
    /// interrupted); the caller still owns disposing of the outcome (spec: reply-failure
    /// callback, task 14), it is not itself an error.
    fn send(&mut self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError>;

    /// A cloneable, `Send + Sync` handle that can reply on this worker's behalf from outside
    /// its own request loop (task 12: the deferred-`READ` path). Replies through it are
    /// otherwise indistinguishable from ones sent via [`send`](TransportWorker::send).
    fn reply_sender(&self) -> ReplySender;
}

/// Sends a reply on behalf of a specific worker from a detached task (decision 3: the deferred
/// `READ` path). `dyn`-dispatched deliberately: this is the one place in the crate allowed to
/// allocate on the hot path's periphery, since it is only ever reached from the cold,
/// already-allocating deferred-read task, never from a worker's own inline request loop.
pub trait ReplySink: Send + Sync {
    fn send(&self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError>;
}

/// A cloneable handle bound to one worker's connection, usable from any task (task 12).
#[derive(Clone)]
pub struct ReplySender {
    inner: Arc<dyn ReplySink>,
}

impl ReplySender {
    pub fn new(inner: Arc<dyn ReplySink>) -> Self {
        Self { inner }
    }

    pub fn send(&self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError> {
        self.inner.send(iov)
    }
}

/// The result of a [`TransportWorker::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Aborted,
}

/// Errors a [`Transport`], a [`TransportWorker`] or a [`Notifier`] can produce. A notification's
/// `ENOENT`, the kernel not knowing the inode or entry invalidated, is not one: it succeeds.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The kernel tore the mount down.
    #[error("Disconnected from the FUSE device")]
    Disconnected,
    #[error("I/O error: {0}")]
    Io(String),
}

/// A handle for sending unsolicited kernel notifications. Only its shape is fixed here; the
/// `inval_inode`/`inval_entry` methods are added in task 14 ([`crate::notify`]).
#[lore_macro::test_pub]
#[derive(Clone)]
pub struct Notifier {
    pub(crate) fd: Arc<OwnedFd>,
}
