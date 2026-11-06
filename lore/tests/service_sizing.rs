// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes a process builds once it has sized its threads for running the service. A binary
//! of its own: the sizing has to happen before either runtime is built, and each is built once per
//! process.
use lore_base::runtime::*;

/// The service adds net threads to the core runtime's, and `LORE_MAX_THREADS` still bounds the
/// total: a limit of half the threads requested scales both runtimes down to fit.
#[test]
fn the_service_runtimes_are_built_within_the_thread_limit() {
    let requested = threads_for_connections(
        default_thread_counts(processor_count(), lore_io::requested_max_threads()),
        NetConnections::Served(lore_transport::connection::MAX_STORAGE_CONNECTIONS),
    );
    let limit = (requested.total() / 2).max(POOL_COUNT * MIN_THREADS_PER_POOL);
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::remove_var("LORE_NET_THREADS");
        std::env::set_var("LORE_MAX_THREADS", limit.to_string());
    }
    assert!(lore::size_threads_for_service());

    let counts = thread_counts();
    assert_eq!(counts, apportion_thread_counts(requested, limit));
    assert!(
        counts.total() <= limit,
        "{counts:?} exceeds the limit of {limit}"
    );
    assert_eq!(core_runtime().metrics().num_workers(), counts.worker);
    assert_eq!(net_runtime().metrics().num_workers(), counts.net);
}
