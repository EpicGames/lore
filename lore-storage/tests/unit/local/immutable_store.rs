// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use bytes::Bytes;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentFlags;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::PackfileSize;
use lore_storage::Partition;
use lore_storage::hash;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::StoreError;
use lore_storage::local::immutable_store::info::info_path_for_store_root;
use lore_storage::local::store_lock::Intent;
use lore_storage::store_types::StoreMatch;
use lore_storage::store_types::StoreMatchResult;
use lore_storage::test_util::PausePoint;
use lore_storage::test_util::pause_at;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

mod info;
#[cfg(feature = "oodle")]
mod oodle_migration;
mod payload_gate;

use lore_storage::FlushOptions;
use lore_storage::local::immutable_store::*;

use crate::store_fixture::assert_payloads_read_back;
use crate::store_fixture::compact_group_toward;
use crate::store_fixture::group_entries;
use crate::store_fixture::payloads_in_one_group;
use crate::store_fixture::payloads_in_two_groups;
use crate::store_fixture::payloads_of_size_in_one_group;
use crate::store_fixture::put_payload;
use crate::store_fixture::try_compact_group_toward;
use crate::store_fixture::try_put_payload;
use crate::store_fixture::wait_for;
use crate::store_fixture::wait_until;

fn write_bucket_file(path: &Path, version: u32) {
    let entry = ImmutableStoreEntry::default();
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = version;
    header.count = 1;
    let mut bytes = Vec::with_capacity(
        size_of::<ImmutableStoreHeader>() + 4 + size_of::<ImmutableStoreEntry>(),
    );
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(entry.as_bytes());
    std::fs::write(path, bytes).unwrap();
}

/// A bucket larger than [`BUCKET_HEAD_READ`] does not fit the head the composite open
/// returns, so it is loaded by scattering one vectored read straight into the chunk
/// allocations of both `GrowVec`s. Entry contents are distinct per index, so a scatter that
/// landed a chunk at the wrong offset would show up as swapped entries rather than as a
/// length mismatch.
#[tokio::test]
async fn deserialize_scatters_a_bucket_larger_than_the_head_read() {
    let dir = lore_base::test_util::TempDir::new("is_scatter_");
    let path = dir.path().join("bucket");

    let per_entry = size_of::<u32>() + size_of::<ImmutableStoreEntry>();
    let count = (BUCKET_HEAD_READ / per_entry) + 64;
    assert!(
        size_of::<ImmutableStoreHeader>() + count * per_entry > BUCKET_HEAD_READ,
        "the bucket has to exceed the head read for this to test anything"
    );

    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = count as u32;

    let mut bytes = Vec::with_capacity(size_of::<ImmutableStoreHeader>() + count * per_entry);
    bytes.extend_from_slice(header.as_bytes());
    for index in 0..count {
        bytes.extend_from_slice(&(index as u32).to_le_bytes());
    }
    for index in 0..count {
        let mut entry = ImmutableStoreEntry::default();
        entry.address.hash = Hash::from([index as u8; 32]);
        entry.data.size_content = index as u64;
        entry.data.pack_offset = index as u32;
        bytes.extend_from_slice(entry.as_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let file = ImmutableStoreBucket::deserialize_files(path).await.unwrap();
    let (sorted_index, entry) = (file.sorted_index, file.entry);

    assert_eq!(sorted_index.len(), count);
    assert_eq!(entry.len(), count);
    for index in 0..count {
        assert_eq!(sorted_index[index], index as u32, "sorted index at {index}");
        assert_eq!(
            entry[index].address.hash,
            Hash::from([index as u8; 32]),
            "entry hash at {index}"
        );
        assert_eq!(entry[index].data.size_content, index as u64);
        assert_eq!(entry[index].data.pack_offset, index as u32);
    }
}

/// Client-shaped settings: the ones a repository store is opened with in a process that retains
/// stores. A test builds a store's state under one claim and examines it under the next, so the
/// store keeps its state across holds.
fn client_settings() -> ImmutableStoreSettings {
    ImmutableStoreSettings {
        protect_local_fragment: true,
        implicit_durable_stored: false,
        reuse_state_across_holds: true,
        ..Default::default()
    }
}

/// A client store over `dir` flushing after no delay, in the background as `flush_background`
/// says.
async fn open_flushing(dir: &Path, flush_background: bool) -> Arc<LocalImmutableStore> {
    LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            flush_background,
            flush_delay_seconds: 0,
            ..client_settings()
        },
    )
    .await
    .expect("store opens")
}

/// Whether a delayed flush was pending for `address`'s group, having awaited it.
async fn awaited_flush(store: &LocalImmutableStore, address: Address) -> bool {
    store.group[address.hash.data()[0] as usize]
        .flush
        .lock()
        .await
        .join_next()
        .await
        .is_some()
}

/// A fragment of `payload` stored with `flags`.
fn stored_fragment(payload: &Bytes, flags: FragmentFlags) -> Fragment {
    Fragment {
        flags: flags.as_u32(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    }
}

/// A put of the payload over an entry that lost it restores the payload of every entry of its hash,
/// which compaction left without one, whichever of them the put lands on.
#[tokio::test]
async fn a_put_over_an_entry_without_its_payload_restores_every_entry_of_its_hash() {
    let payload = Bytes::from_static(b"content two files hold, its payload compacted away");
    let fragment = stored_fragment(&payload, FragmentFlags::PayloadStoredDurable);
    let partition = Partition::from([0x5A; 16]);
    let at = |byte: u8| Address {
        hash: hash::hash_slice(&payload),
        context: Context::from([byte; 16]),
    };
    for (first, second) in [(0x29, 0xD6), (0xD6, 0x29)] {
        let dir = lore_base::test_util::TempDir::new("is_restore_siblings_");
        let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> =
            LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
                .await
                .expect("store opens");
        for byte in [first, second] {
            store
                .clone()
                .put(partition, at(byte), fragment, Some(payload.clone()), false)
                .await
                .expect("put the payload");
        }
        let mut resume = None;
        loop {
            resume = store
                .clone()
                .compact(0, resume, false, None)
                .await
                .expect("compact");
            if resume.is_none() {
                break;
            }
        }
        for byte in [first, second] {
            assert!(store.clone().get(partition, at(byte)).await.is_err());
        }

        store
            .clone()
            .put(
                partition,
                at(second),
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put the payload again");

        for byte in [first, second] {
            let got = store
                .clone()
                .get(partition, at(byte))
                .await
                .unwrap_or_else(|err| panic!("{byte:#x} after {second:#x}: {err}"));
            assert_eq!(
                got.payload,
                Some(payload.clone()),
                "{byte:#x} after {second:#x}"
            );
        }
    }
}

/// A durable write schedules the delayed flush only with `flush_background`, so only then does
/// its entry reach the on-disk index without an explicit flush.
#[tokio::test]
async fn a_durable_write_reaches_the_index_only_with_background_flush() {
    let partition = Partition::from([9u8; 16]);
    let (address, payload) = payload_off_bucket_zero(0);
    let fragment = stored_fragment(&payload, FragmentFlags::PayloadStoredDurable);
    for flush_background in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_durable_flush_");
        {
            let store = open_flushing(dir.path(), flush_background).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store
                .put(partition, address, fragment, Some(payload.clone()), false)
                .await
                .expect("put succeeds");
            assert_eq!(awaited_flush(&store, address).await, flush_background);
        }
        let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> =
            open_flushing(dir.path(), flush_background).await;
        assert_eq!(
            store.get(partition, address).await.is_ok(),
            flush_background,
            "flush_background {flush_background}"
        );
    }
}

/// Marking a flushed local entry durable, without a payload to store, reaches the on-disk index
/// in the background only with `flush_background`.
#[tokio::test]
async fn a_durable_mark_on_a_local_entry_reaches_the_index_only_with_background_flush() {
    let partition = Partition::from([9u8; 16]);
    let (address, payload) = payload_off_bucket_zero(0);
    for flush_background in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_durable_mark_");
        {
            let store = open_flushing(dir.path(), flush_background).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store
                .clone()
                .put(
                    partition,
                    address,
                    stored_fragment(&payload, FragmentFlags::PayloadStoredLocal),
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("local put succeeds");
            assert!(awaited_flush(&store, address).await);
            dyn_store
                .put(
                    partition,
                    address,
                    stored_fragment(&payload, FragmentFlags::PayloadStoredDurable),
                    None,
                    false,
                )
                .await
                .expect("durable mark succeeds");
            assert_eq!(awaited_flush(&store, address).await, flush_background);
        }
        let found = open_flushing(dir.path(), flush_background)
            .await
            .find(partition, address, Lookup::Read)
            .await
            .expect("the local entry was flushed");
        assert_eq!(
            found.data.flags & FragmentFlags::PayloadStoredDurable
                == FragmentFlags::PayloadStoredDurable,
            flush_background,
            "flush_background {flush_background}"
        );
    }
}

/// A durable copy into another partition reaches the on-disk index in the background only with
/// `flush_background`.
#[tokio::test]
async fn a_durable_copy_reaches_the_index_only_with_background_flush() {
    let source = Partition::from([9u8; 16]);
    let destination = Partition::from([10u8; 16]);
    let (address, payload) = payload_off_bucket_zero(0);
    for flush_background in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_durable_copy_");
        {
            let store = open_flushing(dir.path(), flush_background).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store
                .clone()
                .put(
                    source,
                    address,
                    stored_fragment(&payload, FragmentFlags::PayloadStoredLocal),
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("local put succeeds");
            assert!(awaited_flush(&store, address).await);
            dyn_store
                .copy(
                    source,
                    address,
                    destination,
                    address.context,
                    CopyBehavior {
                        durable: true,
                        do_not_replicate: false,
                    },
                )
                .await
                .expect("copy succeeds");
            assert_eq!(awaited_flush(&store, address).await, flush_background);
        }
        let found = open_flushing(dir.path(), flush_background)
            .await
            .find(destination, address, Lookup::Read)
            .await
            .expect("the source entry was flushed");
        assert_eq!(
            found.partition == destination,
            flush_background,
            "flush_background {flush_background}"
        );
    }
}

/// Server-shaped settings: groups start at the full 256 buckets.
fn server_settings() -> ImmutableStoreSettings {
    ImmutableStoreSettings {
        protect_local_fragment: false,
        implicit_durable_stored: true,
        initial_fan_out_level: BUCKET_COUNT,
        ..Default::default()
    }
}

/// Store `count` fragments and return their addresses.
async fn put_fragments(
    store: &Arc<LocalImmutableStore>,
    partition: Partition,
    count: u16,
) -> Vec<Address> {
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let mut addresses = Vec::new();
    for seed in 0..count {
        let (address, payload) = payload_off_bucket_zero(seed);
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        dyn_store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put succeeds");
        addresses.push(address);
    }
    addresses
}

/// A flush returns only once a bucket write another flush claimed has landed. The claim clears
/// the bucket's mark before the write, so the group reads clean to a flush starting meanwhile,
/// which skipping it would let return with an entry acknowledged before it still unwritten: a
/// process killed then loses the entry.
#[tokio::test]
async fn a_flush_waits_for_a_bucket_write_another_flush_claimed() {
    let dir = lore_base::test_util::TempDir::new("is_flush_claimed_");
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings {
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let partition = Partition::from([9u8; 16]);
    let (first, _) = payload_off_bucket_zero(1);
    put_payload(&store, partition, first.context, b"written before", false).await;
    dyn_store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush");
    let address = put_payload(
        &store,
        partition,
        first.context,
        b"claimed meanwhile",
        false,
    )
    .await;

    let pause = pause_at(PausePoint::Serializing);
    let group_index = address.hash.data()[0] as usize;
    let delayed = lore_base::lore_spawn!(ImmutableStoreGroup::flush_delayed(
        Arc::downgrade(&store),
        group_index,
        Duration::ZERO
    ));
    pause.reached.notified().await;
    let flushing = dyn_store.clone();
    let mut flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut flush)
            .await
            .is_err(),
        "the flush returned while the claimed bucket write was still to land"
    );
    pause.resume.notify_one();
    delayed.await.expect("delayed flush");
    flush.await.expect("flush task").expect("flush");
}

