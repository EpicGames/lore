// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // The tests read the working tree's modes directly.

//! `lore branch merge` of a change of a file's executable bit on one branch against other changes
//! of the file on the other, as the working tree and the merge revision hold it.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use lore_revision::branch;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::metadata::MetadataInherit;
use lore_revision::node::NodeFileMode;
use lore_revision::repository::load_filter;
use lore_revision::revision::cherry_pick::CherryPickOptions;
use lore_revision::revision::cherry_pick::cherry_pick_boxed;
use lore_revision::revision::cherry_pick::cherry_pick_resolve_mine_boxed;
use lore_revision::revision::cherry_pick::cherry_pick_resolve_theirs_boxed;
use lore_revision::revision::revert::RevertOptions;
use lore_revision::revision::revert::revert_boxed;
use lore_revision::revision::revert::revert_resolve_mine_boxed;
use lore_revision::revision::revert::revert_resolve_theirs_boxed;
use lore_revision::revision::sync::SyncError;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage_and_commit;
use super::instance::start_merge;
use super::instance::switch_to;
use super::instance::sync_to;
use super::instance::try_merge_branch;
use super::instance::working_tree;
use super::instance::write;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 4] = ["Dir/a.txt", "Dir/b.txt", "Dir/Sub/c.txt", "Top.txt"];

/// Commits `base` on the main branch, then `feature` on a branch of that name and `main` on the
/// main branch, both made on `base`, and merges the feature branch into main. Answers whether the
/// merge committed.
async fn merge(instance: &Instance, base: &[Edit], feature: &[Edit], main: &[Edit]) -> bool {
    if !base.is_empty() {
        commit_edits(instance, base, "Base").await;
    }
    create_branch(instance, "feature").await;
    commit_edits(instance, feature, "Feature").await;
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    commit_edits(instance, main, "Main").await;
    merge_branch(instance, "feature").await
}

/// Whether the working tree's file at `path` and the file the merge revision holds there both
/// carry the executable bit, and what the working tree's file holds.
async fn merged_file(instance: &Instance, path: &str) -> (bool, bool, Vec<u8>) {
    let absolute = instance.path.join(path);
    let on_disk = std::fs::metadata(&absolute)
        .expect("Failed to read the file's mode")
        .permissions()
        .mode()
        & 0o100
        != 0;
    let state = revision_state(instance, current(instance).await).await;
    let node = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("Failed to find the file");
    let committed = state
        .node(instance.repository.clone(), node.node)
        .await
        .expect("Failed to read the file")
        .mode
        & NodeFileMode::Executable.bits()
        != 0;
    let content = std::fs::read(&absolute).expect("Failed to read the file");
    (on_disk, committed, content)
}

/// A branch giving a file the executable bit merges with one editing the file, moving it, renaming
/// it or its directory only in case, or moving and editing it, whichever of the two is merged into
/// the other: the working tree and the merge revision hold the file where the other branch left
/// it, with what it holds and the bit.
#[tokio::test(flavor = "multi_thread")]
async fn a_mode_change_merges_with_an_edit_or_a_move_of_the_file() {
    const CASES: [(&[Edit], &str, &str); 5] = [
        (&[Edit::Write("Dir/a.txt", "edited")], "Dir/a.txt", "edited"),
        (
            &[Edit::Move("Dir/a.txt", "Dir/z.txt")],
            "Dir/z.txt",
            "Dir/a.txt",
        ),
        (
            &[Edit::Move("Dir/a.txt", "Dir/A.txt")],
            "Dir/A.txt",
            "Dir/a.txt",
        ),
        (&[Edit::Move("Dir", "dir")], "dir/a.txt", "Dir/a.txt"),
        (
            &[
                Edit::Move("Dir/a.txt", "Dir/z.txt"),
                Edit::Write("Dir/z.txt", "edited"),
            ],
            "Dir/z.txt",
            "edited",
        ),
    ];
    for (other, merged_at, content) in &CASES {
        for chmod_on_feature in [true, false] {
            run(
                "a_mode_change_merges_with_an_edit_or_a_move_of_the_file",
                &TREE,
                move |instance| async move {
                    let chmod = [Edit::Chmod("Dir/a.txt", true)];
                    let (feature, main) = if chmod_on_feature {
                        (&chmod[..], *other)
                    } else {
                        (*other, &chmod[..])
                    };
                    let context = format!("{feature:?} into {main:?}");
                    assert!(
                        merge(&instance, &[], feature, main).await,
                        "{context}: the merge did not commit"
                    );
                    assert_eq!(
                        merged_file(&instance, merged_at).await,
                        (true, true, content.as_bytes().to_vec()),
                        "{context}"
                    );
                    assert_eq!(working_tree(&instance.path).len(), TREE.len(), "{context}");
                },
            )
            .await;
        }
    }
}

