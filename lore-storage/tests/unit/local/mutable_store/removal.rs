// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A key holding zero is absent: a swap from zero creates it, a matching swap to zero or a store
//! of zero removes its entry, on disk as in memory, a write that changes nothing flags no bucket,
//! and a listing leaves out an entry holding zero that an older version wrote.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;

use futures::StreamExt;
use lore_base::test_util::TempDir;
use lore_storage::FlushOptions;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::immutable_store::StoreError;
use lore_storage::local::fan_out::BucketGroup;
use lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX;
use lore_storage::local::fan_out::bucket_index_for;
use lore_storage::local::mutable_store::*;
use lore_storage::local::store_lock::Intent;
use lore_storage::mutable_store::MutableStore;
use lore_storage::store_types::KeyType;

use super::client_settings;
use super::make_in_memory_immutable;

const KEY_TYPE: KeyType = KeyType::BranchMetadata;

/// The group every key of these tests is in, so that at level 1 they share one bucket.
const GROUP: u8 = 0x42;

/// A key of [`GROUP`], placed among the others by `order`, which is also its bucket byte.
fn key(order: u8) -> Hash {
    let mut key = Hash::default();
    key.data_mut()[0] = GROUP;
    key.data_mut()[1] = order;
    key.data_mut()[5] = 0x77;
    key
}

/// The key the store files `key` under.
fn typed(mut key: Hash) -> Hash {
    key.data_mut()[2] = KEY_TYPE as u8;
    key
}

/// A non-zero value, distinct for each `seed`.
fn value(seed: u64) -> Hash {
    Hash::from_u64(seed + 1)
}

/// Opens the store under `dir`, as a client does: groups at level 1, written at once.
async fn open(dir: &Path) -> Arc<LocalMutableStore> {
    open_with(dir, client_settings()).await
}

async fn open_with(dir: &Path, settings: MutableStoreSettings) -> Arc<LocalMutableStore> {
    Arc::new(
        LocalMutableStore::new(Some(dir), settings, make_in_memory_immutable().await)
            .await
            .expect("store opens"),
    )
}

async fn swap(store: &Arc<LocalMutableStore>, key: Hash, expected: Hash, value: Hash) -> Hash {
    store
        .clone()
        .compare_and_swap(Partition::default(), key, expected, value, KEY_TYPE)
        .await
        .expect("swaps")
}

async fn put(store: &Arc<LocalMutableStore>, key: Hash, value: Hash) {
    store
        .clone()
        .store(Partition::default(), key, value, KEY_TYPE)
        .await
        .expect("stores");
}

/// What a load of `key` reads, zero for a key it does not find.
async fn load(store: &Arc<LocalMutableStore>, key: Hash) -> Hash {
    match store
        .clone()
        .load(Partition::default(), key, KEY_TYPE)
        .await
    {
        Ok(value) => value,
        Err(StoreError::AddressNotFound(_)) => Hash::default(),
        Err(err) => panic!("load of {key} failed: {err:?}"),
    }
}

/// The keys a listing of [`KEY_TYPE`] sends, with their values, in key order.
async fn listed(store: &Arc<LocalMutableStore>) -> Vec<(Hash, Hash)> {
    let mut stream = store
        .clone()
        .list(Partition::default(), KEY_TYPE)
        .await
        .expect("lists");
    let mut pairs = Vec::new();
    while let Some(pair) = stream.next().await {
        pairs.push(pair);
    }
    pairs.sort();
    pairs
}

/// The group and bucket index of the bucket holding `key`.
fn bucket_of(store: &LocalMutableStore, key: Hash) -> (usize, usize) {
    let typed = typed(key);
    let group_index = typed.data()[0] as usize;
    let count = store.group[group_index]
        .bucket_count
        .load(atomic::Ordering::Relaxed);
    (group_index, bucket_index_for(&typed, count))
}

/// Whether the bucket holding `key` is flagged for the next flush.
fn flagged(store: &LocalMutableStore, key: Hash) -> bool {
    let (group_index, bucket_index) = bucket_of(store, key);
    store.group[group_index].dirty[bucket_index].load(atomic::Ordering::Relaxed)
}

fn held(store: &LocalMutableStore) -> bool {
    store
        .lock
        .as_ref()
        .expect("a disk-backed store has a lock")
        .is_held()
}

