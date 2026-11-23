// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes a process that serves builds. A binary of its own: each runtime is built once per
//! process, and shutdown is terminal for it. One test, since a second would race this one to
//! build the runtimes and could shut them down under it.
use std::time::Duration;
use std::time::Instant;

use lore_base::log::LoreLogLevel;
use lore_base::runtime::*;

/// Shared with the unit tests and the other runtime binaries.
#[path = "support/timer.rs"]
mod timer;

/// Shared with `tests/default_runtime.rs`.
#[path = "support/report.rs"]
mod report;

/// A process that serves, with no configuration, asks for one core and one net thread per
/// processor, and a thread limit scales both runtimes from that one request: the core runtime is
/// not sized as if the net runtime had its default of two. Its core runtime keeps a timer per
/// worker: its sleeps and timeouts fire, and its shutdown completes with a sleep still pending,
/// which that timer wakes. A `LORE_CORE_TIMER` that names neither timer keeps it, and is reported
/// with a warning when the process reports its runtime, after the runtime is built.
#[test]
fn a_serving_process_sizes_its_runtimes_and_keeps_a_timer_per_core_worker() {
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
    let requested = ThreadCounts {
        net: processor_count(),
        ..default_thread_counts(processor_count(), lore_io::requested_max_threads())
    };
    let limit = (requested.total() / 2).max(POOL_COUNT * MIN_THREADS_PER_POOL);
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::remove_var("LORE_NET_THREADS");
        std::env::set_var(CORE_TIMER_ENV, "bogus");
        std::env::set_var("LORE_MAX_THREADS", limit.to_string());
    }

    let core = serving_runtime(None);
    let reported = report::reported_runtime();
    assert!(
        reported
            .iter()
            .any(|(level, message)| *level == LoreLogLevel::Warn && message.contains("bogus")),
        "{reported:?}"
    );
    let timer = if cfg!(tokio_unstable) {
        CoreTimer::PerWorker
    } else {
        CoreTimer::Shared
    };
    assert!(
        reported.contains(&report::reported_timer_line(timer)),
        "{reported:?}"
    );

    let counts = thread_counts();
    assert_eq!(counts, apportion_thread_counts(requested, limit));
    assert_eq!(core.metrics().num_workers(), counts.worker);
    assert_eq!(net_runtime().metrics().num_workers(), counts.net);

    timer::assert_timers_fire(&core);
    let sleep = timer::pending_sleep(&core);
    let start = Instant::now();
    runtime_shutdown_timeout(SHUTDOWN_TIMEOUT);
    assert!(
        start.elapsed() < SHUTDOWN_TIMEOUT,
        "shutdown ran to its timeout"
    );
    assert_eq!(timer::completes_after_shutdown(sleep), cfg!(tokio_unstable));
}
