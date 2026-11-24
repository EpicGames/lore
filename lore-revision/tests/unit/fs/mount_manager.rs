// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write external `.lore` configs directly; what these test is how a store or
// instance that cannot be read affects the rest of the walk.
#![allow(clippy::disallowed_methods)]

#[cfg(target_os = "linux")]
use std::fs::Permissions;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::test_util::TempFile;
use lore_revision::fs::mount_manager::MountManager;
#[cfg(target_os = "linux")]
use lore_revision::fs::mount_manager::MountManagerError;
#[cfg(target_os = "linux")]
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::instance::InstanceId;
use lore_revision::instance::register_instance;
use lore_revision::repository::CONFIG;
use lore_revision::repository::DOT_LORE;
use lore_revision::shared_store::SHARED_STORE_DIR;
use lore_revision::shared_store::registry::SharedStoreRegistry;
use lore_revision::shared_store::registry::SharedStoreRegistryEntry;
use lore_storage::FlushOptions;

use crate::fs::filesystem_provider::setup_test_execution;

/// The minimal config of a FUSE-backed instance.
#[cfg(target_os = "linux")]
const FUSE_CONFIG: &[u8] = b"[vfs]\nvfs_type = \"Fuse\"\n";

/// Seals `path` to `0o000` until dropped, when permissions are restored to `0o755`.
#[cfg(target_os = "linux")]
struct Sealed(PathBuf);

#[cfg(target_os = "linux")]
impl Sealed {
    fn over(path: PathBuf) -> Self {
        std::fs::set_permissions(&path, Permissions::from_mode(0o000))
            .expect("directory permissions can be sealed");
        Self(path)
    }
}

#[cfg(target_os = "linux")]
impl Drop for Sealed {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, Permissions::from_mode(0o755));
    }
}

/// Registers a FUSE instance at `path` on `entry`'s store and writes its external `.lore`
/// directory, returning the new instance's id.
#[cfg(target_os = "linux")]
async fn register_fuse_instance(entry: &SharedStoreRegistryEntry, path: &str) -> InstanceId {
    let repository = entry
        .create_null_repository_context()
        .await
        .expect("store opens");
    let instance_id = InstanceId::generate();
    register_instance(&repository, instance_id, path)
        .await
        .expect("instance registers");
    repository
        .flush(FlushOptions::default())
        .await
        .expect("registration flushes to disk");
    let dot_lore = external_dir_for_instance(
        &lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root"),
        instance_id,
    )
    .expect("external dir resolves")
    .join(DOT_LORE);
    std::fs::create_dir_all(&dot_lore).expect("external dot_lore dir is created");
    std::fs::write(dot_lore.join(CONFIG), FUSE_CONFIG).expect("config is written");
    instance_id
}

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

/// Stores a registration whose blob the store of `entry` doesn't hold, flushed to disk, and checks
/// that the store, opened again, lists it as lost.
async fn store_registration_with_missing_blob(entry: &SharedStoreRegistryEntry) {
    let repository = entry
        .create_null_repository_context()
        .await
        .expect("store opens");
    let (key, key_type) =
        lore_revision::instance::instance_key(repository.salt(), InstanceId::generate());
    repository
        .try_write_mutable_store()
        .expect("the store takes writes")
        .store(
            repository.id,
            key,
            lore_base::types::Hash::from_u64(1),
            key_type,
        )
        .await
        .expect("the dangling registration is stored");
    repository
        .flush(FlushOptions::default())
        .await
        .expect("the registration flushes");
    drop(repository);
    let reopened = entry
        .create_null_repository_context()
        .await
        .expect("store reopens");
    assert_eq!(
        lore_revision::instance::registrations(&reopened)
            .await
            .expect("the store's instances list")
            .lost
            .len(),
        1,
        "the registration whose blob the store lacks is lost"
    );
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

            let (mounted, pending, registered, _) = MountManager::mounts_of_entries(
                registry.entries(),
                &mut lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .await
            .expect("mounting past the store that cannot be opened succeeds");

            assert!(mounted.is_empty() && pending.is_empty());
            assert_eq!(
                registered, None,
                "the instances of a store that cannot be opened are unknown"
            );
            assert!(
                opened(readable_store.path()),
                "the walk opens the store after the skipped one"
            );
        })
        .await;
}

