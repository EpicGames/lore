// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that can share a process, as one integration-test binary.
//!
//! They link the library as an ordinary dependency instead of compiling it a
//! second time in test mode. A test that needs a process of its own goes in a
//! separate `tests/*.rs` file instead.

mod auth;
mod authnz;
mod cache;
mod correlation;
mod execution_state;
mod grpc;
mod hooks;
mod http;
mod lock;
mod notification;
mod plugins;
mod protocol;
mod quic;
mod root;
mod server;
mod settings;
mod store;
mod telemetry;
mod topology;
mod uds;
mod uds_server;
mod util;

/// Ensures that when we run our test suite for `lore-server` it is using
/// the same store policies that lore-server will run in production
#[ctor::ctor]
fn init_test_policies() {
    lore_storage::assume_server_policies();
}
