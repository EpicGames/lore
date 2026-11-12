// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The undelete of a node staged for deletion that the working tree still holds, by the stage
//! and by a staging walk.

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
use lore_revision::fs::filesystem_provider::StageIntent;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::state::Mark;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::test_anchor_states;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::helper::test_scan_with_intent;
use crate::stage::stage_root_child;

/// Staging a file staged for deletion by a merge, which the working tree still holds, takes the
/// deletion back and keeps the merge's flag: the merge flags stand until an unstage drops them.
#[tokio::test]
async fn an_undelete_keeps_the_merge_flags() {
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
            test_file_write(&instance.path.join("a.txt"), b"a");
            let state = test_commit_tree(&instance, "First").await;
            let repository = instance.repository.clone();
            let file = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("a.txt"))
                .await
                .expect("a.txt");
            for flags in [NodeFlags::StagedMerge, NodeFlags::StagedDelete] {
                state
                    .node_mark(&repository, file, Mark::staged(flags, true))
                    .await
                    .expect("the staged mark");
            }
            let marked = state.node(repository.clone(), file).await.expect("a.txt");
            assert!(marked.is_staged_delete() && marked.is_staged_merge());

            stage_root_child(&repository, &state, "a.txt", file, StageOptions::default()).await;

            let staged = state.node(repository.clone(), file).await.expect("a.txt");
            assert!(!staged.is_staged_delete(), "the deletion is taken back");
            assert!(staged.is_staged_modify());
            assert!(staged.is_staged_merge(), "the merge flag stands");
        })
        .await;
}

/// A case rename of a file staged for deletion by a merge with no dirty half, which the working
/// tree still holds under another case, takes the deletion back and stages the move, keeping the
/// dirty mark the undelete set.
#[tokio::test]
async fn a_case_rename_of_an_undeleted_file_keeps_it_dirty() {
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
            test_file_write(&instance.path.join("a.txt"), b"a");
            let state = test_commit_tree(&instance, "First").await;
            let repository = instance.repository.clone();
            let file = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("a.txt"))
                .await
                .expect("a.txt");
            for flags in [NodeFlags::StagedMerge, NodeFlags::StagedDelete] {
                state
                    .node_mark(&repository, file, Mark::staged(flags, true))
                    .await
                    .expect("the staged mark");
            }
            let marked = state.node(repository.clone(), file).await.expect("a.txt");
            assert!(marked.is_staged_delete() && !marked.is_dirty());
            #[allow(clippy::disallowed_methods)]
            std::fs::rename(instance.path.join("a.txt"), instance.path.join("A.txt"))
                .expect("the case rename on disk");

            let options = StageOptions {
                case_change: StageCaseChange::Rename,
                ..StageOptions::default()
            };
            stage_root_child(&repository, &state, "A.txt", file, options).await;

            let staged = state.node(repository.clone(), file).await.expect("A.txt");
            assert!(staged.is_staged_move(), "flags {:#x}", staged.flags);
            assert!(staged.is_dirty(), "flags {:#x}", staged.flags);
        })
        .await;
}

/// Staging a file staged for deletion whose merge conflict was resolved meanwhile, taking one
/// side, takes the deletion back and keeps the conflict resolved with its side, so the commit
/// takes it.
#[tokio::test]
async fn an_undelete_keeps_a_merge_conflicts_resolution() {
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
            test_file_write(&instance.path.join("a.txt"), b"a");
            let state = test_commit_tree(&instance, "First").await;
            let repository = instance.repository.clone();
            let file = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("a.txt"))
                .await
                .expect("a.txt");
            for flags in [NodeFlags::StagedMergeConflict, NodeFlags::StagedDelete] {
                state
                    .node_mark(&repository, file, Mark::staged(flags, true))
                    .await
                    .expect("the staged mark");
            }
            let block = state
                .block(
                    repository.clone(),
                    lore_revision::node::NodeBlock::index(file),
                )
                .await
                .expect("a.txt's block");
            // Resolved taking our side, as `merge resolve mine` leaves it.
            state.node_edit_for_tests(&block, file, |held| {
                held.flags |= (NodeFlags::StagedMergeResolved | NodeFlags::StagedMergeMine).bits();
            });

            stage_root_child(&repository, &state, "a.txt", file, StageOptions::default()).await;

            let staged = state.node(repository.clone(), file).await.expect("a.txt");
            assert!(!staged.is_staged_delete(), "the deletion is taken back");
            assert!(staged.is_staged_merge_conflict());
            assert!(
                staged.is_staged_merge_resolved(),
                "the conflict stays resolved"
            );
            assert!(staged.is_staged_merge_mine(), "with the side it took");
        })
        .await;
}

