// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The core runtime a process that serves builds when `LORE_CORE_TIMER` asks for the shared timer.
//! A binary of its own: the variable is read when the runtime is built, which is once per process.
use std::time::Duration;

use lore_base::runtime::*;

/// Shared with the unit tests and the other runtime binaries.
#[path = "support/timer.rs"]
mod timer;

/// With `LORE_CORE_TIMER=shared`, a serving process's core runtime keeps Tokio's default timer: its
/// sleeps and timeouts fire, and it fails a sleep still pending at shutdown.
#[test]
fn a_serving_process_keeps_the_shared_timer_when_the_variable_asks_for_it() {
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::set_var(CORE_TIMER_ENV, "shared");
    }

    let core = serving_runtime(None);

    timer::assert_timers_fire(&core);
    let sleep = timer::pending_sleep(&core);
    runtime_shutdown_timeout(Duration::from_secs(10));
    assert!(!timer::completes_after_shutdown(sleep));
}
