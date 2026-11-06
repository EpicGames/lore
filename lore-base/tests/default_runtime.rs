// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes a process builds when it sizes them for nothing. A binary of its own: each runtime
//! is built once per process.
use lore_base::runtime::*;

/// A process that sets no connections and no net threads gets the defaults, one core thread per
/// processor and the net runtime's two, not the sizing for a command's connections.
#[test]
fn a_process_that_sizes_for_nothing_keeps_the_defaults() {
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::remove_var("LORE_NET_THREADS");
        std::env::remove_var("LORE_MAX_THREADS");
    }

    let core = core_runtime();

    let defaults = default_thread_counts(processor_count(), lore_io::requested_max_threads());
    assert_eq!(thread_counts(), defaults);
    assert_eq!(core.metrics().num_workers(), defaults.worker);
    assert_eq!(net_runtime().metrics().num_workers(), DEFAULT_NET_THREADS);
}
