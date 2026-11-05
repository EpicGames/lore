// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H8, the move half: a C API move dropped between its move and its marks leaving a directory it
//! changed unstaged, which a commit neither descends into nor rehashes. Dropped before the mark of
//! the directory the node joined, the moved node stands dirty under a clean directory and a commit
//! finds nothing staged; dropped before the mark of the one it left, a commit keeps that
//! directory's old hash, the moved node still under it by that hash. Each scenario drops a move at
//! every point it parks at, then commits; the one beside an add pins that a dropped move undoes no
//! mark, and the one under the root that a move marking nothing notes nothing. The trees are one
//! level deep, so no drop lands inside a mark's climb: a climb dropped midway leaves a dirty
//! directory under a clean one (C12).
//!
//! The change half: a C API add, modify or delete dropped between the write of its node and the
//! climb from its parent leaving the node staged and dirty under a clean directory, which a commit
//! neither finds nor rehashes. Each climbs first, and notes an unfinished change where it is
//! dropped before its node changed. Each scenario drops one at every point it parks at, then
//! commits. The delete's pins that the C API delete runs its batch in a task of its own that a
//! dropped call does not cancel; lore-revision's scenarios drop `node_delete_level` itself inside
//! its climb. The restore's pins that a restore, which climbs after its write, finds the parent its
//! delete marked; the add under the root pins that an add marking nothing notes nothing.

use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::interface::LoreNodeType;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::capi;
use crate::capi::Events;
use crate::capi::Handle;
use crate::capi::PENDING;
use crate::capi::Reported;
use crate::harness::catalogue::modelled::Modelling;
use crate::harness::invariants;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// A C API handle over a committed tree, and the calls of the scenario: the move it drops, and
/// an add beside it where it has one.
struct Moving {
    handle: Handle,
    /// Each node the setup made, by path.
    nodes: Vec<(&'static str, NodeID)>,
    events: [Events; 2],
    status: [AtomicI32; 2],
}

impl Modelled for Moving {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        vec![(self.handle.state(), self.handle.repository())]
    }
}

/// How a C API scenario is modelled: its check commits, whose spawned tasks are no tasks of the
/// scheduler's, and rehashes the tree through code that writes outside every section
/// ([`hashes_hold`]), so it runs after the checkpoint, unrecorded; and with no round trip, whose
/// save would move the revision the State stands on, which the commit then refuses as a branch
/// another instance advanced.
fn committing() -> Modelling {
    Modelling {
        check_first: false,
        round_trips: false,
        ..Modelling::default()
    }
}

impl Moving {
    /// The node the setup made at `path`, the root at `""`.
    fn id(&self, path: &str) -> NodeID {
        capi::id_of(&self.nodes, path)
    }
}

/// A move of `node` into `destination` as `name`, and the paths a scenario reads.
#[derive(Clone, Copy)]
struct Move {
    name: &'static str,
    /// The tree, directories ending in `/`, made by adds and committed.
    paths: &'static [&'static str],
    /// A file modified after the commit, so the next commit has a change of its own.
    modified: Option<&'static str>,
    /// A file added to the destination beside the move, by a call of its own.
    added: Option<&'static str>,
    node: &'static str,
    destination: &'static str,
    to: &'static str,
    /// The directory the node leaves, the destination where it renames in place; `""` for the
    /// root, which no mark changes.
    source: &'static str,
    /// Whether the node is a directory, whose subtree the move marks after it moves it.
    subtree: bool,
}

/// The tree `paths` added through the C API and committed, then `modified` modified.
async fn committed(paths: &'static [&'static str], modified: Option<&'static str>) -> Moving {
    let handle = Handle::load().await;
    let nodes = handle.seed_paths(paths).await;
    capi::set_branch(handle.tree).await;
    let events = Events::default();
    let status = capi::commit(handle.tree, &events).await;
    assert_eq!(status, 0, "the tree commits: {:?}", events.all());
    let moving = Moving {
        handle,
        nodes,
        events: Default::default(),
        status: [AtomicI32::new(PENDING), AtomicI32::new(PENDING)],
    };
    if let Some(path) = modified {
        let events = Events::default();
        let status = capi::modify(moving.handle.tree, moving.id(path), &events).await;
        assert_eq!(status, 0, "modifying '{path}' succeeds: {:?}", events.all());
    }
    moving
}

