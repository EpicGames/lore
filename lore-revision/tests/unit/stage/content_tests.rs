// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What the stage writes into a node it stages in place.

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeBlock;
use lore_revision::node::ROOT_NODE;
use lore_revision::stage::StageOptions;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::stage::stage_root_child;

/// Staging a directory whose node carries a mode clears the mode, as a directory holds none,
/// and stages the directory modified.
#[tokio::test]
async fn staging_a_directory_clears_its_mode() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            #[allow(clippy::disallowed_methods)]
            std::fs::create_dir(instance.path.join("d")).expect("mkdir d");
            test_file_write(&instance.path.join("d").join("a.txt"), b"a");
            let state = test_commit_tree(&instance, "First").await;
            let repository = instance.repository.clone();
            let dir = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("d"))
                .await
                .expect("d");
            let block = state
                .block(repository.clone(), NodeBlock::index(dir))
                .await
                .expect("d's block");
            assert!(state.node_change_in_place(
                &block,
                dir,
                (ROOT_NODE, hash_string("d")),
                |held| {
                    held.set_mode(0o755);
                    true
                },
            ));

            stage_root_child(&repository, &state, "d", dir, StageOptions::default()).await;

            let staged = state.node(repository.clone(), dir).await.expect("d");
            assert_eq!(staged.mode, 0, "the directory holds no mode");
            assert!(staged.is_staged_modify());
        })
        .await;
}

/// Staging a file the working tree changed records the size the walk measured, and leaves the
/// mode for the commit to compare.
#[tokio::test]
async fn staging_a_changed_file_records_its_size() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            test_file_write(&instance.path.join("a.txt"), b"short");
            let state = test_commit_tree(&instance, "First").await;
            let repository = instance.repository.clone();
            let file = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("a.txt"))
                .await
                .expect("a.txt");
            let committed = state.node(repository.clone(), file).await.expect("a.txt");
            assert_eq!(committed.size, 5);
            test_file_write(&instance.path.join("a.txt"), b"a longer content");

            stage_root_child(&repository, &state, "a.txt", file, StageOptions::default()).await;

            let staged = state.node(repository.clone(), file).await.expect("a.txt");
            assert!(staged.is_staged_modify());
            assert_eq!(staged.size, 16, "the size the walk measured");
            assert_eq!(staged.mode, committed.mode, "the mode stays");
        })
        .await;
}
