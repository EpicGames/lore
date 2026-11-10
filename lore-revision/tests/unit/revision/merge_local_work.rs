// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! `lore branch merge` of a directory delete against local work in the directory.

use std::sync::Arc;

use lore_revision::branch;
use lore_revision::repository::load_filter;

use super::instance::Edit;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::run;
use super::instance::switch_to;
use super::instance::try_merge_branch;
use super::instance::working_tree;
use super::instance::write;

/// The files every fixture starts from, committed before an ignore rule names any of them.
const TREE: [&str; 3] = ["Dir/debug.log", "Dir/keep.txt", "Top.txt"];

/// A merge deleting a directory is refused where a committed file in it holds a local edit,
/// whether an ignore rule names the file, names its directory or names neither: the rules govern
/// only what the tree does not track, and the edit is the only copy of what it holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_merged_directory_delete_is_refused_over_an_edited_tracked_file() {
    for (rules, edited) in [
        ("*.log\n", "Dir/keep.txt"),
        ("*.log\n", "Dir/debug.log"),
        ("Dir/\n", "Dir/keep.txt"),
    ] {
        run(
            "a_merged_directory_delete_is_refused_over_an_edited_tracked_file",
            &TREE,
            move |mut instance| async move {
                commit_edits(&instance, &[Edit::Write(".loreignore", rules)], "Ignore").await;
                let filter = load_filter(&instance.path).expect("Failed to load the filter");
                instance.repository = Arc::new(instance.repository.to_filter_context(filter));
                create_branch(&instance, "feature").await;
                commit_edits(&instance, &[Edit::Delete("Dir")], "Delete").await;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                commit_edits(&instance, &[Edit::Write("Top.txt", "main")], "Main").await;
                write(&instance, edited, "edited locally");

                let refused = try_merge_branch(&instance, "feature")
                    .await
                    .expect_err("A merge does not delete local work");

                assert!(
                    refused.is_local_modifications(),
                    "{edited} under {rules:?}: the refusal names the local modification: {refused}"
                );
                assert_eq!(
                    working_tree(&instance.path).get(edited),
                    Some(&b"edited locally".to_vec()),
                    "{edited} under {rules:?}: the edit stays in place"
                );
            },
        )
        .await;
    }
}
