// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The stage of a file whose merge conflict was resolved: its marks set no merge state of their
//! own, so the resolution and its side stand.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::TestRepository;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::stage::stage_root_child;

/// An execution whose calls run with `--force`.
fn forced() -> Arc<lore_revision::interface::ExecutionContext> {
    Arc::new(lore_revision::interface::ExecutionContext::new_client(
        lore_revision::interface::LoreGlobalArgs {
            force: 1,
            ..Default::default()
        },
        lore_revision::relay::EventDispatcher::no_dispatch(),
    ))
}

/// A repository whose revision holds `a.txt`, and the node of `a.txt`, staged as a merge conflict
/// resolved taking their side.
async fn resolved(instance: &TestRepository) -> (Arc<State>, NodeID) {
    test_file_write(&instance.path.join("a.txt"), b"a");
    let state = test_commit_tree(instance, "First").await;
    let repository = instance.repository.clone();
    let file = state
        .find_subnode(repository.clone(), ROOT_NODE, hash_string("a.txt"))
        .await
        .expect("a.txt");
    state
        .node_mark(
            &repository,
            file,
            Mark::staged(
                NodeFlags::StagedMergeResolved | NodeFlags::StagedMergeTheirs,
                true,
            ),
        )
        .await
        .expect("the resolution");
    (state, file)
}

/// Asserts `file` is still a conflict resolved taking their side.
async fn assert_resolved(state: &State, instance: &TestRepository, file: NodeID) {
    let held = state
        .node(instance.repository.clone(), file)
        .await
        .expect("a.txt");
    assert!(
        held.is_staged_merge_resolved() && held.is_staged_merge_theirs(),
        "the resolution and its side stand, flags {:#x}",
        held.flags
    );
}

/// A case rename of a resolved conflict stages the move and keeps the resolution.
#[tokio::test]
async fn a_case_rename_keeps_a_resolution() {
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
            let (state, file) = resolved(&instance).await;
            #[allow(clippy::disallowed_methods)]
            std::fs::rename(instance.path.join("a.txt"), instance.path.join("A.txt"))
                .expect("the case rename on disk");

            let options = StageOptions {
                case_change: StageCaseChange::Rename,
                ..StageOptions::default()
            };
            stage_root_child(&instance.repository, &state, "A.txt", file, options).await;

            let name = state
                .node_name_clone(instance.repository.clone(), file)
                .await
                .expect("the name");
            assert_eq!(name, "A.txt");
            let held = state
                .node(instance.repository.clone(), file)
                .await
                .expect("A.txt");
            assert!(held.is_staged_move(), "flags {:#x}", held.flags);
            assert_resolved(&state, &instance, file).await;
        })
        .await;
}

/// A forced stage of a resolved conflict the working tree changed stages the modification and
/// keeps the resolution.
#[tokio::test]
async fn a_forced_stage_keeps_a_resolution() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(forced(), async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            let (state, file) = resolved(&instance).await;
            test_file_write(&instance.path.join("a.txt"), b"changed");

            stage_root_child(
                &instance.repository,
                &state,
                "a.txt",
                file,
                StageOptions::default(),
            )
            .await;

            let held = state
                .node(instance.repository.clone(), file)
                .await
                .expect("a.txt");
            assert!(held.is_staged_modify(), "flags {:#x}", held.flags);
            assert_resolved(&state, &instance, file).await;
        })
        .await;
}

/// A forced stage of a resolved conflict staged for deletion that the working tree still holds
/// takes the deletion back and keeps the resolution: both its marks set no merge state.
#[tokio::test]
async fn a_forced_undelete_keeps_a_resolution() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(forced(), async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            let (state, file) = resolved(&instance).await;
            state
                .node_mark(
                    &instance.repository,
                    file,
                    Mark::staged(NodeFlags::StagedDelete, true),
                )
                .await
                .expect("the deletion");

            stage_root_child(
                &instance.repository,
                &state,
                "a.txt",
                file,
                StageOptions::default(),
            )
            .await;

            let held = state
                .node(instance.repository.clone(), file)
                .await
                .expect("a.txt");
            assert!(!held.is_staged_delete(), "the deletion is taken back");
            assert_resolved(&state, &instance, file).await;
        })
        .await;
}
