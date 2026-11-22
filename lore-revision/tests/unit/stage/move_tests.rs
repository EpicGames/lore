// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A staged or dirty move's marks on the directories around the node it moves, and on what it
//! moves, and what a commit takes of a dirty move never staged.

use std::sync::Arc;

use lore_revision::change::FileAction;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::stage::StageError;
use lore_revision::stage::StageOptions;
use lore_revision::state::Clear;
use lore_revision::state::State;

use crate::dirty_move_fixture::assert_no_change_marked;
use crate::dirty_move_fixture::chained;
use crate::dirty_move_fixture::children_named;
use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirtied;
use crate::dirty_move_fixture::dirty_move;
use crate::dirty_move_fixture::dirty_moved;
use crate::dirty_move_fixture::holds;
use crate::dirty_move_fixture::made_directory;
use crate::dirty_move_fixture::node_at;
use crate::dirty_move_fixture::node_id_at;
use crate::dirty_move_fixture::node_of;
use crate::dirty_move_fixture::removed_directory;
use crate::dirty_move_fixture::rename;
use crate::dirty_move_fixture::restaged;
use crate::dirty_move_fixture::scanned;
use crate::dirty_move_fixture::stage_path;
use crate::dirty_move_fixture::staged_move;
use crate::dirty_move_fixture::staged_moved;
use crate::dirty_move_fixture::staged_tree;
use crate::dirty_move_fixture::status_and_marks;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::helper::TestRepository;
use crate::helper::mount_intermediate;
use crate::helper::test_commit;
use crate::helper::test_file_write;

/// Removes the file `path` below `instance`'s working tree.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn removed(instance: &TestRepository, path: &str) {
    std::fs::remove_file(instance.path.join(path)).expect("the delete on disk");
}

/// Moves `dir/sub` over `dir` in `instance`'s working tree, which then holds `sub`'s files in
/// `dir` and nothing else of the old `dir`.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn flattened(instance: &TestRepository) {
    rename(instance, "dir/sub", "tmp");
    std::fs::remove_dir_all(instance.path.join("dir")).expect("the delete on disk");
    rename(instance, "tmp", "dir");
}

/// Moves `a` into a new directory of its own name in `instance`'s working tree, as `a/b`.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn nested(instance: &TestRepository) {
    rename(instance, "a", "tmp");
    std::fs::create_dir(instance.path.join("a")).expect("mkdir");
    rename(instance, "tmp", "a/b");
}

/// Clears the dirty half of the flags of each of `nodes` in the staged `state`, and of each
/// ancestor nothing below it keeps dirty, and stores it as the staged State, as a merge stages its
/// changes: with no dirty half.
async fn staged_alone(instance: &TestRepository, state: &Arc<State>, nodes: &[NodeID]) {
    for node in nodes {
        state
            .node_clear(&instance.repository, *node, Clear::Dirty)
            .await
            .expect("the dirty half cleared");
    }
    let signature = state
        .serialize(instance.repository.clone(), &instance.write_token)
        .await
        .expect("the staged state serializes");
    lore_revision::instance::store_staged_anchor(&instance.repository, signature)
        .await
        .expect("the staged anchor");
}

/// Commits `dir/a.txt` and `dir/b.txt`, then stages the deletion of `dir/b.txt` with no dirty
/// half, as a merge stages one; answers its node.
async fn child_staged_deleted(instance: &TestRepository) -> NodeID {
    committed(instance, &["dir/a.txt", "dir/b.txt"]).await;
    removed(instance, "dir/b.txt");
    let state = stage_path(instance, "dir/b.txt").await;
    let b = node_id_at(instance, &state, "dir/b.txt").await;
    staged_alone(instance, &state, &[b]).await;
    let before = node_of(instance, &state, b).await;
    assert!(
        before.is_staged_delete() && !before.is_dirty(),
        "flags {:#x}",
        before.flags
    );
    b
}

/// Commits `dir/a.txt` and `dir/b.txt`, then marks `dir/b.txt` dirty as deleted; answers its
/// node.
async fn child_dirty_deleted(instance: &TestRepository) -> NodeID {
    committed(instance, &["dir/a.txt", "dir/b.txt"]).await;
    removed(instance, "dir/b.txt");
    let state = dirtied(instance, "dir/b.txt").await;
    let b = node_id_at(instance, &state, "dir/b.txt").await;
    let before = node_of(instance, &state, b).await;
    assert!(
        before.is_dirty_delete() && !before.is_staged(),
        "flags {:#x}",
        before.flags
    );
    b
}

/// The files the deleted directory fixtures commit below `dir`: one in it, and one in a directory
/// in it, which a walk of the subtree reaches only by its recursion.
const BELOW_DIR: [&str; 2] = ["dir/a.txt", "dir/s/n.txt"];

/// The nodes of [`BELOW_DIR`] in `state`, each deleted with `dir`.
async fn deleted_below(instance: &TestRepository, state: &State) -> [NodeID; 2] {
    let below = [
        node_id_at(instance, state, BELOW_DIR[0]).await,
        node_id_at(instance, state, BELOW_DIR[1]).await,
    ];
    for node in below {
        let before = node_of(instance, state, node).await;
        assert!(before.is_deleted(), "flags {:#x}", before.flags);
    }
    below
}

/// Commits [`BELOW_DIR`], moves `dir` to `moved` in the working tree, then stages the deletion of
/// `dir` with no dirty half on it nor below it; answers the nodes of [`BELOW_DIR`].
async fn dir_staged_deleted(instance: &TestRepository) -> [NodeID; 2] {
    committed(instance, &BELOW_DIR).await;
    rename(instance, "dir", "moved");
    let state = stage_path(instance, "dir").await;
    let below = deleted_below(instance, &state).await;
    staged_alone(instance, &state, &below).await;
    let dir = node_id_at(instance, &state, "dir").await;
    for node in [dir, below[0], below[1]] {
        let before = node_of(instance, &state, node).await;
        assert!(
            before.is_staged_delete() && !before.is_dirty(),
            "flags {:#x}",
            before.flags
        );
    }
    below
}

/// Commits [`BELOW_DIR`], moves `dir` to `moved` in the working tree, then marks `dir` dirty as
/// deleted; answers the nodes of [`BELOW_DIR`].
async fn dir_dirty_deleted(instance: &TestRepository) -> [NodeID; 2] {
    committed(instance, &BELOW_DIR).await;
    rename(instance, "dir", "moved");
    let state = dirtied(instance, "dir").await;
    let before = node_at(instance, &state, "dir").await;
    assert!(
        before.is_dirty_delete() && !before.is_staged(),
        "flags {:#x}",
        before.flags
    );
    deleted_below(instance, &state).await
}

/// Writes `path` in the working tree and marks it dirty, as added; answers its node.
async fn dirty_added(instance: &TestRepository, path: &str) -> NodeID {
    written(instance, &[path]);
    let state = dirtied(instance, path).await;
    let node = node_id_at(instance, &state, path).await;
    let before = node_of(instance, &state, node).await;
    assert!(
        before.is_dirty_add() && !before.is_staged(),
        "flags {:#x}",
        before.flags
    );
    node
}

/// Writes `path` in the working tree and stages its add with no dirty half, as a merge stages
/// one; answers its node.
async fn merge_added(instance: &TestRepository, path: &str) -> NodeID {
    written(instance, &[path]);
    let state = stage_path(instance, path).await;
    let node = node_id_at(instance, &state, path).await;
    staged_alone(instance, &state, &[node]).await;
    let before = node_of(instance, &state, node).await;
    assert!(
        before.is_staged_add() && !before.is_dirty(),
        "flags {:#x}",
        before.flags
    );
    node
}

/// A file staged as moved into a committed directory stages that directory, with the base
/// `Staged` bit alone, and marks it dirty with the moved file, as the C API's move marks them: the
/// commit rehashes it, holding the file there.
#[tokio::test]
async fn a_staged_move_into_a_committed_directory_stages_it() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/keep.txt", "a.txt"]).await;

        let state = staged_moved(&instance, "a.txt", "dir/a.txt").await;
        assert!(
            node_at(&instance, &state, "dir/a.txt")
                .await
                .is_staged_move()
        );
        let dir = node_at(&instance, &state, "dir").await;
        assert!(
            dir.is_staged() && !dir.is_staged_move(),
            "flags {:#x}",
            dir.flags
        );
        assert!(dir.is_dirty(), "flags {:#x}", dir.flags);
    })
    .await;
}

