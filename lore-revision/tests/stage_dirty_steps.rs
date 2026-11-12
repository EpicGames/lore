// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The chain steps a stage of a directory's dirty files takes, counted process-wide.
//!
//! A process of its own: the stage spawns a walk per file onto the runtime's workers, whose
//! finds only the process-wide count covers, which another test's finds would add to.
#![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to write-token discipline.

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreNodeStagedAction;
use lore_revision::interface::LoreString;
use lore_revision::lore::RepositoryId;
use lore_revision::stage::StageOptions;
use lore_revision::state::State;
use lore_revision::state::step_counting;

#[path = "helper.rs"]
mod helper;

use helper::TestRepository;
use helper::test_commit_tree;
use helper::test_file_write;
use helper::test_repository_create;
use helper::test_store_create;

/// How many files each directory of the test holds.
const FILES: usize = 256;

/// The chain steps every thread's finds have taken.
fn chain_steps() -> u64 {
    step_counting::set_chain_step_counting(true);
    step_counting::total_chain_steps()
}

/// Writes [`FILES`] files into `dir` below `instance`.
fn write_files(instance: &TestRepository, dir: &str) {
    std::fs::create_dir_all(instance.path.join(dir)).expect("the directory");
    for index in 0..FILES {
        test_file_write(
            &instance.path.join(dir).join(format!("f{index}")),
            index.to_string().as_bytes(),
        );
    }
}

/// The paths `dir` below `instance` names, as the commands take them.
fn named(instance: &TestRepository, dir: &str) -> LoreArray<LoreString> {
    LoreArray::from_vec(vec![LoreString::from(&instance.path.join(dir))])
}

/// Marks `dir` dirty, then stages it from its dirty marks, answering with the chain steps the
/// stage's finds took and the staged revision.
async fn staged_steps(instance: &TestRepository, dir: &str) -> (u64, Hash) {
    lore_revision::file::dirty::dirty(instance.repository.clone(), named(instance, dir))
        .await
        .expect("the dirty marks");
    let before = chain_steps();
    let signature = lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        named(instance, dir),
        StageOptions::default(),
    )
    .await
    .expect("the stage");
    (chain_steps() - before, signature)
}

/// The staged revision `signature` names.
async fn staged_state(instance: &TestRepository, signature: Hash) -> std::sync::Arc<State> {
    State::deserialize(instance.repository.clone(), signature)
        .await
        .expect("the staged state")
}

/// How many children of the directory at `path` in `staged` carry `action`.
async fn children_staged(
    instance: &TestRepository,
    staged: &State,
    path: &str,
    action: LoreNodeStagedAction,
) -> usize {
    let dir = staged
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("the directory")
        .node;
    let mut count = 0;
    for child in staged
        .node_children(instance.repository.clone(), dir)
        .await
        .expect("the directory's children")
    {
        let node = staged
            .node(instance.repository.clone(), child)
            .await
            .expect("the child's record");
        count += usize::from(node.staged_action() == action);
    }
    count
}

/// A stage of a directory's dirty files resolves each file's node from one listing of the
/// directory, its finds taking a few steps per file at most, where a search of its chain for each
/// costs N(N-1)/2 steps for N: its new files, its new empty directories, its new directories each
/// holding a new file, whose files are the targets, its deleted files, and the deleted file of
/// each of its directories. Each is staged.
///
/// The bound of 4N holds whatever order the file system lists the directory in: the first target
/// below the directory searches its chain twice before a second target lists it, once for the
/// directories above the targets and once in its walk, up to N steps each; each target then takes
/// a step or so below its own directory, and the root and the directory's own lookups take a
/// dozen. The new directories are made in reverse, so a file system listing the newest first, as
/// tmpfs does, puts the first target's directory last in the chain the dirty marks built.
#[tokio::test]
async fn staging_a_directorys_dirty_files_searches_its_chain_for_none_of_them() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            for dir in ["new", "dirs", "nested"] {
                std::fs::create_dir_all(instance.path.join(dir)).expect("the directory");
                test_file_write(&instance.path.join(dir).join("seed"), b"seed");
            }
            write_files(&instance, "gone");
            for index in 0..FILES {
                let dir = instance.path.join("emptied").join(format!("d{index}"));
                std::fs::create_dir_all(&dir).expect("the directory");
                test_file_write(&dir.join("f"), b"f");
            }
            test_commit_tree(&instance, "First").await;

            write_files(&instance, "new");
            let (steps, signature) = staged_steps(&instance, "new").await;
            assert!(steps < 4 * FILES as u64, "the new files took {steps} steps");
            let staged = staged_state(&instance, signature).await;
            let adds = children_staged(&instance, &staged, "new", LoreNodeStagedAction::Add).await;
            assert_eq!(adds, FILES, "the new files are staged");

            for index in 0..FILES {
                std::fs::create_dir(instance.path.join("dirs").join(format!("d{index}")))
                    .expect("the directory");
            }
            let (steps, signature) = staged_steps(&instance, "dirs").await;
            assert!(
                steps < 4 * FILES as u64,
                "the new directories took {steps} steps"
            );
            let staged = staged_state(&instance, signature).await;
            let adds = children_staged(&instance, &staged, "dirs", LoreNodeStagedAction::Add).await;
            assert_eq!(adds, FILES, "the new directories are staged");

            for index in (0..FILES).rev() {
                let dir = instance.path.join("nested").join(format!("d{index}"));
                std::fs::create_dir(&dir).expect("the directory");
                test_file_write(&dir.join("f"), b"f");
            }
            let (steps, signature) = staged_steps(&instance, "nested").await;
            assert!(
                steps < 4 * FILES as u64,
                "the new directories of a file took {steps} steps"
            );
            let staged = staged_state(&instance, signature).await;
            let adds =
                children_staged(&instance, &staged, "nested", LoreNodeStagedAction::Add).await;
            assert_eq!(adds, FILES, "the new directories of a file are staged");
            for index in 0..FILES {
                let dir = format!("nested/d{index}");
                let adds =
                    children_staged(&instance, &staged, &dir, LoreNodeStagedAction::Add).await;
                assert_eq!(adds, 1, "the file in {dir} is staged");
            }

            for index in 0..FILES {
                std::fs::remove_file(instance.path.join("gone").join(format!("f{index}")))
                    .expect("the delete on disk");
            }
            let (steps, signature) = staged_steps(&instance, "gone").await;
            assert!(
                steps < 4 * FILES as u64,
                "the deleted files took {steps} steps"
            );
            let staged = staged_state(&instance, signature).await;
            let deletes =
                children_staged(&instance, &staged, "gone", LoreNodeStagedAction::Delete).await;
            assert_eq!(deletes, FILES, "the deleted files are staged");

            for index in 0..FILES {
                std::fs::remove_file(instance.path.join(format!("emptied/d{index}/f")))
                    .expect("the delete on disk");
            }
            let (steps, signature) = staged_steps(&instance, "emptied").await;
            assert!(
                steps < 4 * FILES as u64,
                "the deleted files of directories took {steps} steps"
            );
            let staged = staged_state(&instance, signature).await;
            for index in 0..FILES {
                let dir = format!("emptied/d{index}");
                let deletes =
                    children_staged(&instance, &staged, &dir, LoreNodeStagedAction::Delete).await;
                assert_eq!(deletes, 1, "the file in {dir} is staged deleted");
            }
        })
        .await;
}
