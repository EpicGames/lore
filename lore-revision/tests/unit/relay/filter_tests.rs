// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use bytes::Bytes;
use lore_base::error::RepositoryNotFound;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::branch::push::LoreBranchPushStatsEventData;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::logging::LoreLogLevel;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryError;

use crate::helper::setup_test_execution;

/// A dispatcher filtered by `pass`, and the kinds of the events its callback receives, in order.
/// The callback calls `on_event` with each event before it records the event.
fn recording_dispatcher(
    pass: fn(&LoreEvent) -> bool,
    on_event: impl Fn(&LoreEvent) + Send + Sync + 'static,
) -> (EventDispatcher, Arc<Mutex<Vec<&'static str>>>) {
    let delivered: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = delivered.clone();
    let callback: lore_revision::interface::LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| {
            on_event(event);
            sink.lock().unwrap().push(match event {
                LoreEvent::Log(_) => "log",
                LoreEvent::BranchPushStats(_) => "push stats",
                LoreEvent::Error(_) => "error",
                LoreEvent::Complete(_) => "complete",
                LoreEvent::End(_) => "end",
                _ => "other",
            });
        }));
    (
        EventDispatcher::new(callback).with_event_filter(pass),
        delivered,
    )
}

fn log() -> LoreEvent {
    LoreEvent::Log(EventDispatcher::make_log(
        LoreLogLevel::Info,
        "event".to_string(),
    ))
}

fn push_stats() -> LoreEvent {
    LoreEvent::BranchPushStats(LoreBranchPushStatsEventData::default())
}

fn passes_log(event: &LoreEvent) -> bool {
    matches!(event, LoreEvent::Log(_))
}

/// A refused event never reaches the callback.
#[test]
fn a_refused_event_never_reaches_the_callback() {
    let (dispatcher, delivered) = recording_dispatcher(passes_log, |_| {});

    let status =
        lore_base::runtime::runtime().block_on(LORE_CONTEXT.scope(setup_test_execution(), async {
            dispatcher.send(push_stats());
            dispatcher.send(log());
            dispatcher.send_with_bytes(push_stats(), Bytes::from_static(b"payload"));
            dispatcher.send_error(RepositoryError::from(RepositoryNotFound {
                repository: "refused".to_string(),
            }));
            dispatcher.send(log());
            dispatcher
                .complete(LoreErrorDetail {
                    error_code: 13,
                    ..LoreErrorDetail::default()
                })
                .await
        }));

    assert_eq!(status, 13);
    assert_eq!(
        *delivered.lock().unwrap(),
        ["log", "log", "complete", "end"]
    );
}

/// A refused event is dropped by the send, before it reaches the channel. The callback is held on
/// the event sent ahead of it, so a refused event in the channel would still hold its buffer.
#[test]
fn a_refused_event_is_dropped_before_it_reaches_the_channel() {
    // `recv` returns once `release` is dropped: after the assertion, or as a failing assertion
    // unwinds.
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let (dispatcher, delivered) = recording_dispatcher(passes_log, move |_| {
        let _ = gate.lock().unwrap().recv();
    });
    let bytes = Bytes::from(vec![0u8; 16]);

    lore_base::runtime::runtime().block_on(LORE_CONTEXT.scope(setup_test_execution(), async {
        dispatcher.send(log());
        dispatcher.send_with_bytes(push_stats(), bytes.clone());
        assert!(
            bytes.is_unique(),
            "the refused event's buffer is held past its send"
        );
        drop(release);
        dispatcher.complete(LoreErrorDetail::default()).await;
    }));

    assert_eq!(*delivered.lock().unwrap(), ["log", "complete", "end"]);
}

/// `Complete` reaches the callback past a filter that refuses every event, carrying the status.
#[test]
fn complete_reaches_the_callback_past_a_filter_that_refuses_everything() {
    let carried: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    let seen = carried.clone();
    let (dispatcher, delivered) = recording_dispatcher(
        |_| false,
        move |event| {
            if let LoreEvent::Complete(data) = event {
                *seen.lock().unwrap() = Some(data.status);
            }
        },
    );

    let status =
        lore_base::runtime::runtime().block_on(LORE_CONTEXT.scope(setup_test_execution(), async {
            dispatcher.send(log());
            dispatcher
                .complete(LoreErrorDetail {
                    error_code: 13,
                    ..LoreErrorDetail::default()
                })
                .await
        }));

    assert_eq!((status, *carried.lock().unwrap()), (13, Some(13)));
    assert_eq!(*delivered.lock().unwrap(), ["complete", "end"]);
}
