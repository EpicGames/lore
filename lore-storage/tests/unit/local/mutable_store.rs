// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic;

use lore_storage::FlushOptions;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::format_bucket_path;
use lore_storage::local::mutable_store::*;
use lore_storage::local::store_lock::Intent;
use lore_storage::store_types::KeyType;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

use crate::store_fixture::wait_for;

mod removal;

fn write_bucket_file(path: &Path, version: u32) {
    let entry = MutableStoreEntry::default();
    let mut header = MutableStoreHeader::new_zeroed();
    header.version = version;
    header.count = 1;
    let mut bytes =
        Vec::with_capacity(size_of::<MutableStoreHeader>() + 4 + size_of::<MutableStoreEntry>());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(entry.as_bytes());
    std::fs::write(path, bytes).unwrap();
}

/// A bucket larger than the head the composite open returns is loaded by scattering one
/// vectored read into both `GrowVec`s. Per-index contents make a misplaced chunk visible as
/// swapped entries rather than as a length mismatch.
#[tokio::test]
async fn deserialize_scatters_a_bucket_larger_than_the_head_read() {
    let dir = lore_base::test_util::TempDir::new("ms_scatter_");
    let path = dir.path().join("bucket");

    let head = lore_storage::local::immutable_store::BUCKET_HEAD_READ;
    let per_entry = size_of::<u32>() + size_of::<MutableStoreEntry>();
    let count = (head / per_entry) + 64;
    assert!(
        size_of::<MutableStoreHeader>() + count * per_entry > head,
        "the bucket has to exceed the head read for this to test anything"
    );

    let mut header = MutableStoreHeader::new_zeroed();
    header.version = MutableStoreVersion::LazyFanOut as u32;
    header.count = count as u32;

    let mut bytes = Vec::with_capacity(size_of::<MutableStoreHeader>() + count * per_entry);
    bytes.extend_from_slice(header.as_bytes());
    for index in 0..count {
        bytes.extend_from_slice(&(index as u32).to_le_bytes());
    }
    for index in 0..count {
        let entry = MutableStoreEntry {
            key: Hash::from([index as u8; 32]),
            ..Default::default()
        };
        bytes.extend_from_slice(entry.as_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let (sorted_index, entry, version) = MutableStoreBucket::deserialize_files(path, false)
        .await
        .unwrap();

    assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
    assert_eq!(sorted_index.len(), count);
    assert_eq!(entry.len(), count);
    for index in 0..count {
        assert_eq!(sorted_index[index], index as u32, "sorted index at {index}");
        assert_eq!(
            entry[index].key,
            Hash::from([index as u8; 32]),
            "key at {index}"
        );
    }
}

#[tokio::test]
async fn deserialize_accepts_typed_items_v2() {
    let dir = lore_base::test_util::TempDir::new("ms_v2_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, MutableStoreVersion::TypedItems as u32);
    let result = MutableStoreBucket::deserialize_files(path, false).await;
    assert!(result.is_ok(), "v2 (TypedItems) bucket should deserialize");
}

#[tokio::test]
async fn deserialize_accepts_lazy_fan_out_v3() {
    let dir = lore_base::test_util::TempDir::new("ms_v3_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, MutableStoreVersion::LazyFanOut as u32);
    let result = MutableStoreBucket::deserialize_files(path, false).await;
    assert!(result.is_ok(), "v3 (LazyFanOut) bucket should deserialize");
}

#[tokio::test]
async fn deserialize_rejects_unknown_future_version() {
    let dir = lore_base::test_util::TempDir::new("ms_v100_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, 100);
    let result = MutableStoreBucket::deserialize_files(path, false).await;
    assert!(result.is_err(), "v100 bucket should be rejected as too new");
}

/// A torn write can leave a bucket file at its correct byte length but entirely
/// zero-filled, so the header reads count=0 while the size implies a nonzero count. This
/// recovers to an empty bucket rather than hard-erroring.
#[tokio::test]
async fn deserialize_recovers_zero_filled_bucket() {
    let dir = lore_base::test_util::TempDir::new("ms_zerofill_");
    let path = dir.path().join("bucket");
    // Correct length for a 1-entry bucket, but all zeros.
    let len = size_of::<MutableStoreHeader>() + size_of::<u32>() + size_of::<MutableStoreEntry>();
    std::fs::write(&path, vec![0u8; len]).unwrap();

    let (sorted_index, entry, version) = MutableStoreBucket::deserialize_files(path, false)
        .await
        .expect("zero-filled bucket should recover to empty");
    assert_eq!(sorted_index.len(), 0);
    assert_eq!(entry.len(), 0);
    assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
}

#[tokio::test]
async fn deserialize_authoritative_errors_and_preserves_corrupt_bucket() {
    let dir = lore_base::test_util::TempDir::new("ms_auth_corrupt_");
    let path = dir.path().join("bucket");
    let len = size_of::<MutableStoreHeader>() + size_of::<u32>() + size_of::<MutableStoreEntry>();
    std::fs::write(&path, vec![0u8; len]).unwrap();

    let result = MutableStoreBucket::deserialize_files(path.clone(), true).await;
    assert!(
        result.is_err(),
        "authoritative store must not reset a corrupt bucket"
    );
    assert!(
        path.exists(),
        "authoritative store must preserve the corrupt bucket file"
    );
}

/// Client-shaped settings: groups start at level 1, and the store keeps its state across holds,
/// as one does in a process that retains stores. A test builds a store's state under one claim
/// and examines it under the next.
fn client_settings() -> MutableStoreSettings {
    MutableStoreSettings {
        initial_fan_out_level: 1,
        reuse_state_across_holds: true,
        ..Default::default()
    }
}

/// Store `count` keys spread across groups, at bucket bytes that route away from bucket 0
/// once a group is at 256 — so a group misread as pre-fan-out sends a lookup to a different
/// bucket than the one holding it.
async fn store_keys(store: &Arc<LocalMutableStore>, partition: Partition, count: u8) -> Vec<Hash> {
    use lore_storage::mutable_store::MutableStore;
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    let mut keys = Vec::new();
    for index in 0..count {
        let mut key = Hash::default();
        key.data_mut()[0] = index;
        key.data_mut()[1] = 0xAB;
        dyn_store
            .clone()
            .store(
                partition,
                key,
                Hash::from_u64(index as u64 + 1),
                KeyType::BranchMetadata,
            )
            .await
            .expect("store succeeds");
        keys.push(key);
    }
    keys
}

/// Run the background timer's flush for every group's bucket 0, and nothing else. At level 1
/// that is the only addressable bucket.
async fn run_delayed_flush(store: &Arc<LocalMutableStore>) {
    let weak = Arc::downgrade(store);
    for group_index in 0..GROUP_COUNT {
        // Sweeps every dirty bucket in the group, so no bucket index is named.
        LocalMutableStore::flush_delayed(weak.clone(), group_index, 0).await;
    }
}

/// A flush that finds a group's lock held by a bucket write reads the group once the write has
/// ended. A write that ends without landing flags the bucket again before it releases the lock, so
/// the flush writes the bucket; returned instead, it would acknowledge an entry that is only in
/// memory.
#[tokio::test]
async fn a_flush_writes_a_bucket_whose_claimed_write_did_not_land() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_flush_write_failed_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                flush_delay_seconds: 3600,
                ..client_settings()
            },
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let keys = store_keys(&store, Partition::default(), 1).await;
    let group_index = keys[0].data()[0] as usize;
    let group = &store.group[group_index];

    // A write that took the lock and claimed the mark, and has not written the file.
    let writing = group.flush_lock.clone().lock_owned().await;
    assert!(group.dirty[0].swap(false, atomic::Ordering::Relaxed));
    let flushing: Arc<dyn MutableStore> = store.clone();
    let mut flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut flush)
            .await
            .is_err(),
        "the flush returned while a claimed bucket write held the group"
    );
    // The write ends without landing, which flags the bucket again before the lock goes.
    group.dirty[0].store(true, atomic::Ordering::Relaxed);
    drop(writing);
    flush.await.expect("flush task").expect("flush");

    assert_bucket_file_holds(dir.path(), group_index, &keys[0]).await;
}

/// A flush with a bucket to write skips the groups it finds clean, each read under the group's
/// flush lock, as in the immutable store's test of this name: a bucket write in one of them that
/// ends without landing flags the bucket again before it releases the lock.
#[tokio::test]
async fn a_flush_skips_no_group_whose_claimed_write_did_not_land() {
    use lore_storage::mutable_store::MutableStore;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_flush_skip_failed_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                flush_delay_seconds: 3600,
                ..client_settings()
            },
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let keys = store_keys(&store, Partition::default(), 2).await;
    let group = &store.group[1];

    // A write that took the lock of group 1 and claimed the mark, and has not written the file.
    // The flush is held before it tries each group's lock, so that the write can end between
    // its reads of groups 0 and 1.
    let writing = group.flush_lock.clone().lock_owned().await;
    assert!(group.dirty[0].swap(false, atomic::Ordering::Relaxed));
    let scanning = [
        lore_storage::test_util::pause_at(PausePoint::Scanning),
        lore_storage::test_util::pause_at(PausePoint::Scanning),
    ];
    let flushing: Arc<dyn MutableStore> = store.clone();
    let flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    scanning[0].reached.notified().await;
    scanning[0].resume.notify_one();
    scanning[1].reached.notified().await;
    // The write ends without landing, which flags the bucket again before the lock goes.
    group.dirty[0].store(true, atomic::Ordering::Relaxed);
    drop(writing);
    scanning[1].resume.notify_one();
    flush.await.expect("flush task").expect("flush");

    for (group_index, key) in keys.iter().enumerate() {
        assert_bucket_file_holds(dir.path(), group_index, key).await;
    }
}

/// Assert that the file of bucket 0 of group `group_index` under `dir` holds `key`, whose first
/// two bytes name it among the keys [`store_keys`] stores.
async fn assert_bucket_file_holds(dir: &Path, group_index: usize, key: &Hash) {
    let (_, written, _) = MutableStoreBucket::deserialize_files(
        format_bucket_path(&dir.join("mutable"), group_index, 0),
        false,
    )
    .await
    .expect("the bucket file reads");
    assert!(
        written
            .iter()
            .any(|entry| entry.key.data()[..2] == key.data()[..2]),
        "group {group_index}: the flush acknowledged a key its bucket file does not hold"
    );
}

