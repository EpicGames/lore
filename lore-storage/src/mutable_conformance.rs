// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The contract every [`MutableStore`] owes its callers, as an executable battery.
//!
//! A caller reaching a store through `Arc<dyn MutableStore>` cannot see which implementation it
//! has, so the guarantees must not depend on that. This module states those guarantees once and
//! checks them against every implementation, rather than restating them in each store's own tests
//! in each store's own words.
//!
//! It is behind the `test-util` feature rather than `#[cfg(test)]`: the implementations live in
//! several crates, and each calls [`verify_mutable_store`] from a test of its own, with the
//! construction that crate already uses.
//!
//! The clauses, derived from [`crate::local::mutable_store::LocalMutableStore`] behaviour:
//!
//! 1. **A stored key loads back.** Storing a non-zero value and immediately loading it returns
//!    that value.
//! 2. **Overwrite is visible.** Storing a second value for the same key replaces the first; a
//!    subsequent load returns the new value.
//! 3. **Zero removes the key.** Storing `Hash::default()` makes subsequent loads return
//!    `AddressNotFound`, whether or not an entry existed before.
//! 4. **Partition and key type isolate entries.** A value stored under one partition or key type
//!    cannot be read under another.
//! 5. **Compare-and-swap returns the previous value.** The returned hash is what the key held
//!    when the store compared it with `expected`. A returned value equal to `expected` means the
//!    swap succeeded; any other value means it did not and nothing was changed.
//! 6. **`list` with `Untyped` is always empty.** Typed entries are never reported under
//!    [`KeyType::Untyped`].
//! 7. **`list` filters by key type and partition.** Only entries of the requested type appear,
//!    and only entries belonging to the requested partition — unless the partition is null, in
//!    which case all partitions are included. A key a store or a swap to zero removed is not
//!    listed.
//! 8. **A swap to zero removes the key.** A compare-and-swap whose expected value matches and
//!    whose new value is zero leaves the key absent, and one from zero to zero leaves an absent
//!    key absent. A removed key is created again by a swap from zero.
//! 9. **Concurrent swaps form one history.** Every swap a racer makes answers zero or a value the
//!    key held, a swap that changes nothing answers zero or a value the key held while the call
//!    ran, no two swaps that change the key succeed from one non-zero value, and the key ends
//!    holding what those swaps leave it, see [`race_swaps`]. The key holds zero again each time a
//!    swap removes it, so swaps from zero succeed once per removal, and a zero answer is not placed
//!    in time.
//!
//! # Known violations
//!
//! A new implementation that does not yet satisfy every clause declares the checks it fails via
//! [`Capabilities::known_violations`]. The battery then *requires* those to fail: a defect that
//! gets fixed without being delisted fails the test just as loudly as a new regression. The list
//! is the work queue toward full compliance.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;

use crate::FlushOptions;
use crate::Hash;
use crate::Partition;
use crate::immutable_store::StoreError;
use crate::mutable_store::MutableStore;
use crate::store_types::KeyType;

/// One case in the battery. Named so a store can declare which ones it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// Loading a key that was never stored returns `AddressNotFound`.
    LoadMissingKeyReturnsNotFound,
    /// A stored non-zero value can be loaded back by the same partition and key type.
    LoadStoredKeyReturnsValue,
    /// After storing `Hash::default()` for an existing key, loading it returns `AddressNotFound`.
    LoadDeletedKeyReturnsNotFound,
    /// A value stored under one partition is invisible from a different partition.
    LoadDoesNotCrossPartitions,
    /// A value stored under one key type is invisible under a different key type.
    LoadDoesNotCrossKeyTypes,
    /// Storing a second value for an existing key replaces the first.
    StoreOverwriteUpdatesValue,
    /// Storing the same value for an existing key is accepted and the value is still loadable.
    StoreIdempotentOnSameValue,
    /// Storing `Hash::default()` for an existing key makes subsequent loads return
    /// `AddressNotFound`.
    StoreZeroDeletesKey,
    /// Storing `Hash::default()` for a key that was never stored succeeds without creating an
    /// entry; load returns `AddressNotFound`.
    StoreZeroOnMissingIsNoOp,
    /// When the current value equals `expected`, compare-and-swap installs the new value and
    /// returns the previous value (equal to `expected`).
    CasSucceedsWhenExpectedMatches,
    /// When the current value differs from `expected`, compare-and-swap leaves the entry
    /// unchanged and returns the actual current value (not equal to `expected`).
    CasFailsWhenExpectedDoesNotMatch,
    /// When the key does not exist and `expected` is zero, compare-and-swap inserts the new value
    /// and returns `Hash::default()` (equal to `expected`).
    CasInsertsWhenMissingAndExpectedIsZero,
    /// When the key does not exist and `expected` is non-zero, compare-and-swap does nothing and
    /// returns `Hash::default()` (not equal to `expected`).
    CasDoesNothingWhenMissingAndExpectedIsNonZero,
    /// When the current value equals `expected` and the new value is zero, compare-and-swap
    /// removes the key and returns the previous value.
    CasToZeroRemovesKey,
    /// When the current value differs from `expected`, a compare-and-swap to zero leaves the entry
    /// unchanged and returns the current value.
    CasToZeroFailsWhenExpectedDoesNotMatch,
    /// When the key does not exist and `expected` is non-zero, a compare-and-swap to zero does
    /// nothing and returns `Hash::default()`.
    CasToZeroOnMissingAnswersZero,
    /// When the key does not exist, a compare-and-swap from zero to zero succeeds and creates no
    /// entry.
    CasZeroToZeroOnMissingIsNoOp,
    /// A key a compare-and-swap removed is created again by a compare-and-swap from zero.
    CasRecreatesRemovedKey,
    /// A compare-and-swap from the current value to that same value succeeds and leaves it.
    CasToHeldValueSucceeds,
    /// Compare-and-swaps raced over a few keys form one history per key, see [`race_swaps`].
    ConcurrentSwapsFormOneHistory,
    /// `list` with [`KeyType::Untyped`] returns an empty stream regardless of what is stored.
    ListUntypedIsEmpty,
    /// Entries stored with a typed key type appear in a subsequent `list` for that type.
    ListReturnsStoredEntries,
    /// Entries of a different key type than the one requested are not returned by `list`.
    ListFiltersOtherKeyTypes,
    /// `list` with a specific partition does not return entries stored under other partitions.
    ListRespectsPartition,
    /// `list` with a null partition returns entries from all partitions.
    ListNullPartitionMatchesAll,
    /// `list` leaves out a key a store of zero removed and one a compare-and-swap to zero removed.
    ListLeavesOutRemovedKeys,
    /// `flush` completes without error for both `sync_data = false` and `sync_data = true`.
    FlushSucceeds,
}