/// The entries of the bucket holding `key`, read from disk where the store has not read them yet,
/// once the bucket is found to keep its invariants: one sorted slot per entry, naming each entry
/// once, in key order.
async fn entries(store: &LocalMutableStore, key: Hash) -> Vec<MutableStoreEntry> {
    let typed = typed(key);
    let group_index = typed.data()[0] as usize;
    let (bucket_index, mut bucket) = store.group[group_index].bucket_for_write(&typed).await;
    if let Some(path) = store.path.as_ref() {
        bucket
            .deserialize(path, group_index, bucket_index, false, false)
            .await
            .expect("the bucket reads");
    }
    let count = bucket.entry.len();
    assert_eq!(
        bucket.sorted_index.len(),
        count,
        "the bucket has one sorted slot per entry"
    );
    let mut named = vec![false; count];
    let mut previous: Option<(Hash, Partition)> = None;
    for slot in 0..count {
        let index = bucket.sorted_index[slot] as usize;
        assert!(index < count, "slot {slot} names entry {index} of {count}");
        assert!(!named[index], "two slots name entry {index}");
        named[index] = true;
        let entry = bucket.entry[index];
        let order = (entry.key, entry.partition);
        if let Some(previous) = previous {
            assert!(previous < order, "slot {slot} is out of key order");
        }
        previous = Some(order);
    }
    bucket.entry.iter().copied().collect()
}

/// Whether `entries` holds an entry for `key`.
fn has_entry(entries: &[MutableStoreEntry], key: Hash) -> bool {
    entries.iter().any(|entry| entry.key == typed(key))
}

/// Gives `key` an entry holding zero, as a version that wrote such entries left one, and writes
/// it to disk.
async fn leave_zero_entry(store: &Arc<LocalMutableStore>, key: Hash) {
    // A change, made the way every change is: under a write claim.
    let claim = store.hold(Intent::Write).await.expect("claims");
    let typed = typed(key);
    let group_index = typed.data()[0] as usize;
    let group = &store.group[group_index];
    {
        let (bucket_index, mut bucket) = group.bucket_for_write(&typed).await;
        let path = store.path.clone().expect("a disk-backed store");
        bucket
            .deserialize(&path, group_index, bucket_index, false, false)
            .await
            .expect("the bucket reads");
        let (_, found, slot) = bucket.lookup(Partition::default(), typed);
        assert!(!found, "{key} has no entry yet");
        let index = bucket.entry.len() as u32;
        bucket.sorted_index.insert(slot, index);
        bucket.entry.push(MutableStoreEntry {
            partition: Partition::default(),
            key: typed,
            value: Hash::default(),
        });
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
    }
    drop(claim);
    store
        .clone()
        .flush(FlushOptions::SYNC_DATA)
        .await
        .expect("flushes");
}

#[tokio::test]
async fn a_swap_from_zero_creates_an_absent_key() {
    let dir = TempDir::new("ms_zero_creates_");
    let store = open(dir.path()).await;

    assert_eq!(
        swap(&store, key(1), Hash::default(), value(1)).await,
        Hash::default(),
        "the swap succeeds"
    );
    assert_eq!(load(&store, key(1)).await, value(1));
    assert!(has_entry(&entries(&store, key(1)).await, key(1)));
    assert!(flagged(&store, key(1)), "the new entry flags its bucket");
}

/// A matching swap to zero removes the key's entry, which stays removed through a flush and a
/// reopen, and every other key in the bucket keeps its value.
#[tokio::test]
async fn a_matching_swap_to_zero_removes_the_entry() {
    let dir = TempDir::new("ms_zero_swap_removes_");
    let keys: Vec<Hash> = (1..=5).map(|order| key(order * 10)).collect();
    {
        let store = open(dir.path()).await;
        for (seed, key) in keys.iter().enumerate() {
            swap(&store, *key, Hash::default(), value(seed as u64)).await;
        }
        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");

        assert_eq!(
            swap(&store, keys[2], value(2), Hash::default()).await,
            value(2),
            "the swap succeeds"
        );
        let after = entries(&store, keys[2]).await;
        assert_eq!(
            after.len(),
            keys.len() - 1,
            "the bucket holds one entry less"
        );
        assert!(!has_entry(&after, keys[2]), "and none for the key");
        assert!(
            !listed(&store)
                .await
                .iter()
                .any(|(listed, _)| *listed == typed(keys[2])),
            "a listing leaves the key out"
        );
        assert_eq!(load(&store, keys[2]).await, Hash::default());
        assert!(flagged(&store, keys[2]), "the removal flags its bucket");

        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        assert!(!flagged(&store, keys[2]), "the flush writes the removal");
        assert!(!held(&store), "and releases the store");
    }

    let store = open(dir.path()).await;
    let reopened = entries(&store, keys[2]).await;
    assert_eq!(reopened.len(), keys.len() - 1, "the removal reached disk");
    assert!(!has_entry(&reopened, keys[2]));
    for (seed, key) in keys.iter().enumerate() {
        let expected = if seed == 2 {
            Hash::default()
        } else {
            value(seed as u64)
        };
        assert_eq!(load(&store, *key).await, expected, "key {seed}");
    }
}

