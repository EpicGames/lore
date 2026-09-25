// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A task lifecycle observer that records every event, for the test binaries that install one.
use std::sync::Arc;
use std::sync::Mutex;

use lore_base::runtime::LoreTaskLifecycleEvent;
use lore_base::runtime::LoreTaskSpawn;
use lore_base::runtime::TaskLifecycleObserver;
use lore_base::runtime::try_lore_context;

/// The label of the `LORE_CONTEXT` the tests spawn under.
pub const SPAWNER_LABEL: &str = "spawner";

/// The label [`RecordingObserver`] reports for a task spawned without a `LORE_CONTEXT`.
pub const NO_CONTEXT_LABEL: &str = "<no context>";

pub struct RecordedEvent {
    pub event: LoreTaskLifecycleEvent,
    pub file: &'static str,
    pub line: u32,
    pub context_label: &'static str,
}

#[derive(Default)]
pub struct EventRecorder {
    events: Mutex<Vec<RecordedEvent>>,
}

impl EventRecorder {
    pub fn recorded(&self) -> std::sync::MutexGuard<'_, Vec<RecordedEvent>> {
        self.events
            .lock()
            .expect("no test holds this lock on panic")
    }
}

/// Shares its recorder with the test, which reads the events back after the observer is installed.
pub struct RecordingObserver(pub Arc<EventRecorder>);

impl TaskLifecycleObserver for RecordingObserver {
    fn context_label(&self) -> &'static str {
        try_lore_context()
            .and_then(|context| Arc::downcast::<&'static str>(context).ok())
            .map_or(NO_CONTEXT_LABEL, |label| *label)
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
