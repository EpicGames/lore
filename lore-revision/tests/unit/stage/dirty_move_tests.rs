// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A stage of a move `lore file dirty move` marked, by the name of its destination, of the path it
//! left or of a directory above them, with or without `--scan`, and the commit that takes it as a
//! move.

use lore_revision::change::FileAction;
use lore_revision::change::Flags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::stage::StageOptions;
use lore_revision::state::State;

use super::staged_as_marked;
use super::staged_with;
use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirtied;
use crate::dirty_move_fixture::dirty_moved;
use crate::dirty_move_fixture::holds;
use crate::dirty_move_fixture::made_directory;
use crate::dirty_move_fixture::node_id_at;
use crate::dirty_move_fixture::node_of;
use crate::dirty_move_fixture::status_changes;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::helper::TestRepository;
use crate::helper::test_commit;
use crate::helper::test_file_write;

/// How a test stages the working tree.
#[derive(Clone, Copy, Debug)]
enum StagedBy {
    /// The destination of the move, by name, without `--scan`.
    Destination,
    /// The whole working tree, from its dirty marks.
    Whole,
    /// The whole working tree, with `--scan`.
    Scan,
}

const STAGES: [StagedBy; 3] = [StagedBy::Destination, StagedBy::Whole, StagedBy::Scan];

/// Stages `instance`'s working tree as `by` says, `destination` being the path the move took the
/// node to.
async fn stage_by(instance: &TestRepository, by: StagedBy, destination: &str) {
    match by {
        StagedBy::Destination => staged_as_marked(instance, &[destination]).await,
        StagedBy::Whole => staged_as_marked(instance, &[""]).await,
        StagedBy::Scan => {
            staged_with(
                instance,
                &[""],
                StageOptions {
                    scan: true,
                    ..Default::default()
                },
            )
            .await;
        }
    }
}

/// The staged State the instance holds.
async fn staged_state(instance: &TestRepository) -> std::sync::Arc<State> {
    let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    staged.expect("a staged State")
}

/// Asserts that `revision` records `node` in its delta as a move, with modify set where `modified`.
async fn assert_recorded_move(
    instance: &TestRepository,
    revision: &State,
    node: NodeID,
    modified: bool,
    case: &str,
) {
    let delta = revision
        .node_change(instance.repository.clone(), node)
        .await
        .expect("the delta")
        .unwrap_or_else(|| panic!("{case}: the delta names the moved node"));
    assert_eq!(delta.action, FileAction::Move as u16, "{case}: {delta:?}");
    assert_eq!(
        Flags::from_bits_retain(delta.flags).contains(Flags::Modify),
        modified,
        "{case}: {delta:?}"
    );
}

/// Asserts that a status of the instance after the commit of `revision` reports nothing.
async fn assert_nothing_left(
    instance: &TestRepository,
    revision: &std::sync::Arc<State>,
    case: &str,
) {
    let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    if let Some(staged) = staged {
        let changes = status_changes(instance, revision, &staged).await;
        assert!(changes.is_empty(), "{case}: {changes:?}");
    }
}

/// A file moved and marked with `lore file dirty move`, its content unchanged or edited, is staged
/// as a move, carrying the edit as a modification, whether the stage names its destination or
/// takes the whole tree, with or without a scan; the commit records the move, with modify set
/// where the content changed, and leaves nothing for a status to report.
#[tokio::test]
async fn a_stage_takes_a_dirty_moved_file_as_a_move() {
    for edited in [false, true] {
        for by in STAGES {
            with_instance(|instance| async move {
                let case = format!("edited {edited}, staged by {by:?}");
                let first = committed(&instance, &["src/ex/walk.rs", "dst/other.txt"]).await;
                let moved = node_id_at(&instance, &first, "src/ex/walk.rs").await;
                dirty_moved(&instance, "src/ex/walk.rs", "dst/walk.rs").await;
                if edited {
                    test_file_write(&instance.path.join("dst/walk.rs"), b"walk, edited");
                    dirtied(&instance, "dst/walk.rs").await;
                }

                stage_by(&instance, by, "dst/walk.rs").await;
                let staged = staged_state(&instance).await;
                let staged = node_of(&instance, &staged, moved).await;
                let modify = NodeFlags::StagedModify.bits();
                assert!(
                    staged.is_staged_move() && (staged.flags & modify == modify) == edited,
                    "{case}: flags {:#x}",
                    staged.flags
                );

                let revision = test_commit(&instance, "Moved").await;
                assert_eq!(
                    node_id_at(&instance, &revision, "dst/walk.rs").await,
                    moved,
                    "{case}"
                );
                assert!(
                    !holds(&instance, &revision, "src/ex/walk.rs").await,
                    "{case}"
                );
                assert_recorded_move(&instance, &revision, moved, edited, &case).await;
                assert_nothing_left(&instance, &revision, &case).await;
            })
            .await;
        }
    }
}

