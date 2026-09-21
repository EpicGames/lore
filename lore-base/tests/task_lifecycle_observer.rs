// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Observer behaviour that needs its own test binary: the observer is installed
//! once per process and cannot be replaced, so a test that installs one cannot
//! share a binary with any other test — it would observe their tasks, and they
//! would deny a later install. One test per binary keeps the install isolated.
use std::any::Any;
use std::sync::Arc;
use std::sync::Mutex;

use lore_base::lore_spawn_net_nocontext;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::LoreTaskLifecycleEvent;
use lore_base::runtime::LoreTaskSpawn;
use lore_base::runtime::TaskLifecycleObserver;
use lore_base::runtime::net_runtime;
use lore_base::runtime::set_task_lifecycle_observer;
use lore_base::runtime::try_lore_context;

const SPAWNER_LABEL: &str = "spawner";

struct RecordedEvent {
    event: LoreTaskLifecycleEvent,
    file: &'static str,
    line: u32,
    context_label: &'static str,
}

#[derive(Default)]
struct EventRecorder {
    events: Mutex<Vec<RecordedEvent>>,
}

impl EventRecorder {
    fn recorded(&self) -> std::sync::MutexGuard<'_, Vec<RecordedEvent>> {
        self.events
            .lock()
            .expect("no test holds this lock on panic")
    }
}

/// Shares its recorder with the test, which reads the events back after the observer is installed.
struct RecordingObserver(Arc<EventRecorder>);

impl TaskLifecycleObserver for RecordingObserver {
    fn context_label(&self) -> &'static str {
        try_lore_context()
            .and_then(|context| Arc::downcast::<&'static str>(context).ok())
            .map_or("<no context>", |label| *label)
    }

    fn on_event(&self, event: LoreTaskLifecycleEvent, spawn: &LoreTaskSpawn) {
        self.0.recorded().push(RecordedEvent {
            event,
            file: spawn.file,
            line: spawn.line,
            context_label: spawn.context_label,
        });
    }
}

/// Both ends of a task report the context it was spawned under, so an up/down counter keyed on
/// the label pairs the task's increment with its decrement. The task here runs with no context of
/// its own, which is the case the invariant has to survive: a label resolved when the task
/// finished rather than when it was spawned would name a different series than the one the start
/// counted against.
#[test]
fn events_report_the_context_the_task_was_spawned_under() {
    let recorder = Arc::new(EventRecorder::default());
    assert!(
        set_task_lifecycle_observer(Box::new(RecordingObserver(Arc::clone(&recorder)))),
        "this binary installs the only observer of its process"
    );

    let context: Arc<dyn Any + Send + Sync> = Arc::new(SPAWNER_LABEL);
    let task_ran_without_the_context = net_runtime()
        .block_on(LORE_CONTEXT.scope(context, async {
            lore_spawn_net_nocontext!(async { try_lore_context().is_none() }).await
        }))
        .expect("spawned task joins");

    assert!(
        task_ran_without_the_context,
        "the task carried its spawner's context, so this cannot show where the label was resolved"
    );

    let events = recorder.recorded();
    let started = events
        .iter()
        .find(|recorded| {
            recorded.event == LoreTaskLifecycleEvent::Started
                && recorded.context_label == SPAWNER_LABEL
        })
        .expect("the spawn under the context reported a start");
    let completed = events
        .iter()
        .find(|recorded| {
            recorded.event == LoreTaskLifecycleEvent::Completed
                && recorded.file == started.file
                && recorded.line == started.line
        })
        .expect("the same task reported a completion");

    assert_eq!(
        completed.context_label, SPAWNER_LABEL,
        "the task started under {SPAWNER_LABEL:?} but finished under {:?}",
        completed.context_label
    );
}
