// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The file-history shortcut of the test for a file ID a merge kept: whether the base's line held
//! the side's file at its path before it added its own there, with a file history naming the
//! revision that last changed the side's file.

use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::change::NodeChange;
use lore_revision::diff::diff_revision_paths;
use lore_revision::revision::ChangedNodes;
use lore_revision::revision::REPLACED_LOOKBACK;
use lore_revision::revision::held_before_replacement;
use lore_revision::state::State;
use lore_revision::util::collect_stream::collect_stream_with_summary;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::run;
use super::instance::switch_to;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 2] = ["Dir/c.txt", "Top.txt"];

/// The file the base's line deletes and adds again.
const ADDED_AGAIN: &str = "Dir/c.txt";

/// The change at [`ADDED_AGAIN`] of a diff of `base` and `side`, which replaces the file's ID
/// alone.
async fn churn(instance: &Instance, base: Hash, side: Hash) -> NodeChange {
    let (from, to) = (
        State::deserialize(instance.repository.clone(), base)
            .await
            .expect("Failed to read the base"),
        State::deserialize(instance.repository.clone(), side)
            .await
            .expect("Failed to read the side"),
    );
    let repository = instance.repository.clone();
    let (_, changes) =
        collect_stream_with_summary(|tx| diff_revision_paths(repository, from, to, None, tx))
            .await
            .expect("Failed to diff the revisions");
    let change = changes
        .into_iter()
        .find(|change| change.path().as_str() == ADDED_AGAIN)
        .expect("A change at the file");
    assert_eq!(
        change.from.address.hash, change.to.address.hash,
        "the same content"
    );
    assert_ne!(
        change.from.address.context, change.to.address.context,
        "another file ID"
    );
    change
}

/// Whether the shortcut answers that `revision` is in the history of the base of `change`.
async fn held(change: &NodeChange, revision: Hash) -> bool {
    held_before_replacement(change, revision, &mut ChangedNodes::default()).await
}

/// Deletes [`ADDED_AGAIN`] and adds it again holding `content`, in two revisions, and answers
/// the second, or a revision above it editing `Top.txt` where `moved`, so that the base's delta
/// holds no change of the file and its file history names the add.
async fn delete_and_add_again(instance: &Instance, content: &'static str, moved: bool) -> Hash {
    commit_edits(instance, &[Edit::Delete(ADDED_AGAIN)], "Delete").await;
    let added = commit_edits(instance, &[Edit::Write(ADDED_AGAIN, content)], "Add again").await;
    if moved {
        return commit_edits(instance, &[Edit::Write("Top.txt", "moved")], "Moved").await;
    }
    added
}

/// The revision that last changed the side's file is the one the base's line held it in below the
/// add, whose delta changed it: the shortcut answers it is in the base's history, finding the add
/// in the base's delta or in the base file's history.
#[tokio::test(flavor = "multi_thread")]
async fn the_held_revision_that_changed_the_file_is_in_the_base_history() {
    for moved in [false, true] {
        run(
            "the_held_revision_that_changed_the_file_is_in_the_base_history",
            &TREE,
            move |instance| async move {
                let edited =
                    commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, "edited")], "Edit").await;
                let base = delete_and_add_again(&instance, "edited", moved).await;

                assert!(
                    held(&churn(&instance, base, edited).await, edited).await,
                    "moved {moved}"
                );
            },
        )
        .await;
    }
}

/// The revision that last changed the side's file is one the history of the file the base's line
/// held below the add names: the shortcut answers it is in the base's history, finding the add in
/// the base's delta or in the base file's history.
#[tokio::test(flavor = "multi_thread")]
async fn a_revision_the_held_file_history_names_is_in_the_base_history() {
    for moved in [false, true] {
        run(
            "a_revision_the_held_file_history_names_is_in_the_base_history",
            &TREE,
            move |instance| async move {
                let edited =
                    commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, "edited")], "Edit").await;
                let side = commit_edits(&instance, &[Edit::Write("Top.txt", "side")], "Top").await;
                let base = delete_and_add_again(&instance, "edited", moved).await;

                assert!(
                    held(&churn(&instance, base, side).await, edited).await,
                    "moved {moved}"
                );
            },
        )
        .await;
    }
}