/// **A list that cannot read a bucket fails**, rather than handing back a stream that ends as if
/// complete with that bucket's keys left out, which a caller cannot tell from keys that are not
/// there.
#[tokio::test]
async fn a_list_that_cannot_read_a_bucket_fails() {
    use futures::StreamExt;
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_list_unreadable_");
    let settings = || MutableStoreSettings {
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let keys = {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let keys = store_keys(&store, Partition::default(), 2).await;
        let flushing: Arc<dyn MutableStore> = store;
        flushing
            .flush(FlushOptions::default())
            .await
            .expect("flush");
        keys
    };
    let store = |path: PathBuf| async move {
        Arc::new(
            LocalMutableStore::new(Some(path), settings(), make_in_memory_immutable().await)
                .await
                .expect("store opens"),
        )
    };
    let listing: Arc<dyn MutableStore> = store(dir.path().to_path_buf()).await;
    let listed = listing
        .list(Partition::default(), KeyType::BranchMetadata)
        .await
        .expect("lists")
        .collect::<Vec<_>>()
        .await;
    assert_eq!(listed.len(), keys.len(), "a list reads every bucket");

    // Group 1's bucket file replaced by a directory, which opens and fails every read.
    let bucket = format_bucket_path(&dir.path().join("mutable"), 1, 0);
    std::fs::remove_file(&bucket).expect("bucket file");
    std::fs::create_dir(&bucket).expect("directory in its place");
    let reopened = store(dir.path().to_path_buf()).await;
    let lock = reopened
        .lock
        .clone()
        .expect("a disk-backed store has a lock");
    let listing: Arc<dyn MutableStore> = reopened;
    assert!(
        listing
            .list(Partition::default(), KeyType::BranchMetadata)
            .await
            .is_err(),
        "a list that could not read a bucket handed back a stream leaving its keys out"
    );
    wait_for(
        async || !lock.is_held(),
        "the producers waiting to send ended with the list, and released the claim",
    )
    .await;
}

/// A store over `dir` whose group `07` fanned out past one bucket, opened afresh so that none of
/// its buckets is in memory.
async fn fanned_out_store(dir: &Path) -> Arc<LocalMutableStore> {
    use lore_storage::mutable_store::MutableStore;
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let store = Arc::new(
        LocalMutableStore::new(Some(dir), settings(), make_in_memory_immutable().await)
            .await
            .expect("store opens"),
    );
    let writing: Arc<dyn MutableStore> = store.clone();
    store_spread_keys(&writing, 0..6).await;
    writing.flush(FlushOptions::default()).await.expect("flush");
    assert!(
        store.group[7].bucket_count.load(atomic::Ordering::Relaxed) > 1,
        "the group fanned out"
    );
    Arc::new(
        LocalMutableStore::new(Some(dir), settings(), make_in_memory_immutable().await)
            .await
            .expect("store reopens"),
    )
}

/// Spawn a list of `store`'s keys, which ends once the stream is handed back, and drive it for
/// 100 ms, which it is still waiting at when group `07`'s first bucket is held.
async fn start_listing(
    store: &Arc<LocalMutableStore>,
) -> tokio::task::JoinHandle<
    Result<lore_storage::store_types::KeyValueStream, lore_storage::StoreError>,
> {
    use lore_storage::mutable_store::MutableStore;
    let listing: Arc<dyn MutableStore> = store.clone();
    let mut listing = lore_base::lore_spawn!(async move {
        listing
            .list(Partition::default(), KeyType::BranchMetadata)
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut listing)
            .await
            .is_err(),
        "the list waits for the group it cannot read yet"
    );
    listing
}

/// **A list dropped before it returns stops reading buckets.** Its producers go on only while
/// something can still receive the stream: one left reading its group would otherwise read every
/// remaining bucket file of it, and keep the store claimed, for no caller.
#[tokio::test]
async fn a_list_dropped_before_it_returns_stops_reading_buckets() {
    let dir = lore_base::test_util::TempDir::new("ms_list_dropped_");
    let store = fanned_out_store(dir.path()).await;
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let group = store.group[7].clone();

    // Group 7's producer waits at its first bucket, which the list waits for.
    let blocker = group.bucket(0).clone().write_owned().await;
    let listing = start_listing(&store).await;
    listing.abort();
    assert!(listing.await.is_err_and(|err| err.is_cancelled()));
    drop(blocker);

    wait_for(
        async || !lock.is_held(),
        "the producers ended and released the claim",
    )
    .await;
    assert_eq!(
        group.bucket(1).read().await.entry.len(),
        0,
        "the producer left the rest of its group unread"
    );
}

/// **A list's producers send nothing until it hands its stream back.** Keys sent sooner would
/// queue in the stream with no caller yet to take them, all of a large store's while one group is
/// slow to read, and go for nothing if that group then fails the list.
#[tokio::test]
async fn a_list_sends_nothing_before_it_returns() {
    use futures::StreamExt;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_list_gated_");
    let store = fanned_out_store(dir.path()).await;
    let sending = lore_storage::test_util::pause_at(PausePoint::Listing);

    // Group 7's producer waits at its first bucket, which the list waits for; every other group
    // has been read meanwhile.
    let blocker = store.group[7].bucket(0).clone().write_owned().await;
    let listing = start_listing(&store).await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            sending.reached.notified()
        )
        .await
        .is_err(),
        "no producer sends while the list waits for another group"
    );

    drop(blocker);
    let stream = listing.await.expect("the list ends").expect("lists");
    sending.reached.notified().await;
    sending.resume.notify_one();
    assert_eq!(
        stream.collect::<Vec<_>>().await.len(),
        6,
        "and once it has returned, every key is sent"
    );
}

/// **A list leaves out no key a fan-out moves.** A producer reads its group's bucket count once it
/// holds the group's layout, so a flush that fanned the group out after the producer loaded it at
/// the old layout leaves the walk reading the new one.
#[tokio::test]
async fn a_list_leaves_out_no_key_a_fan_out_moves() {
    use lore_storage::mutable_store::MutableStore;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_list_fan_out_");
    let local = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                fan_out_threshold: 2,
                flush_delay_seconds: 3600,
                ..client_settings()
            },
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let store: Arc<dyn MutableStore> = local.clone();
    store_spread_keys(&store, 0..6).await;
    let group = local.group[7].clone();
    assert_eq!(group.bucket_count.load(atomic::Ordering::Relaxed), 1);

    // Every producer waits to send once the list has returned; group 07's then fans out.
    let sending: Vec<_> = (0..local.group.len())
        .map(|_| lore_storage::test_util::pause_at(PausePoint::Listing))
        .collect();
    let stream = store
        .clone()
        .list(Partition::default(), KeyType::BranchMetadata)
        .await
        .expect("lists");
    for pause in &sending {
        pause.reached.notified().await;
    }
    store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush");
    assert!(
        group.bucket_count.load(atomic::Ordering::Relaxed) > 1,
        "the flush fanned the group out"
    );
    for pause in &sending {
        pause.resume.notify_one();
    }
    assert_eq!(
        listed_keys(stream).await,
        sorted_keys((0..6).map(spread_key)),
        "the list sends every key of the group it loaded before the fan-out, once each"
    );
}

/// **A list leaves out no key a fan-out moves while it walks a group.** The producer walks its
/// group holding the group's layout, so a flush fanning the group out waits for the walk to end,
/// and the walk sends every key at the layout it read.
#[tokio::test]
async fn a_list_leaves_out_no_key_a_fan_out_moves_while_it_walks() {
    use lore_storage::mutable_store::MutableStore;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_list_walk_fan_out_");
    let local = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                fan_out_threshold: 2,
                flush_delay_seconds: 3600,
                ..client_settings()
            },
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let store: Arc<dyn MutableStore> = local.clone();
    store_spread_keys(&store, 0..6).await;
    store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush");
    let group = local.group[7].clone();
    let walked_level = group.bucket_count.load(atomic::Ordering::Relaxed);
    assert_eq!(walked_level, 32);
    // Three more keys in bucket 0 at 32 buckets, which the next flush fans out.
    let crowding: Vec<Hash> = (1..4u8)
        .map(|byte| {
            let mut key = spread_key(0);
            key.data_mut()[1] = byte;
            key
        })
        .collect();
    for key in &crowding {
        store
            .clone()
            .store(
                Partition::default(),
                *key,
                Hash::from_u64(1),
                KeyType::BranchMetadata,
            )
            .await
            .expect("store succeeds");
    }

    let walking = lore_storage::test_util::pause_at(PausePoint::Walking);
    let stream = store
        .clone()
        .list(Partition::default(), KeyType::BranchMetadata)
        .await
        .expect("lists");
    walking.reached.notified().await;
    let mut flushing = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.flush(FlushOptions::default()).await })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut flushing)
            .await
            .is_err(),
        "the flush waits for the walk"
    );
    walking.resume.notify_one();
    assert_eq!(
        listed_keys(stream).await,
        sorted_keys((0..6).map(spread_key).chain(crowding)),
        "the walk sends every key at the layout it read, once each"
    );
    flushing.await.expect("the flush ends").expect("flush");
    assert!(
        group.bucket_count.load(atomic::Ordering::Relaxed) > walked_level,
        "the flush fanned the group out once the walk ended"
    );
}

/// The keys `stream` sends, sorted.
async fn listed_keys(stream: lore_storage::store_types::KeyValueStream) -> Vec<Hash> {
    use futures::StreamExt;
    sorted_keys(stream.map(|(key, _)| key).collect::<Vec<_>>().await)
}

/// `keys`, typed as branch metadata as the store keeps them, sorted.
fn sorted_keys(keys: impl IntoIterator<Item = Hash>) -> Vec<Hash> {
    let mut keys: Vec<Hash> = keys
        .into_iter()
        .map(|mut key| {
            key.data_mut()[2] = KeyType::BranchMetadata as u8;
            key
        })
        .collect();
    keys.sort();
    keys
}

/// **A list whose stream is unread holds back no flush.** A producer sends into a stream that never
/// waits, so a walk holding the group's layout ends however far behind its caller reads, and a
/// flush fanning the group out then takes the layout.
#[tokio::test]
async fn a_list_whose_stream_is_unread_holds_back_no_flush() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_list_unread_");
    let local = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                fan_out_threshold: 64,
                flush_delay_seconds: 3600,
                ..client_settings()
            },
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let store: Arc<dyn MutableStore> = local.clone();
    let keys: Vec<Hash> = (0..512u32)
        .map(|index| {
            let mut key = Hash::default();
            key.data_mut()[0] = 7;
            key.data_mut()[4..8].copy_from_slice(&index.to_le_bytes());
            key
        })
        .collect();
    for (index, key) in keys.iter().enumerate() {
        store
            .clone()
            .store(
                Partition::default(),
                *key,
                Hash::from_u64(index as u64 + 1),
                KeyType::BranchMetadata,
            )
            .await
            .expect("store succeeds");
    }
    assert_eq!(
        local.group[7].bucket_count.load(atomic::Ordering::Relaxed),
        1
    );

    let stream = store
        .clone()
        .list(Partition::default(), KeyType::BranchMetadata)
        .await
        .expect("lists");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.clone().flush(FlushOptions::default()),
    )
    .await
    .expect("no producer holds its group's layout while its stream is unread")
    .expect("flush");
    assert!(
        local.group[7].bucket_count.load(atomic::Ordering::Relaxed) > 1,
        "the flush fanned the group out"
    );
    assert_eq!(
        listed_keys(stream).await,
        sorted_keys(keys),
        "the list sends every key once read"
    );
}

