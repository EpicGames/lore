// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What `instance move` does when recording the new path fails after the directory has moved, and
//! with the directories it creates above its target.
//!
//! The step that records a path is injected, so each failure is forced rather than waited for.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use lore_base::env::CallEnvironment;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::instance::InstanceError;
use lore_revision::instance::InstanceId;
use lore_revision::instance::instance_move_with;
use lore_revision::interface::ExecutionContext;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryPaths;

use crate::helper::default_repository_creation_args;

/// A call whose global data directory is `global`, so no test touches the real one.
fn execution_in(global: &Path) -> Arc<ExecutionContext> {
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

/// A regular instance at `source`, and a `target` beside it that does not exist.
struct Instance {
    _global: TempDir,
    parent: TempDir,
    source: PathBuf,
    target: PathBuf,
    repository: Arc<RepositoryContext>,
}

impl Instance {
    async fn new() -> Self {
        let global = TempDir::new("lore-move-rollback-global-");
        let parent = TempDir::new("lore-move-rollback-parent-");
        let source = parent.path().join("source");
        let target = parent.path().join("target");

        let dot_path = source.join(DOT_LORE);
        std::fs::create_dir_all(&dot_path).expect("create the instance's dot directory");
        std::fs::write(
            dot_path.join("config.toml"),
            toml::to_string(&RepositoryConfig::default()).expect("serialize the config"),
        )
        .expect("write the config");

        let (immutable_store, mutable_store) =
            lore_revision::repository::create_client_memory_stores()
                .await
                .expect("in-memory stores");
        let mut args = default_repository_creation_args(immutable_store, mutable_store);
        args.paths = Some(RepositoryPaths::new(source.clone(), dot_path));
        args.instance_id = InstanceId::generate();

        Self {
            _global: global,
            parent,
            source,
            target,
            repository: Arc::new(RepositoryContext::new(args)),
        }
    }

    fn global(&self) -> &Path {
        self._global.path()
    }

    fn parent(&self) -> &Path {
        self.parent.path()
    }

    fn target_str(&self) -> String {
        self.target.display().to_string()
    }
}

fn paths(calls: &Mutex<Vec<PathBuf>>) -> Vec<PathBuf> {
    calls.lock().expect("calls").clone()
}

/// The registration at the target fails and the one at the source is restored: the instance is
/// back where it started and the error says only that the new path was not recorded.
#[tokio::test]
async fn a_failed_registration_moves_the_instance_back_and_restores_its_registration() {
    let instance = Instance::new().await;
    let calls = Mutex::new(Vec::new());

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &instance.target_str(),
                async |path: &Path| {
                    calls.lock().expect("calls").push(path.to_path_buf());
                    if path == instance.target {
                        Err(InstanceError::internal("the registry is unavailable"))
                    } else {
                        Ok(())
                    }
                },
            )
            .await
        })
        .await;

    let message = result.expect_err("the move fails").to_string();
    assert!(
        message.contains("Failed to record the new path"),
        "{message}"
    );
    assert!(
        !message.contains("could not be restored"),
        "the source registration was restored: {message}"
    );
    assert!(
        instance.source.join(DOT_LORE).exists(),
        "back at the source"
    );
    assert!(!instance.target.exists(), "nothing left at the target");
    assert_eq!(
        paths(&calls),
        [instance.target.clone(), instance.source.clone()],
        "the target is recorded first, then the source is restored"
    );
}

/// Neither registration can be written: the instance is still moved back, and the error tells
/// the user to repair the registration.
#[tokio::test]
async fn a_registration_that_cannot_be_restored_is_reported() {
    let instance = Instance::new().await;
    let calls = Mutex::new(Vec::new());

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &instance.target_str(),
                async |path: &Path| {
                    calls.lock().expect("calls").push(path.to_path_buf());
                    Err(InstanceError::internal("the registry is unavailable"))
                },
            )
            .await
        })
        .await;

    let message = result.expect_err("the move fails").to_string();
    assert!(
        message.contains("its registration could not be restored"),
        "{message}"
    );
    assert!(message.contains("update-path"), "{message}");
    assert!(
        instance.source.join(DOT_LORE).exists(),
        "back at the source"
    );
    assert!(!instance.target.exists(), "nothing left at the target");
    assert_eq!(
        paths(&calls),
        [instance.target.clone(), instance.source.clone()]
    );
}