/// Every live directory of `report` records the address and size a rehash of the whole tree
/// computes from its children, as a commit that rehashed each directory whose children changed
/// leaves them: one it skipped keeps the hash of the children it had. Rehashes `state` in place,
/// every directory staged for it, so a check runs it last.
async fn hashes_hold(state: &Arc<State>, repository: &Arc<RepositoryContext>, report: &Report) {
    let directories: Vec<(NodeID, Node)> = report
        .nodes
        .iter()
        .filter(|(id, node)| **id == ROOT_NODE || (node.is_directory() && !node.is_link()))
        .map(|(id, node)| (*id, *node))
        .collect();
    for (id, _) in &directories {
        let block = invariants::block_of(state, repository, *id).await;
        block.write_for_tests().node(Node::index(*id)).flags |= NodeFlags::Staged.bits();
    }
    lore_revision::commit::rehash_directory(repository.clone(), state.clone(), ROOT_NODE)
        .await
        .unwrap_or_else(|error| panic!("the committed tree rehashes: {error}"));
    // A commit moves the root's hash and size into the tree's record, leaving its node's zero.
    let tree = state
        .tree(repository.clone())
        .await
        .expect("the tree reads");
    for (id, recorded) in directories {
        let block = invariants::block_of(state, repository, id).await;
        let rehashed = block.node(Node::index(id));
        let recorded = if id == ROOT_NODE {
            (tree.hash_root, tree.size)
        } else {
            (recorded.address.hash, recorded.size)
        };
        assert_eq!(
            (rehashed.address.hash, rehashed.size),
            recorded,
            "the commit recorded the hash of '{}' ({id}) its children have",
            report.paths[&id]
        );
    }
}

/// What the runs of a scenario saw.
#[derive(Default)]
struct Seen {
    /// Runs whose call was dropped once its change landed: a move's node moved, a change's node
    /// changed.
    dropped_moved: AtomicUsize,
    /// Runs whose call was dropped after it marked a directory, before its node changed: for a
    /// move the directory the node leaves, for a change the one it lands in.
    dropped_marked: AtomicUsize,
    /// Of a move's, runs dropped before it marked the destination.
    dropped_between_marks: AtomicUsize,
    /// Runs whose change was dropped before it landed, wherever.
    dropped_unlanded: AtomicUsize,
}

