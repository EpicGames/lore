// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The moves an SWFS mount keeps, and the renamed files it queues to give bytes of their own once
//! something is made where they came from.
// The tests make the files a mount holds in a directory of their own.
#![allow(clippy::disallowed_methods)]
use lore_base::test_util::TempDir;
use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::mount_io::MountIo;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::fs::swfs::renamed_at_risk::Carried;
use lore_revision::fs::swfs::renamed_at_risk::KEPT_MOVES;
use lore_revision::fs::swfs::renamed_at_risk::RenamedAtRisk;
use lore_revision::fs::swfs::renamed_at_risk::give_renamed_files_at_risk_bytes_of_their_own;
use lore_revision::fs::swfs::renamed_at_risk::note_move;

use super::listing_fixture::Held;
use super::listing_fixture::ignoring;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::test_repository;
use super::listing_fixture::written;

fn queued(renamed: &RenamedAtRisk) -> Vec<String> {
    let mut queued = renamed.at_risk();
    queued.sort();
    queued
}

#[test]
fn a_file_made_where_a_renamed_file_came_from_puts_the_renamed_file_at_risk() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "b.txt", false, true);
    assert!(
        queued(&renamed).is_empty(),
        "a rename alone puts nothing at risk"
    );

    renamed.created("A.txt", false);

    assert_eq!(queued(&renamed), ["b.txt"]);
}

#[test]
fn a_file_moved_to_where_a_renamed_file_came_from_puts_the_renamed_file_at_risk() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "b.txt", false, true);

    renamed.moved("other.txt", "a.txt", false, true);

    assert_eq!(queued(&renamed), ["b.txt"]);
}

#[test]
fn a_file_made_elsewhere_puts_nothing_at_risk() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "b.txt", false, true);

    renamed.created("c.txt", false);
    renamed.created("a.txt.bak", false);
    renamed.created("a.txt/inner", false);

    assert!(queued(&renamed).is_empty());
}

#[test]
fn a_renamed_file_renamed_again_is_kept_from_its_first_origin() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "b.txt", false, true);
    renamed.moved("b.txt", "c.txt", false, true);

    assert_eq!(renamed.origin_of("c.txt").as_deref(), Some("a.txt"));
    assert_eq!(renamed.origin_of("b.txt"), None);
    renamed.created("b.txt", false);
    assert!(queued(&renamed).is_empty(), "nothing came from b.txt");
    renamed.created("a.txt", false);
    assert_eq!(queued(&renamed), ["c.txt"]);
}

/// A file moved back where it came from is read from there, and is kept as moved to itself, so a
/// later move of it is still known to move no bytes of its own.
#[test]
fn a_file_moved_back_where_it_came_from_is_read_from_there() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "b.txt", false, true);

    renamed.moved("b.txt", "a.txt", false, true);

    assert_eq!(renamed.origin_of("b.txt"), None);
    assert_eq!(renamed.origin_of("a.txt"), None);
    assert_eq!(renamed.carried("a.txt"), Some(Carried::Moved));
    assert!(queued(&renamed).is_empty());
    let before = renamed.mark();
    renamed.prune_unlisted(before, |_| false);
    assert_eq!(renamed.carried("a.txt"), Some(Carried::Moved));
}

/// What is made below a directory moved back where it was is new, and puts nothing at risk.
#[test]
fn what_is_made_below_a_directory_moved_back_is_at_no_risk() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "e", true, true);
    renamed.moved("e", "d", true, true);

    renamed.created("d/new", false);
    renamed.created("d/sub", true);

    assert!(queued(&renamed).is_empty());
    assert_eq!(renamed.origin_of("d/x.txt"), None);
}

#[test]
fn a_file_below_a_renamed_directory_is_kept_from_below_its_origin() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "e", true, true);

    assert_eq!(
        renamed.origin_of("e/sub/x.txt").as_deref(),
        Some("d/sub/x.txt")
    );
    renamed.created("d/sub/x.txt", false);
    assert_eq!(queued(&renamed), ["e/sub/x.txt"]);
}

#[test]
fn a_directory_made_where_a_renamed_directory_came_from_puts_it_at_risk() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "e", true, true);

    renamed.created("d", true);

    assert_eq!(queued(&renamed), ["e"]);
}

#[test]
fn a_directory_moved_over_where_a_renamed_file_came_from_puts_the_file_at_risk() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d/a.txt", "b.txt", false, true);

    renamed.moved("other", "d", true, true);

    assert_eq!(queued(&renamed), ["b.txt"]);
}

