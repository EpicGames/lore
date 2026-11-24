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
use lore_revision::repository::RepositoryContext;
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
    node_held(&instance.repository, state, path).await.is_some()
}

/// The node `state` holds at `path`, or `None` where it holds none; a lookup failing other than
/// for a missing name fails the test.
pub async fn node_held(
    repository: &Arc<RepositoryContext>,
    state: &State,
    path: &str,
) -> Option<NodeID> {
    match state.find_node_link(repository.clone(), path).await {
        Ok(link) => link.is_valid().then_some(link.node),
        Err(error) if error.is_node_not_found() => None,
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

/// Removes the file `path` below `instance`'s working tree.
pub fn removed(instance: &TestRepository, path: &str) {
    std::fs::remove_file(instance.path.join(path)).expect("the delete on disk");
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

/// An unstaged dirty move whose target the working tree then lost: of the file `d.txt`, or where
/// `directory`, of the directory `d` holding `k.txt`, to the place `target` names, with a file or
/// directory made at the committed path before the removal where `taken`. Its tests on an
/// ordinary instance and through a mount share what each settles to, so the two keep parity.
#[derive(Clone, Copy, Debug)]
pub struct RemovedMove {
    pub target: Target,
    pub directory: bool,
    pub taken: bool,
}

/// Where a [`RemovedMove`] takes the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// `e.txt`, or the directory `e`, a path the revision lacks.
    Free,
    /// `e.txt`, or the directory `e` holding `k.txt`, which the revision holds as other nodes that
    /// the working tree lost before the move.
    Committed,
    /// `n/d.txt`, or the directory `n/d`, in `n`, a directory marked dirty as added, which the
    /// working tree loses with what it holds.
    Added,
}

/// How far a [`RemovedMove`] is settled before the commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// Not at all: the commit follows the removal.
    Unmarked,
    /// Marked, as `lore file dirty` of the committed path and the path the working tree lost
    /// marks it, and a status with `--scan` through a mount.
    Marked,
    /// Marked, then staged.
    Staged,
}

/// What a staged State holds at a path, as the tests of a [`RemovedMove`] read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    Nothing,
    DirtyMove,
    DirtyDelete,
    DirtyAdd,
    StagedMove,
    StagedDelete,
    StagedAdd,
    /// A record none of the others describes, with its flags.
    Other(u16),
}

impl Held {
    /// What `node`, a staged State's record at a path, holds; [`Held::Nothing`] where it is none.
    pub fn of(node: Option<&Node>) -> Self {
        let Some(node) = node else {
            return Held::Nothing;
        };
        if node.is_staged() {
            if node.is_staged_delete() {
                Held::StagedDelete
            } else if node.is_staged_add() {
                Held::StagedAdd
            } else if node.is_staged_move() {
                Held::StagedMove
            } else {
                Held::Other(node.flags)
            }
        } else if node.is_dirty_delete() {
            Held::DirtyDelete
        } else if node.is_dirty_add() {
            Held::DirtyAdd
        } else if node.is_dirty_move() {
            Held::DirtyMove
        } else {
            Held::Other(node.flags)
        }
    }
}

/// What the staged State of `repository` holds at each of `paths`.
pub async fn held_at(repository: &Arc<RepositoryContext>, paths: &[&str]) -> Vec<Held> {
    let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
        .await
        .expect("the states");
    let staged = staged.expect("a staged State");
    let mut held = Vec::new();
    for path in paths {
        let node = match node_held(repository, &staged, path).await {
            Some(node) => Some(
                staged
                    .node(repository.clone(), node)
                    .await
                    .expect("the node"),
            ),
            None => None,
        };
        held.push(Held::of(node.as_ref()));
    }
    held
}

/// Which of `paths` `revision`, a revision of `repository`, holds.
pub async fn committed_paths<'a>(
    repository: &Arc<RepositoryContext>,
    revision: &State,
    paths: &[&'a str],
) -> Vec<&'a str> {
    let mut committed = Vec::new();
    for &path in paths {
        if node_held(repository, revision, path).await.is_some() {
            committed.push(path);
        }
    }
    committed
}

