// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::gc_event::GcEventSinkRef;
use crate::immutable_store::ImmutableStore;
use crate::local::immutable_store::ImmutableStoreCreateOptions;

/// Apply the limits of `options` to `store` and run its background incremental evictor and
/// compactor, once per store. Options without limits leave the store as it is. The tasks read the
/// limits at each pass, so a store already collecting enforces the latest limits applied, at the
/// delays it was first given. They hold only a weak reference and end when the last strong
/// reference to `store` drops. These periodic passes run silently; only load-triggered passes
/// report to a command's event sink.
pub fn spawn_gc(store: &Arc<dyn ImmutableStore>, options: &ImmutableStoreCreateOptions) {
    let Some(counters) = store.gc_counters() else {
        return;
    };
    if !options.collects() {
        return;
    }
    let max_capacity = options.max_capacity.unwrap_or(0);
    let max_size = options.max_size.unwrap_or(0);
    counters.set_caps(max_size, max_capacity, false);
    if max_capacity > 0 {
        lore_base::lore_debug!(
            "Store evictor enforcing max capacity of {}",
            evictor_capacity(max_capacity)
        );
        if !counters.evictor_spawned.swap(true, Ordering::AcqRel) {
            drop(lore_base::lore_spawn!(evictor(
                Arc::downgrade(store),
                counters.clone(),
                options.eviction_delay,
                false,
            )));
        }
    }
    if max_size > 0 {
        lore_base::lore_debug!("Store compactor enforcing max size of {max_size}");
        if !counters.compactor_spawned.swap(true, Ordering::AcqRel) {
            drop(lore_base::lore_spawn!(compactor(
                Arc::downgrade(store),
                counters,
                options.compaction_delay,
                false,
            )));
        }
    }
}

/// The fewest fragments the periodic evictor enforces a capacity down to: a lower capacity is raised
/// to it, so that its passes leave small stores alone.
const EVICTOR_FLOOR: usize = 1024 * 1024;

/// The capacity the periodic evictor enforces for `max_capacity`.
fn evictor_capacity(max_capacity: usize) -> usize {
    #[cfg(feature = "test-util")]
    let floor = crate::test_util::evictor_floor(EVICTOR_FLOOR);
    #[cfg(not(feature = "test-util"))]
    let floor = EVICTOR_FLOOR;
    max_capacity.max(floor)
}

/// Evictor task: enforces the max capacity `limits` holds at regular intervals, skipping the
/// passes where it is 0.
pub async fn evictor(
    store: Weak<dyn ImmutableStore>,
    limits: Arc<GcCounters>,
    eviction_delay: Option<Duration>,
    sync_data: bool,
) {
    let eviction_delay = eviction_delay.unwrap_or(Duration::from_secs(10));
    // No startup pass: sleep first so short-lived processes exit before the first scan.
    // Their over-capacity is caught by the load-driven trigger in `GcCounters`.
    loop {
        tokio::time::sleep(eviction_delay).await;
        {
            let Some(real_store) = store.upgrade() else {
                break;
            };
            let max_capacity = limits.max_capacity.load(Ordering::Relaxed);
            if max_capacity == 0 {
                continue;
            }
            #[cfg(feature = "test-util")]
            let _task = crate::test_util::store_task(crate::test_util::StoreTask::Evictor);
            // Background maintenance is silent — not tied to any command's event stream.
            if let Err(err) = real_store
                .evict(evictor_capacity(max_capacity), sync_data, None)
                .await
            {
                lore_base::lore_warn!("Store evictor failed: {err}");
            }
        }
    }
    lore_base::lore_debug!("Store evictor exiting");
}

/// Compactor task: enforces the max size `limits` holds at regular intervals, skipping the steps
/// where it is 0. A step that fails is reported and ends the pass, and the next interval resumes
/// past the groups it covered.
pub async fn compactor(
    store: Weak<dyn ImmutableStore>,
    limits: Arc<GcCounters>,
    compaction_delay: Option<Duration>,
    sync_data: bool,
) {
    let compaction_delay = compaction_delay.unwrap_or(Duration::from_secs(60 * 60 * 24));
    let mut at = if let Some(store) = store.upgrade() {
        store.compact_resume_at().await
    } else {
        None
    };
    loop {
        // No resume point: defer the fresh-start scan to the interval so short-lived
        // processes never pay it. A sentinel resume proceeds immediately and steps
        // without sleeping.
        if at.is_none() {
            tokio::time::sleep(compaction_delay).await;
        }
        {
            let Some(real_store) = store.upgrade() else {
                break;
            };
            let max_size = limits.max_size.load(Ordering::Relaxed) as usize;
            if max_size == 0 {
                at = None;
                continue;
            }
            #[cfg(feature = "test-util")]
            let _task = crate::test_util::store_task(crate::test_util::StoreTask::Compactor);
            // A pass that was stopped rather than finished leaves its resume point behind,
            // so re-reading after the wait picks that group back up instead of restarting.
            if at.is_none() {
                at = real_store.clone().compact_resume_at().await;
            }
            // Background maintenance is silent — not tied to any command's event stream.
            match real_store.compact(max_size, at, sync_data, None).await {
                Ok(Some(step_at)) => {
                    at = Some(step_at);
                    lore_base::lore_debug!(
                        "Store compactor completed a step, now at {}",
                        at.unwrap_or_default()
                    );
                }
                Ok(None) => {
                    at = None;
                    lore_base::lore_debug!("Store compactor finished");
                }
                Err(err) => {
                    at = None;
                    lore_base::lore_warn!("Store compactor failed: {err}");
                }
            }
        }
    }
    lore_base::lore_debug!("Store compactor exiting");
}

