// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! FUSE instances mounted through the service's `MountManager` and its FUSE manager:
//! create, sync, the refusal outside the service, remount after a restart, and unmount on stop.
//!
//! Every test takes `SERIAL`: they share the process-wide `MountManagerState` and the
//! `LORE_GLOBAL_PATH` override, and `unmount_all` acts on every mount the manager holds.
//!
//! A process of its own: the tests initialise the process-wide mount manager, which every
//! repository path resolution in the process consults from then on, and set `LORE_GLOBAL_PATH`
//! while the process runs. `integration` drives a mounted instance through the same manager.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_fuse::skip_without_fuse;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::mount_manager::StopHook;
use lore_revision::fs::fuse::mount_manager::StopStep;
use lore_revision::fs::mount_manager::MountManager;
use lore_revision::fs::mount_manager_state::MountManagerState;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::instance::InstanceId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::DOT_LOREIGNORE;
use lore_revision::repository::DOT_URC;
use lore_revision::repository::DOT_URCIGNORE;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::VfsType;
use lore_revision::repository::create_local;
use lore_revision::repository::load_and_connect;
use lore_revision::repository::load_repository_config_from_dot_dir;
use lore_revision::revision::sync::SyncOptions;
use lore_revision::shared_store::get_shared_store_path_for_repo;
use lore_revision::state::State;
use lore_storage::local::store_lock::StoreLock;

#[path = "support/fuse_service.rs"]
mod fuse_service;
#[path = "support/fuse_test_util.rs"]
mod fuse_test_util;
#[path = "helper.rs"]
mod helper;
#[path = "fuse_mount/integration.rs"]
mod integration;
#[path = "support/xorshift.rs"]
mod xorshift;

use fuse_service::*;
use lore_storage::FlushOptions;

const CHILD_ENV: &str = "LORE_FUSE_MOUNT_TEST_OUTSIDE_SERVICE_CHILD";

#[tokio::test(flavor = "multi_thread")]
async fn create_local_with_fuse_mounts_and_lists_empty_root() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-create-");
        let _guard = MountGuard(path.clone());

        let (repository, _token) = create_instance(
            &path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        let instance_id = repository.instance_id;

        let entry = mount_entry(&path).expect("create_local mounts the instance");
        assert_eq!(entry.fstype, "fuse.lore");
        assert_eq!(
            entry.source,
            format!("lore:{}", instance_id.text_encoding())
        );
        let mount_options: Vec<&str> = entry.mount_options.split(',').collect();
        for option in ["rw", "nosuid", "nodev", "noatime"] {
            assert!(
                mount_options.contains(&option),
                "mount options {} lack {option}",
                entry.mount_options
            );
        }
        assert!(
            entry.super_options.contains("default_permissions"),
            "super options {} lack default_permissions",
            entry.super_options
        );

        assert!(
            list_names(&path).await.is_empty(),
            "a fresh instance is empty"
        );

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        assert!(
            manager
                .mounts()
                .contains(&(path.clone(), instance_id, VfsType::Fuse)),
            "the manager tracks the mount as FUSE"
        );
        assert_eq!(
            manager.mounted(&path).expect("the path makes a mount path"),
            Some((instance_id, VfsType::Fuse)),
            "the manager names the instance mounted at the path and its VFS"
        );
        assert_eq!(
            manager
                .check_for_external_lore_dir(&path)
                .expect("the lookup succeeds"),
            lore_revision::global::external_dir::check_for_external_lore_dir(instance_id)
                .expect("the global lookup succeeds"),
            "the mount leads to the instance's external .lore directory"
        );
        assert_eq!(
            manager
                .check_for_external_lore_dir(&store)
                .expect("the lookup succeeds"),
            None,
            "a path that is no mount leads to no external directory"
        );
        assert!(
            !served_repository(&manager, &path).disable_cache(),
            "a fresh FUSE mount caches fetched content, as a loaded FUSE instance does"
        );

        let dot_lore = external_dir_for_instance(instance_id)
            .expect("external dir resolves")
            .join(DOT_LORE);
        let config = load_repository_config_from_dot_dir(&dot_lore).expect("config loads");
        let base_time = config
            .vfs
            .and_then(|vfs| vfs.base_time)
            .expect("create_local records the base time");
        assert!(
            base_time > 1_000_000_000_000,
            "base time {base_time} must be in milliseconds"
        );

        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("an idle mount unmounts");
        assert!(!is_mounted(&path), "the mount is gone after unmount");
        assert_eq!(
            manager.mounted(&path).expect("the path makes a mount path"),
            None,
            "and the manager forgets it"
        );
    }))
    .await;
}

/// Whether this process holds the claim on the store `name` of the shared store `config` names,
/// as a command or a mount using it does. A lock opened on a store directory shares that
/// directory's state with every other lock in the process, so it reports what the process holds.
async fn shared_store_held(config: &RepositoryConfig, name: &str) -> bool {
    let shared = get_shared_store_path_for_repo(config)
        .await
        .expect("the shared store resolves")
        .expect("the instance uses a shared store");
    StoreLock::new(shared.join(name), false)
        .expect("a store directory")
        .is_held()
}

