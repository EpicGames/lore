// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use super::*;

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
            match lore_revision::global::external_dir::resolve::resolve()
                .expect("the external root resolves")
                .lookup(instance_id)
            {
                lore_revision::global::external_dir::resolve::Lookup::Present(dot_lore) => {
                    Some(dot_lore)
                }
                lookup => panic!("the instance's external directory is {lookup:?}"),
            },
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

        let dot_lore = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            instance_id,
        )
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

/// **The contexts over a FUSE instance's in-memory stores carry the records cached beside them**:
/// the one `create_local` returns once the instance is mounted, and the one that serves the mount
/// once it moves, as every context over the stores does.
#[tokio::test(flavor = "multi_thread")]
async fn contexts_over_an_in_memory_fuse_instance_carry_its_cached_records() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    service_execution().await;
    let execution = helper::test_execution_with(lore_revision::interface::LoreGlobalArgs {
        in_memory: 1,
        ..lore_revision::interface::LoreGlobalArgs::default()
    });
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-in-memory-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-in-memory-target-");
        let target = target_parent.join("moved");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());

        let (repository, _token) = create_instance(
            &path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        let dot_dir = repository
            .dot_dir_path()
            .expect("the instance has a Lore directory")
            .to_path_buf();
        let cached = lore_revision::repository::get_cached_in_memory_stores(&dot_dir)
            .expect("the instance's stores are cached");
        assert!(Arc::ptr_eq(
            repository
                .instance_records()
                .expect("the context carries the instance's records"),
            &cached.records
        ));

        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager
            .fuse()
            .move_mount(repository.instance_id, &path, &target)
            .await
            .expect("the mount moves");
        assert!(Arc::ptr_eq(
            served_repository(&manager, &target)
                .instance_records()
                .expect("the moved mount's context carries the instance's records"),
            &cached.records
        ));

        manager
            .fuse()
            .unmount(&target, false)
            .await
            .expect("an idle mount unmounts");
        lore_revision::repository::repository_release(
            dot_dir.parent().expect("the Lore directory has a parent"),
        );
    }))
    .await;
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
        let write = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            repository.instance_id,
        )
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
                (
                    &dot_lore_of(&repository),
                    lore_revision::global::external_dir::canonical_global_data_dir()
                        .expect("the global data directory"),
                ),
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
                .copy_children_of(&[ROOT_INO], &mut looked_up);
            let entered: Vec<String> = looked_up
                .iter()
                .map(|(_, name)| String::from_utf8_lossy(name).into_owned())
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
                (
                    &dot_lore_of(&repository),
                    lore_revision::global::external_dir::canonical_global_data_dir()
                        .expect("the global data directory"),
                ),
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
                (
                    &dot_lore_of(&repository),
                    lore_revision::global::external_dir::canonical_global_data_dir()
                        .expect("the global data directory"),
                ),
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

/// A refused stop retries its busy mounts for one busy window in all, not for one window each.
#[tokio::test(flavor = "multi_thread")]
async fn unmount_all_retries_its_busy_mounts_for_one_window_in_all() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_first_dir, first_path) = canonical_dir("lore-fuse-mount-busy-");
        let (_second_dir, second_path) = canonical_dir("lore-fuse-mount-busy-");
        let _first_guard = MountGuard(first_path.clone());
        let _second_guard = MountGuard(second_path.clone());
        let _first = fresh_fuse_instance(&store, &first_path).await;
        let _second = fresh_fuse_instance(&store, &second_path).await;
        let window = Duration::from_secs(1);
        let first_holder = Holder::open_for(&first_path, window).await;
        let second_holder = Holder::open_for(&second_path, window).await;
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let _resume = ResumeMounting(manager.clone());

        let started = std::time::Instant::now();
        let error = manager
            .fuse()
            .unmount_all(false)
            .await
            .expect_err("busy mounts fail a plain unmount_all");
        let waited = started.elapsed();

        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(
            waited < window + window / 2,
            "the stop must retry for one window in all, not {waited:?}"
        );
        first_holder.close().await;
        second_holder.close().await;
        manager
            .fuse()
            .unmount_all(false)
            .await
            .expect("once idle, every mount unmounts");
    }))
    .await;
}

