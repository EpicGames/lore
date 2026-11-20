// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // Test fixtures rename files in the working tree directly.

//! A rename that changes only the case of a name, carried by a commit to every working tree that
//! syncs, switches or merges across it.

use std::collections::BTreeMap;

use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::filter::FilterMode;
use lore_revision::state;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::RENAMES_BELOW;
use super::instance::commit;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::names_along;
use super::instance::node_at;
use super::instance::renamed;
use super::instance::renamed_all;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage;
use super::instance::stage_and_commit;
use super::instance::switch_to;
use super::instance::sync_to;
use super::instance::working_tree;
use super::instance::write;
use crate::case_folding::case_folding;
use crate::case_folding::case_folding_address;
use crate::case_folding::children;
use crate::helper::test_reported;

/// The files every fixture starts from.
const TREE: [&str; 5] = [
    "Dir/Build.cs",
    "Dir/Sub/c.txt",
    "Dir/other.txt",
    ELSEWHERE,
    "Top.txt",
];

/// A file of [`TREE`] the fixtures edit where a change beside the rename is needed.
const ELSEWHERE: &str = "Elsewhere.txt";

/// The changes the revision diff reports from `from` to `to`, by action and path.
async fn reported(instance: &Instance, from: Hash, to: Hash) -> Vec<(String, String)> {
    let changes = state::diff_collect(
        instance.repository.clone(),
        revision_state(instance, from).await,
        instance.repository.clone(),
        revision_state(instance, to).await,
        None,
        FilterMode::Full,
    )
    .await
    .expect("Failed to diff the revisions");
    test_reported(&changes)
}

/// A revision whose only change renames `from` to `to`, a path differing only in case, commits
/// without forcing. A sync and a branch switch across it rename the path on disk and leave
/// everything else as it was, in either direction.
async fn sync_and_switch_across(name: &str, from: &'static str, to: &'static str) {
    run(name, &TREE, move |instance| async move {
        let first = current(&instance).await;
        let before = working_tree(&instance.path);
        create_branch(&instance, "feature").await;
        std::fs::rename(instance.path.join(from), instance.path.join(to)).expect("rename");
        let second = stage_and_commit(&instance, &instance.path, "Rename").await;
        let after = renamed(&before, from, to);
        assert_eq!(
            reported(&instance, first, second).await,
            vec![("V".to_string(), to.to_string())]
        );
        assert_synced_and_switched_across(&instance, (first, second), &before, &after).await;
    })
    .await;
}

/// Syncs the instance, on the feature branch at `second`, back to `first` and forward again, then
/// switches it to main, at `first`, and back to the feature branch, and checks that each leaves
/// the working tree holding `before` at `first` and `after` at `second`.
async fn assert_synced_and_switched_across(
    instance: &Instance,
    (first, second): (Hash, Hash),
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) {
    sync_to(instance, first).await.expect("Failed to sync");
    assert_eq!(&working_tree(&instance.path), before, "synced back");
    sync_to(instance, second).await.expect("Failed to sync");
    assert_eq!(&working_tree(&instance.path), after, "synced forward");

    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    assert_eq!(&working_tree(&instance.path), before, "switched to main");
    switch_to(instance, "feature").await;
    assert_eq!(
        &working_tree(&instance.path),
        after,
        "switched to the feature branch"
    );
}