/// A flush that finds a group's lock held by a bucket write reads the group once the write has
/// ended. A write that ends without landing flags the bucket again before it releases the lock, so
/// the flush writes the bucket; returned instead, it would acknowledge an entry that is only in
/// memory.
#[tokio::test]
async fn a_flush_writes_a_bucket_whose_claimed_write_did_not_land() {
    let dir = lore_base::test_util::TempDir::new("is_flush_write_failed_");
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings {
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let partition = Partition::from([9u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let address = put_payload(&store, partition, Context::default(), &payloads[0], false).await;
    let group = &store.group[group_index];

    // A write that took the lock and claimed the mark, and has not written the file.
    let writing = group.flush_lock.clone().lock_owned().await;
    assert!(group.dirty[0].swap(false, atomic::Ordering::Relaxed));
    let flushing: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let mut flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut flush)
            .await
            .is_err(),
        "the flush returned while a claimed bucket write held the group"
    );
    // The write ends without landing, which flags the bucket again before the lock goes.
    group.dirty[0].store(true, atomic::Ordering::Relaxed);
    drop(writing);
    flush.await.expect("flush task").expect("flush");

    assert_bucket_file_holds(&store, group_index, address).await;
}

/// A flush with a bucket to write skips the groups it finds clean, each read under the group's
/// flush lock. A bucket write in one of them that claimed a mark and ends without landing flags
/// the bucket again before it releases the lock; read before the lock, the group would be skipped
/// with the bucket left in memory.
#[tokio::test]
async fn a_flush_skips_no_group_whose_claimed_write_did_not_land() {
    let dir = lore_base::test_util::TempDir::new("is_flush_skip_failed_");
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings {
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let partition = Partition::from([9u8; 16]);
    let mut groups = payloads_in_two_groups(1);
    groups.sort_by_key(|(group_index, _)| *group_index);
    let [(flagged_group, flagged), (claimed_group, claimed)] = groups;
    let flagged = put_payload(&store, partition, Context::default(), &flagged[0], false).await;
    let claimed = put_payload(&store, partition, Context::default(), &claimed[0], false).await;
    let group = &store.group[claimed_group];

    // A write that took the lock of the later group and claimed the mark, and has not written
    // the file. The flush is held before it tries each group's lock, so that the write can end
    // between its reads of the two groups.
    let writing = group.flush_lock.clone().lock_owned().await;
    assert!(group.dirty[0].swap(false, atomic::Ordering::Relaxed));
    let scanning: Vec<_> = (0..=claimed_group)
        .map(|_| pause_at(PausePoint::Scanning))
        .collect();
    let flushing: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    for pause in &scanning[..claimed_group] {
        pause.reached.notified().await;
        pause.resume.notify_one();
    }
    scanning[claimed_group].reached.notified().await;
    // The write ends without landing, which flags the bucket again before the lock goes.
    group.dirty[0].store(true, atomic::Ordering::Relaxed);
    drop(writing);
    scanning[claimed_group].resume.notify_one();
    flush.await.expect("flush task").expect("flush");

    assert_bucket_file_holds(&store, flagged_group, flagged).await;
    assert_bucket_file_holds(&store, claimed_group, claimed).await;
}

/// Assert that the file of bucket 0 of group `group_index` holds `address`.
async fn assert_bucket_file_holds(
    store: &LocalImmutableStore,
    group_index: usize,
    address: Address,
) {
    let root = store.path.clone().expect("a disk-backed store has a path");
    let written =
        ImmutableStoreBucket::deserialize_files(format_bucket_path(&root, group_index, 0))
            .await
            .expect("the bucket file reads")
            .entry;
    assert!(
        written.iter().any(|entry| entry.address == address),
        "group {group_index}: the flush acknowledged an entry its bucket file does not hold"
    );
}

/// A fan-out commit removes the `.new` files it did not write before it writes `level.pending`.
/// A commit stopped before its commit point leaves its `.new` files behind, and recovery renames
/// every `.new` file it finds, so a bucket the next commit left empty would take the older file,
/// whose entries point where compaction may since have truncated and rewritten the packfile.
#[tokio::test]
async fn a_fan_out_commit_removes_new_files_it_did_not_write() {
    let dir = lore_base::test_util::TempDir::new("is_stale_new_");
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings {
            fan_out_threshold: 4,
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let partition = Partition::from([3u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    for payload in &payloads[..2] {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }
    dyn_store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush at level 1");

    let group_path = dir
        .path()
        .join("immutable")
        .join("index")
        .join(format!("{group_index:02x}"));
    let used: Vec<usize> = payloads
        .iter()
        .map(|payload| {
            lore_storage::local::fan_out::bucket_index_for(&hash::hash_slice(payload), 32)
        })
        .collect();
    let stale = (1..32)
        .find(|bucket| !used.contains(bucket))
        .expect("a bucket the fan-out leaves empty");
    let stale_path = lore_storage::local::fan_out::bucket_new_path(&group_path, stale);
    std::fs::copy(group_path.join("index_00"), &stale_path).expect("an interrupted commit's file");

    for payload in &payloads[2..] {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }
    dyn_store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush fanning out");
    assert_eq!(
        store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed),
        32,
        "the group fanned out"
    );
    assert!(
        !stale_path.exists(),
        "the commit left a .new file it did not write for recovery to rename"
    );
}

/// A flush dropped part way through a fan-out commit, as a caller giving up on it drops it, leaves
/// the commit running to its end under the group's flush lock: once the lock is free, the group is
/// committed, and every entry reads back after a reopen. The flush here is the group's first,
/// dropped writing its second `.new` file.
#[tokio::test]
async fn a_flush_dropped_while_its_commit_writes_new_files_runs_the_commit_to_its_end() {
    let dir = lore_base::test_util::TempDir::new("is_dropped_commit_");
    let settings = || ImmutableStoreSettings {
        fan_out_threshold: 4,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let partition = Partition::from([5u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(12);
    let mut stored = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
        for payload in &payloads {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }

        let first = pause_at(PausePoint::Serializing);
        let flushing = dyn_store.clone();
        let flush =
            lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
        first.reached.notified().await;
        let second = pause_at(PausePoint::Serializing);
        first.resume.notify_one();
        second.reached.notified().await;
        flush.abort();
        assert!(flush.await.is_err(), "the flush was dropped");
        second.resume.notify_one();

        let group = &store.group[group_index];
        drop(group.flush_lock.lock().await);
        assert_eq!(group.pending_level.load(atomic::Ordering::Relaxed), 0);
        assert_eq!(
            group.committed_level.load(atomic::Ordering::Relaxed),
            group.bucket_count.load(atomic::Ordering::Relaxed),
            "the dropped flush's commit ran to its end"
        );
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("the next flush");
    }

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
        .await
        .expect("store reopens");
    assert_payloads_read_back(&store, partition, stored).await;
}

/// A flush dropped once its fan-out commit's `level.pending` is on disk, before the write returned
/// to the commit, leaves the commit running to its end under the group's flush lock, so a write
/// that records the group's level and writes a bucket file directly, as an obliterate does, waits
/// for it. Otherwise the commit left `level.pending` and a `.new` file older than that bucket file
/// on disk, and the next open rolled the commit forward over the bucket file, losing what it held
/// since. Here the commit is the group's first.
#[tokio::test]
async fn a_flush_dropped_while_its_commit_writes_level_pending_runs_the_commit_to_its_end() {
    let dir = lore_base::test_util::TempDir::new("is_dropped_pending_");
    let settings = || ImmutableStoreSettings {
        fan_out_threshold: 4,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let partition = Partition::from([7u8; 16]);
    let (_, payloads) = payloads_in_one_group(5);
    let mut stored = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
        for payload in &payloads[..2] {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }

        let committing = pause_at(PausePoint::Committing);
        let flushing = dyn_store.clone();
        let flush =
            lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
        committing.reached.notified().await;
        flush.abort();
        assert!(flush.await.is_err(), "the flush was dropped");
        committing.resume.notify_one();

        for payload in &payloads[2..4] {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }
        // An obliterate writes the bucket holding its tombstone itself, durably.
        let obliterated =
            put_payload(&store, partition, Context::default(), &payloads[4], false).await;
        dyn_store
            .clone()
            .obliterate(
                partition,
                obliterated,
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate");
    }

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
        .await
        .expect("store reopens");
    assert_payloads_read_back(&store, partition, stored).await;
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
    let dir = lore_base::test_util::TempDir::new("is_open_during_commit_");
    let settings = || ImmutableStoreSettings {
        fan_out_threshold: 4,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let partition = Partition::from([9u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(12);
    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
        .await
        .expect("store opens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    for payload in &payloads {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }
    let level = lore_storage::local::fan_out::level_for(1, payloads.len(), 4);
    let written: std::collections::HashSet<usize> = payloads
        .iter()
        .map(|payload| {
            lore_storage::local::fan_out::bucket_index_for(&hash::hash_slice(payload), level)
        })
        .collect();

    let mut serializing = pause_at(PausePoint::Serializing);
    let flushing = dyn_store.clone();
    let flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    for _ in 1..written.len() {
        serializing.reached.notified().await;
        let next = pause_at(PausePoint::Serializing);
        serializing.resume.notify_one();
        serializing = next;
    }
    serializing.reached.notified().await;
    flush.abort();
    assert!(flush.await.is_err(), "the flush was dropped");
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    assert!(
        lock.is_held(),
        "the commit a dropped flush left running keeps the store claimed"
    );

    let mut opening = {
        let dir = dir.path().to_path_buf();
        lore_base::lore_spawn!(async move { LocalImmutableStore::new(Some(dir), settings()).await })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut opening)
            .await
            .is_err(),
        "a store opened on the directory while the commit ran"
    );
    serializing.resume.notify_one();
    let opened = opening.await.expect("open task").expect("store opens");
    assert_eq!(
        opened.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed),
        level,
        "the store opened during the commit read the group at the level the commit replaced"
    );
}

/// A flush dropped before its group flush claims the last flag it writes leaves the flock to the
/// flags, and the group flush releases it once it has written them and ended: the flush's claim is
/// gone, nothing is left unflushed, and no other release follows.
#[tokio::test]
async fn a_group_flush_left_by_a_dropped_flush_releases_the_flock_once_it_ends() {
    let dir = lore_base::test_util::TempDir::new("is_release_after_dropped_flush_");
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings {
            fan_out_threshold: 4,
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let (_, payloads) = payloads_in_one_group(12);
    for payload in &payloads {
        put_payload(
            &store,
            Partition::from([9u8; 16]),
            Context::default(),
            payload,
            false,
        )
        .await;
    }
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let serializing = pause_at(PausePoint::Serializing);
    let flushing: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    serializing.reached.notified().await;
    flush.abort();
    assert!(flush.await.unwrap_err().is_cancelled());
    assert!(
        lock.is_held(),
        "the flags the group flush has yet to claim keep the flock"
    );

    serializing.resume.notify_one();
    wait_for(
        async || !lock.is_held(),
        "the group flush releases the flock once it ends",
    )
    .await;
}

/// Copy the tree at `source` to `destination`: a copy of a store directory with no write in
/// flight is what a process killed then leaves on disk.
fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create the copy");
    for entry in std::fs::read_dir(source).expect("list the store") {
        let entry = entry.expect("entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy a file");
        }
    }
}

/// After a group's second fan-out commit fails once its `level.pending` is on disk, the next flush
/// rolls it forward before it writes a bucket: a store killed there reads back every entry the
/// commit held, and each bucket file of the group holds only the entries of its own bucket at the
/// committed level, as once the flush ends and the store reopens.
#[tokio::test]
async fn a_second_commit_failing_after_its_pending_marker_keeps_every_entry_in_its_bucket() {
    let dir = lore_base::test_util::TempDir::new("is_failed_fan_out_");
    let killed = lore_base::test_util::TempDir::new("is_failed_fan_out_killed_");
    let settings = || ImmutableStoreSettings {
        fan_out_threshold: 4,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let partition = Partition::from([0x4cu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(8);
    let mut stored = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
        for payload in &payloads[..2] {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush at level 1");
        for payload in &payloads[2..5] {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }
        let level = lore_storage::local::fan_out::level_for(1, stored.len(), 4);
        let bucket = stored
            .iter()
            .map(|(address, _)| {
                lore_storage::local::fan_out::bucket_index_for(&address.hash, level)
            })
            .find(|&bucket| bucket != 0)
            .expect("an entry outside bucket 0 at the fanned out level");
        let blocked = format_bucket_path(&dir.path().join("immutable"), group_index, bucket);
        super::fail_commit_at_rename(&blocked, || {
            dyn_store.clone().flush(FlushOptions::default())
        })
        .await;

        for payload in &payloads[5..] {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }
        let serializing = pause_at(PausePoint::Serializing);
        let flushing = dyn_store.clone();
        let flush =
            lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
        serializing.reached.notified().await;
        copy_tree(dir.path(), killed.path());
        serializing.resume.notify_one();
        flush
            .await
            .expect("the flush runs")
            .expect("the next flush");
    }

    for (store_path, entries) in [(killed.path(), &stored[..5]), (dir.path(), &stored[..])] {
        let store = LocalImmutableStore::new(Some(store_path.to_path_buf()), settings())
            .await
            .expect("store reopens");
        assert_payloads_read_back(&store, partition, entries.iter().cloned()).await;
        let level = store.group[group_index]
            .committed_level
            .load(atomic::Ordering::Relaxed);
        assert_eq!(level, 32, "the group fanned out");
        for bucket_index in 0..BUCKET_COUNT {
            let path = format_bucket_path(&store_path.join("immutable"), group_index, bucket_index);
            if !path.exists() {
                continue;
            }
            let file = ImmutableStoreBucket::deserialize_files(path)
                .await
                .expect("the bucket file reads");
            for entry in file.entry.iter() {
                assert_eq!(
                    lore_storage::local::fan_out::bucket_index_for(&entry.address.hash, level),
                    bucket_index,
                    "an entry in the file of bucket {bucket_index}"
                );
            }
        }
    }
}

/// A delayed flush rolls a fan-out commit that failed once its `level.pending` was on disk forward
/// before it writes a bucket, and then writes the group's buckets.
#[tokio::test]
async fn a_delayed_flush_rolls_a_pending_commit_forward_before_it_writes_a_bucket() {
    let dir = lore_base::test_util::TempDir::new("is_delayed_pending_");
    let settings = || ImmutableStoreSettings {
        fan_out_threshold: 4,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let partition = Partition::from([0x4du8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    let pending = format_bucket_path(&dir.path().join("immutable"), group_index, 0)
        .with_file_name(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME);
    let mut stored = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
        for payload in &payloads {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }
        let level = lore_storage::local::fan_out::level_for(1, payloads.len(), 4);
        let bucket = lore_storage::local::fan_out::bucket_index_for(&stored[0].0.hash, level);
        let blocked = format_bucket_path(&dir.path().join("immutable"), group_index, bucket);
        super::fail_commit_at_rename(&blocked, || {
            dyn_store.clone().flush(FlushOptions::default())
        })
        .await;

        ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), group_index, Duration::ZERO)
            .await;

        let group = &store.group[group_index];
        assert_eq!(group.pending_level.load(atomic::Ordering::Relaxed), 0);
        assert_eq!(group.committed_level.load(atomic::Ordering::Relaxed), level);
        assert!(!pending.exists(), "the commit left its level.pending");
        assert!(
            group
                .dirty
                .iter()
                .all(|dirty| !dirty.load(atomic::Ordering::Relaxed)),
            "a bucket was left dirty"
        );
    }

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
        .await
        .expect("store reopens");
    assert_payloads_read_back(&store, partition, stored).await;
}

/// An eviction in a group whose first fan-out commit is pending writes no level marker: rolling the
/// commit forward records the group's level.
#[tokio::test]
async fn an_eviction_writes_no_level_marker_while_a_commit_is_pending() {
    let dir = lore_base::test_util::TempDir::new("is_evict_pending_");
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings {
            fan_out_threshold: 4,
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let partition = Partition::from([0x4eu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    let mut addresses = vec![];
    for payload in &payloads {
        addresses.push(put_payload(&store, partition, Context::default(), payload, true).await);
    }
    let level = lore_storage::local::fan_out::level_for(1, payloads.len(), 4);
    let bucket = lore_storage::local::fan_out::bucket_index_for(&addresses[0].hash, level);
    let blocked = format_bucket_path(&dir.path().join("immutable"), group_index, bucket);
    super::fail_commit_at_rename(&blocked, || {
        dyn_store.clone().flush(FlushOptions::default())
    })
    .await;

    let (evicted, _) = store
        .clone()
        .evict_group_sized(group_index, 1, store.path.clone(), true, false)
        .await;

    assert!(evicted > 0, "nothing was evicted, so no bucket write ran");
    let marker = format_bucket_path(&dir.path().join("immutable"), group_index, 0)
        .with_file_name(lore_storage::local::fan_out::MARKER_FILENAME);
    assert!(!marker.exists(), "the eviction wrote the level marker");
    assert_eq!(
        store.group[group_index]
            .committed_level
            .load(atomic::Ordering::Relaxed),
        0
    );
}

/// A group directory that cannot be stat'ed at open, as on a transient I/O error, fails the open
/// rather than being taken for a group never written. Taken for one, the group was laid out at the
/// initial level, where lookups missed its bucket files, and a write then committed that level
/// over them. The next open reads the group at the level it was written at.
#[tokio::test]
async fn a_group_directory_that_cannot_be_read_fails_the_open() {
    let dir = lore_base::test_util::TempDir::new("is_group_stat_");
    let settings = || ImmutableStoreSettings {
        fan_out_threshold: 4,
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let partition = Partition::from([6u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(12);
    let mut stored = vec![];
    let level = {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
            .await
            .expect("store opens");
        for payload in &payloads {
            let address = put_payload(&store, partition, Context::default(), payload, false).await;
            stored.push((address, payload.clone()));
        }
        let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
        dyn_store
            .flush(FlushOptions::default())
            .await
            .expect("flush");
        store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed)
    };
    assert!(level > 1, "the group fanned out");

    lore_storage::test_util::fail_directory_stats(1);
    assert!(
        LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
            .await
            .is_err(),
        "an open that could not read a group directory went on as if the group were unwritten"
    );

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
        .await
        .expect("store reopens");
    assert_eq!(
        store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed),
        level
    );
    assert_payloads_read_back(&store, partition, stored).await;
}

/// Run the background timer's flush for every group, and nothing else.
async fn run_delayed_flush(store: &Arc<LocalImmutableStore>) {
    let weak = Arc::downgrade(store);
    for group_index in 0..GROUP_COUNT {
        ImmutableStoreGroup::flush_delayed(weak.clone(), group_index, Duration::ZERO).await;
    }
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

/// Content whose hash puts it in a bucket other than 0 once a group is at level 256, so a
/// group misread as pre-fan-out sends a lookup to a different bucket than the one holding it.
fn payload_off_bucket_zero(seed: u16) -> (Address, Bytes) {
    for salt in 0..1024u16 {
        let payload = Bytes::from(seed.to_le_bytes().repeat(32 + salt as usize));
        let hash = hash::hash_slice(payload.as_ref());
        if hash.data()[1] != 0 {
            return (
                Address {
                    hash,
                    context: Context::from([0u8; 16]),
                },
                payload,
            );
        }
    }
    panic!("no payload hashed away from bucket 0");
}

/// The delayed background flush writes bucket files without going through the two-phase
/// commit, and clears the dirty flags as it goes — so the flush that ends the command finds
/// the group clean and skips it. Left like that, the group has committed bucket files and no
/// level marker, which [`crate::local::fan_out::read_group_level`] reads back as a legacy
/// pre-fan-out layout at 256 buckets. A client group is written at level 1, where everything
/// lands in `index_00`; at 256 the hash maps somewhere else entirely and that file is never
/// opened again, so the entry is on disk and unreachable — `Address not found` with the
/// payload still sitting in the packstore.
#[tokio::test]
async fn a_group_the_delayed_flush_persisted_reopens_at_its_written_level() {
    let dir = lore_base::test_util::TempDir::new("is_delayed_level_");
    let partition = Partition::from([7u8; 16]);

    let addresses = {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            1,
            "a client store starts its groups at level 1"
        );

        let addresses = put_fragments(&store, partition, 32).await;
        // Persist exactly the way the background timer does, and nothing else: no
        // `flush`, so the two-phase commit that would write the markers never runs.
        run_delayed_flush(&store).await;
        addresses
    };

    let index_root = dir.path().join("immutable").join("index");
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

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
        .await
        .expect("store reopens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    for address in &addresses {
        let group = &store.group[address.hash.data()[0] as usize];
        assert_eq!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            1,
            "group for {address} reopened at the wrong level"
        );
        dyn_store
            .clone()
            .get(partition, *address)
            .await
            .unwrap_or_else(|err| {
                panic!("{address} was written and persisted but reads back as {err:?}")
            });
    }
}

/// A flush asked for `fast_flush` writes each bucket file in place, keeping the file, and one not
/// asked for it through a renamed temporary file, with and without `sync_data`. Either way it leaves
/// no temporary file, and the store reopens with every entry. The second flush is the one observed:
/// the first commits the groups' level, which renames `.new` files whatever the options.
#[tokio::test]
async fn a_flush_writes_bucket_files_in_place_only_with_fast_flush() {
    use lore_storage::immutable_store::ImmutableStore;
    for fast_flush in [false, true] {
        for sync_data in [false, true] {
            let dir = lore_base::test_util::TempDir::new("is_fast_flush_");
            let partition = Partition::from([7u8; 16]);
            let root = dir.path().join("immutable").join("index");
            let mut addresses = Vec::new();
            {
                let store =
                    LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
                        .await
                        .expect("store opens");
                let dyn_store: Arc<dyn ImmutableStore> = store.clone();
                let mut before = None;
                // Enough fragments per batch that the second lands in groups the first wrote.
                for seeds in [0..256u16, 256..512] {
                    for seed in seeds {
                        let (address, payload) = payload_off_bucket_zero(seed);
                        let fragment = Fragment {
                            flags: 0,
                            size_payload: payload.len() as u32,
                            size_content: payload.len() as u64,
                        };
                        dyn_store
                            .clone()
                            .put(partition, address, fragment, Some(payload), false)
                            .await
                            .expect("put succeeds");
                        addresses.push(address);
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

            let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
                .await
                .expect("store reopens");
            let dyn_store: Arc<dyn ImmutableStore> = store.clone();
            for address in &addresses {
                dyn_store
                    .clone()
                    .get(partition, *address)
                    .await
                    .unwrap_or_else(|err| {
                        panic!(
                            "fast flush {fast_flush} sync data {sync_data}: {address} reads back \
                             as {err:?}"
                        )
                    });
            }
        }
    }
}

mod oodle_migration_seed {
    use super::*;

    /// A store with fragments on disk and no info file, as one written by a binary predating
    /// the file. Reports the highest group index holding data.
    async fn store_predating_the_info_file(dir: &lore_base::test_util::TempDir) -> i32 {
        let highest = {
            let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
                .await
                .expect("store opens");
            let addresses = put_fragments(&store, Partition::from([7u8; 16]), 32).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store
                .flush(FlushOptions::SYNC_DATA)
                .await
                .expect("store flushes");
            addresses
                .iter()
                .map(|address| address.hash.data()[0] as i32)
                .max()
                .expect("fragments were written")
        };
        std::fs::remove_file(info_path_for_store_root(&dir.path().join("immutable")))
            .expect("the info file a newer binary wrote is removed");
        highest
    }

    /// A group that was never written holds no payload of any codec, so a store made only of
    /// them starts with nothing to migrate - and says so rather than sweeping all 256.
    #[tokio::test]
    async fn a_store_with_no_written_group_has_nothing_to_migrate() {
        let dir = lore_base::test_util::TempDir::new("is_seed_fresh_");
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");

        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1
        );
    }

    /// A store written before the info file existed is seeded from the groups actually on
    /// disk, so the pass starts at the highest one holding data.
    #[cfg(feature = "oodle")]
    #[tokio::test]
    async fn a_store_predating_the_info_file_is_seeded_from_its_written_groups() {
        let dir = lore_base::test_util::TempDir::new("is_seed_existing_");
        let highest = store_predating_the_info_file(&dir).await;

        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store reopens");
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            highest
        );
    }

    /// A binary that cannot decode Oodle can do nothing about a store that holds it, so it
    /// records no work rather than a bookmark it could never act on.
    #[cfg(not(feature = "oodle"))]
    #[tokio::test]
    async fn a_binary_without_oodle_records_no_migration() {
        let dir = lore_base::test_util::TempDir::new("is_seed_no_oodle_");
        store_predating_the_info_file(&dir).await;

        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store reopens");
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1
        );
    }
}

/// A store closed before its first index flush holds packfile directories and no bucket file. It
/// reopens fan-out aware at the initial level, not at the flat layout of a store written before
/// lazy fan-out.
#[tokio::test]
async fn a_store_closed_before_its_first_index_flush_reopens_fan_out_aware() {
    let dir = lore_base::test_util::TempDir::new("is_unflushed_new_");
    let settings = || ImmutableStoreSettings {
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
            .await
            .expect("store opens");
        put_fragments(&store, Partition::from([4u8; 16]), 8).await;
    }
    let groups = std::fs::read_dir(dir.path().join("immutable").join("index"))
        .expect("the payloads made their packfile directories");
    for group in groups {
        let group = group.expect("a group directory").path();
        assert!(
            !lore_storage::local::fan_out::group_has_buckets(&group)
                .await
                .expect("the group directory lists"),
            "{} holds a bucket file before the first index flush",
            group.display()
        );
    }

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), settings())
        .await
        .expect("store reopens");
    let initial = settings().initial_fan_out_level;
    for (group_index, group) in store.group.iter().enumerate() {
        assert_eq!(
            group.serialize_version.load(atomic::Ordering::Relaxed),
            ImmutableStoreVersion::LazyFanOut as u32,
            "group {group_index:02x} reopened at the pre-fan-out version"
        );
        assert_eq!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            initial,
            "group {group_index:02x} reopened at the wrong level"
        );
    }
}

/// A store written before lazy fan-out carries no level markers and a flat 256-bucket
/// layout, which `read_group_level` reports as `PreFanOut`. Such a group already reads back
/// at the level it was written at, so nothing may start writing markers into it: that would
/// change which flush path the next flush takes on a store this version has to leave alone.
#[tokio::test]
async fn a_pre_fan_out_store_gains_no_level_markers() {
    let dir = lore_base::test_util::TempDir::new("is_prefanout_");
    let partition = Partition::from([3u8; 16]);
    let index_root = dir.path().join("immutable").join("index");

    // A group as the previous format left it: one bucket file at the pre-fan-out version,
    // no marker beside it.
    let legacy_group = index_root.join("2a");
    std::fs::create_dir_all(&legacy_group).expect("group dir");
    write_bucket_file(
        &legacy_group.join("index_10"),
        ImmutableStoreVersion::LastAccessInEntry as u32,
    );

    let addresses = {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");
        assert_eq!(
            store.group[0x2a]
                .bucket_count
                .load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "a marker-less group holding bucket files opens pre-fan-out"
        );
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "a legacy store keeps even its fresh groups at the flat layout"
        );

        let addresses = put_fragments(&store, partition, 32).await;
        run_delayed_flush(&store).await;
        addresses
    };

    assert_eq!(
        level_markers(&index_root),
        Vec::<PathBuf>::new(),
        "a pre-fan-out store must come back out with the layout it went in with"
    );

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
        .await
        .expect("store reopens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    for address in &addresses {
        let group = &store.group[address.hash.data()[0] as usize];
        assert_eq!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "group for {address} reopened at the wrong level"
        );
        dyn_store
            .clone()
            .get(partition, *address)
            .await
            .unwrap_or_else(|err| panic!("{address} reads back as {err:?}"));
    }
}

/// A server store starts its groups at the full 256 buckets, where a missing marker is read
/// back as exactly that. It gains no markers either, for the same reason.
#[tokio::test]
async fn a_server_shaped_store_gains_no_level_markers() {
    let dir = lore_base::test_util::TempDir::new("is_server_level_");
    let partition = Partition::from([9u8; 16]);
    let index_root = dir.path().join("immutable").join("index");

    let addresses = {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), server_settings())
            .await
            .expect("store opens");
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "a server store starts at the flat layout"
        );

        let addresses = put_fragments(&store, partition, 32).await;
        run_delayed_flush(&store).await;
        addresses
    };

    assert_eq!(
        level_markers(&index_root),
        Vec::<PathBuf>::new(),
        "a group already at 256 reads back at 256 without a marker"
    );

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), server_settings())
        .await
        .expect("store reopens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    for address in &addresses {
        let group = &store.group[address.hash.data()[0] as usize];
        assert_eq!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "group for {address} reopened at the wrong level"
        );
        dyn_store
            .clone()
            .get(partition, *address)
            .await
            .unwrap_or_else(|err| panic!("{address} reads back as {err:?}"));
    }
}

/// A group that already carries a marker keeps the level it records: the initial-level write
/// is for groups that have never had one, and must not overwrite a committed level.
#[tokio::test]
async fn a_marked_group_keeps_the_level_it_recorded() {
    let dir = lore_base::test_util::TempDir::new("is_marked_level_");
    let partition = Partition::from([5u8; 16]);
    let index_root = dir.path().join("immutable").join("index");

    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
        let _ = put_fragments(&store, partition, 8).await;
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
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store reopens");
        let _ = put_fragments(&store, partition, 16).await;
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
fn lazy_fan_out_version_is_five() {
    assert_eq!(ImmutableStoreVersion::LazyFanOut as u32, 5);
}

#[test]
fn format_bucket_path_is_index_group_bucket() {
    let root = Path::new("/store");
    for index in [0usize, 0xab, 255] {
        let byte = index as u8;
        assert_eq!(
            format_bucket_path(root, index, index),
            root.join("index").join(format!("{byte:02x}")).join(format!(
                "{}{byte:02x}",
                lore_storage::local::fan_out::BUCKET_FILENAME_PREFIX
            ))
        );
    }
    assert_eq!(
        format_bucket_path(root, 0x0f, 0xf0),
        Path::new("/store/index/0f/index_f0")
    );
}

#[tokio::test]
async fn deserialize_accepts_last_access_in_entry_v4() {
    let dir = lore_base::test_util::TempDir::new("is_v4_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, ImmutableStoreVersion::LastAccessInEntry as u32);
    let result = ImmutableStoreBucket::deserialize_files(path).await;
    assert!(
        result.is_ok(),
        "v4 (LastAccessInEntry) bucket should deserialize"
    );
}

#[tokio::test]
async fn deserialize_accepts_lazy_fan_out_v5() {
    let dir = lore_base::test_util::TempDir::new("is_v5_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, ImmutableStoreVersion::LazyFanOut as u32);
    let result = ImmutableStoreBucket::deserialize_files(path).await;
    assert!(result.is_ok(), "v5 (LazyFanOut) bucket should deserialize");
}

#[tokio::test]
async fn deserialize_rejects_unknown_future_version() {
    let dir = lore_base::test_util::TempDir::new("is_v100_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, 100);
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    assert!(result.is_err(), "v100 bucket should be rejected as too new");
    // Future-version files MUST be preserved on disk — recovery would clobber data
    // written by a newer binary.
    assert!(
        path.exists(),
        "future-version bucket file must be preserved, not deleted"
    );
}

/// Write a v5 bucket file whose header claims `header_count` entries but contains
/// only `actual_entries_on_disk` entry slots — the crash-mid-flush shape.
fn write_bucket_file_with_count_mismatch(
    path: &Path,
    header_count: u32,
    actual_entries_on_disk: u32,
) {
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = header_count;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(header.as_bytes());
    for i in 0..actual_entries_on_disk {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    for _ in 0..actual_entries_on_disk {
        let entry = ImmutableStoreEntry::default();
        bytes.extend_from_slice(entry.as_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn deserialize_recovers_from_bad_count_header() {
    // Mirrors the production server log shape (header.count > what file fits).
    let dir = lore_base::test_util::TempDir::new("is_badcount_");
    let path = dir.path().join("bucket");
    write_bucket_file_with_count_mismatch(&path, 670, 518);
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let BucketFile {
        sorted_index,
        entry,
        mark_dirty,
        ..
    } = result.expect("count-mismatch corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!mark_dirty);
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn deserialize_recovers_from_invalid_version() {
    // 0xFFFF is above the future-version sentinel range, so it's corruption.
    let dir = lore_base::test_util::TempDir::new("is_badver_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, 0xFFFF);
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let BucketFile {
        sorted_index,
        entry,
        ..
    } = result.expect("invalid-version corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn deserialize_recovers_from_truncated_entries() {
    let dir = lore_base::test_util::TempDir::new("is_trunc_");
    let path = dir.path().join("bucket");
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = 3;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(header.as_bytes());
    for i in 0u32..3 {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    let entry = ImmutableStoreEntry::default();
    bytes.extend_from_slice(entry.as_bytes());
    bytes.extend_from_slice(&entry.as_bytes()[..size_of::<ImmutableStoreEntry>() / 2]);
    std::fs::write(&path, bytes).unwrap();

    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let BucketFile {
        sorted_index,
        entry,
        ..
    } = result.expect("truncated-entries corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn deserialize_recovers_from_short_header() {
    // File too small to even hold the header.
    let dir = lore_base::test_util::TempDir::new("is_shorthdr_");
    let path = dir.path().join("bucket");
    std::fs::write(&path, [0u8; 4]).unwrap();
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let BucketFile {
        sorted_index,
        entry,
        ..
    } = result.expect("short-header corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn store_recovers_from_corrupt_bucket_and_remains_usable() {
    // End-to-end: corrupt a bucket file and verify the store is still usable for
    // writes and reads on that bucket. Original content is lost (expected).
    use lore_storage::options::ReadOptions;
    use lore_storage::options::WriteOptions;
    use lore_storage::read::read;
    use lore_storage::write::StoreResult;
    use lore_storage::write::write_content;

    let dir = lore_base::test_util::TempDir::new("is_e2e_recover_");
    let store_path = dir.path().to_path_buf();
    let partition = Partition::from([0x42u8; 16]);
    let context = Context::from([0x07u8; 16]);
    let payload = Bytes::from(vec![0xCDu8; 256]);

    let address = {
        let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
            Some(&store_path),
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let StoreResult { address, .. } = write_content(
            store.clone(),
            partition,
            context,
            payload.clone(),
            WriteOptions::default(),
            None,
            lore_storage::write_tracker::WriteContext::none(),
            None,
        )
        .await
        .unwrap();

        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
        address
    };

    // initial_fan_out_level=1 → bucket index is always 0; group is hash[0].
    let group_index = address.hash.data()[0] as usize;
    let bucket_path = store_path
        .join("immutable")
        .join("index")
        .join(format!("{group_index:02x}"))
        .join("index_00");
    assert!(
        bucket_path.exists(),
        "bucket file should exist after flush at {bucket_path:?}"
    );

    // Crash-mid-flush shape: header claims N entries, body is short.
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = 4096;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&[0u8; size_of::<u32>() * 4]);
    std::fs::write(&bucket_path, bytes).unwrap();

    let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
        Some(&store_path),
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Original content is gone (data lost), but the bucket is operational.
    let read_result = read(
        store.clone(),
        partition,
        address,
        None,
        ReadOptions::default(),
        None,
    )
    .await;
    assert!(
        read_result.is_err(),
        "originally stored content must be reported missing after recovery"
    );

    let StoreResult {
        address: new_address,
        ..
    } = write_content(
        store.clone(),
        partition,
        context,
        payload.clone(),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(new_address, address);

    let (_fragment, bytes) = read(
        store.clone(),
        partition,
        new_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_ref());
}

#[test]
fn immutable_store_settings_default_includes_fan_out_fields() {
    let s = ImmutableStoreSettings::default();
    assert_eq!(s.initial_fan_out_level, 1);
    assert_eq!(
        s.fan_out_threshold,
        lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT
    );
}

#[tokio::test]
async fn store_initializes_group_bucket_count_from_settings_level_1() {
    use std::sync::atomic::Ordering;
    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
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
    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ..Default::default()
        },
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

/// A group compacted to the end reports complete, so the resume point moves past it, and is
/// rewritten down to its non-durable payloads.
#[tokio::test]
async fn group_compaction_reports_a_finished_group_complete() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_stop_report_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x0cu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(8);
    for (index, payload) in payloads.iter().enumerate() {
        put_payload(
            &store,
            partition,
            Context::default(),
            payload,
            index % 2 == 0,
        )
        .await;
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    assert!(
        compact_group(&store, group_index).await,
        "a group that ran to the end must report complete"
    );
    assert_eq!(
        store.group[group_index]
            .packstore
            .total_size()
            .await
            .unwrap(),
        4 * 4096,
        "the sweep must have rewritten the group down to its non-durable payloads"
    );
}

/// Whether each entry still points at a payload, by whether it is durable.
fn payload_kept_by_durability(entries: &[ImmutableStoreEntry]) -> Vec<(bool, bool)> {
    let durable = FragmentFlags::PayloadStoredDurable.bits();
    let mut kept: Vec<(bool, bool)> = entries
        .iter()
        .map(|entry| (entry.data.flags & durable != 0, entry.data.pack_file != 0))
        .collect();
    kept.sort_unstable();
    kept
}

/// Local payloads count toward a group's size target although they are never evicted, so a
/// group they alone keep over the target loses every durable payload and keeps every local one:
/// 8 KiB of durable payloads, under the 12 KiB target alone, beside 16 KiB of local ones.
#[tokio::test]
async fn size_eviction_counts_local_payloads_toward_the_target() {
    let dir = lore_base::test_util::TempDir::new("is_evict_local_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x0eu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(6);
    for (index, payload) in payloads.iter().enumerate() {
        put_payload(&store, partition, Context::default(), payload, index < 2).await;
    }

    let evicted = store
        .clone()
        .evict_group_sized(group_index, 3 * 4096, store.path.clone(), true, false)
        .await;

    assert_eq!(evicted, (2, 2 * 4096), "both durable payloads are evicted");
    let local = FragmentFlags::PayloadStoredLocal.bits();
    assert!(
        group_entries(&store, group_index)
            .await
            .iter()
            .all(|entry| (entry.data.pack_file != 0) == (entry.data.flags & local != 0)),
        "an entry claims a local payload exactly when it holds one"
    );
    assert_eq!(
        payload_kept_by_durability(&group_entries(&store, group_index).await),
        [(false, true); 4]
            .into_iter()
            .chain([(true, false); 2])
            .collect::<Vec<_>>(),
        "every local payload is kept"
    );
}

/// Each bucket is held to its own share of the group's target: a bucket over its share loses its
/// durable payloads, while one within its share keeps them, though the group is over its target.
/// The group holds 160 KiB over a 128 KiB target, a share of 4 KiB in each of 32 buckets.
#[tokio::test]
async fn size_eviction_leaves_a_bucket_within_its_share() {
    use lore_storage::local::fan_out::bucket_index_for;

    const BUCKET_COUNT: usize = 32;

    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            initial_fan_out_level: BUCKET_COUNT,
            ..client_settings()
        },
    )
    .await
    .unwrap();

    let partition = Partition::from([0x18u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(40);
    let mut by_bucket: HashMap<usize, Vec<&Vec<u8>>> = HashMap::new();
    for payload in &payloads {
        let bucket_index = bucket_index_for(&hash::hash_slice(payload), BUCKET_COUNT);
        by_bucket.entry(bucket_index).or_default().push(payload);
    }
    let over = *by_bucket
        .iter()
        .find(|(_, payloads)| payloads.len() > 1)
        .expect("a bucket holds two payloads")
        .0;
    let within = *by_bucket
        .iter()
        .find(|(_, payloads)| payloads.len() == 1)
        .expect("a bucket holds one payload")
        .0;
    for (bucket_index, payloads) in &by_bucket {
        for (index, payload) in payloads.iter().enumerate() {
            let durable = (*bucket_index == over || *bucket_index == within) && index == 0;
            put_payload(&store, partition, Context::default(), payload, durable).await;
        }
    }

    let evicted = store
        .clone()
        .evict_group_sized(group_index, BUCKET_COUNT * 4096, None, true, false)
        .await;

    assert_eq!(evicted, (1, 4096), "only the bucket over its share evicts");
    let kept_in = |bucket_index: usize, entries: &[ImmutableStoreEntry]| {
        payload_kept_by_durability(
            &entries
                .iter()
                .filter(|entry| bucket_index_for(&entry.address.hash, BUCKET_COUNT) == bucket_index)
                .copied()
                .collect::<Vec<_>>(),
        )
    };
    let entries = group_entries(&store, group_index).await;
    assert_eq!(
        kept_in(within, &entries),
        vec![(true, true)],
        "the bucket within its share keeps its durable payload"
    );
    assert!(
        kept_in(over, &entries).contains(&(true, false)),
        "the bucket over its share evicts its durable payload"
    );
}

/// A bucket with nothing it may evict is left unmarked, so it is not written again, though its
/// group evicts from another bucket. The target equals the durable payloads' size, and both
/// buckets exceed their share. The store is in memory, where no write after eviction clears the
/// marks.
#[tokio::test]
async fn size_eviction_leaves_a_bucket_with_nothing_evictable_clean() {
    use lore_storage::local::fan_out::bucket_index_for;

    const BUCKET_COUNT: usize = 32;
    const PAYLOAD_SIZE: usize = 64;

    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            initial_fan_out_level: BUCKET_COUNT,
            ..client_settings()
        },
    )
    .await
    .unwrap();

    let partition = Partition::from([0x1au8; 16]);
    let (group_index, payloads) = payloads_of_size_in_one_group(8 * BUCKET_COUNT, PAYLOAD_SIZE);
    let mut by_bucket: HashMap<usize, Vec<&Vec<u8>>> = HashMap::new();
    for payload in &payloads {
        let bucket_index = bucket_index_for(&hash::hash_slice(payload), BUCKET_COUNT);
        by_bucket.entry(bucket_index).or_default().push(payload);
    }
    let (&durable_bucket, durable_payloads) = by_bucket
        .iter()
        .max_by_key(|(_, payloads)| payloads.len())
        .unwrap();
    let (&local_bucket, local_payloads) = by_bucket
        .iter()
        .find(|(bucket_index, _)| **bucket_index != durable_bucket)
        .unwrap();
    for payload in durable_payloads {
        put_payload(&store, partition, Context::default(), payload, true).await;
    }
    for payload in local_payloads {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }
    let group = &store.group[group_index];
    for dirty in &group.dirty {
        dirty.store(false, atomic::Ordering::Relaxed);
    }

    let target_size = durable_payloads.len() * PAYLOAD_SIZE;
    assert!(local_payloads.len() * PAYLOAD_SIZE > target_size / BUCKET_COUNT);
    let (evicted, _) = store
        .clone()
        .evict_group_sized(group_index, target_size, None, true, false)
        .await;

    assert!(evicted > 0, "the bucket of durable payloads evicts");
    assert!(group.dirty[durable_bucket].load(atomic::Ordering::Relaxed));
    assert!(
        !group.dirty[local_bucket].load(atomic::Ordering::Relaxed),
        "the bucket of local payloads is not marked for writing"
    );
}

/// Local fragments count toward a bucket's capacity target although they are never evicted, so
/// a bucket they alone keep over the target loses its durable fragments. A capacity of 732 gives
/// each of the 256 single-bucket groups a target of 2 fragments: above the one durable fragment,
/// below the four in all.
#[tokio::test]
async fn capacity_eviction_counts_local_fragments_toward_the_target() {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(None, client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x14u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    for (index, payload) in payloads.iter().enumerate() {
        put_payload(&store, partition, Context::default(), payload, index == 0).await;
    }

    let evicted = store.clone().evict(732, false, None).await.unwrap();

    assert_eq!(evicted, 1, "the durable fragment is evicted");
    assert_eq!(
        payload_kept_by_durability(&group_entries(&store, group_index).await),
        vec![(false, true); 3],
        "every local fragment is kept"
    );
}

/// A bucket over its capacity target with nothing it may evict starts no eviction, so a store
/// that local fragments alone keep over the target reports no eviction to the command. The
/// target is 2 fragments per bucket, under the four local fragments.
#[tokio::test]
async fn capacity_eviction_with_nothing_evictable_reports_nothing() {
    use lore_storage::immutable_store::ImmutableStore;

    #[derive(Default)]
    struct EvictionSink {
        begins: AtomicUsize,
    }

    impl lore_storage::gc_event::GcEventSink for EvictionSink {
        fn eviction_begin(&self, _target_fragments: u64) {
            self.begins.fetch_add(1, atomic::Ordering::Relaxed);
        }
        fn eviction_progress(&self, _evicted: u64) {}
        fn eviction_end(&self, _total_evicted: u64) {}
        fn compaction_begin(&self, _target_bytes: u64) {}
        fn compaction_progress(&self, _compacted_bytes: u64) {}
        fn compaction_end(&self, _total_compacted_bytes: u64) {}
    }

    let store = LocalImmutableStore::new(None, client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x16u8; 16]);
    let (_group_index, payloads) = payloads_in_one_group(4);
    for payload in &payloads {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }

    let sink = Arc::new(EvictionSink::default());
    let evicted = store
        .clone()
        .evict(732, false, Some(sink.clone()))
        .await
        .unwrap();

    assert_eq!(evicted, 0);
    assert_eq!(
        sink.begins.load(atomic::Ordering::Relaxed),
        0,
        "no eviction is announced"
    );
}

/// The packfile and offset each entry points at, by hash.
fn payload_locations(entries: &[ImmutableStoreEntry]) -> Vec<(Hash, u32, u32)> {
    entries
        .iter()
        .map(|entry| {
            (
                entry.address.hash,
                entry.data.pack_file,
                entry.data.pack_offset,
            )
        })
        .collect()
}

/// Compact `group_index` of a client `store` towards a target of one byte, so that the size
/// never ends the sweep. Returns whether the group was left complete.
async fn compact_group(store: &Arc<LocalImmutableStore>, group_index: usize) -> bool {
    compact_group_toward(store, group_index, 1, true).await
}

/// Rewriting packfiles whose payloads are all still referenced reclaims nothing, so
/// compaction leaves them in place however far the group is over its target. A rewrite copies
/// payloads in hash order, so payloads stored in reverse hash order move under any rewrite, even
/// one that ends in the packfile it started from.
#[tokio::test]
async fn compaction_leaves_a_fully_referenced_group_in_place() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_skip_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x10u8; 16]);
    let (group_index, mut payloads) = payloads_in_one_group(4);
    payloads.sort_unstable_by_key(|payload| std::cmp::Reverse(hash::hash_slice(payload)));
    for payload in &payloads {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let group = &store.group[group_index];
    assert_eq!(group.packstore.sizes().await.unwrap(), vec![4 * 4096]);
    let stored_at = payload_locations(&group_entries(&store, group_index).await);

    assert!(
        compact_group(&store, group_index).await,
        "a group with nothing to reclaim is complete"
    );
    assert_eq!(
        payload_locations(&group_entries(&store, group_index).await),
        stored_at,
        "the packfile is not rewritten"
    );
}

/// A packfile holding payloads that eviction orphaned is rewritten once, down to the payloads
/// still referenced, which read back afterwards. Packfile 1 stops taking writes for its rewrite,
/// so its payloads move to a new packfile 2, which is fully referenced and not copied back.
#[tokio::test]
async fn compaction_rewrites_a_packfile_with_orphaned_payloads() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_orphaned_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x12u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let mut local = vec![];
    for (index, payload) in payloads.iter().enumerate() {
        let durable = index < 2;
        let address = put_payload(&store, partition, Context::default(), payload, durable).await;
        if !durable {
            local.push((address, payload.clone()));
        }
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    assert!(compact_group(&store, group_index).await);
    assert_eq!(
        store.group[group_index]
            .packstore
            .total_size()
            .await
            .unwrap(),
        2 * 4096,
        "the durable payloads eviction orphaned are reclaimed"
    );
    assert!(
        group_entries(&store, group_index)
            .await
            .iter()
            .filter(|entry| entry.data.pack_file != 0)
            .all(|entry| entry.data.pack_file == 2),
        "the rewritten payloads are copied once"
    );
    assert_payloads_read_back(&store, partition, local).await;
}

/// A rewrite writes the buckets it moved entries in, not every bucket of the group: in a group of
/// 256 buckets holding a few entries, every other bucket holds nothing in the packfile, and
/// writing each, synced, made one compaction pass thousands of fsyncs.
#[tokio::test]
async fn compaction_writes_only_the_buckets_it_changed() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_buckets_");
    let settings = || ImmutableStoreSettings {
        initial_fan_out_level: BUCKET_COUNT,
        ..client_settings()
    };
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();

    let partition = Partition::from([0x13u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let mut addresses = vec![];
    let mut local = vec![];
    for (index, payload) in payloads.iter().enumerate() {
        let durable = index < 2;
        let address = put_payload(&store, partition, Context::default(), payload, durable).await;
        addresses.push(address);
        if !durable {
            local.push((address, payload.clone()));
        }
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    // Compaction passes over a group with a bucket not loaded.
    store.deserialize_all_buckets().await.unwrap();

    assert!(compact_group(&store, group_index).await);
    assert_eq!(
        store.group[group_index]
            .packstore
            .total_size()
            .await
            .unwrap(),
        2 * 4096,
        "the packfile is rewritten"
    );
    let mut group_path = dir.join("immutable").join("index");
    lore_storage::local::fan_out::push_group_dir(&mut group_path, group_index);
    let bucket_files = std::fs::read_dir(&group_path)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("index_") && !name.contains('.'))
        })
        .count();
    let mut buckets: Vec<usize> = addresses
        .iter()
        .map(|address| lore_storage::local::fan_out::bucket_index_for(&address.hash, BUCKET_COUNT))
        .collect();
    buckets.sort_unstable();
    buckets.dedup();
    assert!(
        bucket_files <= buckets.len(),
        "{bucket_files} bucket files for entries in {} buckets",
        buckets.len()
    );
    assert_payloads_read_back(&store, partition, local.clone()).await;

    drop(store);
    let reopened = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();
    assert_payloads_read_back(&reopened, partition, local).await;
}

/// Store one durable and `local` non-durable payloads of 4 KiB in one group, in reverse hash
/// order so that any rewrite moves them, and compact it once eviction has orphaned the durable
/// one. Returns the group's packfile bytes, and whether the local payloads stayed where they were
/// stored.
async fn compact_with_one_orphan(dir_prefix: &str, local: usize) -> (usize, bool) {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new(dir_prefix);
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x1eu8; 16]);
    let (group_index, mut payloads) = payloads_in_one_group(1 + local);
    payloads.sort_unstable_by_key(|payload| std::cmp::Reverse(hash::hash_slice(payload)));
    for (index, payload) in payloads.iter().enumerate() {
        put_payload(&store, partition, Context::default(), payload, index == 0).await;
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let durable = FragmentFlags::PayloadStoredDurable.bits();
    let local_locations = |entries: Vec<ImmutableStoreEntry>| {
        payload_locations(
            &entries
                .into_iter()
                .filter(|entry| entry.data.flags & durable == 0)
                .collect::<Vec<_>>(),
        )
    };
    let stored_at = local_locations(group_entries(&store, group_index).await);

    assert!(compact_group(&store, group_index).await);
    (
        store.group[group_index]
            .packstore
            .total_size()
            .await
            .unwrap(),
        local_locations(group_entries(&store, group_index).await) == stored_at,
    )
}

/// One orphaned payload in eleven is under the share of unreferenced bytes that makes a rewrite
/// worth its copying, so compaction leaves the group's packfile in place.
#[tokio::test]
async fn compaction_leaves_a_group_below_the_unreferenced_threshold() {
    let (stored_size, in_place) = compact_with_one_orphan("is_compact_below_", 10).await;

    assert!(in_place, "the packfile is not rewritten");
    assert_eq!(
        stored_size,
        11 * 4096,
        "the orphaned payload is not reclaimed"
    );
}

/// One orphaned payload in ten is the share of unreferenced bytes that makes a rewrite worth its
/// copying, so compaction reclaims it.
#[tokio::test]
async fn compaction_rewrites_a_group_at_the_unreferenced_threshold() {
    let (stored_size, in_place) = compact_with_one_orphan("is_compact_at_", 9).await;

    assert!(!in_place, "the packfile is rewritten");
    assert_eq!(stored_size, 9 * 4096, "the orphaned payload is reclaimed");
}

/// A store over `dir` holding the payloads of `layout[n]` in packfile `n + 1` of their group, each
/// durable as its flag says, each packfile closed to writes to open the next, reopened as a later
/// process finds it: every packfile under the size limit takes writes. Returns the store and the
/// address of each payload.
async fn store_in_packfiles(
    dir: &Path,
    partition: Partition,
    layout: &[Vec<(&Vec<u8>, bool)>],
) -> (Arc<LocalImmutableStore>, Vec<Address>) {
    use lore_storage::immutable_store::ImmutableStore;

    let mut addresses = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        let mut group_index = 0;
        for (index, payloads) in layout.iter().enumerate() {
            if index > 0 {
                store.group[group_index]
                    .packstore
                    .stop_writes(&[index as u32])
                    .await
                    .unwrap();
            }
            for (payload, durable) in payloads {
                group_index = hash::hash_slice(payload).data()[0] as usize;
                addresses.push(
                    put_payload(&store, partition, Context::default(), payload, *durable).await,
                );
            }
        }
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    store.deserialize_all_buckets().await.unwrap();
    (store, addresses)
}

/// The packfile an entry for `address` points at.
fn packfile_of(entries: &[ImmutableStoreEntry], address: Address) -> u32 {
    entries
        .iter()
        .find(|entry| entry.address == address)
        .expect("an entry for the address")
        .data
        .pack_file
}

/// Compaction closes every packfile it rewrites before rewriting any, so a payload it copies
/// lands in a packfile it leaves alone and is copied once. Copied into packfile 2, packfile 1's
/// payload would be copied again when packfile 2 is rewritten.
#[tokio::test]
async fn compaction_copies_each_payload_once() {
    let dir = lore_base::test_util::TempDir::new("is_compact_once_");
    let partition = Partition::from([0x22u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let (store, addresses) = store_in_packfiles(
        dir.as_ref(),
        partition,
        &[
            vec![(&payloads[0], true), (&payloads[1], false)],
            vec![(&payloads[2], true), (&payloads[3], false)],
        ],
    )
    .await;

    assert!(compact_group(&store, group_index).await);

    assert_eq!(
        packfile_of(&group_entries(&store, group_index).await, addresses[1]),
        3,
        "packfile 1's payload is copied into a packfile the pass leaves alone"
    );
    assert_eq!(
        store.group[group_index]
            .packstore
            .total_size()
            .await
            .unwrap(),
        2 * 4096
    );
}

/// However many packfiles a group has, compaction rewrites only those holding unreferenced bytes:
/// every orphaned payload is reclaimed, and a fully referenced packfile keeps its payloads where
/// they are. The odd packfiles hold a durable payload for eviction to orphan, the even ones only
/// local payloads.
#[tokio::test]
async fn compaction_rewrites_only_the_packfiles_with_orphans() {
    const PACKFILE_COUNT: usize = 5;

    let dir = lore_base::test_util::TempDir::new("is_compact_many_");
    let partition = Partition::from([0x20u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(2 * PACKFILE_COUNT);
    let layout: Vec<_> = payloads
        .chunks(2)
        .enumerate()
        .map(|(index, pair)| vec![(&pair[0], index % 2 == 0), (&pair[1], false)])
        .collect();
    let (store, addresses) = store_in_packfiles(dir.as_ref(), partition, &layout).await;
    let group = &store.group[group_index];
    assert_eq!(
        group.packstore.sizes().await.unwrap(),
        vec![2 * 4096; PACKFILE_COUNT]
    );
    let untouched_at: Vec<_> = group_entries(&store, group_index)
        .await
        .into_iter()
        .filter(|entry| entry.data.pack_file % 2 == 0)
        .map(|entry| (entry.address, entry.data.pack_file, entry.data.pack_offset))
        .collect();

    assert!(compact_group(&store, group_index).await);

    let entries = group_entries(&store, group_index).await;
    for location in &untouched_at {
        assert!(
            entries.iter().any(|entry| {
                (entry.address, entry.data.pack_file, entry.data.pack_offset) == *location
            }),
            "a payload of a fully referenced packfile stays where it is"
        );
    }
    let local: Vec<_> = layout
        .iter()
        .flatten()
        .zip(&addresses)
        .filter(|((_, durable), _)| !durable)
        .collect();
    assert_eq!(
        group.packstore.total_size().await.unwrap(),
        local.len() * 4096,
        "every orphaned payload is reclaimed"
    );
    assert_payloads_read_back(
        &store,
        partition,
        local
            .into_iter()
            .map(|((payload, _), address)| (*address, (*payload).clone())),
    )
    .await;
}

/// A stop can cut eviction short, so a group compacted with a stop pending is reported
/// incomplete, which keeps the resume point at it, whatever its packfiles hold.
#[tokio::test]
async fn compaction_with_a_stop_pending_reports_incomplete() {
    let dir = lore_base::test_util::TempDir::new("is_compact_stopped_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x24u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    for (index, payload) in payloads.iter().enumerate() {
        put_payload(&store, partition, Context::default(), payload, index < 2).await;
    }

    let _stopped = GcStopRequest::raise(&store.stop_requests, false);
    assert!(!compact_group(&store, group_index).await);
}

/// A pass that ends before rewriting a packfile it closed, here because the group fell below its
/// target, opens that packfile to writes again. Both durable payloads are evicted first. Of
/// 16 KiB over a 13 KiB target, rewriting packfile 1 leaves 12 KiB. With packfile 1 then closed,
/// packfile 2 is the lowest that takes writes.
#[tokio::test]
async fn compaction_reopens_the_packfiles_it_does_not_rewrite() {
    let dir = lore_base::test_util::TempDir::new("is_compact_reopen_");
    let partition = Partition::from([0x26u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    let (store, _) = store_in_packfiles(
        dir.as_ref(),
        partition,
        &[
            vec![(&payloads[0], true), (&payloads[1], false)],
            vec![(&payloads[2], true), (&payloads[3], false)],
        ],
    )
    .await;

    store
        .clone()
        .evict_group_sized(group_index, 1, store.path.clone(), true, false)
        .await;
    assert!(compact_group_toward(&store, group_index, 13 * 1024, true).await);

    let packstore = &store.group[group_index].packstore;
    packstore.stop_writes(&[1]).await.unwrap();
    let address = put_payload(&store, partition, Context::default(), &payloads[4], false).await;
    assert_eq!(
        packfile_of(&group_entries(&store, group_index).await, address),
        2,
        "packfile 2, left unrewritten, takes writes again"
    );
}

/// Passes over a group that keeps taking batches of durable and local payloads keep its number
/// of packfiles bounded: writes fill the lowest packfile, so a pass finds orphaned payloads in
/// one packfile and copies them into another it leaves alone.
#[tokio::test]
async fn repeated_compaction_keeps_the_packfile_count_bounded() {
    const BATCH: usize = 16;

    let dir = lore_base::test_util::TempDir::new("is_compact_bounded_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x28u8; 16]);
    let (group_index, payloads) = payloads_of_size_in_one_group(10 * BATCH, 256);
    for (round, batch) in payloads.chunks(BATCH).enumerate() {
        for (index, payload) in batch.iter().enumerate() {
            put_payload(
                &store,
                partition,
                Context::default(),
                payload,
                index % 4 < 2,
            )
            .await;
        }
        assert!(compact_group(&store, group_index).await);
        let count = store.group[group_index]
            .packstore
            .sizes()
            .await
            .unwrap()
            .len();
        assert!(count <= 2, "round {round} leaves {count} packfiles");
    }
}

/// A bucket that fails to load holds none of its entries, so compaction leaves its group alone
/// rather than truncate the payloads only that bucket references. A bucket file from a newer
/// format stands in for the real one, which is set aside and then restored.
#[tokio::test]
async fn compaction_skips_a_group_with_a_bucket_that_failed_to_load() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_unloaded_");
    let partition = Partition::from([0x2au8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let mut addresses = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        for (index, payload) in payloads.iter().enumerate() {
            addresses
                .push(put_payload(&store, partition, Context::default(), payload, index < 2).await);
        }
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    let bucket_path = format_bucket_path(&dir.path().join("immutable"), group_index, 0);
    let aside = dir.path().join("bucket-aside");
    std::fs::rename(&bucket_path, &aside).unwrap();
    write_bucket_file(&bucket_path, 100);
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        assert!(store.deserialize_all_buckets().await.is_err());

        assert!(
            compact_group(&store, group_index).await,
            "the group is left for a later process"
        );
        assert_eq!(
            store.group[group_index].packstore.sizes().await.unwrap(),
            vec![4 * 4096],
            "the packfile is not rewritten"
        );
    }

    std::fs::rename(&aside, &bucket_path).unwrap();
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, addresses.into_iter().zip(payloads)).await;
}

/// A reopened store loads its index on demand, and eviction sees only the entries loaded.
/// Loading the whole index lets it see every entry. The target is 2 fragments per bucket, under
/// the four durable fragments.
#[tokio::test]
async fn loading_the_index_lets_eviction_see_every_entry() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_load_index_");
    let partition = Partition::from([0x2cu8; 16]);
    let (_group_index, payloads) = payloads_in_one_group(4);
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        for payload in &payloads {
            put_payload(&store, partition, Context::default(), payload, true).await;
        }
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_eq!(
        store.clone().evict(732, false, None).await.unwrap(),
        0,
        "no bucket is loaded to evict from"
    );

    store.clone().load_index().await.unwrap();
    assert!(
        store.clone().evict(732, false, None).await.unwrap() > 0,
        "the loaded bucket is evicted from"
    );
}

/// A payload that a local entry also holds is not evicted, though a durable entry holding it
/// sorts first: evicting the durable entry would free nothing.
#[tokio::test]
async fn size_eviction_leaves_a_payload_a_local_entry_holds() {
    let store = LocalImmutableStore::new(None, client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x2eu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(2);
    put_payload(
        &store,
        partition,
        Context::from([0x01u8; 16]),
        &payloads[0],
        true,
    )
    .await;
    put_payload(
        &store,
        partition,
        Context::from([0x02u8; 16]),
        &payloads[0],
        false,
    )
    .await;
    put_payload(&store, partition, Context::default(), &payloads[1], false).await;

    assert_eq!(
        store
            .clone()
            .evict_group_sized(group_index, 1, None, true, false)
            .await,
        (0, 0)
    );
    assert!(
        group_entries(&store, group_index)
            .await
            .iter()
            .all(|entry| entry.data.pack_file != 0),
        "every entry keeps its payload"
    );
}

/// A server store, which protects no payload, evicts every payload of a group compacted toward
/// one byte and reclaims all of its packfile bytes.
#[tokio::test]
async fn server_compaction_reclaims_the_evicted_payloads() {
    let dir = lore_base::test_util::TempDir::new("is_compact_server_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            protect_local_fragment: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let partition = Partition::from([0x30u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    for payload in &payloads {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }

    assert!(compact_group_toward(&store, group_index, 1, false).await);
    assert_eq!(
        store.group[group_index].packstore.sizes().await.unwrap(),
        vec![0]
    );
}

/// Loading the index reports a bucket that fails to load on every call, not only the first.
#[tokio::test]
async fn loading_the_index_reports_a_failed_load_each_time() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_load_failed_");
    let (group_index, payloads) = payloads_in_one_group(1);
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        put_payload(
            &store,
            Partition::from([0x32u8; 16]),
            Context::default(),
            &payloads[0],
            false,
        )
        .await;
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }
    write_bucket_file(
        &format_bucket_path(&dir.path().join("immutable"), group_index, 0),
        100,
    );

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert!(store.clone().load_index().await.is_err());
    assert!(store.clone().load_index().await.is_err());
}

/// A packfile compaction cannot empty, because a payload in it cannot be copied, is kept, with
/// the entry still pointing into it, while the payloads copied out of it read back.
#[tokio::test]
async fn compaction_keeps_a_packfile_it_cannot_empty() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_kept_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x34u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let mut local = vec![];
    for (index, payload) in payloads.iter().enumerate() {
        let address = put_payload(&store, partition, Context::default(), payload, index < 2).await;
        if index >= 2 {
            local.push((address, payload.clone()));
        }
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let mut unreadable_hash = [0xffu8; 32];
    unreadable_hash[0] = group_index as u8;
    let unreadable = ImmutableData {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: 64,
        size_content: 64,
        pack_offset: 1 << 20,
        pack_file: 1,
        last_access: 0,
    };
    {
        let mut bucket = store.group[group_index].bucket(0).write().await;
        let index = bucket.entry.len() as u32;
        bucket.entry.push(ImmutableStoreEntry {
            address: Address {
                hash: Hash::from(unreadable_hash),
                context: Context::default(),
            },
            partition,
            data: unreadable,
        });
        bucket.sorted_index.push(index);
    }

    assert!(compact_group(&store, group_index).await);

    assert_eq!(
        store.group[group_index].packstore.sizes().await.unwrap()[0],
        4 * 4096,
        "the packfile is not truncated"
    );
    assert!(
        group_entries(&store, group_index)
            .await
            .iter()
            .any(|entry| entry.data == unreadable),
        "the entry still points into it"
    );
    assert_payloads_read_back(&store, partition, local).await;
}

/// A payload two entries share counts once toward what a packfile references, as a rewrite
/// copies it once, so an orphan beside it is still reclaimed. Counted per entry, the shared
/// payload would make up for the orphaned durable one.
#[tokio::test]
async fn compaction_counts_a_shared_payload_once() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_shared_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();

    let partition = Partition::from([0x1cu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(3);
    let shared = &payloads[0];
    let mut local = vec![];
    for context in [Context::from([0x01u8; 16]), Context::from([0x02u8; 16])] {
        let address = put_payload(&store, partition, context, shared, false).await;
        local.push((address, shared.clone()));
    }
    let address = put_payload(&store, partition, Context::default(), &payloads[1], false).await;
    local.push((address, payloads[1].clone()));
    put_payload(&store, partition, Context::default(), &payloads[2], true).await;
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    assert_eq!(
        store.group[group_index].packstore.sizes().await.unwrap()[0],
        3 * 4096,
        "the two entries share one payload"
    );

    assert!(compact_group(&store, group_index).await);
    assert_eq!(
        store.group[group_index]
            .packstore
            .total_size()
            .await
            .unwrap(),
        2 * 4096,
        "the orphaned durable payload is reclaimed"
    );
    assert_payloads_read_back(&store, partition, local).await;
}

/// A step that commits to work reports one begin and owes exactly one end; a call that
/// gives up before committing reports neither, and does no work from the resume point.
#[tokio::test]
async fn compaction_reports_one_end_for_every_begin() {
    use lore_storage::immutable_store::ImmutableStore;

    #[derive(Default)]
    struct CountingSink {
        begins: AtomicUsize,
        ends: AtomicUsize,
    }

    impl lore_storage::gc_event::GcEventSink for CountingSink {
        fn eviction_begin(&self, _target_fragments: u64) {}
        fn eviction_progress(&self, _evicted: u64) {}
        fn eviction_end(&self, _total_evicted: u64) {}
        fn compaction_begin(&self, _target_bytes: u64) {
            self.begins.fetch_add(1, atomic::Ordering::Relaxed);
        }
        fn compaction_progress(&self, _compacted_bytes: u64) {}
        fn compaction_end(&self, _total_compacted_bytes: u64) {
            self.ends.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    let dir = lore_base::test_util::TempDir::new("is_sink_pairing_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    // Content, so the pass finds itself above the limit and announces a begin.
    let payload = vec![0x5au8; 4096];
    store
        .clone()
        .put(
            Partition::from([0x0du8; 16]),
            Address {
                hash: lore_storage::hash::hash_slice(&payload),
                context: Context::default(),
            },
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let sink = Arc::new(CountingSink::default());

    let resume = store
        .clone()
        .compact_packfiles(1, None, false, Some(sink.clone()))
        .await
        .unwrap()
        .expect("a step over a 256 group store leaves groups to come");

    assert_eq!(
        sink.begins.load(atomic::Ordering::Relaxed),
        1,
        "a committed step announces exactly one begin"
    );
    assert_eq!(
        sink.ends.load(atomic::Ordering::Relaxed),
        1,
        "a committed step owes an end for the begin it reported"
    );

    let _stopped = GcStopRequest::raise(&store.stop_requests, false);

    assert_eq!(
        store
            .clone()
            .compact_packfiles(1, Some(resume), false, Some(sink.clone()))
            .await
            .unwrap(),
        None,
        "a stopped call must not take another round from the resume point"
    );
    assert_eq!(
        sink.begins.load(atomic::Ordering::Relaxed),
        1,
        "a call that gives up before committing must not announce a begin"
    );
    assert_eq!(
        sink.ends.load(atomic::Ordering::Relaxed),
        1,
        "a call that gives up before committing reports neither begin nor end"
    );
}

/// A stop asks the passes in flight to give up; it is not a switch that stays off. The
/// store is shared by path, so a caller quiescing it leaves the others collecting.
#[tokio::test]
async fn a_stop_lifts_once_it_has_drained() {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(None, ImmutableStoreSettings::default())
        .await
        .unwrap();

    store.clone().stop_gc(false).await;

    assert!(
        !store.gc_stop_requested(),
        "a stop that is not terminating must lift so a shared store keeps collecting"
    );
}

/// Two callers overlap whenever handles closing on one path race each other or a
/// shutdown. The first to drain must not lift the second's request, or the second waits
/// out a whole pass instead of the pass giving up at its next packfile.
#[tokio::test]
async fn a_stop_stays_raised_while_another_is_outstanding() {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(None, ImmutableStoreSettings::default())
        .await
        .unwrap();

    {
        let _outstanding = GcStopRequest::raise(&store.stop_requests, false);
        store.clone().stop_gc(false).await;
        assert!(
            store.gc_stop_requested(),
            "a drain that completes must leave another caller's request raised"
        );
    }

    assert!(
        !store.gc_stop_requested(),
        "the outstanding request going away leaves the store collecting again"
    );
}

/// A last-access stamp far enough in the past that a resolve marks the bucket for rewrite.
const STALE_ACCESS: u64 = 1;

/// Answer the group and bucket index of the first bucket in `store` holding an entry. The
/// hash decides where a put lands, so a test that has to reach the entry it stored searches
/// rather than derives.
async fn populated_bucket(store: &Arc<LocalImmutableStore>) -> (usize, usize) {
    for (group_index, group) in store.group.iter().enumerate() {
        for (bucket_index, cell) in group.bucket.iter().enumerate() {
            if let Some(bucket) = cell.get()
                && !bucket.read().await.entry.is_empty()
            {
                return (group_index, bucket_index);
            }
        }
    }
    panic!("a put must populate a bucket");
}

/// A delayed flush far enough out that no test meets it.
const QUIET_FLUSH_DELAY_SECONDS: u64 = 60 * 60;

/// Default settings with the delayed flush pushed past any test's length, keeping the store's
/// state across holds as [`client_settings`] does. A put of a fragment not stored durably
/// schedules a sweep a few seconds out, and a test that ran long enough to meet it would have the
/// flags it asserts on flushed underneath it.
fn quiet_settings() -> ImmutableStoreSettings {
    ImmutableStoreSettings {
        flush_delay_seconds: QUIET_FLUSH_DELAY_SECONDS,
        reuse_state_across_holds: true,
        ..Default::default()
    }
}

/// Puts a non-empty directory where a bucket's file goes, so neither a write to that
/// path nor a rename over it can succeed.
fn block_bucket_file(store: &LocalImmutableStore, group_index: usize, bucket_index: usize) {
    let root = store.path.clone().expect("a disk-backed store has a path");
    let file = format_bucket_path(&root, group_index, bucket_index);
    let _ = std::fs::remove_file(&file);
    std::fs::create_dir_all(file.join("occupied")).expect("a directory where the bucket file goes");
}

/// Store one fragment in `store`, set its last-access stamp to `stamp`, and clear the dirty
/// flag of the bucket it landed in. Returns that bucket and the address naming the entry.
async fn backdated_fragment(
    store: &Arc<LocalImmutableStore>,
    stamp: u64,
) -> ((usize, usize), Partition, Address) {
    use lore_storage::immutable_store::ImmutableStore;

    let partition = Partition::from([0x11u8; 16]);
    let payload = vec![0x22u8; 128];
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::from([0x33u8; 16]),
    };
    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    store
        .clone()
        .put(
            partition,
            address,
            fragment,
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();

    let (group_index, bucket_index) = populated_bucket(store).await;
    let group = store.group[group_index].clone();
    group.bucket(bucket_index).write().await.entry[0]
        .data
        .last_access = stamp;
    group.dirty[bucket_index].store(false, atomic::Ordering::Relaxed);

    ((group_index, bucket_index), partition, address)
}

/// Resolves one backdated fragment and reports its stamp and its bucket's dirty
/// flag.
///
/// `lookup` decides which half of the stamping rule is under test: a read updates
/// the stamp and marks the bucket for flush without marking it changed, and a write
/// marks it changed.
async fn resolve_one_fragment(atime: bool, stamp: u64, lookup: Lookup) -> (u64, bool) {
    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            atime,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let ((group_index, bucket_index), partition, address) = backdated_fragment(&store, stamp).await;

    store.find(partition, address, lookup).await.unwrap();

    let group = store.group[group_index].clone();
    (
        group.bucket(bucket_index).read().await.entry[0]
            .data
            .last_access,
        group.dirty[bucket_index].load(atomic::Ordering::Relaxed),
    )
}

/// Eviction and compaction rank entries by last access, so a resolve moves the stamp to now
/// whatever its age — ranking by write time reclaims a fragment every command reads ahead of
/// one nothing has touched since it landed. A move this small rides along with whatever
/// writes the bucket next rather than rewriting it on its own.
#[tokio::test]
async fn a_small_move_advances_the_stamp_without_dirtying_the_bucket() {
    let recent = LocalImmutableStore::last_access().saturating_sub(10);

    // On a write, so that the granularity threshold is the only thing keeping the
    // bucket clean — on a read it would stay clean whatever the move.
    let (last_access, dirty) = resolve_one_fragment(true, recent, Lookup::Write).await;

    assert!(last_access > recent, "a resolve always advances the stamp");
    assert!(!dirty, "a small move must not schedule a rewrite");
}

/// A stamp that moved past the window is worth a bucket rewrite of its own.
#[tokio::test]
async fn a_stale_stamp_dirties_the_bucket_holding_it() {
    let (_last_access, dirty) = resolve_one_fragment(true, STALE_ACCESS, Lookup::Write).await;
    assert!(
        dirty,
        "a stamp this far behind, on a write, has to reach disk"
    );

    let (_last_access, dirty) = resolve_one_fragment(true, STALE_ACCESS, Lookup::Read).await;
    assert!(
        !dirty,
        "but a read does not mark it changed: a read holds no write claim, so a \
         change it recorded would be unaccounted for"
    );
}

/// **The legacy packfile upgrade runs under a claim, and only it pays for one.**
///
/// The upgrade rewrites every entry's packfile reference, writes new per-group
/// packfiles and unlinks the old pack directory. Unclaimed, two processes opening
/// the same legacy store both run it, and a third reading those packfiles sees them
/// deleted underneath it and is never told the store changed.
///
/// The epoch file is the observable: a write claim advances it, and an ordinary open
/// of a store that has its info file — which takes a read claim and never writes —
/// leaves it as it was. That the ordinary case announces nothing is half the point,
/// since a top-level `pack` directory exists only on a legacy store and no other open
/// should pay for this.
#[tokio::test]
async fn a_legacy_packfile_upgrade_runs_under_a_claim() {
    let ordinary = lore_base::test_util::TempDir::new("is_open_plain_");
    let open = || {
        LocalImmutableStore::new(
            Some(ordinary.to_path_buf()),
            ImmutableStoreSettings::default(),
        )
    };
    // The first open gives the store its info file, which is a write of its own.
    drop(open().await.unwrap());
    let epoch = ordinary.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    drop(open().await.unwrap());
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "an ordinary open reads and writes nothing, so it announces nothing"
    );

    let legacy = lore_base::test_util::TempDir::new("is_open_legacy_");
    let immutable = legacy.to_path_buf().join("immutable");
    // A top-level pack directory is what marks a store as pre-per-group. The info file
    // is there already, so the upgrade is the only thing the open writes.
    std::fs::create_dir_all(immutable.join("pack")).expect("legacy pack directory");
    lore_storage::local::immutable_store::info::write_info_file(
        &lore_storage::local::immutable_store::info::ImmutableStoreInfo::default(),
        &info_path_for_store_root(&immutable),
    )
    .await
    .expect("an info file");

    let store = LocalImmutableStore::new(
        Some(legacy.to_path_buf()),
        ImmutableStoreSettings::default(),
    )
    .await
    .unwrap();
    assert!(
        immutable.join("epoch").exists(),
        "the upgrade must claim the store, which announces itself by writing the epoch"
    );
    assert!(
        !store.lock.as_ref().unwrap().is_held(),
        "and the claim must not outlive the open that took it"
    );
}

/// **A read's access stamp marks the bucket for flush but not as changed.**
///
/// The stamp ranks fragments for eviction. Recorded on a read it has no claim
/// behind it — a read hold never marks the store dirty — so a dirtied bucket would
/// be unflushed state nothing accounts for: released over, then dropped by the next
/// reload, or worse, enough to make `note_flushed` refuse to clear so that a
/// read-heavy process holds the store against every other one for as long as it
/// runs.
///
/// On a write there is already a write claim and the epoch has already moved, so
/// keeping it costs nothing — and a `put` of content the store already has is still
/// a touch of that content, which is what the ranking is for.
#[tokio::test]
async fn a_read_stamp_leaves_the_bucket_clean_and_a_write_stamp_does_not() {
    let dir = lore_base::test_util::TempDir::new("is_atime_dirty_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();

    let ((group_index, bucket_index), partition, address) =
        backdated_fragment(&store, STALE_ACCESS).await;
    let group = store.group[group_index].clone();
    let dirty = || group.dirty[bucket_index].load(atomic::Ordering::Relaxed);
    assert!(!dirty(), "the helper leaves the bucket clean");

    store
        .find(partition, address, Lookup::Read)
        .await
        .expect("finds");
    assert!(
        !dirty(),
        "a read's stamp is not a change: it holds no write claim, so it must not \
         mark the store as having unflushed modifications"
    );
    assert!(
        group.needs_flush(bucket_index),
        "but it is worth writing — a client reads far more than it writes, so \
         ranking that ignored reads would rank on almost nothing"
    );

    // Backdate again: the first lookup moved the stamp to now, so a second would
    // not cross the granularity threshold.
    group.bucket(bucket_index).write().await.entry[0]
        .data
        .last_access = STALE_ACCESS;

    store
        .find(partition, address, Lookup::Write)
        .await
        .expect("finds");
    assert!(
        dirty(),
        "a write's stamp is worth keeping: the claim and the epoch are already paid"
    );
}

/// **A background pass that found no work leaves the store unclaimed.**
///
/// A write claim marks the store dirty before the caller writes, and only a flush
/// clears that — so a pass which evicts or compacts nothing would keep the flock
/// on a store nobody is using. The evictor and compactor run on a timer for the
/// life of the process, unattended, which is how a process that has finished with
/// a store ends up holding it against every other process indefinitely.
#[tokio::test]
async fn a_background_pass_with_nothing_to_do_releases_the_store() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_gc_claim_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    assert!(!lock.is_held(), "nothing has used the store yet");
    let before = std::fs::read(dir.to_path_buf().join("immutable").join("epoch")).ok();

    // An empty store, with caps far above anything in it: there is nothing to
    // evict and nothing to compact. (Not `usize::MAX` — the per-group target
    // multiplies the cap by a percentage before dividing, which overflows.)
    const ROOMY: usize = 1 << 30;
    store.clone().evict(ROOMY, false, None).await.unwrap();
    assert!(
        !lock.is_held(),
        "an eviction pass that evicted nothing must not keep the store claimed"
    );

    store
        .clone()
        .compact(ROOMY, None, false, None)
        .await
        .unwrap();
    assert!(
        !lock.is_held(),
        "nor must a compaction pass that compacted nothing"
    );

    // **And neither may announce a change.** Releasing the flock is not enough: a
    // write claim advances the epoch as it is taken, and an advance tells every
    // other process on this store that its entire in-memory state is invalid. These
    // passes run on a timer for the life of the process, so a pass that claims to
    // write while doing nothing makes every peer discard and re-read the store
    // several times a minute over nothing at all.
    assert_eq!(
        std::fs::read(dir.to_path_buf().join("immutable").join("epoch")).ok(),
        before,
        "a pass that changed nothing must not tell anyone it did"
    );
}

/// **A reload clears the record of having loaded, not just the contents.**
///
/// `deserialize_all_buckets` short-circuits on a store-level latch. Left set across
/// a reload — which empties every bucket and forgets every packstore — it makes the
/// call a permanent no-op, and the three callers that need every bucket loaded
/// (packfile compaction, verify, the legacy packfile upgrade) are each handed a
/// store they believe is complete and which holds nothing. Compaction in particular
/// then reads a total size of zero, decides the store is under its cap, and never
/// runs again for the life of the process.
#[tokio::test]
async fn a_reload_clears_the_record_of_having_loaded_every_bucket() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_reload_latch_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    let payload = vec![0x4du8; 128];
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::default(),
    };
    store
        .clone()
        .put(
            Partition::from([0x91u8; 16]),
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    store.deserialize_all_buckets().await.unwrap();
    assert!(
        store.deserialized_all.load(atomic::Ordering::Relaxed),
        "the store has loaded every bucket"
    );

    store.refresh().await.unwrap();

    assert!(
        !store.deserialized_all.load(atomic::Ordering::Relaxed),
        "a reload discarded those buckets, so the store must not still claim to hold them"
    );
    // And the claim is not merely reset but honest: loading again finds the fragment
    // the reload dropped, rather than short-circuiting over an empty store.
    store.deserialize_all_buckets().await.unwrap();
    let group = &store.group[address.hash.data()[0] as usize];
    let slot = lore_storage::local::fan_out::bucket_index_for(
        &address.hash,
        group.bucket_count.load(atomic::Ordering::Relaxed),
    );
    assert!(
        !group.bucket(slot).read().await.entry.is_empty(),
        "the fragment the reload dropped is loaded again"
    );
}

/// **A store reloads at its first claim after its flock was released, although the epoch is
/// unchanged, unless it reuses its state across holds.** The record of having loaded every
/// bucket, which a reload clears, says which happened.
#[tokio::test]
async fn a_released_store_reloads_at_its_next_claim_unless_it_reuses_state_across_holds() {
    use lore_storage::immutable_store::ImmutableStore;

    for reuse_state_across_holds in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_reuse_across_holds_");
        let store = LocalImmutableStore::new(
            Some(dir.to_path_buf()),
            ImmutableStoreSettings {
                reuse_state_across_holds,
                ..quiet_settings()
            },
        )
        .await
        .unwrap();
        let lock = store.lock.clone().expect("a disk-backed store has a lock");

        let span = store.clone().hold_for_command().await.unwrap();
        store.deserialize_all_buckets().await.unwrap();
        drop(span);
        assert!(!lock.is_held(), "the flock is released between the claims");

        let _claim = store.hold(Intent::Read).await.unwrap();
        assert_eq!(
            store.deserialized_all.load(atomic::Ordering::Relaxed),
            reuse_state_across_holds,
            "the store keeps what it loaded only if it reuses its state across holds"
        );
    }
}

/// **A reload empties a bucket without replacing what identifies it.**
///
/// `serialize_lock` is how two writers to one bucket exclude each other, and it is
/// held through an `Arc` clone taken before the write starts. Assigning a default
/// bucket over it hands out a *fresh* lock, so a writer holding the old clone
/// excludes against a mutex nobody else takes — the exclusion silently stops
/// existing at the moment a reload happens, which is when writers are least
/// expected to be tidy.
#[tokio::test]
async fn a_reload_keeps_the_lock_that_orders_writes_to_a_bucket() {
    let dir = lore_base::test_util::TempDir::new("is_reset_identity_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    let group = &store.group[0];
    let bucket = group.bucket(0);
    bucket
        .write()
        .await
        .entry
        .push(ImmutableStoreEntry::default());
    let before = Arc::clone(&bucket.read().await.serialize_lock);

    group.invalidate(BUCKET_COUNT, 0, 1).await;

    let after = Arc::clone(&bucket.read().await.serialize_lock);
    assert!(
        bucket.read().await.entry.is_empty(),
        "the contents must go, which is the point of a reload"
    );
    assert!(
        Arc::ptr_eq(&before, &after),
        "but the lock that orders writes to this bucket must be the same one"
    );
}

/// **A delayed flush with nothing to write claims nothing.**
///
/// It wakes long after the operation that scheduled it, usually after that
/// command's own flush has written everything. Claiming to write then would
/// advance the epoch and tell every other process on the store that its state is
/// worthless, over a sweep that writes nothing.
#[tokio::test]
async fn a_delayed_flush_with_nothing_to_write_claims_nothing() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_delayed_quiet_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let ((group_index, _), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[0].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let epoch = dir.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), group_index, Duration::ZERO).await;

    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "a sweep that writes nothing announces nothing"
    );
    assert!(!store.lock.as_ref().unwrap().is_held());
}

/// **A delayed flush with no bucket flagged writes no level marker, even for a group never
/// written, and claims nothing.** A group's marker goes on disk before its first bucket file, so a
/// group with no bucket to write has none to commit, and claiming to write would advance the epoch
/// over a sweep that writes nothing.
#[tokio::test]
async fn a_delayed_flush_writes_no_level_marker_for_a_group_with_nothing_flagged() {
    let dir = lore_base::test_util::TempDir::new("is_delayed_no_marker_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let immutable = dir.to_path_buf().join("immutable");
    let epoch = immutable.join("epoch");
    let marker = lore_storage::local::fan_out::group_dir_path(&immutable.join("index"), 0)
        .join(lore_storage::local::fan_out::MARKER_FILENAME);
    let before = std::fs::read(&epoch).ok();

    ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), 0, Duration::ZERO).await;

    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "the sweep claimed nothing to write"
    );
    assert!(!marker.exists(), "and wrote no level marker");
    assert!(!store.lock.as_ref().unwrap().is_held());
}

/// **A delayed flush releases the flock a span left to it.** The span ends while the flush is
/// writing one bucket, with another still flagged, so the flock outlives the span. The flush's
/// own guard is what releases it once both are written: a flush the span had covered would hold
/// no guard, and leave the flock held with nothing left to release it.
#[tokio::test]
async fn a_delayed_flush_releases_the_flock_a_span_left_to_it() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_delayed_span_release_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let (group_index, payloads) = payloads_in_one_group(8);
    let first = &payloads[0];
    let second = payloads
        .iter()
        .find(|payload| hash::hash_slice(payload).data()[1] != hash::hash_slice(first).data()[1])
        .expect("two of the payloads fall in different buckets");
    let partition = Partition::from([9u8; 16]);

    let span = store.clone().hold_for_command().await.expect("claims");
    put_payload(&store, partition, Context::default(), first, false).await;
    put_payload(&store, partition, Context::default(), second, false).await;
    let pause = pause_at(PausePoint::Serializing);
    let flush = lore_base::lore_spawn!(ImmutableStoreGroup::flush_delayed(
        Arc::downgrade(&store),
        group_index,
        Duration::ZERO
    ));
    pause.reached.notified().await;
    drop(span);
    assert!(
        lock.is_held(),
        "the bucket the flush has yet to write keeps the flock past the span"
    );

    pause.resume.notify_one();
    flush.await.expect("delayed flush");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while lock.is_held() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the flush released the flock once it had written both buckets"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// **An open store holds the claim; a closed one holds nothing.**
///
/// The flock says which process is using the store, so it belongs to the span
/// during which one is — a handle from open to close, or a command. Operations
/// inside that span join the claim rather than taking one each, which is both
/// what makes the flock mean "in use" and what keeps a per-operation lock
/// acquisition and epoch read off every call.
///
/// Between spans nothing is held, which is what lets another process take the
/// store even while this one keeps the last state in memory.
#[tokio::test]
async fn a_claim_spans_the_use_of_a_store_not_each_operation() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_claim_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let partition = Partition::from([0x33u8; 16]);
    let address = Address {
        hash: lore_storage::hash::hash_slice(b"absent"),
        context: Context::default(),
    };
    let mut results = [StoreMatchResult::default(); 1];

    assert!(!lock.is_held(), "a store nobody is using is not claimed");
    store
        .clone()
        .query(partition, &[address], &mut results)
        .await
        .unwrap();
    assert!(
        !lock.is_held(),
        "and an operation on its own leaves it unclaimed once it returns"
    );

    {
        let _claim = store.clone().hold_for_command().await.unwrap();
        assert!(lock.is_held(), "a claimed store is held");
        store
            .clone()
            .query(partition, &[address], &mut results)
            .await
            .unwrap();
        assert!(
            lock.is_held(),
            "an operation inside the claim joins it rather than releasing on the way out"
        );
    }
    assert!(
        !lock.is_held(),
        "and the claim goes when whoever took it is done"
    );
}

/// **A store serves what another process wrote, not what it had cached.**
///
/// Two stores over one directory, which is what two processes are. The first
/// caches an empty bucket by looking for an address that is not there; the second
/// then writes that address and flushes. The first must find it on its next
/// operation — which it can only do by noticing the epoch moved and dropping the
/// bucket it had.
///
/// This is the whole point of the epoch, and it is the only end-to-end exercise of
/// [`ImmutableStoreGroup::invalidate`]: the lock-interleave probe drives reads
/// only, so nothing there ever advances an epoch or reaches a refresh.
#[tokio::test]
async fn a_store_drops_what_another_one_changed_underneath_it() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_refresh_");
    let partition = Partition::from([0x11u8; 16]);
    let payload = vec![0x7eu8; 512];
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::default(),
    };

    let reader =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    // Caches the bucket, empty, and observes the epoch as it stands.
    let mut results = [StoreMatchResult::default(); 1];
    reader
        .clone()
        .query(partition, &[address], &mut results)
        .await
        .unwrap();
    assert_eq!(
        results[0].match_made,
        StoreMatch::MatchNone,
        "nothing has been written yet"
    );

    {
        let writer =
            LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
                .await
                .unwrap();
        writer
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(Bytes::from(payload.clone())),
                false,
            )
            .await
            .unwrap();
        writer.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    let mut results = [StoreMatchResult::default(); 1];
    reader
        .clone()
        .query(partition, &[address], &mut results)
        .await
        .unwrap();
    assert_eq!(
        results[0].match_made,
        StoreMatch::MatchFull,
        "the cached empty bucket must have been dropped and re-read"
    );
}

/// **A read into the caller's buffer serves what another process wrote, as a query does.**
///
/// `get_into` is how a block load reads the local store, so it claims the store like any
/// other operation: the claim is what notices the epoch moved and drops the empty bucket
/// this store cached on its first look.
#[tokio::test]
async fn a_read_into_a_callers_buffer_serves_what_another_store_wrote() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_refresh_into_");
    let partition = Partition::from([0x12u8; 16]);
    let payload = vec![0x5du8; 512];
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::default(),
    };

    let reader =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();
    let mut buffer = vec![0u8; payload.len()];
    // SAFETY: the buffer outlives both reads, and nothing else touches it.
    let mut dst = unsafe { lore_storage::CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };

    // Caches the bucket, empty, and observes the epoch as it stands.
    assert!(
        reader
            .clone()
            .get_into(partition, address, &mut dst)
            .await
            .is_err(),
        "nothing has been written yet"
    );

    {
        let writer =
            LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
                .await
                .unwrap();
        writer
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(Bytes::from(payload.clone())),
                false,
            )
            .await
            .unwrap();
        writer.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    let (_fragment, read) = reader
        .clone()
        .get_into(partition, address, &mut dst)
        .await
        .expect("the cached empty bucket must have been dropped and re-read");
    assert!(
        matches!(read, lore_storage::store_types::PayloadRead::IntoBuffer),
        "the payload is the content, so it lands in the caller's buffer"
    );
    assert_eq!(buffer, payload);
}

/// **A store given its info file as it opens announces it.** The info file is store state
/// like any other, so writing it takes a write claim, which advances the epoch before the
/// file is written; the claim ends with the open.
#[tokio::test]
async fn opening_a_store_with_no_info_file_writes_one_under_a_write_claim() {
    let dir = lore_base::test_util::TempDir::new("is_info_open_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    let immutable = dir.to_path_buf().join("immutable");
    assert!(
        info_path_for_store_root(&immutable).exists(),
        "the open gave the store its info file"
    );
    assert!(
        immutable.join("epoch").exists(),
        "under a write claim, which announced it"
    );
    assert!(
        !store.lock.as_ref().unwrap().is_held(),
        "and the claim ended with the open"
    );
}

/// **A store reloads the info another one wrote.** The info file is part of what a store
/// keeps from disk, so a change to it advances the epoch, and a store that finds the epoch
/// moved reads it again with everything else.
#[tokio::test]
async fn a_store_reloads_the_info_another_one_wrote() {
    let dir = lore_base::test_util::TempDir::new("is_info_reload_");
    let open =
        || LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default());
    let reader = open().await.unwrap();
    let writer = open().await.unwrap();
    assert_eq!(
        reader.info.read().await.next_group_index_to_migrate_oodle,
        -1,
        "a fresh store has nothing to migrate"
    );

    writer
        .update_store_info(|info| info.next_group_index_to_migrate_oodle = 7)
        .await
        .expect("updates the info file");
    assert!(
        !writer.lock.as_ref().unwrap().is_held(),
        "the update lets the store go once the file is written"
    );

    let _hold = reader.hold(Intent::Read).await.expect("claims");
    assert_eq!(
        reader.info.read().await.next_group_index_to_migrate_oodle,
        7,
        "the reload read the info file again"
    );
}

/// A store that never reclaims records no access, so a resolve neither moves the stamp nor
/// dirties the bucket holding it.
#[tokio::test]
async fn a_resolve_records_nothing_without_atime() {
    let (last_access, dirty) = resolve_one_fragment(false, STALE_ACCESS, Lookup::Write).await;

    assert_eq!(last_access, STALE_ACCESS);
    assert!(!dirty);
}

/// Ranking by access is only worth anything if a stamp outlives the process that made it, so
/// this reads the bucket back off disk rather than out of the store that wrote it.
#[tokio::test]
async fn a_stamp_reaches_the_bucket_file() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_atime_persist_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();

    let ((group_index, bucket_index), partition, address) =
        backdated_fragment(&store, STALE_ACCESS).await;

    // Driven as a read, which is the case that matters: a client reads far more
    // than it writes, so a stamp that only outlived writes would rank on almost
    // nothing. A read marks the bucket for the next flush without marking the store
    // as changed, so the flush writes it under a read claim.
    store.find(partition, address, Lookup::Read).await.unwrap();
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let root = store.path.clone().expect("a disk-backed store has a path");
    let entry = ImmutableStoreBucket::deserialize_files(format_bucket_path(
        &root,
        group_index,
        bucket_index,
    ))
    .await
    .unwrap()
    .entry;

    assert_eq!(entry.len(), 1, "the stamp must have dirtied the bucket");
    assert!(
        entry[0].data.last_access > STALE_ACCESS,
        "the stamp a resolve made must survive the flush"
    );
}

/// **A flush with nothing to write announces nothing.** It runs after every
/// command, read-only ones included, and on every handle close; claiming to write
/// over nothing would make every other process on the store reload after each.
#[tokio::test]
async fn a_flush_with_nothing_to_write_announces_nothing() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_flush_quiet_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let epoch = dir.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    {
        let _span = store.clone().hold_for_command().await.unwrap();
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    assert_eq!(std::fs::read(&epoch).ok(), before, "nothing was written");
    assert!(!lock.is_held(), "and nothing is left claimed");
}

/// **A read's access stamp reaches disk without announcing a change**, on the
/// regular flush path a group takes once its level is committed.
#[tokio::test]
async fn a_read_stamp_reaches_disk_without_announcing_a_change() {
    let dir = lore_base::test_util::TempDir::new("is_stamp_quiet_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), partition, address) =
        backdated_fragment(&store, STALE_ACCESS).await;
    // Written once as a change, so the group's level is committed and the stamp below
    // goes through the regular path.
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    lore_storage::immutable_store::ImmutableStore::flush(store.clone(), FlushOptions::SYNC_DATA)
        .await
        .unwrap();
    assert!(!lock.is_held());

    let epoch = dir.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    store.find(partition, address, Lookup::Read).await.unwrap();
    lore_storage::immutable_store::ImmutableStore::flush(store.clone(), FlushOptions::SYNC_DATA)
        .await
        .unwrap();

    let root = store.path.clone().expect("a disk-backed store has a path");
    let entry = ImmutableStoreBucket::deserialize_files(format_bucket_path(
        &root,
        group_index,
        bucket_index,
    ))
    .await
    .unwrap()
    .entry;
    assert!(
        entry[0].data.last_access > STALE_ACCESS,
        "the stamp reached disk"
    );
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "without telling any other process the store changed"
    );
    assert!(!lock.is_held(), "and without keeping the store claimed");
}

/// **A write keeps the store claimed until it is flushed**, so no other process can
/// read a store this one has changed and not yet written.
#[tokio::test]
async fn a_write_keeps_the_store_claimed_until_it_is_flushed() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_write_claim_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    backdated_fragment(&store, STALE_ACCESS).await;
    assert!(lock.is_held(), "an unflushed write holds the store");

    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!lock.is_held(), "and the flush that settles it releases it");
}

/// **A read through the store records its stamp as a read**: flagged for flush,
/// never as a change, and never keeping the store claimed.
#[tokio::test]
async fn a_read_through_the_store_records_its_stamp_as_a_read() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_query_stamp_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), partition, address) =
        backdated_fragment(&store, STALE_ACCESS).await;

    let mut results = [StoreMatchResult::default(); 1];
    store
        .clone()
        .query(partition, &[address], &mut results)
        .await
        .unwrap();
    let group = &store.group[group_index];
    assert!(
        !group.dirty[bucket_index].load(atomic::Ordering::Relaxed),
        "a query is not a change"
    );
    assert!(
        group.needs_flush(bucket_index),
        "but its stamp is worth writing"
    );

    store.note_flushed();
    assert!(
        !lock.is_held(),
        "and a stamp alone never keeps the store claimed"
    );
}

/// **An eviction writes what it evicted and releases the store.** Nothing else
/// schedules those buckets for writing, so a pass that left them dirty would hold
/// the flock for as long as the process kept the store.
#[tokio::test]
async fn an_eviction_writes_what_it_evicted_and_releases_the_store() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_evict_write_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            protect_local_fragment: false,
            ..quiet_settings()
        },
    )
    .await
    .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    let epoch = dir.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();

    let evicted = store.clone().evict(1, false, None).await.unwrap();
    assert!(evicted > 0, "a cap of one evicts the fragment");
    assert!(!lock.is_held(), "the pass wrote what it evicted and let go");
    assert_ne!(
        std::fs::read(&epoch).ok(),
        before,
        "an eviction is a change, and says so"
    );

    let root = store.path.clone().expect("a disk-backed store has a path");
    let entry = ImmutableStoreBucket::deserialize_files(format_bucket_path(
        &root,
        group_index,
        bucket_index,
    ))
    .await
    .unwrap()
    .entry;
    assert!(entry.is_empty(), "the eviction reached disk");
}

/// **An eviction writes what it evicted on its own claim**, never on the span of another use of
/// the store, which can end while the write runs: on that span alone, the flock would go as the
/// last bucket write lands, with the write still counted as running, which test builds fail. A
/// pass on its own schedule, or one a load fired, runs alongside whatever use is under way.
#[tokio::test]
async fn an_eviction_writes_on_its_own_claim_past_another_span() {
    use lore_storage::immutable_store::ImmutableStore;
    use lore_storage::test_util::PausePoint;

    let dir = lore_base::test_util::TempDir::new("is_evict_span_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            protect_local_fragment: false,
            ..quiet_settings()
        },
    )
    .await
    .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let span = ImmutableStore::hold_for_command(store.clone())
        .await
        .unwrap();
    let scanning = lore_storage::test_util::pause_at(PausePoint::Scanning);
    let evicting = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.evict(1, false, None).await })
    };
    scanning.reached.notified().await;
    assert_eq!(
        lock.shared.covered.load(atomic::Ordering::Relaxed),
        0,
        "the write of what the pass evicted counts on no span"
    );
    drop(span);
    scanning.resume.notify_one();
    let evicted = evicting.await.expect("the pass ends").expect("evicts");

    assert!(evicted > 0, "a cap of one evicts the fragment");
    assert!(
        !lock.is_held(),
        "the pass let the store go once it had written what it evicted"
    );
}