/// A base whose file came through a merge, in the base's delta or in its file history, leaves the
/// question to the walk.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_added_through_a_merge_defers_to_the_walk() {
    for moved in [false, true] {
        run(
            "a_file_added_through_a_merge_defers_to_the_walk",
            &TREE,
            move |instance| async move {
                let first = current(&instance).await;
                create_branch(&instance, "added-again").await;
                delete_and_add_again(&instance, ADDED_AGAIN, false).await;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                assert!(
                    merge_branch(&instance, "added-again").await,
                    "the merge did not commit"
                );
                let mut base = current(&instance).await;
                if moved {
                    base =
                        commit_edits(&instance, &[Edit::Write("Top.txt", "moved")], "Moved").await;
                }

                assert!(
                    !held(&churn(&instance, base, first).await, first).await,
                    "moved {moved}"
                );
            },
        )
        .await;
    }
}

/// A base's line that held nothing at the path in the [`REPLACED_LOOKBACK`] revisions below the add
/// leaves the question to the walk, as one whose history starts within them does, and one that
/// held the side's file in the last of them answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_line_holding_nothing_within_the_lookback_defers_to_the_walk() {
    for (between, answers) in [
        (REPLACED_LOOKBACK - 1, false),
        (REPLACED_LOOKBACK - 2, true),
    ] {
        run(
            "a_line_holding_nothing_within_the_lookback_defers_to_the_walk",
            &TREE,
            move |instance| async move {
                let first = current(&instance).await;
                commit_edits(&instance, &[Edit::Delete(ADDED_AGAIN)], "Delete").await;
                for index in 0..between {
                    let content = if index % 2 == 0 { "between" } else { "Top.txt" };
                    commit_edits(&instance, &[Edit::Write("Top.txt", content)], "Between").await;
                }
                let base =
                    commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, ADDED_AGAIN)], "Add").await;

                assert_eq!(
                    held(&churn(&instance, base, first).await, first).await,
                    answers,
                    "{between} revisions between"
                );
            },
        )
        .await;
    }
    run(
        "a_line_holding_nothing_within_the_lookback_defers_to_the_walk",
        &["Top.txt"],
        |instance| async move {
            create_branch(&instance, "side").await;
            let side =
                commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, ADDED_AGAIN)], "Side").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let base =
                commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, ADDED_AGAIN)], "Add").await;

            assert!(!held(&churn(&instance, base, side).await, side).await);
        },
    )
    .await;
}

/// A base's line that held another file at the path below the add, from a delete and add of its
/// own, leaves the question to the walk.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_file_with_another_file_id_defers_to_the_walk() {
    run(
        "a_held_file_with_another_file_id_defers_to_the_walk",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            delete_and_add_again(&instance, ADDED_AGAIN, false).await;
            let base = delete_and_add_again(&instance, ADDED_AGAIN, false).await;

            assert!(!held(&churn(&instance, base, first).await, first).await);
        },
    )
    .await;
}

/// A side's file last changed in a revision the held file's history does not name, as where the
/// base's line edited the file and edited it back after it, leaves the question to the walk.
#[tokio::test(flavor = "multi_thread")]
async fn a_revision_the_held_file_history_does_not_name_defers_to_the_walk() {
    run(
        "a_revision_the_held_file_history_does_not_name_defers_to_the_walk",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, "edited")], "Edit").await;
            commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, ADDED_AGAIN)], "Back").await;
            let base = delete_and_add_again(&instance, ADDED_AGAIN, false).await;

            assert!(!held(&churn(&instance, base, first).await, first).await);
        },
    )
    .await;
}

/// A read of the base's line that fails leaves the question to the walk: the same question answers
/// where the reads go to the repository that holds the revisions.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_that_fails_defers_to_the_walk() {
    run(
        "a_read_that_fails_defers_to_the_walk",
        &TREE,
        |instance| async move {
            let edited =
                commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, "edited")], "Edit").await;
            let base = delete_and_add_again(&instance, "edited", false).await;
            let mut change = churn(&instance, base, edited).await;
            assert!(held(&change, edited).await, "the reads succeed");

            let (immutable, mutable, _execution) =
                test_store_create().await.expect("Failed to create stores");
            change.from.mapping.repository = in_memory::repository(immutable, mutable);
            change.from.mapping.state = State::deserialize(instance.repository.clone(), base)
                .await
                .expect("Failed to read the base");

            assert!(!held(&change, edited).await, "the reads fail");
        },
    )
    .await;
}
