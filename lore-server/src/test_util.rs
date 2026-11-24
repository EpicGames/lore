// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Points where a test stops the server's own code, so that its race tests run in every unit
//! run.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

/// A point of a push where a task run under [`step_push`] stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushStep {
    /// A fast-forward merge has diffed the pushed revision against the latest and not yet
    /// applied the diff.
    MergeDiffed,
}

/// Where a held task reports each step it reaches, and what it waits for to go on.
#[derive(Clone)]
struct Hold {
    reached: UnboundedSender<PushStep>,
    resume: Arc<Notify>,
}

tokio::task_local! {
    static STEPPED: Hold;
}

/// Runs `future`, stopping at each [`PushStep`] it reaches: the step is sent on `reached`, and
/// the task waits for a notification of `resume` before it goes on. Tasks `future` spawns don't
/// stop.
pub fn step_push<F: Future>(
    reached: UnboundedSender<PushStep>,
    resume: Arc<Notify>,
    future: F,
) -> impl Future<Output = F::Output> {
    STEPPED.scope(Hold { reached, resume }, future)
}

/// Stops this task at `step`, where it runs under [`step_push`].
pub(crate) async fn push_step(step: PushStep) {
    let Ok(hold) = STEPPED.try_with(Hold::clone) else {
        return;
    };
    if hold.reached.send(step).is_ok() {
        hold.resume.notified().await;
    }
}
