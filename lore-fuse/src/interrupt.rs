// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! In-flight request tracking for `INTERRUPT` (spec: "Interrupts"): one fixed slot per worker,
//! each holding the unique of whatever request that worker currently has in flight, so an
//! `INTERRUPT` can find (and wake) the right worker without a lock or a per-request allocation.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::sync::Notify;

/// One worker's current request, if any (`unique == 0` means idle).
struct InFlightSlot {
    unique: AtomicU64,
    /// The unique the last INTERRUPT for this slot targeted. `notify_waiters` wakes only a waiter
    /// already registered, so an INTERRUPT that lands before the request first polls
    /// `interrupted` is read from here instead.
    interrupted: AtomicU64,
    interrupt: Notify,
}

impl Default for InFlightSlot {
    fn default() -> Self {
        Self {
            unique: AtomicU64::new(0),
            interrupted: AtomicU64::new(0),
            interrupt: Notify::new(),
        }
    }
}

/// One fixed slot per worker: [`begin`](InFlightSlots::begin) claims it for a request's
/// `unique`, [`interrupt`](InFlightSlots::interrupt) wakes whichever slot currently holds a
/// matching `unique`.
pub struct InFlightSlots {
    slots: Box<[InFlightSlot]>,
}

impl InFlightSlots {
    pub fn new(workers: usize) -> Self {
        Self {
            slots: (0..workers).map(|_| InFlightSlot::default()).collect(),
        }
    }

    /// Claims `worker`'s slot for `unique`. The returned guard resets the slot back to idle
    /// (unique 0) on drop, including on cancellation or panic.
    pub fn begin(&self, worker: usize, unique: u64) -> SlotGuard<'_> {
        let slot = &self.slots[worker];
        slot.unique.store(unique, Ordering::Release);
        SlotGuard { slot, unique }
    }

    /// Wakes whichever slot currently holds `unique`, if any. Returns whether one was found; the
    /// caller (dispatch's `INTERRUPT` handling) uses this to decide between "no reply" (a slot
    /// was interrupted, its own `select!` will reply `EINTR`) and `EAGAIN` (unknown unique).
    ///
    /// `unique == 0` never matches: it is this module's own sentinel for an idle slot, never a
    /// value the kernel assigns to a real request.
    pub fn interrupt(&self, unique: u64) -> bool {
        if unique == 0 {
            return false;
        }
        for slot in self.slots.iter() {
            if slot.unique.load(Ordering::Acquire) == unique {
                slot.interrupted.store(unique, Ordering::Release);
                slot.interrupt.notify_waiters();
                return true;
            }
        }
        false
    }
}

/// Held for the duration of one in-flight request. [`interrupted`](SlotGuard::interrupted)
/// resolves only while [`InFlightSlots::interrupt`] is targeting this slot's current `unique`;
/// dropping the guard (including via cancellation) resets the slot to idle.
pub struct SlotGuard<'a> {
    slot: &'a InFlightSlot,
    unique: u64,
}

impl SlotGuard<'_> {
    pub async fn interrupted(&self) {
        let notified = self.slot.interrupt.notified();
        let mut notified = std::pin::pin!(notified);
        // Registered first: an INTERRUPT after this wakes it, and one before it stored the
        // unique, which Notify's lock makes visible here. A stale unique of an earlier request
        // never matches this one's.
        notified.as_mut().enable();
        if self.slot.interrupted.load(Ordering::Acquire) == self.unique {
            return;
        }
        notified.await;
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.slot.unique.store(0, Ordering::Release);
    }
}
