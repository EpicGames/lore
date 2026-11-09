// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Payloads and entries for tests of the local immutable store's eviction and compaction.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic;
use std::time::Duration;

use bytes::Bytes;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentFlags;
use lore_storage::Partition;
use lore_storage::hash;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::immutable_store::StoreError;
use lore_storage::local::immutable_store::CompactionInstruments;
use lore_storage::local::immutable_store::ImmutableStoreEntry;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::local::store_lock::Intent;

/// A payload of `size` bytes, at least 4, that `seed` makes distinct.
fn seeded_payload(seed: u32, size: usize) -> Vec<u8> {
    let mut payload = vec![0u8; size];
    payload[..4].copy_from_slice(&seed.to_le_bytes());
    payload
}

/// A payload of 4 KiB whose hash falls in a group within `groups`.
pub(crate) fn payload_in_groups(groups: std::ops::Range<usize>) -> Vec<u8> {
    (0u32..)
        .map(|seed| seeded_payload(seed, 4096))
        .find(|payload| groups.contains(&(hash::hash_slice(payload).data()[0] as usize)))
        .expect("some seed lands in the range")
}

/// `count` payloads of 4 KiB whose hashes all fall in one group, and that group's index.
pub(crate) fn payloads_in_one_group(count: usize) -> (usize, Vec<Vec<u8>>) {
    payloads_of_size_in_one_group(count, 4096)
}

/// `count` payloads of `size` bytes, at least 4, whose hashes all fall in one group, and that
/// group's index.
pub(crate) fn payloads_of_size_in_one_group(count: usize, size: usize) -> (usize, Vec<Vec<u8>>) {
    let payload = |seed: u32| seeded_payload(seed, size);
    let mut seeds_by_group: HashMap<usize, Vec<u32>> = HashMap::new();
    for seed in 0u32.. {
        let group_index = hash::hash_slice(&payload(seed)).data()[0] as usize;
        let seeds = seeds_by_group.entry(group_index).or_default();
        seeds.push(seed);
        if seeds.len() == count {
            return (
                group_index,
                seeds.iter().map(|seed| payload(*seed)).collect(),
            );
        }
    }
    unreachable!("some group fills before the seeds run out")
}

/// `count` payloads of 4 KiB in each of two groups, the first two groups to hold `count`, with
/// each group's index.
pub(crate) fn payloads_in_two_groups(count: usize) -> [(usize, Vec<Vec<u8>>); 2] {
    let payload = |seed: u32| seeded_payload(seed, 4096);
    let mut seeds_by_group: HashMap<usize, Vec<u32>> = HashMap::new();
    let mut full = vec![];
    for seed in 0u32.. {
        let group_index = hash::hash_slice(&payload(seed)).data()[0] as usize;
        let seeds = seeds_by_group.entry(group_index).or_default();
        seeds.push(seed);
        if seeds.len() == count {
            full.push((
                group_index,
                seeds.iter().map(|seed| payload(*seed)).collect(),
            ));
            if full.len() == 2 {
                return [full.remove(0), full.remove(0)];
            }
        }
    }
    unreachable!("two groups fill before the seeds run out")
}

/// Store `payload` in `store` under `context`, as a client stores it: held locally, and durable
/// as `durable` says. Returns the address naming it.
pub(crate) async fn put_payload(
    store: &Arc<LocalImmutableStore>,
    partition: Partition,
    context: Context,
    payload: &[u8],
    durable: bool,
) -> Address {
    try_put_payload(store, partition, context, payload, durable, false)
        .await
        .unwrap()
}

/// As [`put_payload`], writing the payload again even when one is stored for its hash with
/// `force`, and returning the error the store fails with.
pub(crate) async fn try_put_payload(
    store: &Arc<LocalImmutableStore>,
    partition: Partition,
    context: Context,
    payload: &[u8],
    durable: bool,
    force: bool,
) -> Result<Address, StoreError> {
    let address = Address {
        hash: hash::hash_slice(payload),
        context,
    };
    let fragment = Fragment {
        flags: if durable {
            (FragmentFlags::PayloadStoredLocal | FragmentFlags::PayloadStoredDurable).bits()
        } else {
            FragmentFlags::PayloadStoredLocal.bits()
        },
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    store
        .clone()
        .put(
            partition,
            address,
            fragment,
            Some(Bytes::copy_from_slice(payload)),
            force,
        )
        .await?;
    Ok(address)
}

/// Wait until `condition` holds, or `task` finishes, for at most 10 s.
pub(crate) async fn wait_until<T>(condition: impl Fn() -> bool, task: &tokio::task::JoinHandle<T>) {
    for _ in 0..10_000 {
        if condition() || task.is_finished() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the condition never held");
}

/// Wait until `condition` holds, failing with `failure` after 10 s.
pub(crate) async fn wait_for(condition: impl AsyncFn() -> bool, failure: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect(failure);
}

/// Assert that each of `payloads`, an address and the bytes stored under it, reads back from
/// `store`.
pub(crate) async fn assert_payloads_read_back(
    store: &Arc<LocalImmutableStore>,
    partition: Partition,
    payloads: impl IntoIterator<Item = (Address, Vec<u8>)>,
) {
    for (address, payload) in payloads {
        let (_fragment, read) = store
            .clone()
            .get(partition, address)
            .await
            .and_then(lore_storage::store_types::StoreGetData::into_payload)
            .expect("the payload reads back");
        assert_eq!(read.as_ref(), payload.as_slice());
    }
}

/// Compact `group_index` of `store` towards `target_size` bytes, protecting local payloads as
/// `protect_local_fragment` says. Returns whether the group was left complete.
pub(crate) async fn compact_group_toward(
    store: &Arc<LocalImmutableStore>,
    group_index: usize,
    target_size: usize,
    protect_local_fragment: bool,
) -> bool {
    try_compact_group_toward(store, group_index, target_size, protect_local_fragment)
        .await
        .unwrap()
}

/// As [`compact_group_toward`], returning the error that ended a rewrite. The group is compacted
/// under a write claim, as a compaction pass compacts it.
pub(crate) async fn try_compact_group_toward(
    store: &Arc<LocalImmutableStore>,
    group_index: usize,
    target_size: usize,
    protect_local_fragment: bool,
) -> Result<bool, StoreError> {
    let _claim = store.hold(Intent::Write).await?;
    store
        .clone()
        .compact_group_packfiles(
            group_index,
            store.path.clone(),
            target_size,
            protect_local_fragment,
            false,
            CompactionInstruments::default(),
            None,
        )
        .await
}

/// The entries of the group's loaded buckets, by hash.
pub(crate) async fn group_entries(
    store: &LocalImmutableStore,
    group_index: usize,
) -> Vec<ImmutableStoreEntry> {
    let group = &store.group[group_index];
    let mut entries = vec![];
    for bucket_index in 0..group.bucket_count.load(atomic::Ordering::Relaxed) {
        let Some(bucket) = group.try_bucket(bucket_index) else {
            continue;
        };
        entries.extend(bucket.read().await.entry.iter().copied());
    }
    entries.sort_unstable_by_key(|entry| entry.address.hash);
    entries
}