/// Explores `step`, its move dropped at each point it parks at, beside its add where it has one,
/// then commits: where the node moved, or the tree holds a change of its own, the commit succeeds
/// and records the hash every directory's children have; where neither, it finds nothing staged.
/// Some run drops the move after it marked the directory the node leaves, the node where it was;
/// where that is not the destination and nothing else stages the destination, some before the
/// destination's mark; and, for a directory, some after it moved the node, before its subtree's
/// marks.
fn dropped_then_committed(step: Move) {
    let seen = Arc::new(Seen::default());
    let counted = seen.clone();
    let mut ops: Vec<Op<Moving>> = vec![Box::new(move |c: Arc<Moving>| {
        Box::pin(async move {
            let status = capi::move_node(
                c.handle.tree,
                c.id(step.node),
                c.id(step.destination),
                step.to.rsplit_once('/').map_or(step.to, |(_, name)| name),
                &c.events[0],
            )
            .await;
            c.status[0].store(status, Ordering::SeqCst);
        })
    })];
    if let Some(added) = step.added {
        ops.push(Box::new(move |c: Arc<Moving>| {
            Box::pin(async move {
                let status = capi::add(
                    c.handle.tree,
                    c.id(step.destination),
                    added,
                    LoreNodeType::File,
                    &c.events[1],
                )
                .await;
                c.status[1].store(status, Ordering::SeqCst);
            })
        }));
    }
    let changed = step.modified.is_some() || step.added.is_some();
    let scenario = Scenario::<Moving> {
        name: step.name,
        setup: Box::new(move || Box::pin(committed(step.paths, step.modified))),
        ops,
        cancellable: vec![0],
        check: Box::new(move |c: Arc<Moving>| {
            let seen = counted.clone();
            Box::pin(async move {
                let state = c.handle.state();
                let repository = c.handle.repository();
                let report = crate::holding(&state, &repository, Rules::CLOSED).await;
                let moved = report.at(step.to).is_some();
                assert_ne!(
                    moved,
                    report.at(step.node).is_some(),
                    "the node stands at one of its two paths"
                );
                let status = c.status[0].load(Ordering::SeqCst);
                if status != PENDING {
                    assert_eq!(status, 0, "the move succeeds: {:?}", c.events[0].all());
                    assert!(moved, "a move that returned moved the node");
                }
                if step.added.is_some() {
                    assert_eq!(
                        c.status[1].load(Ordering::SeqCst),
                        0,
                        "the add succeeds: {:?}",
                        c.events[1].all()
                    );
                }
                let staged = |path| report.at(path).is_some_and(|(_, node)| node.is_staged());
                // The guard is armed before the first mark where one runs, so a drop before that
                // mark writes may note the move or not; every other run is decided.
                let marks = (step.source != step.destination && !step.source.is_empty())
                    || !step.destination.is_empty();
                let noted = state.has_unfinished_change();
                if !marks || status != PENDING || moved {
                    assert!(
                        !noted,
                        "a move that marks nothing, finished or moved its node notes nothing"
                    );
                } else if !changed && (staged(step.source) || staged(step.destination)) {
                    assert!(
                        noted,
                        "a move dropped after a mark, before its node moved, notes it"
                    );
                }
                if status == PENDING && moved {
                    seen.dropped_moved.fetch_add(1, Ordering::Relaxed);
                } else if status == PENDING && staged(step.source) {
                    seen.dropped_marked.fetch_add(1, Ordering::Relaxed);
                    if !staged(step.destination) {
                        seen.dropped_between_marks.fetch_add(1, Ordering::Relaxed);
                    }
                }

                let events = Events::default();
                let committed = capi::commit(c.handle.tree, &events).await;
                if moved || changed {
                    assert_eq!(committed, 0, "the commit succeeds: {:?}", events.all());
                    let state = c.handle.state();
                    let report = crate::holding(&state, &repository, Rules::CLOSED).await;
                    hashes_hold(&state, &repository, &report).await;
                } else {
                    let reported = events.all();
                    assert!(
                        reported.iter().any(|event| matches!(
                            event,
                            Reported::Complete(_, message) if message.contains("Nothing staged")
                        )),
                        "a commit with nothing moved finds nothing staged: {reported:?}"
                    );
                }
            })
        }),
    };
    let covered = crate::explored(
        scenario,
        committing(),
        Exploration {
            cancellation: true,
            sleep_sets: false,
            ..Exploration::default()
        },
    );
    if !step.source.is_empty() {
        assert!(
            seen.dropped_marked.load(Ordering::Relaxed) > 0,
            "no run dropped the move after its first mark with the node where it was, in {} runs",
            covered.runs
        );
    }
    if step.source != step.destination && !changed {
        assert!(
            seen.dropped_between_marks.load(Ordering::Relaxed) > 0,
            "no run dropped the move between its two marks, in {} runs",
            covered.runs
        );
    }
    if step.subtree {
        assert!(
            seen.dropped_moved.load(Ordering::Relaxed) > 0,
            "no run dropped the move between its move and its subtree's marks, in {} runs",
            covered.runs
        );
    }
}