/// A branch editing a file and giving it the executable bit merges with one editing other lines of
/// the file, whichever of the two is merged into the other: the merge of the text joins the two
/// edits, and the working tree and the merge revision hold the joined file with the bit.
#[tokio::test(flavor = "multi_thread")]
async fn a_mode_change_with_an_edit_merges_into_a_merge_of_the_text() {
    const BASE: [Edit; 1] = [Edit::Write("Dir/a.txt", "1\n2\n3\n4\n5\n")];
    const CHMOD: [Edit; 2] = [
        Edit::Write("Dir/a.txt", "one\n2\n3\n4\n5\n"),
        Edit::Chmod("Dir/a.txt", true),
    ];
    const EDIT: [Edit; 1] = [Edit::Write("Dir/a.txt", "1\n2\n3\n4\nfive\n")];
    for (feature, main) in [(&CHMOD[..], &EDIT[..]), (&EDIT[..], &CHMOD[..])] {
        run(
            "a_mode_change_with_an_edit_merges_into_a_merge_of_the_text",
            &TREE,
            move |instance| async move {
                let context = format!("{feature:?} into {main:?}");
                assert!(
                    merge(&instance, &BASE, feature, main).await,
                    "{context}: the merge did not commit"
                );
                assert_eq!(
                    merged_file(&instance, "Dir/a.txt").await,
                    (true, true, b"one\n2\n3\n4\nfive\n".to_vec()),
                    "{context}"
                );
            },
        )
        .await;
    }
}

/// A branch giving a file the executable bit merges with one deleting the file, whichever of the
/// two is merged into the other: the merge deletes the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_mode_change_merges_with_a_delete_of_the_file() {
    const CHMOD: [Edit; 1] = [Edit::Chmod("Dir/a.txt", true)];
    const DELETE: [Edit; 1] = [Edit::Delete("Dir/a.txt")];
    for (feature, main) in [(&CHMOD[..], &DELETE[..]), (&DELETE[..], &CHMOD[..])] {
        run(
            "a_mode_change_merges_with_a_delete_of_the_file",
            &TREE,
            move |instance| async move {
                let context = format!("{feature:?} into {main:?}");
                assert!(
                    merge(&instance, &[], feature, main).await,
                    "{context}: the merge did not commit"
                );
                assert!(
                    !working_tree(&instance.path).contains_key("Dir/a.txt"),
                    "{context}"
                );
            },
        )
        .await;
    }
}