/// **A compaction claims to write on its own**, never on the span of another use of the store,
/// which can end while the pass runs, as its claim to read is its own. A claim the span covered
/// is counted as one, and fails test builds where the span ends between the check and the count.
#[tokio::test]
async fn a_compaction_claims_to_write_on_its_own_past_another_span() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_span_");
    let partition = Partition::from([0x32u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    let store = store_for_rewrite(&dir, partition, &payloads[..4], 2).await;
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let span = ImmutableStore::hold_for_command(store.clone())
        .await
        .unwrap();
    put_payload(&store, partition, Context::default(), &payloads[4], false).await;
    let selected = pause_at(PausePoint::Selected);
    let compacting = {
        let store = store.clone();
        lore_base::lore_spawn!(
            async move { store.compact(1, Some(group_index), false, None).await }
        )
    };
    tokio::time::timeout(Duration::from_secs(5), selected.reached.notified())
        .await
        .expect("the pass selects packfiles of the group to rewrite");
    assert_eq!(
        lock.shared.covered.load(atomic::Ordering::Relaxed),
        0,
        "the pass's claim to write counts on no span"
    );
    drop(span);
    selected.resume.notify_one();
    compacting.await.expect("the pass ends").expect("compacts");
    store.clone().flush(FlushOptions::default()).await.unwrap();
    assert!(
        !lock.is_held(),
        "the pass let the store go once it had ended"
    );
}

/// **A compaction measures the store on its own claims.** A pass measures the packstores as it
/// starts, under its claim to read, and as a step ends, under its claim to write, never on the
/// cover of a span of whatever use of the store is under way, which can end while it measures. A
/// measurement the span covered is counted as one, and fails test builds where the span ends
/// between the check and the count.
#[tokio::test]
async fn a_compaction_measures_the_store_on_its_own_claims_past_another_span() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_measure_");
    let partition = Partition::from([0x33u8; 16]);
    let (_, payloads) = payloads_in_one_group(4);
    let store = store_for_rewrite(&dir, partition, &payloads, 2).await;
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let span = ImmutableStore::hold_for_command(store.clone())
        .await
        .unwrap();
    let measures = [
        pause_at(PausePoint::Measuring),
        pause_at(PausePoint::Measuring),
    ];
    let compacting = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.compact(1, None, false, None).await })
    };
    for (measure, when) in measures
        .iter()
        .zip(["as the pass starts", "as the step ends"])
    {
        tokio::time::timeout(Duration::from_secs(5), measure.reached.notified())
            .await
            .unwrap_or_else(|_| panic!("the pass measures the store {when}"));
        assert_eq!(
            lock.shared.covered.load(atomic::Ordering::Relaxed),
            0,
            "the pass measures the store {when} on its own claim, counted on no span"
        );
        measure.resume.notify_one();
    }
    let next = compacting.await.expect("the pass ends").expect("compacts");
    assert!(next.is_some(), "the step left groups for the next one");
    drop(span);
}

