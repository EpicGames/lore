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
//! stages an edit of `other.txt`, which no operation touches, checks that the removed directories
//! its staged tree takes for their rehash alone are those the model derives
//! ([`Model::settle_bare`]), and commits. The commit must succeed, store directories whose
//! addresses hash their children, one child per folded name, and leave one revision in all three
//! runs; the unaided run is checked against the base, the staged tree it took and the working tree
//! as well ([`assert_kept`], [`assert_marked`]).
//!
//! The sequences leave out the shape the commit does not handle yet: marking dirty a new node at
//! a path a moved committed node left. No new node is added or moved there, and where a move the
//! commit leaves out had its path at such a path, what the working tree holds there is not
//! checked. A name the staged tree holds twice below a directory, as a node staged for deletion and
//! one moved to its name, is drawn as any other: `lore dirty move` and `lore stage` take the live
//! one.
//!
//! A removal marked dirty is drawn where nothing at or below it is staged and every committed
//! node a move took there is a file whose committed path is free, which it goes back to.
//!
//! A path that spells a committed path in another case is not staged either: a stage refuses it as
//! a case mismatch.
// Fixtures write in their own temporary directories, outside any repository write-token discipline.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
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
use crate::without_front;
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
    /// The paths of the directories removed and marked dirty that hold a node staged for deletion
    /// below them, or that the commit takes staged for what lies below them alone
    /// ([`Self::settle_bare`]), which the staged tree holds and does not mark deleted, and which the
    /// commit deletes with everything below them, as the working tree no longer holds them.
    unmarked: BTreeSet<String>,
    /// Each committed file a removal marked dirty took from the working tree while another node held
    /// its committed path, with its path in the staged tree and its committed path: it keeps its
    /// move, for the commit to take it back once that path frees.
    homeward: Vec<(String, String)>,
    /// The committed paths of the directories a staged deletion took a committed node out of that
    /// stood under another directory than its committed one, which the commit stages for their
    /// rehash ([`Model::left_directories`]).
    left_by_deletion: BTreeSet<String>,
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
            .chain(&self.unmarked)
            .any(|held| held.eq_ignore_ascii_case(path))
    }

    /// Whether every committed node at or below `removed` that a move took there is a file not
    /// staged, whose committed path lies below a directory the working tree holds, so its removal
    /// marked dirty takes it back there, marked deleted. Where `taken` allows, and nothing below
    /// `removed` is staged for deletion, another node may hold that path, the file then keeping its
    /// move ([`Self::homeward`]); elsewhere nothing holds it.
    fn goes_home(&self, removed: &str, taken: bool) -> bool {
        let taken = taken && below_in(&self.staged_deleted, removed).is_empty();
        self.subtree(removed).iter().all(|held| {
            let entry = &self.paths[held];
            let Some(origin) = entry.origin.as_deref().filter(|origin| origin != held) else {
                return true;
            };
            let parent = parent_of(origin);
            !entry.directory
                && !entry.staged
                && (taken || !self.holds(origin))
                && !below_or_at(parent, removed)
                && (parent.is_empty() || self.paths.get(parent).is_some_and(|at| at.directory))
        })
    }

    /// Whether a stage of `path`, a path a move took a node to, finds a change at or below it to
    /// stage: a new file, a file edited and marked dirty, or a removal marked dirty. A move marked
    /// dirty alone is none, so the stage leaves it as it is.
    fn changes_at_or_below(&self, path: &str) -> bool {
        self.subtree(path).iter().any(|held| {
            let entry = &self.paths[held];
            !entry.directory && (entry.origin.is_none() || self.edited.contains(held))
        }) || !self.dirty_deleted_below(path).is_empty()
    }

    /// Whether the node at `at`, committed at `origin`, stands under another directory than the
    /// one it was committed in.
    fn moved_out(&self, at: &str, origin: &str) -> bool {
        let (from, under) = (parent_of(origin), parent_of(at));
        if under.is_empty() {
            !from.is_empty()
        } else {
            self.paths
                .get(under)
                .and_then(|parent| parent.origin.as_deref())
                != Some(from)
        }
    }

    /// The committed paths of the directories a staged change took a committed node out of, which
    /// the commit stages for their rehash: the committed parent of each staged node standing under
    /// another directory, and of each a staged deletion took from under another one.
    fn left_directories(&self) -> BTreeSet<String> {
        let mut left = self.left_by_deletion.clone();
        for (at, entry) in &self.paths {
            if let Some(origin) = entry.origin.as_deref().filter(|_| entry.staged)
                && self.moved_out(at, origin)
            {
                left.insert(parent_of(origin).to_string());
            }
        }
        left.remove("");
        left
    }

    /// Takes each directory removed and marked dirty at or above a directory a staged change took a
    /// committed node out of ([`Self::left_directories`]) as gone: the commit stages it for its
    /// rehash, if only for one below it, and deletes it with everything below it, as for one
    /// holding a node staged for deletion ([`Self::unmarked`]). Asserts that the directories the
    /// commit stages so for what lies below them alone are those: each `before`, the staged tree
    /// the commit takes, holds staged that way, and each the commit stages, holding in `base` a
    /// staged node marked dirty that `before` holds elsewhere.
    fn settle_bare(
        &mut self,
        ops: &[Op],
        base: &BTreeMap<NodeID, Placed>,
        before: &BTreeMap<NodeID, Placed>,
    ) {
        let left = self.left_directories();
        let modelled: BTreeSet<String> = self
            .dirty_deleted
            .iter()
            .filter(|removed| left.iter().any(|left| below_or_at(left, removed)))
            .cloned()
            .collect();
        let staging: BTreeSet<NodeID> = before
            .iter()
            .filter(|(_, at)| at.node.is_staged() && at.node.is_dirty())
            .filter_map(|(id, at)| {
                let committed = base.get(id)?;
                (committed.parent != at.parent && committed.parent != ROOT_NODE)
                    .then_some(committed.parent)
            })
            .collect();
        let held: BTreeSet<String> = before
            .iter()
            .filter(|(id, at)| {
                let bare = at.node.is_staged()
                    && at.node.flags & (NodeFlags::ActionBits.bits() | NodeFlags::MergeBits.bits())
                        == 0;
                at.node.is_directory()
                    && (bare || staging.contains(id))
                    && self.dirty_deleted.contains(&at.path)
            })
            .map(|(_, at)| at.path.clone())
            .collect();
        assert_eq!(
            modelled, held,
            "{ops:?}: the removed directories the commit stages for their rehash"
        );
        for removed in modelled {
            self.dirty_deleted.remove(&removed);
            self.unmarked.insert(removed);
        }
    }

    /// Marks the removal of `path` and everything below it, as marking it dirty does: a new node
    /// leaves, a committed node a move took there goes back to be marked deleted, a committed node
    /// standing at its committed path is marked deleted there, and a directory holding a node staged
    /// for deletion below it is not.
    fn mark_removed(&mut self, path: &str) {
        for held in self.subtree(path) {
            let Some(origin) = self.paths[&held].origin.clone() else {
                continue;
            };
            if origin != held && self.holds(&origin) {
                self.homeward.push((held, origin));
            } else if origin != held {
                self.dirty_deleted.insert(origin);
            } else if below_in(&self.staged_deleted, &held).is_empty() {
                self.dirty_deleted.insert(held);
            } else {
                self.unmarked.insert(held);
            }
        }
        self.remove(path);
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
                let staged_whole = self.holds_staged_below(from);
                let moves_staged =
                    staged_whole || self.paths.get(from).is_some_and(|entry| entry.staged);
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
                for marks in [
                    &mut self.staged_deleted,
                    &mut self.dirty_deleted,
                    &mut self.unmarked,
                ] {
                    for deleted in below_in(marks, from) {
                        marks.remove(&deleted);
                        marks.insert(format!("{to}{}", &deleted[from.len()..]));
                    }
                }
                for (staged, _) in &mut self.homeward {
                    if below_or_at(staged, from) {
                        *staged = format!("{to}{}", &staged[from.len()..]);
                    }
                }
                self.moved.insert(to.clone());
                if staged_whole {
                    self.stage_directory_move(to);
                }
                if moves_staged {
                    self.stage_destination(to);
                }
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
            Op::StageMoved(path) if !self.changes_at_or_below(path) => {}
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
                for held in self.subtree(path) {
                    if let Some(origin) = self.paths[&held].origin.clone()
                        && self.moved_out(&held, &origin)
                    {
                        self.left_by_deletion.insert(parent_of(&origin).to_string());
                    }
                }
                let committed = self.committed_at_or_below(path);
                self.remove(path);
                for deleted in self.dirty_deleted_below(path) {
                    self.dirty_deleted.remove(&deleted);
                }
                self.staged_deleted.extend(committed);
            }
            Op::RevertAdd(path) | Op::DirtyDelete(path) => self.mark_removed(path),
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

    /// The topmost committed nodes at or below `path`, whose deletion a stage of its removal
    /// records: a new node leaves the staged tree with nothing to delete.
    fn committed_at_or_below(&self, path: &str) -> Vec<String> {
        let mut committed: Vec<String> = Vec::new();
        for held in self.subtree(path) {
            if self.paths[&held].origin.is_some()
                && !committed.iter().any(|above| below_or_at(&held, above))
            {
                committed.push(held);
            }
        }
        committed
    }

    /// Whether `path` is a directory not staged that holds a node staged in its own right below
    /// it, a staged deletion included, so `lore dirty move` of it stages its whole move, as a stage
    /// inside it does ([`Self::stage_directory_move`]).
    fn holds_staged_below(&self, path: &str) -> bool {
        self.paths
            .get(path)
            .is_some_and(|entry| entry.directory && !entry.staged)
            && (self
                .subtree(path)
                .iter()
                .any(|held| held != path && self.paths[held].staged)
                || below_in(&self.staged_deleted, path)
                    .iter()
                    .any(|deleted| deleted != path))
    }

    /// Stages the directories above `path`, where a move took a node holding staged content, as
    /// the move's climb does, up to the first staged one: a new directory takes a staged add, and
    /// a directory a move took there its whole move ([`Self::stage_directory_move`]).
    fn stage_destination(&mut self, path: &str) {
        let mut above = Vec::new();
        let mut at = parent_of(path);
        while !at.is_empty() {
            let Some(entry) = self.paths.get(at) else {
                break;
            };
            if entry.staged {
                break;
            }
            above.push(at.to_string());
            at = parent_of(at);
        }
        for directory in above {
            match self.paths[&directory].origin.clone() {
                None => {
                    self.paths
                        .get_mut(&directory)
                        .expect("a held directory")
                        .staged = true;
                }
                Some(origin) if origin != directory => self.stage_directory_move(&directory),
                Some(_) => {}
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

    /// Whether the removal of `path`, marked dirty, is drawn: nothing at or below it is staged, and
    /// every committed node a move took there goes back ([`Self::goes_home`]), past a node holding
    /// its committed path where `taken` allows.
    fn removable(&self, path: &str, taken: bool) -> bool {
        self.subtree(path)
            .iter()
            .all(|held| !self.paths[held].staged)
            && self.goes_home(path, taken)
    }

    /// What the commit makes of each of [`Self::homeward`], as this working tree holds its
    /// committed path: each whose committed path a node holds with nothing staged at or below it
    /// keeps its move past the commit, at the path the staged tree holds it at; each whose
    /// committed path nothing holds goes back, marked deleted there. One a staged node keeps out
    /// leaves the revision, as a move left out that cannot go back does.
    fn homeward_after(&self) -> (Vec<&str>, Vec<&str>) {
        let mut kept = Vec::new();
        let mut home = Vec::new();
        for (staged, origin) in &self.homeward {
            let holder = self
                .paths
                .keys()
                .find(|path| path.eq_ignore_ascii_case(origin));
            match holder {
                Some(holder) => {
                    if self.subtree(holder).iter().all(|at| !self.paths[at].staged) {
                        kept.push(staged.as_str());
                    }
                }
                None => home.push(origin.as_str()),
            }
        }
        (kept, home)
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
                    if self.left_by_a_move(&path) {
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
                    .filter(|file| !self.recased(file))
                    .map(Op::StageEdit),
            ),
            5 => ops.extend(movable().map(Op::StageDelete)),
            6 => {
                for path in &self.moved {
                    if self.paths.contains_key(path) && !self.recased(path) {
                        ops.push(Op::StageMoved(path.clone()));
                    }
                }
            }
            7 => ops.extend(self.files().into_iter().map(Op::DirtyEdit)),
            8 => {
                for path in movable() {
                    let in_place = self.paths[&path].origin.as_deref() == Some(path.as_str());
                    if in_place && self.removable(&path, true) {
                        ops.push(Op::DirtyDelete(path));
                    }
                }
            }
            _ => {
                for path in movable() {
                    if self.paths[&path].origin.is_none() && self.removable(&path, false) {
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
    model.settle_bare(ops, &base, &before);

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
        let gone: BTreeSet<NodeID> = before
            .iter()
            .filter(|(_, at)| model.unmarked.contains(&at.path))
            .map(|(id, _)| *id)
            .collect();
        let kept = Kept {
            base: &base,
            before: &before,
            after: &after,
            gone: &gone,
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
/// holds that directory, to the front of it in `order` of name ([`to_front`]), then removes the
/// directory they moved through ([`without_front`]). A name the staged tree holds twice below one
/// directory must be a node staged for deletion and one moved to its name, of which the move takes
/// the live one.
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
            }
            to_front(instance, child).await;
        }
    }
    without_front(instance).await;
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

/// The trees a commit is checked by: the base revision, the staged tree it took, the revision it
/// stored, and the directories of the staged tree the working tree no longer holds, which the
/// commit deletes with everything below them.
struct Kept<'a> {
    base: &'a BTreeMap<NodeID, Placed>,
    before: &'a BTreeMap<NodeID, Placed>,
    after: &'a BTreeMap<NodeID, Placed>,
    gone: &'a BTreeSet<NodeID>,
}

/// Asserts that the revision `after` holds the base `base` with the staged changes of `before`,
/// the staged tree the commit took, and nothing else: a node staged or holding a staged node
/// stands where `before` holds it, every other base node where `base` does, a new node only where
/// it is staged or holds a staged node, and every such node is there, a file edited only where
/// its edit was staged, no node staged for deletion or in a directory gone from the working tree,
/// and a base node gone only under a staged deletion, in such a directory, or in the subtree of a
/// move left out, which the commit warns of, once. Answers the moves left out.
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
        gone,
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
        } else if within(before, *id, gone) {
            assert!(
                !after.contains_key(id),
                "{ops:?}: {} lies in a directory gone from the working tree and stays",
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
        let deleted = deleted_with(before, *id)
            || deleted_with_base(base, before, *id)
            || within(before, *id, gone);
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
        gone: _,
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
    let (kept, home) = model.homeward_after();
    let expected: BTreeMap<&str, bool> = model
        .paths
        .iter()
        .filter(|(path, _)| !excused(path))
        .map(|(path, entry)| (path.as_str(), entry.directory))
        .chain(kept.iter().map(|path| (*path, false)))
        .collect();
    assert_eq!(held, expected, "{ops:?}: the staged tree's paths");
    let at_path = |path: &str| marked.values().find(|at| at.path == path);
    let gone_with = |path: &str| model.unmarked.iter().any(|gone| below_or_at(path, gone));
    let left_with = |path: &str| {
        before
            .iter()
            .any(|(id, at)| at.path == path && within(before, *id, displaced))
    };
    let deleted = model.dirty_deleted.iter().map(String::as_str).chain(home);
    for path in deleted.filter(|path| !gone_with(path) && !left_with(path)) {
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

/// Runs sequences `first` to `first + SEQUENCES_PER_TEST`, each as [`sequence_of`] runs it.
async fn sequences_from(first: u64) {
    for seed in first..first + SEQUENCES_PER_TEST {
        sequence_of(seed).await;
    }
}

/// Runs the sequence `seed` draws in every [`Order`], and asserts that the orders leave one
/// revision.
async fn sequence_of(seed: u64) {
    {
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

/// The sequences of seeds past the ones the tests above draw that remove a directory holding a
/// committed file moved into it while another node holds that file's committed path.
#[tokio::test]
async fn generated_sequences_removing_past_a_taken_place() {
    for seed in [3216, 6043] {
        sequence_of(seed).await;
    }
}

/// The sequences of seeds past the ones the tests above draw that move a directory holding a
/// staged change with `lore dirty move`, which stages its whole move, and then edit a file that
/// moved with it, which the commit takes, as it takes the content of every file staged as moved.
#[tokio::test]
async fn generated_sequences_moving_a_directory_holding_a_staged_change() {
    for seed in [3406, 6929] {
        sequence_of(seed).await;
    }
}

/// The sequences of seeds past the ones the tests above draw that stage the removal of a new file,
/// which leaves nothing staged, and that remove a file in a directory a staged move displaces,
/// whose removal leaves the revision with the directory.
#[tokio::test]
async fn generated_sequences_past_a_dropped_add_and_a_displaced_removal() {
    for seed in [4132, 9591] {
        sequence_of(seed).await;
    }
}

/// The sequence of a seed past the ones the tests above draw that removes a directory a staged
/// move took a file out of, which the commit deletes with everything below it, as the directory is
/// staged for its rehash and gone.
#[tokio::test]
async fn generated_sequences_removing_a_directory_a_staged_move_left() {
    sequence_of(8543).await;
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
