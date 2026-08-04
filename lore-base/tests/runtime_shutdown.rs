// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shutdown behaviour that needs its own test binary: `runtime_shutdown_timeout`
//! is terminal for the process, so it cannot share a binary with tests that
//! expect a live runtime afterwards.
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::runtime::core_runtime;
use lore_base::runtime::net_runtime;
use lore_base::runtime::runtime_shutdown_timeout;
use lore_base::runtime::runtime_spawn_guarded;

/// `Runtime::block_on` and `Runtime::shutdown_timeout` both panic in an async
/// context, and the C `lore_shutdown()` can be called from one, so neither may
/// run on the calling thread. Guarded tasks must still be flushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_from_an_async_context_flushes_without_panicking() {
    let _core = core_runtime();
    let _net = net_runtime();

    let flushed = Arc::new(AtomicBool::new(false));
    let guarded = flushed.clone();
    runtime_spawn_guarded(async move {
        guarded.store(true, Ordering::Release);
    });

    runtime_shutdown_timeout(Duration::from_secs(5));

    assert!(
        flushed.load(Ordering::Acquire),
        "guarded task was not flushed by shutdown"
    );
}