/// A store holding an instance registration whose blob is missing leaves the registered instances
/// unknown, so no external directory marked as being cloned is removed. The walk goes on to open
/// the next store.
#[tokio::test]
async fn a_registration_whose_blob_is_missing_leaves_the_registered_instances_unknown() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let broken_store = TempDir::new("lore-mount-manager-missing-blob-store-");
            let readable_store = TempDir::new("lore-mount-manager-next-store-");
            let registry = registry_of(&[broken_store.path(), readable_store.path()]);
            store_registration_with_missing_blob(&registry.entries()[0]).await;
            assert!(!opened(readable_store.path()));

            let (mounted, pending, registered, _) = MountManager::mounts_of_entries(
                registry.entries(),
                &mut lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .await
            .expect("mounting past the missing blob succeeds");

            assert!(mounted.is_empty() && pending.is_empty());
            assert_eq!(
                registered, None,
                "the instances of a store holding a registration whose blob is missing are unknown"
            );
            assert!(
                opened(readable_store.path()),
                "the walk opens the next store"
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
                .flush(FlushOptions::default())
                .await
                .expect("registration flushes to disk");
            drop(repository);
            let dot_lore = external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                instance_id,
            )
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

            let (mounted, pending, _, _) = MountManager::mounts_of_entries(
                registry.entries(),
                &mut lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .await
            .expect("mounting past the unreadable instance succeeds");

            assert!(mounted.is_empty() && pending.is_empty());
            assert!(
                opened(next_store.path()),
                "the walk opens the store after the skipped instance"
            );
        })
        .await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_unlistable_store_does_not_stop_the_others() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // 0o000 does not block root, so the seal below would not simulate an unreadable
        // store.
        return;
    }
    let broken_store = TempDir::new("lore-mount-manager-broken-store-");
    let _sealed = Sealed::over(broken_store.path().to_path_buf());
    let readable_store = TempDir::new("lore-mount-manager-readable-store-");

    let mut registry = SharedStoreRegistry::default();
    registry
        .register("lore://broken".to_string(), broken_store.path())
        .expect("register accepts the broken store");
    registry
        .register("lore://readable".to_string(), readable_store.path())
        .expect("register accepts the readable store");

    let instance_id = register_fuse_instance(&registry.entries()[1], "").await;
    assert!(
        registry.entries()[0]
            .create_null_repository_context()
            .await
            .is_err(),
        "the sealed store opens"
    );

    let (mounted, pending, _, _) = MountManager::mounts_of_entries(
        registry.entries(),
        &mut lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root"),
    )
    .await
    .expect("mounting past the unlistable store succeeds");

    assert!(mounted.is_empty());
    assert_eq!(pending.len(), 1, "the readable store's instance is pending");
    assert_eq!(pending[0].instance.instance_id, instance_id);
}

/// A store holding a registration whose blob the store doesn't hold still mounts its other
/// instances, and the next store's instances are mounted too.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_store_with_a_registration_whose_blob_is_missing_mounts_its_other_instances() {
    let broken_store = TempDir::new("lore-mount-manager-missing-blob-store-");
    let readable_store = TempDir::new("lore-mount-manager-next-store-");
    let mut registry = SharedStoreRegistry::default();
    registry
        .register("lore://missing-blob".to_string(), broken_store.path())
        .expect("register accepts the store with the missing blob");
    registry
        .register("lore://next".to_string(), readable_store.path())
        .expect("register accepts the next store");

    store_registration_with_missing_blob(&registry.entries()[0]).await;
    let mut expected = vec![
        register_fuse_instance(&registry.entries()[0], "").await,
        register_fuse_instance(&registry.entries()[1], "").await,
    ];
    expected.sort();

    let (mounted, pending, _, _) = MountManager::mounts_of_entries(
        registry.entries(),
        &mut lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root"),
    )
    .await
    .expect("mounting past the missing blob succeeds");

    assert!(mounted.is_empty());
    let mut pending: Vec<_> = pending
        .iter()
        .map(|mount| mount.instance.instance_id)
        .collect();
    pending.sort();
    assert_eq!(
        pending, expected,
        "the instances of both stores are pending"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_unreadable_instance_does_not_stop_the_rest_of_its_store() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // 0o000 does not block root, so the seal below would not simulate an unreadable
        // instance.
        return;
    }
    let store = TempDir::new("lore-mount-manager-store-");

    let mut registry = SharedStoreRegistry::default();
    registry
        .register("lore://store".to_string(), store.path())
        .expect("register accepts the store");
    let entry = &registry.entries()[0];

    // A path that exists, so the instance would be mounted were its config readable rather
    // than skipped as one whose mount point was deleted.
    let unreadable_path = TempDir::new("lore-mount-manager-unreadable-instance-");
    let unreadable_id =
        register_fuse_instance(entry, &unreadable_path.path().display().to_string()).await;
    let dot_lore = external_dir_for_instance(
        &lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root"),
        unreadable_id,
    )
    .expect("external dir resolves")
    .join(DOT_LORE);
    let _sealed = Sealed::over(dot_lore.clone());
    assert!(
        lore_revision::repository::load_repository_config_from_dot_dir(&dot_lore).is_err(),
        "the sealed instance's config reads"
    );

    let readable_id = register_fuse_instance(entry, "").await;

    let (mounted, pending, registered, _) = MountManager::mounts_of_entries(
        registry.entries(),
        &mut lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root"),
    )
    .await
    .expect("mounting past the unreadable instance succeeds");

    assert!(mounted.is_empty());
    assert_eq!(pending.len(), 1, "only the readable instance is pending");
    assert_eq!(pending[0].instance.instance_id, readable_id);
    assert_eq!(
        registered,
        Some(std::collections::BTreeSet::from([
            unreadable_id,
            readable_id
        ])),
        "every instance a listed store registers is known, mountable or not"
    );
}