/// A directory holding a file, moved and marked with `lore file dirty move`, is staged as a move
/// whether the stage names its destination or the path it left, or takes the whole tree, with or
/// without a scan: the revision holds the directory and its file at the new path, and nothing at
/// the old one.
#[tokio::test]
async fn a_stage_takes_a_dirty_moved_directory_as_a_move() {
    for (by, named) in [
        (StagedBy::Destination, "e"),
        (StagedBy::Destination, "d"),
        (StagedBy::Whole, "e"),
        (StagedBy::Scan, "e"),
    ] {
        with_instance(|instance| async move {
            let case = format!("staged by {by:?} {named}");
            let first = committed(&instance, &["d/k.txt", "other.txt"]).await;
            let directory = node_id_at(&instance, &first, "d").await;
            let file = node_id_at(&instance, &first, "d/k.txt").await;
            dirty_moved(&instance, "d", "e").await;

            stage_by(&instance, by, named).await;
            let revision = test_commit(&instance, "Moved").await;

            assert_eq!(
                node_id_at(&instance, &revision, "e").await,
                directory,
                "{case}"
            );
            assert_eq!(
                node_id_at(&instance, &revision, "e/k.txt").await,
                file,
                "{case}"
            );
            assert!(!holds(&instance, &revision, "d").await, "{case}");
            assert_recorded_move(&instance, &revision, directory, false, &case).await;
        })
        .await;
    }
}

/// A directory holding no file, empty or holding only empty directories, moved and marked with
/// `lore file dirty move`, is staged as a move by a stage naming its destination or the path it
/// left, or of the whole tree, with or without a scan: the revision holds it, and what it holds,
/// at the new path, and nothing at the old one, and a status reports nothing left.
#[tokio::test]
async fn a_stage_takes_a_dirty_moved_directory_holding_no_file_as_a_move() {
    let below: [&[&str]; 2] = [&[], &["Sub", "Other"]];
    for held in below {
        for (by, named) in [
            (StagedBy::Destination, "e"),
            (StagedBy::Destination, "d"),
            (StagedBy::Whole, "e"),
            (StagedBy::Scan, "e"),
        ] {
            with_instance(|instance| async move {
                let case = format!("holding {held:?}, staged by {by:?} {named}");
                made_directory(&instance, "d");
                for directory in held {
                    made_directory(&instance, &format!("d/{directory}"));
                }
                let first = committed(&instance, &["Top.txt"]).await;
                let directory = node_id_at(&instance, &first, "d").await;
                dirty_moved(&instance, "d", "e").await;

                stage_by(&instance, by, named).await;
                let revision = test_commit(&instance, "Moved").await;

                assert_eq!(
                    node_id_at(&instance, &revision, "e").await,
                    directory,
                    "{case}"
                );
                for name in held {
                    assert!(
                        holds(&instance, &revision, &format!("e/{name}")).await,
                        "{case}"
                    );
                }
                assert!(!holds(&instance, &revision, "d").await, "{case}");
                assert_recorded_move(&instance, &revision, directory, false, &case).await;
                assert_nothing_left(&instance, &revision, &case).await;
            })
            .await;
        }
    }
}

/// A stage of one unchanged file of a directory moved and marked with `lore file dirty move`
/// stages the directory's move, and with it each other file that moves with it alone: the commit
/// records both files moved.
#[tokio::test]
async fn a_stage_of_one_file_of_a_dirty_moved_directory_stages_its_whole_move() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/k.txt", "d/other.txt", "Top.txt"]).await;
        let k = node_id_at(&instance, &first, "d/k.txt").await;
        let other = node_id_at(&instance, &first, "d/other.txt").await;
        dirty_moved(&instance, "d", "e").await;

        staged_as_marked(&instance, &["e/k.txt"]).await;
        let staged = staged_state(&instance).await;
        for node in [k, other] {
            let node = node_of(&instance, &staged, node).await;
            assert!(node.is_staged_move(), "flags {:#x}", node.flags);
        }

        let revision = test_commit(&instance, "Moved").await;
        assert_eq!(node_id_at(&instance, &revision, "e/other.txt").await, other);
        assert!(!holds(&instance, &revision, "d").await);
        for node in [k, other] {
            assert_recorded_move(&instance, &revision, node, false, "a file of e").await;
        }
        assert_nothing_left(&instance, &revision, "e").await;
    })
    .await;
}

/// What a stage of a dirty-moved file names besides, or in place of, its destination.
#[derive(Clone, Copy, Debug)]
enum Named {
    /// The path the move left, alone.
    Source,
    /// The path the move left, alone, with `--scan`.
    SourceScanned,
    /// The path the move left and the destination.
    Both,
    /// The path the move left, where a new file stands marked dirty.
    NewAtSource,
    /// The directories holding the path the move left and the destination.
    Directories,
}

