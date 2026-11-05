// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A staged or dirty move's marks on the directories around the node it moves, and on what it
//! moves.

use std::future::Future;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeID;
use lore_revision::stage::StageOptions;
use lore_revision::state::Clear;
use lore_revision::state::State;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::TestRepository;
use crate::helper::test_commit;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;

/// Runs `body` on a repository instance of its own.
async fn with_instance<F: Future<Output = ()>>(body: impl FnOnce(TestRepository) -> F) {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            body(instance).await;
        })
        .await;
}

/// The node `state` holds at `path`.
async fn node_id_at(instance: &TestRepository, state: &State, path: &str) -> NodeID {
    let link = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    link.node
}

/// Whether `state` holds a node at `path`; a lookup failing other than for a missing name fails
/// the test.
async fn holds(instance: &TestRepository, state: &State, path: &str) -> bool {
    match state
        .find_node_link(instance.repository.clone(), path)
        .await
    {
        Ok(link) => link.is_valid(),
        Err(error) if error.is_node_not_found() => false,
        Err(error) => panic!("{path}: {error}"),
    }
}

/// The node `state` holds at `path`, its record.
async fn node_at(instance: &TestRepository, state: &State, path: &str) -> Node {
    node_of(instance, state, node_id_at(instance, state, path).await).await
}

/// The record of `node` in `state`.
async fn node_of(instance: &TestRepository, state: &State, node: NodeID) -> Node {
    state
        .node(instance.repository.clone(), node)
        .await
        .expect("the node")
}

/// Renames `from` to `to` below `instance`'s working tree.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn rename(instance: &TestRepository, from: &str, to: &str) {
    std::fs::rename(instance.path.join(from), instance.path.join(to)).expect("the move on disk");
}

/// Removes the file `path` below `instance`'s working tree.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn removed(instance: &TestRepository, path: &str) {
    std::fs::remove_file(instance.path.join(path)).expect("the delete on disk");
}

/// Writes `files`, each a path below `instance`'s working tree holding its own path, with the
/// directories above them.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn written(instance: &TestRepository, files: &[&str]) {
    for file in files {
        let path = instance.path.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        test_file_write(&path, file.as_bytes());
    }
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

/// Renames `from` to `to` below `instance`'s working tree, then stages the move.
async fn moved(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
    rename(instance, from, to);
    staged_move(instance, from, to).await
}

/// Stages the move of `from` to `to`, which the working tree holds moved.
async fn staged_move(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
    let signature = lore_revision::file::stage::stage_move(
        instance.repository.clone(),
        &instance.write_token,
        instance.path.join(from).to_string_lossy().into_owned(),
        instance.path.join(to).to_string_lossy().into_owned(),
        StageOptions::default(),
    )
    .await
    .expect("the staged move");
    staged(instance, signature).await
}

/// Renames `from` to `to` below `instance`'s working tree, then marks the move dirty.
async fn dirty_moved(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
    rename(instance, from, to);
    dirty_move(instance, from, to).await
}

/// Marks the move of `from` to `to` dirty, which the working tree holds moved.
async fn dirty_move(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
    let signature = lore_revision::file::dirty::dirty_move(
        instance.repository.clone(),
        instance.path.join(from).to_string_lossy().into_owned(),
        instance.path.join(to).to_string_lossy().into_owned(),
    )
    .await
    .expect("the dirty move");
    staged(instance, signature).await
}

/// The staged State `signature` names.
async fn staged(instance: &TestRepository, signature: Hash) -> Arc<State> {
    State::deserialize(instance.repository.clone(), signature)
        .await
        .expect("the staged state")
}

/// Writes `files` as [`written`] does and commits them.
async fn committed(instance: &TestRepository, files: &[&str]) -> Arc<State> {
    written(instance, files);
    test_commit_tree(instance, "First").await
}

/// Stages the working tree's `path` as a scan finds it.
async fn stage_path(instance: &TestRepository, path: &str) -> Arc<State> {
    let signature = lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
        StageOptions {
            scan: true,
            ..Default::default()
        },
    )
    .await
    .expect("the stage");
    staged(instance, signature).await
}

