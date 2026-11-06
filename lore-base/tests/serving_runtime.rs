// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes a process that serves builds. A binary of its own: each runtime is built once per
//! process.
use lore_base::runtime::*;

/// A process that serves, with no configuration, asks for one core and one net thread per
/// processor, and a thread limit scales both runtimes from that one request: the core runtime is
/// not sized as if the net runtime had its default of two.
#[test]
fn a_serving_process_sizes_both_runtimes_for_one_net_thread_per_processor() {
    let requested = ThreadCounts {
        net: processor_count(),
        ..default_thread_counts(processor_count(), lore_io::requested_max_threads())
    };
    let limit = (requested.total() / 2).max(POOL_COUNT * MIN_THREADS_PER_POOL);
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::remove_var("LORE_NET_THREADS");
        std::env::set_var("LORE_MAX_THREADS", limit.to_string());
    }

    let core = serving_runtime(None);

    let counts = thread_counts();
    assert_eq!(counts, apportion_thread_counts(requested, limit));
    assert_eq!(core.metrics().num_workers(), counts.worker);
    assert_eq!(net_runtime().metrics().num_workers(), counts.net);
}
