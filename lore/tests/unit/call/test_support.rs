// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shared test harness for the dispatch wrappers: a callback that records the events a call sends,
//! readers over the recorded events, and an in-memory store registered as a storage handle.

use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;

use lore::interface::LoreEventCallback;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_revision::event::LoreCompleteEventData;
use lore_revision::event::LoreEvent;

/// A callback that keeps every event it receives whole, and the events it has kept.
pub(crate) fn recording_callback() -> (Arc<Mutex<Vec<LoreEvent>>>, LoreEventCallback) {
    let events: Arc<Mutex<Vec<LoreEvent>>> = Arc::default();
    let sink = events.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        sink.lock().unwrap().push(event.clone());
    }));
    (events, callback)
}

/// The events in `events` other than log lines, in order. Once `lore::log::initialize` has
/// installed the process-wide log callback, a call's log lines reach its callback too, so how many
/// the callback receives depends on what ran before the call in the process.
pub(crate) fn without_log_lines(events: &[LoreEvent]) -> Vec<&LoreEvent> {
    events
        .iter()
        .filter(|event| !matches!(event, LoreEvent::Log(_)))
        .collect()
}

/// An in-memory store with `identity`, registered as a storage handle.
pub(crate) async fn registered_store(identity: &str) -> (Arc<StoreInternal>, LoreStore) {
    let store = in_memory_for_tests(identity).await;
    let store_handle = handle::register(store.clone());
    (store, store_handle)
}

/// The `Complete` events in `events`, in order.
pub(crate) fn completes(events: &[LoreEvent]) -> Vec<&LoreCompleteEventData> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::Complete(data) => Some(data),
            _ => None,
        })
        .collect()
}

/// Whether `events` holds an `Error` event.
pub(crate) fn has_error_event(events: &[LoreEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, LoreEvent::Error(_)))
}

/// The `status` of every `Complete` event in `events`, in order.
pub(crate) fn complete_statuses(events: &[LoreEvent]) -> Vec<i32> {
    completes(events).iter().map(|data| data.status).collect()
}

/// The message of the first `Complete` event in `events`. It is the only place a refused batch
/// reports the offending entry's index and the rule it broke; the entry's own terminal carries an
/// error code alone.
pub(crate) fn complete_message(events: &[LoreEvent]) -> &str {
    completes(events)
        .first()
        .expect("the call must complete")
        .error
        .message
        .as_str()
}

/// The handle id of the first `StorageOpened` event in `events`.
pub(crate) fn opened_handle_id(events: &[LoreEvent]) -> Option<u64> {
    events.iter().find_map(|event| match event {
        LoreEvent::StorageOpened(data) => Some(data.handle_id),
        _ => None,
    })
}

/// Events a callback recorded. Its `Debug` is the events as JSON, for the message of a failed
/// assertion; `LoreEvent` has no `Debug`.
pub(crate) struct Recorded(Vec<LoreEvent>);

impl Deref for Recorded {
    type Target = [LoreEvent];

    fn deref(&self) -> &[LoreEvent] {
        &self.0
    }
}

impl std::fmt::Debug for Recorded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(&self.0) {
            Ok(json) => formatter.write_str(&json),
            Err(err) => write!(formatter, "events not serializable: {err}"),
        }
    }
}

/// The events `events` holds, leaving it empty.
pub(crate) fn take_recorded(events: &Mutex<Vec<LoreEvent>>) -> Recorded {
    Recorded(std::mem::take(&mut events.lock().unwrap()))
}

/// The events `events` holds other than log lines, as [`without_log_lines`] keeps them, in order,
/// leaving it empty.
pub(crate) fn take_without_log_lines(events: &Mutex<Vec<LoreEvent>>) -> Vec<LoreEvent> {
    let mut taken = std::mem::take(&mut *events.lock().unwrap());
    taken.retain(|event| !matches!(event, LoreEvent::Log(_)));
    taken
}
