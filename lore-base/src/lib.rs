// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shared Lore types and native runtime services.
//!
//! Disable the default `native` feature for portable consumers such as Workers
//! Wasm. This retains types, errors, text validation and version information,
//! but omits OS-backed services, native allocation and random-number sources.
//! Cargo features are additive: every dependency in a portable build must avoid
//! enabling `native` for this crate.
#[cfg(feature = "native")]
pub mod allocator;
#[cfg(feature = "native")]
pub mod directories;
#[cfg(feature = "native")]
pub mod env;
pub mod error;
pub mod metadata;
#[cfg(feature = "native")]
pub mod fs;
#[cfg(feature = "native")]
pub mod log;
#[cfg(feature = "native")]
pub mod retry;
#[cfg(feature = "native")]
pub mod runtime;
// Compiled for the crates that enable the feature from their dev-dependencies,
// this one included. A normal build does not get it.
#[cfg(feature = "test-util")]
pub mod test_util;
pub mod text;
pub mod types;
pub mod version;