/// A swap from a value the key does not hold, to a value or to zero, changes nothing and answers
/// the value the key holds.
#[tokio::test]
async fn a_mismatched_swap_changes_nothing_and_answers_the_held_value() {
    let dir = TempDir::new("ms_zero_mismatch_");
    let store = open(dir.path()).await;
    swap(&store, key(1), Hash::default(), value(1)).await;
    store
        .clone()
        .flush(FlushOptions::SYNC_DATA)
        .await
        .expect("flushes");

    for to in [value(2), Hash::default()] {
        assert_eq!(
            swap(&store, key(1), value(9), to).await,
            value(1),
            "a swap to {to} answers the held value"
        );
        assert!(!flagged(&store, key(1)), "and flags nothing");
    }
    assert_eq!(load(&store, key(1)).await, value(1));
    assert_eq!(entries(&store, key(1)).await.len(), 1);
    assert!(!held(&store));
}

/// A swap from a non-zero value on an absent key answers zero and writes nothing.
#[tokio::test]
async fn a_swap_from_a_value_on_an_absent_key_answers_zero_and_writes_nothing() {
    let dir = TempDir::new("ms_zero_absent_mismatch_");
    let store = open(dir.path()).await;

    for to in [value(2), Hash::default()] {
        assert_eq!(
            swap(&store, key(1), value(1), to).await,
            Hash::default(),
            "a swap to {to} answers zero"
        );
        assert!(!flagged(&store, key(1)), "and flags nothing");
    }
    assert!(entries(&store, key(1)).await.is_empty(), "no entry is made");
    assert!(!held(&store));
}

/// A write that changes nothing leaves the bucket unflagged and the store unclaimed: a store of
/// the held value, a store of zero or a swap from zero to zero on an absent key, and a swap from
/// the held value to itself.
#[tokio::test]
async fn writes_that_change_nothing_flag_no_bucket() {
    let dir = TempDir::new("ms_zero_unchanged_");
    let store = open(dir.path()).await;
    put(&store, key(1), value(1)).await;
    store
        .clone()
        .flush(FlushOptions::SYNC_DATA)
        .await
        .expect("flushes");
    assert!(!held(&store));

    put(&store, key(1), value(1)).await;
    assert!(!flagged(&store, key(1)), "a store of the held value");
    put(&store, key(2), Hash::default()).await;
    assert!(!flagged(&store, key(2)), "a store of zero on an absent key");
    assert_eq!(
        swap(&store, key(3), Hash::default(), Hash::default()).await,
        Hash::default(),
        "a swap from zero to zero on an absent key succeeds"
    );
    assert!(
        !flagged(&store, key(3)),
        "a swap from zero to zero on an absent key"
    );
    assert_eq!(swap(&store, key(1), value(1), value(1)).await, value(1));
    assert!(
        !flagged(&store, key(1)),
        "a swap from the held value to itself"
    );

    let after = entries(&store, key(1)).await;
    assert_eq!(after.len(), 1, "none of them makes an entry");
    assert!(!held(&store), "and none keeps the store claimed");
}

/// An entry holding zero, as an older version left one, is absent: a swap from a value answers
/// zero and changes nothing, and a swap from zero to a value replaces it.
#[tokio::test]
async fn over_an_old_zero_entry_a_swap_from_zero_succeeds() {
    let dir = TempDir::new("ms_zero_old_entry_swap_");
    {
        let store = open(dir.path()).await;
        leave_zero_entry(&store, key(1)).await;
    }
    let store = open(dir.path()).await;
    let before = entries(&store, key(1)).await;
    assert_eq!(before.len(), 1, "the old entry is on disk");
    assert!(before[0].value.is_zero(), "holding zero");
    assert_eq!(
        load(&store, key(1)).await,
        Hash::default(),
        "and loads as absent"
    );

    assert_eq!(
        swap(&store, key(1), value(9), value(2)).await,
        Hash::default(),
        "a swap from a value answers zero"
    );
    assert!(!flagged(&store, key(1)), "and changes nothing");

    assert_eq!(
        swap(&store, key(1), Hash::default(), value(1)).await,
        Hash::default(),
        "a swap from zero succeeds"
    );
    assert_eq!(load(&store, key(1)).await, value(1));
    assert_eq!(
        entries(&store, key(1)).await.len(),
        1,
        "in the old entry's place"
    );
}