/// What the store under test is expected to satisfy, so the work queue is explicit.
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    /// Names the store in assertion messages. A failure has to say which implementation failed.
    pub label: &'static str,
    /// Checks this store is known to fail today. Each entry should name the defect in a comment
    /// at the call site. Required to keep failing — see the module docs.
    pub known_violations: &'static [Check],
}

impl Capabilities {
    /// A store expected to pass every check.
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            known_violations: &[],
        }
    }

    /// Declare the checks this store fails today.
    pub fn known_violations(mut self, checks: &'static [Check]) -> Self {
        self.known_violations = checks;
        self
    }
}

/// Fail the current check with a formatted message rather than panicking directly.
macro_rules! require {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

macro_rules! require_eq {
    ($left:expr, $right:expr, $($arg:tt)*) => {
        {
            let left = $left;
            let right = $right;
            if left != right {
                return Err(format!(
                    "{} (got {left:?}, expected {right:?})",
                    format!($($arg)*)
                ));
            }
        }
    };
}

/// Run the whole battery. Panics on the first unexpected outcome, naming the check and the store.
/// It must run on a multi-thread runtime, as [`race_swaps`] does.
pub async fn verify_mutable_store(store: Arc<dyn MutableStore>, caps: Capabilities) {
    settle(
        &caps,
        Check::LoadMissingKeyReturnsNotFound,
        load_missing_key_returns_not_found(&store).await,
    );
    settle(
        &caps,
        Check::LoadStoredKeyReturnsValue,
        load_stored_key_returns_value(&store).await,
    );
    settle(
        &caps,
        Check::LoadDeletedKeyReturnsNotFound,
        load_deleted_key_returns_not_found(&store).await,
    );
    settle(
        &caps,
        Check::LoadDoesNotCrossPartitions,
        load_does_not_cross_partitions(&store).await,
    );
    settle(
        &caps,
        Check::LoadDoesNotCrossKeyTypes,
        load_does_not_cross_key_types(&store).await,
    );
    settle(
        &caps,
        Check::StoreOverwriteUpdatesValue,
        store_overwrite_updates_value(&store).await,
    );
    settle(
        &caps,
        Check::StoreIdempotentOnSameValue,
        store_idempotent_on_same_value(&store).await,
    );
    settle(
        &caps,
        Check::StoreZeroDeletesKey,
        store_zero_deletes_key(&store).await,
    );
    settle(
        &caps,
        Check::StoreZeroOnMissingIsNoOp,
        store_zero_on_missing_is_no_op(&store).await,
    );
    settle(
        &caps,
        Check::CasSucceedsWhenExpectedMatches,
        cas_succeeds_when_expected_matches(&store).await,
    );
    settle(
        &caps,
        Check::CasFailsWhenExpectedDoesNotMatch,
        cas_fails_when_expected_does_not_match(&store).await,
    );
    settle(
        &caps,
        Check::CasInsertsWhenMissingAndExpectedIsZero,
        cas_inserts_when_missing_and_expected_is_zero(&store).await,
    );
    settle(
        &caps,
        Check::CasDoesNothingWhenMissingAndExpectedIsNonZero,
        cas_does_nothing_when_missing_and_expected_is_nonzero(&store).await,
    );
    settle(
        &caps,
        Check::CasToZeroRemovesKey,
        cas_to_zero_removes_key(&store).await,
    );
    settle(
        &caps,
        Check::CasToZeroFailsWhenExpectedDoesNotMatch,
        cas_to_zero_fails_when_expected_does_not_match(&store).await,
    );
    settle(
        &caps,
        Check::CasToZeroOnMissingAnswersZero,
        cas_to_zero_on_missing_answers_zero(&store).await,
    );
    settle(
        &caps,
        Check::CasZeroToZeroOnMissingIsNoOp,
        cas_zero_to_zero_on_missing_is_no_op(&store).await,
    );
    settle(
        &caps,
        Check::CasRecreatesRemovedKey,
        cas_recreates_removed_key(&store).await,
    );
    settle(
        &caps,
        Check::CasToHeldValueSucceeds,
        cas_to_held_value_succeeds(&store).await,
    );
    settle(
        &caps,
        Check::ConcurrentSwapsFormOneHistory,
        concurrent_swaps_form_one_history(&store).await,
    );
    settle(
        &caps,
        Check::ListUntypedIsEmpty,
        list_untyped_is_empty(&store).await,
    );
    settle(
        &caps,
        Check::ListReturnsStoredEntries,
        list_returns_stored_entries(&store).await,
    );
    settle(
        &caps,
        Check::ListFiltersOtherKeyTypes,
        list_filters_other_key_types(&store).await,
    );
    settle(
        &caps,
        Check::ListRespectsPartition,
        list_respects_partition(&store).await,
    );
    settle(
        &caps,
        Check::ListNullPartitionMatchesAll,
        list_null_partition_matches_all(&store).await,
    );
    settle(
        &caps,
        Check::ListLeavesOutRemovedKeys,
        list_leaves_out_removed_keys(&store).await,
    );
    settle(&caps, Check::FlushSucceeds, flush_succeeds(&store).await);
}

/// Decide what an outcome means given what the store declared.
///
/// A check that passes while listed as a known violation is as much a failure as one that fails
/// while not listed: the first means the list has rotted, and a rotted list is how a defect stops
/// being tracked.
fn settle(caps: &Capabilities, check: Check, outcome: Result<(), String>) {
    let known = caps.known_violations.contains(&check);
    match (outcome, known) {
        (Ok(()), false) | (Err(_), true) => {}
        (Err(why), false) => panic!("[{}] {check:?}: {why}", caps.label),
        (Ok(()), true) => panic!(
            "[{}] {check:?} is listed as a known violation but now holds — remove it from \
             known_violations",
            caps.label
        ),
    }
}

/// A random partition, key, and non-zero value, all unique to each call.
fn unique_fixture() -> (Partition, Hash, Hash) {
    (
        Partition::from(rand::random::<[u8; 16]>()),
        Hash::from(rand::random::<[u8; 32]>()),
        nonzero_hash(),
    )
}

/// A random hash that is guaranteed to be non-zero.
fn nonzero_hash() -> Hash {
    let mut bytes = rand::random::<[u8; 32]>();
    bytes[0] |= 1;
    Hash::from(bytes)
}

fn is_not_found(err: &StoreError) -> bool {
    matches!(err, StoreError::AddressNotFound(_))
}

/// Drain a `list` call into a `Vec`, propagating errors.
async fn collect_list(
    store: &Arc<dyn MutableStore>,
    partition: Partition,
    key_type: KeyType,
) -> Result<Vec<(Hash, Hash)>, String> {
    let mut stream = store
        .clone()
        .list(partition, key_type)
        .await
        .map_err(|e| format!("list({key_type:?}) failed: {e:?}"))?;
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    Ok(items)
}

// ── load ─────────────────────────────────────────────────────────────────────

/// Clause 1 at its boundary: a key the store has never seen is not there.
async fn load_missing_key_returns_not_found(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, _) = unique_fixture();
    let err = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .expect_err("load of an unstored key must fail");
    require!(
        is_not_found(&err),
        "load of an unstored key must return AddressNotFound, got {err:?}"
    );
    Ok(())
}

