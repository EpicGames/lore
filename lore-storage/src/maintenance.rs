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
) {
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
                    break;
                }
            }
        }
        lore_base::lore_debug!("Store compactor done");
    }

    if max_capacity > 0 {
        let _ = store.evict(max_capacity, sync_data, sink).await;
        lore_base::lore_debug!("Store evictor done");
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::local::immutable_store::ImmutableStoreSettings;
    use crate::local::immutable_store::LocalImmutableStore;
    use crate::test_util::TempDir;

    fn generate_tempdir() -> TempDir {
        TempDir::new("lore-storage-maintenance-test-")
    }

    async fn create_test_store(path: Option<PathBuf>) -> Arc<dyn ImmutableStore> {
        // These GC-mechanics tests put non-durable fragments and then expect
        // eviction/compaction to act on them, so they run with local-fragment
        // protection disabled (server behavior). Protection itself is covered by
        // the `evict_bucket_*` unit tests.
        LocalImmutableStore::new(
            path,
            ImmutableStoreSettings {
                protect_local_fragment: false,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn gc_compaction_reduces_fragment_count() {
        let dir = generate_tempdir();
        let store = create_test_store(Some(dir.to_path_buf())).await;

        let partition = crate::Partition::from([0x01; 16]);

        for i in 0u8..10 {
            let data = vec![i; 1024];
            let hash = crate::hash_slice(&data);
            let address = crate::Address {
                hash,
                context: crate::Context::from([i; 16]),
            };
            let frag = crate::Fragment {
                flags: 0,
                size_payload: data.len() as u32,
                size_content: data.len() as u64,
            };
            store
                .clone()
                .put(
                    partition,
                    address,
                    frag,
                    Some(bytes::Bytes::from(data)),
                    false,
                )
                .await
                .unwrap();
        }

        store.clone().flush(true).await.unwrap();

        let count_before = store.clone().fragment_count().await;

        // Run gc with a very small max_size to trigger compaction, and small capacity
        // for eviction (1 byte to force eviction).
        gc(store.clone(), 1, 1, false, None).await;

        let count_after = store.clone().fragment_count().await;

        assert!(
            count_after.unwrap_or(0) < count_before.unwrap_or(0),
            "gc should reduce fragment count: before={count_before:?}, after={count_after:?}"
        );
    }

    #[tokio::test]
    async fn gc_skips_compaction_when_max_size_zero() {
        let store = create_test_store(None).await;
        gc(store, 0, 0, false, None).await;
    }

    /// With local-fragment protection on, an aggressive size/capacity GC pass must
    /// not evict or orphan a non-durable fragment, even when it is the only data and
    /// the caps force the per-bucket target to zero (regression: an empty durable
    /// candidate set left the size-eviction cutoff at `u64::MAX`, orphaning everything).
    #[tokio::test]
    async fn gc_protects_non_durable_fragment() {
        let dir = generate_tempdir();
        let store = LocalImmutableStore::new(
            Some(dir.to_path_buf()),
            ImmutableStoreSettings {
                protect_local_fragment: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let partition = crate::Partition::from([0x09; 16]);
        let data = vec![0xab_u8; 4096];
        let hash = crate::hash_slice(&data);
        let address = crate::Address {
            hash,
            context: crate::Context::from([1; 16]),
        };
        let frag = crate::Fragment {
            flags: 0, // non-durable: no PayloadStoredDurable bit
            size_payload: data.len() as u32,
            size_content: data.len() as u64,
        };
        store
            .clone()
            .put(
                partition,
                address,
                frag,
                Some(bytes::Bytes::from(data.clone())),
                false,
            )
            .await
            .unwrap();
        store.clone().flush(true).await.unwrap();

        // Tiny caps drive both eviction and compaction with a zero per-bucket target.
        gc(store.clone(), 1, 1, false, None).await;

        let (_frag, payload) = store
            .clone()
            .get(
                partition,
                address,
                crate::store_types::StoreMatch::MatchHash,
            )
            .await
            .expect("non-durable fragment must survive GC");
        assert_eq!(payload.len(), data.len());
    }

    #[tokio::test]
    async fn gc_runs_eviction_only() {
        let dir = generate_tempdir();
        let store = create_test_store(Some(dir.to_path_buf())).await;

        let partition = crate::Partition::from([0x02; 16]);

        for i in 0u8..5 {
            let data = vec![i; 2048];
            let hash = crate::hash_slice(&data);
            let address = crate::Address {
                hash,
                context: crate::Context::from([i; 16]),
            };
            let frag = crate::Fragment {
                flags: 0,
                size_payload: data.len() as u32,
                size_content: data.len() as u64,
            };
            store
                .clone()
                .put(
                    partition,
                    address,
                    frag,
                    Some(bytes::Bytes::from(data)),
                    false,
                )
                .await
                .unwrap();
        }
        store.clone().flush(true).await.unwrap();

        // max_size=0 skips compaction, max_capacity=1 triggers eviction
        gc(store.clone(), 0, 1, false, None).await;

        let count = store.clone().fragment_count().await.unwrap_or(0);
        assert!(
            count < 5,
            "eviction should have removed some fragments: count={count}"
        );
    }

    #[tokio::test]
    async fn evictor_exits_when_store_dropped() {
        let store = create_test_store(None).await;
        let weak = Arc::downgrade(&store);
        drop(store);

        evictor(weak, 1024 * 1024, Some(Duration::from_millis(10)), false).await;
    }

    #[tokio::test]
    async fn compactor_exits_when_store_dropped() {
        let store = create_test_store(None).await;
        let weak = Arc::downgrade(&store);
        drop(store);

        compactor(weak, 1024, Some(Duration::from_millis(10)), false).await;
    }

    #[tokio::test]
    async fn evict_bucket() {
        use tokio::sync::RwLock;

        use crate::local::immutable_store::ImmutableData;
        use crate::local::immutable_store::ImmutableStoreBucket;
        use crate::local::immutable_store::ImmutableStoreEntry;

        let mut bucket = ImmutableStoreBucket::default();

        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<crate::Address>(),
            partition: rand::random::<crate::Partition>(),
            data: ImmutableData {
                flags: 0,
                size_payload: 100,
                size_content: 100,
                pack_offset: 0,
                pack_file: 0,
                last_access: 100,
            },
        });
        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<crate::Address>(),
            partition: rand::random::<crate::Partition>(),
            data: ImmutableData {
                flags: 0,
                size_payload: 101,
                size_content: 101,
                pack_offset: 0,
                pack_file: 0,
                last_access: 101,
            },
        });
        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<crate::Address>(),
            partition: rand::random::<crate::Partition>(),
            data: ImmutableData {
                flags: 0,
                size_payload: 99,
                size_content: 99,
                pack_offset: 0,
                pack_file: 0,
                last_access: 99,
            },
        });
        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<crate::Address>(),
            partition: rand::random::<crate::Partition>(),
            data: ImmutableData {
                flags: 0,
                size_payload: 500,
                size_content: 500,
                pack_offset: 0,
                pack_file: 0,
                last_access: 500,
            },
        });
        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<crate::Address>(),
            partition: rand::random::<crate::Partition>(),
            data: ImmutableData {
                flags: 0,
                size_payload: 100,
                size_content: 100,
                pack_offset: 0,
                pack_file: 0,
                last_access: 100,
            },
        });
        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<crate::Address>(),
            partition: rand::random::<crate::Partition>(),
            data: ImmutableData {
                flags: 0,
                size_payload: 1000,
                size_content: 1000,
                pack_offset: 0,
                pack_file: 0,
                last_access: 1000,
            },
        });

        // Sorting not important for eviction test, it can be invalid order
        bucket.sorted_index.push(1);
        bucket.sorted_index.push(4);
        bucket.sorted_index.push(0);
        bucket.sorted_index.push(3);
        bucket.sorted_index.push(5);
        bucket.sorted_index.push(2);

        let bucket = Arc::new(RwLock::new(bucket));
        let dirty = std::sync::atomic::AtomicBool::new(false);

        let evict_count =
            LocalImmutableStore::evict_oldest_bucket(bucket.clone(), &dirty, 3, false).await;

        assert_eq!(evict_count, 3);

        let bucket = bucket.read().await;
        for entry in bucket.entry.iter() {
            assert!(entry.data.last_access > 100);
            // We marked the entries to be the same last access as size, make sure data was preserved
            assert_eq!(entry.data.last_access, entry.data.size_payload as u64);
        }
    }

    /// With local-fragment protection enabled, non-durable fragments must never be
    /// evicted and must not count toward the capacity target, even when the bucket
    /// is far over capacity and the oldest fragments are the non-durable ones.
    #[tokio::test]
    async fn evict_bucket_protects_non_durable() {
        use tokio::sync::RwLock;

        use crate::FragmentFlags;
        use crate::local::immutable_store::ImmutableData;
        use crate::local::immutable_store::ImmutableStoreBucket;
        use crate::local::immutable_store::ImmutableStoreEntry;

        let durable = FragmentFlags::PayloadStoredDurable.bits();

        // (last_access, durable?) — the two oldest fragments are non-durable and
        // would be the first to go under a pure-LRU policy.
        let fixtures = [
            (10, false),
            (20, false),
            (100, true),
            (200, true),
            (300, true),
            (400, true),
        ];

        let mut bucket = ImmutableStoreBucket::default();
        for (i, (last_access, is_durable)) in fixtures.iter().enumerate() {
            bucket.entry.push(ImmutableStoreEntry {
                address: rand::random::<crate::Address>(),
                partition: rand::random::<crate::Partition>(),
                data: ImmutableData {
                    flags: if *is_durable { durable } else { 0 },
                    size_payload: 100,
                    size_content: 100,
                    pack_offset: 0,
                    pack_file: 0,
                    last_access: *last_access,
                },
            });
            bucket.sorted_index.push(i as u32);
        }

        let bucket = Arc::new(RwLock::new(bucket));
        let dirty = std::sync::atomic::AtomicBool::new(false);

        // target_capacity = 2 over 4 durable fragments → evict the 2 oldest durable
        // (last_access 100 and 200). The 2 non-durable fragments are protected and
        // do not count toward the target.
        let evict_count =
            LocalImmutableStore::evict_oldest_bucket(bucket.clone(), &dirty, 2, true).await;

        assert_eq!(evict_count, 2);

        let bucket = bucket.read().await;
        // Both non-durable fragments survived despite being the oldest.
        assert_eq!(
            bucket
                .entry
                .iter()
                .filter(|e| e.data.flags & durable == 0)
                .count(),
            2
        );
        // Only the two newest durable fragments remain.
        let mut durable_access: Vec<u64> = bucket
            .entry
            .iter()
            .filter(|e| e.data.flags & durable != 0)
            .map(|e| e.data.last_access)
            .collect();
        durable_access.sort_unstable();
        assert_eq!(durable_access, vec![300, 400]);
    }

    /// With protection disabled (server behavior) eviction is pure LRU and the
    /// durable flag is ignored — the oldest fragments are evicted regardless.
    #[tokio::test]
    async fn evict_bucket_unprotected_ignores_durable_flag() {
        use tokio::sync::RwLock;

        use crate::FragmentFlags;
        use crate::local::immutable_store::ImmutableData;
        use crate::local::immutable_store::ImmutableStoreBucket;
        use crate::local::immutable_store::ImmutableStoreEntry;

        let durable = FragmentFlags::PayloadStoredDurable.bits();
        let fixtures = [(10, false), (20, false), (100, true), (200, true)];

        let mut bucket = ImmutableStoreBucket::default();
        for (i, (last_access, is_durable)) in fixtures.iter().enumerate() {
            bucket.entry.push(ImmutableStoreEntry {
                address: rand::random::<crate::Address>(),
                partition: rand::random::<crate::Partition>(),
                data: ImmutableData {
                    flags: if *is_durable { durable } else { 0 },
                    size_payload: 100,
                    size_content: 100,
                    pack_offset: 0,
                    pack_file: 0,
                    last_access: *last_access,
                },
            });
            bucket.sorted_index.push(i as u32);
        }

        let bucket = Arc::new(RwLock::new(bucket));
        let dirty = std::sync::atomic::AtomicBool::new(false);

        // target_capacity = 2 over all 4 fragments → evict the 2 oldest, which are
        // the non-durable ones, since protection is off.
        let evict_count =
            LocalImmutableStore::evict_oldest_bucket(bucket.clone(), &dirty, 2, false).await;

        assert_eq!(evict_count, 2);

        let bucket = bucket.read().await;
        let mut remaining: Vec<u64> = bucket.entry.iter().map(|e| e.data.last_access).collect();
        remaining.sort_unstable();
        assert_eq!(remaining, vec![100, 200]);
    }

    #[tokio::test]
    async fn compact_bucket() {
        use std::sync::OnceLock;

        use bytes::Bytes;
        use tokio::task::JoinSet;

        use crate::local::immutable_store::BUCKET_COUNT;
        use crate::local::immutable_store::ImmutableData;
        use crate::local::immutable_store::ImmutableStoreEntry;
        use crate::local::immutable_store::ImmutableStoreGroup;
        use crate::packstore::PackStore;

        let tempdir = generate_tempdir();
        let group = Arc::new(ImmutableStoreGroup {
            bucket: [const { OnceLock::new() }; BUCKET_COUNT],
            dirty: std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false)),
            bucket_count: std::sync::atomic::AtomicUsize::new(
                crate::local::fan_out::FAN_OUT_LEVEL_MAX,
            ),
            serialize_version: std::sync::atomic::AtomicU32::new(
                crate::local::immutable_store::ImmutableStoreVersion::LazyFanOut as u32,
            ),
            fan_out_threshold: crate::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
            committed_level: std::sync::atomic::AtomicUsize::new(
                crate::local::fan_out::FAN_OUT_LEVEL_MAX,
            ),
            packstore: PackStore::new(Some(tempdir.to_path_buf()), 1, None),
            flush: tokio::sync::Mutex::new(JoinSet::new()),
        });

        // Buffer lengths are primes to ensure test actually verify the correct thing
        let first_buffer = Bytes::copy_from_slice(&[0, 1, 2, 3, 4, 5, 6]);
        let second_buffer = Bytes::copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let third_buffer = Bytes::copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        let first_hash = crate::Hash::hash_buffer(first_buffer.as_ref());
        let second_hash = crate::Hash::hash_buffer(second_buffer.as_ref());
        let third_hash = crate::Hash::hash_buffer(third_buffer.as_ref());

        let mut hashed = [
            (first_hash, first_buffer),
            (second_hash, second_buffer),
            (third_hash, third_buffer),
        ];
        hashed.sort_by_key(|a| a.0);

        let (smaller_hash, smaller_buffer) = hashed[0].clone();
        let (mid_hash, mid_buffer) = hashed[1].clone();
        let (greater_hash, greater_buffer) = hashed[2].clone();

        {
            let smaller_packdata = group
                .packstore
                .store(smaller_buffer.clone())
                .await
                .expect("Failed to store packdata");

            let mid_packdata = group
                .packstore
                .store(mid_buffer.clone())
                .await
                .expect("Failed to store packdata");

            let greater_packdata = group
                .packstore
                .store(greater_buffer.clone())
                .await
                .expect("Failed to store packdata");

            let mut bucket = group.bucket(0).write().await;

            let mut mid_context: crate::Context = rand::random();
            let mut mid_repository: crate::Partition = rand::random();

            // Ensure some order
            mid_context.data_mut()[0] = 1;
            mid_repository.data_mut()[0] = 1;

            let mut smaller_context = mid_context;
            smaller_context.data_mut()[0] = 0;

            let mut smaller_repository = mid_repository;
            smaller_repository.data_mut()[0] = 0;

            let mut greater_context = mid_context;
            greater_context.data_mut()[0] = 2;

            let mut greater_repository = mid_repository;
            greater_repository.data_mut()[0] = 2;

            // index 0, sort order 2, Deduplicated, should be compacted to same packfile as previous
            bucket.entry.push(ImmutableStoreEntry {
                address: crate::Address {
                    hash: mid_hash,
                    context: mid_context,
                },
                partition: greater_repository,
                data: ImmutableData {
                    flags: 0,
                    size_payload: mid_buffer.len() as u32,
                    size_content: mid_buffer.len() as u64,
                    pack_offset: mid_packdata.offset,
                    pack_file: mid_packdata.id,
                    last_access: 0,
                },
            });

            // index 1, sort order 4, Should be compacted to a new packfile
            bucket.entry.push(ImmutableStoreEntry {
                address: crate::Address {
                    hash: greater_hash,
                    context: greater_context,
                },
                partition: greater_repository,
                data: ImmutableData {
                    flags: 0,
                    size_payload: greater_buffer.len() as u32,
                    size_content: greater_buffer.len() as u64,
                    pack_offset: greater_packdata.offset,
                    pack_file: greater_packdata.id,
                    last_access: 0,
                },
            });

            // index 2, sort order 0, This should remain due to other packfile
            bucket.entry.push(ImmutableStoreEntry {
                address: crate::Address {
                    hash: smaller_hash,
                    context: mid_context,
                },
                partition: mid_repository,
                data: ImmutableData {
                    flags: 0,
                    size_payload: smaller_buffer.len() as u32,
                    size_content: smaller_buffer.len() as u64,
                    pack_offset: smaller_packdata.offset,
                    pack_file: smaller_packdata.id + 1,
                    last_access: 0,
                },
            });

            // index 3, sort order 1, This should be compacted to new packfile
            bucket.entry.push(ImmutableStoreEntry {
                address: crate::Address {
                    hash: mid_hash,
                    context: smaller_context,
                },
                partition: smaller_repository,
                data: ImmutableData {
                    flags: 0,
                    size_payload: mid_buffer.len() as u32,
                    size_content: mid_buffer.len() as u64,
                    pack_offset: mid_packdata.offset,
                    pack_file: mid_packdata.id,
                    last_access: 0,
                },
            });

            // index 4, sort order 3, This should remain, different packfile
            bucket.entry.push(ImmutableStoreEntry {
                address: crate::Address {
                    hash: greater_hash,
                    context: mid_context,
                },
                partition: mid_repository,
                data: ImmutableData {
                    flags: 0,
                    size_payload: greater_buffer.len() as u32,
                    size_content: greater_buffer.len() as u64,
                    pack_offset: greater_packdata.offset,
                    pack_file: greater_packdata.id + 2,
                    last_access: 0,
                },
            });

            bucket.sorted_index.push(2);
            bucket.sorted_index.push(3);
            bucket.sorted_index.push(0);
            bucket.sorted_index.push(4);
            bucket.sorted_index.push(1);
        }

        group
            .packstore
            .stop_write(1)
            .await
            .expect("Failed to stop write");

        let compacted_size =
            LocalImmutableStore::compact_bucket_packfile_impl(&group, 0, 0, 1, false).await;

        // Two instances of the data should have been rewritten to new packfiles
        assert_eq!(compacted_size, mid_buffer.len() + greater_buffer.len());
    }
}