/// A plain unmount that waits for a held mount holds none of the core runtime's blocking threads
/// between its attempts, so the service's other commands, which read their requests and write
/// their events on those threads, go on meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_unmount_leaves_the_blocking_threads_free() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-busy-");
        let _guard = MountGuard(path.clone());
        let _instance = fresh_fuse_instance(&store, &path).await;
        let holder = Holder::open_for(&path, Duration::from_secs(2)).await;
        // Every blocking thread but one stays parked, as the service's accept loop parks one.
        let mut releases = Vec::new();
        let blocking_threads =
            lore_base::runtime::core_blocking_threads(lore_base::runtime::thread_counts());
        for _ in 1..blocking_threads {
            let (parked, ready) = tokio::sync::oneshot::channel::<()>();
            let (release, released) = std::sync::mpsc::channel::<()>();
            drop(lore_base::lore_spawn_blocking!(move || {
                let _ = parked.send(());
                let _ = released.recv();
            }));
            ready.await.expect("a blocking thread parks");
            releases.push(release);
        }
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let unmounting = {
            let manager = manager.clone();
            let path = path.clone();
            lore_base::lore_spawn!(async move { manager.fuse().unmount(&path, false).await })
        };
        // Within the window, after the first attempt has found the mount busy.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let probed = std::time::Instant::now();
        lore_base::lore_spawn_blocking!(|| ())
            .await
            .expect("the probe runs");
        let probe = probed.elapsed();

        drop(releases);
        holder.close().await;
        let unmounted = unmounting.await.expect("the unmount task completes");
        assert!(
            probe < Duration::from_millis(500),
            "a blocking task waited {probe:?} behind the unmount's wait"
        );
        assert!(
            unmounted.is_ok(),
            "the unmount must succeed once the holder lets go: {unmounted:?}"
        );
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

        let external_root = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            InstanceId::generate(),
        )
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

/// Creating a FUSE instance while `external-root` names a missing root fails, and makes neither
/// the root, nor its parent, nor anything at the default location.
#[tokio::test(flavor = "multi_thread")]
async fn creating_a_fuse_instance_makes_no_missing_configured_root() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    service_execution().await;
    let global = TempDir::new("lore-fuse-mount-missing-root-");
    let data = global.path().join("data");
    std::fs::create_dir(&data).expect("the global data directory is made");
    let volume = global.path().join("volume");
    Setting {
        path: volume.join("root").display().to_string(),
        id: RootId::generate(),
        generation: NonZeroU64::MIN,
    }
    .save(&data)
    .await
    .expect("the setting is written");
    Box::pin(LORE_CONTEXT.scope(execution_in(global.path()), async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-missing-root-");
        let _guard = MountGuard(path.clone());

        let error = clone_fails(&store, &path)
            .await
            .expect("creating with a missing root fails");

        assert!(error.is_internal(), "{error}");
        assert!(!is_mounted(&path), "nothing is mounted");
        assert!(!volume.exists(), "the root and its parent aren't made");
        assert!(
            !data.join("external").exists(),
            "nothing is made at the default location"
        );
    }))
    .await;
}

/// Creating a FUSE instance while a move of the external root holds exclusive access fails as
/// internal before anything is written: nothing is mounted, the mount point stays empty, and no
/// external directory is made.
#[tokio::test(flavor = "multi_thread")]
async fn creating_a_fuse_instance_is_refused_while_a_move_holds_the_external_root() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    service_execution().await;
    let global = TempDir::new("lore-fuse-mount-held-root-");
    let data = global.path().join("data");
    std::fs::create_dir(&data).expect("the global data directory is made");
    assert!(
        lore_revision::global::external_dir::access::try_exclusive(&data).expect("attempt"),
        "exclusive access is taken"
    );
    Box::pin(LORE_CONTEXT.scope(execution_in(global.path()), async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-held-root-");
        let _guard = MountGuard(path.clone());

        let error = clone_fails(&store, &path)
            .await
            .expect("creating while a move holds the root fails");

        assert!(error.is_internal(), "{error}");
        assert!(!is_mounted(&path), "nothing is mounted");
        assert_eq!(
            std::fs::read_dir(&path)
                .expect("list the mount point")
                .count(),
            0,
            "nothing is written at the path"
        );
        assert!(
            !data.join("external").exists(),
            "no external directory is made"
        );
    }))
    .await;
}

