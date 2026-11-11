// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Commits of generated sequences of dirty moves, dirty adds, edits, deletions and stages, checked
//! against what every commit must keep rather than against the shapes earlier reviews found.
//!
//! Each sequence starts from [`BASE`] and takes one to six operations, each a kind drawn first and
//! then one of the operations of that kind valid in the working tree as it stands, by a xorshift
//! generator seeded with the sequence's number, so every run draws the same sequences. One kind is
//! a move to a committed path another move freed, so returning moves meet a name taken, in its
//! own case or another. A sequence runs three times, on instances of its own: as the commit's walk
//! meets the children of each directory the moves touch unaided, and with them moved to the front
//! of the directory in ascending and in descending order of name ([`to_front`]). Every run then
//! stages an edit of `other.txt`, which no operation touches, and commits. The commit must
//! succeed, store directories whose addresses hash their children, one child per folded name, and
//! leave one revision in all three runs; the unaided run is checked against the base, the staged
//! tree it took and the working tree as well ([`assert_kept`], [`assert_marked`]).
//!
//! The sequences leave out the shapes the commit does not handle yet:
//! - Marking dirty a new node at a path a moved committed node left: no new node is
//!   added or moved there, and where a move the commit leaves out had its path at such a path, what
//!   the working tree holds there is not checked.
//! - A name the staged tree holds twice below a directory, as a node staged for deletion
//!   and one moved to its name, which `lore dirty move` and `lore stage` take the first of: such a
//!   name is neither moved to the front nor staged.
//! - Marking dirty the removal of a directory that holds a new node or one moved into
//!   it, or of a new directory that holds a committed node, which replaces their marks.
//! - A move of a directory holding a node whose removal is marked dirty, which loses
//!   that mark past a commit that leaves the move out.
//! - Marking dirty the removal of a directory below which a removal is staged, which
//!   then reads as staged.
//! - A new node at the name of a node staged for deletion.
//!
//! A path that spells a committed path in another case is not staged either: a stage refuses it as
//! a case mismatch.
// Fixtures write in their own temporary directories, outside any repository write-token discipline.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use lore_revision::node::Node;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::State;

use crate::assert_addresses;
use crate::committed;
use crate::dirtied;
use crate::dirty_moved;
use crate::helper::TestRepository;
use crate::made_directory;
use crate::removed_directory;
use crate::restaged;
use crate::stage_path;
use crate::staged_tree;
use crate::to_front;
use crate::tree_paths;
use crate::warnings_of;
use crate::with_instance;
use crate::written;

/// The committed tree every sequence starts from.
const BASE: [&str; 7] = ["a/f1", "a/f2", "a/s/g1", "b/h1", "c/k1", "r1", "other.txt"];

/// The names an operation gives what it moves or adds: some the base tree holds, in its case and
/// in another, so a move or an add lands on a name another node leaves or keeps.
const NAMES: [&str; 8] = ["a", "A", "b", "f1", "F1", "h1", "s", "n"];

/// The sequences each test runs, numbered from its first.
const SEQUENCES_PER_TEST: u64 = 100;

/// An operation on the working tree and the marks it leaves.
#[derive(Clone, Debug)]
enum Op {
    /// `lore dirty move` of a file or a directory.
    Move(String, String),
    /// A new file, marked dirty.
    AddFile(String),
    /// A new directory, marked dirty.
    AddDirectory(String),
    /// An edit of a file, staged.
    StageEdit(String),
    /// The removal of a file or a directory, staged.
    StageDelete(String),
    /// A stage of what a move left at its new path.
    StageMoved(String),
    /// An edit of a file, marked dirty.
    DirtyEdit(String),
    /// The removal of a committed file or directory at its committed path, marked dirty.
    DirtyDelete(String),
    /// The removal of a new file or directory, marked dirty again.
    RevertAdd(String),
}

/// What the working tree holds at a path.
#[derive(Clone)]
struct Entry {
    directory: bool,
    /// The base path of the node, `None` for a new one.
    origin: Option<String>,
    /// Whether a stage took it.
    staged: bool,
    /// Whether a stage took its content, which a commit then reads as it finds it.
    content_staged: bool,
}

