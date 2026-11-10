// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A forced stage of a directory walks it on disk and stages everything in it, as a forced
//! `--scan` does, and an unforced one without `--scan` stages only what is marked dirty below it.
// A fixture builds the working tree directly; what these test is what the stage takes from it.
#![allow(clippy::disallowed_methods)]

use crate::helper::holds;
use crate::helper::ignoring_repository;
use crate::helper::in_execution;
use crate::helper::next_node;
use crate::helper::next_tree;
use crate::helper::stage_paths;
use crate::helper::test_file_write;

/// A forced stage of a committed directory deleted from the working tree, nothing below it marked
/// dirty, stages its deletion, with `--scan` or without.
#[tokio::test]
async fn a_forced_stage_of_a_deleted_directory_stages_its_deletion() {
    for scan in [false, true] {
        in_execution(async move {
            let (ruled, _, tempdir) = ignoring_repository(&["gen/a.gen"], "gen/\n").await;
            std::fs::remove_dir_all(tempdir.join("gen")).expect("Delete failed");
            stage_paths(&ruled, &tempdir, &["gen"], scan, true).await;
            assert!(
                next_node(&ruled, "gen/a.gen").await.is_staged_delete(),
                "a forced stage of gen, scanning {scan}, left the removal of gen/a.gen unstaged"
            );
        })
        .await;
    }
}

/// A forced stage of a committed empty directory below a directory the ignore file names, deleted
/// from the working tree, stages its deletion, with `--scan` or without.
#[tokio::test]
async fn a_forced_stage_of_a_deleted_empty_directory_below_an_ignored_one_stages_its_deletion() {
    for scan in [false, true] {
        in_execution(async move {
            let (ruled, _, tempdir) =
                ignoring_repository(&["Engine/Source/a.cs"], "/Engine/\n").await;
            std::fs::create_dir(tempdir.join("Engine/Empty")).expect("Create directory failed");
            stage_paths(&ruled, &tempdir, &["Engine/Empty"], false, true).await;
            lore_revision::commit::commit_boxed(
                ruled.clone(),
                ruled
                    .try_write_token()
                    .expect("The repository was opened for writing"),
                lore_revision::commit::CommitOptions::new("Empty".to_string()),
            )
            .await
            .expect("Commit failed");
            std::fs::remove_dir(tempdir.join("Engine/Empty")).expect("Delete failed");
            stage_paths(&ruled, &tempdir, &["Engine/Empty"], scan, true).await;
            assert!(
                next_node(&ruled, "Engine/Empty").await.is_staged_delete(),
                "a forced stage of Engine/Empty, scanning {scan}, left its removal unstaged"
            );
        })
        .await;
    }
}

/// A forced stage of a directory, or of the whole tree, stages an unchanged file the tree tracks
/// and a file the ignore file names that it does not, with `--scan` or without.
#[tokio::test]
async fn a_forced_stage_of_a_directory_stages_everything_in_it() {
    for name in [".", "gen"] {
        for scan in [false, true] {
            in_execution(async move {
                let (ruled, _, tempdir) = ignoring_repository(&["gen/keep.txt"], "*.log\n").await;
                test_file_write(&tempdir.join("gen/new.log"), b"untracked");
                stage_paths(&ruled, &tempdir, &[name], scan, true).await;
                assert!(
                    next_node(&ruled, "gen/keep.txt").await.is_staged(),
                    "a forced stage of {name}, scanning {scan}, left gen/keep.txt unstaged"
                );
                assert!(
                    next_node(&ruled, "gen/new.log").await.is_staged_add(),
                    "a forced stage of {name}, scanning {scan}, left gen/new.log unstaged"
                );
            })
            .await;
        }
    }
}

/// An unforced stage of a directory without `--scan` stages only what is marked dirty below it:
/// neither an unchanged file, nor a file the ignore file names, nor the removal of a directory
/// nothing marked.
#[tokio::test]
async fn an_unforced_stage_of_a_directory_stages_only_what_is_marked_dirty() {
    in_execution(async {
        let (ruled, _, tempdir) =
            ignoring_repository(&["gen/keep.txt", "gen/old/a.txt"], "*.log\n").await;
        test_file_write(&tempdir.join("gen/new.log"), b"untracked");
        std::fs::remove_dir_all(tempdir.join("gen/old")).expect("Delete failed");
        stage_paths(&ruled, &tempdir, &["gen"], false, false).await;
        let tree = next_tree(&ruled).await;
        assert!(
            !next_node(&ruled, "gen/keep.txt").await.is_staged(),
            "an unforced stage of gen staged the unchanged gen/keep.txt"
        );
        assert!(
            !next_node(&ruled, "gen/old/a.txt").await.is_staged_delete(),
            "an unforced stage of gen staged the removal of gen/old, which nothing marked"
        );
        assert!(
            !holds(&ruled, &tree, "gen/new.log").await,
            "an unforced stage of gen took in gen/new.log"
        );
    })
    .await;
}