/// A file staged as moved out of a committed directory stages the directory it left, and
/// dirties it not: its children changed, so the commit rehashes it, and the revision holds the
/// file in one place.
#[tokio::test]
async fn a_staged_move_out_of_a_committed_directory_stages_the_one_it_left() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["dir/keep.txt", "dir/a.txt", "other/o.txt"]).await;
        let before = node_at(&instance, &first, "dir").await.address;

        let state = staged_moved(&instance, "dir/a.txt", "other/a.txt").await;
        let left = node_at(&instance, &state, "dir").await;

        let revision = test_commit(&instance, "Moved").await;
        let dir = node_at(&instance, &revision, "dir").await;
        assert_ne!(
            dir.address, before,
            "the commit rehashed the directory the file left"
        );
        assert!(left.is_staged(), "flags {:#x}", left.flags);
        assert!(!left.is_dirty(), "flags {:#x}", left.flags);
    })
    .await;
}

/// A file staged as moved out of a directory no commit holds, which `lore dirty` added as a dirty
/// add above a file it marked, leaves that directory unstaged: the commit drops it, with nothing
/// left in it, where a staged mark would make it keep it.
#[tokio::test]
async fn a_staged_move_out_of_a_directory_marked_dirty_leaves_it_unstaged() {
    with_instance(|instance| async move {
        committed(&instance, &["other/o.txt"]).await;
        dirty_added(&instance, "new/f.txt").await;
        let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
            .await
            .expect("the states");
        let before = node_at(&instance, &staged.expect("a staged state"), "new").await;
        assert!(
            before.is_dirty_add() && !before.is_staged(),
            "flags {:#x}",
            before.flags
        );
        assert!(before.address.is_zero(), "no commit recorded it");

        let state = staged_moved(&instance, "new/f.txt", "other/f.txt").await;
        let left = node_at(&instance, &state, "new").await;
        assert!(!left.is_staged(), "flags {:#x}", left.flags);
        let revision = test_commit(&instance, "Moved").await;
        assert!(holds(&instance, &revision, "other/f.txt").await);
        assert!(!holds(&instance, &revision, "new").await);
    })
    .await;
}

/// A file staged as moved out of a directory marked dirty as added, as a scan marks one it finds,
/// leaves that directory unstaged, its add with it: the commit drops it.
#[tokio::test]
async fn a_staged_move_out_of_a_dirty_added_directory_leaves_it_unstaged() {
    with_instance(|instance| async move {
        committed(&instance, &["other/o.txt"]).await;
        written(&instance, &["new/f.txt"]);
        let state = dirtied(&instance, "new").await;
        let before = node_at(&instance, &state, "new").await;
        assert!(
            before.is_dirty_add() && !before.is_staged(),
            "flags {:#x}",
            before.flags
        );

        let state = staged_moved(&instance, "new/f.txt", "other/f.txt").await;
        let left = node_at(&instance, &state, "new").await;
        assert!(
            left.is_dirty_add() && !left.is_staged(),
            "flags {:#x}",
            left.flags
        );
        let revision = test_commit(&instance, "Moved").await;
        assert!(holds(&instance, &revision, "other/f.txt").await);
        assert!(!holds(&instance, &revision, "new").await);
    })
    .await;
}

/// A file staged as moved out of a directory staged as moved keeps that directory's move: the
/// mark of the directory it left sets the base bit alone, where it is not set.
#[tokio::test]
async fn a_staged_move_out_of_a_moved_directory_keeps_its_move() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/a.txt", "dir/b.txt"]).await;
        staged_moved(&instance, "dir", "moved").await;

        let state = staged_moved(&instance, "moved/a.txt", "a.txt").await;
        let left = node_at(&instance, &state, "moved").await;
        assert!(left.is_staged_move(), "flags {:#x}", left.flags);
    })
    .await;
}

/// A directory staged as moved out of a committed directory stages the directory it left and
/// marks its subtree moved.
#[tokio::test]
async fn a_staged_move_of_a_directory_out_of_a_committed_one_marks_both() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/keep.txt", "dir/sub/x.txt", "other/o.txt"]).await;

        let state = staged_moved(&instance, "dir/sub", "other/sub").await;
        let left = node_at(&instance, &state, "dir").await;
        assert!(left.is_staged(), "flags {:#x}", left.flags);
        let x = node_at(&instance, &state, "other/sub/x.txt").await;
        assert!(x.is_staged_move(), "flags {:#x}", x.flags);
    })
    .await;
}

/// A file staged as moved over a committed file replaces it: the target, a sibling under one name
/// with the node once it moved, is staged as deleted by its own node, the revision holds the
/// moved file's content there, and its delta records the target's deletion and the move.
#[tokio::test]
async fn a_staged_move_over_a_committed_file_replaces_it() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt", "b.txt"]).await;
        let a = node_id_at(&instance, &first, "a.txt").await;
        let b = node_id_at(&instance, &first, "b.txt").await;
        let content = node_of(&instance, &first, a).await.address;

        let state = staged_moved(&instance, "a.txt", "b.txt").await;
        let target = node_of(&instance, &state, b).await;
        assert!(target.is_staged_delete(), "flags {:#x}", target.flags);
        let node = node_of(&instance, &state, a).await;
        assert!(node.is_staged_move(), "flags {:#x}", node.flags);
        let revision = test_commit(&instance, "Replaced").await;
        assert!(!holds(&instance, &revision, "a.txt").await);
        assert_eq!(
            node_at(&instance, &revision, "b.txt").await.address,
            content
        );
        let deletion = revision
            .node_deletion(instance.repository.clone(), b)
            .await
            .expect("the revision's delta");
        assert_eq!(
            deletion.map(|delta| delta.action),
            Some(FileAction::Delete as u16)
        );
        let change = revision
            .node_change(instance.repository.clone(), a)
            .await
            .expect("the revision's delta");
        assert_eq!(
            change.map(|delta| delta.action),
            Some(FileAction::Move as u16)
        );
    })
    .await;
}

/// A scan of a file staged as moved over a committed one, marked dirty with its content unchanged,
/// compares it as it compares any staged file: the dirty mark goes and the move stays staged.
#[tokio::test]
async fn a_scan_of_a_file_staged_as_moved_over_a_committed_one_compares_its_content() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt", "b.txt"]).await;
        let a = node_id_at(&instance, &first, "a.txt").await;
        staged_moved(&instance, "a.txt", "b.txt").await;
        let marked = dirtied(&instance, "b.txt").await;
        assert!(node_of(&instance, &marked, a).await.is_dirty_move());

        scanned(&instance).await;
        let staged = staged_tree(&instance).await;
        let node = node_of(&instance, &staged, a).await;
        assert!(
            node.is_staged_move() && !node.is_dirty(),
            "flags {:#x}",
            node.flags
        );
    })
    .await;
}

/// A directory staged as moved keeps a child's staged deletion: the move's marks of its subtree
/// leave a node staged for deletion as it is.
#[tokio::test]
async fn a_staged_move_of_a_directory_keeps_a_staged_deleted_child() {
    with_instance(|instance| async move {
        let b = child_staged_deleted(&instance).await;

        let state = staged_moved(&instance, "dir", "moved").await;
        let after = node_of(&instance, &state, b).await;
        assert!(after.is_staged_delete(), "flags {:#x}", after.flags);
    })
    .await;
}

/// A directory staged as moved keeps a child's dirty deletion: a staged mark of the move would
/// take the action the two halves share off it.
#[tokio::test]
async fn a_staged_move_of_a_directory_keeps_a_dirty_deleted_child() {
    with_instance(|instance| async move {
        let b = child_dirty_deleted(&instance).await;

        let state = staged_moved(&instance, "dir", "moved").await;
        let after = node_of(&instance, &state, b).await;
        assert!(after.is_dirty_delete(), "flags {:#x}", after.flags);
    })
    .await;
}

