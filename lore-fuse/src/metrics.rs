// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Lock-free per-opcode metrics: a log2-bucketed latency histogram plus an allocation
//! high-water mark, so recording a sample never takes a lock and never allocates (spec:
//! "Allocation discipline").

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::abi::OPCODE_COUNT;
use crate::abi::Opcode;

const BUCKETS: usize = 32;

/// One opcode's running latency histogram and allocation high-water mark.
#[derive(Default)]
pub struct OpcodeMetrics {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    log2_buckets: [AtomicU64; BUCKETS],
    max_allocations: AtomicU64,
}

impl OpcodeMetrics {
    fn record(&self, elapsed: Duration, allocations: u64) {
        let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;

        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        self.log2_buckets[bucket_for(ns)].fetch_add(1, Ordering::Relaxed);
        self.max_allocations
            .fetch_max(allocations, Ordering::Relaxed);
    }

    fn snapshot(&self) -> OpcodeSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let max_ns = self.max_ns.load(Ordering::Relaxed);
        let max_allocations = self.max_allocations.load(Ordering::Relaxed);

        let buckets: [u64; BUCKETS] =
            std::array::from_fn(|i| self.log2_buckets[i].load(Ordering::Relaxed));

        OpcodeSnapshot {
            count,
            p50_ns: percentile_ns(&buckets, count, 50),
            p99_ns: percentile_ns(&buckets, count, 99),
            max_ns,
            max_allocations,
        }
    }
}

/// The bucket index for a latency sample: bucket `i` covers `[2^i, 2^(i+1))` nanoseconds,
/// bucket 0 covers `0`.
fn bucket_for(ns: u64) -> usize {
    if ns == 0 {
        0
    } else {
        (64 - ns.leading_zeros()) as usize - 1
    }
    .min(BUCKETS - 1)
}

/// The upper edge of `bucket`, used as that bucket's representative latency when computing a
/// percentile.
fn bucket_ceiling_ns(bucket: usize) -> u64 {
    1u64 << (bucket + 1).min(63)
}

/// Estimates the `percentile`-th percentile latency from a log2 histogram, by walking buckets
/// in order until the cumulative count reaches the target rank.
fn percentile_ns(buckets: &[u64; BUCKETS], count: u64, percentile: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    // Ceiling rank in [1, count], so p99 of a single sample still lands on that sample.
    let target = (count * percentile).div_ceil(100).max(1);
    let mut cumulative = 0u64;
    for (bucket, &n) in buckets.iter().enumerate() {
        cumulative += n;
        if cumulative >= target {
            return bucket_ceiling_ns(bucket);
        }
    }
    bucket_ceiling_ns(BUCKETS - 1)
}

/// A point-in-time read of one opcode's metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpcodeSnapshot {
    pub count: u64,
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub max_allocations: u64,
}

/// Per-opcode metrics for the whole session, indexed densely by [`Opcode`] value.
pub struct Metrics {
    per_opcode: [OpcodeMetrics; OPCODE_COUNT],
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            per_opcode: std::array::from_fn(|_| OpcodeMetrics::default()),
        }
    }

    /// Records one request's latency and allocation count against `opcode`. Never allocates:
    /// every counter is a fixed-size atomic, touched only with `Relaxed` stores.
    pub fn record(&self, opcode: Opcode, elapsed: Duration, allocations: u64) {
        self.per_opcode[opcode as u32 as usize].record(elapsed, allocations);
    }

    pub fn snapshot(&self, opcode: Opcode) -> OpcodeSnapshot {
        self.per_opcode[opcode as u32 as usize].snapshot()
    }

    /// `opcode`'s latency histogram as it stands, which a later read less this one gives the
    /// samples of the window between ([`Histogram::since`]).
    pub fn buckets(&self, opcode: Opcode) -> Histogram {
        Histogram {
            buckets: std::array::from_fn(|i| {
                self.per_opcode[opcode as u32 as usize].log2_buckets[i].load(Ordering::Relaxed)
            }),
        }
    }
}

/// A log2 latency histogram read from [`Metrics`]: bucket `i` counts samples in
/// `[2^i, 2^(i+1))` nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Histogram {
    buckets: [u64; BUCKETS],
}

impl Histogram {
    /// The samples recorded after `earlier`, a read of the same opcode's histogram.
    pub fn since(&self, earlier: &Histogram) -> Histogram {
        Histogram {
            buckets: std::array::from_fn(|i| self.buckets[i].saturating_sub(earlier.buckets[i])),
        }
    }

    /// This histogram plus `other`, as one opcode's.
    pub fn plus(&self, other: &Histogram) -> Histogram {
        Histogram {
            buckets: std::array::from_fn(|i| self.buckets[i] + other.buckets[i]),
        }
    }

    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// The upper edge of the bucket holding the `percentile`-th percentile sample; 0 when empty.
    pub fn percentile_ns(&self, percentile: u64) -> u64 {
        percentile_ns(&self.buckets, self.count(), percentile)
    }

    /// The samples in the buckets that start at `ns` or above, exact where `ns` is a power of
    /// two.
    pub fn at_least_ns(&self, ns: u64) -> u64 {
        self.buckets
            .iter()
            .enumerate()
            .filter(|&(bucket, _)| bucket > 0 && 1u64 << bucket >= ns)
            .map(|(_, &n)| n)
            .sum()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