/// **A healing verify releases the store.** Its flush runs inside the pass's own
/// write claim, so the pass itself has to release what that claim marked.
#[tokio::test]
async fn a_healing_verify_releases_the_store() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_verify_release_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    store.clone().verify(true).await.unwrap();
    assert!(
        !lock.is_held(),
        "a verify that healed nothing leaves nothing claimed"
    );
}

/// **A verify heals a payload a fan-out would move out of its walk.** It walks a group under the
/// group's flush lock, so a flush fanning the group out waits for the walk, which reads one
/// layout and finds every entry of the group at it.
#[tokio::test]
async fn a_verify_heals_a_corrupt_payload_a_fan_out_would_move_out_of_its_walk() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_verify_fan_out_");
    let store = LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        ImmutableStoreSettings {
            fan_out_threshold: 4,
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .expect("store opens");
    let partition = Partition::from([3u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(6);
    let mut addresses = vec![];
    for payload in &payloads[..4] {
        addresses.push(put_payload(&store, partition, Context::default(), payload, false).await);
    }
    ImmutableStore::flush(store.clone(), FlushOptions::default())
        .await
        .expect("flush at level 1");
    let corrupt = *addresses
        .iter()
        .find(|address| lore_storage::local::fan_out::bucket_index_for(&address.hash, 32) != 0)
        .expect("a payload a fan-out to 32 buckets moves out of bucket 0");
    let found = store
        .clone()
        .find(partition, corrupt, Lookup::Read)
        .await
        .expect("the entry is there to corrupt");
    store.group[group_index]
        .packstore
        .obliterate(
            found.data.pack_file,
            found.data.pack_offset,
            found.data.size_payload,
        )
        .await
        .expect("overwrite the payload");
    for payload in &payloads[4..] {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }

    let checking: Vec<_> = (0..=group_index)
        .map(|_| pause_at(PausePoint::Checking))
        .collect();
    let verifying = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.verify(true).await })
    };
    for pause in &checking[..group_index] {
        pause.reached.notified().await;
        pause.resume.notify_one();
    }
    checking[group_index].reached.notified().await;
    let mut flushing = {
        let store = store.clone();
        lore_base::lore_spawn!(async move {
            ImmutableStore::flush(store, FlushOptions::default()).await
        })
    };
    let flushed_meanwhile = tokio::time::timeout(Duration::from_millis(100), &mut flushing)
        .await
        .is_ok();
    checking[group_index].resume.notify_one();
    verifying.await.expect("the verify ends").expect("verifies");
    let resolved = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        corrupt,
    )
    .await
    .expect("query answers");
    assert_eq!(
        resolved.match_made,
        StoreMatch::MatchNone,
        "the verify healed the payload that no longer verifies"
    );
    assert!(!flushed_meanwhile, "the flush waits for the walk");
    flushing.await.expect("the flush ends").expect("flush");
    assert!(
        store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed)
            > 1,
        "the group fanned out once the walk ended"
    );
}

/// **A verify fails when a bucket cannot be loaded**, rather than passing over the entries the
/// bucket holds.
#[tokio::test]
async fn a_verify_fails_when_a_bucket_cannot_be_loaded() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_verify_unloaded_");
    let partition = Partition::from([3u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), quiet_settings())
            .await
            .expect("store opens");
        put_payload(&store, partition, Context::default(), &payloads[0], false).await;
        ImmutableStore::flush(store.clone(), FlushOptions::default())
            .await
            .expect("flush");
    }
    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), quiet_settings())
        .await
        .expect("store reopens");
    block_bucket_file(&store, group_index, 0);
    assert!(
        store.verify(false).await.is_err(),
        "a verify that could not load a bucket passed"
    );
}

/// **A flush settles a bucket it has nothing to write for.** An emptied bucket above
/// the committed level has no file to write and none to overwrite; its flags left
/// set would keep the store claimed after the flush.
#[tokio::test]
async fn a_flush_settles_an_emptied_bucket_it_has_nothing_to_write_for() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_empty_settle_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let group = store.group[0].clone();
    let _ = group.bucket(0);
    group.dirty[0].store(true, atomic::Ordering::Relaxed);
    group.soft_dirty[0].store(true, atomic::Ordering::Relaxed);

    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!group.needs_flush(0), "the flags were settled");
    assert!(!lock.is_held(), "so nothing keeps the store claimed");
}

/// **A background pass with nothing to do takes no claim at all.** It runs on a timer
/// for the life of the process, including while nothing has the store open, and a
/// process with nothing open blocks nobody: a pass that claimed the store just to look
/// would wait on, and then hold out, every other process using it.
#[tokio::test]
async fn a_background_pass_with_nothing_to_do_takes_no_claim() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_gc_unclaimed_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    // Another process's claim: a second description of the lock file excludes this
    // process's own.
    let elsewhere = lore_base::fs::lock::FSLock::acquire_exact_path(
        &dir.to_path_buf().join("immutable").join("lock"),
    )
    .await
    .expect("takes the store's lock");

    const ROOMY: usize = 1 << 30;
    tokio::time::timeout(
        Duration::from_secs(5),
        store.clone().evict(ROOMY, false, None),
    )
    .await
    .expect("an eviction pass with nothing to evict does not wait for the store")
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        store.clone().compact(ROOMY, None, false, None),
    )
    .await
    .expect("nor does a compaction pass with nothing to compact")
    .unwrap();
    drop(elsewhere);
}

/// **A background pass with work to do leaves a store in use elsewhere for later.** It
/// runs on a timer, so nothing is lost by running another time — where queueing behind
/// a hold with no bound would keep the pass, and the permit `stop_gc` waits for, for as
/// long as the other process kept the store.
#[tokio::test]
async fn a_background_pass_leaves_a_store_in_use_for_later() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_gc_deferred_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            protect_local_fragment: false,
            ..quiet_settings()
        },
    )
    .await
    .unwrap();
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let elsewhere = lore_base::fs::lock::FSLock::acquire_exact_path(
        &dir.to_path_buf().join("immutable").join("lock"),
    )
    .await
    .expect("another process takes the store's lock");
    let evicted = tokio::time::timeout(Duration::from_secs(5), store.clone().evict(1, false, None))
        .await
        .expect("an eviction pass does not wait for a store another process holds")
        .unwrap();
    assert_eq!(evicted, 0, "and evicts nothing from it");
    let compacted = tokio::time::timeout(
        Duration::from_secs(5),
        store.clone().compact(1, None, false, None),
    )
    .await
    .expect("nor does a compaction pass")
    .unwrap();
    assert_eq!(compacted, None);

    drop(elsewhere);
    let evicted = store.clone().evict(1, false, None).await.unwrap();
    assert!(
        evicted > 0,
        "and a later pass evicts once the store is free"
    );
}

/// **A compaction pass measures the store under its claim.** The claim reloads a store
/// another process has changed, so a pass measures what is on disk now rather than what
/// this store held in memory; one that measured first would claim to write, and
/// compact, a store another process had already shrunk under its cap.
#[tokio::test]
async fn a_compaction_pass_decides_again_from_a_store_its_claim_reloaded() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_recheck_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    let held_in_memory = store.packstore_total_size().await;
    assert!(
        held_in_memory > 0,
        "the fragment's payload is in a packfile"
    );

    // Another store on the directory empties that packfile, as its own compaction would,
    // under a write claim that announces it.
    let other = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    {
        let _claim = other.hold(Intent::Write).await.unwrap();
        assert_eq!(other.packstore_total_size().await, held_in_memory);
        // Out of the set being written first, as compaction takes it: a packfile still
        // open for writes cannot be truncated.
        let packstore = &other.group[group_index].packstore;
        let _ = packstore.stop_writes(&[1]).await;
        let _ = packstore.truncate(1, ()).await;
        assert_eq!(
            other.packstore_total_size().await,
            0,
            "the packfile is emptied"
        );
    }
    other.note_flushed();

    let epoch = dir.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    let compacted = store
        .clone()
        .compact(held_in_memory, None, false, None)
        .await
        .unwrap();
    assert_eq!(compacted, None, "reloaded, the store is under its cap");
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "so the pass never claims to write"
    );
}

/// **A compaction whose write claim reloaded the store rewrites nothing.** The claim is
/// taken after the pass has loaded every bucket; a reload in between empties them, and
/// a pass that went on would rewrite packfiles against buckets that no longer describe
/// them — truncating payloads still referenced on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_whose_write_claim_reloaded_the_store_rewrites_nothing() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_reload_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let ((group_index, bucket_index), partition, address) =
        backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    // Held, so the pass stops between the buckets it loads and the write claim it takes.
    let permit = store
        .compaction
        .acquire()
        .await
        .expect("the compaction permit");
    let pass = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.compact(1, None, false, None).await })
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !store.deserialized_all.load(atomic::Ordering::Relaxed) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pass loads every bucket"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // Another store on the directory announces a write, which the pass's store learns of
    // only when it next claims to write.
    let other = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    drop(other.hold(Intent::Write).await.unwrap());
    other.note_flushed();

    drop(permit);
    let compacted = tokio::time::timeout(Duration::from_secs(10), pass)
        .await
        .expect("the pass ends")
        .expect("the task completes")
        .expect("compacts");
    assert_eq!(
        compacted, None,
        "a pass that found its buckets reloaded stops where it began"
    );

    let reopened = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let verified = reopened
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, false)
        .await
        .expect("the fragment is still indexed, with its payload where the index says");
    assert!(
        verified.verification_result.is_ok(),
        "and the payload is intact"
    );
}

/// **A verify that finds nothing releases the store.** A healing verify claims to
/// write, which marks the store dirty as the claim is taken, so a verify that returned
/// early without releasing — at an address it does not hold, or a bucket it cannot
/// load — would keep the store claimed.
#[tokio::test]
async fn a_verify_of_an_absent_address_releases_the_store() {
    let dir = lore_base::test_util::TempDir::new("is_verify_absent_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let absent = Address {
        hash: lore_storage::hash::hash_slice(b"never stored"),
        context: Context::default(),
    };
    let verified = store
        .clone()
        .verify_fragment(
            absent,
            Partition::from([0x11u8; 16]),
            StoreMatch::MatchFull,
            true,
        )
        .await;
    assert!(verified.is_err(), "there is nothing at that address");
    assert!(
        !lock.is_held(),
        "and the verify that found so leaves nothing claimed"
    );
}

/// **A change that fails to reach disk keeps its flag and the store's claim**, whether
/// the write was synced or not. The flag is cleared as the write begins, so a failure
/// that left it clear would never write the change, and a flush scan would find the
/// store clean and release its flock over state that is only in memory.
#[tokio::test]
async fn a_change_that_fails_to_reach_disk_keeps_its_flag_and_the_claim() {
    use lore_storage::immutable_store::ImmutableStore;

    for sync_data in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_failed_change_");
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
            .await
            .unwrap();
        let lock = store.lock.clone().expect("a disk-backed store has a lock");
        let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
        let group = store.group[group_index].clone();
        // Written once, so the group's level is committed and the write below takes the
        // regular path to the bucket's own file.
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
        assert!(!lock.is_held());

        block_bucket_file(&store, group_index, bucket_index);
        // A change, made the way every change is: under a write claim.
        drop(store.hold(Intent::Write).await.unwrap());
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

/// **An access stamp that fails to reach disk stays a stamp**: its flag is re-armed to
/// be written again, and it neither becomes a change nor keeps the store claimed.
#[tokio::test]
async fn a_stamp_that_fails_to_reach_disk_stays_a_stamp() {
    use lore_storage::immutable_store::ImmutableStore;

    for sync_data in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_failed_stamp_");
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
            .await
            .unwrap();
        let lock = store.lock.clone().expect("a disk-backed store has a lock");
        let ((group_index, bucket_index), partition, address) =
            backdated_fragment(&store, STALE_ACCESS).await;
        let group = store.group[group_index].clone();
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

        block_bucket_file(&store, group_index, bucket_index);
        store.find(partition, address, Lookup::Read).await.unwrap();
        assert!(
            group.soft_dirty[bucket_index].load(atomic::Ordering::Relaxed),
            "the read stamped the bucket"
        );

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
            group.soft_dirty[bucket_index].load(atomic::Ordering::Relaxed),
            "sync_data {sync_data}: the stamp keeps its flag, to be written again"
        );
        assert!(
            !group.dirty[bucket_index].load(atomic::Ordering::Relaxed),
            "sync_data {sync_data}: and is still not a change"
        );
        assert!(
            !lock.is_held(),
            "sync_data {sync_data}: so it keeps nothing claimed"
        );
    }
}

/// **A flush that fails to commit its level keeps the store claimed until a flush commits it.**
/// The failure here is at the commit point of a level transition, after the buckets are
/// written to their `.new` files. No commit point records those files, so the buckets stay
/// flagged for the next flush to write again, and a flagged bucket keeps the flock. The flush
/// that commits the level releases it.
#[tokio::test]
async fn a_flush_that_fails_to_commit_its_level_keeps_the_store_claimed_until_one_commits_it() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_failed_flush_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

    // A group's first flush commits its level through `level.pending`, after writing
    // its buckets; a directory there fails the commit.
    let root = store.path.clone().expect("a disk-backed store has a path");
    let mut group_path = root.as_path().join("index");
    lore_storage::local::fan_out::push_group_dir(&mut group_path, group_index);
    std::fs::create_dir_all(
        group_path
            .join(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME)
            .join("occupied"),
    )
    .expect("a directory where the commit point goes");

    assert!(
        store.clone().flush(FlushOptions::SYNC_DATA).await.is_err(),
        "the level cannot be committed"
    );
    assert!(
        store.group[group_index].needs_flush(bucket_index),
        "the bucket stays flagged"
    );
    assert!(lock.is_held(), "and keeps the store claimed");

    std::fs::remove_dir_all(group_path.join(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME))
        .expect("the commit point is freed");
    store
        .clone()
        .flush(FlushOptions::SYNC_DATA)
        .await
        .expect("the level is committed");
    assert!(!store.group[group_index].needs_flush(bucket_index));
    assert!(!lock.is_held(), "and the store is released");
}

/// **A flush reloads a store another one changed before it writes.** Its flags can be
/// set on state that no longer describes the directory, and a flush that wrote them
/// without reloading first would put back a bucket the other store had since
/// rewritten — losing the entry that store added.
#[tokio::test]
async fn a_flush_reloads_a_changed_store_before_it_writes() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_flush_reload_");
    let settings = || ImmutableStoreSettings {
        isolate_partitions: true,
        ..quiet_settings()
    };
    let mine = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();
    let ((group_index, bucket_index), _, address) = backdated_fragment(&mine, STALE_ACCESS).await;
    mine.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    mine.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    // Another store on the directory adds the same content under a second partition.
    let theirs = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();
    let payload = vec![0x22u8; 128];
    theirs
        .clone()
        .put(
            Partition::from([0x44u8; 16]),
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();
    theirs.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    // A flag on this store's copy of the bucket, which no longer describes it.
    mine.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    mine.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let root = mine.path.clone().expect("a disk-backed store has a path");
    let entry = ImmutableStoreBucket::deserialize_files(format_bucket_path(
        &root,
        group_index,
        bucket_index,
    ))
    .await
    .unwrap()
    .entry;
    assert_eq!(
        entry.len(),
        2,
        "the entry the other store added is still on disk"
    );
}

/// **A flush of a change announces it, even with no write claim behind the flag.** A
/// bucket can be marked changed under a read claim — a load that upgrades what it read
/// marks the bucket for rewrite — and the flush that writes it is then the first to
/// claim to write, so it is the one that must advance the epoch.
#[tokio::test]
async fn a_flush_of_a_change_announces_it_without_a_write_claim_behind_it() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_flush_announce_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), _, _) = backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(!lock.is_held());

    let epoch = dir.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    assert_ne!(
        std::fs::read(&epoch).ok(),
        before,
        "the change is announced"
    );
    assert!(
        !lock.is_held(),
        "and the flush that wrote it lets the store go"
    );
}

/// **A delayed flush writes access stamps without announcing a change.** It claims only
/// as far as its work needs, as an explicit flush does: stamps under a read claim.
#[tokio::test]
async fn a_delayed_flush_writes_stamps_without_announcing_them() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_delayed_stamps_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let ((group_index, bucket_index), partition, address) =
        backdated_fragment(&store, STALE_ACCESS).await;
    store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();

    let epoch = dir.to_path_buf().join("immutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    store.find(partition, address, Lookup::Read).await.unwrap();
    ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), group_index, Duration::ZERO).await;

    let root = store.path.clone().expect("a disk-backed store has a path");
    let entry = ImmutableStoreBucket::deserialize_files(format_bucket_path(
        &root,
        group_index,
        bucket_index,
    ))
    .await
    .unwrap()
    .entry;
    assert!(
        entry[0].data.last_access > STALE_ACCESS,
        "the stamp reached disk"
    );
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "without announcing a change"
    );
    assert!(!lock.is_held(), "and without keeping the store claimed");
}

/// **A bucket flagged while a sweep runs schedules the next sweep.** The running sweep
/// may already have passed that bucket, so leaving the bucket to it would leave it
/// flagged, keeping the store claimed with nothing coming to write it.
#[tokio::test]
async fn a_bucket_flagged_while_a_sweep_runs_schedules_the_next() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_sweep_schedule_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let ((group_index, _), partition, address) = backdated_fragment(&store, STALE_ACCESS).await;
    let group = store.group[group_index].clone();
    assert!(
        group.scheduled.load(atomic::Ordering::Relaxed),
        "the put scheduled a sweep"
    );

    // That sweep has woken, and is still running.
    group.scheduled.store(false, atomic::Ordering::Relaxed);
    let running = group.flush.lock().await.len();

    // The same content under another context lands in the same group.
    let payload = vec![0x22u8; 128];
    store
        .clone()
        .put(
            partition,
            Address {
                hash: address.hash,
                context: Context::from([0x44u8; 16]),
            },
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();

    assert_eq!(
        group.flush.lock().await.len(),
        running + 1,
        "a write during a running sweep schedules another"
    );
    assert!(group.scheduled.load(atomic::Ordering::Relaxed));
}

/// **A reload takes the levels a fresh survey read and drops every flag.** A group
/// left at its old level would address buckets by the wrong layout, and a flag left
/// set would flush state the reload has just discarded.
#[tokio::test]
async fn invalidating_a_group_takes_the_surveyed_levels_and_clears_its_flags() {
    let store = LocalImmutableStore::new(None, ImmutableStoreSettings::default())
        .await
        .unwrap();
    let group = store.group[0].clone();
    let _ = group.bucket(3);
    group.dirty[3].store(true, atomic::Ordering::Relaxed);
    group.soft_dirty[5].store(true, atomic::Ordering::Relaxed);

    group.invalidate(64, 32, 7).await;

    assert!(
        !group.needs_flush(3) && !group.needs_flush(5),
        "no flag survives a reload"
    );
    assert_eq!(group.bucket_count.load(atomic::Ordering::Relaxed), 64);
    assert_eq!(group.committed_level.load(atomic::Ordering::Relaxed), 32);
    assert_eq!(group.serialize_version.load(atomic::Ordering::Relaxed), 7);
}

/// **A write inside a span keeps the flock past the span until it is flushed.** The
/// operation itself takes nothing; the bucket it flags is what the span's end finds.
#[tokio::test]
async fn a_write_inside_a_span_keeps_the_flock_past_the_span_until_flushed() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_span_write_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let span = store.clone().hold_for_command().await.unwrap();
    let payload = vec![0x3cu8; 64];
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::default(),
    };
    store
        .clone()
        .put(
            Partition::from([0x77u8; 16]),
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();
    drop(span);
    assert!(
        lock.is_held(),
        "the bucket the write flagged keeps the flock past the span"
    );

    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(
        !lock.is_held(),
        "and the flush that writes it releases the flock"
    );
}

/// **A store kept over its deleted directory reloads from it recreated, and empty.** The
/// next use of the path is handed the kept store, whose claim must neither fail to take the
/// flock in a directory that is gone nor serve what the deleted directory held.
#[tokio::test]
async fn a_store_kept_over_its_deleted_directory_reloads_it_recreated_and_empty() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_deleted_dir_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let payload = vec![0x2du8; 64];
    let partition = Partition::from([0x2du8; 16]);
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::default(),
    };
    let span = store.clone().hold_for_command().await.unwrap();
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    drop(span);

    std::fs::remove_dir_all(dir.join("immutable")).expect("deletes the store directory");
    let _span = store
        .clone()
        .hold_for_command()
        .await
        .expect("claims the store over its recreated directory");

    let found = store.find(partition, address, Lookup::Read).await.unwrap();
    assert_ne!(
        found.matching,
        StoreMatch::MatchFull,
        "nothing of the deleted directory is served"
    );
}

/// **A store holds no packfile open between uses.** The handles close as the flock goes,
/// so another process the lock admits finds none of this process's on a packfile it
/// writes or removes, and open again at the next use that reaches them. The size the GC
/// counters carry for the loaded packfiles goes and returns with them, so a reopen is not
/// counted on top of the first load.
#[tokio::test]
async fn a_store_holds_no_packfile_open_between_uses() {
    use lore_storage::immutable_store::ImmutableStore;

    async fn open_packfiles(store: &LocalImmutableStore) -> usize {
        let mut open = 0;
        for group in store.group.iter() {
            open += group.packstore.open_packfiles().await;
        }
        open
    }

    let dir = lore_base::test_util::TempDir::new("is_release_files_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    store.set_gc_caps(usize::MAX, usize::MAX, false);
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let payload = Bytes::from(vec![0x5au8; 96]);
    let partition = Partition::from([0x51u8; 16]);
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::default(),
    };
    let span = store.clone().hold_for_command().await.unwrap();
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(payload.clone()),
            false,
        )
        .await
        .unwrap();
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    assert!(
        open_packfiles(&store).await > 0,
        "the write opened the group's packfiles"
    );
    drop(span);
    assert!(!lock.is_held());
    assert_eq!(
        open_packfiles(&store).await,
        0,
        "and the end of the use closed them"
    );

    let span = store.clone().hold_for_command().await.unwrap();
    let got = store.clone().get(partition, address).await.unwrap();
    assert_eq!(
        got.payload.as_ref(),
        Some(&payload),
        "the next use reads through reopened packfiles"
    );
    assert!(open_packfiles(&store).await > 0);
    let counted = store.gc_counters.loaded_size();
    assert!(counted > 0, "the reopen counted the packfiles' size");
    drop(span);
    assert_eq!(open_packfiles(&store).await, 0);
    assert_eq!(
        store.gc_counters.loaded_size(),
        0,
        "closing took the size out of the GC counters"
    );

    let span = store.clone().hold_for_command().await.unwrap();
    let got = store.clone().get(partition, address).await.unwrap();
    assert_eq!(got.payload.as_ref(), Some(&payload));
    assert_eq!(
        store.gc_counters.loaded_size(),
        counted,
        "and a reopen counts them once, not on top of the first load"
    );
    drop(span);
}

/// Whether the flock of the store over `dir` is free to another open file description, which a
/// holder in this process excludes as a holder in another process would.
async fn flock_free(dir: &Path) -> bool {
    lore_base::fs::lock::FSLock::try_acquire_exact_path(&dir.join("immutable").join("lock"))
        .await
        .expect("attempts the flock")
        .is_some()
}

/// Puts a payload in a task of its own, held in flight at its packfile write, and drops the put
/// there, leaving the write to the driver as a dropped put does.
async fn abandon_a_packfile_write(
    store: &Arc<LocalImmutableStore>,
) -> Arc<lore_storage::test_util::InFlight> {
    let in_flight = lore_storage::test_util::hold_packfile_write();
    let put = {
        let store = store.clone();
        lore_base::lore_spawn!(async move {
            put_payload(
                &store,
                Partition::default(),
                Context::default(),
                &[0x6bu8; 64],
                false,
            )
            .await
        })
    };
    in_flight.issued.notified().await;
    put.abort();
    assert!(put.await.unwrap_err().is_cancelled());
    in_flight
}

/// **A packfile write abandoned in flight keeps the flock until it lands, past the store that
/// issued it.** The driver runs a write to its end after its future is dropped, and a process
/// the flock admitted meanwhile would size the packfile without it and write over its range.
/// Here a flagged bucket keeps the flock as the write's own claim goes, so no release is tried
/// before the store is dropped.
#[tokio::test]
async fn an_abandoned_packfile_write_keeps_the_flock_past_its_store_until_it_lands() {
    let dir = lore_base::test_util::TempDir::new("is_abandoned_write_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    put_payload(
        &store,
        Partition::default(),
        Context::default(),
        &[0x6au8; 64],
        false,
    )
    .await;
    let in_flight = abandon_a_packfile_write(&store).await;
    drop(store);
    assert!(
        !flock_free(dir.path()).await,
        "the write in flight keeps the flock past its store"
    );

    in_flight.complete();
    wait_for(
        async || flock_free(dir.path()).await,
        "the flock goes once the write lands",
    )
    .await;
}

