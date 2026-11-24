// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Lets `lore_macro`'s derives name this crate as `::lore_base` inside it too.
extern crate self as lore_base;

pub mod allocator;
pub mod directories;
pub mod encoding;
pub mod env;
pub mod error;
pub mod fs;
pub mod log;
pub mod retry;
pub mod runtime;
// Compiled for the crates that enable the feature from their dev-dependencies,
// this one included. A normal build does not get it.
#[cfg(feature = "test-util")]
pub mod test_util;
pub mod text;
pub mod timestamp;
pub mod types;
pub mod units;
pub mod version;
pub mod wire_schema;
