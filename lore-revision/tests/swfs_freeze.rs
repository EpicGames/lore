// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A mount's freeze: a freeze or a thaw has finished when it returns, a thaw SWFS couldn't finish
//! is finished by the next command, one whose state can't be reloaded still thaws, one found when
//! it is mounted is thawed, and the operation that holds it is held by one command at a time.
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
use lore_revision::fs::swfs::file::SwfsFileArray;
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

/// Held by each test for as long as it mounts, so this binary's tests, which run in parallel,
/// mount one at a time: the driver has eight mount slots, shared by every process on the machine,
/// and refuses a ninth mount (error 1450), parking the process that asked. Mounts by other
/// processes still count against the slots.
static MOUNTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What a test mounts with: a global data directory of its own, so no test touches the real one,
/// a directory to mount in, and [`MOUNTING`], held for as long as the test runs.
struct Mounting {
    global: TempDir,
    parent: TempDir,
    _one_at_a_time: tokio::sync::MutexGuard<'static, ()>,
}

/// What the test `name` mounts with, once no other test mounts, or nothing where the machine has no
/// `EpicVFS` driver, so no mount can be made and the test is skipped.
async fn mounting(name: &str) -> Option<Mounting> {
    if std::env::var_os("LORE_TEST_NO_SWFS").is_some() {
        eprintln!("skipped: LORE_TEST_NO_SWFS is set");
        return None;
    }
    let one_at_a_time = MOUNTING.lock().await;
    Some(Mounting {
        global: TempDir::new(&format!("lore-swfs-{name}-global-")),
        parent: TempDir::new(&format!("lore-swfs-{name}-mount-")),
        _one_at_a_time: one_at_a_time,
    })
}

