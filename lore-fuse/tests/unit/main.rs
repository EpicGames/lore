// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that can share a process, as one integration-test binary.
//!
//! They link the library as an ordinary dependency instead of compiling it a
//! second time in test mode. A test that needs a process of its own goes in a
//! separate `tests/*.rs` file instead.
#![cfg(target_os = "linux")]

mod abi;
mod buffer;
mod codec;
mod dispatch;
mod error;
mod filesystem;
mod init;
mod interrupt;
/// Shared with `tests/mount_read_write.rs`.
#[path = "../support/memory_fs.rs"]
mod memory_fs;
mod metrics;
mod mount;
mod mount_read_write;
mod mount_synthetic;
mod notify;
mod protocol_bench;
mod reply;
mod session;
/// Shared with `tests/protocol_bench.rs`.
#[path = "../support/synthetic.rs"]
mod synthetic;
mod test_stub;
mod test_support;
mod transport;
