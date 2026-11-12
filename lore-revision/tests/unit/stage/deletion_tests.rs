// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // Test fixtures edit the working tree directly.

//! The stage of the deletion of a node staged for deletion already, by the stage and by a staging
//! walk: it is marked again only for the staged flags it lacks, such as a merge resolution's.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
use lore_revision::fs::filesystem_provider::StageIntent;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::stage::StageStats;
use lore_revision::stage::stage_delete;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::test_anchor_states;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::helper::test_scan_with_intent;

/// The flags a resolution taking our side stages a merge's deletion with.
const RESOLVED_MINE: NodeFlags = NodeFlags::from_bits_retain(
    NodeFlags::StagedMerge.bits()
        | NodeFlags::StagedMergeMine.bits()
        | NodeFlags::StagedMergeResolved.bits(),
);

/// The node at `path` in `state`.
async fn node_at(repository: &Arc<RepositoryContext>, state: &State, path: &str) -> NodeID {
    state
        .find_node_link(repository.clone(), path)
        .await
        .expect("the path")
        .node
}

/// Stages the deletion of `path` with `node_flags`, answering how many files and directories the
/// stage counted deleted.
async fn delete(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    path: &str,
    node_flags: NodeFlags,
) -> (u64, u64) {
    let stats = Arc::new(StageStats::default());
    let Ok(relative_path) = RelativePath::from_str(path);
    stage_delete(
        repository.clone(),
        state.clone(),
        relative_path,
        node_at(repository, state, path).await,
        node_flags,
        stats.clone(),
        None,
    )
    .await
    .expect("the staged deletion");
    (
        stats.file_delete_count.load(Ordering::Relaxed),
        stats.directory_delete_count.load(Ordering::Relaxed),
    )
}

/// A file or a directory staged for deletion is marked again by a stage of its deletion only for
/// the merge flags it lacks, a directory with everything below it, and once marked is left as it
/// is: a repeated stage counts nothing.
#[tokio::test]
async fn a_staged_deletion_is_marked_again_only_for_the_flags_it_lacks() {
    for (path, deleted) in [("Dir/a.txt", (1, 0)), ("Dir", (1, 1))] {
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
                std::fs::create_dir_all(instance.path.join("Dir")).expect("Dir");
                test_file_write(&instance.path.join("Dir/a.txt"), b"a");
                let state = test_commit_tree(&instance, "First").await;
                let repository = instance.repository.clone();

                assert_eq!(
                    delete(&repository, &state, path, NodeFlags::NoFlags).await,
                    deleted
                );
                assert_eq!(
                    delete(&repository, &state, path, NodeFlags::NoFlags).await,
                    (0, 0),
                    "{path}: a deletion staged already is left as it is"
                );
                assert_eq!(
                    delete(&repository, &state, path, RESOLVED_MINE).await,
                    deleted,
                    "{path}: the resolution's flags mark it again"
                );
                assert_eq!(
                    delete(&repository, &state, path, RESOLVED_MINE).await,
                    (0, 0),
                    "{path}: a node holding the resolution's flags is left as it is"
                );
                for marked in [path, "Dir/a.txt"] {
                    let node = state
                        .node(
                            repository.clone(),
                            node_at(&repository, &state, marked).await,
                        )
                        .await
                        .expect("the node");
                    assert!(node.is_staged_delete(), "{marked}: flags {:#x}", node.flags);
                    assert!(
                        node.is_staged_merge_mine() && node.is_staged_merge(),
                        "{marked}: flags {:#x}",
                        node.flags
                    );
                }
            })
            .await;
    }
}

/// A staging walk recording a merge over a file staged for deletion, which the working tree
/// lacks, marks it with the merge's flags; one recording no merge leaves it as it is.
#[tokio::test]
async fn a_merge_staging_walk_marks_a_staged_deletion_with_its_flags() {
    for (node_flags, merged) in [(NodeFlags::StagedMerge, true), (NodeFlags::NoFlags, false)] {
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
                let file = node_at(&repository, &staged, "a.txt").await;
                staged
                    .node_mark(
                        &repository,
                        file,
                        Mark::staged(NodeFlags::StagedDelete, true),
                    )
                    .await
                    .expect("the staged mark");
                std::fs::remove_file(instance.path.join("a.txt")).expect("the deletion on disk");

                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent {
                        node_flags,
                        file_id: None,
                    }),
                )
                .await;

                let node = staged.node(repository.clone(), file).await.expect("a.txt");
                assert!(node.is_staged_delete(), "flags {:#x}", node.flags);
                assert_eq!(node.is_staged_merge(), merged, "flags {:#x}", node.flags);
            })
            .await;
    }
}