/// A swap from zero to zero over an entry holding zero removes the entry, and the removal reaches
/// disk.
#[tokio::test]
async fn over_an_old_zero_entry_a_swap_from_zero_to_zero_removes_it() {
    let dir = TempDir::new("ms_zero_old_entry_remove_");
    {
        let store = open(dir.path()).await;
        put(&store, key(2), value(2)).await;
        leave_zero_entry(&store, key(1)).await;
    }
    {
        let store = open(dir.path()).await;
        assert_eq!(entries(&store, key(1)).await.len(), 2);
        assert_eq!(
            swap(&store, key(1), Hash::default(), Hash::default()).await,
            Hash::default(),
            "the swap succeeds"
        );
        let after = entries(&store, key(1)).await;
        assert_eq!(after.len(), 1, "the old entry is removed");
        assert!(!has_entry(&after, key(1)));
        assert!(flagged(&store, key(1)), "the removal flags its bucket");
        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
    }
    let store = open(dir.path()).await;
    let reopened = entries(&store, key(1)).await;
    assert_eq!(reopened.len(), 1, "the removal reached disk");
    assert!(!has_entry(&reopened, key(1)));
    assert_eq!(load(&store, key(2)).await, value(2));
}

/// A listing leaves out an entry holding zero, as an older version left one, as a load reads it as
/// absent, and lists the key beside it.
#[tokio::test]
async fn a_listing_leaves_out_an_old_zero_entry() {
    let dir = TempDir::new("ms_zero_old_entry_list_");
    {
        let store = open(dir.path()).await;
        put(&store, key(2), value(2)).await;
        leave_zero_entry(&store, key(1)).await;
    }
    let store = open(dir.path()).await;
    assert_eq!(
        entries(&store, key(1)).await.len(),
        2,
        "the old entry is on disk"
    );
    assert_eq!(listed(&store).await, vec![(typed(key(2)), value(2))]);
}

/// A removal from a bucket whose sorted slots are out of key order, as a corrupt file can leave
/// them, still leaves one slot per entry, each naming an entry in range: the slot naming the entry
/// the removal moves is found whatever order the slots are in.
#[tokio::test]
async fn a_removal_from_a_bucket_with_slots_out_of_order_keeps_every_slot_in_range() {
    let dir = TempDir::new("ms_zero_out_of_order_");
    let store = open(dir.path()).await;
    // Entries in the order 1, 3, 2, so key 2's entry is the last, the one a removal moves.
    for order in [1, 3, 2] {
        put(&store, key(order), value(u64::from(order))).await;
    }
    let typed_key = typed(key(1));
    let group_index = typed_key.data()[0] as usize;
    {
        // Slots in entry order, 1, 3, 2, where key order is 1, 2, 3: a search for key 2 ends
        // between keys 1 and 3, short of key 2's slot.
        let (_, mut bucket) = store.group[group_index].bucket_for_write(&typed_key).await;
        assert_eq!(bucket.entry.len(), 3, "the three keys share a bucket");
        for slot in 0..3 {
            bucket.sorted_index[slot] = slot as u32;
        }
    }

    put(&store, key(1), Hash::default()).await;

    let (_, bucket) = store.group[group_index].bucket_for_write(&typed_key).await;
    let count = bucket.entry.len();
    assert_eq!(count, 2, "the removal removes one entry");
    assert_eq!(
        bucket.sorted_index.len(),
        count,
        "one sorted slot per entry"
    );
    let mut named = vec![false; count];
    for slot in 0..count {
        let index = bucket.sorted_index[slot] as usize;
        assert!(index < count, "slot {slot} names entry {index} of {count}");
        assert!(!named[index], "two slots name entry {index}");
        named[index] = true;
    }
}