/// A branch moving a file and giving it the executable bit, merged into one editing another file,
/// stages the moved file with the bit, which a commit records where it reads none from the working
/// tree, as on a platform that holds no bit.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_with_a_mode_change_stages_the_moved_file_with_the_bit() {
    for moved_to in ["Dir/z.txt", "Dir/A.txt"] {
        run(
            "a_move_with_a_mode_change_stages_the_moved_file_with_the_bit",
            &TREE,
            move |instance| async move {
                create_branch(&instance, "feature").await;
                commit_edits(
                    &instance,
                    &[
                        Edit::Move("Dir/a.txt", moved_to),
                        Edit::Chmod(moved_to, true),
                    ],
                    "Feature",
                )
                .await;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                commit_edits(&instance, &[Edit::Write("Top.txt", "edited")], "Main").await;
                start_merge(&instance, "feature", true)
                    .await
                    .expect("Failed to merge");
                let staged = lore_revision::instance::load_staged_revision(&instance.repository)
                    .await
                    .expect("Failed to load the staged revision")
                    .expect("A merge left a staged revision");
                let state = revision_state(&instance, staged).await;
                let node = state
                    .find_node_link(instance.repository.clone(), moved_to)
                    .await
                    .expect("Failed to find the moved file");
                let mode = state
                    .node(instance.repository.clone(), node.node)
                    .await
                    .expect("Failed to read the moved file")
                    .mode;
                assert_ne!(
                    mode & NodeFileMode::Executable.bits(),
                    0,
                    "{moved_to}: {mode:o}"
                );
            },
        )
        .await;
    }
}

/// A branch giving a file the executable bit, merged into one editing another file, gives the
/// working tree's file the bit without writing it again: the file keeps its content and the time
/// it was last written.
#[tokio::test(flavor = "multi_thread")]
async fn a_mode_change_alone_merges_without_writing_the_file() {
    run(
        "a_mode_change_alone_merges_without_writing_the_file",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Chmod("Top.txt", true)], "Feature").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "edited")], "Main").await;
            let written =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30);
            std::fs::File::options()
                .write(true)
                .open(instance.path.join("Top.txt"))
                .and_then(|file| file.set_modified(written))
                .expect("Failed to set the file's time");
            assert!(
                merge_branch(&instance, "feature").await,
                "the merge did not commit"
            );
            assert_eq!(
                merged_file(&instance, "Top.txt").await,
                (true, true, b"Top.txt".to_vec())
            );
            let modified = std::fs::metadata(instance.path.join("Top.txt"))
                .and_then(|metadata| metadata.modified())
                .expect("Failed to read the file's time");
            assert_eq!(modified, written);
        },
    )
    .await;
}

/// A branch editing a file and giving it the executable bit, merged into one making the same edit,
/// gives the working tree's file the bit without writing it again: the file keeps the time it was
/// last written.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_edit_with_a_mode_change_merges_without_writing_the_file() {
    run(
        "the_same_edit_with_a_mode_change_merges_without_writing_the_file",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            commit_edits(
                &instance,
                &[Edit::Write("Top.txt", "same"), Edit::Chmod("Top.txt", true)],
                "Feature",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write("Top.txt", "same")], "Main").await;
            let written =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30);
            std::fs::File::options()
                .write(true)
                .open(instance.path.join("Top.txt"))
                .and_then(|file| file.set_modified(written))
                .expect("Failed to set the file's time");
            assert!(
                merge_branch(&instance, "feature").await,
                "the merge did not commit"
            );
            assert_eq!(
                merged_file(&instance, "Top.txt").await,
                (true, true, b"same".to_vec())
            );
            let modified = std::fs::metadata(instance.path.join("Top.txt"))
                .and_then(|metadata| metadata.modified())
                .expect("Failed to read the file's time");
            assert_eq!(modified, written);
        },
    )
    .await;
}

/// A branch deleting a file, then adding it again with the executable bit, merged into one editing
/// another file, takes the new file with its own bit: the working tree and the merge revision hold
/// it with the bit the old one lacked.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_added_again_merges_with_its_own_mode() {
    run(
        "a_file_added_again_merges_with_its_own_mode",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Delete("Top.txt")], "Delete").await;
            commit_edits(
                &instance,
                &[
                    Edit::Write("Top.txt", "added again"),
                    Edit::Chmod("Top.txt", true),
                ],
                "Add again",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "edited")], "Main").await;
            assert!(
                merge_branch(&instance, "feature").await,
                "the merge did not commit"
            );
            assert_eq!(
                merged_file(&instance, "Top.txt").await,
                (true, true, b"added again".to_vec())
            );
        },
    )
    .await;
}