/// With an available root other than the default location, a FUSE instance is created in that
/// root, mounted from it, moved with its external directory left there, and mounted from it again,
/// and nothing of it is made at the default location.
#[tokio::test(flavor = "multi_thread")]
async fn a_fuse_instance_lives_in_a_configured_root() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    service_execution().await;
    let global = TempDir::new("lore-fuse-mount-configured-root-");
    let data = global.path().join("data");
    std::fs::create_dir(&data).expect("the global data directory is made");
    let root = global.path().join("root");
    lore_io::create_dir_all_private(&root).expect("the root is made");
    let id = RootId::generate();
    lore_revision::global::external_dir::marker::Marker {
        id,
        generation: NonZeroU64::MIN,
    }
    .save(&root)
    .await
    .expect("the marker is written");
    Setting {
        path: root.display().to_string(),
        id,
        generation: NonZeroU64::MIN,
    }
    .save(&data)
    .await
    .expect("the setting is written");
    Box::pin(LORE_CONTEXT.scope(execution_in(global.path()), async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-configured-root-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-configured-target-");
        let target = target_parent.join("moved");
        let _guard = MountGuard(path.clone());
        let _target_guard = MountGuard(target.clone());

        let (repository, _token) = create_instance(
            &path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;
        let instance_id = repository.instance_id;
        let external = root.join(instance_id.text_encoding().as_str());
        assert!(external.join(DOT_LORE).is_dir(), "created in the root");
        assert!(
            !data.join("external").exists(),
            "nothing is made at the default location"
        );
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        let recorded = |at: &Path| {
            manager
                .record_at(at)
                .expect("the path makes a mount path")
                .map(|record| record.external_dir.clone())
        };
        assert_eq!(
            recorded(&path),
            Some(external.clone()),
            "mounted from the root"
        );
        write_through(path.join("local.txt"), b"written in the root").await;
        assert!(
            external.join("write").join("local.txt").is_file(),
            "the write lands in the root"
        );

        manager
            .fuse()
            .move_mount(instance_id, &path, &target)
            .await
            .expect("the mount moves");
        assert_eq!(
            recorded(&target),
            Some(external.clone()),
            "moved over the root"
        );
        assert_eq!(
            read_file(target.join("local.txt")).await,
            b"written in the root"
        );

        manager
            .fuse()
            .unmount(&target, false)
            .await
            .expect("unmounts");
        let state = lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root");
        let lookup = state.lookup(instance_id);
        let lore_revision::global::external_dir::resolve::Lookup::Present(dot_lore) = lookup else {
            panic!("the instance's external directory is {lookup:?}");
        };
        let config = load_repository_config_from_dot_dir(&dot_lore).expect("config loads");
        manager
            .fuse()
            .remount_instance(
                repository.clone(),
                &instance_metadata(&repository, &target),
                (
                    &dot_lore,
                    lore_revision::global::external_dir::canonical_global(&state.global)
                        .expect("the global data directory"),
                ),
                &config,
            )
            .await
            .expect("mounts again from the root");
        assert_eq!(recorded(&target), Some(external.clone()));
        assert_eq!(
            read_file(target.join("local.txt")).await,
            b"written in the root"
        );
        manager
            .fuse()
            .unmount(&target, false)
            .await
            .expect("unmounts");
    }))
    .await;
}