/// A path no manager mounts names no instance and leads to no external directory.
#[test]
fn a_path_nothing_is_mounted_at_names_no_instance() {
    let parent = TempDir::new("lore-mount-manager-test-");
    let manager = MountManager::for_test();
    let unmounted = parent.path().join("unmounted");
    assert_eq!(manager.mounted(&unmounted).expect("lookup succeeds"), None);
    assert!(!manager.is_mounted(&unmounted).expect("lookup succeeds"));
    assert_eq!(
        manager
            .check_for_external_lore_dir(&unmounted)
            .expect("lookup succeeds"),
        None,
        "an unregistered path must not resolve to any external dir"
    );
    assert!(manager.mounts().is_empty());
}

/// Checks a mount just made at `path` against the stops, as a create or a remount does.
#[cfg(target_os = "linux")]
async fn check_against_the_stops(
    manager: &MountManager,
    path: &Path,
) -> Result<(), MountManagerError> {
    let mount_path = MountPath::new(path).expect("valid mount path");
    manager.fuse().refuse_while_stopping(&mount_path).await
}

/// Asserts that `checked` is a stop's refusal of a mount.
#[cfg(target_os = "linux")]
fn assert_refused(checked: Result<(), MountManagerError>, context: &str) {
    let error = checked.expect_err(context);
    assert!(
        error.is_internal() && error.to_string().contains("is not mounted"),
        "{context}: unexpected error: {error}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn no_stop_refuses_a_new_mount() {
    let manager = MountManager::for_test();
    check_against_the_stops(&manager, Path::new("/tmp/lore-mount-manager-test-kept"))
        .await
        .expect("no stop runs");
}

/// A stop that has taken its snapshot may have missed the mount, so the mount is refused.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_mount_made_while_a_stop_runs_is_refused() {
    let manager = MountManager::for_test();
    manager.fuse().stopping.store(true, Ordering::SeqCst);
    assert_refused(
        check_against_the_stops(&manager, Path::new("/tmp/lore-mount-manager-test-stopping")).await,
        "a stop runs",
    );
}

/// A stop that went ahead keeps refusing mounts after a plain stop refused alongside it
/// clears `stopping`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_stop_that_went_ahead_refuses_mounts_after_one_alongside_it_was_refused() {
    for force in [false, true] {
        let manager = MountManager::for_test();
        manager
            .fuse()
            .unmount_all(force)
            .await
            .expect("with nothing mounted, the stop goes ahead");
        manager.fuse().stopping.store(false, Ordering::SeqCst);
        assert_refused(
            check_against_the_stops(&manager, Path::new("/tmp/lore-mount-manager-test-exiting"))
                .await,
            &format!("force {force}: the service is exiting"),
        );
    }
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
            assert!(manager.mounts().is_empty());

            manager
                .swfs()
                .close_all()
                .await
                .expect("closing a manager with no mounts succeeds");

            assert!(manager.mounts().is_empty());
        })
        .await;
}