/// A `version` file that cannot be read fails the open and is left as it is.
#[tokio::test]
async fn an_unreadable_version_file_fails_the_open_and_is_kept() {
    let dir = lore_base::test_util::TempDir::new("ms_version_unreadable_");
    let mutable = dir.path().join("mutable");
    std::fs::create_dir_all(mutable.join("index")).expect("index directory");
    // Over the whole-file read limit, which fails the read and not a stamp's write.
    let version = mutable.join("version");
    let length = lore_io::WHOLE_FILE_LIMIT as u64 + 1;
    std::fs::File::create(&version)
        .and_then(|file| file.set_len(length))
        .expect("version file");

    let opened = LocalMutableStore::new(
        Some(dir.path()),
        client_settings(),
        make_in_memory_immutable().await,
    )
    .await;

    assert!(
        opened.is_err(),
        "the open fails rather than stamping the version"
    );
    assert_eq!(
        std::fs::metadata(&version).expect("version file").len(),
        length,
        "the version file is left as it is"
    );
}

/// A delayed flush that cannot write its group's level marker finishes and leaves the bucket
/// dirty. The next store into the bucket starts another, which writes it once the marker can be
/// written.
#[tokio::test]
async fn a_store_after_a_failed_delayed_flush_starts_another() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_delayed_retry_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                flush_delay_seconds: 1,
                ..client_settings()
            },
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    let store_key = |byte: u8| {
        let dyn_store = dyn_store.clone();
        async move {
            let mut key = Hash::default();
            key.data_mut()[0] = 7;
            key.data_mut()[1] = byte;
            dyn_store
                .store(
                    Partition::default(),
                    key,
                    Hash::from_u64(u64::from(byte) + 1),
                    KeyType::BranchMetadata,
                )
                .await
                .expect("store succeeds");
        }
    };
    let group_path = dir.path().join("mutable").join("index").join("07");
    let marker = group_path.join(lore_storage::local::fan_out::MARKER_FILENAME);
    let bucket_file = lore_storage::local::fan_out::bucket_path(&group_path, 0);
    let group = store.group[7].clone();
    std::fs::create_dir_all(marker.join("blocker")).expect("block the level marker");

    store_key(0).await;
    wait_for_delayed_flush(&group).await;
    assert!(store.group[7].dirty[0].load(atomic::Ordering::Relaxed));
    assert!(!bucket_file.exists());

    std::fs::remove_dir_all(&marker).expect("unblock the level marker");
    store_key(1).await;
    assert!(
        sweep_pending(&group).await,
        "the store into a bucket a failed delayed flush left dirty started no delayed flush"
    );
    wait_for_delayed_flush(&group).await;
    assert!(!store.group[7].dirty[0].load(atomic::Ordering::Relaxed));
    assert!(bucket_file.exists() && marker.exists());
}

/// Whether a delayed flush of `group` is scheduled or running.
async fn sweep_pending(group: &lore_storage::local::mutable_store::MutableStoreGroup) -> bool {
    let mut flush = group.flush.lock().await;
    while flush.try_join_next().is_some() {}
    group.scheduled.load(atomic::Ordering::Relaxed) || !flush.is_empty()
}

/// Wait until no delayed flush of `group` is pending, for at most 10 s.
async fn wait_for_delayed_flush(group: &lore_storage::local::mutable_store::MutableStoreGroup) {
    for _ in 0..1000 {
        if !sweep_pending(group).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the delayed flush did not finish");
}

/// A fan-out commit that fails after writing its `level.pending` is one the next open's recovery
/// completes, renaming its `.new` files over whatever is there. Until a flush rolls it forward, no
/// bucket file is written outside it: here a group's first commit fails at one rename, and a newer
/// value in that bucket, written and flushed since, must survive the recovery rather than be
/// renamed over by the commit's older file.
#[tokio::test]
async fn a_commit_failing_after_its_pending_marker_is_rolled_forward_before_any_write() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_commit_tail_");
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        ..client_settings()
    };
    let key = |byte: u8| {
        let mut key = Hash::default();
        key.data_mut()[0] = 7;
        key.data_mut()[1] = byte;
        key
    };
    let group_path = dir.path().join("mutable").join("index").join("07");
    let level = lore_storage::local::fan_out::level_for(1, 6, 2);
    let bucket = lore_storage::local::fan_out::bucket_index_for(&key(0), level);
    let blocked = lore_storage::local::fan_out::bucket_path(&group_path, bucket);
    {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let dyn_store: Arc<dyn MutableStore> = store.clone();
        for byte in 0..6u8 {
            dyn_store
                .clone()
                .store(
                    Partition::default(),
                    key(byte),
                    Hash::from_u64(u64::from(byte) + 1),
                    KeyType::BranchMetadata,
                )
                .await
                .expect("store succeeds");
        }
        std::fs::create_dir_all(&blocked).expect("block the bucket's rename");
        std::fs::write(blocked.join("blocker"), b"x").expect("block the bucket's rename");
        assert!(
            dyn_store
                .clone()
                .flush(FlushOptions::default())
                .await
                .is_err(),
            "the commit's rename fails"
        );
        std::fs::remove_dir_all(&blocked).expect("unblock the bucket");

        dyn_store
            .clone()
            .store(
                Partition::default(),
                key(0),
                Hash::from_u64(100),
                KeyType::BranchMetadata,
            )
            .await
            .expect("store the newer value");
        LocalMutableStore::flush_delayed(Arc::downgrade(&store), 7, 0).await;
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");
    }

    let reopened: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    assert_eq!(
        reopened
            .load(Partition::default(), key(0), KeyType::BranchMetadata)
            .await
            .expect("load"),
        Hash::from_u64(100),
        "recovery renamed the failed commit's older file over the value flushed since"
    );
}

/// A key in group `07` that is in bucket `bucket` once the group is at 32 buckets.
fn spread_key(bucket: u8) -> Hash {
    let mut key = Hash::default();
    key.data_mut()[0] = 7;
    key.data_mut()[1] = bucket << 3;
    key
}

/// Store a value in each of group `07`'s `buckets` at 32 buckets, `bucket + 1` in `bucket`. Six of
/// them in one bucket at level 1 fan the group out to 32 at its next flush, at a fan-out threshold
/// of 2.
async fn store_spread_keys(
    store: &Arc<dyn lore_storage::mutable_store::MutableStore>,
    buckets: std::ops::Range<u8>,
) {
    for bucket in buckets {
        store
            .clone()
            .store(
                Partition::default(),
                spread_key(bucket),
                Hash::from_u64(u64::from(bucket) + 1),
                KeyType::BranchMetadata,
            )
            .await
            .expect("store succeeds");
    }
}

/// A flush dropped part way through a fan-out commit, as a caller giving up on it drops it, leaves
/// the commit running to its end under the group's flush lock: once the lock is free, the group is
/// committed, and every entry reads back after a reopen. The flush here is the group's first,
/// dropped writing its second `.new` file.
#[tokio::test]
async fn a_flush_dropped_while_its_commit_writes_new_files_runs_the_commit_to_its_end() {
    use lore_storage::mutable_store::MutableStore;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_dropped_commit_");
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    {
        let local = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let store: Arc<dyn MutableStore> = local.clone();
        store_spread_keys(&store, 0..6).await;

        let first = lore_storage::test_util::pause_at(PausePoint::Serializing);
        let flushing = store.clone();
        let flush =
            lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
        first.reached.notified().await;
        let second = lore_storage::test_util::pause_at(PausePoint::Serializing);
        first.resume.notify_one();
        second.reached.notified().await;
        flush.abort();
        assert!(flush.await.is_err(), "the flush was dropped");
        second.resume.notify_one();

        let group = &local.group[7];
        drop(group.flush_lock.lock().await);
        assert_eq!(group.pending_level.load(atomic::Ordering::Relaxed), 0);
        assert_eq!(
            group.committed_level.load(atomic::Ordering::Relaxed),
            group.bucket_count.load(atomic::Ordering::Relaxed),
            "the dropped flush's commit ran to its end"
        );
        store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("the next flush");
    }

    let reopened: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    for bucket in 0..6u8 {
        assert_eq!(
            reopened
                .clone()
                .load(
                    Partition::default(),
                    spread_key(bucket),
                    KeyType::BranchMetadata
                )
                .await
                .expect("load"),
            Hash::from_u64(u64::from(bucket) + 1),
            "value in bucket {bucket} after reopening"
        );
    }
}

/// A group directory that cannot be stat'ed at open, as on a transient I/O error, fails the open
/// rather than being taken for a group never written. Taken for one, the group was laid out at the
/// initial level, where lookups missed its bucket files, and a write then committed that level
/// over them. The next open reads the group at the level it was written at.
#[tokio::test]
async fn a_group_directory_that_cannot_be_read_fails_the_open() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_group_stat_");
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let level = {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let dyn_store: Arc<dyn MutableStore> = store.clone();
        store_spread_keys(&dyn_store, 0..6).await;
        dyn_store
            .flush(FlushOptions::default())
            .await
            .expect("flush");
        store.group[7].bucket_count.load(atomic::Ordering::Relaxed)
    };
    assert!(level > 1, "the group fanned out");

    lore_storage::test_util::fail_directory_stats(1);
    assert!(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .is_err(),
        "an open that could not read a group directory went on as if the group were unwritten"
    );

    let reopened = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    assert_eq!(
        reopened.group[7]
            .bucket_count
            .load(atomic::Ordering::Relaxed),
        level
    );
    let reopened: Arc<dyn MutableStore> = reopened;
    for bucket in 0..6u8 {
        assert_eq!(
            reopened
                .clone()
                .load(
                    Partition::default(),
                    spread_key(bucket),
                    KeyType::BranchMetadata
                )
                .await
                .expect("load"),
            Hash::from_u64(u64::from(bucket) + 1),
            "value in bucket {bucket} after reopening"
        );
    }
}