/// **A mounted instance holds its stores for as long as it is mounted.** The kernel's requests
/// arrive outside any command, so the mount claims the stores as a command does for its duration,
/// and lets them go once it is unmounted and nothing serves through it. A context it served
/// through keeps the claims past the unmount, as work still running through one does.
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_holds_its_stores_until_it_is_unmounted() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-claims-");
        let _guard = MountGuard(path.clone());
        let config = shared_store_config(&store, VfsType::Fuse);

        let repository = fresh_fuse_instance(&store, &path).await;
        assert!(
            shared_store_held(&config, "immutable").await
                && shared_store_held(&config, "mutable").await,
            "a mounted instance holds both stores with no command running"
        );

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let served = served_repository(&manager, &path);
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("an idle mount unmounts");
        // Its filesystem provider serves through the mount.
        drop(repository);
        assert!(
            shared_store_held(&config, "immutable").await
                && shared_store_held(&config, "mutable").await,
            "a context the mount served through keeps its claims"
        );

        drop(served);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while shared_store_held(&config, "immutable").await
            || shared_store_held(&config, "mutable").await
        {
            assert!(
                std::time::Instant::now() < deadline,
                "an unmounted instance let go of its stores"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_mount_keeps_writes_under_the_instance_write_directory_across_a_remount() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-write-");
        let _guard = MountGuard(path.clone());
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let (repository, token) = create_instance(
            &path,
            repository_id,
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        let (_twin_dir, target) =
            commit_from_twin(&store, repository_id, &[("tracked.txt", b"tracked")]).await;
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("operation begins");
        lore_revision::fs::realize::realize_state(
            repository.clone(),
            repository.clone(),
            operation,
            State::new(),
            target.clone(),
            SyncOptions::default(),
        )
        .await
        .expect("sync succeeds");
        lore_revision::instance::store_current_anchor(&repository, target.revision())
            .await
            .expect("anchor stores");
        repository
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        drop(token);
        // The mount records its writes in the staged State under the instance's write token,
        // which a context holding a sibling of it would keep from it.
        let released = Arc::new(repository.without_command_locks());
        drop(repository);
        let repository = released;

        let written = path.clone();
        lore_base::lore_spawn_blocking!(move || {
            std::fs::create_dir(written.join("Intermediate")).expect("mkdir through the mount");
            // Writes through the mount, as a build makes them.
            #[allow(clippy::disallowed_methods)]
            std::fs::write(written.join("Intermediate/obj.o"), b"object")
                .expect("write through the mount");
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_file(written.join("tracked.txt")).expect("unlink through the mount");
        })
        .await
        .expect("write task completes");
        let write = external_dir_for_instance(repository.instance_id)
            .expect("external dir resolves")
            .join("write");
        assert_eq!(
            std::fs::read(write.join("Intermediate/obj.o")).expect("the host file"),
            b"object"
        );

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
        let config =
            load_repository_config_from_dot_dir(&dot_lore_of(&repository)).expect("config loads");
        manager
            .fuse()
            .remount_instance(
                repository.clone(),
                &instance_metadata(&repository, &path),
                &dot_lore_of(&repository),
                &config,
            )
            .await
            .expect("remount succeeds");
        assert_eq!(list_names(&path).await, vec!["Intermediate"]);
        assert_eq!(
            read_file(path.join("Intermediate/obj.o")).await,
            b"object",
            "a remount serves what write/ kept"
        );
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn loading_a_fuse_instance_never_enters_the_mount() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    LORE_CONTEXT
        .scope(execution, async move {
            let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
            let (_dir, path) = canonical_dir("lore-fuse-mount-load-");
            let _guard = MountGuard(path.clone());
            let (repository, token) = create_instance(
                &path,
                RepositoryId::from(uuid::Uuid::now_v7()),
                shared_store_config(&store, VfsType::Fuse),
            )
            .await;
            drop(repository);
            drop(token);

            let manager = MountManagerState::mount_manager().expect("manager is initialised");
            let mount = manager
                .fuse()
                .mounted(&path)
                .expect("the instance is FUSE-mounted");

            let loaded = load_and_connect(&path, RepositoryAccess::ReadOnly)
                .await
                .expect("the service loads a mounted FUSE instance");
            assert!(
                loaded.file_system().begin_operation().await.is_ok(),
                "the loaded instance answers through the FUSE provider"
            );

            // Desktop services look names up in every new mount (GNOME's trash backend asks for
            // `.Trash` and `.Trash-<uid>`), so the check is on the names lore itself reads.
            let lore_names = [DOT_LORE, DOT_URC, DOT_LOREIGNORE, DOT_URCIGNORE];
            let mut looked_up = Vec::new();
            mount
                .filesystem
                .spellings()
                .copy_children_of(ROOT_INO, &mut looked_up);
            let entered: Vec<String> = looked_up
                .iter()
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .filter(|name| lore_names.contains(&name.as_str()))
                .collect();
            assert!(
                entered.is_empty(),
                "loading must read nothing inside the service's own mount; it looked up {entered:?}"
            );
            drop(loaded);
            manager
                .fuse()
                .unmount(&path, false)
                .await
                .expect("unmounts");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_into_fuse_instance_shows_committed_tree() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-sync-");
        let _guard = MountGuard(path.clone());
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        let (repository, token) = create_instance(
            &path,
            repository_id,
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        let (_twin_dir, target) = commit_from_twin(
            &store,
            repository_id,
            &[
                ("readme.txt", b"hello from the twin"),
                ("src/main.rs", b"fn main() {}"),
            ],
        )
        .await;

        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("the FUSE provider begins an operation");
        assert!(operation.holds_state(), "a FUSE operation holds its state");
        lore_revision::fs::realize::realize_state(
            repository.clone(),
            repository.clone(),
            operation,
            State::new(),
            target,
            SyncOptions::default(),
        )
        .await
        .expect("syncing into the virtual clone succeeds");

        assert_eq!(list_names(&path).await, vec!["readme.txt", "src"]);
        assert_eq!(list_names(&path.join("src")).await, vec!["main.rs"]);
        assert_eq!(
            read_file(path.join("readme.txt")).await,
            b"hello from the twin"
        );

        // The published snapshot must not keep the command's write token alive.
        drop(repository);
        drop(token);
        let _free =
            tokio::time::timeout(Duration::from_secs(5), RepositoryWriteToken::acquire(&path))
                .await
                .expect("the write token is free once the command's contexts drop");

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// Runs only as the child of `fuse_instance_outside_service_is_refused`: a process that never
/// initialised a mount manager, as a command outside the service is.
#[tokio::test(flavor = "multi_thread")]
async fn outside_service_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    use_private_global_dir();
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async move {
        assert!(MountManagerState::mount_manager().is_none());
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");

        let (_create_dir, create_path) = canonical_dir("lore-fuse-mount-outside-create-");
        let token = RepositoryWriteToken::acquire(&create_path).await;
        let refused = create_local(
            &create_path,
            &token,
            RepositoryId::from(uuid::Uuid::now_v7()),
            lore_base::types::Context::from(uuid::Uuid::now_v7()),
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            shared_store_config(&store, VfsType::Fuse),
            false,
        )
        .await
        .expect_err("creating a FUSE instance outside the service is refused");
        assert!(
            refused.to_string().contains("outside the service"),
            "unexpected refusal: {refused}"
        );
        drop(token);

        // An instance whose on-disk config says FUSE, as one left behind by a stopped service.
        let (_load_dir, load_path) = canonical_dir("lore-fuse-mount-outside-load-");
        let (repository, token) = create_instance(
            &load_path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::None),
        )
        .await;
        drop(repository);
        drop(token);
        lore_revision::util::config::save(
            &shared_store_config(&store, VfsType::Fuse),
            load_path
                .join(DOT_LORE)
                .join(lore_revision::repository::CONFIG),
        )
        .await
        .expect("config rewrites");
        let refused = load_and_connect(&load_path, RepositoryAccess::ReadOnly)
            .await
            .expect_err("loading a FUSE instance outside the service is refused");
        let text = refused.to_string();
        assert!(
            text.contains("Loading a fuse repository without using the service"),
            "unexpected refusal: {text}"
        );
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fuse_instance_outside_service_is_refused() {
    let exe = std::env::current_exe().expect("test binary path");
    let output = lore_base::lore_spawn_blocking!(move || {
        std::process::Command::new(exe)
            .args(["--exact", "outside_service_child", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env_remove("LORE_GLOBAL_PATH")
            .output()
    })
    .await
    .expect("child task completes")
    .expect("child runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The repository context the mount at `path` currently serves reads through.
fn served_repository(
    manager: &lore_revision::fs::mount_manager::MountManager,
    path: &Path,
) -> Arc<lore_revision::repository::RepositoryContext> {
    manager
        .fuse()
        .mounted(path)
        .expect("a FUSE mount at the path")
        .filesystem
        .served()
        .repository
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_endpoint_is_lazily_unmounted_before_remount() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-stale-");
        let _guard = MountGuard(path.clone());
        let (repository, token) = create_instance(
            &path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        drop(token);
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");

        // A crashed service: the device end closes with no unmount.
        let crashed_path = path.clone();
        lore_base::lore_spawn_blocking!(move || {
            let mount = lore_fuse::mount::mount(
                &crashed_path,
                &lore_fuse::mount::MountOptions {
                    fsname: "lore:crashed".to_string(),
                    max_read: 1 << 20,
                    fusermount: None,
                },
            )
            .expect("mounts the stand-in for the crashed service");
            drop(mount);
        })
        .await
        .expect("mount task completes");
        let stale_path = path.clone();
        assert!(
            lore_base::lore_spawn_blocking!(move || lore_fuse::mount::is_stale_mount(&stale_path))
                .await
                .expect("stale probe completes"),
            "the endpoint must be stale before the remount"
        );

        let config =
            load_repository_config_from_dot_dir(&dot_lore_of(&repository)).expect("config loads");
        manager
            .fuse()
            .remount_instance(
                repository.clone(),
                &instance_metadata(&repository, &path),
                &dot_lore_of(&repository),
                &config,
            )
            .await
            .expect("remount detaches the stale endpoint and mounts again");

        assert_eq!(
            lore_mounts_at(&path),
            1,
            "the stale mount must not stay stacked"
        );
        let entry = mount_entry(&path).expect("remounted");
        assert_eq!(
            entry.source,
            format!("lore:{}", repository.instance_id.text_encoding())
        );
        assert!(list_names(&path).await.is_empty());

        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remount_after_restart_keeps_inode_numbers() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-restart-");
        let _guard = MountGuard(path.clone());
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let (repository, token) = create_instance(
            &path,
            repository_id,
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        let (_twin_dir, target) = commit_from_twin(
            &store,
            repository_id,
            &[("top.txt", b"top"), ("dir/nested.txt", b"nested")],
        )
        .await;
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("operation begins");
        lore_revision::fs::realize::realize_state(
            repository.clone(),
            repository.clone(),
            operation,
            State::new(),
            target.clone(),
            SyncOptions::default(),
        )
        .await
        .expect("sync succeeds");
        lore_revision::instance::store_current_anchor(&repository, target.revision())
            .await
            .expect("anchor stores");
        repository
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        drop(token);

        let probes = [
            path.join("top.txt"),
            path.join("dir"),
            path.join("dir/nested.txt"),
        ];
        let mut before = Vec::new();
        for probe in &probes {
            before.push(inode_of(probe.clone()).await);
        }

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
        assert!(!is_mounted(&path));

        let config =
            load_repository_config_from_dot_dir(&dot_lore_of(&repository)).expect("config loads");
        manager
            .fuse()
            .remount_instance(
                repository.clone(),
                &instance_metadata(&repository, &path),
                &dot_lore_of(&repository),
                &config,
            )
            .await
            .expect("remount succeeds");

        let mut after = Vec::new();
        for probe in &probes {
            after.push(inode_of(probe.clone()).await);
        }
        assert_eq!(before, after, "inode numbers survive a restart");
        assert!(
            !served_repository(&manager, &path).disable_cache(),
            "a remounted instance caches fetched content, as a loaded FUSE instance does"
        );
        assert_eq!(read_file(path.join("dir/nested.txt")).await, b"nested");

        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unmount_all_reports_busy_paths_and_leaves_them_mounted() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_idle_dir, idle_path) = canonical_dir("lore-fuse-mount-idle-");
        let _busy_guard = MountGuard(busy_path.clone());
        let _idle_guard = MountGuard(idle_path.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let idle = fresh_fuse_instance(&store, &idle_path).await;
        let (_twin_dir, target) = commit_from_twin(&store, idle.id, &[("kept.txt", b"kept")]).await;
        let operation = idle
            .file_system()
            .begin_operation()
            .await
            .expect("operation begins");
        lore_revision::fs::realize::realize_state(
            idle.clone(),
            idle.clone(),
            operation,
            State::new(),
            target,
            SyncOptions::default(),
        )
        .await
        .expect("sync into the idle instance succeeds");
        let idle_inode = inode_of(idle_path.join("kept.txt")).await;
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount fails a plain unmount_all");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        let text = error.to_string();
        assert!(
            text.contains(&busy_path.display().to_string()),
            "the busy path is reported: {text}"
        );
        assert!(
            !text.contains(&idle_path.display().to_string()),
            "an idle path is not reported busy: {text}"
        );

        assert!(is_mounted(&busy_path), "the busy mount stays mounted");
        assert!(
            list_names(&busy_path).await.is_empty(),
            "the busy mount is still served"
        );
        assert!(
            manager.fuse().mounted(&busy_path).is_some(),
            "the manager still tracks the busy mount"
        );

        // A refused stop leaves the service serving every instance, idle ones included.
        assert!(is_mounted(&idle_path), "the idle mount stays mounted");
        assert_eq!(
            entry_source(&idle_path),
            Some(format!("lore:{}", idle.instance_id.text_encoding()))
        );
        assert_eq!(list_names(&idle_path).await, vec!["kept.txt"]);
        assert_eq!(read_file(idle_path.join("kept.txt")).await, b"kept");
        assert_eq!(
            inode_of(idle_path.join("kept.txt")).await,
            idle_inode,
            "the idle instance keeps its inode numbers"
        );
        assert!(
            manager.fuse().mounted(&idle_path).is_some(),
            "the manager still tracks the idle mount"
        );
        drop(idle);

        holder.close().await;
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("once idle, every mount unmounts");
        assert!(!is_mounted(&busy_path));
        assert!(!is_mounted(&idle_path));
        assert!(manager.mounts().is_empty(), "no mount is left tracked");
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unmount_all_force_lazily_unmounts_busy_paths() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    LORE_CONTEXT
        .scope(execution, async move {
            let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
            let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-force-");
            let _busy_guard = MountGuard(busy_path.clone());
            let _busy = fresh_fuse_instance(&store, &busy_path).await;
            let holder = Holder::open(&busy_path).await;

            let manager = MountManagerState::mount_manager().expect("manager is initialised");
            let _resume = ResumeMounting(manager.clone());
            manager
                .fuse()
                .unmount_all(true)
                .await
                .expect("a forced unmount_all detaches busy mounts");
            assert!(!is_mounted(&busy_path), "the busy mount is detached");
            assert!(manager.fuse().mounted(&busy_path).is_none());

            holder.close().await;
        })
        .await;
}

/// A path detached outside the manager no longer holds a mount, so a forced stop counts it as
/// detached, detaches the rest and forgets both; a second forced stop has nothing left to do.
#[tokio::test(flavor = "multi_thread")]
async fn unmount_all_force_detaches_the_rest_after_one_was_detached_outside() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_outside_dir, outside_path) = canonical_dir("lore-fuse-mount-outside-");
        let (_rest_dir, rest_path) = canonical_dir("lore-fuse-mount-rest-");
        let _outside_guard = MountGuard(outside_path.clone());
        let _rest_guard = MountGuard(rest_path.clone());
        let _outside = fresh_fuse_instance(&store, &outside_path).await;
        let _rest = fresh_fuse_instance(&store, &rest_path).await;
        lore_fuse::mount::unmount_path(&outside_path, true)
            .expect("the path detaches outside the manager");
        assert!(!is_mounted(&outside_path));

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        manager
            .fuse()
            .unmount_all(true)
            .await
            .expect("a forced stop goes past a path detached outside it");
        assert!(!is_mounted(&rest_path), "the rest is detached");
        assert!(manager.mounts().is_empty(), "no mount is left tracked");
        manager
            .fuse()
            .unmount_all(true)
            .await
            .expect("a second forced stop has nothing left to do");
    }))
    .await;
}

/// A plain stop, which refuses and remounts on any failure, counts a path detached outside the
/// manager as unmounted and goes ahead.
#[tokio::test(flavor = "multi_thread")]
async fn unmount_all_detaches_the_rest_after_one_was_detached_outside() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_outside_dir, outside_path) = canonical_dir("lore-fuse-mount-outside-");
        let (_rest_dir, rest_path) = canonical_dir("lore-fuse-mount-rest-");
        let _outside_guard = MountGuard(outside_path.clone());
        let _rest_guard = MountGuard(rest_path.clone());
        let _outside = fresh_fuse_instance(&store, &outside_path).await;
        let _rest = fresh_fuse_instance(&store, &rest_path).await;
        lore_fuse::mount::unmount_path(&outside_path, true)
            .expect("the path detaches outside the manager");
        assert!(!is_mounted(&outside_path));

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("a plain stop goes past a path detached outside it");
        assert!(!is_mounted(&rest_path), "the rest is unmounted");
        assert!(!is_mounted(&outside_path), "nothing is mounted again");
        assert!(manager.mounts().is_empty(), "no mount is left tracked");
    }))
    .await;
}

/// A clone the stopping manager refuses leaves no external directory behind, so the next start
/// does not find a half-made instance to mount.
#[tokio::test(flavor = "multi_thread")]
async fn a_fuse_clone_refused_by_a_stop_leaves_no_external_directory() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_late_dir, late_path) = canonical_dir("lore-fuse-mount-late-");
        let _late_guard = MountGuard(late_path.clone());
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("with nothing mounted, the stop goes ahead");

        let external_root = external_dir_for_instance(InstanceId::generate())
            .expect("external dir")
            .parent()
            .expect("external root")
            .to_path_buf();
        let before = names_in(&external_root);
        assert_refused_by_the_stop(clone_fails(&store, &late_path).await);
        assert_eq!(
            names_in(&external_root),
            before,
            "a refused clone leaves no external directory"
        );
    }))
    .await;
}