/// A FUSE instance whose mount point is there but empty, as when the service is stopped or a move
/// is under way, is not stale: a prune keeps it. Once its mount point is gone it is, so a start
/// removes it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_unmounted_fuse_instance_is_stale_only_once_its_mount_point_is_gone() {
    use lore_revision::instance::InstanceStaleness;
    use lore_revision::instance::instance_staleness;

    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let parent = TempDir::new("lore-mount-manager-unmounted-");
            let mount_point = parent.path().join("mounted");
            std::fs::create_dir(&mount_point).expect("the mount point is made");
            let instance_id = InstanceId::generate();
            let dot_lore = external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                instance_id,
            )
            .expect("external dir resolves")
            .join(DOT_LORE);
            std::fs::create_dir_all(&dot_lore).expect("external dot_lore dir is created");
            std::fs::write(dot_lore.join(CONFIG), FUSE_CONFIG).expect("config is written");
            let path = mount_point.display().to_string();
            let state = lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root");

            assert_eq!(
                instance_staleness(&state, &path, instance_id).await,
                InstanceStaleness::Active,
                "an empty mount point is an unmounted instance"
            );
            std::fs::remove_dir(&mount_point).expect("the mount point is removed");
            assert_eq!(
                instance_staleness(&state, &path, instance_id).await,
                InstanceStaleness::PathMissing,
                "a missing mount point is a deleted instance"
            );
        })
        .await;
}

/// A start skips a virtual instance whose external directory is unknown, here while a move of the
/// external root is recorded, and removes nothing of it, though its mount point is gone.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_start_skips_an_unknown_virtual_instance_and_removes_nothing() {
    use lore_revision::global::external_dir::record::MoveKind;
    use lore_revision::global::external_dir::record::MovePhase;
    use lore_revision::global::external_dir::record::MoveRecord;

    let global = TempDir::new("lore-mount-manager-unknown-global-");
    LORE_CONTEXT
        .scope(crate::helper::execution_in(global.path()), async {
            let store = TempDir::new("lore-mount-manager-unknown-store-");
            let parent = TempDir::new("lore-mount-manager-unknown-path-");
            let registry = registry_of(&[store.path()]);
            let gone = parent.path().join("gone");
            let instance_id =
                register_fuse_instance(&registry.entries()[0], &gone.display().to_string()).await;
            let external = external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                instance_id,
            )
            .expect("external dir resolves");
            let data =
                lore_revision::global::get_global_data_dir().expect("the global data directory");
            MoveRecord {
                from: String::new(),
                to: parent.path().join("root").display().to_string(),
                kind: MoveKind::Rename,
                phase: MovePhase::Place,
            }
            .save(&data)
            .await
            .expect("save the record");

            let (mounted, pending, _, _) = MountManager::mounts_of_entries(
                registry.entries(),
                &mut lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .await
            .expect("the start goes on");

            assert!(mounted.is_empty() && pending.is_empty());
            assert!(external.join(DOT_LORE).is_dir(), "nothing is removed");
        })
        .await;
}

/// A start resolves the external root once for every registration of every store, and once more
/// under shared access at its first virtual instance, and each FUSE instance it finds carries the
/// canonical path of the global data directory it was resolved for.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_start_resolves_the_external_root_once_and_once_under_shared_access() {
    use lore_revision::global::external_dir::resolve::test_util::reads_in;

    let global = TempDir::new("lore-mount-manager-once-global-");
    LORE_CONTEXT
        .scope(crate::helper::execution_in(global.path()), async {
            let stores = [
                TempDir::new("lore-mount-manager-once-store-"),
                TempDir::new("lore-mount-manager-once-store-"),
            ];
            let parent = TempDir::new("lore-mount-manager-once-path-");
            let registry = registry_of(&[stores[0].path(), stores[1].path()]);
            for index in 0..4 {
                let path = parent.path().join(format!("instance-{index}"));
                std::fs::create_dir(&path).expect("make the mount point");
                register_fuse_instance(&registry.entries()[index % 2], &path.display().to_string())
                    .await;
            }
            let data =
                lore_revision::global::get_global_data_dir().expect("the global data directory");
            let before = reads_in(&data);

            let (_, pending, _, _) = MountManager::mounts_of_entries(
                registry.entries(),
                &mut lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .await
            .expect("the start finds the instances");

            let after = reads_in(&data);
            assert_eq!(pending.len(), 4);
            assert_eq!(
                (after.setting, after.record),
                (before.setting + 2, before.record + 2),
                "one resolution, and one under shared access"
            );
            let canonical = std::fs::canonicalize(&data).expect("canonicalize");
            assert!(
                pending
                    .iter()
                    .all(|fuse| fuse.global.as_ref() == canonical.as_path())
            );
        })
        .await;
}

