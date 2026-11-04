// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write external `.lore` configs directly; what these test is how a store or
// instance that cannot be read affects the rest of the walk.
#![allow(clippy::disallowed_methods)]

use std::path::Path;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::test_util::TempFile;
use lore_revision::fs::mount_manager::MountManager;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::instance::InstanceId;
use lore_revision::instance::register_instance;
use lore_revision::repository::CONFIG;
use lore_revision::repository::DOT_LORE;
use lore_revision::shared_store::SHARED_STORE_DIR;
use lore_revision::shared_store::registry::SharedStoreRegistry;

use crate::fs::filesystem_provider::setup_test_execution;

/// A registry of the stores at `paths`, in order.
fn registry_of(paths: &[&Path]) -> SharedStoreRegistry {
    let mut registry = SharedStoreRegistry::default();
    for (index, path) in paths.iter().enumerate() {
        registry
            .register(format!("lore://store-{index}"), path)
            .expect("register accepts the store");
    }
    registry
}

/// Whether the store at `path` has been opened: opening it creates its store directory.
fn opened(path: &Path) -> bool {
    path.join(SHARED_STORE_DIR).exists()
}

/// A store that cannot be opened is skipped, and the walk goes on to open the next store.
#[tokio::test]
async fn a_store_that_cannot_be_opened_is_skipped() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let broken_store = TempFile::with_contents("lore-mount-manager-broken-store-", b"");
            let readable_store = TempDir::new("lore-mount-manager-readable-store-");
            let registry = registry_of(&[broken_store.path(), readable_store.path()]);
            assert!(
                registry.entries()[0]
                    .create_null_repository_context()
                    .await
                    .is_err(),
                "a file in place of the store fails to open"
            );
            assert!(!opened(readable_store.path()));

            let mounted = MountManager::mounts_of_entries(registry.entries())
                .await
                .expect("mounting past the store that cannot be opened succeeds");

            assert!(mounted.is_empty());
            assert!(
                opened(readable_store.path()),
                "the walk opens the store after the skipped one"
            );
        })
        .await;
}

/// A store holding an instance registration whose metadata is missing cannot list its
/// instances. It is skipped, and the walk goes on to open the next store.
#[tokio::test]
async fn a_store_whose_instances_cannot_be_listed_is_skipped() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let broken_store = TempDir::new("lore-mount-manager-unlisted-store-");
            let readable_store = TempDir::new("lore-mount-manager-listed-store-");
            let registry = registry_of(&[broken_store.path(), readable_store.path()]);
            let broken = registry.entries()[0]
                .create_null_repository_context()
                .await
                .expect("store opens");
            let (key, key_type) =
                lore_revision::instance::instance_key(broken.salt(), InstanceId::generate());
            broken
                .try_write_mutable_store()
                .expect("the store takes writes")
                .store(
                    broken.id,
                    key,
                    lore_base::types::Hash::from_u64(1),
                    key_type,
                )
                .await
                .expect("the dangling registration is stored");
            broken.flush(false).await.expect("the registration flushes");
            drop(broken);
            let reopened = registry.entries()[0]
                .create_null_repository_context()
                .await
                .expect("store reopens");
            assert!(
                lore_revision::instance::list_instances(&reopened)
                    .await
                    .is_err(),
                "listing the store's instances fails"
            );
            drop(reopened);
            assert!(!opened(readable_store.path()));

            let mounted = MountManager::mounts_of_entries(registry.entries())
                .await
                .expect("mounting past the unlisted store succeeds");

            assert!(mounted.is_empty());
            assert!(
                opened(readable_store.path()),
                "the walk opens the store after the skipped one"
            );
        })
        .await;
}

/// An instance whose config cannot be read is skipped, and the walk goes on to open the next
/// store.
#[tokio::test]
async fn an_instance_whose_config_cannot_be_read_is_skipped() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let store = TempDir::new("lore-mount-manager-store-");
            let next_store = TempDir::new("lore-mount-manager-next-store-");
            let registry = registry_of(&[store.path(), next_store.path()]);
            let repository = registry.entries()[0]
                .create_null_repository_context()
                .await
                .expect("store opens");
            let instance_id = InstanceId::generate();
            register_instance(&repository, instance_id, "")
                .await
                .expect("instance registers");
            repository
                .flush(false)
                .await
                .expect("registration flushes to disk");
            drop(repository);
            let dot_lore = external_dir_for_instance(instance_id)
                .expect("external dir resolves")
                .join(DOT_LORE);
            std::fs::create_dir_all(&dot_lore).expect("external dot_lore dir is created");
            std::fs::write(dot_lore.join(CONFIG), b"this is not toml = = =")
                .expect("config is written");
            assert!(
                lore_revision::repository::load_repository_config_from_dot_dir(&dot_lore).is_err(),
                "the instance's config fails to load"
            );
            assert!(!opened(next_store.path()));

            let mounted = MountManager::mounts_of_entries(registry.entries())
                .await
                .expect("mounting past the unreadable instance succeeds");

            assert!(mounted.is_empty());
            assert!(
                opened(next_store.path()),
                "the walk opens the store after the skipped instance"
            );
        })
        .await;
}

/// The service closes its mounts as it exits whether or not it made any, so closing a manager with
/// none succeeds and leaves it with none.
#[tokio::test]
async fn closing_a_manager_with_no_mounts_succeeds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let manager = MountManager::initialize()
                .await
                .expect("a manager with no stores registered initializes");
            assert_eq!(manager.mount_count(), 0);

            manager
                .close_all()
                .await
                .expect("closing a manager with no mounts succeeds");

            assert_eq!(manager.mount_count(), 0);
        })
        .await;
}