/// A store of zero removes the key's entry, an entry holding zero included, and the removal
/// reaches disk.
#[tokio::test]
async fn a_store_of_zero_removes_the_entry() {
    let dir = TempDir::new("ms_zero_store_removes_");
    let keys: Vec<Hash> = (1..=3).map(|order| key(order * 10)).collect();
    {
        let store = open(dir.path()).await;
        for (seed, key) in keys.iter().enumerate() {
            put(&store, *key, value(seed as u64)).await;
        }
        leave_zero_entry(&store, key(99)).await;
    }
    {
        let store = open(dir.path()).await;
        assert_eq!(entries(&store, keys[0]).await.len(), keys.len() + 1);

        put(&store, keys[1], Hash::default()).await;
        put(&store, key(99), Hash::default()).await;
        let after = entries(&store, keys[1]).await;
        assert_eq!(after.len(), keys.len() - 1, "both entries are removed");
        assert!(!has_entry(&after, keys[1]));
        assert!(!has_entry(&after, key(99)));
        assert!(flagged(&store, keys[1]), "the removals flag their bucket");
        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
    }
    let store = open(dir.path()).await;
    let reopened = entries(&store, keys[0]).await;
    assert_eq!(reopened.len(), keys.len() - 1, "the removals reached disk");
    assert_eq!(load(&store, keys[0]).await, value(0));
    assert_eq!(load(&store, keys[1]).await, Hash::default());
    assert_eq!(load(&store, keys[2]).await, value(2));
}

/// Removals at the first and last sorted slot, of an entry the bucket stores before others and of
/// the entry it stores last, each leave the bucket's invariants and every other key, down to an
/// empty bucket that reaches disk.
#[tokio::test]
async fn removals_at_the_first_and_last_sorted_slot_keep_every_other_key() {
    let dir = TempDir::new("ms_zero_sorted_ends_");
    // Stored out of key order, so that the first and the last key in order are not the last
    // entry the bucket stores.
    let stored = [30u8, 10, 50, 20, 40];
    let mut present: Vec<u8> = stored.to_vec();
    {
        let store = open(dir.path()).await;
        for order in stored {
            put(&store, key(order), value(order as u64)).await;
        }
        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");

        // The first in order, then the last in order, then the entry stored last, then the rest.
        for removed in [10u8, 50, 20, 30, 40] {
            assert_eq!(
                swap(&store, key(removed), value(removed as u64), Hash::default()).await,
                value(removed as u64)
            );
            present.retain(|order| *order != removed);
            let after = entries(&store, key(removed)).await;
            assert_eq!(after.len(), present.len(), "after removing {removed}");
            assert!(!has_entry(&after, key(removed)));
            for order in &present {
                assert_eq!(
                    load(&store, key(*order)).await,
                    value(*order as u64),
                    "{order} after removing {removed}"
                );
            }
            let mut expected_listing: Vec<(Hash, Hash)> = present
                .iter()
                .map(|order| (typed(key(*order)), value(*order as u64)))
                .collect();
            expected_listing.sort();
            assert_eq!(listed(&store).await, expected_listing);
        }
        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        assert!(!held(&store), "the emptied bucket is written");
    }
    let store = open(dir.path()).await;
    assert!(
        entries(&store, key(10)).await.is_empty(),
        "the emptied bucket reached disk"
    );
    for order in stored {
        assert_eq!(load(&store, key(order)).await, Hash::default());
    }
}

/// How a test persists the store's changes.
#[derive(Clone, Copy, Debug)]
enum Persist {
    /// An explicit flush.
    Flush,
    /// The delayed flush's sweep, run at once.
    Sweep,
}

async fn persist(store: &Arc<LocalMutableStore>, how: Persist) {
    match how {
        Persist::Flush => store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes"),
        Persist::Sweep => {
            let weak = Arc::downgrade(store);
            for group_index in 0..GROUP_COUNT {
                LocalMutableStore::flush_delayed(weak.clone(), group_index, 0).await;
            }
        }
    }
}

/// A removal that leaves a bucket empty reaches disk, written by a flush or by the delayed flush,
/// and leaves the bucket unflagged and the store unclaimed.
#[tokio::test]
async fn a_removal_that_empties_a_bucket_reaches_disk() {
    for how in [Persist::Flush, Persist::Sweep] {
        let dir = TempDir::new("ms_zero_emptied_");
        {
            let store = open(dir.path()).await;
            put(&store, key(1), value(1)).await;
            persist(&store, how).await;
            assert_eq!(
                swap(&store, key(1), value(1), Hash::default()).await,
                value(1)
            );
            assert!(
                entries(&store, key(1)).await.is_empty(),
                "{how:?}: the bucket is empty"
            );
            persist(&store, how).await;
            assert!(
                !flagged(&store, key(1)),
                "{how:?}: the empty bucket is written"
            );
            assert!(!held(&store), "{how:?}: and releases the store");
        }
        let store = open(dir.path()).await;
        assert!(
            entries(&store, key(1)).await.is_empty(),
            "{how:?}: the removal reached disk"
        );
        assert_eq!(load(&store, key(1)).await, Hash::default());
    }
}