/// A flush dropped once its fan-out commit's `level.pending` is on disk, before the write returned
/// to the commit, leaves the commit running to its end under the group's flush lock, so a delayed
/// bucket write, which writes the bucket file directly, waits for it. Otherwise the commit left
/// `level.pending` and a `.new` file older than that bucket file on disk, and the next open rolled
/// the commit forward over it, losing what it held since. Here the commit is the group's first.
#[tokio::test]
async fn a_flush_dropped_while_its_commit_writes_level_pending_runs_the_commit_to_its_end() {
    use lore_storage::mutable_store::MutableStore;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_dropped_pending_");
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let dyn_store: Arc<dyn MutableStore> = store.clone();
        store_spread_keys(&dyn_store, 0..6).await;

        let committing = lore_storage::test_util::pause_at(PausePoint::Committing);
        let flushing = dyn_store.clone();
        let flush =
            lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
        committing.reached.notified().await;
        flush.abort();
        assert!(flush.await.is_err(), "the flush was dropped");
        committing.resume.notify_one();

        dyn_store
            .clone()
            .store(
                Partition::default(),
                spread_key(0),
                Hash::from_u64(100),
                KeyType::BranchMetadata,
            )
            .await
            .expect("store the newer value");
        LocalMutableStore::flush_delayed(Arc::downgrade(&store), 7, 0).await;
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");
    }

    let reopened: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    for bucket in 0..6u8 {
        let expected = if bucket == 0 {
            100
        } else {
            u64::from(bucket) + 1
        };
        assert_eq!(
            reopened
                .clone()
                .load(
                    Partition::default(),
                    spread_key(bucket),
                    KeyType::BranchMetadata
                )
                .await
                .expect("load"),
            Hash::from_u64(expected),
            "value in bucket {bucket} after reopening"
        );
    }
}

/// A flush dropped part way through a fan-out commit leaves the commit running, and until it has
/// run to its end the store stays claimed and a store opened on the directory waits for it. Read
/// before the commit ends, the group is at the level the commit replaces, and what a store writes
/// at that level then sits under the marker the commit records, where no lookup finds it and the
/// next fan-out gathers it beside the key's current entry. The commit here is the group's first,
/// dropped as it takes the contents of its last `.new` file: every bucket's flag is claimed and no
/// write is in flight, so nothing but the commit itself keeps the store.
#[tokio::test]
async fn a_store_opened_while_a_dropped_flush_commits_waits_for_the_commit() {
    use lore_storage::mutable_store::MutableStore;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_open_during_commit_");
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let local = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let store: Arc<dyn MutableStore> = local.clone();
    store_spread_keys(&store, 0..6).await;
    let level = lore_storage::local::fan_out::level_for(1, 6, 2);

    let mut serializing = lore_storage::test_util::pause_at(PausePoint::Serializing);
    let flushing = store.clone();
    let flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    for _ in 1..6 {
        serializing.reached.notified().await;
        let next = lore_storage::test_util::pause_at(PausePoint::Serializing);
        serializing.resume.notify_one();
        serializing = next;
    }
    serializing.reached.notified().await;
    flush.abort();
    assert!(flush.await.is_err(), "the flush was dropped");
    let lock = local.lock.clone().expect("a disk-backed store has a lock");
    assert!(
        lock.is_held(),
        "the commit a dropped flush left running keeps the store claimed"
    );

    let mut opening = {
        let dir = dir.path().to_path_buf();
        lore_base::lore_spawn!(async move {
            LocalMutableStore::new(Some(dir), settings(), make_in_memory_immutable().await).await
        })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut opening)
            .await
            .is_err(),
        "a store opened on the directory while the commit ran"
    );
    serializing.resume.notify_one();
    let opened = opening.await.expect("open task").expect("store opens");
    assert_eq!(
        opened.group[7].bucket_count.load(atomic::Ordering::Relaxed),
        level,
        "the store opened during the commit read the group at the level the commit replaced"
    );
}

/// A flush dropped before its group flush claims the last flag it writes leaves the flocks to the
/// flags, this store's and the immutable store's, which the flags keep as well, and the group
/// flush releases both once it has written them and ended, as in the immutable store's test of
/// this name.
#[tokio::test]
async fn a_group_flush_left_by_a_dropped_flush_releases_the_flock_once_it_ends() {
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use lore_storage::mutable_store::MutableStore;
    use lore_storage::test_util::PausePoint;
    let dir = lore_base::test_util::TempDir::new("ms_release_after_dropped_flush_");
    let immutable = LocalImmutableStore::new(
        Some(dir.path().join("immutable")),
        ImmutableStoreSettings {
            flush_delay_seconds: 3600,
            ..ImmutableStoreSettings::default()
        },
    )
    .await
    .expect("immutable store opens");
    let local = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                fan_out_threshold: 2,
                flush_delay_seconds: 3600,
                ..client_settings()
            },
            immutable.clone(),
        )
        .await
        .expect("store opens"),
    );
    let store: Arc<dyn MutableStore> = local.clone();
    let locks = [&immutable.lock, &local.lock]
        .map(|lock| lock.clone().expect("a disk-backed store has a lock"));
    let span = (
        immutable.clone().hold_for_command().await.unwrap(),
        store.clone().hold_for_command().await.unwrap(),
    );
    store_spread_keys(&store, 0..6).await;
    drop(span);

    let serializing = lore_storage::test_util::pause_at(PausePoint::Serializing);
    let flushing = store.clone();
    let flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    serializing.reached.notified().await;
    flush.abort();
    assert!(flush.await.unwrap_err().is_cancelled());
    assert_eq!(
        locks.each_ref().map(|lock| lock.is_held()),
        [true, true],
        "the flags the group flush has yet to claim keep both flocks"
    );

    serializing.resume.notify_one();
    wait_for(
        async || locks.iter().all(|lock| !lock.is_held()),
        "the group flush releases both flocks once it ends",
    )
    .await;
}

/// After a group's second fan-out commit fails once its `level.pending` is on disk and the group is
/// flushed again, every entry reads back once the store reopens, and each bucket file of the group
/// holds only the entries of its own bucket at the committed level. The commit fails at the rename
/// of bucket 0, so that its level-1 file, holding every entry, stays until a roll-forward renames
/// the commit's file over it.
#[tokio::test]
async fn a_second_commit_failing_after_its_pending_marker_keeps_every_entry_in_its_bucket() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_failed_fan_out_");
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    {
        let store: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        // Bucket 0 is left empty at 32 buckets, so that only the commit replaces its file.
        store_spread_keys(&store, 1..3).await;
        store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush at level 1");
        store_spread_keys(&store, 3..7).await;
        let blocked = format_bucket_path(&dir.path().join("mutable"), 7, 0);
        let level_one = dir.path().join("level_one_bucket");
        std::fs::rename(&blocked, &level_one).expect("hold the level-1 file");
        super::fail_commit_at_rename(&blocked, || store.clone().flush(FlushOptions::default()))
            .await;
        std::fs::rename(&level_one, &blocked).expect("restore the level-1 file");

        store_spread_keys(&store, 7..10).await;
        store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("the next flush");
    }

    let reopened = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    let level = reopened.group[7]
        .committed_level
        .load(atomic::Ordering::Relaxed);
    let dyn_store: Arc<dyn MutableStore> = reopened;
    for bucket in 1..10u8 {
        assert_eq!(
            dyn_store
                .clone()
                .load(
                    Partition::default(),
                    spread_key(bucket),
                    KeyType::BranchMetadata
                )
                .await
                .expect("load"),
            Hash::from_u64(u64::from(bucket) + 1),
            "value in bucket {bucket} after reopening"
        );
    }
    assert_eq!(level, 32, "the group fanned out");
    for bucket_index in 0..BUCKET_COUNT {
        let path = format_bucket_path(&dir.path().join("mutable"), 7, bucket_index);
        if !path.exists() {
            continue;
        }
        let (_, entries, _) = MutableStoreBucket::deserialize_files(path, true)
            .await
            .expect("the bucket file reads");
        for entry in entries.iter() {
            assert_eq!(
                lore_storage::local::fan_out::bucket_index_for(&entry.key, level),
                bucket_index,
                "an entry in the file of bucket {bucket_index}"
            );
        }
    }
}

/// A delayed flush rolls a fan-out commit that failed once its `level.pending` was on disk forward
/// before it writes a bucket, and then writes the bucket.
#[tokio::test]
async fn a_delayed_flush_rolls_a_pending_commit_forward_before_it_writes_a_bucket() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_delayed_pending_");
    let settings = || MutableStoreSettings {
        fan_out_threshold: 2,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let pending = dir
        .path()
        .join("mutable")
        .join("index")
        .join("07")
        .join(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME);
    {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let dyn_store: Arc<dyn MutableStore> = store.clone();
        store_spread_keys(&dyn_store, 0..6).await;
        let blocked = format_bucket_path(&dir.path().join("mutable"), 7, 0);
        super::fail_commit_at_rename(&blocked, || {
            dyn_store.clone().flush(FlushOptions::default())
        })
        .await;

        LocalMutableStore::flush_delayed(Arc::downgrade(&store), 7, 0).await;

        let group = &store.group[7];
        assert_eq!(group.pending_level.load(atomic::Ordering::Relaxed), 0);
        assert_eq!(group.committed_level.load(atomic::Ordering::Relaxed), 32);
        assert!(!pending.exists(), "the commit left its level.pending");
        assert!(
            group
                .dirty
                .iter()
                .all(|dirty| !dirty.load(atomic::Ordering::Relaxed)),
            "a bucket was left dirty"
        );
    }

    let reopened: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    for bucket in 0..6u8 {
        assert_eq!(
            reopened
                .clone()
                .load(
                    Partition::default(),
                    spread_key(bucket),
                    KeyType::BranchMetadata
                )
                .await
                .expect("load"),
            Hash::from_u64(u64::from(bucket) + 1),
            "value in bucket {bucket} after reopening"
        );
    }
}

/// A fan-out commit removes the `.new` files it did not write before it writes `level.pending`,
/// so that recovery, which renames every `.new` file it finds, cannot give a bucket the commit
/// left empty an older commit's file.
#[tokio::test]
async fn a_fan_out_commit_removes_new_files_it_did_not_write() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_stale_new_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                fan_out_threshold: 4,
                ..client_settings()
            },
            make_in_memory_immutable().await,
        )
        .await
        .expect("store opens"),
    );
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    let key = |bucket_byte: u8| {
        let mut key = Hash::default();
        key.data_mut()[0] = 7;
        key.data_mut()[1] = bucket_byte;
        key
    };
    let store_key = |byte: u8| {
        let dyn_store = dyn_store.clone();
        async move {
            dyn_store
                .store(
                    Partition::default(),
                    key(byte),
                    Hash::from_u64(u64::from(byte) + 1),
                    KeyType::BranchMetadata,
                )
                .await
                .expect("store succeeds");
        }
    };
    for byte in [0, 1] {
        store_key(byte).await;
    }
    dyn_store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush at level 1");
    let group_path = dir.path().join("mutable").join("index").join("07");
    let stale_path = lore_storage::local::fan_out::bucket_new_path(&group_path, 5);
    std::fs::copy(group_path.join("index_00"), &stale_path).expect("an interrupted commit's file");

    for byte in [2, 3, 4] {
        store_key(byte).await;
    }
    dyn_store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush fanning out");
    assert_eq!(
        store.group[7].bucket_count.load(atomic::Ordering::Relaxed),
        32,
        "the group fanned out"
    );
    assert!(
        !stale_path.exists(),
        "the commit left a .new file it did not write for recovery to rename"
    );
}

