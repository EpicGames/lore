// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shutdown behaviour that needs its own test binary: `runtime_shutdown_timeout`
//! is terminal for the process, so it cannot share a binary with tests that
//! expect a live runtime afterwards.
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::runtime::claim_runtime_shutdown;
use lore_base::runtime::core_runtime;
use lore_base::runtime::net_runtime;
use lore_base::runtime::runtime_shutdown_started;
use lore_base::runtime::runtime_shutdown_timeout;
use lore_base::runtime::runtime_spawn_guarded;
use lore_base::runtime::shutdown_block_on;

/// `Runtime::block_on` and `Runtime::shutdown_timeout` both panic in an async
/// context, and the C `lore_shutdown()` can be called from one, so neither may
/// run on the calling thread. Guarded tasks must still be flushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_from_an_async_context_flushes_without_panicking() {
    let _core = core_runtime();
    let _net = net_runtime();

    assert!(
        !runtime_shutdown_started(),
        "shutdown must not report before one is asked for"
    );

    // Only one caller gets the claim, so only one runs the teardown.
    assert!(claim_runtime_shutdown(), "the first claim must win");
    assert!(
        !claim_runtime_shutdown(),
        "a second claim must lose, so only one caller runs the teardown"
    );
    assert!(
        runtime_shutdown_started(),
        "the claim must close admission immediately"
    );

    // Claiming the shutdown closes admission for new calls, and must not stop the
    // teardown's own work: the drains and the connection close both run through
    // `shutdown_block_on` after this point, and gating it on the claim would skip
    // them and lose the writes they flush.
    let drained = Arc::new(AtomicBool::new(false));
    let flag = drained.clone();
    let completed = shutdown_block_on(
        async move {
            flag.store(true, Ordering::Release);
        },
        Duration::from_secs(5),
    );
    assert!(completed, "teardown work must still run after the claim");
    assert!(
        drained.load(Ordering::Acquire),
        "teardown work must actually be driven, not skipped"
    );

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
    assert!(runtime_shutdown_started(), "admission stays closed");

    // Repeating the shutdown is a no-op rather than a panic.
    runtime_shutdown_timeout(Duration::from_secs(5));
}