/// A clone the stopping manager refuses removes the mount point it made, and keeps one that was
/// there before it.
#[tokio::test(flavor = "multi_thread")]
async fn a_fuse_clone_refused_by_a_stop_removes_only_the_mount_point_it_made() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_kept_dir, kept_path) = canonical_dir("lore-fuse-mount-kept-");
        let made_path = kept_path.join("made");
        let _kept_guard = MountGuard(kept_path.clone());
        let _made_guard = MountGuard(made_path.clone());
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("with nothing mounted, the stop goes ahead");

        assert_refused_by_the_stop(clone_fails(&store, &made_path).await);
        assert!(
            !made_path.exists(),
            "the mount point the refused clone made is removed"
        );
        assert_refused_by_the_stop(clone_fails(&store, &kept_path).await);
        assert!(kept_path.is_dir(), "a mount point that was there stays");
    }))
    .await;
}

/// A stop that has taken its snapshot of the mounts may have missed a mount made since, so that
/// mount is refused while the stop runs: unmounted, and forgotten. Through a remount, which has
/// nothing of its own to undo, so the refusal is what leaves neither behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_made_while_a_stop_runs_is_refused_unmounted_and_forgotten() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-late-");
        let _guard = MountGuard(path.clone());
        let (repository, token) = create_instance(
            &path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        drop(token);
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
        let _resume = ResumeMounting(manager.clone());
        // As a stop leaves it from its snapshot of the mounts until it is refused.
        manager.fuse().stopping.store(true, Ordering::SeqCst);

        let config =
            load_repository_config_from_dot_dir(&dot_lore_of(&repository)).expect("config loads");
        let error = manager
            .fuse()
            .remount_instance(
                repository.clone(),
                &instance_metadata(&repository, &path),
                &dot_lore_of(&repository),
                &config,
            )
            .await
            .expect_err("a stop runs");
        assert!(
            error.to_string().contains("the service is stopping"),
            "not the stop's refusal: {error}"
        );
        assert!(!is_mounted(&path), "the refused mount is unmounted");
        assert!(manager.fuse().mounted(&path).is_none(), "and forgotten");
    }))
    .await;
}

/// Creates a FUSE instance at `path` on `store`, as a clone does, and answers its failure.
async fn clone_fails(store: &Path, path: &Path) -> Option<RepositoryError> {
    let token = RepositoryWriteToken::acquire(path).await;
    let created = create_local(
        path,
        &token,
        RepositoryId::from(uuid::Uuid::now_v7()),
        lore_base::types::Context::from(uuid::Uuid::from_u128(0x5eed)),
        lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
        shared_store_config(store, VfsType::Fuse),
        false,
    )
    .await;
    created.err()
}

/// Asserts that `error` is the stopping manager's refusal of a mount.
fn assert_refused_by_the_stop(error: Option<RepositoryError>) {
    let error = error.expect("a mount made while the service stops fails");
    let text = error.to_string();
    assert!(
        text.contains("the service is stopping"),
        "not the stop's refusal: {text}"
    );
}

/// The names in `dir`, none when it does not exist.
// A fixture read, outside any repository access.
#[allow(clippy::disallowed_methods)]
fn names_in(dir: &Path) -> std::collections::BTreeSet<std::ffi::OsString> {
    std::fs::read_dir(dir).map_or_default(|entries| {
        entries
            .filter_map(|entry| Some(entry.ok()?.file_name()))
            .collect()
    })
}

/// Lets the shared manager mount again when dropped, so a test that stops it leaves it usable for
/// the tests after it even when one of its assertions fails first.
struct ResumeMounting(Arc<MountManager>);

impl Drop for ResumeMounting {
    fn drop(&mut self) {
        self.0.fuse().resume_mounting();
    }
}

/// A stop that goes ahead ends mounting: a mount a command creates afterwards, as one on another
/// connection may while the stop runs, is unmounted and fails. A refused stop leaves it open.
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_created_after_a_stop_went_ahead_is_unmounted_and_fails() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_open_dir, open_path) = canonical_dir("lore-fuse-mount-open-");
        let (_late_dir, late_path) = canonical_dir("lore-fuse-mount-late-");
        let _busy_guard = MountGuard(busy_path.clone());
        let _open_guard = MountGuard(open_path.clone());
        let _late_guard = MountGuard(late_path.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount refuses the stop");
        let _open = fresh_fuse_instance(&store, &open_path).await;
        assert!(
            is_mounted(&open_path),
            "a refused stop leaves mounting open"
        );
        holder.close().await;

        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("once idle, the stop goes ahead");
        assert_refused_by_the_stop(clone_fails(&store, &late_path).await);
        assert!(!is_mounted(&late_path), "nor is it left mounted");
        assert!(manager.fuse().mounted(&late_path).is_none());
    }))
    .await;
}