/// A directory staged as deleted, then staged as moved, takes the deletion off its subtree,
/// which its deletion stamped on every node below it: the revision holds it moved, whole.
#[tokio::test]
async fn a_staged_move_of_a_staged_deleted_directory_moves_its_subtree() {
    with_instance(|instance| async move {
        let below = dir_staged_deleted(&instance).await;

        let state = staged_move(&instance, "dir", "moved").await;
        for node in below {
            let after = node_of(&instance, &state, node).await;
            assert!(after.is_staged_move(), "flags {:#x}", after.flags);
        }
        let revision = test_commit(&instance, "Moved").await;
        for path in ["moved/a.txt", "moved/s/n.txt"] {
            assert!(holds(&instance, &revision, path).await, "{path}");
        }
    })
    .await;
}

/// A directory marked dirty as deleted, then staged as moved, takes the deletion off its subtree
/// all the same.
#[tokio::test]
async fn a_staged_move_of_a_dirty_deleted_directory_moves_its_subtree() {
    with_instance(|instance| async move {
        let below = dir_dirty_deleted(&instance).await;

        let state = staged_move(&instance, "dir", "moved").await;
        for node in below {
            let after = node_of(&instance, &state, node).await;
            assert!(after.is_staged_move(), "flags {:#x}", after.flags);
        }
    })
    .await;
}

/// A directory staged as moved over the directory above it, which the move replaces, keeps its
/// subtree: the target's deletion, staged once the directory has left it, reaches nothing below
/// it.
#[tokio::test]
async fn a_staged_move_of_a_directory_over_its_parent_keeps_its_subtree() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["dir/sub/x.txt", "dir/y.txt"]).await;
        let x = node_id_at(&instance, &first, "dir/sub/x.txt").await;
        flattened(&instance);

        let state = staged_move(&instance, "dir/sub", "dir").await;
        let after = node_of(&instance, &state, x).await;
        assert!(after.is_staged_move(), "flags {:#x}", after.flags);
        let revision = test_commit(&instance, "Flattened").await;
        assert!(holds(&instance, &revision, "dir/x.txt").await);
        assert!(!holds(&instance, &revision, "dir/y.txt").await);
    })
    .await;
}

/// A directory staged as moved over the directory above it keeps the changes staged below it: a
/// child staged as deleted stays deleted, and one staged as added stays added, as the target's
/// deletion is staged once the directory has left it.
#[tokio::test]
async fn a_staged_move_of_a_directory_over_its_parent_keeps_its_childrens_changes() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/sub/x.txt", "dir/sub/z.txt", "dir/y.txt"]).await;
        removed(&instance, "dir/sub/z.txt");
        written(&instance, &["dir/sub/new.txt"]);
        let state = stage_path(&instance, "dir/sub").await;
        let z = node_id_at(&instance, &state, "dir/sub/z.txt").await;
        let new = node_id_at(&instance, &state, "dir/sub/new.txt").await;
        flattened(&instance);

        let state = staged_move(&instance, "dir/sub", "dir").await;
        let deleted = node_of(&instance, &state, z).await;
        assert!(deleted.is_staged_delete(), "flags {:#x}", deleted.flags);
        let added = node_of(&instance, &state, new).await;
        assert!(added.is_staged_add(), "flags {:#x}", added.flags);
        let revision = test_commit(&instance, "Flattened").await;
        assert!(holds(&instance, &revision, "dir/new.txt").await);
        assert!(!holds(&instance, &revision, "dir/z.txt").await);
    })
    .await;
}

/// A file or a directory staged as renamed in case alone stays, the directory with its subtree:
/// the target the move looks up, by a name's hash, which the case leaves alone, is the node itself,
/// which the move does not delete. What a commit then makes of the case alone is the directory
/// hash's to settle.
#[tokio::test]
async fn a_case_only_staged_rename_of_a_file_or_a_directory_deletes_nothing() {
    with_instance(|instance| async move {
        committed(&instance, &["File.txt", "Dir/a.txt"]).await;

        staged_moved(&instance, "File.txt", "file.txt").await;
        let state = staged_moved(&instance, "Dir", "dir").await;
        for path in ["file.txt", "dir", "dir/a.txt"] {
            let node = node_at(&instance, &state, path).await;
            assert!(!node.is_staged_delete(), "{path}: flags {:#x}", node.flags);
        }
    })
    .await;
}

/// A directory staged as moved below itself is refused, spelled in either case, as lookups ignore
/// case: the working tree can hold it there, moved into a new directory of its old name, but no
/// node can hold itself.
#[tokio::test]
async fn a_staged_move_of_a_directory_into_itself_is_refused() {
    with_instance(|instance| async move {
        committed(&instance, &["a/x.txt"]).await;
        nested(&instance);

        for from in ["a", "A"] {
            let result = lore_revision::file::stage::stage_move(
                instance.repository.clone(),
                &instance.write_token,
                instance.path.join(from).to_string_lossy().into_owned(),
                instance.path.join("a/b").to_string_lossy().into_owned(),
                StageOptions::default(),
            )
            .await;
            assert!(
                result.is_err_and(|error| error.is_invalid_arguments()),
                "{from}: the move below itself is refused"
            );
        }
    })
    .await;
}

/// A dirty file staged as moved into a clean directory, two levels down, leaves each level
/// dirty: a dirty node's ancestors are dirty, which a walk of the dirty flags relies on to reach
/// it.
#[tokio::test]
async fn a_staged_move_of_a_dirty_file_into_a_clean_directory_dirties_it() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/a.txt", "other/deep/o.txt"]).await;
        test_file_write(&instance.path.join("dir/a.txt"), b"edited");
        dirtied(&instance, "dir/a.txt").await;

        let state = staged_moved(&instance, "dir/a.txt", "other/deep/a.txt").await;
        let a = node_at(&instance, &state, "other/deep/a.txt").await;
        assert!(
            a.is_dirty(),
            "the moved file stays dirty: flags {:#x}",
            a.flags
        );
        for path in ["other/deep", "other"] {
            let level = node_at(&instance, &state, path).await;
            assert!(level.is_dirty(), "{path}: flags {:#x}", level.flags);
        }
    })
    .await;
}

/// A file marked dirty as added, then staged as moved, stays an addition: it is in no revision a
/// move could name.
#[tokio::test]
async fn a_staged_move_of_a_dirty_added_file_keeps_it_added() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/keep.txt"]).await;
        let new = dirty_added(&instance, "new.txt").await;

        let state = staged_moved(&instance, "new.txt", "dir/new.txt").await;
        let after = node_of(&instance, &state, new).await;
        assert!(after.is_staged_add(), "flags {:#x}", after.flags);
        let revision = test_commit(&instance, "Added").await;
        assert!(holds(&instance, &revision, "dir/new.txt").await);
    })
    .await;
}

/// A child marked dirty as added stays an addition when its directory is staged as moved: it is in
/// no revision a move could name.
#[tokio::test]
async fn a_staged_move_of_a_directory_keeps_a_dirty_added_file_added() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/a.txt"]).await;
        let new = dirty_added(&instance, "dir/new.txt").await;

        let state = staged_moved(&instance, "dir", "moved").await;
        let after = node_of(&instance, &state, new).await;
        assert!(after.is_staged_add(), "flags {:#x}", after.flags);
    })
    .await;
}

/// A directory moved with `lore dirty move` keeps a child's dirty deletion: the move's marks of
/// its subtree leave a dirty-deleted node as it is.
#[tokio::test]
async fn a_dirty_move_of_a_directory_keeps_a_dirty_deleted_child() {
    with_instance(|instance| async move {
        let b = child_dirty_deleted(&instance).await;

        let state = dirty_moved(&instance, "dir", "moved").await;
        let after = node_of(&instance, &state, b).await;
        assert!(after.is_dirty_delete(), "flags {:#x}", after.flags);
    })
    .await;
}