/// Clause 1 at its core: a stored non-zero value comes back on the next load.
async fn load_stored_key_returns_value(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("load after store failed: {e:?}"))?;
    require_eq!(loaded, value, "load must return the value that was stored");
    Ok(())
}

/// Clause 3: a zero stored over a live entry makes load report absence.
async fn load_deleted_key_returns_not_found(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    store
        .clone()
        .store(partition, key, Hash::default(), KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store-zero failed: {e:?}"))?;
    let err = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .expect_err("load after store-zero must fail");
    require!(
        is_not_found(&err),
        "load after storing zero must return AddressNotFound, got {err:?}"
    );
    Ok(())
}

/// Clause 4, partition axis: a value stored in partition A is absent in partition B.
async fn load_does_not_cross_partitions(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition_a, key, value) = unique_fixture();
    let partition_b = Partition::from(rand::random::<[u8; 16]>());
    store
        .clone()
        .store(partition_a, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let err = store
        .clone()
        .load(partition_b, key, KeyType::BranchMetadata)
        .await
        .expect_err("load from a different partition must fail");
    require!(
        is_not_found(&err),
        "load from a different partition must return AddressNotFound, got {err:?}"
    );
    Ok(())
}

/// Clause 4, key-type axis: the key-type byte is part of the stored key; loading under a
/// different type is a miss.
async fn load_does_not_cross_key_types(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let err = store
        .clone()
        .load(partition, key, KeyType::BranchId)
        .await
        .expect_err("load under a different key type must fail");
    require!(
        is_not_found(&err),
        "load under a different key type must return AddressNotFound, got {err:?}"
    );
    Ok(())
}

// ── store ─────────────────────────────────────────────────────────────────────

/// Clause 2: the second store wins.
async fn store_overwrite_updates_value(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value_a) = unique_fixture();
    let value_b = nonzero_hash();
    store
        .clone()
        .store(partition, key, value_a, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("first store failed: {e:?}"))?;
    store
        .clone()
        .store(partition, key, value_b, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("second store failed: {e:?}"))?;
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("load after overwrite failed: {e:?}"))?;
    require_eq!(loaded, value_b, "load must return the overwritten value");
    Ok(())
}

