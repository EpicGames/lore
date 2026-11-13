// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::ImmutableStoreSettings;
use lore_storage::MutableStore;
use lore_storage::MutableStoreSettings;
use lore_storage::StoreError;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_storage::test_util::fail_packfile_syncs;
use lore_storage::test_util::packfile_sync_failures_left;

use super::test_helpers::store_fragment;
use crate::helper::default_repository_creation_args;
use crate::helper::setup_test_execution;

/// The mutable store's key the tests write.
fn key() -> Hash {
    Hash::from_u64(0x6E)
}

/// The stores on disk under `root`.
async fn disk_stores(root: &Path) -> (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>) {
    let immutable = lore_storage::local::immutable_store::create(
        Some(root),
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("the immutable store opens");
    let mutable: Arc<dyn MutableStore> = lore_storage::local::mutable_store::create(
        Some(root),
        MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .expect("the mutable store opens");
    (immutable, mutable)
}

/// A context holding a write token over stores on disk under `root`, with a fragment written to
/// its immutable store and not yet synced, and [`key`] written to its mutable store and not yet
/// flushed.
async fn context_with_unflushed_writes(root: &Path) -> Arc<RepositoryContext> {
    let (immutable, mutable) = disk_stores(root).await;
    let repository = Arc::new(
        RepositoryContext::new(default_repository_creation_args(immutable, mutable))
            .with_write_token(RepositoryWriteToken::acquire(root).await),
    );
    let payload = b"a payload whose sync fails";
    store_fragment(
        &repository,
        payload,
        FragmentFlags::empty(),
        payload.len() as u64,
    )
    .await;
    repository
        .try_write_mutable_store()
        .expect("a write token")
        .store(
            repository.id,
            key(),
            Hash::from_u64(0xE6),
            KeyType::BranchMetadata,
        )
        .await
        .expect("stores");
    repository
}

/// What a mutable store opened afresh on the directory under `root` holds for [`key`] in
/// `partition`.
async fn load_from_disk(root: &Path, partition: Partition) -> Result<Hash, StoreError> {
    let (_immutable, mutable) = disk_stores(root).await;
    mutable
        .load(partition, key(), KeyType::BranchMetadata)
        .await
}

/// A flush whose immutable store's flush fails returns the error and does not flush the mutable
/// store.
#[tokio::test]
async fn a_flush_skips_the_mutable_store_after_a_failed_immutable_flush() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let root = TempDir::new("repository-flush-immutable-fails-");
            let repository = context_with_unflushed_writes(root.path()).await;
            let partition = repository.id;

            fail_packfile_syncs(1);
            assert!(
                repository.flush(FlushOptions::SYNC_DATA).await.is_err(),
                "the flush returns the immutable store's error"
            );
            drop(repository);

            assert!(
                load_from_disk(root.path(), partition)
                    .await
                    .is_err_and(|err| err.is_address_not_found()),
                "the mutable store is not flushed once the immutable store's flush failed"
            );
        })
        .await;
}

/// The flush a command with a write token ends with does not flush the mutable store once the
/// immutable store's flush has failed.
#[tokio::test]
async fn a_post_command_flush_skips_the_mutable_store_after_a_failed_immutable_flush() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let root = TempDir::new("post-command-flush-immutable-fails-");
            let repository = context_with_unflushed_writes(root.path()).await;
            let partition = repository.id;

            let mutable =
                Arc::downgrade(&repository.try_mutable_store_arc().expect("a write token"));
            fail_packfile_syncs(1);
            repository.try_spawn_post_command_flush(FlushOptions::SYNC_DATA);
            drop(repository);
            // The flush's task holds the mutable store until its flush has ended.
            tokio::time::timeout(Duration::from_secs(10), async {
                while mutable.strong_count() > 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the flush ends within 10 seconds");
            assert_eq!(
                packfile_sync_failures_left(),
                0,
                "the flush ran, and its immutable store's flush failed"
            );

            assert!(
                load_from_disk(root.path(), partition)
                    .await
                    .is_err_and(|err| err.is_address_not_found()),
                "the mutable store is not flushed once the immutable store's flush failed"
            );
        })
        .await;
}