/// **A packfile write that lands between its put's abort and the put's drop lets the flock go once
/// the put is dropped.** An abort drops the put's future only at the task's next poll, so the write
/// lands while the put still awaits it, and what the write was issued with goes with the put.
#[tokio::test]
async fn a_packfile_write_landing_before_its_aborted_put_is_dropped_lets_the_flock_go() {
    let dir = lore_base::test_util::TempDir::new("is_landed_before_drop_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let in_flight = lore_storage::test_util::hold_packfile_write();
    let put = {
        let store = store.clone();
        lore_base::lore_spawn!(async move {
            put_payload(
                &store,
                Partition::default(),
                Context::default(),
                &[0x6cu8; 64],
                false,
            )
            .await
        })
    };
    in_flight.issued.notified().await;
    put.abort();
    in_flight.complete();
    assert!(put.await.unwrap_err().is_cancelled());
    drop(store);

    wait_for(
        async || flock_free(dir.path()).await,
        "the flock goes once the aborted put is dropped",
    )
    .await;
}

/// **A use that ends with a packfile write in flight keeps the flock until the write lands.**
#[tokio::test]
async fn a_use_ending_with_a_packfile_write_in_flight_keeps_the_flock_until_it_lands() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_write_past_use_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let span = store.clone().hold_for_command().await.unwrap();
    let in_flight = abandon_a_packfile_write(&store).await;
    drop(span);
    assert!(
        lock.is_held(),
        "the write in flight keeps the flock past the use"
    );

    in_flight.complete();
    wait_for(
        async || !lock.is_held(),
        "the flock goes once the write lands",
    )
    .await;
}

/// **A store joins only a claim this process already holds, and without waiting.** Outside a
/// span, and for a store with nothing on disk, there is nothing to join; inside a span the join
/// is a counted guard, which keeps the flock past the span until it is dropped and covers no
/// operation.
#[tokio::test]
async fn a_store_joins_only_a_claim_it_already_holds() {
    use lore_storage::immutable_store::ImmutableStore;

    let in_memory = LocalImmutableStore::new(None, quiet_settings())
        .await
        .unwrap();
    assert!(in_memory.try_join_claim().is_none());

    let dir = lore_base::test_util::TempDir::new("is_try_join_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
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

/// **A task that could not join a claim takes one that keeps the flock and covers no operation**,
/// as a joined claim does: an operation outside the task that ran on it would lose the flock as
/// the task ends.
#[tokio::test]
async fn a_task_s_own_claim_keeps_the_flock_and_covers_no_operation() {
    let dir = lore_base::test_util::TempDir::new("is_task_claim_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();

    let claim = lore_storage::immutable_store::claim_for_task(&dyn_store, None)
        .await
        .expect("the task's own claim");
    assert!(lock.is_held(), "the task's claim keeps the flock");
    assert!(
        !lock.covers(Intent::Read),
        "the task's claim covers no operation"
    );
    drop(claim);
    assert!(!lock.is_held());
}

/// **A group flush left running by a dropped flush holds its group's writes until it ends**, which
/// it does once the bucket write it awaits has landed. The group's next flush, and the release of
/// the flock, wait for the group flush rather than for the flush that started it.
#[tokio::test]
async fn a_group_flush_left_by_a_dropped_flush_holds_its_group_until_it_ends() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_dropped_flush_group_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    put_payload(
        &store,
        Partition::default(),
        Context::default(),
        b"abandoned",
        false,
    )
    .await;

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

/// **A write in flight keeps the flock past the last guard, and the release follows it**, so that
/// no other process takes the store while a write this one dispatched has yet to land.
#[tokio::test]
async fn a_write_in_flight_keeps_the_flock_until_it_lands() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_write_in_flight_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), quiet_settings())
        .await
        .unwrap();
    let lock = store.lock.clone().expect("a disk-backed store has a lock");

    let span = store.clone().hold_for_command().await.unwrap();
    let in_flight = Arc::clone(&store.group[0].writes).read_owned().await;
    drop(span);
    assert!(lock.is_held(), "the write in flight keeps the flock");

    drop(in_flight);
    while lock.is_held() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

fn payload_data(pack_file: u32, encoding: u32, storage: u32) -> ImmutableData {
    ImmutableData {
        flags: encoding | storage,
        size_payload: if pack_file == 0 { 0 } else { 100 },
        size_content: 256,
        pack_offset: if pack_file == 0 { 0 } else { 200 },
        pack_file,
        last_access: 0,
    }
}

#[test]
fn merge_from_copy_source_adopts_payload_and_encoding() {
    // Target had its own uncompressed payload. Source has the same content stored
    // compressed in a different pack file. After merge, target adopts source's pack
    // pointer and the encoding flag that describes those bytes — keeping target's
    // pre-existing flags would mis-describe the new payload.
    let mut target = payload_data(1, 0, 0);
    let source = payload_data(2, FragmentFlags::PayloadCompressedZstd.bits(), 0);

    target.merge_from_copy_source(source, false);

    assert_eq!(target.pack_file, 2, "target adopts source's pack_file");
    assert_eq!(target.pack_offset, 200);
    assert_eq!(target.size_payload, 100);
    assert_ne!(
        target.flags & FragmentFlags::PayloadCompressedZstd.bits(),
        0,
        "encoding flag must follow the adopted payload",
    );
    assert_ne!(
        target.flags & FragmentFlags::PayloadStoredLocal.bits(),
        0,
        "adopted bytes are locally available",
    );
}

#[test]
fn merge_from_copy_source_preserves_target_durable() {
    // Target had PayloadStoredDurable from a prior remote round-trip on the destination
    // tuple. A subsequent local copy must not unset that bit.
    let mut target = payload_data(0, 0, FragmentFlags::PayloadStoredDurable.bits());
    let source = payload_data(2, 0, FragmentFlags::PayloadStoredDurable.bits());

    target.merge_from_copy_source(source, false);

    assert_ne!(
        target.flags & FragmentFlags::PayloadStoredDurable.bits(),
        0,
        "target's prior Durable must be preserved",
    );
}

#[test]
fn merge_from_copy_source_durable_only_from_caller() {
    // Source carries Durable; target had none. With `durable=false`, source's Durable
    // must NOT propagate. With `durable=true`, the caller's intent sets the bit.
    let source = payload_data(2, 0, FragmentFlags::PayloadStoredDurable.bits());

    let mut local_only = payload_data(0, 0, 0);
    local_only.merge_from_copy_source(source, false);
    assert_eq!(
        local_only.flags & FragmentFlags::PayloadStoredDurable.bits(),
        0,
        "Durable must not propagate from source on a local-only copy",
    );

    let mut remote_confirmed = payload_data(0, 0, 0);
    remote_confirmed.merge_from_copy_source(source, true);
    assert_ne!(
        remote_confirmed.flags & FragmentFlags::PayloadStoredDurable.bits(),
        0,
        "caller's `durable=true` sets the bit",
    );
}

#[tokio::test]
async fn copy_adopts_source_payload_and_decompresses_through_target_partition() {
    // Prime target with uncompressed payload at one address, prime source with the same
    // hash but compressed payload, then copy source → target. The target entry must
    // adopt source's payload pointer along with the matching encoding flag so a read
    // against the target partition decompresses correctly and returns the original bytes.
    use std::sync::atomic::Ordering;

    use lore_storage::compress::COMPRESSION_MODE;
    use lore_storage::compress::CompressionMode;
    use lore_storage::options::ReadOptions;
    use lore_storage::options::WriteOptions;
    use lore_storage::read::read;
    use lore_storage::write::StoreResult;
    use lore_storage::write::write_content;

    let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
        None::<&Path>,
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .unwrap();

    let target_partition = Partition::from([0x01u8; 16]);
    let source_partition = Partition::from([0x02u8; 16]);
    let context = Context::from([0x03u8; 16]);
    // Highly compressible content so compression actually triggers when enabled.
    let payload: Vec<u8> = vec![0xABu8; 4096];

    // Prime target (uncompressed).
    let prev_mode = COMPRESSION_MODE.swap(CompressionMode::NoCompression as u32, Ordering::AcqRel);
    let StoreResult {
        address: target_address,
        ..
    } = write_content(
        store.clone(),
        target_partition,
        context,
        Bytes::from(payload.clone()),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();

    // Prime source (compressed).
    COMPRESSION_MODE.store(CompressionMode::Zstd as u32, Ordering::Release);
    let StoreResult {
        address: source_address,
        ..
    } = write_content(
        store.clone(),
        source_partition,
        context,
        Bytes::from(payload.clone()),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();
    // Restore mode for any other tests sharing this process.
    COMPRESSION_MODE.store(prev_mode, Ordering::Release);

    assert_eq!(target_address, source_address, "same content → same hash");

    // Copy source → target with durable=false (pure local). Pass the source context as the
    // destination context so the address tuple is preserved (cross-partition copy with the
    // hash + context invariant the original test relied on).
    store
        .clone()
        .copy(
            source_partition,
            source_address,
            target_partition,
            source_address.context,
            CopyBehavior {
                durable: false,
                do_not_replicate: false,
            },
        )
        .await
        .unwrap();

    // Read from target partition: payload bytes must round-trip identically. The helper
    // must have adopted source's pack pointer AND encoding flag together — if encoding
    // and bytes were ever desynchronized, decompression in `read` would fail or return
    // garbage.
    let (_fragment, bytes) = read(
        store.clone(),
        target_partition,
        target_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_slice());
}

#[tokio::test]
async fn copy_same_partition_new_context_adopts_payload_without_transfer() {
    // Same partition, different context — the in-partition deduplication path. The destination
    // entry must adopt the source's payload pointer (no payload transfer) and the read against
    // the new `(partition, hash, target_context)` tuple must return the same bytes that were
    // originally written under the source context.
    use lore_storage::options::ReadOptions;
    use lore_storage::options::WriteOptions;
    use lore_storage::read::read;
    use lore_storage::write::StoreResult;
    use lore_storage::write::write_content;

    let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
        None::<&Path>,
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .unwrap();

    let partition = Partition::from([0xA1u8; 16]);
    let source_context = Context::from([0xB1u8; 16]);
    let target_context = Context::from([0xB2u8; 16]);
    let payload: Vec<u8> = b"in-partition new-context dedup payload".to_vec();

    // Seed the source tuple `(partition, hash, source_context)`.
    let StoreResult {
        address: source_address,
        ..
    } = write_content(
        store.clone(),
        partition,
        source_context,
        Bytes::from(payload.clone()),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(source_address.context, source_context);

    // Copy within the same partition, retagging the destination with `target_context`. The
    // store's `copy` is the only call we make — there must be no payload transfer; the
    // destination tuple gets its own entry that points at the source's payload data.
    store
        .clone()
        .copy(
            partition,
            source_address,
            partition,
            target_context,
            CopyBehavior {
                durable: false,
                do_not_replicate: false,
            },
        )
        .await
        .unwrap();

    // The destination address shares the source's hash but takes the target context.
    let destination_address = Address {
        hash: source_address.hash,
        context: target_context,
    };

    let (_fragment, bytes) = read(
        store.clone(),
        partition,
        destination_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_slice());

    // Source tuple must remain readable independently — copy creates a new entry, it does
    // not consume or repoint the source.
    let (_fragment, bytes) = read(
        store.clone(),
        partition,
        source_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_slice());
}

/// The local store is the reference implementation of the store contract: it resolves an
/// address in a single bucket pass, so it can establish every level at no extra cost and has
/// no reason to under-report any of them.
#[tokio::test]
async fn satisfies_the_immutable_store_contract() {
    let dir = lore_base::test_util::TempDir::new("is_conformance_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("create store");

    lore_storage::conformance::verify_immutable_store(
        store,
        lore_storage::conformance::Capabilities::new("LocalImmutableStore").stores_metadata_only(),
    )
    .await;
}

/// Healing a payload that no longer verifies has to leave the store answering absence, so
/// that the content is offered again.
///
/// Clearing the payload pointer alone would not: an entry still answers a full match, which
/// tells a peer the store holds content it can no longer serve, and a peer told that stops
/// offering it. The association the caller asked about and its siblings under other contexts
/// go together, since they name the one payload that failed.
///
/// A payload is shared by every association that deduplicated onto it, so one proven bad is
/// bad in whichever partition names it, and all of them go. Leaving one behind leaves that
/// partition being told the content is here until it happens to verify for itself.
#[tokio::test]
async fn healing_a_corrupt_payload_drops_the_associations_that_named_it() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_drop_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let payload = Bytes::from_static(b"a payload that will stop verifying");
    let hash = lore_storage::hash::hash_slice(payload.as_ref());
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    let partition = Partition::from([0x61u8; 16]);
    let address = Address {
        hash,
        context: Context::from([0x62u8; 16]),
    };
    let sibling = Address {
        hash,
        context: Context::from([0x63u8; 16]),
    };
    let neighbour = Partition::from([0x64u8; 16]);

    for (in_partition, at) in [
        (partition, address),
        (partition, sibling),
        (neighbour, address),
    ] {
        store
            .clone()
            .put(in_partition, at, fragment, Some(payload.clone()), false)
            .await
            .expect("put the payload");
    }

    // Corrupt the stored bytes in place, which is what verification is there to notice.
    let group_index = hash.data()[0] as usize;
    let found = store
        .clone()
        .find(partition, address, Lookup::Read)
        .await
        .expect("the entry is there to corrupt");
    store.group[group_index]
        .packstore
        .obliterate(
            found.data.pack_file,
            found.data.pack_offset,
            found.data.size_payload,
        )
        .await
        .expect("overwrite the payload");

    let result = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, true)
        .await
        .expect("verify answers");
    assert!(
        result.healed,
        "a payload that does not verify was not healed"
    );

    for gone in [address, sibling] {
        let resolved = lore_storage::immutable_store::query_one(
            &(store.clone() as Arc<dyn ImmutableStore>),
            partition,
            gone,
        )
        .await
        .expect("query answers");
        assert_eq!(
            resolved.match_made,
            StoreMatch::MatchNone,
            "a healed association still answers a match, so nothing will offer {gone} again"
        );
    }

    let resolved = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        neighbour,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        resolved.match_made,
        StoreMatch::MatchNone,
        "another partition was left naming the payload this store proved it cannot serve"
    );
}

/// An entry that names no payload is the wedged state itself, and verifying has to be able
/// to undo it.
///
/// Nothing failed here - there is no payload to fail - so this is the state left behind by
/// whatever dropped one, and left alone it is permanent: the entry answers a full match, the
/// store is asked for bytes it does not have, and the peer that holds them is told not to
/// send them. Reporting it as verified is what makes it permanent, so it counts as a failure
/// and heals the same way.
#[tokio::test]
async fn verifying_an_entry_with_no_payload_reports_it_and_heals_the_wedge() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_wedge_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let payload = Bytes::from_static(b"bytes this store will never hold");
    let hash = lore_storage::hash::hash_slice(payload.as_ref());
    let partition = Partition::from([0x71u8; 16]);
    let address = Address {
        hash,
        context: Context::from([0x72u8; 16]),
    };

    // A header with no payload, which is what an entry left behind by a drop looks like.
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            None,
            false,
        )
        .await
        .expect("put the header alone");

    let before = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        before.match_made,
        StoreMatch::MatchFull,
        "the wedge this heals is an entry with no payload answering a full match"
    );

    let reported = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, false)
        .await
        .expect("verify answers");
    assert!(
        reported.verification_result.is_err(),
        "an entry the store cannot serve was reported as verified"
    );
    assert!(!reported.healed, "verify healed without being asked to");

    let healed = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, true)
        .await
        .expect("verify answers");
    assert!(healed.healed, "the wedged entry was not healed");

    let after = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        after.match_made,
        StoreMatch::MatchNone,
        "the entry survived healing, so the address stays wedged"
    );
}

/// A flush without syncing replaces a bucket's file rather than rewriting it, so that a process
/// stopped during the write leaves the file it last wrote, as a hard link to that file shows.
#[tokio::test]
async fn an_unsynced_flush_replaces_the_bucket_file() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_unsynced_replace_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x5au8; 16]);
    let (group_index, payloads) = payloads_in_one_group(2);
    let bucket_path = format_bucket_path(&dir.join("immutable"), group_index, 0);
    put_payload(&store, partition, Context::default(), &payloads[0], false).await;
    store.clone().flush(FlushOptions::default()).await.unwrap();
    let written = std::fs::read(&bucket_path).unwrap();
    let link = dir.join("last_written");
    std::fs::hard_link(&bucket_path, &link).unwrap();

    put_payload(&store, partition, Context::default(), &payloads[1], false).await;
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

/// Healing has to survive a reopen, which means the emptied bucket reaching disk.
///
/// Dropping the last entry a bucket holds leaves nothing to write, and a flush that treats
/// that as nothing to do leaves the file the bucket was last written to in place. The store
/// then reports the association again the next time it opens, healed in memory only.
#[tokio::test]
async fn healing_the_last_entry_in_a_bucket_survives_reopening_the_store() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_reopen_");
    let path = std::path::PathBuf::from(dir.as_ref());

    let payload = Bytes::from_static(b"a payload that will not survive its own bucket");
    let hash = lore_storage::hash::hash_slice(payload.as_ref());
    let partition = Partition::from([0x91u8; 16]);
    let address = Address {
        hash,
        context: Context::from([0x92u8; 16]),
    };

    {
        let store = LocalImmutableStore::new(
            Some(path.clone()),
            ImmutableStoreSettings {
                isolate_partitions: true,
                ..Default::default()
            },
        )
        .await
        .expect("create store");

        store
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put the payload");

        // On disk before anything goes wrong, so the file healing has to account for exists.
        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flush the bucket");

        let group_index = hash.data()[0] as usize;
        let found = store
            .clone()
            .find(partition, address, Lookup::Read)
            .await
            .expect("the entry is there to corrupt");
        store.group[group_index]
            .packstore
            .obliterate(
                found.data.pack_file,
                found.data.pack_offset,
                found.data.size_payload,
            )
            .await
            .expect("overwrite the payload");

        let healed = store
            .clone()
            .verify_fragment(address, partition, StoreMatch::MatchFull, true)
            .await
            .expect("verify answers");
        assert!(healed.healed, "the corrupt payload was not healed");

        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flush the removal");
    }

    let reopened = LocalImmutableStore::new(
        Some(path),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("reopen store");

    let resolved = lore_storage::immutable_store::query_one(
        &(reopened as Arc<dyn ImmutableStore>),
        partition,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        resolved.match_made,
        StoreMatch::MatchNone,
        "reopening the store loaded back the association healing removed"
    );
}

/// An obliterated fragment is meant to hold no payload, so verifying one reports nothing
/// wrong and heals nothing.
///
/// A tombstone is indistinguishable from a wedged entry by its pack file alone - both name
/// none - so what separates them is the flag. Reporting a deletion as corruption would put
/// an operator onto a fault that is not there, and healing one would flush the bucket and
/// claim a repair having changed nothing.
#[tokio::test]
async fn verifying_an_obliterated_fragment_reports_nothing_and_heals_nothing() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_tombstone_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let payload = Bytes::from_static(b"content that is about to be obliterated");
    let partition = Partition::from([0x81u8; 16]);
    let address = Address {
        hash: lore_storage::hash::hash_slice(payload.as_ref()),
        context: Context::from([0x82u8; 16]),
    };

    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(payload),
            false,
        )
        .await
        .expect("put the payload");

    store
        .clone()
        .obliterate(
            partition,
            address,
            Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
        )
        .await
        .expect("obliterate the fragment");

    let tombstone = store
        .clone()
        .find(partition, address, Lookup::Read)
        .await
        .expect("the tombstone is in the index");
    assert_eq!(tombstone.matching, StoreMatch::MatchFull);
    assert_ne!(
        tombstone.data.flags & FragmentFlags::PayloadObliterated.bits(),
        0,
        "obliterate did not leave a tombstone to verify against"
    );

    let reported = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, false)
        .await
        .expect("verify answers");
    assert!(
        reported.verification_result.is_ok(),
        "an intentional deletion was reported as a fault"
    );

    let healed = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, true)
        .await
        .expect("verify answers");
    assert!(
        !healed.healed,
        "healing claimed a repair on a fragment that is meant to hold no payload"
    );

    let after = store
        .clone()
        .find(partition, address, Lookup::Read)
        .await
        .expect("the tombstone is still in the index");
    assert_eq!(
        after.matching,
        StoreMatch::MatchFull,
        "healing took the tombstone with it"
    );
    assert_ne!(
        after.data.flags & FragmentFlags::PayloadObliterated.bits(),
        0,
        "healing cleared the tombstone's flag"
    );
}

/// A store that isolates partitions reports further than it reads, and this is the only
/// implementation of that split.
///
/// A sibling context in the same partition is a partition match, which `query` reports so a
/// caller can duplicate the association with a copy, and which `get` refuses so that nothing
/// crossing a wire without its level is mistaken for an association of the caller's own. The
/// battery bounds the reported level from above and cannot assert this, because a store that
/// resolves associations alone is entitled to answer `MatchNone` here instead.
#[tokio::test]
async fn an_isolating_store_reports_further_than_it_reads() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_scope_split_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let partition = Partition::from([0x51u8; 16]);
    let payload = Bytes::from_static(b"one hash, two contexts, one partition");
    let stored = Address {
        hash: lore_storage::hash::hash_slice(payload.as_ref()),
        context: Context::from([0x52u8; 16]),
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    store
        .clone()
        .put(partition, stored, fragment, Some(payload), false)
        .await
        .expect("put under the storing context");

    let sibling = Address {
        hash: stored.hash,
        context: Context::from([0x53u8; 16]),
    };

    let resolved = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        sibling,
    )
    .await
    .expect("query a sibling context");
    assert_eq!(
        resolved.match_made,
        StoreMatch::MatchPartition,
        "an isolating store must still report the partition match a copy would act on"
    );
    assert_eq!(resolved.partition, partition);

    assert!(
        store
            .clone()
            .get_metadata(partition, sibling)
            .await
            .expect("get_metadata answers rather than failing")
            .match_made
            == StoreMatch::MatchNone,
        "an isolating store described an association it does not hold"
    );
    assert!(
        store.clone().get(partition, sibling).await.is_err(),
        "an isolating store served a sibling context's payload"
    );
}

/// An entry stored without its payload holds nothing locally, whatever flags it was stored with:
/// `get_metadata` reports no `PayloadStoredLocal`, as `query` reports no `stored_local`.
#[tokio::test]
async fn an_entry_stored_without_its_payload_is_not_reported_local() {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(None, ImmutableStoreSettings::default())
        .await
        .expect("an in-memory store");
    let partition = Partition::from([0x71u8; 16]);
    let payload = Bytes::from_static(b"the payload the entry is stored without");
    let address = Address {
        hash: hash::hash_slice(&payload),
        context: Context::from([0x71u8; 16]),
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    store
        .clone()
        .put(partition, address, fragment, None, false)
        .await
        .expect("the entry is stored without its payload");

    let metadata = store
        .clone()
        .get_metadata(partition, address)
        .await
        .expect("get_metadata answers");
    assert_ne!(
        metadata.match_made,
        StoreMatch::MatchNone,
        "the entry is indexed"
    );
    assert_eq!(
        metadata.fragment.flags & FragmentFlags::PayloadStoredLocal,
        0,
        "get_metadata reported a payload the entry does not hold"
    );
    let resolved = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        address,
    )
    .await
    .expect("query the entry");
    assert!(!resolved.stored_local);
}

/// The source forms `copy` accepts: an exact association, and any association a partition
/// holds. A caller acting on a partition match only ever has the second.
mod copy_source {
    use lore_storage::immutable_store::ImmutableStore;

    use super::*;

    type Store = Arc<dyn ImmutableStore>;

    async fn store_with(entries: &[(Partition, Context)], payload: &[u8]) -> (Store, Address) {
        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let address = Address {
            hash: lore_storage::hash::hash_slice(payload),
            context: Context::default(),
        };
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        for (partition, context) in entries {
            store
                .clone()
                .put(
                    *partition,
                    Address {
                        hash: address.hash,
                        context: *context,
                    },
                    fragment,
                    Some(Bytes::copy_from_slice(payload)),
                    false,
                )
                .await
                .expect("seed association");
        }
        (store, address)
    }

    async fn readable(
        store: &Store,
        partition: Partition,
        address: Address,
        payload: &[u8],
    ) -> bool {
        lore_storage::read::read(
            store.clone(),
            partition,
            address,
            None,
            lore_storage::options::ReadOptions::default(),
            None,
        )
        .await
        .is_ok_and(|(_fragment, bytes)| bytes.as_ref() == payload)
    }

