// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that can share a process, as one integration-test binary.
//!
//! They link the library as an ordinary dependency instead of compiling it a
//! second time in test mode. A test that needs a process of its own goes in a
//! separate `tests/*.rs` file instead.

mod clone;
mod driver;
#[cfg(target_family = "windows")]
mod find;
#[cfg(target_family = "windows")]
mod iocp;
mod pool;
#[cfg(any(target_os = "macos", target_os = "linux", target_family = "windows"))]
mod priority;
mod psync;
mod rename;
#[cfg(target_family = "windows")]
mod stat;
