// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Which layer mounts lie at or below a path a reset names, and how a reset moves an entry back.

use lore_revision::file::reset::ResetOptions;
use lore_revision::file::reset::mount_at_or_below;
use lore_revision::file::reset::reset;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::util::path::RelativePath;

use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirty_moved;
use crate::dirty_move_fixture::removed_directory;
use crate::dirty_move_fixture::with_instance;
use crate::helper::relative_path;

/// Every mount lies below the root, which covers no path as a path.
#[test]
fn every_mount_lies_below_the_root() {
    assert!(mount_at_or_below(
        &RelativePath::new(),
        &relative_path("Tools/sdk")
    ));
}

/// A reset of the destination of a dirty move whose source directories the working tree no longer
/// holds creates them and moves the file back into them.
#[tokio::test]
async fn a_reset_moves_an_entry_back_into_directories_the_working_tree_lost() {
    with_instance(|instance| async move {
        committed(&instance, &["d/e/a", "other.txt"]).await;
        dirty_moved(&instance, "d/e/a", "b").await;
        removed_directory(&instance, "d");

        reset(
            instance.repository.clone(),
            &instance.write_token,
            LoreArray::from_vec(vec![LoreString::from(&instance.path.join("b"))]),
            LoreString::default(),
            ResetOptions::default(),
        )
        .await
        .expect("the reset");

        assert_eq!(
            std::fs::read(instance.path.join("d/e/a")).expect("d/e/a on disk"),
            b"d/e/a"
        );
        assert!(!instance.path.join("b").exists(), "the reset left b behind");
    })
    .await;
}

/// A reset of the destination of a dirty move of a directory whose source directories the working
/// tree no longer holds moves the directory back whole, with what it holds.
#[cfg(unix)]
#[tokio::test]
async fn a_reset_moves_a_directory_back_whole_into_directories_the_working_tree_lost() {
    use std::os::unix::fs::MetadataExt;

    with_instance(|instance| async move {
        committed(&instance, &["d/e/moved/a", "d/e/moved/b", "other.txt"]).await;
        dirty_moved(&instance, "d/e/moved", "b").await;
        removed_directory(&instance, "d");
        let inode = |path: &str| {
            std::fs::metadata(instance.path.join(path))
                .expect("the directory")
                .ino()
        };
        let before = inode("b");

        reset(
            instance.repository.clone(),
            &instance.write_token,
            LoreArray::from_vec(vec![LoreString::from(&instance.path.join("b"))]),
            LoreString::default(),
            ResetOptions::default(),
        )
        .await
        .expect("the reset");

        assert_eq!(inode("d/e/moved"), before);
        for file in ["d/e/moved/a", "d/e/moved/b"] {
            assert_eq!(
                std::fs::read(instance.path.join(file)).expect("the file on disk"),
                file.as_bytes()
            );
        }
        assert!(!instance.path.join("b").exists(), "the reset left b behind");
    })
    .await;
}
