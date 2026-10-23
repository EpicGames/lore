// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use futures::StreamExt;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::commit::CommitOptions;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::stage::StageOptions;
use zerocopy::IntoBytes;

use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;
use crate::helper::generate_tempdir;
use crate::helper::test_file_write;

/// A context on `initial`'s repository and stores, opened under the call's execution context.
fn open(initial: &RepositoryContext, token: &RepositoryWriteToken) -> Arc<RepositoryContext> {
    let mutable_store = initial
        .try_mutable_store_arc()
        .expect("The initial context must be writable");
    Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(initial.immutable_store(), mutable_store)
                .with_path(
                    initial
                        .require_path()
                        .expect("The repository must have a path"),
                )
                .with_id(initial.id)
                .with_instance_id(initial.instance_id),
        )
        .with_write_token(token.share()),
    )
}

async fn branch_count(repository: Arc<RepositoryContext>) -> usize {
    branch::list(repository)
        .await
        .expect("Failed to list branches")
        .count()
        .await
}

/// A repository at `path` with one commit, and the context that made it.
async fn committed_repository(
    path: &std::path::Path,
    token: &RepositoryWriteToken,
) -> Arc<RepositoryContext> {
    let initial = repository::create_local(
        path,
        token,
        RepositoryId::from(uuid::Uuid::now_v7()),
        BranchId::from(uuid::Uuid::now_v7()),
        branch::DEFAULT_DEFAULT_NAME.to_string(),
        repository::RepositoryConfig::default(),
        false,
    )
    .await
    .expect("Failed to initialize repository");
    test_file_write(&path.join("file.txt"), b"committed");
    lore_revision::file::stage::stage(
        initial.clone(),
        token,
        LoreArray::from_vec(vec![LoreString::from(path)]),
        StageOptions {
            scan: true,
            ..StageOptions::default()
        },
    )
    .await
    .expect("Failed to stage");
    lore_revision::commit::commit_boxed(
        initial.clone(),
        token,
        CommitOptions {
            message: "Initial".to_string(),
            link_messages: std::collections::HashMap::new(),
            link: None,
            layer_messages: std::collections::HashMap::new(),
            layer: None,
        },
    )
    .await
    .expect("Failed to commit");
    initial
}

fn execution(dry_run: u8) -> Arc<ExecutionContext> {
    Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs {
            dry_run,
            ..Default::default()
        }
        .set_offline(),
        EventDispatcher::no_dispatch(),
    ))
}

/// A branch created in a dry run is not listed once the call ends.
#[tokio::test]
async fn a_dry_run_branch_create_leaves_the_store_alone() {
    LORE_CONTEXT
        .scope(execution(0), async move {
            let tempdir = generate_tempdir();
            let path = tempdir.to_path_buf();
            let token = RepositoryWriteToken::acquire(&path).await;
            let initial = committed_repository(&path, &token).await;
            let branches = branch_count(open(&initial, &token)).await;

            LORE_CONTEXT
                .scope(execution(1), async {
                    // Opened inside the scope, as a command opens its contexts in the call.
                    branch::create::create(
                        open(&initial, &token),
                        &token,
                        "dry-run".to_string(),
                        None,
                        String::new(),
                        false,
                    )
                    .await
                })
                .await
                .expect("A dry run branch create must succeed");

            assert_eq!(
                branch_count(open(&initial, &token)).await,
                branches,
                "a dry run must not create the branch"
            );
        })
        .await;
}

/// A repository opened for writing in a dry run keeps the legacy anchor file of a repository
/// whose anchor has not moved to the mutable store, which is where its anchor is still read
/// from.
#[tokio::test]
async fn a_dry_run_keeps_a_legacy_anchor_file() {
    LORE_CONTEXT
        .scope(execution(0), async move {
            let tempdir = generate_tempdir();
            let path = tempdir.to_path_buf();
            let token = RepositoryWriteToken::acquire(&path).await;
            let initial = committed_repository(&path, &token).await;
            let anchor = lore_revision::instance::load_current_anchor_boxed(&initial)
                .await
                .expect("Failed to load the anchor");

            // The anchor as a repository from before the move left it: in the file, with no
            // keys in the mutable store.
            let anchor_path = initial
                .dot_dir_path()
                .expect("The repository must have a dot directory")
                .join(lore_revision::anchor::CURRENT);
            let mut bytes = anchor.0.as_bytes().to_vec();
            bytes.extend_from_slice(anchor.1.as_bytes());
            test_file_write(&anchor_path, &bytes);
            for function in [
                lore_revision::instance::ANCHOR_CURRENT,
                lore_revision::instance::ANCHOR_CURRENT_BRANCH,
            ] {
                let (key, key_type) = lore_revision::instance::anchor_key(
                    initial.salt(),
                    function,
                    initial.instance_id,
                );
                initial
                    .write_mutable_store(&token)
                    .store(initial.id, key, Hash::default(), key_type)
                    .await
                    .expect("Failed to remove the anchor key");
            }
            initial.flush(true).await.expect("Failed to flush");

            LORE_CONTEXT
                .scope(
                    execution(1),
                    repository::load_and_connect_with_token(
                        &path,
                        RepositoryAccess::ReadWrite,
                        Some(token.share()),
                    ),
                )
                .await
                .expect("A dry run must open the repository");

            assert!(anchor_path.exists(), "a dry run must keep the anchor file");
            assert_eq!(
                lore_revision::instance::load_current_anchor_boxed(&open(&initial, &token))
                    .await
                    .expect("The anchor must still load"),
                anchor
            );
        })
        .await;
}

/// A repository opened for writing in a dry run leaves a missing instance file missing, and a
/// later command recovers the instance it belonged to.
#[tokio::test]
async fn a_dry_run_writes_no_missing_instance_file() {
    LORE_CONTEXT
        .scope(execution(0), async move {
            let tempdir = generate_tempdir();
            let path = tempdir.to_path_buf();
            let token = RepositoryWriteToken::acquire(&path).await;
            let initial = committed_repository(&path, &token).await;
            let instance_path = initial
                .dot_dir_path()
                .expect("The repository must have a dot directory")
                .join(lore_revision::repository::INSTANCE);
            let instance = std::fs::read(&instance_path).expect("Failed to read the instance");
            initial.flush(true).await.expect("Failed to flush");
            lore_io::IoDriver::global()
                .remove_file(&instance_path)
                .await
                .expect("Failed to remove the instance file");

            LORE_CONTEXT
                .scope(
                    execution(1),
                    repository::load_and_connect_with_token(
                        &path,
                        RepositoryAccess::ReadWrite,
                        Some(token.share()),
                    ),
                )
                .await
                .expect("A dry run must open the repository");
            assert!(
                !instance_path.exists(),
                "a dry run must not write the instance file"
            );

            repository::load_and_connect_with_token(
                &path,
                RepositoryAccess::ReadWrite,
                Some(token.share()),
            )
            .await
            .expect("The repository must open");
            assert_eq!(
                std::fs::read(&instance_path).expect("The instance file must be written"),
                instance,
                "a later command must recover the same instance"
            );
        })
        .await;
}
