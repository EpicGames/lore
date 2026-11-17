// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::lore_spawn;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::logging::LoreLogLevel;
use lore_revision::relay::EventDispatcher;

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
    let callback: lore_revision::interface::LoreEventCallback =
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
    let callback: lore_revision::interface::LoreEventCallback =
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

/// A drain that has settled on waiting refuses a hold, whatever the channel's senders say.
/// A `send` holds an upgraded sender for the length of its push, so a hold granted on the
/// strength of one alone would keep open the channel the drain is waiting to see closed.
#[test]
fn a_hold_is_refused_behind_a_settled_drain() {
    let callback: lore_revision::interface::LoreEventCallback =
        Some(Box::new(|_event: &LoreEvent| {}));
    let dispatcher = Arc::new(EventDispatcher::new(callback));

    lore_base::runtime::runtime().block_on(async {
        // Stands in for the upgrade a `send` holds while it pushes: a sender the dispatcher
        // does not own and no hold accounts for.
        let in_flight = dispatcher.sender().expect("channel is open");

        let draining = {
            let dispatcher = dispatcher.clone();
            lore_spawn!(async move { dispatcher.drain().await })
        };

        // The forwarder is held for the length of the join, so a drain that has it is one
        // that has settled and is waiting.
        while dispatcher.forwarder.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        assert!(
            dispatcher.keep_open().is_none(),
            "a hold behind a settled drain keeps open the channel it waits on"
        );

        drop(in_flight);
        draining.await.expect("the draining task");
    });
}

/// A dispatcher handed back closes without the `End` its forwarder sends on a close, and returns
/// the callback, which a call run elsewhere then reports to, `End` included.
#[test]
fn a_handed_back_dispatcher_returns_its_callback_without_an_end() {
    let delivered: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = delivered.clone();
    let callback: lore_revision::interface::LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(event.clone());
        }));

    let handed_back = lore_base::runtime::runtime().block_on(async {
        let dispatcher = EventDispatcher::new(callback);
        dispatcher.hand_back().await
    });

    assert!(
        delivered.lock().unwrap().is_empty(),
        "no End reaches the callback"
    );
    let callback = handed_back.expect("the callback is handed back");
    let status = lore_base::runtime::runtime().block_on(async {
        EventDispatcher::new(Some(callback))
            .complete(LoreErrorDetail::default())
            .await
    });
    assert_eq!(status, 0);
    let delivered = delivered.lock().unwrap();
    assert_eq!(
        delivered.len(),
        2,
        "the second dispatcher's Complete and End"
    );
    assert!(matches!(delivered[1], LoreEvent::End(_)));
}
