// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes a process builds under a thread limit, with the syscall pool's request raised. A
//! binary of its own: each runtime and the syscall pool are built once per process, from the
//! environment they read then.
use lore_base::runtime::*;

/// The syscall pool's request `LORE_IO_POOL_THREADS` sets.
const IO_POOL_THREADS: usize = 32;

/// Under a `LORE_MAX_THREADS` that leaves the core workers and a syscall pool raised by
/// `LORE_IO_POOL_THREADS` twice the raised request to split, the pool keeps its request, the pool
/// the process builds runs that many threads, and the core runtime runs the even share.
#[test]
fn a_raised_syscall_pool_keeps_its_request_under_a_limit() {
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        std::env::remove_var("LORE_NET_THREADS");
        std::env::set_var("LORE_IO_POOL_THREADS", IO_POOL_THREADS.to_string());
    }
    let requested = default_thread_counts(processor_count(), lore_io::requested_max_threads());
    assert_eq!(requested.io, IO_POOL_THREADS);
    let limit = requested.blocking + requested.net + 2 * IO_POOL_THREADS;
    // Safety: as above.
    unsafe {
        std::env::set_var("LORE_MAX_THREADS", limit.to_string());
    }

    let core = core_runtime();
    let counts = thread_counts();
    assert_eq!(
        counts,
        ThreadCounts {
            worker: requested.worker.min(IO_POOL_THREADS),
            ..requested
        }
    );
    assert_eq!(core.metrics().num_workers(), counts.worker);
    assert_eq!(lore_io::pool_stats().max_threads, IO_POOL_THREADS);
}
