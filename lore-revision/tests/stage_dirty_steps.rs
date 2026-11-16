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

/// The steps every thread's finds and children walks have taken.
#[derive(Clone, Copy)]
struct Steps {
    /// The steps of the finds by name.
    finds: u64,
    /// The steps of the children walks: the listings and the scans of a directory's children.
    walks: u64,
}

impl Steps {
    /// The steps taken so far.
    fn now() -> Self {
        step_counting::set_chain_step_counting(true);
        Self {
            finds: step_counting::total_chain_steps(),
            walks: step_counting::total_walk_steps(),
        }
    }

    /// The steps taken since `before`.
    fn since(before: Self) -> Self {
        let now = Self::now();
        Self {
            finds: now.finds - before.finds,
            walks: now.walks - before.walks,
        }
    }
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
    let (steps, signature) = stage_steps(instance, named(instance, dir)).await;
    (steps.finds, signature)
}

/// Stages `paths`, answering with the steps the stage took and the staged revision.
async fn stage_steps(instance: &TestRepository, paths: LoreArray<LoreString>) -> (Steps, Hash) {
    let before = Steps::now();
    let signature = lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        paths,
        StageOptions::default(),
    )
    .await
    .expect("the stage");
    (Steps::since(before), signature)
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

/// How many files the committed directory of the shared directories test holds.
const ENTRIES: usize = 4096;

/// How many new directories, each holding two new files, the shared directories test makes in
/// its committed directory.
const FOLDERS: usize = 64;

/// Writes [`FOLDERS`] new directories into `dir` below `instance`, each holding two new files,
/// answering with the paths of the files.
fn write_folders(instance: &TestRepository, dir: &str) -> Vec<LoreString> {
    let mut files = Vec::with_capacity(2 * FOLDERS);
    for index in (0..FOLDERS).rev() {
        let folder = instance.path.join(dir).join(format!("d{index}"));
        std::fs::create_dir(&folder).expect("the directory");
        for name in ["a", "b"] {
            test_file_write(&folder.join(name), name.as_bytes());
            files.push(LoreString::from(&folder.join(name)));
        }
    }
    files
}

/// Whether `dir` in `staged` holds [`FOLDERS`] directories staged as added, one per name, each
/// holding its two files staged as added.
async fn folders_staged(instance: &TestRepository, staged: &State, dir: &str) -> bool {
    if children_staged(instance, staged, dir, LoreNodeStagedAction::Add).await != FOLDERS {
        return false;
    }
    for index in 0..FOLDERS {
        let folder = format!("{dir}/d{index}");
        if children_staged(instance, staged, &folder, LoreNodeStagedAction::Add).await != 2 {
            return false;
        }
    }
    true
}

/// A stage of new directories in a committed directory, each holding two new files, creates the
/// directories from one listing of the committed directory, where a search or a scan of its chain
/// for each new directory costs N steps for N entries, and creates each directory once, with its
/// files, whether the stage names the files or the directory their dirty marks are below.
///
/// The bound of 4N steps for each of the finds and the walks of the stage of the marked files holds
/// a few listings of the directory, and a few steps for each new directory and file.
#[tokio::test]
async fn staging_new_directories_of_a_committed_directory_lists_it_once() {
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
            for dir in ["named", "marked"] {
                std::fs::create_dir_all(instance.path.join(dir)).expect("the directory");
                for index in 0..ENTRIES {
                    test_file_write(
                        &instance.path.join(dir).join(format!("f{index}")),
                        index.to_string().as_bytes(),
                    );
                }
            }
            test_commit_tree(&instance, "First").await;

            let files = write_folders(&instance, "named");
            let (_, signature) = stage_steps(&instance, LoreArray::from_vec(files)).await;
            let staged = staged_state(&instance, signature).await;
            assert!(
                folders_staged(&instance, &staged, "named").await,
                "the new directories of the named files are staged once, with their files"
            );

            write_folders(&instance, "marked");
            lore_revision::file::dirty::dirty(
                instance.repository.clone(),
                named(&instance, "marked"),
            )
            .await
            .expect("the dirty marks");
            let (steps, signature) = stage_steps(&instance, named(&instance, "marked")).await;
            let bound = 4 * (ENTRIES + FOLDERS) as u64;
            assert!(
                steps.finds < bound,
                "the marked files took {} find steps",
                steps.finds
            );
            assert!(
                steps.walks < bound,
                "the marked files took {} walk steps",
                steps.walks
            );
            let staged = staged_state(&instance, signature).await;
            assert!(
                folders_staged(&instance, &staged, "marked").await,
                "the new directories of the marked files are staged once, with their files"
            );
        })
        .await;
}