/// The registration fails and the instance cannot be moved back: it stays at the target, the
/// source is not registered, and the error says where the instance is.
#[tokio::test]
async fn an_instance_that_cannot_be_moved_back_is_reported_where_it_is() {
    let instance = Instance::new().await;
    let calls = Mutex::new(Vec::new());

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &instance.target_str(),
                async |path: &Path| {
                    calls.lock().expect("calls").push(path.to_path_buf());
                    // Something now occupies the source and is not empty, so the instance
                    // cannot be renamed back onto it on any platform.
                    std::fs::create_dir_all(&instance.source).expect("occupy the source");
                    std::fs::write(instance.source.join("blocker"), b"").expect("fill the source");
                    Err(InstanceError::internal("the registry is unavailable"))
                },
            )
            .await
        })
        .await;

    let message = result.expect_err("the move fails").to_string();
    assert!(message.contains("also failed"), "{message}");
    assert!(message.contains("update-path"), "{message}");
    assert!(
        instance.target.join(DOT_LORE).exists(),
        "the instance is still at the target"
    );
    assert_eq!(
        paths(&calls),
        std::slice::from_ref(&instance.target),
        "the source registration is not restored while the instance is elsewhere"
    );
}

/// A target whose directories do not exist yet, two levels below the instance's parent.
fn deep_target(instance: &Instance) -> PathBuf {
    instance.parent().join("new").join("deeper").join("target")
}

/// A move into directories that do not exist creates them, and the instance moves there.
#[tokio::test]
async fn a_move_creates_the_directories_above_its_target() {
    let instance = Instance::new().await;
    let target = deep_target(&instance);

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &target.display().to_string(),
                async |_path: &Path| Ok(()),
            )
            .await
        })
        .await;

    result.expect("the move succeeds");
    assert!(
        target.join(DOT_LORE).exists(),
        "the instance is at the target"
    );
    assert!(!instance.source.exists(), "and no longer at the source");
}

/// A move whose registration fails moves the instance back, and removes the directories it
/// created for the target.
#[tokio::test]
async fn a_move_that_is_undone_removes_the_directories_it_created() {
    let instance = Instance::new().await;
    let target = deep_target(&instance);

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &target.display().to_string(),
                async |path: &Path| {
                    if path == target {
                        Err(InstanceError::internal("the registry is unavailable"))
                    } else {
                        Ok(())
                    }
                },
            )
            .await
        })
        .await;

    let message = result.expect_err("the move fails").to_string();
    assert!(
        message.contains("Failed to record the new path"),
        "{message}"
    );
    assert!(
        instance.source.join(DOT_LORE).exists(),
        "back at the source"
    );
    assert!(
        !instance.parent().join("new").exists(),
        "the directories the move created are gone"
    );
}

/// A move whose rename fails removes the directories it created for the target.
#[cfg(unix)]
#[tokio::test]
async fn a_move_that_fails_removes_the_directories_it_created() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    // Safety: `geteuid` takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root renames out of a directory it cannot write to");
        return;
    }
    let instance = Instance::new().await;
    let elsewhere = TempDir::new("lore-move-rollback-elsewhere-");
    let target = elsewhere.path().join("new").join("target");
    // A directory that cannot be written to refuses to give up the instance's directory.
    let source_parent = instance.parent();
    std::fs::set_permissions(source_parent, Permissions::from_mode(0o555))
        .expect("make the source's parent read-only");

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &target.display().to_string(),
                async |_path: &Path| Ok(()),
            )
            .await
        })
        .await;
    std::fs::set_permissions(source_parent, Permissions::from_mode(0o755))
        .expect("make the source's parent writable again");

    let message = result.expect_err("the rename fails").to_string();
    assert!(message.contains("Failed to move directory"), "{message}");
    assert!(
        instance.source.join(DOT_LORE).exists(),
        "still at the source"
    );
    assert!(
        !elsewhere.path().join("new").exists(),
        "the directory the move created is gone"
    );
}

/// The message a move to `target` is refused with as invalid arguments, before it registers
/// anything, the instance left at its source.
async fn refused_move(instance: &Instance, target: &Path) -> String {
    let calls = Mutex::new(Vec::new());
    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &target.display().to_string(),
                async |path: &Path| {
                    calls.lock().expect("calls").push(path.to_path_buf());
                    Ok(())
                },
            )
            .await
        })
        .await;
    assert!(paths(&calls).is_empty(), "nothing is registered");
    assert!(
        instance.source.join(DOT_LORE).exists(),
        "still at the source"
    );
    let error = result.expect_err("the move is refused");
    assert!(
        matches!(error, InstanceError::InvalidArguments(_)),
        "{error}"
    );
    error.to_string()
}