/// Storing the same value twice is accepted and the entry is still readable.
async fn store_idempotent_on_same_value(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("first store failed: {e:?}"))?;
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("second store (same value) failed: {e:?}"))?;
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("load after idempotent store failed: {e:?}"))?;
    require_eq!(
        loaded,
        value,
        "load must still return the value after storing it twice"
    );
    Ok(())
}

/// Clause 3, existing-entry branch: zero over a live entry makes it invisible to load.
async fn store_zero_deletes_key(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    store
        .clone()
        .store(partition, key, Hash::default(), KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("delete-via-zero failed: {e:?}"))?;
    let err = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .expect_err("load after delete must fail");
    require!(
        is_not_found(&err),
        "load after storing zero must return AddressNotFound, got {err:?}"
    );
    Ok(())
}

/// Clause 3, absent-key branch: storing zero for a key that does not exist succeeds and creates
/// no entry.
async fn store_zero_on_missing_is_no_op(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, _) = unique_fixture();
    store
        .clone()
        .store(partition, key, Hash::default(), KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store-zero on missing key failed: {e:?}"))?;
    let err = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .expect_err("load after store-zero on missing key must fail");
    require!(
        is_not_found(&err),
        "load after storing zero for a nonexistent key must return AddressNotFound, got {err:?}"
    );
    Ok(())
}

// ── compare_and_swap ──────────────────────────────────────────────────────────

/// Clause 5, success branch: when the current value equals `expected`, the swap happens and the
/// previous value is returned.
async fn cas_succeeds_when_expected_matches(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, initial) = unique_fixture();
    let replacement = nonzero_hash();
    store
        .clone()
        .store(partition, key, initial, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let previous = store
        .clone()
        .compare_and_swap(
            partition,
            key,
            initial,
            replacement,
            KeyType::BranchMetadata,
        )
        .await
        .map_err(|e| format!("compare_and_swap failed: {e:?}"))?;
    require_eq!(
        previous,
        initial,
        "compare_and_swap must return the previous value (equal to expected) on success"
    );
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("load after successful CAS failed: {e:?}"))?;
    require_eq!(
        loaded,
        replacement,
        "load after a successful CAS must return the replacement value"
    );
    Ok(())
}

/// Clause 5, failure branch: when the current value differs from `expected`, nothing is swapped
/// and the actual current value is returned.
async fn cas_fails_when_expected_does_not_match(
    store: &Arc<dyn MutableStore>,
) -> Result<(), String> {
    let (partition, key, actual) = unique_fixture();
    let replacement = nonzero_hash();
    // Guarantee wrong_expected != actual by flipping a known byte.
    let wrong_expected = {
        let mut bytes = rand::random::<[u8; 32]>();
        bytes[0] = actual.data()[0].wrapping_add(1) | 1;
        Hash::from(bytes)
    };
    store
        .clone()
        .store(partition, key, actual, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let returned = store
        .clone()
        .compare_and_swap(
            partition,
            key,
            wrong_expected,
            replacement,
            KeyType::BranchMetadata,
        )
        .await
        .map_err(|e| format!("compare_and_swap failed: {e:?}"))?;
    require_eq!(
        returned,
        actual,
        "compare_and_swap must return the actual current value when the swap fails"
    );
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("load after failed CAS failed: {e:?}"))?;
    require_eq!(loaded, actual, "entry must be unchanged after a failed CAS");
    Ok(())
}