/// The working tree a sequence leaves: each path, the paths moves took their nodes to, the paths
/// of the nodes removed and marked dirty and of those removed and staged, as the staged tree holds
/// them, and the files edited and marked dirty since they were last staged.
#[derive(Clone, Default)]
struct Model {
    paths: BTreeMap<String, Entry>,
    moved: BTreeSet<String>,
    dirty_deleted: BTreeSet<String>,
    staged_deleted: BTreeSet<String>,
    edited: BTreeSet<String>,
    /// The base path of each committed node a move took from it.
    moved_from: BTreeSet<String>,
}

impl Model {
    fn of_base() -> Self {
        let mut model = Self::default();
        for file in BASE {
            model
                .paths
                .insert(file.to_string(), base_entry(file, false));
            let mut parent = file;
            while let Some((above, _)) = parent.rsplit_once('/') {
                model
                    .paths
                    .insert(above.to_string(), base_entry(above, true));
                parent = above;
            }
        }
        model
    }

    fn files(&self) -> Vec<String> {
        self.paths
            .iter()
            .filter(|(path, entry)| !entry.directory && path.as_str() != "other.txt")
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn directories(&self) -> Vec<String> {
        self.paths
            .iter()
            .filter(|(_, entry)| entry.directory)
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Whether `path`, folded, names what the working tree holds or a committed node removed and
    /// marked dirty, which the staged tree holds.
    fn holds(&self, path: &str) -> bool {
        self.paths
            .keys()
            .chain(&self.dirty_deleted)
            .any(|held| held.eq_ignore_ascii_case(path))
    }

    /// Each new path below a directory, the root among them, that nothing holds.
    fn free_paths(&self) -> Vec<String> {
        let mut parents = vec![String::new()];
        parents.extend(self.directories());
        let mut free = Vec::new();
        for parent in &parents {
            for name in NAMES {
                let path = join(parent, name);
                if !self.holds(&path) {
                    free.push(path);
                }
            }
        }
        free
    }

    /// Each committed path, in its own case and with its last name in upper case, that a moved
    /// committed node left and nothing holds, below a directory the working tree holds.
    fn freed_paths(&self) -> Vec<String> {
        let mut freed = Vec::new();
        for (path, entry) in &self.paths {
            let Some(origin) = &entry.origin else {
                continue;
            };
            if origin == path {
                continue;
            }
            let (parent, name) = origin.rsplit_once('/').unwrap_or(("", origin));
            if !parent.is_empty() && !self.paths.get(parent).is_some_and(|at| at.directory) {
                continue;
            }
            for spelled in [origin.clone(), join(parent, &name.to_ascii_uppercase())] {
                if !self.holds(&spelled) && !freed.contains(&spelled) {
                    freed.push(spelled);
                }
            }
        }
        freed
    }

    /// Whether `path`, folded, is the committed path of a node a move took from it and that does
    /// not stand there again.
    fn left_by_a_move(&self, path: &str) -> bool {
        self.moved_from.iter().any(|origin| {
            origin.eq_ignore_ascii_case(path)
                && self
                    .paths
                    .get(origin)
                    .is_none_or(|entry| entry.origin.as_ref() != Some(origin))
        })
    }

    /// Whether `path`, folded, is the path of a node the staged tree holds staged for deletion.
    fn names_staged_deleted(&self, path: &str) -> bool {
        self.staged_deleted
            .iter()
            .any(|deleted| deleted.eq_ignore_ascii_case(path))
    }

    /// Whether `path`, one of its ancestors or a path below it spells a committed path in another
    /// case, which a stage refuses as a name case mismatch.
    fn recased(&self, path: &str) -> bool {
        let based = Self::of_base();
        let mut spelled = self.subtree(path);
        let mut above = path;
        while let Some((parent, _)) = above.rsplit_once('/') {
            spelled.push(parent.to_string());
            above = parent;
        }
        spelled.iter().any(|at| {
            based
                .paths
                .keys()
                .any(|committed| committed != at && committed.eq_ignore_ascii_case(at))
        })
    }

    /// The paths at or below `path`.
    fn subtree(&self, path: &str) -> Vec<String> {
        self.paths
            .keys()
            .filter(|held| below_or_at(held, path))
            .cloned()
            .collect()
    }

    /// Removes `path` and everything below it.
    fn remove(&mut self, path: &str) {
        for held in self.subtree(path) {
            self.paths.remove(&held);
            self.moved.remove(&held);
            self.edited.remove(&held);
        }
    }

    /// The committed nodes removed and marked dirty at or below `path`.
    fn dirty_deleted_below(&self, path: &str) -> Vec<String> {
        below_in(&self.dirty_deleted, path)
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Move(from, to) => {
                for held in self.subtree(from) {
                    let entry = self.paths.remove(&held).expect("a held path");
                    self.moved_from.extend(entry.origin.clone());
                    let renamed = format!("{to}{}", &held[from.len()..]);
                    self.moved.remove(&held);
                    if self.edited.remove(&held) {
                        self.edited.insert(renamed.clone());
                    }
                    self.paths.insert(renamed, entry);
                }
                for deleted in below_in(&self.staged_deleted, from) {
                    self.staged_deleted.remove(&deleted);
                    self.staged_deleted
                        .insert(format!("{to}{}", &deleted[from.len()..]));
                }
                self.moved.insert(to.clone());
            }
            Op::AddFile(path) | Op::AddDirectory(path) => {
                let entry = Entry {
                    directory: matches!(op, Op::AddDirectory(_)),
                    origin: None,
                    staged: false,
                    content_staged: false,
                };
                self.paths.insert(path.clone(), entry);
            }
            Op::StageEdit(path) => {
                self.stage_moves_above(path);
                let entry = self.paths.get_mut(path).expect("a held file");
                entry.staged = true;
                entry.content_staged = true;
                self.edited.remove(path);
            }
            Op::StageMoved(path) => {
                self.stage_moves_above(path);
                for held in self.subtree(path) {
                    let edited = self.edited.remove(&held);
                    let entry = self.paths.get_mut(&held).expect("a held path");
                    entry.staged = true;
                    entry.content_staged |= edited || entry.origin.is_none();
                }
                for deleted in self.dirty_deleted_below(path) {
                    self.dirty_deleted.remove(&deleted);
                }
            }
            Op::DirtyEdit(path) => {
                if !self.paths[path].content_staged {
                    self.edited.insert(path.clone());
                }
            }
            Op::StageDelete(path) => {
                self.stage_moves_above(path);
                self.remove(path);
                for deleted in self.dirty_deleted_below(path) {
                    self.dirty_deleted.remove(&deleted);
                }
                self.staged_deleted.insert(path.clone());
            }
            Op::RevertAdd(path) => self.remove(path),
            Op::DirtyDelete(path) => {
                self.remove(path);
                self.dirty_deleted.insert(path.clone());
            }
        }
    }

    /// Stages the move of each directory above `path` that moved and is not staged, the outermost
    /// first, as a stage of `path` does: the directory and each node that moves with it alone take
    /// a staged move, which takes in the content the commit finds, except a file edited and marked
    /// dirty, whose edit stays dirty.
    fn stage_moves_above(&mut self, path: &str) {
        let mut above = Vec::new();
        let mut at = path;
        while let Some((parent, _)) = at.rsplit_once('/') {
            above.push(parent.to_string());
            at = parent;
        }
        for directory in above.into_iter().rev() {
            let moved = self.paths.get(&directory).is_some_and(|entry| {
                entry.directory
                    && !entry.staged
                    && entry
                        .origin
                        .as_ref()
                        .is_some_and(|origin| *origin != directory)
            });
            if moved {
                self.stage_directory_move(&directory);
            }
        }
    }

    /// Stages the move of `directory` whole, as [`Self::stage_moves_above`] describes.
    fn stage_directory_move(&mut self, directory: &str) {
        self.paths
            .get_mut(directory)
            .expect("a held directory")
            .staged = true;
        let mut pending = vec![directory.to_string()];
        while let Some(parent) = pending.pop() {
            let parent_origin = self.paths[&parent].origin.clone();
            let children: Vec<String> = self
                .paths
                .keys()
                .filter(|path| path.len() > parent.len() && parent_of(path) == parent)
                .cloned()
                .collect();
            for child in children {
                let entry = &self.paths[&child];
                let name = &child[parent.len() + 1..];
                let with_parent = parent_origin
                    .as_ref()
                    .is_some_and(|origin| entry.origin.as_deref() == Some(&join(origin, name)));
                if entry.staged || !with_parent || self.edited.contains(&child) {
                    continue;
                }
                let entry = self.paths.get_mut(&child).expect("a held child");
                entry.staged = true;
                entry.content_staged = true;
                if entry.directory {
                    pending.push(child);
                }
            }
        }
    }

    /// Every operation of `kind` valid in this working tree.
    fn ops(&self, kind: usize) -> Vec<Op> {
        let mut ops = Vec::new();
        let movable = || self.files().into_iter().chain(self.directories());
        match kind {
            0 | 1 => {
                let targets = if kind == 0 {
                    self.free_paths()
                } else {
                    self.freed_paths()
                };
                for from in movable() {
                    if !self.dirty_deleted_below(&from).is_empty() {
                        continue;
                    }
                    let new = self
                        .subtree(&from)
                        .iter()
                        .any(|held| self.paths[held].origin.is_none());
                    for to in &targets {
                        if !below_or_at(to, &from) && !(new && self.left_by_a_move(to)) {
                            ops.push(Op::Move(from.clone(), to.clone()));
                        }
                    }
                }
            }
            2 | 3 => {
                for path in self.free_paths() {
                    if self.left_by_a_move(&path) || self.names_staged_deleted(&path) {
                        continue;
                    }
                    ops.push(if kind == 2 {
                        Op::AddFile(path)
                    } else {
                        Op::AddDirectory(path)
                    });
                }
            }
            4 => ops.extend(
                self.files()
                    .into_iter()
                    .filter(|file| !self.recased(file) && !self.names_staged_deleted(file))
                    .map(Op::StageEdit),
            ),
            5 => ops.extend(movable().map(Op::StageDelete)),
            6 => {
                for path in &self.moved {
                    if self.paths.contains_key(path)
                        && !self.recased(path)
                        && !self.names_staged_deleted(path)
                    {
                        ops.push(Op::StageMoved(path.clone()));
                    }
                }
            }
            7 => ops.extend(self.files().into_iter().map(Op::DirtyEdit)),
            8 => {
                for path in movable() {
                    let subtree = self.subtree(&path);
                    let unmoved = subtree.iter().all(|held| {
                        self.paths[held].origin.as_deref() == Some(held.as_str())
                            && !self.paths[held].staged
                    });
                    if unmoved && below_in(&self.staged_deleted, &path).is_empty() {
                        ops.push(Op::DirtyDelete(path));
                    }
                }
            }
            _ => {
                for path in movable() {
                    let entry = &self.paths[&path];
                    let holds_base = self
                        .subtree(&path)
                        .iter()
                        .any(|held| self.paths[held].origin.is_some());
                    if entry.origin.is_none() && !entry.staged && !holds_base {
                        ops.push(Op::RevertAdd(path));
                    }
                }
            }
        }
        ops
    }
}

/// The entry of the committed `path`.
fn base_entry(path: &str, directory: bool) -> Entry {
    Entry {
        directory,
        origin: Some(path.to_string()),
        staged: false,
        content_staged: false,
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The paths of `paths` at or below `path`.
fn below_in(paths: &BTreeSet<String>, path: &str) -> Vec<String> {
    paths
        .iter()
        .filter(|held| below_or_at(held, path))
        .cloned()
        .collect()
}

fn below_or_at(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A xorshift64* generator, so a sequence's number alone decides what it draws.
struct Draw(u64);

impl Draw {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % bound
    }
}

/// How many kinds [`Model::ops`] draws from.
const KINDS: usize = 10;

/// The operations of sequence `seed`, a kind drawn first so moves, which are the most numerous,
/// do not crowd out the rest.
fn sequence(seed: u64) -> Vec<Op> {
    let mut draw = Draw::new(seed);
    let mut model = Model::of_base();
    let count = 1 + draw.below(6);
    let mut ops = Vec::with_capacity(count);
    while ops.len() < count {
        let of_kind = model.ops(draw.below(KINDS));
        if of_kind.is_empty() {
            continue;
        }
        let op = of_kind[draw.below(of_kind.len())].clone();
        model.apply(&op);
        ops.push(op);
    }
    ops
}

/// How a run orders the children of the directories the moves touch before its commit.
#[derive(Clone, Copy, Debug)]
enum Order {
    Unaided,
    Ascending,
    Descending,
}

/// A node as a tree holds it: under `parent`, by `name`, at `path`.
struct Placed {
    parent: NodeID,
    name: String,
    path: String,
    node: Node,
}

/// Every node below the root of `state`, by ID. A staged tree may hold two nodes of one name, as
/// one staged for deletion and another moved to its name.
async fn placed(instance: &TestRepository, state: &State) -> BTreeMap<NodeID, Placed> {
    let mut found = BTreeMap::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        let children = state
            .node_children(instance.repository.clone(), directory)
            .await
            .expect("the directory's children");
        for id in children {
            let name = state
                .node_name_clone(instance.repository.clone(), id)
                .await
                .expect("the child's name");
            let node = state
                .node(instance.repository.clone(), id)
                .await
                .expect("the node");
            let path = join(&prefix, &name);
            if node.is_directory() {
                pending.push((id, path.clone()));
            }
            found.insert(
                id,
                Placed {
                    parent: node.parent,
                    name,
                    path,
                    node,
                },
            );
        }
    }
    found
}

/// The nodes of `tree` that are staged or hold a staged node.
fn staged_or_holding(tree: &BTreeMap<NodeID, Placed>) -> BTreeSet<NodeID> {
    let mut found = BTreeSet::new();
    for (id, placed) in tree {
        if placed.node.is_staged() {
            let mut at = *id;
            while at != ROOT_NODE && found.insert(at) {
                match tree.get(&at) {
                    Some(above) => at = above.parent,
                    None => break,
                }
            }
        }
    }
    found
}

/// What a run leaves: the revision's paths, each with the base path of its node or `+` for a new
/// one.
struct Run {
    labels: BTreeMap<String, String>,
}

/// Runs `ops` on `instance` in `order`, commits, checks the commit against the invariants where
/// `order` is [`Order::Unaided`], and answers what the other orders are compared by.
async fn run(instance: &TestRepository, ops: &[Op], order: Order) -> Run {
    let first = committed(instance, &BASE).await;
    let base = placed(instance, &first).await;
    let mut model = Model::of_base();
    let mut edited = BTreeSet::new();
    for (index, op) in ops.iter().enumerate() {
        match op {
            Op::Move(from, to) => {
                dirty_moved(instance, from, to).await;
            }
            Op::AddFile(path) => {
                written(instance, &[path]);
                dirtied(instance, path).await;
            }
            Op::AddDirectory(path) => {
                made_directory(instance, path);
                dirtied(instance, path).await;
            }
            Op::StageEdit(path) => {
                let state = restaged(instance, path).await;
                edited.insert(crate::node_id_at(instance, &state, path).await);
            }
            Op::StageDelete(path) => {
                removed(instance, &model, path);
                stage_path(instance, path).await;
            }
            Op::StageMoved(path) => {
                let state = stage_path(instance, path).await;
                for file in model.edited.iter().filter(|file| below_or_at(file, path)) {
                    edited.insert(crate::node_id_at(instance, &state, file).await);
                }
            }
            Op::DirtyEdit(path) => {
                std::fs::write(instance.path.join(path), format!("dirty edit {index}"))
                    .expect("the edit on disk");
                let state = dirtied(instance, path).await;
                if model.paths[path].content_staged {
                    edited.insert(crate::node_id_at(instance, &state, path).await);
                }
            }
            Op::DirtyDelete(path) | Op::RevertAdd(path) => {
                removed(instance, &model, path);
                dirtied(instance, path).await;
            }
        }
        model.apply(op);
    }
    if !matches!(order, Order::Unaided) {
        to_front_where_moved(instance, ops, &model, order).await;
    }
    let state = restaged(instance, "other.txt").await;
    edited.insert(crate::node_id_at(instance, &state, "other.txt").await);
    let before = placed(instance, &*staged_tree(instance).await).await;

    let (committed, warnings) = warnings_of(lore_revision::commit::commit_boxed(
        instance.repository.clone(),
        &instance.write_token,
        lore_revision::commit::CommitOptions::new("Generated".to_string()),
    ))
    .await;
    committed.unwrap_or_else(|error| panic!("{ops:?} {order:?}: the commit failed: {error}"));
    let revision = test_revision(instance).await;
    let after = placed(instance, &revision).await;
    let held = tree_paths(instance, &revision).await;
    assert_addresses(instance, &revision, &held).await;
    assert!(warnings <= 1, "{ops:?} {order:?}: {warnings} warnings");

    if matches!(order, Order::Unaided) {
        let kept = Kept {
            base: &base,
            before: &before,
            after: &after,
        };
        let displaced = assert_kept(ops, &kept, &edited, warnings);
        let staged = staged_or_current(instance).await;
        tree_paths(instance, &staged).await;
        let marked = placed(instance, &staged).await;
        assert_marked(ops, &model, &kept, &marked, &displaced);
    }

    Run {
        labels: after
            .iter()
            .map(|(id, placed)| {
                let origin = base
                    .get(id)
                    .map_or_else(|| "+".to_string(), |at| at.path.clone());
                (placed.path.clone(), origin)
            })
            .collect(),
    }
}

/// Removes `path`, a file or a directory as `model` holds it, below `instance`'s working tree.
fn removed(instance: &TestRepository, model: &Model, path: &str) {
    if model.paths.get(path).is_some_and(|entry| entry.directory) {
        removed_directory(instance, path);
    } else {
        std::fs::remove_file(instance.path.join(path)).expect("the delete on disk");
    }
}

/// Moves the children of each directory a move of `ops` took a node from or to, where `model`
/// holds that directory, to the front of it in `order` of name ([`to_front`]). A name the staged
/// tree holds twice below one directory is left where it is; the two must be a node staged for
/// deletion and one moved to its name.
async fn to_front_where_moved(instance: &TestRepository, ops: &[Op], model: &Model, order: Order) {
    let mut parents = BTreeSet::new();
    for op in ops {
        if let Op::Move(from, to) = op {
            parents.insert(parent_of(from).to_string());
            parents.insert(parent_of(to).to_string());
        }
    }
    parents.retain(|parent| {
        parent.is_empty() || model.paths.get(parent).is_some_and(|entry| entry.directory)
    });
    let tree = staged_or_current(instance).await;
    let staged = placed(instance, &tree).await;
    for parent in parents {
        let parent_id = if parent.is_empty() {
            ROOT_NODE
        } else {
            crate::node_id_at(instance, &tree, &parent).await
        };
        let mut children: Vec<&String> = model
            .paths
            .keys()
            .filter(|path| !path.is_empty() && parent_of(path) == parent && *path != &parent)
            .collect();
        if matches!(order, Order::Descending) {
            children.reverse();
        }
        for child in children {
            let name = child.rsplit('/').next().unwrap_or(child);
            let named: Vec<&Placed> = staged
                .values()
                .filter(|at| at.parent == parent_id && at.name.eq_ignore_ascii_case(name))
                .collect();
            if named.len() > 1 {
                assert!(
                    named.len() == 2
                        && named.iter().filter(|at| at.node.is_staged_delete()).count() == 1,
                    "{ops:?}: {child} names {} nodes, flags {:?}",
                    named.len(),
                    named.iter().map(|at| at.node.flags).collect::<Vec<_>>()
                );
                continue;
            }
            to_front(instance, child).await;
        }
    }
}
/// The staged tree the instance holds, or its current revision where it holds none.
async fn staged_or_current(instance: &TestRepository) -> std::sync::Arc<State> {
    let (current, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    staged.unwrap_or(current)
}

/// The revision the instance holds current.
async fn test_revision(instance: &TestRepository) -> std::sync::Arc<State> {
    let (current, _, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    current
}

/// The trees a commit is checked by: the base revision, the staged tree it took, and the revision
/// it stored.
struct Kept<'a> {
    base: &'a BTreeMap<NodeID, Placed>,
    before: &'a BTreeMap<NodeID, Placed>,
    after: &'a BTreeMap<NodeID, Placed>,
}

/// Asserts that the revision `after` holds the base `base` with the staged changes of `before`,
/// the staged tree the commit took, and nothing else: a node staged or holding a staged node
/// stands where `before` holds it, every other base node where `base` does, a new node only where
/// it is staged or holds a staged node, and every such node is there, a file edited only where
/// its edit was staged, no node staged for deletion, and a base node gone only under a staged
/// deletion or in the subtree of a move left out, which the commit warns of, once. Answers the
/// moves left out.
fn assert_kept(
    ops: &[Op],
    kept: &Kept<'_>,
    edited: &BTreeSet<NodeID>,
    warnings: usize,
) -> BTreeSet<NodeID> {
    let Kept {
        base,
        before,
        after,
    } = *kept;
    let staged = staged_or_holding(before);
    for (id, at) in after {
        let expected = if staged.contains(id) {
            before.get(id)
        } else {
            base.get(id)
        };
        let Some(expected) = expected else {
            panic!(
                "{ops:?}: {} is a node neither staged nor in the base",
                at.path
            );
        };
        assert_eq!(
            (at.parent, &at.name),
            (expected.parent, &expected.name),
            "{ops:?}: {} stands elsewhere than {}",
            at.path,
            expected.path
        );
        if let Some(committed) = base.get(id)
            && at.node.is_file()
        {
            assert_eq!(
                at.node.address == committed.node.address,
                !edited.contains(id),
                "{ops:?}: the content of {}",
                at.path
            );
        }
    }
    for (id, at) in before {
        if at.node.is_staged_delete() && at.node.is_staged() {
            assert!(
                !after.contains_key(id),
                "{ops:?}: {} is staged for deletion and stays",
                at.path
            );
        } else if staged.contains(id) && !base.contains_key(id) && !deleted_with(before, *id) {
            assert!(
                after.contains_key(id),
                "{ops:?}: the new {} is staged and left out",
                at.path
            );
        }
    }
    let displaced: BTreeSet<NodeID> = before
        .iter()
        .filter(|(id, at)| {
            at.node.is_dirty_move()
                && !staged.contains(id)
                && !after.contains_key(id)
                && !deleted_with(before, **id)
        })
        .map(|(id, _)| *id)
        .collect();
    for (id, at) in base {
        if after.contains_key(id) {
            continue;
        }
        let deleted = deleted_with(before, *id) || deleted_with_base(base, before, *id);
        assert!(
            deleted || within(before, *id, &displaced),
            "{ops:?}: {} left the revision with no staged deletion and no move left out",
            at.path
        );
    }
    assert_eq!(
        warnings,
        usize::from(!displaced.is_empty()),
        "{ops:?}: the warnings, with the moves left out {displaced:?}"
    );
    displaced
}

/// Whether the revision `after` holds `id`, and each directory `before` holds it below, where the
/// staged tree `before` holds them: the commit took the move of each.
fn taken_in_place(
    before: &BTreeMap<NodeID, Placed>,
    after: &BTreeMap<NodeID, Placed>,
    id: NodeID,
) -> bool {
    let mut at = id;
    while let Some(placed) = before.get(&at) {
        let kept = after
            .get(&at)
            .is_some_and(|kept| (kept.parent, &kept.name) == (placed.parent, &placed.name));
        if !kept {
            return false;
        }
        at = placed.parent;
    }
    true
}

/// Whether `before` holds `id` at or below one of `nodes`.
fn within(before: &BTreeMap<NodeID, Placed>, id: NodeID, nodes: &BTreeSet<NodeID>) -> bool {
    let mut at = id;
    while let Some(placed) = before.get(&at) {
        if nodes.contains(&at) {
            return true;
        }
        at = placed.parent;
    }
    false
}

/// Whether `before` holds `id`, or an ancestor of it, staged for deletion.
fn deleted_with(before: &BTreeMap<NodeID, Placed>, id: NodeID) -> bool {
    let mut at = id;
    while let Some(placed) = before.get(&at) {
        if placed.node.is_staged_delete() {
            return true;
        }
        at = placed.parent;
    }
    false
}

/// Whether `before` holds an ancestor `base` holds `id` below staged for deletion.
fn deleted_with_base(
    base: &BTreeMap<NodeID, Placed>,
    before: &BTreeMap<NodeID, Placed>,
    id: NodeID,
) -> bool {
    let mut at = id;
    while let Some(placed) = base.get(&at) {
        if before
            .get(&at)
            .is_some_and(|marked| marked.node.is_staged_delete())
        {
            return true;
        }
        at = placed.parent;
    }
    false
}

/// Asserts that `marked`, the staged tree the commit left, marks the working tree `model` holds:
/// it holds the model's paths with their kinds, besides the committed nodes removed and marked
/// dirty, which it marks deleted at their paths; each node marked moved and not staged, unless
/// its move could not go back (`displaced`), stands at the path it had, modified where it was, and
/// marked moved where the commit left out its move or one above it, and not where the revision
/// took them; a file is marked modified only where it was marked modified and not staged before
/// the commit; and a file edited and marked dirty, and not staged since, is marked dirty.
fn assert_marked(
    ops: &[Op],
    model: &Model,
    kept: &Kept<'_>,
    marked: &BTreeMap<NodeID, Placed>,
    displaced: &BTreeSet<NodeID>,
) {
    let Kept {
        base,
        before,
        after,
    } = *kept;
    let aspc: Vec<&str> = displaced
        .iter()
        .filter_map(|id| before.get(id))
        .map(|at| at.path.as_str())
        .filter(|path| {
            after
                .iter()
                .any(|(id, kept)| base.contains_key(id) && kept.path.eq_ignore_ascii_case(path))
        })
        .collect();
    let excused = |path: &str| {
        aspc.iter()
            .any(|left| below_or_at(&path.to_lowercase(), &left.to_lowercase()))
    };
    let held: BTreeMap<&str, bool> = marked
        .values()
        .filter(|at| !at.node.is_dirty_delete() && !excused(&at.path))
        .map(|at| (at.path.as_str(), at.node.is_directory()))
        .collect();
    let expected: BTreeMap<&str, bool> = model
        .paths
        .iter()
        .filter(|(path, _)| !excused(path))
        .map(|(path, entry)| (path.as_str(), entry.directory))
        .collect();
    assert_eq!(held, expected, "{ops:?}: the staged tree's paths");
    let at_path = |path: &str| marked.values().find(|at| at.path == path);
    for path in &model.dirty_deleted {
        assert!(
            at_path(path).is_some_and(|at| at.node.is_dirty_delete()),
            "{ops:?}: {path} is not marked deleted"
        );
    }
    for path in &model.edited {
        assert!(
            at_path(path).is_some_and(|at| at.node.is_dirty()),
            "{ops:?}: the edit of {path} is not marked"
        );
    }
    let staged = staged_or_holding(before);
    for (id, at) in before {
        let at_a_path_left = after.get(id).is_some_and(|kept| excused(&kept.path));
        if !at.node.is_dirty_move()
            || staged.contains(id)
            || !base.contains_key(id)
            || at_a_path_left
            || within(before, *id, displaced)
        {
            continue;
        }
        let left_out = !taken_in_place(before, after, *id);
        let again = marked.get(id);
        assert!(
            again.is_some_and(|again| {
                again.path == at.path
                    && again.node.is_dirty_move() == left_out
                    && again.node.is_dirty_modify() == at.node.is_dirty_modify()
            }),
            "{ops:?}: the move to {}, left out {left_out}, does not keep its marks, flags {:#x}, \
             the staged tree holding {:?}",
            at.path,
            at.node.flags,
            again.map(|again| (&again.path, again.node.flags))
        );
    }
    for (id, at) in marked {
        if !at.node.is_file() || !at.node.is_dirty_modify() {
            continue;
        }
        let modified = model.edited.contains(&at.path)
            || before
                .get(id)
                .is_some_and(|was| was.node.is_dirty_modify() && !was.node.is_staged())
            || after.get(id).is_some_and(|kept| excused(&kept.path));
        assert!(
            modified,
            "{ops:?}: {} is marked modified and was not before the commit, flags {:#x}",
            at.path,
            before.get(id).map_or(0, |was| was.node.flags)
        );
    }
}

/// Runs sequences `first` to `first + SEQUENCES_PER_TEST`, each in every [`Order`], and asserts
/// that the orders leave one revision.
async fn sequences_from(first: u64) {
    for seed in first..first + SEQUENCES_PER_TEST {
        let ops = sequence(seed);
        let mut runs = Vec::new();
        for order in [Order::Unaided, Order::Ascending, Order::Descending] {
            eprintln!("seed {seed}, {order:?} order: {ops:?}");
            let ops = ops.clone();
            let (sender, receiver) = std::sync::mpsc::channel();
            with_instance(|instance| async move {
                let done = run(&instance, &ops, order).await;
                let _ = sender.send(done.labels);
            })
            .await;
            runs.push((order, receiver.recv().expect("the run's revision")));
        }
        let (_, unaided) = &runs[0];
        for (order, labels) in &runs[1..] {
            assert_eq!(
                labels, unaided,
                "seed {seed}, {ops:?}: the revision in {order:?} order differs from the unaided one"
            );
        }
    }
}

#[tokio::test]
async fn generated_sequences_0() {
    sequences_from(0).await;
}

#[tokio::test]
async fn generated_sequences_1() {
    sequences_from(SEQUENCES_PER_TEST).await;
}

#[tokio::test]
async fn generated_sequences_2() {
    sequences_from(2 * SEQUENCES_PER_TEST).await;
}

#[tokio::test]
async fn generated_sequences_3() {
    sequences_from(3 * SEQUENCES_PER_TEST).await;
}
