// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use lore_base::test_util::TempDir;
use lore_storage::Context;
use lore_storage::FlushOptions;
use lore_storage::Hash;
use lore_storage::ImmutableStoreSettings;
use lore_storage::LocalImmutableStore;
use lore_storage::LocalMutableStore;
use lore_storage::MutableStore;
use lore_storage::MutableStoreSettings;
use lore_storage::Partition;
use lore_storage::flush_stores;
use lore_storage::store_types::KeyType;
use lore_storage::test_util::fail_packfile_syncs;

use crate::store_fixture::put_payload;

/// The immutable store on disk under `root`, and the mutable store opened beside it.
async fn disk_stores(root: &Path) -> (Arc<LocalImmutableStore>, Arc<dyn MutableStore>) {
    let immutable =
        LocalImmutableStore::new(Some(root.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .expect("the immutable store opens");
    let mutable = LocalMutableStore::new(
        Some(root),
        MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .expect("the mutable store opens");
    (immutable, Arc::new(mutable))
}

/// A flush of the stores whose immutable store's flush fails returns its error and does not flush
/// the mutable store: a mutable store opened on the directory afterwards finds none of its values.
#[tokio::test]
async fn a_failed_immutable_flush_leaves_the_mutable_store_unflushed() {
    let dir = TempDir::new("flush-stores-immutable-fails-");
    let partition = Partition::from([0x3Cu8; 16]);
    let key = Hash::from([0x5Au8; 32]);
    {
        let (immutable, mutable) = disk_stores(dir.path()).await;
        put_payload(
            &immutable,
            partition,
            Context::default(),
            b"a payload whose sync fails",
            false,
        )
        .await;
        mutable
            .clone()
            .store(
                partition,
                key,
                Hash::from([0xA5u8; 32]),
                KeyType::BranchMetadata,
            )
            .await
            .expect("stores");

        fail_packfile_syncs(1);
        let flushed = flush_stores(immutable, Some(mutable), FlushOptions::SYNC_DATA).await;
        assert!(flushed.is_err(), "the immutable store's flush fails");
    }

    let (_immutable, reopened) = disk_stores(dir.path()).await;
    let loaded = reopened.load(partition, key, KeyType::BranchMetadata).await;
    assert!(
        loaded.is_err_and(|err| err.is_address_not_found()),
        "the mutable store is not flushed once the immutable store's flush failed"
    );
}
