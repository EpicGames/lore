// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What `instance delete` does with a regular instance's directory: it renames it aside, so the
//! path is free, then deletes it. A taken aside name, a failed rename and a failed deregistration
//! each leave the instance as it was.
//!
//! The step that deregisters the instance is injected, so a failed deregistration is forced
//! rather than set up. The removal itself, and a mount point removed in place, are tested with
//! `lore-io`'s removal job.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::env::CallEnvironment;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::instance::InstanceError;
use lore_revision::instance::InstanceId;
use lore_revision::instance::deleting_aside_path;
use lore_revision::instance::instance_delete;
use lore_revision::instance::instance_delete_with;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryPaths;
use lore_revision::repository::SharedStoreToUseConfig;
use lore_revision::shared_store::SHARED_STORE_CONFIG;
use lore_revision::shared_store::SHARED_STORE_DIR;
use lore_revision::shared_store::SharedStoreConfig;
use lore_revision::shared_store::shared_store_subdir_for_remote;

use crate::helper::default_repository_creation_args;

/// A call whose global data directory is `global`, so no test touches the real one.
fn execution_in(global: &Path) -> Arc<ExecutionContext> {
    execution_with(global, LoreGlobalArgs::default())
}

/// A call as [`execution_in`] makes it, with `globals`.
fn execution_with(global: &Path, globals: LoreGlobalArgs) -> Arc<ExecutionContext> {
    let environment = CallEnvironment {
        values: [Some(global.display().to_string()), None, None],
    };
    Arc::new(
        ExecutionContext::new_client_with_user_id(
            globals,
            EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        )
        .with_environment(environment),
    )
}

/// A regular instance at `path`, with a file in a nested directory of its working tree.
struct Instance {
    _global: TempDir,
    parent: TempDir,
    path: PathBuf,
    instance_id: InstanceId,
    config: RepositoryConfig,
}

impl Instance {
    fn new() -> Self {
        let global = TempDir::new("lore-instance-delete-global-");
        let parent = TempDir::new("lore-instance-delete-parent-");
        let path = parent.path().join("instance");
        std::fs::create_dir_all(path.join(DOT_LORE)).expect("create the instance's dot directory");
        std::fs::create_dir_all(path.join("dir/sub")).expect("create a working tree directory");
        std::fs::write(path.join("dir/sub/file.txt"), b"content").expect("write a working file");
        let instance = Self {
            _global: global,
            parent,
            path,
            instance_id: InstanceId::generate(),
            config: RepositoryConfig::default(),
        };
        instance.write_config();
        instance
    }

    /// The instance, with its config naming a shared store that exists, so a delete deregisters
    /// it.
    fn with_shared_store() -> Self {
        let mut instance = Self::new();
        let remote_url = "lore://localhost/instance-delete".to_string();
        let stores = instance.parent.path().join("stores");
        let store = stores
            .join(shared_store_subdir_for_remote(&remote_url))
            .join(SHARED_STORE_DIR);
        std::fs::create_dir_all(&store).expect("create the shared store");
        std::fs::write(
            store.join(SHARED_STORE_CONFIG),
            toml::to_string(&SharedStoreConfig {
                remote_url: Some(remote_url.clone()),
                store_config: None,
            })
            .expect("serialize the shared store config"),
        )
        .expect("write the shared store config");
        instance.config.remote_url = Some(remote_url);
        instance.config.shared_store_to_use = Some(SharedStoreToUseConfig {
            use_shared_store: Some(true),
            shared_store_path: Some(stores.display().to_string()),
        });
        instance.write_config();
        instance
    }

    fn write_config(&self) {
        std::fs::write(
            self.path.join(DOT_LORE).join("config.toml"),
            toml::to_string(&self.config).expect("serialize the config"),
        )
        .expect("write the config");
    }

    fn aside(&self) -> PathBuf {
        deleting_aside_path(&self.path, self.instance_id)
    }

    /// Puts a directory with a file of its own at the aside name.
    fn take_the_aside_name(&self) {
        std::fs::create_dir(self.aside()).expect("take the aside name");
        std::fs::write(self.aside().join("other.txt"), b"other").expect("write another file");
    }

    /// Asserts that the instance's files and the directory at the aside name are as they were.
    fn assert_unchanged(&self) {
        assert!(self.path.join("dir/sub/file.txt").exists());
        assert!(self.aside().join("other.txt").exists());
    }

    async fn repository(&self) -> Arc<RepositoryContext> {
        let (immutable_store, mutable_store) =
            lore_revision::repository::create_client_memory_stores()
                .await
                .expect("in-memory stores");
        let mut args = default_repository_creation_args(immutable_store, mutable_store);
        args.paths = Some(RepositoryPaths::new(
            self.path.clone(),
            self.path.join(DOT_LORE),
        ));
        args.instance_id = self.instance_id;
        Arc::new(RepositoryContext::new(args))
    }

    /// Runs the delete with `deregister` in place of the real step.
    async fn delete_with(
        &self,
        deregister: impl AsyncFn(&Path, InstanceId) -> Result<(), InstanceError>,
    ) -> Result<(), InstanceError> {
        self.delete_in(execution_in(self._global.path()), deregister)
            .await
    }

    /// Runs the delete as a dry run.
    async fn dry_run_delete(&self) -> Result<(), InstanceError> {
        let globals = LoreGlobalArgs {
            dry_run: 1,
            ..Default::default()
        };
        self.delete_in(execution_with(self._global.path(), globals), deregistered)
            .await
    }