/// A directory moved with `lore dirty move` keeps a child's staged deletion with no dirty half,
/// as a merge stages one: the move's dirty marks would take the action the halves share off it.
#[tokio::test]
async fn a_dirty_move_of_a_directory_keeps_a_staged_deleted_child() {
    with_instance(|instance| async move {
        let b = child_staged_deleted(&instance).await;

        let state = dirty_moved(&instance, "dir", "moved").await;
        let after = node_of(&instance, &state, b).await;
        assert!(after.is_staged_delete(), "flags {:#x}", after.flags);
    })
    .await;
}

/// A directory and its file staged as added with no dirty half, as a merge stages them, then
/// moved with `lore dirty move`, stay additions: they are in no revision a move could name.
#[tokio::test]
async fn a_dirty_move_of_a_directory_added_by_a_merge_keeps_it_added() {
    with_instance(|instance| async move {
        committed(&instance, &["keep.txt"]).await;
        let f = merge_added(&instance, "added/f.txt").await;
        let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
            .await
            .expect("the states");
        let before = node_at(&instance, &staged.expect("a staged state"), "added").await;
        assert!(
            before.is_staged_add() && !before.is_dirty(),
            "flags {:#x}",
            before.flags
        );

        let state = dirty_moved(&instance, "added", "moved").await;
        let added = node_id_at(&instance, &state, "moved").await;
        for node in [added, f] {
            let after = node_of(&instance, &state, node).await;
            assert!(after.is_staged_add(), "flags {:#x}", after.flags);
        }
    })
    .await;
}

/// A file staged as added with no dirty half, as a merge stages one, in a committed directory then
/// moved with `lore dirty move`, stays an addition: it is in no revision a move could name.
#[tokio::test]
async fn a_dirty_move_of_a_directory_keeps_a_merges_added_file_added() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/a.txt"]).await;
        let new = merge_added(&instance, "dir/new.txt").await;

        let state = dirty_moved(&instance, "dir", "moved").await;
        let after = node_of(&instance, &state, new).await;
        assert!(after.is_staged_add(), "flags {:#x}", after.flags);
    })
    .await;
}

/// A directory marked dirty as deleted, then moved with `lore dirty move`, takes the deletion off
/// its subtree, which its deletion stamped on every node below it.
#[tokio::test]
async fn a_dirty_move_of_a_dirty_deleted_directory_moves_its_subtree() {
    with_instance(|instance| async move {
        let below = dir_dirty_deleted(&instance).await;

        let state = dirty_move(&instance, "dir", "moved").await;
        for node in below {
            let after = node_of(&instance, &state, node).await;
            assert!(after.is_dirty_move(), "flags {:#x}", after.flags);
        }
    })
    .await;
}

/// A directory staged as deleted with no dirty half, then moved with `lore dirty move`, keeps the
/// staged deletion of its subtree: a dirty mark leaves a staged action as it is.
#[tokio::test]
async fn a_dirty_move_of_a_staged_deleted_directory_keeps_its_subtrees_deletion() {
    with_instance(|instance| async move {
        let below = dir_staged_deleted(&instance).await;

        let state = dirty_move(&instance, "dir", "moved").await;
        for node in below {
            let after = node_of(&instance, &state, node).await;
            assert!(after.is_staged_delete(), "flags {:#x}", after.flags);
        }
    })
    .await;
}

/// A directory moved with `lore dirty move` below itself is refused, as the staged move is.
#[tokio::test]
async fn a_dirty_move_of_a_directory_into_itself_is_refused() {
    with_instance(|instance| async move {
        committed(&instance, &["a/x.txt"]).await;
        nested(&instance);

        let result = lore_revision::file::dirty::dirty_move(
            instance.repository.clone(),
            instance.path.join("a").to_string_lossy().into_owned(),
            instance.path.join("a/b").to_string_lossy().into_owned(),
        )
        .await;
        assert!(
            result.is_err_and(|error| error.is_invalid_arguments()),
            "the move below itself is refused"
        );
    })
    .await;
}

/// A directory moved with `lore dirty move` onto the path a committed directory holds marks that
/// directory deleted with what it holds, and stands beside it under the same name, the path below
/// naming what it holds; and so does a directory moved onto the directory holding it, whose
/// deletion, marked once the move is done, leaves out the moved directory.
#[tokio::test]
async fn a_dirty_move_onto_a_committed_directory_marks_it_deleted_with_its_subtree() {
    for onto_parent in [false, true] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["dir/sub/x.txt", "dir/y.txt", "other/z.txt"]).await;
            let (from, replaced_file) = if onto_parent {
                flattened(&instance);
                ("dir/sub", "dir/y.txt")
            } else {
                removed_directory(&instance, "dir");
                rename(&instance, "other", "dir");
                ("other", "dir/sub/x.txt")
            };
            let moved = node_id_at(&instance, &first, from).await;
            let replaced = node_id_at(&instance, &first, "dir").await;
            let below = node_id_at(&instance, &first, replaced_file).await;

            let state = dirty_move(&instance, from, "dir").await;
            for (node, what) in [(replaced, "the replaced directory"), (below, replaced_file)] {
                let after = node_of(&instance, &state, node).await;
                assert!(
                    after.is_dirty_delete() && !after.is_staged(),
                    "{from}: {what}, flags {:#x}",
                    after.flags
                );
            }
            assert_eq!(children_named(&instance, &state, ROOT_NODE, "dir").await, 2);
            assert_eq!(node_id_at(&instance, &state, "dir").await, moved, "{from}");
            let held_below = if onto_parent {
                "dir/x.txt"
            } else {
                "dir/z.txt"
            };
            let file = node_at(&instance, &state, held_below).await;
            assert!(
                file.is_dirty_move() && !file.is_staged(),
                "{from}: {held_below}, flags {:#x}",
                file.flags
            );
        })
        .await;
    }
}

/// What a dirty mark put at the path a `lore dirty move` takes.
#[derive(Clone, Copy, Debug)]
enum DirtyOccupant {
    /// A new file marked dirty as added.
    Added,
    /// A copy of a committed file marked dirty as copied.
    Copied,
    /// A committed file another `lore dirty move` moved there.
    Moved,
    /// A committed directory another `lore dirty move` moved there, with the file it holds.
    MovedDirectory,
}

/// `lore dirty move` onto a path whose node a dirty mark added or copied there, or another `lore
/// dirty move` moved there, marks it as a dirty mark of that path, gone from the working tree,
/// marks it: the added or copied node leaves the tree, and the moved one goes back to its
/// committed path, marked deleted there with what it holds.
#[tokio::test]
async fn a_dirty_move_onto_a_path_marked_added_or_moved_there_marks_it_as_gone() {
    for occupant in [
        DirtyOccupant::Added,
        DirtyOccupant::Copied,
        DirtyOccupant::Moved,
        DirtyOccupant::MovedDirectory,
    ] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a/x", "c.txt", "d/y", "e.txt"]).await;
            let held = crate::dirty_move_fixture::tree_paths(&instance, &first).await;
            let (from, committed_at) = match occupant {
                DirtyOccupant::MovedDirectory => ("a", Some("d")),
                DirtyOccupant::Moved => ("e.txt", Some("c.txt")),
                DirtyOccupant::Added | DirtyOccupant::Copied => ("e.txt", None),
            };
            match occupant {
                DirtyOccupant::Added => {
                    dirty_added(&instance, "b").await;
                }
                DirtyOccupant::Copied => {
                    std::fs::copy(instance.path.join("c.txt"), instance.path.join("b"))
                        .expect("the copy on disk");
                    lore_revision::file::dirty::dirty_copy(
                        instance.repository.clone(),
                        instance.path.join("c.txt").to_string_lossy().into_owned(),
                        instance.path.join("b").to_string_lossy().into_owned(),
                    )
                    .await
                    .expect("the dirty copy");
                }
                DirtyOccupant::Moved | DirtyOccupant::MovedDirectory => {
                    let at = committed_at.expect("a committed path");
                    dirty_moved(&instance, at, "b").await;
                }
            }
            let placing = staged_tree(&instance).await;
            let placed = node_id_at(&instance, &placing, "b").await;
            match occupant {
                DirtyOccupant::MovedDirectory => removed_directory(&instance, "b"),
                _ => removed(&instance, "b"),
            }

            let state = dirty_moved(&instance, from, "b").await;
            assert_eq!(
                node_id_at(&instance, &state, "b").await,
                held[from],
                "{occupant:?}"
            );
            assert_eq!(
                children_named(&instance, &state, ROOT_NODE, "b").await,
                1,
                "{occupant:?}"
            );
            let Some(at) = committed_at else {
                return;
            };
            assert_eq!(
                node_id_at(&instance, &state, at).await,
                placed,
                "{occupant:?}"
            );
            let below = match occupant {
                DirtyOccupant::MovedDirectory => Some(held["d/y"]),
                _ => None,
            };
            for node in std::iter::once(placed).chain(below) {
                let returned = node_of(&instance, &state, node).await;
                assert!(
                    returned.is_dirty_delete() && !returned.is_staged(),
                    "{occupant:?}: node {node}, flags {:#x}",
                    returned.flags
                );
            }
        })
        .await;
    }
}

