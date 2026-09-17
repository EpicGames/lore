// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Sizing a process's threads for relaying, which is what the Lore client does
//! at startup before it builds a runtime and calls in.
//!
//! A test target of its own because the runtime is built once per process: a test
//! sharing a process with others could not be the one that builds it, and would
//! measure whatever settings won the race instead.

/// The whole point of the sizing: a process that relays gets the pools of one
/// that relays, not of one that works.
///
/// It has to happen before the runtime is built, and both callers arrange that —
/// the client from its own startup, the library from its call entry points. This
/// covers the sizing itself; that the client calls it early enough is covered by
/// there being no runtime yet at the point it does.
#[test]
fn sizing_for_relaying_builds_the_runtime_with_the_fewest_workers() {
    // Both settings, since relaying needs both. Named through the environment so
    // this reads nothing of the machine it runs on: a developer with the service
    // turned off would otherwise find this test measuring a working runtime.
    //
    // Safety: single-threaded, before any runtime exists, and nothing else in
    // this target reads the environment concurrently.
    unsafe {
        std::env::set_var("LORE_USE_SERVICE", "1");
        std::env::set_var("LORE_SERVICE_EXECUTABLE", "/nonexistent/lore");
    }

    assert!(
        lore::will_use_service(),
        "both settings are named, so this process relays"
    );

    lore::size_threads_for_relaying();

    let expected = lore_base::runtime::TokioSettings::relay_only()
        .worker_threads
        .expect("sizing for relaying asks for a worker count");
    assert_eq!(
        lore::runtime().metrics().num_workers(),
        expected,
        "the runtime must be built with the relaying worker count"
    );
}
