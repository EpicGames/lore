// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Recovery of a move of the external root in the background mounts the moved root's SWFS
//! instances into the running mount manager.
//!
//! Mounting needs the `EpicVFS` driver running, and hangs without it, so a machine without the
//! driver sets `LORE_TEST_NO_SWFS`, which skips the test. The test runs on Windows x86-64 only.
#![cfg(all(windows, target_arch = "x86_64"))]
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;

use lore_base::fs::lock::FSLock;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::fs::mount_manager::MountManager;
use lore_revision::global::external_dir::marker::Marker;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::recovery::AtStart;
use lore_revision::global::external_dir::recovery::recover_at_start;
use lore_revision::global::external_dir::recovery::recover_in_background;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::instance::InstanceId;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::SharedStoreToUseConfig;
use lore_revision::repository::VfsConfig;
use lore_revision::repository::VfsType;
use lore_revision::shared_store::registry::SharedStoreRegistry;
use lore_revision::util::config::SaveableConfig;
use lore_storage::FlushOptions;

#[allow(dead_code)]
mod helper;

/// The remote the instance's config names.
const REMOTE: &str = "lore://swfs-external-root.invalid/repository";

/// A first move interrupted at its switch, after the root was renamed, with the store of its SWFS
/// instance held at start: the start mounts nothing of the root, and once the holder lets go, the
/// background recovery finishes the move and mounts the instance into the running manager.
#[tokio::test]
async fn a_recovered_root_mounts_its_swfs_instances_into_the_running_manager() {
    if std::env::var_os("LORE_TEST_NO_SWFS").is_some() {
        eprintln!("skipped: LORE_TEST_NO_SWFS is set");
        return;
    }
    let base = TempDir::new("lore-swfs-external-root-");
    let stores = TempDir::new("lore-swfs-external-root-stores-");
    let mounts = TempDir::new("lore-swfs-external-root-mounts-");
    let volume = TempDir::new("lore-swfs-external-root-volume-");
    LORE_CONTEXT
        .scope(helper::execution_in(base.path()), async {
            let global =
                lore_revision::global::get_global_data_dir().expect("the global data directory");
            let config = RepositoryConfig {
                remote_url: Some(REMOTE.to_string()),
                shared_store_to_use: Some(SharedStoreToUseConfig {
                    use_shared_store: Some(true),
                    shared_store_path: Some(stores.path().display().to_string()),
                }),
                vfs: Some(VfsConfig {
                    vfs_type: VfsType::Swfs,
                    base_time: None,
                }),
                ..Default::default()
            };
            let shared = stores
                .path()
                .join(lore_revision::shared_store::shared_store_subdir_for_remote(
                    REMOTE,
                ))
                .join(lore_revision::shared_store::SHARED_STORE_DIR);
            let mut registry = SharedStoreRegistry::default();
            registry
                .register(REMOTE.to_string(), shared.parent().expect("its directory"))
                .expect("register the store");
            let (_, lock) = SharedStoreRegistry::load_locked()
                .await
                .expect("lock the registry");
            registry.save(lock).await.expect("save the registry");

            let instance_id = InstanceId::generate();
            let mount_path = mounts.path().join("mount");
            let repository = registry.entries()[0]
                .create_null_repository_context()
                .await
                .expect("open the store");
            lore_revision::instance::store_instance_registration(
                &repository,
                instance_id,
                mount_path.to_str().expect("UTF-8"),
            )
            .await
            .expect("register the instance");
            repository
                .flush(FlushOptions::default())
                .await
                .expect("flush the registration");
            drop(repository);

            let default = global.join("external");
            let dot_lore = default.join(instance_id.text_encoding()).join(".lore");
            std::fs::create_dir_all(&dot_lore).expect("the external directory");
            lore_revision::util::config::save(
                &config,
                dot_lore.join(lore_revision::repository::CONFIG),
            )
            .await
            .expect("the config");
            let id = RootId::generate();
            Marker {
                id,
                generation: NonZeroU64::MIN,
            }
            .save(&default)
            .await
            .expect("the marker");
            Setting {
                path: String::new(),
                id,
                generation: NonZeroU64::MIN,
            }
            .save(&global)
            .await
            .expect("the setting");
            let target = volume.path().join("root");
            std::fs::rename(&default, &target).expect("the rename");
            MoveRecord {
                from: String::new(),
                to: target.display().to_string(),
                kind: MoveKind::Rename,
                phase: MovePhase::Switch,
            }
            .save(&global)
            .await
            .expect("the record");
            let other = FSLock::try_acquire_exact_path(&shared.join("immutable").join("lock"))
                .await
                .expect("look at the flock")
                .expect("hold the store");

            assert_eq!(
                recover_at_start(&global, std::time::Duration::ZERO).await,
                AtStart::Retry
            );
            let manager = Box::pin(MountManager::initialize())
                .await
                .expect("the manager starts");
            assert!(
                manager.swfs().mounts().is_empty(),
                "nothing mounts while the move waits"
            );

            drop(other);
            recover_in_background(global.clone(), async {
                Box::pin(manager.mount_registered_instances())
                    .await
                    .expect("the recovered root's instances mount");
            })
            .await;

            assert_eq!(manager.swfs().mounts(), [(mount_path, instance_id)]);
            manager.swfs().close_all().await.expect("close the mounts");
        })
        .await;
}