/// How the node a dirty move replaces holds a staged change.
#[derive(Clone, Copy, Debug)]
enum StagedOccupant {
    /// A committed file with a staged edit.
    Edit,
    /// A new file staged as added.
    Add,
    /// A committed directory with a staged edit inside.
    Directory,
    /// A committed file staged as moved there.
    Move,
}

/// `lore dirty move` onto a path whose node holds a staged change, an edit, an add, an edit inside
/// a directory, or a move, stages that node's deletion, as `lore stage move` does, since a dirty mark
/// leaves a staged change standing: the staged tree holds one live node at the name, the moved
/// one, and a commit takes its move, as it holds a staged edit, and the replaced node's deletion,
/// which the delta records where a revision held the node, a staged add having none.
#[tokio::test]
async fn a_dirty_move_onto_a_node_holding_a_staged_change_stages_its_deletion() {
    for occupant in [
        StagedOccupant::Edit,
        StagedOccupant::Add,
        StagedOccupant::Directory,
        StagedOccupant::Move,
    ] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a.txt", "b.txt", "p/f", "q/g", "other.txt"]).await;
            let (from, to, edited) = match occupant {
                StagedOccupant::Edit => ("a.txt", "b.txt", "a.txt"),
                StagedOccupant::Add | StagedOccupant::Move => ("a.txt", "n.txt", "a.txt"),
                StagedOccupant::Directory => ("q", "p", "q/g"),
            };
            let moved = node_id_at(&instance, &first, from).await;
            let edited_node = node_id_at(&instance, &first, edited).await;
            let replaced = match occupant {
                StagedOccupant::Edit => {
                    let state = restaged(&instance, "b.txt").await;
                    node_id_at(&instance, &state, "b.txt").await
                }
                StagedOccupant::Add => {
                    written(&instance, &["n.txt"]);
                    let state = stage_path(&instance, "n.txt").await;
                    node_id_at(&instance, &state, "n.txt").await
                }
                StagedOccupant::Directory => {
                    restaged(&instance, "p/f").await;
                    removed_directory(&instance, "p");
                    node_id_at(&instance, &first, "p").await
                }
                StagedOccupant::Move => {
                    staged_moved(&instance, "b.txt", "n.txt").await;
                    node_id_at(&instance, &first, "b.txt").await
                }
            };
            restaged(&instance, edited).await;

            let state = dirty_moved(&instance, from, to).await;
            let deleted = node_of(&instance, &state, replaced).await;
            assert!(
                deleted.is_staged_delete(),
                "{occupant:?}: the replaced node, flags {:#x}",
                deleted.flags
            );
            let mut live = Vec::new();
            for child in state
                .node_children(instance.repository.clone(), ROOT_NODE)
                .await
                .expect("the root's children")
            {
                let name = state
                    .node_name_clone(instance.repository.clone(), child)
                    .await
                    .expect("the child's name");
                if name == to && !node_of(&instance, &state, child).await.is_deleted() {
                    live.push(child);
                }
            }
            assert_eq!(live, [moved], "{occupant:?}: the live nodes at {to}");

            let revision = test_commit(&instance, "Replaced").await;
            assert_no_change_marked(&instance).await;
            let paths = crate::dirty_move_fixture::tree_paths(&instance, &revision).await;
            assert_eq!(
                paths.get(to),
                Some(&moved),
                "{occupant:?}: the node at {to}"
            );
            assert!(!paths.contains_key(from), "{occupant:?}: {from} stays");
            let edited_path = match occupant {
                StagedOccupant::Directory => "p/g",
                _ => to,
            };
            let content = node_at(&instance, &revision, edited_path).await;
            assert_eq!(
                (paths[edited_path], content.size),
                (edited_node, b"edited".len() as u64),
                "{occupant:?}: the moved node's content"
            );
            let delta = revision
                .node_deletion(instance.repository.clone(), replaced)
                .await
                .expect("the revision's delta");
            let deletion = match occupant {
                StagedOccupant::Add => None,
                _ => Some(FileAction::Delete as u16),
            };
            assert_eq!(
                delta.map(|delta| delta.action),
                deletion,
                "{occupant:?}: the replaced node's delta"
            );
        })
        .await;
    }
}

/// What holds the committed path of a node `lore dirty move` moved, before another move replaces it.
#[derive(Clone, Copy, Debug)]
enum PlaceHeld {
    /// Another file `lore dirty move` moved there.
    Moved,
    /// Another file `lore dirty move` moved there with a staged edit, its move staged.
    StagedMove,
    /// A new file marked dirty as added.
    Added,
}

/// `lore dirty move` onto a file another `lore dirty move` put there, whose committed path a third
/// file holds, marks it deleted at that path, beside the file holding it, as no other place holds
/// it: the moved file alone holds the name it took. The commit takes what is staged alone, the
/// replaced file's deletion where the file holding its path is staged there, and otherwise leaves
/// the status and the dirty marks as they were.
#[tokio::test]
async fn a_dirty_move_onto_a_moved_file_whose_place_another_holds_marks_it_deleted_there() {
    for held in [PlaceHeld::Moved, PlaceHeld::StagedMove, PlaceHeld::Added] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a.txt", "c.txt", "d.txt", "other.txt"]).await;
            let [a, c, d] = [
                node_id_at(&instance, &first, "a.txt").await,
                node_id_at(&instance, &first, "c.txt").await,
                node_id_at(&instance, &first, "d.txt").await,
            ];
            dirty_moved(&instance, "c.txt", "b.txt").await;
            let holder = match held {
                PlaceHeld::Moved | PlaceHeld::StagedMove => {
                    if matches!(held, PlaceHeld::StagedMove) {
                        restaged(&instance, "d.txt").await;
                    }
                    dirty_moved(&instance, "d.txt", "c.txt").await;
                    d
                }
                PlaceHeld::Added => dirty_added(&instance, "c.txt").await,
            };
            restaged(&instance, "other.txt").await;

            let state = dirty_moved(&instance, "a.txt", "b.txt").await;
            assert_eq!(
                children_named(&instance, &state, ROOT_NODE, "b.txt").await,
                1,
                "{held:?}: b.txt holds the moved file alone"
            );
            assert_eq!(node_id_at(&instance, &state, "b.txt").await, a, "{held:?}");
            let replaced = node_of(&instance, &state, c).await;
            assert!(
                replaced.is_dirty_delete() && !replaced.is_staged(),
                "{held:?}: the replaced file, flags {:#x}",
                replaced.flags
            );
            assert_eq!(
                children_named(&instance, &state, ROOT_NODE, "c.txt").await,
                2,
                "{held:?}: the replaced file beside the one holding c.txt"
            );
            assert_eq!(
                node_id_at(&instance, &state, "c.txt").await,
                holder,
                "{held:?}"
            );

            let before = status_and_marks(&instance, &first, &state).await;
            let revision = test_commit(&instance, "Edited").await;
            if matches!(held, PlaceHeld::StagedMove) {
                assert_no_change_marked(&instance).await;
            } else {
                let staged = staged_tree(&instance).await;
                let after = status_and_marks(&instance, &revision, &staged).await;
                assert_eq!(after, before, "{held:?}: the status and the marks");
            }
            let paths = crate::dirty_move_fixture::tree_paths(&instance, &revision).await;
            let expected: Vec<(&str, NodeID)> = match held {
                PlaceHeld::StagedMove => vec![("a.txt", a), ("c.txt", d)],
                _ => vec![("a.txt", a), ("c.txt", c), ("d.txt", d)],
            };
            for (path, node) in &expected {
                assert_eq!(paths.get(*path), Some(node), "{held:?}: {path}");
            }
            assert_eq!(paths.len(), expected.len() + 1, "{held:?}: {paths:?}");
            if matches!(held, PlaceHeld::StagedMove) {
                let delta = revision
                    .node_deletion(instance.repository.clone(), c)
                    .await
                    .expect("the revision's delta");
                assert_eq!(
                    delta.map(|delta| delta.action),
                    Some(FileAction::Delete as u16)
                );
            }
        })
        .await;
    }
}

