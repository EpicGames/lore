// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that can share a process, as one integration-test binary.
//!
//! They link the library as an ordinary dependency instead of compiling it a
//! second time in test mode. A test that needs a process of its own goes in a
//! separate `tests/*.rs` file instead.

mod chunker;
mod compress;
mod concurrency;
mod content;
mod deduplicated_gc;
mod defragment;
mod fragment_engine;
mod hash;
mod immutable_store;
mod local;
mod maintenance;
mod mutable_conformance;
mod packstore;
mod read;
mod read_flight;
mod store_fixture;
mod write;
mod write_stats;
mod write_tracker;
