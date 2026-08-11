// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;

/// Default bounded channel capacity for discovery → execution pipeline.
pub const DEFAULT_WORK_CHANNEL_CAPACITY: usize = 200_000;

/// Ceiling on concurrently-spawned directory discovery tasks during a commit.
///
/// Build-time tunable: start heavily parallel here and reduce by
/// experimentation. Once this many directory tasks are live, deeper
/// directories recurse inline instead of spawning, which bounds peak memory
/// from discovery run-ahead (each live directory task pins a deserialized
/// name-table block). Because overflow recurses inline rather than blocking on
/// a permit, any value >= 1 is correct — it only trades parallelism for memory.
pub const MAX_CONCURRENT_DIRECTORY_TASKS: usize = 10_000;

/// Statistics accumulated by the discovery (producer) side.
/// When the producer finishes and the channel drains, these are the final totals.
#[derive(Default)]
pub struct DiscoveryStats {
    pub total_files: AtomicU64,
    pub total_bytes: AtomicU64,
    pub complete: AtomicBool,
}