/// With no root ID stored, creating the first FUSE instance makes the default root, `external/` in
/// the global data directory, with the instance's external directory in it.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_fuse_instance_with_no_root_id_makes_the_default_root() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    service_execution().await;
    let global = TempDir::new("lore-fuse-mount-default-root-");
    Box::pin(LORE_CONTEXT.scope(execution_in(global.path()), async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-default-root-");
        let _guard = MountGuard(path.clone());
        let default_root = global.path().join("data").join("external");
        assert!(!default_root.exists(), "no default root before the first");

        let (repository, _token) = create_instance(
            &path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            shared_store_config(&store, VfsType::Fuse),
        )
        .await;

        assert!(is_mounted(&path), "the instance is mounted");
        assert!(
            default_root
                .join(repository.instance_id.text_encoding().as_str())
                .join(DOT_LORE)
                .is_dir(),
            "the instance's external directory is in the default root"
        );
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
                (
                    &dot_lore_of(&repository),
                    lore_revision::global::external_dir::canonical_global_data_dir()
                        .expect("the global data directory"),
                ),
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
        let external = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            instance.instance_id,
        )
        .expect("external dir");

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
        let external = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            instance.instance_id,
        )
        .expect("external dir");
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
        let external = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            instance.instance_id,
        )
        .expect("external dir");
        let instance_id = instance.instance_id;
        let holder = Holder::open(&path).await;
        let command = opened_as_a_command(instance, &path).await;

        let error = lore_revision::instance::instance_delete(command.clone())
            .await
            .expect_err("a busy instance refuses the delete");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        let error =
            lore_revision::instance::instance_move(command.clone(), &target.display().to_string())
                .await
                .expect_err("a busy instance refuses the move");
        assert!(error.is_mounts_busy(), "unexpected error: {error}");
        assert!(is_mounted(&path), "still mounted");
        assert!(external.exists(), "with its external directory");
        assert!(!target.exists(), "and nothing at the target");
        let registered = load_and_connect(&path, RepositoryAccess::ReadOnly)
            .await
            .expect("the instance opens");
        let instances = lore_revision::instance::list_instances(&registered)
            .await
            .expect("list the instances");
        drop(registered);
        assert!(
            instances
                .iter()
                .any(|registered| registered.instance_id == instance_id
                    && Path::new(&registered.path) == path),
            "and registered at its path: {instances:?}"
        );
        holder.close().await;
        lore_revision::instance::instance_delete(command)
            .await
            .expect("once released, the instance is deleted");
        assert!(!is_mounted(&path), "unmounted");
        assert!(!external.exists(), "its external directory is removed");
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
                (
                    &dot_lore_of(&repository),
                    lore_revision::global::external_dir::canonical_global_data_dir()
                        .expect("the global data directory"),
                ),
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

        let data = lore_revision::global::get_global_data_dir().expect("the global data directory");
        let before = reads_in(&data);
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
            reads_in(&data).setting,
            before.setting + 2,
            "the start resolves the external root once, and once under shared access"
        );
        assert_eq!(
            restarted
                .record_at(&path)
                .expect("the path makes a mount path")
                .map(|record| record.global.clone()),
            Some(std::sync::Arc::from(
                std::fs::canonicalize(&data).expect("canonicalize the global data directory")
            )),
            "the mount records the global data directory it was resolved for"
        );
        let before = reads_in(&data);
        restarted
            .check_for_external_lore_dir(&path)
            .expect("the lookup succeeds")
            .expect("the mount leads to the external directory");
        assert_eq!(
            reads_in(&data),
            before,
            "a mounted path's lookup reads no root file"
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
        let external = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            instance_id,
        )
        .expect("external dir");
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
            .args(["--exact", "needs_fuse::initialize_child", "--nocapture"])
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
            &external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                source_id,
            )
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
        let clone_external = external_dir_for_instance(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
            clone_id,
        )
        .expect("external dir");
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
            &lore_revision::global::external_dir::external_dirs_root(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .map(std::path::Path::to_path_buf)
            .expect("external root"),
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
                &lore_revision::global::external_dir::external_dirs_root(
                    &lore_revision::global::external_dir::resolve::resolve()
                        .expect("resolve the external root")
                )
                .map(std::path::Path::to_path_buf)
                .expect("external root")
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
            &lore_revision::global::external_dir::external_dirs_root(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .map(std::path::Path::to_path_buf)
            .expect("external root"),
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
                &lore_revision::global::external_dir::external_dirs_root(
                    &lore_revision::global::external_dir::resolve::resolve()
                        .expect("resolve the external root")
                )
                .map(std::path::Path::to_path_buf)
                .expect("external root")
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
            &lore_revision::global::external_dir::external_dirs_root(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .map(std::path::Path::to_path_buf)
            .expect("external root"),
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
                &lore_revision::global::external_dir::external_dirs_root(
                    &lore_revision::global::external_dir::resolve::resolve()
                        .expect("resolve the external root")
                )
                .map(std::path::Path::to_path_buf)
                .expect("external root")
            ),
            before,
            "the new instance's external directory is removed"
        );
        assert!(
            external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                source_id
            )
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
            &lore_revision::global::external_dir::external_dirs_root(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
            )
            .map(std::path::Path::to_path_buf)
            .expect("external root"),
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
                &lore_revision::global::external_dir::external_dirs_root(
                    &lore_revision::global::external_dir::resolve::resolve()
                        .expect("resolve the external root")
                )
                .map(std::path::Path::to_path_buf)
                .expect("external root")
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

/// Releases, when dropped, the exclusive access a test took to the external root of the global
/// data directory it holds, so a failing test leaves the binary's later tests their mounts.
struct ReleasesExclusive(std::path::PathBuf);

impl Drop for ReleasesExclusive {
    fn drop(&mut self) {
        let _ = lore_revision::global::external_dir::access::release_exclusive(&self.0);
    }
}

/// Cloning a FUSE instance while a move of the external root holds exclusive access is refused
/// as internal before it writes anything: no destination and no external directory. Once the
/// access is shared again, the instance is cloned.
#[tokio::test(flavor = "multi_thread")]
async fn cloning_a_fuse_instance_is_refused_while_a_move_holds_the_external_root() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        use lore_revision::global::external_dir::access::try_exclusive;

        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-clone-source-");
        let (_target_dir, target_parent) = canonical_dir("lore-fuse-mount-clone-target-");
        let destination = target_parent.join("clone");
        let _guard = MountGuard(path.clone());
        let _clone_guard = MountGuard(destination.clone());
        synced_fuse_instance(&store, &path, &[("readme.txt", b"tracked")]).await;
        let root = lore_revision::global::external_dir::external_dirs_root(
            &lore_revision::global::external_dir::resolve::resolve()
                .expect("resolve the external root"),
        )
        .map(std::path::Path::to_path_buf)
        .expect("external root");
        let before = names_in(&root);
        let data = lore_revision::global::get_global_data_dir().expect("the global data directory");
        assert!(try_exclusive(&data).expect("attempt"));
        let exclusive = ReleasesExclusive(data);

        let refused = clone_instance_at(
            &path,
            &destination,
            true,
            async |_: CommandContext, _: &ClonePlan| -> Result<(), InstanceError> {
                panic!("a refused call clones nothing")
            },
        )
        .await;
        drop(exclusive);

        let error = refused.expect_err("cloning is refused");
        assert!(error.is_internal(), "unexpected error: {error}");
        assert!(!destination.exists());
        assert_eq!(names_in(&root), before, "no external directory is written");
        let cloned = clone_instance_at(
            &path,
            &destination,
            true,
            async |repository: CommandContext, plan: &ClonePlan| {
                lore_revision::instance::instance_clone(repository, plan).await
            },
        )
        .await
        .expect("cloning succeeds");
        assert!(cloned.is_some() && is_mounted(&destination));
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
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

/// After a warm-up, the service's lookup of a mounted FUSE instance's path answers its external
/// `.lore` directory within [`LOOKUP_ALLOCATION_BUDGET`].
#[tokio::test(flavor = "multi_thread")]
async fn the_lookup_of_a_mounted_path_stays_within_its_budget() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-lookup-allocations-store-");
        let (_dir, path) = canonical_dir("lore-fuse-lookup-allocations-");
        let _guard = MountGuard(path.clone());
        let repository = fresh_fuse_instance(&store, &path).await;
        let state = lore_revision::global::external_dir::resolve::resolve()
            .expect("resolve the external root");
        let expected = external_dir_for_instance(&state, repository.instance_id)
            .expect("the external directory")
            .join(DOT_LORE);
        assert_eq!(
            get_dot_lore_path(&path).expect("the warm-up lookup"),
            expected
        );

        lore_base::allocator::set_allocation_counting(true);
        let restore = DisableCountingOnDrop;
        let before = lore_base::allocator::thread_allocation_count();
        let found = get_dot_lore_path(&path);
        let allocations = lore_base::allocator::thread_allocation_count() - before;
        drop(restore);

        assert_eq!(found.expect("the lookup"), expected);
        assert!(
            allocations <= LOOKUP_ALLOCATION_BUDGET,
            "the lookup made {allocations} allocations, over its budget of \
             {LOOKUP_ALLOCATION_BUDGET}"
        );
        drop(repository);
    }))
    .await;
}

