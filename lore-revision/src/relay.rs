// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_base::lore_spawn_core;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::WeakUnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::event::EventError;
use crate::event::LoreCompleteEventData;
use crate::event::LoreEndEventData;
use crate::event::LoreErrorDetail;
use crate::event::LoreErrorEventData;
use crate::event::LoreEvent;
use crate::event::LoreLogEventData;
use crate::interface::LoreEventCallback;
use crate::logging::LoreLogLevel;
use crate::util;

/// Item sent through the mpsc event channel. Each event may carry an
/// optional `Bytes` keepalive that pins a buffer referenced by the
/// event's payload for the duration of the callback invocation.
///
/// The events that use the keepalive today are `LoreEvent::StorageGetData`
/// and `LoreEvent::StorageGetFragment`, whose `LoreBytes` views point into
/// the carried `Bytes`. The forwarder holds the `Bytes` clone while the
/// callback runs, then drops it. Since `Bytes` is itself refcounted,
/// the caller's task may drop its own clone as soon as `send_with_bytes`
/// returns — the buffer stays alive until every registered keepalive
/// has been consumed.
type DispatchedEvent = (LoreEvent, Option<Bytes>);

/// Keeps a dispatcher's channel open past `complete`, for a task that goes on
/// sending after the command that started it has answered — a notification
/// subscription. `drain` waits for the forwarder only while nothing holds one
/// of these; dropping it lets the channel close once every sender is gone.
pub struct KeepOpen {
    _sender: UnboundedSender<DispatchedEvent>,
    holds: Arc<parking_lot::Mutex<Holds>>,
}

impl Drop for KeepOpen {
    fn drop(&mut self) {
        self.holds.lock().count -= 1;
    }
}

/// The holds on a dispatcher's channel, and whether a drain has settled on waiting for that
/// channel to close.
///
/// Both behind one lock, so taking a hold and settling on the wait are ordered against each
/// other. A hold taken after a drain has settled would keep open the channel the drain is
/// waiting to see closed, which for a subscription is until the subscription ends.
#[derive(Default)]
struct Holds {
    count: usize,
    settled: bool,
}

#[lore_macro::test_pub]
pub struct EventDispatcher {
    pub correlation_id: String,
    pub completed: CancellationToken,
    pub weak_sender: Option<WeakUnboundedSender<DispatchedEvent>>,
    pub strong_sender: Mutex<Option<UnboundedSender<DispatchedEvent>>>,
    /// The [`KeepOpen`] holds on the channel. Counted here rather than read off
    /// the channel's strong count, which does not tell a hold apart from the
    /// upgrade a `send` takes for the length of its push.
    holds: Arc<parking_lot::Mutex<Holds>>,
    /// The forwarder task, kept so `complete` can await the task itself rather
    /// than only the token it cancels. A runtime torn down under an in-flight
    /// call drops the task, which leaves the token uncancelled forever, but the
    /// join still resolves.
    forwarder: Mutex<Option<JoinHandle<()>>>,
}

impl Default for EventDispatcher {
    fn default() -> Self {
        Self {
            correlation_id: String::default(),
            completed: CancellationToken::new(),
            weak_sender: None,
            strong_sender: Mutex::new(None),
            holds: Arc::default(),
            forwarder: Mutex::new(None),
        }
    }
}

impl EventDispatcher {
    /// The forwarder is pinned to core rather than following the caller: it invokes host
    /// callbacks for the life of the dispatcher, and a dispatcher built from a net task would
    /// otherwise run them on the runtime driving sockets.
    pub fn new(callback: LoreEventCallback) -> Self {
        let completed = CancellationToken::new();
        let (sender, mut receiver) = unbounded_channel();
        let weak_sender = sender.downgrade();
        let forwarder = if let Some(callback) = callback {
            let completed = completed.clone();

            // Spawn a forwarder task which will exit once all dispatchers
            // have terminated and mpsc channel has no producers. Each
            // item carries an optional `Bytes` keepalive; the forwarder
            // drops it AFTER the callback returns, so any `LoreBytes`
            // view in the event points at a live buffer for the full
            // callback invocation.
            Some(lore_spawn_core!(async move {
                while let Some((event, _keepalive)) = receiver.recv().await {
                    callback(&event);
                    // `_keepalive` drops here — the referenced buffer
                    // is released after the callback has finished.
                }
                callback(&LoreEvent::End(LoreEndEventData::default()));
                completed.cancel();
            }))
        } else {
            completed.cancel();
            None
        };

        Self {
            correlation_id: String::default(),
            completed,
            weak_sender: Some(weak_sender),
            strong_sender: Mutex::new(Some(sender)),
            holds: Arc::default(),
            forwarder: Mutex::new(forwarder),
        }
    }

