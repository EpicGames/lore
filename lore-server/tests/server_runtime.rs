// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The runtimes the server builds. A binary of its own: each runtime is built once per process.
use lore_base::runtime::*;
use lore_server::settings::Settings;

/// The server with its built-in settings runs one core thread and one net thread per processor,
/// since serving thousands of concurrent client connections is its normal case.
#[test]
fn the_server_runs_one_core_and_one_net_thread_per_processor_by_default() {
    let config = lore_base::test_util::TempDir::new("lore-server-runtime-");
    // Safety: single-threaded, before any runtime exists, and nothing else in this target reads
    // the environment concurrently.
    unsafe {
        for var in [
            "LORE_NET_THREADS",
            "LORE_MAX_THREADS",
            "LORE__TOKIO__NET_THREADS",
            "LORE__TOKIO__WORKER_THREADS",
        ] {
            std::env::remove_var(var);
        }
    }
    let (settings, _) =
        Settings::load(config.path().to_str(), None).expect("the server's built-in settings");

    let core = lore_server::server::server_runtime(&settings);

    let processors = processor_count();
    assert_eq!(
        core.metrics().num_workers(),
        processors.max(MIN_THREADS_PER_POOL)
    );
    assert_eq!(net_runtime().metrics().num_workers(), processors);
}