/// Every `level` marker under a store's index directory.
fn level_markers(index_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(groups) = std::fs::read_dir(index_root) else {
        return found;
    };
    for group in groups.flatten() {
        let marker = group
            .path()
            .join(lore_storage::local::fan_out::MARKER_FILENAME);
        if marker.exists() {
            found.push(marker);
        }
    }
    found
}

/// The mutable twin of the immutable store's regression: a group persisted only by the
/// delayed flush must be reopened at the level it was written at. Left marker-less, a level-1
/// group is read back as a pre-fan-out 256-bucket layout and everything in `index_00` moves
/// out of reach — here that is branch heads and revision metadata.
///
/// Reachable when a sub-256 store is given a non-zero flush delay. The client's level-1
/// default sets the delay to 0, which stops a write scheduling the sweep, and the server
/// runs the delayed flush with groups at 256, where a missing marker reads back correctly;
/// this pins the invariant rather than leaving it to those defaults.
#[tokio::test]
async fn a_group_the_delayed_flush_persisted_reopens_at_its_written_level() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_delayed_level_");
    let partition = Partition::default();
    let index_root = dir.path().join("mutable").join("index");

    let keys = {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                client_settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            1,
            "a client store starts its groups at level 1"
        );

        let keys = store_keys(&store, partition, 32).await;
        // Persist the way the background timer does, and nothing else: no `flush`, so the
        // two-phase commit that would write the markers never runs.
        run_delayed_flush(&store).await;
        keys
    };

    let mut checked = 0;
    for group in std::fs::read_dir(&index_root)
        .expect("index dir exists")
        .flatten()
    {
        let has_bucket = std::fs::read_dir(group.path())
            .expect("group dir")
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("index_"))
            });
        if !has_bucket {
            continue;
        }
        checked += 1;
        assert_eq!(
            lore_storage::local::fan_out::read_level_marker(&group.path())
                .await
                .expect("marker readable"),
            Some(1),
            "group {} must record the level its bucket files were written at",
            group.path().display()
        );
    }
    assert!(
        checked > 0,
        "the delayed flush has to have written something"
    );

    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            client_settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    for (index, key) in keys.iter().enumerate() {
        assert_eq!(
            store.group[key.data()[0] as usize]
                .bucket_count
                .load(atomic::Ordering::Relaxed),
            1,
            "group for {key} reopened at the wrong level"
        );
        let loaded = dyn_store
            .clone()
            .load(partition, *key, KeyType::BranchMetadata)
            .await
            .unwrap_or_else(|err| {
                panic!("{key} was stored and persisted but reads back as {err:?}")
            });
        assert_eq!(loaded, Hash::from_u64(index as u64 + 1));
    }
}

/// A flush asked for `fast_flush` writes each bucket file in place, keeping the file, and one not
/// asked for it through a renamed temporary file, with and without `sync_data`. Either way it leaves
/// no temporary file, and the store reopens with every entry. The second flush is the one observed:
/// the first commits the groups' level, which renames `.new` files whatever the options.
#[tokio::test]
async fn a_flush_writes_bucket_files_in_place_only_with_fast_flush() {
    use lore_storage::mutable_store::MutableStore;
    for fast_flush in [false, true] {
        for sync_data in [false, true] {
            let dir = lore_base::test_util::TempDir::new("ms_fast_flush_");
            let partition = Partition::default();
            let root = dir.path().join("mutable").join("index");
            let mut stored = Vec::new();
            {
                let store = Arc::new(
                    LocalMutableStore::new(
                        Some(dir.path()),
                        client_settings(),
                        make_in_memory_immutable().await,
                    )
                    .await
                    .expect("store opens"),
                );
                let dyn_store: Arc<dyn MutableStore> = store.clone();
                let mut before = None;
                // The second batch's keys are in the first's groups, so it grows their files.
                for batch in [0xABu8, 0xCD] {
                    for index in 0..32u8 {
                        let mut key = Hash::default();
                        key.data_mut()[0] = index;
                        key.data_mut()[1] = batch;
                        let value = Hash::from_u64(u64::from(batch) << 8 | u64::from(index));
                        dyn_store
                            .clone()
                            .store(partition, key, value, KeyType::BranchMetadata)
                            .await
                            .expect("store succeeds");
                        stored.push((key, value));
                    }
                    dyn_store
                        .clone()
                        .flush(FlushOptions {
                            sync_data,
                            fast_flush,
                        })
                        .await
                        .expect("flush succeeds");
                    match &before {
                        None => before = Some(super::bucket_files(&root)),
                        Some(before) => super::assert_rewritten(&root, before, fast_flush),
                    }
                }
            }

            let store = Arc::new(
                LocalMutableStore::new(
                    Some(dir.path()),
                    client_settings(),
                    make_in_memory_immutable().await,
                )
                .await
                .expect("store reopens"),
            );
            let dyn_store: Arc<dyn MutableStore> = store.clone();
            for (key, value) in &stored {
                let loaded = dyn_store
                    .clone()
                    .load(partition, *key, KeyType::BranchMetadata)
                    .await
                    .unwrap_or_else(|err| {
                        panic!(
                            "fast flush {fast_flush} sync data {sync_data}: {key} reads back as \
                             {err:?}"
                        )
                    });
                assert_eq!(loaded, *value);
            }
        }
    }
}

/// A store at the flat 256-bucket layout — every legacy store, and every server store —
/// gains no markers: such a group already reads back at the level it was written at.
#[tokio::test]
async fn a_flat_layout_store_gains_no_level_markers() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_flat_level_");
    let partition = Partition::default();
    let index_root = dir.path().join("mutable").join("index");
    let settings = || MutableStoreSettings {
        initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
        ..Default::default()
    };

    let keys = {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "this store starts at the flat layout"
        );

        let keys = store_keys(&store, partition, 32).await;
        let weak = Arc::downgrade(&store);
        for group_index in 0..GROUP_COUNT {
            LocalMutableStore::flush_delayed(weak.clone(), group_index, 0).await;
        }
        keys
    };

    assert_eq!(
        level_markers(&index_root),
        Vec::<PathBuf>::new(),
        "a group already at 256 reads back at 256 without a marker"
    );

    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    for (index, key) in keys.iter().enumerate() {
        assert_eq!(
            store.group[key.data()[0] as usize]
                .bucket_count
                .load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "group for {key} reopened at the wrong level"
        );
        let loaded = dyn_store
            .clone()
            .load(partition, *key, KeyType::BranchMetadata)
            .await
            .unwrap_or_else(|err| panic!("{key} reads back as {err:?}"));
        assert_eq!(loaded, Hash::from_u64(index as u64 + 1));
    }
}

/// A group that already carries a marker keeps the level it records: the initial-level write
/// is for groups that have never had one, and must not overwrite a committed level.
#[tokio::test]
async fn a_marked_group_keeps_the_level_it_recorded() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_marked_level_");
    let partition = Partition::default();
    let index_root = dir.path().join("mutable").join("index");

    {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                client_settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let _ = store_keys(&store, partition, 8).await;
        let dyn_store: Arc<dyn MutableStore> = store.clone();
        // A real flush commits the level through the two-phase path.
        dyn_store
            .flush(FlushOptions::default())
            .await
            .expect("flush succeeds");
    }

    let before: Vec<(PathBuf, Vec<u8>)> = level_markers(&index_root)
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).expect("marker readable");
            (path, bytes)
        })
        .collect();
    assert!(
        !before.is_empty(),
        "the flush has to have committed a level"
    );

    {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                client_settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store reopens"),
        );
        let _ = store_keys(&store, partition, 16).await;
        run_delayed_flush(&store).await;
    }

    for (path, bytes) in before {
        assert_eq!(
            std::fs::read(&path).expect("marker still readable"),
            bytes,
            "marker at {} was rewritten",
            path.display()
        );
    }
}

#[test]
fn lazy_fan_out_version_is_three() {
    assert_eq!(MutableStoreVersion::LazyFanOut as u32, 3);
}

#[tokio::test]
async fn latest_version_constant_in_deserialize_path_matches_lazy_fan_out() {
    let dir = lore_base::test_util::TempDir::new("ms_latest_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, MutableStoreVersion::LazyFanOut as u32);
    let (_, _, version) = MutableStoreBucket::deserialize_files(path, false)
        .await
        .unwrap();
    assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
}

#[test]
fn mutable_store_settings_default_is_client_friendly() {
    let s = MutableStoreSettings::default();
    assert_eq!(s.flush_delay_seconds, DEFAULT_FLUSH_DELAY_SECONDS);
    assert_eq!(s.initial_fan_out_level, 1);
    assert_eq!(
        s.fan_out_threshold,
        lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT
    );
}

/// End-to-end: after a bucket file is zero-filled, the store still opens and the bucket is
/// usable for a store/load round-trip.
#[tokio::test]
async fn store_recovers_from_zero_filled_bucket_and_remains_usable() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_e2e_recover_");
    let store_path = dir.path().to_path_buf();
    let partition = Partition::default();
    let mut key = Hash::default();
    key.data_mut()[0] = 0x10;
    key.data_mut()[1] = 0xAB;
    let value = Hash::from_u64(42);

    {
        let store: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings {
                    initial_fan_out_level: 1,
                    ..Default::default()
                },
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        store
            .clone()
            .store(partition, key, value, KeyType::BranchMetadata)
            .await
            .unwrap();
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    // initial_fan_out_level=1 → bucket index is always 0; group is (typed) key[0].
    // `LocalMutableStore::new` roots the store under a `mutable/` subdirectory.
    let group_index = key.data()[0] as usize;
    let bucket_path = format_bucket_path(&store_path.join("mutable"), group_index, 0);
    assert!(
        bucket_path.exists(),
        "bucket file should exist after flush at {bucket_path:?}"
    );

    // Torn write: correct byte length, entirely zero-filled.
    let len = std::fs::metadata(&bucket_path).unwrap().len() as usize;
    std::fs::write(&bucket_path, vec![0u8; len]).unwrap();

    let store: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );

    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .unwrap();
    let reloaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .unwrap();
    assert_eq!(reloaded, value, "bucket must be usable after recovery");
}

