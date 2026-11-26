// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes a process builds once it has sized its threads for running the service. A binary
//! of its own: the sizing has to happen before either runtime is built, and each is built once per
//! process.
use lore_base::runtime::*;

/// The service adds net threads to the core runtime's, and `LORE_MAX_THREADS` still bounds the
/// total: under a limit that leaves the core runtime and the syscall pool twice the syscall pool's
/// request to split, the blocking and net pools keep their requests, the syscall pool keeps its
/// own, and the core runtime runs the even share.
#[test]
fn the_service_runtimes_are_built_within_the_thread_limit() {
    let requested = threads_for_connections(
        default_thread_counts(processor_count(), lore_io::requested_max_threads()),
        NetConnections::Served(lore_transport::connection::MAX_STORAGE_CONNECTIONS),
    );
    let worker = requested.worker.min(requested.io);
    let limit = requested.blocking + requested.net + 2 * requested.io;
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::remove_var("LORE_NET_THREADS");
        std::env::set_var("LORE_MAX_THREADS", limit.to_string());
    }
    assert!(lore::size_threads_for_service());

    let counts = thread_counts();
    assert_eq!(
        counts,
        ThreadCounts {
            worker,
            ..requested
        }
    );
    assert_eq!(core_runtime().metrics().num_workers(), counts.worker);
    assert_eq!(net_runtime().metrics().num_workers(), counts.net);
    assert_eq!(lore_io::pool_stats().max_threads, counts.io);
}
