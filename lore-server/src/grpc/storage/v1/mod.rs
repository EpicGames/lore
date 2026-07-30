// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod copy;
pub mod get;
pub mod get_metadata;
pub mod get_resolved;
pub mod mutable_compare_and_swap;
pub mod mutable_load;
pub mod mutable_store;
pub mod put;
pub mod query;
pub mod service;
pub mod verify;

#[cfg(test)]
pub(crate) mod test_utils;

/// Backpressure limit for streaming storage handlers — matches the QUIC public stream handler's 500 per stream × 8 streams = 4000 per connection so gRPC (single stream per connection) gets equivalent per-connection parallelism.
pub(crate) const STREAM_PROCESS_LIMIT: usize = 4000;