/// Run eviction, then compaction, in a single pass, so that compaction reclaims the payloads of the
/// entries eviction removes.
pub async fn gc(
    store: Arc<dyn ImmutableStore>,
    max_size: usize,
    max_capacity: usize,
    sync_data: bool,
    sink: Option<GcEventSinkRef>,
) {
    #[cfg(feature = "test-util")]
    let _task = crate::test_util::store_task(crate::test_util::StoreTask::GcPass);
    if max_capacity > 0 {
        let _ = store
            .clone()
            .evict(max_capacity, sync_data, sink.clone())
            .await;
        lore_base::lore_debug!("Store evictor done");
    }

    if max_size > 0 {
        let mut at = store.clone().compact_resume_at().await;
        loop {
            match store
                .clone()
                .compact(max_size, at, sync_data, sink.clone())
                .await
            {
                Ok(Some(step_at)) => {
                    at = Some(step_at);
                    lore_base::lore_debug!(
                        "Store compactor completed a step, now at {}",
                        at.unwrap_or_default()
                    );
                }
                Ok(None) => {
                    lore_base::lore_debug!("Store compactor finished");
                    break;
                }
                Err(err) => {
                    lore_base::lore_warn!("Store compactor failed: {err}");
                    break;
                }
            }
        }
        lore_base::lore_debug!("Store compactor done");
    }
}

/// Per-store running totals, collected purely as a byproduct of LOADING data from
/// disk — packstore sizes in [`crate::packstore::PackStore::resume`] and bucket
/// fragment counts in `ImmutableStoreBucket::deserialize`. Nothing on the write path
/// touches these; the periodic background tasks remain the authoritative full scan
/// for long-lived processes.
///
/// Because loading only ever *adds*, the totals are a lower bound on the true store
/// size/count: if the loaded subset alone exceeds a cap, the store is definitely over
/// it, so a single GC pass is fired directly (once per process, deduped by the
/// `*_fired` flags; the pass itself is further serialized by the store's
/// eviction/compaction semaphores). If the loaded subset stays under, nothing fires —
/// short-lived commands then do no scanning at all.
///
/// One instance per [`crate::local::immutable_store::LocalImmutableStore`] (never a
/// process-global), so parallel commands on different repositories keep independent
/// counters. Its caps are also the limits the store's periodic evictor and compactor enforce.
pub struct GcCounters {
    total_size: AtomicU64,
    fragment_count: AtomicUsize,
    /// Compaction cap in bytes; 0 disables compaction.
    max_size: AtomicU64,
    /// Eviction cap in fragments; 0 disables eviction.
    max_capacity: AtomicUsize,
    sync_data: AtomicBool,
    compaction_fired: AtomicBool,
    eviction_fired: AtomicBool,
    evictor_spawned: AtomicBool,
    compactor_spawned: AtomicBool,
    /// Back-reference to the owning store, needed to fire a pass. Set once after the
    /// store's `Arc` exists (the store can't exist when its groups are constructed).
    store: OnceLock<Weak<dyn ImmutableStore>>,
}

impl Default for GcCounters {
    fn default() -> Self {
        Self {
            total_size: AtomicU64::new(0),
            fragment_count: AtomicUsize::new(0),
            max_size: AtomicU64::new(0),
            max_capacity: AtomicUsize::new(0),
            sync_data: AtomicBool::new(false),
            compaction_fired: AtomicBool::new(false),
            eviction_fired: AtomicBool::new(false),
            evictor_spawned: AtomicBool::new(false),
            compactor_spawned: AtomicBool::new(false),
            store: OnceLock::new(),
        }
    }
}

