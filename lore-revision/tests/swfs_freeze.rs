// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A mount's freeze: one found when it is mounted is thawed, and the operation that holds it is held
//! by one command at a time.
//!
//! Mounting needs the `EpicVFS` driver running, and hangs without it, so a machine without the
//! driver sets `LORE_TEST_NO_SWFS`, which skips the tests, as `-m "not swfs"` does the smoke
//! tests. The tests run on Windows x86-64 only.
#![cfg(all(windows, target_arch = "x86_64"))]
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use lore_base::env::CallEnvironment;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::swfs::api_interface::SwfsInterface;
use lore_revision::fs::swfs::filesystem::SwfsFilesystem;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::mount_resources::SwfsExecutionToken;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::instance::InstanceId;
use lore_revision::interface::ExecutionContext;
use lore_revision::lore::RepositoryId;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::VfsConfig;
use lore_revision::repository::VfsType;

/// How long a command that has reached the operation lock is watched, to see that it goes on
/// waiting. Every passing run waits it out, so it is kept short.
const STILL_WAITING: Duration = Duration::from_millis(10);

/// A call whose global data directory is `global`, so no test touches the real one.
fn execution_in(global: &std::path::Path) -> Arc<ExecutionContext> {
    let environment = CallEnvironment {
        values: [Some(global.display().to_string()), None, None],
    };
    Arc::new(
        ExecutionContext::new_client_with_user_id(
            Default::default(),
            EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        )
        .with_environment(environment),
    )
}

async fn mount_at(
    instance_id: InstanceId,
    mount_path: &std::path::Path,
    config: &RepositoryConfig,
) -> Arc<SwfsInterface> {
    let resources = MountResources::premount_instance_with_config(
        RepositoryId::default(),
        instance_id,
        MountPath::new(mount_path).expect("mount path"),
        config,
    )
    .await
    .expect("premount resources");
    SwfsInterface::mount(resources).expect("mount")
}

/// Whether the machine has no `EpicVFS` driver, so no mount can be made.
fn swfs_unavailable() -> bool {
    std::env::var_os("LORE_TEST_NO_SWFS").is_some()
}

#[tokio::test]
async fn mounting_a_frozen_instance_thaws_it_when_ready() {
    if swfs_unavailable() {
        eprintln!("skipped: LORE_TEST_NO_SWFS is set");
        return;
    }
    let global = TempDir::new("lore-swfs-stale-freeze-global-");
    let parent = TempDir::new("lore-swfs-stale-freeze-mount-");
    let mount_path = parent.path().join("mount");
    let instance_id = InstanceId::generate();
    let config = RepositoryConfig {
        vfs: Some(VfsConfig {
            vfs_type: VfsType::Swfs,
            ..Default::default()
        }),
        ..Default::default()
    };

    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            // Freeze a mount and close it without thawing, as a service that ends mid-operation
            // does.
            let first = mount_at(instance_id, &mount_path, &config).await;
            first.wait_for_ready().await.expect("first mount ready");
            first
                .freeze(SwfsExecutionToken::new())
                .await
                .expect("freeze");
            assert!(first.is_frozen().expect("frozen state"));
            first.close();

            let second = mount_at(instance_id, &mount_path, &config).await;
            assert!(
                second.is_frozen().expect("frozen state"),
                "the freeze must outlive the mount, or this test shows nothing"
            );

            second.wait_for_ready().await.expect("second mount ready");
            assert!(
                !second.is_frozen().expect("frozen state"),
                "a mount found frozen must be thawed once it is ready"
            );
            second.close();
        })
        .await;
}

fn swfs_config() -> RepositoryConfig {
    RepositoryConfig {
        vfs: Some(VfsConfig {
            vfs_type: VfsType::Swfs,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_command_waits_for_the_operation_of_the_one_holding_the_mount() {
    if swfs_unavailable() {
        eprintln!("skipped: LORE_TEST_NO_SWFS is set");
        return;
    }
    let global = TempDir::new("lore-swfs-operation-global-");
    let parent = TempDir::new("lore-swfs-operation-mount-");
    let mount_path = parent.path().join("mount");
    let first_command = execution_in(global.path());
    let second_command = execution_in(global.path());

    LORE_CONTEXT
        .scope(first_command.clone(), async {
            let mount = mount_at(InstanceId::generate(), &mount_path, &swfs_config()).await;
            mount.wait_for_ready().await.expect("mount ready");
            let filesystem = Arc::new(SwfsFilesystem::new(&mount_path, mount.clone()));

            let first = FilesystemProvider::begin_operation(&*filesystem)
                .await
                .expect("first operation");
            assert!(mount.is_frozen().expect("frozen state"));

            // A second command, which has its own execution context, begins its operation. It
            // signals just before, the runtime runs one thread, and `begin_operation` awaits
            // nothing before the operation lock, so when the signal is received the task has
            // reached the lock.
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::CurrentThread,
                "the signal shows the task reached the lock only on a one-thread runtime"
            );
            let (beginning, begun) = tokio::sync::oneshot::channel();
            let mut second = tokio::spawn({
                let filesystem = filesystem.clone();
                LORE_CONTEXT.scope(second_command.clone(), async move {
                    beginning
                        .send(())
                        .expect("the test is waiting for the signal");
                    FilesystemProvider::begin_operation(&*filesystem).await
                })
            });
            begun
                .await
                .expect("the second command signals before it begins");

            let finished = tokio::time::timeout(STILL_WAITING, &mut second).await;
            assert!(
                finished.is_err(),
                "the second command must wait while the first holds the operation"
            );
            assert!(
                mount.is_frozen().expect("frozen state"),
                "the first command's freeze must stand while the second waits"
            );

            first.finalize().await.expect("first thaw");

            let second = tokio::time::timeout(Duration::from_secs(10), second)
                .await
                .expect("the second command starts once the first has finished")
                .expect("joins")
                .expect("second operation");
            assert!(
                mount.is_frozen().expect("frozen state"),
                "the second command freezes the mount for itself"
            );

            LORE_CONTEXT
                .scope(second_command.clone(), second.finalize())
                .await
                .expect("second thaw");
            assert!(!mount.is_frozen().expect("frozen state"));
            mount.close();
        })
        .await;
}

#[tokio::test]
async fn an_operation_inside_the_commands_own_is_refused() {
    if swfs_unavailable() {
        eprintln!("skipped: LORE_TEST_NO_SWFS is set");
        return;
    }
    let global = TempDir::new("lore-swfs-nested-global-");
    let parent = TempDir::new("lore-swfs-nested-mount-");
    let mount_path = parent.path().join("mount");

    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let mount = mount_at(InstanceId::generate(), &mount_path, &swfs_config()).await;
            mount.wait_for_ready().await.expect("mount ready");
            let filesystem = Arc::new(SwfsFilesystem::new(&mount_path, mount.clone()));

            let outer = FilesystemProvider::begin_operation(&*filesystem)
                .await
                .expect("outer operation");
            let inner = tokio::time::timeout(
                Duration::from_secs(10),
                FilesystemProvider::begin_operation(&*filesystem),
            )
            .await
            .expect("an inner operation must be refused, not wait for the outer one");

            assert!(inner.is_err());
            outer.finalize().await.expect("thaw");
            mount.close();
        })
        .await;
}
