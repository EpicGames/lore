// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The fixtures of the tests of staged and dirty moves, and of what a commit takes of a dirty
//! move, which the unit tests share with the process-isolated ones.
// Fixtures write in their own temporary directories, outside any repository write-token discipline.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::change::FileAction;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::lore::RepositoryId;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::stage::StageOptions;
use lore_revision::state::Mark;
use lore_revision::state::State;

use crate::helper::TestRepository;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::helper::test_store_create;

/// Runs `body` on a repository instance of its own.
pub async fn with_instance<F: Future<Output = ()>>(body: impl FnOnce(TestRepository) -> F) {
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
pub async fn node_id_at(instance: &TestRepository, state: &State, path: &str) -> NodeID {
    let link = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    link.node
}

/// Whether `state` holds a node at `path`; a lookup failing other than for a missing name fails
/// the test.
pub async fn holds(instance: &TestRepository, state: &State, path: &str) -> bool {
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
pub async fn node_at(instance: &TestRepository, state: &State, path: &str) -> Node {
    node_of(instance, state, node_id_at(instance, state, path).await).await
}

/// The record of `node` in `state`.
pub async fn node_of(instance: &TestRepository, state: &State, node: NodeID) -> Node {
    state
        .node(instance.repository.clone(), node)
        .await
        .expect("the node")
}

/// Renames `from` to `to` below `instance`'s working tree.
pub fn rename(instance: &TestRepository, from: &str, to: &str) {
    std::fs::rename(instance.path.join(from), instance.path.join(to)).expect("the move on disk");
}

/// Writes `files`, each a path below `instance`'s working tree holding its own path, with the
/// directories above them.
pub fn written(instance: &TestRepository, files: &[&str]) {
    for file in files {
        let path = instance.path.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        test_file_write(&path, file.as_bytes());
    }
}

/// Renames `from` to `to` below `instance`'s working tree, then marks the move dirty.
pub async fn dirty_moved(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
    rename(instance, from, to);
    dirty_move(instance, from, to).await
}

/// Marks the move of `from` to `to` dirty, which the working tree holds moved.
pub async fn dirty_move(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
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
pub async fn staged(instance: &TestRepository, signature: Hash) -> Arc<State> {
    State::deserialize(instance.repository.clone(), signature)
        .await
        .expect("the staged state")
}

/// Renames `from` to `to` below `instance`'s working tree, then stages the move, as `lore stage
/// move` does.
pub async fn staged_moved(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
    rename(instance, from, to);
    staged_move(instance, from, to).await
}

/// Stages the move of `from` to `to`, which the working tree holds moved.
pub async fn staged_move(instance: &TestRepository, from: &str, to: &str) -> Arc<State> {
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

/// Writes `files` as [`written`] does and commits them.
pub async fn committed(instance: &TestRepository, files: &[&str]) -> Arc<State> {
    written(instance, files);
    test_commit_tree(instance, "First").await
}

/// Stages the working tree's `path` as a scan finds it.
pub async fn stage_path(instance: &TestRepository, path: &str) -> Arc<State> {
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
pub async fn dirtied(instance: &TestRepository, path: &str) -> Arc<State> {
    let signature = lore_revision::file::dirty::dirty(
        instance.repository.clone(),
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
    )
    .await
    .expect("the path marked dirty");
    staged(instance, signature).await
}

/// Scans the whole working tree, as `lore status --scan` does, recording first the metadata of
/// the instance's branch that a status reads.
pub async fn scanned(instance: &TestRepository) {
    let (_, branch) = lore_revision::instance::load_current_anchor_boxed(&instance.repository)
        .await
        .expect("the current anchor");
    let mut metadata = lore_revision::metadata::Metadata::new();
    lore_revision::branch::metadata_populate(
        &mut metadata,
        branch,
        lore_revision::branch::DEFAULT_DEFAULT_NAME,
        lore_revision::branch::default_category(),
        "test",
        0,
        Vec::new(),
    )
    .expect("the branch metadata");
    let hash = metadata
        .serialize(instance.repository.clone())
        .await
        .expect("the branch metadata stored");
    lore_revision::branch::mutable_store_metadata(instance.repository.clone(), branch, hash)
        .await
        .expect("the branch metadata recorded");
    lore_revision::repository::status::status_boxed(
        instance.repository.clone(),
        None,
        lore_revision::repository::status::StatusOptions {
            staged: true,
            scan: true,
            check_dirty: false,
            reset: false,
            sync_point: false,
            revision_only: false,
            count: false,
        },
    )
    .await
    .expect("the scan");
}

/// Asserts that the instance's staged tree, where it holds one, marks no node dirty with an action
/// besides a move: a node marked moved keeps that mark past a commit that took the move.
pub async fn assert_no_change_marked(instance: &TestRepository) {
    let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    let Some(staged) = staged else {
        return;
    };
    for (path, node) in tree_paths(instance, &staged).await {
        let node = node_of(instance, &staged, node).await;
        assert!(
            !node.is_dirty_add() && !node.is_dirty_modify() && !node.is_dirty_delete(),
            "{path}: flags {:#x}",
            node.flags
        );
    }
}

/// The changes a status of the whole tree reports of `state` against `first`, the revision it is
/// staged on: each one's action, the path it comes from and the node it names.
pub async fn status_changes(
    instance: &TestRepository,
    first: &Arc<State>,
    state: &Arc<State>,
) -> BTreeSet<(u16, String, NodeID)> {
    lore_revision::state::diff_collect(
        instance.repository.clone(),
        first.clone(),
        instance.repository.clone(),
        state.clone(),
        None,
        lore_revision::filter::FilterMode::CHANGES,
    )
    .await
    .expect("the status")
    .iter()
    .map(|change| {
        (
            change.action as u16,
            change.from.mapping.path.to_string(),
            change.to.mapping.node,
        )
    })
    .collect()
}

/// What a status of `state` against `first`, the revision it is staged on, reports and what
/// `state` marks dirty ([`dirty_marks`]), but for `other.txt`, which a test stages and commits
/// beside the marks it keeps. A node reported or marked added is named by its path and kind
/// alone, as a commit carrying it past adds it again under another ID.
pub async fn status_and_marks(
    instance: &TestRepository,
    first: &Arc<State>,
    state: &Arc<State>,
) -> (
    BTreeSet<(u16, String, NodeID)>,
    BTreeSet<(String, NodeID, u16)>,
) {
    let added = NodeFlags::DirtyAdd.bits();
    let changes = status_changes(instance, first, state)
        .await
        .into_iter()
        .filter(|(_, path, _)| path != "other.txt")
        .map(|(action, path, node)| {
            let node = if action == FileAction::Add as u16 {
                INVALID_NODE
            } else {
                node
            };
            (action, path, node)
        })
        .collect();
    let marks = dirty_marks(instance, state)
        .await
        .into_iter()
        .filter(|(path, _, _)| path != "other.txt")
        .map(|(path, node, flags)| {
            let node = if flags & added == added {
                INVALID_NODE
            } else {
                node
            };
            (path, node, flags)
        })
        .collect();
    (changes, marks)
}

/// Each node below the root `state` holds, two of one name among them, with its path and the dirty
/// marks it carries.
pub async fn dirty_marks(
    instance: &TestRepository,
    state: &State,
) -> BTreeSet<(String, NodeID, u16)> {
    let mut found = BTreeSet::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        let children = state
            .node_children(instance.repository.clone(), directory)
            .await
            .expect("the directory's children");
        for child in children {
            let name = state
                .node_name_clone(instance.repository.clone(), child)
                .await
                .expect("the child's name");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let node = node_of(instance, state, child).await;
            if node.is_directory() {
                pending.push((child, path.clone()));
            }
            found.insert((path, child, node.flags & NodeFlags::DirtyBits.bits()));
        }
    }
    found
}

/// Writes new content to the committed file `path` and stages it.
pub async fn restaged(instance: &TestRepository, path: &str) -> Arc<State> {
    test_file_write(&instance.path.join(path), b"edited");
    stage_path(instance, path).await
}

/// Creates the directory `path` below `instance`'s working tree.
pub fn made_directory(instance: &TestRepository, path: &str) {
    std::fs::create_dir(instance.path.join(path)).expect("mkdir");
}

/// Removes the directory `path` and everything in it below `instance`'s working tree.
pub fn removed_directory(instance: &TestRepository, path: &str) {
    std::fs::remove_dir_all(instance.path.join(path)).expect("the delete on disk");
}

/// The staged State the instance holds.
pub async fn staged_tree(instance: &TestRepository) -> Arc<State> {
    let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    staged.expect("a staged state")
}

/// How many children of the directory `parent` in `state` carry `name`.
pub async fn children_named(
    instance: &TestRepository,
    state: &State,
    parent: NodeID,
    name: &str,
) -> usize {
    let mut count = 0;
    let children = state
        .node_children(instance.repository.clone(), parent)
        .await
        .expect("the directory's children");
    for child in children {
        let held = state
            .node_name_clone(instance.repository.clone(), child)
            .await
            .expect("the child's name");
        count += usize::from(held == name);
    }
    count
}

/// Every path below the root `state` holds, with its node. Fails the test where a directory holds
/// two children of one name, as lookups, which fold case, see it.
pub async fn tree_paths(instance: &TestRepository, state: &State) -> BTreeMap<String, NodeID> {
    let mut found = BTreeMap::new();
    let mut folded = BTreeSet::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        let children = state
            .node_children(instance.repository.clone(), directory)
            .await
            .expect("the directory's children");
        for child in children {
            let name = state
                .node_name_clone(instance.repository.clone(), child)
                .await
                .expect("the child's name");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            assert!(folded.insert(path.to_lowercase()), "two nodes at {path}");
            if node_of(instance, state, child).await.is_directory() {
                pending.push((child, path.clone()));
            }
            found.insert(path, child);
        }
    }
    found
}

/// The committed `h/x.txt`, `h/sub/keep.txt`, `y.txt` and `other.txt`, with `h/x.txt` moved with
/// `lore dirty move` into `h/sub`, then `y.txt` onto `h/x.txt`: a chain, the second move holding
/// the first one's committed path. Answers the nodes of `h/x.txt` and `y.txt`, and what the
/// revision holds.
pub async fn chained(instance: &TestRepository) -> (NodeID, NodeID, BTreeMap<String, NodeID>) {
    let first = committed(
        instance,
        &["h/x.txt", "h/sub/keep.txt", "y.txt", "other.txt"],
    )
    .await;
    let x = node_id_at(instance, &first, "h/x.txt").await;
    let y = node_id_at(instance, &first, "y.txt").await;
    let held = tree_paths(instance, &first).await;
    dirty_moved(instance, "h/x.txt", "h/sub/x.txt").await;
    dirty_moved(instance, "y.txt", "h/x.txt").await;
    (x, y, held)
}

/// Asserts that each directory of `revision`, which holds `held`, holds the address its children
/// hash to, as a commit rehashing it derives.
pub async fn assert_addresses(
    instance: &TestRepository,
    revision: &State,
    held: &BTreeMap<String, NodeID>,
) {
    let rehashed = State::deserialize(instance.repository.clone(), revision.revision())
        .await
        .expect("the revision again");
    let mut directories = vec![ROOT_NODE];
    for node in held.values() {
        if node_of(instance, revision, *node).await.is_directory() {
            directories.push(*node);
        }
    }
    for directory in &directories {
        rehashed
            .node_mark(
                &instance.repository,
                *directory,
                Mark::staged(NodeFlags::Staged, false),
            )
            .await
            .expect("the directory marked for its rehash");
    }
    lore_revision::commit::rehash_directory(
        instance.repository.clone(),
        rehashed.clone(),
        ROOT_NODE,
    )
    .await
    .expect("the rehash");
    let root = revision
        .tree(instance.repository.clone())
        .await
        .expect("the revision's tree")
        .hash_root;
    assert_eq!(
        node_of(instance, &rehashed, ROOT_NODE).await.address.hash,
        root,
        "the root holds the hash its children hash to"
    );
    for directory in directories.into_iter().skip(1) {
        assert_eq!(
            node_of(instance, &rehashed, directory).await.address,
            node_of(instance, revision, directory).await.address,
            "directory {directory} holds the address its children hash to"
        );
    }
}
