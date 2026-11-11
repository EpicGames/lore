// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `instance delete` must not act on the SWFS instance a checkout's `.lore/instance` file names
//! unless the service holds that checkout's path as the instance's mount.
//!
//! This file is its own test binary because it installs the process-global mount manager.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lore_base::env::CallEnvironment;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::fs::mount_manager_state::MountManagerState;
use lore_revision::global::external_dir::external_lore_dir;
use lore_revision::instance::InstanceId;
use lore_revision::instance::instance_delete;
use lore_revision::interface::ExecutionContext;
use lore_revision::lore::RepositoryId;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryContextCreationArgs;
use lore_revision::repository::RepositoryPaths;

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

#[tokio::test]
async fn delete_refuses_a_checkout_that_names_an_swfs_instance_it_is_not_mounted_as() {
    let global = TempDir::new("lore-swfs-guard-global-");
    let checkout = TempDir::new("lore-swfs-guard-checkout-");

    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            // An SWFS instance exists: its external `.lore` is in the global data directory.
            let swfs_instance = InstanceId::generate();
            let external = external_lore_dir(swfs_instance).expect("global data directory");
            std::fs::create_dir_all(&external).expect("create the external directory");

            // The service runs but holds no mounts, so the checkout's path is not one.
            MountManagerState::initialize()
                .await
                .expect("an empty mount manager initializes");

            // A regular checkout whose instance id is the SWFS instance's.
            let dot_path = checkout.path().join(DOT_LORE);
            std::fs::create_dir_all(&dot_path).expect("create the checkout's dot directory");
            std::fs::write(
                dot_path.join("config.toml"),
                toml::to_string(&RepositoryConfig::default()).expect("serialize the config"),
            )
            .expect("write the config");
            let (immutable_store, mutable_store) =
                lore_revision::repository::create_client_memory_stores()
                    .await
                    .expect("in-memory stores");
            let repository = Arc::new(RepositoryContext::new(RepositoryContextCreationArgs {
                paths: Some(RepositoryPaths::new(
                    checkout.path().to_path_buf(),
                    dot_path.clone(),
                )),
                immutable_store,
                mutable_store,
                id: RepositoryId::default(),
                instance_id: swfs_instance,
                remote: Err(lore_transport::ProtocolError::from(
                    lore_base::error::NoRemote,
                )),
                filter: Arc::default(),
                filesystem_provider: None,
            }));

            let result = instance_delete(repository).await;

            assert!(
                result.is_err(),
                "a checkout the service does not hold as a mount is not an SWFS instance to delete"
            );
            assert!(
                external.exists(),
                "the SWFS instance's external directory must be left alone"
            );
            assert!(
                dot_path.exists(),
                "nothing of the checkout should have been deleted"
            );
        })
        .await;
}