/// Renames `from` to `to` below `instance`'s working tree, and asserts that marking the move dirty
/// fails with an internal error, leaving the staged anchor, the dirty marks and the status of the
/// staged tree, staged on `first`, as they were.
async fn assert_dirty_move_refused(
    instance: &TestRepository,
    first: &Arc<State>,
    (from, to): (&str, &str),
) {
    let staged_view = || async {
        let anchor = lore_revision::instance::load_staged_revision(&instance.repository)
            .await
            .expect("the staged anchor");
        let staged = staged_tree(instance).await;
        (
            anchor,
            crate::dirty_move_fixture::dirty_marks(instance, &staged).await,
            crate::dirty_move_fixture::status_changes(instance, first, &staged).await,
        )
    };
    let before = staged_view().await;
    rename(instance, from, to);
    let error = lore_revision::file::dirty::dirty_move(
        instance.repository.clone(),
        instance.path.join(from).to_string_lossy().into_owned(),
        instance.path.join(to).to_string_lossy().into_owned(),
    )
    .await
    .expect_err("the dirty move is refused");
    assert!(error.is_internal(), "the refusal: {error}");
    assert_eq!(staged_view().await, before, "the staged tree");
}

/// `lore dirty move` onto a directory another `lore dirty move` put there, whose committed parent a
/// third move then put inside it, is refused: the directory cannot go back to its committed path to
/// be marked deleted, and would stay live beside the moved node.
#[tokio::test]
async fn a_dirty_move_onto_a_moved_directory_holding_its_committed_parent_is_refused() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["p/x/f.txt", "m.txt", "other.txt"]).await;
        dirty_moved(&instance, "p/x", "x").await;
        dirty_moved(&instance, "p", "x/p").await;
        removed_directory(&instance, "x");
        assert_dirty_move_refused(&instance, &first, ("m.txt", "x")).await;
    })
    .await;
}

/// `lore dirty move` onto a directory marked added, which holds a committed file another `lore
/// dirty move` put there whose committed path a third file holds, is refused: the file cannot go
/// back to be marked deleted, and keeps the directory live beside the moved node.
#[tokio::test]
async fn a_dirty_move_onto_an_added_directory_holding_a_file_kept_from_its_place_is_refused() {
    for held in [PlaceHeld::Moved, PlaceHeld::Added] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["c.txt", "d.txt", "m.txt", "other.txt"]).await;
            made_directory(&instance, "n");
            let state = dirtied(&instance, "n").await;
            assert!(
                node_at(&instance, &state, "n").await.is_dirty_add(),
                "{held:?}"
            );
            dirty_moved(&instance, "c.txt", "n/c.txt").await;
            match held {
                PlaceHeld::Added => {
                    dirty_added(&instance, "c.txt").await;
                }
                _ => {
                    dirty_moved(&instance, "d.txt", "c.txt").await;
                }
            }
            removed_directory(&instance, "n");
            assert_dirty_move_refused(&instance, &first, ("m.txt", "n")).await;
        })
        .await;
    }
}

/// `lore dirty move` onto a directory marked added, holding a committed file another `lore dirty
/// move` put there, is taken where that file can leave: it goes back to its committed path marked
/// deleted, or, where a staged node holds that path, leaves the tree with the directory. The
/// directory leaves, and the moved file alone holds the name.
#[tokio::test]
async fn a_dirty_move_onto_an_added_directory_holding_a_file_that_can_leave_takes_its_name() {
    for staged_holder in [false, true] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["c.txt", "m.txt", "other.txt"]).await;
            let c = node_id_at(&instance, &first, "c.txt").await;
            let m = node_id_at(&instance, &first, "m.txt").await;
            made_directory(&instance, "n");
            dirtied(&instance, "n").await;
            dirty_moved(&instance, "c.txt", "n/c.txt").await;
            if staged_holder {
                written(&instance, &["c.txt"]);
                stage_path(&instance, "c.txt").await;
            }
            removed_directory(&instance, "n");

            let state = dirty_moved(&instance, "m.txt", "n").await;
            let case = format!("staged holder {staged_holder}");
            assert_eq!(
                children_named(&instance, &state, ROOT_NODE, "n").await,
                1,
                "{case}: n holds the moved file alone"
            );
            assert_eq!(node_id_at(&instance, &state, "n").await, m, "{case}");
            let holder = node_id_at(&instance, &state, "c.txt").await;
            if staged_holder {
                let record = node_of(&instance, &state, holder).await;
                assert!(
                    holder != c && record.is_staged(),
                    "{case}: c.txt, flags {:#x}",
                    record.flags
                );
                let gone = node_of(&instance, &state, c).await;
                assert!(
                    gone.is_discarded(),
                    "{case}: the file left, flags {:#x}",
                    gone.flags
                );
            } else {
                let back = node_of(&instance, &state, c).await;
                assert!(
                    holder == c && back.is_dirty_delete() && !back.is_staged(),
                    "{case}: c.txt, flags {:#x}",
                    back.flags
                );
            }
        })
        .await;
    }
}

/// What a test writes at the committed path of a node `lore dirty move` moved away.
#[derive(Clone, Copy, Debug)]
enum LeftBehind {
    /// A new file at the moved file's committed path.
    File,
    /// A new file at its path in the moved directory's committed path.
    InDirectory,
    /// Nothing, the path marked dirty while the working tree lacks it.
    Nothing,
}

/// A dirty mark of the committed path of a node `lore dirty move` moved away marks what the working
/// tree holds there as new, added beside the moved node, and leaves the moved node's move as it is:
/// a new file written at the moved file's path or at its path in a moved directory is marked
/// added, and a path the working tree lacks is no deletion of the moved node.
#[tokio::test]
async fn marking_the_path_a_dirty_move_left_keeps_the_move() {
    for left in [
        LeftBehind::File,
        LeftBehind::InDirectory,
        LeftBehind::Nothing,
    ] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a.txt", "d/f.txt", "other.txt"]).await;
            let (path, moved) = if matches!(left, LeftBehind::InDirectory) {
                dirty_moved(&instance, "d", "e").await;
                ("d/f.txt", node_id_at(&instance, &first, "d/f.txt").await)
            } else {
                dirty_moved(&instance, "a.txt", "b.txt").await;
                ("a.txt", node_id_at(&instance, &first, "a.txt").await)
            };
            if !matches!(left, LeftBehind::Nothing) {
                written(&instance, &[path]);
            }

            let state = dirtied(&instance, path).await;
            let after = node_of(&instance, &state, moved).await;
            assert!(
                after.is_dirty_move() && !after.is_dirty_modify() && !after.is_staged(),
                "{left:?}: the moved node, flags {:#x}",
                after.flags
            );
            if matches!(left, LeftBehind::Nothing) {
                assert!(!holds(&instance, &state, path).await, "{left:?}: {path}");
            } else {
                let added = node_id_at(&instance, &state, path).await;
                assert_ne!(added, moved, "{left:?}: the node at {path}");
                let added = node_of(&instance, &state, added).await;
                assert!(
                    added.is_dirty_add() && !added.is_staged(),
                    "{left:?}: {path}, flags {:#x}",
                    added.flags
                );
            }
        })
        .await;
    }
}