/// A mount whose parent directory was renamed, and another made at the old name, is listed under a
/// path the stop no longer reaches, so it cannot be detached: a plain stop is refused and keeps it,
/// and a forced one forgets it and goes ahead with it still attached.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_meeting_a_mount_it_cannot_detach_is_refused_unless_forced() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_outer_dir, outer) = canonical_dir("lore-fuse-mount-moved-");
        let path = outer.join("before/mount");
        let moved = outer.join("after/mount");
        let _guard = MountGuard(path.clone());
        let _moved_guard = MountGuard(moved.clone());
        std::fs::create_dir(outer.join("before")).expect("the parent is made");
        let _instance = fresh_fuse_instance(&store, &path).await;
        // Fixture edits beside the mount, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::rename(outer.join("before"), outer.join("after")).expect("the parent renames");
        std::fs::create_dir(outer.join("before")).expect("the old parent name is made again");
        assert!(is_mounted(&moved), "the mount moves with its parent");

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a plain stop that cannot detach a mount is refused");
        assert!(!error.is_mounts_busy(), "unexpected error: {error}");
        assert!(
            manager.fuse().mounted(&path).is_some(),
            "a refused stop keeps the mount"
        );

        manager
            .fuse()
            .unmount_all(true)
            .await
            .expect("a forced stop goes ahead");
        assert!(
            manager.fuse().mounted(&path).is_none(),
            "a forced stop forgets the mount"
        );
        assert!(is_mounted(&moved), "the mount stays attached");
    }))
    .await;
}

/// A forced stop forgets a mount it cannot detach even once the mount point's parent is gone,
/// when its path no longer resolves to the key the manager holds it under.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_stop_forgets_a_mount_whose_parent_was_renamed() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_outer_dir, outer) = canonical_dir("lore-fuse-mount-moved-");
        let path = outer.join("before/mount");
        let moved = outer.join("after/mount");
        let _moved_guard = MountGuard(moved.clone());
        std::fs::create_dir(outer.join("before")).expect("the parent is made");
        let instance = fresh_fuse_instance(&store, &path).await;
        // A fixture edit beside the mount, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::rename(outer.join("before"), outer.join("after")).expect("the parent renames");
        assert!(is_mounted(&moved), "the mount moves with its parent");

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        manager
            .fuse()
            .unmount_all(true)
            .await
            .expect("a forced stop goes ahead");
        let mounts = manager.mounts();
        assert!(
            !mounts.iter().any(|(_, id, _)| *id == instance.instance_id),
            "a forced stop forgets the mount: {mounts:?}"
        );
        assert!(is_mounted(&moved), "the mount stays attached");
    }))
    .await;
}

/// A stop forgets a mount detached elsewhere after its mount point's parent was renamed, when its
/// path no longer resolves to the key the manager holds it under.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_forgets_a_mount_detached_elsewhere_after_its_parent_was_renamed() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_outer_dir, outer) = canonical_dir("lore-fuse-mount-moved-");
        let path = outer.join("before/mount");
        let moved = outer.join("after/mount");
        let _moved_guard = MountGuard(moved.clone());
        std::fs::create_dir(outer.join("before")).expect("the parent is made");
        let instance = fresh_fuse_instance(&store, &path).await;
        // A fixture edit beside the mount, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::rename(outer.join("before"), outer.join("after")).expect("the parent renames");
        lore_fuse::mount::unmount_path(&moved, true).expect("the mount detaches at its new place");
        assert!(!is_mounted(&moved), "the mount is detached");

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("a stop goes past a mount detached elsewhere");
        let mounts = manager.mounts();
        assert!(
            !mounts.iter().any(|(_, id, _)| *id == instance.instance_id),
            "the stop forgets the mount: {mounts:?}"
        );
    }))
    .await;
}

/// A refused stop forgets, rather than mounts again, a mount it unmounted whose mount point's parent
/// is gone, as a start does: it was detached elsewhere after its parent was renamed away, and the
/// old path is not made again.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_stop_forgets_a_mount_whose_parent_is_gone() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_outer_dir, outer) = canonical_dir("lore-fuse-mount-moved-");
        let path = outer.join("before/mount");
        let moved = outer.join("after/mount");
        let _busy_guard = MountGuard(busy_path.clone());
        let _guard = MountGuard(path.clone());
        let _moved_guard = MountGuard(moved.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        std::fs::create_dir(outer.join("before")).expect("the parent is made");
        let instance = fresh_fuse_instance(&store, &path).await;
        // A fixture edit beside the mount, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::rename(outer.join("before"), outer.join("after")).expect("the parent renames");
        lore_fuse::mount::unmount_path(&moved, true).expect("the mount detaches at its new place");
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount refuses the stop");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(
            !outer.join("before").exists(),
            "the old parent is not made again"
        );
        let mounts = manager.mounts();
        assert!(
            !mounts.iter().any(|(_, id, _)| *id == instance.instance_id),
            "the refused stop forgets the mount: {mounts:?}"
        );
        assert!(is_mounted(&busy_path), "the busy mount stays mounted");
        holder.close().await;
    }))
    .await;
}

/// A refused stop forgets, rather than mounts again, a mount whose mount point was renamed after it
/// was detached elsewhere, as a start finds the mount point missing; the old path is not made
/// again.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_stop_forgets_a_mount_whose_mount_point_was_renamed() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_outer_dir, outer) = canonical_dir("lore-fuse-mount-renamed-");
        let path = outer.join("mount");
        let _busy_guard = MountGuard(busy_path.clone());
        let _guard = MountGuard(path.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let instance = fresh_fuse_instance(&store, &path).await;
        lore_fuse::mount::unmount_path(&path, true).expect("the mount detaches");
        // A fixture edit beside the mount, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::rename(&path, outer.join("renamed")).expect("the mount point renames");
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount refuses the stop");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(!path.exists(), "the old mount point is not made again");
        let mounts = manager.mounts();
        assert!(
            !mounts.iter().any(|(_, id, _)| *id == instance.instance_id),
            "the refused stop forgets the mount: {mounts:?}"
        );
        assert!(is_mounted(&busy_path), "the busy mount stays mounted");
        holder.close().await;
    }))
    .await;
}

/// A refused stop forgets a mount detached elsewhere after its parent was renamed, though the
/// parent's old name was made again: a start finds the mount point itself missing.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_stop_forgets_a_mount_whose_parent_was_renamed_and_made_again() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_outer_dir, outer) = canonical_dir("lore-fuse-mount-moved-");
        let path = outer.join("before/mount");
        let moved = outer.join("after/mount");
        let _busy_guard = MountGuard(busy_path.clone());
        let _guard = MountGuard(path.clone());
        let _moved_guard = MountGuard(moved.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        std::fs::create_dir(outer.join("before")).expect("the parent is made");
        let instance = fresh_fuse_instance(&store, &path).await;
        // A fixture edit beside the mount, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::rename(outer.join("before"), outer.join("after")).expect("the parent renames");
        lore_fuse::mount::unmount_path(&moved, true).expect("the mount detaches at its new place");
        std::fs::create_dir(outer.join("before")).expect("the old parent name is made again");
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount refuses the stop");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(!path.exists(), "the old mount point is not made again");
        let mounts = manager.mounts();
        assert!(
            !mounts.iter().any(|(_, id, _)| *id == instance.instance_id),
            "the refused stop forgets the mount: {mounts:?}"
        );
        holder.close().await;
    }))
    .await;
}

/// A mount someone else unmounts while a refused stop mounts it again stays unmounted: the stop
/// detaches the mount it made rather than record it.
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_unmounted_while_a_refused_stop_remounts_it_stays_unmounted() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_idle_dir, idle_path) = canonical_dir("lore-fuse-mount-idle-");
        let _busy_guard = MountGuard(busy_path.clone());
        let _idle_guard = MountGuard(idle_path.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let _idle = fresh_fuse_instance(&store, &idle_path).await;
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let unmounted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _hook = on_stop_steps(&manager, {
            let (manager, idle_path, unmounted) =
                (manager.clone(), idle_path.clone(), unmounted.clone());
            Arc::new(move |step| {
                let (manager, idle_path, unmounted) =
                    (manager.clone(), idle_path.clone(), unmounted.clone());
                Box::pin(async move {
                    if step == StopStep::Remounting {
                        manager
                            .fuse()
                            .unmount(&idle_path, true)
                            .await
                            .expect("the idle mount unmounts");
                        unmounted.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
        });
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount refuses the stop");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(
            unmounted.load(std::sync::atomic::Ordering::SeqCst),
            "the idle mount was unmounted as the stop remounted it"
        );
        assert!(!is_mounted(&idle_path), "the idle mount stays unmounted");
        assert!(manager.fuse().mounted(&idle_path).is_none());
        assert!(is_mounted(&busy_path), "the busy mount stays mounted");
        holder.close().await;
    }))
    .await;
}

/// The hook a test runs at each step of the shared manager's stops; none outside such a test.
static STOP_HOOK: std::sync::Mutex<Option<StopHook>> = std::sync::Mutex::new(None);

/// Runs `hook` at each step of `manager`'s stops until the answer is dropped.
fn on_stop_steps(manager: &MountManager, hook: StopHook) -> StopHookSet {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        manager.fuse().set_stop_hook(Arc::new(|step| {
            let hook = STOP_HOOK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            Box::pin(async move {
                if let Some(hook) = hook {
                    hook(step).await;
                }
            })
        }));
    });
    *STOP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    StopHookSet
}

/// Clears the test's stop hook when dropped.
struct StopHookSet;