/// A start with no virtual instance registered takes no access to the external root, so a move of
/// it can take exclusive access.
#[tokio::test]
async fn a_start_with_no_virtual_instance_takes_no_access_to_the_external_root() {
    use lore_revision::util::config::SaveableConfig;

    let global = TempDir::new("lore-mount-manager-ordinary-global-");
    LORE_CONTEXT
        .scope(crate::helper::execution_in(global.path()), async {
            let store = TempDir::new("lore-mount-manager-ordinary-store-");
            let parent = TempDir::new("lore-mount-manager-ordinary-path-");
            let registry = registry_of(&[store.path()]);
            let (_, lock) = SharedStoreRegistry::load_locked()
                .await
                .expect("lock the registry");
            registry.save(lock).await.expect("save the registry");
            let repository = registry.entries()[0]
                .create_null_repository_context()
                .await
                .expect("store opens");
            register_instance(
                &repository,
                InstanceId::generate(),
                &parent.path().display().to_string(),
            )
            .await
            .expect("an ordinary instance registers");
            repository
                .flush(FlushOptions::default())
                .await
                .expect("registration flushes to disk");
            drop(repository);

            let manager = MountManager::initialize()
                .await
                .expect("the start succeeds");

            assert!(manager.mounts().is_empty());
            let data =
                lore_revision::global::get_global_data_dir().expect("the global data directory");
            assert!(
                lore_base::fs::lock::FSLock::try_acquire_file_lock(data.join("external"))
                    .expect("attempt")
                    .is_some(),
                "a move can take exclusive access"
            );
        })
        .await;
}

/// While a move of the external root holds exclusive access, a start mounts no virtual instance
/// of that global data directory and goes on: a FUSE instance whose mount point is gone keeps its
/// external directory, which a start holding shared access removes.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_start_refused_access_to_the_external_root_skips_every_virtual_instance() {
    use lore_revision::global::external_dir::access::release_exclusive;
    use lore_revision::global::external_dir::access::try_exclusive;
    use lore_revision::util::config::SaveableConfig;

    let global = TempDir::new("lore-mount-manager-refused-global-");
    LORE_CONTEXT
        .scope(crate::helper::execution_in(global.path()), async {
            let store = TempDir::new("lore-mount-manager-refused-store-");
            let parent = TempDir::new("lore-mount-manager-refused-path-");
            let registry = registry_of(&[store.path()]);
            let (_, lock) = SharedStoreRegistry::load_locked()
                .await
                .expect("lock the registry");
            registry.save(lock).await.expect("save the registry");
            let gone = parent.path().join("gone").display().to_string();
            let instance_id = register_fuse_instance(&registry.entries()[0], &gone).await;
            let external = external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                instance_id,
            )
            .expect("external dir resolves");
            let data =
                lore_revision::global::get_global_data_dir().expect("the global data directory");
            assert!(try_exclusive(&data).expect("attempt"));

            let manager = MountManager::initialize().await.expect("the start goes on");
            assert!(manager.mounts().is_empty());
            assert!(
                external.join(DOT_LORE).is_dir(),
                "the instance whose mount point is gone is kept"
            );

            release_exclusive(&data).expect("shared access is taken");
            MountManager::initialize().await.expect("the start goes on");
            assert!(
                !external.exists(),
                "a start holding shared access removes it"
            );
        })
        .await;
}