/// A flush without syncing replaces a bucket's file rather than rewriting it, so that a process
/// stopped during the write leaves the file it last wrote, as a hard link to that file shows.
#[tokio::test]
async fn an_unsynced_flush_replaces_the_bucket_file() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_unsynced_replace_");
    let store: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let partition = Partition::default();
    let key = |index: u8| {
        let mut key = Hash::default();
        key.data_mut()[0] = 0x21;
        key.data_mut()[1] = index;
        key
    };
    let bucket_path = format_bucket_path(&dir.path().join("mutable"), 0x21, 0);
    store
        .clone()
        .store(
            partition,
            key(1),
            Hash::from_u64(1),
            KeyType::BranchMetadata,
        )
        .await
        .unwrap();
    store.clone().flush(FlushOptions::default()).await.unwrap();
    let written = std::fs::read(&bucket_path).unwrap();
    let link = dir.path().join("last_written");
    std::fs::hard_link(&bucket_path, &link).unwrap();

    store
        .clone()
        .store(
            partition,
            key(2),
            Hash::from_u64(2),
            KeyType::BranchMetadata,
        )
        .await
        .unwrap();
    store.clone().flush(FlushOptions::default()).await.unwrap();

    assert_eq!(
        std::fs::read(&link).unwrap(),
        written,
        "the file last written is left whole"
    );
    assert_ne!(
        std::fs::read(&bucket_path).unwrap(),
        written,
        "the bucket's file holds the new write"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn local_mutable_store_satisfies_conformance_battery() {
    lore_storage::mutable_conformance::verify_mutable_store(
        make_in_memory_mutable().await,
        lore_storage::mutable_conformance::Capabilities::new("LocalMutableStore"),
    )
    .await;
}

/// A local mutable store kept in memory, with default settings.
pub(crate) async fn make_in_memory_mutable() -> Arc<LocalMutableStore> {
    create(
        None::<&Path>,
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .expect("Failed to create in-memory mutable store")
}

async fn make_in_memory_immutable() -> Arc<dyn ImmutableStore> {
    lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("Failed to create in-memory immutable store")
}

#[tokio::test]
async fn store_initializes_group_bucket_count_from_settings_level_1() {
    use std::sync::atomic::Ordering;
    let store = LocalMutableStore::new(
        None::<&Path>,
        MutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    for group in store.group.iter() {
        assert_eq!(group.bucket_count.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn store_initializes_group_bucket_count_from_settings_level_256() {
    use std::sync::atomic::Ordering;
    let store = LocalMutableStore::new(
        None::<&Path>,
        MutableStoreSettings {
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ..Default::default()
        },
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    for group in store.group.iter() {
        assert_eq!(
            group.bucket_count.load(Ordering::Relaxed),
            lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX
        );
    }
}

/// **A migration store takes no claim of its own, so its caller must hold one.**
///
/// `for_migration` builds a view over an old on-disk layout with `lock: None`,
/// which means `hold` returns nothing and no epoch is advanced however much the
/// migration rewrites. Every other process on the directory would read the same
/// epoch before and after, serve the state it had, and flush that back over the
/// migrated files.
///
/// This pins the property the caller's obligation rests on: the migration store is
/// silent, so silence has to be made impossible some other way.
#[tokio::test]
async fn a_migration_store_holds_nothing_and_announces_nothing() {
    let dir = lore_base::test_util::TempDir::new("ms_migration_claim_");
    let path = dir.to_path_buf().join("mutable");
    std::fs::create_dir_all(&path).expect("store directory");

    let migrating = Arc::new(LocalMutableStore::for_migration(path.clone(), Vec::new()));
    assert!(
        migrating
            .hold(Intent::Write)
            .await
            .expect("holds")
            .is_none(),
        "a migration view has no lock, so it can claim nothing"
    );
    assert!(
        !path.join("epoch").exists(),
        "and announces nothing, however much it rewrites — which is why the caller \
         has to hold the claim across the whole migration"
    );
}

/// **A claim outlives the store object that took it.**
///
/// What the migration rests on. It takes a claim from the live store, drops that
/// store so the rewrite has sole ownership of the files, and re-creates one
/// afterwards to read the migrated data back — so the claim has to survive the
/// object. It does because the flock belongs to the directory, not to a store: the
/// store built afterwards joins the same one rather than contending with it, which
/// before that was a process blocking against itself with an unbounded wait.
#[tokio::test]
async fn a_claim_outlives_the_store_that_took_it() {
    let dir = lore_base::test_util::TempDir::new("ms_claim_outlives_");
    let settings = MutableStoreSettings::default;

    let first = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let claim = first
        .hold(Intent::Read)
        .await
        .expect("claims")
        .expect("a disk-backed store claims something");
    drop(first);

    let immutable = make_in_memory_immutable().await;
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        LocalMutableStore::new(Some(dir.to_path_buf()), settings(), immutable),
    )
    .await
    .expect("a store built while the claim is held joins it rather than waiting on it")
    .unwrap();
    let lock = second.lock.clone().expect("a disk-backed store has a lock");
    assert!(
        lock.is_held(),
        "the claim taken by the dropped store must still hold the directory"
    );

    drop(claim);
    assert!(!lock.is_held(), "and releasing it releases the directory");
}

/// **A store joins only a claim this process already holds, and without waiting.** Outside a
/// span, and for a store with nothing on disk, there is nothing to join; inside a span the join
/// is a counted guard, which keeps the flock past the span until it is dropped and covers no
/// operation.
#[tokio::test]
async fn a_store_joins_only_a_claim_it_already_holds() {
    use lore_storage::mutable_store::MutableStore;

    let in_memory = LocalMutableStore::new(
        None::<&Path>,
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    assert!(in_memory.try_join_claim().is_none());

    let dir = lore_base::test_util::TempDir::new("ms_try_join_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    assert!(
        store.try_join_claim().is_none(),
        "nothing to join outside a span"
    );

    let span = store.clone().hold_for_command().await.unwrap();
    let joined = store.try_join_claim().expect("the span's claim to join");
    drop(span);
    assert!(
        lock.is_held(),
        "the joined claim keeps the flock past the span"
    );
    assert!(
        !lock.covers(Intent::Read),
        "the joined claim covers no operation"
    );
    drop(joined);
    assert!(!lock.is_held());
}

/// **A store reloads at its first claim after its flock was released, although the epoch is
/// unchanged, unless it reuses its state across holds.** The claim records whether it reloaded.
#[tokio::test]
async fn a_released_store_reloads_at_its_next_claim_unless_it_reuses_state_across_holds() {
    for reuse_state_across_holds in [false, true] {
        let dir = lore_base::test_util::TempDir::new("ms_reuse_across_holds_");
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.to_path_buf()),
                MutableStoreSettings {
                    reuse_state_across_holds,
                    ..Default::default()
                },
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        let claim = store
            .hold(Intent::Read)
            .await
            .expect("claims")
            .expect("a disk-backed store claims something");
        assert_eq!(
            claim.is_stale(),
            !reuse_state_across_holds,
            "the store opened under a hold of its own, released since, and reloads only if it \
             does not reuse its state across holds"
        );
    }
}

/// **Stamping the version claims the store; recognising it costs nothing.**
///
/// The probe, the version read and the stamp are all decisions about on-disk state,
/// and the stamp is a write — so another process must not be rewriting it
/// underneath them. The case that recurs is a `version` file holding a value this
/// client does not recognise, which is what a newer client writes: it leaves the
/// version at `Initial`, so an older client re-stamps on every open.
///
/// The epoch is the observable. A stamp takes a write claim and advances it; an
/// open that reads a version it recognises writes nothing and must leave it alone.
#[tokio::test]
async fn stamping_the_version_claims_the_store() {
    let dir = lore_base::test_util::TempDir::new("ms_version_claim_");
    let epoch = dir.to_path_buf().join("mutable").join("epoch");

    let store = LocalMutableStore::new(
        Some(dir.to_path_buf()),
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    drop(store);

    let after_stamp = std::fs::read(&epoch).ok();
    assert!(
        after_stamp.is_some(),
        "creating the store stamped its version, which must be claimed and announced"
    );

    // Reopening reads a version it recognises, so there is nothing to stamp.
    let store = LocalMutableStore::new(
        Some(dir.to_path_buf()),
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    drop(store);

    assert_eq!(
        std::fs::read(&epoch).ok(),
        after_stamp,
        "an open that writes nothing must not invalidate every other process's state"
    );
}

/// **A store serves what another process wrote, not what it had cached.**
///
/// Two stores over one directory, which is what two processes are. The first
/// caches an empty bucket with a miss; the second stores the key and flushes. The
/// first must find it on its next operation, which it can only do by noticing the
/// epoch moved and dropping the bucket it held.
///
/// The only end-to-end exercise of [`MutableStoreGroup::invalidate`]: the
/// lock-interleave probe drives reads alone, so nothing there advances an epoch.
#[tokio::test]
async fn a_store_drops_what_another_one_changed_underneath_it() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_refresh_");
    let partition = Partition::default();
    let key = Hash::from_u64(0x5eed);
    let value = Hash::from_u64(0x1234);

    let reader: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    assert!(
        reader
            .clone()
            .load(partition, key, KeyType::BranchMetadata)
            .await
            .is_err(),
        "nothing has been written yet, and the miss is what caches the bucket"
    );

    {
        let writer: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(dir.to_path_buf()),
                MutableStoreSettings::default(),
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        writer
            .clone()
            .store(partition, key, value, KeyType::BranchMetadata)
            .await
            .unwrap();
        writer.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    assert_eq!(
        reader
            .clone()
            .load(partition, key, KeyType::BranchMetadata)
            .await
            .unwrap(),
        value,
        "the cached empty bucket must have been dropped and re-read"
    );
}

/// **A delayed flush releases the store once it has written.** Its own write claim
/// counts as a writer in flight, so declaring the store clean while that claim is
/// alive is refused, and the flock would stay held until some later flush.
#[tokio::test]
async fn a_delayed_flush_releases_the_store() {
    let dir = lore_base::test_util::TempDir::new("ms_delayed_release_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            client_settings(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store_keys(&store, Partition::default(), 2).await;
    assert!(lock.is_held(), "unflushed writes hold the store");

    run_delayed_flush(&store).await;
    assert!(!lock.is_held(), "and the sweeps that wrote them release it");
}

/// **A write keeps the store claimed until it is flushed, and a flush with nothing
/// to write announces nothing.**
#[tokio::test]
async fn a_write_holds_the_store_until_flushed_and_an_idle_flush_is_silent() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_flush_claim_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store_keys(&store, Partition::default(), 1).await;
    assert!(lock.is_held(), "an unflushed write holds the store");
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!lock.is_held(), "and the flush that settles it releases it");

    let epoch = dir.to_path_buf().join("mutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "nothing to write, nothing said"
    );
    assert!(!lock.is_held());
}

/// **A listing keeps its claim while its producers are still reading.** They go on
/// after the stream is handed back, so a claim released when `list` returns would
/// leave them reading bucket files no claim covers.
#[tokio::test]
async fn a_listing_keeps_the_store_claimed_while_it_reads() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_list_claim_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            client_settings(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store_keys(&store, Partition::default(), 1).await;
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!lock.is_held());

    // Holds the first producer that has read its group, so it is still at work.
    let listing = lore_storage::test_util::pause_at(lore_storage::test_util::PausePoint::Listing);
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store
            .clone()
            .list(Partition::default(), KeyType::BranchMetadata),
    )
    .await
    .expect("a listing hands its stream back while its producers wait")
    .unwrap();
    assert!(lock.is_held(), "a listing still reading holds its claim");

    listing.resume.notify_one();
    drop(stream);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while lock.is_held() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the producers finished and released the claim"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// **A listing started inside a span keeps the store claimed past the span** while its
/// producers read. A listing that relied on the span would leave them reading bucket files with
/// no flock once the span ends.
#[tokio::test]
async fn a_listing_keeps_the_store_claimed_past_the_span_it_started_in() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_list_span_claim_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store_keys(&store, Partition::default(), 1).await;
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!lock.is_held());

    let span = store.clone().hold_for_command().await.unwrap();
    // Holds the first producer that has read its group, so it is still at work.
    let listing = lore_storage::test_util::pause_at(lore_storage::test_util::PausePoint::Listing);
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store
            .clone()
            .list(Partition::default(), KeyType::BranchMetadata),
    )
    .await
    .expect("a listing hands its stream back while its producers wait")
    .unwrap();
    drop(span);
    assert!(
        lock.is_held(),
        "a listing still reading holds the store past the span"
    );

    listing.resume.notify_one();
    drop(stream);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while lock.is_held() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the producers stopped once the stream was dropped, and released the store"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// **A load claims to read and a compare-and-swap claims to write.** A load changes
/// nothing, so it announces nothing and keeps nothing claimed; a compare-and-swap
/// changes the store, so it announces the change and keeps the store claimed until
/// it is flushed.
#[tokio::test]
async fn a_load_announces_nothing_and_a_compare_and_swap_holds_the_store() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_load_swap_claims_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!lock.is_held());
    let epoch = dir.to_path_buf().join("mutable").join("epoch");
    let before = std::fs::read(&epoch).ok();

    let key = Hash::from([0x5au8; 32]);
    let _ = store
        .clone()
        .load(Partition::default(), key, KeyType::BranchMetadata)
        .await;
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "a load announces nothing"
    );
    assert!(!lock.is_held(), "and keeps nothing claimed");

    store
        .clone()
        .compare_and_swap(
            Partition::default(),
            key,
            Hash::default(),
            Hash::from([0xa5u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .expect("swaps");
    assert_ne!(
        std::fs::read(&epoch).ok(),
        before,
        "a compare-and-swap announces its change"
    );
    assert!(
        lock.is_held(),
        "and keeps the store claimed until it is flushed"
    );

    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!lock.is_held(), "which releases it");
}

/// **A compare-and-swap to the value already stored flags nothing.** It succeeds, and
/// leaves the bucket unflagged: a flagged bucket is rewritten by the next flush and keeps
/// the store claimed until then, over a value that did not change — which is what
/// recording a branch tip that has not moved does.
#[tokio::test]
async fn a_compare_and_swap_to_the_stored_value_flags_nothing() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_cas_unchanged_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let key = Hash::from([0x5au8; 32]);
    let value = Hash::from([0xa5u8; 32]);
    store
        .clone()
        .store(Partition::default(), key, value, KeyType::BranchMetadata)
        .await
        .unwrap();
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    let (group_index, bucket_index) = populated_bucket(&store).await;
    let flagged = || store.group[group_index].dirty[bucket_index].load(atomic::Ordering::Relaxed);
    assert!(!flagged(), "the flush wrote the stored value");

    let previous = store
        .clone()
        .compare_and_swap(
            Partition::default(),
            key,
            value,
            value,
            KeyType::BranchMetadata,
        )
        .await
        .expect("swaps");
    assert_eq!(previous, value, "the swap succeeds");
    assert!(
        !flagged(),
        "and flags nothing for a value that did not change"
    );
}

/// **A claim to rewrite announces the rewrite and keeps nothing claimed.** The version
/// migration rewrites the store's files wholesale, so the epoch has to move before it
/// does; but nothing it writes goes through a bucket a flush would clear, so a dirty
/// mark would keep the store claimed for good.
#[tokio::test]
async fn a_claim_to_rewrite_announces_it_and_keeps_nothing_claimed() {
    let dir = lore_base::test_util::TempDir::new("ms_rewrite_claim_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    assert!(!lock.is_held());
    let epoch = dir.to_path_buf().join("mutable").join("epoch");
    let before = std::fs::read(&epoch).ok();

    let claim = store
        .hold_to_rewrite()
        .await
        .expect("claims")
        .expect("a disk-backed store claims something");
    assert_ne!(
        std::fs::read(&epoch).ok(),
        before,
        "the rewrite is announced before it happens"
    );
    drop(claim);
    assert!(!lock.is_held(), "and nothing is left for a flush to clear");
}

/// **A bucket flagged again after a sweep began schedules the next sweep.** The running
/// sweep may already have written that bucket, so leaving it to that sweep would leave
/// it flagged, keeping the store claimed with nothing coming to write it.
#[tokio::test]
async fn a_bucket_flagged_while_a_sweep_runs_schedules_the_next() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_sweep_schedule_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings {
                flush_delay_seconds: 60 * 60,
                ..Default::default()
            },
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let key = Hash::from([0x5au8; 32]);
    store
        .clone()
        .store(
            Partition::default(),
            key,
            Hash::from([0xa5u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .unwrap();
    let (group_index, bucket_index) = populated_bucket(&store).await;
    let group = store.group[group_index].clone();
    assert!(
        group.scheduled.load(atomic::Ordering::Relaxed),
        "the write scheduled a sweep"
    );

    // That sweep has woken and written the bucket, and is still running.
    group.scheduled.store(false, atomic::Ordering::Relaxed);
    group.dirty[bucket_index].store(false, atomic::Ordering::Relaxed);
    let running = group.flush.lock().await.len();

    store
        .clone()
        .store(
            Partition::default(),
            key,
            Hash::from([0xb6u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .unwrap();
    assert_eq!(
        group.flush.lock().await.len(),
        running + 1,
        "a write during a running sweep schedules another"
    );
}

/// **An untyped listing takes no claim.** It lists nothing, so it has nothing to hold
/// the store for, and must not wait for another process using it.
#[tokio::test]
async fn an_untyped_listing_takes_no_claim() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_list_untyped_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    // Another process's claim: a second description of the lock file excludes this
    // process's own.
    let elsewhere = lore_base::fs::lock::FSLock::acquire_exact_path(
        &dir.to_path_buf().join("mutable").join("lock"),
    )
    .await
    .expect("takes the store's lock");

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.clone().list(Partition::default(), KeyType::Untyped),
    )
    .await
    .expect("an untyped listing does not wait for the store")
    .unwrap();
    drop(elsewhere);
}

/// **A change that fails to reach disk keeps its flag and the store's claim**, whether
/// the write was synced or not. The flag is cleared as the write begins, so a failure
/// that left it clear would never write the change, and a flush scan would find the
/// store clean and release its flock over state that is only in memory.
#[tokio::test]
async fn a_change_that_fails_to_reach_disk_keeps_its_flag_and_the_claim() {
    use lore_storage::mutable_store::MutableStore;

    for sync_data in [false, true] {
        let dir = lore_base::test_util::TempDir::new("ms_failed_change_");
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.to_path_buf()),
                client_settings(),
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        let lock = store.lock.clone().expect("a disk-backed store has a lock");
        let key = Hash::from([0x5au8; 32]);
        // Written once, so the group's level is committed and the write below takes the
        // regular path to the bucket's own file.
        store
            .clone()
            .store(
                Partition::default(),
                key,
                Hash::from([0xa5u8; 32]),
                KeyType::BranchMetadata,
            )
            .await
            .unwrap();
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
        assert!(!lock.is_held());

        let (group_index, bucket_index) = populated_bucket(&store).await;
        let root = store.path.clone().expect("a disk-backed store has a path");
        let file = format_bucket_path(&root, group_index, bucket_index);
        std::fs::remove_file(&file).expect("the bucket was written");
        std::fs::create_dir_all(file.join("occupied"))
            .expect("a directory where the bucket file goes");

        // A change, made the way every change is: under a write claim.
        drop(store.hold(Intent::Write).await.unwrap());
        let group = store.group[group_index].clone();
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

        assert!(
            store
                .clone()
                .flush(FlushOptions {
                    sync_data,
                    ..FlushOptions::default()
                })
                .await
                .is_err(),
            "sync_data {sync_data}: the bucket cannot be written"
        );
        assert!(
            group.dirty[bucket_index].load(atomic::Ordering::Relaxed),
            "sync_data {sync_data}: the change keeps its flag, to be written again"
        );
        assert!(
            lock.is_held(),
            "sync_data {sync_data}: and the store stays claimed while it is not on disk"
        );
    }
}

/// **A write inside a span keeps the flock past the span until it is flushed**, for the
/// reason the immutable store's test gives, and the flock of the immutable store beside it,
/// which a command claims first: a process keeping only this store's would claim the immutable
/// store after it at its next command, and deadlock against a process opening both. Released
/// by an immediate flush and by a delayed one alike.
#[tokio::test]
async fn a_write_inside_a_span_keeps_both_flocks_past_the_span_until_flushed() {
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use lore_storage::mutable_store::MutableStore;

    for delayed in [false, true] {
        let dir = lore_base::test_util::TempDir::new("ms_span_write_");
        let immutable = LocalImmutableStore::new(
            Some(dir.path().join("immutable")),
            ImmutableStoreSettings::default(),
        )
        .await
        .unwrap();
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.to_path_buf()),
                client_settings(),
                immutable.clone(),
            )
            .await
            .unwrap(),
        );
        let locks = [&immutable.lock, &store.lock]
            .map(|lock| lock.clone().expect("a disk-backed store has a lock"));

        // In the order a command claims them, and drops them.
        let span = (
            immutable.clone().hold_for_command().await.unwrap(),
            store.clone().hold_for_command().await.unwrap(),
        );
        store
            .clone()
            .store(
                Partition::default(),
                Hash::from([0x5au8; 32]),
                Hash::from([0xa5u8; 32]),
                KeyType::BranchMetadata,
            )
            .await
            .unwrap();
        drop(span);
        assert_eq!(
            locks.each_ref().map(|lock| lock.is_held()),
            [true, true],
            "delayed {delayed}: the bucket the write flagged keeps both flocks past the span"
        );

        if delayed {
            run_delayed_flush(&store).await;
        } else {
            store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
        }
        assert_eq!(
            locks.each_ref().map(|lock| lock.is_held()),
            [false, false],
            "delayed {delayed}: and the flush that writes it releases both"
        );
    }
}

/// **A group flush left running by a dropped flush holds its group's writes until it ends**, for
/// the reason the immutable store's test of this name gives.
#[tokio::test]
async fn a_group_flush_left_by_a_dropped_flush_holds_its_group_until_it_ends() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_dropped_flush_group_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            client_settings(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    store_keys(&store, Partition::default(), 1).await;

    let in_flight = lore_storage::test_util::hold_store_write();
    let flush = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.flush(FlushOptions::default()).await })
    };
    in_flight.issued.notified().await;
    flush.abort();
    assert!(flush.await.unwrap_err().is_cancelled());
    assert!(
        store
            .group
            .iter()
            .any(|group| group.writes.try_write().is_err()),
        "the group flush keeps its group's writes in flight"
    );

    in_flight.complete();
    wait_for(
        async || {
            store
                .group
                .iter()
                .all(|group| group.writes.try_write().is_ok())
        },
        "the group's writes go once the group flush ends",
    )
    .await;
}