impl Drop for StopHookSet {
    fn drop(&mut self) {
        *STOP_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// A mount someone else unmounts while a refused stop runs is not mounted again.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_stop_does_not_remount_a_mount_unmounted_while_it_ran() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_idle_dir, idle_path) = canonical_dir("lore-fuse-mount-idle-");
        let _busy_guard = MountGuard(busy_path.clone());
        let _idle_guard = MountGuard(idle_path.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let _idle = fresh_fuse_instance(&store, &idle_path).await;
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let unmounted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _hook = on_stop_steps(&manager, {
            let (manager, idle_path, unmounted) =
                (manager.clone(), idle_path.clone(), unmounted.clone());
            Arc::new(move |step| {
                let (manager, idle_path, unmounted) =
                    (manager.clone(), idle_path.clone(), unmounted.clone());
                Box::pin(async move {
                    if step == StopStep::Unmounted {
                        manager
                            .fuse()
                            .unmount(&idle_path, true)
                            .await
                            .expect("the idle mount unmounts");
                        unmounted.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
        });
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount refuses the stop");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(
            !error.to_string().contains(&idle_path.display().to_string()),
            "the stop unmounted the idle mount: {error}"
        );
        assert!(
            unmounted.load(std::sync::atomic::Ordering::SeqCst),
            "the idle mount was unmounted while the stop ran"
        );
        assert!(
            !is_mounted(&idle_path),
            "the idle mount is not mounted again"
        );
        assert!(manager.fuse().mounted(&idle_path).is_none());
        assert!(is_mounted(&busy_path), "the busy mount stays mounted");
        holder.close().await;
    }))
    .await;
}

/// A plain stop refused while a forced stop runs leaves mounting closed: the forced stop set the
/// service exiting before it took its snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_stop_refused_while_a_forced_stop_runs_leaves_mounting_closed() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_late_dir, late_path) = canonical_dir("lore-fuse-mount-late-");
        let _busy_guard = MountGuard(busy_path.clone());
        let _late_guard = MountGuard(late_path.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let paused = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let snapshots = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _hook = on_stop_steps(&manager, {
            let (paused, release) = (paused.clone(), release.clone());
            Arc::new(move |step| {
                let first = step == StopStep::Snapshot
                    && snapshots.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                let (paused, release) = (paused.clone(), release.clone());
                Box::pin(async move {
                    if first {
                        paused.notify_one();
                        release.notified().await;
                    }
                })
            })
        });
        let forced = lore_base::lore_spawn!({
            let manager = manager.clone();
            async move { manager.fuse().unmount_all(true).await }
        });
        paused.notified().await;

        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("the busy mount refuses the plain stop");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert_refused_by_the_stop(clone_fails(&store, &late_path).await);
        assert!(!is_mounted(&late_path), "nor is it left mounted");

        release.notify_one();
        forced
            .await
            .expect("the forced stop's task")
            .expect("the forced stop goes ahead");
        assert!(
            !is_mounted(&busy_path),
            "the forced stop detaches the busy mount"
        );
        holder.close().await;
    }))
    .await;
}

/// Writes `contents` to `path` through the mount, as a program would.
// A fixture write through the mount, outside any repository write.
#[allow(clippy::disallowed_methods)]
async fn write_through(path: std::path::PathBuf, contents: &'static [u8]) {
    lore_base::lore_spawn_blocking!(move || std::fs::write(&path, contents))
        .await
        .expect("write task completes")
        .expect("the write through the mount succeeds");
}

/// A moved mount serves the instance at its target, its uncommitted writes too, and leaves the
/// source mount point in place and empty, where a move back mounts it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_mount_serves_its_writes_at_the_target_and_leaves_the_source_empty() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-move-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("moved");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());
        let instance = fresh_fuse_instance(&store, &path).await;
        write_through(path.join("local.txt"), b"written before the move").await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .move_mount(instance.instance_id, &path, &target)
            .await
            .expect("the mount moves");
        assert!(is_mounted(&target), "mounted at the target");
        assert!(!is_mounted(&path), "and no longer at the source");
        assert!(manager.fuse().mounted(&target).is_some());
        assert!(manager.fuse().mounted(&path).is_none());
        assert!(
            path.is_dir() && names_in(&path).is_empty(),
            "the source mount point stays, empty"
        );
        assert_eq!(
            read_file(target.join("local.txt")).await,
            b"written before the move"
        );

        manager
            .fuse()
            .move_mount(instance.instance_id, &target, &path)
            .await
            .expect("the mount moves back onto the mount point it left");
        assert!(is_mounted(&path), "mounted at the source again");
        assert!(!is_mounted(&target));
        assert_eq!(
            read_file(path.join("local.txt")).await,
            b"written before the move"
        );
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// A move whose target cannot take the mount, a directory holding a file, mounts the instance at
/// its source again and leaves the target as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_onto_a_directory_holding_files_mounts_the_instance_at_its_source_again() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-move-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("occupied");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());
        // Fixture edits beside the mount, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        let made = std::fs::create_dir(&target)
            .and_then(|()| std::fs::write(target.join("held.txt"), b"held"));
        made.expect("the target is made, holding a file");
        let instance = fresh_fuse_instance(&store, &path).await;
        write_through(path.join("local.txt"), b"kept").await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let error = manager
            .fuse()
            .move_mount(instance.instance_id, &path, &target)
            .await
            .expect_err("a target holding a file refuses the mount");
        assert!(
            error.to_string().contains("not empty"),
            "unexpected error: {error}"
        );
        assert!(is_mounted(&path), "mounted at its source again");
        assert!(manager.fuse().mounted(&path).is_some(), "and tracked there");
        assert!(!is_mounted(&target));
        assert_eq!(
            names_in(&target),
            std::collections::BTreeSet::from([std::ffi::OsString::from("held.txt")]),
            "the target is left as it was"
        );
        assert_eq!(read_file(path.join("local.txt")).await, b"kept");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// A move of a mount something holds is refused by its plain unmount, with nothing changed.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_of_a_busy_mount_is_refused_with_nothing_changed() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-move-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("moved");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());
        let instance = fresh_fuse_instance(&store, &path).await;
        let holder = Holder::open(&path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let error = manager
            .fuse()
            .move_mount(instance.instance_id, &path, &target)
            .await
            .expect_err("a busy mount refuses the move");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(is_mounted(&path), "still mounted at the source");
        assert!(manager.fuse().mounted(&path).is_some(), "and tracked there");
        assert!(!target.exists(), "no mount point is made at the target");
        holder.close().await;
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// A move while a stop runs is refused before anything changes, and the stop treats the mount as
/// it would any other.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_while_the_service_stops_is_refused_with_nothing_changed() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_idle_dir, idle_path) = canonical_dir("lore-fuse-mount-idle-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("moved");
        let _busy_guard = MountGuard(busy_path.clone());
        let _idle_guard = MountGuard(idle_path.clone());
        let _target_guard = MountGuard(target.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let idle = fresh_fuse_instance(&store, &idle_path).await;
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let seen = Arc::new(std::sync::Mutex::new(None));
        let _hook = on_stop_steps(&manager, {
            let (manager, idle_path, target, seen) = (
                manager.clone(),
                idle_path.clone(),
                target.clone(),
                seen.clone(),
            );
            let instance_id = idle.instance_id;
            Arc::new(move |step| {
                let (manager, idle_path, target, seen) = (
                    manager.clone(),
                    idle_path.clone(),
                    target.clone(),
                    seen.clone(),
                );
                Box::pin(async move {
                    if step != StopStep::Snapshot {
                        return;
                    }
                    let moved = manager
                        .fuse()
                        .move_mount(instance_id, &idle_path, &target)
                        .await
                        .map_err(|error| error.to_string());
                    let mounted = is_mounted(&idle_path);
                    *seen
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some((moved, mounted, target.exists()));
                })
            })
        });
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("a busy mount refuses the stop");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        let (moved, mounted, target_made) = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("the move ran while the stop did");
        let refusal = moved.expect_err("the move is refused while the service stops");
        assert!(
            refusal.contains("the service is stopping"),
            "not the stop's refusal: {refusal}"
        );
        assert!(mounted, "the refused move left the mount in place");
        assert!(!target_made, "and made nothing at the target");
        assert!(
            is_mounted(&idle_path),
            "the refused stop leaves the instance mounted where it was"
        );
        assert!(!target.exists());
        holder.close().await;
    }))
    .await;
}

/// The instance at `path` opened as the service opens it for a move or a delete: without its
/// stores, its directory lock or its write token, which the command takes when it needs them.
/// The context `fresh_fuse_instance` answers holds the write token, so it is dropped first.
async fn opened_as_a_command(
    instance: Arc<lore_revision::repository::RepositoryContext>,
    path: &Path,
) -> Arc<lore_revision::repository::RepositoryContext> {
    drop(instance);
    lore_revision::repository::load_and_connect_with_token(path, RepositoryAccess::NoStore, None)
        .await
        .expect("the instance opens")
}

/// Runs `on_unmounted` where a move has unmounted its instance and has yet to mount it at its
/// target, until the answer is dropped.
fn on_move_unmounted(
    manager: &MountManager,
    on_unmounted: impl Fn() -> lore_revision::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> StopHookSet {
    on_stop_steps(
        manager,
        Arc::new(move |step| {
            if step == StopStep::MoveUnmounted {
                on_unmounted()
            } else {
                Box::pin(async {})
            }
        }),
    )
}

/// A stop that goes ahead while a move has its instance unmounted refuses the mount at the
/// target and at the source both. The instance is left unmounted, with its source mount point and
/// its external directory in place, for the next start to mount where it is registered.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_overtaken_by_a_stop_leaves_the_instance_for_the_next_start() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-move-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("moved");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());
        let instance = fresh_fuse_instance(&store, &path).await;
        let external = external_dir_for_instance(instance.instance_id).expect("external dir");

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let _hook = on_move_unmounted(&manager, {
            let manager = manager.clone();
            move || {
                let manager = manager.clone();
                Box::pin(async move {
                    manager
                        .fuse()
                        .unmount_all(false)
                        .await
                        .expect("with nothing else mounted, the stop goes ahead");
                })
            }
        });
        let error = manager
            .fuse()
            .move_mount(instance.instance_id, &path, &target)
            .await
            .expect_err("the stop refuses the move's mounts");
        assert!(
            error.to_string().contains("the service is stopping"),
            "unexpected error: {error}"
        );
        assert!(
            !is_mounted(&path) && !is_mounted(&target),
            "mounted nowhere"
        );
        assert!(
            manager.fuse().mounted(&path).is_none() && manager.fuse().mounted(&target).is_none()
        );
        assert!(
            path.is_dir(),
            "the source mount point stays, as the registration names it"
        );
        assert!(!target.exists(), "the target mount point is removed");
        assert!(external.exists(), "the external directory stays");
    }))
    .await;
}

