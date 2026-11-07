// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_fuse::abi::Opcode;
use lore_fuse::metrics::*;

#[test]
fn record_one_sample_per_request_updates_count_and_latency() {
    let metrics = Metrics::new();

    metrics.record(Opcode::Getattr, Duration::from_micros(10), 0);
    metrics.record(Opcode::Getattr, Duration::from_micros(20), 0);

    let snapshot = metrics.snapshot(Opcode::Getattr);
    assert_eq!(snapshot.count, 2);
    assert!(snapshot.max_ns >= Duration::from_micros(20).as_nanos() as u64);
}

#[test]
fn different_opcodes_do_not_share_counters() {
    let metrics = Metrics::new();

    metrics.record(Opcode::Getattr, Duration::from_micros(1), 0);

    assert_eq!(metrics.snapshot(Opcode::Getattr).count, 1);
    assert_eq!(metrics.snapshot(Opcode::Lookup).count, 0);
}

#[test]
fn max_allocations_tracks_high_water_mark() {
    let metrics = Metrics::new();

    metrics.record(Opcode::Read, Duration::from_nanos(1), 0);
    metrics.record(Opcode::Read, Duration::from_nanos(1), 5);
    metrics.record(Opcode::Read, Duration::from_nanos(1), 2);

    assert_eq!(metrics.snapshot(Opcode::Read).max_allocations, 5);
}

#[test]
fn a_window_of_buckets_gives_its_count_percentiles_and_tail() {
    let metrics = Metrics::new();
    metrics.record(Opcode::Lookup, Duration::from_micros(500), 0);
    let before = metrics.buckets(Opcode::Lookup);

    for _ in 0..98 {
        metrics.record(Opcode::Lookup, Duration::from_micros(10), 0);
    }
    metrics.record(Opcode::Lookup, Duration::from_micros(200), 0);
    metrics.record(Opcode::Lookup, Duration::from_micros(300), 0);
    metrics.record(Opcode::Create, Duration::from_micros(300), 0);
    let window = metrics.buckets(Opcode::Lookup).since(&before);

    assert_eq!(window.count(), 100, "the window holds only its own samples");
    assert_eq!(
        window.percentile_ns(50),
        1 << 14,
        "10 µs lands in [8.2, 16.4) µs"
    );
    assert_eq!(
        window.percentile_ns(99),
        1 << 18,
        "200 µs lands in [131, 262) µs"
    );
    assert_eq!(
        window.at_least_ns(1 << 17),
        2,
        "the two samples from 131 µs up, not the one before the window"
    );
    assert_eq!(
        metrics
            .buckets(Opcode::Create)
            .since(&Histogram::default())
            .count(),
        1
    );
}

#[test]
fn record_never_allocates() {
    lore_base::allocator::set_allocation_counting(true);
    let metrics = Metrics::new();
    // Warm up: the first record may initialize static data lazily.
    metrics.record(Opcode::Getattr, Duration::from_nanos(1), 0);

    let before = lore_base::allocator::thread_allocation_count();
    metrics.record(Opcode::Getattr, Duration::from_nanos(1), 0);
    let after = lore_base::allocator::thread_allocation_count();

    lore_base::allocator::set_allocation_counting(false);
    assert_eq!(after, before, "Metrics::record must not allocate");
}