/// Clause 5, missing-key + zero-expected branch: CAS on a nonexistent key with
/// `expected == Hash::default()` inserts the value and returns `Hash::default()`.
async fn cas_inserts_when_missing_and_expected_is_zero(
    store: &Arc<dyn MutableStore>,
) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    let returned = store
        .clone()
        .compare_and_swap(
            partition,
            key,
            Hash::default(),
            value,
            KeyType::BranchMetadata,
        )
        .await
        .map_err(|e| format!("compare_and_swap on missing key failed: {e:?}"))?;
    require_eq!(
        returned,
        Hash::default(),
        "compare_and_swap must return Hash::default() for a key that did not exist"
    );
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("load after CAS-insert failed: {e:?}"))?;
    require_eq!(
        loaded,
        value,
        "load after CAS-insert must return the inserted value"
    );
    Ok(())
}

/// Clause 5, missing-key + nonzero-expected branch: CAS on a nonexistent key with a non-zero
/// `expected` does not insert anything and returns `Hash::default()`.
async fn cas_does_nothing_when_missing_and_expected_is_nonzero(
    store: &Arc<dyn MutableStore>,
) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    let nonzero_expected = nonzero_hash();
    let returned = store
        .clone()
        .compare_and_swap(
            partition,
            key,
            nonzero_expected,
            value,
            KeyType::BranchMetadata,
        )
        .await
        .map_err(|e| format!("compare_and_swap failed: {e:?}"))?;
    require_eq!(
        returned,
        Hash::default(),
        "compare_and_swap on a missing key must return Hash::default()"
    );
    let err = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .expect_err("load after no-op CAS must fail — no entry was created");
    require!(
        is_not_found(&err),
        "load after no-op CAS must return AddressNotFound, got {err:?}"
    );
    Ok(())
}

/// Compare-and-swap of `key` in `partition` from `expected` to `value`, answering the value it
/// held.
async fn swap(
    store: &Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    expected: Hash,
    value: Hash,
) -> Result<Hash, String> {
    store
        .clone()
        .compare_and_swap(partition, key, expected, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("compare_and_swap({expected} -> {value}) failed: {e:?}"))
}

/// What a load of `key` in `partition` reads, zero for a key it does not find.
async fn load_or_zero(
    store: &Arc<dyn MutableStore>,
    partition: Partition,
    key: Hash,
    key_type: KeyType,
) -> Result<Hash, String> {
    match store.clone().load(partition, key, key_type).await {
        Ok(value) => Ok(value),
        Err(err) if is_not_found(&err) => Ok(Hash::default()),
        Err(err) => Err(format!("load failed: {err:?}")),
    }
}

/// Clause 8: a matching swap to zero removes the key.
async fn cas_to_zero_removes_key(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let previous = swap(store, partition, key, value, Hash::default()).await?;
    require_eq!(
        previous,
        value,
        "a swap to zero must return the previous value on success"
    );
    let loaded = load_or_zero(store, partition, key, KeyType::BranchMetadata).await?;
    require!(
        loaded.is_zero(),
        "load after a swap to zero must return AddressNotFound, got {loaded:?}"
    );
    Ok(())
}

/// Clause 8, failure branch: a swap to zero from the wrong value leaves the entry.
async fn cas_to_zero_fails_when_expected_does_not_match(
    store: &Arc<dyn MutableStore>,
) -> Result<(), String> {
    let (partition, key, actual) = unique_fixture();
    let wrong_expected = nonzero_hash();
    store
        .clone()
        .store(partition, key, actual, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let returned = swap(store, partition, key, wrong_expected, Hash::default()).await?;
    require_eq!(
        returned,
        actual,
        "a refused swap to zero must return the current value"
    );
    let loaded = load_or_zero(store, partition, key, KeyType::BranchMetadata).await?;
    require_eq!(
        loaded,
        actual,
        "a refused swap to zero must leave the entry"
    );
    Ok(())
}

/// Clause 8, missing-key branch: a swap to zero from a value the key never held answers zero.
async fn cas_to_zero_on_missing_answers_zero(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, expected) = unique_fixture();
    let returned = swap(store, partition, key, expected, Hash::default()).await?;
    require_eq!(
        returned,
        Hash::default(),
        "a swap to zero of a missing key must return Hash::default()"
    );
    let loaded = load_or_zero(store, partition, key, KeyType::BranchMetadata).await?;
    require!(
        loaded.is_zero(),
        "a swap to zero of a missing key must create nothing, got {loaded:?}"
    );
    Ok(())
}