#[test]
fn a_file_moved_out_of_a_renamed_directory_is_kept_from_below_the_directorys_origin() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "e", true, true);
    renamed.moved("e/x.txt", "y.txt", false, true);

    assert_eq!(renamed.origin_of("y.txt").as_deref(), Some("d/x.txt"));
}

#[test]
fn a_directory_holding_renamed_files_carries_them_when_it_moves() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "d/b.txt", false, true);
    renamed.created("a.txt", false);

    renamed.moved("d", "e", true, true);

    assert_eq!(renamed.origin_of("e/b.txt").as_deref(), Some("a.txt"));
    assert_eq!(queued(&renamed), ["e/b.txt"]);
}

#[test]
fn a_deleted_renamed_file_is_neither_kept_nor_at_risk() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "d/b.txt", false, true);
    renamed.moved("c.txt", "d/e.txt", false, true);
    renamed.created("a.txt", false);

    renamed.deleted("d");

    assert!(queued(&renamed).is_empty());
    assert_eq!(renamed.origin_of("d/e.txt"), None);
}

#[test]
fn a_renamed_file_a_thaw_named_is_no_longer_kept() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "b.txt", false, true);

    renamed.forget_named(&[node("b.txt")]);

    assert_eq!(renamed.origin_of("b.txt"), None);
    renamed.created("a.txt", false);
    assert!(queued(&renamed).is_empty());
}

/// Past the moves a mount keeps, a move is put at risk at once rather than kept.
#[test]
fn a_move_past_the_kept_moves_is_put_at_risk_at_once() {
    let renamed = RenamedAtRisk::default();
    for index in 0..KEPT_MOVES {
        renamed.moved(&format!("from{index}"), &format!("to{index}"), false, true);
    }
    assert!(queued(&renamed).is_empty());

    renamed.moved("last", "moved", false, true);

    assert_eq!(renamed.origin_of("moved"), None);
    assert_eq!(queued(&renamed), ["moved"]);
}

#[tokio::test]
async fn a_file_at_risk_is_rewritten_with_its_own_bytes_and_no_longer_kept() {
    in_context(async {
        let mount = TempDir::new("swfs-renamed-at-risk");
        std::fs::create_dir_all(mount.path().join("e")).expect("making a directory");
        std::fs::write(mount.path().join("b.txt"), b"renamed").expect("writing");
        std::fs::write(mount.path().join("e/x.txt"), b"below").expect("writing");
        let renamed = RenamedAtRisk::default();
        renamed.moved("a.txt", "b.txt", false, true);
        renamed.moved("d", "e", true, true);
        renamed.moved("gone.txt", "missing.txt", false, true);
        for origin in ["a.txt", "d", "gone.txt"] {
            renamed.created(origin, origin == "d");
        }

        give_renamed_files_at_risk_bytes_of_their_own(&renamed, mount.path(), MountIo::global())
            .await;

        assert!(queued(&renamed).is_empty());
        assert_eq!(
            std::fs::read(mount.path().join("b.txt")).expect("reading"),
            b"renamed"
        );
        assert_eq!(
            std::fs::read(mount.path().join("e/x.txt")).expect("reading"),
            b"below"
        );
        assert_eq!(renamed.origin_of("b.txt"), None);
        assert_eq!(renamed.origin_of("missing.txt"), None);
        // The files below a directory still renamed from its origin are read from there.
        assert_eq!(renamed.origin_of("e/x.txt").as_deref(), Some("d/x.txt"));
    })
    .await;
}

/// Another handle writing the file keeps it from being rewritten, until the next operation.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_file_another_handle_writes_stays_at_risk() {
    in_context(async {
        let mount = TempDir::new("swfs-renamed-at-risk");
        let path = mount.path().join("b.txt");
        std::fs::write(&path, b"renamed").expect("writing");
        let renamed = RenamedAtRisk::default();
        renamed.moved("a.txt", "b.txt", false, true);
        renamed.created("a.txt", false);
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("opening the file to write");

        give_renamed_files_at_risk_bytes_of_their_own(&renamed, mount.path(), MountIo::global())
            .await;

        assert_eq!(queued(&renamed), ["b.txt"]);
        assert_eq!(renamed.origin_of("b.txt").as_deref(), Some("a.txt"));
        drop(writer);
        give_renamed_files_at_risk_bytes_of_their_own(&renamed, mount.path(), MountIo::global())
            .await;
        assert!(queued(&renamed).is_empty());
    })
    .await;
}