/// Resolving a conflict of two edits of a file to one side chooses only the content: the working
/// tree and the merge revision hold the file with the bit the merge gives it, where the side
/// resolved against set the bit and where it took it away, resolving to mine and to theirs.
#[tokio::test(flavor = "multi_thread")]
async fn resolving_a_conflict_to_one_side_keeps_the_merged_mode() {
    type Case = (
        &'static [Edit],
        &'static [Edit],
        &'static [Edit],
        bool,
        bool,
    );
    const EXECUTABLE: [Edit; 1] = [Edit::Chmod("Dir/a.txt", true)];
    const CASES: [Case; 4] = [
        (
            &[],
            &[
                Edit::Write("Dir/a.txt", "theirs"),
                Edit::Chmod("Dir/a.txt", true),
            ],
            &[Edit::Write("Dir/a.txt", "mine")],
            true,
            true,
        ),
        (
            &[],
            &[Edit::Write("Dir/a.txt", "theirs")],
            &[
                Edit::Write("Dir/a.txt", "mine"),
                Edit::Chmod("Dir/a.txt", true),
            ],
            false,
            true,
        ),
        (
            &EXECUTABLE,
            &[
                Edit::Write("Dir/a.txt", "theirs"),
                Edit::Chmod("Dir/a.txt", false),
            ],
            &[Edit::Write("Dir/a.txt", "mine")],
            true,
            false,
        ),
        (
            &EXECUTABLE,
            &[Edit::Write("Dir/a.txt", "theirs")],
            &[
                Edit::Write("Dir/a.txt", "mine"),
                Edit::Chmod("Dir/a.txt", false),
            ],
            false,
            false,
        ),
    ];
    for (base, feature, main, mine, executable) in &CASES {
        run(
            "resolving_a_conflict_to_one_side_keeps_the_merged_mode",
            &TREE,
            move |instance| async move {
                let context = format!("{feature:?} into {main:?}, resolved to mine {mine}");
                assert!(
                    !merge(&instance, base, feature, main).await,
                    "{context}: the merge committed"
                );
                let path = instance.path.join("Dir/a.txt");
                let paths = LoreArray::from_vec(vec![LoreString::from(path.as_path())]);
                let resolved = if *mine {
                    branch::merge::merge_resolve_mine(
                        instance.repository.clone(),
                        &instance.write_token,
                        paths,
                    )
                    .await
                } else {
                    branch::merge::merge_resolve_theirs(
                        instance.repository.clone(),
                        &instance.write_token,
                        paths,
                    )
                    .await
                };
                resolved.expect("Failed to resolve the conflict");
                commit(&instance, "Resolved")
                    .await
                    .expect("Failed to commit the merge");
                let chosen: &[u8] = if *mine { b"mine" } else { b"theirs" };
                assert_eq!(
                    merged_file(&instance, "Dir/a.txt").await,
                    (*executable, *executable, chosen.to_vec()),
                    "{context}"
                );
            },
        )
        .await;
    }
}

/// What a test cherry-picks or reverts, and the side it resolves the conflict left to.
#[derive(Debug, Clone, Copy)]
enum Picked {
    CherryPickMine,
    CherryPickTheirs,
    RevertMine,
    RevertTheirs,
}

