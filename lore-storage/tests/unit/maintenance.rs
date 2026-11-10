// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_storage::FlushOptions;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::maintenance::*;

use crate::store_fixture::payloads_in_one_group;
use crate::store_fixture::put_payload;

fn generate_tempdir() -> TempDir {
    TempDir::new("lore-storage-maintenance-test-")
}

async fn create_test_store(path: Option<PathBuf>) -> Arc<dyn ImmutableStore> {
    // These GC-mechanics tests put non-durable fragments and then expect
    // eviction/compaction to act on them, so they run with local-fragment
    // protection disabled (server behavior). Protection itself is covered by
    // the `evict_bucket_*` unit tests. The passes act on what earlier claims
    // loaded, so the store keeps its state across holds.
    LocalImmutableStore::new(
        path,
        ImmutableStoreSettings {
            protect_local_fragment: false,
            reuse_state_across_holds: true,
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

    let partition = lore_storage::Partition::from([0x01; 16]);

    for i in 0u8..10 {
        let data = vec![i; 1024];
        let hash = lore_storage::hash_slice(&data);
        let address = lore_storage::Address {
            hash,
            context: lore_storage::Context::from([i; 16]),
        };
        let frag = lore_storage::Fragment {
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

    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

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

    let partition = lore_storage::Partition::from([0x09; 16]);
    let data = vec![0xab_u8; 4096];
    let hash = lore_storage::hash_slice(&data);
    let address = lore_storage::Address {
        hash,
        context: lore_storage::Context::from([1; 16]),
    };
    let frag = lore_storage::Fragment {
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
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    gc(store.clone(), 1, 1, false, None).await;

    let (_frag, payload) = store
        .clone()
        .get(partition, address)
        .await
        .and_then(lore_storage::store_types::StoreGetData::into_payload)
        .expect("non-durable fragment must survive GC");
    assert_eq!(payload.len(), data.len());
}

/// Stopping a pass while it runs must never lose data. Compaction moves payloads
/// between packfiles and eviction drops them, so this races a stop against a pass with
/// caps low enough to force both and requires every protected fragment to read back.
/// Half the fragments are durable, in the same group as the protected ones, so that
/// eviction orphans them and compaction has the protected ones to move. Where the stop
/// lands varies between runs; the requirement holds wherever it lands.
#[tokio::test]
async fn fragments_survive_a_stopped_gc_pass() {
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

    let partition = lore_storage::Partition::from([0x0b; 16]);
    let (_group_index, payloads) = payloads_in_one_group(32);
    let mut stored = vec![];
    for (index, payload) in payloads.iter().enumerate() {
        let durable = index % 2 == 0;
        let address = put_payload(
            &store,
            partition,
            lore_storage::Context::default(),
            payload,
            durable,
        )
        .await;
        if !durable {
            stored.push((address, payload.len()));
        }
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let store: Arc<dyn ImmutableStore> = store;
    let pass = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { gc(store, 1, 1, false, None).await })
    };
    tokio::task::yield_now().await;
    store.clone().stop_gc(true).await;
    pass.await.unwrap();

    for (address, size) in stored {
        let (_fragment, payload) = store
            .clone()
            .get(partition, address)
            .await
            .and_then(lore_storage::store_types::StoreGetData::into_payload)
            .expect("fragment must survive a stopped pass");
        assert_eq!(payload.len(), size);
    }
}

#[tokio::test]
async fn gc_runs_eviction_only() {
    let dir = generate_tempdir();
    let store = create_test_store(Some(dir.to_path_buf())).await;

    let partition = lore_storage::Partition::from([0x02; 16]);

    for i in 0u8..5 {
        let data = vec![i; 2048];
        let hash = lore_storage::hash_slice(&data);
        let address = lore_storage::Address {
            hash,
            context: lore_storage::Context::from([i; 16]),
        };
        let frag = lore_storage::Fragment {
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
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

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

/// A store whose first compaction step fails, counting the steps it is asked for.
struct FailingCompactStore {
    inner: Arc<dyn ImmutableStore>,
    steps: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ImmutableStore for FailingCompactStore {
    async fn get_metadata(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        address: lore_storage::Address,
    ) -> Result<lore_storage::StoreGetData, lore_storage::immutable_store::StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    fn is_local(&self) -> bool {
        self.inner.clone().is_local()
    }

    async fn query(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        addresses: &[lore_storage::Address],
        results: &mut [lore_storage::store_types::StoreMatchResult],
    ) -> Result<(), lore_storage::immutable_store::StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        address: lore_storage::Address,
    ) -> Result<lore_storage::StoreGetData, lore_storage::immutable_store::StoreError> {
        self.inner.clone().get(partition, address).await
    }

    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::immutable_store::StoreError,
    > {
        self.inner.clone().hold_for_command().await
    }

    async fn put(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        address: lore_storage::Address,
        fragment: lore_storage::Fragment,
        payload: Option<bytes::Bytes>,
        force: bool,
    ) -> Result<(), lore_storage::immutable_store::StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        address: lore_storage::Address,
        stats: Arc<lore_storage::store_types::StoreObliterateStats>,
    ) -> Result<(), lore_storage::immutable_store::StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, lore_storage::immutable_store::StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        _max_size: usize,
        _at: Option<usize>,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, lore_storage::immutable_store::StoreError> {
        let step = self
            .steps
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if step == 0 {
            return Err(lore_storage::immutable_store::StoreError::internal(
                "FailingCompactStore: first step fails",
            ));
        }
        Ok(None)
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        None
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn flush(
        self: Arc<Self>,
        options: FlushOptions,
    ) -> Result<(), lore_storage::immutable_store::StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(
        self: Arc<Self>,
        heal: bool,
    ) -> Result<(), lore_storage::immutable_store::StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: lore_storage::Partition,
        source_address: lore_storage::Address,
        destination_partition: lore_storage::Partition,
        destination_context: lore_storage::Context,
        behavior: lore_storage::immutable_store::CopyBehavior,
    ) -> Result<(), lore_storage::immutable_store::StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

/// A failed compaction step is reported and the compactor goes on to the next interval, rather
/// than stopping for the rest of the process.
#[tokio::test]
async fn compactor_goes_on_after_a_failed_step() {
    let inner: Arc<dyn ImmutableStore> = create_test_store(None).await;
    let store = Arc::new(FailingCompactStore {
        inner,
        steps: std::sync::atomic::AtomicUsize::new(0),
    });
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    let weak = Arc::downgrade(&dyn_store);
    #[allow(clippy::disallowed_methods)] // Test-local task, joined below.
    let compactor = tokio::spawn(compactor(weak, 1024, Some(Duration::from_millis(1)), false));

    for _ in 0..1000 {
        if store.steps.load(std::sync::atomic::Ordering::Relaxed) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let steps = store.steps.load(std::sync::atomic::Ordering::Relaxed);
    drop(dyn_store);
    drop(store);
    compactor.await.unwrap();
    assert!(steps >= 2, "the compactor stopped after {steps} step");
}

#[tokio::test]
async fn evict_bucket() {
    use lore_storage::local::immutable_store::ImmutableData;
    use lore_storage::local::immutable_store::ImmutableStoreBucket;
    use lore_storage::local::immutable_store::ImmutableStoreEntry;
    use tokio::sync::RwLock;

    let mut bucket = ImmutableStoreBucket::default();

    bucket.entry.push(ImmutableStoreEntry {
        address: rand::random::<lore_storage::Address>(),
        partition: rand::random::<lore_storage::Partition>(),
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
        address: rand::random::<lore_storage::Address>(),
        partition: rand::random::<lore_storage::Partition>(),
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
        address: rand::random::<lore_storage::Address>(),
        partition: rand::random::<lore_storage::Partition>(),
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
        address: rand::random::<lore_storage::Address>(),
        partition: rand::random::<lore_storage::Partition>(),
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
        address: rand::random::<lore_storage::Address>(),
        partition: rand::random::<lore_storage::Partition>(),
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
        address: rand::random::<lore_storage::Address>(),
        partition: rand::random::<lore_storage::Partition>(),
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
/// evicted, even when the bucket is far over capacity and the oldest fragments
/// are the non-durable ones. They still count toward the capacity target: with a
/// capacity of 3 over six fragments, the three oldest durable ones go and the
/// newest durable one remains.
#[tokio::test]
async fn evict_bucket_protects_non_durable() {
    use lore_storage::FragmentFlags;
    use lore_storage::local::immutable_store::ImmutableData;
    use lore_storage::local::immutable_store::ImmutableStoreBucket;
    use lore_storage::local::immutable_store::ImmutableStoreEntry;
    use tokio::sync::RwLock;

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
            address: rand::random::<lore_storage::Address>(),
            partition: rand::random::<lore_storage::Partition>(),
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

    let evict_count =
        LocalImmutableStore::evict_oldest_bucket(bucket.clone(), &dirty, 3, true).await;

    assert_eq!(evict_count, 3);

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
    let durable_access: Vec<u64> = bucket
        .entry
        .iter()
        .filter(|e| e.data.flags & durable != 0)
        .map(|e| e.data.last_access)
        .collect();
    assert_eq!(durable_access, vec![400]);
}

/// A bucket that its non-durable fragments alone keep over the capacity target
/// loses every durable fragment, and keeps every non-durable one.
#[tokio::test]
async fn evict_bucket_evicts_every_durable_when_non_durable_exceed_target() {
    use lore_storage::FragmentFlags;
    use lore_storage::local::immutable_store::ImmutableData;
    use lore_storage::local::immutable_store::ImmutableStoreBucket;
    use lore_storage::local::immutable_store::ImmutableStoreEntry;
    use tokio::sync::RwLock;

    let durable = FragmentFlags::PayloadStoredDurable.bits();
    let fixtures = [
        (10, true),
        (20, false),
        (30, false),
        (40, false),
        (50, true),
    ];

    let mut bucket = ImmutableStoreBucket::default();
    for (i, (last_access, is_durable)) in fixtures.iter().enumerate() {
        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<lore_storage::Address>(),
            partition: rand::random::<lore_storage::Partition>(),
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

    let evict_count =
        LocalImmutableStore::evict_oldest_bucket(bucket.clone(), &dirty, 2, true).await;

    assert_eq!(evict_count, 2);

    let bucket = bucket.read().await;
    let mut remaining: Vec<u64> = bucket.entry.iter().map(|e| e.data.last_access).collect();
    remaining.sort_unstable();
    assert_eq!(remaining, vec![20, 30, 40]);
}

/// With protection disabled (server behavior) eviction is pure LRU and the
/// durable flag is ignored — the oldest fragments are evicted regardless.
#[tokio::test]
async fn evict_bucket_unprotected_ignores_durable_flag() {
    use lore_storage::FragmentFlags;
    use lore_storage::local::immutable_store::ImmutableData;
    use lore_storage::local::immutable_store::ImmutableStoreBucket;
    use lore_storage::local::immutable_store::ImmutableStoreEntry;
    use tokio::sync::RwLock;

    let durable = FragmentFlags::PayloadStoredDurable.bits();
    let fixtures = [(10, false), (20, false), (100, true), (200, true)];

    let mut bucket = ImmutableStoreBucket::default();
    for (i, (last_access, is_durable)) in fixtures.iter().enumerate() {
        bucket.entry.push(ImmutableStoreEntry {
            address: rand::random::<lore_storage::Address>(),
            partition: rand::random::<lore_storage::Partition>(),
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

/// A group of 256 buckets over a packstore in `dir`, outside any store.
fn test_group(dir: &TempDir) -> Arc<lore_storage::local::immutable_store::ImmutableStoreGroup> {
    use std::sync::OnceLock;

    use lore_storage::local::immutable_store::BUCKET_COUNT;
    use lore_storage::local::immutable_store::ImmutableStoreGroup;
    use lore_storage::packstore::PackStore;
    use tokio::task::JoinSet;

    Arc::new(ImmutableStoreGroup {
        bucket: [const { OnceLock::new() }; BUCKET_COUNT],
        dirty: std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false)),
        soft_dirty: std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false)),
        bucket_count: std::sync::atomic::AtomicUsize::new(
            lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
        ),
        serialize_version: std::sync::atomic::AtomicU32::new(
            lore_storage::local::immutable_store::ImmutableStoreVersion::LazyFanOut as u32,
        ),
        fan_out_threshold: lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
        committed_level: std::sync::atomic::AtomicUsize::new(
            lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
        ),
        pending_level: std::sync::atomic::AtomicUsize::new(0),
        flush_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        writes: Arc::default(),
        packstore: PackStore::new(Some(dir.to_path_buf()), 1, None),
        flush: tokio::sync::Mutex::new(JoinSet::new()),
        scheduled: std::sync::atomic::AtomicBool::new(false),
        payload_gate: Default::default(),
    })
}

/// Push entries with `hash` holding `data`, each under its own context, to bucket 0 of `group`.
async fn push_entries(
    group: &lore_storage::local::immutable_store::ImmutableStoreGroup,
    hash: lore_storage::Hash,
    data: &[lore_storage::local::immutable_store::ImmutableData],
) {
    use lore_storage::local::immutable_store::ImmutableStoreEntry;

    let mut bucket = group.bucket(0).write().await;
    for data in data {
        let index = bucket.entry.len() as u32;
        bucket.entry.push(ImmutableStoreEntry {
            address: lore_storage::Address {
                hash,
                context: lore_storage::Context::from([index as u8; 16]),
            },
            partition: lore_storage::Partition::default(),
            data: *data,
        });
        bucket.sorted_index.push(index);
    }
}

/// Entry data for a payload of `size` bytes at `pack_file` and `pack_offset`, with `flags`.
fn entry_data(
    pack_file: u32,
    pack_offset: u32,
    size: u32,
    flags: u32,
) -> lore_storage::local::immutable_store::ImmutableData {
    lore_storage::local::immutable_store::ImmutableData {
        flags,
        size_payload: size,
        size_content: size as u64,
        pack_offset,
        pack_file,
        last_access: 0,
    }
}

/// Rewriting a packfile moves only the entries at a payload's location in it, changing where
/// they point and nothing else. Another copy of the same content elsewhere, and an obliterated
/// entry with the same hash, keep what they hold.
#[tokio::test]
async fn compact_bucket_moves_only_the_entries_at_the_copied_location() {
    use lore_storage::FragmentFlags;

    let tempdir = generate_tempdir();
    let group = test_group(&tempdir);
    let payload = bytes::Bytes::from_static(&[7; 64]);
    let hash = lore_storage::Hash::hash_buffer(payload.as_ref());
    let stored = group.packstore.store(payload).await.unwrap();
    let local = FragmentFlags::PayloadStoredLocal.bits();
    let obliterated = entry_data(0, 0, 0, FragmentFlags::PayloadObliterated.bits());
    let elsewhere = entry_data(9, 0, 64, local);
    push_entries(
        &group,
        hash,
        &[
            entry_data(stored.id, stored.offset, 64, local),
            elsewhere,
            obliterated,
        ],
    )
    .await;
    group.packstore.stop_writes(&[stored.id]).await.unwrap();

    assert_eq!(
        LocalImmutableStore::compact_bucket_packfile(&group, 0, 0, stored.id).await,
        (64, false)
    );

    let bucket = group.bucket(0).read().await;
    let moved = bucket.entry[0].data;
    assert_ne!(moved.pack_file, stored.id, "the copied entry moves");
    assert_eq!(
        entry_data(stored.id, stored.offset, 64, local),
        lore_storage::local::immutable_store::ImmutableData {
            pack_file: stored.id,
            pack_offset: stored.offset,
            ..moved
        },
        "only where the copied entry points changes"
    );
    assert_eq!(bucket.entry[1].data, elsewhere, "another copy stays");
    assert_eq!(
        bucket.entry[2].data, obliterated,
        "the obliterated entry stays obliterated"
    );
}

/// A payload that cannot be copied keeps its entries where they are, and the bucket reports an
/// entry still in the packfile, so that the packfile is not truncated under it.
#[tokio::test]
async fn compact_bucket_keeps_the_entries_it_cannot_copy() {
    let tempdir = generate_tempdir();
    let group = test_group(&tempdir);
    let payload = bytes::Bytes::from_static(&[5; 64]);
    let stored = group.packstore.store(payload.clone()).await.unwrap();
    let beyond_the_end = entry_data(stored.id, 4096, 64, 0);
    push_entries(
        &group,
        lore_storage::Hash::hash_buffer(payload.as_ref()),
        &[entry_data(stored.id, stored.offset, 64, 0)],
    )
    .await;
    push_entries(
        &group,
        lore_storage::Hash::from([0xffu8; 32]),
        &[beyond_the_end],
    )
    .await;
    group.packstore.stop_writes(&[stored.id]).await.unwrap();

    let (_, referenced) =
        LocalImmutableStore::compact_bucket_packfile(&group, 0, 0, stored.id).await;

    assert!(referenced, "an entry still points into the packfile");
    let bucket = group.bucket(0).read().await;
    assert_eq!(bucket.entry[1].data, beyond_the_end);
    assert!(
        bucket.entry.iter().all(|entry| entry.data.pack_file != 0),
        "no entry loses its payload"
    );
}

/// Compaction moves an entry only to a copy of its own size: of three entries at one offset
/// claiming different sizes, the largest more than the packfile holds, each that moves reads
/// back its own bytes from its copy, and the one left keeps the packfile.
#[tokio::test]
async fn compact_bucket_moves_an_entry_only_to_a_copy_of_its_size() {
    use lore_storage::FragmentFlags;

    let tempdir = generate_tempdir();
    let group = test_group(&tempdir);
    let payload = bytes::Bytes::from((0..32u8).collect::<Vec<_>>());
    let hash = lore_storage::Hash::hash_buffer(payload.as_ref());
    let stored = group.packstore.store(payload.clone()).await.unwrap();
    let local = FragmentFlags::PayloadStoredLocal.bits();
    push_entries(
        &group,
        hash,
        &[
            entry_data(stored.id, stored.offset, 16, local),
            entry_data(stored.id, stored.offset, 32, local),
            entry_data(stored.id, stored.offset, 64, local),
        ],
    )
    .await;
    group.packstore.stop_writes(&[stored.id]).await.unwrap();

    let (_, referenced) =
        LocalImmutableStore::compact_bucket_packfile(&group, 0, 0, stored.id).await;

    assert!(referenced, "the entry left in the packfile keeps it");
    let bucket = group.bucket(0).read().await;
    for entry in bucket.entry.iter() {
        if entry.data.pack_file == stored.id {
            continue;
        }
        let read = group
            .packstore
            .load(
                entry.data.pack_file,
                entry.data.pack_offset,
                entry.data.size_payload,
            )
            .await
            .unwrap();
        assert_eq!(read.as_ref(), &payload[..entry.data.size_payload as usize]);
    }
}

/// A bucket none of whose payloads in the packfile can be copied changes nothing and reports
/// the packfile still referenced.
#[tokio::test]
async fn compact_bucket_with_nothing_it_can_copy_changes_nothing() {
    let tempdir = generate_tempdir();
    let group = test_group(&tempdir);
    let stored = group
        .packstore
        .store(bytes::Bytes::from_static(&[5; 64]))
        .await
        .unwrap();
    let beyond_the_end = entry_data(stored.id, 4096, 64, 0);
    push_entries(
        &group,
        lore_storage::Hash::from([0x11u8; 32]),
        &[beyond_the_end],
    )
    .await;
    group.packstore.stop_writes(&[stored.id]).await.unwrap();

    assert_eq!(
        LocalImmutableStore::compact_bucket_packfile(&group, 0, 0, stored.id).await,
        (0, true)
    );
    assert_eq!(group.bucket(0).read().await.entry[0].data, beyond_the_end);
}

/// `gc` evicts before it compacts, so that compaction reclaims what eviction removes: its one
/// eviction begins before the first compaction step.
#[tokio::test]
async fn gc_evicts_before_it_compacts() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct OrderSink {
        events: Mutex<Vec<&'static str>>,
    }

    impl lore_storage::gc_event::GcEventSink for OrderSink {
        fn eviction_begin(&self, _target_fragments: u64) {
            self.events.lock().unwrap().push("eviction");
        }
        fn eviction_progress(&self, _evicted: u64) {}
        fn eviction_end(&self, _total_evicted: u64) {}
        fn compaction_begin(&self, _target_bytes: u64) {
            self.events.lock().unwrap().push("compaction");
        }
        fn compaction_progress(&self, _compacted_bytes: u64) {}
        fn compaction_end(&self, _total_compacted_bytes: u64) {}
    }

    let tempdir = generate_tempdir();
    let store = LocalImmutableStore::new(
        Some(tempdir.to_path_buf()),
        ImmutableStoreSettings::default(),
    )
    .await
    .unwrap();
    let partition = lore_storage::Partition::from([0x51u8; 16]);
    let (_, payloads) = payloads_in_one_group(8);
    for payload in &payloads {
        put_payload(
            &store,
            partition,
            lore_storage::Context::default(),
            payload,
            true,
        )
        .await;
    }
    let sink = Arc::new(OrderSink::default());

    gc(store.clone(), 1, 1, false, Some(sink.clone())).await;

    let events = sink.events.lock().unwrap();
    assert_eq!(events.first(), Some(&"eviction"));
    assert!(events.len() > 1 && events[1..].iter().all(|event| *event == "compaction"));
}

#[tokio::test]
async fn compact_bucket() {
    use bytes::Bytes;
    use lore_storage::local::immutable_store::ImmutableData;
    use lore_storage::local::immutable_store::ImmutableStoreEntry;

    let tempdir = generate_tempdir();
    let group = test_group(&tempdir);

    // Buffer lengths are primes to ensure test actually verify the correct thing
    let first_buffer = Bytes::copy_from_slice(&[0, 1, 2, 3, 4, 5, 6]);
    let second_buffer = Bytes::copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    let third_buffer = Bytes::copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

    let first_hash = lore_storage::Hash::hash_buffer(first_buffer.as_ref());
    let second_hash = lore_storage::Hash::hash_buffer(second_buffer.as_ref());
    let third_hash = lore_storage::Hash::hash_buffer(third_buffer.as_ref());

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

        let mut mid_context: lore_storage::Context = rand::random();
        let mut mid_repository: lore_storage::Partition = rand::random();

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
            address: lore_storage::Address {
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
            address: lore_storage::Address {
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
            address: lore_storage::Address {
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
            address: lore_storage::Address {
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
            address: lore_storage::Address {
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
        .stop_writes(&[1])
        .await
        .expect("Failed to stop write");

    let (compacted_size, referenced) =
        LocalImmutableStore::compact_bucket_packfile(&group, 0, 0, 1).await;

    // Two instances of the data should have been rewritten to new packfiles
    assert_eq!(compacted_size, mid_buffer.len() + greater_buffer.len());
    assert!(!referenced);
}