/// Clause 8, zero-to-zero branch: a swap from zero to zero of a missing key succeeds and creates
/// nothing.
async fn cas_zero_to_zero_on_missing_is_no_op(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, _) = unique_fixture();
    let returned = swap(store, partition, key, Hash::default(), Hash::default()).await?;
    require_eq!(
        returned,
        Hash::default(),
        "a swap from zero to zero of a missing key must succeed"
    );
    let loaded = load_or_zero(store, partition, key, KeyType::BranchMetadata).await?;
    require!(
        loaded.is_zero(),
        "a swap from zero to zero must create nothing, got {loaded:?}"
    );
    Ok(())
}

/// Clause 8, recreation: a removed key is absent, so a swap from zero creates it again.
async fn cas_recreates_removed_key(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, first) = unique_fixture();
    let second = nonzero_hash();
    swap(store, partition, key, Hash::default(), first).await?;
    swap(store, partition, key, first, Hash::default()).await?;
    let returned = swap(store, partition, key, Hash::default(), second).await?;
    require_eq!(
        returned,
        Hash::default(),
        "a swap from zero of a removed key must succeed"
    );
    let loaded = load_or_zero(store, partition, key, KeyType::BranchMetadata).await?;
    require_eq!(loaded, second, "the recreated key must hold the new value");
    Ok(())
}

/// Clause 5, unchanged branch: a swap from the current value to itself succeeds.
async fn cas_to_held_value_succeeds(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let returned = swap(store, partition, key, value, value).await?;
    require_eq!(returned, value, "a swap to the current value must succeed");
    let loaded = load_or_zero(store, partition, key, KeyType::BranchMetadata).await?;
    require_eq!(loaded, value, "a swap to the current value must leave it");
    Ok(())
}

/// Clause 9, over a few fresh keys.
async fn concurrent_swaps_form_one_history(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, _, _) = unique_fixture();
    let keys: Vec<Hash> = (0..4).map(|_| nonzero_hash()).collect();
    race_swaps(store, partition, KeyType::BranchMetadata, &keys, 8, 40)
        .await
        .map(drop)
}

/// One compare-and-swap a racer of [`race_swaps`] made: the index of its key, the value it
/// expected, the value it swapped to, the value the store answered, and when the call started and
/// answered.
#[derive(Clone, Copy, Debug)]
pub struct RacedSwap {
    pub key: usize,
    pub expected: Hash,
    pub value: Hash,
    pub held: Hash,
    pub started: Instant,
    pub answered: Instant,
}

/// Races `racers` tasks, each making `swaps` compare-and-swaps of a random one of `keys`, of
/// `key_type` in `partition`, and checks the answers. Each swap is from the value a load of the
/// key reads, or from the value the racer last saw the key hold, to that same value, to a value
/// unique to the swap, or to zero. `keys` must be absent when the race starts, and the race must
/// run on a multi-thread runtime: on a current-thread one, racers of a store whose swap doesn't
/// suspend never overlap.
///
/// Answers the value each key holds at the end, in the order of `keys`, once:
///
/// - every swap that changed nothing, refused or to the value it expected, answered zero or a
///   value the key held while the call ran: for a non-zero value, the swap that gave the key that
///   value started no later than the call answered, and the swap that changed the key from it, if
///   any, answered no earlier than the call started,
/// - every swap that changed the key from a non-zero value swapped from a value such a swap gave
///   it, and no two changed it from one value,
/// - the swaps that changed the key chain from zero into one history, which ends in at most one
///   value,
/// - and a load of the key reads where that history ends.
///
/// # Errors
///
/// A current-thread runtime, the first key whose answers fail any of these, or a store call that
/// fails.
pub async fn race_swaps(
    store: &Arc<dyn MutableStore>,
    partition: Partition,
    key_type: KeyType,
    keys: &[Hash],
    racers: usize,
    swaps: usize,
) -> Result<Vec<Hash>, String> {
    require_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread,
        "the race runs on a multi-thread runtime"
    );
    let shared: Arc<[Hash]> = Arc::from(keys);
    let mut tasks = tokio::task::JoinSet::new();
    for racer in 0..racers {
        lore_base::lore_spawn!(
            tasks,
            race(
                store.clone(),
                partition,
                key_type,
                shared.clone(),
                racer,
                swaps
            )
        );
    }
    let mut made = Vec::with_capacity(racers * swaps);
    while let Some(joined) = tasks.join_next().await {
        made.extend(joined.map_err(|e| format!("racer task failed: {e}"))??);
    }
    let mut ends = Vec::with_capacity(keys.len());
    for (index, key) in keys.iter().enumerate() {
        let end = history_end(made.iter().filter(|swap| swap.key == index))
            .map_err(|why| format!("key {key}: {why}"))?;
        let loaded = load_or_zero(store, partition, *key, key_type).await?;
        require_eq!(
            loaded,
            end,
            "key {key}: a load must read where the swaps' history ends"
        );
        ends.push(end);
    }
    Ok(ends)
}