/// Resolving a conflict a cherry-pick or a revert leaves to one side chooses only the content, as
/// resolving a merge's does: the working tree and the revision committed hold the file with the
/// bit the cherry-pick or the revert gives it, where the side resolved against changed the bit.
#[tokio::test(flavor = "multi_thread")]
async fn resolving_a_cherry_pick_or_a_revert_to_one_side_keeps_the_merged_mode() {
    type Case = (
        Picked,
        &'static [Edit],
        &'static [Edit],
        (bool, &'static [u8]),
    );
    const CASES: [Case; 4] = [
        (
            Picked::CherryPickMine,
            &[
                Edit::Write("Dir/a.txt", "theirs"),
                Edit::Chmod("Dir/a.txt", true),
            ],
            &[Edit::Write("Dir/a.txt", "mine")],
            (true, b"mine"),
        ),
        (
            Picked::CherryPickTheirs,
            &[Edit::Write("Dir/a.txt", "theirs")],
            &[
                Edit::Write("Dir/a.txt", "mine"),
                Edit::Chmod("Dir/a.txt", true),
            ],
            (true, b"theirs"),
        ),
        (
            Picked::RevertMine,
            &[
                Edit::Write("Dir/a.txt", "reverted"),
                Edit::Chmod("Dir/a.txt", true),
            ],
            &[Edit::Write("Dir/a.txt", "mine")],
            (false, b"mine"),
        ),
        (
            Picked::RevertTheirs,
            &[Edit::Write("Dir/a.txt", "reverted")],
            &[
                Edit::Write("Dir/a.txt", "mine"),
                Edit::Chmod("Dir/a.txt", true),
            ],
            (true, b"Dir/a.txt"),
        ),
    ];
    for (picked, edits, main, (executable, content)) in &CASES {
        run(
            "resolving_a_cherry_pick_or_a_revert_to_one_side_keeps_the_merged_mode",
            &TREE,
            move |instance| async move {
                let context = format!("{picked:?} of {edits:?} against {main:?}");
                let repository = || instance.repository.clone();
                let token = &instance.write_token;
                let path = instance.path.join("Dir/a.txt");
                let paths = || LoreArray::from_vec(vec![LoreString::from(path.as_path())]);
                let resolved =
                    if matches!(picked, Picked::CherryPickMine | Picked::CherryPickTheirs) {
                        create_branch(&instance, "feature").await;
                        let revision = commit_edits(&instance, edits, "Picked").await;
                        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                        let before = commit_edits(&instance, main, "Main").await;
                        let options = CherryPickOptions {
                            message: "Cherry-pick".to_string(),
                            no_commit: false,
                            inherit_metadata: MetadataInherit::default(),
                        };
                        cherry_pick_boxed(repository(), token, revision, options)
                            .await
                            .expect("Failed to cherry-pick");
                        assert_eq!(current(&instance).await, before, "{context}: committed");
                        if matches!(picked, Picked::CherryPickMine) {
                            cherry_pick_resolve_mine_boxed(repository(), token, paths()).await
                        } else {
                            cherry_pick_resolve_theirs_boxed(repository(), token, paths()).await
                        }
                    } else {
                        let revision = commit_edits(&instance, edits, "Reverted").await;
                        let before = commit_edits(&instance, main, "Main").await;
                        let options = RevertOptions {
                            message: "Revert".to_string(),
                            no_commit: false,
                        };
                        revert_boxed(repository(), token, revision, options)
                            .await
                            .expect("Failed to revert");
                        assert_eq!(current(&instance).await, before, "{context}: committed");
                        if matches!(picked, Picked::RevertMine) {
                            revert_resolve_mine_boxed(repository(), token, paths()).await
                        } else {
                            revert_resolve_theirs_boxed(repository(), token, paths()).await
                        }
                    };
                resolved.expect("Failed to resolve the conflict");
                commit(&instance, "Resolved")
                    .await
                    .expect("Failed to commit the resolution");
                assert_eq!(
                    merged_file(&instance, "Dir/a.txt").await,
                    (*executable, *executable, content.to_vec()),
                    "{context}"
                );
            },
        )
        .await;
    }
}

/// Whether the working tree's file at `path` carries the executable bit.
fn executable_on_disk(instance: &Instance, path: &str) -> bool {
    std::fs::metadata(instance.path.join(path))
        .expect("Failed to read the file's mode")
        .permissions()
        .mode()
        & 0o100
        != 0
}

/// What brings in the edits a refused command would apply.
#[derive(Clone, Copy, Debug)]
enum Incoming {
    /// A merge of a branch committing the edits.
    Merge,
    /// A revert of a revision committing the edits, which takes away the bit they give.
    Revert,
}

/// A merge or a revert refused for a local edit leaves the working tree as it was: the executable
/// bit the merged branch gives another file stays off it, where the file holds the content it held
/// and where it already holds the content the merged branch gives it, and the bit the revert would
/// take away stays on.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_merge_leaves_the_bits_in_the_working_tree() {
    type Case = (
        Incoming,
        &'static [Edit],
        &'static [(&'static str, &'static str)],
    );
    const CASES: [Case; 3] = [
        (
            Incoming::Merge,
            &[
                Edit::Chmod("Top.txt", true),
                Edit::Write("Dir/a.txt", "feature"),
            ],
            &[("Dir/a.txt", "edited locally")],
        ),
        (
            Incoming::Merge,
            &[
                Edit::Write("Top.txt", "feature"),
                Edit::Chmod("Top.txt", true),
                Edit::Write("Dir/a.txt", "feature"),
            ],
            &[("Top.txt", "feature"), ("Dir/a.txt", "edited locally")],
        ),
        (
            Incoming::Revert,
            &[
                Edit::Chmod("Top.txt", true),
                Edit::Write("Dir/a.txt", "reverted"),
            ],
            &[("Dir/a.txt", "edited locally")],
        ),
    ];
    for (incoming, edits, local) in CASES {
        run(
            "a_refused_merge_leaves_the_bits_in_the_working_tree",
            &TREE,
            move |instance| async move {
                let reverted = match incoming {
                    Incoming::Merge => {
                        create_branch(&instance, "feature").await;
                        commit_edits(&instance, edits, "Feature").await;
                        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                        None
                    }
                    Incoming::Revert => Some(commit_edits(&instance, edits, "Reverted").await),
                };
                for (path, content) in local {
                    write(&instance, path, content);
                }
                let refused = match reverted {
                    None => try_merge_branch(&instance, "feature").await,
                    Some(revision) => {
                        let options = RevertOptions {
                            message: "Revert".to_string(),
                            no_commit: false,
                        };
                        revert_boxed(
                            instance.repository.clone(),
                            &instance.write_token,
                            revision,
                            options,
                        )
                        .await
                    }
                }
                .expect_err("The command does not write over local work");
                let context = format!("{incoming:?} of {edits:?}");
                assert!(refused.is_local_modifications(), "{context}: {refused}");
                assert_eq!(
                    executable_on_disk(&instance, "Top.txt"),
                    matches!(incoming, Incoming::Revert),
                    "{context}"
                );
            },
        )
        .await;
    }
}

