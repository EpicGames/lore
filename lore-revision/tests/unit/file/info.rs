// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// A fixture builds working-tree state directly, outside any revision; what these test is what the
// walk measures of it.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;

use lore_revision::MAX_CONCURRENT_TREE_TASKS;
use lore_revision::file::info::*;
use lore_revision::filter::FilterStates;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::RepositoryContext;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

/// A repository over the working tree at `root`, admitting every path in it.
async fn os_repository(root: &Path) -> Arc<RepositoryContext> {
    let (immutable_store, mutable_store, _context) =
        test_store_create().await.expect("making test stores");
    Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store).with_path(root),
    ))
}

/// A directory holding two files and a subdirectory holding a third, 175 bytes in all.
fn write_tree(root: &Path) {
    let tree = root.join("tree");
    std::fs::create_dir_all(tree.join("inner")).expect("create directory");
    std::fs::write(tree.join("first.txt"), vec![b'a'; 100]).expect("write file");
    std::fs::write(tree.join("third.txt"), vec![b'c'; 25]).expect("write file");
    std::fs::write(tree.join("inner").join("second.txt"), vec![b'b'; 50]).expect("write file");
}

/// What the walk measures at `path`, under the working tree at `root`.
async fn measure(root: &Path, path: RelativePath) -> u64 {
    let repository = os_repository(root).await;
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("an operation over the working tree");
    calculate_local_size_recurse(
        operation,
        repository,
        path,
        FileInfo::Directory,
        FilterStates::ROOT,
    )
    .await
    .expect("a measured tree")
}

/// What the walk measures of the tree [`write_tree`] wrote under `root`.
async fn measure_tree(root: &Path) -> u64 {
    measure(
        root,
        RelativePath::new_from_initial_path("tree").expect("relative path"),
    )
    .await
}

#[tokio::test]
async fn a_directory_measures_every_file_below_it() {
    let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
    write_tree(dir.path());

    assert_eq!(175, measure_tree(dir.path()).await);
}

/// The repository's own directory adds nothing, whatever the working tree holds at its name.
#[tokio::test]
async fn the_repository_directory_is_not_measured() {
    let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
    write_tree(dir.path());
    std::fs::write(dir.path().join(DOT_LORE), vec![b'd'; 40]).expect("write file");

    assert_eq!(175, measure(dir.path(), RelativePath::default()).await);
}

/// A subtree is walked inline once the fan-out budget is spent, measuring what a walk of its
/// own would have.
#[tokio::test]
async fn a_subtree_is_measured_inline_once_the_budget_is_spent() {
    let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
    write_tree(dir.path());

    let _permits = local_size_task_semaphore()
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_TREE_TASKS as u32)
        .await
        .expect("the whole fan-out budget");

    assert_eq!(175, measure_tree(dir.path()).await);
}
