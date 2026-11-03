// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Counts one operation's immutable store requests by kind and records the peak number of requests
/// in flight for each kind.
///
/// One tracker is shared by every task of an operation, so its counts cover the whole task tree
/// however wide it fans out. It only observes and never makes a request wait.
#[derive(Default)]
pub struct StoreRequestTracker {
    /// The per-address fragment lookups.
    lookups: InFlightCounter,
    /// The reads of metadata blobs referenced by changed file metadata blocks.
    metadata_reads: InFlightCounter,
}

impl StoreRequestTracker {
    /// Counts a fragment lookup as in flight until the returned guard is dropped.
    pub fn track_lookup(&self) -> StoreRequestGuard<'_> {
        self.lookups.track()
    }

    /// The most fragment lookups that were in flight at once.
    pub fn peak_lookups_in_flight(&self) -> u64 {
        self.lookups.peak()
    }

    /// The number of fragment lookups tracked so far.
    pub fn lookups(&self) -> u64 {
        self.lookups.total()
    }

    /// Counts a metadata blob read as in flight until the returned guard is dropped.
    pub fn track_metadata_read(&self) -> StoreRequestGuard<'_> {
        self.metadata_reads.track()
    }

    /// The most metadata blob reads that were in flight at once.
    pub fn peak_metadata_reads_in_flight(&self) -> u64 {
        self.metadata_reads.peak()
    }

    /// The number of metadata blob reads tracked so far.
    pub fn metadata_reads(&self) -> u64 {
        self.metadata_reads.total()
    }
}

#[derive(Default)]
struct InFlightCounter {
    in_flight: AtomicU64,
    peak: AtomicU64,
    total: AtomicU64,
}

impl InFlightCounter {
    fn track(&self) -> StoreRequestGuard<'_> {
        let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(in_flight, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        StoreRequestGuard { counter: self }
    }

    fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

/// A request counted as in flight by a [`StoreRequestTracker`] until dropped.
pub struct StoreRequestGuard<'a> {
    counter: &'a InFlightCounter,
}

impl Drop for StoreRequestGuard<'_> {
    fn drop(&mut self) {
        self.counter.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}