/// Marks the working tree's `path` dirty.
async fn dirtied(instance: &TestRepository, path: &str) -> Arc<State> {
    let signature = lore_revision::file::dirty::dirty(
        instance.repository.clone(),
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
    )
    .await
    .expect("the path marked dirty");
    staged(instance, signature).await
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
/// `Staged` bit alone, and dirties nothing: the commit rehashes it, holding the file there.
#[tokio::test]
async fn a_staged_move_into_a_committed_directory_stages_it() {
    with_instance(|instance| async move {
        committed(&instance, &["dir/keep.txt", "a.txt"]).await;

        let state = moved(&instance, "a.txt", "dir/a.txt").await;
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
        assert!(!dir.is_dirty(), "a staged move dirties nothing");
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

        let state = moved(&instance, "dir/a.txt", "other/a.txt").await;
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

        let state = moved(&instance, "new/f.txt", "other/f.txt").await;
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

        let state = moved(&instance, "new/f.txt", "other/f.txt").await;
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
        moved(&instance, "dir", "moved").await;

        let state = moved(&instance, "moved/a.txt", "a.txt").await;
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

        let state = moved(&instance, "dir/sub", "other/sub").await;
        let left = node_at(&instance, &state, "dir").await;
        assert!(left.is_staged(), "flags {:#x}", left.flags);
        let x = node_at(&instance, &state, "other/sub/x.txt").await;
        assert!(x.is_staged_move(), "flags {:#x}", x.flags);
    })
    .await;
}

/// A file staged as moved over a committed file replaces it: the target, a sibling under one name
/// with the node once it moved, is staged as deleted by its own node, and the revision holds the
/// moved file's content there.
#[tokio::test]
async fn a_staged_move_over_a_committed_file_replaces_it() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt", "b.txt"]).await;
        let a = node_id_at(&instance, &first, "a.txt").await;
        let b = node_id_at(&instance, &first, "b.txt").await;
        let content = node_of(&instance, &first, a).await.address;

        let state = moved(&instance, "a.txt", "b.txt").await;
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
    })
    .await;
}

/// A directory staged as moved keeps a child's staged deletion: the move's marks of its subtree
/// leave a node staged for deletion as it is.
#[tokio::test]
async fn a_staged_move_of_a_directory_keeps_a_staged_deleted_child() {
    with_instance(|instance| async move {
        let b = child_staged_deleted(&instance).await;

        let state = moved(&instance, "dir", "moved").await;
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

        let state = moved(&instance, "dir", "moved").await;
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

/// A file or a directory staged as renamed in case alone stays, the directory with its subtree,
/// and dirties nothing: the target the move looks up, by a name's hash, which the case leaves
/// alone, is the node itself, which the move does not delete. What a commit then makes of the
/// case alone is the directory hash's to settle.
#[tokio::test]
async fn a_case_only_staged_rename_of_a_file_or_a_directory_deletes_nothing() {
    with_instance(|instance| async move {
        committed(&instance, &["File.txt", "Dir/a.txt"]).await;

        moved(&instance, "File.txt", "file.txt").await;
        let state = moved(&instance, "Dir", "dir").await;
        for path in ["file.txt", "dir", "dir/a.txt"] {
            let node = node_at(&instance, &state, path).await;
            assert!(
                !node.is_staged_delete() && !node.is_dirty(),
                "{path}: flags {:#x}",
                node.flags
            );
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

        let state = moved(&instance, "dir/a.txt", "other/deep/a.txt").await;
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

        let state = moved(&instance, "new.txt", "dir/new.txt").await;
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

        let state = moved(&instance, "dir", "moved").await;
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

/// A directory staged as deleted with no dirty half, then moved with `lore dirty move`, takes the
/// deletion off its subtree all the same.
#[tokio::test]
async fn a_dirty_move_of_a_staged_deleted_directory_moves_its_subtree() {
    with_instance(|instance| async move {
        let below = dir_staged_deleted(&instance).await;

        let state = dirty_move(&instance, "dir", "moved").await;
        for node in below {
            let after = node_of(&instance, &state, node).await;
            assert!(after.is_dirty_move(), "flags {:#x}", after.flags);
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