    #[tokio::test]
    async fn a_zero_context_takes_any_association_in_the_partition() {
        let payload = b"zero context names any association".as_slice();
        let partition = Partition::from([0x11u8; 16]);
        let held = Context::from([0x12u8; 16]);
        let wanted = Context::from([0x13u8; 16]);
        let (store, address) = store_with(&[(partition, held)], payload).await;

        store
            .clone()
            .copy(
                partition,
                Address::zero_context_hash(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("a partition holding the hash must answer a source naming no context");

        assert!(
            readable(
                &store,
                partition,
                Address {
                    hash: address.hash,
                    context: wanted
                },
                payload
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_zero_context_crosses_partitions() {
        let payload = b"zero context across partitions".as_slice();
        let source = Partition::from([0x21u8; 16]);
        let destination = Partition::from([0x22u8; 16]);
        let held = Context::from([0x23u8; 16]);
        let wanted = Context::from([0x24u8; 16]);
        let (store, address) = store_with(&[(source, held)], payload).await;

        store
            .clone()
            .copy(
                source,
                Address::zero_context_hash(address.hash),
                destination,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("copy from a source partition naming no context");

        assert!(
            readable(
                &store,
                destination,
                Address {
                    hash: address.hash,
                    context: wanted
                },
                payload
            )
            .await
        );
    }

    /// The partition is still the boundary: naming no context widens the search inside one
    /// partition, never across them.
    #[tokio::test]
    async fn a_zero_context_does_not_reach_another_partition() {
        let payload = b"zero context stays in its partition".as_slice();
        let held_in = Partition::from([0x31u8; 16]);
        let asked_of = Partition::from([0x32u8; 16]);
        let (store, address) = store_with(&[(held_in, Context::from([0x33u8; 16]))], payload).await;

        let err = store
            .clone()
            .copy(
                asked_of,
                Address::zero_context_hash(address.hash),
                Partition::from([0x34u8; 16]),
                Context::from([0x35u8; 16]),
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect_err("a partition holding nothing has no association to name");
        assert!(matches!(err, StoreError::AddressNotFound(_)));
    }

    /// A named context is resolved exactly. A sibling holding the same hash is not a fallback,
    /// which is the whole difference between the two forms.
    #[tokio::test]
    async fn a_named_context_does_not_widen_to_a_sibling() {
        let payload = b"an exact source is exact".as_slice();
        let partition = Partition::from([0x41u8; 16]);
        let (store, address) =
            store_with(&[(partition, Context::from([0x42u8; 16]))], payload).await;

        let err = store
            .clone()
            .copy(
                partition,
                Address {
                    hash: address.hash,
                    context: Context::from([0x43u8; 16]),
                },
                partition,
                Context::from([0x44u8; 16]),
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect_err("a context the partition does not hold must not resolve to a sibling");
        assert!(matches!(err, StoreError::AddressNotFound(_)));
    }

    /// Obliterating a fragment tree must terminate when a child shares its
    /// parent's bucket.
    ///
    /// Obliterating an address takes the write lock on the bucket that address
    /// lives in, and a child chooses its own bucket from its own hash.
    /// `tokio::sync::RwLock` is not reentrant, so a child that lands in the
    /// bucket its parent is holding used to wait on a lock the same task
    /// already owned, and the obliterate never returned. At one bucket to a
    /// group - where a client store starts - every child in the parent's group
    /// collides, which is one child in 256; a 3.4 MB file of 53 chunks hung one
    /// run in five.
    ///
    /// The collision is searched for rather than written down because both
    /// hashes are content-derived: the first byte chooses the group, and with
    /// one bucket in it the group is the bucket.
    #[tokio::test]
    async fn a_child_in_its_parent_bucket_does_not_deadlock_the_obliterate() {
        let partition = Partition::from([0x61u8; 16]);
        let context = Context::from([0x62u8; 16]);

        let (payload, leaf_hash, root_hash, references) = (0u32..)
            .find_map(|salt| {
                // Longer than its list's one reference, which a list describes no more bytes
                // than.
                let payload =
                    format!("leaf payload {salt}, the content of a one-leaf list").into_bytes();
                let leaf_hash = lore_storage::hash::hash_slice(&payload);
                let references = vec![FragmentReference {
                    hash: leaf_hash,
                    offset_content: 0,
                }];
                let root_hash = lore_storage::hash::hash_slice(references.as_bytes());
                (root_hash.data()[0] == leaf_hash.data()[0])
                    .then_some((payload, leaf_hash, root_hash, references))
            })
            .expect("a leaf hashing into its own list's group");

        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");

        store
            .clone()
            .put(
                partition,
                Address {
                    hash: leaf_hash,
                    context,
                },
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(Bytes::copy_from_slice(&payload)),
                false,
            )
            .await
            .expect("put leaf");

        let references = Bytes::copy_from_slice(references.as_bytes());
        store
            .clone()
            .put(
                partition,
                Address {
                    hash: root_hash,
                    context,
                },
                Fragment {
                    flags: FragmentFlags::PayloadFragmented.bits(),
                    size_payload: references.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(references),
                false,
            )
            .await
            .expect("put fragment list");

        let stats = Arc::new(lore_storage::store_types::StoreObliterateStats::default());
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            store.clone().obliterate(
                partition,
                Address {
                    hash: root_hash,
                    context,
                },
                stats.clone(),
            ),
        )
        .await
        .expect("obliterating a tree whose child shares its parent's bucket must terminate")
        .expect("obliterate");

        assert_eq!(
            stats.num_fragments.load(atomic::Ordering::Relaxed),
            2,
            "both the list and the leaf it references are fragments"
        );

        let addresses = [
            Address {
                hash: root_hash,
                context,
            },
            Address {
                hash: leaf_hash,
                context,
            },
        ];
        let mut results = [StoreMatchResult::default(); 2];
        store
            .clone()
            .query(partition, &addresses, &mut results)
            .await
            .expect("query");
        for result in results {
            assert_eq!(
                result.match_made,
                StoreMatch::MatchNone,
                "an obliterated address must not resolve"
            );
        }
    }

    /// A tombstone is not a representation to adopt, so the walk passes over it and copies the
    /// live association beside it.
    #[tokio::test]
    async fn a_zero_context_skips_an_obliterated_association() {
        let payload = b"one obliterated reference, one alive".as_slice();
        let partition = Partition::from([0x51u8; 16]);
        let doomed = Context::from([0x52u8; 16]);
        let alive = Context::from([0x53u8; 16]);
        let wanted = Context::from([0x54u8; 16]);
        let (store, address) =
            store_with(&[(partition, doomed), (partition, alive)], payload).await;

        store
            .clone()
            .obliterate(
                partition,
                Address {
                    hash: address.hash,
                    context: doomed,
                },
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate one reference");

        store
            .clone()
            .copy(
                partition,
                Address::zero_context_hash(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("the surviving association is the one to copy from");

        assert!(
            readable(
                &store,
                partition,
                Address {
                    hash: address.hash,
                    context: wanted
                },
                payload
            )
            .await
        );
    }

    #[tokio::test]
    async fn an_obliterated_source_is_not_copied() {
        let payload = b"the only reference is obliterated".as_slice();
        let partition = Partition::from([0x61u8; 16]);
        let doomed = Context::from([0x62u8; 16]);
        let (store, address) = store_with(&[(partition, doomed)], payload).await;

        let source = Address {
            hash: address.hash,
            context: doomed,
        };
        store
            .clone()
            .obliterate(
                partition,
                source,
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate the only reference");

        for named in [source, Address::zero_context_hash(address.hash)] {
            let err = store
                .clone()
                .copy(
                    partition,
                    named,
                    partition,
                    Context::from([0x63u8; 16]),
                    CopyBehavior {
                        durable: false,
                        do_not_replicate: false,
                    },
                )
                .await
                .expect_err("a tombstone is not an association to copy from");
            assert!(matches!(err, StoreError::AddressNotFound(_)));
        }
    }

    /// A hash the partition holds only the representation of. The walk records it as the
    /// fallback rather than passing over it, so the copy still registers the destination — as it
    /// does for an exact source that has no payload either.
    #[tokio::test]
    async fn a_zero_context_falls_back_to_an_association_without_its_payload() {
        let payload = b"representation held without its payload".as_slice();
        let partition = Partition::from([0x81u8; 16]);
        let held = Context::from([0x82u8; 16]);
        let wanted = Context::from([0x83u8; 16]);

        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let address = Address {
            hash: lore_storage::hash::hash_slice(payload),
            context: held,
        };
        store
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                None,
                false,
            )
            .await
            .expect("seed the representation alone");

        store
            .clone()
            .copy(
                partition,
                Address::zero_context_hash(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("the representation alone is still a source");

        let resolved = lore_storage::immutable_store::query_one(
            &store,
            partition,
            Address {
                hash: address.hash,
                context: wanted,
            },
        )
        .await
        .expect("query the destination");
        assert_eq!(resolved.match_made, StoreMatch::MatchFull);
    }

    /// A `query` naming a context hands back a source `copy` resolves exactly, which is the
    /// pairing the write path relies on to avoid the wider search.
    #[tokio::test]
    async fn a_partition_match_names_a_context_copy_resolves_exactly() {
        let payload = b"query names the association copy reads".as_slice();
        let partition = Partition::from([0x71u8; 16]);
        let held = Context::from([0x72u8; 16]);
        let wanted = Context::from([0x73u8; 16]);
        let (store, address) = store_with(&[(partition, held)], payload).await;

        let resolved = lore_storage::immutable_store::query_one(
            &store,
            partition,
            Address {
                hash: address.hash,
                context: wanted,
            },
        )
        .await
        .expect("query a sibling context");
        assert_eq!(resolved.match_made, StoreMatch::MatchPartition);
        assert_eq!(resolved.context, held);

        store
            .clone()
            .copy(
                resolved.partition,
                resolved.source_address(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("the source a match named must be one copy resolves");
    }
}

/// An obliterated association stays obliterated whatever is stored under the others.
mod obliteration {
    use lore_storage::immutable_store::ImmutableStore;
    use lore_storage::immutable_store::claim_for_task;

    use super::*;

    fn fragment(payload: &[u8]) -> Fragment {
        Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        }
    }

    async fn put(store: &Arc<dyn ImmutableStore>, partition: Partition, payload: &[u8]) -> Address {
        let address = Address {
            hash: lore_storage::hash::hash_slice(payload),
            context: Context::default(),
        };
        store
            .clone()
            .put(
                partition,
                address,
                fragment(payload),
                Some(Bytes::copy_from_slice(payload)),
                false,
            )
            .await
            .expect("put");
        address
    }

    async fn obliterate(store: &Arc<dyn ImmutableStore>, partition: Partition, address: Address) {
        store
            .clone()
            .obliterate(
                partition,
                address,
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate");
    }

    /// Storing content under a new association points every entry of its hash at the payload,
    /// except a tombstone: the obliterated association reads as not found, and the content can
    /// still be stored under further associations, rather than colliding with the tombstone's
    /// empty sizes. With every association obliterated the payload is zeroed, and the next store
    /// writes a new one.
    #[tokio::test]
    async fn a_store_under_another_partition_leaves_a_tombstone_obliterated() {
        for zeroed in [false, true] {
            let store = create(
                None::<&Path>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("create store");
            let payload = b"content one association of which is obliterated".repeat(8);
            let obliterated = Partition::from([0x71u8; 16]);
            let kept = Partition::from([0x72u8; 16]);
            let address = put(&store, obliterated, &payload).await;
            put(&store, kept, &payload).await;
            obliterate(&store, obliterated, address).await;
            if zeroed {
                obliterate(&store, kept, address).await;
            }

            put(&store, Partition::from([0x73u8; 16]), &payload).await;

            assert!(
                matches!(
                    store.clone().get(obliterated, address).await,
                    Err(StoreError::AddressNotFound(_))
                ),
                "the obliterated association reads again, payload zeroed {zeroed}"
            );
            let mut result = [StoreMatchResult::default()];
            store
                .clone()
                .query(obliterated, &[address], &mut result)
                .await
                .expect("query");
            assert_eq!(result[0].match_made, StoreMatch::MatchNone);
            put(&store, Partition::from([0x74u8; 16]), &payload).await;
        }
    }

    /// A put without its payload over a tombstone is refused, whatever flags it carries, and
    /// leaves the tombstone as it was, so that the payload is zeroed once the last live
    /// association of its hash is obliterated. A put with its payload replaces the tombstone.
    #[tokio::test]
    async fn only_a_put_with_its_payload_replaces_a_tombstone() {
        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let payload = b"content one association of which is put again once obliterated".repeat(4);
        let tombstoned = Partition::from([0x7bu8; 16]);
        let live = Partition::from([0x7cu8; 16]);
        let address = put(&store, tombstoned, &payload).await;
        put(&store, live, &payload).await;
        obliterate(&store, tombstoned, address).await;

        for flags in [0, FragmentFlags::PayloadStoredDurable.bits()] {
            let refused = store
                .clone()
                .put(
                    tombstoned,
                    address,
                    Fragment {
                        flags,
                        ..fragment(&payload)
                    },
                    None,
                    false,
                )
                .await;
            assert!(
                matches!(refused, Err(StoreError::PayloadNotFound(_))),
                "{flags:#x}: {refused:?}"
            );
        }
        let stats = Arc::new(lore_storage::store_types::StoreObliterateStats::default());
        store
            .clone()
            .obliterate(live, address, stats.clone())
            .await
            .expect("obliterate");
        assert_eq!(stats.num_payloads.load(atomic::Ordering::Relaxed), 1);

        put(&store, tombstoned, &payload).await;
        let (fragment, read_back) = store
            .clone()
            .get(tombstoned, address)
            .await
            .and_then(lore_storage::StoreGetData::into_payload)
            .expect("the put with its payload replaces the tombstone");
        assert_eq!(fragment.flags & FragmentFlags::PayloadObliterated.bits(), 0);
        assert_eq!(read_back.as_ref(), payload.as_slice());
        assert!(matches!(
            store.clone().get(live, address).await,
            Err(StoreError::AddressNotFound(_))
        ));
    }

    async fn store_over(dir: &Path) -> (Arc<LocalImmutableStore>, Arc<dyn ImmutableStore>) {
        let store = LocalImmutableStore::new(
            Some(dir.to_path_buf()),
            ImmutableStoreSettings {
                flush_delay_seconds: 3600,
                ..client_settings()
            },
        )
        .await
        .expect("store opens");
        let dyn_store: Arc<dyn ImmutableStore> = store.clone();
        (store, dyn_store)
    }

    /// An obliterate writes the tombstone to disk before it zeroes the payload: a process killed
    /// while the tombstone is written reopens with the payload whole, and one killed once the
    /// obliterate returns reopens with the address obliterated, rather than serving zeros.
    #[tokio::test]
    async fn the_tombstone_is_on_disk_before_the_payload_is_zeroed() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_order_");
        let (_store, dyn_store) = store_over(dir.path()).await;
        let payload = b"content to be obliterated".repeat(16);
        let partition = Partition::from([0x75u8; 16]);
        let address = put(&dyn_store, partition, &payload).await;
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");

        let pause = pause_at(PausePoint::Serializing);
        let obliterating = dyn_store.clone();
        let obliterate_task = lore_base::lore_spawn!(async move {
            obliterate(&obliterating, partition, address).await;
        });
        pause.reached.notified().await;
        let writing = lore_base::test_util::TempDir::new("is_obliterate_order_writing_");
        copy_tree(dir.path(), writing.path());
        pause.resume.notify_one();
        obliterate_task.await.expect("obliterate task");
        let (_reopened, reopened) = store_over(writing.path()).await;
        let (_fragment, read_back) = reopened
            .get(partition, address)
            .await
            .and_then(lore_storage::StoreGetData::into_payload)
            .expect("the payload reads back while its tombstone is written");
        assert_eq!(read_back.as_ref(), payload.as_slice());

        let killed = lore_base::test_util::TempDir::new("is_obliterate_order_killed_");
        copy_tree(dir.path(), killed.path());
        let (_reopened, reopened) = store_over(killed.path()).await;
        match reopened.get(partition, address).await {
            Err(StoreError::AddressNotFound(_)) => {}
            Ok(data) => panic!(
                "the live entry survived the obliterate on disk, serving {} zero bytes",
                data.payload.map_or(0, |payload| payload
                    .iter()
                    .filter(|byte| **byte == 0)
                    .count())
            ),
            Err(err) => panic!("the obliterated address reads as {err}"),
        }
    }

    /// An obliterate dropped while it writes the tombstone leaves the write running, and the store
    /// stays claimed until the write ends. The obliterate's claim goes with it, and the write has
    /// claimed the bucket's flag and has no file write in flight yet, so nothing but the write
    /// itself keeps the store.
    #[tokio::test]
    async fn a_dropped_obliterate_keeps_the_store_claimed_while_its_tombstone_is_written() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_dropped_");
        let (store, dyn_store) = store_over(dir.path()).await;
        let partition = Partition::from([0x77u8; 16]);
        let address = put(
            &dyn_store,
            partition,
            &b"content to be obliterated".repeat(16),
        )
        .await;
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");

        let pause = pause_at(PausePoint::Serializing);
        let obliterating = dyn_store.clone();
        let obliterate_task = lore_base::lore_spawn!(async move {
            obliterate(&obliterating, partition, address).await;
        });
        pause.reached.notified().await;
        obliterate_task.abort();
        assert!(obliterate_task.await.unwrap_err().is_cancelled());
        let lock = store.lock.clone().expect("a disk-backed store has a lock");
        assert!(
            lock.is_held(),
            "the tombstone write a dropped obliterate left running keeps the store claimed"
        );

        pause.resume.notify_one();
        wait_for(
            async || !lock.is_held(),
            "the flock goes once the tombstone write ends",
        )
        .await;
    }

    /// An obliterate dropped before its tombstone's bucket write claims the bucket's flag leaves the
    /// flock to the flag, and the write releases it once it has written the bucket and ended.
    #[tokio::test]
    async fn a_bucket_write_left_by_a_dropped_obliterate_releases_the_flock_once_it_ends() {
        let dir = lore_base::test_util::TempDir::new("is_release_after_dropped_obliterate_");
        let (store, dyn_store) = store_over(dir.path()).await;
        let partition = Partition::from([0x78u8; 16]);
        let address = put(
            &dyn_store,
            partition,
            &b"content to be obliterated".repeat(16),
        )
        .await;
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");
        let group = &store.group[address.hash.data()[0] as usize];
        let lock = store.lock.clone().expect("a disk-backed store has a lock");

        let flushing = group.flush_lock.clone().lock_owned().await;
        let obliterating = dyn_store.clone();
        let obliterate_task = lore_base::lore_spawn!(async move {
            obliterate(&obliterating, partition, address).await;
        });
        wait_until(
            || {
                group
                    .dirty
                    .iter()
                    .any(|flag| flag.load(atomic::Ordering::Relaxed))
            },
            &obliterate_task,
        )
        .await;
        obliterate_task.abort();
        assert!(obliterate_task.await.unwrap_err().is_cancelled());
        assert!(lock.is_held(), "the tombstone's flag keeps the flock");

        drop(flushing);
        wait_for(
            async || !lock.is_held(),
            "the bucket write releases the flock once it ends",
        )
        .await;
    }

    /// A read that located a payload before an obliterate tombstoned its entry loads it whole: the
    /// obliterate zeroes the payload only once the reads holding the group's payload gate are done.
    #[tokio::test]
    async fn a_read_that_located_the_payload_loads_it_before_it_is_zeroed() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_read_");
        let (store, dyn_store) = store_over(dir.path()).await;
        let payload = b"content a read is loading".repeat(16);
        let partition = Partition::from([0x76u8; 16]);
        let address = put(&dyn_store, partition, &payload).await;

        // Each task holds a claim of its own, as spawned work does: one covered by the other's
        // guard would run on with no flock once that guard goes.
        let pause = pause_at(PausePoint::Located);
        let reading = dyn_store.clone();
        let claim = claim_for_task(&dyn_store, None).await;
        let read = lore_base::lore_spawn!(async move {
            let _claim = claim;
            reading.get(partition, address).await
        });
        pause.reached.notified().await;
        let obliterating = dyn_store.clone();
        let claim = claim_for_task(&dyn_store, None).await;
        let obliterate_task = lore_base::lore_spawn!(async move {
            let _claim = claim;
            obliterating
                .obliterate(
                    partition,
                    address,
                    Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
                )
                .await
        });
        let gate = &store.group[address.hash.data()[0] as usize].payload_gate;
        wait_until(|| gate.is_draining(), &obliterate_task).await;
        assert!(
            !obliterate_task.is_finished(),
            "the obliterate zeroed the payload while a read had located it"
        );
        pause.resume.notify_one();
        let (_fragment, read_back) = read
            .await
            .expect("read task")
            .and_then(lore_storage::StoreGetData::into_payload)
            .expect("the read that located the payload loads it");
        assert_eq!(read_back.as_ref(), payload.as_slice());
        obliterate_task
            .await
            .expect("obliterate task")
            .expect("obliterate");
        assert!(matches!(
            dyn_store.clone().get(partition, address).await,
            Err(StoreError::AddressNotFound(_))
        ));
    }

    /// An obliterate zeroes a payload only while no compaction runs, as compaction moves payloads
    /// and truncates packfiles, and the range zeroed has to hold this payload still.
    #[tokio::test]
    async fn an_obliterate_zeroes_only_while_no_compaction_runs() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_compaction_");
        let (store, dyn_store) = store_over(dir.path()).await;
        let (group_index, payloads) = payloads_in_one_group(8);
        let partition = Partition::from([0x77u8; 16]);
        let mut addresses = vec![];
        for payload in &payloads {
            addresses.push(put(&dyn_store, partition, payload).await);
        }
        for address in &addresses[4..] {
            obliterate(&dyn_store, partition, *address).await;
        }
        dyn_store
            .clone()
            .load_index()
            .await
            .expect("load the index");

        assert_eq!(addresses[0].hash.data()[0] as usize, group_index);
        // Each task holds a claim of its own, for the reason the test above gives.
        let pause = pause_at(PausePoint::Stopped);
        let compacting = dyn_store.clone();
        let claim = claim_for_task(&dyn_store, None).await;
        let compaction = lore_base::lore_spawn!(async move {
            let _claim = claim;
            let mut at = None;
            while let Ok(Some(next)) = compacting.clone().compact(1, at, false, None).await {
                at = Some(next);
            }
        });
        pause.reached.notified().await;
        let obliterating = dyn_store.clone();
        let target = addresses[0];
        let claim = claim_for_task(&dyn_store, None).await;
        let mut obliterate_task = lore_base::lore_spawn!(async move {
            let _claim = claim;
            obliterating
                .obliterate(
                    partition,
                    target,
                    Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
                )
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut obliterate_task)
                .await
                .is_err(),
            "the obliterate zeroed a payload while compaction was rewriting its packfile"
        );
        pause.resume.notify_one();
        compaction.await.expect("compaction task");
        obliterate_task
            .await
            .expect("obliterate task")
            .expect("obliterate");
        assert_payloads_read_back(
            &store,
            partition,
            addresses[1..4]
                .iter()
                .zip(&payloads[1..4])
                .map(|(address, payload)| (*address, payload.clone())),
        )
        .await;
    }

    /// An obliterate in a group whose fan-out a failed flush left uncommitted commits it, as the
    /// tombstone can only reach disk with the commit, rather than failing until a later flush does.
    #[tokio::test]
    async fn an_obliterate_commits_a_fan_out_left_uncommitted() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_uncommitted_");
        let settings = || ImmutableStoreSettings {
            fan_out_threshold: 2,
            flush_delay_seconds: 3600,
            ..client_settings()
        };
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn ImmutableStore> = store.clone();
        let partition = Partition::from([0x79u8; 16]);
        let (group_index, payloads) = payloads_in_one_group(4);
        let mut addresses = vec![];
        for payload in &payloads[..2] {
            addresses.push(put(&dyn_store, partition, payload).await);
        }
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");
        for payload in &payloads[2..] {
            addresses.push(put(&dyn_store, partition, payload).await);
        }
        let pending = format_bucket_path(&dir.join("immutable"), group_index, 0)
            .with_file_name(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME);
        std::fs::create_dir_all(&pending).expect("block level.pending");
        std::fs::write(pending.join("blocker"), b"x").expect("block level.pending");
        assert!(
            dyn_store
                .clone()
                .flush(FlushOptions::default())
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&pending).expect("unblock level.pending");
        let group = &store.group[group_index];
        assert_ne!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            group.committed_level.load(atomic::Ordering::Relaxed),
            "the group fanned out without committing"
        );

        obliterate(&dyn_store, partition, addresses[0]).await;

        let killed = lore_base::test_util::TempDir::new("is_obliterate_uncommitted_killed_");
        copy_tree(dir.path(), killed.path());
        let reopened = LocalImmutableStore::new(Some(killed.to_path_buf()), settings())
            .await
            .expect("store reopens");
        let reopened_dyn: Arc<dyn ImmutableStore> = reopened.clone();
        assert!(matches!(
            reopened_dyn.get(partition, addresses[0]).await,
            Err(StoreError::AddressNotFound(_))
        ));
        assert_payloads_read_back(
            &reopened,
            partition,
            addresses[1..]
                .iter()
                .copied()
                .zip(payloads[1..].iter().cloned()),
        )
        .await;
    }

    /// An obliterate dropped while it commits its group's fan-out, which a failed flush left
    /// uncommitted, leaves the commit running to its end under the group's flush lock: once the lock
    /// is free, the group is committed.
    #[tokio::test]
    async fn an_obliterate_dropped_while_it_commits_its_group_runs_the_commit_to_its_end() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_dropped_commit_");
        let settings = || ImmutableStoreSettings {
            fan_out_threshold: 2,
            flush_delay_seconds: 3600,
            ..client_settings()
        };
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn ImmutableStore> = store.clone();
        let partition = Partition::from([0x7au8; 16]);
        let (group_index, payloads) = payloads_in_one_group(4);
        let mut addresses = vec![];
        for payload in &payloads[..2] {
            addresses.push(put(&dyn_store, partition, payload).await);
        }
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");
        for payload in &payloads[2..] {
            addresses.push(put(&dyn_store, partition, payload).await);
        }
        let pending = format_bucket_path(&dir.join("immutable"), group_index, 0)
            .with_file_name(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME);
        std::fs::create_dir_all(&pending).expect("block level.pending");
        std::fs::write(pending.join("blocker"), b"x").expect("block level.pending");
        assert!(
            dyn_store
                .clone()
                .flush(FlushOptions::default())
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&pending).expect("unblock level.pending");

        let committing =
            lore_storage::test_util::pause_at(lore_storage::test_util::PausePoint::Committing);
        let obliterating = dyn_store.clone();
        let address = addresses[0];
        let obliterate = lore_base::lore_spawn!(async move {
            obliterating
                .obliterate(
                    partition,
                    address,
                    Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
                )
                .await
        });
        committing.reached.notified().await;
        obliterate.abort();
        assert!(obliterate.await.is_err(), "the obliterate was dropped");
        committing.resume.notify_one();

        let group = &store.group[group_index];
        drop(group.flush_lock.lock().await);
        assert_eq!(group.pending_level.load(atomic::Ordering::Relaxed), 0);
        assert_eq!(
            group.committed_level.load(atomic::Ordering::Relaxed),
            group.bucket_count.load(atomic::Ordering::Relaxed),
            "the dropped obliterate's commit ran to its end"
        );
    }

    /// An obliterate dropped while it writes the bucket holding its tombstone leaves the write
    /// running to its end under the group's flush lock: once the lock is free, the tombstone is on
    /// disk, and a copy of the store reopened then does not find the obliterated entry.
    #[tokio::test]
    async fn an_obliterate_dropped_while_it_writes_its_bucket_runs_the_write_to_its_end() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_dropped_write_");
        let settings = || ImmutableStoreSettings {
            flush_delay_seconds: 3600,
            ..client_settings()
        };
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn ImmutableStore> = store.clone();
        let partition = Partition::from([0x7bu8; 16]);
        let (group_index, payloads) = payloads_in_one_group(2);
        let obliterated = put(&dyn_store, partition, &payloads[0]).await;
        let kept = put(&dyn_store, partition, &payloads[1]).await;
        dyn_store
            .clone()
            .flush(FlushOptions::default())
            .await
            .expect("flush");

        let serializing =
            lore_storage::test_util::pause_at(lore_storage::test_util::PausePoint::Serializing);
        let obliterating = dyn_store.clone();
        let obliterate = lore_base::lore_spawn!(async move {
            obliterating
                .obliterate(
                    partition,
                    obliterated,
                    Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
                )
                .await
        });
        serializing.reached.notified().await;
        obliterate.abort();
        assert!(obliterate.await.is_err(), "the obliterate was dropped");
        serializing.resume.notify_one();
        drop(store.group[group_index].flush_lock.lock().await);

        let killed = lore_base::test_util::TempDir::new("is_obliterate_dropped_write_killed_");
        copy_tree(dir.path(), killed.path());
        let reopened = LocalImmutableStore::new(Some(killed.to_path_buf()), settings())
            .await
            .expect("store reopens");
        let reopened_dyn: Arc<dyn ImmutableStore> = reopened.clone();
        assert!(matches!(
            reopened_dyn.get(partition, obliterated).await,
            Err(StoreError::AddressNotFound(_))
        ));
        assert_payloads_read_back(&reopened, partition, [(kept, payloads[1].clone())]).await;
    }

    /// A fan-out commit that fails after writing its `level.pending` is one the next open's
    /// recovery completes, renaming its `.new` files over whatever is there. Until a flush rolls it
    /// forward, nothing writes a bucket file outside it: here a group's first commit fails at one
    /// rename, and an obliterate's tombstone in that bucket must survive the recovery rather than
    /// be renamed over, bringing the zeroed payload back as live.
    #[tokio::test]
    async fn a_commit_failing_after_its_pending_marker_is_rolled_forward_before_any_write() {
        let dir = lore_base::test_util::TempDir::new("is_commit_tail_fails_");
        let settings = || ImmutableStoreSettings {
            fan_out_threshold: 2,
            flush_delay_seconds: 3600,
            ..client_settings()
        };
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn ImmutableStore> = store.clone();
        let partition = Partition::from([0x7au8; 16]);
        let (group_index, payloads) = payloads_in_one_group(6);
        let mut addresses = vec![];
        for payload in &payloads {
            addresses.push(put(&dyn_store, partition, payload).await);
        }
        let level = lore_storage::local::fan_out::level_for(1, payloads.len(), 2);
        let bucket = lore_storage::local::fan_out::bucket_index_for(&addresses[0].hash, level);
        let blocked = format_bucket_path(&dir.join("immutable"), group_index, bucket);
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
        assert!(
            blocked
                .with_file_name(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME)
                .exists(),
            "the commit wrote level.pending before the rename failed"
        );
        std::fs::remove_dir_all(&blocked).expect("unblock the bucket");

        obliterate(&dyn_store, partition, addresses[0]).await;

        let killed = lore_base::test_util::TempDir::new("is_commit_tail_fails_killed_");
        copy_tree(dir.path(), killed.path());
        let reopened = LocalImmutableStore::new(Some(killed.to_path_buf()), settings())
            .await
            .expect("store reopens");
        let reopened_dyn: Arc<dyn ImmutableStore> = reopened.clone();
        match reopened_dyn.get(partition, addresses[0]).await {
            Err(StoreError::AddressNotFound(_)) => {}
            other => panic!(
                "the obliterated address came back from recovery: {:?}",
                other.map(|data| data.payload.map(|payload| payload.len()))
            ),
        }
        assert_payloads_read_back(
            &reopened,
            partition,
            addresses[1..]
                .iter()
                .copied()
                .zip(payloads[1..].iter().cloned()),
        )
        .await;
    }

    /// An obliterate in a group with no level marker yet fails when the marker cannot be written,
    /// as its tombstone would read back at another level: it writes no bucket file and leaves the
    /// payload whole.
    #[tokio::test]
    async fn an_obliterate_that_cannot_commit_its_group_level_zeroes_nothing() {
        let dir = lore_base::test_util::TempDir::new("is_obliterate_marker_");
        let (_store, dyn_store) = store_over(dir.path()).await;
        let payload = b"content in a group with no level marker".repeat(16);
        let partition = Partition::from([0x7au8; 16]);
        let address = put(&dyn_store, partition, &payload).await;
        let marker = format_bucket_path(&dir.join("immutable"), address.hash.data()[0] as usize, 0)
            .with_file_name(lore_storage::local::fan_out::MARKER_FILENAME);
        std::fs::create_dir_all(marker.join("blocker")).expect("block the level marker");

        let obliterating = dyn_store
            .clone()
            .obliterate(
                partition,
                address,
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await;

        assert!(matches!(obliterating, Err(StoreError::Internal(_))));
        let group_dir = marker
            .parent()
            .expect("the marker is in its group directory");
        assert!(
            !lore_storage::local::fan_out::group_has_buckets(group_dir)
                .await
                .expect("the group directory lists"),
            "a bucket file was written without its group's level marker"
        );
        let (_fragment, read_back) = dyn_store
            .get(partition, address)
            .await
            .and_then(lore_storage::StoreGetData::into_payload)
            .expect("the address reads back");
        assert_eq!(read_back.as_ref(), payload.as_slice());
    }

    /// A fragment list held without its payload names fragments the store cannot reach: the
    /// obliterate fails and obliterates nothing, the list included, so that once the list is stored
    /// a retry obliterates it with its fragments.
    #[tokio::test]
    async fn a_fragment_list_without_its_payload_is_not_obliterated_alone() {
        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let partition = Partition::from([0x78u8; 16]);
        let mut addresses = vec![];
        let mut references = vec![];
        let mut content = 0u64;
        for leaf in [
            b"the first leaf of a list held without its payload".as_slice(),
            b"the second leaf of that list, long enough for the list to describe".as_slice(),
        ] {
            let address = put(&store, partition, leaf).await;
            references.push(FragmentReference {
                hash: address.hash,
                offset_content: content,
            });
            content += leaf.len() as u64;
            addresses.push(address);
        }
        let list = Bytes::copy_from_slice(references.as_bytes());
        let root = Address {
            hash: hash::hash_slice(&list),
            context: Context::default(),
        };
        let list_fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: list.len() as u32,
            size_content: content,
        };
        store
            .clone()
            .put(partition, root, list_fragment, None, false)
            .await
            .expect("put of the list's representation");
        addresses.push(root);

        let refused = store
            .clone()
            .obliterate(
                partition,
                root,
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await;
        assert!(
            matches!(refused, Err(StoreError::PayloadNotFound(_))),
            "an obliterate that cannot reach the fragments a list names: {refused:?}"
        );
        let mut results = [StoreMatchResult::default(); 3];
        store
            .clone()
            .query(partition, &addresses, &mut results)
            .await
            .expect("query");
        for result in results {
            assert_eq!(
                result.match_made,
                StoreMatch::MatchFull,
                "a refused obliterate changes nothing"
            );
        }

        store
            .clone()
            .put(partition, root, list_fragment, Some(list), false)
            .await
            .expect("put of the list");
        obliterate(&store, partition, root).await;
        store
            .clone()
            .query(partition, &addresses, &mut results)
            .await
            .expect("query");
        for result in results {
            assert_eq!(
                result.match_made,
                StoreMatch::MatchNone,
                "the retry obliterates them all"
            );
        }
    }
}

/// `put` refuses a fragment no store should keep, and records whether an entry holds its payload
/// from where the payload is, not from the flags it is given.
mod put_validation {
    use lore_storage::immutable_store::ImmutableStore;
    use lore_storage::local::fan_out::BucketGroup;

    use super::*;

    #[tokio::test]
    async fn invalid_fragments_are_refused_and_not_stored() {
        use FragmentFlags as F;
        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let payload = Bytes::from(b"a payload to describe wrongly".repeat(4));
        let len = payload.len();
        let plain = Fragment {
            flags: 0,
            size_payload: len as u32,
            size_content: len as u64,
        };
        let internal: fn(&StoreError) -> bool = StoreError::is_internal;
        let oversized: fn(&StoreError) -> bool = StoreError::is_oversized;
        let cases = [
            (
                "obliterated flag",
                Fragment {
                    flags: F::PayloadObliterated.bits(),
                    ..plain
                },
                true,
                internal,
            ),
            (
                "obliterating flag",
                Fragment {
                    flags: F::PayloadObliterating.bits(),
                    ..plain
                },
                true,
                internal,
            ),
            (
                "two compressors",
                Fragment {
                    flags: (F::PayloadCompressedLZ4 | F::PayloadCompressedZstd).bits(),
                    size_content: len as u64 * 2,
                    ..plain
                },
                true,
                internal,
            ),
            (
                "compressed and fragmented",
                Fragment {
                    flags: (F::PayloadCompressedLZ4 | F::PayloadFragmented).bits(),
                    size_content: len as u64 * 2,
                    ..plain
                },
                true,
                internal,
            ),
            (
                "payload over its content",
                Fragment {
                    size_content: len as u64 - 1,
                    ..plain
                },
                true,
                internal,
            ),
            (
                "plain payload under its content",
                Fragment {
                    size_content: len as u64 + 1,
                    ..plain
                },
                true,
                internal,
            ),
            (
                "fragment list of a part of a reference",
                Fragment {
                    flags: F::PayloadFragmented.bits(),
                    size_content: len as u64 * 10,
                    ..plain
                },
                true,
                internal,
            ),
            (
                "fragment list of a part of a reference without its payload",
                Fragment {
                    flags: F::PayloadFragmented.bits(),
                    size_content: len as u64 * 10,
                    ..plain
                },
                false,
                internal,
            ),
            (
                "oversized content without a payload",
                Fragment {
                    size_content: lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 * 4,
                    ..plain
                },
                false,
                oversized,
            ),
            (
                "local without a payload, sizes disagreeing",
                Fragment {
                    flags: F::PayloadStoredLocal.bits(),
                    size_content: len as u64 + 7,
                    ..plain
                },
                false,
                internal,
            ),
        ];
        let partition = Partition::from([0x31u8; 16]);
        for (index, (name, fragment, with_payload, refused_as)) in cases.into_iter().enumerate() {
            let address = Address {
                hash: Hash::from([index as u8 + 1; 32]),
                context: Context::default(),
            };
            let stored = store
                .clone()
                .put(
                    partition,
                    address,
                    fragment,
                    with_payload.then(|| payload.clone()),
                    false,
                )
                .await;
            assert!(stored.as_ref().is_err_and(refused_as), "{name}: {stored:?}");
            let mut result = [StoreMatchResult::default()];
            store
                .clone()
                .query(partition, &[address], &mut result)
                .await
                .expect("query");
            assert_eq!(result[0].match_made, StoreMatch::MatchNone, "{name} stored");
        }
    }

    #[tokio::test]
    async fn the_local_flag_says_whether_the_entry_holds_its_payload() {
        use FragmentFlags as F;
        let store = LocalImmutableStore::new(None, ImmutableStoreSettings::default())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn ImmutableStore> = store.clone();
        let payload = b"a payload stored once and described twice".repeat(3);
        let fragment = |flags: FragmentFlags| Fragment {
            flags: flags.bits(),
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        let address = Address {
            hash: hash::hash_slice(&payload),
            context: Context::default(),
        };
        let (stored, described) = (Partition::from([0x41u8; 16]), Partition::from([0x42u8; 16]));
        dyn_store
            .clone()
            .put(
                stored,
                address,
                fragment(F::empty()),
                Some(Bytes::copy_from_slice(&payload)),
                false,
            )
            .await
            .expect("put with the payload");
        dyn_store
            .clone()
            .put(described, address, fragment(F::empty()), None, false)
            .await
            .expect("put of the representation alone");
        for partition in [stored, described] {
            let found = store
                .find(partition, address, Lookup::Read)
                .await
                .expect("find");
            assert_ne!(found.data.pack_file, 0, "the entry holds the payload");
            assert_ne!(
                found.data.flags & F::PayloadStoredLocal,
                0,
                "an entry holding its payload is local, whatever it was stored with"
            );
        }
        dyn_store
            .clone()
            .put(
                stored,
                address,
                fragment(F::PayloadStoredDurable),
                None,
                false,
            )
            .await
            .expect("put of the representation marked durable");
        let found = store
            .find(stored, address, Lookup::Read)
            .await
            .expect("find");
        assert_eq!(
            found.data.flags & F::PayloadStored,
            (F::PayloadStoredLocal | F::PayloadStoredDurable).bits(),
            "an entry marked durable and holding its payload stays local"
        );

        let other = Address {
            hash: Hash::from([0x43u8; 32]),
            context: Context::default(),
        };
        dyn_store
            .clone()
            .put(
                stored,
                other,
                fragment(F::PayloadStoredLocal | F::PayloadStoredDurable),
                None,
                false,
            )
            .await
            .expect("put of a representation claimed local");
        let found = store.find(stored, other, Lookup::Read).await.expect("find");
        assert_eq!(found.data.pack_file, 0);
        assert_eq!(
            found.data.flags & F::PayloadStored,
            F::PayloadStoredDurable.bits(),
            "an entry without its payload is not local, whatever it was stored with"
        );
    }

    /// An entry an earlier build stored holding its payload without `PayloadStoredLocal` reads back
    /// local through `get` and `get_into` once the store is reopened.
    #[tokio::test]
    async fn an_entry_stored_without_the_local_flag_reads_back_local_after_a_reopen() {
        use FragmentFlags as F;
        let dir = lore_base::test_util::TempDir::new("is_local_flag_reopen_");
        let payload = b"a payload an earlier build stored without the local flag".repeat(3);
        let partition = Partition::from([0x44u8; 16]);
        let address = Address {
            hash: hash::hash_slice(&payload),
            context: Context::default(),
        };
        {
            let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
                .await
                .expect("store opens");
            let dyn_store: Arc<dyn ImmutableStore> = store.clone();
            dyn_store
                .clone()
                .put(
                    partition,
                    address,
                    Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    },
                    Some(Bytes::copy_from_slice(&payload)),
                    false,
                )
                .await
                .expect("put");
            let group = &store.group[address.hash.data()[0] as usize];
            let (bucket_index, mut bucket) = group.bucket_for_write(&address.hash).await;
            for entry in bucket.entry.iter_mut() {
                entry.data.flags &= !F::PayloadStoredLocal.bits();
            }
            group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
            drop(bucket);
            dyn_store
                .flush(FlushOptions::SYNC_DATA)
                .await
                .expect("flush");
        }

        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store reopens");
        let found = store
            .find(partition, address, Lookup::Read)
            .await
            .expect("find");
        assert_eq!(found.data.flags & F::PayloadStoredLocal, 0);
        let dyn_store: Arc<dyn ImmutableStore> = store.clone();
        let got = dyn_store
            .clone()
            .get(partition, address)
            .await
            .expect("get");
        assert_ne!(got.fragment.flags & F::PayloadStoredLocal, 0);
        let mut buffer = vec![0u8; payload.len()];
        // SAFETY: `buffer` outlives the read and nothing else touches it until the read returns.
        let mut dst = unsafe { lore_storage::CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
        let (fragment, _read) = dyn_store
            .get_into(partition, address, &mut dst)
            .await
            .expect("get_into");
        assert_ne!(fragment.flags & F::PayloadStoredLocal, 0);
    }

    /// `put` stores the fragment shapes the workspace writes: a fragment list without its payload,
    /// a compressed payload marked durable, the cache priority and revision state bits, and the
    /// do-not-replicate behavior flag, which the store takes off before it validates. It also
    /// stores a fragment carrying a flag bit or a compressor bit this build does not know.
    #[tokio::test]
    async fn valid_fragments_are_stored() {
        use FragmentFlags as F;
        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let payload = Bytes::from(b"a payload described rightly".repeat(4));
        let len = payload.len();
        let plain = Fragment {
            flags: 0,
            size_payload: len as u32,
            size_content: len as u64,
        };
        let cases = [
            (
                "fragment list without its payload",
                Fragment {
                    flags: F::PayloadFragmented.bits(),
                    size_payload: 2 * size_of::<FragmentReference>() as u32,
                    size_content: 1 << 20,
                },
                false,
            ),
            (
                "compressed and durable",
                Fragment {
                    flags: (F::PayloadCompressedZstd | F::PayloadStoredDurable).bits(),
                    size_content: len as u64 * 3,
                    ..plain
                },
                true,
            ),
            (
                "cache priority and revision state",
                Fragment {
                    flags: (F::PayloadLocalCachePriority | F::PayloadRevisionState).bits(),
                    ..plain
                },
                true,
            ),
            (
                "do not replicate",
                Fragment {
                    flags: F::PayloadDoNotReplicate.bits(),
                    ..plain
                },
                true,
            ),
            (
                "a flag bit this build does not know",
                Fragment {
                    flags: 1 << 30,
                    ..plain
                },
                true,
            ),
            (
                "a compressor bit this build does not know",
                Fragment {
                    flags: 1 << 4,
                    size_content: len as u64 * 3,
                    ..plain
                },
                true,
            ),
        ];
        let partition = Partition::from([0x51u8; 16]);
        for (index, (name, fragment, with_payload)) in cases.into_iter().enumerate() {
            let address = Address {
                hash: Hash::from([index as u8 + 0x60; 32]),
                context: Context::default(),
            };
            store
                .clone()
                .put(
                    partition,
                    address,
                    fragment,
                    with_payload.then(|| payload.clone()),
                    false,
                )
                .await
                .unwrap_or_else(|err| panic!("{name} refused: {err:?}"));
            let mut result = [StoreMatchResult::default()];
            store
                .clone()
                .query(partition, &[address], &mut result)
                .await
                .expect("query");
            assert_eq!(
                result[0].match_made,
                StoreMatch::MatchFull,
                "{name} not stored"
            );
        }
    }
}

/// Set the last access of each entry of the group for which `stamp` names one.
async fn stamp_entries(
    store: &LocalImmutableStore,
    group_index: usize,
    stamp: impl Fn(&ImmutableStoreEntry) -> Option<u64>,
) {
    let group = &store.group[group_index];
    for bucket_index in 0..group.bucket_count.load(atomic::Ordering::Relaxed) {
        let Some(bucket) = group.try_bucket(bucket_index) else {
            continue;
        };
        for entry in bucket.write().await.entry.iter_mut() {
            if let Some(last_access) = stamp(entry) {
                entry.data.last_access = last_access;
            }
        }
    }
}

/// Size eviction takes the least recently accessed payloads first, each ranked by the newest
/// access among the entries holding it, and stops once the bucket is below its share: of five
/// payloads, the two accessed longest ago go, and the one an old entry shares with a recent one
/// stays.
#[tokio::test]
async fn size_eviction_takes_the_least_recently_accessed_payloads_first() {
    let store = LocalImmutableStore::new(None, client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x41u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    assert_eq!(
        store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed),
        1
    );
    let mut addresses = vec![];
    for payload in &payloads {
        addresses.push(put_payload(&store, partition, Context::default(), payload, true).await);
    }
    let recent = put_payload(
        &store,
        partition,
        Context::from([0x02u8; 16]),
        &payloads[4],
        true,
    )
    .await;
    stamp_entries(&store, group_index, |entry| {
        if entry.address == recent {
            return Some(50);
        }
        addresses
            .iter()
            .position(|address| *address == entry.address)
            .map(|index| [10, 20, 30, 40, 5][index])
    })
    .await;

    assert_eq!(
        store
            .clone()
            .evict_group_sized(group_index, 14_000, None, true, false)
            .await,
        (2, 2 * 4096)
    );

    let entries = group_entries(&store, group_index).await;
    let holds_payload = |address: Address| {
        entries
            .iter()
            .any(|entry| entry.address == address && entry.data.pack_file != 0)
    };
    assert!(!holds_payload(addresses[0]) && !holds_payload(addresses[1]));
    assert!(addresses[2..].iter().all(|address| holds_payload(*address)));
    assert!(holds_payload(recent));
}

/// Size eviction takes a payload only from the entries it may take it from, so a local entry
/// keeps the payload it shares with a durable one even where an index out of order lists
/// another entry between the two.
#[tokio::test]
async fn size_eviction_keeps_a_local_payload_the_index_lists_apart() {
    let store = LocalImmutableStore::new(None, client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x42u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(2);
    let durable = put_payload(
        &store,
        partition,
        Context::from([0x01u8; 16]),
        &payloads[0],
        true,
    )
    .await;
    let other = put_payload(&store, partition, Context::default(), &payloads[1], false).await;
    let local = put_payload(
        &store,
        partition,
        Context::from([0x02u8; 16]),
        &payloads[0],
        false,
    )
    .await;
    {
        let mut bucket = store.group[group_index].bucket(0).write().await;
        let position = |address: Address| {
            bucket
                .entry
                .iter()
                .position(|entry| entry.address == address)
                .unwrap() as u32
        };
        let order = [position(durable), position(other), position(local)];
        bucket.sorted_index = Default::default();
        for index in order {
            bucket.sorted_index.push(index);
        }
    }

    store
        .clone()
        .evict_group_sized(group_index, 1, None, true, false)
        .await;

    let entries = group_entries(&store, group_index).await;
    let local_entry = entries.iter().find(|entry| entry.address == local).unwrap();
    assert_ne!(
        local_entry.data.pack_file, 0,
        "the local entry keeps its payload"
    );
    assert_ne!(
        local_entry.data.flags & FragmentFlags::PayloadStoredLocal.bits(),
        0
    );
}

/// Compaction copies every payload of a packfile it can, past one it cannot copy, and goes on
/// to rewrite the next packfile: the packfile with the payload it cannot copy is kept, the
/// local payload in it moves, and the next packfile is truncated.
#[tokio::test]
async fn compaction_continues_past_a_payload_it_cannot_copy() {
    let dir = lore_base::test_util::TempDir::new("is_compact_past_failure_");
    let partition = Partition::from([0x43u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    let layout = vec![
        vec![
            (&payloads[0], true),
            (&payloads[1], true),
            (&payloads[2], false),
        ],
        vec![(&payloads[3], true), (&payloads[4], false)],
    ];
    let (store, addresses) = store_in_packfiles(dir.as_ref(), partition, &layout).await;
    let mut unreadable_hash = [0u8; 32];
    unreadable_hash[0] = group_index as u8;
    {
        let mut bucket = store.group[group_index].bucket(0).write().await;
        let index = bucket.entry.len() as u32;
        bucket.entry.push(ImmutableStoreEntry {
            address: Address {
                hash: Hash::from(unreadable_hash),
                context: Context::default(),
            },
            partition,
            data: ImmutableData {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: 7000,
                size_content: 7000,
                pack_offset: 6000,
                pack_file: 1,
                last_access: 0,
            },
        });
        bucket.sorted_index.push(index);
    }

    assert!(compact_group(&store, group_index).await);

    let sizes = store.group[group_index].packstore.sizes().await.unwrap();
    assert_eq!(sizes[0], 3 * 4096, "the packfile it cannot empty is kept");
    assert_eq!(sizes[1], 0, "the next packfile is truncated");
    let entries = group_entries(&store, group_index).await;
    assert_ne!(
        packfile_of(&entries, addresses[2]),
        1,
        "the local payload moves"
    );
    assert_payloads_read_back(
        &store,
        partition,
        [
            (addresses[2], payloads[2].clone()),
            (addresses[4], payloads[4].clone()),
        ],
    )
    .await;
}

/// Compaction sweeps every bucket of a fanned-out group: the local payloads in each read back
/// from their copies, and the durable payloads it evicts are reclaimed.
#[tokio::test]
async fn compaction_rewrites_a_packfile_spread_over_many_buckets() {
    use lore_storage::immutable_store::ImmutableStore;
    use lore_storage::local::fan_out::bucket_index_for;

    let dir = lore_base::test_util::TempDir::new("is_compact_buckets_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            initial_fan_out_level: 32,
            ..client_settings()
        },
    )
    .await
    .unwrap();
    let partition = Partition::from([0x44u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(16);
    let mut local = vec![];
    for (index, payload) in payloads.iter().enumerate() {
        let address = put_payload(
            &store,
            partition,
            Context::default(),
            payload,
            index % 2 == 0,
        )
        .await;
        if index % 2 == 1 {
            local.push((address, payload.clone()));
        }
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    store.deserialize_all_buckets().await.unwrap();
    assert_eq!(
        store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed),
        32
    );
    let mut buckets: Vec<_> = local
        .iter()
        .map(|(address, _)| bucket_index_for(&address.hash, 32))
        .collect();
    buckets.dedup();
    assert!(buckets.len() > 1, "the local payloads span buckets");

    assert!(compact_group(&store, group_index).await);

    assert_eq!(
        store.group[group_index]
            .packstore
            .total_size()
            .await
            .unwrap(),
        local.len() * 4096
    );
    assert_payloads_read_back(&store, partition, local).await;
}

/// A bucket write that fails leaves the bucket dirty for the next flush, and no temporary file.
#[tokio::test]
async fn a_failed_bucket_write_leaves_the_bucket_dirty() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_failed_write_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x45u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(2);
    put_payload(&store, partition, Context::default(), &payloads[0], false).await;
    store.clone().flush(FlushOptions::default()).await.unwrap();
    let bucket_path = format_bucket_path(&dir.join("immutable"), group_index, 0);
    std::fs::remove_file(&bucket_path).unwrap();
    std::fs::create_dir(&bucket_path).unwrap();
    std::fs::write(bucket_path.join("blocker"), b"x").unwrap();

    put_payload(&store, partition, Context::default(), &payloads[1], false).await;
    let _ = store.clone().flush(FlushOptions::default()).await;

    assert!(store.group[group_index].dirty[0].load(atomic::Ordering::Relaxed));
    let mut temporary = bucket_path.into_os_string();
    temporary.push(".tmp");
    assert!(!Path::new(&temporary).exists());
}

/// A fan-out commit that fails once a bucket's `.new` file is written leaves the bucket dirty,
/// so that the next flush writes it again, and its payload reads back once the store reopens.
#[tokio::test]
async fn a_failed_fan_out_commit_leaves_its_buckets_dirty() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_failed_commit_");
    let partition = Partition::from([0x46u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let pending = format_bucket_path(&dir.join("immutable"), group_index, 0)
        .with_file_name(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME);
    let address = {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        let address = put_payload(&store, partition, Context::default(), &payloads[0], false).await;
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::write(pending.join("blocker"), b"x").unwrap();

        assert!(store.clone().flush(FlushOptions::default()).await.is_err());
        assert!(store.group[group_index].dirty[0].load(atomic::Ordering::Relaxed));

        std::fs::remove_dir_all(&pending).unwrap();
        store.clone().flush(FlushOptions::default()).await.unwrap();
        address
    };

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, [(address, payloads[0].clone())]).await;
}

/// A group whose buckets are laid out for a fan-out that no commit recorded on disk is neither
/// evicted nor rewritten.
#[tokio::test]
async fn compaction_skips_a_group_whose_fan_out_is_not_committed() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_uncommitted_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x47u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    for (index, payload) in payloads.iter().enumerate() {
        put_payload(&store, partition, Context::default(), payload, index < 2).await;
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    store.deserialize_all_buckets().await.unwrap();
    let group = &store.group[group_index];
    group.committed_level.store(32, atomic::Ordering::Relaxed);

    assert!(compact_group(&store, group_index).await);

    assert_eq!(group.packstore.sizes().await.unwrap(), vec![4 * 4096]);
    assert!(
        group_entries(&store, group_index)
            .await
            .iter()
            .all(|entry| entry.data.pack_file != 0),
        "nothing is evicted"
    );
}

/// A packfile is kept when a bucket file cannot be written after its payloads are copied, the
/// error is returned, and the copies reach disk with the next flush, so that every payload reads
/// back once the store reopens.
#[tokio::test]
async fn compaction_keeps_a_packfile_when_a_bucket_write_fails() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_write_fails_");
    let partition = Partition::from([0x48u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let bucket_path = format_bucket_path(&dir.join("immutable"), group_index, 0);
    let mut local = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        for (index, payload) in payloads.iter().enumerate() {
            let address =
                put_payload(&store, partition, Context::default(), payload, index < 2).await;
            if index >= 2 {
                local.push((address, payload.clone()));
            }
        }
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
        std::fs::remove_file(&bucket_path).unwrap();
        std::fs::create_dir(&bucket_path).unwrap();
        std::fs::write(bucket_path.join("blocker"), b"x").unwrap();

        let failed = try_compact_group_toward(&store, group_index, 1, true).await;
        assert!(failed.is_err(), "the failed bucket write is returned");

        assert_eq!(
            store.group[group_index].packstore.sizes().await.unwrap()[0],
            4 * 4096,
            "the packfile is not truncated"
        );
        std::fs::remove_dir_all(&bucket_path).unwrap();
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, local).await;
}

/// A compaction in a group with no level marker yet, whose marker cannot be written, writes no
/// bucket file, neither in the eviction nor in the rewrite, returns the error and keeps the
/// packfile, so that every local payload reads back once the store reopens.
#[tokio::test]
async fn compaction_keeps_a_packfile_when_its_group_level_marker_cannot_be_written() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_marker_fails_");
    let settings = || ImmutableStoreSettings {
        flush_delay_seconds: 3600,
        ..client_settings()
    };
    let partition = Partition::from([0x49u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let marker = format_bucket_path(&dir.join("immutable"), group_index, 0)
        .with_file_name(lore_storage::local::fan_out::MARKER_FILENAME);
    let mut local = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
            .await
            .unwrap();
        for (index, payload) in payloads.iter().enumerate() {
            let address =
                put_payload(&store, partition, Context::default(), payload, index < 2).await;
            if index >= 2 {
                local.push((address, payload.clone()));
            }
        }
        std::fs::create_dir_all(marker.join("blocker")).unwrap();

        let failed = try_compact_group_toward(&store, group_index, 1, true).await;
        assert!(failed.is_err(), "the failed marker write is returned");

        assert_eq!(
            store.group[group_index].packstore.sizes().await.unwrap()[0],
            4 * 4096,
            "the packfile is not truncated"
        );
        assert!(
            !lore_storage::local::fan_out::group_has_buckets(marker.parent().unwrap())
                .await
                .unwrap(),
            "a bucket file was written without its group's level marker"
        );
        std::fs::remove_dir_all(&marker).unwrap();
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, local).await;
}

/// A rewrite that fails to write a bucket file ends the compaction pass and returns the error:
/// the next packfile is neither copied nor truncated.
#[tokio::test]
async fn a_failed_rewrite_ends_the_compaction_pass() {
    let dir = lore_base::test_util::TempDir::new("is_compact_failure_ends_");
    let partition = Partition::from([0x4au8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let layout = vec![
        vec![(&payloads[0], true), (&payloads[1], false)],
        vec![(&payloads[2], true), (&payloads[3], false)],
    ];
    let (store, _) = store_in_packfiles(dir.as_ref(), partition, &layout).await;
    let bucket_path = format_bucket_path(&dir.join("immutable"), group_index, 0);
    std::fs::remove_file(&bucket_path).unwrap();
    std::fs::create_dir(&bucket_path).unwrap();
    std::fs::write(bucket_path.join("blocker"), b"x").unwrap();

    let failed = try_compact_group_toward(&store, group_index, 1, true).await;
    assert!(failed.is_err(), "the failed bucket write is returned");

    assert_eq!(
        store.group[group_index].packstore.sizes().await.unwrap(),
        vec![2 * 4096, 2 * 4096, 4096],
        "only the first packfile's local payload is copied"
    );
}

/// While a group's fan-out is not committed on disk, a delayed flush writes none of its bucket
/// files, which are laid out for the committed level, and leaves the buckets dirty, so that the
/// next flush commits the fan-out and every payload reads back once the store reopens.
#[tokio::test]
async fn no_bucket_file_is_written_while_a_fan_out_is_uncommitted() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_uncommitted_fan_out_");
    let settings = || ImmutableStoreSettings {
        fan_out_threshold: 2,
        ..client_settings()
    };
    let partition = Partition::from([0x4bu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let bucket_path = format_bucket_path(&dir.join("immutable"), group_index, 0);
    let pending = bucket_path.with_file_name(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME);
    let mut addresses = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
            .await
            .unwrap();
        for payload in &payloads[..2] {
            addresses
                .push(put_payload(&store, partition, Context::default(), payload, false).await);
        }
        store.clone().flush(FlushOptions::default()).await.unwrap();
        let committed = std::fs::read(&bucket_path).unwrap();
        for payload in &payloads[2..] {
            addresses
                .push(put_payload(&store, partition, Context::default(), payload, false).await);
        }
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::write(pending.join("blocker"), b"x").unwrap();
        assert!(store.clone().flush(FlushOptions::default()).await.is_err());
        let group = &store.group[group_index];
        assert_ne!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            group.committed_level.load(atomic::Ordering::Relaxed),
            "the group fanned out without committing"
        );

        ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), group_index, Duration::ZERO)
            .await;

        assert_eq!(std::fs::read(&bucket_path).unwrap(), committed);
        assert!(!format_bucket_path(&dir.join("immutable"), group_index, 1).exists());
        assert!(group.dirty[0].load(atomic::Ordering::Relaxed));
        std::fs::remove_dir_all(&pending).unwrap();
        store.clone().flush(FlushOptions::default()).await.unwrap();
    }

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, addresses.into_iter().zip(payloads)).await;
}

/// A group whose packfiles fail to load is left out of compaction while the groups after it
/// load and compact, and its payloads read back once its packfiles can be read again.
#[tokio::test]
async fn compaction_goes_on_past_a_group_whose_packfiles_fail_to_load() {
    use lore_storage::immutable_store::ImmutableStore;

    use crate::store_fixture::payloads_in_two_groups;

    let dir = lore_base::test_util::TempDir::new("is_compact_unloadable_");
    let partition = Partition::from([0x49u8; 16]);
    let [mut first, mut second] = payloads_in_two_groups(4);
    if first.0 > second.0 {
        std::mem::swap(&mut first, &mut second);
    }
    let ((failing, failing_payloads), (loading, loading_payloads)) = (first, second);
    let mut failing_local = vec![];
    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        for (group_payloads, local) in [
            (&failing_payloads, Some(&mut failing_local)),
            (&loading_payloads, None),
        ] {
            let mut local = local;
            for (index, payload) in group_payloads.iter().enumerate() {
                let address =
                    put_payload(&store, partition, Context::default(), payload, index < 2).await;
                if index >= 2
                    && let Some(local) = local.as_mut()
                {
                    local.push((address, payload.clone()));
                }
            }
        }
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    }
    let pack = dir
        .join("immutable")
        .join("index")
        .join(format!("{failing:02x}"))
        .join("pack");
    let packfile = pack.join("1");
    let aside = pack.join("1.aside");
    std::fs::rename(&packfile, &aside).unwrap();
    std::fs::create_dir(&packfile).unwrap();
    std::fs::write(pack.join("2"), b"").unwrap();

    {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        assert!(store.clone().load_index().await.is_err());
        assert!(compact_group(&store, failing).await);
        assert!(compact_group(&store, loading).await);
        assert_eq!(
            store.group[loading].packstore.total_size().await.unwrap(),
            2 * 4096,
            "the group that loads is compacted"
        );
    }

    std::fs::remove_dir(&packfile).unwrap();
    std::fs::rename(&aside, &packfile).unwrap();
    std::fs::remove_file(pack.join("2")).unwrap();
    assert_eq!(std::fs::metadata(&packfile).unwrap().len(), 4 * 4096);
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, failing_local).await;
}

/// A client store over `dir` holding `payloads`, all in one group, the first `durable` of them
/// durable and the rest local, flushed, so that compacting the group evicts the durable ones and
/// rewrites packfile 1 down to the local ones.
async fn store_for_rewrite(
    dir: &Path,
    partition: Partition,
    payloads: &[Vec<u8>],
    durable: usize,
) -> Arc<LocalImmutableStore> {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    for (index, payload) in payloads.iter().enumerate() {
        put_payload(
            &store,
            partition,
            Context::default(),
            payload,
            index < durable,
        )
        .await;
    }
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    store
}

/// A payload written into a packfile before compaction stopped writes to it, whose entry is not
/// in the index yet, is copied by the rewrite once the entry is in, rather than truncated away
/// under the entry: compaction waits for it.
#[tokio::test]
async fn compaction_waits_for_an_entry_written_into_a_packfile_it_rewrites() {
    let dir = lore_base::test_util::TempDir::new("is_compact_unindexed_");
    let partition = Partition::from([0x31u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    let store = store_for_rewrite(&dir, partition, &payloads[..4], 2).await;

    let selected = pause_at(PausePoint::Selected);
    let compaction = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { compact_group(&store, group_index).await })
    };
    selected.reached.notified().await;

    let written = pause_at(PausePoint::Written);
    let put = {
        let store = store.clone();
        let payload = payloads[4].clone();
        lore_base::lore_spawn!(async move {
            put_payload(&store, partition, Context::default(), &payload, false).await
        })
    };
    written.reached.notified().await;
    selected.resume.notify_one();

    let gate = &store.group[group_index].payload_gate;
    wait_until(|| gate.is_draining(), &compaction).await;
    assert!(!compaction.is_finished(), "compaction waits for the entry");
    written.resume.notify_one();
    put.await.unwrap();
    assert!(compaction.await.unwrap());

    assert!(
        group_entries(&store, group_index)
            .await
            .iter()
            .filter(|entry| entry.data.pack_file != 0)
            .all(|entry| entry.data.pack_file == 2),
        "the rewrite copied the late entry's payload with the others"
    );
    assert_payloads_read_back(
        &store,
        partition,
        payloads[2..].iter().map(|payload| {
            (
                Address::zero_context_hash(hash::hash_slice(payload)),
                payload.clone(),
            )
        }),
    )
    .await;
}

/// A payload written but not yet in the index does not make its packfile look reclaimable:
/// compaction waits for the entry before it measures what the index references, and leaves the
/// fully referenced group in place.
#[tokio::test]
async fn compaction_measures_a_written_payload_once_its_entry_is_in() {
    let dir = lore_base::test_util::TempDir::new("is_compact_measure_unindexed_");
    let partition = Partition::from([0x32u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(4);
    let store = store_for_rewrite(&dir, partition, &payloads[..3], 0).await;

    let written = pause_at(PausePoint::Written);
    let put = {
        let store = store.clone();
        let payload = payloads[3].clone();
        lore_base::lore_spawn!(async move {
            put_payload(&store, partition, Context::default(), &payload, false).await
        })
    };
    written.reached.notified().await;
    let compaction = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { compact_group(&store, group_index).await })
    };

    let gate = &store.group[group_index].payload_gate;
    wait_until(|| gate.is_draining(), &compaction).await;
    assert!(!compaction.is_finished(), "compaction waits for the entry");
    written.resume.notify_one();
    put.await.unwrap();
    assert!(compaction.await.unwrap());

    assert!(
        group_entries(&store, group_index)
            .await
            .iter()
            .all(|entry| entry.data.pack_file == 1),
        "the packfile is not rewritten"
    );
    assert_eq!(
        store.group[group_index].packstore.sizes().await.unwrap(),
        vec![4 * 4096]
    );
}

/// Read `address`'s payload through `get`, or through `get_into` with `into`.
async fn read_payload(
    store: Arc<LocalImmutableStore>,
    partition: Partition,
    address: Address,
    into: bool,
) -> Result<Vec<u8>, StoreError> {
    use lore_storage::immutable_store::ImmutableStore;

    if !into {
        let (_fragment, payload) = store.get(partition, address).await?.into_payload()?;
        return Ok(payload.to_vec());
    }
    let mut buffer = vec![0u8; lore_storage::FRAGMENT_SIZE_THRESHOLD];
    // SAFETY: `buffer` outlives the read and nothing else touches it until the read returns.
    let mut dst = unsafe { lore_storage::CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let (fragment, read) = store.get_into(partition, address, &mut dst).await?;
    Ok(match read {
        lore_storage::store_types::PayloadRead::Returned(payload) => payload.to_vec(),
        lore_storage::store_types::PayloadRead::IntoBuffer => {
            buffer.truncate(fragment.size_payload as usize);
            buffer
        }
    })
}

/// A read, through `get` or `get_into`, that found its payload's location before compaction moved
/// the entry loads it before the packfile is truncated: compaction waits for the read.
#[tokio::test]
async fn compaction_waits_for_a_read_of_a_packfile_it_truncates() {
    for into in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_compact_reading_");
        let partition = Partition::from([0x33u8; 16]);
        let (group_index, payloads) = payloads_in_one_group(4);
        let store = store_for_rewrite(&dir, partition, &payloads, 2).await;

        let stopped = pause_at(PausePoint::Stopped);
        let compaction = {
            let store = store.clone();
            lore_base::lore_spawn!(async move { compact_group(&store, group_index).await })
        };
        stopped.reached.notified().await;

        let located = pause_at(PausePoint::Located);
        let read = lore_base::lore_spawn!(read_payload(
            store.clone(),
            partition,
            Address::zero_context_hash(hash::hash_slice(&payloads[2])),
            into
        ));
        located.reached.notified().await;
        stopped.resume.notify_one();

        let gate = &store.group[group_index].payload_gate;
        wait_until(|| gate.is_draining(), &compaction).await;
        assert!(
            !compaction.is_finished(),
            "compaction waits for the read before it truncates"
        );
        located.resume.notify_one();
        let read = read.await.unwrap().expect("the read loads its payload");
        assert_eq!(read, payloads[2]);
        assert!(compaction.await.unwrap());

        assert_eq!(
            store.group[group_index].packstore.sizes().await.unwrap(),
            vec![0, 2 * 4096],
            "packfile 1 is truncated once the read is done"
        );
        assert_payloads_read_back(
            &store,
            partition,
            payloads[2..].iter().map(|payload| {
                (
                    Address::zero_context_hash(hash::hash_slice(payload)),
                    payload.clone(),
                )
            }),
        )
        .await;
    }
}

/// A bucket being written to disk holds up neither a lookup nor an insert in it, and the insert
/// made during the write reaches the next one.
#[tokio::test]
async fn a_bucket_write_in_flight_holds_up_no_lookup_or_insert() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_bucket_write_in_flight_");
    let settings = || ImmutableStoreSettings {
        initial_fan_out_level: 1,
        ..client_settings()
    };
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();
    let partition = Partition::from([0x37u8; 16]);
    let (_, payloads) = payloads_in_one_group(2);
    let first = put_payload(&store, partition, Context::default(), &payloads[0], false).await;

    let serializing = pause_at(PausePoint::Serializing);
    let flush = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.flush(FlushOptions::default()).await })
    };
    serializing.reached.notified().await;

    let during = tokio::time::timeout(Duration::from_secs(10), async {
        let second = put_payload(&store, partition, Context::default(), &payloads[1], false).await;
        assert_payloads_read_back(&store, partition, [(first, payloads[0].clone())]).await;
        second
    })
    .await;
    serializing.resume.notify_one();
    let second = during.expect("the insert and the lookup do not wait for the bucket write");
    flush.await.unwrap().unwrap();

    store.clone().flush(FlushOptions::default()).await.unwrap();
    drop(store);
    let reopened = LocalImmutableStore::new(Some(dir.to_path_buf()), settings())
        .await
        .unwrap();
    assert_payloads_read_back(
        &reopened,
        partition,
        [(first, payloads[0].clone()), (second, payloads[1].clone())],
    )
    .await;
}

/// Run the operation `start` spawns until it reaches [`PausePoint::Located`], and assert that a
/// drain of `group_index`'s payload gate waits for it there. Returns what the operation returns.
async fn assert_held_while_located<T: Send + 'static>(
    store: &Arc<LocalImmutableStore>,
    group_index: usize,
    start: impl FnOnce() -> tokio::task::JoinHandle<T>,
) -> T {
    let located = pause_at(PausePoint::Located);
    let operation = start();
    located.reached.notified().await;
    let drain = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.group[group_index].payload_gate.drain().await })
    };

    let gate = &store.group[group_index].payload_gate;
    wait_until(|| gate.is_draining(), &drain).await;
    assert!(!drain.is_finished(), "the drain waits for the load");
    located.resume.notify_one();
    drain.await.unwrap();
    operation.await.unwrap()
}

/// Obliterating a fragment list holds the group's payload gate from its lookup until it has loaded
/// the list, so that a drain, as compaction runs before it truncates a packfile, waits for the
/// load.
#[tokio::test]
async fn obliterate_holds_the_payload_gate_while_it_loads_the_fragment_list() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_obliterate_gate_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x38u8; 16]);
    let leaf_payload = b"the one leaf of the list, longer than the list's one reference";
    let leaf = put_payload(&store, partition, Context::default(), leaf_payload, false).await;
    let references = [FragmentReference {
        hash: leaf.hash,
        offset_content: 0,
    }];
    let list = Address::zero_context_hash(hash::hash_slice(references.as_bytes()));
    store
        .clone()
        .put(
            partition,
            list,
            Fragment {
                flags: (FragmentFlags::PayloadFragmented | FragmentFlags::PayloadStoredLocal)
                    .bits(),
                size_payload: references.as_bytes().len() as u32,
                size_content: leaf_payload.len() as u64,
            },
            Some(Bytes::copy_from_slice(references.as_bytes())),
            false,
        )
        .await
        .unwrap();
    let group_index = list.hash.data()[0] as usize;

    assert_held_while_located(&store, group_index, || {
        let store = store.clone();
        lore_base::lore_spawn!(async move {
            store
                .obliterate(
                    partition,
                    list,
                    Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
                )
                .await
        })
    })
    .await
    .unwrap();
    assert!(
        store.clone().get(partition, leaf).await.is_err(),
        "the list loaded, and its leaf is obliterated"
    );
}