/// Mount resources serving a revision that holds `tracked.txt` and `d/x.txt`.
async fn serving_tracked() -> (MountResources, TempDir) {
    let mount = TempDir::new("swfs-renamed-at-risk-served");
    let repository = test_repository().await;
    let state = revision_state(
        &repository,
        &[
            ("tracked.txt", Held::File(b"tracked")),
            ("d/x.txt", Held::File(b"x")),
        ],
    )
    .await;
    let mount_path = MountPath::new(mount.path()).expect("a mount path");
    let resources = MountResources::for_test(repository, Some(state), mount_path).await;
    resources.modified().set_trusted(true);
    (resources, mount)
}

/// An editor's save: a temporary file the user made, renamed over the file it saves. It holds bytes
/// of its own, so the next temporary file made where it was puts nothing at risk.
#[tokio::test]
async fn a_move_of_a_file_the_revision_does_not_serve_is_not_kept() {
    in_context(Box::pin(async {
        let (resources, _mount) = serving_tracked().await;

        note_move(&resources, "tracked.txt.tmp", "tracked.txt", false).await;
        resources.renamed().created("tracked.txt.tmp", false);

        assert_eq!(resources.renamed().origin_of("tracked.txt"), None);
        assert!(queued(resources.renamed()).is_empty());
    }))
    .await;
}

#[tokio::test]
async fn a_move_of_a_file_the_revision_serves_unchanged_is_kept() {
    in_context(Box::pin(async {
        let (resources, _mount) = serving_tracked().await;

        note_move(&resources, "tracked.txt", "renamed.txt", false).await;
        note_move(&resources, "d", "e", true).await;

        assert_eq!(
            resources.renamed().origin_of("renamed.txt").as_deref(),
            Some("tracked.txt")
        );
        assert_eq!(
            resources.renamed().origin_of("e/x.txt").as_deref(),
            Some("d/x.txt")
        );
    }))
    .await;
}

#[tokio::test]
async fn a_move_of_a_file_the_user_changed_is_not_kept() {
    in_context(Box::pin(async {
        let (resources, _mount) = serving_tracked().await;
        resources.modified().record(written("tracked.txt")).await;

        note_move(&resources, "tracked.txt", "renamed.txt", false).await;

        assert_eq!(resources.renamed().origin_of("renamed.txt"), None);
    }))
    .await;
}

/// A file renamed again is still read from where it first came from, and one moved out of a
/// renamed directory from below the directory's origin, unless the user changed it.
#[tokio::test]
async fn a_move_of_what_a_kept_move_left_is_kept_unless_the_user_changed_it() {
    in_context(Box::pin(async {
        let (resources, _mount) = serving_tracked().await;
        note_move(&resources, "tracked.txt", "b.txt", false).await;
        note_move(&resources, "d", "e", true).await;
        resources.modified().record(written("e/x.txt")).await;

        note_move(&resources, "b.txt", "c.txt", false).await;
        note_move(&resources, "e/x.txt", "y.txt", false).await;

        assert_eq!(
            resources.renamed().origin_of("c.txt").as_deref(),
            Some("tracked.txt")
        );
        assert_eq!(resources.renamed().origin_of("y.txt"), None);
    }))
    .await;
}

/// A file renamed back to where it was, as an undo does, is still read from the source there, so
/// a move of it later is kept, though the record holds the path changed.
#[tokio::test]
async fn a_file_renamed_back_and_renamed_again_is_kept() {
    in_context(Box::pin(async {
        let (resources, _mount) = serving_tracked().await;
        for (from, to) in [
            ("tracked.txt", "b.txt"),
            ("b.txt", "tracked.txt"),
            ("tracked.txt", "c.txt"),
        ] {
            note_move(&resources, from, to, false).await;
            resources
                .modified()
                .record(Change::Moved {
                    from: from.into(),
                    to: to.into(),
                    directory: false,
                })
                .await;
        }

        assert_eq!(
            resources.renamed().origin_of("c.txt").as_deref(),
            Some("tracked.txt")
        );
    }))
    .await;
}