/// Commits what `instance` has staged and answers which of `paths` the new revision holds, or
/// `None` where nothing is staged.
pub async fn commit_holding<'a>(
    instance: &TestRepository,
    paths: &[&'a str],
) -> Option<Vec<&'a str>> {
    match lore_revision::commit::commit_boxed(
        instance.repository.clone(),
        &instance.write_token,
        lore_revision::commit::CommitOptions::new("Removed move".to_string()),
    )
    .await
    {
        Ok(_) => {
            let (revision, _) =
                lore_revision::instance::load_current_anchor_boxed(&instance.repository)
                    .await
                    .expect("the current anchor");
            let revision = State::deserialize(instance.repository.clone(), revision)
                .await
                .expect("the revision");
            Some(committed_paths(&instance.repository, &revision, paths).await)
        }
        Err(error) if error.is_nothing_staged() => None,
        Err(error) => panic!("the commit: {error}"),
    }
}

impl RemovedMove {
    /// A file and a directory moved to `target`, each with its committed path free and taken.
    pub fn each(target: Target) -> [RemovedMove; 4] {
        [(false, false), (false, true), (true, false), (true, true)].map(|(directory, taken)| {
            RemovedMove {
                target,
                directory,
                taken,
            }
        })
    }

    /// The files the revision holds, `other.txt` among them.
    pub fn committed_files(self) -> &'static [&'static str] {
        match (self.target, self.directory) {
            (Target::Committed, false) => &["d.txt", "e.txt", "other.txt"],
            (Target::Committed, true) => &["d/k.txt", "e/k.txt", "other.txt"],
            (_, false) => &["d.txt", "other.txt"],
            (_, true) => &["d/k.txt", "other.txt"],
        }
    }

    /// How far its tests settle it before the commit: every way, but for a stage where its
    /// committed path is taken and the revision holds its target, which leaves the target's
    /// deletion unstaged on an ordinary instance.
    pub fn settles(self) -> &'static [Settled] {
        if self.target == Target::Committed && self.taken {
            &[Settled::Unmarked, Settled::Marked]
        } else {
            &[Settled::Unmarked, Settled::Marked, Settled::Staged]
        }
    }

    /// The committed path.
    pub fn from(self) -> &'static str {
        if self.directory { "d" } else { "d.txt" }
    }

    /// The target.
    pub fn to(self) -> &'static str {
        match (self.target, self.directory) {
            (Target::Added, false) => "n/d.txt",
            (Target::Added, true) => "n/d",
            (_, false) => "e.txt",
            (_, true) => "e",
        }
    }

    /// The path the working tree loses: the target, or `n`, which holds it.
    pub fn removed(self) -> &'static str {
        match self.target {
            Target::Added => "n",
            Target::Free | Target::Committed => self.to(),
        }
    }

    /// The paths [`Self::held`] and [`Self::committed`] read, beside `other.txt`, which the
    /// revision holds throughout.
    pub fn paths(self) -> &'static [&'static str] {
        match (self.target, self.directory) {
            (Target::Added, false) => &["d.txt", "n", "n/d.txt"],
            (Target::Added, true) => &["d", "d/k.txt", "n", "n/d", "n/d/k.txt"],
            (_, false) => &["d.txt", "e.txt"],
            (_, true) => &["d", "d/k.txt", "e", "e/k.txt"],
        }
    }

    /// What the staged State holds at each of [`Self::paths`] once settled as `settled` says, as
    /// `lore file dirty` settles it. Marking takes the node out of `n` before `n` is discarded,
    /// back to its committed path, and marks it deleted there, and the nodes the revision holds
    /// at the target come back to it, marked deleted; where the committed path is taken, the node
    /// keeps its move, `n` stays, and what took the path is marked added. A file below a moved
    /// directory is then marked deleted at the Free and Committed targets; at the Added one it
    /// keeps its move, and a stage stages its deletion. A stage stages the other marks, and a moved
    /// directory the working tree no longer holds as deleted.
    pub fn held(self, settled: Settled) -> Vec<Held> {
        use Held::*;
        let held: &[Held] = match (self.target, self.directory, self.taken, settled) {
            (Target::Added, false, _, Settled::Unmarked) => &[Nothing, DirtyAdd, DirtyMove],
            (Target::Added, false, false, Settled::Marked) => &[DirtyDelete, Nothing, Nothing],
            (Target::Added, false, true, Settled::Marked) => &[DirtyAdd, DirtyAdd, DirtyMove],
            (Target::Added, false, false, Settled::Staged) => &[StagedDelete, Nothing, Nothing],
            (Target::Added, false, true, Settled::Staged) => &[StagedAdd, StagedAdd, StagedDelete],
            (Target::Added, true, _, Settled::Unmarked) => {
                &[Nothing, Nothing, DirtyAdd, DirtyMove, DirtyMove]
            }
            (Target::Added, true, false, Settled::Marked) => {
                &[DirtyDelete, DirtyDelete, Nothing, Nothing, Nothing]
            }
            (Target::Added, true, true, Settled::Marked) => {
                &[DirtyAdd, Nothing, DirtyAdd, DirtyMove, DirtyMove]
            }
            (Target::Added, true, false, Settled::Staged) => {
                &[StagedDelete, StagedDelete, Nothing, Nothing, Nothing]
            }
            (Target::Added, true, true, Settled::Staged) => {
                &[StagedAdd, Nothing, StagedAdd, StagedDelete, StagedDelete]
            }
            (Target::Committed, false, false, Settled::Marked) => &[DirtyDelete, DirtyDelete],
            (Target::Committed, false, false, Settled::Staged) => &[StagedDelete, StagedDelete],
            (Target::Committed, true, false, Settled::Marked) => {
                &[DirtyDelete, DirtyDelete, DirtyDelete, DirtyDelete]
            }
            (Target::Committed, true, false, Settled::Staged) => {
                &[StagedDelete, StagedDelete, StagedDelete, StagedDelete]
            }
            (_, false, _, Settled::Unmarked) => &[Nothing, DirtyMove],
            (_, false, false, Settled::Marked) => &[DirtyDelete, Nothing],
            (_, false, true, Settled::Marked) => &[DirtyAdd, DirtyMove],
            (_, false, false, Settled::Staged) => &[StagedDelete, Nothing],
            (_, false, true, Settled::Staged) => &[StagedAdd, StagedDelete],
            (_, true, _, Settled::Unmarked) => &[Nothing, Nothing, DirtyMove, DirtyMove],
            (_, true, false, Settled::Marked) => &[DirtyDelete, DirtyDelete, Nothing, Nothing],
            (_, true, true, Settled::Marked) => &[DirtyAdd, Nothing, DirtyMove, DirtyDelete],
            (_, true, false, Settled::Staged) => &[StagedDelete, StagedDelete, Nothing, Nothing],
            (Target::Free, true, true, Settled::Staged) => {
                &[StagedAdd, Nothing, StagedDelete, StagedDelete]
            }
            (_, true, true, Settled::Staged) => &[StagedAdd, Nothing, StagedMove, StagedDelete],
        };
        held.to_vec()
    }

    /// Which of [`Self::paths`] and `other.txt` the commit's revision holds once settled as
    /// `settled` says, or `None` where the commit has nothing to commit: until a stage, nothing,
    /// and after it, the deletion of the moved node, with what took its path.
    pub fn committed(self, settled: Settled) -> Option<Vec<&'static str>> {
        (settled == Settled::Staged).then(|| {
            let mut held = Vec::new();
            if self.taken {
                held.push(self.from());
            }
            held.push("other.txt");
            held
        })
    }
}
