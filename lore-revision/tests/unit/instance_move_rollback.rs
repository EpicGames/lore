// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What `instance move` does when recording the new path fails after the directory has moved.
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
        values: [Some(global.display().to_string()), None],
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
    _parent: TempDir,
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
            _parent: parent,
            source,
            target,
            repository: Arc::new(RepositoryContext::new(args)),
        }
    }

    fn global(&self) -> &Path {
        self._global.path()
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