/// A stop refused while a move has its instance unmounted leaves the move to finish.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_beside_a_refused_stop_mounts_the_instance_at_its_target() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_busy_dir, busy_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-move-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("moved");
        let _busy_guard = MountGuard(busy_path.clone());
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());
        let _busy = fresh_fuse_instance(&store, &busy_path).await;
        let instance = fresh_fuse_instance(&store, &path).await;
        let holder = Holder::open(&busy_path).await;

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let _hook = on_move_unmounted(&manager, {
            let manager = manager.clone();
            move || {
                let manager = manager.clone();
                Box::pin(async move {
                    let error = manager
                        .fuse()
                        .unmount_all(false)
                        .await
                        .expect_err("the busy mount refuses the stop");
                    assert!(error.is_mounts_busy(), "unexpected error: {error}");
                })
            }
        });
        manager
            .fuse()
            .move_mount(instance.instance_id, &path, &target)
            .await
            .expect("the move finishes once the stop is refused");
        assert!(is_mounted(&target), "mounted at the target");
        assert!(manager.fuse().mounted(&target).is_some());
        assert!(is_mounted(&busy_path), "the busy mount stays mounted");
        holder.close().await;
        manager
            .fuse()
            .unmount(&target, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// A move whose registration cannot be written mounts the instance back at its source, and once
/// the source is registered again removes the mount point it made at the target.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_whose_registration_fails_mounts_the_instance_back_at_its_source() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-move-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("moved");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());
        let instance = fresh_fuse_instance(&store, &path).await;
        write_through(path.join("local.txt"), b"kept").await;
        let command = opened_as_a_command(instance, &path).await;

        let error = lore_revision::instance::instance_move_with(
            command,
            &target.display().to_string(),
            async |at: &Path| {
                if at == target.as_path() {
                    Err(lore_revision::instance::InstanceError::internal(
                        "the registry is unavailable",
                    ))
                } else {
                    Ok(())
                }
            },
        )
        .await
        .expect_err("the move fails");
        let message = error.to_string();
        assert!(
            message.contains("Failed to record the new path"),
            "{message}"
        );
        assert!(
            !message.contains("also failed") && !message.contains("could not be restored"),
            "the instance is back and registered at its source: {message}"
        );
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        assert!(is_mounted(&path), "mounted at its source again");
        assert!(manager.fuse().mounted(&path).is_some());
        assert!(!target.exists(), "the target mount point is removed");
        assert_eq!(read_file(path.join("local.txt")).await, b"kept");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// Deleting a FUSE instance unmounts it, then removes its external directory and its mount
/// point.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_an_instance_unmounts_it_and_removes_its_directories() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, parent) = canonical_dir("lore-fuse-mount-delete-");
        let path = parent.join("instance");
        let _guard = MountGuard(path.clone());
        let instance = fresh_fuse_instance(&store, &path).await;
        write_through(path.join("local.txt"), b"deleted with it").await;
        let external = external_dir_for_instance(instance.instance_id).expect("external dir");
        assert!(external.exists());
        let command = opened_as_a_command(instance, &path).await;

        lore_revision::instance::instance_delete(command)
            .await
            .expect("the instance is deleted");
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        assert!(!is_mounted(&path), "unmounted");
        assert!(manager.fuse().mounted(&path).is_none(), "and forgotten");
        assert!(!external.exists(), "its external directory is removed");
        assert!(!path.exists(), "and its mount point");
    }))
    .await;
}

/// Deleting or moving an instance something holds is refused as busy, the error a caller can tell
/// apart, with nothing changed.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_or_moving_a_busy_instance_is_refused_as_busy() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-busy-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-move-target-");
        let target = target_parent.join("moved");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());
        let instance = fresh_fuse_instance(&store, &path).await;
        let external = external_dir_for_instance(instance.instance_id).expect("external dir");
        let holder = Holder::open(&path).await;
        let command = opened_as_a_command(instance, &path).await;

        let error = lore_revision::instance::instance_delete(command.clone())
            .await
            .expect_err("a busy instance refuses the delete");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        let error = lore_revision::instance::instance_move(command, &target.display().to_string())
            .await
            .expect_err("a busy instance refuses the move");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(is_mounted(&path), "still mounted");
        assert!(external.exists(), "with its external directory");
        assert!(!target.exists(), "and nothing at the target");
        holder.close().await;
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// A stop passes over a mount in its snapshot that another command unmounted meanwhile, while the
/// same instance is mounted elsewhere under its name, as a move leaves it: it neither detaches by
/// path what it no longer tracks nor takes the name standing elsewhere for a failed unmount.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_passes_over_a_snapshot_mount_unmounted_meanwhile() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-first-");
        let (_elsewhere_dir, elsewhere) = canonical_dir("lore-fuse-mount-elsewhere-");
        let _guard = MountGuard(path.clone());
        let _elsewhere_guard = MountGuard(elsewhere.clone());
        let repository = fresh_fuse_instance(&store, &path).await;
        let config =
            load_repository_config_from_dot_dir(&dot_lore_of(&repository)).expect("config loads");
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .remount_instance(
                repository.clone(),
                &instance_metadata(&repository, &elsewhere),
                &dot_lore_of(&repository),
                &config,
            )
            .await
            .expect("the instance mounts a second time, under the same name");

        let _resume = ResumeMounting(manager.clone());
        let _hook = on_stop_steps(&manager, {
            let (manager, path) = (manager.clone(), path.clone());
            Arc::new(move |step| {
                let (manager, path) = (manager.clone(), path.clone());
                Box::pin(async move {
                    if step == StopStep::Snapshot {
                        manager
                            .fuse()
                            .unmount(&path, false)
                            .await
                            .expect("the first mount unmounts while the stop runs");
                    }
                })
            })
        });
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("the stop passes over the mount it no longer tracks");
        assert!(!is_mounted(&path), "the first mount stays unmounted");
        assert!(!is_mounted(&elsewhere), "the stop unmounts the other");
    }))
    .await;
}

const INITIALIZE_CHILD_ENV: &str = "LORE_FUSE_MOUNT_TEST_INITIALIZE_CHILD";