const RECOVERY_CHILD_ENV: &str = "LORE_FUSE_MOUNT_TEST_RECOVERY_CHILD";

/// Writes a log message of the recovery child to its stderr, which its parent reports on failure.
fn log_to_stderr(level: lore_base::log::LoreLogLevel, location: &str, message: &str) {
    eprintln!("[{level:?}] {location}: {message}");
}

/// A first move interrupted at its switch, after the root was renamed, with a store of its FUSE
/// instance held at start by another holder: the start leaves the move waiting and the instance
/// unmounted, and once the holder lets go, the background recovery joins a command's hold in this
/// process, finishes the move and mounts the instance into the running manager, from the moved
/// root, while the command still holds the store.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_child() {
    use lore_base::fs::lock::FSLock;
    use lore_revision::global::external_dir::marker::Marker;
    use lore_revision::global::external_dir::record::MoveKind;
    use lore_revision::global::external_dir::record::MovePhase;
    use lore_revision::global::external_dir::record::MoveRecord;
    use lore_revision::global::external_dir::recovery::AtStart;
    use lore_revision::global::external_dir::recovery::recover_at_start;
    use lore_revision::global::external_dir::recovery::recover_in_background;
    use lore_storage::local::store_lock::Intent;

    if std::env::var_os(RECOVERY_CHILD_ENV).is_none() {
        return;
    }
    skip_without_fuse!();
    lore_base::log::set_log_level(lore_base::log::LoreLogLevel::Debug);
    lore_base::log::set_log_callback(Some(log_to_stderr));
    let _serial = SERIAL.lock().await;
    let execution = service_execution().await;
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let (_store_dir, store) = canonical_dir("lore-fuse-mount-store-");
        let (_dir, path) = canonical_dir("lore-fuse-mount-recovery-");
        eprintln!("recovery child: creating the instance");
        let _guard = MountGuard(path.clone());
        let config = shared_store_config(&store, VfsType::Fuse);
        let (repository, token) = create_instance(
            &path,
            RepositoryId::from(uuid::Uuid::now_v7()),
            config.clone(),
        )
        .await;
        let instance_id = repository.instance_id;
        repository
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        drop(repository);
        drop(token);
        let manager = MountManagerState::mount_manager().expect("manager is initialised");
        manager.fuse().unmount_all(false).await.expect("unmounts");
        manager.fuse().resume_mounting();
        assert!(!is_mounted(&path));

        let global =
            lore_revision::global::get_global_data_dir().expect("the global data directory");
        let default = global.join("external");
        let (_volume, volume) = canonical_dir("lore-fuse-mount-volume-");
        let target = volume.join("root");
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
        // A fixture rename of the root, as a move does, outside any repository write.
        #[allow(clippy::disallowed_methods)]
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
        let shared = lore_revision::shared_store::locate_shared_store_for_repo(&config)
            .await
            .expect("find the shared store")
            .expect("the config names one");
        let other = FSLock::try_acquire_exact_path(&shared.join("immutable").join("lock"))
            .await
            .expect("look at the flock")
            .expect("hold the store");

        eprintln!("recovery child: recovering at the start");
        assert_eq!(
            recover_at_start(&global, std::time::Duration::ZERO).await,
            AtStart::Retry
        );
        eprintln!("recovery child: mounting while the move waits");
        Box::pin(manager.mount_registered_instances())
            .await
            .expect("the start's mounting skips the waiting root");
        assert!(!is_mounted(&path), "nothing mounts while the move waits");

        drop(other);
        let command = StoreLock::new(shared.join("immutable"), true).expect("the lock");
        let mut held = command
            .acquire(Intent::Read)
            .await
            .expect("a command holds the store");
        held.note_refreshed();
        eprintln!("recovery child: recovering in the background");
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            recover_in_background(global.clone(), async {
                eprintln!("recovery child: mounting the recovered root");
                Box::pin(manager.mount_registered_instances())
                    .await
                    .expect("the recovered root's instances mount");
            }),
        )
        .await
        .expect("recovery in the background ends within 60 s");
        drop(held);
        eprintln!("recovery child: recovered");

        assert!(
            is_mounted(&path),
            "the instance mounts once the move is recovered"
        );
        let record = manager
            .record_at(&path)
            .expect("the path makes a mount path")
            .expect("a mount records where its instance lives");
        assert_eq!(
            record.external_dir,
            target.join(instance_id.text_encoding())
        );
        manager.fuse().unmount_all(false).await.expect("unmounts");
    }))
    .await;
}

