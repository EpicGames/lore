// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that can share a process, as one integration-test binary.
//!
//! They link the library as an ordinary dependency instead of compiling it a
//! second time in test mode. A test that needs a process of its own goes in a
//! separate `tests/*.rs` file instead.

mod auth;
mod connection;
mod grpc;
mod local_socket;
mod quic;
mod session;
mod types;
mod uds;

/// Points the token store at a directory of this process's own, in its fallback form, so that no
/// test reads or writes the developer's credentials or keyring. It is removed when the process
/// exits ([`remove_token_store_sandbox`]).
///
/// A constructor because the environment is process-wide: this runs before the harness starts
/// the threads that would make writing to it a data race.
#[ctor::ctor]
fn sandbox_token_store() {
    let sandbox = lore_base::test_util::TempDir::new("lore-transport-unit-tests-");

    // Safety: constructors run before `main`, so this is the single-threaded
    // window where writing to the environment has no reader to race.
    unsafe {
        std::env::set_var("LORE_AUTH_PATH", sandbox.path());
        std::env::set_var("LORE_AUTH_STORE", "fallback");
    }
    if let Ok(mut held) = SANDBOX.lock() {
        *held = Some(sandbox);
    }
}

/// The directory [`sandbox_token_store`] points the token store at. A static is never dropped, so
/// [`remove_token_store_sandbox`] takes it out to remove it.
static SANDBOX: std::sync::Mutex<Option<lore_base::test_util::TempDir>> =
    std::sync::Mutex::new(None);

/// Removes the sandbox directory when the process exits.
#[ctor::dtor]
fn remove_token_store_sandbox() {
    if let Ok(mut held) = SANDBOX.lock() {
        drop(held.take());
    }
}