#[tokio::test]
async fn mounting_a_frozen_instance_thaws_it_when_ready() {
    let Some(mounting) = mounting("stale-freeze").await else {
        return;
    };
    let mount_path = mounting.parent.path().join("mount");
    let instance_id = InstanceId::generate();
    let config = swfs_config();

    LORE_CONTEXT
        .scope(execution_in(mounting.global.path()), async {
            // Freeze a mount and close it without thawing, as a service that ends mid-operation
            // does.
            let first = mount_at(instance_id, &mount_path, &config).await;
            first.wait_for_ready().await.expect("first mount ready");
            abandon_a_freeze(&first).await;
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

/// A mounted instance with no revision, which is all the tests need of one.
struct Mounted {
    mount: Arc<SwfsInterface>,
    filesystem: Arc<SwfsFilesystem>,
    _parent: TempDir,
}

/// Mounts a new instance in `parent`, and waits for it to be ready.
async fn mounted(parent: TempDir) -> Mounted {
    let mount_path = parent.path().join("mount");
    let mount = mount_at(InstanceId::generate(), &mount_path, &swfs_config()).await;
    mount.wait_for_ready().await.expect("mount ready");
    Mounted {
        filesystem: Arc::new(SwfsFilesystem::new(&mount_path, mount.clone())),
        mount,
        _parent: parent,
    }
}

/// Freezes `mount` for an operation that then ends without its thaw, as one a command panicked or
/// was cancelled in does: the mount is left frozen, and no command holds it.
async fn abandon_a_freeze(mount: &Arc<SwfsInterface>) {
    let operation = mount.lock_operation().await.expect("hold the mount");
    let operation = mount
        .freeze(SwfsExecutionToken::new(), operation)
        .await
        .expect("freeze");
    drop(operation);
}

#[tokio::test]
async fn an_operations_thaw_has_finished_when_it_returns() {
    let Some(mounting) = mounting("thaw-finished").await else {
        return;
    };

    LORE_CONTEXT
        .scope(execution_in(mounting.global.path()), async {
            let mounted = mounted(mounting.parent).await;
            // SWFS finishes an empty mount's freeze and thaw within moments, so one operation
            // seldom shows a call that returned early: a hundred, one after another, do.
            for operation in 0..100 {
                let begun = FilesystemProvider::begin_operation(&*mounted.filesystem)
                    .await
                    .expect("begin the operation");
                assert!(
                    mounted.mount.is_frozen().expect("frozen state"),
                    "operation {operation}'s freeze has taken hold when it begins"
                );
                begun.finalize().await.expect("thaw");
                assert!(
                    !mounted.mount.is_frozen().expect("frozen state"),
                    "operation {operation}'s thaw has finished when it returns"
                );
            }
            mounted.mount.close();
        })
        .await;
}

#[tokio::test]
async fn the_next_command_ends_a_freeze_no_command_holds() {
    let Some(mounting) = mounting("abandoned-freeze").await else {
        return;
    };

    LORE_CONTEXT
        .scope(execution_in(mounting.global.path()), async {
            let mounted = mounted(mounting.parent).await;
            abandon_a_freeze(&mounted.mount).await;

            let next = FilesystemProvider::begin_operation(&*mounted.filesystem)
                .await
                .expect("the next command thaws the mount, then begins as usual");
            assert!(
                mounted.mount.is_frozen().expect("frozen state"),
                "the next command froze the mount for itself"
            );
            next.finalize().await.expect("thaw");
            assert!(!mounted.mount.is_frozen().expect("frozen state"));
            mounted.mount.close();
        })
        .await;
}

#[tokio::test]
async fn the_next_command_finishes_a_thaw_swfs_could_not() {
    let Some(mounting) = mounting("failed-thaw").await else {
        return;
    };

    LORE_CONTEXT
        .scope(execution_in(mounting.global.path()), async {
            let mounted = mounted(mounting.parent).await;
            // A command's thaw failed: the mount is left frozen, with the thaw kept.
            abandon_a_freeze(&mounted.mount).await;
            mounted.mount.keep_failed_thaw(SwfsFileArray::empty());

            let next = FilesystemProvider::begin_operation(&*mounted.filesystem)
                .await
                .expect("the next command thaws the mount, then begins as usual");
            assert!(!mounted.mount.has_failed_thaw(), "the thaw is finished");
            assert!(
                mounted.mount.is_frozen().expect("frozen state"),
                "the next command froze the mount for itself"
            );
            next.finalize().await.expect("thaw");
            assert!(!mounted.mount.is_frozen().expect("frozen state"));
            mounted.mount.close();
        })
        .await;
}

#[tokio::test]
async fn a_kept_thaw_with_no_freeze_left_to_end_is_dropped() {
    let Some(mounting) = mounting("stale-thaw").await else {
        return;
    };

    LORE_CONTEXT
        .scope(execution_in(mounting.global.path()), async {
            let mounted = mounted(mounting.parent).await;
            mounted.mount.keep_failed_thaw(SwfsFileArray::empty());

            let next = FilesystemProvider::begin_operation(&*mounted.filesystem)
                .await
                .expect("a thaw with nothing to thaw doesn't hold the next command back");
            assert!(!mounted.mount.has_failed_thaw());
            next.finalize().await.expect("thaw");
            mounted.mount.close();
        })
        .await;
}

#[tokio::test]
async fn a_command_waits_for_the_operation_of_the_one_holding_the_mount() {
    let Some(mounting) = mounting("operation").await else {
        return;
    };
    let first_command = execution_in(mounting.global.path());
    let second_command = execution_in(mounting.global.path());

    LORE_CONTEXT
        .scope(first_command.clone(), async {
            let Mounted {
                mount,
                filesystem,
                _parent,
            } = mounted(mounting.parent).await;

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
async fn an_operation_whose_state_cant_be_reloaded_thaws_and_fails() {
    let Some(mounting) = mounting("undescribed").await else {
        return;
    };

    LORE_CONTEXT
        .scope(execution_in(mounting.global.path()), async {
            let mounted = mounted(mounting.parent).await;
            let operation = FilesystemProvider::begin_operation(&*mounted.filesystem)
                .await
                .expect("begin the operation");
            // The instance's current revision becomes one the store doesn't hold, so the thaw
            // can't reload the state it describes the operation's files from.
            let repository = mounted.mount.resources().repository();
            let dot = repository.dot_dir_path().expect("dot directory");
            std::fs::create_dir_all(dot).expect("create the dot directory");
            std::fs::write(dot.join(lore_revision::anchor::CURRENT), [1u8; 48])
                .expect("write the anchor");

            let error = operation
                .finalize()
                .await
                .expect_err("the operation fails, so its command does");
            // Every failure here is internal, so the trace tells which step failed.
            assert!(
                format!("{error:?}").contains("Describing the files the operation wrote"),
                "the failure is the state's, not the thaw's: {error:?}"
            );
            assert!(
                !mounted.mount.is_frozen().expect("frozen state"),
                "the mount is thawed all the same"
            );
            assert!(
                !mounted.mount.has_failed_thaw(),
                "the thaw finished, so none is kept"
            );
            mounted.mount.close();
        })
        .await;
}

#[tokio::test]
async fn becoming_ready_leaves_the_freeze_of_a_command_holding_the_mount() {
    let Some(mounting) = mounting("ready-held").await else {
        return;
    };
    let command = execution_in(mounting.global.path());
    let readying = execution_in(mounting.global.path());

    LORE_CONTEXT
        .scope(command, async {
            let Mounted {
                mount,
                filesystem,
                _parent,
            } = mounted(mounting.parent).await;

            // A command found the mount before it was ready, and froze it.
            let operation = FilesystemProvider::begin_operation(&*filesystem)
                .await
                .expect("the command's operation");

            // The mount is then readied. It signals just before, the runtime runs one thread, and
            // on a mount SWFS has finished starting `wait_for_ready` awaits nothing before the
            // operation lock, so when the signal is received the task has reached the lock.
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::CurrentThread,
                "the signal shows the task reached the lock only on a one-thread runtime"
            );
            let (readying_signal, reached) = tokio::sync::oneshot::channel();
            let mut ready = tokio::spawn({
                let mount = mount.clone();
                LORE_CONTEXT.scope(readying, async move {
                    readying_signal
                        .send(())
                        .expect("the test is waiting for the signal");
                    mount.wait_for_ready().await
                })
            });
            reached.await.expect("the mount signals before it readies");

            let finished = tokio::time::timeout(STILL_WAITING, &mut ready).await;
            assert!(
                finished.is_err(),
                "readying waits while the command holds the mount"
            );
            assert!(
                mount.is_frozen().expect("frozen state"),
                "the command's freeze stands while the mount readies"
            );

            operation.finalize().await.expect("the command's thaw");
            tokio::time::timeout(Duration::from_secs(10), ready)
                .await
                .expect("readying finishes once the command has")
                .expect("joins")
                .expect("ready");
            assert!(!mount.is_frozen().expect("frozen state"));
            mount.close();
        })
        .await;
}

#[tokio::test]
async fn an_operation_inside_the_commands_own_is_refused() {
    let Some(mounting) = mounting("nested").await else {
        return;
    };

    LORE_CONTEXT
        .scope(execution_in(mounting.global.path()), async {
            let Mounted {
                mount,
                filesystem,
                _parent,
            } = mounted(mounting.parent).await;

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
