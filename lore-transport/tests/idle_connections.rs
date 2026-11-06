// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The idle connection checks, as the tasks they spawn report themselves. A binary of its own: the
//! task lifecycle observer is installed once per process, and would observe any other test's tasks.
use std::sync::Arc;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::LoreTaskLifecycleEvent;
use lore_base::runtime::LoreTaskSpawn;
use lore_base::runtime::TaskLifecycleObserver;
use lore_base::runtime::set_task_lifecycle_observer;
use lore_base::runtime::try_lore_context;
use lore_transport::auth::AuthMode;
use lore_transport::connection::*;

/// The label of a task spawned under a `LORE_CONTEXT`.
const IN_A_CALL: &str = "in a call";

/// The label of a task spawned without one.
const IN_NO_CALL: &str = "in no call";

/// Records, as each task spawned in the connection module starts, the label of the context ambient
/// where it was spawned.
#[derive(Default)]
struct ConnectionTasks(parking_lot::Mutex<Vec<&'static str>>);

struct Observer(Arc<ConnectionTasks>);

impl TaskLifecycleObserver for Observer {
    fn context_label(&self) -> &'static str {
        if try_lore_context().is_some() {
            IN_A_CALL
        } else {
            IN_NO_CALL
        }
    }

    fn on_event(&self, event: LoreTaskLifecycleEvent, spawn: &LoreTaskSpawn) {
        if event == LoreTaskLifecycleEvent::Started && spawn.file.ends_with("connection.rs") {
            self.0.0.lock().push(spawn.context_label);
        }
    }
}

/// The checks, and the closes they start, belong to no call, so the checks don't carry the context
/// of the call that starts them, which would report them as its own: a call starts the checks, and
/// a check starts its close where no call's context is ambient.
#[tokio::test(flavor = "multi_thread")]
async fn the_idle_checks_and_their_closes_run_in_no_call() {
    let tasks = Arc::new(ConnectionTasks::default());
    assert!(
        set_task_lifecycle_observer(Box::new(Observer(tasks.clone()))),
        "this binary installs the only observer of its process"
    );
    add_connection(
        "lore://idle-in-no-call.test.invalid",
        "",
        false,
        AuthMode::Auto,
        Connection::detached(Vec::new()),
    );

    let checks = LORE_CONTEXT.sync_scope(Arc::new(()), || close_idle_connections(Duration::ZERO));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tasks.0.lock().len() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(checks);

    assert_eq!(
        *tasks.0.lock(),
        [IN_A_CALL, IN_NO_CALL],
        "the checks started in a call, and the close a check started"
    );
}
