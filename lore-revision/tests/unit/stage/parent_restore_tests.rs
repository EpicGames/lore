// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The stage of the paths below a directory that a resolve restores from a merge parent.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::stage::stage_from_parent_state;
use lore_revision::state::Mark;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;
use tokio::sync::Semaphore;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;

/// A restore stages each node it changes afresh, keeping nothing an earlier stage left: a deletion
/// staged with our side of a merge, its file back in the working tree, is deleted again by a
/// restore of their side and staged as the merge's deletion, without our side.
#[tokio::test]
async fn a_restore_stages_a_node_staged_before_afresh() {
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
            let repository = instance.repository.clone();
            std::fs::create_dir_all(instance.path.join("d")).expect("the directory");
            test_file_write(&instance.path.join("d/f.txt"), b"f");
            test_file_write(&instance.path.join("d/g.txt"), b"g");
            let staged = test_commit_tree(&instance, "Both").await;
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_file(instance.path.join("d/f.txt")).expect("the delete");
            let theirs = test_commit_tree(&instance, "Theirs").await;
            test_file_write(&instance.path.join("d/f.txt"), b"f");

            let file = staged
                .find_node_link(repository.clone(), "d/f.txt")
                .await
                .expect("d/f.txt")
                .node;
            staged
                .node_mark(
                    &repository,
                    file,
                    Mark::staged(
                        NodeFlags::StagedDelete
                            | NodeFlags::StagedMerge
                            | NodeFlags::StagedMergeMine,
                        true,
                    ),
                )
                .await
                .expect("the deletion on our side");
            let directory = theirs
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("d"))
                .await
                .expect("d");

            let operation = repository
                .file_system()
                .begin_operation()
                .await
                .expect("a filesystem operation");
            stage_from_parent_state(
                operation.clone(),
                repository.clone(),
                staged.clone(),
                repository.clone(),
                theirs,
                "d".parse::<RelativePath>().expect("a path"),
                directory,
                StageOptions {
                    case_change: StageCaseChange::Keep,
                    node_flags: NodeFlags::StagedMerge | NodeFlags::StagedMergeTheirs,
                    file_id: None,
                    no_children: false,
                    scan: true,
                },
                Arc::default(),
                Arc::new(Semaphore::new(4)),
            )
            .await
            .expect("the restore");
            operation.finalize().await.expect("the operation finishes");

            assert!(
                !instance.path.join("d/f.txt").exists(),
                "their side deletes the file"
            );
            let restaged = staged
                .node(repository.clone(), file)
                .await
                .expect("d/f.txt");
            assert!(
                restaged.is_staged_delete() && restaged.is_staged_merge(),
                "staged as the merge's deletion, flags {:#x}",
                restaged.flags
            );
            assert!(
                !restaged.is_staged_merge_mine(),
                "the earlier stage's side stands, flags {:#x}",
                restaged.flags
            );
        })
        .await;
}