/// An instance whose path the skip answers for is neither mounted nor left to mount, as the
/// mount of a recovered root leaves one the service mounts already.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_instance_the_skip_answers_for_is_left_as_it_is() {
    let store = TempDir::new("lore-mount-manager-skipped-store-");
    let mut registry = SharedStoreRegistry::default();
    registry
        .register("lore://skipped".to_string(), store.path())
        .expect("register the store");
    let mount_point = TempDir::new("lore-mount-manager-skipped-mount-");
    let mounted_path = mount_point.path().to_str().expect("UTF-8");
    register_fuse_instance(&registry.entries()[0], mounted_path).await;
    let mut state =
        lore_revision::global::external_dir::resolve::resolve().expect("resolve the external root");

    let (mounted, pending, _, _) =
        MountManager::mounts_of_entries_skipping(registry.entries(), &mut state, |path| {
            path == Path::new(mounted_path)
        })
        .await
        .expect("the instances");
    let (_, kept, _, _) =
        MountManager::mounts_of_entries_skipping(registry.entries(), &mut state, |_| false)
            .await
            .expect("the instances");

    assert!(mounted.is_empty());
    assert!(pending.is_empty());
    assert_eq!(kept.len(), 1, "unskipped, the instance is left to mount");
}

/// The mounts of the registered instances, as a start and recovery in the background find them,
/// come with what interrupted clones left removed: here a lock file no clone holds.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_registered_mounts_come_with_what_interrupted_clones_left_removed() {
    let store = TempDir::new("lore-mount-manager-swept-store-");
    let mut registry = SharedStoreRegistry::default();
    registry
        .register("lore://swept".to_string(), store.path())
        .expect("register the store");
    let mount_point = TempDir::new("lore-mount-manager-swept-instance-");
    let instance_id = register_fuse_instance(
        &registry.entries()[0],
        mount_point.path().to_str().expect("UTF-8"),
    )
    .await;
    let mut state =
        lore_revision::global::external_dir::resolve::resolve().expect("resolve the external root");
    let dot_lore = external_dir_for_instance(&state, instance_id)
        .expect("external dir resolves")
        .join(DOT_LORE);
    std::fs::write(dot_lore.join(CONFIG), b"").expect("a config naming no VFS");
    let stray_lock = lore_revision::global::external_dir::external_dirs_root(&state)
        .expect("the external root")
        .join(format!(
            "{}{}",
            InstanceId::generate().text_encoding(),
            lore_base::fs::lock::FSLock::FILE_LOCK_SUFFIX
        ));
    std::fs::write(&stray_lock, b"").expect("a lock file no clone holds");

    let (mounted, pending) =
        MountManager::registered_mounts(registry.entries(), &mut state, |_| false)
            .await
            .expect("the mounts");

    assert!(mounted.is_empty() && pending.is_empty());
    assert!(
        !stray_lock.exists(),
        "the lock file no clone holds is removed"
    );
}

/// While the external root waits for a move, a start's mounting mounts no virtual instance, and
/// answers at once, without opening a store another holder keeps: a start then goes on, accepts
/// calls, and leaves the move to recovery in the background.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mounting_of_a_root_waiting_for_a_move_opens_no_store() {
    let store = TempDir::new("lore-mount-manager-waiting-store-");
    let mut registry = SharedStoreRegistry::default();
    registry
        .register("lore://waiting".to_string(), store.path())
        .expect("register the store");
    let mount_point = TempDir::new("lore-mount-manager-waiting-instance-");
    register_fuse_instance(
        &registry.entries()[0],
        mount_point.path().to_str().expect("UTF-8"),
    )
    .await;
    let global = TempDir::new("lore-mount-manager-waiting-global-");
    lore_revision::global::external_dir::record::MoveRecord {
        from: String::new(),
        to: global.path().join("moved").display().to_string(),
        kind: lore_revision::global::external_dir::record::MoveKind::Rename,
        phase: lore_revision::global::external_dir::record::MovePhase::Switch,
    }
    .save(global.path())
    .await
    .expect("the record of a waiting move");
    let mut state =
        lore_revision::global::external_dir::resolve::resolve_in(global.path().to_path_buf());
    let _other = lore_base::fs::lock::FSLock::try_acquire_exact_path(
        &Path::new(registry.entries()[0].path())
            .join(SHARED_STORE_DIR)
            .join("immutable")
            .join("lock"),
    )
    .await
    .expect("look at the flock")
    .expect("hold the store");

    let found = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        MountManager::mounts_of_entries_skipping(registry.entries(), &mut state, |_| false),
    )
    .await
    .expect("the mounting answers without waiting for the store");

    let (mounted, pending, _, _) = found.expect("the mounting succeeds");
    assert!(mounted.is_empty() && pending.is_empty());
}