/// A file moved and marked with `lore file dirty move`, its content unchanged or edited, is staged
/// as a move by a stage naming the path it left, alone, with or without a scan, or with its
/// destination, or the directories holding both; where a new file marked dirty stands at the path
/// it left, the stage takes that new file and the move. The commit records the move, with modify
/// set where the content changed, and leaves nothing for a status to report.
#[tokio::test]
async fn a_stage_naming_the_path_a_dirty_move_left_takes_the_move() {
    for edited in [false, true] {
        for named in [
            Named::Source,
            Named::SourceScanned,
            Named::Both,
            Named::NewAtSource,
            Named::Directories,
        ] {
            with_instance(|instance| async move {
                let case = format!("edited {edited}, named {named:?}");
                let first = committed(&instance, &["src/ex/walk.rs", "dst/other.txt"]).await;
                let moved = node_id_at(&instance, &first, "src/ex/walk.rs").await;
                dirty_moved(&instance, "src/ex/walk.rs", "dst/walk.rs").await;
                if edited {
                    test_file_write(&instance.path.join("dst/walk.rs"), b"walk, edited");
                    dirtied(&instance, "dst/walk.rs").await;
                }
                if matches!(named, Named::NewAtSource) {
                    written(&instance, &["src/ex/walk.rs"]);
                    dirtied(&instance, "src/ex/walk.rs").await;
                }

                let paths: &[&str] = match named {
                    Named::Source | Named::SourceScanned | Named::NewAtSource => {
                        &["src/ex/walk.rs"]
                    }
                    Named::Both => &["src/ex/walk.rs", "dst/walk.rs"],
                    Named::Directories => &["src", "dst"],
                };
                staged_with(
                    &instance,
                    paths,
                    StageOptions {
                        scan: matches!(named, Named::SourceScanned),
                        ..Default::default()
                    },
                )
                .await;
                let staged = staged_state(&instance).await;
                let node = node_of(&instance, &staged, moved).await;
                let modify = NodeFlags::StagedModify.bits();
                assert!(
                    node.is_staged_move() && (node.flags & modify == modify) == edited,
                    "{case}: flags {:#x}",
                    node.flags
                );

                let revision = test_commit(&instance, "Moved").await;
                assert_eq!(
                    node_id_at(&instance, &revision, "dst/walk.rs").await,
                    moved,
                    "{case}"
                );
                if matches!(named, Named::NewAtSource) {
                    assert_ne!(
                        node_id_at(&instance, &revision, "src/ex/walk.rs").await,
                        moved,
                        "{case}: the new file at the path the move left"
                    );
                } else {
                    assert!(
                        !holds(&instance, &revision, "src/ex/walk.rs").await,
                        "{case}"
                    );
                }
                assert_recorded_move(&instance, &revision, moved, edited, &case).await;
                assert_nothing_left(&instance, &revision, &case).await;
            })
            .await;
        }
    }
}

/// A stage of a directory above the path a dirty move left, and not above its destination, stages
/// nothing of the move: the file keeps its dirty move, unstaged.
#[tokio::test]
async fn a_stage_of_a_directory_above_only_the_path_a_dirty_move_left_leaves_the_move() {
    for edited in [false, true] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["src/ex/walk.rs", "dst/other.txt"]).await;
            let moved = node_id_at(&instance, &first, "src/ex/walk.rs").await;
            dirty_moved(&instance, "src/ex/walk.rs", "dst/walk.rs").await;
            if edited {
                test_file_write(&instance.path.join("dst/walk.rs"), b"walk, edited");
                dirtied(&instance, "dst/walk.rs").await;
            }

            staged_as_marked(&instance, &["src"]).await;
            let staged = staged_state(&instance).await;
            let node = node_of(&instance, &staged, moved).await;
            assert!(
                node.is_dirty_move() && !node.is_staged(),
                "edited {edited}: flags {:#x}",
                node.flags
            );
        })
        .await;
    }
}

/// An edited file of a directory moved and marked with `lore file dirty move` is staged as a move
/// carrying the edit by a stage naming the directory, its destination or the path it left, or of
/// the whole tree, with or without a scan: the commit records the file moved with modify set.
#[tokio::test]
async fn a_stage_of_a_dirty_moved_directory_takes_an_edited_file_in_it_as_a_move() {
    for (by, named) in [
        (StagedBy::Destination, "e"),
        (StagedBy::Destination, "d"),
        (StagedBy::Whole, "e"),
        (StagedBy::Scan, "e"),
    ] {
        with_instance(|instance| async move {
            let case = format!("staged by {by:?} {named}");
            let first = committed(&instance, &["d/k.txt", "other.txt"]).await;
            let directory = node_id_at(&instance, &first, "d").await;
            let file = node_id_at(&instance, &first, "d/k.txt").await;
            dirty_moved(&instance, "d", "e").await;
            test_file_write(&instance.path.join("e/k.txt"), b"k, edited");
            dirtied(&instance, "e/k.txt").await;

            stage_by(&instance, by, named).await;
            let revision = test_commit(&instance, "Moved").await;

            assert_eq!(
                node_id_at(&instance, &revision, "e/k.txt").await,
                file,
                "{case}"
            );
            assert_recorded_move(&instance, &revision, directory, false, &case).await;
            assert_recorded_move(&instance, &revision, file, true, &case).await;
            assert_nothing_left(&instance, &revision, &case).await;
        })
        .await;
    }
}
