// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The spawn location a task reports to the observer, in a binary of its own for the reason given
//! in `task_lifecycle_observer.rs`.
use std::any::Any;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::LoreTaskLifecycleEvent;
use lore_base::runtime::net_runtime;
use lore_base::runtime::set_task_lifecycle_observer;

mod observer;

use observer::EventRecorder;
use observer::NO_CONTEXT_LABEL;
use observer::RecordingObserver;
use observer::SPAWNER_LABEL;

/// A task reports the line of the `lore_spawn!` that spawned it, whether or not it was spawned
/// under a `LORE_CONTEXT`.
#[test]
fn tasks_report_the_line_that_spawned_them() {
    let recorder = Arc::new(EventRecorder::default());
    assert!(
        set_task_lifecycle_observer(Box::new(RecordingObserver(Arc::clone(&recorder)))),
        "this binary installs the only observer of its process"
    );

    let context: Arc<dyn Any + Send + Sync> = Arc::new(SPAWNER_LABEL);
    let spawns = net_runtime().block_on(async {
        let (unscoped_line, unscoped) = (line!(), lore_spawn!(async {}));
        unscoped.await.expect("task joins");
        let scoped_line = LORE_CONTEXT
            .scope(context, async {
                let (line, scoped) = (line!(), lore_spawn!(async {}));
                scoped.await.expect("task joins");
                line
            })
            .await;
        [
            (unscoped_line, NO_CONTEXT_LABEL),
            (scoped_line, SPAWNER_LABEL),
        ]
    });

    let events = recorder.recorded();
    for (line, label) in spawns {
        assert!(
            events.iter().any(|recorded| {
                recorded.event == LoreTaskLifecycleEvent::Started
                    && recorded.file == file!()
                    && recorded.line == line
                    && recorded.context_label == label
            }),
            "no task spawned under {label:?} reported a start at {}:{line}",
            file!()
        );
    }
}