    async fn delete_in(
        &self,
        execution: Arc<ExecutionContext>,
        deregister: impl AsyncFn(&Path, InstanceId) -> Result<(), InstanceError>,
    ) -> Result<(), InstanceError> {
        LORE_CONTEXT
            .scope(execution, async {
                instance_delete_with(self.repository().await, deregister).await
            })
            .await
    }

    fn left_in_parent(&self) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(self.parent.path())
            .expect("list the parent")
            .map(|entry| entry.expect("an entry").file_name())
            .filter(|name| name != "stores")
            .collect()
    }
}

async fn deregistered(_path: &Path, _instance_id: InstanceId) -> Result<(), InstanceError> {
    Ok(())
}

/// Makes the directory at `path` one whose entries cannot be added or removed, until dropped.
#[cfg(target_family = "unix")]
struct ReadOnly(PathBuf);

#[cfg(target_family = "unix")]
impl ReadOnly {
    /// `None` where the permission does not hold, as for root, whom the system lets past it.
    fn new(path: PathBuf) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555))
            .expect("make the directory read-only");
        let read_only = Self(path);
        let probe = read_only.0.join("probe");
        if std::fs::create_dir(&probe).is_ok() {
            let _ = std::fs::remove_dir(&probe);
            eprintln!(
                "Skipped: {} is writable all the same",
                read_only.0.display()
            );
            return None;
        }
        Some(read_only)
    }
}

#[cfg(target_family = "unix")]
impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// The instance's directory is renamed aside before its files are deleted, and the renamed
/// directory is deleted too, so the parent ends up holding neither.
#[tokio::test]
async fn a_deleted_instance_leaves_nothing_at_or_beside_its_path() {
    let instance = Instance::new();

    LORE_CONTEXT
        .scope(execution_in(instance._global.path()), async {
            instance_delete(instance.repository().await)
                .await
                .expect("delete the instance");
        })
        .await;

    let left = instance.left_in_parent();
    assert!(left.is_empty(), "the parent still holds {left:?}");
}

/// The files are deleted from the renamed directory, so a delete that cannot remove them has
/// already freed the instance's path, and leaves them under the aside name.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_failed_delete_leaves_the_files_aside_and_the_path_free() {
    let instance = Instance::new();
    let aside = instance.aside();
    let Some(read_only) = ReadOnly::new(instance.path.join("dir/sub")) else {
        return;
    };
    // The directory is renamed with its permissions, so they still hold under the aside name.
    let _read_only_aside = ReadOnly(aside.join("dir/sub"));

    let result = instance.delete_with(deregistered).await;
    drop(read_only);

    result.expect_err("the files cannot be deleted");
    assert!(!instance.path.exists(), "the instance's path is free");
    assert!(
        aside.join("dir/sub/file.txt").exists(),
        "the files are left under {}",
        aside.display()
    );
}

/// A rename aside that fails, other than for a mount point, fails the delete before anything is
/// removed.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_failed_rename_fails_the_delete_before_anything_is_removed() {
    let instance = Instance::new();
    let Some(_read_only) = ReadOnly::new(instance.parent.path().to_path_buf()) else {
        return;
    };

    let result = instance.delete_with(deregistered).await;

    result.expect_err("the directory cannot be renamed");
    assert!(instance.path.join("dir/sub/file.txt").exists());
    assert!(!instance.aside().exists(), "nothing was renamed aside");
}

/// The rename is not attempted onto a name something already holds, and nothing is changed.
#[tokio::test]
async fn a_taken_aside_name_fails_the_delete_before_anything_changes() {
    let instance = Instance::with_shared_store();
    instance.take_the_aside_name();

    let result = instance
        .delete_with(
            async |_: &Path, _: InstanceId| -> Result<(), InstanceError> {
                panic!("an instance whose delete cannot go through is not deregistered")
            },
        )
        .await;

    assert!(result.expect_err("the aside name is taken").is_internal());
    instance.assert_unchanged();
}

/// A dry run checks the aside name as a delete does, and changes nothing either way.
#[tokio::test]
async fn a_dry_run_refuses_a_taken_aside_name() {
    let instance = Instance::new();
    instance.dry_run_delete().await.expect("a free aside name");
    instance.take_the_aside_name();

    let result = instance.dry_run_delete().await;

    assert!(result.expect_err("the aside name is taken").is_internal());
    instance.assert_unchanged();
}

/// The deregistration comes before the rename, so a deregistration that fails leaves the
/// instance where it was, with all its files.
#[tokio::test]
async fn a_failed_deregistration_leaves_the_instance_intact() {
    let instance = Instance::with_shared_store();

    let result = instance
        .delete_with(async |path: &Path, _: InstanceId| {
            assert!(path.join(DOT_LORE).exists(), "deregistered at its own path");
            Err(InstanceError::internal("the registry is unavailable"))
        })
        .await;

    result.expect_err("the deregistration fails");
    assert!(instance.path.join("dir/sub/file.txt").exists());
    assert!(!instance.aside().exists(), "nothing was renamed aside");
}

/// The aside name is the instance directory's name with the instance's ID, in the same parent.
#[test]
fn the_aside_name_is_a_sibling_named_for_the_instance() {
    let instance_id = InstanceId::generate();
    let aside = deleting_aside_path(Path::new("/parent/instance"), instance_id);

    assert_eq!(
        aside,
        Path::new("/parent").join(format!("instance.deleting-{instance_id}"))
    );
}
