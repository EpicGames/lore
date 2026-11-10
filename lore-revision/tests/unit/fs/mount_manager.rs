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
#[cfg(target_os = "linux")]
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
    let dot_lore = external_dir_for_instance(instance_id)
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

            let (mounted, pending) = MountManager::mounts_of_entries(registry.entries())
                .await
                .expect("mounting past the store that cannot be opened succeeds");

            assert!(mounted.is_empty() && pending.is_empty());
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
            broken
                .flush(FlushOptions::default())
                .await
                .expect("the registration flushes");
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

            let (mounted, pending) = MountManager::mounts_of_entries(registry.entries())
                .await
                .expect("mounting past the unlisted store succeeds");

            assert!(mounted.is_empty() && pending.is_empty());
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
                .flush(FlushOptions::default())
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

            let (mounted, pending) = MountManager::mounts_of_entries(registry.entries())
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

    let (mounted, pending) = MountManager::mounts_of_entries(registry.entries())
        .await
        .expect("mounting past the unlistable store succeeds");

    assert!(mounted.is_empty());
    assert_eq!(pending.len(), 1, "the readable store's instance is pending");
    assert_eq!(pending[0].instance.instance_id, instance_id);
}

/// A store that opens but whose instances cannot be listed, as a registration naming
/// metadata the store does not hold leaves it, is skipped, and the next store's instances
/// are still mounted.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_store_whose_instances_cannot_be_listed_does_not_stop_the_others() {
    let broken_store = TempDir::new("lore-mount-manager-unlisted-store-");
    let readable_store = TempDir::new("lore-mount-manager-listed-store-");
    let mut registry = SharedStoreRegistry::default();
    registry
        .register("lore://unlisted".to_string(), broken_store.path())
        .expect("register accepts the unlisted store");
    registry
        .register("lore://listed".to_string(), readable_store.path())
        .expect("register accepts the listed store");

    let broken = registry.entries()[0]
        .create_null_repository_context()
        .await
        .expect("the unlisted store opens");
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
    broken
        .flush(FlushOptions::default())
        .await
        .expect("the registration flushes");
    let reopened = registry.entries()[0]
        .create_null_repository_context()
        .await
        .expect("the unlisted store opens");
    assert!(
        lore_revision::instance::list_instances(&reopened)
            .await
            .is_err(),
        "the unlisted store's instances list"
    );
    let instance_id = register_fuse_instance(&registry.entries()[1], "").await;

    let (mounted, pending) = MountManager::mounts_of_entries(registry.entries())
        .await
        .expect("mounting past the unlisted store succeeds");

    assert!(mounted.is_empty());
    assert_eq!(pending.len(), 1, "the listed store's instance is pending");
    assert_eq!(pending[0].instance.instance_id, instance_id);
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
    let dot_lore = external_dir_for_instance(unreadable_id)
        .expect("external dir resolves")
        .join(DOT_LORE);
    let _sealed = Sealed::over(dot_lore.clone());
    assert!(
        lore_revision::repository::load_repository_config_from_dot_dir(&dot_lore).is_err(),
        "the sealed instance's config reads"
    );

    let readable_id = register_fuse_instance(entry, "").await;

    let (mounted, pending) = MountManager::mounts_of_entries(registry.entries())
        .await
        .expect("mounting past the unreadable instance succeeds");

    assert!(mounted.is_empty());
    assert_eq!(pending.len(), 1, "only the readable instance is pending");
    assert_eq!(pending[0].instance.instance_id, readable_id);
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
            let dot_lore = external_dir_for_instance(instance_id)
                .expect("external dir resolves")
                .join(DOT_LORE);
            std::fs::create_dir_all(&dot_lore).expect("external dot_lore dir is created");
            std::fs::write(dot_lore.join(CONFIG), FUSE_CONFIG).expect("config is written");
            let path = mount_point.display().to_string();

            assert_eq!(
                instance_staleness(&path, instance_id).await,
                InstanceStaleness::Active,
                "an empty mount point is an unmounted instance"
            );
            std::fs::remove_dir(&mount_point).expect("the mount point is removed");
            assert_eq!(
                instance_staleness(&path, instance_id).await,
                InstanceStaleness::PathMissing,
                "a missing mount point is a deleted instance"
            );
        })
        .await;
}
