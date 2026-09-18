// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The two ways a caller reads a filesystem diff, held to the same answer.
//!
//! Collecting gives each spawned subtree its own vector and folds it into the parent's on join;
//! reading one change at a time hands every subtree a clone of the sender and folds nothing.
//! Those are the only paths that differ, so a walk over a tree deep enough to spawn subtrees
//! reports the same set of changes either way. Order does not carry across: a collected walk
//! keeps a subtree's changes contiguous, and a read one interleaves them by completion.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::change::NodeChange;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::stage::StageOptions;

    include!("helper.rs");

    /// What a change reports about a path, which is what reading has to carry across unchanged.
    fn reported(changes: &mut [NodeChange]) -> Vec<(String, String, u16)> {
        lore_revision::change::sort_by_path(changes);
        changes
            .iter()
            .map(|change| {
                (
                    change.path().as_str().to_string(),
                    change.action.as_string_short().to_string(),
                    change.flags.bits(),
                )
            })
            .collect()
    }

    /// A tree spread over subdirectories, which is what makes the walk spawn subtree tasks:
    /// a directory is where a walk recurses, and the two paths differ only in what a spawned
    /// recursion emits through.
    fn write_tree(root: &std::path::Path) {
        for directory in 0..6 {
            let directory = root.join(format!("dir{directory}"));
            std::fs::create_dir_all(&directory).expect("Create directory failed");
            for file in 0..4 {
                test_file_write(
                    directory.join(format!("file{file}.txt")).as_path(),
                    format!("contents {file}").as_bytes(),
                );
            }
        }
    }

    /// A tree with an add, a modification and a deletion under every kind of subtree, walked
    /// twice: the answer is the change set, and reading is only how it is carried.
    #[tokio::test]
    async fn collecting_and_reading_report_the_same_changes() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let path = fixture.path.clone();

                write_tree(&path);
                file::stage::stage(
                    repository.clone(),
                    &fixture.write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        scan: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to stage the fixture");
                commit::commit_boxed(
                    repository.clone(),
                    &fixture.write_token,
                    CommitOptions::new("Initial".to_string()),
                )
                .await
                .expect("Commit failed");

                test_file_write(path.join("dir0").join("file0.txt").as_path(), b"edited");
                std::fs::remove_file(path.join("dir1").join("file1.txt"))
                    .expect("Remove file failed");
                test_file_write(path.join("dir2").join("added.txt").as_path(), b"new");
                std::fs::create_dir_all(path.join("dir3").join("subdir"))
                    .expect("Create directory failed");
                test_file_write(
                    path.join("dir3").join("subdir").join("added.txt").as_path(),
                    b"new",
                );

                let (current, staged) = test_anchor_states(&repository).await;
                let mut collected = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current,
                    lore_revision::fs::filesystem_provider::FilesystemDiffIntent::Report,
                )
                .await;

                let (current, staged) = test_anchor_states(&repository).await;
                let mut read = test_scan_streaming(repository.clone(), staged, current).await;

                assert!(
                    !collected.is_empty(),
                    "the fixture's edits must report changes"
                );
                assert_eq!(
                    reported(&mut collected),
                    reported(&mut read),
                    "reading a diff one change at a time must report what collecting does"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