/// Where a file goes does not give it bytes of its own: one moved to a path the ignore rules
/// exclude, as an editor's backup copy is, or through one, is still read from where it came from.
#[tokio::test]
async fn a_file_moved_to_a_path_the_revision_does_not_serve_is_kept() {
    in_context(Box::pin(async {
        let (resources, _mount) = serving_tracked().await;

        resources.modified().set_ignore(ignoring("*~\n*.tmp\n"));

        note_move(&resources, "tracked.txt", "tracked.txt~", false).await;
        resources.renamed().created("tracked.txt", false);
        note_move(&resources, "d/x.txt", "d/x.tmp", false).await;
        note_move(&resources, "d/x.tmp", "d/y.txt", false).await;
        resources.renamed().created("d/x.txt", false);

        assert_eq!(queued(resources.renamed()), ["d/y.txt", "tracked.txt~"]);
    }))
    .await;
}

/// What a kept move left below a directory the user made moves with it, though the directory's
/// own move is not kept.
#[test]
fn a_directory_the_user_made_carries_what_kept_moves_left_in_it() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("x", "u/x", false, true);

    renamed.moved("u", "v", true, false);

    assert_eq!(renamed.origin_of("v/x").as_deref(), Some("x"));
    assert_eq!(renamed.origin_of("v"), None);
}

/// Each level found through a directory SWFS no longer lists is read from below that
/// directory's destination, however the levels below were found.
#[test]
fn what_was_found_through_a_forgotten_directory_is_re_sourced_at_every_level() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "e", true, true);
    renamed.moved("e/sub", "s", true, true);
    renamed.moved("s/f", "t", false, true);
    assert_eq!(renamed.origin_of("t").as_deref(), Some("d/sub/f"));
    let before = renamed.mark();

    renamed.prune_unlisted(before, |destination| destination != "e");

    assert_eq!(renamed.origin_of("s").as_deref(), Some("e/sub"));
    assert_eq!(renamed.origin_of("t").as_deref(), Some("e/sub/f"));
}

/// A directory forgotten along with the one it was found through re-sources what was found
/// through it to below its own destination.
#[test]
fn what_was_found_through_two_forgotten_directories_is_read_below_the_nearer() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "ee", true, true);
    renamed.moved("ee/sub", "s", true, true);
    renamed.moved("s/f", "t", false, true);
    let before = renamed.mark();

    renamed.prune_unlisted(before, |destination| destination == "t");

    assert_eq!(renamed.origin_of("t").as_deref(), Some("s/f"));
}

#[test]
fn a_move_swfs_no_longer_lists_as_renamed_is_forgotten() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("a.txt", "b.txt", false, true);
    renamed.moved("c.txt", "d.txt", false, true);
    let before = renamed.mark();
    renamed.moved("e.txt", "f.txt", false, true);

    renamed.prune_unlisted(before, |destination| destination == "d.txt");

    assert_eq!(renamed.origin_of("b.txt"), None);
    assert_eq!(renamed.origin_of("d.txt").as_deref(), Some("c.txt"));
    assert_eq!(
        renamed.origin_of("f.txt").as_deref(),
        Some("e.txt"),
        "a move kept since the mark is left to the next prune"
    );
}

/// Once a thaw named a renamed directory, SWFS reads what is below it from the source at its own
/// path, so a file renamed out from below it is read from where it stood there.
#[test]
fn a_file_found_through_a_directory_swfs_no_longer_lists_is_read_from_below_its_destination() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "e", true, true);
    renamed.moved("e/x.txt", "e/y.txt", false, true);
    assert_eq!(renamed.origin_of("e/y.txt").as_deref(), Some("d/x.txt"));
    let before = renamed.mark();

    renamed.prune_unlisted(before, |destination| destination != "e");

    assert_eq!(renamed.origin_of("e/y.txt").as_deref(), Some("e/x.txt"));
    assert_eq!(queued(&renamed), ["e/y.txt"]);
}

/// A thaw named a renamed directory before a file in it was renamed and something was made where
/// it was: the file is read from there in the source, so it is at risk.
#[test]
fn a_file_renamed_below_a_directory_swfs_no_longer_lists_is_put_at_risk_by_what_is_made_there() {
    let renamed = RenamedAtRisk::default();
    renamed.moved("d", "e", true, true);
    let before = renamed.mark();
    renamed.prune_unlisted(before, |_| false);

    renamed.moved("e/x.txt", "e/y.txt", false, true);
    renamed.created("e/x.txt", false);

    assert_eq!(renamed.origin_of("e/y.txt").as_deref(), Some("e/x.txt"));
    assert_eq!(queued(&renamed), ["e/y.txt"]);
}