/// A target below a file is refused before anything is created.
#[tokio::test]
async fn a_target_below_a_file_is_refused() {
    let instance = Instance::new().await;
    let file = instance.parent().join("file");
    std::fs::write(&file, b"").expect("a file where a directory would go");

    let message = refused_move(&instance, &file.join("new").join("target")).await;

    assert!(message.contains("which is not a directory"), "{message}");
}

/// A target inside the instance is refused before anything is created in its working tree.
#[tokio::test]
async fn a_target_inside_the_instance_is_refused() {
    let instance = Instance::new().await;

    let message = refused_move(&instance, &instance.source.join("new").join("target")).await;

    assert!(message.contains("inside the instance"), "{message}");
    assert!(
        !instance.source.join("new").exists(),
        "nothing is created in the working tree"
    );
}

/// A target below a symbolic link that leads into the instance is refused before anything is
/// created in its working tree, as the rename into its own subtree would fail.
#[cfg(unix)]
#[tokio::test]
async fn a_target_through_a_link_into_the_instance_is_refused() {
    let instance = Instance::new().await;
    let elsewhere = TempDir::new("lore-move-rollback-elsewhere-");
    let link = elsewhere.path().join("link");
    std::os::unix::fs::symlink(&instance.source, &link).expect("link to the instance");

    let message = refused_move(&instance, &link.join("new").join("target")).await;

    assert!(message.contains("inside the instance"), "{message}");
    assert!(
        !instance.source.join("new").exists(),
        "nothing is created in the working tree"
    );
}

/// A symbolic link at the target that leads nowhere exists all the same: the move is refused, and
/// the link stays rather than being replaced by the instance.
#[cfg(unix)]
#[tokio::test]
async fn a_dangling_link_at_the_target_is_refused() {
    let instance = Instance::new().await;
    std::os::unix::fs::symlink(instance.parent().join("nowhere"), &instance.target)
        .expect("a link that leads nowhere");

    let message = refused_move(&instance, &instance.target).await;

    assert!(message.contains("already exists"), "{message}");
    assert!(
        instance
            .target
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink()),
        "the link is still there"
    );
}

/// A target below a symbolic link to the directory holding the instance is outside it, and the
/// instance moves there.
#[cfg(unix)]
#[tokio::test]
async fn a_target_through_a_link_beside_the_instance_moves_it() {
    let instance = Instance::new().await;
    let elsewhere = TempDir::new("lore-move-rollback-elsewhere-");
    let link = elsewhere.path().join("link");
    std::os::unix::fs::symlink(instance.parent(), &link).expect("link to the instance's parent");

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &link.join("beside").display().to_string(),
                async |_path: &Path| Ok(()),
            )
            .await
        })
        .await;

    result.expect("the move succeeds");
    assert!(
        instance.parent().join("beside").join(DOT_LORE).exists(),
        "the instance is beside its old path"
    );
}

/// A target that leaves the instance through `..` names the directory it reaches, and the
/// instance moves there.
#[tokio::test]
async fn a_target_through_dot_dot_moves_beside_the_instance() {
    let instance = Instance::new().await;
    let target = instance.source.join("..").join("beside");

    let result = LORE_CONTEXT
        .scope(execution_in(instance.global()), async {
            instance_move_with(
                instance.repository.clone(),
                &target.display().to_string(),
                async |_path: &Path| Ok(()),
            )
            .await
        })
        .await;

    result.expect("the move succeeds");
    assert!(
        instance.parent().join("beside").join(DOT_LORE).exists(),
        "the instance is beside its old path"
    );
}

/// A target inside another repository's working tree, known by its `.lore` or older `.urc`
/// directory, is refused before anything is created.
#[tokio::test]
async fn a_move_into_another_repository_is_refused() {
    for dot in [DOT_LORE, ".urc"] {
        let instance = Instance::new().await;
        let other = instance.parent().join("other");
        std::fs::create_dir_all(other.join(dot)).expect("make another repository");
        let target = other.join("nested");

        refused_move(&instance, &target).await;

        assert!(!target.exists(), "{dot}: nothing is created at the target");
    }
}
