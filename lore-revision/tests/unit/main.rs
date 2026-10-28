// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that can share a process, as one integration-test binary.
//!
//! They link the library as an ordinary dependency instead of compiling it a
//! second time in test mode. A test that needs a process of its own goes in a
//! separate `tests/*.rs` file instead.

mod auth;
mod branch;
mod change;
mod commit;
mod dependency;
mod event;
mod file;
mod filter;
mod fs;
mod global;
/// The fixtures the crate's process-isolated tests include, shared with these.
#[path = "../helper.rs"]
mod helper;
mod history;
mod immutable;
mod infer;
mod instance;
mod interface;
mod link;
mod lock;
mod merge;
mod metadata;
mod node;
mod paged_remote;
mod protocol;
mod relay;
mod repository;
mod revision;
mod stage;
mod state;
mod store;
mod util;

/// Points the unit tests at a global data and config directory of their own, rather than the
/// machine's, so a test's external `.lore` lookups and shared store registry stay inside it
/// instead of touching the real user's Lore directories. It is removed when the process exits
/// ([`remove_sandbox_global_dir`]).
///
/// A constructor because the environment is process-wide: this runs before the harness starts
/// the threads that would make writing to it a data race.
#[ctor::ctor]
fn sandbox_global_dir() {
    let sandbox = lore_base::test_util::TempDir::new("lore-revision-unit-tests-");

    // Safety: constructors run before `main`, so this is the single-threaded window where
    // writing to the environment has no reader to race.
    unsafe {
        std::env::set_var("LORE_GLOBAL_PATH", sandbox.path());
    }
    if let Ok(mut held) = SANDBOX.lock() {
        *held = Some(sandbox);
    }
}

/// The directory [`sandbox_global_dir`] points the tests at. A static is never dropped, so
/// [`remove_sandbox_global_dir`] takes it out to remove it.
static SANDBOX: std::sync::Mutex<Option<lore_base::test_util::TempDir>> =
    std::sync::Mutex::new(None);

/// Removes the sandbox directory when the process exits.
#[ctor::dtor]
fn remove_sandbox_global_dir() {
    if let Ok(mut held) = SANDBOX.lock() {
        drop(held.take());
    }
}
