// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

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
/// The only event that uses the keepalive today is
/// `LoreEvent::StorageGetData`, whose `LoreBytes` view points into the
/// carried `Bytes`. The forwarder holds the `Bytes` clone while the
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
    holders: Arc<AtomicUsize>,
}

impl Drop for KeepOpen {
    fn drop(&mut self) {
        self.holders.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct EventDispatcher {
    pub correlation_id: String,
    pub completed: CancellationToken,
    pub weak_sender: Option<WeakUnboundedSender<DispatchedEvent>>,
    pub strong_sender: Mutex<Option<UnboundedSender<DispatchedEvent>>>,
    /// How many [`KeepOpen`] holds are live. Counted here rather than read off
    /// the channel's strong count: a `send` upgrades the weak sender for as
    /// long as the push takes, and that counted too, so a task logging while
    /// `drain` looked made it return with events still queued.
    holders: Arc<AtomicUsize>,
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
            holders: Arc::new(AtomicUsize::new(0)),
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
            holders: Arc::new(AtomicUsize::new(0)),
            forwarder: Mutex::new(forwarder),
        }
    }

    pub fn no_dispatch() -> Self {
        Self {
            correlation_id: String::default(),
            completed: CancellationToken::new(),
            weak_sender: None,
            strong_sender: Mutex::new(None),
            holders: Arc::new(AtomicUsize::new(0)),
            forwarder: Mutex::new(None),
        }
    }

    fn sender(&self) -> Option<UnboundedSender<DispatchedEvent>> {
        self.weak_sender
            .as_ref()
            .and_then(|sender| sender.upgrade())
    }

    /// Keeps the channel open past `complete`, so that events sent afterwards
    /// still reach the callback; `End` then follows the last hold rather than
    /// `Complete`. `None` once the channel has closed.
    pub fn keep_open(&self) -> Option<KeepOpen> {
        let sender = self.sender()?;
        self.holders.fetch_add(1, Ordering::AcqRel);
        Some(KeepOpen {
            _sender: sender,
            holders: self.holders.clone(),
        })
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
        if self.holders.load(Ordering::Acquire) == 0 {
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

#[cfg(test)]
mod complete_outcome_tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use super::EventDispatcher;
    use crate::event::LoreCompleteEventData;
    use crate::event::LoreErrorDetail;
    use crate::event::LoreEvent;
    use crate::interface::LoreString;

    // Captures the single `Complete` event a `complete` call emits. The
    // callback is the real dispatch boundary, so the assertion reads the
    // event a consumer would actually receive.
    fn capture_complete(error: LoreErrorDetail) -> LoreCompleteEventData {
        let captured: Arc<Mutex<Option<LoreCompleteEventData>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        let callback: crate::interface::LoreEventCallback =
            Some(Box::new(move |event: &LoreEvent| {
                if let LoreEvent::Complete(data) = event {
                    *sink.lock().unwrap() = Some(data.clone());
                }
            }));

        let dispatcher = EventDispatcher::new(callback);
        lore_base::runtime::runtime().block_on(dispatcher.complete(error));

        let data = captured.lock().unwrap().take();
        data.expect("complete must emit a Complete event")
    }

    #[test]
    fn default_detail_completes_with_status_zero_and_empty_detail() {
        let data = capture_complete(LoreErrorDetail::default());

        assert_eq!(data.status, 0);
        assert_eq!(data.error.error_code, 0);
        assert!(data.error.message.is_empty());
        assert!(data.error.trace_locations.is_empty());
    }

    #[test]
    fn populated_detail_completes_with_its_code_and_carries_detail() {
        let detail = LoreErrorDetail {
            error_code: 13,
            message: LoreString::from("not found"),
            ..LoreErrorDetail::default()
        };

        let data = capture_complete(detail);

        // `status` is the detail's `error_code`, so the two agree.
        assert_eq!(data.status, 13);
        assert_eq!(data.error.error_code, 13);
        assert_eq!(data.error.message.as_str(), "not found");
    }
}

#[cfg(test)]
mod drain_tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use lore_base::lore_spawn;

    use super::EventDispatcher;
    use crate::event::LoreErrorDetail;
    use crate::event::LoreEvent;
    use crate::logging::LoreLogLevel;

    /// A caller that reads what its callback collected once `complete` returns
    /// must see everything sent ahead of `Complete`, however busy the other
    /// tasks sharing the dispatcher are. A send in flight on another task used
    /// to read as a subscription holding the channel open, so `drain` returned
    /// without waiting and the CLI could print — or exit — before its events
    /// arrived.
    #[test]
    fn complete_waits_for_delivery_while_another_task_is_sending() {
        let delivered: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = delivered.clone();
        let callback: crate::interface::LoreEventCallback =
            Some(Box::new(move |event: &LoreEvent| {
                // Lag behind the senders, so that the queue is never empty when
                // `complete` looks.
                std::thread::sleep(Duration::from_millis(1));
                sink.lock().unwrap().push(event.clone());
            }));
        let dispatcher = Arc::new(EventDispatcher::new(callback));

        // A task that logs while the command completes, the way a session
        // release or a store flush does.
        let done = Arc::new(AtomicBool::new(false));
        let busy = {
            let dispatcher = dispatcher.clone();
            let done = done.clone();
            lore_spawn!(async move {
                while !done.load(Ordering::Relaxed) {
                    dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
                        LoreLogLevel::Trace,
                        "busy".to_string(),
                    )));
                    tokio::task::yield_now().await;
                }
            })
        };

        for _ in 0..20 {
            dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
                LoreLogLevel::Info,
                "before complete".to_string(),
            )));
        }
        let status = lore_base::runtime::runtime().block_on(async {
            let status = dispatcher.complete(LoreErrorDetail::default()).await;
            done.store(true, Ordering::Relaxed);
            let _ = busy.await;
            status
        });
        assert_eq!(status, 0);

        let delivered = delivered.lock().unwrap();
        let complete_at = delivered
            .iter()
            .position(|event| matches!(event, LoreEvent::Complete(_)))
            .expect("Complete must have been through the callback when complete returns");
        let before = delivered[..complete_at]
            .iter()
            .filter(|event| {
                matches!(event, LoreEvent::Log(log) if log.message.as_str() == "before complete")
            })
            .count();
        assert_eq!(
            before, 20,
            "every event sent ahead of Complete arrives ahead of it"
        );
    }

    /// A hold is what keeps `complete` from waiting: a subscription's events
    /// come after the command has answered, and so does `End`.
    #[test]
    fn a_hold_lets_events_through_after_complete_and_ends_when_dropped() {
        let delivered: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = delivered.clone();
        let callback: crate::interface::LoreEventCallback =
            Some(Box::new(move |event: &LoreEvent| {
                sink.lock().unwrap().push(event.clone());
            }));
        let dispatcher = EventDispatcher::new(callback);
        let hold = dispatcher.keep_open().expect("channel is open");

        lore_base::runtime::runtime().block_on(async {
            dispatcher.complete(LoreErrorDetail::default()).await;
            dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
                LoreLogLevel::Info,
                "after complete".to_string(),
            )));
            drop(hold);
            dispatcher.completed.cancelled().await;
        });

        let delivered = delivered.lock().unwrap();
        let kinds: Vec<&str> = delivered
            .iter()
            .map(|event| match event {
                LoreEvent::Complete(_) => "complete",
                LoreEvent::Log(log) if log.message.as_str() == "after complete" => "after",
                LoreEvent::End(_) => "end",
                _ => "other",
            })
            .filter(|kind| *kind != "other")
            .collect();
        assert_eq!(kinds, ["complete", "after", "end"]);
    }
}