/// Runs only as the child of `initialize_remounts_registered_fuse_instances`, in a process
/// whose global data directory holds nothing but this instance's shared store, so the registry
/// scan sees exactly one FUSE instance.
#[tokio::test(flavor = "multi_thread")]
async fn initialize_child() {
    if std::env::var_os(INITIALIZE_CHILD_ENV).is_none() {
        return;
    }
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-initialize-");
        let _guard = MountGuard(path.clone());
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let (repository, token) = create_instance(
            &path,
            repository_id,
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        let (_twin_dir, target) =
            commit_from_twin(&store, repository_id, &[("scanned.txt", b"scanned")]).await;
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("operation begins");
        lore_revision::fs::realize::realize_state(
            repository.clone(),
            repository.clone(),
            operation,
            State::new(),
            target.clone(),
            SyncOptions::default(),
        )
        .await
        .expect("sync succeeds");
        lore_revision::instance::store_current_anchor(&repository, target.revision())
            .await
            .expect("anchor stores");
        repository
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        let instance_id = repository.instance_id;
        let inode = inode_of(path.join("scanned.txt")).await;
        drop(repository);
        drop(token);

        // The service stops, then a new one starts and finds the instance through the registry.
        let previous = MountManagerState::mount_manager().expect("manager is initialised");
        previous
            .fuse()
            .unmount_all(false)
            .await
            .expect("the idle instance unmounts");
        assert!(!is_mounted(&path));

        let restarted = lore_revision::fs::mount_manager::MountManager::initialize()
            .await
            .expect("a restarted service initialises");
        assert!(
            restarted
                .mounts()
                .contains(&(path.clone(), instance_id, VfsType::Fuse)),
            "the registry scan remounts the FUSE instance"
        );
        assert_eq!(
            entry_source(&path),
            Some(format!("lore:{}", instance_id.text_encoding()))
        );
        assert_eq!(list_names(&path).await, vec!["scanned.txt"]);
        assert_eq!(read_file(path.join("scanned.txt")).await, b"scanned");
        assert_eq!(inode_of(path.join("scanned.txt")).await, inode);

        restarted.fuse().unmount_all(false).await.expect("unmounts");
        assert!(!is_mounted(&path));

        // The mount point is deleted while the service is down: the next start removes the
        // instance's external directory instead of recreating the path and mounting it.
        // A fixture edit, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_dir(&path).expect("mount point removes");
        let external = external_dir_for_instance(instance_id).expect("external dir");
        assert!(external.exists());
        let cleaned = lore_revision::fs::mount_manager::MountManager::initialize()
            .await
            .expect("a service over a deleted mount point initialises");
        assert!(cleaned.mounts().is_empty(), "{:?}", cleaned.mounts());
        assert!(!path.exists(), "the mount point is not recreated");
        assert!(!external.exists(), "the external directory is removed");
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_remounts_registered_fuse_instances() {
    skip_without_fuse!();
    let exe = std::env::current_exe().expect("test binary path");
    let output = lore_base::lore_spawn_blocking!(move || {
        std::process::Command::new(exe)
            .args(["--exact", "initialize_child", "--nocapture"])
            .env(INITIALIZE_CHILD_ENV, "1")
            .env_remove("LORE_GLOBAL_PATH")
            .output()
    })
    .await
    .expect("child task completes")
    .expect("child runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Removes `path` through the mount, as a program would.
// A fixture removal through the mount, outside any repository write.
#[allow(clippy::disallowed_methods)]
async fn remove_through(path: std::path::PathBuf) {
    lore_base::lore_spawn_blocking!(move || std::fs::remove_file(&path))
        .await
        .expect("remove task completes")
        .expect("the removal through the mount succeeds");
}

/// The modification time `path` reports through the mount.
async fn mtime_of(path: std::path::PathBuf) -> std::time::SystemTime {
    lore_base::lore_spawn_blocking!(move || std::fs::metadata(&path)?.modified())
        .await
        .expect("stat task completes")
        .expect("stat through the mount succeeds")
}

/// The name of the file [`synced_fuse_instance`]'s second sync adds, for which that swap records
/// a time.
const SWAPPED: &str = "swapped.txt";

/// A virtual clone at `path` on `store`, synced to a revision of `files` committed from an OS
/// instance, then to one that adds [`SWAPPED`], as syncs swap them in and record them. The first
/// publish onto the empty revision records no time, so every file of `files` reports the base time,
/// and the second swap records one for [`SWAPPED`].
async fn synced_fuse_instance(store: &Path, path: &Path, files: &[(&str, &[u8])]) -> InstanceId {
    let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
    let (repository, token) = create_instance(
        path,
        repository_id,
        shared_store_config(store, VfsType::Fuse),
    )
    .await;
    let twin = Box::pin(Twin::new(store, repository_id)).await;
    let first = Box::pin(twin.commit(files)).await;
    let second = Box::pin(twin.commit(&[(SWAPPED, b"added by the second sync")])).await;
    let mut served = State::new();
    for target in [first, second] {
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("the FUSE provider begins an operation");
        lore_revision::fs::realize::realize_state(
            repository.clone(),
            repository.clone(),
            operation,
            served,
            target.clone(),
            SyncOptions::default(),
        )
        .await
        .expect("syncing into the virtual clone succeeds");
        // As a sync records the revision it swapped in, which a mount of the instance serves.
        lore_revision::instance::store_current_anchor(&repository, target.revision())
            .await
            .expect("record the current revision");
        served = target;
    }
    repository
        .flush(FlushOptions::default())
        .await
        .expect("flush the anchors");
    let instance_id = repository.instance_id;
    drop(repository);
    drop(token);
    instance_id
}

/// The instances the store of the instance at `path` registers.
async fn instances_registered_beside(path: &Path) -> Vec<InstanceId> {
    let repository = load_and_connect(path, RepositoryAccess::ReadOnly)
        .await
        .expect("the instance opens");
    lore_revision::instance::list_instances(&repository)
        .await
        .expect("list the instances")
        .iter()
        .map(|instance| instance.instance_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Plans cloning the instance at `path` to `destination` over the instance opened without its
/// stores, as the command plans it in the service, and opens the instance holding its repository
/// lock alone, as the command then does, for `clone` to clone it with. Finishes cloning whatever
/// `clone` answers.
async fn clone_instance_at(
    path: &Path,
    destination: &Path,
    allow_copy: bool,
    clone: impl AsyncFnOnce(CommandContext, &ClonePlan) -> Result<(), InstanceError>,
) -> Result<Option<InstanceId>, InstanceError> {
    let unlocked = lore_revision::repository::load_and_connect_with_token(
        path,
        RepositoryAccess::NoStore,
        None,
    )
    .await
    .expect("the instance opens without its stores");
    let planned = lore_revision::instance::instance_clone_plan(
        unlocked.clone(),
        &destination.display().to_string(),
        allow_copy,
    )
    .await;
    drop(unlocked);
    let Some(plan) = planned? else {
        return Ok(None);
    };
    let token = RepositoryWriteToken::acquire(path).await;
    let repository = lore_revision::repository::load_and_connect_exclusive(path, token.share())
        .await
        .expect("the instance opens holding its lock alone");
    let cloned = clone(repository, &plan).await;
    drop(token);
    let clone_id = plan.clone_id();
    lore_revision::instance::instance_clone_finish(plan).await;
    cloned.map(|()| Some(clone_id))
}

type CommandContext = Arc<lore_revision::repository::RepositoryContext>;
type ClonePlan = lore_revision::instance::InstanceClonePlan;
type InstanceError = lore_revision::instance::InstanceError;

/// Whether the volume of the external directories clones files, so cloning a virtual clone
/// shares blocks rather than copying them.
fn external_dirs_clone_files() -> bool {
    let root = lore_revision::global::external_dir::external_dirs_root().expect("external root");
    std::fs::create_dir_all(&root).expect("external root");
    lore_io::clone::detect(&root, &root)
        .expect("probe the external directories' volume")
        .backend
        .shares_blocks()
}

/// Cloning a virtual clone mounts a new instance at the destination, on the source's revision,
/// with a copy of its local changes: the files written through the mount, and the deletions. The
/// tracked files report the times the source's sync recorded, and a write in the new instance
/// leaves the source alone. The new instance is registered at the destination with an ID of its
/// own, and its external directory holds no mark of an instance being cloned.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_clone_of_a_fuse_instance_mounts_its_revision_and_local_changes() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-clone-source-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-clone-target-");
        let destination = target_parent.join("clone");
        let _guard = MountGuard(path.clone());
        let _clone_guard = MountGuard(destination.clone());
        let source_id = synced_fuse_instance(
            &store,
            &path,
            &[
                ("readme.txt", b"tracked"),
                ("gone.txt", b"deleted through the mount"),
                ("src/main.rs", b"fn main() {}"),
            ],
        )
        .await;
        write_through(path.join("local.txt"), b"written before cloning").await;
        remove_through(path.join("gone.txt")).await;
        let base_time = load_repository_config_from_dot_dir(
            &external_dir_for_instance(source_id)
                .expect("external dir")
                .join(DOT_LORE),
        )
        .expect("the source's config")
        .vfs_base_time();
        let swapped = mtime_of(path.join(SWAPPED)).await;
        assert_ne!(
            swapped,
            std::time::UNIX_EPOCH + Duration::from_millis(base_time),
            "the second sync recorded a time of its own"
        );

        let clone_id = clone_instance_at(
            &path,
            &destination,
            true,
            async |repository: CommandContext, plan: &ClonePlan| {
                lore_revision::instance::instance_clone(repository, plan).await
            },
        )
        .await
        .expect("cloning succeeds")
        .expect("not a dry run");

        assert!(is_mounted(&destination), "the new instance is mounted");
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let mounted = manager.fuse().mounted(&destination).expect("and tracked");
        assert_eq!(mounted.instance_id, clone_id);
        assert_ne!(clone_id, source_id, "the new instance has an ID of its own");
        assert_eq!(
            list_names(&destination).await,
            vec!["local.txt", "readme.txt", "src", SWAPPED]
        );
        assert_eq!(read_file(destination.join("readme.txt")).await, b"tracked");
        assert_eq!(
            read_file(destination.join("local.txt")).await,
            b"written before cloning"
        );
        assert_eq!(
            mtime_of(destination.join(SWAPPED)).await,
            swapped,
            "the new instance reports the time the source's sync recorded"
        );
        let clone_external = external_dir_for_instance(clone_id).expect("external dir");
        assert!(
            clone_external.join(DOT_LORE).is_dir(),
            "the new instance keeps its external directory"
        );
        assert!(
            !clone_external
                .join(DOT_LORE)
                .join(lore_revision::instance::CLONE_IN_PROGRESS_MARK)
                .exists(),
            "the new instance is no longer marked as being cloned"
        );

        write_through(
            destination.join("local.txt"),
            b"rewritten in the new instance",
        )
        .await;
        assert_eq!(
            read_file(path.join("local.txt")).await,
            b"written before cloning",
            "the source keeps its own copy"
        );
        let registered = load_and_connect(&destination, RepositoryAccess::ReadOnly)
            .await
            .expect("the new instance opens at its destination");
        assert_eq!(registered.instance_id, clone_id);
        let instances = lore_revision::instance::list_instances(&registered)
            .await
            .expect("list the instances");
        assert!(
            instances
                .iter()
                .any(|instance| instance.instance_id == clone_id
                    && Path::new(&instance.path) == destination),
            "the new instance is registered at its destination: {instances:?}"
        );
        drop(registered);
        for mounted in [&destination, &path] {
            manager
                .fuse()
                .unmount(mounted, false)
                .await
                .expect("unmounts");
        }
    }))
    .await;
}

/// Cloning a virtual clone whose registration fails undoes everything: nothing is mounted at the
/// destination, which is not made, the new instance is not registered, and its external directory
/// is removed once cloning is finished.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_clone_of_a_fuse_instance_that_fails_to_register_leaves_nothing() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-clone-source-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-clone-target-");
        let destination = target_parent.join("clone");
        let _guard = MountGuard(path.clone());
        let _clone_guard = MountGuard(destination.clone());
        synced_fuse_instance(&store, &path, &[("readme.txt", b"tracked")]).await;
        write_through(path.join("local.txt"), b"local").await;
        let registered = instances_registered_beside(&path).await;
        let before = names_in(
            &lore_revision::global::external_dir::external_dirs_root().expect("external root"),
        );

        let failed = clone_instance_at(
            &path,
            &destination,
            true,
            async |repository: CommandContext, plan: &ClonePlan| {
                let failed = lore_revision::instance::instance_clone_with(
                    repository,
                    plan,
                    lore_revision::instance::move_clone_into_place,
                    async |_: &CommandContext, _: InstanceId, _: &str| {
                        Err(InstanceError::internal("refused"))
                    },
                )
                .await;
                assert!(
                    plan.in_progress().is_dir(),
                    "the external directory stays until cloning is finished"
                );
                failed
            },
        )
        .await;

        assert!(failed.is_err(), "cloning fails");
        assert!(!is_mounted(&destination), "nothing is mounted");
        assert!(!destination.exists(), "no mount point is made");
        assert_eq!(
            names_in(
                &lore_revision::global::external_dir::external_dirs_root().expect("external root")
            ),
            before,
            "the new instance's external directory is removed"
        );
        assert_eq!(
            instances_registered_beside(&path).await,
            registered,
            "the new instance is not registered"
        );
        assert_eq!(read_file(path.join("local.txt")).await, b"local");
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// Cloning a virtual clone to a destination another takes after the plan, as a second cloning to
/// the same path does, is refused as a taken destination before it registers, so it retires no
/// registration there, and leaves the directory found there alone.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_clone_whose_destination_is_taken_is_refused_before_it_registers() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-clone-source-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-clone-target-");
        let destination = target_parent.join("clone");
        let _guard = MountGuard(path.clone());
        let _clone_guard = MountGuard(destination.clone());
        synced_fuse_instance(&store, &path, &[("readme.txt", b"tracked")]).await;
        let registered = instances_registered_beside(&path).await;
        let before = names_in(
            &lore_revision::global::external_dir::external_dirs_root().expect("external root"),
        );

        let taken = destination.clone();
        let registering = std::sync::atomic::AtomicBool::new(false);
        let failed = clone_instance_at(
            &path,
            &destination,
            true,
            async |repository: CommandContext, plan: &ClonePlan| {
                std::fs::create_dir(&taken).expect("take the destination");
                lore_revision::instance::instance_clone_with(
                    repository,
                    plan,
                    lore_revision::instance::move_clone_into_place,
                    async |repository: &CommandContext, clone: InstanceId, target: &str| {
                        registering.store(true, Ordering::SeqCst);
                        lore_revision::instance::register_clone(repository, clone, target).await
                    },
                )
                .await
            },
        )
        .await;

        let error = failed.expect_err("cloning fails");
        assert!(error.is_invalid_arguments(), "unexpected error: {error}");
        assert!(
            !registering.load(Ordering::SeqCst),
            "cloning is refused before it registers"
        );
        assert!(!is_mounted(&destination), "nothing is mounted");
        assert!(
            destination.is_dir() && names_in(&destination).is_empty(),
            "the directory found there stays"
        );
        assert_eq!(instances_registered_beside(&path).await, registered);
        assert_eq!(
            names_in(
                &lore_revision::global::external_dir::external_dirs_root().expect("external root")
            ),
            before,
            "the new instance's external directory is removed"
        );
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// Cloning a virtual clone whose mount a stop refuses removes the mount point it made and the new
/// instance's registration, and its external directory once cloning is finished.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_clone_of_a_fuse_instance_whose_mount_is_refused_leaves_nothing() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-clone-source-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-clone-target-");
        let destination = target_parent.join("clone");
        let _guard = MountGuard(path.clone());
        let _clone_guard = MountGuard(destination.clone());
        let source_id = synced_fuse_instance(&store, &path, &[("readme.txt", b"tracked")]).await;
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());
        let before = names_in(
            &lore_revision::global::external_dir::external_dirs_root().expect("external root"),
        );

        let stopping = manager.clone();
        let failed = clone_instance_at(
            &path,
            &destination,
            true,
            async |repository: CommandContext, plan: &ClonePlan| {
                stopping
                    .fuse()
                    .unmount_all(false)
                    .await
                    .expect("the stop goes ahead");
                lore_revision::instance::instance_clone(repository, plan).await
            },
        )
        .await;

        assert!(failed.is_err(), "cloning fails");
        assert!(!is_mounted(&destination), "nothing is mounted");
        assert!(!destination.exists(), "the mount point it made is removed");
        assert_eq!(
            names_in(
                &lore_revision::global::external_dir::external_dirs_root().expect("external root")
            ),
            before,
            "the new instance's external directory is removed"
        );
        assert!(
            external_dir_for_instance(source_id)
                .expect("external dir")
                .is_dir(),
            "the source keeps its external directory"
        );
    }))
    .await;
}