/// A move of file `p/x` into `q`.
#[test]
fn a_move_dropped_at_every_point_commits_the_tree_it_left() {
    dropped_then_committed(Move {
        name: "h8: a move dropped at every point, then a commit",
        paths: &["p/", "p/x", "p/y", "q/", "q/z"],
        modified: None,
        added: None,
        node: "p/x",
        destination: "q",
        to: "q/x",
        source: "p",
        subtree: false,
    });
}

/// A rename of file `p/x` to `p/w`, in place.
#[test]
fn a_rename_dropped_at_every_point_commits_the_tree_it_left() {
    dropped_then_committed(Move {
        name: "h8: a rename dropped at every point, then a commit",
        paths: &["p/", "p/x", "p/y", "q/", "q/z"],
        modified: None,
        added: None,
        node: "p/x",
        destination: "p",
        to: "p/w",
        source: "p",
        subtree: false,
    });
}

/// A move of directory `p/d` into `q`, whose subtree is marked after the move: dropped between,
/// the file below keeps no staged move, which its history then lacks (C12), and the hashes are
/// still the tree's.
#[test]
fn a_directory_move_dropped_at_every_point_commits_the_tree_it_left() {
    dropped_then_committed(Move {
        name: "h8: a directory move dropped at every point, then a commit",
        paths: &["p/", "p/d/", "p/d/f", "p/y", "q/", "q/z"],
        modified: None,
        added: None,
        node: "p/d",
        destination: "q",
        to: "q/d",
        source: "p",
        subtree: true,
    });
}

/// A move of file `p/x` into `q` beside a modification of `q/z`, so every commit has a change to
/// record whatever the move did.
#[test]
fn a_move_dropped_at_every_point_beside_a_change_commits_the_tree() {
    dropped_then_committed(Move {
        name: "h8: a move dropped at every point beside a change, then a commit",
        paths: &["p/", "p/x", "p/y", "q/", "q/z"],
        modified: Some("q/z"),
        added: None,
        node: "p/x",
        destination: "q",
        to: "q/x",
        source: "p",
        subtree: false,
    });
}

/// A move of file `p/x` into `q` beside an add of `q/w`, whose climb may stop at `q` marked by
/// the move: the move undoes no mark where it is dropped, so `w` is never dirty under a clean
/// `q`, and the commit records both.
#[test]
fn a_move_dropped_beside_an_add_into_its_destination_commits_both() {
    dropped_then_committed(Move {
        name: "h8: a move dropped at every point beside an add, then a commit",
        paths: &["p/", "p/x", "p/y", "q/", "q/z"],
        modified: None,
        added: Some("w"),
        node: "p/x",
        destination: "q",
        to: "q/x",
        source: "p",
        subtree: false,
    });
}

/// A rename of file `x` to `w` under the root, which marks no directory: dropped anywhere, it
/// notes no unfinished change.
#[test]
fn a_rename_under_the_root_dropped_at_every_point_notes_nothing() {
    dropped_then_committed(Move {
        name: "h8: a rename under the root dropped at every point, then a commit",
        paths: &["x", "y"],
        modified: None,
        added: None,
        node: "x",
        destination: "",
        to: "w",
        source: "",
        subtree: false,
    });
}

