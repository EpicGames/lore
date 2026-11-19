// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use lore_base::lore_spawn;
use tokio_util::task::AbortOnDropHandle;

/// Default bounded channel capacity for discovery → execution pipeline.
pub const DEFAULT_WORK_CHANNEL_CAPACITY: usize = 200_000;

/// Ceiling on concurrently-spawned directory discovery tasks during a commit.
///
/// Build-time tunable: start heavily parallel here and reduce by
/// experimentation. Once this many directory tasks are live, a walk enters a
/// deeper directory itself instead of spawning it, which bounds peak memory
/// from discovery run-ahead (each live directory task pins a deserialized
/// name-table block). Because overflow is walked inline rather than blocking on
/// a permit, any value >= 1 is correct — it only trades parallelism for memory.
pub const MAX_CONCURRENT_DIRECTORY_TASKS: usize = 10_000;

/// Ceiling on concurrently-spawned directory tasks during a stage walk.
///
/// Orders of magnitude below the commit ceiling above, and derived from the
/// machine rather than fixed. The stage walk waits on the `lore-io` syscall
/// pool, so concurrency beyond what keeps that pool fed buys no throughput
/// while the per-directory spawn and wake-up handoff keeps costing. Sweeping a
/// full `stage --scan` of a large tree, the curve is shallow above the knee and
/// steep below it: overshooting wastes some throughput and memory,
/// undershooting starves the pool and costs far more. Core count is the
/// deliberate trade-off — a portable value on the safe side of the cliff rather
/// than one host's measured optimum.
///
/// As with the commit ceiling, overflow recurses inline rather than blocking on
/// a permit, so any value >= 1 is correct.
pub fn max_concurrent_stage_directory_tasks() -> usize {
    lore_base::runtime::processor_count()
}

/// Statistics accumulated by the discovery (producer) side, and the lock that keeps the
/// operation's progress events in the order they were read.
/// When the producer finishes and the channel drains, these are the final totals.
#[derive(Default)]
pub struct DiscoveryStats {
    pub total_files: AtomicU64,
    pub total_bytes: AtomicU64,
    pub complete: AtomicBool,
    /// Held while a progress event is read and sent ([`Self::send_progress`]). Its value is
    /// whether the operation's [`ProgressTicker`] may send.
    progress: parking_lot::Mutex<bool>,
}

impl DiscoveryStats {
    /// Reads and sends a progress event of the operation these count, with `send`, while no other
    /// one is: events are sent in the order they were read, so discovery, once reported complete,
    /// stays complete, and a count that only grows never falls.
    pub fn send_progress(&self, send: impl FnOnce()) {
        let _sending = self.progress.lock();
        send();
    }

    /// [`Self::send_progress`] for a tick of a [`ProgressTicker`]: sends nothing once the ticker
    /// stopped.
    fn send_tick(&self, send: impl FnOnce()) {
        let ticking = self.progress.lock();
        if *ticking {
            send();
        }
    }

    /// Lets a ticker send, or stops it, in order with the progress events sent.
    fn set_ticking(&self, ticking: bool) {
        *self.progress.lock() = ticking;
    }
}

/// A task that sends an operation's progress every period until it is dropped. Dropping it waits
/// for a tick being sent, and no tick is sent after the drop returns, on any path that drops it.
/// An operation runs at most one ticker on its stats at a time: dropping one stops the ticks of
/// any other.
pub struct ProgressTicker<S: AsRef<DiscoveryStats> + Send + Sync + 'static> {
    stats: Arc<S>,
    _task: AbortOnDropHandle<()>,
}

impl<S: AsRef<DiscoveryStats> + Send + Sync + 'static> ProgressTicker<S> {
    /// Starts sending a progress event of `stats` with `send` every `period`, the first at once.
    /// `send` sends the event alone and must not take the order lock: the ticker holds it
    /// ([`DiscoveryStats::send_progress`]) around `send`.
    pub fn start(stats: Arc<S>, period: Duration, send: impl Fn(&Arc<S>) + Send + 'static) -> Self {
        AsRef::<DiscoveryStats>::as_ref(&*stats).set_ticking(true);
        let ticking = stats.clone();
        let mut interval = tokio::time::interval(period);
        let task = AbortOnDropHandle::new(lore_spawn!(async move {
            loop {
                interval.tick().await;
                AsRef::<DiscoveryStats>::as_ref(&*ticking).send_tick(|| send(&ticking));
            }
        }));
        Self { stats, _task: task }
    }
}

impl<S: AsRef<DiscoveryStats> + Send + Sync + 'static> Drop for ProgressTicker<S> {
    fn drop(&mut self) {
        AsRef::<DiscoveryStats>::as_ref(&*self.stats).set_ticking(false);
    }
}

impl AsRef<DiscoveryStats> for DiscoveryStats {
    fn as_ref(&self) -> &DiscoveryStats {
        self
    }
}