impl GcCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the caps + sync flag from the store's create options. A cap of 0 disables its
    /// collection.
    pub fn set_caps(&self, max_size: usize, max_capacity: usize, sync_data: bool) {
        self.max_size.store(max_size as u64, Ordering::Relaxed);
        self.max_capacity.store(max_capacity, Ordering::Relaxed);
        self.sync_data.store(sync_data, Ordering::Relaxed);
    }

    /// Record the store's `Arc` (downgraded) so load hooks can fire a pass on it.
    pub fn set_store(&self, store: &Arc<dyn ImmutableStore>) {
        let _ = self.store.set(Arc::downgrade(store));
    }

    /// Forget everything loaded so far, so a store that is reloading from disk does
    /// not count the same bytes twice.
    ///
    /// Both totals are cumulative `fetch_add`s fed by [`Self::add_loaded_size`] and
    /// [`Self::add_loaded_fragments`], which are reached only from a packstore
    /// `resume()` and a bucket deserialize. A store that discards its in-memory state
    /// and re-reads the directory calls each of those a second time for the same
    /// files, so without this the totals climb on every reload and the compaction and
    /// eviction triggers fire against a number that describes no store.
    ///
    /// Zero is the right value rather than a subtraction because a reload discards
    /// *all* of it: the caller has just invalidated every group, so the correct total
    /// is the one a freshly constructed store has, and each group re-adds its own as
    /// it is next touched.
    ///
    /// The fired-once latches are deliberately left alone. They exist to keep one
    /// process from firing a pass per crossing, and a reload is not a new process.
    pub fn reset_loaded(&self) {
        self.total_size.store(0, Ordering::Relaxed);
        self.fragment_count.store(0, Ordering::Relaxed);
    }

    /// Account for `bytes` of just-loaded packstore data; fire one compaction pass if
    /// the running total crosses `max_size` (once per process).
    pub fn add_loaded_size(self: &Arc<Self>, bytes: u64) {
        let max = self.max_size.load(Ordering::Relaxed);
        if max == 0 || bytes == 0 {
            return;
        }
        let total = self.total_size.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if total > max
            && self
                .compaction_fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            self.clone().fire(true);
        }
    }

    /// Take `bytes` of packstore data back out of the running total, for a packstore
    /// closing the files it had counted, so that loading them again is not counted on
    /// top. The fired-once latch is left alone, as in [`Self::reset_loaded`].
    pub fn remove_loaded_size(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut total = self.total_size.load(Ordering::Relaxed);
        while let Err(current) = self.total_size.compare_exchange_weak(
            total,
            total.saturating_sub(bytes),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            total = current;
        }
    }

    /// The compaction and eviction caps, as `(max_size, max_capacity)`. Test builds only.
    #[cfg(feature = "test-util")]
    pub fn caps(&self) -> (usize, usize) {
        (
            self.max_size.load(Ordering::Relaxed) as usize,
            self.max_capacity.load(Ordering::Relaxed),
        )
    }

    /// The bytes of packstore data counted as loaded. Test builds only.
    #[cfg(feature = "test-util")]
    pub fn loaded_size(&self) -> u64 {
        self.total_size.load(Ordering::Relaxed)
    }

    /// Account for `count` just-deserialized bucket fragments; fire one eviction pass
    /// if the running total crosses `max_capacity` (once per process).
    pub fn add_loaded_fragments(self: &Arc<Self>, count: usize) {
        let max = self.max_capacity.load(Ordering::Relaxed);
        if max == 0 || count == 0 {
            return;
        }
        let total = self.fragment_count.fetch_add(count, Ordering::Relaxed) + count;
        if total > max
            && self
                .eviction_fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            self.clone().fire(false);
        }
    }

    /// Spawn a single GC pass (compaction or eviction) on the owning store. Spawned,
    /// never awaited inline — the caller holds packstore/bucket locks the pass needs,
    /// which are released by the time the spawned task runs. `gc` acquires the
    /// store's eviction/compaction semaphore, so it can't overlap the periodic tasks.
    fn fire(self: Arc<Self>, compaction: bool) {
        let Some(store) = self.store.get().and_then(Weak::upgrade) else {
            return;
        };
        let sync_data = self.sync_data.load(Ordering::Relaxed);
        let (max_size, max_capacity) = if compaction {
            (self.max_size.load(Ordering::Relaxed) as usize, 0)
        } else {
            (0, self.max_capacity.load(Ordering::Relaxed))
        };
        // Bind the sink to the triggering call's context *now*, synchronously on its
        // stack, before spawning — correct even when commands run concurrently in one
        // long-running process.
        let sink = crate::gc_event::current_gc_event_sink();
        drop(lore_base::lore_spawn!(async move {
            gc(store, max_size, max_capacity, sync_data, sink).await;
        }));
    }
}