/// Cloning a virtual clone onto a volume that cannot clone files is refused without a copy
/// allowed, before anything is written.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_clone_of_a_fuse_instance_that_would_copy_needs_a_copy_allowed() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        if external_dirs_clone_files() {
            return;
        }
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-clone-source-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-clone-target-");
        let destination = target_parent.join("clone");
        let _guard = MountGuard(path.clone());
        synced_fuse_instance(&store, &path, &[("readme.txt", b"tracked")]).await;
        let before = names_in(
            &lore_revision::global::external_dir::external_dirs_root().expect("external root"),
        );

        let refused = clone_instance_at(
            &path,
            &destination,
            false,
            async |_: CommandContext, _: &ClonePlan| -> Result<(), InstanceError> {
                panic!("a refused clone is not made")
            },
        )
        .await;

        let error = refused.expect_err("cloning is refused");
        assert!(error.is_not_supported(), "unexpected error: {error}");
        assert!(!destination.exists());
        assert_eq!(
            names_in(
                &lore_revision::global::external_dir::external_dirs_root().expect("external root")
            ),
            before,
            "no external directory is written"
        );
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .unmount(&path, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// A start removes what a crash left while cloning among the external directories: a marked
/// external directory no instance is registered for, with the empty mount point its mark names,
/// one a removal renamed aside, and a lock file no clone holds. It keeps a marked one whose
/// instance is registered, removing only its mark, one whose lock another clone holds, and every
/// other entry. Where some instances are unknown it removes no marked directory.
#[tokio::test(flavor = "multi_thread")]
// Fixtures written and cleaned up beside the new instances, outside any repository write.
#[allow(clippy::disallowed_methods)]
async fn a_start_removes_what_interrupted_clones_left_and_keeps_registered_clones() {
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        use lore_base::fs::lock::FSLock;
        use lore_revision::fs::fuse::mount_manager::remove_interrupted_clones;
        use lore_revision::instance::CLONE_IN_PROGRESS_MARK;

        let root = lore_revision::global::external_dir::external_dirs_root().expect("root");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-sweep-target-");
        let marked = |id: InstanceId, names: &Path| {
            let dot = root.join(id.text_encoding()).join(DOT_LORE);
            std::fs::create_dir_all(&dot).expect("a Lore directory");
            test_file_write(
                &dot.join(CLONE_IN_PROGRESS_MARK),
                names.as_os_str().as_encoded_bytes(),
            );
            test_file_write(&root.join(id.text_encoding()).join("deleted"), b"a\n");
        };
        let (unregistered, registered, held, unmarked, stray) = (
            InstanceId::generate(),
            InstanceId::generate(),
            InstanceId::generate(),
            InstanceId::generate(),
            InstanceId::generate(),
        );
        let mount_point = target_parent.join("made");
        std::fs::create_dir(&mount_point).expect("the mount point cloning made");
        let filled = target_parent.join("filled");
        std::fs::create_dir(&filled).expect("a directory holding a file");
        test_file_write(&filled.join("kept.txt"), b"kept");
        marked(unregistered, &mount_point);
        marked(registered, &target_parent.join("registered"));
        marked(held, &filled);
        let held_lock = FSLock::try_acquire_file_lock(root.join(held.text_encoding()))
            .expect("lock")
            .expect("the lock of an instance still being cloned");
        let unmarked_dot = root.join(unmarked.text_encoding()).join(DOT_LORE);
        std::fs::create_dir_all(&unmarked_dot).expect("a Lore directory");
        let aside = root.join(format!(
            "{}.removing",
            InstanceId::generate().text_encoding()
        ));
        std::fs::create_dir_all(aside.join("write")).expect("a removal cut short");
        let stray_lock = root.join(format!("{}.lock", stray.text_encoding()));
        test_file_write(&stray_lock, b"");
        let other = root.join("not-an-instance.removing");
        std::fs::create_dir_all(&other).expect("another entry");

        remove_interrupted_clones(None).await;
        assert!(
            root.join(unregistered.text_encoding()).exists(),
            "with instances unknown, no marked directory is removed"
        );
        assert!(mount_point.is_dir());
        assert!(!aside.exists(), "a removal cut short is finished");
        assert!(
            !stray_lock.exists(),
            "a lock file no clone holds is removed"
        );

        remove_interrupted_clones(Some(&std::collections::BTreeSet::from([registered]))).await;
        assert!(!root.join(unregistered.text_encoding()).exists());
        assert!(
            !mount_point.exists(),
            "with the empty mount point its mark names"
        );
        let kept = root.join(registered.text_encoding());
        assert!(kept.join("deleted").is_file(), "a registered clone stays");
        assert!(!kept.join(DOT_LORE).join(CLONE_IN_PROGRESS_MARK).exists());
        assert!(
            root.join(held.text_encoding()).join("deleted").is_file(),
            "an instance whose lock another holds stays"
        );
        assert!(filled.join("kept.txt").is_file());
        assert!(unmarked_dot.is_dir(), "an unmarked directory stays");
        assert!(
            !root
                .join(format!("{}.lock", unmarked.text_encoding()))
                .exists(),
            "and gets no lock file"
        );
        assert!(other.is_dir(), "an entry no clone names stays");
        drop(held_lock);
        for left in [
            kept,
            root.join(held.text_encoding()),
            root.join(unmarked.text_encoding()),
            other,
        ] {
            std::fs::remove_dir_all(left).expect("clean up");
        }
        let _ = std::fs::remove_file(root.join(format!("{}.lock", held.text_encoding())));
    }))
    .await;
}