/// A removal that empties a bucket of a group written at the flat layout without a level marker,
/// as the delayed flush of a server store and every store from before fan-out leave one, reaches
/// disk through the flush that records the group's level.
#[tokio::test]
async fn a_removal_that_empties_a_bucket_of_a_flat_group_without_a_marker_reaches_disk() {
    let dir = TempDir::new("ms_zero_emptied_flat_");
    let flat = || MutableStoreSettings {
        initial_fan_out_level: FAN_OUT_LEVEL_MAX,
        ..client_settings()
    };
    {
        let store = open_with(dir.path(), flat()).await;
        put(&store, key(1), value(1)).await;
        persist(&store, Persist::Sweep).await;
    }
    {
        let store = open_with(dir.path(), flat()).await;
        let group = &store.group[GROUP as usize];
        assert_eq!(
            (
                group.bucket_count.load(atomic::Ordering::Relaxed),
                group.committed_level.load(atomic::Ordering::Relaxed)
            ),
            (FAN_OUT_LEVEL_MAX, 0),
            "the group has bucket files and no marker"
        );
        assert_eq!(
            swap(&store, key(1), value(1), Hash::default()).await,
            value(1)
        );
        persist(&store, Persist::Flush).await;
        assert!(!flagged(&store, key(1)), "the empty bucket is written");
        assert!(!held(&store));
    }
    let store = open_with(dir.path(), flat()).await;
    assert!(
        entries(&store, key(1)).await.is_empty(),
        "the removal reached disk"
    );
    assert_eq!(load(&store, key(1)).await, Hash::default());
}

/// Racers swapping and removing keys that share one bucket, with flushes running alongside, leave
/// each key holding what its history of swaps says, with no entry for a key that ends absent, in
/// memory and on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn racing_swaps_and_removals_in_one_bucket_form_one_history() {
    let dir = TempDir::new("ms_zero_race_");
    let keys: Vec<Hash> = (0..16u8).map(|order| key(order * 8 + 3)).collect();
    let ends = {
        let store = open(dir.path()).await;
        let stop = Arc::new(AtomicBool::new(false));
        let flusher = {
            let store = store.clone();
            let stop = stop.clone();
            lore_base::lore_spawn!(async move {
                while !stop.load(atomic::Ordering::Relaxed) {
                    store
                        .clone()
                        .flush(FlushOptions::default())
                        .await
                        .expect("flushes");
                    tokio::task::yield_now().await;
                }
            })
        };
        let racing: Arc<dyn MutableStore> = store.clone();
        let ends = lore_storage::mutable_conformance::race_swaps(
            &racing,
            Partition::default(),
            KEY_TYPE,
            &keys,
            16,
            200,
        )
        .await
        .unwrap_or_else(|why| panic!("{why}"));
        stop.store(true, atomic::Ordering::Relaxed);
        flusher.await.expect("the flusher ends");
        assert_holds(&store, &keys, &ends).await;
        store
            .clone()
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        assert!(!held(&store));
        ends
    };
    let store = open(dir.path()).await;
    assert_holds(&store, &keys, &ends).await;
}

/// Asserts that the bucket holding `keys` holds an entry for each key whose end is non-zero, with
/// that value, and no other entry.
async fn assert_holds(store: &Arc<LocalMutableStore>, keys: &[Hash], ends: &[Hash]) {
    let in_bucket = entries(store, keys[0]).await;
    let present = ends.iter().filter(|end| !end.is_zero()).count();
    assert_eq!(
        in_bucket.len(),
        present,
        "one entry per key that ends present"
    );
    for (key, end) in keys.iter().zip(ends) {
        let entry = in_bucket.iter().find(|entry| entry.key == typed(*key));
        assert_eq!(
            entry.map(|entry| entry.value),
            (!end.is_zero()).then_some(*end),
            "the entry of {key}"
        );
        assert_eq!(load(store, *key).await, *end);
    }
}