/// Verifying a fragment holds the group's payload gate from its lookup until it has loaded the
/// payloads it checks, so that a drain waits for the load.
#[tokio::test]
async fn verify_holds_the_payload_gate_while_it_loads_the_payload() {
    let dir = lore_base::test_util::TempDir::new("is_verify_gate_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x39u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let address = put_payload(&store, partition, Context::default(), &payloads[0], false).await;

    let verified = assert_held_while_located(&store, group_index, || {
        lore_base::lore_spawn!(store.clone().verify_fragment(
            address,
            partition,
            StoreMatch::MatchFull,
            false
        ))
    })
    .await
    .unwrap();
    assert!(verified.verification_result.is_ok());
}

/// A store verifying its write holds the group's payload gate from its lookup until it has loaded
/// the payload it checks, so that a drain waits for the load.
#[tokio::test]
async fn a_verified_store_holds_the_payload_gate_while_it_loads_the_payload() {
    let dir = lore_base::test_util::TempDir::new("is_verify_write_gate_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            verify_write: true,
            ..client_settings()
        },
    )
    .await
    .unwrap();
    let partition = Partition::from([0x3cu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);

    let address = assert_held_while_located(&store, group_index, || {
        let store = store.clone();
        let payload = payloads[0].clone();
        lore_base::lore_spawn!(async move {
            put_payload(&store, partition, Context::default(), &payload, false).await
        })
    })
    .await;
    assert_payloads_read_back(&store, partition, [(address, payloads[0].clone())]).await;
}

/// **A verified store succeeds once an eviction took what it stored.** A store verifying its
/// writes finds its entry again once it is in and reads the payload back, and an eviction may take
/// the entry in between, as it takes any it ranks: the store stored what it was given, and nothing
/// of it is left to verify.
#[tokio::test]
async fn a_verified_store_succeeds_once_an_eviction_took_what_it_stored() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_verify_write_evicted_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            verify_write: true,
            ..client_settings()
        },
    )
    .await
    .unwrap();
    let partition = Partition::from([0x3du8; 16]);
    let (_, payloads) = payloads_in_one_group(1);

    let verifying = pause_at(PausePoint::Verifying);
    let storing = {
        let store = store.clone();
        let payload = payloads[0].clone();
        lore_base::lore_spawn!(async move {
            try_put_payload(&store, partition, Context::default(), &payload, true, false).await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), verifying.reached.notified())
        .await
        .expect("the store verifies what it wrote");
    let evicted = store.clone().evict(1, false, None).await.unwrap();
    assert_eq!(
        evicted, 1,
        "the eviction takes the durable entry the store put in"
    );
    verifying.resume.notify_one();
    storing
        .await
        .expect("the store ends")
        .expect("a store whose entry an eviction took succeeds");
}

/// Stores of one new hash in flight together each write the payload. Every entry takes the
/// payload of the store that inserted first, the other copy stays unreferenced, and each address
/// reads the payload back.
#[tokio::test]
async fn stores_of_one_new_hash_in_flight_together_read_back() {
    let dir = lore_base::test_util::TempDir::new("is_same_hash_stores_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x3au8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let payload = payloads[0].clone();

    let written = pause_at(PausePoint::Written);
    let first = {
        let store = store.clone();
        let payload = payload.clone();
        lore_base::lore_spawn!(async move {
            put_payload(&store, partition, Context::from([1u8; 16]), &payload, false).await
        })
    };
    written.reached.notified().await;
    let second = put_payload(&store, partition, Context::from([2u8; 16]), &payload, false).await;
    written.resume.notify_one();
    let first = first.await.unwrap();

    let entries = group_entries(&store, group_index).await;
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| (entry.data.pack_file, entry.data.pack_offset) == (1, 4096)),
        "both entries take the payload the second store inserted"
    );
    assert_eq!(
        store.group[group_index].packstore.sizes().await.unwrap(),
        vec![2 * 4096],
        "the first store's copy stays in the packfile, unreferenced"
    );
    assert_payloads_read_back(
        &store,
        partition,
        [(first, payload.clone()), (second, payload)],
    )
    .await;
}

/// Compaction syncs the group's packfiles before it writes a bucket it rewrote, without holding
/// the bucket, so that a lookup and an insert in it go on during the sync, and the insert reaches
/// the next bucket write.
#[tokio::test]
async fn a_compaction_sync_holds_up_no_lookup_or_insert() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_compact_sync_");
    let partition = Partition::from([0x3bu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(5);
    let store = store_for_rewrite(&dir, partition, &payloads[..4], 2).await;

    let syncing = pause_at(PausePoint::Syncing);
    let compaction = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { compact_group(&store, group_index).await })
    };
    syncing.reached.notified().await;

    let during = tokio::time::timeout(Duration::from_secs(10), async {
        let late = put_payload(&store, partition, Context::default(), &payloads[4], false).await;
        assert_payloads_read_back(
            &store,
            partition,
            [(
                Address::zero_context_hash(hash::hash_slice(&payloads[2])),
                payloads[2].clone(),
            )],
        )
        .await;
        late
    })
    .await;
    syncing.resume.notify_one();
    let late = during.expect("the insert and the lookup do not wait for the sync");
    assert!(compaction.await.unwrap());

    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    drop(store);
    let reopened = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(
        &reopened,
        partition,
        payloads[2..4]
            .iter()
            .map(|payload| {
                (
                    Address::zero_context_hash(hash::hash_slice(payload)),
                    payload.clone(),
                )
            })
            .chain([(late, payloads[4].clone())]),
    )
    .await;
}

/// A store whose payload write fails reports the failure and leaves no entry, and the next store
/// of the payload stores it.
#[tokio::test]
async fn a_failed_payload_write_leaves_no_entry() {
    let dir = lore_base::test_util::TempDir::new("is_failed_payload_write_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x3du8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);

    lore_storage::test_util::fail_packfile_writes(1);
    let failed = try_put_payload(
        &store,
        partition,
        Context::default(),
        &payloads[0],
        false,
        false,
    )
    .await;
    assert!(failed.is_err());
    assert!(group_entries(&store, group_index).await.is_empty());

    let address = put_payload(&store, partition, Context::default(), &payloads[0], false).await;
    assert_payloads_read_back(&store, partition, [(address, payloads[0].clone())]).await;
}

/// A forced store writes the payload again, though one is stored for its hash, and the entry
/// points at the new copy.
#[tokio::test]
async fn a_forced_store_points_the_entry_at_a_new_copy() {
    let dir = lore_base::test_util::TempDir::new("is_forced_store_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x3eu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    put_payload(&store, partition, Context::default(), &payloads[0], false).await;

    let address = try_put_payload(
        &store,
        partition,
        Context::default(),
        &payloads[0],
        false,
        true,
    )
    .await
    .unwrap();

    let entries = group_entries(&store, group_index).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (entries[0].data.pack_file, entries[0].data.pack_offset),
        (1, 4096)
    );
    assert_payloads_read_back(&store, partition, [(address, payloads[0].clone())]).await;
}

/// A store giving a payload to an entry held without one changes the entry's stored flags only
/// as it inserts the payload, so that until then the entry is as it was.
#[tokio::test]
async fn an_entry_takes_new_flags_with_its_payload() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_flags_with_payload_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x3fu8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let address = Address::zero_context_hash(hash::hash_slice(&payloads[0]));
    let durable = FragmentFlags::PayloadStoredDurable.bits();
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: durable,
                size_payload: payloads[0].len() as u32,
                size_content: payloads[0].len() as u64,
            },
            None,
            false,
        )
        .await
        .unwrap();

    let written = pause_at(PausePoint::Written);
    let put = {
        let store = store.clone();
        let payload = payloads[0].clone();
        lore_base::lore_spawn!(async move {
            put_payload(&store, partition, Context::default(), &payload, false).await
        })
    };
    written.reached.notified().await;
    let entries = group_entries(&store, group_index).await;
    assert_eq!(
        (entries[0].data.flags, entries[0].data.pack_file),
        (durable, 0),
        "the entry is as it was while the payload is written"
    );
    written.resume.notify_one();
    put.await.unwrap();

    let entries = group_entries(&store, group_index).await;
    assert_eq!(
        entries[0].data.flags,
        durable | FragmentFlags::PayloadStoredLocal.bits()
    );
    assert_ne!(entries[0].data.pack_file, 0);
}

