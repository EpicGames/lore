// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_base::runtime::runtime;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::WeakUnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
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

pub struct EventDispatcher {
    pub correlation_id: String,
    pub completed: CancellationToken,
    pub weak_sender: Option<WeakUnboundedSender<DispatchedEvent>>,
    pub strong_sender: Mutex<Option<UnboundedSender<DispatchedEvent>>>,
}

impl Default for EventDispatcher {
    fn default() -> Self {
        Self {
            correlation_id: String::default(),
            completed: CancellationToken::new(),
            weak_sender: None,
            strong_sender: Mutex::new(None),
        }
    }
}

impl EventDispatcher {
    #[allow(clippy::disallowed_methods)]
    pub fn new(callback: LoreEventCallback) -> Self {
        let completed = CancellationToken::new();
        let (sender, mut receiver) = unbounded_channel();
        let weak_sender = sender.downgrade();
        if let Some(callback) = callback {
            let completed = completed.clone();

            // Spawn a forwarder task which will exit once all dispatchers
            // have terminated and mpsc channel has no producers. Each
            // item carries an optional `Bytes` keepalive; the forwarder
            // drops it AFTER the callback returns, so any `LoreBytes`
            // view in the event points at a live buffer for the full
            // callback invocation.
            runtime().spawn(async move {
                while let Some((event, _keepalive)) = receiver.recv().await {
                    callback(&event);
                    // `_keepalive` drops here — the referenced buffer
                    // is released after the callback has finished.
                }
                callback(&LoreEvent::End(LoreEndEventData::default()));
                completed.cancel();
            });
        } else {
            completed.cancel();
        };

        Self {
            correlation_id: String::default(),
            completed,
            weak_sender: Some(weak_sender),
            strong_sender: Mutex::new(Some(sender)),
        }
    }

    pub fn no_dispatch() -> Self {
        Self {
            correlation_id: String::default(),
            completed: CancellationToken::new(),
            weak_sender: None,
            strong_sender: Mutex::new(None),
        }
    }

    pub fn sender(&self) -> Option<UnboundedSender<DispatchedEvent>> {
        self.weak_sender
            .as_ref()
            .and_then(|sender| sender.upgrade())
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

        // Drop this strong reference, let the dispatcher task exit out and signal the end event
        // if this is the only strong reference to the event channel
        drop(self.strong_sender.lock().await.take());

        // If there are other strong references remaining it means the end event will come
        // whenever that completes (such as an ongoing notification subscription)
        if self
            .weak_sender
            .as_ref()
            .map(|sender| sender.strong_count())
            .unwrap_or_default()
            == 0
        {
            self.completed.cancelled().await;
        }

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