/// The node of the committed file `a.txt`, which the working tree still holds, after a staging walk
/// with `intent` over a staged State holding it marked staged with each of `marks` in turn.
async fn undeleted_by_staging_walk(marks: [NodeFlags; 2], intent: StageIntent) -> Node {
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
            test_file_write(&instance.path.join("a.txt"), b"a");
            test_commit_tree(&instance, "First").await;
            let repository = instance.repository.clone();
            let (current, staged) = test_anchor_states(&repository).await;
            let file = staged
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("a.txt"))
                .await
                .expect("a.txt");
            for flags in marks {
                staged
                    .node_mark(&repository, file, Mark::staged(flags, true))
                    .await
                    .expect("the staged mark");
            }
            let marked = staged.node(repository.clone(), file).await.expect("a.txt");
            assert!(marked.is_staged_delete(), "flags {:#x}", marked.flags);

            test_scan_with_intent(
                repository.clone(),
                staged.clone(),
                current,
                FilesystemDiffIntent::Stage(intent),
            )
            .await;
            staged.node(repository.clone(), file).await.expect("a.txt")
        })
        .await
}

/// A staging walk over a file staged for deletion by a merge, which the working tree still holds,
/// takes the deletion back and keeps the merge's flag, as the stage does.
#[tokio::test]
async fn a_staging_walks_undelete_keeps_the_merge_flags() {
    let staged = undeleted_by_staging_walk(
        [NodeFlags::StagedMerge, NodeFlags::StagedDelete],
        StageIntent::default(),
    )
    .await;
    assert!(!staged.is_staged_delete(), "the deletion is taken back");
    assert!(staged.is_staged_modify(), "flags {:#x}", staged.flags);
    assert!(staged.is_staged_merge(), "the merge flag stands");
}

/// A staging walk recording a merge over a file staged for deletion in conflict takes the deletion
/// back and keeps the conflict, which the walk's merge flags leave standing.
#[tokio::test]
async fn a_merge_staging_walks_undelete_keeps_the_conflict() {
    let staged = undeleted_by_staging_walk(
        [NodeFlags::StagedMergeConflict, NodeFlags::StagedDelete],
        StageIntent {
            node_flags: NodeFlags::StagedMerge,
            file_id: None,
        },
    )
    .await;
    assert!(!staged.is_staged_delete(), "the deletion is taken back");
    assert!(
        staged.is_staged_merge_unresolved(),
        "the conflict stands, flags {:#x}",
        staged.flags
    );
}

/// A staging walk over an add taken back, a node staged both added and deleted that the working
/// tree holds, takes the deletion back and stages the add it was.
#[tokio::test]
async fn a_staging_walks_undelete_of_an_add_taken_back_stages_the_add() {
    let staged = undeleted_by_staging_walk(
        [
            NodeFlags::StagedMerge,
            NodeFlags::StagedAdd | NodeFlags::StagedDelete,
        ],
        StageIntent::default(),
    )
    .await;
    assert!(!staged.is_staged_delete(), "the deletion is taken back");
    assert!(staged.is_staged_add(), "flags {:#x}", staged.flags);
    assert!(!staged.is_staged_modify(), "flags {:#x}", staged.flags);
}