/// Whether the file the staged revision holds at `path` carries the executable bit.
async fn staged_executable(instance: &Instance, path: &str) -> bool {
    let staged = lore_revision::instance::load_staged_revision(&instance.repository)
        .await
        .expect("Failed to load the staged revision")
        .expect("A merge left a staged revision");
    let state = revision_state(instance, staged).await;
    let node = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("Failed to find the file");
    state
        .node(instance.repository.clone(), node.node)
        .await
        .expect("Failed to read the file")
        .mode
        & NodeFileMode::Executable.bits()
        != 0
}

/// Resolving a conflict to a side that deleted the file and added another with the executable bit
/// stages the new file with its own bit, which a commit records where it reads no bit from the
/// working tree, as on Windows.
#[tokio::test(flavor = "multi_thread")]
async fn resolving_to_a_file_added_again_stages_its_own_mode() {
    run(
        "resolving_to_a_file_added_again_stages_its_own_mode",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Delete("Dir/a.txt")], "Delete").await;
            commit_edits(
                &instance,
                &[
                    Edit::Write("Dir/a.txt", "added again"),
                    Edit::Chmod("Dir/a.txt", true),
                ],
                "Add again",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "mine")], "Main").await;
            assert!(
                !merge_branch(&instance, "feature").await,
                "the merge committed"
            );
            let path = instance.path.join("Dir/a.txt");
            branch::merge::merge_resolve_theirs(
                instance.repository.clone(),
                &instance.write_token,
                LoreArray::from_vec(vec![LoreString::from(path.as_path())]),
            )
            .await
            .expect("Failed to resolve the conflict");
            assert!(executable_on_disk(&instance, "Dir/a.txt"));
            assert!(staged_executable(&instance, "Dir/a.txt").await);
        },
    )
    .await;
}

