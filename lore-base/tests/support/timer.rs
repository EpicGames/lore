// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Checks on the timer a runtime keeps, shared by the test binaries that build a core runtime.
use std::pin::Pin;
use std::sync::mpsc::RecvTimeoutError;
use std::task::Context;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use tokio::runtime::Handle;
use tokio::task::JoinSet;
use tokio::time::Sleep;

const DELAY: Duration = Duration::from_millis(10);

/// The tasks [`assert_timers_fire`] sets timers from at once.
const TASKS: usize = 256;

/// How long [`assert_timers_fire`] waits, by the wall clock, for its timers to fire.
const BOUND: Duration = Duration::from_secs(30);

/// Sets a sleep and a timeout on `runtime` from outside its workers, then many at once from tasks
/// on its workers, each sleep reset to a later deadline, and asserts each fires no earlier than its
/// deadline and all within [`BOUND`].
pub fn assert_timers_fire(runtime: &Handle) {
    let runtime = runtime.clone();
    let (done, finished) = std::sync::mpsc::channel();
    // A thread of its own, so a timer that never fires fails the test rather than hang it.
    let worker = std::thread::spawn(move || {
        runtime.block_on(set_timers());
        let _ = done.send(());
    });
    if let Err(RecvTimeoutError::Timeout) = finished.recv_timeout(BOUND) {
        panic!("the timers did not fire within {BOUND:?}");
    }
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

async fn set_timers() {
    let start = Instant::now();
    tokio::time::sleep(DELAY).await;
    let timed_out = tokio::time::timeout(DELAY, std::future::pending::<()>()).await;
    assert!(
        timed_out.is_err(),
        "the timeout let a pending future finish"
    );
    assert!(start.elapsed() >= 2 * DELAY, "a timer fired early");

    let mut tasks = JoinSet::new();
    for _ in 0..TASKS {
        lore_base::lore_spawn!(tasks, async {
            let start = Instant::now();
            let sleep = tokio::time::sleep(DELAY);
            tokio::pin!(sleep);
            sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + 2 * DELAY);
            sleep.await;
            let slept = start.elapsed();
            let timed_out = tokio::time::timeout(DELAY, std::future::pending::<()>()).await;
            (slept, timed_out.is_err(), start.elapsed() - slept)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        let (slept, timed_out, waited) = joined.expect("task joins");
        assert!(slept >= 2 * DELAY, "a reset sleep fired after {slept:?}");
        assert!(timed_out, "the timeout let a pending future finish");
        assert!(waited >= DELAY, "a timeout fired after {waited:?}");
    }
}

/// A sleep on `runtime` that lasts far longer than any test, set and polled once from the
/// calling thread, outside the runtime's workers.
pub fn pending_sleep(runtime: &Handle) -> Pin<Box<Sleep>> {
    let _entered = runtime.enter();
    let mut sleep = Box::pin(tokio::time::sleep(Duration::from_secs(3600)));
    let mut context = Context::from_waker(Waker::noop());
    assert!(sleep.as_mut().poll(&mut context).is_pending());
    sleep
}

/// Whether `sleep`, set by [`pending_sleep`], completes when polled after its runtime has shut
/// down, which tells the timers apart. Tokio's alternative timer, a wheel per worker, wakes the
/// sleeps in its wheels and those set from outside its workers as the workers stop; a sleep a
/// worker set but has not yet put in its wheel is not woken. Tokio's default timer fails every
/// pending sleep, and a failed sleep panics when polled.
pub fn completes_after_shutdown(mut sleep: Pin<Box<Sleep>>) -> bool {
    let mut context = Context::from_waker(Waker::noop());
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sleep.as_mut().poll(&mut context).is_ready()
    }))
    .unwrap_or(false)
}