/// One racer of [`race_swaps`], answering the swaps it made.
async fn race(
    store: Arc<dyn MutableStore>,
    partition: Partition,
    key_type: KeyType,
    keys: Arc<[Hash]>,
    racer: usize,
    swaps: usize,
) -> Result<Vec<RacedSwap>, String> {
    let mut seen = vec![Hash::default(); keys.len()];
    let mut made = Vec::with_capacity(swaps);
    for swap in 0..swaps {
        let key = rand::random_range(0..keys.len());
        let expected = if rand::random_bool(0.5) {
            load_or_zero(&store, partition, keys[key], key_type).await?
        } else {
            seen[key]
        };
        let value = if rand::random_ratio(1, 4) {
            expected
        } else if rand::random_ratio(1, 3) {
            Hash::default()
        } else {
            raced_value(racer, swap)
        };
        let started = Instant::now();
        let held = store
            .clone()
            .compare_and_swap(partition, keys[key], expected, value, key_type)
            .await
            .map_err(|e| format!("compare_and_swap failed: {e:?}"))?;
        let answered = Instant::now();
        seen[key] = if held == expected { value } else { held };
        made.push(RacedSwap {
            key,
            expected,
            value,
            held,
            started,
            answered,
        });
    }
    Ok(made)
}

/// The non-zero value swap `swap` of racer `racer` swaps to, which no other swap uses.
fn raced_value(racer: usize, swap: usize) -> Hash {
    let mut bytes = [0u8; 32];
    bytes[0] = 0x5a;
    bytes[8..16].copy_from_slice(&(racer as u64).to_le_bytes());
    bytes[16..24].copy_from_slice(&(swap as u64).to_le_bytes());
    Hash::from(bytes)
}

/// Where the history of one key's raced swaps ends, as [`race_swaps`] checks it.
pub fn history_end<'a>(swaps: impl Iterator<Item = &'a RacedSwap>) -> Result<Hash, String> {
    let (changed, unchanged): (Vec<&RacedSwap>, Vec<&RacedSwap>) =
        swaps.partition(|swap| swap.held == swap.expected && swap.value != swap.expected);
    let given: HashMap<Hash, &RacedSwap> = changed
        .iter()
        .filter(|swap| !swap.value.is_zero())
        .map(|swap| (swap.value, *swap))
        .collect();
    let mut taken: HashMap<Hash, &RacedSwap> = HashMap::new();
    let mut from_zero = Vec::new();
    for swap in &changed {
        if swap.expected.is_zero() {
            from_zero.push(swap.value);
            continue;
        }
        require!(
            given.contains_key(&swap.expected),
            "a swap changed the key from {}, which no swap gave it",
            swap.expected
        );
        require!(
            taken.insert(swap.expected, *swap).is_none(),
            "two swaps changed the key from {}",
            swap.expected
        );
    }
    for swap in &unchanged {
        answered_while_held(swap, &given, &taken)?;
    }
    let mut walked = 0;
    let mut end = Hash::default();
    for start in from_zero {
        walked += 1;
        let mut at = start;
        while let Some(next) = taken.get(&at) {
            walked += 1;
            require!(
                walked <= changed.len(),
                "the swaps' history loops through {at}"
            );
            at = next.value;
        }
        if !at.is_zero() {
            require!(end.is_zero(), "the history ends at both {end} and {at}");
            end = at;
        }
    }
    require_eq!(
        walked,
        changed.len(),
        "every swap that changed the key must chain from zero"
    );
    Ok(end)
}

/// Checks that `swap`, which changed nothing, answered zero or a value the key held while the call
/// ran, as [`race_swaps`] describes. `given` maps each non-zero value a swap that changed the key
/// gave it to that swap, and `taken` each non-zero value such a swap changed it from to that swap.
/// A zero answer is not placed in time, as the key is absent over several spans.
fn answered_while_held(
    swap: &RacedSwap,
    given: &HashMap<Hash, &RacedSwap>,
    taken: &HashMap<Hash, &RacedSwap>,
) -> Result<(), String> {
    let held = swap.held;
    if held.is_zero() {
        return Ok(());
    }
    let Some(giver) = given.get(&held) else {
        return Err(format!(
            "a swap that changed nothing answered {held}, which the key never held"
        ));
    };
    require!(
        giver.started <= swap.answered,
        "a swap answered {held} before the swap that gave the key that value started"
    );
    if let Some(taker) = taken.get(&held) {
        require!(
            taker.answered >= swap.started,
            "a swap started after the swap from {held} answered, and answered {held}"
        );
    }
    Ok(())
}

// ── list ──────────────────────────────────────────────────────────────────────

/// Clause 6: the early-return for `Untyped` produces an empty stream even when typed entries
/// exist.
async fn list_untyped_is_empty(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let items = collect_list(store, partition, KeyType::Untyped).await?;
    require!(
        items.is_empty(),
        "list with KeyType::Untyped must return an empty stream, got {} item(s)",
        items.len()
    );
    Ok(())
}

