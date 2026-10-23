// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::KeyType;
use lore_revision::store::write_discarding::WriteDiscardingMutableStore;
use lore_storage::Hash;
use lore_storage::MutableStore;
use lore_storage::Partition;

async fn in_memory_store() -> Arc<dyn MutableStore> {
    let immutable = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("Failed to create in-memory immutable store");
    lore_storage::local::mutable_store::create(
        None::<&std::path::Path>,
        lore_storage::MutableStoreSettings::default(),
        immutable,
    )
    .await
    .expect("Failed to create in-memory mutable store")
}

/// Writes through the store leave `base` as it was, and a swap answers the value `base` holds,
/// which reads as made where that is the value expected.
#[tokio::test]
async fn a_write_discarding_store_leaves_its_base_unchanged() {
    let base = in_memory_store().await;
    let partition = Partition::from(rand::random::<[u8; 16]>());
    let (stored, missing) = (
        Hash::from(rand::random::<[u8; 32]>()),
        Hash::from(rand::random::<[u8; 32]>()),
    );
    let (original, replacement) = (
        Hash::from(rand::random::<[u8; 32]>()),
        Hash::from(rand::random::<[u8; 32]>()),
    );
    base.clone()
        .store(partition, stored, original, KeyType::BranchId)
        .await
        .expect("Failed to store in the base");

    let discarding: Arc<dyn MutableStore> =
        Arc::new(WriteDiscardingMutableStore::new(base.clone()));
    discarding
        .clone()
        .store(partition, stored, replacement, KeyType::BranchId)
        .await
        .expect("Failed to store");
    let swapped = discarding
        .clone()
        .compare_and_swap(partition, stored, original, replacement, KeyType::BranchId)
        .await
        .expect("Failed to swap");
    let inserted = discarding
        .clone()
        .compare_and_swap(
            partition,
            missing,
            Hash::default(),
            replacement,
            KeyType::BranchId,
        )
        .await
        .expect("Failed to swap");

    assert_eq!(swapped, original, "a swap answers the value the base holds");
    assert_eq!(
        inserted,
        Hash::default(),
        "a swap of a missing key answers zero"
    );
    assert_eq!(
        base.clone()
            .load(partition, stored, KeyType::BranchId)
            .await
            .ok(),
        Some(original),
        "the base keeps its value"
    );
    assert!(
        base.clone()
            .load(partition, missing, KeyType::BranchId)
            .await
            .is_err(),
        "the base holds no value for a key only swapped in"
    );
}
