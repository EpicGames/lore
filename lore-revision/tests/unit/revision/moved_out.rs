// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // The tests move and edit files in the working tree directly.

//! Tests of a file that `lore file dirty move` moves out of a directory below the root, edited and
//! staged: the revision records the directory it left, the diff against its parent reports the
//! move, and a merge of the branch into its parent takes it.

use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::change::FileAction;
use lore_revision::file;
use lore_revision::stage::StageOptions;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::named;
use super::instance::node_at;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage;
use super::instance::switch_to;
use super::instance::write;
use super::merge_deletions::delta_entries;
use super::stale_directory::actions;
use super::stale_directory::changes_between;
use crate::dirty_move_fixture::node_held;

/// The files the tests start from, each holding its own path: the moved file beside one it leaves
/// in its directory, below the root.
const TREE: [&str; 4] = [
    "src/ex/walk.rs",
    "src/ex/keep.rs",
    "dst/other.rs",
    "Top.txt",
];

/// How the edited move reaches the staged changes.
#[derive(Clone, Copy, Debug)]
enum Staging {
    /// The destination marked dirty and staged by its path, with no scan.
    Destination,
    /// A scan of the working tree.
    Scan,
}

/// Moves `src/ex/walk.rs` to `dst/walk.rs` in the working tree, marks the move with
/// `lore file dirty move`, edits the moved file and stages it as `staging` says.
async fn dirty_moved_and_edited(instance: &Instance, staging: Staging) {
    let (from, to) = (
        instance.path.join("src/ex/walk.rs"),
        instance.path.join("dst/walk.rs"),
    );
    std::fs::rename(&from, &to).expect("rename");
    file::dirty::dirty_move(
        instance.repository.clone(),
        from.to_string_lossy().into_owned(),
        to.to_string_lossy().into_owned(),
    )
    .await
    .expect("Failed to mark the move dirty");
    write(instance, "dst/walk.rs", "edited");
    match staging {
        Staging::Destination => {
            file::dirty::dirty(instance.repository.clone(), named(instance, "dst/walk.rs"))
                .await
                .expect("Failed to mark the edit dirty");
            file::stage::stage(
                instance.repository.clone(),
                &instance.write_token,
                named(instance, "dst/walk.rs"),
                StageOptions::default(),
            )
            .await
            .expect("Failed to stage the destination");
        }
        Staging::Scan => stage(instance, &instance.path).await,
    }
}

/// Whether `revision` holds a node at `path`.
async fn holds(instance: &Instance, revision: Hash, path: &str) -> bool {
    node_held(
        &instance.repository,
        &*revision_state(instance, revision).await,
        path,
    )
    .await
    .is_some()
}

/// The commit of the move gives the directory the file left the hash and size of what it holds
/// now, records it and the moved file in its delta, and the diff against its parent reports the
/// move; a merge of the branch into its parent, which moved on meanwhile, leaves no file at the
/// path the move left. Nothing else in the directory changes, so only the move makes the commit
/// rehash it. The instance's directory is named for `name`, the test's.
async fn assert_move_out_recorded(name: &str, staging: Staging) {
    run(name, &TREE, move |instance| async move {
        create_branch(&instance, "mover").await;
        let parent = current(&instance).await;
        dirty_moved_and_edited(&instance, staging).await;
        let moved = commit(&instance, "Move")
            .await
            .expect("Failed to commit the move");

        let (before, after) = (
            node_at(&instance, parent, "src/ex").await,
            node_at(&instance, moved, "src/ex").await,
        );
        assert_ne!(after.address.hash, before.address.hash, "{staging:?}");
        assert_eq!(after.size, "src/ex/keep.rs".len() as u64, "{staging:?}");
        let recorded = delta_entries(&instance, moved).await;
        for path in ["src/ex", "dst/walk.rs"] {
            assert!(
                recorded.iter().any(|(_, held, ..)| held == path),
                "{staging:?}: {path} in {recorded:?}"
            );
        }
        let changes = changes_between(&instance, parent, moved).await;
        assert!(
            changes
                .iter()
                .any(|change| change.action == FileAction::Move
                    && change.path().as_str() == "dst/walk.rs"
                    && change.move_source().map(|from| from.as_str()) == Some("src/ex/walk.rs")),
            "{staging:?}: {:?}",
            actions(&changes)
        );

        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
        commit_edits(&instance, &[Edit::Write("Top.txt", "main")], "Main").await;
        assert!(merge_branch(&instance, "mover").await, "{staging:?}");
        let merged = current(&instance).await;
        assert!(
            !holds(&instance, merged, "src/ex/walk.rs").await,
            "{staging:?}"
        );
        assert!(holds(&instance, merged, "dst/walk.rs").await, "{staging:?}");
        assert!(
            !instance.path.join("src/ex/walk.rs").exists(),
            "{staging:?}"
        );
    })
    .await;
}

/// [`assert_move_out_recorded`], the destination staged by its path.
#[tokio::test]
async fn a_dirty_move_out_of_a_directory_staged_at_its_destination_changes_the_directory() {
    assert_move_out_recorded(
        "a_dirty_move_out_of_a_directory_staged_at_its_destination_changes_the_directory",
        Staging::Destination,
    )
    .await;
}

/// [`assert_move_out_recorded`], the move staged by a scan.
#[tokio::test]
async fn a_dirty_move_out_of_a_directory_staged_by_a_scan_changes_the_directory() {
    assert_move_out_recorded(
        "a_dirty_move_out_of_a_directory_staged_by_a_scan_changes_the_directory",
        Staging::Scan,
    )
    .await;
}
