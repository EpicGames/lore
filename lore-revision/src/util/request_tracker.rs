// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Counts the immutable store reads one operation issues, and the most it has in flight at once.
///
/// One tracker is shared by every task of the operation, so the counts cover the whole task tree
/// however wide it fans out. It only observes and never makes a request wait.
#[derive(Default)]
pub struct StoreRequestTracker {
    in_flight: AtomicU64,
    peak_in_flight: AtomicU64,
    requests: AtomicU64,
}

impl StoreRequestTracker {
    /// Counts a request as in flight until the returned guard is dropped.
    pub fn track(&self) -> StoreRequestGuard<'_> {
        let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_in_flight.fetch_max(in_flight, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        StoreRequestGuard { tracker: self }
    }

    /// The most requests that were in flight at once.
    pub fn peak_in_flight(&self) -> u64 {
        self.peak_in_flight.load(Ordering::Relaxed)
    }

    /// The number of requests tracked so far.
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

/// A request counted as in flight by a [`StoreRequestTracker`] until dropped.
pub struct StoreRequestGuard<'a> {
    tracker: &'a StoreRequestTracker,
}

impl Drop for StoreRequestGuard<'_> {
    fn drop(&mut self) {
        self.tracker.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}