/// Clause 7, positive side: a stored entry appears in the list for its key type.
async fn list_returns_stored_entries(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value) = unique_fixture();
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store failed: {e:?}"))?;
    let items = collect_list(store, partition, KeyType::BranchMetadata).await?;
    let values: Vec<Hash> = items.iter().map(|(_, v)| *v).collect();
    require!(
        values.contains(&value),
        "list must include the stored value; got {values:?}"
    );
    Ok(())
}

/// Clause 7, negative side: entries stored under one key type do not appear in a list for a
/// different key type.
async fn list_filters_other_key_types(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, key, value_metadata) = unique_fixture();
    let value_branch_id = nonzero_hash();

    store
        .clone()
        .store(partition, key, value_metadata, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store BranchMetadata failed: {e:?}"))?;
    store
        .clone()
        .store(partition, key, value_branch_id, KeyType::BranchId)
        .await
        .map_err(|e| format!("store BranchId failed: {e:?}"))?;

    let metadata_values: Vec<Hash> = collect_list(store, partition, KeyType::BranchMetadata)
        .await?
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    require!(
        metadata_values.contains(&value_metadata),
        "list(BranchMetadata) must return the BranchMetadata entry"
    );
    require!(
        !metadata_values.contains(&value_branch_id),
        "list(BranchMetadata) must not return a BranchId entry"
    );

    let branch_id_values: Vec<Hash> = collect_list(store, partition, KeyType::BranchId)
        .await?
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    require!(
        branch_id_values.contains(&value_branch_id),
        "list(BranchId) must return the BranchId entry"
    );
    require!(
        !branch_id_values.contains(&value_metadata),
        "list(BranchId) must not return a BranchMetadata entry"
    );

    Ok(())
}

/// Clause 7, partition filter: entries from another partition are excluded when the caller names
/// a specific partition.
async fn list_respects_partition(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition_a, key, value_a) = unique_fixture();
    let partition_b = Partition::from(rand::random::<[u8; 16]>());
    let value_b = nonzero_hash();

    store
        .clone()
        .store(partition_a, key, value_a, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store in partition_a failed: {e:?}"))?;
    store
        .clone()
        .store(partition_b, key, value_b, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store in partition_b failed: {e:?}"))?;

    let values_a: Vec<Hash> = collect_list(store, partition_a, KeyType::BranchMetadata)
        .await?
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    require!(
        values_a.contains(&value_a),
        "list(partition_a) must return the entry stored in partition_a"
    );
    require!(
        !values_a.contains(&value_b),
        "list(partition_a) must not return an entry stored in partition_b"
    );
    Ok(())
}

/// Clause 7, null-partition: a null partition acts as a wildcard and returns entries from every
/// partition.
async fn list_null_partition_matches_all(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition_a, key_a, value_a) = unique_fixture();
    let (partition_b, key_b, value_b) = unique_fixture();

    store
        .clone()
        .store(partition_a, key_a, value_a, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store in partition_a failed: {e:?}"))?;
    store
        .clone()
        .store(partition_b, key_b, value_b, KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store in partition_b failed: {e:?}"))?;

    let all_values: Vec<Hash> = collect_list(store, Partition::default(), KeyType::BranchMetadata)
        .await?
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    require!(
        all_values.contains(&value_a),
        "list(null partition) must include entries from partition_a"
    );
    require!(
        all_values.contains(&value_b),
        "list(null partition) must include entries from partition_b"
    );
    Ok(())
}

/// Clause 7, removed keys: a key a store of zero removed, and one a swap to zero removed, are not
/// listed.
async fn list_leaves_out_removed_keys(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    let (partition, stored, value) = unique_fixture();
    let swapped = nonzero_hash();
    for key in [stored, swapped] {
        store
            .clone()
            .store(partition, key, value, KeyType::BranchMetadata)
            .await
            .map_err(|e| format!("store failed: {e:?}"))?;
    }
    store
        .clone()
        .store(partition, stored, Hash::default(), KeyType::BranchMetadata)
        .await
        .map_err(|e| format!("store-zero failed: {e:?}"))?;
    swap(store, partition, swapped, value, Hash::default()).await?;
    let items = collect_list(store, partition, KeyType::BranchMetadata).await?;
    require!(
        items.is_empty(),
        "list must leave out keys a store or a swap to zero removed, got {items:?}"
    );
    Ok(())
}

// ── flush ─────────────────────────────────────────────────────────────────────

/// flush completes without error for both the soft (`sync_data = false`) and hard
/// (`sync_data = true`) forms.
async fn flush_succeeds(store: &Arc<dyn MutableStore>) -> Result<(), String> {
    store
        .clone()
        .flush(FlushOptions::default())
        .await
        .map_err(|e| format!("flush(false) failed: {e:?}"))?;
    store
        .clone()
        .flush(FlushOptions::SYNC_DATA)
        .await
        .map_err(|e| format!("flush(true) failed: {e:?}"))?;
    Ok(())
}