/// A dirty move onto a link's mount is refused as not supported, as the staged move is, and leaves
/// the moved file where it was.
#[tokio::test]
async fn a_dirty_move_onto_a_mount_is_not_supported() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt"]).await;
        let a = node_id_at(&instance, &first, "a.txt").await;
        mount_intermediate(
            &instance.repository,
            "vendor",
            RepositoryId::from(uuid::Uuid::now_v7()),
        )
        .await;
        let refused = lore_revision::file::dirty::dirty_move(
            instance.repository.clone(),
            instance.path.join("a.txt").to_string_lossy().into_owned(),
            instance.path.join("vendor").to_string_lossy().into_owned(),
        )
        .await;
        assert!(
            refused.is_err_and(|error| error.is_not_supported()),
            "the move onto the mount was not refused as not supported"
        );
        let state = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &state, "a.txt").await, a);
    })
    .await;
}

/// A commit leaves out a move `lore dirty move` marked and no stage took: the revision holds the
/// file at its committed path and name with its committed content, and the staged tree still
/// holds it moved and marked dirty.
#[tokio::test]
async fn a_commit_leaves_out_a_dirty_move_never_staged() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["base.txt", "src/file.txt"]).await;
        let base = node_id_at(&instance, &first, "base.txt").await;
        let content = node_of(&instance, &first, base).await.address;
        dirty_moved(&instance, "base.txt", "src/moved.txt").await;
        restaged(&instance, "src/file.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert_eq!(node_id_at(&instance, &revision, "base.txt").await, base);
        assert_eq!(node_of(&instance, &revision, base).await.address, content);
        assert!(!holds(&instance, &revision, "src/moved.txt").await);

        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "src/moved.txt").await, base);
        let moved = node_of(&instance, &staged, base).await;
        assert!(
            moved.is_dirty_move() && !moved.is_staged(),
            "flags {:#x}",
            moved.flags
        );
        assert!(!holds(&instance, &staged, "base.txt").await);
    })
    .await;
}

/// A file `lore dirty move` moved into a directory only marked dirty as added stays in the
/// revision: the commit drops the directory, and the file stands at its committed path.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_into_a_directory_only_marked_added() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["base.txt", "src/file.txt"]).await;
        let base = node_id_at(&instance, &first, "base.txt").await;
        made_directory(&instance, "newdir");
        let state = dirtied(&instance, "newdir").await;
        let newdir = node_at(&instance, &state, "newdir").await;
        assert!(
            newdir.is_dirty_add() && !newdir.is_staged(),
            "flags {:#x}",
            newdir.flags
        );
        dirty_moved(&instance, "base.txt", "newdir/base.txt").await;
        restaged(&instance, "src/file.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert_eq!(node_id_at(&instance, &revision, "base.txt").await, base);
        assert!(!holds(&instance, &revision, "newdir").await);

        let staged = staged_tree(&instance).await;
        assert_eq!(
            node_id_at(&instance, &staged, "newdir/base.txt").await,
            base
        );
        let moved = node_of(&instance, &staged, base).await;
        assert!(moved.is_dirty_move(), "flags {:#x}", moved.flags);
        let newdir = node_id_at(&instance, &staged, "newdir").await;
        let added = node_of(&instance, &staged, newdir).await;
        assert!(added.is_dirty_add(), "flags {:#x}", added.flags);
        assert_eq!(
            children_named(&instance, &staged, newdir, "base.txt").await,
            1
        );
    })
    .await;
}

/// A stage of an edit inside a directory `lore dirty move` renamed stages the directory's whole
/// move, as `lore stage move` marks it: the directory and each file moved with it alone carry a
/// staged move and no dirty one, an edit marked dirty stays a modification alone, with no staged
/// mark beside it, and a rename inside stays a dirty move.
#[tokio::test]
async fn a_stage_inside_a_dirty_moved_directory_stages_its_whole_move() {
    with_instance(|instance| async move {
        committed(&instance, &["d/a.txt", "d/b.txt", "d/c.txt", "d/k.txt"]).await;
        dirty_moved(&instance, "d", "e").await;
        test_file_write(&instance.path.join("e/b.txt"), b"edited in place");
        dirtied(&instance, "e/b.txt").await;
        dirty_moved(&instance, "e/k.txt", "e/q.txt").await;
        let state = restaged(&instance, "e/a.txt").await;

        let staged_move = NodeFlags::StagedMove;
        let directory = node_at(&instance, &state, "e").await;
        assert!(
            directory.flags & staged_move.bits() == staged_move.bits() && !directory.is_dirty_add(),
            "e: flags {:#x}",
            directory.flags
        );
        let moved = node_at(&instance, &state, "e/c.txt").await;
        assert_eq!(
            moved.flags,
            (NodeFlags::File | staged_move).bits(),
            "e/c.txt: flags {:#x}",
            moved.flags
        );
        let edited = node_at(&instance, &state, "e/b.txt").await;
        assert_eq!(
            edited.flags,
            (NodeFlags::File | NodeFlags::DirtyModify).bits(),
            "e/b.txt: flags {:#x}",
            edited.flags
        );
        let renamed = node_at(&instance, &state, "e/q.txt").await;
        assert!(
            renamed.is_dirty_move() && !renamed.is_staged(),
            "e/q.txt: flags {:#x}",
            renamed.flags
        );
        assert!(node_at(&instance, &state, "e/a.txt").await.is_staged());
    })
    .await;
}

/// A committed file `lore dirty move` moved into a directory only marked dirty as added, which a
/// second dirty move then moves, stays in the revision at its committed path: the move of the
/// directory marks the file moved below it, not added with it.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_into_an_added_directory_a_dirty_move_then_moves() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["base.txt", "src/file.txt"]).await;
        let base = node_id_at(&instance, &first, "base.txt").await;
        made_directory(&instance, "newdir");
        dirtied(&instance, "newdir").await;
        dirty_moved(&instance, "base.txt", "newdir/base.txt").await;
        let state = dirty_moved(&instance, "newdir", "moved").await;
        let moved = node_of(&instance, &state, base).await;
        assert!(
            moved.is_dirty_move() && !moved.is_dirty_add(),
            "flags {:#x}",
            moved.flags
        );
        restaged(&instance, "src/file.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert_eq!(node_id_at(&instance, &revision, "base.txt").await, base);
        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "moved/base.txt").await, base);
    })
    .await;
}

/// A commit leaves out a directory `lore dirty move` moved and no stage took: the revision holds
/// its subtree at the committed path, and the staged tree still holds it moved.
#[tokio::test]
async fn a_commit_leaves_out_a_dirty_move_of_a_directory_never_staged() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["dir/a.txt", "dir/s/n.txt", "other.txt"]).await;
        let n = node_id_at(&instance, &first, "dir/s/n.txt").await;
        dirty_moved(&instance, "dir", "moved").await;
        restaged(&instance, "other.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert!(holds(&instance, &revision, "dir/a.txt").await);
        assert_eq!(node_id_at(&instance, &revision, "dir/s/n.txt").await, n);
        assert!(!holds(&instance, &revision, "moved").await);

        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "moved/s/n.txt").await, n);
        let moved = node_at(&instance, &staged, "moved").await;
        assert!(
            moved.is_dirty_move() && !moved.is_staged(),
            "flags {:#x}",
            moved.flags
        );
    })
    .await;
}

/// Nested moves `lore dirty move` marked and no stage took, a directory out of its parent and the
/// parent into it, both leave the revision as committed.
#[tokio::test]
async fn a_commit_leaves_out_nested_dirty_moves_never_staged() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/b/f.txt", "other.txt"]).await;
        let f = node_id_at(&instance, &first, "a/b/f.txt").await;
        dirty_moved(&instance, "a/b", "b").await;
        dirty_moved(&instance, "a", "b/a").await;
        restaged(&instance, "other.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert_eq!(node_id_at(&instance, &revision, "a/b/f.txt").await, f);
        assert!(!holds(&instance, &revision, "b").await);

        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "b/f.txt").await, f);
        assert!(holds(&instance, &staged, "b/a").await);
        assert!(!holds(&instance, &staged, "a").await);
    })
    .await;
}