/// A C API change of one node the scenario drops: an add of a file, a modify or a delete of one,
/// or a restore, by an add of its name, of one the setup deleted.
#[derive(Clone, Copy)]
enum Change {
    /// An add of file `name` under the directory.
    Add(&'static str),
    /// A modify of the file at this path.
    Modify(&'static str),
    /// A delete of the file at this path.
    Delete(&'static str),
    /// An add restoring the file at this path, which the setup deleted.
    Restore(&'static str),
}

/// A change dropped at every point, and the paths a scenario reads.
#[derive(Clone, Copy)]
struct Changed {
    name: &'static str,
    /// The tree, directories ending in `/`, made by adds and committed.
    paths: &'static [&'static str],
    /// The directory the change lands in, which it marks with its ancestors.
    directory: &'static str,
    change: Change,
}

/// Explores `step`, its change dropped at each point it parks at, then commits: where the change
/// landed, or the tree holds a change of its own (a restore's deletion), the commit succeeds and
/// records the hash every directory's children have; where neither, it finds nothing staged. A
/// change dropped after it marked its directory, before its node changed, notes an unfinished
/// change, and one that finished or changed its node notes none. Some run drops it there.
fn change_dropped_then_committed(step: Changed) {
    let seen = Arc::new(Seen::default());
    let counted = seen.clone();
    let path = |name: &'static str| -> String {
        if step.directory.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", step.directory)
        }
    };
    let target = match step.change {
        Change::Add(name) => path(name),
        Change::Modify(path) | Change::Delete(path) | Change::Restore(path) => path.to_string(),
    };
    let scenario = Scenario::<Moving> {
        name: step.name,
        setup: Box::new(move || {
            Box::pin(async move {
                let moving = committed(step.paths, None).await;
                if let Change::Restore(path) = step.change {
                    let events = Events::default();
                    let status =
                        capi::delete(moving.handle.tree, &[moving.id(path)], &events).await;
                    assert_eq!(status, 0, "deleting '{path}' succeeds: {:?}", events.all());
                }
                moving
            })
        }),
        ops: vec![Box::new(move |c: Arc<Moving>| {
            Box::pin(async move {
                let directory = c.id(step.directory);
                let status = match step.change {
                    Change::Add(name) => {
                        capi::add(
                            c.handle.tree,
                            directory,
                            name,
                            LoreNodeType::File,
                            &c.events[0],
                        )
                        .await
                    }
                    Change::Modify(path) => {
                        capi::modify(c.handle.tree, c.id(path), &c.events[0]).await
                    }
                    Change::Delete(path) => {
                        capi::delete(c.handle.tree, &[c.id(path)], &c.events[0]).await
                    }
                    Change::Restore(path) => {
                        let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
                        capi::add(
                            c.handle.tree,
                            directory,
                            name,
                            LoreNodeType::File,
                            &c.events[0],
                        )
                        .await
                    }
                };
                c.status[0].store(status, Ordering::SeqCst);
            })
        })],
        cancellable: vec![0],
        check: Box::new(move |c: Arc<Moving>| {
            let seen = counted.clone();
            let target = target.clone();
            Box::pin(async move {
                let state = c.handle.state();
                let repository = c.handle.repository();
                let report = crate::holding(&state, &repository, Rules::CLOSED).await;
                let node = report.at(&target).map(|(_, node)| node);
                let landed = match step.change {
                    Change::Add(_) => node.is_some(),
                    Change::Modify(_) => node.is_some_and(|node| node.is_staged_modify()),
                    Change::Delete(_) => node.is_some_and(|node| node.is_staged_delete()),
                    Change::Restore(_) => node.is_some_and(|node| !node.is_staged_delete()),
                };
                let status = c.status[0].load(Ordering::SeqCst);
                if status != PENDING {
                    assert_eq!(status, 0, "the change succeeds: {:?}", c.events[0].all());
                    assert!(landed, "a change that returned landed");
                }
                if matches!(step.change, Change::Delete(_)) {
                    assert!(landed, "a dropped delete's batch runs on in its own task");
                }
                let marked = report
                    .at(step.directory)
                    .is_some_and(|(_, node)| node.is_staged());
                let noted = state.has_unfinished_change();
                if step.directory.is_empty() || status != PENDING || landed {
                    assert!(
                        !noted,
                        "a change that marks nothing, finished or landed notes nothing"
                    );
                } else if marked && !matches!(step.change, Change::Restore(_)) {
                    assert!(
                        noted,
                        "a change dropped after its mark, before it landed, notes it"
                    );
                    seen.dropped_marked.fetch_add(1, Ordering::Relaxed);
                }
                if status == PENDING && landed {
                    seen.dropped_moved.fetch_add(1, Ordering::Relaxed);
                }
                if status == PENDING && !landed {
                    seen.dropped_unlanded.fetch_add(1, Ordering::Relaxed);
                }

                let events = Events::default();
                let committed = capi::commit(c.handle.tree, &events).await;
                if landed || matches!(step.change, Change::Restore(_)) {
                    assert_eq!(committed, 0, "the commit succeeds: {:?}", events.all());
                    let state = c.handle.state();
                    let report = crate::holding(&state, &repository, Rules::CLOSED).await;
                    hashes_hold(&state, &repository, &report).await;
                } else {
                    let reported = events.all();
                    assert!(
                        reported.iter().any(|event| matches!(
                            event,
                            Reported::Complete(_, message) if message.contains("Nothing staged")
                        )),
                        "a commit with nothing changed finds nothing staged: {reported:?}"
                    );
                }
            })
        }),
    };
    let covered = crate::explored(
        scenario,
        committing(),
        Exploration {
            cancellation: true,
            sleep_sets: false,
            ..Exploration::default()
        },
    );
    let count = |counter: &AtomicUsize| counter.load(Ordering::Relaxed);
    if !step.directory.is_empty() && matches!(step.change, Change::Add(_) | Change::Modify(_)) {
        assert!(
            count(&seen.dropped_marked) > 0,
            "no run dropped the change after its mark, before it landed, in {} runs",
            covered.runs
        );
    }
    // The other windows the checks above claim, each reached by some run.
    if matches!(step.change, Change::Delete(_) | Change::Restore(_)) {
        assert!(
            count(&seen.dropped_moved) > 0,
            "no run dropped the call after its change landed, in {} runs",
            covered.runs
        );
    }
    if !matches!(step.change, Change::Delete(_)) {
        assert!(
            count(&seen.dropped_unlanded) > 0,
            "no run dropped the change before it landed, in {} runs",
            covered.runs
        );
    }
}

/// An add of file `q/w`.
#[test]
fn an_add_dropped_at_every_point_commits_the_tree() {
    change_dropped_then_committed(Changed {
        name: "h8: an add dropped at every point, then a commit",
        paths: &["p/", "p/x", "q/", "q/z"],
        directory: "q",
        change: Change::Add("w"),
    });
}

/// A modify of file `q/z`.
#[test]
fn a_modify_dropped_at_every_point_commits_the_tree() {
    change_dropped_then_committed(Changed {
        name: "h8: a modify dropped at every point, then a commit",
        paths: &["p/", "p/x", "q/", "q/z"],
        directory: "q",
        change: Change::Modify("q/z"),
    });
}

/// A delete of file `q/z`, whose batch runs in a task of its own that a dropped call does not
/// cancel: every run lands it. The hooks tests drop `node_delete_level` itself.
#[test]
fn a_delete_dropped_at_every_point_commits_the_tree() {
    change_dropped_then_committed(Changed {
        name: "h8: a delete dropped at every point, then a commit",
        paths: &["p/", "p/x", "q/", "q/z"],
        directory: "q",
        change: Change::Delete("q/z"),
    });
}

/// A restore of file `q/z`, which the setup deleted: the deletion staged `q` already, so the
/// commit always has a change to record.
#[test]
fn a_restore_dropped_at_every_point_commits_the_tree() {
    change_dropped_then_committed(Changed {
        name: "h8: a restore dropped at every point, then a commit",
        paths: &["p/", "p/x", "q/", "q/z"],
        directory: "q",
        change: Change::Restore("q/z"),
    });
}

/// An add of file `w` under the root, which marks no directory: dropped anywhere, it notes no
/// unfinished change.
#[test]
fn an_add_under_the_root_dropped_at_every_point_notes_nothing() {
    change_dropped_then_committed(Changed {
        name: "h8: an add under the root dropped at every point, then a commit",
        paths: &["p/", "p/x"],
        directory: "",
        change: Change::Add("w"),
    });
}
