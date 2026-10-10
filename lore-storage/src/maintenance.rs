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

/// Spawn the background incremental evictor and compactor for `store`, configured
/// by `options`. The tasks hold only a weak reference and self-cancel when the last
/// strong reference to `store` drops at command completion. These periodic background
/// passes run silently; only load-triggered passes report to a command's event sink.
pub fn spawn_gc(store: &Arc<dyn ImmutableStore>, options: &ImmutableStoreCreateOptions) {
    if let Some(max_capacity) = options.max_capacity
        && max_capacity > 0
    {
        let weak = Arc::downgrade(store);
        let eviction_delay = options.eviction_delay;
        drop(lore_base::lore_spawn!(async move {
            evictor(weak, max_capacity, eviction_delay, false).await;
        }));
    }
    if let Some(max_size) = options.max_size
        && max_size > 0
    {
        let weak = Arc::downgrade(store);
        let compaction_delay = options.compaction_delay;
        drop(lore_base::lore_spawn!(async move {
            compactor(weak, max_size, compaction_delay, false).await;
        }));
    }
}

/// Evictor task: enforces max capacity at regular intervals.
pub async fn evictor(
    store: Weak<dyn ImmutableStore>,
    max_capacity: usize,
    eviction_delay: Option<Duration>,
    sync_data: bool,
) {
    use std::cmp::max;

    let max_capacity = max(max_capacity, 1024 * 1024);
    let eviction_delay = eviction_delay.unwrap_or(Duration::from_secs(10));
    lore_base::lore_debug!("Store evictor enforcing max capacity of {max_capacity}");
    // No startup pass: sleep first so short-lived processes exit before the first scan.
    // Their over-capacity is caught by the load-driven trigger in `GcCounters`.
    loop {
        tokio::time::sleep(eviction_delay).await;
        {
            let Some(real_store) = store.upgrade() else {
                break;
            };
            // Background maintenance is silent — not tied to any command's event stream.
            if let Err(err) = real_store.evict(max_capacity, sync_data, None).await {
                lore_base::lore_warn!("Store evictor failed: {err}");
            }
        }
    }
    lore_base::lore_debug!("Store evictor exiting");
}

/// Compactor task: enforces max size at regular intervals.
pub async fn compactor(
    store: Weak<dyn ImmutableStore>,
    max_size: usize,
    compaction_delay: Option<Duration>,
    sync_data: bool,
) {
    let compaction_delay = compaction_delay.unwrap_or(Duration::from_secs(60 * 60 * 24));
    lore_base::lore_debug!("Store compactor enforcing max size of {max_size}");
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
                    lore_base::lore_warn!("Store compactor failed: {err}");
                    break;
                }
            }
        }
    }
    lore_base::lore_debug!("Store compactor exiting");
}

/// Run compaction and eviction in a single pass.
pub async fn gc(
    store: Arc<dyn ImmutableStore>,
    max_size: usize,
    max_capacity: usize,
    sync_data: bool,
    sink: Option<GcEventSinkRef>,
    grace_period: Option<Duration>,
) -> Result<(), crate::gc::GcError> {
    let mut at = store.clone().compact_resume_at().await;

    if max_size > 0 {
        loop {
            let store = store.clone();
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
                    return Err(crate::gc::GcError::CompactionFailed(err.to_string()));
                }
            }
        }
        lore_base::lore_debug!("Store compactor done");
    }

    if max_capacity > 0 {
        // Enforce staging leases and grace period timestamp checking for atomic rollbacks
        let _transaction = crate::gc_lease::acquire_exclusive_sweep()
            .map_err(|e| crate::gc::GcError::LeaseDenied(e.to_string()))?;
            
        store.evict_with_grace(max_capacity, sync_data, sink, grace_period).await
            .map_err(|e| crate::gc::GcError::EvictionFailed(e.to_string()))?;
        lore_base::lore_debug!("Store evictor done");
    }
    
    Ok(())
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
/// counters.
pub struct GcCounters {
    total_size: AtomicU64,
    fragment_count: AtomicUsize,
    /// Compaction cap in bytes; 0 disables the compaction trigger (read-only / `--no-gc`).
    max_size: AtomicU64,
    /// Eviction cap in fragments; 0 disables the eviction trigger.
    max_capacity: AtomicUsize,
    sync_data: AtomicBool,
    compaction_fired: AtomicBool,
    eviction_fired: AtomicBool,
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
            store: OnceLock::new(),
        }
    }
}

impl GcCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the caps + sync flag from the store's create options. Caps of 0 leave the
    /// corresponding trigger disabled (read-only / `--no-gc` opens pass 0 here).
    pub fn set_caps(&self, max_size: usize, max_capacity: usize, sync_data: bool) {
        self.max_size.store(max_size as u64, Ordering::Relaxed);
        self.max_capacity.store(max_capacity, Ordering::Relaxed);
        self.sync_data.store(sync_data, Ordering::Relaxed);
    }

    /// Record the store's `Arc` (downgraded) so load hooks can fire a pass on it.
    pub fn set_store(&self, store: &Arc<dyn ImmutableStore>) {
        let _ = self.store.set(Arc::downgrade(store));
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