/// A file `lore dirty move` moved out of the committed directory it alone filled stays in that
/// directory in the revision, whichever of the two directories the commit reaches first.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_out_of_the_directory_it_alone_filled() {
    for (from, to, keep) in [
        ("a/x.txt", "b/x.txt", "b/keep.txt"),
        ("b/x.txt", "a/x.txt", "a/keep.txt"),
    ] {
        with_instance(|instance| async move {
            let first = committed(&instance, &[from, keep, "other.txt"]).await;
            let x = node_id_at(&instance, &first, from).await;
            dirty_moved(&instance, from, to).await;
            restaged(&instance, "other.txt").await;

            let revision = test_commit(&instance, "Edited").await;
            assert_eq!(node_id_at(&instance, &revision, from).await, x, "{from}");
            assert!(!holds(&instance, &revision, to).await, "{to}");
        })
        .await;
    }
}

/// A case-only rename `lore dirty move` marked and no stage took leaves the revision holding the
/// committed spelling.
#[tokio::test]
async fn a_commit_leaves_out_a_case_only_dirty_rename_never_staged() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["base.txt", "other.txt"]).await;
        let base = node_id_at(&instance, &first, "base.txt").await;
        dirty_moved(&instance, "base.txt", "Base.txt").await;
        restaged(&instance, "other.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        let name = revision
            .node_name_clone(instance.repository.clone(), base)
            .await
            .expect("the name");
        assert_eq!(name, "base.txt");
    })
    .await;
}

/// A commit with only a move `lore dirty move` marked finds nothing to commit, and leaves the move
/// marked.
#[tokio::test]
async fn a_commit_of_only_a_dirty_move_finds_nothing_to_commit() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["base.txt", "other.txt"]).await;
        let base = node_id_at(&instance, &first, "base.txt").await;
        dirty_moved(&instance, "base.txt", "moved.txt").await;

        let result = lore_revision::commit::commit_boxed(
            instance.repository.clone(),
            &instance.write_token,
            lore_revision::commit::CommitOptions::new("Nothing".to_string()),
        )
        .await;
        assert!(
            result.is_err_and(|error| error.is_nothing_staged()),
            "the commit finds nothing staged"
        );
        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "moved.txt").await, base);
    })
    .await;
}

/// A file both staged and moved with `lore dirty move` is committed where the staged tree holds
/// it: its flags carry the move in the action its staged change shares.
#[tokio::test]
async fn a_commit_takes_a_dirty_move_of_a_staged_file() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["base.txt", "src/file.txt"]).await;
        let base = node_id_at(&instance, &first, "base.txt").await;
        restaged(&instance, "base.txt").await;
        let state = dirty_moved(&instance, "base.txt", "src/base.txt").await;
        let moved = node_of(&instance, &state, base).await;
        assert!(moved.is_staged(), "flags {:#x}", moved.flags);

        let revision = test_commit(&instance, "Moved").await;
        assert_eq!(node_id_at(&instance, &revision, "src/base.txt").await, base);
        assert!(!holds(&instance, &revision, "base.txt").await);
    })
    .await;
}

/// A file `lore dirty move` moved into a directory only marked dirty as added stays in the
/// revision at its committed path where a scan has since marked a new file there as added: the
/// commit drops that add and the directory.
#[tokio::test]
async fn a_commit_keeps_a_dirty_moved_file_whose_committed_path_a_dirty_add_holds() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt", "src/file.txt"]).await;
        let a = node_id_at(&instance, &first, "a.txt").await;
        let content = node_of(&instance, &first, a).await.address;
        made_directory(&instance, "newdir");
        dirtied(&instance, "newdir").await;
        dirty_moved(&instance, "a.txt", "newdir/a.txt").await;
        written(&instance, &["a.txt"]);
        scanned(&instance).await;
        let staged = staged_tree(&instance).await;
        let added = node_at(&instance, &staged, "a.txt").await;
        assert!(
            added.is_dirty_add() && !added.is_staged(),
            "flags {:#x}",
            added.flags
        );
        restaged(&instance, "src/file.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert_eq!(node_id_at(&instance, &revision, "a.txt").await, a);
        assert_eq!(node_of(&instance, &revision, a).await.address, content);
        assert_eq!(
            children_named(&instance, &revision, ROOT_NODE, "a.txt").await,
            1
        );
        assert!(!holds(&instance, &revision, "newdir").await);
    })
    .await;
}

/// A committed empty directory a file in which is only marked dirty as added stays in the
/// revision: the commit drops the add and takes no deletion of the directory.
#[tokio::test]
async fn a_commit_keeps_a_committed_empty_directory_holding_only_a_dirty_add() {
    with_instance(|instance| async move {
        made_directory(&instance, "empty");
        let first = committed(&instance, &["other.txt"]).await;
        let empty = node_id_at(&instance, &first, "empty").await;
        dirty_added(&instance, "empty/new.txt").await;
        restaged(&instance, "other.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert_eq!(node_id_at(&instance, &revision, "empty").await, empty);
        assert!(!holds(&instance, &revision, "empty/new.txt").await);
    })
    .await;
}

/// A committed empty directory only marked dirty as deleted stays in the revision.
#[tokio::test]
async fn a_commit_keeps_a_committed_empty_directory_only_marked_deleted() {
    with_instance(|instance| async move {
        made_directory(&instance, "empty");
        let first = committed(&instance, &["other.txt"]).await;
        let empty = node_id_at(&instance, &first, "empty").await;
        removed_directory(&instance, "empty");
        let state = dirtied(&instance, "empty").await;
        let marked = node_at(&instance, &state, "empty").await;
        assert!(
            marked.is_dirty_delete() && !marked.is_staged(),
            "flags {:#x}",
            marked.flags
        );
        restaged(&instance, "other.txt").await;

        let revision = test_commit(&instance, "Edited").await;
        assert_eq!(node_id_at(&instance, &revision, "empty").await, empty);
    })
    .await;
}

/// Marking a moved file's new path dirty keeps its move where the revision holds another file at
/// that path: the moved file carries its change, and the other file is no part of it.
#[tokio::test]
async fn marking_a_moved_file_dirty_keeps_its_move_over_a_committed_path() {
    with_instance(|instance| async move {
        let (_, y, _) = chained(&instance).await;

        let state = dirtied(&instance, "h/x.txt").await;
        assert_eq!(node_id_at(&instance, &state, "h/x.txt").await, y);
        let moved = node_of(&instance, &state, y).await;
        assert!(
            moved.is_dirty_move() && !moved.is_staged(),
            "flags {:#x}",
            moved.flags
        );
    })
    .await;
}

/// A staged move into, onto, out of or within a mount, or of the mount itself, is refused as not
/// supported.
#[tokio::test]
async fn a_staged_move_across_a_link_is_not_supported() {
    for (from, to) in [
        ("a.txt", "vendor/src/a.txt"),
        ("a.txt", "vendor/src/Intermediate"),
        ("vendor/src/Intermediate", "b.txt"),
        ("vendor/src/Intermediate", "vendor/src/b.txt"),
        ("vendor", "moved"),
    ] {
        with_instance(|instance| async move {
            committed(&instance, &["a.txt"]).await;
            mount_intermediate(
                &instance.repository,
                "vendor",
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            let refused = lore_revision::file::stage::stage_move(
                instance.repository.clone(),
                &instance.write_token,
                instance.path.join(from).to_string_lossy().into_owned(),
                instance.path.join(to).to_string_lossy().into_owned(),
                StageOptions::default(),
            )
            .await;
            assert!(
                matches!(refused, Err(StageError::NotSupported(_))),
                "the move of {from} to {to} was not refused as not supported: {refused:?}"
            );
        })
        .await;
    }
}

/// A staged move onto the repository root is refused as invalid.
#[tokio::test]
async fn a_staged_move_onto_the_root_is_invalid() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/a.txt"]).await;
        let refused = lore_revision::file::stage::stage_move(
            instance.repository.clone(),
            &instance.write_token,
            instance.path.join("dir").to_string_lossy().into_owned(),
            instance.path.to_string_lossy().into_owned(),
            StageOptions::default(),
        )
        .await;
        assert!(
            matches!(refused, Err(StageError::InvalidArguments(_))),
            "the move onto the root was not refused as invalid: {refused:?}"
        );
    })
    .await;
}