/// The group and bucket index of the first bucket in `store` holding an entry.
async fn populated_bucket(store: &LocalMutableStore) -> (usize, usize) {
    for (group_index, group) in store.group.iter().enumerate() {
        for bucket_index in 0..group.bucket_count.load(atomic::Ordering::Relaxed) {
            if !group.bucket(bucket_index).read().await.entry.is_empty() {
                return (group_index, bucket_index);
            }
        }
    }
    panic!("a store populates a bucket");
}

/// **A reload takes the levels a fresh survey read and drops every flag**, for the
/// reason the immutable store's test gives.
#[tokio::test]
async fn invalidating_a_group_takes_the_surveyed_levels_and_clears_its_flags() {
    let store = LocalMutableStore::new(
        None::<std::path::PathBuf>,
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    let group = store.group[0].clone();
    let _ = group.bucket(3);
    group.dirty[3].store(true, atomic::Ordering::Relaxed);

    group.invalidate(64, 32, 7).await;

    assert!(
        !group.dirty[3].load(atomic::Ordering::Relaxed),
        "no flag survives a reload"
    );
    assert_eq!(group.bucket_count.load(atomic::Ordering::Relaxed), 64);
    assert_eq!(group.committed_level.load(atomic::Ordering::Relaxed), 32);
    assert_eq!(group.serialize_version.load(atomic::Ordering::Relaxed), 7);
}

#[tokio::test]
async fn level_1_store_and_load_round_trip() {
    use lore_storage::mutable_store::MutableStore;
    let store: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            None::<&Path>,
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let partition = Partition::default();
    let mut key = Hash::default();
    // Set bytes that, at level 256, would route to bucket 0xAB; at level 1 must still route to bucket 0.
    key.data_mut()[0] = 0x10;
    key.data_mut()[1] = 0xAB;
    let value = Hash::from_u64(42);
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .unwrap();
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .unwrap();
    assert_eq!(loaded, value);
}

/// At fan-out levels < 256 a single bucket holds entries spanning several bucket-byte
/// (`data[1]`) values, so within the bucket the full-hash sort orders entries primarily
/// by bucket byte and only secondarily by `data[2]` (the key-type byte). A binary
/// search that compares only `data[2]` can land on an entry whose bucket byte differs
/// from the target's and erroneously conclude no match exists, missing entries that
/// are actually present. The fix carves the bucket's `sorted_index` into one slice per
/// bucket-byte value before running the per-slice key-type search. This regression
/// test inserts one `Instance` and one `BranchMetadata` entry into the same bucket
/// at each level in the ladder and verifies `list(Instance)` returns the `Instance`
/// entry; phase two adds two more `Instance` entries plus a mix of filler entries
/// across the bucket's bucket-byte range and verifies all three `Instance` entries
/// are enumerated.
#[tokio::test]
async fn list_finds_typed_entries_at_each_fan_out_level() {
    use futures::StreamExt;
    use lore_storage::mutable_store::MutableStore;

    for &level in &[1usize, 32, 64, 128, 256] {
        let store: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                None::<&Path>,
                MutableStoreSettings {
                    initial_fan_out_level: level,
                    ..Default::default()
                },
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );

        let partition = Partition::default();
        let stride = 256 / level;

        // Phase 1: insert one Instance and one BranchMetadata in the same bucket and
        // verify list(Instance) finds the Instance. The simple two-entry shape is the
        // original failing case from the test_background_prune_during_clone smoke
        // flake.
        let d1_inst1 = 0u8;
        let d1_meta = if stride >= 2 { 1u8 } else { 0u8 };

        let mut k_inst1 = Hash::default();
        k_inst1.data_mut()[0] = 0x42;
        k_inst1.data_mut()[1] = d1_inst1;

        let mut k_meta = Hash::default();
        k_meta.data_mut()[0] = 0x42;
        k_meta.data_mut()[1] = d1_meta;
        if d1_inst1 == d1_meta {
            k_meta.data_mut()[3] = 1;
        }

        let v_inst1 = Hash::from_u64(1);
        let v_meta = Hash::from_u64(2);
        store
            .clone()
            .store(partition, k_inst1, v_inst1, KeyType::Instance)
            .await
            .unwrap();
        store
            .clone()
            .store(partition, k_meta, v_meta, KeyType::BranchMetadata)
            .await
            .unwrap();

        let mut stream = store
            .clone()
            .list(partition, KeyType::Instance)
            .await
            .unwrap();
        let mut found_phase1: Vec<(Hash, Hash)> = Vec::new();
        while let Some(item) = stream.next().await {
            found_phase1.push(item);
        }
        assert_eq!(
            found_phase1.len(),
            1,
            "level {level} phase 1: list(Instance) returned {} entries, expected 1",
            found_phase1.len()
        );
        assert_eq!(
            found_phase1[0].1, v_inst1,
            "level {level} phase 1: wrong value returned"
        );

        // Phase 2: insert two more Instance entries plus a mix of non-Instance entries
        // — all into the same bucket. At fan-out levels < 256 the entries take distinct
        // bucket-byte values within the single bucket's range, exercising the per-slice
        // walk over scattered Instance entries. At level 256 only one bucket-byte value
        // routes to a given bucket, so the two extras share `data[1]` with the first
        // and are differentiated via `data[5]`; this exercises the within-bucket
        // `stride == 1` fast path with multiple Instance entries packed together.
        let (d1_inst2, d1_inst3) = if stride >= 2 {
            ((stride / 2) as u8, (stride - 1) as u8)
        } else {
            (0u8, 0u8)
        };

        let mut k_inst2 = Hash::default();
        k_inst2.data_mut()[0] = 0x42;
        k_inst2.data_mut()[1] = d1_inst2;
        k_inst2.data_mut()[5] = 1;

        let mut k_inst3 = Hash::default();
        k_inst3.data_mut()[0] = 0x42;
        k_inst3.data_mut()[1] = d1_inst3;
        k_inst3.data_mut()[5] = 2;

        let v_inst2 = Hash::from_u64(11);
        let v_inst3 = Hash::from_u64(12);
        store
            .clone()
            .store(partition, k_inst2, v_inst2, KeyType::Instance)
            .await
            .unwrap();
        store
            .clone()
            .store(partition, k_inst3, v_inst3, KeyType::Instance)
            .await
            .unwrap();

        let other_kts = [
            KeyType::BranchMetadata,
            KeyType::BranchId,
            KeyType::BranchLatestPointer,
            KeyType::RepositoryMetadata,
            KeyType::RepositoryId,
        ];
        let filler_d1_max = stride.min(8);
        let mut counter: u64 = 100;
        for d1_idx in 0..filler_d1_max {
            let d1 = d1_idx as u8;
            for &kt in &other_kts {
                let mut k = Hash::default();
                k.data_mut()[0] = 0x42;
                k.data_mut()[1] = d1;
                k.data_mut()[6] = (counter & 0xff) as u8;
                k.data_mut()[7] = ((counter >> 8) & 0xff) as u8;
                store
                    .clone()
                    .store(partition, k, Hash::from_u64(counter), kt)
                    .await
                    .unwrap();
                counter += 1;
            }
        }

        // Phase 3: list(Instance) must return all three Instance entries despite the
        // filler entries scattered through the bucket.
        let mut stream = store
            .clone()
            .list(partition, KeyType::Instance)
            .await
            .unwrap();
        let mut found_phase3: Vec<Hash> = Vec::new();
        while let Some((_k, v)) = stream.next().await {
            found_phase3.push(v);
        }
        found_phase3.sort();
        let mut expected = vec![v_inst1, v_inst2, v_inst3];
        expected.sort();
        assert_eq!(
            found_phase3, expected,
            "level {level} phase 3: expected three Instance entries, got {found_phase3:?}"
        );

        // Phase 4 (fan-out levels > 1 only): populate two additional buckets — bucket 5
        // and bucket 10 — each with one Instance entry plus filler entries spanning the
        // full bucket-byte sub-range of that bucket. This exercises cross-bucket
        // enumeration AND the per-slice walk inside each non-zero bucket: at fan-out
        // levels < 256 the new buckets each hold entries with `stride` distinct
        // bucket-byte values, so finding the Instance still requires walking past
        // non-matching slices. Skipped at level 1 because only bucket 0 exists.
        if level > 1 {
            let extra_buckets = [5usize, 10usize];
            let mut extra_inst_values: Vec<Hash> = Vec::new();
            for (next_inst_value, &bucket_idx) in (13u64..).zip(extra_buckets.iter()) {
                let d1_lo = bucket_idx * stride;
                let d1_hi = d1_lo + stride;

                let mut k_inst_extra = Hash::default();
                k_inst_extra.data_mut()[0] = 0x42;
                k_inst_extra.data_mut()[1] = d1_lo as u8;
                let v_inst_extra = Hash::from_u64(next_inst_value);
                store
                    .clone()
                    .store(partition, k_inst_extra, v_inst_extra, KeyType::Instance)
                    .await
                    .unwrap();
                extra_inst_values.push(v_inst_extra);

                for d1_value in d1_lo..d1_hi {
                    for &kt in &other_kts {
                        let mut k = Hash::default();
                        k.data_mut()[0] = 0x42;
                        k.data_mut()[1] = d1_value as u8;
                        k.data_mut()[6] = (counter & 0xff) as u8;
                        k.data_mut()[7] = ((counter >> 8) & 0xff) as u8;
                        store
                            .clone()
                            .store(partition, k, Hash::from_u64(counter), kt)
                            .await
                            .unwrap();
                        counter += 1;
                    }
                }
            }

            let mut stream = store
                .clone()
                .list(partition, KeyType::Instance)
                .await
                .unwrap();
            let mut found_phase4: Vec<Hash> = Vec::new();
            while let Some((_k, v)) = stream.next().await {
                found_phase4.push(v);
            }
            found_phase4.sort();
            let mut expected = vec![v_inst1, v_inst2, v_inst3];
            expected.extend(extra_inst_values);
            expected.sort();
            assert_eq!(
                found_phase4,
                expected,
                "level {level} phase 4: expected {} Instance entries across multiple \
                 buckets, got {found_phase4:?}",
                expected.len()
            );
        }
    }
}
