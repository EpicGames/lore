// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use lore_storage::FlushOptions;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::immutable_store::StoreError;
use lore_storage::local::store_lock::StoreGuard;
use lore_storage::mutable_conformance::Capabilities;
use lore_storage::mutable_conformance::Check;
use lore_storage::mutable_conformance::RacedSwap;
use lore_storage::mutable_conformance::history_end;
use lore_storage::mutable_conformance::race_swaps;
use lore_storage::mutable_conformance::verify_mutable_store;
use lore_storage::mutable_store::MutableStore;
use lore_storage::store_types::KeyType;
use lore_storage::store_types::KeyValueStream;

use crate::local::mutable_store::make_in_memory_mutable;

fn value(n: u8) -> Hash {
    Hash::from([n; 32])
}

/// A swap of the history's one key from `expected` to `value`, answering `held`, called from
/// `span.0` to `span.1` milliseconds after `start`.
fn swap(start: Instant, span: (u64, u64), expected: Hash, value: Hash, held: Hash) -> RacedSwap {
    RacedSwap {
        key: 0,
        expected,
        value,
        held,
        started: start + Duration::from_millis(span.0),
        answered: start + Duration::from_millis(span.1),
    }
}

/// A key given `value(1)` from 10 to 20 ms, then swapped from it to `value(2)` from 30 to 40 ms,
/// with `probe` beside those two swaps.
fn history_with(start: Instant, probe: RacedSwap) -> Result<Hash, String> {
    let given = swap(start, (10, 20), Hash::default(), value(1), Hash::default());
    let taken = swap(start, (30, 40), value(1), value(2), value(1));
    history_end([given, taken, probe].iter())
}

/// A swap to the value it expected, or a refused one, answering a value the key held at some
/// point while the call ran, keeps the history: from the instant the swap that gave the key the
/// value started to the instant the swap from it answered, both included. One answering zero
/// keeps it too.
#[test]
fn a_swap_that_changes_nothing_answers_a_value_held_while_it_ran() {
    let start = Instant::now();
    for span in [(5, 10), (15, 25), (22, 28), (35, 45), (40, 50)] {
        for probe in [
            swap(start, span, value(1), value(1), value(1)),
            swap(start, span, value(9), value(8), value(1)),
        ] {
            assert_eq!(history_with(start, probe), Ok(value(2)), "{probe:?}");
        }
    }
    for probe in [
        swap(
            start,
            (0, 5),
            Hash::default(),
            Hash::default(),
            Hash::default(),
        ),
        swap(start, (0, 5), value(9), value(8), Hash::default()),
    ] {
        assert_eq!(history_with(start, probe), Ok(value(2)), "{probe:?}");
    }
}

/// A swap to the value it expected, or a refused one, answering a value the key held only before
/// the call started or only after it answered, fails the history.
#[test]
fn a_swap_that_changes_nothing_answering_a_value_not_held_while_it_ran_fails_the_history() {
    let start = Instant::now();
    for span in [(0, 5), (45, 55)] {
        for probe in [
            swap(start, span, value(1), value(1), value(1)),
            swap(start, span, value(9), value(8), value(1)),
        ] {
            assert!(history_with(start, probe).is_err(), "{probe:?}");
        }
    }
}

/// A history fails where a swap changed the key from a value no swap gave it, where two swaps
/// changed it from one value, where its changes loop, where it ends at two values, and where a
/// change isn't reached from zero, each naming its break.
#[test]
fn a_broken_history_fails_naming_its_break() {
    let start = Instant::now();
    let changed = |from: u8, to: u8| swap(start, (0, 1), value(from), value(to), value(from));
    let cases = [
        (
            vec![changed(0, 1), changed(2, 3)],
            format!(
                "a swap changed the key from {}, which no swap gave it",
                value(2)
            ),
        ),
        (
            vec![changed(0, 1), changed(1, 2), changed(1, 3)],
            format!("two swaps changed the key from {}", value(1)),
        ),
        (
            vec![changed(0, 1), changed(1, 2), changed(2, 1)],
            format!("the swaps' history loops through {}", value(1)),
        ),
        (
            vec![changed(0, 1), changed(0, 2)],
            format!("the history ends at both {} and {}", value(1), value(2)),
        ),
        (
            vec![changed(0, 1), changed(3, 4), changed(4, 3)],
            "every swap that changed the key must chain from zero (got 1, expected 3)".to_string(),
        ),
    ];
    for (history, broken) in cases {
        assert_eq!(history_end(history.iter()), Err(broken), "{history:?}");
    }
}

/// A mutable store that answers a swap from a non-zero value to that same value with the value,
/// without reading the key, after yielding to the runtime. It passes every other call to `base`.
struct UnreadSwapToExpected {
    base: Arc<dyn MutableStore>,
}