/// How long [`a_held_store_is_recovered_in_the_background_and_mounted`] waits for its child before
/// it kills it, so that a child that hangs fails the test, naming where its threads wait, rather
/// than holding up the binary. A killed child's mount is stale, which probes of the mount table
/// pass over.
const RECOVERY_CHILD_BOUND: std::time::Duration = std::time::Duration::from_secs(120);

#[tokio::test(flavor = "multi_thread")]
async fn a_held_store_is_recovered_in_the_background_and_mounted() {
    skip_without_fuse!();
    let _serial = SERIAL.lock().await;
    let exe = std::env::current_exe().expect("test binary path");
    let (ran, waited) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = ran.send(run_recovery_child(exe));
    });
    let (status, stdout, stderr, stopped) = waited
        .await
        .expect("the child's thread answers")
        .expect("the child runs");
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        status.success() && stdout.contains("1 passed"),
        "child failed{stopped}:\n{stdout}\n{}",
        String::from_utf8_lossy(&stderr)
    );
}

/// Runs [`recovery_child`] in a child process for up to [`RECOVERY_CHILD_BOUND`], reading its
/// output as it runs, and answers its status, its stdout, its stderr, and, where it was killed at
/// the bound, where each of its threads stopped.
fn run_recovery_child(
    exe: std::path::PathBuf,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>, String)> {
    let mut child = std::process::Command::new(exe)
        .args(["--exact", "needs_fuse::recovery_child", "--nocapture"])
        .env(RECOVERY_CHILD_ENV, "1")
        .env_remove("LORE_GLOBAL_PATH")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let readers = [
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    ]
    .map(|pipe| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = std::io::Read::read_to_end(&mut pipe, &mut text);
            }
            text
        })
    });
    let started = std::time::Instant::now();
    let mut stopped = String::new();
    while child.try_wait()?.is_none() {
        if started.elapsed() >= RECOVERY_CHILD_BOUND {
            stopped = threads_of(child.id());
            child.kill()?;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let status = child.wait()?;
    let [stdout, stderr] = readers.map(|reader| reader.join().unwrap_or_default());
    Ok((status, stdout, stderr, stopped))
}

/// Where each thread of the process `pid` stopped, by its name and the kernel function it waits
/// in, as `/proc` shows them to the same user.
fn threads_of(pid: u32) -> String {
    let read = |path: std::path::PathBuf| {
        std::fs::read_to_string(path)
            .map_or_else(|err| err.to_string(), |text| text.trim().to_string())
    };
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return format!(" at the bound, with no threads to read for process {pid}");
    };
    let mut threads: Vec<String> = tasks
        .filter_map(Result::ok)
        .map(|task| {
            format!(
                "{} {}: {}",
                task.file_name().to_string_lossy(),
                read(task.path().join("comm")),
                read(task.path().join("wchan"))
            )
        })
        .collect();
    threads.sort();
    format!(
        " at the bound, its threads waiting in:\n{}",
        threads.join("\n")
    )
}