/// A revision making each of [`RENAMES_BELOW`] reaches a sync and a branch switch across it in
/// either direction: neither leaves a path below a directory's other spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_below_a_case_only_rename_of_a_directory_reaches_sync_and_switch() {
    for (edits, renames) in &RENAMES_BELOW {
        run(
            "a_move_below_a_case_only_rename_of_a_directory_reaches_sync_and_switch",
            &TREE,
            move |instance| async move {
                let first = current(&instance).await;
                let before = working_tree(&instance.path);
                create_branch(&instance, "feature").await;
                let second = commit_edits(&instance, edits, "Rename").await;
                let after = renamed_all(&before, renames);
                assert_synced_and_switched_across(&instance, (first, second), &before, &after)
                    .await;
            },
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_of_a_file_reaches_sync_and_switch() {
    sync_and_switch_across(
        "a_case_only_rename_of_a_file_reaches_sync_and_switch",
        "Dir/Build.cs",
        "Dir/build.cs",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_of_a_directory_reaches_sync_and_switch() {
    sync_and_switch_across(
        "a_case_only_rename_of_a_directory_reaches_sync_and_switch",
        "Dir/Sub",
        "Dir/sub",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_of_a_top_level_file_reaches_sync_and_switch() {
    sync_and_switch_across(
        "a_case_only_rename_of_a_top_level_file_reaches_sync_and_switch",
        "Top.txt",
        "top.txt",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_of_a_top_level_directory_reaches_sync_and_switch() {
    sync_and_switch_across(
        "a_case_only_rename_of_a_top_level_directory_reaches_sync_and_switch",
        "Dir",
        "dir",
    )
    .await;
}

/// A branch renaming `from` to `to`, a path differing only in case, alongside an edit of another
/// file, merges into a branch that edited a third: the merge renames the path on disk and the
/// merge revision records the new name.
async fn merge_across(name: &str, from: &'static str, to: &'static str) {
    run(name, &TREE, move |instance| async move {
        let before = working_tree(&instance.path);
        create_branch(&instance, "feature").await;
        std::fs::rename(instance.path.join(from), instance.path.join(to)).expect("rename");
        write(&instance, ELSEWHERE, "feature");
        stage_and_commit(&instance, &instance.path, "Rename").await;

        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
        write(&instance, "Top.txt", "main");
        stage_and_commit(&instance, &instance.path.join("Top.txt"), "Edit on main").await;
        assert!(
            merge_branch(&instance, "feature").await,
            "the merge committed"
        );

        let mut expected = renamed(&before, from, to);
        expected.insert(ELSEWHERE.to_string(), b"feature".to_vec());
        expected.insert("Top.txt".to_string(), b"main".to_vec());
        assert_eq!(working_tree(&instance.path), expected);
        let names = names_along(&instance, current(&instance).await, to).await;
        assert_eq!(names, to.split('/').collect::<Vec<_>>());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_of_a_file_reaches_a_merge() {
    merge_across(
        "a_case_only_rename_of_a_file_reaches_a_merge",
        "Dir/Build.cs",
        "Dir/build.cs",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_of_a_directory_reaches_a_merge() {
    merge_across(
        "a_case_only_rename_of_a_directory_reaches_a_merge",
        "Dir/Sub",
        "Dir/sub",
    )
    .await;
}

/// A directory's address covers the case of its children's names, and differs from the address
/// the rule folding case gives the same children, whichever their case.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_address_covers_the_case_of_child_names() {
    run(
        "a_directory_address_covers_the_case_of_child_names",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            std::fs::rename(
                instance.path.join("Dir/Build.cs"),
                instance.path.join("Dir/build.cs"),
            )
            .expect("rename");
            let second = stage_and_commit(&instance, &instance.path, "Rename").await;

            let upper = node_at(&instance, first, "Dir").await.address.hash;
            let lower = node_at(&instance, second, "Dir").await.address.hash;
            assert_ne!(upper, lower);
            let state = revision_state(&instance, second).await;
            let directory = state
                .find_node_link(instance.repository.clone(), "Dir")
                .await
                .expect("Failed to find the directory")
                .node;
            let folded = case_folding_address(
                children(&instance.repository, &state, directory)
                    .await
                    .into_iter()
                    .map(|(_, node)| node)
                    .collect(),
            );
            assert_ne!(folded, upper);
            assert_ne!(folded, lower);
        },
    )
    .await;
}

/// A tree hashed under the rule folding case keeps working: lookups in it ignore case, a diff
/// against the same tree hashed with exact names reports nothing, and a case-only rename committed
/// on it reaches a sync, while the directory the commit leaves alone keeps its address.
#[tokio::test(flavor = "multi_thread")]
async fn a_tree_hashed_folding_case_carries_a_case_only_rename() {
    run(
        "a_tree_hashed_folding_case_carries_a_case_only_rename",
        &TREE,
        |instance| async move {
            let exact = current(&instance).await;
            let folding = case_folding(&instance.repository, &instance.write_token, exact).await;
            assert_ne!(
                node_at(&instance, folding, "Dir").await.address,
                node_at(&instance, exact, "Dir").await.address
            );
            assert!(node_at(&instance, folding, "dir/SUB/C.txt").await.is_file());
            assert_eq!(reported(&instance, folding, exact).await, vec![]);
            assert_eq!(reported(&instance, exact, folding).await, vec![]);

            branch::store_latest(
                instance.repository.clone(),
                instance.main,
                exact,
                folding,
                BranchLatestStatus::Divergent,
            )
            .await
            .expect("Failed to move the branch");
            let before = working_tree(&instance.path);
            sync_to(&instance, folding).await.expect("Failed to sync");
            assert_eq!(
                working_tree(&instance.path),
                before,
                "the sync changes nothing"
            );
            std::fs::rename(
                instance.path.join("Dir/Build.cs"),
                instance.path.join("Dir/build.cs"),
            )
            .expect("rename");
            let renamed_revision = stage_and_commit(&instance, &instance.path, "Rename").await;
            assert_eq!(
                reported(&instance, folding, renamed_revision).await,
                vec![("V".to_string(), "Dir/build.cs".to_string())]
            );
            assert_eq!(
                node_at(&instance, renamed_revision, "Dir/Sub")
                    .await
                    .address,
                node_at(&instance, folding, "Dir/Sub").await.address
            );

            sync_to(&instance, folding).await.expect("Failed to sync");
            assert_eq!(working_tree(&instance.path), before, "synced back");
            sync_to(&instance, renamed_revision)
                .await
                .expect("Failed to sync");
            assert_eq!(
                working_tree(&instance.path),
                renamed(&before, "Dir/Build.cs", "Dir/build.cs"),
                "synced forward"
            );
        },
    )
    .await;
}

/// On a tree hashed under the rule folding case, a staged edit and its revert commit once, as the
/// commit rehashes the directories above the file with exact names, and record no change. The next
/// such edit and revert find nothing staged.
#[tokio::test(flavor = "multi_thread")]
async fn a_tree_hashed_folding_case_commits_an_unchanged_file_once() {
    run(
        "a_tree_hashed_folding_case_commits_an_unchanged_file_once",
        &TREE,
        |instance| async move {
            let exact = current(&instance).await;
            let folding = case_folding(&instance.repository, &instance.write_token, exact).await;
            branch::store_latest(
                instance.repository.clone(),
                instance.main,
                exact,
                folding,
                BranchLatestStatus::Divergent,
            )
            .await
            .expect("Failed to move the branch");
            sync_to(&instance, folding).await.expect("Failed to sync");
            let file = instance.path.join("Dir/Build.cs");

            write(&instance, "Dir/Build.cs", "edited");
            stage(&instance, &file).await;
            write(&instance, "Dir/Build.cs", "Dir/Build.cs");
            let rehashed = stage_and_commit(&instance, &file, "Edit and revert").await;
            assert_ne!(rehashed, folding);
            assert_eq!(reported(&instance, folding, rehashed).await, vec![]);

            write(&instance, "Dir/Build.cs", "edited");
            stage(&instance, &file).await;
            write(&instance, "Dir/Build.cs", "Dir/Build.cs");
            stage(&instance, &file).await;
            let refused = commit(&instance, "Edit and revert again").await;
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_nothing_staged()),
                "{refused:?}"
            );
        },
    )
    .await;
}

/// A sync back across a revision renaming a directory only in case and moving and editing a file
/// below it keeps the executable bit the working tree alone gave the file: the file is written at
/// the path it moves back to with the bit, and nothing stays below the directory's new spelling.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_back_across_a_move_below_a_case_only_rename_keeps_a_local_bit() {
    use std::os::unix::fs::PermissionsExt;

    run(
        "a_sync_back_across_a_move_below_a_case_only_rename_keeps_a_local_bit",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            let before = working_tree(&instance.path);
            let edits = [
                Edit::Move("Dir", "dir"),
                Edit::Move("dir/Sub/c.txt", "dir/Sub/d.txt"),
                Edit::Write("dir/Sub/d.txt", "edited"),
            ];
            commit_edits(&instance, &edits, "Rename").await;
            std::fs::set_permissions(
                instance.path.join("dir/Sub/d.txt"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("Failed to change the mode");

            sync_to(&instance, first).await.expect("Failed to sync");
            assert_eq!(working_tree(&instance.path), before);
            let mode = std::fs::metadata(instance.path.join("Dir/Sub/c.txt"))
                .expect("Failed to read the file")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "{mode:o}");
        },
    )
    .await;
}
