// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::maintenance::*;

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
    store.clone().flush(true).await.unwrap();

    // Tiny caps drive both eviction and compaction with a zero per-bucket target.
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
/// Where the stop lands varies between runs; the requirement holds wherever it lands.
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
    let mut stored = vec![];
    for i in 0u8..32 {
        let data = vec![i; 4096];
        let hash = lore_storage::hash_slice(&data);
        let address = lore_storage::Address {
            hash,
            context: lore_storage::Context::from([i; 16]),
        };
        let frag = lore_storage::Fragment {
            // Non-durable, so eviction is forbidden to reclaim it and the fragment must
            // survive however far the pass got.
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
                Some(bytes::Bytes::from(data.clone())),
                false,
            )
            .await
            .unwrap();
        stored.push((address, data.len()));
    }
    store.clone().flush(true).await.unwrap();

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
/// evicted and must not count toward the capacity target, even when the bucket
/// is far over capacity and the oldest fragments are the non-durable ones.
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

#[tokio::test]
async fn compact_bucket() {
    use std::sync::OnceLock;

    use bytes::Bytes;
    use lore_storage::local::immutable_store::BUCKET_COUNT;
    use lore_storage::local::immutable_store::ImmutableData;
    use lore_storage::local::immutable_store::ImmutableStoreEntry;
    use lore_storage::local::immutable_store::ImmutableStoreGroup;
    use lore_storage::packstore::PackStore;
    use tokio::task::JoinSet;

    let tempdir = generate_tempdir();
    let group = Arc::new(ImmutableStoreGroup {
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
        flush_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        packstore: PackStore::new(Some(tempdir.to_path_buf()), 1, None),
        flush: tokio::sync::Mutex::new(JoinSet::new()),
        scheduled: std::sync::atomic::AtomicBool::new(false),
    });

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
        .stop_write(1)
        .await
        .expect("Failed to stop write");

    let compacted_size =
        LocalImmutableStore::compact_bucket_packfile_impl(&group, 0, 0, 1, false).await;

    // Two instances of the data should have been rewritten to new packfiles
    assert_eq!(compacted_size, mid_buffer.len() + greater_buffer.len());
}