#[async_trait]
impl MutableStore for UnreadSwapToExpected {
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.base.clone().hold_for_command().await
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.base.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        self.base
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        if value == expected && !expected.is_zero() {
            tokio::task::yield_now().await;
            return Ok(expected);
        }
        self.base
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.base.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.base.clone().flush(options).await
    }
}

/// The race fails a store that answers a swap to the value it expected without reading the key:
/// its racers swap from values they saw the key hold before another racer swapped from them.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_race_fails_a_store_answering_a_swap_to_the_expected_value_unread() {
    let store: Arc<dyn MutableStore> = Arc::new(UnreadSwapToExpected {
        base: make_in_memory_mutable().await,
    });
    let keys: Vec<Hash> = (1..=2).map(Hash::from_u64).collect();
    let raced = race_swaps(
        &store,
        Partition::default(),
        KeyType::BranchMetadata,
        &keys,
        8,
        200,
    )
    .await;
    assert!(raced.is_err(), "{raced:?}");
}

/// A mutable store whose list also sends each key a store or a swap to zero removed, holding zero,
/// as a store that keeps an entry holding zero does. It passes every call to `base`.
struct ListsRemovedKeys {
    base: Arc<dyn MutableStore>,
    removed: Mutex<Vec<(Partition, Hash, KeyType)>>,
}

impl ListsRemovedKeys {
    fn removed(&self, partition: Partition, key: Hash, key_type: KeyType) {
        self.removed
            .lock()
            .expect("the removed keys' lock")
            .push((partition, key, key_type));
    }
}

#[async_trait]
impl MutableStore for ListsRemovedKeys {
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.base.clone().hold_for_command().await
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.base.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        self.base
            .clone()
            .store(partition, key, value, key_type)
            .await?;
        if value.is_zero() {
            self.removed(partition, key, key_type);
        }
        Ok(())
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let held = self
            .base
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await?;
        if held == expected && value.is_zero() {
            self.removed(partition, key, key_type);
        }
        Ok(held)
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        let mut listed = self.base.clone().list(partition, key_type).await?;
        if key_type == KeyType::Untyped {
            return Ok(listed);
        }
        let (stream, sender) = KeyValueStream::new();
        while let Some(item) = listed.next().await {
            let _ = sender.send(item);
        }
        for (removed_partition, key, removed_type) in
            self.removed.lock().expect("the removed keys' lock").iter()
        {
            if *removed_type == key_type && (partition.is_zero() || *removed_partition == partition)
            {
                let _ = sender.send((*key, Hash::default()));
            }
        }
        Ok(stream)
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.base.clone().flush(options).await
    }
}

/// The battery fails a store whose list sends the keys a store or a swap to zero removed, and
/// fails it at that check alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_battery_fails_a_store_listing_removed_keys() {
    verify_mutable_store(
        Arc::new(ListsRemovedKeys {
            base: make_in_memory_mutable().await,
            removed: Mutex::default(),
        }),
        Capabilities::new("ListsRemovedKeys").known_violations(&[Check::ListLeavesOutRemovedKeys]),
    )
    .await;
}

/// The race fails on a current-thread runtime, where racers of a store whose swap doesn't suspend
/// never overlap.
#[tokio::test]
async fn the_race_refuses_a_current_thread_runtime() {
    let store: Arc<dyn MutableStore> = make_in_memory_mutable().await;
    let raced = race_swaps(
        &store,
        Partition::default(),
        KeyType::BranchMetadata,
        &[Hash::from_u64(1)],
        2,
        2,
    )
    .await;
    assert!(raced.is_err(), "{raced:?}");
}

/// A mutable store whose swap loads the key and then stores the value, each under the lock of its
/// own call to `base`, so another writer can come between them. It passes every other call to
/// `base`.
struct SwapReadThenWritten {
    base: Arc<dyn MutableStore>,
}

#[async_trait]
impl MutableStore for SwapReadThenWritten {
    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.base.clone().hold_for_command().await
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.base.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        self.base
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let held = match self.base.clone().load(partition, key, key_type).await {
            Ok(held) => held,
            Err(StoreError::AddressNotFound(_)) => Hash::default(),
            Err(err) => return Err(err),
        };
        if held != expected {
            return Ok(held);
        }
        self.base
            .clone()
            .store(partition, key, value, key_type)
            .await?;
        Ok(expected)
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.base.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.base.clone().flush(options).await
    }
}

/// On a multi-thread runtime the race fails a store whose swap reads and writes the key apart:
/// two racers read one value and both swap from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_race_fails_a_store_whose_swap_reads_and_writes_apart() {
    let store: Arc<dyn MutableStore> = Arc::new(SwapReadThenWritten {
        base: make_in_memory_mutable().await,
    });
    let keys: Vec<Hash> = (1..=2).map(Hash::from_u64).collect();
    let raced = race_swaps(
        &store,
        Partition::default(),
        KeyType::BranchMetadata,
        &keys,
        8,
        200,
    )
    .await;
    assert!(raced.is_err(), "{raced:?}");
}