/// A resolve to one side of every path the merge touched never undoes a change of the executable
/// bit: a file the merged branch only gave the bit, merged cleanly beside a conflict elsewhere,
/// keeps it in the working tree and the merge revision when the whole tree is resolved to mine.
#[tokio::test(flavor = "multi_thread")]
async fn resolving_every_path_to_mine_keeps_a_bit_the_merge_gave_a_clean_file() {
    run(
        "resolving_every_path_to_mine_keeps_a_bit_the_merge_gave_a_clean_file",
        &TREE,
        |instance| async move {
            assert!(
                !merge(
                    &instance,
                    &[],
                    &[
                        Edit::Chmod("Top.txt", true),
                        Edit::Write("Dir/a.txt", "theirs")
                    ],
                    &[Edit::Write("Dir/a.txt", "mine")],
                )
                .await,
                "the merge committed"
            );
            branch::merge::merge_resolve_mine(
                instance.repository.clone(),
                &instance.write_token,
                LoreArray::from_vec(vec![LoreString::from(instance.path.as_path())]),
            )
            .await
            .expect("Failed to resolve the merge");
            commit(&instance, "Resolved")
                .await
                .expect("Failed to commit the merge");
            assert_eq!(
                merged_file(&instance, "Top.txt").await,
                (true, true, b"Top.txt".to_vec())
            );
            assert_eq!(
                merged_file(&instance, "Dir/a.txt").await,
                (false, false, b"mine".to_vec())
            );
        },
    )
    .await;
}

/// A conflict at a file the view keeps out of the working tree adopts the merged branch's edit, with
/// the executable bit the merge gives the file: against an edit that also gives the file the bit on
/// the branch merged into, the merge stages the merged branch's edit with the bit.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_out_of_view_is_adopted_with_the_merged_mode() {
    run(
        "an_edit_out_of_view_is_adopted_with_the_merged_mode",
        &TREE,
        |mut instance| async move {
            create_branch(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "feature")], "Feature").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(
                &instance,
                &[
                    Edit::Write("Dir/a.txt", "main"),
                    Edit::Chmod("Dir/a.txt", true),
                ],
                "Main",
            )
            .await;
            let dot = instance.path.join(lore_revision::repository::DOT_LORE);
            std::fs::write(dot.join(lore_revision::repository::VIEW_FILTER), "/Dir\n")
                .expect("Failed to write the view");
            let filter = load_filter(&instance.path).expect("Failed to load the filter");
            instance.repository = Arc::new(instance.repository.to_filter_context(filter));
            start_merge(&instance, "feature", true)
                .await
                .expect("Failed to merge");
            assert!(staged_executable(&instance, "Dir/a.txt").await);
        },
    )
    .await;
}

