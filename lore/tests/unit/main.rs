// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that can share a process, as one integration-test binary.
//!
//! They link the library as an ordinary dependency instead of compiling it a
//! second time in test mode. A test that needs a process of its own goes in a
//! separate `tests/*.rs` file instead.

mod auth;
mod branch;
mod call;
mod call_delegation;
mod log;
mod remote;
mod repository;
mod revision;
mod revision_tree;
mod sandbox_tests;
mod storage;

/// Points the unit tests at a global config, credential store and service socket of
/// their own, rather than the machine's, and keeps them from relaying.
///
/// A test sets its own fixture up in the process running it, and a service knows
/// nothing of that fixture, so relaying a test's calls fails them wholesale. A
/// developer who turns the service on for their own use must still be able to run
/// the tests. Turning relaying off does not stop a test binding a socket, and
/// `remote::network`'s round trip binds whatever name it is given.
/// `.cargo/config.toml` covers both for everything cargo runs, with a fixed socket,
/// which leaves a concurrent run of this crate, and a test binary run directly or
/// from an editor.
///
/// A constructor because the environment is process-wide: this runs before the
/// harness starts the threads that would make writing to it a data race.
#[ctor::ctor]
fn sandbox_machine_settings() {
    let sandbox = std::env::temp_dir().join(format!("lore-unit-tests-{}", std::process::id()));
    std::fs::create_dir_all(&sandbox)
        .unwrap_or_else(|error| panic!("creating the sandbox at {}: {error}", sandbox.display()));

    // Safety: constructors run before `main`, so this is the single-threaded
    // window where writing to the environment has no reader to race.
    unsafe {
        std::env::set_var("LORE_USE_SERVICE", "0");
        std::env::set_var("LORE_GLOBAL_PATH", &sandbox);
        std::env::set_var("LORE_AUTH_PATH", &sandbox);
        std::env::set_var(
            "LORE_SERVICE_SOCKET",
            format!("lore_service-unit-{}", std::process::id()),
        );
    }
}