    pub fn no_dispatch() -> Self {
        Self {
            correlation_id: String::default(),
            completed: CancellationToken::new(),
            weak_sender: None,
            strong_sender: Mutex::new(None),
            holds: Arc::default(),
            forwarder: Mutex::new(None),
        }
    }

    #[lore_macro::test_pub]
    fn sender(&self) -> Option<UnboundedSender<DispatchedEvent>> {
        self.weak_sender
            .as_ref()
            .and_then(|sender| sender.upgrade())
    }

    /// Keeps the channel open past `complete`, so that events sent afterwards
    /// still reach the callback; `End` then follows the last hold rather than
    /// `Complete`. `None` once the channel has closed, and once
    /// [`drain`](Self::drain) has settled on waiting for it to close, which a
    /// hold granted afterwards would keep open.
    pub fn keep_open(&self) -> Option<KeepOpen> {
        let mut holds = self.holds.lock();
        if holds.settled {
            return None;
        }
        let sender = self.sender()?;
        holds.count += 1;
        Some(KeepOpen {
            _sender: sender,
            holds: self.holds.clone(),
        })
    }

    /// Settles whether a drain waits for the channel to close, which it does where nothing
    /// holds the channel open.
    ///
    /// Decided under the lock a hold is taken under, so a hold cannot appear behind the
    /// decision and keep open the channel the wait is for.
    fn settle(&self) -> bool {
        let mut holds = self.holds.lock();
        holds.settled = holds.count == 0;
        holds.settled
    }

    pub fn send(&self, event: LoreEvent) {
        self.send_inner(event, None);
    }

    /// Emit an event whose payload references a caller-owned buffer.
    /// The `Bytes` clone travels with the event through the channel and
    /// is dropped only after the forwarder has returned from the
    /// user callback — keeping the bytes valid for the duration of the
    /// callback invocation without requiring the caller's task to
    /// outlive the dispatch.
    pub fn send_with_bytes(&self, event: LoreEvent, bytes: Bytes) {
        self.send_inner(event, Some(bytes));
    }

    fn send_inner(&self, event: LoreEvent, keepalive: Option<Bytes>) {
        if let Some(sender) = self.sender()
            && let Err(_err) = sender.send((event, keepalive))
        {
            /*
            generate_log(
                self.correlation_id.as_str(),
                LoreLogLevel::Trace,
                format!("Failed to send event: {err}"),
            );
            */
        }
    }

    pub fn send_error(&self, error: impl EventError) {
        crate::lore_error!("{}", error.inner());
        self.send(LoreEvent::Error(LoreErrorEventData::from_inner_error(
            &error,
        )));
    }

    /// Waits for every event sent so far to have been through the callback, so a
    /// caller reading what its callback collected sees all of it.
    ///
    /// Called by [`complete`](Self::complete), and directly by a relayed call,
    /// which the service completes instead.
    pub async fn drain(&self) {
        // Drop this strong reference, let the dispatcher task exit out and signal the end event
        // if this is the only strong reference to the event channel
        drop(self.strong_sender.lock().await.take());

        // A hold means the end event will come whenever its holder is done
        // (an ongoing notification subscription), so there is nothing to wait
        // for here.
        if self.settle() {
            // Await the forwarder task, not just the token it cancels on its way
            // out. The two finish together in the normal case, but a runtime torn
            // down under an in-flight call drops the task before it can cancel
            // anything, and waiting on the token then never returns. Joining a
            // dropped task resolves as cancelled, so the call fails instead.
            match self.forwarder.lock().await.take() {
                Some(forwarder) => drop(forwarder.await),
                None => self.completed.cancelled().await,
            }
        }
    }

    pub async fn complete(&self, error: LoreErrorDetail) -> i32 {
        // `status` is the detail's `error_code` so the two agree by
        // construction: `0` with the empty default detail on success, the
        // detail's `error_code` with that detail on failure.
        let status = error.error_code;
        // Log a failing completion so consumers that surface log events (the
        // CLI, the server) show the message and trace.
        if status != 0 {
            crate::lore_error!("{}", error.message_with_trace());
        }
        self.send(LoreEvent::Complete(LoreCompleteEventData { status, error }));
        self.drain().await;

        status
    }

    /// Completes a command from its `Result`: builds the detail and returns the
    /// status the `Complete` event carries.
    pub async fn complete_result<T, E>(&self, result: Result<T, E>) -> i32
    where
        E: lore_error_set::FfiError + std::fmt::Display + lore_error_set::HasTrace,
    {
        self.complete(LoreErrorDetail::from_result(result)).await
    }

    pub fn make_log(level: LoreLogLevel, message: String) -> LoreLogEventData {
        LoreLogEventData {
            level,
            category: 0,
            timestamp: util::time::timestamp(),
            location: Default::default(),
            message: message.into(),
        }
    }
}