/// A durable flush syncs the payloads of the entries in each bucket file it writes, including
/// entries inserted after the flush began: a payload stored while the flush writes another bucket
/// is synced by the time the flush returns.
#[tokio::test]
async fn a_durable_flush_syncs_the_payloads_its_bucket_files_point_at() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_durable_flush_");
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            initial_fan_out_level: 32,
            flush_delay_seconds: 3600,
            ..client_settings()
        },
    )
    .await
    .unwrap();
    let partition = Partition::from([0x40u8; 16]);
    let (group_index, mut payloads) = payloads_in_one_group(16);
    payloads.sort_by_key(|payload| {
        lore_storage::local::fan_out::bucket_index_for(&hash::hash_slice(payload), 32)
    });
    let late = payloads.pop().unwrap();
    put_payload(&store, partition, Context::default(), &payloads[0], false).await;
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    put_payload(&store, partition, Context::default(), &payloads[0], true).await;

    let serializing = pause_at(PausePoint::Serializing);
    let flush = {
        let store = store.clone();
        lore_base::lore_spawn!(async move { store.flush(FlushOptions::SYNC_DATA).await })
    };
    serializing.reached.notified().await;
    put_payload(&store, partition, Context::default(), &late, false).await;
    serializing.resume.notify_one();
    flush.await.unwrap().unwrap();

    assert!(
        !store.group[group_index].packstore.is_dirty(1).await,
        "the late payload is synced"
    );
}

/// The directory group `group_index` of a store over `dir` keeps its packfiles in.
fn group_pack_dir(dir: &Path, group_index: usize) -> PathBuf {
    lore_storage::local::fan_out::group_dir_path(&dir.join("immutable").join("index"), group_index)
        .join("pack")
}

/// The header and bytes of the file of bucket `bucket_index` of group `group_index`, in a store
/// over `dir`.
fn read_bucket_file(
    dir: &Path,
    group_index: usize,
    bucket_index: usize,
) -> (ImmutableStoreHeader, Vec<u8>) {
    let bytes = std::fs::read(format_bucket_path(
        &dir.join("immutable"),
        group_index,
        bucket_index,
    ))
    .unwrap();
    let (header, _) = ImmutableStoreHeader::read_from_prefix(&bytes).unwrap();
    (header, bytes)
}

/// Rewrite the header of the file of bucket 0 of group `group_index`, in a store over `dir`.
fn edit_bucket_header(
    dir: &Path,
    group_index: usize,
    edit: impl FnOnce(&mut ImmutableStoreHeader),
) {
    let (mut header, mut bytes) = read_bucket_file(dir, group_index, 0);
    edit(&mut header);
    bytes[..size_of::<ImmutableStoreHeader>()].copy_from_slice(header.as_bytes());
    std::fs::write(
        format_bucket_path(&dir.join("immutable"), group_index, 0),
        bytes,
    )
    .unwrap();
}

/// The bytes of a bucket file holding `count` entries and no packfile sizes after them.
fn bucket_file_len(count: usize) -> usize {
    size_of::<ImmutableStoreHeader>()
        + count * (size_of::<u32>() + size_of::<ImmutableStoreEntry>())
}

/// A bucket file records the size of the packfile taking writes in its header, and none when the
/// write synced every packfile first.
#[tokio::test]
async fn a_bucket_file_records_the_size_of_the_packfile_taking_writes() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_sizes_recorded_");
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let partition = Partition::from([0x41u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(3);
    for payload in &payloads[..2] {
        put_payload(&store, partition, Context::default(), payload, false).await;
    }
    store.clone().flush(FlushOptions::default()).await.unwrap();

    let (header, bytes) = read_bucket_file(&dir, group_index, 0);
    assert_eq!(
        (header.packfile_sizes, header.first_packfile_size),
        (SIZES_RECORDED | (1 << SIZES_COUNT_SHIFT) | 1, 8192)
    );
    assert_eq!(bytes.len(), bucket_file_len(2));

    put_payload(&store, partition, Context::default(), &payloads[2], false).await;
    store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
    let (header, bytes) = read_bucket_file(&dir, group_index, 0);
    assert_eq!(header.packfile_sizes, SIZES_RECORDED);
    assert_eq!(bytes.len(), bucket_file_len(3));
}

/// A bucket file holds the packfile sizes its header has no room for after its entries, in fewer
/// bytes than an entry, so that a reader that does not know of them counts the entries alone. The
/// store reopens with the entries intact.
#[tokio::test]
async fn a_bucket_file_holds_the_sizes_its_header_has_no_room_for_after_its_entries() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_sizes_trailer_");
    let partition = Partition::from([0x42u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let pack = group_pack_dir(&dir, group_index);
    std::fs::create_dir_all(&pack).unwrap();
    for (id, len) in [(1, 64), (2, 32), (3, 16)] {
        std::fs::write(pack.join(id.to_string()), vec![0u8; len]).unwrap();
    }
    let (address, bucket_index) = {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        let address = put_payload(&store, partition, Context::default(), &payloads[0], false).await;
        store.clone().flush(FlushOptions::default()).await.unwrap();
        let bucket_count = store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed);
        let bucket_index =
            lore_storage::local::fan_out::bucket_index_for(&address.hash, bucket_count);
        (address, bucket_index)
    };

    let (header, bytes) = read_bucket_file(&dir, group_index, bucket_index);
    assert_eq!(
        (header.packfile_sizes, header.first_packfile_size),
        (SIZES_RECORDED | (3 << SIZES_COUNT_SHIFT) | 1, 64 + 4096)
    );
    let entries_end = bucket_file_len(1);
    assert_eq!(
        &bytes[entries_end..],
        [
            PackfileSize { id: 2, size: 32 },
            PackfileSize { id: 3, size: 16 }
        ]
        .as_bytes()
    );
    assert_eq!(
        (bytes.len() - size_of::<ImmutableStoreHeader>())
            / (size_of::<u32>() + size_of::<ImmutableStoreEntry>()),
        1
    );

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, [(address, payloads[0].clone())]).await;
    assert!(!store.group[group_index].dirty[bucket_index].load(atomic::Ordering::Relaxed));
}

/// A bucket write that would record more packfile sizes than a bucket file holds syncs every
/// packfile of the group instead, and records none.
#[tokio::test]
async fn more_packfiles_to_record_than_a_bucket_file_holds_are_synced_instead() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_sizes_overflow_");
    let partition = Partition::from([0x43u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let pack = group_pack_dir(&dir, group_index);
    std::fs::create_dir_all(&pack).unwrap();
    for id in 1..=TRAILER_SIZES + 2 {
        std::fs::write(pack.join(id.to_string()), [0u8; 16]).unwrap();
    }
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let address = put_payload(&store, partition, Context::default(), &payloads[0], false).await;
    store.clone().flush(FlushOptions::default()).await.unwrap();
    let bucket_count = store.group[group_index]
        .bucket_count
        .load(atomic::Ordering::Relaxed);
    let bucket_index = lore_storage::local::fan_out::bucket_index_for(&address.hash, bucket_count);

    let (header, bytes) = read_bucket_file(&dir, group_index, bucket_index);
    assert_eq!(header.packfile_sizes, SIZES_RECORDED);
    assert_eq!(bytes.len(), bucket_file_len(1));
    assert!(!store.group[group_index].packstore.is_dirty(1).await);
}

/// A store over `dir` that wrote two payloads of one group, at offsets 0 and 4096 of packfile 1,
/// to their bucket file without a sync, and closed. Returns the group, the two addresses, and the
/// two payloads with a third of the group.
async fn two_payloads_written(
    dir: &Path,
    partition: Partition,
) -> (usize, Vec<Address>, Vec<Vec<u8>>) {
    use lore_storage::immutable_store::ImmutableStore;

    let (group_index, payloads) = payloads_in_one_group(3);
    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    let mut addresses = vec![];
    for payload in &payloads[..2] {
        addresses.push(put_payload(&store, partition, Context::default(), payload, false).await);
    }
    store.clone().flush(FlushOptions::default()).await.unwrap();
    (group_index, addresses, payloads)
}

/// The entry for `address` among the loaded entries of group `group_index`.
async fn loaded_entry(
    store: &LocalImmutableStore,
    group_index: usize,
    address: Address,
) -> ImmutableStoreEntry {
    group_entries(store, group_index)
        .await
        .into_iter()
        .find(|entry| entry.address == address)
        .expect("the entry is loaded")
}

/// A bucket file pointing past the end of its packfile, as after a crash lost the end of the
/// packfile, has those payload locations cleared when it is loaded and is written again: the
/// payload past the end is no longer found, a write at the new end does not take its place, and
/// storing it again stores it.
#[tokio::test]
async fn payloads_past_the_end_of_their_packfile_are_cleared_when_their_bucket_loads() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_sizes_lost_tail_");
    let partition = Partition::from([0x44u8; 16]);
    let (group_index, addresses, payloads) = two_payloads_written(&dir, partition).await;
    std::fs::OpenOptions::new()
        .write(true)
        .open(group_pack_dir(&dir, group_index).join("1"))
        .unwrap()
        .set_len(4096)
        .unwrap();

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, [(addresses[0], payloads[0].clone())]).await;
    let lost = loaded_entry(&store, group_index, addresses[1]).await;
    assert_eq!(
        (
            lost.data.pack_file,
            lost.data.flags & FragmentFlags::PayloadStoredLocal.bits()
        ),
        (0, 0)
    );
    assert!(store.group[group_index].dirty[0].load(atomic::Ordering::Relaxed));
    assert!(store.clone().get(partition, addresses[1]).await.is_err());

    let third = put_payload(&store, partition, Context::default(), &payloads[2], false).await;
    put_payload(&store, partition, Context::default(), &payloads[1], false).await;
    assert_payloads_read_back(
        &store,
        partition,
        [
            (addresses[0], payloads[0].clone()),
            (addresses[1], payloads[1].clone()),
            (third, payloads[2].clone()),
        ],
    )
    .await;
}

/// A bucket file pointing into a packfile missing when the store opens has those payload locations
/// cleared when it is loaded.
#[tokio::test]
async fn payloads_in_a_missing_packfile_are_cleared_when_their_bucket_loads() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_sizes_missing_packfile_");
    let partition = Partition::from([0x45u8; 16]);
    let (group_index, payloads) = payloads_in_one_group(1);
    let address = {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
            .await
            .unwrap();
        store.group[group_index]
            .packstore
            .stop_writes(&[1])
            .await
            .unwrap();
        let address = put_payload(&store, partition, Context::default(), &payloads[0], false).await;
        store.clone().flush(FlushOptions::default()).await.unwrap();
        address
    };
    std::fs::remove_file(group_pack_dir(&dir, group_index).join("2")).unwrap();

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert!(store.clone().get(partition, address).await.is_err());
    assert_eq!(
        loaded_entry(&store, group_index, address)
            .await
            .data
            .pack_file,
        0
    );
}

/// A bucket file recording no packfile sizes, as an earlier version writes, is trusted: its
/// entries are loaded as they are, though one points past the end of its packfile.
#[tokio::test]
async fn a_bucket_file_recording_no_sizes_is_trusted() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_sizes_unrecorded_");
    let partition = Partition::from([0x46u8; 16]);
    let (group_index, addresses, _) = two_payloads_written(&dir, partition).await;
    edit_bucket_header(&dir, group_index, |header| {
        header.packfile_sizes = 0;
        header.first_packfile_size = 0;
    });
    std::fs::OpenOptions::new()
        .write(true)
        .open(group_pack_dir(&dir, group_index).join("1"))
        .unwrap()
        .set_len(4096)
        .unwrap();

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    store.clone().get(partition, addresses[0]).await.unwrap();
    assert_eq!(
        loaded_entry(&store, group_index, addresses[1])
            .await
            .data
            .pack_file,
        1
    );
    assert!(!store.group[group_index].dirty[0].load(atomic::Ordering::Relaxed));
}

/// A bucket file whose header counts more packfile sizes than follow its entries, as when a crash
/// cut it short, has its entries checked: those past the end of their packfile are cleared.
#[tokio::test]
async fn a_bucket_file_with_torn_sizes_has_its_entries_checked() {
    let dir = lore_base::test_util::TempDir::new("is_sizes_torn_");
    let partition = Partition::from([0x47u8; 16]);
    let (group_index, addresses, payloads) = two_payloads_written(&dir, partition).await;
    edit_bucket_header(&dir, group_index, |header| {
        header.packfile_sizes = SIZES_RECORDED | (2 << SIZES_COUNT_SHIFT) | 1;
    });
    std::fs::OpenOptions::new()
        .write(true)
        .open(group_pack_dir(&dir, group_index).join("1"))
        .unwrap()
        .set_len(4096)
        .unwrap();

    let store = LocalImmutableStore::new(Some(dir.to_path_buf()), client_settings())
        .await
        .unwrap();
    assert_payloads_read_back(&store, partition, [(addresses[0], payloads[0].clone())]).await;
    assert_eq!(
        loaded_entry(&store, group_index, addresses[1])
            .await
            .data
            .pack_file,
        0
    );
}

/// A group at the full bucket level records no level for a bucket write outside a flush, so its
/// first flush runs the fan-out commit. The commit writes a bucket emptied since such a write:
/// left out, its file keeps the entries memory removed, which the next open loads back, pointing
/// into packfile ranges compaction reuses.
#[tokio::test]
async fn the_first_flush_writes_a_bucket_emptied_since_its_file_was_written() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_first_flush_emptied_");
    let partition = Partition::from([0x48u8; 16]);
    // Two payloads of one bucket: the first hash byte chooses the group, the second the bucket.
    let mut first_in_bucket: HashMap<[u8; 2], Vec<u8>> = HashMap::new();
    let (group_index, bucket_index, kept, obliterated) = (0u32..)
        .find_map(|seed| {
            let payload =
                format!("payload {seed} of a bucket emptied before its first flush").into_bytes();
            let hash = hash::hash_slice(&payload);
            let bucket = [hash.data()[0], hash.data()[1]];
            let earlier = first_in_bucket.insert(bucket, payload.clone())?;
            Some((
                usize::from(bucket[0]),
                usize::from(bucket[1]),
                earlier,
                payload,
            ))
        })
        .expect("two payloads hashing into one bucket");

    // Kept across holds, so that the eviction below finds the entries the obliterate's claim left.
    let store = LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            reuse_state_across_holds: true,
            ..server_settings()
        },
    )
    .await
    .unwrap();
    let kept = put_payload(&store, partition, Context::default(), &kept, false).await;
    let obliterated = put_payload(&store, partition, Context::default(), &obliterated, false).await;
    store
        .clone()
        .obliterate(
            partition,
            obliterated,
            Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
        )
        .await
        .expect("obliterate");
    let group = &store.group[group_index];
    assert_eq!(
        group.committed_level.load(atomic::Ordering::Relaxed),
        0,
        "the obliterate's bucket write records no level for a group at the full level"
    );
    let bucket_path = format_bucket_path(
        store.path.as_ref().expect("a disk-backed store has a path"),
        group_index,
        bucket_index,
    );
    assert_eq!(
        ImmutableStoreBucket::deserialize_files(bucket_path.clone())
            .await
            .unwrap()
            .entry
            .len(),
        2,
        "the obliterate wrote the bucket with the entry and the tombstone"
    );

    assert_eq!(
        store.clone().evict(1, false, None).await.unwrap(),
        2,
        "a capacity of one evicts the entry and the tombstone"
    );
    store.clone().flush(FlushOptions::default()).await.unwrap();

    assert!(
        ImmutableStoreBucket::deserialize_files(bucket_path)
            .await
            .unwrap()
            .entry
            .is_empty(),
        "the group's first flush wrote the emptied bucket"
    );
    drop(store);
    let reopened = LocalImmutableStore::new(Some(dir.to_path_buf()), server_settings())
        .await
        .unwrap();
    let err = reopened
        .get(partition, kept)
        .await
        .expect_err("an evicted entry is not loaded back");
    assert!(matches!(err, StoreError::AddressNotFound(_)));
}

mod evict {
    use lore_storage::immutable_store::ImmutableStore;

    use super::*;
    use crate::store_fixture::payload_in_groups;

    /// Settings for a store that uses the full bucket layout, flushes in the background, and has no
    /// eviction protection.
    pub(super) fn background_settings(defer_eviction_flush: bool) -> ImmutableStoreSettings {
        ImmutableStoreSettings {
            protect_local_fragment: false,
            flush_background: true,
            defer_eviction_flush,
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ..quiet_settings()
        }
    }

    /// Stores `payload` with durability set by `durable` and returns its group and bucket.
    async fn put_one(
        store: &Arc<LocalImmutableStore>,
        payload: &[u8],
        durable: bool,
    ) -> (usize, usize) {
        let address = put_payload(
            store,
            Partition::from([0x44u8; 16]),
            Context::default(),
            payload,
            durable,
        )
        .await;
        let group_index = address.hash.data()[0] as usize;
        let bucket_count = store.group[group_index]
            .bucket_count
            .load(atomic::Ordering::Relaxed);
        (
            group_index,
            lore_storage::local::fan_out::bucket_index_for(&address.hash, bucket_count),
        )
    }

    /// Opens a store with `settings` and writes all `payloads` to disk, using each payload's
    /// durability flag. No group has a pending delayed flush. Returns each payload's group and
    /// bucket.
    pub(super) async fn store_with_written(
        dir: &lore_base::test_util::TempDir,
        settings: ImmutableStoreSettings,
        payloads: &[(Vec<u8>, bool)],
    ) -> (Arc<LocalImmutableStore>, Vec<(usize, usize)>) {
        let store = LocalImmutableStore::new(Some(dir.to_path_buf()), settings)
            .await
            .unwrap();
        let mut written = vec![];
        for (payload, durable) in payloads {
            written.push(put_one(&store, payload, *durable).await);
        }
        store.clone().flush(FlushOptions::SYNC_DATA).await.unwrap();
        for group in store.group.iter() {
            group.flush.lock().await.abort_all();
            group.scheduled.store(false, atomic::Ordering::Relaxed);
        }
        (store, written)
    }

    /// One payload in each of two groups, neither durable.
    pub(super) fn two_groups() -> Vec<(Vec<u8>, bool)> {
        let [(_, first), (_, second)] = payloads_in_two_groups(1);
        vec![(first[0].clone(), false), (second[0].clone(), false)]
    }

    pub(super) async fn entries_on_disk(
        store: &LocalImmutableStore,
        group_index: usize,
        bucket_index: usize,
    ) -> usize {
        let root = store.path.clone().expect("a disk-backed store has a path");
        ImmutableStoreBucket::deserialize_files(format_bucket_path(
            &root,
            group_index,
            bucket_index,
        ))
        .await
        .unwrap()
        .entry
        .len()
    }

    /// Polls until the bucket's file holds no entry, failing after `within`.
    async fn wait_until_written(
        store: &LocalImmutableStore,
        (group_index, bucket_index): (usize, usize),
        within: Duration,
    ) {
        tokio::time::timeout(within, async {
            while entries_on_disk(store, group_index, bucket_index).await != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("group {group_index}'s eviction is written within {within:?}"));
    }

    /// **A store that defers its eviction flush relies on each affected group's delayed flush
    /// to write the evicted buckets**, keeping the flock until the flush has written every
    /// bucket.
    #[tokio::test]
    async fn a_deferring_store_schedules_each_evicted_group_and_keeps_the_flock_until_written() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_");
        let (store, written) =
            store_with_written(&dir, background_settings(true), &two_groups()).await;
        let lock = store.lock.clone().expect("a disk-backed store has a lock");

        let evicted = store.clone().evict(1, false, None).await.unwrap();

        assert_eq!(evicted, 2, "a cap of one evicts both fragments");
        for &(group_index, bucket_index) in &written {
            assert!(
                store.group[group_index]
                    .scheduled
                    .load(atomic::Ordering::Relaxed),
                "group {group_index} has a delayed flush due"
            );
            assert_eq!(
                entries_on_disk(&store, group_index, bucket_index).await,
                1,
                "the pass left group {group_index} unwritten"
            );
        }
        assert!(lock.is_held(), "the unwritten buckets keep the flock");

        for &(group_index, _) in &written {
            ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), group_index, Duration::ZERO)
                .await;
        }

        for &(group_index, bucket_index) in &written {
            assert_eq!(
                entries_on_disk(&store, group_index, bucket_index).await,
                0,
                "the delayed flush wrote group {group_index}'s eviction"
            );
        }
        assert!(
            !lock.is_held(),
            "the delayed flushes let the store go once nothing was flagged"
        );
    }

    /// **Every eviction pass schedules each group with unwritten changes, including a pass that
    /// evicts nothing**, so a group whose delayed flush ended without writing it is tried again.
    #[tokio::test]
    async fn a_deferring_store_schedules_a_group_left_flagged_at_a_pass_that_evicts_nothing() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_again_");
        let (store, written) =
            store_with_written(&dir, background_settings(true), &two_groups()).await;
        let (left, _) = written[0];

        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 2);
        // A failed delayed flush leaves this state: woken, with the bucket still flagged for
        // writing.
        store.group[left]
            .scheduled
            .store(false, atomic::Ordering::Relaxed);

        let evicted = store.clone().evict(1, false, None).await.unwrap();

        assert_eq!(evicted, 0, "the second pass finds nothing left to evict");
        assert!(
            store.group[left].scheduled.load(atomic::Ordering::Relaxed),
            "the group the first pass left flagged has a delayed flush due again"
        );
    }

    /// **An evicting pass schedules a group holding only access stamps, and a pass that evicts
    /// nothing leaves it alone**, since stamps never hold the flock.
    #[tokio::test]
    async fn a_deferring_store_schedules_a_group_holding_only_stamps_after_an_evicting_pass() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_stamps_");
        let settings = ImmutableStoreSettings {
            protect_local_fragment: true,
            ..background_settings(true)
        };
        let [(_, kept), (_, evictable)] = payloads_in_two_groups(1);
        let payloads = [(kept[0].clone(), false), (evictable[0].clone(), true)];
        let (store, written) = store_with_written(&dir, settings, &payloads).await;
        let (stamped, stamped_bucket) = written[0];
        store.group[stamped].soft_dirty[stamped_bucket].store(true, atomic::Ordering::Relaxed);

        assert_eq!(
            store.clone().evict(1, false, None).await.unwrap(),
            1,
            "the pass evicts only the durable fragment"
        );
        assert!(
            store.group[stamped]
                .scheduled
                .load(atomic::Ordering::Relaxed),
            "the stamped group has a delayed flush due after an evicting pass"
        );

        store.group[stamped]
            .scheduled
            .store(false, atomic::Ordering::Relaxed);
        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 0);
        assert!(
            !store.group[stamped]
                .scheduled
                .load(atomic::Ordering::Relaxed),
            "a pass that evicts nothing schedules no flush for stamps alone"
        );
    }

    /// **When a fan-out awaits its commit and no bucket is flagged, a pass that evicts nothing
    /// takes no claim**: claiming to write would tell every other process the store changed, and
    /// nothing holds the flock for the commit.
    #[tokio::test]
    async fn a_deferring_store_leaves_a_pending_commit_alone_at_a_pass_that_evicts_nothing() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_no_claim_");
        let (store, written) =
            store_with_written(&dir, background_settings(true), &two_groups()).await;
        let lock = store.lock.clone().expect("a disk-backed store has a lock");
        let group = &store.group[written[0].0];
        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 2);
        for &(index, _) in &written {
            ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), index, Duration::ZERO).await;
        }
        assert!(!lock.is_held(), "the delayed flushes let the store go");
        let epoch = dir.to_path_buf().join("immutable").join("epoch");
        let before = std::fs::read(&epoch).ok();
        let full = group.bucket_count.load(atomic::Ordering::Relaxed);
        group
            .committed_level
            .store(full / 2, atomic::Ordering::Relaxed);

        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 0);

        assert_eq!(
            std::fs::read(&epoch).ok(),
            before,
            "the pass told no other process the store changed"
        );
        assert_eq!(
            group.committed_level.load(atomic::Ordering::Relaxed),
            full / 2,
            "the commit waits for a pass that evicts"
        );
        assert!(!lock.is_held(), "the store stays free");
    }

    /// **A pass that ends after garbage collection was asked to stop schedules no delayed
    /// flush**, which would claim the store after background work was to end.
    #[tokio::test]
    async fn a_deferring_store_schedules_nothing_once_asked_to_stop() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_stop_");
        let (store, written) =
            store_with_written(&dir, background_settings(true), &two_groups()).await;
        let (group_index, bucket_index) = written[0];
        store.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        store.stop_requests.fetch_add(1, atomic::Ordering::Relaxed);

        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 0);

        assert!(
            !store.group[group_index]
                .scheduled
                .load(atomic::Ordering::Relaxed),
            "no delayed flush is due"
        );
        store.stop_requests.fetch_sub(1, atomic::Ordering::Relaxed);
    }

    /// **When asked to sync, a store with deferred eviction flushes writes the evicted buckets at
    /// the end of the pass**, since the delayed flush writes without syncing.
    #[tokio::test]
    async fn a_deferring_store_writes_as_the_pass_ends_when_asked_to_sync() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_sync_");
        let (store, written) =
            store_with_written(&dir, background_settings(true), &two_groups()).await;
        let lock = store.lock.clone().expect("a disk-backed store has a lock");

        let evicted = store.clone().evict(1, true, None).await.unwrap();

        assert_eq!(evicted, 2, "a cap of one evicts both fragments");
        for &(group_index, bucket_index) in &written {
            assert_eq!(
                entries_on_disk(&store, group_index, bucket_index).await,
                0,
                "the pass wrote group {group_index}'s eviction"
            );
        }
        assert!(!lock.is_held(), "the pass wrote what it evicted and let go");
    }

    /// **A store with deferral enabled but background flushing disabled writes each pass's
    /// evictions when the pass ends**, because its changes to durable entries wait for an explicit
    /// flush rather than a delayed one.
    #[tokio::test]
    async fn a_store_deferring_without_background_flushing_writes_as_the_pass_ends() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_no_bg_");
        let settings = ImmutableStoreSettings {
            flush_background: false,
            ..background_settings(true)
        };
        let (store, written) = store_with_written(&dir, settings, &two_groups()).await;
        let lock = store.lock.clone().expect("a disk-backed store has a lock");

        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 2);

        for &(group_index, bucket_index) in &written {
            assert_eq!(
                entries_on_disk(&store, group_index, bucket_index).await,
                0,
                "the pass wrote group {group_index}'s eviction"
            );
        }
        assert!(!lock.is_held(), "the pass wrote what it evicted and let go");
    }

    /// **An in-memory store does not defer**, having no disk to write to and so no delayed flush
    /// that would clear a group's due flag.
    #[tokio::test]
    async fn an_in_memory_store_schedules_no_delayed_flush() {
        let store = LocalImmutableStore::new(None, background_settings(true))
            .await
            .unwrap();
        let mut groups = vec![];
        for (payload, durable) in two_groups() {
            groups.push(put_one(&store, &payload, durable).await.0);
        }
        for &group_index in &groups {
            store.group[group_index]
                .scheduled
                .store(false, atomic::Ordering::Relaxed);
        }

        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 2);

        for group_index in groups {
            assert!(
                !store.group[group_index]
                    .scheduled
                    .load(atomic::Ordering::Relaxed),
                "group {group_index} has no delayed flush due"
            );
        }
    }

    /// **A store with deferred eviction flushes writes evicted buckets in a lower-numbered group
    /// well before those in a higher-numbered group.** The groups' delayed flushes are spread
    /// across one flush delay. With a delay of 16 seconds, a group below 32 is due within two
    /// seconds of the pass, and a group numbered 224 or higher is due after more than fourteen.
    #[tokio::test]
    async fn a_deferring_store_spreads_the_groups_writes_across_the_flush_delay() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_spread_");
        let settings = ImmutableStoreSettings {
            flush_delay_seconds: 16,
            ..background_settings(true)
        };
        let payloads = [
            (payload_in_groups(0..32), false),
            (payload_in_groups(224..GROUP_COUNT), false),
        ];
        let (store, written) = store_with_written(&dir, settings, &payloads).await;
        let (low, high) = (written[0], written[1]);

        let evicted = store.clone().evict(1, false, None).await.unwrap();

        assert_eq!(evicted, 2, "a cap of one evicts both fragments");
        for (group_index, _) in [low, high] {
            assert!(
                store.group[group_index]
                    .scheduled
                    .load(atomic::Ordering::Relaxed),
                "the pass scheduled group {group_index}'s delayed flush"
            );
        }
        wait_until_written(&store, low, Duration::from_secs(10)).await;
        assert_eq!(
            entries_on_disk(&store, high.0, high.1).await,
            1,
            "the high group's delayed flush is not yet due"
        );
    }

    /// **A store with background flushing enabled and eviction flushing not deferred writes
    /// each bucket from which it evicted entries at the end of the eviction pass**, then
    /// releases the store.
    #[tokio::test]
    async fn a_background_flushing_store_that_does_not_defer_writes_as_the_pass_ends() {
        let dir = lore_base::test_util::TempDir::new("is_evict_no_defer_");
        let (store, written) =
            store_with_written(&dir, background_settings(false), &two_groups()).await;
        let lock = store.lock.clone().expect("a disk-backed store has a lock");

        let evicted = store.clone().evict(1, false, None).await.unwrap();

        assert_eq!(evicted, 2, "a cap of one evicts both fragments");
        for &(group_index, bucket_index) in &written {
            assert_eq!(
                entries_on_disk(&store, group_index, bucket_index).await,
                0,
                "the pass wrote group {group_index}'s eviction"
            );
        }
        assert!(!lock.is_held(), "the pass wrote what it evicted and let go");
    }
}

mod flush_delayed {
    use lore_storage::immutable_store::ImmutableStore;

    use super::evict::background_settings;
    use super::evict::entries_on_disk;
    use super::evict::store_with_written;
    use super::evict::two_groups;
    use super::*;

    /// **When a store defers eviction writes, the delayed flush completes a fan-out waiting for its
    /// commit** by writing the whole group, then releases the store.
    #[tokio::test]
    async fn a_deferring_stores_delayed_flush_commits_a_pending_fan_out() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_commit_");
        let (store, written) =
            store_with_written(&dir, background_settings(true), &two_groups()).await;
        let lock = store.lock.clone().expect("a disk-backed store has a lock");
        let (group_index, bucket_index) = written[0];
        let group = &store.group[group_index];
        let full = group.bucket_count.load(atomic::Ordering::Relaxed);
        group
            .committed_level
            .store(full / 2, atomic::Ordering::Relaxed);

        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 2);
        assert!(
            group.scheduled.load(atomic::Ordering::Relaxed),
            "the group has a delayed flush due"
        );
        for &(index, _) in &written {
            ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), index, Duration::ZERO).await;
        }

        assert_eq!(
            group.committed_level.load(atomic::Ordering::Relaxed),
            full,
            "the delayed flush committed the group's layout"
        );
        assert_eq!(
            entries_on_disk(&store, group_index, bucket_index).await,
            0,
            "and wrote the eviction"
        );
        assert!(!lock.is_held(), "and let the store go");
    }

    /// **A store with deferred eviction writes leaves a group below the full layout to its delayed
    /// flush**, like any other group, and that flush writes the eviction.
    #[tokio::test]
    async fn a_deferring_store_writes_a_group_below_the_full_layout_in_its_delayed_flush() {
        let dir = lore_base::test_util::TempDir::new("is_evict_defer_level_");
        let settings = ImmutableStoreSettings {
            initial_fan_out_level: 1,
            ..background_settings(true)
        };
        let (store, written) = store_with_written(&dir, settings, &two_groups()).await;
        let lock = store.lock.clone().expect("a disk-backed store has a lock");

        assert_eq!(store.clone().evict(1, false, None).await.unwrap(), 2);

        for &(group_index, bucket_index) in &written {
            assert!(
                store.group[group_index]
                    .scheduled
                    .load(atomic::Ordering::Relaxed),
                "group {group_index} has a delayed flush due"
            );
            assert_eq!(
                entries_on_disk(&store, group_index, bucket_index).await,
                1,
                "the pass left group {group_index} unwritten"
            );
        }
        for &(group_index, _) in &written {
            ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), group_index, Duration::ZERO)
                .await;
        }
        for &(group_index, bucket_index) in &written {
            assert_eq!(
                entries_on_disk(&store, group_index, bucket_index).await,
                0,
                "the delayed flush wrote group {group_index}'s eviction"
            );
        }
        assert!(!lock.is_held(), "the delayed flushes let the store go");
    }

    /// **A deferring store's delayed flush fans out a group whose bucket holds more entries than
    /// the fan-out threshold**, as only a flush of the whole group does.
    #[tokio::test]
    async fn a_deferring_stores_delayed_flush_fans_out_a_group_past_its_threshold() {
        let dir = lore_base::test_util::TempDir::new("is_delayed_fan_out_");
        let store = LocalImmutableStore::new(
            Some(dir.to_path_buf()),
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                fan_out_threshold: 2,
                ..background_settings(true)
            },
        )
        .await
        .unwrap();
        let (group_index, payloads) = payloads_in_one_group(3);
        for payload in &payloads {
            put_payload(
                &store,
                Partition::from([0x44u8; 16]),
                Context::default(),
                payload,
                false,
            )
            .await;
        }
        let group = &store.group[group_index];
        group.flush.lock().await.abort_all();
        group.scheduled.store(false, atomic::Ordering::Relaxed);
        assert_eq!(group.bucket_count.load(atomic::Ordering::Relaxed), 1);

        ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), group_index, Duration::ZERO)
            .await;

        let bucket_count = group.bucket_count.load(atomic::Ordering::Relaxed);
        assert!(bucket_count > 1, "the delayed flush fanned the group out");
        assert_eq!(
            group.committed_level.load(atomic::Ordering::Relaxed),
            bucket_count,
            "and committed its new layout"
        );
    }
}

mod eviction_flush_delay {
    use super::*;

    /// Group wait times increase in equal steps. The first group waits for a fraction of the
    /// flush delay, and the last waits for the full delay.
    #[test]
    fn spreads_groups_in_equal_steps_up_to_the_flush_delay() {
        let flush_delay = Duration::from_secs(10);
        let step = flush_delay / GROUP_COUNT as u32;

        for group_index in 0..GROUP_COUNT {
            assert_eq!(
                LocalImmutableStore::eviction_flush_delay(flush_delay, group_index),
                step * (group_index as u32 + 1),
                "group {group_index} waits its own number of steps"
            );
        }
        assert_eq!(
            LocalImmutableStore::eviction_flush_delay(flush_delay, GROUP_COUNT - 1),
            flush_delay,
            "the last group waits the whole delay"
        );
    }
}