/// A conflict the branch diff's auto-resolve merges as text gives the file the executable bit the
/// merge gives it: a branch editing one line and giving the file the bit, diffed against one
/// editing another line, reports the joined file with the bit.
#[tokio::test(flavor = "multi_thread")]
async fn an_auto_resolved_conflict_takes_the_merged_mode() {
    run(
        "an_auto_resolved_conflict_takes_the_merged_mode",
        &TREE,
        |instance| async move {
            commit_edits(
                &instance,
                &[Edit::Write("Dir/a.txt", "1\n2\n3\n4\n5\n")],
                "Base",
            )
            .await;
            create_branch(&instance, "feature").await;
            let feature = commit_edits(
                &instance,
                &[
                    Edit::Write("Dir/a.txt", "one\n2\n3\n4\n5\n"),
                    Edit::Chmod("Dir/a.txt", true),
                ],
                "Feature",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let main = commit_edits(
                &instance,
                &[Edit::Write("Dir/a.txt", "1\n2\n3\n4\nfive\n")],
                "Main",
            )
            .await;
            let id = |name: &'static str| {
                let repository = instance.repository.clone();
                async move {
                    branch::resolve(repository, name)
                        .await
                        .expect("Failed to resolve the branch")
                        .id
                }
            };
            let diff = branch::diff3_collect(
                instance.repository.clone(),
                id("feature").await,
                feature,
                id(branch::DEFAULT_DEFAULT_NAME).await,
                main,
                None,
                false,
                true,
            )
            .await
            .expect("Failed to diff the branches");
            assert!(diff.conflicts.is_empty(), "{:?}", diff.conflicts);
            let resolved = diff
                .changes
                .iter()
                .find(|change| change.path().as_str() == "Dir/a.txt")
                .expect("The diff lists the file");
            assert!(resolved.flags.is_conflict_automerged());
            assert_eq!(
                resolved.to.mode & NodeFileMode::Executable.bits(),
                NodeFileMode::Executable.bits()
            );
        },
    )
    .await;
}

/// A sync refused for a local edit leaves the working tree as it was: the executable bit the
/// revision synced to gives another file stays off it, where the file holds the content it held
/// and where it already holds the content that revision gives it.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_sync_leaves_the_bits_in_the_working_tree() {
    type Case = (&'static [Edit], &'static [(&'static str, &'static str)]);
    const CASES: [Case; 2] = [
        (
            &[
                Edit::Chmod("Top.txt", true),
                Edit::Write("Dir/a.txt", "synced"),
            ],
            &[("Dir/a.txt", "edited locally")],
        ),
        (
            &[
                Edit::Write("Top.txt", "synced"),
                Edit::Chmod("Top.txt", true),
                Edit::Write("Dir/a.txt", "synced"),
            ],
            &[("Top.txt", "synced"), ("Dir/a.txt", "edited locally")],
        ),
    ];
    for (edits, local) in CASES {
        run(
            "a_refused_sync_leaves_the_bits_in_the_working_tree",
            &TREE,
            move |instance| async move {
                let base = current(&instance).await;
                let synced = commit_edits(&instance, edits, "Synced").await;
                sync_to(&instance, base)
                    .await
                    .expect("Failed to sync back to the base");
                for (path, content) in local {
                    write(&instance, path, content);
                }
                let refused = sync_to(&instance, synced)
                    .await
                    .expect_err("A sync does not write over local work");
                assert!(
                    matches!(refused, SyncError::LocalModifications(_)),
                    "{edits:?}: {refused}"
                );
                assert!(!executable_on_disk(&instance, "Top.txt"), "{edits:?}");
            },
        )
        .await;
    }
}

/// A sync gives every file whose executable bit alone it changes that bit, both ways, where it
/// changes more of them than it sets at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_carries_every_bit_it_changes() {
    run(
        "a_sync_carries_every_bit_it_changes",
        &TREE,
        |instance| async move {
            let paths: Vec<String> = (0..200)
                .map(|index| format!("Many/{index:03}.sh"))
                .collect();
            for path in &paths {
                write(&instance, path, path);
            }
            let base = stage_and_commit(&instance, &instance.path, "Base").await;
            for path in &paths {
                std::fs::set_permissions(
                    instance.path.join(path),
                    std::fs::Permissions::from_mode(0o755),
                )
                .expect("Failed to change the mode");
            }
            let synced = stage_and_commit(&instance, &instance.path, "Synced").await;
            for (revision, executable) in [(base, false), (synced, true)] {
                sync_to(&instance, revision)
                    .await
                    .expect("Failed to sync the bits");
                let missed: Vec<&String> = paths
                    .iter()
                    .filter(|path| executable_on_disk(&instance, path) != executable)
                    .collect();
                assert!(missed.is_empty(), "{executable}: {missed:?}");
            }
        },
    )
    .await;
}
