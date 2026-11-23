// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes a process builds when it sizes them for nothing. A binary of its own: each runtime
//! is built once per process, and shutdown is terminal for it.
use std::time::Duration;

use lore_base::runtime::*;

/// Shared with the unit tests and the other runtime binaries.
#[path = "support/timer.rs"]
mod timer;

/// Shared with `tests/serving_runtime.rs`.
#[path = "support/report.rs"]
mod report;

/// A process that sets no connections and no net threads gets the defaults, one core thread per
/// processor and the net runtime's two, not the sizing for a command's connections. Its core
/// runtime keeps Tokio's default timer, whatever `LORE_CORE_TIMER` says: its sleeps and timeouts
/// fire, and it fails a sleep still pending at shutdown. A serving runtime asked for later is that
/// same runtime, and the report names its timer.
#[test]
fn a_process_that_sizes_for_nothing_keeps_the_defaults() {
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::remove_var("LORE_NET_THREADS");
        std::env::remove_var("LORE_MAX_THREADS");
        std::env::set_var(CORE_TIMER_ENV, "per-worker");
    }

    let core = core_runtime();
    assert_eq!(
        lore_base::allocator::reap::heap_reap_running(),
        lore_base::allocator::reap::heap_reap_enabled(),
        "the core runtime starts the heap reap thread where rpmalloc is the global allocator"
    );

    let defaults = default_thread_counts(processor_count(), lore_io::requested_max_threads());
    assert_eq!(thread_counts(), defaults);
    assert_eq!(core.metrics().num_workers(), defaults.worker);
    assert_eq!(net_runtime().metrics().num_workers(), DEFAULT_NET_THREADS);

    serving_runtime(None);
    assert!(
        report::reported_runtime().contains(&report::reported_timer_line(CoreTimer::Shared)),
        "the report names a timer the core runtime does not keep"
    );

    timer::assert_timers_fire(&core);
    let sleep = timer::pending_sleep(&core);
    runtime_shutdown_timeout(Duration::from_secs(10));
    assert!(!timer::completes_after_shutdown(sleep));
}
