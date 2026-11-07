// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A [`Transport`] test double (task 12): an in-memory request queue and reply log, so
//! `dispatch`/`session` tests exercise the real worker loop without a kernel or a real
//! `/dev/fuse` fd. Gated behind `test-util` so other crates in the workspace can use it too.

use std::collections::VecDeque;
use std::io::IoSlice;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::buffer::RequestBuffer;
use crate::error::FuseError;
use crate::transport::Notifier;
use crate::transport::ReplySender;
use crate::transport::ReplySink;
use crate::transport::SendOutcome;
use crate::transport::Transport;
use crate::transport::TransportError;
use crate::transport::TransportWorker;

/// An in-memory [`Transport`]: requests are pushed in by the test, replies are recorded for the
/// test to inspect. `abort_next_send` makes the *next* `send` on any worker (inline or
/// deferred) behave as if the kernel had already dropped the request, so tests can exercise
/// `SendOutcome::Aborted` without a real, racy interruption.
#[derive(Default)]
pub struct MockTransport {
    pub requests: Mutex<VecDeque<Vec<u8>>>,
    pub replies: Mutex<Vec<(usize, Vec<u8>)>>,
    pub abort_next_send: AtomicBool,
    /// Runs once, at the next `send` on any worker, before that send reads `abort_next_send`:
    /// a change racing a reply between the request's answer and its delivery.
    pub before_send: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// One more than the worker index whose `worker()` call fails; 0, the default, fails none.
    pub fail_worker: AtomicUsize,
    next_worker: AtomicUsize,
    request_available: Notify,
}

impl MockTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queues one request's raw bytes and wakes a worker blocked in `receive`.
    pub fn push_request(&self, bytes: Vec<u8>) {
        self.requests.lock().push_back(bytes);
        self.request_available.notify_one();
    }

    fn record_reply(&self, index: usize, iov: &[IoSlice<'_>]) -> SendOutcome {
        let before_send = self.before_send.lock().take();
        if let Some(before_send) = before_send {
            before_send();
        }
        if self.abort_next_send.swap(false, Ordering::SeqCst) {
            return SendOutcome::Aborted;
        }
        let mut bytes = Vec::with_capacity(iov.iter().map(|s| s.len()).sum());
        for slice in iov {
            bytes.extend_from_slice(slice);
        }
        self.replies.lock().push((index, bytes));
        SendOutcome::Sent
    }
}

impl Transport for Arc<MockTransport> {
    type Worker = MockWorker;

    fn worker(&self) -> Result<Self::Worker, FuseError> {
        let index = self.next_worker.fetch_add(1, Ordering::SeqCst);
        if self.fail_worker.load(Ordering::SeqCst) == index + 1 {
            return Err(FuseError::Internal(format!(
                "MockTransport fails worker {index}"
            )));
        }
        Ok(MockWorker {
            transport: Arc::clone(self),
            index,
        })
    }

    fn notifier(&self) -> Result<Notifier, FuseError> {
        Err(FuseError::Internal(
            "MockTransport has no notifier".to_string(),
        ))
    }
}

/// One [`MockTransport`] worker: `index` is this worker's identity, used to tag recorded
/// replies so `deferred_reply_goes_to_the_originating_worker_fd` can tell them apart.
pub struct MockWorker {
    transport: Arc<MockTransport>,
    index: usize,
}

impl MockWorker {
    pub fn index(&self) -> usize {
        self.index
    }
}

impl TransportWorker for MockWorker {
    async fn receive(&mut self, buffer: &mut RequestBuffer) -> Result<usize, TransportError> {
        loop {
            if let Some(bytes) = self.transport.requests.lock().pop_front() {
                let out = buffer.as_mut();
                out[..bytes.len()].copy_from_slice(&bytes);
                return Ok(bytes.len());
            }
            self.transport.request_available.notified().await;
        }
    }

    fn send(&mut self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError> {
        Ok(self.transport.record_reply(self.index, iov))
    }

    fn reply_sender(&self) -> ReplySender {
        ReplySender::new(Arc::new(MockReplySink {
            transport: Arc::clone(&self.transport),
            index: self.index,
        }))
    }
}

/// A [`MockWorker`]'s deferred-reply target: records to the same transport, tagged with the
/// same worker index, as an inline `send` would.
struct MockReplySink {
    transport: Arc<MockTransport>,
    index: usize,
}

impl ReplySink for MockReplySink {
    fn send(&self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError> {
        Ok(self.transport.record_reply(self.index, iov))
    }
}
