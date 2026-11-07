// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Kernel FUSE protocol crate: ABI, codec, mount, session, and dispatch.
//!
//! Speaks `/dev/fuse` directly, with no libfuse and no third-party FUSE crate. The crate's
//! `README.md` describes the design.
//!
//! Linux only: elsewhere the crate is empty, so a build of the whole workspace still succeeds.
#![cfg(target_os = "linux")]

pub mod abi;
pub mod buffer;
pub mod codec;
pub mod dispatch;
pub mod error;
pub mod filesystem;
pub mod init;
pub mod interrupt;
pub mod metrics;
pub mod mount;
pub mod notify;
pub mod reply;
pub mod session;
#[cfg(feature = "test-util")]
pub mod test_support;
pub mod transport;

pub use error::FuseError;
