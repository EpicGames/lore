// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Probes a test reads and arms inside the library, built with the `test-util` feature.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use tokio::sync::Notify;

use crate::fs::filesystem_provider::DirectoryEntry;
use crate::fs::filesystem_provider::FsError;

/// The tasks one walk spawns: counts those live, for a test to read the peak, holds each one
/// before its work until released, so a test can see the tasks pile up, fails a listing where a
/// test arms it, and sizes the walk's task budget. A walk holds one in a `static`, shared by the
/// whole process, so the tests using one run alone in their binary, one after another.
pub struct TaskProbe {
    live: AtomicUsize,
    peak: AtomicUsize,
    spawned: AtomicUsize,
    held: AtomicBool,
    released: Notify,
    fail_after: AtomicUsize,
    budget: AtomicUsize,
}

impl TaskProbe {
    /// A probe with nothing counted, held or armed.
    pub(crate) const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            spawned: AtomicUsize::new(0),
            held: AtomicBool::new(false),
            released: Notify::const_new(),
            fail_after: AtomicUsize::new(usize::MAX),
            budget: AtomicUsize::new(crate::MAX_CONCURRENT_TREE_TASKS),
        }
    }

    /// Zeroes the counts, disarms the failure, restores the budget and releases a hold.
    pub fn reset(&self) {
        self.live.store(0, Ordering::Relaxed);
        self.peak.store(0, Ordering::Relaxed);
        self.spawned.store(0, Ordering::Release);
        self.fail_after.store(usize::MAX, Ordering::Release);
        self.budget
            .store(crate::MAX_CONCURRENT_TREE_TASKS, Ordering::Relaxed);
        self.release();
    }

    /// Sizes the budget the walk's next run spawns under, so a test can exceed it with a small
    /// tree. [`crate::MAX_CONCURRENT_TREE_TASKS`] until set.
    pub fn set_budget(&self, permits: usize) {
        self.budget.store(permits, Ordering::Relaxed);
    }

    /// The permits the walk's budget holds.
    pub(crate) fn budget(&self) -> usize {
        self.budget.load(Ordering::Relaxed)
    }

    /// Fails the first entry the walk lists once `spawned` tasks have been spawned, as an entry
    /// the listing cannot read fails.
    pub fn fail_entry_after(&self, spawned: usize) {
        self.fail_after.store(spawned, Ordering::Release);
    }

    /// `entry`, or the failure [`Self::fail_entry_after`] armed in its place, which it answers
    /// once.
    pub(crate) fn entry(
        &self,
        entry: Result<DirectoryEntry, FsError>,
    ) -> Result<DirectoryEntry, FsError> {
        let armed = self.fail_after.load(Ordering::Acquire);
        if self.spawned.load(Ordering::Acquire) >= armed
            && self
                .fail_after
                .compare_exchange(armed, usize::MAX, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return Err(FsError::internal("The listing failed"));
        }
        entry
    }

    /// Stops each task about to do its work until [`Self::release`] is called.
    pub fn hold(&self) {
        self.held.store(true, Ordering::Release);
    }

    /// Lets the tasks [`Self::hold`] stopped go on.
    pub fn release(&self) {
        self.held.store(false, Ordering::Release);
        self.released.notify_waiters();
    }

    /// Waits while a hold is on. The wakeup is registered before the flag is read, so a release
    /// between the two is not missed.
    pub(crate) async fn wait_if_held(&self) {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !self.held.load(Ordering::Acquire) {
                return;
            }
            released.await;
        }
    }

    /// The tasks live now.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// The most tasks live at once since the last reset.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// The tasks spawned since the last reset.
    pub fn spawned(&self) -> usize {
        self.spawned.load(Ordering::Acquire)
    }

    /// Counts a task as spawned, and as live until the guard is dropped.
    pub(crate) fn enter(&self) -> Live<'_> {
        self.spawned.fetch_add(1, Ordering::AcqRel);
        let live = self.live.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(live, Ordering::AcqRel);
        Live(self)
    }
}

/// A task [`TaskProbe::enter`] counts as live until this is dropped.
pub(crate) struct Live<'a>(&'a TaskProbe);

impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}
