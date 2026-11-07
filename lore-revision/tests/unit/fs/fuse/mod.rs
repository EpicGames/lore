// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod access_recorder;
mod allocations;
mod attr;
mod bench;
mod clock;
mod counts;
mod deletions;
mod discover;
mod filesystem_state;
mod fragment_cache;
mod inode;
mod inode_number;
mod listing;
mod listing_cache;
mod local_tree;
mod lookup_latency;
mod notifications;
mod operation;
mod provider;
mod read;
mod read_cache_bench;
mod served;
mod spelling;
mod swap;
mod view;
mod vs_ext4_bench;
mod write;
mod write_mount;

// Fixtures shared with the FUSE binaries that need a process of their own.
#[path = "../../../support/bench_stats.rs"]
mod bench_stats;
#[path = "../../../support/fuse_fixture.rs"]
mod fuse_fixture;
#[path = "../../../support/fuse_test_util.rs"]
mod fuse_test_util;
mod holding_store;
mod staged_state;
